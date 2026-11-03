//! The Win32 window a plugin editor draws into, shared by the VST3 and CLAP hosts; the
//! Windows counterpart of `x11win`.
//!
//! An editor is either a top-level window of its own, or embedded as a child of the app's
//! main window. Embedded editors live in a clipping "container" window so they can be
//! scrolled and partially covered by the host's layout. The windows belong to the UI thread,
//! whose event loop (winit's) dispatches their messages.
//! Captured editors use a mapped, borderless tool window outside the virtual desktop;
//! it never activates or becomes a separate taskbar/Alt-Tab entry.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Once;

use anyhow::{Context, Result, ensure};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, ClientToScreen, CombineRgn, CreateRectRgn, DeleteObject, GetStockObject, HBRUSH, HRGN, RDW_ALLCHILDREN, RDW_UPDATENOW, RGN_OR,
    RedrawWindow, SetWindowRgn,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetFocus, ReleaseCapture, SetCapture, SetFocus,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use super::{Placement, Spot, WinEvent, WindowId};

const CLASS: PCWSTR = w!("nodeplayer-plugin-editor");

type SubclassProc =
    Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT>;

// These helpers need no common-controls widgets or process-wide initialization. They
// coexist with a plugin's own subclass chain and can only be used on the owner thread.
#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        hwnd: HWND,
        callback: SubclassProc,
        id: usize,
        data: usize,
    ) -> windows::Win32::Foundation::BOOL;
    fn RemoveWindowSubclass(
        hwnd: HWND,
        callback: SubclassProc,
        id: usize,
    ) -> windows::Win32::Foundation::BOOL;
    fn DefSubclassProc(hwnd: HWND, message: u32, value: WPARAM, position: LPARAM) -> LRESULT;
}

thread_local! {
    static CAPTURE_INPUTS: RefCell<Vec<Weak<CaptureInput>>> = const { RefCell::new(Vec::new()) };
}

/// Graph input owns physical capture/focus. The private editor sees native ownership
/// while its callbacks run, and retains logical ownership between forwarded events.
pub(crate) struct CaptureInput {
    root: HWND,
    capture: Cell<HWND>,
    focus: Cell<HWND>,
    depth: Cell<u32>,
    suppress_capture: Cell<HWND>,
    suppress_focus: Cell<HWND>,
    returning_capture: Cell<HWND>,
    returning_focus: Cell<HWND>,
    hooks: RefCell<Vec<HWND>>,
    popup_origins: RefCell<Vec<(HWND, POINT)>>,
    creation_hook: Cell<HHOOK>,
}

