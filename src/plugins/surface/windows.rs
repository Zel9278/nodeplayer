//! Worker-local capture and input for an editor HWND owned by its existing host window.
//!
//! Native printing and readback are synchronous. Only the editor's UI thread
//! may use this adapter; none of these operations belongs in the parent or audio callback.
//! GPU-owned surfaces may not support this capture path. Capture hosts stay mapped outside
//! the virtual desktop so native children and timers remain active without a desktop window.
//! Offscreen sources require WM_PRINT support; a screen-DC readback cannot
//! recover their pixels.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardState, SetKeyboardState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{EditorFrame, EditorPointer, EditorPopup, PointerKind};
use crate::plugins::window::CaptureInput;

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: *mut c_void, dc: *mut c_void, flags: u32) -> i32;
}

const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

/// Borrows its source HWND. The owner must keep that window alive until this is dropped.
/// GDI resources belong to the adapter; the HWND and all plugin children do not.
pub struct NativeCapture {
    hwnd: HWND,
    drag_target: Option<HWND>,
    bitmap: Option<Bitmap>,
    input: Rc<CaptureInput>,
    popups: Vec<(HWND, Bitmap)>,
    modifiers: u8,
    // Native handles, capture, and plugin window procedures stay on their owner thread.
    _main_thread: PhantomData<Rc<()>>,
}

impl NativeCapture {
    pub fn new(handle: crate::plugins::WindowId) -> Result<Self> {
        let hwnd = HWND(handle as *mut c_void);
        ensure!(
            unsafe { IsWindow(hwnd) }.as_bool(),
            "editor capture HWND was destroyed"
        );
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
        ensure!(
            process == unsafe { GetCurrentProcessId() }
                && thread == unsafe { GetCurrentThreadId() },
            "editor capture must run on the source window's owner thread"
        );
        let capture = Self {
            hwnd,
            drag_target: None,
            bitmap: None,
            input: CaptureInput::new(hwnd)?,
            popups: Vec::new(),
            modifiers: 0,
            _main_thread: PhantomData,
        };
        capture.check_window()?;
        Ok(capture)
    }

    fn check_window(&self) -> Result<()> {
        // HWNDs may cross process boundaries elsewhere, but native operations here must
        // stay with the worker that created the editor, on its UI thread.
        unsafe {
            ensure!(
                IsWindow(self.hwnd).as_bool(),
                "editor capture HWND was destroyed"
            );
            let mut process = 0;
            let thread = GetWindowThreadProcessId(self.hwnd, Some(&mut process));
            ensure!(
                process == GetCurrentProcessId() && thread == GetCurrentThreadId(),
                "editor capture must run on the source window's owner thread"
            );
        }
        Ok(())
    }

