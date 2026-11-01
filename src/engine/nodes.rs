//! Built-in DSP nodes.

use std::f32::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use super::{MidiBuf, MidiEvent, Processor, StereoBuf};

pub(crate) fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One-pole smoother so parameter jumps don't click.
#[derive(Clone, Copy)]
pub(super) struct Smoothed {
    current: f32,
    pub(super) target: f32,
}

impl Smoothed {
    pub(super) fn new(v: f32) -> Self {
        Self { current: v, target: v }
    }

    #[inline]
    pub(super) fn next(&mut self) -> f32 {
        self.current += (self.target - self.current) * 0.002;
        self.current
    }
}

// ---------------------------------------------------------------------------

pub struct Gain {
    gain: Smoothed,
}

/// Passes audio on unchanged, for nodes whose work happens elsewhere (the rate shift).
pub struct Through;

impl Processor for Through {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        o.l[..n].copy_from_slice(&i.l[..n]);
        o.r[..n].copy_from_slice(&i.r[..n]);
    }
}

impl Gain {
    pub fn new(db: f32) -> Self {
        Self { gain: Smoothed::new(db_to_gain(db)) }
    }
}

impl Processor for Gain {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        for k in 0..n {
            let g = self.gain.next();
            o.l[k] = i.l[k] * g;
            o.r[k] = i.r[k] * g;
        }
    }

    fn set_param(&mut self, _index: usize, value: f32) {
        self.gain.target = db_to_gain(value);
    }
}

// ---------------------------------------------------------------------------

/// Constant-power stereo balance, `pan` in -1..=1.
pub struct Pan {
    pan: Smoothed,
}

impl Pan {
    pub fn new(pan: f32) -> Self {
        Self { pan: Smoothed::new(pan) }
    }
}

impl Processor for Pan {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        for k in 0..n {
            let a = (self.pan.next() + 1.0) * 0.25 * PI;
            o.l[k] = i.l[k] * a.cos() * std::f32::consts::SQRT_2;
            o.r[k] = i.r[k] * a.sin() * std::f32::consts::SQRT_2;
        }
    }

    fn set_param(&mut self, _index: usize, value: f32) {
        self.pan.target = value.clamp(-1.0, 1.0);
    }
}

// ---------------------------------------------------------------------------

pub struct Delay {
    buf: [Vec<f32>; 2],
    pos: usize,
    sample_rate: f32,
    time: Smoothed,
    feedback: f32,
    mix: f32,
}

impl Delay {
    pub const MAX_SECONDS: f32 = 2.0;
    pub const TIME_MS: usize = 0;
    pub const FEEDBACK: usize = 1;
    pub const MIX: usize = 2;

    pub fn new(sample_rate: f32, time_ms: f32, feedback: f32, mix: f32) -> Self {
        let len = (sample_rate * Self::MAX_SECONDS) as usize + 2;
        Self {
            buf: [vec![0.0; len], vec![0.0; len]],
            pos: 0,
            sample_rate,
            time: Smoothed::new(time_ms * 0.001 * sample_rate),
            feedback,
            mix,
        }
    }
}

impl Processor for Delay {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        let len = self.buf[0].len();
        for k in 0..n {
            let d = self.time.next().clamp(1.0, (len - 2) as f32);
            let read = self.pos as f32 - d + len as f32;
            let (ri, frac) = (read as usize % len, read.fract());
            for (ch, (x, y)) in [(i.l[k], &mut o.l[k]), (i.r[k], &mut o.r[k])].into_iter().enumerate() {
                let b = &mut self.buf[ch];
                let wet = b[ri] * (1.0 - frac) + b[(ri + 1) % len] * frac;
                b[self.pos] = x + wet * self.feedback;
                *y = x * (1.0 - self.mix) + wet * self.mix;
            }
            self.pos = (self.pos + 1) % len;
        }
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            Self::TIME_MS => self.time.target = value * 0.001 * self.sample_rate,
            Self::FEEDBACK => self.feedback = value.clamp(0.0, 0.95),
            Self::MIX => self.mix = value.clamp(0.0, 1.0),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------

pub const MIXER_INPUTS: usize = 4;

pub struct Mixer {
    gains: [Smoothed; MIXER_INPUTS],
}

impl Mixer {
    pub fn new(gains_db: [f32; MIXER_INPUTS]) -> Self {
        Self { gains: gains_db.map(|g| Smoothed::new(db_to_gain(g))) }
    }
}

impl Processor for Mixer {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let o = &mut outputs[0];
        o.clear(n);
        for (input, g) in inputs.iter().zip(&mut self.gains) {
            for k in 0..n {
                let g = g.next();
                o.l[k] += input.l[k] * g;
                o.r[k] += input.r[k] * g;
            }
        }
    }

    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(g) = self.gains.get_mut(index) {
            g.target = db_to_gain(value);
        }
    }
}

