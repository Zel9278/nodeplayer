//! CLAP editors: the plugin's GUI (X11, Win32 or Cocoa) parented to a [`HostWindow`].

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap_sys::ext::gui::*;
use clap_sys::plugin::clap_plugin;

use super::HostShared;
use crate::plugins::window::HostWindow;
use crate::plugins::{Placement, PluginInfo, PxRect, WinEvent};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const API: &std::ffi::CStr = CLAP_WINDOW_API_X11;
#[cfg(target_os = "windows")]
const API: &std::ffi::CStr = CLAP_WINDOW_API_WIN32;
#[cfg(target_os = "macos")]
const API: &std::ffi::CStr = CLAP_WINDOW_API_COCOA;

fn window_handle(window: &HostWindow) -> clap_window_handle {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    return clap_window_handle { x11: window.handle() as _ };
    #[cfg(target_os = "windows")]
    return clap_window_handle { win32: window.handle() as _ };
    #[cfg(target_os = "macos")]
    return clap_window_handle { cocoa: window.handle() as _ };
}

pub struct ClapEditor {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
    shared: *const HostShared,
    window: HostWindow,
    resizable: bool,
    /// Zoom requested by the host and when it last changed, and the zoom last applied.
    want_zoom: (f32, Instant),
    zoom: f32,
}

impl ClapEditor {
    /// # Safety
    /// `plugin`, `gui` and `shared` must outlive the editor.
    pub unsafe fn open(
        plugin: *const clap_plugin,
        gui: *const clap_plugin_gui,
        shared: *const HostShared,
        info: &PluginInfo,
        placement: Placement,
    ) -> Result<Self> {
        unsafe {
            let g = &*gui;
            let api = API.as_ptr();
            if !g.is_api_supported.is_some_and(|f| f(plugin, api, false)) {
                bail!("{} has no embeddable {} GUI", info.name, API.to_string_lossy());
            }
            if !g.create.is_some_and(|f| f(plugin, api, false)) {
                bail!("{}: GUI create failed", info.name);
            }
            let (mut w, mut h) = (0u32, 0u32);
            if !g.get_size.is_some_and(|f| f(plugin, &mut w, &mut h)) || w == 0 || h == 0 {
                (w, h) = (400, 300);
            }
            let resizable = g.can_resize.is_some_and(|f| f(plugin));
            let window = match HostWindow::create(placement, w, h, &info.name, info.is_bridged()) {
                Ok(win) => win,
                Err(e) => {
                    if let Some(d) = g.destroy {
                        d(plugin);
                    }
                    return Err(e);
                }
            };
            window.set_fixed_size(w, h, !resizable);
            let parent = clap_window { api, specific: window_handle(&window) };
            if !g.set_parent.is_some_and(|f| f(plugin, &parent)) {
                if let Some(d) = g.destroy {
                    d(plugin);
                }
                bail!("{}: set_parent failed", info.name);
            }
            if let Some(f) = g.show {
                f(plugin);
            }
            log::debug!("{}: CLAP GUI {w}x{h}, resizable {resizable}", info.name);
            Ok(Self { plugin, gui, shared, window, resizable, want_zoom: (1.0, Instant::now()), zoom: 1.0 })
        }
    }

    fn gui(&self) -> &clap_plugin_gui {
        unsafe { &*self.gui }
    }

    /// Asks the plugin for `w`×`h` (adjusted to what it accepts) and sizes our window to it.
    fn resize_to(&self, w: u32, h: u32) {
        let g = self.gui();
        let (mut w, mut h) = (w.max(1), h.max(1));
        unsafe {
            if let Some(adjust) = g.adjust_size {
                adjust(self.plugin, &mut w, &mut h);
            }
            if g.set_size.is_some_and(|f| f(self.plugin, w, h)) {
                self.window.resize(w, h);
            }
        }
    }

    fn apply_zoom(&mut self) {
        let (f, since) = self.want_zoom;
        if (f - self.zoom).abs() < 1e-3 || since.elapsed() < Duration::from_millis(150) {
            return;
        }
        self.zoom = f;
        let g = self.gui();
        unsafe {
            if g.set_scale.is_some_and(|s| s(self.plugin, f as f64)) {
                let (mut w, mut h) = (0u32, 0u32);
                if g.get_size.is_some_and(|s| s(self.plugin, &mut w, &mut h)) && w > 0 && h > 0 {
                    self.window.resize(w, h);
                }
            } else if self.resizable {
                let (bw, bh) = self.window.base.get();
                self.resize_to((bw * f).round() as u32, (bh * f).round() as u32);
            }
        }
        log::debug!("CLAP editor zoom {f:.2}: {:?}", self.window.size());
    }
}

impl crate::plugins::Editor for ClapEditor {
    fn placement(&self) -> Placement {
        self.window.placement
    }

    fn size(&self) -> (u32, u32) {
        self.window.size()
    }

    fn set_zoom(&mut self, factor: f32) {
        let f = ((factor * 100.0).round() / 100.0).clamp(0.25, 4.0);
        if (f - self.want_zoom.0).abs() > 1e-3 {
            self.want_zoom = (f, Instant::now());
        }
    }

    fn place(&mut self, clip: Option<PxRect>, origin: (i32, i32)) {
        self.window.place(clip, origin);
    }

    fn idle(&mut self) -> bool {
        let shared = unsafe { &*self.shared };
        if shared.gui_closed.swap(false, Ordering::AcqRel) {
            return false;
        }
        // The plugin resized itself.
        let req = shared.resize_request.swap(0, Ordering::AcqRel);
        if req != 0 {
            let (w, h) = ((req >> 32) as u32, req as u32);
            self.window.set_fixed_size(w, h, !self.resizable);
            self.window.resize(w, h);
            let s = self.zoom;
            self.window.base.set((w as f32 / s, h as f32 / s));
        }
        self.apply_zoom();
        let mut open = true;
        for ev in self.window.poll() {
            match ev {
                WinEvent::Close => open = false,
                WinEvent::Resized(w, h) if self.resizable => self.resize_to(w, h),
                WinEvent::Resized(..) => {}
            }
        }
        open
    }
}

impl Drop for ClapEditor {
    fn drop(&mut self) {
        // Before the window goes, which happens when the fields drop after this.
        unsafe {
            if let Some(d) = self.gui().destroy {
                d(self.plugin);
            }
        }
    }
}
