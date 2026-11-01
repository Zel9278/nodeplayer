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
            // The plugin resized itself (its own resize handle, or answering a scale change).
            let s = self.window.scale.get();
            self.window.base.set((w as f32 / s, h as f32 / s));
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
    content_scale: Option<ComPtr<IPlugViewContentScaleSupport>>,
    /// Zoom requested by the host and when it last changed, and the zoom last applied.
    want_zoom: (f32, Instant),
    zoom: f32,
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
                    window.base.set((w2 as f32, h2 as f32));
                }
            }
            let content_scale = view.cast::<IPlugViewContentScaleSupport>();
            log::debug!("{title}: editor {w}x{h}, resizable {resizable}, content scale {}", content_scale.is_some());
            Ok(Self { view, frame, window, content_scale, want_zoom: (1.0, Instant::now()), zoom: 1.0 })
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn window_id(&self) -> u32 {
        self.window.id
    }

    fn apply_zoom(&mut self) {
        let (f, since) = self.want_zoom;
        if (f - self.zoom).abs() < 1e-3 || since.elapsed() < Duration::from_millis(150) {
            return;
        }
        self.zoom = f;
        let w = &self.window;
        let (bw, bh) = w.base.get();
        unsafe {
            if let Some(cs) = &self.content_scale {
                // The plugin rescales its GUI and normally answers with resizeView.
                w.scale.set(f);
                cs.setContentScaleFactor(f);
                let mut r: ViewRect = std::mem::zeroed();
                if self.view.getSize(&mut r) == kResultOk {
                    let size = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                    if size != w.size() && size.0 > 0 && size.1 > 0 {
                        w.resize(size.0, size.1);
                    }
                }
            } else if self.frame.resizable.get() {
                // No scaling support: a resizable editor at least gets the zoomed area.
                let mut r = ViewRect { left: 0, top: 0, right: (bw * f).round() as i32, bottom: (bh * f).round() as i32 };
                if self.view.checkSizeConstraint(&mut r) == kResultTrue {
                    w.resize((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                    self.view.onSize(&mut r);
                }
            }
        }
        log::debug!("editor zoom {f:.2}: {:?}", self.window.size());
    }
}

impl crate::plugins::Editor for Editor {
    fn placement(&self) -> Placement {
        self.window.placement
    }

    fn size(&self) -> (u32, u32) {
        self.window.size()
    }

    /// Applied once the value has settled for a moment, since re-laying out a plugin GUI is slow.
    fn set_zoom(&mut self, factor: f32) {
        let f = ((factor * 100.0).round() / 100.0).clamp(0.25, 4.0);
        if (f - self.want_zoom.0).abs() > 1e-3 {
            self.want_zoom = (f, Instant::now());
        }
    }

    fn place(&mut self, spot: Option<&Spot>) {
        self.window.place(spot);
    }

    fn idle(&mut self) -> bool {
        self.frame.pump();
        self.apply_zoom();
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