// ---------------------------------------------------------------------------

/// Peak levels shared with the UI, stored as f32 bits.
#[derive(Default)]
pub struct Meter([AtomicU32; 2]);

impl Meter {
    pub fn take(&self) -> [f32; 2] {
        [0, 1].map(|c| f32::from_bits(self.0[c].swap(0, Ordering::Relaxed)))
    }

    fn feed(&self, ch: usize, peak: f32) {
        let _ = self.0[ch].try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            (peak > f32::from_bits(old)).then_some(peak.to_bits())
        });
    }
}

/// Sink node: applies master volume and meters. The engine reads its input buffer directly.
pub struct Output {
    volume: Smoothed,
    meter: Arc<Meter>,
}

impl Output {
    pub fn new(volume_db: f32, meter: Arc<Meter>) -> Self {
        Self { volume: Smoothed::new(db_to_gain(volume_db)), meter }
    }
}

impl Processor for Output {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], _outputs: &mut [StereoBuf]) {
        // The engine mixes `inputs[0]` to the device after this, so scale it in place.
        let i = &mut inputs[0];
        let mut peak = [0f32; 2];
        for k in 0..n {
            let g = self.volume.next();
            i.l[k] *= g;
            i.r[k] *= g;
            peak[0] = peak[0].max(i.l[k].abs());
            peak[1] = peak[1].max(i.r[k].abs());
        }
        self.meter.feed(0, peak[0]);
        self.meter.feed(1, peak[1]);
    }

    fn set_param(&mut self, _index: usize, value: f32) {
        self.volume.target = db_to_gain(value);
    }
}

// ---------------------------------------------------------------------------

/// Settings of [`KazuLimiter`], mirroring the sliders of the original JSFX.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LimiterParams {
    pub threshold_db: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub output_db: f32,
    pub bypass: bool,
    /// Graph points per second / 10.
    pub graph_speed: f32,
}

impl Default for LimiterParams {
    fn default() -> Self {
        Self { threshold_db: -6.0, release_ms: 50.0, lookahead_ms: 5.0, output_db: 0.0, bypass: false, graph_speed: 10.0 }
    }
}

impl LimiterParams {
    pub const THRESHOLD: usize = 0;
    pub const RELEASE: usize = 1;
    pub const LOOKAHEAD: usize = 2;
    pub const OUTPUT: usize = 3;
    pub const BYPASS: usize = 4;
    pub const SPEED: usize = 5;
}

/// Gain the limiter applied, shared with the UI: the lowest gain since the last read and a
/// history of the lowest gain per graph step.
pub struct LimiterMeter {
    gain: AtomicU32,
    history: Box<[AtomicU32]>,
    pos: AtomicUsize,
}

impl Default for LimiterMeter {
    fn default() -> Self {
        let one = || AtomicU32::new(1f32.to_bits());
        Self { gain: one(), history: (0..Self::HISTORY).map(|_| one()).collect(), pos: AtomicUsize::new(0) }
    }
}

impl LimiterMeter {
    pub const HISTORY: usize = 250;

    pub fn take_gain(&self) -> f32 {
        f32::from_bits(self.gain.swap(1f32.to_bits(), Ordering::Relaxed))
    }

    /// Gain history, oldest first.
    pub fn history(&self) -> Vec<f32> {
        let pos = self.pos.load(Ordering::Acquire);
        (0..Self::HISTORY)
            .map(|k| f32::from_bits(self.history[(pos + k) % Self::HISTORY].load(Ordering::Relaxed)))
            .collect()
    }

