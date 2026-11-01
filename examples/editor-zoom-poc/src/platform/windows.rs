//! Native Win32 reference surface and a deliberately conservative CPU capture path.
//!
//! PrintWindow works for the GDI fixture and many software-rendered editors. It is not a
//! universal capture API for GPU-backed plugin child windows. Keep the reference window
//! visible: the client-DC fallback depends on the desktop compositor's visible contents.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::{CapturedFrame, PointerEvent, PointerKind};

const CLASS: PCWSTR = w!("nodeplayer-editor-zoom-poc-reference");

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: *mut c_void, dc: *mut c_void, flags: u32) -> i32;
    fn SetProcessDpiAwarenessContext(context: *mut c_void) -> i32;
}

struct State {
    fixture: bool,
    count: Cell<u64>,
    slider: Cell<i32>,
    down: Cell<bool>,
}

pub struct NativeEditor {
    hwnd: HWND,
    // A stable allocation used by GWLP_USERDATA; destroyed after the HWND.
    state: Box<State>,
    drag_target: Option<HWND>,
}

fn color(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

unsafe fn fill(dc: HDC, rect: RECT, color: COLORREF) {
    unsafe {
        let brush = CreateSolidBrush(color);
        if !brush.is_invalid() {
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush);
        }
    }
}

unsafe fn text(dc: HDC, x: i32, y: i32, value: &str) {
    let text: Vec<u16> = value.encode_utf16().collect();
    unsafe {
        let _ = TextOutW(dc, x, y, &text);
    }
}

