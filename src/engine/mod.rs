//! Real-time audio engine.
//!
//! The UI thread owns the graph model and compiles it into a [`Schedule`]. Nodes and schedules
//! are moved to the audio thread through a lock-free command ring; whatever the audio thread
//! replaces is sent back through a garbage ring so it is freed off the real-time thread.

pub mod eq;
pub mod nodes;

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

    fn set_param(&mut self, _index: usize, _value: f32) {}

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
}

pub enum Command {
    AddNode(u32, Box<NodeSlot>),
    /// Swaps the processor of a live node; the old one comes back as [`Garbage::Replaced`].
    ReplaceNode(u32, Box<NodeSlot>),
    RemoveNode(u32),
    SetSchedule(Box<Schedule>),
    SetParam(u32, usize, f32),
}

/// Objects handed back to the UI thread to be freed there.
pub enum Garbage {
    Node(u32, Box<NodeSlot>),
    Replaced(u32, Box<NodeSlot>),
    Schedule(#[allow(dead_code)] Box<Schedule>),
}

struct AudioState {
    slots: Vec<Option<Box<NodeSlot>>>,
    schedule: Box<Schedule>,
    cmd_rx: rtrb::Consumer<Command>,
    garbage_tx: rtrb::Producer<Garbage>,
}

impl AudioState {
    fn throw_away(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage_tx.push(g) {
            // Should never happen with a sane queue size; leak rather than free on this thread.
            std::mem::forget(g);
        }
    }

    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.pop() {
            match cmd {
                Command::AddNode(id, slot) => {
                    if let Some(old) = self.slots[id as usize].replace(slot) {
                        self.throw_away(Garbage::Node(id, old));
                    }
                }
                Command::ReplaceNode(id, slot) => {
                    if let Some(mut old) = self.slots[id as usize].replace(slot) {
                        let g = old.enter();
                        old.proc.deactivate();
                        drop(g);
                        self.throw_away(Garbage::Replaced(id, old));
                    }
                }
                Command::RemoveNode(id) => {
                    if let Some(mut old) = self.slots[id as usize].take() {
                        let g = old.enter();
                        old.proc.deactivate();
                        drop(g);
                        self.throw_away(Garbage::Node(id, old));
                    }
                }
                Command::SetSchedule(s) => {
                    let old = std::mem::replace(&mut self.schedule, s);
                    self.throw_away(Garbage::Schedule(old));
                }
                Command::SetParam(id, idx, v) => {
                    if let Some(slot) = &mut self.slots[id as usize] {
                        slot.proc.set_param(idx, v);
                    }
                }
            }
        }
    }

    fn run_block(&mut self, n: usize) {
        for step in &self.schedule.steps {
            let Some(mut slot) = self.slots[step.node as usize].take() else { continue };
            for (port, sources) in slot.ins.iter_mut().zip(&step.inputs) {
                port.clear(n);
                for &(src, out) in sources {
                    if let Some(s) = &self.slots[src as usize] {
                        if let Some(b) = s.outs.get(out as usize) {
                            port.add_from(b, n);
                        }
                    }
                }
            }
            slot.midi_in.clear();
            for &src in &step.midi {
                if let Some(s) = &self.slots[src as usize] {
                    slot.midi_in.merge(s.midi_out.events(), n);
                }
            }
            let NodeSlot { proc, tag, ins, outs, midi_in, midi_out } = &mut *slot;
            midi_out.clear();
            let _g = tag.as_deref().map(crate::plugins::guard::enter_borrowed);
            proc.process_midi(n, ins, outs, midi_in.events(), midi_out);
            self.slots[step.node as usize] = Some(slot);
        }
    }

    fn render(&mut self, data: &mut [f32], channels: usize) {
        self.handle_commands();
        let frames = data.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.run_block(n);
            let out = &mut data[done * channels..(done + n) * channels];
            out.fill(0.0);
            for &sink in &self.schedule.sinks {
                let Some(slot) = &self.slots[sink as usize] else { continue };
                let Some(buf) = slot.ins.first() else { continue };
                for (i, frame) in out.chunks_exact_mut(channels).enumerate() {
                    frame[0] += buf.l[i];
                    if channels > 1 {
                        frame[1] += buf.r[i];
                    }
                }
            }
            done += n;
        }
    }
}

pub struct Engine {
    _stream: cpal::Stream,
    cmd_tx: rtrb::Producer<Command>,
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
        let state = AudioState {
            slots: (0..MAX_NODES).map(|_| None).collect(),
            schedule: Box::default(),
            cmd_rx,
            garbage_tx,
        };
        let state = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));

        let build = |config: cpal::StreamConfig| {
            let state = state.clone();
            let mut local: Option<AudioState> = None;
            device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Take ownership of the state on the first callback; no locking afterwards.
                    if local.is_none() {
                        local = state.try_lock().ok().and_then(|mut s| s.take());
                    }
                    match &mut local {
                        Some(s) => s.render(data, channels),
                        None => data.fill(0.0),
                    }
                },
                |e| log::error!("audio stream error: {e}"),
                None,
            )
        };

        config.buffer_size = cpal::BufferSize::Fixed(512);
        let stream = match build(config.clone()) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("fixed buffer size rejected ({e}), using device default");
                config.buffer_size = cpal::BufferSize::Default;
                build(config)?
            }
        };
        stream.play()?;
        log::info!("audio: {device_name} @ {sample_rate} Hz, {channels} ch");

        Ok(Self { _stream: stream, cmd_tx, garbage_rx, sample_rate, channels, device_name })
    }

    pub fn send(&mut self, cmd: Command) {
        if self.cmd_tx.push(cmd).is_err() {
            log::error!("engine command queue full; command dropped");
        }
    }

    pub fn collect_garbage(&mut self) -> Vec<Garbage> {
        std::iter::from_fn(|| self.garbage_rx.pop().ok()).collect()
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
        let mut st = AudioState { slots: (0..MAX_NODES).map(|_| None).collect(), schedule: Box::default(), cmd_rx, garbage_tx };
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
