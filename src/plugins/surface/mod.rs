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
pub const MAX_POPUPS: usize = 16;
pub const MOD_SHIFT: u8 = 1;
pub const MOD_CONTROL: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_SUPER: u8 = 8;
/// Capture service budget; presentation is paced independently by the viewport/display.
pub const CAPTURE_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 240);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct EditorFrame {
    pub width: u32,
    pub height: u32,
    /// Native input units, independent of captured pixels (points on Cocoa).
    pub native_size: (u32, u32),
    /// Top-left origin, tightly packed, straight-alpha RGBA8.
    pub rgba: Vec<u8>,
    /// Child windows, ordered from back to front, in native editor coordinates.
    pub popups: Vec<EditorPopup>,
}

#[derive(Clone, Debug)]
pub struct EditorPopup {
    pub origin: [f64; 2],
    pub width: u32,
    pub height: u32,
    pub native_size: (u32, u32),
    pub rgba: Vec<u8>,
    pub interactive: bool,
}

fn validate_image(width: u32, height: u32, native_size: (u32, u32), bytes: usize) -> Result<usize> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "invalid editor image dimensions"
    );
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .context("editor image size overflow")?;
    ensure!(
        pixels <= MAX_PIXELS && pixels.checked_mul(4) == Some(bytes),
        "invalid editor image byte count"
    );
    ensure!(
        native_size.0 > 0
            && native_size.1 > 0
            && native_size.0 <= MAX_DIMENSION
            && native_size.1 <= MAX_DIMENSION,
        "invalid native editor dimensions"
    );
    Ok(pixels)
}

impl EditorFrame {
    pub fn validate(&self) -> Result<()> {
        let mut pixels =
            validate_image(self.width, self.height, self.native_size, self.rgba.len())?;
        ensure!(self.popups.len() <= MAX_POPUPS, "too many editor popups");
        for popup in &self.popups {
            ensure!(
                popup
                    .origin
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= 1_000_000.0),
                "invalid editor popup origin"
            );
            pixels = pixels
                .checked_add(validate_image(
                    popup.width,
                    popup.height,
                    popup.native_size,
                    popup.rgba.len(),
                )?)
                .context("editor popup pixel count overflow")?;
        }
        ensure!(
            pixels <= MAX_PIXELS,
            "editor and popups exceed pixel budget"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum PointerKind {
    Move,
    Down(u8),
    Up(u8),
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// Dismiss transient windows and abandon any native pointer ownership.
    Cancel,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EditorPointer {
    pub position: [f64; 2],
    pub kind: PointerKind,
    /// Bit 0 primary, bit 1 secondary, bit 2 middle.
    pub buttons: u8,
    /// Shift, Control, Alt/Option and Super/Command at the input event boundary.
    #[serde(default)]
    pub modifiers: u8,
    /// Click sequence on a button transition; zero for motion, wheel, or cancellation.
    pub click_count: u8,
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
        ensure!(self.modifiers & !15 == 0, "invalid editor modifiers");
        ensure!(
            matches!(self.kind, PointerKind::Down(_) | PointerKind::Up(_)) || self.click_count == 0,
            "click count on a non-button editor event"
        );
        if matches!(self.kind, PointerKind::Down(_)) {
            ensure!(self.click_count > 0, "editor mouse-down has no click count");
        }
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
            PointerKind::Cancel => ensure!(self.buttons == 0, "cancel must release editor buttons"),
            PointerKind::Move => {}
        }
        Ok(())
    }
}

/// The native source uses a private rendering host, separate from the graph's masked parent.
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
        opened_at: Instant::now(),
        painted: false,
        errors: CaptureErrors::default(),
    }))
}

struct CapturedEditor {
    // Release capture resources before the plugin's native view is destroyed.
    native: NativeCapture,
    source: Box<dyn Editor>,
    placement: Placement,
    pending: Option<EditorFrame>,
    next_capture: Instant,
    opened_at: Instant,
    painted: bool,
    errors: CaptureErrors,
}

/// Recoverable UI state plus bounded evidence retained for this editor open.
#[derive(Default)]
pub(crate) struct CaptureErrors {
    current: Option<String>,
    first: Option<String>,
}

