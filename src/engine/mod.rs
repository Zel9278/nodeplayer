//! Real-time audio engine.
//!
//! The UI thread owns the graph model and compiles it into a [`Schedule`]. Nodes and schedules
//! are moved to the audio thread through a lock-free command ring; whatever the audio thread
//! replaces is sent back through a garbage ring so it is freed off the real-time thread.

pub mod compensation;
pub mod eq;
pub mod nodes;
pub mod timing;

use compensation::EdgeDelay;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const MAX_BLOCK: usize = 1024;
pub const MAX_NODES: usize = 1024;
const CMD_QUEUE_LEN: usize = 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct StereoBuf {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

impl StereoBuf {
    pub fn new() -> Self {
        Self { l: vec![0.0; MAX_BLOCK], r: vec![0.0; MAX_BLOCK] }
    }

    pub fn clear(&mut self, n: usize) {
        self.l[..n].fill(0.0);
        self.r[..n].fill(0.0);
    }

    pub fn add_from(&mut self, other: &StereoBuf, n: usize) {
        for (d, s) in self.l[..n].iter_mut().zip(&other.l[..n]) {
            *d += s;
        }
        for (d, s) in self.r[..n].iter_mut().zip(&other.r[..n]) {
            *d += s;
        }
    }
}

/// A short MIDI message (no sysex) at frame `time` of the current block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiEvent {
    pub time: u32,
    pub data: [u8; 3],
}

impl MidiEvent {
    pub fn new(time: u32, data: &[u8]) -> Option<Self> {
        let len = midi_len(*data.first()?);
        if len == 0 || data.len() < len {
            return None;
        }
        let mut d = [0; 3];
        d[..len].copy_from_slice(&data[..len]);
        Some(Self { time, data: d })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data[..midi_len(self.data[0])]
    }

    pub fn channel(&self) -> u8 {
        self.data[0] & 0x0f
    }
}

/// Length of a short message by its status byte; 0 for sysex and data bytes.
pub fn midi_len(status: u8) -> usize {
    match status {
        0x80..=0xbf | 0xe0..=0xef | 0xf2 => 3,
        0xc0..=0xdf | 0xf1 | 0xf3 => 2,
        0xf6 | 0xf8..=0xff => 1,
        _ => 0,
    }
}

/// Preallocated MIDI events of one port for one block, sorted by time.
pub struct MidiBuf {
    events: Vec<MidiEvent>,
}

impl MidiBuf {
    /// Black MIDI brings tens of thousands of events a block.
    pub const CAPACITY: usize = 1 << 14;

    pub fn new() -> Self {
        Self { events: Vec::with_capacity(Self::CAPACITY) }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Appends an event, dropping it when the buffer is full. Callers push in time order.
    pub fn push(&mut self, e: MidiEvent) -> bool {
        if self.events.len() < Self::CAPACITY {
            self.events.push(e);
            true
        } else {
            false
        }
    }

    pub fn events(&self) -> &[MidiEvent] {
        &self.events
    }

    /// Inserts without clamping future timestamps; PDC uses an absolute future queue.
    pub fn push_sorted(&mut self, event: MidiEvent) -> bool {
        if self.events.len() == Self::CAPACITY {
            return false;
        }
        let at = self.events.partition_point(|e| e.time <= event.time);
        self.events.insert(at, event);
        true
    }

    /// Merges `other` in, keeping time order (and arrival order for equal times) without
    /// allocating.
    pub fn merge(&mut self, other: &[MidiEvent], n: usize) {
        let last = n.saturating_sub(1) as u32;
        for e in other {
            let e = MidiEvent { time: e.time.min(last), ..*e };
            let at = self.events.partition_point(|x| x.time <= e.time);
            if self.events.len() < Self::CAPACITY {
                self.events.insert(at, e);
            }
        }
    }
}

impl Default for MidiBuf {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Song time at the start of an audio block, separate from the engine's sample clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transport {
    pub state: PlaybackState,
    pub position_seconds: f64,
    /// File seconds per output frame (includes varispeed); zero while seeking or paused.
    pub seconds_per_frame: f64,
}

impl Transport {
    pub fn is_playing(self) -> bool {
        self.state == PlaybackState::Playing
    }

    pub fn at_offset(self, frames: usize) -> Self {
        Self {
            position_seconds: self.position_seconds
                + if self.is_playing() {
                    frames as f64 * self.seconds_per_frame
                } else {
                    0.0
                },
            ..self
        }
    }
}

/// A DSP node running on the audio thread. All audio ports are stereo; a node has at most one
/// MIDI input and one MIDI output.
pub trait Processor: Send {
    /// Source nodes provide the block's song clock. Must never wait for another thread.
    fn transport(&mut self) -> Option<Transport> {
        None
    }

    /// Called before processing, including while paused/stopped so tails and live MIDI run.
    fn set_transport(&mut self, _transport: Transport) {}

    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]);

    /// [`process`](Self::process) for nodes with MIDI ports. `midi_out` starts out empty.
    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        _midi_in: &[MidiEvent],
        _midi_out: &mut MidiBuf,
    ) {
        self.process(n, inputs, outputs);
    }

    /// Engine sample time, independent of track position. Existing processors remain source
    /// compatible; bridges override this to timestamp their fixed-latency data plane.
    fn process_timed(
        &mut self,
        _first_sample: u64,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process_midi(n, inputs, outputs, midi_in, midi_out);
    }

    fn set_param(&mut self, _index: usize, _value: f32) {}

    /// Apply pending parameter edits on the processing thread without synthesizing an
    /// audio block or advancing either timeline. Used before state save/restart quiescence.
    fn flush_params(&mut self) {}

    /// Suspend/resume a hosted inner processor while retaining the wrapper's warm
    /// compensation history. Unsupported processors return the supplied ownership.
    fn replace_plugin_inner(&mut self, replacement: Option<Box<dyn Processor>>) -> Option<Box<dyn Processor>> {
        replacement
    }

    /// Replace preallocated dry-path timing without restarting the plugin. Always return
    /// ownership of an allocation for control-thread retirement, even when unsupported.
    fn replace_latency_bypass(
        &mut self,
        bypass: Box<nodes::LatencyBypass>,
    ) -> Box<nodes::LatencyBypass> {
        bypass
    }

    /// Called on the audio thread when the node leaves the engine (removed or replaced), so
    /// plugins can stop processing on the thread that ran them.
    fn deactivate(&mut self) {}
}

pub struct NodeSlot {
    proc: Box<dyn Processor>,
    /// The plugin running in `proc`, for crash reports.
    tag: Option<std::sync::Arc<crate::plugins::guard::Tag>>,
    ins: Vec<StereoBuf>,
    outs: Vec<StereoBuf>,
    midi_in: MidiBuf,
    midi_out: MidiBuf,
}

impl NodeSlot {
    pub fn new(proc: Box<dyn Processor>, inputs: usize, outputs: usize) -> Box<Self> {
        Box::new(Self {
            proc,
            tag: None,
            ins: (0..inputs).map(|_| StereoBuf::new()).collect(),
            outs: (0..outputs).map(|_| StereoBuf::new()).collect(),
            midi_in: MidiBuf::new(),
            midi_out: MidiBuf::new(),
        })
    }

    pub fn with_tag(mut self: Box<Self>, tag: Option<std::sync::Arc<crate::plugins::guard::Tag>>) -> Box<Self> {
        self.tag = tag;
        self
    }

    pub fn tag(&self) -> Option<std::sync::Arc<crate::plugins::guard::Tag>> {
        self.tag.clone()
    }

    fn enter(&self) -> Option<crate::plugins::guard::Entered> {
        self.tag.as_deref().map(crate::plugins::guard::enter_borrowed)
    }
}

pub struct Step {
    pub node: u32,
    /// For each input port, the `(node, output port)` pairs summed into it.
    pub inputs: Vec<Vec<(u32, u32)>>,
    /// Nodes whose MIDI output is merged into this node's MIDI input.
    pub midi: Vec<u32>,
}

/// Nodes in topological order plus the sinks whose first input goes to the device.
#[derive(Default)]
pub struct Schedule {
    pub steps: Vec<Step>,
    pub sinks: Vec<u32>,
    /// None is the legacy, uncompensated execution path (default).
    pub timing: Option<compensation::TimingPlan>,
}

impl Schedule {
    pub fn epoch(&self) -> u64 {
        self.timing.as_ref().map_or(0, |plan| plan.epoch)
    }