    fn feed(&self, gain: f32) {
        let _ = self.gain.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            (gain < f32::from_bits(old)).then_some(gain.to_bits())
        });
    }

    fn push(&self, gain: f32) {
        let pos = self.pos.load(Ordering::Relaxed);
        self.history[pos].store(gain.to_bits(), Ordering::Relaxed);
        self.pos.store((pos + 1) % Self::HISTORY, Ordering::Release);
    }

    fn clear(&self) {
        for h in self.history.iter() {
            h.store(1f32.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Port of the "Kazu Limiter" JSFX: per-channel peak envelope with instant attack and an
/// exponential release, stereo-linked by taking the lower gain. "Lookahead" is the decay time
/// of the peak envelope (the signal isn't delayed).
pub struct KazuLimiter {
    sample_rate: f32,
    p: LimiterParams,
    threshold: f32,
    release_coef: f32,
    lookahead_coef: f32,
    output_gain: f32,
    refresh: u32,
    envelope: [f32; 2],
    gain: [f32; 2],
    count: u32,
    graph_min: f32,
    meter: Arc<LimiterMeter>,
}

impl KazuLimiter {
    pub fn new(sample_rate: f32, p: LimiterParams, meter: Arc<LimiterMeter>) -> Self {
        let mut l = Self {
            sample_rate,
            p,
            threshold: 1.0,
            release_coef: 0.0,
            lookahead_coef: 0.0,
            output_gain: 1.0,
            refresh: 1,
            envelope: [0.0; 2],
            gain: [1.0; 2],
            count: 0,
            graph_min: 1.0,
            meter,
        };
        l.update();
        l
    }

    fn update(&mut self) {
        let sr = self.sample_rate;
        let threshold = db_to_gain(self.p.threshold_db);
        self.release_coef = (-1.0 / (self.p.release_ms / 1000.0 * sr)).exp();
        self.lookahead_coef = (-1.0 / (self.p.lookahead_ms / 1000.0 * sr)).exp();
        self.output_gain = db_to_gain(self.p.output_db);
        self.refresh = ((sr / (self.p.graph_speed * 10.0)) as u32).max(1);
        // Like the original, a threshold change starts the detector (and graph) afresh.
        if threshold != self.threshold {
            self.envelope = [0.0; 2];
            self.gain = [1.0; 2];
            self.meter.clear();
        }
        self.threshold = threshold;
    }
}

impl Processor for KazuLimiter {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        if self.p.bypass {
            for k in 0..n {
                o.l[k] = i.l[k] * self.output_gain;
                o.r[k] = i.r[k] * self.output_gain;
            }
            return;
        }
        let mut block_min = 1f32;
        for k in 0..n {
            let x = [i.l[k], i.r[k]];
            for (ch, sample) in x.iter().enumerate() {
                let env = &mut self.envelope[ch];
                *env = (*env * self.lookahead_coef).max(sample.abs());
                let target = if *env <= self.threshold {
                    1.0
                } else {
                    self.threshold / *env
                };
                let g = &mut self.gain[ch];
                *g = if target < *g { target } else { self.release_coef * *g + (1.0 - self.release_coef) * target };
            }
            let gain = self.gain[0].min(self.gain[1]);
            o.l[k] = x[0] * gain * self.output_gain;
            o.r[k] = x[1] * gain * self.output_gain;

            block_min = block_min.min(gain);
            self.graph_min = self.graph_min.min(gain);
            self.count += 1;
            if self.count >= self.refresh {
                self.count = 0;
                self.meter.push(self.graph_min);
                self.graph_min = 1.0;
            }
        }
        self.meter.feed(block_min);
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            LimiterParams::THRESHOLD => self.p.threshold_db = value,
            LimiterParams::RELEASE => self.p.release_ms = value,
            LimiterParams::LOOKAHEAD => self.p.lookahead_ms = value,
            LimiterParams::OUTPUT => self.p.output_db = value,
            LimiterParams::BYPASS => self.p.bypass = value > 0.5,
            LimiterParams::SPEED => self.p.graph_speed = value,
            _ => return,
        }
        self.update();
    }
}

// ---------------------------------------------------------------------------

/// Lock-free history of the most recent audio, written by the audio thread and read by
/// visualizers. Readers may see a torn frame now and then, which is fine for drawing.
pub struct Tap {
    l: Box<[AtomicU32]>,
    r: Box<[AtomicU32]>,
    written: AtomicU64,
}

impl Tap {
    pub const FRAMES: usize = 1 << 15;
    const MASK: u64 = Self::FRAMES as u64 - 1;

    pub fn new() -> Arc<Self> {
        let buf = || (0..Self::FRAMES).map(|_| AtomicU32::new(0)).collect();
        Arc::new(Self { l: buf(), r: buf(), written: AtomicU64::new(0) })
    }

    /// Total frames written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    pub(crate) fn push(&self, l: &[f32], r: &[f32]) {
        let w = self.written.load(Ordering::Relaxed);
        for (k, (a, b)) in l.iter().zip(r).enumerate() {
            let i = ((w + k as u64) & Self::MASK) as usize;
            self.l[i].store(a.to_bits(), Ordering::Relaxed);
            self.r[i].store(b.to_bits(), Ordering::Relaxed);
        }
        self.written.store(w + l.len() as u64, Ordering::Release);
    }

    /// Fills `l`/`r` with the frames just before `end` (a [`written`](Self::written) count),
    /// zero-padded at the front when there isn't enough history.
    pub fn read(&self, end: u64, l: &mut [f32], r: &mut [f32]) {
        let len = l.len().min(r.len());
        let n = (len.min(Self::FRAMES) as u64).min(end);
        let off = len - n as usize;
        l[..off].fill(0.0);
        r[..off].fill(0.0);
        for k in 0..n {
            let i = ((end - n + k) & Self::MASK) as usize;
            l[off + k as usize] = f32::from_bits(self.l[i].load(Ordering::Relaxed));
            r[off + k as usize] = f32::from_bits(self.r[i].load(Ordering::Relaxed));
        }
    }
}

