//! The Win32 window a plugin editor draws into, shared by the VST3 and CLAP hosts; the
//! Windows counterpart of `x11win`.
//!
//! An editor is either a top-level window of its own, or embedded as a child of the app's
//! main window. Embedded editors live in a clipping "container" window so they can be
//! scrolled and partially covered by the host's layout. The windows belong to the UI thread,
//! whose event loop (winit's) dispatches their messages.

use std::cell::{Cell, RefCell};
use std::sync::Once;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject, HBRUSH, HRGN, RDW_ALLCHILDREN, RDW_UPDATENOW, RedrawWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use super::{Placement, PxRect, WinEvent, WindowId};

const CLASS: PCWSTR = w!("nodeplayer-plugin-editor");

/// What the window procedure shares with its [`HostWindow`].
struct State {
    /// Client size in physical pixels.
    size: Cell<(u32, u32)>,
    /// The user can't resize a top-level window with this set.
    fixed: Cell<bool>,
    events: RefCell<Vec<WinEvent>>,
}

pub struct HostWindow {
    /// The window handed to the plugin.
    id: HWND,
    /// Clipping parent of `id` when embedded.
    container: Option<HWND>,
    pub placement: Placement,
    /// Content scale the plugin currently draws at, and its size at scale 1.
    pub scale: Cell<f32>,
    pub base: Cell<(f32, f32)>,
    /// Last applied embedded geometry: (container rect, plugin offset inside it); None = hidden.
    shown: Cell<Option<(PxRect, (i32, i32))>>,
    state: Box<State>,
}

fn instance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default() }
}

fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            log::warn!("can't register the plugin editor window class");
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        if let Some(s) = state.as_ref() {
            match msg {
                // Closed by the user: the editor's owner decides, and drops us.
                WM_CLOSE => {
                    s.events.borrow_mut().push(WinEvent::Close);
                    return LRESULT(0);
                }
                WM_SIZE => {
                    let size = ((lparam.0 & 0xffff) as u32, ((lparam.0 >> 16) & 0xffff) as u32);
                    if size != s.size.get() && size.0 > 0 && size.1 > 0 {
                        s.size.set(size);
                        s.events.borrow_mut().push(WinEvent::Resized(size.0, size.1));
                    }
                }
                WM_GETMINMAXINFO if s.fixed.get() => {
                    let (w, h) = outer_size(hwnd, s.size.get());
                    let info = &mut *(lparam.0 as *mut MINMAXINFO);
                    info.ptMinTrackSize.x = w;
                    info.ptMinTrackSize.y = h;
                    info.ptMaxTrackSize.x = w;
                    info.ptMaxTrackSize.y = h;
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Size of a top-level window whose client area is `size`.
fn outer_size(hwnd: HWND, (w, h): (u32, u32)) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
        let ex = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
        let _ = AdjustWindowRectEx(&mut r, style, false, ex);
    }
    (r.right - r.left, r.bottom - r.top)
}

fn create(ex: WINDOW_EX_STYLE, title: &str, style: WINDOW_STYLE, (w, h): (i32, i32), parent: Option<HWND>) -> Result<HWND> {
    let title = HSTRING::from(title);
    unsafe {
        CreateWindowExW(ex, CLASS, &title, style, 0, 0, w, h, parent.unwrap_or_default(), None, instance(), None)
            .context("can't create the editor window")
    }
}

impl HostWindow {
    /// `_announce` is for Wine editors under X11; Windows plugins run natively here.
    pub fn create(placement: Placement, w: u32, h: u32, title: &str, _announce: bool) -> Result<Self> {
        register_class();
        let (w, h) = (w.clamp(16, 8192), h.clamp(16, 8192));
        let state = Box::new(State { size: Cell::new((w, h)), fixed: Cell::new(false), events: RefCell::new(Vec::new()) });
        let (id, container) = match placement {
            Placement::Window => {
                let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
                let id = create(WINDOW_EX_STYLE::default(), title, style, (w as i32, h as i32), None)?;
                (id, None)
            }
            Placement::Embedded(parent) => {
                let parent = HWND(parent as *mut _);
                // Hidden until the host places it.
                let style = WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
                let container = create(WINDOW_EX_STYLE::default(), "", style, (w as i32, h as i32), Some(parent))?;
                let id = match create(WINDOW_EX_STYLE::default(), "", style | WS_VISIBLE, (w as i32, h as i32), Some(container)) {
                    Ok(id) => id,
                    Err(e) => {
                        unsafe { DestroyWindow(container).ok() };
                        return Err(e);
                    }
                };
                (id, Some(container))
            }
        };
        unsafe {
            // Messages before this (creation) go to DefWindowProc.
            SetWindowLongPtrW(id, GWLP_USERDATA, &*state as *const State as isize);
        }
        let win = Self {
            id,
            container,
            placement,
            scale: Cell::new(1.0),
            base: Cell::new((w as f32, h as f32)),
            shown: Cell::new(None),
            state,
        };
        if container.is_none() {
            // The window was created with the client size as its outer size.
            win.resize(w, h);
            unsafe {
                let _ = ShowWindow(id, SW_SHOW);
            }
        }
        Ok(win)
    }

    /// The window handed to the plugin.
    pub fn handle(&self) -> WindowId {
        self.id.0 as WindowId
    }

    /// Size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        self.state.size.get()
    }

    pub fn resize(&self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        self.state.size.set((w, h));
        let (ow, oh) = if self.container.is_some() { (w as i32, h as i32) } else { outer_size(self.id, (w, h)) };
        unsafe {
            let _ = SetWindowPos(self.id, None, 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// Lets the user resize a top-level editor, or pins it to `w`×`h`.
    pub fn set_fixed_size(&self, w: u32, h: u32, fixed: bool) {
        if self.container.is_some() {
            return;
        }
        self.state.fixed.set(fixed);
        if fixed {
            self.state.size.set((w.max(1), h.max(1)));
        }
        unsafe {
            let mut style = GetWindowLongW(self.id, GWL_STYLE) as u32;
            let resize = (WS_THICKFRAME | WS_MAXIMIZEBOX).0;
            if fixed {
                style &= !resize;
            } else {
                style |= resize;
            }
            SetWindowLongW(self.id, GWL_STYLE, style as i32);
            let _ = SetWindowPos(self.id, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
    }

    /// Positions an embedded editor. `clip` is the visible area in the parent window and
    /// `origin` where the editor's top-left corner would be (it may lie outside `clip` when
    /// scrolled). `None` hides the editor.
    pub fn place(&self, clip: Option<PxRect>, origin: (i32, i32)) {
        let Some(container) = self.container else { return };
        let next = clip.filter(|c| c.w > 0 && c.h > 0).map(|c| (c, (origin.0 - c.x, origin.1 - c.y)));
        if next == self.shown.get() {
            return;
        }
        unsafe {
            match next {
                Some((c, (ox, oy))) => {
                    let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
                    let _ = SetWindowPos(container, HWND_TOP, c.x, c.y, c.w as i32, c.h as i32, flags);
                    let _ = SetWindowPos(self.id, None, ox, oy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
                None => {
                    let _ = ShowWindow(container, SW_HIDE);
                }
            }
        }
        self.shown.set(next);
    }

    /// What happened since the last call. The messages themselves are dispatched by the UI
    /// thread's event loop, except timers ([`pump_timers`]) and painting: the main window asks
    /// to be repainted again as soon as it's drawn while something plays, and Windows hands
    /// out its WM_PAINT first, so the plugin's would wait indefinitely. Painting ours and the
    /// plugin's windows here, directly, skips that queue (and does nothing when none needs it).
    pub fn poll(&self) -> Vec<WinEvent> {
        pump_timers();
        unsafe {
            let _ = RedrawWindow(self.container.unwrap_or(self.id), None, HRGN::default(), RDW_UPDATENOW | RDW_ALLCHILDREN);
        }
        // Embedded windows are sized by the host only.
        let events = std::mem::take(&mut *self.state.events.borrow_mut());
        if self.container.is_some() { events.into_iter().filter(|e| matches!(e, WinEvent::Close)).collect() } else { events }
    }
}

/// Delivers the UI thread's due `WM_TIMER`s. Windows only makes them while no `WM_PAINT` is
/// pending, and the app repaints nearly every frame while playing, so without this a plugin GUI
/// driven by timers (JUCE's, among many) stops updating. Asking for that one message by number
/// gets it even with a repaint pending.
fn pump_timers() {
    unsafe {
        let mut msg = MSG::default();
        // Bounded: a timer that keeps firing can't hold the UI thread here.
        for _ in 0..64 {
            if !PeekMessageW(&mut msg, None, WM_TIMER, WM_TIMER, PM_REMOVE).as_bool() {
                break;
            }
            DispatchMessageW(&msg);
        }
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.id, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.container.unwrap_or(self.id));
        }
    }
}
