//! Native editor images and input. Capture stays with the plugin; graph zoom stays with egui.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{Editor, Placement, Spot};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
pub mod view;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use linux::NativeCapture;
#[cfg(target_os = "macos")]
use macos::NativeCapture;
#[cfg(target_os = "windows")]
use windows::NativeCapture;

pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_PIXELS: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct EditorFrame {
    pub width: u32,
    pub height: u32,
    /// Native input units, independent of captured pixels (points on Cocoa).
    pub native_size: (u32, u32),
    /// Top-left origin, tightly packed, straight-alpha RGBA8.
    pub rgba: Vec<u8>,
}

impl EditorFrame {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.width > 0
                && self.height > 0
                && self.width <= MAX_DIMENSION
                && self.height <= MAX_DIMENSION,
            "invalid editor image dimensions"
        );
        let pixels = (self.width as usize)
            .checked_mul(self.height as usize)
            .context("editor image size overflow")?;
        ensure!(
            pixels <= MAX_PIXELS && pixels.checked_mul(4) == Some(self.rgba.len()),
            "invalid editor image byte count"
        );
        ensure!(
            self.native_size.0 > 0
                && self.native_size.1 > 0
                && self.native_size.0 <= MAX_DIMENSION
                && self.native_size.1 <= MAX_DIMENSION,
            "invalid native editor dimensions"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum PointerKind {
    Move,
    Down(u8),
    Up(u8),
    Wheel { dx: f32, dy: f32 },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EditorPointer {
    pub position: [f64; 2],
    pub kind: PointerKind,
    /// Bit 0 primary, bit 1 secondary, bit 2 middle.
    pub buttons: u8,
}

impl EditorPointer {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.position
                .iter()
                .all(|p| p.is_finite() && p.abs() <= 1_000_000.0),
            "invalid editor pointer coordinates"
        );
        ensure!(self.buttons & !7 == 0, "invalid editor buttons");
        match self.kind {
            PointerKind::Down(b) | PointerKind::Up(b) => {
                ensure!((1..=3).contains(&b), "invalid editor pointer button")
            }
            PointerKind::Wheel { dx, dy } => ensure!(
                dx.is_finite()
                    && dy.is_finite()
                    && dx.abs() <= 1_000_000.0
                    && dy.abs() <= 1_000_000.0,
                "invalid editor wheel displacement"
            ),
            PointerKind::Move => {}
        }
        Ok(())
    }
}

/// The native source must have its own mapped window, rather than the graph's masked parent.
pub fn capture(source: Box<dyn Editor>, placement: Placement) -> Result<Box<dyn Editor>> {
    let handle = source
        .native_handle()
        .context("this editor does not expose a capture source")?;
    let native = NativeCapture::new(handle)?;
    Ok(Box::new(CapturedEditor {
        native,
        source,
        placement,
        pending: None,
        next_capture: Instant::now(),
        painted: false,
        error: None,
    }))
}

struct CapturedEditor {
    // Release capture resources before the plugin's native view is destroyed.
    native: NativeCapture,
    source: Box<dyn Editor>,
    placement: Placement,
    pending: Option<EditorFrame>,
    next_capture: Instant,
    painted: bool,
    error: Option<String>,
}

impl Editor for CapturedEditor {
    fn placement(&self) -> Placement {
        self.placement
    }
    fn size(&self) -> (u32, u32) {
        self.source.size()
    }
    fn ready(&self) -> bool {
        self.source.ready()
    }
    fn is_captured(&self) -> bool {
        true
    }
    fn take_frame(&mut self) -> Option<EditorFrame> {
        self.pending.take()
    }
    fn capture_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    fn pointer(&mut self, event: EditorPointer) -> Result<()> {
        event.validate()?;
        self.native.pointer(event)
    }
    fn place(&mut self, _spot: Option<&Spot>) {
        // Canvas overlap is applied to the image, never cut out of its native source.
    }
    fn idle(&mut self) -> bool {
        if !self.source.idle() {
            return false;
        }
        if Instant::now() >= self.next_capture {
            self.next_capture = Instant::now() + Duration::from_millis(33);
            let captured = self.native.capture(self.source.size()).and_then(|frame| {
                frame.validate()?;
                Ok(frame)
            });
            match captured {
                Ok(frame) => {
                    // GUI startup can be asynchronous. After the first painting, a uniform
                    // image is legitimate and must be allowed to replace earlier content.
                    if !self.painted
                        && frame
                            .rgba
                            .as_chunks::<4>()
                            .0
                            .windows(2)
                            .all(|p| p[0] == p[1])
                    {
                        return true;
                    }
                    self.painted = true;
                    self.error = None;
                    self.pending = Some(frame);
                }
                Err(error) => {
                    self.error = Some(format!("{error:#}"));
                }
            }
        }
        true
    }
}

#[cfg(target_os = "macos")]
pub fn initialize_worker_app() -> Result<()> {
    macos::initialize_worker_app()
}
#[cfg(target_os = "macos")]
pub fn pump_worker_app() -> Result<()> {
    macos::pump_worker_app()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_frames_and_input_before_native_delivery() {
        let mut frame = EditorFrame {
            width: 2,
            height: 1,
            native_size: (1, 1),
            rgba: vec![255; 8],
        };
        assert!(frame.validate().is_ok());
        frame.width = MAX_DIMENSION + 1;
        assert!(frame.validate().is_err());
        frame.width = 2;
        frame.rgba.pop();
        assert!(frame.validate().is_err());
        let mut event = EditorPointer {
            position: [0.0, 0.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        };
        assert!(event.validate().is_ok());
        event.position[0] = f64::NAN;
        assert!(event.validate().is_err());
        event.position[0] = 0.0;
        event.kind = PointerKind::Up(4);
        assert!(event.validate().is_err());
    }
}