impl CaptureInput {
    pub(crate) fn new(root: HWND) -> Result<Rc<Self>> {
        let input = Rc::new(Self {
            root,
            capture: Cell::new(HWND::default()),
            focus: Cell::new(HWND::default()),
            depth: Cell::new(0),
            suppress_capture: Cell::new(HWND::default()),
            suppress_focus: Cell::new(HWND::default()),
            returning_capture: Cell::new(HWND::default()),
            returning_focus: Cell::new(HWND::default()),
            hooks: RefCell::new(Vec::new()),
            popup_origins: RefCell::new(Vec::new()),
            creation_hook: Cell::new(HHOOK::default()),
        });
        input.refresh()?;
        CAPTURE_INPUTS.with(|inputs| {
            let mut inputs = inputs.borrow_mut();
            inputs.retain(|input| input.strong_count() != 0);
            inputs.push(Rc::downgrade(&input));
        });
        input.creation_hook.set(
            unsafe {
                SetWindowsHookExW(
                    WH_CBT,
                    Some(capture_create_proc),
                    HINSTANCE::default(),
                    windows::Win32::System::Threading::GetCurrentThreadId(),
                )
            }
            .context("watch captured editor popup creation")?,
        );
        // The plugin may create tooltip/owned windows before the capture adapter attaches.
        let mut top = unsafe { GetTopWindow(HWND::default()) }.ok();
        for _ in 0..4096 {
            let Some(hwnd) = top else {
                break;
            };
            top = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
            if hwnd != root && input.owns(hwnd) {
                input.hook(hwnd)?;
                let mut bounds = RECT::default();
                unsafe { GetWindowRect(hwnd, &mut bounds) }
                    .context("read captured popup origin")?;
                input.remember_popup(
                    hwnd,
                    POINT {
                        x: bounds.left,
                        y: bounds.top,
                    },
                );
                unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        capture_origin((bounds.right - bounds.left, bounds.bottom - bounds.top)).0,
                        capture_origin((bounds.right - bounds.left, bounds.bottom - bounds.top)).1,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                }
                .context("keep captured popup off desktop")?;
            }
        }
        Ok(input)
    }

    fn owns(&self, hwnd: HWND) -> bool {
        if hwnd.0.is_null() || !unsafe { IsWindow(hwnd) }.as_bool() {
            return false;
        }
        if hwnd == self.root || unsafe { IsChild(self.root, hwnd) }.as_bool() {
            return true;
        }
        let mut top = unsafe { GetAncestor(hwnd, GA_ROOT) };
        for _ in 0..16 {
            let Ok(owner) = (unsafe { GetWindow(top, GW_OWNER) }) else {
                return false;
            };
            if owner == self.root || unsafe { IsChild(self.root, owner) }.as_bool() {
                return true;
            }
            top = owner;
        }
        false
    }

    fn hook(&self, hwnd: HWND) -> Result<()> {
        if self.hooks.borrow().contains(&hwnd) {
            return Ok(());
        }
        let ptr = self as *const Self as usize;
        ensure!(
            unsafe { SetWindowSubclass(hwnd, Some(capture_input_proc), ptr, ptr) }.as_bool(),
            "subclass captured editor input"
        );
        self.hooks.borrow_mut().push(hwnd);
        Ok(())
    }

    pub(crate) fn refresh(&self) -> Result<()> {
        self.hooks
            .borrow_mut()
            .retain(|hwnd| unsafe { IsWindow(*hwnd).as_bool() });
        self.hook(self.root)?;
        let mut pending = vec![self.root];
        pending.extend(self.popup_origins.borrow().iter().filter_map(|(window, _)| {
            self.owns(*window).then_some(*window)
        }));
        let mut count = 0;
        while let Some(parent) = pending.pop() {
            let mut child = unsafe { GetWindow(parent, GW_CHILD) }.ok();
            while let Some(hwnd) = child {
                count += 1;
                ensure!(count <= 512, "captured editor has too many native children");
                self.hook(hwnd)?;
                pending.push(hwnd);
                child = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
            }
        }
        Ok(())
    }

    pub(crate) fn popups(&self) -> Result<Vec<(HWND, POINT)>> {
        let mut result = Vec::new();
        let mut root_origin = POINT::default();
        ensure!(unsafe { ClientToScreen(self.root, &mut root_origin) }.as_bool(), "locate captured popup host");
        for &(hwnd, origin) in self.popup_origins.borrow().iter() {
            if self.owns(hwnd) && unsafe { IsWindowVisible(hwnd) }.as_bool() {
                ensure!(result.len() < 16, "too many captured editor popups");
                result.push((hwnd, POINT {
                    x: root_origin.x.saturating_add(origin.x),
                    y: root_origin.y.saturating_add(origin.y),
                }));
            }
        }
        // GW_HWNDPREV counts windows above this one. Paint the lowest owned popup first.
        result.sort_by_cached_key(|(hwnd, _)| {
            let mut above = 0usize;
            let mut next = *hwnd;
            while let Ok(previous) = unsafe { GetWindow(next, GW_HWNDPREV) } {
                above += 1;
                if above >= 4096 {
                    break;
                }
                next = previous;
            }
            std::cmp::Reverse(above)
        });
        Ok(result)
    }

    fn remember_popup(&self, hwnd: HWND, origin: POINT) {
        let mut root_origin = POINT::default();
        if !unsafe { ClientToScreen(self.root, &mut root_origin) }.as_bool() {
            return;
        }
        let origin = POINT {
            x: origin.x.saturating_sub(root_origin.x),
            y: origin.y.saturating_sub(root_origin.y),
        };
        let mut origins = self.popup_origins.borrow_mut();
        if let Some((_, previous)) = origins.iter_mut().find(|(window, _)| *window == hwnd) {
            *previous = origin;
        } else {
            origins.push((hwnd, origin));
        }
    }

    pub(crate) fn owner(&self) -> Option<HWND> {
        self.owns(self.capture.get()).then(|| self.capture.get())
    }

    pub(crate) fn contains(&self, hwnd: HWND) -> bool {
        self.owns(hwnd)
    }

    pub(crate) fn cancel(&self) {
        let capture = self.capture.replace(HWND::default());
        let focus = self.focus.replace(HWND::default());
        unsafe {
            let target = if self.owns(capture) { capture } else { focus };
            if self.owns(target) {
                SendMessageW(target, WM_CANCELMODE, WPARAM(0), LPARAM(0));
                // Logical capture was released between callbacks. Deliver the actual
                // cancellation even though there may be no physical Win32 capture now.
                SendMessageW(target, WM_CAPTURECHANGED, WPARAM(0), LPARAM(0));
            }
            if self.owns(GetCapture()) {
                let _ = ReleaseCapture();
            }
        }
    }

    pub(crate) fn enter(&self) -> CaptureInputScope<'_> {
        let depth = self.depth.get();
        self.depth.set(depth + 1);
        let mut scope = CaptureInputScope {
            input: self,
            previous_capture: HWND::default(),
            previous_focus: HWND::default(),
            temporary: Vec::new(),
            outer: depth == 0,
        };
        if depth != 0 {
            return scope;
        }
        unsafe {
            scope.previous_capture = GetCapture();
            scope.previous_focus = GetFocus();
            self.returning_capture.set(scope.previous_capture);
            self.returning_focus.set(scope.previous_focus);
            for hwnd in [scope.previous_capture, scope.previous_focus] {
                if !hwnd.0.is_null() && !self.owns(hwnd) && !scope.temporary.contains(&hwnd) {
                    let ptr = self as *const Self as usize;
                    if SetWindowSubclass(hwnd, Some(capture_input_proc), ptr, ptr).as_bool() {
                        scope.temporary.push(hwnd);
                    }
                }
            }
            if let Some(owner) = self.owner().filter(|owner| *owner != GetCapture()) {
                SetCapture(owner);
            }
            if self.owns(self.focus.get()) && self.focus.get() != GetFocus() {
                let _ = SetFocus(self.focus.get());
            }
        }
        scope
    }
}