impl CaptureErrors {
    pub fn record(&mut self, message: String) {
        self.observe_first(Some(&message));
        self.current = Some(message);
    }
    pub fn observe_first(&mut self, message: Option<&str>) {
        if self.first.is_none() {
            self.first = message.map(|message| message.chars().take(1024).collect());
        }
    }
    pub fn recovered(&mut self) {
        self.current = None;
    }
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
    pub fn first(&self) -> Option<&str> {
        self.first.as_deref()
    }
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
        self.errors.current()
    }
    fn first_capture_error(&self) -> Option<&str> {
        self.errors.first()
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
        let now = Instant::now();
        if now >= self.next_capture {
            self.next_capture = next_capture_deadline(self.next_capture, now);
            let captured = self.native.capture(self.source.size()).and_then(|frame| {
                frame.validate()?;
                Ok(frame)
            });
            match captured {
                Ok(frame) => {
                    // GUI startup can be asynchronous. After the first painting, a uniform
                    // image is legitimate and must be allowed to replace earlier content.
                    if !self.painted
                        && frame.popups.is_empty()
                        && frame
                            .rgba
                            .as_chunks::<4>()
                            .0
                            .windows(2)
                            .all(|p| p[0] == p[1])
                    {
                        if self.opened_at.elapsed() >= STARTUP_TIMEOUT {
                            self.errors.record(
                                "Editor capture produced no image within 5 seconds; try Pop out"
                                    .into(),
                            );
                        }
                        return true;
                    }
                    self.painted = true;
                    self.errors.recovered();
                    self.pending = Some(frame);
                }
                Err(error) => {
                    self.errors.record(format!("{error:#}"));
                }
            }
        }
        true
    }
}

fn next_capture_deadline(previous: Instant, now: Instant) -> Instant {
    // Keep the cadence when a UI tick arrives slightly late. Skip missed intervals
    // after a stall rather than issuing a burst of catch-up captures.
    let next = previous + CAPTURE_INTERVAL;
    if next > now {
        next
    } else {
        now + CAPTURE_INTERVAL
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
    fn capture_error_history_is_bounded_and_preserves_the_first_failure() {
        let mut errors = CaptureErrors::default();
        errors.record("界".repeat(2048));
        assert_eq!(errors.first().unwrap().chars().count(), 1024);
        let first = errors.first().unwrap().to_owned();
        errors.recovered();
        errors.record("later failure".into());
        assert_eq!(errors.current(), Some("later failure"));
        assert_eq!(errors.first(), Some(first.as_str()));
    }

    #[test]
    fn capture_cadence_survives_ui_polling_jitter_and_skips_stalls() {
        let start = Instant::now();
        let mut deadline = start;
        let mut frames = 0;
        for tick in 0..=1000 {
            let now = start + Duration::from_millis(tick);
            if now >= deadline {
                frames += 1;
                deadline = next_capture_deadline(deadline, now);
            }
        }
        assert_eq!(
            frames, 241,
            "a one-second run services the 240 Hz target without polling drift"
        );
        let resumed = start + Duration::from_secs(10);
        assert_eq!(
            next_capture_deadline(deadline, resumed),
            resumed + CAPTURE_INTERVAL
        );
    }

    #[test]
    fn rejects_invalid_frames_and_input_before_native_delivery() {
        let mut frame = EditorFrame {
            popups: Vec::new(),
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
            modifiers: 0,
            click_count: 1,
            position: [0.0, 0.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        };
        assert!(event.validate().is_ok());
        event.position[0] = f64::NAN;
        assert!(event.validate().is_err());
        event.position[0] = 0.0;
        event.modifiers = 16;
        assert!(event.validate().is_err());
        event.modifiers = MOD_SHIFT | MOD_CONTROL | MOD_ALT | MOD_SUPER;
        assert!(event.validate().is_ok());
        event.kind = PointerKind::Up(4);
        assert!(event.validate().is_err());
        event.kind = PointerKind::Down(1);
        event.click_count = 0;
        assert!(event.validate().is_err());
        event.click_count = 2;
        assert!(event.validate().is_ok());
        event.kind = PointerKind::Move;
        assert!(event.validate().is_err());
    }
}
