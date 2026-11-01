//! Experimental whole-plugin process isolation. See README.md for limits and ABI contract.
mod processor;
mod protocol;
mod shared;
mod supervisor;
mod surface;
mod worker;
use super::{Created, Editor, Format, Instance, ParamDesc, Placement, PluginInfo, Spot};
use crate::engine::Processor;
use crate::plugins::surface::{EditorFrame, EditorPointer, PointerKind};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Sender, TrySendError};
use protocol::{Command, Metadata};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};
use surface::FrameMap;
pub use worker::worker_main;

pub const BRIDGE_LATENCY: u32 = shared::DELAY as u32;
#[derive(Clone)]
struct Snapshot {
    metadata: Option<Metadata>,
    state: Option<Vec<u8>>,
    checkpoint_seq: u64,
    displays: HashMap<u32, (f64, String)>,
    status: String,
    revision: u64,
}
struct Core {
    stop: AtomicBool,
    recover: AtomicBool,
    ready: AtomicBool,
    map: Mutex<Option<Arc<shared::Shared>>>,
    /// GUI images have separate ownership and storage; the audio callback never reads this.
    surface: Mutex<Option<Arc<FrameMap>>>,
    /// Reserved close path when the bounded control queue cannot accept a close/release.
    close_editor: AtomicU64,
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
    if cfg!(target_os = "macos") && !crate::runtime::get().experimental_editor_capture {
        bail!("plugin isolation on macOS requires --experimental-editor-capture");
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
        surface: Mutex::new(None),
        close_editor: AtomicU64::new(0),
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
            checkpoint_seq: 0,
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
        // State serialization is only called from non-audio threads. Wait for a landed
        // checkpoint reply, falling back to the cached value on failure or timeout.
        let cached = self.state.clone();
        let seq_before = self.core.snapshot.lock().unwrap().checkpoint_seq;
        if !self.core.enqueue(Command::Checkpoint) {
            return cached;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(snapshot) = self.core.snapshot.try_lock()
                && snapshot.checkpoint_seq > seq_before
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
        let captured = crate::runtime::get().experimental_editor_capture
            && matches!(placement, Placement::Embedded(_));
        let token = self
            .core
            .next_editor
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("remote editor token overflow"))?
            + 1;
        if !self.core.enqueue(Command::Open {
            token,
            placement,
            captured,
        }) {
            bail!("remote editor queue is full");
        }
        Ok(Box::new(RemoteEditor {
            core: self.core.clone(),
            token,
            placement,
            size: (640, 480),
            opened: Instant::now(),
            seen_open: false,
            captured,
            pending_input: VecDeque::new(),
            input_failed: false,
            error: None,
            native_error: None,
            generation: None,
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
    captured: bool,
    pending_input: VecDeque<(u64, EditorPointer)>,
    input_failed: bool,
    error: Option<String>,
    native_error: Option<String>,
    generation: Option<u64>,
}
enum SurfaceAccess {
    Busy,
    Missing,
    Ready(Arc<FrameMap>),
}
impl RemoteEditor {
    const MAX_PENDING_INPUT: usize = 256;

    fn active_surface(&self) -> Result<SurfaceAccess> {
        let surface = match self.core.surface.try_lock() {
            Ok(map) => match map.as_ref() {
                Some(map) => map.clone(),
                None => return Ok(SurfaceAccess::Missing),
            },
            Err(TryLockError::WouldBlock) => return Ok(SurfaceAccess::Busy),
            Err(TryLockError::Poisoned(_)) => bail!("isolated editor surface state poisoned"),
        };
        if surface.token() != self.token {
            return Ok(SurfaceAccess::Missing);
        }
        let generation = match self.core.map.try_lock() {
            Ok(map) => map.as_ref().map(|map| map.generation()),
            Err(TryLockError::WouldBlock) => return Ok(SurfaceAccess::Busy),
            Err(TryLockError::Poisoned(_)) => bail!("isolated worker state poisoned"),
        };
        Ok(if generation == Some(surface.generation()) {
            SurfaceAccess::Ready(surface)
        } else {
            SurfaceAccess::Missing
        })
    }

    fn flush_input(&mut self) -> Result<()> {
        let surface = match self.active_surface()? {
            SurfaceAccess::Ready(surface) => surface,
            SurfaceAccess::Busy => return Ok(()),
            SurfaceAccess::Missing => {
                // Never replay transitions intended for a retired worker/editor.
                self.pending_input.clear();
                self.generation = None;
                return Ok(());
            }
        };
        self.generation = Some(surface.generation());
        while let Some(&(generation, event)) = self.pending_input.front() {
            if generation != surface.generation() {
                self.pending_input.pop_front();
                continue;
            }
            match self.core.tx.try_send(Command::Pointer {
                generation,
                token: self.token,
                event,
            }) {
                Ok(()) => {
                    self.pending_input.pop_front();
                }
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => {
                    bail!("isolated editor control connection closed")
                }
            }
        }
        Ok(())
    }

    fn fail_input(&mut self, message: String) {
        self.error = Some(message);
        self.input_failed = true;
        self.pending_input.clear();
        // Closing the child-owned editor releases its native drag even if the ordinary
        // command queue is saturated. This bounded slot cannot lose a release transition.
        self.core
            .close_editor
            .fetch_max(self.token, Ordering::AcqRel);
    }
}
impl Drop for RemoteEditor {
    fn drop(&mut self) {
        if !self.core.enqueue(Command::Close(self.token)) {
            self.core
                .close_editor
                .fetch_max(self.token, Ordering::AcqRel);
        }
    }
}
impl Editor for RemoteEditor {
    fn placement(&self) -> Placement {
        self.placement
    }
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn is_captured(&self) -> bool {
        self.captured
    }
    fn take_frame(&mut self) -> Option<EditorFrame> {
        if !self.captured || self.input_failed || !self.ready() {
            return None;
        }
        let surface = match self.active_surface() {
            Ok(SurfaceAccess::Ready(surface)) => surface,
            Ok(SurfaceAccess::Busy | SurfaceAccess::Missing) => return None,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                return None;
            }
        };
        self.generation = Some(surface.generation());
        match surface.take_latest() {
            Ok(Some(frame)) => {
                self.size = frame.native_size;
                self.error = None;
                Some(frame)
            }
            Ok(None) => None,
            Err(error) => {
                self.error = Some(format!("isolated editor capture: {error:#}"));
                None
            }
        }
    }
    fn capture_error(&self) -> Option<&str> {
        self.error.as_deref().or(self.native_error.as_deref())
    }
    fn pointer(&mut self, event: EditorPointer) -> Result<()> {
        event.validate()?;
        if !self.captured || self.input_failed {
            bail!("isolated editor does not accept captured input");
        }
        self.flush_input()?;
        let generation = match self.active_surface()? {
            SurfaceAccess::Ready(surface) => surface.generation(),
            SurfaceAccess::Busy => self
                .generation
                .context("isolated editor capture is not ready")?,
            SurfaceAccess::Missing => bail!("isolated editor capture is not ready"),
        };
        if matches!(event.kind, PointerKind::Move)
            && let Some((previous_generation, previous)) = self.pending_input.back_mut()
            && *previous_generation == generation
            && matches!(previous.kind, PointerKind::Move)
        {
            *previous = event;
            return self.flush_input();
        }
        if self.pending_input.len() >= Self::MAX_PENDING_INPUT {
            // Moves and wheel deltas are disposable under pressure; button ordering is not.
            self.pending_input.retain(|(_, event)| {
                matches!(event.kind, PointerKind::Down(_) | PointerKind::Up(_))
            });
        }
        if self.pending_input.len() >= Self::MAX_PENDING_INPUT {
            if matches!(event.kind, PointerKind::Move | PointerKind::Wheel { .. }) {
                return Ok(());
            }
            let message = "isolated editor input queue overflow; editor closed to release its drag"
                .to_owned();
            self.fail_input(message.clone());
            bail!(message);
        }
        self.pending_input.push_back((generation, event));
        if let Err(error) = self.flush_input() {
            self.fail_input(format!("{error:#}"));
            return Err(error);
        }
        Ok(())
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
        if !self.captured && f.is_finite() {
            self.core
                .enqueue(Command::Zoom(self.token, f.clamp(0.1, 8.0)));
        }
    }
    fn place(&mut self, spot: Option<&Spot>) {
        // Never touch HWND/XID here: every native operation belongs to child main thread.
        if self.captured || spot.is_some_and(|s| s.visible.len() > 256) {
            return;
        }
        self.core.enqueue(Command::Place(self.token, spot.cloned()));
    }
    fn idle(&mut self) -> bool {
        if self.input_failed {
            return false;
        }
        if let Ok(s) = self.core.snapshot.try_lock() {
            if let Some(m) = &s.metadata {
                // A previous editor's open/close ACK must not satisfy this open request.
                if m.editor_id == self.token {
                    self.size = m.editor_size;
                    self.native_error = m.editor_error.clone();
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
        if let Err(error) = self.flush_input() {
            self.fail_input(format!("{error:#}"));
            return false;
        }
        self.seen_open || self.opened.elapsed().as_secs() < 30
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured_editor(capacity: usize) -> (RemoteEditor, crossbeam_channel::Receiver<Command>) {
        use ring::rand::SecureRandom;
        let mut random = [0; 16];
        ring::rand::SystemRandom::new().fill(&mut random).unwrap();
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let map = Arc::new(
            shared::Shared::create(
                std::env::temp_dir().join(format!("nodeplayer-captured-input-test-{suffix}")),
                7,
            )
            .unwrap(),
        );
        let surface = Arc::new(FrameMap::create(7, 2).unwrap());
        let (tx, rx) = crossbeam_channel::bounded(capacity);
        let core = Arc::new(Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            map: Mutex::new(Some(map)),
            surface: Mutex::new(Some(surface)),
            close_editor: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            cursor: AtomicU64::new(0),
            algorithm_latency: AtomicU32::new(0),
            instrument: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            next_editor: AtomicU64::new(2),
            tx,
            snapshot: Mutex::new(Snapshot {
                metadata: Some(Metadata {
                    params: Vec::new(),
                    values: Vec::new(),
                    latency: 0,
                    midi: (false, false),
                    instrument: false,
                    editor: true,
                    editor_size: (640, 360),
                    editor_open: true,
                    editor_id: 2,
                    editor_error: None,
                }),
                state: None,
                checkpoint_seq: 0,
                displays: HashMap::new(),
                status: "running".into(),
                revision: 0,
            }),
        });
        (
            RemoteEditor {
                core,
                token: 2,
                placement: Placement::Embedded(1),
                size: (640, 360),
                opened: Instant::now(),
                seen_open: true,
                captured: true,
                pending_input: VecDeque::new(),
                input_failed: false,
                error: None,
                native_error: None,
                generation: Some(7),
            },
            rx,
        )
    }

    fn pointer(kind: PointerKind, x: f64, buttons: u8) -> EditorPointer {
        EditorPointer {
            position: [x, 90.0],
            kind,
            buttons,
        }
    }

    #[test]
    fn captured_editor_never_sends_native_placement_or_graph_zoom() {
        let (mut editor, rx) = captured_editor(4);
        editor.set_zoom(0.5);
        editor.set_zoom(1.5);
        editor.place(None);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn captured_pointer_transitions_retry_in_order_under_queue_pressure() {
        let (mut editor, rx) = captured_editor(1);
        editor.core.tx.try_send(Command::Poll).unwrap();
        editor
            .pointer(pointer(PointerKind::Down(1), 10.0, 1))
            .unwrap();
        editor.pointer(pointer(PointerKind::Move, 20.0, 1)).unwrap();
        editor.pointer(pointer(PointerKind::Move, 30.0, 1)).unwrap();
        editor
            .pointer(pointer(PointerKind::Up(1), 40.0, 0))
            .unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Command::Poll));
        editor.flush_input().unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), Command::Pointer { generation: 7, token: 2, event } if matches!(event.kind, PointerKind::Down(1)) && event.position[0] == 10.0)
        );
        editor.flush_input().unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), Command::Pointer { event, .. } if matches!(event.kind, PointerKind::Move) && event.position[0] == 30.0)
        );
        editor.flush_input().unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), Command::Pointer { event, .. } if matches!(event.kind, PointerKind::Up(1)) && event.position[0] == 40.0 && event.buttons == 0)
        );
        assert!(editor.pending_input.is_empty());
    }

    #[test]
    fn surface_lock_contention_retains_button_transitions_for_retry() {
        let (mut editor, rx) = captured_editor(4);
        let core = editor.core.clone();
        let lock = core.surface.lock().unwrap();
        editor
            .pointer(pointer(PointerKind::Down(1), 10.0, 1))
            .unwrap();
        editor
            .pointer(pointer(PointerKind::Up(1), 20.0, 0))
            .unwrap();
        assert_eq!(editor.pending_input.len(), 2);
        assert!(rx.try_recv().is_err());
        drop(lock);
        editor.flush_input().unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), Command::Pointer { event, .. } if matches!(event.kind, PointerKind::Down(1)))
        );
        assert!(
            matches!(rx.try_recv().unwrap(), Command::Pointer { event, .. } if matches!(event.kind, PointerKind::Up(1)))
        );
    }

    #[test]
    fn retired_surface_never_delivers_pixels_or_input_to_a_replacement() {
        let (mut editor, rx) = captured_editor(4);
        let replacement = Arc::new(FrameMap::create(8, 2).unwrap());
        replacement
            .publish(&EditorFrame {
                width: 1,
                height: 1,
                native_size: (1, 1),
                rgba: vec![1, 2, 3, 255],
            })
            .unwrap();
        *editor.core.surface.lock().unwrap() = Some(replacement);
        assert!(
            editor.take_frame().is_none(),
            "worker generation must match"
        );
        assert!(
            editor
                .pointer(pointer(PointerKind::Down(1), 10.0, 1))
                .is_err()
        );
        *editor.core.surface.lock().unwrap() = Some(Arc::new(FrameMap::create(7, 3).unwrap()));
        assert!(editor.take_frame().is_none(), "editor token must match");
        assert!(
            editor
                .pointer(pointer(PointerKind::Down(1), 10.0, 1))
                .is_err()
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn transition_overflow_and_full_queue_close_use_reserved_slot() {
        let (mut editor, rx) = captured_editor(1);
        editor.core.tx.try_send(Command::Poll).unwrap();
        for index in 0..RemoteEditor::MAX_PENDING_INPUT {
            let down = index % 2 == 0;
            editor
                .pointer(pointer(
                    if down {
                        PointerKind::Down(1)
                    } else {
                        PointerKind::Up(1)
                    },
                    index as f64,
                    u8::from(down),
                ))
                .unwrap();
        }
        assert!(
            editor
                .pointer(pointer(PointerKind::Down(1), 400.0, 1))
                .is_err()
        );
        assert_eq!(
            editor.core.close_editor.load(Ordering::Acquire),
            editor.token
        );
        assert!(!editor.idle());
        assert!(editor.capture_error().unwrap().contains("editor closed"));
        assert!(matches!(rx.try_recv().unwrap(), Command::Poll));
    }

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
            editor_error: None,
        };
        let core = Arc::new(Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            map: Mutex::new(None),
            surface: Mutex::new(None),
            close_editor: AtomicU64::new(0),
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
                checkpoint_seq: 0,
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
            captured: false,
            pending_input: VecDeque::new(),
            input_failed: false,
            error: None,
            native_error: None,
            generation: None,
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