/// Pass-through node that records what flows through it for a visualizer.
pub struct TapNode {
    tap: Arc<Tap>,
}

impl TapNode {
    pub fn new(tap: Arc<Tap>) -> Self {
        Self { tap }
    }
}

impl Processor for TapNode {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        o.l[..n].copy_from_slice(&i.l[..n]);
        o.r[..n].copy_from_slice(&i.r[..n]);
        self.tap.push(&i.l[..n], &i.r[..n]);
    }
}

// ---------------------------------------------------------------------------

/// The audio-thread end of a queue between the graph's MIDI and another thread. Shared so
/// the processor can be rebuilt; only the audio thread locks it, so `try_lock` always wins.
pub type MidiQueue<T> = Arc<std::sync::Mutex<T>>;

pub fn midi_queue() -> (MidiQueue<rtrb::Producer<MidiEvent>>, MidiQueue<rtrb::Consumer<MidiEvent>>) {
    let (tx, rx) = rtrb::RingBuffer::new(MidiBuf::CAPACITY * 4);
    (Arc::new(std::sync::Mutex::new(tx)), Arc::new(std::sync::Mutex::new(rx)))
}

/// Connects the graph's MIDI with other threads: events from `from` come out of the node's
/// MIDI output, events at its MIDI input go to `to` (and with `thru`, on to the output too).
/// Audio, if the node has any, passes through, recorded by `tap`.
#[derive(Default)]
pub struct MidiBridge {
    pub from: Option<MidiQueue<rtrb::Consumer<MidiEvent>>>,
    pub to: Option<MidiQueue<rtrb::Producer<MidiEvent>>>,
    pub tap: Option<Arc<Tap>>,
    pub thru: bool,
}

