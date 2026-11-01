//! The Cocoa view a plugin editor draws into, shared by the VST3 and CLAP hosts; the macOS
//! counterpart of `x11win` and `win32win`.
//!
//! The plugin gets an NSView and adds its own views under it. That view is either the content
//! view of a window of its own, or embedded in the app's main view inside a clipping
//! "container" view, so it can be scrolled and partially covered by the host's layout. Sizes
//! and positions here are in points, the unit macOS plugins use; they handle Retina displays
//! themselves. AppKit (driven by winit's event loop) delivers the views' events and runs the
//! plugins' timers, so nothing needs pumping.

use std::cell::{Cell, RefCell};

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{DefinedClass, Encoding, MainThreadMarker, MainThreadOnly, RefEncode, define_class, msg_send, sel};
use objc2_app_kit::{NSBackingStoreType, NSResponder, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};

use super::{Placement, Spot, WinEvent, WindowId};

#[derive(Default)]
struct ContainerIvars {
    /// The parts of the container nothing covers, in its own coordinates; empty when it's
    /// all visible.
    visible: RefCell<Vec<NSRect>>,
}

define_class!(
    /// The clipping parent of an embedded editor. Clicks where something of the app's covers
    /// it go through to the app's view below.
    // SAFETY: NSView may be subclassed; hitTest: keeps its contract (a descendant or nil).
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NodeplayerEditorContainer"]
    #[ivars = ContainerIvars]
    struct Container;

    impl Container {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            {
                let visible = self.ivars().visible.borrow();
                if !visible.is_empty() {
                    // `point` is in the superview's coordinates.
                    let superview = unsafe { self.superview() };
                    let p = self.convertPoint_fromView(point, superview.as_deref());
                    let inside = |r: &NSRect| {
                        p.x >= r.origin.x && p.x < r.origin.x + r.size.width && p.y >= r.origin.y && p.y < r.origin.y + r.size.height
                    };
                    if !visible.iter().any(inside) {
                        return std::ptr::null_mut();
                    }
                }
            }
            unsafe { msg_send![super(self), hitTest: point] }
        }
    }
);

impl Container {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ContainerIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// Core Graphics' color, only passed from NSColor to CALayer here.
#[repr(C)]
struct CGColor {
    _private: [u8; 0],
}

// SAFETY: CGColorRef is a pointer to the opaque struct CGColor.
unsafe impl RefEncode for CGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}

/// Masks `container`'s layer to `parts` (in its coordinates), or takes the mask off if there
/// are none. The mask cuts off everything below the layer, the plugin's views included.
fn set_mask(container: &NSView, parts: &[NSRect]) {
    let Some(layer): Option<Retained<AnyObject>> = (unsafe { msg_send![container, layer] }) else { return };
    let (Some(ca_layer), Some(transaction), Some(ns_color)) =
        (AnyClass::get(c"CALayer"), AnyClass::get(c"CATransaction"), AnyClass::get(c"NSColor"))
    else {
        return;
    };
    unsafe {
        // No fading in and out: the mask follows the layout from frame to frame.
        let _: () = msg_send![transaction, begin];
        let _: () = msg_send![transaction, setDisableActions: true];
        if parts.is_empty() {
            let _: () = msg_send![&*layer, setMask: None::<&AnyObject>];
        } else {
            let mask: Retained<AnyObject> = msg_send![ca_layer, layer];
            let _: () = msg_send![&*mask, setFrame: container.bounds()];
            let black: Retained<AnyObject> = msg_send![ns_color, blackColor];
            let cg: *const CGColor = msg_send![&*black, CGColor];
            for r in parts {
                let part: Retained<AnyObject> = msg_send![ca_layer, layer];
                let _: () = msg_send![&*part, setFrame: *r];
                let _: () = msg_send![&*part, setBackgroundColor: cg];
                let _: () = msg_send![&*mask, addSublayer: &*part];
            }
            let _: () = msg_send![&*layer, setMask: &*mask];
        }
        let _: () = msg_send![transaction, commit];
    }
}

pub struct HostWindow {
    /// The view handed to the plugin.
    view: Retained<NSView>,
    /// Clipping parent of `view` when embedded.
    container: Option<Retained<Container>>,
    /// The editor's own window otherwise.
    window: Option<Retained<NSWindow>>,
    pub placement: Placement,
    /// Content scale the plugin currently draws at, and its size at scale 1.
    pub scale: Cell<f32>,
    pub base: Cell<(f32, f32)>,
    /// Size of `view` in points.
    size: Cell<(u32, u32)>,
    /// Where the embedded editor was last put; None = hidden.
    shown: RefCell<Option<Spot>>,
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
                let container = Container::new(mtm, frame);
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
            shown: RefCell::new(None),
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
                if let Some(shown) = self.shown.borrow().as_ref() {
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

    /// Moves an embedded editor to `spot` (in points from the top left of the parent view),
    /// cut to its visible parts, or hides it (`None`).
    pub fn place(&self, spot: Option<&Spot>) {
        let Some(container) = &self.container else { return };
        let next = spot.filter(|s| !s.visible.is_empty());
        let mut shown = self.shown.borrow_mut();
        if next == shown.as_ref() {
            return;
        }
        match next {
            Some(s) => {
                self.apply(s);
                container.setHidden(false);
            }
            None => container.setHidden(true),
        }
        *shown = next.cloned();
    }

    fn apply(&self, spot: &Spot) {
        let Some(container) = &self.container else { return };
        let Some(parent) = (unsafe { container.superview() }) else { return };
        let (c, (ox, oy)) = (spot.clip, spot.offset());
        let (cw, ch) = (c.w as f64, c.h as f64);
        let y = if parent.isFlipped() { c.y as f64 } else { parent.bounds().size.height - c.y as f64 - ch };
        container.setFrame(rect(c.x as f64, y, cw, ch));
        let vh = self.size.get().1 as f64;
        let flipped = container.isFlipped();
        let vy = if flipped { oy as f64 } else { ch - oy as f64 - vh };
        self.view.setFrameOrigin(NSPoint::new(ox as f64, vy));
        let parts: Vec<NSRect> = if spot.whole() {
            Vec::new()
        } else {
            spot.local()
                .map(|r| {
                    let (w, h) = (r.w as f64, r.h as f64);
                    let y = if flipped { r.y as f64 } else { ch - r.y as f64 - h };
                    rect(r.x as f64, y, w, h)
                })
                .collect()
        };
        if *container.ivars().visible.borrow() != parts {
            set_mask(container, &parts);
            *container.ivars().visible.borrow_mut() = parts;
        }
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
