//! The Cocoa view a plugin editor draws into, shared by the VST3 and CLAP hosts; the macOS
//! counterpart of `x11win` and `win32win`.
//!
//! The plugin gets an NSView and adds its own views under it. That view is either the content
//! view of a window of its own, or embedded in the app's main view inside a clipping
//! "container" view, so it can be scrolled and partially covered by the host's layout. Sizes
//! and positions here are in points, the unit macOS plugins use; they handle Retina displays
//! themselves. AppKit (driven by winit's event loop) delivers the views' events and runs the
//! plugins' timers, so nothing needs pumping.

use std::cell::Cell;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::{Placement, PxRect, WinEvent, WindowId};

pub struct HostWindow {
    /// The view handed to the plugin.
    view: Retained<NSView>,
    /// Clipping parent of `view` when embedded.
    container: Option<Retained<NSView>>,
    /// The editor's own window otherwise.
    window: Option<Retained<NSWindow>>,
    pub placement: Placement,
    /// Content scale the plugin currently draws at, and its size at scale 1.
    pub scale: Cell<f32>,
    pub base: Cell<(f32, f32)>,
    /// Size of `view` in points.
    size: Cell<(u32, u32)>,
    /// Last applied embedded geometry: (container rect, plugin offset inside it); None = hidden.
    shown: Cell<Option<(PxRect, (i32, i32))>>,
    closed: Cell<bool>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn responds(obj: &AnyObject, sel: Sel) -> bool {
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// Makes `container` cut off what its subviews draw outside it, and keeps it above the layer
/// the app renders into (which is a sublayer of the same view, so plain view order isn't enough).
fn clip_and_raise(container: &NSView) {
    container.setWantsLayer(true);
    // AppKit's own switch, macOS 14 and later.
    if responds(container, sel!(setClipsToBounds:)) {
        unsafe {
            let _: () = msg_send![container, setClipsToBounds: true];
        }
    }
    unsafe {
        let layer: Option<Retained<AnyObject>> = msg_send![container, layer];
        if let Some(layer) = layer {
            let _: () = msg_send![&*layer, setMasksToBounds: true];
            let _: () = msg_send![&*layer, setZPosition: 1000.0f64];
        }
    }
}

const RESIZABLE: NSWindowStyleMask = NSWindowStyleMask::Resizable;

impl HostWindow {
    /// `_announce` is for Wine editors under X11; macOS plugins run natively here.
    pub fn create(placement: Placement, w: u32, h: u32, title: &str, _announce: bool) -> Result<Self> {
        let mtm = MainThreadMarker::new().context("plugin editors open on the main thread only")?;
        let (w, h) = (w.clamp(16, 8192), h.clamp(16, 8192));
        let frame = rect(0.0, 0.0, w as f64, h as f64);
        let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
        let (container, window) = match placement {
            Placement::Window => {
                let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | RESIZABLE;
                let window = unsafe {
                    NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, style, NSBackingStoreType::Buffered, false)
                };
                // Closing only hides it; we notice in `poll` and the owner drops us.
                unsafe { window.setReleasedWhenClosed(false) };
                window.setTitle(&NSString::from_str(title));
                window.setContentView(Some(&view));
                window.center();
                window.makeKeyAndOrderFront(None);
                (None, Some(window))
            }
            Placement::Embedded(parent) => {
                let parent = unsafe { (parent as *const NSView).as_ref() }.context("no parent view")?;
                let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
                // Hidden until the host places it.
                container.setHidden(true);
                clip_and_raise(&container);
                container.addSubview(&view);
                parent.addSubview(&container);
                (Some(container), None)
            }
        };
        Ok(Self {
            view,
            container,
            window,
            placement,
            scale: Cell::new(1.0),
            base: Cell::new((w as f32, h as f32)),
            size: Cell::new((w, h)),
            shown: Cell::new(None),
            closed: Cell::new(false),
        })
    }

    /// The view handed to the plugin.
    pub fn handle(&self) -> WindowId {
        Retained::as_ptr(&self.view) as WindowId
    }

    /// Size in points.
    pub fn size(&self) -> (u32, u32) {
        self.size.get()
    }

    pub fn resize(&self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        self.size.set((w, h));
        let size = NSSize::new(w as f64, h as f64);
        match &self.window {
            Some(window) => window.setContentSize(size),
            None => {
                self.view.setFrameSize(size);
                // Unflipped coordinates count from the bottom, so the offset depends on the size.
                if let Some(shown) = self.shown.get() {
                    self.apply(shown);
                }
            }
        }
    }

    /// Lets the user resize a top-level editor, or pins it to `w`×`h`.
    pub fn set_fixed_size(&self, w: u32, h: u32, fixed: bool) {
        let Some(window) = &self.window else { return };
        if fixed {
            self.size.set((w.max(1), h.max(1)));
        }
        let mut style = window.styleMask();
        style.set(RESIZABLE, !fixed);
        window.setStyleMask(style);
    }

    /// Positions an embedded editor. `clip` is the visible area in the parent view and
    /// `origin` where the editor's top-left corner would be (it may lie outside `clip` when
    /// scrolled), both in points from the top left. `None` hides the editor.
    pub fn place(&self, clip: Option<PxRect>, origin: (i32, i32)) {
        let Some(container) = &self.container else { return };
        let next = clip.filter(|c| c.w > 0 && c.h > 0).map(|c| (c, (origin.0 - c.x, origin.1 - c.y)));
        if next == self.shown.get() {
            return;
        }
        match next {
            Some(shown) => {
                self.apply(shown);
                container.setHidden(false);
            }
            None => container.setHidden(true),
        }
        self.shown.set(next);
    }

    fn apply(&self, (c, (ox, oy)): (PxRect, (i32, i32))) {
        let Some(container) = &self.container else { return };
        let Some(parent) = (unsafe { container.superview() }) else { return };
        let (cw, ch) = (c.w as f64, c.h as f64);
        let y = if parent.isFlipped() { c.y as f64 } else { parent.bounds().size.height - c.y as f64 - ch };
        container.setFrame(rect(c.x as f64, y, cw, ch));
        let vh = self.size.get().1 as f64;
        let vy = if container.isFlipped() { oy as f64 } else { ch - oy as f64 - vh };
        self.view.setFrameOrigin(NSPoint::new(ox as f64, vy));
    }

    /// What happened since the last call: the window was closed or resized by the user.
    pub fn poll(&self) -> Vec<WinEvent> {
        let Some(window) = &self.window else { return Vec::new() };
        let mut events = Vec::new();
        if !self.closed.get() && !window.isVisible() && !window.isMiniaturized() {
            self.closed.set(true);
            events.push(WinEvent::Close);
        }
        let s = self.view.frame().size;
        let size = (s.width.round() as u32, s.height.round() as u32);
        if size != self.size.get() && size.0 > 0 && size.1 > 0 {
            self.size.set(size);
            events.push(WinEvent::Resized(size.0, size.1));
        }
        events
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        match (&self.window, &self.container) {
            (Some(window), _) => {
                window.setContentView(None);
                window.close();
            }
            (None, Some(container)) => container.removeFromSuperview(),
            (None, None) => {}
        }
    }
}
