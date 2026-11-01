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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const MAX_BLOCK: usize = 1024;
pub const MAX_NODES: usize = 1024;
const CMD_QUEUE_LEN: usize = 1024;

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

/// A DSP node running on the audio thread. All audio ports are stereo; a node has at most one
/// MIDI input and one MIDI output.
pub trait Processor: Send {
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
    /// Swaps the processor of a live node; the old one comes back as [`Garbage::Replaced`].
    ReplaceNode(u32, Box<NodeSlot>),
    RemoveNode(u32),
    SetSchedule(Box<Schedule>),
    SetParam(u32, usize, f32),
    ConfigureBypass(u32, Box<nodes::LatencyBypass>),
}

/// Objects handed back to the UI thread to be freed there.
pub enum Garbage {
    Node(u32, Box<NodeSlot>),
    Replaced(u32, Box<NodeSlot>),
    Schedule(#[allow(dead_code)] Box<Schedule>),
    Bypass(#[allow(dead_code)] Box<nodes::LatencyBypass>),
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
                    if let Some(old) = self.slots[id as usize].replace(slot) {
                        self.throw_away(Garbage::Node(id, old));
                    }
                }
                Command::ReplaceNode(id, slot) => {
                    if let Some(plan) = &mut self.schedule.timing {
                        plan.invalidate_node(id);
                    }
                    if let Some(mut old) = self.slots[id as usize].replace(slot) {
                        let g = old.enter();
                        old.proc.deactivate();
                        drop(g);
                        self.throw_away(Garbage::Replaced(id, old));
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
            }
        }
    }

    fn run_block(&mut self, n: usize) {
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
            let _g = tag.as_deref().map(crate::plugins::guard::enter_borrowed);
            proc.process_timed(self.sample_clock, n, ins, outs, midi_in.events(), midi_out);
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

pub struct Engine {
    _stream: Option<cpal::Stream>,
    null_stop: Option<Arc<AtomicBool>>,
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
        let diagnostic = crate::diagnostics::active().filter(|run| run.metrics.enabled());

        let build = |config: cpal::StreamConfig| {
            let state = state.clone();
            let diagnostic = diagnostic.clone();
            let mut previous_callback: Option<std::time::Instant> = None;
            let mut local: Option<AudioState> = None;
            device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let started = diagnostic.as_ref().map(|_| std::time::Instant::now());
                    // Take ownership of the state on the first callback; no locking afterwards.
                    if local.is_none() {
                        local = state.try_lock().ok().and_then(|mut s| s.take());
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
            null_stop: None,
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
    /// Stop is nonblocking, even if a local plugin hangs; process-level watchdog remains needed.
    pub fn start_null(sample_rate: u32, block_size: usize) -> Result<Self> {
        anyhow::ensure!(sample_rate > 0, "null sample rate must be nonzero");
        anyhow::ensure!(
            (1..=MAX_BLOCK).contains(&block_size),
            "invalid null block size"
        );
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(CMD_QUEUE_LEN);
        let status = Arc::new(EngineStatus::default());
        let mut state = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
            sample_clock: 0,
            status: status.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let mut output = vec![0.0; block_size * 2];
        let diagnostic = crate::diagnostics::active().filter(|run| run.metrics.enabled());
        std::thread::Builder::new()
            .name("nodeplayer-null-audio".into())
            .spawn(move || {
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
                        std::thread::sleep(deadline - now);
                    } else {
                        deadline = now;
                    }
                }
            })?;
        Ok(Self {
            _stream: None,
            null_stop: Some(stop),
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
        if let Some(stop) = &self.null_stop {
            stop.store(true, Ordering::Release);
        }
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

    fn spec(id: u32, latency: u64, sink: bool) -> timing::NodeSpec {
        timing::NodeSpec {
            id,
            audio_inputs: if id == 0 { 0 } else { 1 },
            audio_outputs: if sink { 0 } else { 1 },
            midi_input: id != 0,
            midi_output: true,
            sink,
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
