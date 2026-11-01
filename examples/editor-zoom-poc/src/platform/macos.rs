//! AppKit reference window and CPU snapshot backend for the zoom experiment.
//!
//! `cacheDisplayInRect:toBitmapImageRep:` captures AppKit drawing, including native
//! descendants. It is not a promise that an arbitrary Metal/OpenGL plugin exposes
//! its GPU contents to AppKit's bitmap drawing path. The reference window remains
//! visible so this experiment does not depend on hidden-window rendering.

use std::cell::Cell;
use std::ffi::c_void;

use anyhow::{Context, Result, ensure};
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{
    ClassType, DefinedClass, Encoding, MainThreadMarker, MainThreadOnly, Message, RefEncode,
    define_class, msg_send,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapFormat,
    NSBitmapImageFileType, NSBitmapImageRep, NSButton, NSColor, NSEvent, NSEventMask,
    NSEventModifierFlags, NSEventType, NSRectFill, NSResponder, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSRect, NSRunLoop, NSSize, NSString,
};

use super::{CapturedFrame, PointerEvent, PointerKind};

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    fn CACurrentMediaTime() -> f64;
}

#[repr(C)]
struct CGEvent {
    _private: [u8; 0],
}

// Only pointers to this opaque CoreGraphics object cross the Objective-C ABI.
unsafe impl RefEncode for CGEvent {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CGEvent", &[]));
}

#[derive(Default)]
struct EditorViewIvars {
    fixture: bool,
    count: Cell<u64>,
    pressed: Cell<bool>,
    slider: Cell<f64>,
    dragging_slider: Cell<bool>,
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn fill(area: NSRect, rgb: [f64; 3]) {
    let color: Retained<NSColor> = unsafe {
        msg_send![NSColor::class(), colorWithSRGBRed: rgb[0], green: rgb[1], blue: rgb[2], alpha: 1.0f64]
    };
    color.setFill();
    NSRectFill(area);
}

fn in_button(point: NSPoint) -> bool {
    point.x >= 40.0 && point.x < 200.0 && point.y >= 60.0 && point.y < 130.0
}

fn in_slider(point: NSPoint) -> bool {
    point.x >= 40.0 && point.x < 560.0 && point.y >= 190.0 && point.y < 250.0
}

define_class!(
    // The fixture deliberately uses nonblocking mouse handlers: native control
    // tracking loops would otherwise wait for events from the texture viewer.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NodeplayerZoomPocEditorView"]
    #[ivars = EditorViewIvars]
    struct EditorView;

    impl EditorView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool { true }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            fill(bounds, [0.075, 0.09, 0.12]);
            if !self.ivars().fixture { return; }
            let count = self.ivars().count.get();
            for row in 0..9 {
                for column in 0..16 {
                    let shade = if (row + column) % 2 == 0 { 0.13 } else { 0.17 };
                    fill(rect(column as f64 * 40.0, row as f64 * 40.0, 39.0, 39.0),
                        [shade, shade + 0.025, shade + 0.06]);
                }
            }
            // Unequal corner colors make flipped images immediately apparent.
            fill(rect(0.0, 0.0, 28.0, 28.0), [0.95, 0.15, 0.12]);
            fill(rect(bounds.size.width - 28.0, 0.0, 28.0, 28.0), [0.12, 0.9, 0.25]);
            fill(rect(0.0, bounds.size.height - 28.0, 28.0, 28.0), [0.1, 0.3, 0.95]);
            fill(rect(bounds.size.width - 28.0, bounds.size.height - 28.0, 28.0, 28.0), [0.95, 0.8, 0.12]);

            fill(rect(38.0, 58.0, 164.0, 74.0), [0.8, 0.85, 0.95]);
            let color = if self.ivars().pressed.get() { [0.8, 0.2, 0.4] }
                else if count.is_multiple_of(2) { [0.15, 0.4, 0.85] } else { [0.12, 0.7, 0.4] };
            fill(rect(40.0, 60.0, 160.0, 70.0), color);
            // A binary counter stays legible without depending on font APIs.
            for bit in 0..8 {
                let color = if count & (1 << bit) != 0 { [0.95, 0.85, 0.2] } else { [0.25, 0.28, 0.35] };
                fill(rect(240.0 + bit as f64 * 36.0, 80.0, 28.0, 30.0), color);
            }
            fill(rect(40.0, 208.0, 520.0, 24.0), [0.25, 0.29, 0.36]);
            let slider = self.ivars().slider.get();
            fill(rect(40.0, 208.0, 520.0 * slider, 24.0), [0.15, 0.65, 0.8]);
            fill(rect(35.0 + 520.0 * slider, 196.0, 10.0, 48.0), [0.95, 0.95, 0.98]);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if !self.ivars().fixture { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            self.ivars().pressed.set(in_button(point));
            self.ivars().dragging_slider.set(in_slider(point));
            if in_slider(point) { self.ivars().slider.set(((point.x - 40.0) / 520.0).clamp(0.0, 1.0)); }
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().fixture && self.ivars().dragging_slider.get() {
                let point = self.convertPoint_fromView(event.locationInWindow(), None);
                self.ivars().slider.set(((point.x - 40.0) / 520.0).clamp(0.0, 1.0));
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if !self.ivars().fixture { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            if self.ivars().pressed.replace(false) && in_button(point) {
                self.ivars().count.set(self.ivars().count.get().wrapping_add(1));
            }
            self.ivars().dragging_slider.set(false);
            self.setNeedsDisplay(true);
        }
    }
);

impl EditorView {
    fn new(mtm: MainThreadMarker, frame: NSRect, fixture: bool) -> Retained<Self> {
        let allocated = Self::alloc(mtm).set_ivars(EditorViewIvars {
            fixture,
            slider: Cell::new(0.5),
            ..Default::default()
        });
        unsafe { msg_send![super(allocated), initWithFrame: frame] }
    }
}

pub struct NativeEditor {
    app: Retained<NSApplication>,
    window: Retained<NSWindow>,
    view: Retained<EditorView>,
    standalone: bool,
    bitmap: Option<Retained<NSBitmapImageRep>>,
    bitmap_size: (u32, u32, u64),
    drag_target: Option<Retained<NSView>>,
    button_target: Option<Retained<NSButton>>,
}

impl NativeEditor {
    pub fn new(width: u32, height: u32, fixture: bool) -> Result<Self> {
        let mtm =
            MainThreadMarker::new().context("AppKit editor must be created on the main thread")?;
        ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "invalid native editor dimensions"
        );
        let app = NSApplication::sharedApplication(mtm);
        // winit uses an NSApplication subclass. Do not drain its queue or change
        // its delegate from inside eframe. Only standalone backend tests own it.
        let standalone = app.class().name().to_bytes() == b"NSApplication";
        if standalone {
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            app.finishLaunching();
        }
        let frame = rect(0.0, 0.0, width as f64, height as f64);
        let view = EditorView::new(mtm, frame, fixture);
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str(
            "Native editor reference — nodeplayer zoom PoC",
        ));
        window.setContentView(Some(&view));
        window.setAcceptsMouseMovedEvents(true);
        window.center();
        window.makeKeyAndOrderFront(None);
        view.setNeedsDisplay(true);
        window.displayIfNeeded();
        Ok(Self {
            app,
            window,
            view,
            standalone,
            bitmap: None,
            bitmap_size: (0, 0, 0),
            drag_target: None,
            button_target: None,
        })
    }

