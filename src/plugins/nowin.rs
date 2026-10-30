//! Plugin editor windows on macOS: not supported yet (it needs NSView hosting). Plugins still
//! load and run there, and JSFX and native plugins draw their interface with egui inside the
//! node; only the plugins' own editor windows don't open.

use std::cell::Cell;

use anyhow::{Result, bail};

use super::{Placement, PxRect, WinEvent, WindowId};

pub struct HostWindow {
    pub placement: Placement,
    pub scale: Cell<f32>,
    pub base: Cell<(f32, f32)>,
}

impl HostWindow {
    pub fn create(_placement: Placement, _w: u32, _h: u32, _title: &str, _announce: bool) -> Result<Self> {
        bail!("plugin editor windows aren't supported on macOS yet")
    }

    pub fn handle(&self) -> WindowId {
        0
    }

    pub fn size(&self) -> (u32, u32) {
        (0, 0)
    }

    pub fn resize(&self, _w: u32, _h: u32) {}

    pub fn set_fixed_size(&self, _w: u32, _h: u32, _fixed: bool) {}

    pub fn place(&self, _clip: Option<PxRect>, _origin: (i32, i32)) {}

    pub fn poll(&self) -> Vec<WinEvent> {
        Vec::new()
    }
}