    pub fn capture(&mut self, _native_size: (u32, u32)) -> Result<EditorFrame> {
        self.check_window()?;
        self.input.refresh()?;
        let _keyboard = KeyboardModifiers::new(self.modifiers)?;
        let _input = self.input.enter();
        let mut client = RECT::default();
        // SAFETY: check_window verifies a live HWND owned by this thread.
        unsafe { GetClientRect(self.hwnd, &mut client) }.context("read editor client size")?;
        let width = (client.right - client.left).max(0) as u32;
        let height = (client.bottom - client.top).max(0) as u32;
        ensure!(
            width > 0
                && height > 0
                && width <= MAX_DIMENSION
                && height <= MAX_DIMENSION
                && u64::from(width) * u64::from(height) <= MAX_PIXELS,
            "editor exceeds capture dimensions or pixel budget"
        );
        if self
            .bitmap
            .as_ref()
            .is_none_or(|bitmap| bitmap.size != (width, height))
        {
            self.bitmap = Some(Bitmap::new(self.hwnd, width, height)?);
        }
        let bitmap = self.bitmap.as_ref().expect("capture bitmap was allocated");
        unsafe {
            // A plugin that does not paint every pixel must not expose stale frames.
            std::ptr::write_bytes(bitmap.bits, 0, width as usize * height as usize * 4);
            let mut origin = POINT::default();
            ensure!(
                ClientToScreen(self.hwnd, &mut origin).as_bool(),
                "locate editor client origin"
            );
            let root_printed = {
                let _saved = SavedDc::new(bitmap.memory_dc)?;
                // Several plugin editors implement PrintWindow's WM_PRINT protocol but
                // ignore a direct WM_PRINT sent to their root HWND.
                PrintWindow(self.hwnd.0, bitmap.memory_dc.0, 3) != 0
            };
            let root_has_pixels = {
                let _ = GdiFlush();
                let bytes = std::slice::from_raw_parts(
                    bitmap.bits.cast::<u8>(),
                    width as usize * height as usize * 4,
                );
                bytes.iter().any(|byte| *byte != 0)
            };
            let root_printed = root_printed && root_has_pixels;
            let root_clip = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            if root_printed {
                // PrintWindow includes native children, but can omit offscreen children
                // or composite overlapping siblings incorrectly. Reapply the explicit
                // traversal to preserve ancestor clipping and sibling stacking.
                let mut budget = 512;
                print_children(
                    self.hwnd,
                    bitmap.memory_dc,
                    origin,
                    root_clip,
                    0,
                    &mut budget,
                )?;
            } else {
                // Preserve support for windows that implement WM_PRINT but not
                // PrintWindow. The traversal owns both the root and child painting.
                std::ptr::write_bytes(bitmap.bits, 0, width as usize * height as usize * 4);
                let mut budget = 512;
                print_tree(
                    self.hwnd,
                    bitmap.memory_dc,
                    origin,
                    root_clip,
                    0,
                    &mut budget,
                    true,
                )?;
            }
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts(
                bitmap.bits.cast::<u8>(),
                width as usize * height as usize * 4,
            );
            let mut rgba = Vec::with_capacity(pixels.len());
            for pixel in pixels.as_chunks::<4>().0 {
                // BI_RGB does not define DIB alpha; an opaque editor must stay visible.
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
            let mut frame = EditorFrame {
                popups: Vec::new(),
                width,
                height,
                native_size: (width, height),
                native_units_per_point: 1.0,
                rgba,
            };
            let mut previous = std::mem::take(&mut self.popups);
            let mut remaining = MAX_PIXELS - u64::from(width) * u64::from(height);
            for (hwnd, logical_origin) in self.input.popups()? {
                let layered = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_LAYERED.0 != 0;
                let uploaded = if layered {
                    crate::plugins::window::layered::image(hwnd)?
                } else {
                    None
                };
                let mut color_key = COLORREF::default();
                let mut opacity = 255;
                let mut attributes = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
                if layered
                    && uploaded.is_none()
                    && GetLayeredWindowAttributes(
                        hwnd,
                        Some(&mut color_key),
                        Some(&mut opacity),
                        Some(&mut attributes),
                    )
                    .is_err()
                {
                    // A per-pixel layered window has no printable backing. Keep
                    // presenting the editor while its first upload is in flight.
                    continue;
                }
                let mut bounds = RECT::default();
                GetWindowRect(hwnd, &mut bounds).context("read captured popup size")?;
                let (width, height) = (
                    (bounds.right - bounds.left).max(0) as u32,
                    (bounds.bottom - bounds.top).max(0) as u32,
                );
                // Menus can briefly retain WS_VISIBLE while being resized to zero
                // during dismissal. They do not contribute an image in that state.
                if width == 0 || height == 0 { continue; }
                ensure!(
                    width <= MAX_DIMENSION
                        && height <= MAX_DIMENSION
                        && u64::from(width) * u64::from(height) <= remaining,
                    "editor popups exceed capture dimensions or pixel budget"
                );
                remaining -= u64::from(width) * u64::from(height);
                let rgba = if let Some(image) = uploaded {
                    ensure!(
                        (image.width, image.height) == (width, height),
                        "layered popup upload differs from its native size"
                    );
                    image.rgba
                } else {
                    let bitmap = previous
                        .iter()
                        .position(|(window, bitmap)| {
                            *window == hwnd && bitmap.size == (width, height)
                        })
                        .map(|index| previous.swap_remove(index).1)
                        .map_or_else(|| Bitmap::new(hwnd, width, height), Ok)?;
                    std::ptr::write_bytes(bitmap.bits, 0, width as usize * height as usize * 4);
                    let mut popup_budget = 512;
                    let popup_origin = POINT { x: bounds.left, y: bounds.top };
                    let popup_clip = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
                    // Prefer the popup's WM_PRINT handler. PrintWindow can succeed
                    // with an incomplete image before an offscreen popup's first paint.
                    print_tree(hwnd, bitmap.memory_dc, popup_origin, popup_clip, 0, &mut popup_budget, false)?;
                    let _ = GdiFlush();
                    let painted = std::slice::from_raw_parts(
                        bitmap.bits.cast::<u8>(), width as usize * height as usize * 4,
                    ).iter().any(|byte| *byte != 0);
                    if !painted {
                        // Retain editors that implement PrintWindow but ignore WM_PRINT.
                        std::ptr::write_bytes(bitmap.bits, 0, width as usize * height as usize * 4);
                        let printed = {
                            let _saved = SavedDc::new(bitmap.memory_dc)?;
                            PrintWindow(hwnd.0, bitmap.memory_dc.0, 2) != 0
                        };
                        if printed {
                            print_children(hwnd, bitmap.memory_dc, popup_origin, popup_clip, 0, &mut popup_budget)?;
                        }
                    }
                    let _ = GdiFlush();
                    let pixels = std::slice::from_raw_parts(
                        bitmap.bits.cast::<u8>(),
                        width as usize * height as usize * 4,
                    );
                    let mut rgba = Vec::with_capacity(pixels.len());
                    for pixel in pixels.as_chunks::<4>().0 {
                        let a = if layered {
                            let color = u32::from(pixel[2])
                                | (u32::from(pixel[1]) << 8)
                                | (u32::from(pixel[0]) << 16);
                            if attributes & LWA_COLORKEY == LWA_COLORKEY && color == color_key.0 {
                                0
                            } else if attributes & LWA_ALPHA == LWA_ALPHA {
                                opacity
                            } else {
                                255
                            }
                        } else {
                            // BI_RGB alpha is undefined for opaque popup HWNDs,
                            // even when a plugin happens to write some alpha bytes.
                            255
                        };
                        rgba.extend_from_slice(&[
                            pixel[2],
                            pixel[1],
                            pixel[0],
                            a,
                        ]);
                    }
                    self.popups.push((hwnd, bitmap));
                    rgba
                };
                frame.popups.push(EditorPopup {
                    origin: [
                        f64::from(logical_origin.x - origin.x),
                        f64::from(logical_origin.y - origin.y),
                    ],
                    width,
                    height,
                    native_size: (width, height),
                    rgba,
                    interactive: self.input.owner().is_some()
                        || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 == 0,
                });
            }
            Ok(frame)
        }
    }

    pub fn pointer(&mut self, event: EditorPointer) -> Result<()> {
        self.check_window()?;
        event.validate()?;
        self.input.refresh()?;
        self.modifiers = event.modifiers;
        let _keyboard = KeyboardModifiers::new(event.modifiers)?;
        if matches!(event.kind, PointerKind::Cancel) {
            self.drag_target = None;
            self.input.cancel();
            return Ok(());
        }
        ensure!(
            event.position.iter().all(|n| n.is_finite()),
            "invalid editor pointer coordinates"
        );
        ensure!(
            event.buttons & !7 == 0,
            "unsupported editor pointer button mask"
        );
        let mut point = POINT {
            x: event.position[0].round() as i32,
            y: event.position[1].round() as i32,
        };
        unsafe {
            let mut root_origin = POINT::default();
            ensure!(
                ClientToScreen(self.hwnd, &mut root_origin).as_bool(),
                "locate editor pointer origin"
            );
            let popups = self.input.popups()?;
            let popup_target = popups.iter().rev().find_map(|(hwnd, origin)| {
                let mut bounds = RECT::default();
                if GetWindowRect(*hwnd, &mut bounds).is_err()
                    || GetWindowLongW(*hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 != 0
                {
                    return None;
                }
                let local = POINT {
                    x: root_origin.x + point.x - origin.x,
                    y: root_origin.y + point.y - origin.y,
                };
                (local.x >= 0
                    && local.y >= 0
                    && local.x < bounds.right - bounds.left
                    && local.y < bounds.bottom - bounds.top)
                    .then_some(*hwnd)
            });
            let target = self
                .input.owner().or(self.drag_target)
                .filter(|target| {
                    IsWindow(*target).as_bool()
                        && self.input.contains(*target)
                })
                .unwrap_or_else(|| {
                    let mut target = popup_target.unwrap_or(self.hwnd);
                    let mut local = point;
                    if let Some((_, origin)) = popups.iter().find(|(hwnd, _)| *hwnd == target) {
                        local = POINT {
                            x: root_origin.x + point.x - origin.x,
                            y: root_origin.y + point.y - origin.y,
                        };
                    }
                    for _ in 0..32 {
                        let child = ChildWindowFromPointEx(
                            target,
                            local,
                            CWP_SKIPDISABLED | CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT,
                        );
                        if child.0.is_null() || child == target {
                            break;
                        }
                        let _ = MapWindowPoints(target, child, std::slice::from_mut(&mut local));
                        target = child;
                    }
                    target
                });
            if let Some((popup, origin)) = popups
                .iter()
                .find(|(hwnd, _)| *hwnd == GetAncestor(target, GA_ROOT))
            {
                let mut offset = POINT::default();
                let _ = MapWindowPoints(target, *popup, std::slice::from_mut(&mut offset));
                let mut popup_client = POINT::default();
                let mut popup_bounds = RECT::default();
                ensure!(ClientToScreen(*popup, &mut popup_client).as_bool(), "locate popup client");
                GetWindowRect(*popup, &mut popup_bounds).context("locate popup frame")?;
                point = POINT {
                    x: root_origin.x + point.x - origin.x - offset.x - (popup_client.x - popup_bounds.left),
                    y: root_origin.y + point.y - origin.y - offset.y - (popup_client.y - popup_bounds.top),
                };
            } else {
                let _ = MapWindowPoints(self.hwnd, target, std::slice::from_mut(&mut point));
            }
            let mut cursor_point = point;
            ensure!(
                ClientToScreen(target, &mut cursor_point).as_bool(),
                "locate editor pointer cursor"
            );
            self.input.set_cursor(self.input.cursor_position(target, cursor_point));
            let _input = self.input.enter();
            let mut flags = 0u16;
            if event.buttons & 1 != 0 {
                flags |= 1;
            }
            if event.buttons & 2 != 0 {
                flags |= 2;
            }
            if event.buttons & 4 != 0 {
                flags |= 0x10;
            }
            if event.modifiers & super::MOD_SHIFT != 0 { flags |= 0x4; }
            if event.modifiers & super::MOD_CONTROL != 0 { flags |= 0x8; }
            let coordinates =
                |p: POINT| LPARAM((u32::from(p.x as u16) | (u32::from(p.y as u16) << 16)) as isize);
            let (message, value, position) = match event.kind {
                PointerKind::Cancel => {
                    unreachable!("cancellation is handled before entering native ownership")
                }
                PointerKind::Move => (WM_MOUSEMOVE, WPARAM(flags as usize), coordinates(point)),
                PointerKind::Down(button) => {
                    // Win32 replaces the second down with DBLCLK for opted-in classes.
                    let double = event.click_count.is_multiple_of(2)
                        && GetClassLongW(target, GCL_STYLE) & CS_DBLCLKS.0 != 0;
                    let message = match (button, double) {
                        (1, false) => WM_LBUTTONDOWN,
                        (1, true) => WM_LBUTTONDBLCLK,
                        (2, false) => WM_RBUTTONDOWN,
                        (2, true) => WM_RBUTTONDBLCLK,
                        (3, false) => WM_MBUTTONDOWN,
                        (3, true) => WM_MBUTTONDBLCLK,
                        _ => bail!("unsupported editor pointer button"),
                    };
                    self.drag_target = Some(target);
                    (message, WPARAM(flags as usize), coordinates(point))
                }
                PointerKind::Up(button) => {
                    let message = match button {
                        1 => WM_LBUTTONUP,
                        2 => WM_RBUTTONUP,
                        3 => WM_MBUTTONUP,
                        _ => bail!("unsupported editor pointer button"),
                    };
                    (message, WPARAM(flags as usize), coordinates(point))
                }
                PointerKind::Wheel { dx, dy } => {
                    ensure!(
                        dx.is_finite() && dy.is_finite(),
                        "invalid editor wheel displacement"
                    );
                    ensure!(
                        ClientToScreen(target, &mut point).as_bool(),
                        "locate editor wheel target"
                    );
                    let (message, delta) = if dy.abs() >= dx.abs() {
                        (WM_MOUSEWHEEL, dy)
                    } else {
                        (WM_MOUSEHWHEEL, dx)
                    };
                    let delta = delta.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    (
                        message,
                        WPARAM(flags as usize | ((delta as u16 as usize) << 16)),
                        coordinates(point),
                    )
                }
            };
            SendMessageW(target, message, value, position);
            // CaptureInput restores physical graph ownership without notifying the
            // plugin of a capture loss that did not end its forwarded interaction.
            if matches!(event.kind, PointerKind::Up(_)) && event.buttons == 0 {
                self.drag_target = None;
            }
        }
        Ok(())
    }
}

/// Win32 plugins often use GetKeyState rather than mouse-message modifier bits.
/// This changes only the source UI thread's keyboard table while forwarding input;
/// it neither injects physical keys nor changes another window's keyboard state.
struct KeyboardModifiers([u8; 256]);

impl KeyboardModifiers {
    fn new(modifiers: u8) -> Result<Self> {
        let mut saved = [0; 256];
        unsafe { GetKeyboardState(&mut saved) }.context("read editor keyboard state")?;
        let mut keys = saved;
        for (bit, codes) in [
            (super::MOD_SHIFT, [VK_SHIFT, VK_LSHIFT, VK_RSHIFT]),
            (super::MOD_CONTROL, [VK_CONTROL, VK_LCONTROL, VK_RCONTROL]),
            (super::MOD_ALT, [VK_MENU, VK_LMENU, VK_RMENU]),
            (super::MOD_SUPER, [VK_LWIN, VK_LWIN, VK_RWIN]),
        ] {
            for (index, code) in codes.into_iter().enumerate() {
                let pressed = modifiers & bit != 0 && index != 2;
                keys[code.0 as usize] = (keys[code.0 as usize] & 0x7f) | if pressed { 0x80 } else { 0 };
            }
        }
        unsafe { SetKeyboardState(&keys) }.context("set forwarded editor modifiers")?;
        Ok(Self(saved))
    }
}

impl Drop for KeyboardModifiers {
    fn drop(&mut self) {
        let _ = unsafe { SetKeyboardState(&self.0) };
    }
}

unsafe fn print_children(
    hwnd: HWND,
    dc: HDC,
    root_origin: POINT,
    ancestor_clip: RECT,
    depth: usize,
    budget: &mut usize,
) -> Result<()> {
    ensure!(
        depth <= 32 && *budget != 0,
        "captured editor native hierarchy exceeds limits"
    );
    unsafe {
        if let Ok(first) = GetWindow(hwnd, GW_CHILD) {
            let mut child = GetWindow(first, GW_HWNDLAST).unwrap_or(first);
            for _ in 0..512 {
                let previous = GetWindow(child, GW_HWNDPREV).ok();
                print_tree(
                    child,
                    dc,
                    root_origin,
                    ancestor_clip,
                    depth + 1,
                    budget,
                    false,
                )?;
                let Some(previous) = previous else { break };
                child = previous;
            }
        }
    }
    Ok(())
}

unsafe fn print_tree(
    hwnd: HWND,
    dc: HDC,
    root_origin: POINT,
    ancestor_clip: RECT,
    depth: usize,
    budget: &mut usize,
    root: bool,
) -> Result<()> {
    ensure!(
        depth <= 32 && *budget != 0,
        "captured editor native hierarchy exceeds limits"
    );
    *budget -= 1;
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return Ok(());
        }
        let mut bounds = RECT::default();
        GetWindowRect(hwnd, &mut bounds).context("read native editor child bounds")?;
        if root {
            bounds.left = root_origin.x;
            bounds.top = root_origin.y;
        }
        let (x, y) = (bounds.left - root_origin.x, bounds.top - root_origin.y);
        {
            let _saved = SavedDc::new(dc)?;
            IntersectClipRect(
                dc,
                ancestor_clip.left,
                ancestor_clip.top,
                ancestor_clip.right,
                ancestor_clip.bottom,
            );
            let _ = SetViewportOrgEx(dc, x, y, None);
            // PRF_CHILDREN is deliberately absent: only this traversal paints children.
            SendMessageW(
                hwnd,
                WM_PRINT,
                WPARAM(dc.0 as usize),
                LPARAM(
                    (PRF_CLIENT | PRF_ERASEBKGND | if root { 0 } else { PRF_NONCLIENT }) as isize,
                ),
            );
        }
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client).context("read native editor child client bounds")?;
        let mut points = [
            POINT {
                x: client.left,
                y: client.top,
            },
            POINT {
                x: client.right,
                y: client.bottom,
            },
        ];
        let _ = MapWindowPoints(hwnd, HWND::default(), &mut points);
        let clip = RECT {
            left: ancestor_clip.left.max(points[0].x - root_origin.x),
            top: ancestor_clip.top.max(points[0].y - root_origin.y),
            right: ancestor_clip.right.min(points[1].x - root_origin.x),
            bottom: ancestor_clip.bottom.min(points[1].y - root_origin.y),
        };
        if clip.left >= clip.right || clip.top >= clip.bottom {
            return Ok(());
        }
        print_children(hwnd, dc, root_origin, clip, depth, budget)?;
    }
    Ok(())
}

