//! Experimental whole-plugin process isolation. See README.md for limits and ABI contract.
mod processor;
mod protocol;
mod shared;
mod supervisor;
mod worker;
use super::{Created, Editor, Format, Instance, ParamDesc, Placement, PluginInfo, Spot};
use crate::engine::Processor;
use anyhow::{Result, bail};
use crossbeam_channel::Sender;
use protocol::{Command, Metadata};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};
pub use worker::worker_main;

pub const BRIDGE_LATENCY: u32 = shared::DELAY as u32;
#[derive(Clone)]
struct Snapshot {
    metadata: Option<Metadata>,
    state: Option<Vec<u8>>,
    displays: HashMap<u32, (f64, String)>,
    status: String,
    revision: u64,
}
struct Core {
    stop: AtomicBool,
    recover: AtomicBool,
    ready: AtomicBool,
    map: Mutex<Option<Arc<shared::Shared>>>,
    // Retain retired mappings until the Core is freed, never reclaim on a process callback.
    retired: Mutex<Vec<Arc<shared::Shared>>>,
    cursor: AtomicU64,
    algorithm_latency: AtomicU32,
    instrument: AtomicBool,
    underruns: AtomicU64,
    overflows: AtomicU64,
    next_editor: AtomicU64,
    snapshot: Mutex<Snapshot>,
    tx: Sender<Command>,
}
impl Core {
    fn enqueue(&self, command: Command) -> bool {
        self.tx.try_send(command).is_ok()
    }
}
/// Reject unsupported UI embedding backends rather than silently loading locally.
pub fn supported() -> Result<()> {
    if cfg!(target_os = "macos") {
        bail!(
            "plugin isolation is unavailable on macOS: remote NSView embedding is not implemented"
        );
    }
    if !cfg!(all(
        target_pointer_width = "64",
        target_has_atomic = "64",
        target_has_atomic = "32"
    )) {
        bail!("plugin isolation requires native 64-bit process-shared atomics");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if std::env::var_os("DISPLAY").is_none() {
        bail!("plugin isolation requires X11/XWayland; native Wayland embedding is unsupported");
    }
    Ok(())
}
/// Returns a pending cache-only proxy. File creation, plugin loading and spawning happen
/// exclusively on the detached supervisor thread, not the caller/UI thread.
pub fn create(info: &PluginInfo, sr: f32, max_block: usize) -> Result<Created> {
    supported()?;
    if !matches!(info.format, Format::Vst3 | Format::Clap) {
        bail!("only VST3 and CLAP can be isolated");
    }
    if !sr.is_finite() || sr <= 0.0 || max_block == 0 || max_block > crate::engine::MAX_BLOCK {
        bail!("invalid isolated audio configuration");
    }
    let (tx, rx) = crossbeam_channel::bounded(128);
    let core = Arc::new(Core {
        stop: AtomicBool::new(false),
        recover: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        map: Mutex::new(None),
        retired: Mutex::new(Vec::new()),
        cursor: AtomicU64::new(0),
        algorithm_latency: AtomicU32::new(0),
        instrument: AtomicBool::new(info.is_instrument()),
        underruns: AtomicU64::new(0),
        overflows: AtomicU64::new(0),
        next_editor: AtomicU64::new(0),
        tx,
        snapshot: Mutex::new(Snapshot {
            metadata: None,
            state: None,
            displays: HashMap::new(),
            status: "pending".into(),
            revision: 0,
        }),
    });
    let child_core = core.clone();
    let child_info = info.clone();
    std::thread::Builder::new()
        .name("plugin-supervisor".into())
        .spawn(move || supervisor::run(child_core, rx, child_info, sr))?;
    let processor = Box::new(processor::RemoteProcessor::new(core.clone()));
    Ok(Created {
        instance: Box::new(RemoteInstance {
            info: info.clone(),
            core,
            params: Vec::new(),
            values: HashMap::new(),
            displays: HashMap::new(),
            state: None,
            latency: 0,
            midi: (true, true),
            editor: true,
            status: "pending".into(),
            revision: 0,
            restart: false,
        }),
        processor,
    })
}
struct RemoteInstance {
    info: PluginInfo,
    core: Arc<Core>,
    params: Vec<ParamDesc>,
    values: HashMap<u32, f64>,
    displays: HashMap<u32, (f64, String)>,
    state: Option<Vec<u8>>,
    latency: u32,
    midi: (bool, bool),
    editor: bool,
    status: String,
    revision: u64,
    restart: bool,
}
impl Drop for RemoteInstance {
    fn drop(&mut self) {
        self.core.stop.store(true, Ordering::Release);
    }
}
impl Instance for RemoteInstance {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn params(&self) -> &[ParamDesc] {
        &self.params
    }
    fn param_value(&self, id: u32) -> f64 {
        self.values.get(&id).copied().unwrap_or(0.0)
    }
    fn param_display(&self, id: u32, value: f64) -> String {
        if let Some((v, s)) = self.displays.get(&id)
            && (*v - value).abs() < 1e-9
        {
            return s.clone();
        }
        self.core.enqueue(Command::Display(id, value));
        format!("{value:.3}")
    }
    fn set_param(&mut self, id: u32, value: f64) {
        if value.is_finite() {
            let v = value.clamp(0.0, 1.0);
            if self.core.enqueue(Command::SetParam(id, v)) {
                self.values.insert(id, v);
            }
        }
    }
    fn latency(&self) -> u32 {
        self.latency.saturating_add(BRIDGE_LATENCY)
    }
    fn bridge_latency(&self) -> u32 {
        BRIDGE_LATENCY
    }
    fn midi_ports(&self) -> (bool, bool) {
        self.midi
    }
    fn remote_status(&self) -> Option<&str> {
        Some(&self.status)
    }
    fn remote_counters(&self) -> Option<(u64, u64)> {
        Some((
            self.core.underruns.load(Ordering::Relaxed),
            self.core.overflows.load(Ordering::Relaxed),
        ))
    }
    fn request_remote_restart(&mut self) -> bool {
        self.core.recover.store(true, Ordering::Release);
        true
    }
    fn idle(&mut self) {
        let Ok(s) = self.core.snapshot.try_lock() else {
            return;
        };
        self.status = s.status.clone();
        self.state = s.state.clone();
        if self.revision != s.revision {
            self.revision = s.revision;
            if let Some(m) = &s.metadata {
                self.restart |= self.params.len() != m.params.len()
                    || self.params.iter().zip(&m.params).any(|(a, b)| a.id != b.id)
                    || self.latency != m.latency
                    || self.midi != m.midi;
                self.params = m.params.clone();
                self.latency = m.latency;
                self.midi = m.midi;
                self.editor = m.editor;
                for (id, v, display) in &m.values {
                    self.values.insert(*id, *v);
                    self.displays.insert(*id, (*v, display.clone()));
                }
            }
        }
        self.displays.extend(s.displays.clone());
    }
    fn wants_restart(&mut self) -> bool {
        std::mem::take(&mut self.restart)
    }
    fn restart(&mut self, sr: f64, max_block: usize) -> Result<Box<dyn Processor>> {
        // This replaces only graph metadata/ports. Instance/supervisor owns the worker,
        // never the transient Processor. Device rate changes require explicit recreation.
        if !sr.is_finite() || max_block > crate::engine::MAX_BLOCK {
            bail!("invalid remote restart configuration");
        }
        Ok(Box::new(processor::RemoteProcessor::new(self.core.clone())))
    }
    fn save_state(&self) -> Option<Vec<u8>> {
        // State serialization is only called from non-audio threads. Briefly wait for a
        // changed landed blob; unchanged blobs fall back to the cached fast-path value.
        let cached = self.state.clone();
        if !self.core.enqueue(Command::Checkpoint) {
            return cached;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(snapshot) = self.core.snapshot.try_lock()
                && snapshot.state != cached
            {
                return snapshot.state.clone();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cached
    }
    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > protocol::MAX_STATE {
            bail!("remote state exceeds 1 MiB limit");
        }
        if !self.core.enqueue(Command::Load(data.to_vec())) {
            bail!("remote control queue is full; state not enqueued");
        }
        // Retain the requested blob even if child crashes before acknowledging restore.
        self.state = Some(data.to_vec());
        if let Ok(mut s) = self.core.snapshot.try_lock() {
            s.state = self.state.clone();
        }
        Ok(())
    }
    fn has_editor(&self) -> bool {
        self.editor
    }
    fn open_editor(&mut self, placement: Placement) -> Result<Box<dyn Editor>> {
        let token = self
            .core
            .next_editor
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("remote editor token overflow"))?
            + 1;
        if !self.core.enqueue(Command::Open { token, placement }) {
            bail!("remote editor queue is full");
        }
        Ok(Box::new(RemoteEditor {
            core: self.core.clone(),
            token,
            placement,
            size: (640, 480),
            opened: Instant::now(),
            seen_open: false,
        }))
    }
}

struct RemoteEditor {
    core: Arc<Core>,
    token: u64,
    placement: Placement,
    size: (u32, u32),
    opened: Instant,
    seen_open: bool,
}
impl Drop for RemoteEditor {
    fn drop(&mut self) {
        self.core.enqueue(Command::Close(self.token));
    }
}
impl Editor for RemoteEditor {
    fn placement(&self) -> Placement {
        self.placement
    }
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn ready(&self) -> bool {
        self.core
            .snapshot
            .try_lock()
            .ok()
            .and_then(|s| {
                s.metadata
                    .as_ref()
                    .map(|m| m.editor_id == self.token && m.editor_open)
            })
            .unwrap_or(false)
    }
    fn set_zoom(&mut self, f: f32) {
        if f.is_finite() {
            self.core
                .enqueue(Command::Zoom(self.token, f.clamp(0.1, 8.0)));
        }
    }
    fn place(&mut self, spot: Option<&Spot>) {
        // Never touch HWND/XID here: every native operation belongs to child main thread.
        if spot.is_some_and(|s| s.visible.len() > 256) {
            return;
        }
        self.core.enqueue(Command::Place(self.token, spot.cloned()));
    }
    fn idle(&mut self) -> bool {
        if let Ok(s) = self.core.snapshot.try_lock() {
            if let Some(m) = &s.metadata {
                // A previous editor's open/close ACK must not satisfy this open request.
                if m.editor_id == self.token {
                    self.size = m.editor_size;
                    if m.editor_open {
                        self.seen_open = true;
                    } else {
                        return false;
                    }
                } else if self.seen_open || m.editor_id > self.token {
                    return false;
                }
            }
            if s.status.starts_with("faulted") {
                return false;
            }
        }
        self.seen_open || self.opened.elapsed().as_secs() < 30
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_ignores_prior_open_and_close_acknowledgements() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let metadata = Metadata {
            params: Vec::new(),
            values: Vec::new(),
            latency: 0,
            midi: (false, false),
            instrument: false,
            editor: true,
            editor_size: (100, 100),
            editor_open: true,
            editor_id: 1,
        };
        let core = Arc::new(Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            map: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
            cursor: AtomicU64::new(0),
            algorithm_latency: AtomicU32::new(0),
            instrument: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            next_editor: AtomicU64::new(2),
            tx,
            snapshot: Mutex::new(Snapshot {
                metadata: Some(metadata),
                state: None,
                displays: HashMap::new(),
                status: "running".into(),
                revision: 0,
            }),
        });
        let mut editor = RemoteEditor {
            core: core.clone(),
            token: 2,
            placement: Placement::Window,
            size: (640, 480),
            opened: Instant::now(),
            seen_open: false,
        };
        assert!(!editor.ready());
        assert!(editor.idle());
        assert!(!editor.seen_open);
        core.snapshot
            .lock()
            .unwrap()
            .metadata
            .as_mut()
            .unwrap()
            .editor_open = false;
        assert!(editor.idle());
        {
            let mut s = core.snapshot.lock().unwrap();
            let m = s.metadata.as_mut().unwrap();
            m.editor_id = 2;
            m.editor_open = true;
        }
        assert!(editor.ready());
        assert!(editor.idle());
        core.snapshot
            .lock()
            .unwrap()
            .metadata
            .as_mut()
            .unwrap()
            .editor_open = false;
        assert!(!editor.ready());
        assert!(!editor.idle());
        drop(editor);
        assert!(matches!(rx.try_recv().unwrap(), Command::Close(2)));
    }
}
