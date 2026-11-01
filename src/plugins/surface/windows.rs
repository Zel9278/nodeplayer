//! Worker-local capture and input for an editor HWND owned by its existing host window.
//!
//! `PrintWindow` and the fallback DC readback are synchronous. Only the editor's UI thread
//! may use this adapter; none of these operations belongs in the parent or audio callback.
//! GPU-owned surfaces may not support this capture path. The DC fallback needs visible
//! source contents, so this backend does not assume hidden-window rendering.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{EditorFrame, EditorPointer, PointerKind};

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
    // Native handles, capture, and plugin window procedures stay on their owner thread.
    _main_thread: PhantomData<Rc<()>>,
}

impl NativeCapture {
    pub fn new(handle: crate::plugins::WindowId) -> Result<Self> {
        let capture = Self {
            hwnd: HWND(handle as *mut c_void),
            drag_target: None,
            bitmap: None,
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
            let saved = SavedDc::new(bitmap.memory_dc)?;
            // PW_CLIENTONLY | PW_RENDERFULLCONTENT. A plugin can hang this call; it runs
            // only in the worker, whose existing watchdog bounds the parent impact.
            let printed = PrintWindow(self.hwnd.0, bitmap.memory_dc.0, 3) != 0;
            drop(saved);
            if !printed {
                BitBlt(
                    bitmap.memory_dc,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    bitmap.window_dc,
                    0,
                    0,
                    SRCCOPY | CAPTUREBLT,
                )
                .context("PrintWindow and editor DC capture both failed")?;
            }

            // PW_CLIENTONLY leaves holes where WS_CLIPCHILDREN excludes native plugin
            // children. Paint each child at its window origin, with lower siblings first.
            if let Ok(first) = GetWindow(self.hwnd, GW_CHILD) {
                let mut origin = POINT::default();
                ensure!(
                    ClientToScreen(self.hwnd, &mut origin).as_bool(),
                    "locate editor client origin"
                );
                let mut child = GetWindow(first, GW_HWNDLAST).unwrap_or(first);
                for _ in 0..256 {
                    let previous = GetWindow(child, GW_HWNDPREV).ok();
                    let mut bounds = RECT::default();
                    if IsWindowVisible(child).as_bool() && GetWindowRect(child, &mut bounds).is_ok()
                    {
                        bounds.left -= origin.x;
                        bounds.right -= origin.x;
                        bounds.top -= origin.y;
                        bounds.bottom -= origin.y;
                        let _saved = SavedDc::new(bitmap.memory_dc)?;
                        IntersectClipRect(
                            bitmap.memory_dc,
                            bounds.left,
                            bounds.top,
                            bounds.right,
                            bounds.bottom,
                        );
                        let _ = SetViewportOrgEx(bitmap.memory_dc, bounds.left, bounds.top, None);
                        SendMessageW(
                            child,
                            WM_PRINT,
                            WPARAM(bitmap.memory_dc.0 as usize),
                            LPARAM(
                                (PRF_CLIENT | PRF_NONCLIENT | PRF_CHILDREN | PRF_ERASEBKGND)
                                    as isize,
                            ),
                        );
                    }
                    let Some(previous) = previous else { break };
                    child = previous;
                }
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
            Ok(EditorFrame {
                width,
                height,
                native_size: (width, height),
                rgba,
            })
        }
    }

    pub fn pointer(&mut self, event: EditorPointer) -> Result<()> {
        self.check_window()?;
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
            let target = self
                .drag_target
                .filter(|target| {
                    IsWindow(*target).as_bool()
                        && (*target == self.hwnd || IsChild(self.hwnd, *target).as_bool())
                })
                .unwrap_or_else(|| {
                    let mut target = self.hwnd;
                    let mut local = point;
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
            let _ = MapWindowPoints(self.hwnd, target, std::slice::from_mut(&mut point));
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
            let coordinates =
                |p: POINT| LPARAM((u32::from(p.x as u16) | (u32::from(p.y as u16) << 16)) as isize);
            let (message, value, position) = match event.kind {
                PointerKind::Move => (WM_MOUSEMOVE, WPARAM(flags as usize), coordinates(point)),
                PointerKind::Down(button) => {
                    let message = match button {
                        1 => WM_LBUTTONDOWN,
                        2 => WM_RBUTTONDOWN,
                        3 => WM_MBUTTONDOWN,
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
            // Graph input is explicitly forwarded. Releasing plugin mouse capture keeps
            // the physical pointer in the parent instead of bypassing inverse mapping.
            let captured = GetCapture();
            if captured == self.hwnd || IsChild(self.hwnd, captured).as_bool() {
                let _ = ReleaseCapture();
            }
            if matches!(event.kind, PointerKind::Up(_)) && event.buttons == 0 {
                self.drag_target = None;
            }
        }
        Ok(())
    }
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
    use windows::core::{PCWSTR, w};

    const CLASS: PCWSTR = w!("nodeplayer-captured-editor-adapter-test");

    struct State {
        color: [u8; 3],
        down: Cell<bool>,
        clicks: Cell<u32>,
        last_move: Cell<Option<(i32, i32)>>,
        last_up: Cell<Option<(i32, i32)>>,
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
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_err() {
                return;
            }
            let [r, g, b] = if state.clicks.get() == 0 {
                state.color
            } else {
                [37, 171, 116]
            };
            let brush = CreateSolidBrush(COLORREF(
                u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16),
            ));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush);
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
                    WM_LBUTTONDOWN => {
                        state.down.set(true);
                        return LRESULT(0);
                    }
                    WM_MOUSEMOVE => {
                        state.last_move.set(Some(point));
                        return LRESULT(0);
                    }
                    WM_LBUTTONUP => {
                        if state.down.replace(false) {
                            state.clicks.set(state.clicks.get() + 1);
                        }
                        state.last_up.set(Some(point));
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
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
            down: Cell::new(false),
            clicks: Cell::new(0),
            last_move: Cell::new(None),
            last_up: Cell::new(None),
        });
        unsafe {
            let style =
                WS_CLIPCHILDREN | WS_VISIBLE | if parent.is_some() { WS_CHILD } else { WS_POPUP };
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
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
            Ok(TestWindow { hwnd, state })
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
    #[ignore = "requires an interactive Windows desktop"]
    fn captures_and_routes_nested_children_without_owning_source() -> Result<()> {
        let root = fixture(None, (60, 60), (720, 440), [23, 28, 38])?;
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
        let before = capture.capture((1, 1))?;
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
            position: [157.0, 78.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        })?;
        capture.pointer(EditorPointer {
            position: [-10.0, -20.0],
            kind: PointerKind::Move,
            buttons: 1,
        })?;
        capture.pointer(EditorPointer {
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
}