/// Restores clip, viewport origin, and the rest of a plugin-modified capture DC on errors.
struct SavedDc(HDC, i32);

impl SavedDc {
    unsafe fn new(dc: HDC) -> Result<Self> {
        let saved = unsafe { SaveDC(dc) };
        ensure!(saved != 0, "save editor capture DC");
        Ok(Self(dc, saved))
    }
}

impl Drop for SavedDc {
    fn drop(&mut self) {
        unsafe {
            let _ = RestoreDC(self.0, self.1);
        }
    }
}

/// Scoped GDI storage. Restores the selected bitmap before deleting it and releases only
/// the DC borrowed from the source HWND; dropping this never destroys the source window.
struct Bitmap {
    hwnd: HWND,
    size: (u32, u32),
    window_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut c_void,
}

impl Bitmap {
    fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let mut bitmap = Self {
            hwnd,
            size: (width, height),
            window_dc: HDC::default(),
            memory_dc: HDC::default(),
            bitmap: HBITMAP::default(),
            original: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
        };
        unsafe {
            bitmap.window_dc = GetDC(hwnd);
            ensure!(
                !bitmap.window_dc.is_invalid(),
                "GetDC for editor capture failed"
            );
            bitmap.memory_dc = CreateCompatibleDC(bitmap.window_dc);
            ensure!(
                !bitmap.memory_dc.is_invalid(),
                "CreateCompatibleDC for editor capture failed"
            );
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            bitmap.bitmap = CreateDIBSection(
                bitmap.memory_dc,
                &info,
                DIB_RGB_COLORS,
                &mut bitmap.bits,
                None,
                0,
            )
            .context("allocate editor capture bitmap")?;
            ensure!(
                !bitmap.bits.is_null(),
                "editor capture bitmap has no pixel storage"
            );
            bitmap.original = SelectObject(bitmap.memory_dc, bitmap.bitmap);
            ensure!(
                !bitmap.original.is_invalid(),
                "select editor capture bitmap"
            );
        }
        Ok(bitmap)
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            if !self.original.is_invalid() && !self.memory_dc.is_invalid() {
                SelectObject(self.memory_dc, self.original);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.memory_dc.is_invalid() {
                let _ = DeleteDC(self.memory_dc);
            }
            if !self.window_dc.is_invalid() {
                ReleaseDC(self.hwnd, self.window_dc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{COLORREF, HINSTANCE, LRESULT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetCapture, ReleaseCapture, SetCapture, SetFocus,
    };
    use windows::core::{PCWSTR, w};

    const CLASS: PCWSTR = w!("nodeplayer-captured-editor-adapter-test");

    struct State {
        color: [u8; 3],
        mixed_alpha: Cell<bool>,
        timer_ticks: Cell<u32>,
        down: Cell<bool>,
        clicks: Cell<u32>,
        double_clicks: Cell<u32>,
        last_move: Cell<Option<(i32, i32)>>,
        last_up: Cell<Option<(i32, i32)>>,
        paints: Cell<u32>,
        capture_changes: Cell<u32>,
        cancels: Cell<u32>,
        last_cancel: Cell<u32>,
        desktop_moves: Cell<u32>,
        cursor: Cell<Option<(i32, i32)>>,
        physical_cursor: Cell<Option<(i32, i32)>>,
        message_cursor: Cell<Option<(i32, i32)>>,
        requested_cursor: Cell<Option<(i32, i32)>>,
    }

    struct TestWindow {
        hwnd: HWND,
        // The allocation stays alive through the window's final messages.
        state: Box<State>,
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    unsafe fn paint(hwnd: HWND, dc: HDC, state: &State) {
        unsafe {
            state.paints.set(state.paints.get() + 1);
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_err() {
                return;
            }
            let [r, g, b] = if state.clicks.get() == 0 {
                let [r, g, b] = state.color;
                [r.wrapping_add((state.timer_ticks.get() % 64) as u8), g, b]
            } else {
                [37, 171, 116]
            };
            let brush = CreateSolidBrush(COLORREF(
                u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16),
            ));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush);
            if state.mixed_alpha.get() {
                if let Ok(bitmap) = Bitmap::new(hwnd, 2, 1) {
                    let pixels = [b, g, r, 0, b, g, r, 128];
                    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bitmap.bits.cast::<u8>(), pixels.len());
                    let _ = BitBlt(dc, 0, 0, 2, 1, bitmap.memory_dc, 0, 0, SRCCOPY);
                }
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        value: WPARAM,
        position: LPARAM,
    ) -> LRESULT {
        unsafe {
            if let Some(state) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() {
                let point = (position.0 as i16 as i32, (position.0 >> 16) as i16 as i32);
                match message {
                    WM_APP => {
                        let callback = &*(position.0 as *const &dyn Fn());
                        callback();
                        return LRESULT(0);
                    }
                    WM_PAINT => {
                        let mut paint_info = PAINTSTRUCT::default();
                        let dc = BeginPaint(hwnd, &mut paint_info);
                        paint(hwnd, dc, state);
                        let _ = EndPaint(hwnd, &paint_info);
                        return LRESULT(0);
                    }
                    WM_PRINTCLIENT => {
                        paint(hwnd, HDC(value.0 as *mut c_void), state);
                        return LRESULT(0);
                    }
                    WM_ERASEBKGND => return LRESULT(1),
                    WM_TIMER => {
                        state.timer_ticks.set(state.timer_ticks.get() + 1);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                        let _ = SetFocus(hwnd);
                        SetCapture(hwnd);
                        state.down.set(true);
                        if message == WM_LBUTTONDBLCLK {
                            state.double_clicks.set(state.double_clicks.get() + 1);
                        }
                        return LRESULT(0);
                    }
                    WM_MOUSEMOVE => {
                        if let Some((x, y)) = state.requested_cursor.get() {
                            let _ = SetCursorPos(x, y);
                        }
                        let mut cursor = POINT::default();
                        if GetCursorPos(&mut cursor).is_ok() {
                            state.cursor.set(Some((cursor.x, cursor.y)));
                        }
                        if GetPhysicalCursorPos(&mut cursor).is_ok() {
                            state.physical_cursor.set(Some((cursor.x, cursor.y)));
                        }
                        let message = GetMessagePos();
                        state.message_cursor.set(Some((message as i16 as i32, (message >> 16) as i16 as i32)));
                        state.last_move.set(Some(point));
                        return LRESULT(0);
                    }
                    0x02a3 => {
                        // A native control can ignore a queued leave during a captured
                        // drag, while treating an ordinary leave as an outside move.
                        if GetCapture() != hwnd {
                            state.last_move.set(Some((-1, -1)));
                        }
                        return LRESULT(0);
                    }
                    WM_LBUTTONUP => {
                        if state.down.replace(false) {
                            state.clicks.set(state.clicks.get() + 1);
                        }
                        state.last_up.set(Some(point));
                        if GetCapture() == hwnd {
                            let _ = ReleaseCapture();
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    WM_CAPTURECHANGED => {
                        state.capture_changes.set(state.capture_changes.get() + 1);
                        state.down.set(false);
                        return LRESULT(0);
                    }
                    WM_CANCELMODE | WM_KILLFOCUS => {
                        state.cancels.set(state.cancels.get() + 1);
                        state.last_cancel.set(message);
                        state.down.set(false);
                        return LRESULT(0);
                    }
                    WM_WINDOWPOSCHANGED => {
                        let _physical = crate::plugins::window::physical_coordinates();
                        let mut bounds = RECT::default();
                        if GetWindowRect(hwnd, &mut bounds).is_ok()
                            && bounds.right > GetSystemMetrics(SM_XVIRTUALSCREEN)
                            && bounds.bottom > GetSystemMetrics(SM_YVIRTUALSCREEN)
                            && bounds.left < GetSystemMetrics(SM_XVIRTUALSCREEN)
                                + GetSystemMetrics(SM_CXVIRTUALSCREEN)
                            && bounds.top < GetSystemMetrics(SM_YVIRTUALSCREEN)
                                + GetSystemMetrics(SM_CYVIRTUALSCREEN)
                        {
                            state.desktop_moves.set(state.desktop_moves.get() + 1);
                        }
                    }
                    _ => {}
                }
            }
            DefWindowProcW(hwnd, message, value, position)
        }
    }

    fn fixture(
        parent: Option<HWND>,
        position: (i32, i32),
        size: (i32, i32),
        color: [u8; 3],
    ) -> Result<TestWindow> {
        static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
        let registered = REGISTERED.get_or_init(|| unsafe {
            let module = GetModuleHandleW(None).map_err(|error| error.to_string())?;
            let class = WNDCLASSW {
                style: CS_DBLCLKS,
                hInstance: HINSTANCE(module.0),
                lpfnWndProc: Some(window_proc),
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_win32().to_string());
            }
            Ok(())
        });
        registered
            .as_ref()
            .map_err(|error| anyhow::anyhow!("register adapter-test window: {error}"))?;
        let state = Box::new(State {
            color,
            mixed_alpha: Cell::new(false),
            timer_ticks: Cell::new(0),
            down: Cell::new(false),
            clicks: Cell::new(0),
            double_clicks: Cell::new(0),
            last_move: Cell::new(None),
            last_up: Cell::new(None),
            paints: Cell::new(0),
            capture_changes: Cell::new(0),
            cancels: Cell::new(0),
            last_cancel: Cell::new(0),
            desktop_moves: Cell::new(0),
            cursor: Cell::new(None),
            physical_cursor: Cell::new(None),
            message_cursor: Cell::new(None),
            requested_cursor: Cell::new(None),
        });
        unsafe {
            let style =
                WS_CLIPCHILDREN | WS_VISIBLE | if parent.is_some() { WS_CHILD } else { WS_POPUP };
            let hwnd = CreateWindowExW(
                if parent.is_none() { WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW } else { WINDOW_EX_STYLE::default() },
                CLASS,
                w!("captured editor adapter test"),
                style,
                position.0,
                position.1,
                size.0,
                size.1,
                parent.unwrap_or_default(),
                None,
                HINSTANCE(GetModuleHandleW(None)?.0),
                None,
            )?;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize);
            let _ = InvalidateRect(hwnd, None, false);
            let _ = UpdateWindow(hwnd);
            let window = TestWindow { hwnd, state };
            if !IsWindowVisible(hwnd).as_bool() { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
            ensure!(IsWindowVisible(hwnd).as_bool(), "native fixture did not map");
            Ok(window)
        }
    }

    fn pixel(frame: &EditorFrame, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * frame.width as usize + x as usize) * 4;
        frame.rgba[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn rejects_invalid_borrowed_handle() {
        assert!(NativeCapture::new(0).is_err());
    }

    #[test]
    fn forwarded_modifiers_restore_the_ui_thread_keyboard_table() -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
        let mut before = [0; 256];
        unsafe { GetKeyboardState(&mut before) }?;
        {
            let _modifiers = KeyboardModifiers::new(super::super::MOD_SHIFT | super::super::MOD_CONTROL)?;
            assert!(unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0);
            assert!(unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0);
            assert!(unsafe { GetKeyState(i32::from(VK_MENU.0)) } >= 0);
        }
        let mut after = [0; 256];
        unsafe { GetKeyboardState(&mut after) }?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn forwarded_cursor_queries_and_warps_leave_the_desktop_cursor_unchanged() -> Result<()> {
        use crate::plugins::{Placement, window::HostWindow};

        let host = HostWindow::create(Placement::CaptureSource, 640, 360, "cursor regression", false)?;
        let root = HWND(host.handle() as *mut c_void);
        let child = fixture(Some(root), (20, 24), (240, 180), [38, 139, 188])?;
        let mut capture = NativeCapture::new(host.handle())?;
        let mut physical = POINT::default();
        unsafe { GetCursorPos(&mut physical) }?;
        let foreground = unsafe { GetForegroundWindow() };
        let mut origin = POINT::default();
        ensure!(unsafe { ClientToScreen(root, &mut origin) }.as_bool(), "read source origin");

        let pointer = EditorPointer {
            timestamp_ms: None, modifiers: 0, click_count: 0,
            position: [40.0, 40.0], kind: PointerKind::Move, buttons: 0,
        };
        capture.pointer(pointer)?;
        let expected = (origin.x + 40, origin.y + 40);
        assert_eq!(child.state.cursor.get(), Some(expected));
        assert_eq!(child.state.physical_cursor.get(), Some(expected));
        assert_eq!(child.state.message_cursor.get(), Some(expected));

        let warped = (expected.0 + 7, expected.1 - 9);
        child.state.requested_cursor.set(Some(warped));
        capture.pointer(pointer)?;
        assert_eq!(child.state.cursor.get(), Some(warped));
        assert_eq!(child.state.physical_cursor.get(), Some(warped));
        assert_eq!(child.state.message_cursor.get(), Some(expected), "a cursor warp does not rewrite the input message");

        let mut after = POINT::default();
        unsafe { GetCursorPos(&mut after) }?;
        assert_eq!((after.x, after.y), (physical.x, physical.y));
        // Timer/paint callbacks retain the last virtual position, but other threads
        // and the host's code outside a captured callback still read the real cursor.
        {
            let _scope = capture.input.enter();
            assert_eq!(unsafe { GetForegroundWindow() }, root);
            unsafe { GetCursorPos(&mut after) }?;
            assert_eq!((after.x, after.y), warped);
            let other = std::thread::spawn(|| -> Result<((i32, i32), usize)> {
                let mut point = POINT::default();
                unsafe { GetCursorPos(&mut point) }?;
                Ok(((point.x, point.y), unsafe { GetForegroundWindow() }.0 as usize))
            }).join().expect("cursor-query thread panicked")?;
            assert_eq!(other, ((physical.x, physical.y), foreground.0 as usize));
        }
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);

        capture.pointer(EditorPointer { kind: PointerKind::Cancel, ..pointer })?;
        {
            let _scope = capture.input.enter();
            assert_eq!(unsafe { GetForegroundWindow() }, foreground);
            unsafe { GetCursorPos(&mut after) }?;
            assert_eq!((after.x, after.y), (physical.x, physical.y));
        }
        capture.pointer(pointer)?;
        {
            let _scope = capture.input.enter();
            assert_eq!(unsafe { GetForegroundWindow() }, root);
            unsafe { GetCursorPos(&mut after) }?;
            assert_eq!((after.x, after.y), warped);
        }

        host.resize(960, 510);
        let mut resized_origin = POINT::default();
        ensure!(unsafe { ClientToScreen(root, &mut resized_origin) }.as_bool(), "read resized source origin");
        assert_eq!((resized_origin.x, resized_origin.y), (origin.x, origin.y));
        let frame = capture.capture((640, 360))?;
        assert_eq!((frame.width, frame.height), (960, 510));
        assert_eq!(frame.native_size, (960, 510));
        Ok(())
    }

    #[test]
    #[ignore = "requires a Windows window station"]
    fn worker_helper_callbacks_keep_the_editor_context_and_capture_opaque_popups() -> Result<()> {
        use crate::plugins::window::dispatch_worker_message;
        use std::cell::RefCell;

        // Like JUCE's message window, this helper exists before capture attaches
        // and has no owner relationship with the editor.
        let helper = fixture(None, (-2400, -2000), (1, 1), [0, 0, 0])?;
        unsafe { let _ = ShowWindow(helper.hwnd, SW_HIDE); }
        let root = fixture(None, (-1400, -1000), (400, 300), [23, 28, 38])?;
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        let foreground = unsafe { GetForegroundWindow() };
        let mut physical = POINT::default();
        unsafe { GetCursorPos(&mut physical) }?;
        capture.input.set_cursor(POINT { x: -1350, y: -960 });
        let observed = RefCell::new(None);
        let callback: &dyn Fn() = &|| {
            let mut position = POINT::default();
            let read = unsafe { GetCursorPos(&mut position) };
            let popup = fixture(None, (-1360, -970), (120, 90), [180, 90, 30])
                .and_then(|popup| {
                    let mut bounds = RECT::default();
                    unsafe { GetWindowRect(popup.hwnd, &mut bounds) }?;
                    assert_eq!((bounds.left, bounds.top, bounds.right, bounds.bottom), (-1360, -970, -1240, -880));
                    let mut point = POINT { x: 17, y: 23 };
                    assert!(unsafe { ClientToScreen(popup.hwnd, &mut point) }.as_bool());
                    assert_eq!((point.x, point.y), (-1343, -947));
                    assert!(unsafe { ScreenToClient(popup.hwnd, &mut point) }.as_bool());
                    assert_eq!((point.x, point.y), (17, 23));
                    let mut mapped = [POINT { x: 17, y: 23 }];
                    unsafe { MapWindowPoints(popup.hwnd, root.hwnd, &mut mapped); }
                    assert_eq!((mapped[0].x, mapped[0].y), (57, 53));
                    assert_eq!(unsafe { WindowFromPoint(POINT { x: -1343, y: -947 }) }, popup.hwnd);
                    let child = fixture(Some(popup.hwnd), (8, 10), (30, 30), [40, 100, 160])?;
                    assert_eq!(unsafe { WindowFromPoint(POINT { x: -1343, y: -947 }) }, child.hwnd);
                    Ok(popup)
                });
            observed.replace(Some((unsafe { GetForegroundWindow() }, position, read, popup)));
        };
        dispatch_worker_message(&MSG {
            hwnd: helper.hwnd,
            message: WM_APP,
            lParam: LPARAM(&callback as *const &dyn Fn() as isize),
            ..Default::default()
        });
        let (active, position, read, popup) = observed.into_inner().expect("helper was not dispatched");
        read?;
        let popup = popup?;
        popup.state.mixed_alpha.set(true);
        assert_eq!(active, root.hwnd);
        assert_eq!((position.x, position.y), (-1350, -960));
        assert!(capture.input.contains(popup.hwnd));
        assert!(!capture.input.contains(helper.hwnd));
        let frame = capture.capture((400, 300))?;
        assert_eq!(frame.popups.len(), 1);
        assert_eq!(frame.popups[0].origin, [40.0, 30.0]);
        assert_eq!(&frame.popups[0].rgba[..8], &[180, 90, 30, 255, 180, 90, 30, 255]);
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        let mut after = POINT::default();
        unsafe { GetCursorPos(&mut after) }?;
        assert_eq!((after.x, after.y), (physical.x, physical.y));
        Ok(())
    }

    #[test]
    #[ignore = "requires a Windows window station"]
    fn monitor_clamped_submenus_keep_logical_geometry_and_pointer_queries() -> Result<()> {
        use crate::plugins::window::dispatch_worker_message;
        use std::cell::RefCell;

        let helper = fixture(None, (-2400, -2000), (1, 1), [0, 0, 0])?;
        unsafe {
            let _ = ShowWindow(helper.hwnd, SW_HIDE);
        }
        let root = fixture(None, (-1400, -1000), (400, 300), [23, 28, 38])?;
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        let windows = RefCell::new(None);
        let create: &dyn Fn() = &|| {
            windows.replace(Some((|| -> Result<_> {
                // JUCE clamps its menu to a monitor even though the editor is offscreen.
                let menu = fixture(None, (10, 20), (120, 90), [180, 90, 30])?;
                let submenu = fixture(None, (130, 30), (100, 120), [40, 100, 160])?;
                let shadow = fixture(None, (130, 30), (110, 130), [0, 0, 0])?;
                unsafe {
                    let style = GetWindowLongW(shadow.hwnd, GWL_EXSTYLE) as u32;
                    SetWindowLongW(
                        shadow.hwnd,
                        GWL_EXSTYLE,
                        (style | WS_EX_TRANSPARENT.0) as i32,
                    );
                }
                Ok((menu, submenu, shadow))
            })()));
        };
        dispatch_worker_message(&MSG {
            hwnd: helper.hwnd,
            message: WM_APP,
            lParam: LPARAM(&create as *const &dyn Fn() as isize),
            ..Default::default()
        });
        let (menu, submenu, _shadow) = windows.into_inner().expect("helper was not dispatched")?;
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            click_count: 0,
            position: [1545.0, 1047.0],
            kind: PointerKind::Move,
            buttons: 0,
        })?;
        assert_eq!(submenu.state.last_move.get(), Some((15, 17)));
        assert_eq!(submenu.state.cursor.get(), Some((145, 47)));
        assert_eq!(submenu.state.message_cursor.get(), Some((145, 47)));

        let verify: &dyn Fn() = &|| unsafe {
            let mut bounds = RECT::default();
            GetWindowRect(submenu.hwnd, &mut bounds).unwrap();
            assert_eq!(
                (bounds.left, bounds.top, bounds.right, bounds.bottom),
                (130, 30, 230, 150)
            );
            let mut point = POINT { x: 15, y: 17 };
            assert!(ClientToScreen(submenu.hwnd, &mut point).as_bool());
            assert_eq!((point.x, point.y), (145, 47));
            assert_eq!(
                WindowFromPoint(point),
                submenu.hwnd,
                "the shadow must not intercept menu hit testing"
            );
            assert_eq!(WindowFromPoint(POINT { x: 25, y: 37 }), menu.hwnd);
            GetCursorPos(&mut point).unwrap();
            assert_eq!((point.x, point.y), (145, 47));
        };
        dispatch_worker_message(&MSG {
            hwnd: helper.hwnd,
            message: WM_APP,
            lParam: LPARAM(&verify as *const &dyn Fn() as isize),
            ..Default::default()
        });
        let mut physical = RECT::default();
        unsafe { GetWindowRect(submenu.hwnd, &mut physical) }?;
        assert!(physical.right < unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) });
        assert!(physical.bottom < unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) });
        let frame = capture.capture((400, 300))?;
        assert!(
            frame
                .popups
                .iter()
                .any(|popup| popup.origin == [1530.0, 1030.0])
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn captures_and_routes_nested_children_without_owning_source() -> Result<()> {
        let root = fixture(None, (-2400, -2000), (720, 440), [23, 28, 38])?;
        let child = fixture(Some(root.hwnd), (32, 28), (320, 220), [38, 139, 188])?;
        let grandchild = fixture(Some(child.hwnd), (40, 30), (100, 80), [135, 76, 175])?;
        let sibling = fixture(Some(root.hwnd), (60, 50), (70, 50), [210, 79, 67])?;
        unsafe {
            SetWindowPos(
                sibling.hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
        }
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        for state in [&root.state, &child.state, &grandchild.state, &sibling.state] {
            state.paints.set(0);
        }
        let before = capture.capture((1, 1))?;
        for state in [&root.state, &child.state, &grandchild.state, &sibling.state] {
            assert!(
                (1..=2).contains(&state.paints.get()),
                "capture may paint a client once through PrintWindow and once for correct stacking"
            );
        }
        assert_eq!(
            before.native_size,
            (720, 440),
            "actual client size overrides a stale hint"
        );
        assert_eq!(pixel(&before, 40, 36), [38, 139, 188, 255]);
        assert_eq!(pixel(&before, 157, 78), [135, 76, 175, 255]);
        assert_eq!(
            pixel(&before, 80, 70),
            [210, 79, 67, 255],
            "upper sibling paints last"
        );

        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            click_count: 1,
            position: [157.0, 78.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        })?;
        assert!(
            grandchild.state.down.get(),
            "host must not cancel a held drag after forwarding down"
        );
        assert_eq!(grandchild.state.capture_changes.get(), 0);
        assert_eq!(grandchild.state.cancels.get(), 0);
        unsafe {
            SendMessageW(grandchild.hwnd, 0x02a3, WPARAM(0), LPARAM(0));
        }
        assert_eq!(
            grandchild.state.last_move.get(),
            None,
            "a queued native leave must observe the retained drag capture"
        );
        assert_eq!(
            unsafe { GetCapture() },
            HWND::default(),
            "private editor must not retain physical mouse capture"
        );
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            click_count: 0,
            position: [-10.0, -20.0],
            kind: PointerKind::Move,
            buttons: 1,
        })?;
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            click_count: 1,
            position: [-10.0, -20.0],
            kind: PointerKind::Up(1),
            buttons: 0,
        })?;
        assert_eq!(grandchild.state.clicks.get(), 1);
        assert_eq!(grandchild.state.last_move.get(), Some((-82, -78)));
        assert_eq!(
            grandchild.state.last_up.get(),
            Some((-82, -78)),
            "drag release stays with its original descendant"
        );
        for kind in [PointerKind::Down(1), PointerKind::Up(1)] {
            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                position: [157.0, 78.0],
                kind,
                buttons: u8::from(matches!(kind, PointerKind::Down(_))),
                click_count: 2,
            })?;
        }
        assert_eq!(grandchild.state.clicks.get(), 2);
        assert_eq!(grandchild.state.double_clicks.get(), 1);
        assert_eq!(child.state.clicks.get(), 0);
        assert_eq!(sibling.state.clicks.get(), 0);
        let after = capture.capture((720, 440))?;
        assert_eq!(pixel(&after, 157, 78), [37, 171, 116, 255]);
        assert_ne!(before.rgba, after.rgba);

        unsafe {
            SetWindowPos(
                root.hwnd,
                None,
                0,
                0,
                680,
                400,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        let resized = capture.capture((720, 440))?;
        assert_eq!(resized.native_size, (680, 400));
        assert_eq!((resized.width, resized.height), (680, 400));
        assert_eq!(pixel(&resized, 157, 78), [37, 171, 116, 255]);
        drop(capture);
        assert!(
            unsafe { IsWindow(root.hwnd) }.as_bool(),
            "capture does not own its borrowed source window"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a Windows window station"]
    fn virtual_capture_preserves_graph_ownership_and_real_cancellation() -> Result<()> {
        let graph = fixture(None, (-2000, -2000), (100, 100), [1, 2, 3])?;
        let root = fixture(None, (-800, -800), (400, 300), [23, 28, 38])?;
        let child = fixture(Some(root.hwnd), (20, 20), (200, 200), [38, 139, 188])?;
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        unsafe {
            let _ = SetFocus(graph.hwnd);
            SetCapture(graph.hwnd);
        }
        graph.state.cancels.set(0);
        graph.state.capture_changes.set(0);
        for _ in 0..3 {
            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                position: [40.0, 40.0],
                kind: PointerKind::Down(1),
                buttons: 1,
                click_count: 1,
            })?;
            assert!(child.state.down.get());
            assert_eq!(unsafe { GetCapture() }, graph.hwnd);
            assert_eq!(child.state.capture_changes.get(), 0);
            // A popup can retain native capture after its opening button is released.
            // Selecting a row may call SetCapture again on that same logical owner.
            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                position: [40.0, 40.0],
                kind: PointerKind::Down(1),
                buttons: 1,
                click_count: 1,
            })?;
            assert_eq!(
                child.state.capture_changes.get(),
                0,
                "reacquiring the same logical owner must not cancel a popup"
            );
            for x in [30.0, 50.0, -300.0, 600.0] {
                capture.pointer(EditorPointer {
                    timestamp_ms: None,
                    modifiers: 0,
                    position: [x, -50.0],
                    kind: PointerKind::Move,
                    buttons: 1,
                    click_count: 0,
                })?;
                assert!(child.state.down.get());
                assert_eq!(unsafe { GetCapture() }, graph.hwnd);
            }
            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                position: [600.0, -50.0],
                kind: PointerKind::Cancel,
                buttons: 0,
                click_count: 0,
            })?;
            assert!(!child.state.down.get());
            assert!(child.state.cancels.get() > 0);
            assert_eq!(unsafe { GetCapture() }, graph.hwnd);
            child.state.capture_changes.set(0);
        }
        assert_eq!(
            graph.state.capture_changes.get(),
            0,
            "virtual editor capture must not cancel physical graph capture"
        );
        assert_eq!(
            graph.state.cancels.get(),
            0,
            "virtual editor focus must not steal graph focus (last cancellation message={})",
            graph.state.last_cancel.get()
        );
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            position: [40.0, 40.0],
            kind: PointerKind::Down(1),
            buttons: 1,
            click_count: 1,
        })?;
        unsafe {
            SendMessageW(
                child.hwnd,
                WM_CAPTURECHANGED,
                WPARAM(0),
                LPARAM(graph.hwnd.0 as isize),
            );
        }
        assert!(
            !child.state.down.get(),
            "real native capture loss must reach the plugin"
        );
        assert_eq!(child.state.capture_changes.get(), 1);
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            position: [40.0, 40.0],
            kind: PointerKind::Down(1),
            buttons: 1,
            click_count: 1,
        })?;
        unsafe {
            SendMessageW(
                child.hwnd,
                WM_KILLFOCUS,
                WPARAM(graph.hwnd.0 as usize),
                LPARAM(0),
            );
        }
        assert!(
            !child.state.down.get(),
            "real native focus loss must reach the plugin"
        );
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            position: [40.0, 40.0],
            kind: PointerKind::Down(1),
            buttons: 1,
            click_count: 1,
        })?;
        drop(capture);
        assert!(
            !child.state.down.get(),
            "dropping capture cancels its retained interaction"
        );
        unsafe {
            if GetCapture() == graph.hwnd {
                let _ = ReleaseCapture();
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a Windows window station"]
    fn captures_unowned_popups_only_for_the_callback_that_created_them() -> Result<()> {
        let foreground = unsafe { GetForegroundWindow() };
        let root = fixture(None, (-1400, -1000), (400, 300), [23, 28, 38])?;
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        let second = fixture(None, (-2400, -1800), (400, 300), [30, 40, 50])?;
        let mut second_capture = NativeCapture::new(second.hwnd.0 as usize)?;
        let unrelated = fixture(None, (-2000, -1500), (100, 80), [70, 80, 90])?;
        let (popup, nested) = {
            let _scope = capture.input.enter();
            let nested = {
                let _nested = second_capture.input.enter();
                fixture(None, (-2380, -1770), (120, 90), [50, 100, 150])?
            };
            let popup = fixture(None, (-1370, -960), (160, 120), [180, 90, 30])?;
            (popup, nested)
        };
        unsafe {
            assert!(GetWindow(popup.hwnd, GW_OWNER).is_err());
            assert!(GetWindow(nested.hwnd, GW_OWNER).is_err());
            let mut bounds = RECT::default();
            GetWindowRect(popup.hwnd, &mut bounds)?;
            assert!(bounds.right < GetSystemMetrics(SM_XVIRTUALSCREEN));
            assert!(bounds.bottom < GetSystemMetrics(SM_YVIRTUALSCREEN));
        }
        assert!(capture.input.contains(popup.hwnd));
        assert!(!capture.input.contains(nested.hwnd));
        assert!(!capture.input.contains(unrelated.hwnd));
        let frame = capture.capture((400, 300))?;
        assert_eq!(frame.popups.len(), 1);
        assert_eq!(frame.popups[0].origin, [30.0, 40.0]);
        assert_eq!(&frame.popups[0].rgba[..4], &[180, 90, 30, 255]);
        let second_frame = second_capture.capture((400, 300))?;
        assert_eq!(second_frame.popups.len(), 1);
        assert_eq!(second_frame.popups[0].origin, [20.0, 30.0]);
        assert_eq!(&second_frame.popups[0].rgba[..4], &[50, 100, 150, 255]);
        for kind in [PointerKind::Down(1), PointerKind::Up(1)] {
            capture.pointer(EditorPointer {
                timestamp_ms: None, modifiers: 0, click_count: 1,
                position: [50.0, 60.0], kind,
                buttons: u8::from(matches!(kind, PointerKind::Down(_))),
            })?;
        }
        assert_eq!(popup.state.clicks.get(), 1);
        assert_eq!(popup.state.last_up.get(), Some((20, 20)));
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        drop(popup);
        assert!(capture.capture((400, 300))?.popups.is_empty());
        assert_eq!(second_capture.capture((400, 300))?.popups.len(), 1);
        Ok(())
    }

    #[test]
    #[ignore = "requires a Windows window station"]
    fn captures_owned_popup_without_exposing_it_on_the_desktop() -> Result<()> {
        let foreground = unsafe { GetForegroundWindow() };
        let root = fixture(None, (-1400, -1000), (400, 300), [23, 28, 38])?;
        let mut capture = NativeCapture::new(root.hwnd.0 as usize)?;
        let second = fixture(None, (-2400, -1800), (400, 300), [30, 40, 50])?;
        let _second_capture = NativeCapture::new(second.hwnd.0 as usize)?;
        let mut state = Box::new(State {
            color: [180, 90, 30],
            mixed_alpha: Cell::new(false),
            timer_ticks: Cell::new(0),
            down: Cell::new(false),
            clicks: Cell::new(0),
            double_clicks: Cell::new(0),
            last_move: Cell::new(None),
            last_up: Cell::new(None),
            paints: Cell::new(0),
            capture_changes: Cell::new(0),
            cancels: Cell::new(0),
            last_cancel: Cell::new(0),
            desktop_moves: Cell::new(0),
            cursor: Cell::new(None),
            physical_cursor: Cell::new(None),
            message_cursor: Cell::new(None),
            requested_cursor: Cell::new(None),
        });
        let popup = unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_APPWINDOW,
                CLASS,
                w!("owned captured popup"),
                WS_POPUP,
                -1370,
                -960,
                160,
                120,
                root.hwnd,
                None,
                HINSTANCE(GetModuleHandleW(None)?.0),
                None,
            )?;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *state as *mut State as isize);
            let _ = ShowWindow(hwnd, SW_SHOW);
            TestWindow { hwnd, state }
        };
        let frame = capture.capture((400, 300))?;
        assert_eq!(frame.popups.len(), 1);
        let image = &frame.popups[0];
        assert_eq!(image.origin, [30.0, 40.0]);
        assert_eq!(image.native_size, (160, 120));
        assert_eq!(&image.rgba[..4], &[180, 90, 30, 255]);
        unsafe {
            let style = GetWindowLongW(popup.hwnd, GWL_EXSTYLE) as u32;
            assert_eq!(
                style & (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_APPWINDOW).0,
                (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0,
                "captured popup must not activate or enter task switching"
            );
            assert_eq!(GetForegroundWindow(), foreground);
            assert_eq!(
                SendMessageW(popup.hwnd, WM_MOUSEACTIVATE, WPARAM(0), LPARAM(0)),
                LRESULT(MA_NOACTIVATE as isize)
            );
            let mut bounds = RECT::default();
            GetWindowRect(popup.hwnd, &mut bounds)?;
            assert!(
                bounds.right < GetSystemMetrics(SM_XVIRTUALSCREEN)
                    && bounds.bottom < GetSystemMetrics(SM_YVIRTUALSCREEN)
            );
        }
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            position: [50.0, 60.0],
            kind: PointerKind::Down(1),
            buttons: 1,
            click_count: 1,
        })?;
        assert!(popup.state.down.get());
        capture.pointer(EditorPointer {
            timestamp_ms: None,
            modifiers: 0,
            position: [50.0, 60.0],
            kind: PointerKind::Up(1),
            buttons: 0,
            click_count: 1,
        })?;
        assert_eq!(popup.state.clicks.get(), 1);
        assert_eq!(popup.state.last_up.get(), Some((20, 20)));

        // Layered menus upload their own backing rather than painting WM_PRINT.
        // Both APIs can move directly onto the desktop, even when the ordinary
        // WINDOWPOS hook parks every SetWindowPos call.
        unsafe {
            SetWindowLongW(
                popup.hwnd,
                GWL_EXSTYLE,
                (GetWindowLongW(popup.hwnd, GWL_EXSTYLE) as u32 | WS_EX_LAYERED.0) as i32,
            );
            let bitmap = Bitmap::new(popup.hwnd, 160, 120)?;
            let pixels = std::slice::from_raw_parts_mut(bitmap.bits.cast::<u8>(), 160 * 120 * 4);
            for pixel in pixels.as_chunks_mut::<4>().0 {
                *pixel = [16, 32, 64, 128];
            }
            pixels[..4].fill(0);
            let size = windows::Win32::Foundation::SIZE { cx: 160, cy: 120 };
            let source = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 128,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            for (indirect, destination) in [
                (false, POINT { x: 100, y: 200 }),
                (true, POINT { x: 140, y: 220 }),
            ] {
                if indirect {
                    let update = UPDATELAYEREDWINDOWINFO {
                        cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                        pptDst: &destination,
                        psize: &size,
                        hdcSrc: bitmap.memory_dc,
                        pptSrc: &source,
                        pblend: &blend,
                        dwFlags: ULW_ALPHA,
                        ..Default::default()
                    };
                    ensure!(
                        UpdateLayeredWindowIndirect(popup.hwnd, &update).as_bool(),
                        "upload indirect layered popup: {}",
                        windows::core::Error::from_win32()
                    );
                } else {
                    UpdateLayeredWindow(
                        popup.hwnd,
                        HDC::default(),
                        Some(&destination),
                        Some(&size),
                        bitmap.memory_dc,
                        Some(&source),
                        COLORREF::default(),
                        Some(&blend),
                        ULW_ALPHA,
                    )?;
                }
                let frame = capture.capture((400, 300))?;
                assert_eq!(frame.popups.len(), 1);
                let image = &frame.popups[0];
                assert_eq!(
                    image.origin,
                    [
                        f64::from(destination.x + 1400),
                        f64::from(destination.y + 1000)
                    ]
                );
                assert_eq!(&image.rgba[..4], &[0, 0, 0, 0]);
                assert_eq!(&image.rgba[4..8], &[128, 64, 32, 64]);
                assert_eq!(GetForegroundWindow(), foreground);
                assert_eq!(
                    popup.state.desktop_moves.get(),
                    0,
                    "a captured layered popup must never visit the desktop"
                );
            }
            // A position-only update must retain the copied bitmap even when its
            // uploader has already reused the original DIB.
            pixels.fill(0);
            let destination = POINT { x: 180, y: 240 };
            UpdateLayeredWindow(
                popup.hwnd,
                HDC::default(),
                Some(&destination),
                None,
                HDC::default(),
                None,
                COLORREF::default(),
                None,
                UPDATE_LAYERED_WINDOW_FLAGS::default(),
            )?;
            let frame = capture.capture((400, 300))?;
            assert_eq!(frame.popups[0].origin, [1580.0, 1240.0]);
            assert_eq!(&frame.popups[0].rgba[4..8], &[128, 64, 32, 64]);
            assert_eq!(popup.state.desktop_moves.get(), 0);
        }
        drop(popup);
        assert!(capture.capture((400, 300))?.popups.is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn capture_host_stays_off_desktop_while_redrawing_resizing_and_reopening() -> Result<()> {
        use crate::plugins::{Placement, window::HostWindow};
        use std::time::{Duration, Instant};

        let foreground = unsafe { GetForegroundWindow() };
        for _ in 0..2 {
            let host = HostWindow::create(
                Placement::CaptureSource,
                640,
                360,
                "capture regression",
                false,
            )?;
            host.set_fixed_size(640, 360, false);
            let hwnd = HWND(host.handle() as *mut c_void);
            let check_host = || -> Result<()> {
                unsafe {
                    let mut bounds = RECT::default();
                    GetWindowRect(hwnd, &mut bounds)?;
                    ensure!(
                        bounds.right < GetSystemMetrics(SM_XVIRTUALSCREEN)
                            && bounds.bottom < GetSystemMetrics(SM_YVIRTUALSCREEN),
                        "capture source intersects the virtual desktop: {bounds:?}"
                    );
                    ensure!(
                        IsWindowVisible(hwnd).as_bool(),
                        "capture source must stay mapped"
                    );
                    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
                    ensure!(
                        ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0
                            == (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0
                            && ex & WS_EX_APPWINDOW.0 == 0,
                        "capture source entered task switching or permits activation"
                    );
                    ensure!(
                        GetWindowLongW(hwnd, GWL_STYLE) as u32 & (WS_CAPTION | WS_THICKFRAME).0
                            == 0,
                        "capture source gained a desktop frame"
                    );
                    ensure!(
                        GetForegroundWindow() == foreground,
                        "capture source changed foreground focus"
                    );
                }
                Ok(())
            };
            check_host()?;

            let child = fixture(Some(hwnd), (20, 24), (240, 180), [38, 139, 188])?;
            let mut capture = NativeCapture::new(host.handle())?;
            let first = capture.capture((1, 1))?;
            assert_eq!(first.native_size, (640, 360));
            assert_eq!(pixel(&first, 40, 40), [38, 139, 188, 255]);

            ensure!(
                unsafe { SetTimer(child.hwnd, 1, 20, None) } != 0,
                "start capture regression timer"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut changed = false;
            while Instant::now() < deadline {
                host.poll();
                let next = capture.capture((640, 360))?;
                changed |= next.rgba != first.rgba;
                if child.state.timer_ticks.get() >= 3 && changed {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe { KillTimer(child.hwnd, 1) }?;
            ensure!(
                child.state.timer_ticks.get() >= 3 && changed,
                "offscreen editor animation stopped"
            );
            assert_eq!(
                child.state.last_move.get(),
                None,
                "animation must not depend on pointer input"
            );

            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                click_count: 1,
                position: [40.0, 40.0],
                kind: PointerKind::Down(1),
                buttons: 1,
            })?;
            capture.pointer(EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                click_count: 1,
                position: [40.0, 40.0],
                kind: PointerKind::Up(1),
                buttons: 0,
            })?;
            assert_eq!(child.state.clicks.get(), 1);
            assert_eq!(
                pixel(&capture.capture((640, 360))?, 40, 40),
                [37, 171, 116, 255]
            );

            host.resize(480, 300);
            assert_eq!(capture.capture((640, 360))?.native_size, (480, 300));
            check_host()?;
            unsafe {
                // A plugin reposition request and a display-layout notification must not
                // expose the native source after its initial offscreen creation.
                SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )?;
                SendMessageW(hwnd, WM_DISPLAYCHANGE, WPARAM(0), LPARAM(0));
            }
            check_host()?;
            drop(capture);
            assert!(
                unsafe { IsWindow(hwnd) }.as_bool(),
                "capture owns only its readback resources"
            );
            drop(child);
            drop(host);
            assert!(
                !unsafe { IsWindow(hwnd) }.as_bool(),
                "capture host survived editor close"
            );
        }
        Ok(())
    }
}