unsafe fn draw(hwnd: HWND, dc: HDC, state: &State) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_err() {
            return;
        }
        fill(dc, rect, color(23, 28, 38));
        if !state.fixture {
            return;
        }
        for y in (0..rect.bottom).step_by(32) {
            for x in (0..rect.right).step_by(32) {
                if ((x + y) / 32) % 2 == 0 {
                    fill(
                        dc,
                        RECT {
                            left: x,
                            top: y,
                            right: (x + 32).min(rect.right),
                            bottom: (y + 32).min(rect.bottom),
                        },
                        color(30, 39, 51),
                    );
                }
            }
        }
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, color(235, 241, 248));
        text(dc, 40, 24, "Native Win32 editor reference");
        let count = state.count.get();
        let button_color = if state.down.get() {
            color(28, 112, 158)
        } else if count.is_multiple_of(2) {
            color(38, 139, 188)
        } else {
            color(37, 171, 116)
        };
        fill(
            dc,
            RECT {
                left: 40,
                top: 60,
                right: 200,
                bottom: 130,
            },
            button_color,
        );
        text(dc, 60, 86, &format!("Clicks: {count}"));
        text(dc, 230, 84, "Click the zoomed copy");
        text(
            dc,
            40,
            165,
            "Drag the slider through the graph's inverse transform",
        );
        fill(
            dc,
            RECT {
                left: 40,
                top: 215,
                right: 560,
                bottom: 235,
            },
            color(68, 80, 98),
        );
        let x = state.slider.get();
        fill(
            dc,
            RECT {
                left: x - 9,
                top: 200,
                right: x + 9,
                bottom: 250,
            },
            color(248, 187, 70),
        );
        text(
            dc,
            40,
            290,
            &format!(
                "Slider: {}    native pixels stay fixed while graph zooms",
                x - 40
            ),
        );
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref();
        if let Some(state) = state {
            match msg {
                WM_CLOSE => {
                    // The PoC owns the reference editor for its whole lifetime.
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut paint);
                    if !dc.is_invalid() {
                        draw(hwnd, dc, state);
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                WM_PRINTCLIENT => {
                    draw(hwnd, HDC(wparam.0 as *mut c_void), state);
                    return LRESULT(0);
                }
                WM_ERASEBKGND => return LRESULT(1),
                WM_LBUTTONDOWN if state.fixture => {
                    let x = lparam.0 as i16 as i32;
                    let y = (lparam.0 >> 16) as i16 as i32;
                    state
                        .down
                        .set((40..200).contains(&x) && (60..130).contains(&y));
                    if (40..560).contains(&x) && (195..255).contains(&y) {
                        state.slider.set(x.clamp(40, 560));
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                WM_MOUSEMOVE if state.fixture && (wparam.0 & 1) != 0 => {
                    let x = lparam.0 as i16 as i32;
                    let y = (lparam.0 >> 16) as i16 as i32;
                    if !state.down.get() && (185..265).contains(&y) {
                        state.slider.set(x.clamp(40, 560));
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                WM_LBUTTONUP if state.fixture => {
                    let x = lparam.0 as i16 as i32;
                    let y = (lparam.0 >> 16) as i16 as i32;
                    if state.down.replace(false) && (40..200).contains(&x) && (60..130).contains(&y)
                    {
                        state.count.set(state.count.get().saturating_add(1));
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

fn register_class() -> Result<()> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    let result = REGISTERED.get_or_init(|| unsafe {
        let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            hInstance: HINSTANCE(module.0),
            lpfnWndProc: Some(wndproc),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(|e| e.to_string())?,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_win32().to_string());
        }
        Ok(())
    });
    result
        .as_ref()
        .map_err(|e| anyhow::anyhow!("register native editor window class: {e}"))?;
    Ok(())
}

impl NativeEditor {
    pub fn new(width: u32, height: u32, fixture: bool) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "invalid native editor dimensions"
        );
        unsafe {
            // ERROR_ACCESS_DENIED simply means eframe/the process already chose its mode.
            let _ = SetProcessDpiAwarenessContext(-4isize as *mut c_void);
        }
        register_class()?;
        let state = Box::new(State {
            fixture,
            count: Cell::new(0),
            slider: Cell::new(300),
            down: Cell::new(false),
        });
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE::default())
                .context("size native editor window")?;
            let module = GetModuleHandleW(None)?;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS,
                w!("nodeplayer zoom PoC - native reference"),
                style,
                60,
                60,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                HINSTANCE(module.0),
                None,
            )
            .context("create native editor reference window")?;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(hwnd);
            Ok(Self {
                hwnd,
                state,
                drag_target: None,
            })
        }
    }

    pub fn handle(&self) -> usize {
        self.hwnd.0 as usize
    }

    pub fn size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rect) }.is_ok() {
            (
                (rect.right - rect.left).max(1) as u32,
                (rect.bottom - rect.top).max(1) as u32,
            )
        } else {
            (0, 0)
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "invalid native editor dimensions"
        );
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            let style = WINDOW_STYLE(GetWindowLongW(self.hwnd, GWL_STYLE) as u32);
            AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE::default())?;
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        Ok(())
    }

    pub fn pump(&mut self) -> Result<()> {
        unsafe {
            ensure!(
                IsWindow(self.hwnd).as_bool(),
                "native editor reference was destroyed"
            );
            let mut message = MSG::default();
            // Filtering by this HWND also pumps its descendants without consuming the
            // graph window's events. eframe pumps all windows in the normal GUI run.
            let mut count = 0;
            while count < 256 && PeekMessageW(&mut message, self.hwnd, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                count += 1;
            }
            let _ = RedrawWindow(
                self.hwnd,
                None,
                HRGN::default(),
                RDW_UPDATENOW | RDW_ALLCHILDREN,
            );
        }
        Ok(())
    }

    pub fn capture(&mut self) -> Result<CapturedFrame> {
        self.pump()?;
        let (width, height) = self.size();
        ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "native editor has invalid capture dimensions"
        );
        let capture = Capture::new(self.hwnd, width, height)?;
        unsafe {
            // PW_CLIENTONLY | PW_RENDERFULLCONTENT; WM_PRINT/WM_PRINTCLIENT is synchronous.
            let saved = SaveDC(capture.memory_dc);
            let printed = PrintWindow(self.hwnd.0, capture.memory_dc.0, 3) != 0;
            if saved != 0 {
                let _ = RestoreDC(capture.memory_dc, saved);
            }
            if !printed {
                BitBlt(
                    capture.memory_dc,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    capture.window_dc,
                    0,
                    0,
                    SRCCOPY | CAPTUREBLT,
                )
                .context("PrintWindow and client-DC fallback both failed")?;
            }
            // PW_CLIENTONLY can leave WS_CLIPCHILDREN holes in the parent's raster.
            // Each child's WM_PRINT expects its own window origin (including any border),
            // unlike the parent capture's client origin. Paint lower siblings first.
            if let Ok(first) = GetWindow(self.hwnd, GW_CHILD) {
                let mut origin = POINT::default();
                ensure!(
                    ClientToScreen(self.hwnd, &mut origin).as_bool(),
                    "locate native editor client origin"
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
                        let saved = SaveDC(capture.memory_dc);
                        ensure!(saved != 0, "save native child capture DC");
                        IntersectClipRect(
                            capture.memory_dc,
                            bounds.left,
                            bounds.top,
                            bounds.right,
                            bounds.bottom,
                        );
                        let _ = SetViewportOrgEx(capture.memory_dc, bounds.left, bounds.top, None);
                        SendMessageW(
                            child,
                            WM_PRINT,
                            WPARAM(capture.memory_dc.0 as usize),
                            LPARAM(
                                (PRF_CLIENT | PRF_NONCLIENT | PRF_CHILDREN | PRF_ERASEBKGND)
                                    as isize,
                            ),
                        );
                        let _ = RestoreDC(capture.memory_dc, saved);
                    }
                    let Some(previous) = previous else { break };
                    child = previous;
                }
            }
            let _ = GdiFlush();
            let pixels = std::slice::from_raw_parts(
                capture.bits as *const u8,
                width as usize * height as usize * 4,
            );
            let mut rgba = Vec::with_capacity(pixels.len());
            for pixel in pixels.as_chunks::<4>().0 {
                // BI_RGB GDI DIB alpha is undefined; opaque prevents accidental invisibility.
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
            let frame = CapturedFrame {
                width,
                height,
                rgba,
            };
            frame.validate()?;
            Ok(frame)
        }
    }

    pub fn pointer(&mut self, event: PointerEvent) -> Result<()> {
        ensure!(
            event.position.iter().all(|n| n.is_finite()),
            "invalid pointer coordinates"
        );
        let mut root_point = POINT {
            x: event.position[0].round() as i32,
            y: event.position[1].round() as i32,
        };
        unsafe {
            let target = self
                .drag_target
                .filter(|h| IsWindow(*h).as_bool())
                .unwrap_or_else(|| {
                    let mut target = self.hwnd;
                    let mut point = root_point;
                    for _ in 0..32 {
                        let child = ChildWindowFromPointEx(
                            target,
                            point,
                            CWP_SKIPDISABLED | CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT,
                        );
                        if child.0.is_null() || child == target {
                            break;
                        }
                        let _ = MapWindowPoints(target, child, std::slice::from_mut(&mut point));
                        target = child;
                    }
                    target
                });
            let _ = MapWindowPoints(self.hwnd, target, std::slice::from_mut(&mut root_point));
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
                |p: POINT| LPARAM(((p.x as u16 as u32) | ((p.y as u16 as u32) << 16)) as isize);
            let (message, value, position) = match event.kind {
                PointerKind::Move => (
                    WM_MOUSEMOVE,
                    WPARAM(flags as usize),
                    coordinates(root_point),
                ),
                PointerKind::Down(button) => {
                    let message = match button {
                        1 => WM_LBUTTONDOWN,
                        2 => WM_RBUTTONDOWN,
                        3 => WM_MBUTTONDOWN,
                        _ => bail!("unsupported pointer button"),
                    };
                    self.drag_target = Some(target);
                    (message, WPARAM(flags as usize), coordinates(root_point))
                }
                PointerKind::Up(button) => {
                    let message = match button {
                        1 => WM_LBUTTONUP,
                        2 => WM_RBUTTONUP,
                        3 => WM_MBUTTONUP,
                        _ => bail!("unsupported pointer button"),
                    };
                    (message, WPARAM(flags as usize), coordinates(root_point))
                }
                PointerKind::Wheel { dx, dy } => {
                    // Wheel LPARAM is screen coordinates, unlike all button/move messages.
                    let _ = ClientToScreen(target, &mut root_point);
                    let (message, delta) = if dy.abs() >= dx.abs() {
                        (WM_MOUSEWHEEL, dy)
                    } else {
                        (WM_MOUSEHWHEEL, dx)
                    };
                    let delta = delta.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    (
                        message,
                        WPARAM(flags as usize | ((delta as u16 as usize) << 16)),
                        coordinates(root_point),
                    )
                }
            };
            SendMessageW(target, message, value, position);
            // Preserve the graph's real mouse stream. This PoC forwards drags explicitly;
            // OS capture by a plugin would otherwise bypass inverse coordinate conversion.
            let captured = GetCapture();
            if captured == self.hwnd || IsChild(self.hwnd, captured).as_bool() {
                let _ = ReleaseCapture();
            }
            if matches!(event.kind, PointerKind::Up(_)) && event.buttons == 0 {
                self.drag_target = None;
            }
        }
        self.pump()
    }

    pub fn fixture_value(&self) -> Option<u64> {
        self.state.fixture.then(|| self.state.count.get())
    }
}

