//! Bitmap capture and pointer delivery for an editor NSView owned by this process.
//!
//! The owning editor must outlive this adapter, including its native descendants;
//! drop the adapter before destroying the plugin GUI. The source window stays
//! visible. AppKit bitmap drawing does not universally capture plugin-owned
//! Metal/OpenGL surfaces. Pointer delivery is experimental: plugin tracking loops,
//! keyboard/IME, and separate popup windows require additional hosting support.

use std::ffi::c_void;

use anyhow::{Context, Result, ensure};
use objc2::rc::Retained;
use objc2::{ClassType, Encoding, MainThreadMarker, Message, RefEncode, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapFormat, NSBitmapImageFileType,
    NSBitmapImageRep, NSButton, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSView,
    NSWindow,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSRect, NSRunLoop};

use super::{EditorFrame, EditorPointer, PointerKind};

const MAX_CAPTURE_PIXELS: u64 = 16 * 1024 * 1024;

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    fn CACurrentMediaTime() -> f64;
}

#[repr(C)]
struct CGEvent {
    _private: [u8; 0],
}

// SAFETY: only pointers to this opaque CoreGraphics object cross the Objective-C ABI.
unsafe impl RefEncode for CGEvent {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CGEvent", &[]));
}

/// Initialize AppKit on a standalone plugin worker's main thread, before opening an editor.
/// A parent application using winit already owns its NSApplication subclass.
pub fn initialize_worker_app() -> Result<()> {
    let mtm = MainThreadMarker::new().context("AppKit must run on the worker main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    if app.class().name().to_bytes() == b"NSApplication" {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
    }
    Ok(())
}

/// Drain a bounded batch of native events in a standalone worker, without taking winit's queue.
pub fn pump_worker_app() -> Result<()> {
    let mtm = MainThreadMarker::new().context("AppKit must run on the worker main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    if app.class().name().to_bytes() != b"NSApplication" {
        return Ok(());
    }
    let mode = unsafe { NSDefaultRunLoopMode };
    let date = NSDate::distantPast();
    for _ in 0..128 {
        let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&date),
            mode,
            true,
        ) else {
            break;
        };
        app.sendEvent(&event);
    }
    NSRunLoop::currentRunLoop().runMode_beforeDate(mode, &date);
    app.updateWindows();
    Ok(())
}

pub struct NativeCapture {
    bitmap: Option<Retained<NSBitmapImageRep>>,
    bitmap_key: (u64, u64, u64),
    drag_target: Option<Retained<NSView>>,
    button_target: Option<Retained<NSButton>>,
    view: Retained<NSView>,
}

impl NativeCapture {
    /// `handle` is an existing, valid NSView in this process, on the plugin main thread.
    /// Retaining it does not transfer ownership of its plugin GUI or enclosing window.
    pub fn new(handle: crate::plugins::WindowId) -> Result<Self> {
        MainThreadMarker::new().context("native capture must run on the main thread")?;
        ensure!(handle != 0, "native capture has no editor view");
        // SAFETY: the editor supplies its live, process-local NSView handle. Its owner
        // keeps the plugin GUI alive until this adapter is dropped.
        let view = unsafe { Retained::retain(handle as *mut NSView) }
            .context("cannot retain native editor view")?;
        ensure!(
            view.window().is_some(),
            "native editor is not attached to a window"
        );
        Ok(Self {
            bitmap: None,
            bitmap_key: (0, 0, 0),
            drag_target: None,
            button_target: None,
            view,
        })
    }

    pub fn capture(&mut self, _native_size: (u32, u32)) -> Result<EditorFrame> {
        MainThreadMarker::new().context("native capture must run on the main thread")?;
        let window = self
            .view
            .window()
            .context("native editor left its window")?;
        ensure!(
            window.isVisible(),
            "native capture source window is not visible"
        );
        let bounds = self.view.bounds();
        let backing_scale: f64 = unsafe { msg_send![&*window, backingScaleFactor] };
        ensure!(
            bounds.size.width.is_finite()
                && bounds.size.height.is_finite()
                && backing_scale.is_finite()
                && backing_scale > 0.0,
            "invalid native editor capture geometry"
        );
        // Bound the bitmap before asking AppKit to allocate it, including Retina pixels.
        let pixel_width = (bounds.size.width * backing_scale).ceil();
        let pixel_height = (bounds.size.height * backing_scale).ceil();
        ensure!(
            pixel_width > 0.0
                && pixel_height > 0.0
                && pixel_width <= 16384.0
                && pixel_height <= 16384.0
                && pixel_width * pixel_height <= MAX_CAPTURE_PIXELS as f64,
            "native editor exceeds capture pixel budget"
        );
        let native_size = (
            bounds.size.width.round().max(1.0) as u32,
            bounds.size.height.round().max(1.0) as u32,
        );
        self.view.displayIfNeeded();
        let key = (
            bounds.size.width.to_bits(),
            bounds.size.height.to_bits(),
            backing_scale.to_bits(),
        );
        if self.bitmap.is_none() || self.bitmap_key != key {
            self.bitmap = self.view.bitmapImageRepForCachingDisplayInRect(bounds);
            self.bitmap_key = key;
        }
        let bitmap = self
            .bitmap
            .as_ref()
            .context("AppKit could not allocate a view snapshot")?;
        self.view
            .cacheDisplayInRect_toBitmapImageRep(bounds, bitmap);
        rgba_frame(bitmap, native_size)
    }

