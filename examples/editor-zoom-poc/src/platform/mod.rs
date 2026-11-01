//! Native editor surfaces used by the isolated graph-zoom experiment.

use anyhow::{Result, ensure};

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Top-left origin, tightly packed, straight-alpha RGBA8.
    pub rgba: Vec<u8>,
}

impl CapturedFrame {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.width > 0 && self.height > 0, "empty native capture");
        let length = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("capture dimensions overflow"))?;
        ensure!(
            self.rgba.len() == length,
            "native capture has an invalid byte count"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PointerKind {
    Move,
    /// 1 = primary, 2 = secondary, 3 = middle.
    Down(u8),
    Up(u8),
    /// Wheel displacement in logical points.
    Wheel {
        dx: f32,
        dy: f32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    /// Native editor coordinates, from the top-left, independent of graph zoom.
    pub position: [f64; 2],
    pub kind: PointerKind,
    /// Bit 0 primary, bit 1 secondary, bit 2 middle.
    pub buttons: u8,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::NativeEditor;
#[cfg(target_os = "macos")]
pub use macos::NativeEditor;
#[cfg(target_os = "windows")]
pub use windows::NativeEditor;