impl Processor for MidiBridge {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        if let (Some(i), Some(o)) = (inputs.first(), outputs.first_mut()) {
            o.l[..n].copy_from_slice(&i.l[..n]);
            o.r[..n].copy_from_slice(&i.r[..n]);
            if let Some(t) = &self.tap {
                t.push(&i.l[..n], &i.r[..n]);
            }
        }
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process(n, inputs, outputs);
        if self.thru {
            for e in midi_in {
                midi_out.push(*e);
            }
        }
        if let Some(Ok(mut tx)) = self.to.as_ref().map(|t| t.try_lock()) {
            for e in midi_in {
                let _ = tx.push(*e);
            }
        }
        if let Some(Ok(mut rx)) = self.from.as_ref().map(|r| r.try_lock()) {
            while let Ok(e) = rx.pop() {
                if !midi_out.push(MidiEvent { time: 0, ..e }) {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------

const BYPASS_BUDGET_BYTES: usize = 128 * 1024 * 1024;
const BYPASS_MIDI_CAPACITY: usize = 4096;
static BYPASS_ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Shared by live, queued and retired bypasses, so control-side replacement cannot exceed
/// the bound by keeping an old allocation alive until the audio thread acknowledges it.
struct BypassReservation<'a> {
    budget: &'a AtomicUsize,
    bytes: usize,
}

impl<'a> BypassReservation<'a> {
    fn reserve_in(budget: &'a AtomicUsize, limit: usize, bytes: usize) -> anyhow::Result<Self> {
        budget.try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|&next| next <= limit)
        }).map_err(|_| anyhow::anyhow!("prepared plugin bypass memory budget exceeded ({bytes} bytes requested, {limit} byte limit)"))?;
        Ok(Self { budget, bytes })
    }
}

impl Drop for BypassReservation<'_> {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn bypass_allocation_bytes(total_latency: u32) -> anyhow::Result<usize> {
    // Include the warmed output and MidiBuf owned by PluginProc as well as delay storage.
    // Standalone restart fallbacks conservatively reserve the same overhead.
    let fixed = BYPASS_MIDI_CAPACITY
        .checked_mul(size_of::<super::compensation::TimedMidi>())
        .and_then(|n| n.checked_add(super::MAX_BLOCK * 2 * size_of::<f32>()))
        .and_then(|n| n.checked_add(MidiBuf::CAPACITY * size_of::<MidiEvent>()))
        .and_then(|n| n.checked_add(4096))
        .ok_or_else(|| anyhow::anyhow!("prepared bypass allocation size overflow"))?;
    usize::try_from(total_latency)?
        .checked_mul(2 * size_of::<f32>())
        .and_then(|n| n.checked_add(fixed))
        .ok_or_else(|| anyhow::anyhow!("prepared bypass allocation size overflow"))
}

/// Prepared fixed-timeline replacement while a plugin restarts/faults. FX emits delayed dry,
/// instruments emit silence, MIDI passes with bridge latency only. No plugin calls are made.
pub struct LatencyBypass {
    audio: super::compensation::AudioDelay,
    midi: super::compensation::MidiDelay,
    instrument: bool,
    cursor: u64,
    // Declared last: release the reservation only after delay storage has been destroyed.
    _reservation: BypassReservation<'static>,
}

impl LatencyBypass {
    pub fn new(total_latency: u32, bridge_latency: u32, instrument: bool) -> anyhow::Result<Self> {
        super::timing::NodeTiming::from_total(total_latency, bridge_latency)?;
        let reservation = BypassReservation::reserve_in(
            &BYPASS_ALLOCATED_BYTES,
            BYPASS_BUDGET_BYTES,
            bypass_allocation_bytes(total_latency)?,
        )?;
        Ok(Self {
            audio: super::compensation::AudioDelay::new(total_latency as u64)?,
            midi: super::compensation::MidiDelay::new(bridge_latency as u64, BYPASS_MIDI_CAPACITY)?,
            instrument,
            cursor: 0,
            _reservation: reservation,
        })
    }
}

impl Processor for LatencyBypass {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let source = if self.instrument {
            None
        } else {
            inputs.first()
        };
        if let Some(out) = outputs.first_mut() {
            self.audio.write(source, out, n);
        }
        for out in outputs.iter_mut().skip(1) {
            out.clear(n);
        }
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process_timed(self.cursor, n, inputs, outputs, midi_in, midi_out);
    }

    fn process_timed(
        &mut self,
        first_sample: u64,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process(n, inputs, outputs);
        self.midi.process(midi_in, midi_out, first_sample, n);
        self.cursor = first_sample.saturating_add(n as u64);
    }
}