    pub fn pointer(&mut self, pointer: EditorPointer) -> Result<()> {
        MainThreadMarker::new().context("native input must run on the main thread")?;
        ensure!(
            pointer.position.iter().all(|v| v.is_finite()),
            "non-finite pointer coordinates"
        );
        let window = self
            .view
            .window()
            .context("native editor left its window")?;
        // Production HostWindow uses an ordinary, unflipped NSView. The graph protocol
        // always measures from its top-left, independent of the native view orientation.
        let bounds = self.view.bounds();
        let point = NSPoint::new(
            bounds.origin.x + pointer.position[0],
            bounds.origin.y
                + if self.view.isFlipped() {
                    pointer.position[1]
                } else {
                    bounds.size.height - pointer.position[1]
                },
        );
        let location = self.view.convertPoint_toView(point, None);
        let target = self.drag_target.clone().or_else(|| {
            // NSView::hitTest takes coordinates in the receiver's superview, not bounds.
            let superview = unsafe { self.view.superview() };
            let parent_point = self.view.convertPoint_toView(point, superview.as_deref());
            self.view.hitTest(parent_point)
        });
        let Some(target) = target else { return Ok(()) };
        if let PointerKind::Wheel { dx, dy } = pointer.kind {
            ensure!(
                dx.is_finite() && dy.is_finite(),
                "non-finite wheel displacement"
            );
            return forward_wheel(&window, &target, location, dx, dy);
        }
        // NSButton::mouseDown enters a blocking tracking loop. Release-time performClick
        // invokes the normal action without waiting for events from the graph window.
        if matches!(pointer.kind, PointerKind::Down(1))
            && let Some(button) = target.downcast_ref::<NSButton>()
        {
            self.button_target = Some(button.retain());
            self.drag_target = Some(target);
            return Ok(());
        }
        if let Some(button) = self.button_target.take() {
            if matches!(pointer.kind, PointerKind::Up(1)) {
                let local = button.convertPoint_fromView(location, None);
                let bounds = button.bounds();
                if local.x >= bounds.origin.x
                    && local.y >= bounds.origin.y
                    && local.x < bounds.origin.x + bounds.size.width
                    && local.y < bounds.origin.y + bounds.size.height
                {
                    unsafe { button.performClick(None) };
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
            unsafe { CACurrentMediaTime() }, window.windowNumber(), None, 0, 1,
            if matches!(pointer.kind, PointerKind::Up(_)) { 0.0 } else { 1.0 },
        ).context("AppKit could not create forwarded pointer event")?;
        if matches!(pointer.kind, PointerKind::Down(_)) {
            self.drag_target = Some(target.clone());
            window.makeFirstResponder(Some(&target));
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
        if matches!(pointer.kind, PointerKind::Up(_)) && pointer.buttons == 0 {
            self.drag_target = None;
        }
        Ok(())
    }
}

fn forward_wheel(
    window: &NSWindow,
    target: &NSView,
    location: NSPoint,
    dx: f32,
    dy: f32,
) -> Result<()> {
    // Populate real wheel delta fields, but deliver directly to the target view instead
    // of posting global input or moving the user's physical desktop pointer.
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
        let screen = window.convertPointToScreen(location);
        let primary = CGDisplayBounds(CGMainDisplayID());
        CGEventSetLocation(cg, NSPoint::new(screen.x, primary.size.height - screen.y));
        CGEventSetIntegerValueField(cg, 91, window.windowNumber() as i64);
        CGEventSetIntegerValueField(cg, 92, window.windowNumber() as i64);
        let event: Option<Retained<NSEvent>> = msg_send![NSEvent::class(), eventWithCGEvent: cg];
        CFRelease(cg.cast());
        let event = event.context("AppKit could not wrap wheel event")?;
        target.scrollWheel(&event);
    }
    Ok(())
}

fn rgba_frame(bitmap: &NSBitmapImageRep, native_size: (u32, u32)) -> Result<EditorFrame> {
    let width = u32::try_from(bitmap.pixelsWide()).context("invalid snapshot width")?;
    let height = u32::try_from(bitmap.pixelsHigh()).context("invalid snapshot height")?;
    ensure!(
        width > 0
            && height > 0
            && width <= 16384
            && height <= 16384
            && u64::from(width) * u64::from(height) <= MAX_CAPTURE_PIXELS,
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
        // SAFETY: NSBitmapImageRep owns bytesPerRow * pixelsHigh bytes throughout conversion.
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
        return Ok(EditorFrame {
            width,
            height,
            native_size,
            rgba,
        });
    }
    // AppKit converts unusual color models/bit depths, avoiding assumptions about native layouts.
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
    Ok(EditorFrame {
        width,
        height,
        native_size,
        rgba: image.into_raw(),
    })
}
