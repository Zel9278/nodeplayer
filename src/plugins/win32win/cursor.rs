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
    use windows::Win32::Foundation::{BOOL, ERROR_NOACCESS, SetLastError};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::{PCSTR, w};

    type Read = unsafe extern "system" fn(*mut POINT) -> BOOL;
    type Write = unsafe extern "system" fn(i32, i32) -> BOOL;
    type Message = unsafe extern "system" fn() -> u32;
    type Foreground = unsafe extern "system" fn() -> HWND;
    static GET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static PHYSICAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static SET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static MESSAGE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static FOREGROUND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
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
                unsafe { MinHook::enable_hook(address) }
                    .map_err(|error| anyhow!("enable editor cursor API hook: {error:?}"))?;
                installed.push((address, trampoline));
                Ok(())
            })();
            if let Err(error) = result {
                for (address, _) in installed {
                    let _ = unsafe { MinHook::disable_hook(address) };
                }
                // Hooks and trampolines live until process exit. Another thread may
                // already be returning through one even after it has been disabled.
                bail!(error);
            }
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
}