struct PreparedBypass {
    fallback: Box<LatencyBypass>,
    audio: Vec<StereoBuf>,
    midi: MidiBuf,
}

/// A hosted plugin's processor with a bypass switch (parameter 0). `new` retains legacy
/// immediate bypass; `with_latency` opts into prepared, constantly warmed delayed dry.
pub struct PluginProc {
    pub inner: Box<dyn Processor>,
    bypass: bool,
    delayed: Option<PreparedBypass>,
    cursor: u64,
}

impl PluginProc {
    pub fn new(inner: Box<dyn Processor>) -> Self {
        Self {
            inner,
            bypass: false,
            delayed: None,
            cursor: 0,
        }
    }

    /// Control-thread construction only. total includes algorithm + bridge; MIDI inherits
    /// bridge only. Instrument bypass is silent. The inner processor keeps running while
    /// bypassed so remote FIFOs and plugin timelines do not become stale.
    pub fn with_latency(
        inner: Box<dyn Processor>,
        total_latency: u32,
        bridge_latency: u32,
        instrument: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            bypass: false,
            cursor: 0,
            delayed: Some(PreparedBypass {
                fallback: Box::new(LatencyBypass::new(
                    total_latency,
                    bridge_latency,
                    instrument,
                )?),
                audio: vec![StereoBuf::new()],
                midi: MidiBuf::new(),
            }),
        })
    }
}