    pub fn handle(&self) -> usize {
        Retained::as_ptr(&self.view) as usize
    }

    pub fn size(&self) -> (u32, u32) {
        let size = self.view.bounds().size;
        (
            size.width.round().max(1.0) as u32,
            size.height.round().max(1.0) as u32,
        )
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "invalid native editor dimensions"
        );
        self.window
            .setContentSize(NSSize::new(width as f64, height as f64));
        self.bitmap = None;
        Ok(())
    }

    pub fn pump(&mut self) -> Result<()> {
        if self.standalone {
            let mode = unsafe { NSDefaultRunLoopMode };
            let date = NSDate::distantPast();
            for _ in 0..128 {
                let Some(event) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&date),
                    mode,
                    true,
                ) else {
                    break;
                };
                self.app.sendEvent(&event);
            }
            NSRunLoop::currentRunLoop().runMode_beforeDate(mode, &date);
            self.app.updateWindows();
        }
        self.window.displayIfNeeded();
        Ok(())
    }

    pub fn capture(&mut self) -> Result<CapturedFrame> {
        ensure!(
            self.window.isVisible(),
            "native reference window was closed"
        );
        self.view.displayIfNeeded();
        let bounds = self.view.bounds();
        let size = self.size();
        let backing_scale: f64 = unsafe { msg_send![&*self.window, backingScaleFactor] };
        let key = (size.0, size.1, backing_scale.to_bits());
        if self.bitmap.is_none() || self.bitmap_size != key {
            self.bitmap = self.view.bitmapImageRepForCachingDisplayInRect(bounds);
            self.bitmap_size = key;
        }
        let bitmap = self
            .bitmap
            .as_ref()
            .context("AppKit could not allocate a view snapshot")?;
        self.view
            .cacheDisplayInRect_toBitmapImageRep(bounds, bitmap);
        rgba_frame(bitmap)
    }

    pub fn pointer(&mut self, pointer: PointerEvent) -> Result<()> {
        ensure!(
            pointer.position.iter().all(|v| v.is_finite()),
            "non-finite pointer coordinates"
        );
        let point = NSPoint::new(pointer.position[0], pointer.position[1]);
        let location = self.view.convertPoint_toView(point, None);
        let target = self
            .drag_target
            .clone()
            .or_else(|| self.view.hitTest(point));
        let Some(target) = target else { return Ok(()) };

        if let PointerKind::Wheel { dx, dy } = pointer.kind {
            if self.view.ivars().fixture {
                let slider = (self.view.ivars().slider.get() + (dy as f64 + dx as f64) / 520.0)
                    .clamp(0.0, 1.0);
                self.view.ivars().slider.set(slider);
                self.view.setNeedsDisplay(true);
                return Ok(());
            }
            return self.forward_wheel(&target, location, dx, dy);
        }

        // NSButton::mouseDown enters a nested tracking loop. Simulating a click
        // on release invokes its normal action without blocking the viewer.
        if matches!(pointer.kind, PointerKind::Down(1))
            && let Some(button) = target.downcast_ref::<NSButton>()
        {
            self.button_target = Some(button.retain());
            self.drag_target = Some(target.clone());
            return Ok(());
        }
        if let Some(button) = self.button_target.take() {
            if matches!(pointer.kind, PointerKind::Up(1)) {
                let local = button.convertPoint_fromView(location, None);
                let bounds = button.bounds();
                if local.x >= 0.0
                    && local.y >= 0.0
                    && local.x < bounds.size.width
                    && local.y < bounds.size.height
                {
                    unsafe {
                        button.performClick(None);
                    }
                }
                self.drag_target = None;
            } else {
                self.button_target = Some(button);
            }
            return Ok(());
        }

        let event_type = match pointer.kind {
            PointerKind::Down(1) => NSEventType::LeftMouseDown,
            PointerKind::Down(2) => NSEventType::RightMouseDown,
            PointerKind::Down(_) => NSEventType::OtherMouseDown,
            PointerKind::Up(1) => NSEventType::LeftMouseUp,
            PointerKind::Up(2) => NSEventType::RightMouseUp,
            PointerKind::Up(_) => NSEventType::OtherMouseUp,
            PointerKind::Move if pointer.buttons & 1 != 0 => NSEventType::LeftMouseDragged,
            PointerKind::Move if pointer.buttons & 2 != 0 => NSEventType::RightMouseDragged,
            PointerKind::Move if pointer.buttons & 4 != 0 => NSEventType::OtherMouseDragged,
            PointerKind::Move => NSEventType::MouseMoved,
            PointerKind::Wheel { .. } => unreachable!(),
        };
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            event_type, location, NSEventModifierFlags::empty(),
            unsafe { CACurrentMediaTime() }, self.window.windowNumber(), None, 0, 1,
            if matches!(pointer.kind, PointerKind::Up(_)) { 0.0 } else { 1.0 },
        ).context("AppKit could not create forwarded pointer event")?;
        if matches!(pointer.kind, PointerKind::Down(_)) {
            self.drag_target = Some(target.clone());
            self.window.makeFirstResponder(Some(&target));
        }
        match event_type {
            NSEventType::LeftMouseDown => target.mouseDown(&event),
            NSEventType::LeftMouseUp => target.mouseUp(&event),
            NSEventType::RightMouseDown => target.rightMouseDown(&event),
            NSEventType::RightMouseUp => target.rightMouseUp(&event),
            NSEventType::OtherMouseDown => target.otherMouseDown(&event),
            NSEventType::OtherMouseUp => target.otherMouseUp(&event),
            NSEventType::LeftMouseDragged => target.mouseDragged(&event),
            NSEventType::RightMouseDragged => target.rightMouseDragged(&event),
            NSEventType::OtherMouseDragged => target.otherMouseDragged(&event),
            _ => target.mouseMoved(&event),
        }
        if matches!(pointer.kind, PointerKind::Up(_)) {
            self.drag_target = None;
        }
        Ok(())
    }

    pub fn fixture_value(&self) -> Option<u64> {
        self.view
            .ivars()
            .fixture
            .then(|| self.view.ivars().count.get())
    }

    fn forward_wheel(&self, target: &NSView, location: NSPoint, dx: f32, dy: f32) -> Result<()> {
        // A CGEvent gives NSEvent real wheel delta fields; it is delivered to the
        // hit-tested view directly and never posted as system-wide input.
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreateScrollWheelEvent(
                source: *const c_void,
                units: u32,
                wheels: u32,
                ...
            ) -> *mut CGEvent;
            fn CGEventSetLocation(event: *mut CGEvent, point: NSPoint);
            fn CGEventSetIntegerValueField(event: *mut CGEvent, field: u32, value: i64);
            fn CGMainDisplayID() -> u32;
            fn CGDisplayBounds(display: u32) -> NSRect;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRelease(value: *const c_void);
        }
        unsafe {
            let cg = CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                0,
                2,
                dy.round() as i32,
                dx.round() as i32,
            );
            ensure!(!cg.is_null(), "CoreGraphics could not create wheel event");
            let screen = self.window.convertPointToScreen(location);
            // AppKit's screen origin is bottom-left; CGEvent uses top-left.
            let primary = CGDisplayBounds(CGMainDisplayID());
            CGEventSetLocation(cg, NSPoint::new(screen.x, primary.size.height - screen.y));
            // Associate this event with the reference window rather than the
            // application underneath the real desktop cursor.
            CGEventSetIntegerValueField(cg, 91, self.window.windowNumber() as i64);
            CGEventSetIntegerValueField(cg, 92, self.window.windowNumber() as i64);
            let event: Option<Retained<NSEvent>> =
                msg_send![NSEvent::class(), eventWithCGEvent: cg];
            CFRelease(cg.cast());
            let event = event.context("AppKit could not wrap wheel event")?;
            target.scrollWheel(&event);
        }
        Ok(())
    }
}