    #[cfg(test)]
    pub fn total_latency_frames(&self) -> u64 {
        self.timing
            .as_ref()
            .map_or(0, |plan| plan.total_latency_frames)
    }
}

#[derive(Default)]
struct EngineStatus {
    sample_clock: AtomicU64,
    applied_epoch: AtomicU64,
    midi_overflows: AtomicU64,
}

pub enum Command {
    AddNode(u32, Box<NodeSlot>),
    RemoveNode(u32),
    SetSchedule(Box<Schedule>),
    SetParam(u32, usize, f32),
    ConfigureBypass(u32, Box<nodes::LatencyBypass>),
    SuspendPlugin(u32),
    ResumePlugin(u32, Box<dyn Processor>),
}

/// Objects handed back to the UI thread to be freed there.
pub enum Garbage {
    Node(u32, Box<NodeSlot>),
    Schedule(#[allow(dead_code)] Box<Schedule>),
    Bypass(#[allow(dead_code)] Box<nodes::LatencyBypass>),
    PluginSuspended(u32, Option<Box<dyn Processor>>),
    PluginInner(#[allow(dead_code)] Box<dyn Processor>),
}

struct AudioState {
    slots: Vec<Option<Box<NodeSlot>>>,
    schedule: Box<Schedule>,
    cmd_rx: rtrb::Consumer<Command>,
    garbage_tx: rtrb::Producer<Garbage>,
    sample_clock: u64,
    status: Arc<EngineStatus>,
}

impl AudioState {
    /// Finish plugin processing on the thread that ran it. Ownership is then handed back
    /// to the control thread rather than freeing processors in the audio callback.
    fn deactivate_all(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            let _g = slot
                .tag
                .as_deref()
                .map(crate::plugins::guard::enter_borrowed);
            slot.proc.deactivate();
        }
    }

    fn retire(mut self) {
        // Commands not applied before shutdown can also own plugin processors. Retire
        // these on the control thread while their UI-side instances are still alive.
        while let Ok(command) = self.cmd_rx.pop() {
            drop(command);
        }
        for slot in &mut self.slots {
            if let Some(slot) = slot.take() {
                let _g = slot
                    .tag
                    .as_deref()
                    .map(crate::plugins::guard::enter_borrowed);
                drop(slot);
            }
        }
    }

    fn throw_away(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage_tx.push(g) {
            // Should never happen with a sane queue size; leak rather than free on this thread.
            std::mem::forget(g);
        }
    }

    fn handle_commands(&mut self) {
        // Every command can retire at most one object. Stop before popping when retirement
        // has no space; ownership and processing continue next callback after UI collection.
        for _ in 0..CMD_QUEUE_LEN {
            if self.garbage_tx.is_full() {
                break;
            }
            let Ok(cmd) = self.cmd_rx.pop() else { break };
            match cmd {
                Command::AddNode(id, slot) => {
                    if let Some(plan) = &mut self.schedule.timing {
                        plan.invalidate_node(id);
                    }
                    if let Some(mut old) = self.slots[id as usize].replace(slot) {
                        let _g = old.enter();
                        old.proc.deactivate();
                        self.throw_away(Garbage::Node(id, old));
                    }
                }
                Command::RemoveNode(id) => {
                    if let Some(plan) = &mut self.schedule.timing {
                        plan.invalidate_node(id);
                    }
                    if let Some(mut old) = self.slots[id as usize].take() {
                        let g = old.enter();
                        old.proc.deactivate();
                        drop(g);
                        self.throw_away(Garbage::Node(id, old));
                    }
                }
                Command::SetSchedule(mut s) => {
                    if let (Some(new), Some(old)) = (&mut s.timing, &mut self.schedule.timing) {
                        new.inherit(old);
                    }
                    let epoch = s.epoch();
                    let old = std::mem::replace(&mut self.schedule, s);
                    self.status.applied_epoch.store(epoch, Ordering::Release);
                    self.throw_away(Garbage::Schedule(old));
                }
                Command::SetParam(id, idx, v) => {
                    if let Some(slot) = &mut self.slots[id as usize] {
                        slot.proc.set_param(idx, v);
                    }
                }
                Command::ConfigureBypass(id, bypass) => {
                    let retired = match &mut self.slots[id as usize] {
                        Some(slot) => slot.proc.replace_latency_bypass(bypass),
                        None => bypass,
                    };
                    self.throw_away(Garbage::Bypass(retired));
                }
                Command::SuspendPlugin(id) => {
                    let old = if let Some(slot) = &mut self.slots[id as usize] {
                        let _g = slot.tag.as_deref().map(crate::plugins::guard::enter_borrowed);
                        let mut old = slot.proc.replace_plugin_inner(None);
                        if let Some(proc) = &mut old {
                            proc.flush_params();
                            proc.deactivate();
                        }
                        old
                    } else { None };
                    self.throw_away(Garbage::PluginSuspended(id, old));
                }
                Command::ResumePlugin(id, proc) => {
                    let retired = if let Some(slot) = &mut self.slots[id as usize] {
                        let _g = slot.tag.as_deref().map(crate::plugins::guard::enter_borrowed);
                        slot.proc.replace_plugin_inner(Some(proc))
                    } else { Some(proc) };
                    if let Some(mut proc) = retired {
                        proc.deactivate();
                        self.throw_away(Garbage::PluginInner(proc));
                    }
                }
            }
        }
    }

    fn run_block(&mut self, n: usize) {
        // Read the source before any node runs, so graph order cannot move the playhead by
        // a block or give disconnected plugins a different playback state.
        let transport = self
            .schedule
            .steps
            .iter()
            .find_map(|step| self.slots[step.node as usize].as_mut()?.proc.transport())
            .unwrap_or_default();
        for (step_index, step) in self.schedule.steps.iter().enumerate() {
            let Some(mut slot) = self.slots[step.node as usize].take() else {
                continue;
            };
            slot.midi_in.clear();
            if let Some(plan) = &mut self.schedule.timing {
                let routes = &mut plan.steps[step_index];
                if routes.midi_cleanup && n > 0 {
                    for channel in 0..16 {
                        for cc in [64, 123, 120] {
                            slot.midi_in.push(MidiEvent {
                                time: 0,
                                data: [0xb0 | channel, cc, 0],
                            });
                        }
                    }
                    routes.midi_cleanup = false;
                }
                for port in &mut slot.ins {
                    port.clear(n);
                }
                for (port, routes) in slot.ins.iter_mut().zip(&routes.audio) {
                    for &index in routes {
                        let edge = &mut plan.edges[index];
                        let timing::EdgeKind::Audio { output, .. } = edge.key.kind else {
                            unreachable!()
                        };
                        let source = self.slots[edge.key.from as usize]
                            .as_ref()
                            .and_then(|slot| slot.outs.get(output as usize));
                        if let EdgeDelay::Audio(delay) = &mut edge.delay {
                            delay.add(source, port, n);
                        }
                    }
                }
                for &index in &routes.midi {
                    let edge = &mut plan.edges[index];
                    let source = self.slots[edge.key.from as usize]
                        .as_ref()
                        .map_or(&[][..], |slot| slot.midi_out.events());
                    if let EdgeDelay::Midi(delay) = &mut edge.delay {
                        delay.process(source, &mut slot.midi_in, self.sample_clock, n);
                    }
                }
            } else {
                for (port, sources) in slot.ins.iter_mut().zip(&step.inputs) {
                    port.clear(n);
                    for &(src, out) in sources {
                        if let Some(s) = &self.slots[src as usize]
                            && let Some(b) = s.outs.get(out as usize)
                        {
                            port.add_from(b, n);
                        }
                    }
                }
                for &src in &step.midi {
                    if let Some(s) = &self.slots[src as usize] {
                        slot.midi_in.merge(s.midi_out.events(), n);
                    }
                }
            }
            let NodeSlot { proc, tag, ins, outs, midi_in, midi_out } = &mut *slot;
            midi_out.clear();
            // Hardware-bound MIDI nodes emit what they receive; delaying the input aligns
            // their emissions with the device reference, like audio sinks are.
            let input = if let Some(plan) = &mut self.schedule.timing
                && let Some((_, delay, aligned)) = plan.midi_outputs.iter_mut().find(|(node, _, _)| *node == step.node)
            {
                aligned.clear();
                delay.process(midi_in.events(), aligned, self.sample_clock, n);
                aligned.events()
            } else {
                midi_in.events()
            };
            let _g = tag.as_deref().map(crate::plugins::guard::enter_borrowed);
            proc.set_transport(transport);
            proc.process_timed(self.sample_clock, n, ins, outs, input, midi_out);
            self.slots[step.node as usize] = Some(slot);
        }
    }

    fn render(&mut self, data: &mut [f32], channels: usize) {
        self.handle_commands();
        if channels == 0 {
            data.fill(0.0);
            return;
        }
        let frames = data.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.run_block(n);
            let out = &mut data[done * channels..(done + n) * channels];
            out.fill(0.0);
            if let Some(plan) = &mut self.schedule.timing {
                for sink in &mut plan.sinks {
                    let source = self.slots[sink.node as usize]
                        .as_ref()
                        .and_then(|slot| slot.ins.first());
                    sink.delay.mix_device(source, out, channels);
                }
                self.status
                    .midi_overflows
                    .store(plan.midi_overflows(), Ordering::Relaxed);
            } else {
                for &sink in &self.schedule.sinks {
                    let Some(slot) = &self.slots[sink as usize] else {
                        continue;
                    };
                    let Some(buf) = slot.ins.first() else {
                        continue;
                    };
                    for (i, frame) in out.chunks_exact_mut(channels).enumerate() {
                        frame[0] += buf.l[i];
                        if channels > 1 {
                            frame[1] += buf.r[i];
                        }
                    }
                }
            }
            let Some(next) = self.sample_clock.checked_add(n as u64) else {
                // Exhaustion is unreachable in a practical session; never wrap stale MIDI.
                out.fill(0.0);
                return;
            };
            self.sample_clock = next;
            self.status.sample_clock.store(next, Ordering::Release);
            done += n;
        }
    }
}

/// One owner holds the processing state: the startup cell, the audio thread, or the
/// returned-state channel. Receiving that state is the shutdown completion handshake.
struct AudioShutdown {
    stop: Arc<AtomicBool>,
    startup: Arc<std::sync::Mutex<Option<AudioState>>>,
    returned: crossbeam_channel::Receiver<AudioState>,
    null_thread: Option<std::thread::JoinHandle<()>>,
    result: Option<bool>,
}

impl AudioShutdown {
    fn take_returned(&self) -> Option<AudioState> {
        self.returned.try_recv().ok().or_else(|| {
            // A device that never called back, or an immediate close before the null
            // thread starts, has no running processors. Claim its untouched state here.
            self.startup.try_lock().ok()?.take()
        })
    }
}

fn return_audio_state(mut state: AudioState, returned: &crossbeam_channel::Sender<AudioState>) {
    state.deactivate_all();
    if let Err(error) = returned.try_send(state) {
        // The UI already timed out and retained the plugin owners. Never destroy the
        // remaining processor references on this thread after abandoning the handshake.
        std::mem::forget(error.into_inner());
    }
}

pub struct Engine {
    _stream: Option<cpal::Stream>,
    shutdown: AudioShutdown,
    status: Arc<EngineStatus>,
    cmd_tx: rtrb::Producer<Command>,
    /// Control-side FIFO only. Legacy send cannot return ownership on pressure, so retains
    /// it here rather than dropping lifecycle commands. try_send supports bounded callers.
    pending: std::collections::VecDeque<Command>,
    garbage_rx: rtrb::Consumer<Garbage>,
    pub sample_rate: u32,
    pub channels: usize,
    pub device_name: String,
}

impl Engine {
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("no output device"))?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "default".into());
        let supported = device.default_output_config().context("default output config")?;
        let mut config = supported.config();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate;

        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let status = Arc::new(EngineStatus::default());
        let state = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: status.clone(),
        };
        let state = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
        let stop = Arc::new(AtomicBool::new(false));
        let (returned_tx, returned) = crossbeam_channel::bounded(1);
        let diagnostic = crate::diagnostics::active().filter(|run| run.metrics.enabled());

        let build = |config: cpal::StreamConfig| {
            let state = state.clone();
            let stop = stop.clone();
            let returned_tx = returned_tx.clone();
            let diagnostic = diagnostic.clone();
            let mut previous_callback: Option<std::time::Instant> = None;
            // A device error can destroy the callback on an arbitrary thread without
            // calling it again. Only the explicit processing-thread handoff may release
            // this state; otherwise shutdown times out and the UI retains plugin owners.
            let mut local: Option<ManuallyDrop<AudioState>> = None;
            device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let started = diagnostic.as_ref().map(|_| std::time::Instant::now());
                    // Take ownership of the state on the first callback; no locking afterwards.
                    if local.is_none() {
                        local = state
                            .try_lock()
                            .ok()
                            .and_then(|mut s| s.take())
                            .map(ManuallyDrop::new);
                    }
                    if stop.load(Ordering::Acquire) {
                        if let Some(state) = local.take() {
                            return_audio_state(ManuallyDrop::into_inner(state), &returned_tx);
                        }
                        data.fill(0.0);
                        return;
                    }
                    match &mut local {
                        Some(s) => s.render(data, channels),
                        None => data.fill(0.0),
                    }
                    if let (Some(run), Some(started)) = (&diagnostic, started) {
                        let ns = |duration: std::time::Duration| {
                            duration.as_nanos().min(u64::MAX as u128) as u64
                        };
                        let frames = data.len() / channels;
                        let period =
                            previous_callback.map(|previous| ns(started.duration_since(previous)));
                        run.metrics.record_callback(
                            frames as u64,
                            ns(started.elapsed()),
                            period,
                            (frames as u64).saturating_mul(1_000_000_000) / sample_rate as u64,
                        );
                        previous_callback = Some(started);
                    }
                },
                |e| log::error!("audio stream error: {e}"),
                None,
            )
        };

        config.buffer_size = cpal::BufferSize::Fixed(512);
        let stream = match build(config) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("fixed buffer size rejected ({e}), using device default");
                config.buffer_size = cpal::BufferSize::Default;
                build(config)?
            }
        };
        stream.play()?;
        log::info!("audio: {device_name} @ {sample_rate} Hz, {channels} ch");

        Ok(Self {
            _stream: Some(stream),
            shutdown: AudioShutdown {
                stop,
                startup: state,
                returned,
                null_thread: None,
                result: None,
            },
            status,
            cmd_tx,
            pending: Default::default(),
            garbage_rx,
            sample_rate,
            channels,
            device_name,
        })
    }

    /// Diagnostic silent output: executes the same DSP and commands without opening a device.
    /// Shutdown has a bounded completion handshake; a hung local plugin requires retaining
    /// its instance for process lifetime (see [`Self::shutdown`]).
    pub fn start_null(sample_rate: u32, block_size: usize) -> Result<Self> {
        anyhow::ensure!(sample_rate > 0, "null sample rate must be nonzero");
        anyhow::ensure!(
            (1..=MAX_BLOCK).contains(&block_size),
            "invalid null block size"
        );
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let status = Arc::new(EngineStatus::default());
        let state = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: status.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let state = Arc::new(std::sync::Mutex::new(Some(state)));
        let startup = state.clone();
        let (returned_tx, returned) = crossbeam_channel::bounded(1);
        let mut output = vec![0.0; block_size * 2];
        let diagnostic = crate::diagnostics::active().filter(|run| run.metrics.enabled());
        let null_thread = std::thread::Builder::new()
            .name("nodeplayer-null-audio".into())
            .spawn(move || {
                let Some(state) = startup.lock().unwrap().take() else {
                    return;
                };
                let mut state = ManuallyDrop::new(state);
                let period =
                    std::time::Duration::from_secs_f64(block_size as f64 / sample_rate as f64);
                let mut deadline = std::time::Instant::now();
                while !thread_stop.load(Ordering::Acquire) {
                    let started = diagnostic.as_ref().map(|_| std::time::Instant::now());
                    state.render(&mut output, 2);
                    if let (Some(run), Some(started)) = (&diagnostic, started) {
                        // Null rendering is DSP time, never report it as an actual device callback.
                        run.metrics
                            .record_dsp(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
                    }
                    deadline += period;
                    let now = std::time::Instant::now();
                    if deadline > now {
                        // park_timeout can return spuriously; only shutdown may end this
                        // wait early, otherwise the null engine would run ahead of time.
                        while !thread_stop.load(Ordering::Acquire) {
                            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                                break;
                            };
                            std::thread::park_timeout(wait);
                        }
                    } else {
                        deadline = now;
                    }
                }
                return_audio_state(ManuallyDrop::into_inner(state), &returned_tx);
            })?;
        Ok(Self {
            _stream: None,
            shutdown: AudioShutdown {
                stop,
                startup: state,
                returned,
                null_thread: Some(null_thread),
                result: None,
            },
            status,
            cmd_tx,
            pending: Default::default(),
            garbage_rx,
            sample_rate,
            channels: 2,
            device_name: "Null output (diagnostic)".into(),
        })
    }

    pub fn sample_clock(&self) -> u64 {
        self.status.sample_clock.load(Ordering::Acquire)
    }

    /// Stop DSP and retire its references before the caller destroys plugin instances.
    /// Returns false after a bounded wait if a callback or plugin will not finish. In that
    /// case the caller must retain its plugin instances/modules for process lifetime; the
    /// engine also retains a live device stream rather than blocking or racing its callback.
    /// A process watchdog is still required for a wedged in-process plugin.
    #[must_use = "retain plugin instances and modules when audio shutdown does not complete"]
    pub fn shutdown(&mut self) -> bool {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> bool {
        if let Some(result) = self.shutdown.result {
            return result;
        }
        self.shutdown.stop.store(true, Ordering::Release);
        if let Some(thread) = &self.shutdown.null_thread {
            thread.thread().unpark();
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(state) = self.shutdown.take_returned() {
                state.retire();
                self.pending.clear();
                while let Ok(garbage) = self.garbage_rx.pop() {
                    drop(garbage);
                }
                self.shutdown.result = Some(true);
                // After the state handoff, future device callbacks can only fill silence.
                drop(self._stream.take());
                if let Some(thread) = self.shutdown.null_thread.take()
                    && thread.is_finished()
                {
                    let _ = thread.join();
                }
                return true;
            }
            if Instant::now() >= deadline {
                self.shutdown.result = Some(false);
                // Dropping a device stream can join its callback thread. A plugin stuck
                // inside that callback must not turn GUI teardown into an unbounded join.
                if let Some(stream) = self._stream.take() {
                    std::mem::forget(stream);
                }
                log::error!("audio shutdown did not complete; plugin lifetimes must be retained");
                return false;
            }
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    /// ACK for the currently applied timing plan; 0 denotes legacy/off.
    pub fn applied_epoch(&self) -> u64 {
        self.status.applied_epoch.load(Ordering::Acquire)
    }

    pub fn midi_overflows(&self) -> u64 {
        self.status.midi_overflows.load(Ordering::Relaxed)
    }

    fn flush_pending(&mut self) {
        while let Some(command) = self.pending.pop_front() {
            if let Err(rtrb::PushError::Full(command)) = self.cmd_tx.push(command) {
                self.pending.push_front(command);
                break;
            }
        }
    }

    /// Nonblocking, reliable ownership on queue-full: the caller retains and retries command.
    /// Never overtakes a lifecycle command retained by legacy send.
    pub fn try_send(&mut self, cmd: Command) -> std::result::Result<(), Command> {
        self.flush_pending();
        if !self.pending.is_empty() {
            return Err(cmd);
        }
        self.cmd_tx
            .push(cmd)
            .map_err(|rtrb::PushError::Full(cmd)| cmd)
    }

    /// Source-compatible nonblocking legacy API. The control-side backlog is intentionally
    /// not bounded (unlike RT queues); use try_send when a bounded producer is required.
    pub fn send(&mut self, cmd: Command) {
        if let Err(cmd) = self.try_send(cmd) {
            self.pending.push_back(cmd);
        }
    }

    pub fn collect_garbage(&mut self) -> Vec<Garbage> {
        let garbage = std::iter::from_fn(|| self.garbage_rx.pop().ok()).collect();
        self.flush_pending();
        garbage
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::player::Player;

    fn write_sine_wav(path: &std::path::Path, rate: u32, secs: f32, amp: f32) {
        let frames = (rate as f32 * secs) as u32;
        let mut data = Vec::new();
        for i in 0..frames {
            let s = (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * amp;
            let v = (s * i16::MAX as f32) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    fn render_until(st: &mut AudioState, out: &mut Vec<f32>, cond: impl Fn(&[f32]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = vec![0.0; 512 * 2];
        while !cond(out) {
            assert!(Instant::now() < deadline, "timed out");
            st.render(&mut buf, 2);
            out.extend_from_slice(&buf);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn offline_state(schedule: Schedule, slots: Vec<(u32, Box<NodeSlot>)>) -> AudioState {
        let (_tx, cmd_rx) = rtrb::RingBuffer::new(16);
        let (garbage_tx, _rx) = rtrb::RingBuffer::new(16);
        let mut state = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::new(schedule),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: Arc::default(),
        };
        for (id, slot) in slots {
            state.slots[id as usize] = Some(slot);
        }
        state
    }

    #[test]
    fn transport_is_shared_before_sources_run_and_through_plugin_bypass() {
        struct Source(Arc<std::sync::Mutex<Transport>>);
        impl Processor for Source {
            fn transport(&mut self) -> Option<Transport> {
                Some(*self.0.lock().unwrap())
            }
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {
                let mut transport = self.0.lock().unwrap();
                *transport = transport.at_offset(64);
            }
        }
        struct Capture(Arc<std::sync::Mutex<Vec<Transport>>>, Transport);
        impl Processor for Capture {
            fn set_transport(&mut self, transport: Transport) {
                self.1 = transport;
            }
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {
                self.0.lock().unwrap().push(self.1);
            }
        }
        let clock = Arc::new(std::sync::Mutex::new(Transport {
            state: PlaybackState::Playing,
            position_seconds: 3.0,
            seconds_per_frame: 1.0 / 48_000.0,
        }));
        let before = Arc::new(std::sync::Mutex::new(Vec::new()));
        let after = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut wrapped = nodes::PluginProc::with_latency(
            Box::new(Capture(after.clone(), Transport::default())),
            0,
            0,
            false,
        )
        .unwrap();
        wrapped.set_param(0, 1.0);
        let schedule = Schedule {
            steps: (0..3)
                .map(|node| Step {
                    node,
                    inputs: vec![],
                    midi: vec![],
                })
                .collect(),
            ..Default::default()
        };
        let mut state = offline_state(
            schedule,
            vec![
                (
                    0,
                    NodeSlot::new(
                        Box::new(Capture(before.clone(), Transport::default())),
                        0,
                        0,
                    ),
                ),
                (1, NodeSlot::new(Box::new(Source(clock.clone())), 0, 0)),
                (2, NodeSlot::new(Box::new(wrapped), 0, 1)),
            ],
        );
        for playback in [
            PlaybackState::Playing,
            PlaybackState::Paused,
            PlaybackState::Stopped,
        ] {
            *clock.lock().unwrap() = Transport {
                state: playback,
                position_seconds: if playback == PlaybackState::Stopped {
                    0.0
                } else {
                    3.0
                },
                seconds_per_frame: 1.0 / 48_000.0,
            };
            state.run_block(64);
        }
        let seen = before.lock().unwrap();
        assert_eq!(*seen, *after.lock().unwrap());
        assert_eq!(
            seen.iter()
                .map(|t| (t.state, t.position_seconds))
                .collect::<Vec<_>>(),
            [
                (PlaybackState::Playing, 3.0),
                (PlaybackState::Paused, 3.0),
                (PlaybackState::Stopped, 0.0),
            ]
        );
        drop(seen);
        state.slots[1] = None;
        state.run_block(64);
        assert_eq!(after.lock().unwrap().last(), Some(&Transport::default()));
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ShutdownEvent {
        ProcessStarted,
        ProcessFinished,
        StopStarted,
        StopFinished,
        Dropped,
    }

    type ShutdownEvents = Arc<std::sync::Mutex<Vec<(ShutdownEvent, std::thread::ThreadId)>>>;

    struct ShutdownProbe {
        events: ShutdownEvents,
        started: Option<crossbeam_channel::Sender<()>>,
        process_gate: Option<crossbeam_channel::Receiver<()>>,
        stop_gate: Option<crossbeam_channel::Receiver<()>>,
        ran: bool,
    }

    impl ShutdownProbe {
        fn new(
            process_gate: Option<crossbeam_channel::Receiver<()>>,
            stop_gate: Option<crossbeam_channel::Receiver<()>>,
        ) -> (Self, ShutdownEvents, crossbeam_channel::Receiver<()>) {
            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (tx, rx) = crossbeam_channel::bounded(1);
            (
                Self {
                    events: events.clone(),
                    started: Some(tx),
                    process_gate,
                    stop_gate,
                    ran: false,
                },
                events,
                rx,
            )
        }

        fn event(&self, event: ShutdownEvent) {
            self.events
                .lock()
                .unwrap()
                .push((event, std::thread::current().id()));
        }
    }

    impl Processor for ShutdownProbe {
        fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {
            if !std::mem::replace(&mut self.ran, true) {
                self.event(ShutdownEvent::ProcessStarted);
                let _ = self.started.take().unwrap().send(());
                if let Some(gate) = self.process_gate.take() {
                    let _ = gate.recv();
                }
                self.event(ShutdownEvent::ProcessFinished);
            }
        }

        fn deactivate(&mut self) {
            self.event(ShutdownEvent::StopStarted);
            if let Some(gate) = self.stop_gate.take() {
                let _ = gate.recv();
            }
            self.event(ShutdownEvent::StopFinished);
        }
    }

    impl Drop for ShutdownProbe {
        fn drop(&mut self) {
            self.event(ShutdownEvent::Dropped);
        }
    }

    fn install_shutdown_probe(engine: &mut Engine, probe: ShutdownProbe) {
        engine.send(Command::AddNode(0, NodeSlot::new(Box::new(probe), 0, 0)));
        engine.send(Command::SetSchedule(Box::new(Schedule {
            steps: vec![Step {
                node: 0,
                inputs: vec![],
                midi: vec![],
            }],
            ..Default::default()
        })));
    }

    fn assert_shutdown_events(events: &ShutdownEvents) {
        let events = events.lock().unwrap();
        let stages: Vec<_> = events.iter().map(|&(stage, _)| stage).collect();
        assert_eq!(
            stages,
            [
                ShutdownEvent::ProcessStarted,
                ShutdownEvent::ProcessFinished,
                ShutdownEvent::StopStarted,
                ShutdownEvent::StopFinished,
                ShutdownEvent::Dropped,
            ]
        );
        let audio_thread = events[0].1;
        assert_ne!(audio_thread, std::thread::current().id());
        assert!(
            events[..4]
                .iter()
                .all(|&(_, thread)| thread == audio_thread)
        );
        assert_eq!(
            events[4].1,
            std::thread::current().id(),
            "processor refs retire on the control thread"
        );
    }

    #[test]
    fn shutdown_waits_for_inflight_dsp_and_retires_full_command_backlog() {
        let mut engine = Engine::start_null(48_000, 64).unwrap();
        let (release, gate) = crossbeam_channel::bounded(1);
        let (probe, events, started) = ShutdownProbe::new(Some(gate), None);
        install_shutdown_probe(&mut engine, probe);
        started.recv_timeout(Duration::from_secs(5)).unwrap();

        let (queued, queued_events, _) = ShutdownProbe::new(None, None);
        engine.send(Command::AddNode(1, NodeSlot::new(Box::new(queued), 0, 0)));
        // Shutdown must not depend on inserting a command or on applying this backlog.
        while !engine.cmd_tx.is_full() {
            assert!(
                engine
                    .cmd_tx
                    .push(Command::SetSchedule(Box::default()))
                    .is_ok()
            );
        }
        let (pending, pending_events, _) = ShutdownProbe::new(None, None);
        engine.send(Command::AddNode(2, NodeSlot::new(Box::new(pending), 0, 0)));
        assert_eq!(engine.pending.len(), 1);

        let stop = engine.shutdown.stop.clone();
        let observed = events.clone();
        let release_thread = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while !stop.load(Ordering::Acquire) {
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            assert_eq!(
                observed.lock().unwrap().len(),
                1,
                "neither stop nor drop can race the active call"
            );
            release.send(()).unwrap();
        });
        assert!(engine.shutdown());
        release_thread.join().unwrap();
        assert_shutdown_events(&events);
        for events in [queued_events, pending_events] {
            assert_eq!(
                *events.lock().unwrap(),
                [(ShutdownEvent::Dropped, std::thread::current().id())],
                "unstarted queued processors are retired without calling stop on the UI thread"
            );
        }
        assert!(engine.shutdown(), "shutdown is idempotent");
        drop(engine);
        assert_shutdown_events(&events);
    }

    #[test]
    fn shutdown_timeout_preserves_an_inflight_processor_until_late_completion() {
        let mut engine = Engine::start_null(48_000, 64).unwrap();
        let (release, gate) = crossbeam_channel::bounded(1);
        let (probe, events, started) = ShutdownProbe::new(Some(gate), None);
        install_shutdown_probe(&mut engine, probe);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = Instant::now();
        assert!(!engine.shutdown_with_timeout(Duration::from_millis(20)));
        assert!(
            before.elapsed() < Duration::from_secs(1),
            "a stuck plugin cannot force an unbounded join"
        );
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(
            !engine.shutdown(),
            "a timeout remains a failure requiring owner retention"
        );
        // The application retains instance owners after false. Keep the return endpoint
        // here so the synthetic probe can be reclaimed after the deliberate late finish.
        let returned = engine.shutdown.returned.clone();
        drop(engine);
        assert_eq!(events.lock().unwrap().len(), 1);
        release.send(()).unwrap();
        let state = returned.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(events.lock().unwrap().len(), 4);
        state.retire();
        assert_shutdown_events(&events);
    }

    #[test]
    fn shutdown_timeout_also_bounds_a_stuck_stop_processing() {
        let mut engine = Engine::start_null(48_000, 64).unwrap();
        let (release, gate) = crossbeam_channel::bounded(1);
        let (probe, events, started) = ShutdownProbe::new(None, Some(gate));
        install_shutdown_probe(&mut engine, probe);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!engine.shutdown_with_timeout(Duration::from_millis(20)));
        let until = Instant::now() + Duration::from_secs(5);
        while !events
            .lock()
            .unwrap()
            .iter()
            .any(|&(stage, _)| stage == ShutdownEvent::StopStarted)
        {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let stages: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .map(|&(stage, _)| stage)
            .collect();
        assert_eq!(
            stages,
            [
                ShutdownEvent::ProcessStarted,
                ShutdownEvent::ProcessFinished,
                ShutdownEvent::StopStarted
            ]
        );
        let returned = engine.shutdown.returned.clone();
        drop(engine);
        release.send(()).unwrap();
        returned
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .retire();
        assert_shutdown_events(&events);
    }

    #[test]
    fn shutdown_normal_and_implicit_null_exits_stress_processor_thread_affinity() {
        for iteration in 0..32 {
            let mut engine = Engine::start_null(48_000, 64).unwrap();
            let (probe, events, started) = ShutdownProbe::new(None, None);
            install_shutdown_probe(&mut engine, probe);
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            if iteration % 2 == 0 {
                assert!(engine.shutdown());
            }
            drop(engine);
            assert_shutdown_events(&events);
        }
        // Also close before the worker necessarily takes the startup state. Queued
        // wrappers may either be installed/deactivated or remain entirely unstarted.
        for _ in 0..32 {
            let mut engine = Engine::start_null(48_000, 64).unwrap();
            let (probe, events, _) = ShutdownProbe::new(None, None);
            engine.send(Command::AddNode(0, NodeSlot::new(Box::new(probe), 0, 0)));
            assert!(engine.shutdown());
            drop(engine);
            let events = events.lock().unwrap();
            assert_eq!(
                events.last(),
                Some(&(ShutdownEvent::Dropped, std::thread::current().id()))
            );
            assert!(
                !events
                    .iter()
                    .any(|&(stage, _)| stage == ShutdownEvent::ProcessStarted)
            );
            assert!(
                events
                    .iter()
                    .filter(|&&(stage, _)| stage == ShutdownEvent::StopStarted)
                    .count()
                    <= 1
            );
        }
    }

    fn timing_schedule(nodes: &[timing::NodeSpec], edges: &[timing::EdgeKey]) -> Schedule {
        let limits = timing::TimingLimits::default();
        let analysis = timing::analyze(nodes, edges, true, limits).unwrap();
        let mut schedule = Schedule::default();
        for &id in &analysis.order {
            let node = nodes.iter().find(|node| node.id == id).unwrap();
            let mut step = Step {
                node: id,
                inputs: vec![Vec::new(); node.audio_inputs],
                midi: Vec::new(),
            };
            for edge in edges.iter().filter(|e| e.to == id) {
                match edge.kind {
                    timing::EdgeKind::Audio { output, input } => {
                        step.inputs[input].push((edge.from, output))
                    }
                    timing::EdgeKind::Midi => step.midi.push(edge.from),
                }
            }
            schedule.steps.push(step);
            if node.sink {
                schedule.sinks.push(id);
            }
        }
        schedule.timing =
            Some(compensation::TimingPlan::prepare(&analysis, limits, &schedule.steps).unwrap());
        schedule
    }

    struct Impulse;
    impl Processor for Impulse {
        fn process(&mut self, n: usize, _ins: &mut [StereoBuf], outs: &mut [StereoBuf]) {
            outs[0].clear(n);
        }
        fn process_timed(
            &mut self,
            first: u64,
            n: usize,
            _ins: &mut [StereoBuf],
            outs: &mut [StereoBuf],
            _: &[MidiEvent],
            midi: &mut MidiBuf,
        ) {
            outs[0].clear(n);
            if first == 0 {
                outs[0].l[0] = 1.0;
                outs[0].r[0] = -1.0;
                midi.push(MidiEvent {
                    time: 0,
                    data: [0x90, 60, 1],
                });
            }
        }
    }

    struct FixedDelay(compensation::AudioDelay);
    impl Processor for FixedDelay {
        fn process(&mut self, n: usize, ins: &mut [StereoBuf], outs: &mut [StereoBuf]) {
            self.0.write(ins.first(), &mut outs[0], n);
        }
    }

    #[test]
    fn plugin_handoff_keeps_slot_history_and_final_output_warm() {
        struct Constant;
        impl Processor for Constant {
            fn process(&mut self, n: usize, _: &mut [StereoBuf], out: &mut [StereoBuf]) {
                out[0].l[..n].fill(0.25);
                out[0].r[..n].fill(-0.5);
            }
        }
        struct Plugin {
            state: Arc<std::sync::Mutex<compensation::AudioDelay>>,
            flushes: Arc<std::sync::atomic::AtomicUsize>,
            stops: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Processor for Plugin {
            fn process(&mut self, n: usize, input: &mut [StereoBuf], out: &mut [StereoBuf]) {
                self.state.lock().unwrap().write(input.first(), &mut out[0], n);
            }
            fn flush_params(&mut self) {
                self.flushes.fetch_add(1, Ordering::Relaxed);
            }
            fn deactivate(&mut self) {
                self.stops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let delay = Arc::new(std::sync::Mutex::new(compensation::AudioDelay::new(128).unwrap()));
        let flushes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let processor = || Box::new(Plugin {
            state: delay.clone(),
            flushes: flushes.clone(),
            stops: stops.clone(),
        }) as Box<dyn Processor>;
        let plugin = nodes::PluginProc::with_latency(processor(), 128, 0, false).unwrap();
        let schedule = timing_schedule(
            &[spec(0, 0, false), spec(1, 128, false), spec(2, 0, true)],
            &[edge(0, 1), edge(1, 2)],
        );
        let mut state = offline_state(schedule, vec![
            (0, NodeSlot::new(Box::new(Constant), 0, 1)),
            (1, NodeSlot::new(Box::new(plugin), 1, 1)),
            (2, NodeSlot::new(Box::new(nodes::Output::new(0.0, Arc::default())), 1, 0)),
        ]);
        let (mut commands, command_rx) = rtrb::RingBuffer::new(16);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::new(16);
        state.cmd_rx = command_rx;
        state.garbage_tx = garbage_tx;
        let epoch = state.schedule.epoch();
        let slot = state.slots[1].as_ref().unwrap().as_ref() as *const NodeSlot;
        let input_buffer = state.slots[1].as_ref().unwrap().ins[0].l.as_ptr();
        state.render(&mut vec![0.0; 2048 * 2], 2);
        commands.push(Command::ConfigureBypass(1, Box::new(
            nodes::LatencyBypass::new(128, 0, false).unwrap(),
        ))).ok().unwrap();
        commands.push(Command::SuspendPlugin(1)).ok().unwrap();
        for frames in [64, 127, 512] {
            let mut output = vec![0.0; frames * 2];
            state.render(&mut output, 2);
            assert!(output.as_chunks::<2>().0.iter().all(|frame| *frame == [0.25, -0.5]));
        }
        assert_eq!(flushes.load(Ordering::Relaxed), 1);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(matches!(garbage.pop().unwrap(), Garbage::Bypass(_)));
        assert!(matches!(garbage.pop().unwrap(), Garbage::PluginSuspended(1, Some(_))));
        commands.push(Command::ResumePlugin(1, processor())).ok().unwrap();
        let mut output = vec![0.0; 512 * 2];
        state.render(&mut output, 2);
        assert!(output.as_chunks::<2>().0.iter().all(|frame| *frame == [0.25, -0.5]));
        assert_eq!(state.slots[1].as_ref().unwrap().as_ref() as *const NodeSlot, slot);
        assert_eq!(state.slots[1].as_ref().unwrap().ins[0].l.as_ptr(), input_buffer);
        assert_eq!(state.schedule.epoch(), epoch);
        assert!(garbage.pop().is_err());
    }

    fn spec(id: u32, latency: u64, sink: bool) -> timing::NodeSpec {
        timing::NodeSpec {
            id,
            audio_inputs: if id == 0 { 0 } else { 1 },
            audio_outputs: if sink { 0 } else { 1 },
            midi_input: id != 0,
            midi_output: true,
            sink,
            midi_sink: false,
            timing: timing::NodeTiming {
                algorithm_latency_frames: latency,
                ..Default::default()
            },
        }
    }
    fn edge(from: u32, to: u32) -> timing::EdgeKey {
        timing::EdgeKey {
            from,
            to,
            kind: timing::EdgeKind::Audio {
                output: 0,
                input: 0,
            },
            occurrence: 0,
        }
    }

    #[test]
    fn offline_diamond_audio_and_sink_alignment_across_large_callback() {
        let nodes = [
            spec(0, 0, false),
            spec(1, 128, false),
            spec(2, 512, false),
            spec(3, 0, true),
            spec(4, 0, true),
        ];
        let edges = [edge(0, 1), edge(0, 2), edge(1, 3), edge(2, 3), edge(1, 4)];
        let schedule = timing_schedule(&nodes, &edges);
        let output = || NodeSlot::new(Box::new(nodes::Output::new(0.0, Arc::default())), 1, 0);
        let mut state = offline_state(
            schedule,
            vec![
                (0, NodeSlot::new(Box::new(Impulse), 0, 1)),
                (
                    1,
                    NodeSlot::new(
                        Box::new(FixedDelay(compensation::AudioDelay::new(128).unwrap())),
                        1,
                        1,
                    ),
                ),
                (
                    2,
                    NodeSlot::new(
                        Box::new(FixedDelay(compensation::AudioDelay::new(512).unwrap())),
                        1,
                        1,
                    ),
                ),
                (3, output()),
                (4, output()),
            ],
        );
        let mut got = Vec::new();
        for n in [1, 127, 256, 511, 512, 2049] {
            let mut out = vec![99.0; n * 2];
            state.render(&mut out, 2);
            got.extend(out.as_chunks::<2>().0.iter().map(|frame| frame[0]));
        }
        assert_eq!(got.iter().position(|v| *v != 0.0), Some(512));
        assert_eq!(got[512], 3.0);
        assert_eq!(got.iter().sum::<f32>(), 3.0);
        assert_eq!(state.sample_clock, got.len() as u64);
    }

    #[test]
    fn offline_midi_aligns_with_delayed_audio_without_clamping() {
        struct Capture(Arc<std::sync::Mutex<Vec<(u64, u8)>>>);
        impl Processor for Capture {
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {}
            fn process_timed(
                &mut self,
                first: u64,
                _: usize,
                _: &mut [StereoBuf],
                _: &mut [StereoBuf],
                input: &[MidiEvent],
                _: &mut MidiBuf,
            ) {
                self.0
                    .lock()
                    .unwrap()
                    .extend(input.iter().map(|e| (first + e.time as u64, e.data[1])));
            }
        }
        let nodes = [spec(0, 0, false), spec(1, 512, false), spec(2, 0, true)];
        let edges = [
            edge(0, 1),
            edge(1, 2),
            timing::EdgeKey {
                from: 0,
                to: 2,
                kind: timing::EdgeKind::Midi,
                occurrence: 0,
            },
        ];
        let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = offline_state(
            timing_schedule(&nodes, &edges),
            vec![
                (0, NodeSlot::new(Box::new(Impulse), 0, 1)),
                (
                    1,
                    NodeSlot::new(
                        Box::new(FixedDelay(compensation::AudioDelay::new(512).unwrap())),
                        1,
                        1,
                    ),
                ),
                (2, NodeSlot::new(Box::new(Capture(capture.clone())), 1, 0)),
            ],
        );
        for n in [127, 256, 128, 1, 1, 127] {
            state.render(&mut vec![0.0; n * 2], 2);
        }
        assert_eq!(*capture.lock().unwrap(), [(512, 60)]);
    }

    #[test]
    fn hardware_midi_output_is_delayed_to_the_device_reference() {
        struct Seen(Arc<std::sync::Mutex<Vec<(u64, u8)>>>);
        impl Processor for Seen {
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {}
            fn process_timed(
                &mut self,
                first: u64,
                _: usize,
                _: &mut [StereoBuf],
                _: &mut [StereoBuf],
                input: &[MidiEvent],
                _: &mut MidiBuf,
            ) {
                self.0
                    .lock()
                    .unwrap()
                    .extend(input.iter().map(|e| (first + e.time as u64, e.data[1])));
            }
        }
        // The impulse emits at block 0. Node 2 is hardware-bound (no engine edge leaves it)
        // and must emit aligned to the device reference set by the sink, not immediately.
        let mut device = spec(2, 0, false);
        device.midi_sink = true;
        device.midi_output = false;
        let nodes = [spec(0, 0, false), spec(1, 1024, true), device];
        let edges = [
            edge(0, 1),
            timing::EdgeKey {
                from: 0,
                to: 2,
                kind: timing::EdgeKind::Midi,
                occurrence: 0,
            },
        ];
        let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = offline_state(
            timing_schedule(&nodes, &edges),
            vec![
                (0, NodeSlot::new(Box::new(Impulse), 0, 1)),
                (1, NodeSlot::new(Box::new(nodes::Output::new(0.0, Arc::default())), 1, 0)),
                (2, NodeSlot::new(Box::new(Seen(capture.clone())), 1, 0)),
            ],
        );
        for n in [512, 512, 1] {
            state.render(&mut vec![0.0; n * 2], 2);
        }
        assert_eq!(*capture.lock().unwrap(), [(1024, 60)]);
    }

    #[test]
    fn unchanged_plan_preserves_edge_and_sink_impulse_history() {
        let nodes = [
            spec(0, 0, false),
            spec(1, 512, false),
            spec(2, 0, true),
            spec(3, 0, true),
        ];
        let edges = [edge(0, 1), edge(0, 2), edge(1, 2), edge(0, 3)];
        let first = timing_schedule(&nodes, &edges);
        let mut second = timing_schedule(&nodes, &edges);
        let output = || NodeSlot::new(Box::new(nodes::Output::new(0.0, Arc::default())), 1, 0);
        let mut state = offline_state(
            first,
            vec![
                (0, NodeSlot::new(Box::new(Impulse), 0, 1)),
                (
                    1,
                    NodeSlot::new(
                        Box::new(FixedDelay(compensation::AudioDelay::new(512).unwrap())),
                        1,
                        1,
                    ),
                ),
                (2, output()),
                (3, output()),
            ],
        );
        state.render(&mut vec![0.0; 127 * 2], 2);
        second
            .timing
            .as_mut()
            .unwrap()
            .inherit(state.schedule.timing.as_mut().unwrap());
        *state.schedule = second;
        let mut out = vec![0.0; 1024 * 2];
        state.render(&mut out, 2);
        assert_eq!(
            out[(512 - 127) * 2],
            3.0,
            "edge history + fixed fixture + sink history must all survive"
        );
        assert_eq!(out.iter().step_by(2).sum::<f32>(), 3.0);
    }

    #[test]
    fn replacement_invalidates_history_inheritance() {
        let nodes = [spec(0, 0, false), spec(1, 512, false), spec(2, 0, true)];
        let edges = [edge(0, 1), edge(0, 2), edge(1, 2)];
        let first = timing_schedule(&nodes, &edges);
        let mut second = timing_schedule(&nodes, &edges);
        let mut state = offline_state(
            first,
            vec![
                (0, NodeSlot::new(Box::new(Impulse), 0, 1)),
                (
                    1,
                    NodeSlot::new(
                        Box::new(FixedDelay(compensation::AudioDelay::new(512).unwrap())),
                        1,
                        1,
                    ),
                ),
                (
                    2,
                    NodeSlot::new(Box::new(nodes::Output::new(0.0, Arc::default())), 1, 0),
                ),
            ],
        );
        state.render(&mut vec![0.0; 127 * 2], 2);
        state.schedule.timing.as_mut().unwrap().invalidate_node(0);
        second
            .timing
            .as_mut()
            .unwrap()
            .inherit(state.schedule.timing.as_mut().unwrap());
        *state.schedule = second;
        let mut out = vec![0.0; 1024 * 2];
        state.render(&mut out, 2);
        assert_eq!(
            out[(512 - 127) * 2],
            1.0,
            "only the fixture's own history remains, no recycled source edge"
        );
    }

    #[test]
    fn retired_midi_routes_cleanup_notes_before_new_events() {
        struct Notes;
        impl Processor for Notes {
            fn process(&mut self, n: usize, _: &mut [StereoBuf], out: &mut [StereoBuf]) {
                out[0].clear(n);
            }
            fn process_timed(
                &mut self,
                first: u64,
                n: usize,
                input: &mut [StereoBuf],
                out: &mut [StereoBuf],
                _: &[MidiEvent],
                midi: &mut MidiBuf,
            ) {
                self.process(n, input, out);
                if first == 0 {
                    midi.push(MidiEvent {
                        time: 0,
                        data: [0x90, 60, 1],
                    });
                    midi.push(MidiEvent {
                        time: 100,
                        data: [0x80, 60, 0],
                    });
                } else {
                    midi.push(MidiEvent {
                        time: 0,
                        data: [0x90, 62, 1],
                    });
                }
            }
        }
        type RecordedMidi = Arc<std::sync::Mutex<Vec<(u64, [u8; 3])>>>;
        struct Capture(RecordedMidi);
        impl Processor for Capture {
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {}
            fn process_timed(
                &mut self,
                first: u64,
                _: usize,
                _: &mut [StereoBuf],
                _: &mut [StereoBuf],
                midi: &[MidiEvent],
                _: &mut MidiBuf,
            ) {
                self.0.lock().unwrap().extend(
                    midi.iter()
                        .map(|event| (first + event.time as u64, event.data)),
                );
            }
        }
        // First note-on is delivered at 128; its note-off is still queued for 228.
        for transition in ["removed", "resized", "replaced"] {
            let mut nodes = [spec(0, 0, false), spec(1, 128, false), spec(2, 0, false)];
            let midi = timing::EdgeKey {
                from: 0,
                to: 2,
                kind: timing::EdgeKind::Midi,
                occurrence: 0,
            };
            let original = [edge(0, 1), edge(1, 2), midi];
            let capture = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut state = offline_state(
                timing_schedule(&nodes, &original),
                vec![
                    (0, NodeSlot::new(Box::new(Notes), 0, 1)),
                    (
                        1,
                        NodeSlot::new(
                            Box::new(FixedDelay(compensation::AudioDelay::new(128).unwrap())),
                            1,
                            1,
                        ),
                    ),
                    (2, NodeSlot::new(Box::new(Capture(capture.clone())), 1, 1)),
                ],
            );
            state.render(&mut vec![0.0; 200 * 2], 2);
            assert_eq!(*capture.lock().unwrap(), [(128, [0x90, 60, 1])]);
            let mut edges = original.to_vec();
            match transition {
                "removed" => {
                    edges.pop();
                }
                "resized" => nodes[1].timing.algorithm_latency_frames = 0,
                "replaced" => state.schedule.timing.as_mut().unwrap().invalidate_node(0),
                _ => unreachable!(),
            }
            let mut next = timing_schedule(&nodes, &edges);
            next.timing
                .as_mut()
                .unwrap()
                .inherit(state.schedule.timing.as_mut().unwrap());
            // Multiple queued schedules before processing must not swallow pending cleanup.
            let mut final_plan = timing_schedule(&nodes, &edges);
            final_plan
                .timing
                .as_mut()
                .unwrap()
                .inherit(next.timing.as_mut().unwrap());
            *state.schedule = final_plan;
            state.render(&mut vec![0.0; 64 * 2], 2);
            let got = capture.lock().unwrap();
            assert_eq!(&got[1..49], &(0..16).flat_map(|channel| [64, 123, 120].map(move |cc| (200, [0xb0 | channel, cc, 0]))).collect::<Vec<_>>(), "{transition}");
            if transition == "resized" {
                assert_eq!(
                    got[49],
                    (200, [0x90, 62, 1]),
                    "cleanup must precede a new note at the same timestamp"
                );
            }
            assert!(
                !got.iter().any(|(_, data)| data[0] == 0x80),
                "old queued note-off was retired, not replayed"
            );
        }
    }

    #[test]
    fn plan_epoch_is_acknowledged_at_command_application() {
        let schedule = timing_schedule(&[spec(0, 0, false)], &[]);
        let epoch = schedule.epoch();
        let (mut tx, cmd_rx) = rtrb::RingBuffer::new(4);
        let (garbage_tx, mut rx) = rtrb::RingBuffer::new(4);
        let mut state = offline_state(Schedule::default(), vec![]);
        state.cmd_rx = cmd_rx;
        state.garbage_tx = garbage_tx;
        assert!(tx.push(Command::SetSchedule(Box::new(schedule))).is_ok());
        assert_eq!(state.status.applied_epoch.load(Ordering::Acquire), 0);
        state.render(&mut [0.0; 2], 2);
        assert_eq!(state.status.applied_epoch.load(Ordering::Acquire), epoch);
        assert!(matches!(rx.pop(), Ok(Garbage::Schedule(_))));
    }

    #[test]
    fn midi_buffers_merge_in_time_order() {
        let ev = |t, n| MidiEvent::new(t, &[0x90, n, 100]).unwrap();
        let mut b = MidiBuf::new();
        b.merge(&[ev(5, 1), ev(9, 2)], 64);
        b.merge(&[ev(5, 3), ev(0, 4), ev(500, 5)], 64);
        let order: Vec<(u32, u8)> = b.events().iter().map(|e| (e.time, e.data[1])).collect();
        assert_eq!(order, [(0, 4), (5, 1), (5, 3), (9, 2), (63, 5)]);
    }

    #[test]
    fn midi_flows_along_midi_wires() {
        use nodes::{MidiBridge, midi_queue};
        let (mut tx, cmd_rx) = rtrb::RingBuffer::new(64);
        let (garbage_tx, _garbage_rx) = rtrb::RingBuffer::new(64);
        let mut st = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: Arc::default(),
        };
        let (src_tx, src_rx) = midi_queue();
        let (sink_tx, sink_rx) = midi_queue();
        let push = |tx: &mut rtrb::Producer<Command>, c| assert!(tx.push(c).is_ok());
        push(&mut tx, Command::AddNode(0, NodeSlot::new(Box::new(MidiBridge { from: Some(src_rx), ..Default::default() }), 0, 0)));
        push(&mut tx, Command::AddNode(1, NodeSlot::new(Box::new(MidiBridge { to: Some(sink_tx), ..Default::default() }), 0, 0)));
        push(
            &mut tx,
            Command::SetSchedule(Box::new(Schedule {
                steps: vec![Step { node: 0, inputs: vec![], midi: vec![] }, Step { node: 1, inputs: vec![], midi: vec![0] }],
                sinks: vec![],
                ..Default::default()
            })),
        );
        for n in [60, 64, 67] {
            src_tx.lock().unwrap().push(MidiEvent::new(0, &[0x90, n, 90]).unwrap()).unwrap();
        }
        let mut buf = vec![0.0; 256];
        st.render(&mut buf, 2);
        let got: Vec<u8> = std::iter::from_fn(|| sink_rx.lock().unwrap().pop().ok()).map(|e| e.data[1]).collect();
        assert_eq!(got, [60, 64, 67]);
    }

    #[test]
    fn plays_resampled_file_through_graph() {
        let dir = std::env::temp_dir().join(format!("nodeplayer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("sine.wav");
        write_sine_wav(&wav, 44_100, 1.0, 0.5);

        let (mut tx, cmd_rx) = rtrb::RingBuffer::new(64);
        let (garbage_tx, mut garbage_rx) = rtrb::RingBuffer::new(64);
        let mut st = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: Arc::default(),
        };
        let player = Player::new(48_000);
        let meter = Arc::new(nodes::Meter::default());
        let push = |tx: &mut rtrb::Producer<Command>, c| assert!(tx.push(c).is_ok());
        push(&mut tx, Command::AddNode(0, NodeSlot::new(Box::new(player.make_source()), 0, 1)));
        push(&mut tx, Command::AddNode(1, NodeSlot::new(Box::new(nodes::Gain::new(-6.0206)), 1, 1)));
        push(&mut tx, Command::AddNode(2, NodeSlot::new(Box::new(nodes::Output::new(0.0, meter.clone())), 1, 0)));
        push(
            &mut tx,
            Command::SetSchedule(Box::new(Schedule {
                steps: vec![
                    Step { node: 0, inputs: vec![], midi: vec![] },
                    Step { node: 1, inputs: vec![vec![(0, 0)]], midi: vec![] },
                    Step { node: 2, inputs: vec![vec![(1, 0)]], midi: vec![] },
                ],
                sinks: vec![2],
                ..Default::default()
            })),
        );
        player.load(&wav);

        // Wait for audio, then measure a steady 0.25 s window.
        let mut out = Vec::new();
        render_until(&mut st, &mut out, |o| o.iter().any(|s| s.abs() > 0.01));
        let start = out.len();
        render_until(&mut st, &mut out, |o| o.len() >= start + 48_000 / 4 * 2);
        let win = &out[start..start + 48_000 / 4 * 2];
        let rms = (win.iter().map(|x| x * x).sum::<f32>() / win.len() as f32).sqrt();
        let expected = 0.25 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((rms - expected).abs() < 0.01, "rms {rms}, expected {expected}");
        assert!(meter.take()[0] > 0.2);

        // Seeking flushes buffered audio and moves the position.
        player.seek(0.9);
        let t0 = Instant::now();
        while player.position() < 0.9 && t0.elapsed() < Duration::from_secs(2) {
            let mut buf = vec![0.0; 1024];
            st.render(&mut buf, 2);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(player.position() >= 0.9, "position {}", player.position());

        // Playback stops by itself at the end of the file.
        let t0 = Instant::now();
        while player.is_playing() && t0.elapsed() < Duration::from_secs(3) {
            let mut buf = vec![0.0; 1024];
            st.render(&mut buf, 2);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!player.is_playing(), "did not stop at end of file");
        let pos = player.position();
        assert!((pos - 1.0).abs() < 0.05, "end position {pos}");

        // A finished file can be started again: by pressing play, by seeking, or by reloading.
        let replay = |st: &mut AudioState, what: &str| {
            let mut out = Vec::new();
            let t0 = Instant::now();
            while !out.iter().any(|s: &f32| s.abs() > 0.01) && t0.elapsed() < Duration::from_secs(2) {
                let mut buf = vec![0.0; 1024];
                st.render(&mut buf, 2);
                out.extend_from_slice(&buf);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(out.iter().any(|s| s.abs() > 0.01), "no audio after {what}");
            let t0 = Instant::now();
            while player.is_playing() && t0.elapsed() < Duration::from_secs(3) {
                let mut buf = vec![0.0; 1024];
                st.render(&mut buf, 2);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(!player.is_playing(), "{what}: did not stop again");
        };
        player.load(&wav);
        replay(&mut st, "reload");
        player.seek(0.0);
        player.set_playing(true);
        replay(&mut st, "seek + play");
        player.set_playing(true);
        replay(&mut st, "play");

        // Removing a node hands it back for freeing off the audio thread.
        push(&mut tx, Command::RemoveNode(1));
        let mut buf = vec![0.0; 1024];
        st.render(&mut buf, 2);
        assert!(matches!(garbage_rx.pop(), Ok(Garbage::Schedule(_))));
        assert!(matches!(garbage_rx.pop(), Ok(Garbage::Node(1, _))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