impl Processor for PluginProc {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        if let Some(delayed) = &mut self.delayed {
            delayed.fallback.process(n, inputs, &mut delayed.audio);
            self.inner.process(n, inputs, outputs);
            if self.bypass {
                for (index, out) in outputs.iter_mut().enumerate() {
                    if let Some(dry) = delayed.audio.get(index) {
                        out.l[..n].copy_from_slice(&dry.l[..n]);
                        out.r[..n].copy_from_slice(&dry.r[..n]);
                    } else {
                        out.clear(n);
                    }
                }
            }
        } else if self.bypass {
            for (index, out) in outputs.iter_mut().enumerate() {
                if let Some(input) = inputs.get(index) {
                    out.l[..n].copy_from_slice(&input.l[..n]);
                    out.r[..n].copy_from_slice(&input.r[..n]);
                } else {
                    out.clear(n);
                }
            }
        } else {
            self.inner.process(n, inputs, outputs);
        }
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process_timed(self.cursor, n, inputs, outputs, midi_in, midi_out);
    }

    fn process_timed(
        &mut self,
        first_sample: u64,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        if let Some(delayed) = &mut self.delayed {
            delayed.midi.clear();
            delayed.fallback.process_timed(
                first_sample,
                n,
                inputs,
                &mut delayed.audio,
                midi_in,
                &mut delayed.midi,
            );
            self.inner
                .process_timed(first_sample, n, inputs, outputs, midi_in, midi_out);
            if self.bypass {
                for (index, output) in outputs.iter_mut().enumerate() {
                    if let Some(dry) = delayed.audio.get(index) {
                        output.l[..n].copy_from_slice(&dry.l[..n]);
                        output.r[..n].copy_from_slice(&dry.r[..n]);
                    } else {
                        output.clear(n);
                    }
                }
                midi_out.clear();
                for event in delayed.midi.events() {
                    midi_out.push(*event);
                }
            }
        } else if self.bypass {
            for (index, output) in outputs.iter_mut().enumerate() {
                if let Some(input) = inputs.get(index) {
                    output.l[..n].copy_from_slice(&input.l[..n]);
                    output.r[..n].copy_from_slice(&input.r[..n]);
                } else {
                    output.clear(n);
                }
            }
            for event in midi_in {
                midi_out.push(*event);
            }
        } else {
            self.inner
                .process_timed(first_sample, n, inputs, outputs, midi_in, midi_out);
        }
        self.cursor = first_sample.saturating_add(n as u64);
    }

    fn set_param(&mut self, _index: usize, value: f32) {
        self.bypass = value > 0.5;
    }
    fn replace_latency_bypass(&mut self, bypass: Box<LatencyBypass>) -> Box<LatencyBypass> {
        match &mut self.delayed {
            Some(delayed) => std::mem::replace(&mut delayed.fallback, bypass),
            None => bypass,
        }
    }
    fn deactivate(&mut self) {
        self.inner.deactivate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MAX_BLOCK;

    #[test]
    fn delayed_bypass_is_warm_and_keeps_inner_pumping() {
        struct Counter(Arc<AtomicUsize>);
        impl Processor for Counter {
            fn process(&mut self, n: usize, _: &mut [StereoBuf], out: &mut [StereoBuf]) {
                self.0.fetch_add(1, Ordering::Relaxed);
                out[0].clear(n);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut proc =
            PluginProc::with_latency(Box::new(Counter(count.clone())), 128, 64, false).unwrap();
        let (mut input, mut output) = ([StereoBuf::new()], [StereoBuf::new()]);
        let mut midi = MidiBuf::new();
        input[0].l[0] = 1.0;
        proc.process_timed(
            0,
            64,
            &mut input,
            &mut output,
            &[MidiEvent {
                time: 0,
                data: [0x90, 60, 1],
            }],
            &mut midi,
        );
        assert_eq!(output[0].l[0], 0.0);
        proc.set_param(0, 1.0);
        input[0].clear(128);
        midi.clear();
        proc.process_timed(64, 128, &mut input, &mut output, &[], &mut midi);
        assert_eq!(output[0].l[64], 1.0);
        assert_eq!(
            midi.events(),
            &[MidiEvent {
                time: 0,
                data: [0x90, 60, 1]
            }]
        );
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn bypass_budget_counts_live_and_retired_reservations() {
        let budget = AtomicUsize::new(0);
        let live = BypassReservation::reserve_in(&budget, 100, 60).unwrap();
        let retired = BypassReservation::reserve_in(&budget, 100, 40).unwrap();
        assert!(BypassReservation::reserve_in(&budget, 100, 1).is_err());
        assert_eq!(budget.load(Ordering::Relaxed), 100);
        drop(retired);
        let queued = BypassReservation::reserve_in(&budget, 100, 40).unwrap();
        drop(live);
        drop(queued);
        assert_eq!(budget.load(Ordering::Relaxed), 0);
        assert!(BypassReservation::reserve_in(&budget, 100, usize::MAX).is_err());
        assert_eq!(budget.load(Ordering::Relaxed), 0);
        budget.store(1, Ordering::Relaxed);
        assert!(BypassReservation::reserve_in(&budget, usize::MAX, usize::MAX).is_err());
        assert_eq!(budget.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bypass_budget_charges_delay_and_warmed_storage_before_allocating() {
        let fixed = BYPASS_MIDI_CAPACITY * size_of::<crate::engine::compensation::TimedMidi>()
            + MAX_BLOCK * 2 * size_of::<f32>()
            + MidiBuf::CAPACITY * size_of::<MidiEvent>()
            + 4096;
        assert_eq!(bypass_allocation_bytes(0).unwrap(), fixed);
        assert_eq!(bypass_allocation_bytes(512).unwrap(), fixed + 512 * 8);
        let budget = AtomicUsize::new(0);
        let oversized = bypass_allocation_bytes(u32::MAX).unwrap();
        assert!(BypassReservation::reserve_in(&budget, BYPASS_BUDGET_BYTES, oversized).is_err());
        assert_eq!(budget.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn configured_bypass_uses_new_latency_without_restarting_inner() {
        struct Counter(Arc<AtomicUsize>);
        impl Processor for Counter {
            fn process(&mut self, n: usize, _: &mut [StereoBuf], out: &mut [StereoBuf]) {
                self.0.fetch_add(1, Ordering::Relaxed);
                out[0].clear(n);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut proc =
            PluginProc::with_latency(Box::new(Counter(count.clone())), 128, 0, false).unwrap();
        proc.set_param(0, 1.0);
        let (mut input, mut output) = ([StereoBuf::new()], [StereoBuf::new()]);
        let mut midi = MidiBuf::new();
        proc.process_timed(1000, 128, &mut input, &mut output, &[], &mut midi);
        let retired =
            proc.replace_latency_bypass(Box::new(LatencyBypass::new(512, 0, false).unwrap()));
        assert_eq!(retired.audio.frames(), 128);
        input[0].l[0] = 1.0;
        proc.process_timed(1128, 256, &mut input, &mut output, &[], &mut midi);
        assert!(output[0].l[..256].iter().all(|&sample| sample == 0.0));
        input[0].clear(256);
        proc.process_timed(1384, 256, &mut input, &mut output, &[], &mut midi);
        assert!(output[0].l[..256].iter().all(|&sample| sample == 0.0));
        proc.process_timed(1640, 1, &mut input, &mut output, &[], &mut midi);
        assert_eq!(output[0].l[0], 1.0);
        assert_eq!(count.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn instrument_bypass_is_silent_and_handles_absent_inputs() {
        let mut proc = LatencyBypass::new(128, 64, true).unwrap();
        let mut output = [StereoBuf::new()];
        output[0].l.fill(1.0);
        proc.process(256, &mut [], &mut output);
        assert!(output[0].l[..256].iter().all(|v| *v == 0.0));
        assert!(PluginProc::with_latency(Box::new(Through), 1, 2, false).is_err());
    }

    #[test]
    fn legacy_bypass_still_passes_audio_and_midi_immediately() {
        let mut proc = PluginProc::new(Box::new(Through));
        proc.set_param(0, 1.0);
        let (mut input, mut output) = ([StereoBuf::new()], [StereoBuf::new()]);
        input[0].l[0] = 0.5;
        let event = MidiEvent {
            time: 7,
            data: [0x90, 60, 1],
        };
        let mut midi = MidiBuf::new();
        proc.process_midi(127, &mut input, &mut output, &[event], &mut midi);
        assert_eq!(output[0].l[0], 0.5);
        assert_eq!(midi.events(), &[event]);
    }

    #[test]
    fn limiter_holds_threshold_and_releases() {
        let sr = 48_000.0;
        let meter = Arc::new(LimiterMeter::default());
        let mut lim = KazuLimiter::new(sr, LimiterParams::default(), meter.clone());
        let (mut ins, mut outs) = ([StereoBuf::new()], [StereoBuf::new()]);
        let threshold = db_to_gain(-6.0);
        // A loud 1 kHz sine never exceeds the threshold at the output.
        let mut peak = 0f32;
        for b in 0..50 {
            for k in 0..MAX_BLOCK {
                let t = (b * MAX_BLOCK + k) as f32 / sr;
                let v = (t * 1000.0 * std::f32::consts::TAU).sin();
                ins[0].l[k] = v;
                ins[0].r[k] = v * 0.5;
            }
            lim.process(MAX_BLOCK, &mut ins, &mut outs);
            peak = outs[0].l.iter().chain(&outs[0].r).fold(peak, |m, v| m.max(v.abs()));
        }
        assert!(peak <= threshold + 1e-6, "peak {peak} over threshold {threshold}");
        let gr = meter.take_gain();
        assert!((gr - threshold).abs() < 0.01, "gain {gr}");
        assert!(meter.history().iter().any(|g| *g < 0.6));

        // Quiet input: gain recovers to unity within a few release times.
        ins[0].clear(MAX_BLOCK);
        ins[0].l.fill(0.01);
        ins[0].r.fill(0.01);
        for _ in 0..20 {
            lim.process(MAX_BLOCK, &mut ins, &mut outs);
        }
        assert!((outs[0].l[MAX_BLOCK - 1] - 0.01).abs() < 1e-4);
    }

    #[test]
    fn tap_reads_latest_frames_across_wrap() {
        let tap = Tap::new();
        let (mut l, mut r) = (vec![9.0; 4], vec![9.0; 4]);
        tap.push(&[1.0, 2.0], &[-1.0, -2.0]);
        tap.read(tap.written(), &mut l, &mut r);
        assert_eq!(l, [0.0, 0.0, 1.0, 2.0]);
        assert_eq!(r, [0.0, 0.0, -1.0, -2.0]);

        let block: Vec<f32> = (0..1000).map(|k| k as f32).collect();
        for _ in 0..(Tap::FRAMES / 1000 + 2) {
            tap.push(&block, &block);
        }
        tap.read(tap.written(), &mut l, &mut r);
        assert_eq!(l, [996.0, 997.0, 998.0, 999.0]);
    }
}
