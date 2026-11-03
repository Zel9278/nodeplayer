//! VST3 editors: an IPlugView in a [`HostWindow`], plus the IPlugFrame / IRunLoop the plugin
//! needs on Linux. On Windows the plugin's windows get their messages from the UI thread's
//! event loop like ours, so the run loop goes unused there.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use vst3::Steinberg::Linux::*;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use crate::plugins::window::HostWindow;
use crate::plugins::{Placement, Spot, WinEvent};

/// The kind of window a VST3 editor is attached to on this platform.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const PLATFORM_TYPE: FIDString = kPlatformTypeX11EmbedWindowID;
#[cfg(target_os = "windows")]
const PLATFORM_TYPE: FIDString = kPlatformTypeHWND;
#[cfg(target_os = "macos")]
const PLATFORM_TYPE: FIDString = kPlatformTypeNSView;

struct Timer {
    handler: ComPtr<ITimerHandler>,
    interval: Duration,
    next: Instant,
}

/// IPlugFrame + IRunLoop. The run loop is pumped from the UI thread via [`Editor::idle`].
pub struct PlugFrame {
    window: Rc<HostWindow>,
    resizable: Cell<bool>,
    fds: RefCell<Vec<(ComPtr<IEventHandler>, FileDescriptor)>>,
    timers: RefCell<Vec<Timer>>,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if view.is_null() || new_size.is_null() {
            return kInvalidArgument;
        }
        unsafe {
            let r = *new_size;
            let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
            self.window.set_fixed_size(w, h, !self.resizable.get());
            self.window.resize(w, h);
            ((*(*view).vtbl).onSize)(view, new_size);
        }
        kResultOk
    }
}