pub(crate) struct CaptureInputScope<'a> {
    input: &'a CaptureInput,
    previous_capture: HWND,
    previous_focus: HWND,
    temporary: Vec<HWND>,
    outer: bool,
}

impl Drop for CaptureInputScope<'_> {
    fn drop(&mut self) {
        let input = self.input;
        if self.outer {
            unsafe {
                let captured = GetCapture();
                if input.owns(captured) {
                    input.capture.set(captured);
                    input.suppress_capture.set(captured);
                    let _ = ReleaseCapture();
                    input.suppress_capture.set(HWND::default());
                }
                if !self.previous_capture.0.is_null()
                    && !input.owns(self.previous_capture)
                    && IsWindow(self.previous_capture).as_bool()
                    && GetCapture().0.is_null()
                {
                    SetCapture(self.previous_capture);
                }
                let focused = GetFocus();
                if input.owns(focused) {
                    input.focus.set(focused);
                    input.suppress_focus.set(focused);
                    let _ = SetFocus(self.previous_focus);
                    input.suppress_focus.set(HWND::default());
                }
                let ptr = input as *const CaptureInput as usize;
                for hwnd in &self.temporary {
                    let _ = RemoveWindowSubclass(*hwnd, Some(capture_input_proc), ptr);
                }
            }
            input.returning_capture.set(HWND::default());
            input.returning_focus.set(HWND::default());
        }
        input.depth.set(input.depth.get().saturating_sub(1));
    }
}

