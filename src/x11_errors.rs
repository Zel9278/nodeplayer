//! Xlib error handlers are process-global. winit installs one that stores every error and
//! later panics on it, so an X error raised on a *plugin's* own connection (e.g. BadWindow
//! when it destroys a window whose parent we already removed) would bring the app down.
//! This handler forwards only errors on winit's display and logs the rest.

use std::ffi::{c_int, c_uchar, c_ulong, c_void};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

#[repr(C)]
struct XErrorEvent {
    kind: c_int,
    display: *mut c_void,
    resource_id: c_ulong,
    serial: c_ulong,
    error_code: c_uchar,
    request_code: c_uchar,
    minor_code: c_uchar,
}

type Handler = unsafe extern "C" fn(*mut c_void, *mut XErrorEvent) -> c_int;

#[link(name = "X11")]
unsafe extern "C" {
    fn XSetErrorHandler(handler: Option<Handler>) -> Option<Handler>;
}

static HOST_DISPLAY: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static PREVIOUS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn filter(display: *mut c_void, event: *mut XErrorEvent) -> c_int {
    let prev = PREVIOUS.load(Ordering::Acquire);
    if display == HOST_DISPLAY.load(Ordering::Acquire) && prev != 0 {
        let prev: Handler = unsafe { std::mem::transmute(prev) };
        return unsafe { prev(display, event) };
    }
    let e = unsafe { &*event };
    log::debug!(
        "ignored X error from a plugin connection: code {} request {}.{} resource 0x{:x}",
        e.error_code,
        e.request_code,
        e.minor_code,
        e.resource_id
    );
    0
}

/// Installs the filter once. `display` is winit's Xlib `Display*`.
pub fn install(display: *mut c_void) {
    if display.is_null() || !HOST_DISPLAY.load(Ordering::Acquire).is_null() {
        return;
    }
    HOST_DISPLAY.store(display, Ordering::Release);
    let prev = unsafe { XSetErrorHandler(Some(filter)) };
    PREVIOUS.store(prev.map_or(0, |h| h as usize), Ordering::Release);
}