impl IRunLoopTrait for PlugFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: FileDescriptor) -> tresult {
        // The handler is lent to us; to_com_ptr takes our own reference.
        match unsafe { ComRef::from_raw(handler) }.map(|r| r.to_com_ptr()) {
            Some(h) => {
                log::debug!("run loop: register fd {fd}");
                self.fds.borrow_mut().push((h, fd));
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        self.fds.borrow_mut().retain(|(h, _)| h.as_ptr() != handler);
        kResultOk
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, ms: TimerInterval) -> tresult {
        match unsafe { ComRef::from_raw(handler) }.map(|r| r.to_com_ptr()) {
            Some(h) => {
                log::debug!("run loop: register timer {ms} ms");
                let interval = Duration::from_millis(ms.max(1));
                self.timers.borrow_mut().push(Timer { handler: h, interval, next: Instant::now() + interval });
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        self.timers.borrow_mut().retain(|t| t.handler.as_ptr() != handler);
        kResultOk
    }
}

impl PlugFrame {
    /// Runs ready fd handlers and due timers. Callbacks may (un)register handlers re-entrantly,
    /// so the lists are snapshotted before calling out.
    fn pump(&self) {
        #[cfg(unix)]
        let fds: Vec<_> = self.fds.borrow().iter().map(|(h, fd)| (h.clone(), *fd)).collect();
        #[cfg(unix)]
        if !fds.is_empty() {
            let mut pfds: Vec<libc::pollfd> =
                fds.iter().map(|&(_, fd)| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 0) };
            if n > 0 {
                for ((h, fd), p) in fds.iter().zip(&pfds) {
                    if p.revents != 0 {
                        log::trace!("run loop: fd {fd} ready");
                        unsafe { h.onFDIsSet(*fd) };
                    }
                }
            }
        }

        let now = Instant::now();
        let due: Vec<_> = self
            .timers
            .borrow_mut()
            .iter_mut()
            .filter(|t| t.next <= now)
            .map(|t| {
                t.next = now + t.interval;
                t.handler.clone()
            })
            .collect();
        for h in due {
            unsafe { h.onTimer() };
        }
    }
}

pub struct Editor {
    view: ComPtr<IPlugView>,
    frame: ComWrapper<PlugFrame>,
    window: Rc<HostWindow>,
    #[cfg(not(target_os = "macos"))]
    content_scale: Option<ComPtr<IPlugViewContentScaleSupport>>,
    requested_display_scale: f32,
    native_units_per_point: f32,
}

impl Editor {
    /// `announce` keeps the plugin's windows informed of their screen position; see
    /// [`HostWindow::poll`].
    pub fn open(controller: &ComPtr<IEditController>, title: &str, placement: Placement, announce: bool) -> Result<Self> {
        unsafe {
            let view = ComPtr::from_raw(controller.createView(ViewType::kEditor))
                .ok_or_else(|| anyhow!("plugin has no editor"))?;
            if view.isPlatformTypeSupported(PLATFORM_TYPE) != kResultTrue {
                bail!("editor doesn't support {}", std::ffi::CStr::from_ptr(PLATFORM_TYPE).to_string_lossy());
            }

            let mut rect: ViewRect = std::mem::zeroed();
            view.getSize(&mut rect);
            let w = (rect.right - rect.left).max(64) as u32;
            let h = (rect.bottom - rect.top).max(64) as u32;
            let resizable = view.canResize() == kResultTrue;

            let window = Rc::new(HostWindow::create(placement, w, h, title, announce)?);
            window.set_fixed_size(w, h, !resizable);
            let frame = ComWrapper::new(PlugFrame {
                window: window.clone(),
                resizable: Cell::new(resizable),
                fds: RefCell::new(Vec::new()),
                timers: RefCell::new(Vec::new()),
            });
            view.setFrame(frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr());
            if view.attached(window.handle() as *mut c_void, PLATFORM_TYPE) != kResultOk {
                view.setFrame(std::ptr::null_mut());
                bail!("IPlugView::attached failed");
            }
            // Some plugins only know their real size after attaching.
            if view.getSize(&mut rect) == kResultOk {
                let (w2, h2) = ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32);
                if (w2, h2) != (w, h) && w2 > 0 && h2 > 0 {
                    window.set_fixed_size(w2, h2, !resizable);
                    window.resize(w2, h2);
                }
            }
            log::debug!("{title}: editor {w}x{h}, resizable {resizable}");
            #[cfg(not(target_os = "macos"))]
            let content_scale = view.cast::<IPlugViewContentScaleSupport>();
            let factor = crate::plugins::native_display_scale(window.handle());
            let mut editor = Self {
                view, frame, window,
                #[cfg(not(target_os = "macos"))]
                content_scale,
                requested_display_scale: 1.0,
                native_units_per_point: 1.0,
            };
            crate::plugins::Editor::set_display_scale(&mut editor, factor);
            Ok(editor)
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn window_id(&self) -> u32 {
        self.window.id
    }

}

impl crate::plugins::Editor for Editor {
    fn placement(&self) -> Placement {
        self.window.placement
    }

    fn size(&self) -> (u32, u32) {
        self.window.size()
    }

    fn set_display_scale(&mut self, factor: f32) {
        if !crate::plugins::surface::valid_display_scale(factor)
            || (factor - self.requested_display_scale).abs() < 1e-3
        {
            return;
        }
        self.requested_display_scale = factor;
        #[cfg(not(target_os = "macos"))]
        if let Some(content_scale) = &self.content_scale {
            unsafe {
                if content_scale.setContentScaleFactor(factor) == kResultOk {
                    self.native_units_per_point = factor;
                    let mut rect: ViewRect = std::mem::zeroed();
                    if self.view.getSize(&mut rect) == kResultOk {
                        let width = rect.right - rect.left;
                        let height = rect.bottom - rect.top;
                        if width > 0 && height > 0 {
                            self.window.set_fixed_size(width as u32, height as u32, !self.frame.resizable.get());
                            self.window.resize(width as u32, height as u32);
                        }
                    }
                }
            }
        }
    }

    fn native_units_per_point(&self) -> f32 {
        self.native_units_per_point
    }

    fn native_handle(&self) -> Option<crate::plugins::WindowId> {
        Some(self.window.handle())
    }

    fn place(&mut self, spot: Option<&Spot>) {
        self.window.place(spot);
    }

    fn idle(&mut self) -> bool {
        #[cfg(target_os = "windows")]
        if self.window.placement == Placement::Window {
            self.set_display_scale(crate::plugins::native_display_scale(self.window.handle()));
        }
        self.frame.pump();
        let mut open = true;
        for ev in self.window.poll() {
            match ev {
                WinEvent::Close => open = false,
                WinEvent::Resized(w, h) if self.frame.resizable.get() => {
                    let mut r = ViewRect { left: 0, top: 0, right: w as i32, bottom: h as i32 };
                    unsafe {
                        if self.view.checkSizeConstraint(&mut r) == kResultTrue {
                            self.view.onSize(&mut r);
                        }
                    }
                }
                WinEvent::Resized(..) => {}
            }
        }
        open
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // The window itself is destroyed after this, when the last Rc goes.
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
    }
}