impl Drop for NativeEditor {
    fn drop(&mut self) {
        unsafe {
            // State remains alive through DestroyWindow's final messages.
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Scoped GDI handles, including restoring the DC's original selected object before deletion.
struct Capture {
    hwnd: HWND,
    window_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut c_void,
}

impl Capture {
    fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let mut capture = Self {
            hwnd,
            window_dc: HDC::default(),
            memory_dc: HDC::default(),
            bitmap: HBITMAP::default(),
            original: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
        };
        unsafe {
            capture.window_dc = GetDC(hwnd);
            ensure!(
                !capture.window_dc.is_invalid(),
                "GetDC for native editor failed"
            );
            capture.memory_dc = CreateCompatibleDC(capture.window_dc);
            ensure!(!capture.memory_dc.is_invalid(), "CreateCompatibleDC failed");
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
            capture.bitmap = CreateDIBSection(
                capture.memory_dc,
                &info,
                DIB_RGB_COLORS,
                &mut capture.bits,
                None,
                0,
            )
            .context("allocate native editor capture DIB")?;
            ensure!(!capture.bits.is_null(), "capture DIB has no pixel storage");
            capture.original = SelectObject(capture.memory_dc, capture.bitmap);
            ensure!(
                !capture.original.is_invalid(),
                "select native capture bitmap"
            );
            std::ptr::write_bytes(capture.bits, 0, width as usize * height as usize * 4);
        }
        Ok(capture)
    }
}

impl Drop for Capture {
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

    struct ChildFixture {
        hwnd: HWND,
        state: Box<State>,
    }

    impl Drop for ChildFixture {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    fn pixel(frame: &CapturedFrame, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * frame.width as usize + x as usize) * 4;
        frame.rgba[offset..offset + 4].try_into().unwrap()
    }

    /// A plugin's HWND is normally a child of the host-provided parent. The offset makes
    /// this exercise the same capture hierarchy and root-to-child input conversion.
    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn captures_and_routes_nested_child() -> Result<()> {
        let mut editor = NativeEditor::new(720, 440, false)?;
        let state = Box::new(State {
            fixture: true,
            count: Cell::new(0),
            slider: Cell::new(300),
            down: Cell::new(false),
        });
        let hwnd = unsafe {
            let module = GetModuleHandleW(None)?;
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS,
                w!("native child editor fixture"),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
                32,
                28,
                640,
                360,
                editor.hwnd,
                None,
                HINSTANCE(module.0),
                None,
            )?
        };
        let child = ChildFixture { hwnd, state };
        unsafe {
            SetWindowLongPtrW(
                child.hwnd,
                GWLP_USERDATA,
                &*child.state as *const State as isize,
            );
            let _ = InvalidateRect(child.hwnd, None, false);
        }

        let before = editor.capture()?;
        assert_eq!(editor.fixture_value(), None);
        assert_eq!(child.state.count.get(), 0);
        // Sample inside the button, away from text; the source is offset (32, 28).
        assert_eq!(pixel(&before, 32 + 50, 28 + 70), [38, 139, 188, 255]);

        editor.pointer(PointerEvent {
            position: [32.0 + 80.0, 28.0 + 90.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        })?;
        editor.pointer(PointerEvent {
            position: [32.0 + 80.0, 28.0 + 90.0],
            kind: PointerKind::Up(1),
            buttons: 0,
        })?;

        let after = editor.capture()?;
        assert_eq!(child.state.count.get(), 1);
        assert_eq!(pixel(&after, 32 + 50, 28 + 70), [37, 171, 116, 255]);
        assert_ne!(before.rgba, after.rgba);
        if let Some(directory) = std::env::var_os("NODEPLAYER_ZOOM_TEST_ARTIFACTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory)?;
            for (name, frame) in [("child-before.png", &before), ("child-after.png", &after)] {
                image::save_buffer(
                    directory.join(name),
                    &frame.rgba,
                    frame.width,
                    frame.height,
                    image::ColorType::Rgba8,
                )?;
            }
        }
        Ok(())
    }
}
