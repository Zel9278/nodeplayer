//! Cursor and foreground queries use the captured editor only during its callbacks.
//! Moving the desktop cursor to an offscreen HWND would clamp it to a monitor edge
//! and generate new input for the graph, including during an active drag.

use std::cell::Cell;

use anyhow::Result;
use windows::Win32::Foundation::{HWND, POINT};

#[derive(Clone, Copy)]
struct Position {
    current: POINT,
    message: POINT,
}

pub(super) struct Cursor {
    position: Cell<Option<Position>>,
    root: HWND,
}

thread_local! {
    static CURRENT: Cell<*const Cursor> = const { Cell::new(std::ptr::null()) };
    static COORDINATES: Cell<bool> = const { Cell::new(false) };
}

pub(super) struct CoordinateScope(bool);

pub(super) fn coordinates(enabled: bool) -> CoordinateScope {
    CoordinateScope(COORDINATES.with(|current| current.replace(enabled)))
}

impl Drop for CoordinateScope {
    fn drop(&mut self) {
        let _ = COORDINATES.try_with(|current| current.set(self.0));
    }
}

fn offset(hwnd: HWND) -> POINT {
    if !COORDINATES.try_with(Cell::get).unwrap_or(false) {
        return POINT::default();
    }
    // Host-side geometry reads inside popup_offset must retain physical coordinates.
    let _physical = coordinates(false);
    super::CaptureInput::current()
        .and_then(|input| input.popup_offset(hwnd))
        .unwrap_or_default()
}

impl Cursor {
    pub(super) fn new(root: HWND) -> Self {
        Self {
            position: Cell::new(None),
            root,
        }
    }

    pub(super) fn set(&self, point: POINT) {
        self.position.set(Some(Position {
            current: point,
            message: point,
        }));
    }

    pub(super) fn cancel(&self) {
        self.position.set(None);
    }

    pub(super) fn enter(&self) -> Scope<'_> {
        Scope {
            previous: CURRENT.with(|current| current.replace(self)),
            _cursor: self,
        }
    }
}

pub(super) struct Scope<'a> {
    previous: *const Cursor,
    // Keeps the TLS pointer alive and confines the scope to this UI thread.
    _cursor: &'a Cursor,
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|current| current.set(self.previous));
    }
}

fn position() -> Option<Position> {
    CURRENT
        .try_with(|current| {
            // The active scope borrows this Cursor until the previous pointer is restored.
            unsafe { current.get().as_ref() }.and_then(|cursor| cursor.position.get())
        })
        .ok()
        .flatten()
}