unsafe extern "system" fn capture_input_proc(
    hwnd: HWND,
    message: u32,
    value: WPARAM,
    position: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // NativeCapture retains the Rc until every subclass is removed. All access and
    // destruction are confined to this HWND's thread; no Rust mutable borrow spans a call.
    let input = unsafe { &*(data as *const CaptureInput) };
    let next = HWND(position.0 as *mut _);
    if message == WM_WINDOWPOSCHANGING
        && hwnd != input.root
        && !unsafe { IsChild(input.root, hwnd) }.as_bool()
        && input.owns(hwnd)
        && let Some(change) = unsafe { (position.0 as *mut WINDOWPOS).as_mut() }
    {
        let mut bounds = RECT::default();
        let size = if change.flags & SWP_NOSIZE == SWP_NOSIZE
            && unsafe { GetWindowRect(hwnd, &mut bounds) }.is_ok()
        {
            (bounds.right - bounds.left, bounds.bottom - bounds.top)
        } else {
            (change.cx, change.cy)
        };
        let actual = capture_origin(size);
        if change.flags & SWP_NOMOVE != SWP_NOMOVE && (change.x, change.y) != actual {
            input.remember_popup(
                hwnd,
                POINT {
                    x: change.x,
                    y: change.y,
                },
            );
        }
        (change.x, change.y) = actual;
        change.flags &= !SWP_NOMOVE;
    }
    if message == WM_CAPTURECHANGED {
        if hwnd == input.suppress_capture.get() && next.0.is_null()
            || hwnd == input.returning_capture.get() && input.depth.get() != 0 && input.owns(next)
        {
            return LRESULT(0);
        }
        if input.capture.get() == hwnd && next != hwnd {
            input.capture.set(if input.owns(next) {
                next
            } else {
                HWND::default()
            });
        }
    }
    if message == WM_KILLFOCUS {
        let next = HWND(value.0 as *mut _);
        if hwnd == input.suppress_focus.get() && next == input.returning_focus.get()
            || hwnd == input.returning_focus.get() && input.depth.get() != 0 && input.owns(next)
        {
            return LRESULT(0);
        }
        if input.focus.get() == hwnd {
            input.focus.set(HWND::default());
            input.capture.set(HWND::default());
        }
    }
    if message == WM_CANCELMODE && input.owns(hwnd) {
        input.capture.set(HWND::default());
    }
    if message == WM_NCDESTROY {
        if input.capture.get() == hwnd {
            input.capture.set(HWND::default());
        }
        if input.focus.get() == hwnd {
            input.focus.set(HWND::default());
        }
        input
            .popup_origins
            .borrow_mut()
            .retain(|(window, _)| *window != hwnd);
        input.hooks.borrow_mut().retain(|window| *window != hwnd);
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(capture_input_proc), id);
        }
    }
    let _scope = (input.owns(hwnd)
        && matches!(message, WM_TIMER | WM_PAINT | WM_PRINT | WM_PRINTCLIENT))
    .then(|| input.enter());
    unsafe { DefSubclassProc(hwnd, message, value, position) }
}

unsafe extern "system" fn capture_create_proc(
    code: i32,
    value: WPARAM,
    position: LPARAM,
) -> LRESULT {
    if code == HCBT_CREATEWND as i32
        && let Some(create) = unsafe { (position.0 as *mut CBT_CREATEWNDW).as_mut() }
        && let Some(details) = unsafe { create.lpcs.as_mut() }
        && details.style as u32 & WS_CHILD.0 == 0
    {
        let input = CAPTURE_INPUTS.with(|inputs| {
            inputs
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .find(|input| input.owns(details.hwndParent))
        });
        if let Some(input) = input {
            let hwnd = HWND(value.0 as *mut _);
            if input.hook(hwnd).is_ok() {
                input.remember_popup(
                    hwnd,
                    POINT {
                        x: details.x,
                        y: details.y,
                    },
                );
                (details.x, details.y) = capture_origin((details.cx, details.cy));
            }
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, value, position) }
}