fn rgba_frame(bitmap: &NSBitmapImageRep) -> Result<CapturedFrame> {
    let width = u32::try_from(bitmap.pixelsWide()).context("invalid snapshot width")?;
    let height = u32::try_from(bitmap.pixelsHigh()).context("invalid snapshot height")?;
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "invalid snapshot dimensions"
    );
    let channels =
        usize::try_from(bitmap.samplesPerPixel()).context("invalid snapshot component count")?;
    let format = bitmap.bitmapFormat();
    if bitmap.bitsPerSample() == 8
        && !bitmap.isPlanar()
        && channels == 4
        && !format.contains(NSBitmapFormat::FloatingPointSamples)
    {
        let stride = usize::try_from(bitmap.bytesPerRow()).context("invalid snapshot stride")?;
        ensure!(stride >= width as usize * 4, "short AppKit snapshot row");
        let length = stride
            .checked_mul(height as usize)
            .context("AppKit snapshot length overflow")?;
        let data = bitmap.bitmapData();
        ensure!(!data.is_null(), "AppKit snapshot has no pixel buffer");
        // NSBitmapImageRep owns this buffer throughout the conversion.
        let data = unsafe { std::slice::from_raw_parts(data, length) };
        let alpha_first = format.contains(NSBitmapFormat::AlphaFirst);
        let little = format.contains(NSBitmapFormat::ThirtyTwoBitLittleEndian);
        let premultiplied = !format.contains(NSBitmapFormat::AlphaNonpremultiplied);
        let mut rgba = vec![0; width as usize * height as usize * 4];
        for (y, row) in rgba.chunks_exact_mut(width as usize * 4).enumerate() {
            for (pixel, source) in row.as_chunks_mut::<4>().0.iter_mut().zip(
                data[y * stride..y * stride + width as usize * 4]
                    .as_chunks::<4>()
                    .0
                    .iter(),
            ) {
                let samples = if little {
                    [source[3], source[2], source[1], source[0]]
                } else {
                    [source[0], source[1], source[2], source[3]]
                };
                let (rgb, alpha) = if alpha_first {
                    ([samples[1], samples[2], samples[3]], samples[0])
                } else {
                    ([samples[0], samples[1], samples[2]], samples[3])
                };
                for c in 0..3 {
                    pixel[c] = if premultiplied && alpha > 0 {
                        ((rgb[c] as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
                    } else {
                        rgb[c]
                    };
                }
                pixel[3] = alpha;
            }
        }
        return Ok(CapturedFrame {
            width,
            height,
            rgba,
        });
    }
    // Unusual color models/bit depths use AppKit's conversion. This is slower,
    // but avoids interpreting arbitrary native bitmap layouts as RGBA8.
    let properties = NSDictionary::new();
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
    }
    .context("AppKit could not convert snapshot to PNG")?;
    let bytes = unsafe { png.as_bytes_unchecked() };
    let image = image::load_from_memory(bytes)
        .context("cannot decode AppKit snapshot")?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok(CapturedFrame {
        width,
        height,
        rgba: image.into_raw(),
    })
}

impl Drop for NativeEditor {
    fn drop(&mut self) {
        self.window.setContentView(None);
        self.window.close();
    }
}