pub(super) fn initialize() -> Result<()> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    return hooks::initialize();
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    Ok(())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod hooks {
    use super::*;
    use anyhow::{anyhow, bail, ensure};
    use minhook::MinHook;
    use std::ffi::c_void;
    use std::sync::{
        OnceLock,
        atomic::{AtomicPtr, Ordering},
    };
    use windows::Win32::Foundation::{BOOL, ERROR_NOACCESS, RECT, SetLastError};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::{PCSTR, w};

    type Read = unsafe extern "system" fn(*mut POINT) -> BOOL;
    type Write = unsafe extern "system" fn(i32, i32) -> BOOL;
    type Message = unsafe extern "system" fn() -> u32;
    type Foreground = unsafe extern "system" fn() -> HWND;
    type Focus = unsafe extern "system" fn(HWND) -> HWND;
    type AsyncKey = unsafe extern "system" fn(i32) -> i16;
    type Bounds = unsafe extern "system" fn(HWND, *mut RECT) -> BOOL;
    type Convert = unsafe extern "system" fn(HWND, *mut POINT) -> BOOL;
    type Map = unsafe extern "system" fn(HWND, HWND, *mut POINT, u32) -> i32;
    type WindowAt = unsafe extern "system" fn(POINT) -> HWND;
    static GET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static PHYSICAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static SET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static MESSAGE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static FOREGROUND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static GET_FOCUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static SET_FOCUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static ASYNC_KEY: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static BOUNDS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static TO_SCREEN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static FROM_SCREEN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static MAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static AT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    pub(super) fn initialize() -> Result<()> {
        INSTALLED
            .get_or_init(|| {
                std::panic::catch_unwind(install)
                    .unwrap_or_else(|_| Err(anyhow!("editor cursor hook initialization panicked")))
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|error| anyhow!(error.clone()))
            .copied()
    }

    fn install() -> Result<()> {
        let module = unsafe { GetModuleHandleW(w!("user32.dll")) }?;
        let mut installed: Vec<(*mut c_void, *mut c_void)> = Vec::new();
        for (name, replacement, original) in [
            (
                b"GetCursorPos\0".as_slice(),
                get as *const () as *mut c_void,
                &GET,
            ),
            (
                b"GetPhysicalCursorPos\0".as_slice(),
                physical as *const () as *mut c_void,
                &PHYSICAL,
            ),
            (
                b"SetCursorPos\0".as_slice(),
                set as *const () as *mut c_void,
                &SET,
            ),
            (
                b"GetMessagePos\0".as_slice(),
                message as *const () as *mut c_void,
                &MESSAGE,
            ),
            (
                b"GetForegroundWindow\0".as_slice(),
                foreground as *const () as *mut c_void,
                &FOREGROUND,
            ),
            (b"GetFocus\0".as_slice(), get_focus as *const () as *mut c_void, &GET_FOCUS),
            (b"SetFocus\0".as_slice(), set_focus as *const () as *mut c_void, &SET_FOCUS),
            (b"GetAsyncKeyState\0".as_slice(), async_key as *const () as *mut c_void, &ASYNC_KEY),
            (
                b"GetWindowRect\0".as_slice(),
                bounds as *const () as *mut c_void,
                &BOUNDS,
            ),
            (
                b"ClientToScreen\0".as_slice(),
                to_screen as *const () as *mut c_void,
                &TO_SCREEN,
            ),
            (
                b"ScreenToClient\0".as_slice(),
                from_screen as *const () as *mut c_void,
                &FROM_SCREEN,
            ),
            (
                b"MapWindowPoints\0".as_slice(),
                map as *const () as *mut c_void,
                &MAP,
            ),
            (
                b"WindowFromPoint\0".as_slice(),
                window_at as *const () as *mut c_void,
                &AT,
            ),
        ] {
            let result = (|| -> Result<()> {
                let address = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
                    .ok_or_else(|| anyhow!("editor cursor API unavailable"))?
                    as *const () as *mut c_void;
                // GetPhysicalCursorPos can export the same entry point as
                // GetCursorPos. Both use the same read signature and behavior.
                if let Some((_, trampoline)) =
                    installed.iter().find(|(target, _)| *target == address)
                {
                    ensure!(
                        std::ptr::eq(original, &PHYSICAL),
                        "cursor API exports have incompatible signatures"
                    );
                    original.store(*trampoline, Ordering::Release);
                    return Ok(());
                }
                let trampoline =
                    unsafe { MinHook::create_hook(address, replacement) }.map_err(|error| {
                        anyhow!(
                            "hook {}: {error:?}",
                            String::from_utf8_lossy(&name[..name.len() - 1])
                        )
                    })?;
                original.store(trampoline, Ordering::Release);
                installed.push((address, trampoline));
                Ok(())
            })();
            if let Err(error) = result {
                for (address, _) in installed {
                    let _ = unsafe { MinHook::remove_hook(address) };
                }
                bail!(error);
            }
        }
        // Enabling each hook separately repeats MinHook's process-wide thread scan
        // and suspension. On busy machines that exceeds the worker's reply deadline.
        // Publish every trampoline first, then activate only these hooks in one pass.
        let enabled = (|| -> Result<()> {
            for &(address, _) in &installed {
                unsafe { MinHook::queue_enable_hook(address) }
                    .map_err(|error| anyhow!("queue editor cursor API hook: {error:?}"))?;
            }
            unsafe { MinHook::apply_queued() }
                .map_err(|error| anyhow!("enable editor cursor API hooks: {error:?}"))
        })();
        if let Err(error) = enabled {
            for &(address, _) in &installed {
                let _ = unsafe { MinHook::queue_disable_hook(address) };
            }
            let _ = unsafe { MinHook::apply_queued() };
            // An activation can partially succeed. Retain its trampolines for any
            // callbacks already in flight, even after disabling the hooks.
            bail!(error);
        }
        Ok(())
    }

    unsafe fn read(point: *mut POINT, original: &AtomicPtr<c_void>) -> BOOL {
        if let Some(position) = position() {
            let Some(point) = (unsafe { point.as_mut() }) else {
                unsafe { SetLastError(ERROR_NOACCESS) };
                return BOOL(0);
            };
            *point = position.current;
            return BOOL(1);
        }
        let original: Read = unsafe { std::mem::transmute(original.load(Ordering::Acquire)) };
        unsafe { original(point) }
    }

    unsafe extern "system" fn get(point: *mut POINT) -> BOOL {
        unsafe { read(point, &GET) }
    }

    unsafe extern "system" fn physical(point: *mut POINT) -> BOOL {
        unsafe { read(point, &PHYSICAL) }
    }

    unsafe extern "system" fn set(x: i32, y: i32) -> BOOL {
        let handled = CURRENT
            .try_with(|current| {
                let Some(cursor) = (unsafe { current.get().as_ref() }) else {
                    return false;
                };
                let point = POINT { x, y };
                let mut position = cursor.position.get().unwrap_or(Position {
                    current: point,
                    message: point,
                });
                position.current = point;
                cursor.position.set(Some(position));
                true
            })
            .unwrap_or(false);
        if handled {
            return BOOL(1);
        }
        let original: Write = unsafe { std::mem::transmute(SET.load(Ordering::Acquire)) };
        unsafe { original(x, y) }
    }

    unsafe extern "system" fn message() -> u32 {
        if let Some(position) = position() {
            return u32::from(position.message.x as u16)
                | (u32::from(position.message.y as u16) << 16);
        }
        let original: Message = unsafe { std::mem::transmute(MESSAGE.load(Ordering::Acquire)) };
        unsafe { original() }
    }

    unsafe extern "system" fn foreground() -> HWND {
        if let Some(root) = CURRENT
            .try_with(|current| {
                unsafe { current.get().as_ref() }
                    .filter(|cursor| cursor.position.get().is_some())
                    .map(|cursor| cursor.root)
            })
            .ok()
            .flatten()
        {
            return root;
        }
        let original: Foreground =
            unsafe { std::mem::transmute(FOREGROUND.load(Ordering::Acquire)) };
        unsafe { original() }
    }

    unsafe extern "system" fn get_focus() -> HWND {
        if COORDINATES.try_with(Cell::get).unwrap_or(false)
            && let Some(focus) = super::super::CaptureInput::current().and_then(|input| input.focus_target())
        {
            return focus;
        }
        let original: Foreground = unsafe { std::mem::transmute(GET_FOCUS.load(Ordering::Acquire)) };
        unsafe { original() }
    }

    unsafe extern "system" fn set_focus(target: HWND) -> HWND {
        if COORDINATES.try_with(Cell::get).unwrap_or(false)
            && let Some(input) = super::super::CaptureInput::current()
            && (input.contains(target) || target.0.is_null() && input.focus_target().is_some())
        {
            let previous = unsafe { get_focus() };
            if target.0.is_null() { input.blur_editor(); }
            else { input.focus_window(target); }
            return previous;
        }
        let original: Focus = unsafe { std::mem::transmute(SET_FOCUS.load(Ordering::Acquire)) };
        unsafe { original(target) }
    }

    unsafe extern "system" fn async_key(code: i32) -> i16 {
        if (0..256).contains(&code) && COORDINATES.try_with(Cell::get).unwrap_or(false)
            && let Some(input) = super::super::CaptureInput::current()
        {
            // Timers and helper windows run between forwarded events, after the
            // host's keyboard table has been restored. Retain the editor's state
            // for those callbacks as well as synchronous key and mouse messages.
            return input.key_state(code as usize).unwrap_or_else(|| {
                (unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(code) }) & i16::MIN
            });
        }
        let original: AsyncKey = unsafe { std::mem::transmute(ASYNC_KEY.load(Ordering::Acquire)) };
        unsafe { original(code) }
    }

    unsafe extern "system" fn bounds(hwnd: HWND, rect: *mut RECT) -> BOOL {
        let original: Bounds = unsafe { std::mem::transmute(BOUNDS.load(Ordering::Acquire)) };
        let result = unsafe { original(hwnd, rect) };
        if result.as_bool()
            && let Some(rect) = unsafe { rect.as_mut() }
        {
            let delta = offset(hwnd);
            rect.left = rect.left.saturating_add(delta.x);
            rect.right = rect.right.saturating_add(delta.x);
            rect.top = rect.top.saturating_add(delta.y);
            rect.bottom = rect.bottom.saturating_add(delta.y);
        }
        result
    }

    unsafe extern "system" fn to_screen(hwnd: HWND, point: *mut POINT) -> BOOL {
        let original: Convert = unsafe { std::mem::transmute(TO_SCREEN.load(Ordering::Acquire)) };
        let result = unsafe { original(hwnd, point) };
        if result.as_bool()
            && let Some(point) = unsafe { point.as_mut() }
        {
            let delta = offset(hwnd);
            point.x = point.x.saturating_add(delta.x);
            point.y = point.y.saturating_add(delta.y);
        }
        result
    }

    unsafe extern "system" fn from_screen(hwnd: HWND, point: *mut POINT) -> BOOL {
        let original: Convert = unsafe { std::mem::transmute(FROM_SCREEN.load(Ordering::Acquire)) };
        let delta = offset(hwnd);
        if let Some(point) = unsafe { point.as_mut() } {
            point.x = point.x.saturating_sub(delta.x);
            point.y = point.y.saturating_sub(delta.y);
        }
        unsafe { original(hwnd, point) }
    }

    unsafe extern "system" fn map(from: HWND, to: HWND, points: *mut POINT, count: u32) -> i32 {
        let original: Map = unsafe { std::mem::transmute(MAP.load(Ordering::Acquire)) };
        let result = unsafe { original(from, to, points, count) };
        let source = offset(from);
        let destination = offset(to);
        let dx = source.x.saturating_sub(destination.x);
        let dy = source.y.saturating_sub(destination.y);
        if (dx, dy) == (0, 0) || points.is_null() {
            return result;
        }
        for point in unsafe { std::slice::from_raw_parts_mut(points, count as usize) } {
            point.x = point.x.saturating_add(dx);
            point.y = point.y.saturating_add(dy);
        }
        let x = (result as i16 as i32).saturating_add(dx);
        let y = ((result >> 16) as i16 as i32).saturating_add(dy);
        (u32::from(x as u16) | (u32::from(y as u16) << 16)) as i32
    }

    unsafe extern "system" fn window_at(point: POINT) -> HWND {
        if COORDINATES.try_with(Cell::get).unwrap_or(false) {
            let _physical = coordinates(false);
            if let Some(target) = super::super::CaptureInput::current()
                .and_then(|input| input.window_from_point(point))
            {
                return target;
            }
        }
        let original: WindowAt = unsafe { std::mem::transmute(AT.load(Ordering::Acquire)) };
        unsafe { original(point) }
    }
}