impl Drop for CaptureInput {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.creation_hook.get());
        }
        self.cancel();
        let ptr = self as *const Self as usize;
        for hwnd in self.hooks.get_mut().drain(..) {
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(capture_input_proc), ptr);
            }
        }
    }
}

fn with_capture_input<T>(hwnd: HWND, operation: impl FnOnce() -> T) -> T {
    let input = CAPTURE_INPUTS.with(|inputs| {
        inputs
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|input| input.owns(hwnd))
    });
    let _scope = input.as_ref().map(|input| input.enter());
    operation()
}

/// Cuts `container` to the parts of it nothing covers. A window region clips drawing, the
/// plugin's child windows included, and hit testing, so clicks in the holes reach the app.
unsafe fn set_region(container: HWND, spot: &Spot) {
    unsafe {
        if spot.whole() {
            SetWindowRgn(container, HRGN::default(), true);
            return;
        }
        let rgn = CreateRectRgn(0, 0, 0, 0);
        for r in spot.local() {
            let part = CreateRectRgn(r.x, r.y, r.x + r.w as i32, r.y + r.h as i32);
            CombineRgn(rgn, rgn, part, RGN_OR);
            let _ = DeleteObject(part);
        }
        // The window owns the region from here on.
        SetWindowRgn(container, rgn, true);
    }
}

/// What the window procedure shares with its [`HostWindow`].
struct State {
    /// Client size in physical pixels.
    size: Cell<(u32, u32)>,
    /// The user can't resize a top-level window with this set.
    fixed: Cell<bool>,
    capture_source: bool,
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
    /// Where the embedded editor was last put; None = hidden.
    shown: RefCell<Option<Spot>>,
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
                WM_MOUSEACTIVATE if s.capture_source => return LRESULT(MA_NOACTIVATE as isize),
                WM_WINDOWPOSCHANGING if s.capture_source => {
                    if let Some(position) = (lparam.0 as *mut WINDOWPOS).as_mut() {
                        let mut bounds = RECT::default();
                        let size = if position.flags & SWP_NOSIZE == SWP_NOSIZE
                            && GetWindowRect(hwnd, &mut bounds).is_ok()
                        {
                            (bounds.right - bounds.left, bounds.bottom - bounds.top)
                        } else {
                            (position.cx, position.cy)
                        };
                        (position.x, position.y) = capture_origin(size);
                        position.flags &= !SWP_NOMOVE;
                    }
                    return LRESULT(0);
                }
                WM_DISPLAYCHANGE if s.capture_source => {
                    let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    return LRESULT(0);
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

fn capture_origin((w, h): (i32, i32)) -> (i32, i32) {
    // The virtual desktop includes monitors left of and above the primary display.
    // Keep the complete source outside that bounding rectangle, including after resize.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN)
                .saturating_sub(w.max(1))
                .saturating_sub(32),
            GetSystemMetrics(SM_YVIRTUALSCREEN)
                .saturating_sub(h.max(1))
                .saturating_sub(32),
        )
    }
}

fn create(
    ex: WINDOW_EX_STYLE,
    title: &str,
    style: WINDOW_STYLE,
    (x, y, w, h): (i32, i32, i32, i32),
    parent: Option<HWND>,
) -> Result<HWND> {
    let title = HSTRING::from(title);
    unsafe {
        CreateWindowExW(ex, CLASS, &title, style, x, y, w, h, parent.unwrap_or_default(), None, instance(), None)
            .context("can't create the editor window")
    }
}

impl HostWindow {
    /// `_announce` is for Wine editors under X11; Windows plugins run natively here.
    pub fn create(
        placement: Placement,
        w: u32,
        h: u32,
        title: &str,
        _announce: bool,
    ) -> Result<Self> {
        register_class();
        let (w, h) = (w.clamp(16, 8192), h.clamp(16, 8192));
        let state = Box::new(State {
            size: Cell::new((w, h)),
            fixed: Cell::new(false),
            capture_source: placement == Placement::CaptureSource,
            events: RefCell::new(Vec::new()),
        });
        let (id, container) = match placement {
            Placement::Window => {
                let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
                let id = create(WINDOW_EX_STYLE::default(), title, style, (0, 0, w as i32, h as i32), None)?;
                (id, None)
            }
            Placement::CaptureSource => {
                let (x, y) = capture_origin((w as i32, h as i32));
                let id = create(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    "",
                    WS_POPUP | WS_CLIPCHILDREN,
                    (x, y, w as i32, h as i32),
                    None,
                )?;
                (id, None)
            }
            Placement::Embedded(parent) => {
                let parent = HWND(parent as *mut _);
                // Hidden until the host places it.
                let style = WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
                let container = create(WINDOW_EX_STYLE::default(), "", style, (0, 0, w as i32, h as i32), Some(parent))?;
                let id = match create(WINDOW_EX_STYLE::default(), "", style | WS_VISIBLE, (0, 0, w as i32, h as i32), Some(container)) {
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
            shown: RefCell::new(None),
            state,
        };
        if container.is_none() {
            // The window was created with the client size as its outer size.
            win.resize(w, h);
            unsafe {
                if placement == Placement::CaptureSource {
                    // Map only after offscreen positioning, without the first-ShowWindow
                    // startup-state override or an activation/visible desktop flash.
                    SetWindowPos(id, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW)
                        .context("show offscreen editor capture source")?;
                    ensure!(IsWindowVisible(id).as_bool(), "offscreen capture source did not map");
                } else {
                    let _ = ShowWindow(id, SW_SHOW);
                }
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
        if self.placement == Placement::CaptureSource {
            return;
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

    /// Moves an embedded editor to `spot`, cut to its visible parts, or hides it (`None`).
    pub fn place(&self, spot: Option<&Spot>) {
        let Some(container) = self.container else { return };
        let next = spot.filter(|s| !s.visible.is_empty());
        let mut shown = self.shown.borrow_mut();
        if next == shown.as_ref() {
            return;
        }
        unsafe {
            match next {
                Some(s) => {
                    let (c, (ox, oy)) = (s.clip, s.offset());
                    if shown.as_ref().is_none_or(|o| o.local().ne(s.local())) {
                        set_region(container, s);
                    }
                    let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
                    let _ = SetWindowPos(container, HWND_TOP, c.x, c.y, c.w as i32, c.h as i32, flags);
                    let _ = SetWindowPos(self.id, None, ox, oy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
                None => {
                    let _ = ShowWindow(container, SW_HIDE);
                }
            }
        }
        *shown = next.cloned();
    }

    /// What happened since the last call. The messages themselves are dispatched by the UI
    /// thread's event loop, except timers ([`pump_timers`]) and painting: the main window asks
    /// to be repainted again as soon as it's drawn while something plays, and Windows hands
    /// out its WM_PAINT first, so the plugin's would wait indefinitely. Painting ours and the
    /// plugin's windows here, directly, skips that queue (and does nothing when none needs it).
    pub fn poll(&self) -> Vec<WinEvent> {
        pump_timers();
        if self.placement != Placement::CaptureSource { unsafe {
            let _ = RedrawWindow(self.container.unwrap_or(self.id), None, HRGN::default(), RDW_UPDATENOW | RDW_ALLCHILDREN);
        } }
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
            with_capture_input(msg.hwnd, || DispatchMessageW(&msg));
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
