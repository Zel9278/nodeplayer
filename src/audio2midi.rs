//! Audio → MIDI node, ported from `realtime-audio2midi`.
//!
//! Audio passes through unchanged while a [`Tap`] records it. A worker thread cuts the mono mix
//! into 2048-sample frames (hop 1024) and runs the original three detectors on each:
//! - the frame's peak level, auto-gained, picks a channel and note 48 + channel,
//! - the 10 strongest FFT peaks become notes (with pitch bend for the cents offset),
//! - McLeod pitch detection adds the detected note.
//!
//! Velocity and channel come from the level. Notes are released after the note length unless
//! retriggered. Events go out through an ALSA sequencer port.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alsa::seq::{Addr, EvCtrl, EvNote, Event, EventType, PortCap, PortSubscribe, PortType, Seq};
use crossbeam_channel::{Receiver, Sender};
use pitch_detection::detector::PitchDetector;
use pitch_detection::detector::mcleod::McLeodDetector;
use realfft::{RealFftPlanner, RealToComplex};
use realfft::num_complex::Complex;

use crate::engine::MidiEvent;
use crate::engine::nodes::{MidiBridge, MidiQueue, Tap, midi_queue};

pub const FRAME_SIZES: [usize; 5] = [512, 1024, 2048, 4096, 8192];
const CHANNELS: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    On { ch: u8, note: u8, vel: u8 },
    Off { ch: u8, note: u8 },
    /// -8192..=8191, 0 is centered.
    Bend { ch: u8, value: i16 },
    Ctrl { ch: u8, param: u8, value: u8 },
}

impl Msg {
    pub fn to_event(self) -> MidiEvent {
        let data = match self {
            Msg::On { ch, note, vel } => [0x90 | ch, note, vel],
            Msg::Off { ch, note } => [0x80 | ch, note, 0],
            Msg::Bend { ch, value } => {
                let v = (value as i32 + 8192).clamp(0, 16383) as u16;
                [0xe0 | ch, (v & 0x7f) as u8, (v >> 7) as u8]
            }
            Msg::Ctrl { ch, param, value } => [0xb0 | ch, param, value],
        };
        MidiEvent { time: 0, data }
    }
}

/// Which MIDI channel a detector's notes go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChannelMode {
    /// Louder notes on higher channels (the original behavior).
    ByLevel,
    /// Always this channel, 0-based.
    Fixed(u8),
}

impl ChannelMode {
    fn pick(self, level: f32) -> u8 {
        match self {
            Self::ByLevel => amplitude_to_channel(level),
            Self::Fixed(ch) => ch.min(CHANNELS - 1),
        }
    }

    fn label(self) -> String {
        match self {
            Self::ByLevel => "by level".into(),
            Self::Fixed(ch) => format!("ch {}", ch + 1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VelocityMode {
    /// Level in dB mapped onto the velocity range (sine-calibrated RMS for the level and pitch
    /// detectors, normalized peak amplitude for FFT peaks).
    Decibel,
    /// The original formulas: linear level × 127, the raw FFT magnitude for peaks and the
    /// pitch clarity for detected notes. Mostly saturates at 127.
    Original,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Input {
    Mid,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub amplitude_notes: bool,
    pub fft_peaks: bool,
    pub pitch_detect: bool,
    pub pitch_bend: bool,
    /// 0..=100, scales velocities like the original's volume argument.
    pub volume_percent: f32,
    pub note_ms: f32,

    // Analysis
    pub frame: usize,
    pub hop: usize,
    pub input: Input,

    // Gain applied to levels before they become velocities and channels
    pub auto_gain: bool,
    /// Ceiling of the automatic gain.
    pub max_gain_db: f32,
    /// Gain when automatic gain is off.
    pub gain_db: f32,

    // Level detector
    pub level_base_note: u8,
    pub level_channel: ChannelMode,

    // FFT peaks
    pub max_peaks: usize,
    /// Minimum (unnormalized) FFT magnitude of a peak, in dB.
    pub peak_threshold_db: f32,
    pub peak_min_hz: f32,
    pub peak_max_hz: f32,
    pub peak_channel: ChannelMode,
    /// Pitch bend range in semitones; the synth must use the same (an RPN is sent for it).
    pub bend_range: f32,

    // Pitch detection (McLeod)
    pub power_threshold: f32,
    pub clarity_threshold: f32,
    pub pitch_channel: ChannelMode,

    // Output
    pub transpose: i32,
    pub min_note: u8,
    pub max_note: u8,
    /// Notes at or below this velocity are dropped (Original mode; the dB curve's floor does
    /// this job there).
    pub min_velocity: u8,

    // Velocity curve
    pub velocity_mode: VelocityMode,
    /// Level that maps to the lowest velocity; quieter is silent.
    pub vel_floor_db: f32,
    /// Shape of the curve: 1 is linear in dB, above 1 favors soft velocities.
    pub vel_curve: f32,
    pub vel_min: u8,
    pub vel_max: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            amplitude_notes: true,
            fft_peaks: true,
            pitch_detect: true,
            pitch_bend: true,
            volume_percent: 100.0,
            note_ms: 10.0,
            frame: 2048,
            hop: 1024,
            input: Input::Mid,
            // Off by default: it lifts quiet passages by up to 20 dB, which makes everything loud.
            auto_gain: false,
            max_gain_db: 20.0,
            gain_db: 0.0,
            level_base_note: 48,
            level_channel: ChannelMode::ByLevel,
            max_peaks: 10,
            peak_threshold_db: -80.0,
            peak_min_hz: 0.0,
            peak_max_hz: 24_000.0,
            peak_channel: ChannelMode::ByLevel,
            bend_range: 2.0,
            power_threshold: 0.001,
            clarity_threshold: 0.3,
            pitch_channel: ChannelMode::ByLevel,
            transpose: 0,
            min_note: 0,
            max_note: 127,
            min_velocity: 5,
            velocity_mode: VelocityMode::Decibel,
            vel_floor_db: -60.0,
            vel_curve: 1.0,
            vel_min: 1,
            vel_max: 127,
        }
    }
}

impl Config {
    /// The original program's behavior: its velocity formulas and automatic gain.
    pub fn original() -> Self {
        Self { velocity_mode: VelocityMode::Original, auto_gain: true, ..Self::default() }
    }

    /// Velocity for a level, plus its 0..1 position used by "by level" channels. `original` is
    /// the level the original formula used (volume included), `measured` the linear amplitude
    /// for the dB curve.
    fn velocity(&self, original: f32, measured: f32) -> (u8, f32) {
        match self.velocity_mode {
            VelocityMode::Original => ((original * 127.0).min(127.0) as u8, original),
            VelocityMode::Decibel => {
                let db = 20.0 * measured.max(1e-9).log10();
                if db < self.vel_floor_db {
                    return (0, 0.0);
                }
                let t = ((db - self.vel_floor_db) / -self.vel_floor_db).clamp(0.0, 1.0).powf(self.vel_curve);
                let (lo, hi) = (self.vel_min as f32, self.vel_max as f32);
                let volume = (self.volume_percent / 100.0).clamp(0.0, 1.0);
                (((lo + (hi - lo) * t) * volume).round().clamp(0.0, 127.0) as u8, t)
            }
        }
    }

    /// Velocities at or below this are dropped.
    fn velocity_gate(&self) -> u8 {
        match self.velocity_mode {
            VelocityMode::Original => self.min_velocity,
            // Only what's below the floor (velocity 0) is dropped, so the volume or range
            // settings can't silence everything.
            VelocityMode::Decibel => 0,
        }
    }

    /// Keeps values usable whatever a saved file contained.
    fn sanitized(mut self) -> Self {
        if !FRAME_SIZES.contains(&self.frame) {
            self.frame = 2048;
        }
        self.hop = self.hop.clamp(self.frame / 8, self.frame);
        self.max_peaks = self.max_peaks.clamp(1, 32);
        self.bend_range = self.bend_range.clamp(1.0, 24.0);
        self.max_note = self.max_note.min(127);
        self.min_note = self.min_note.min(self.max_note);
        self.vel_floor_db = self.vel_floor_db.clamp(-120.0, -6.0);
        self.vel_curve = self.vel_curve.clamp(0.1, 10.0);
        self.vel_max = self.vel_max.clamp(1, 127);
        self.vel_min = self.vel_min.min(self.vel_max);
        self
    }
}

/// GM's percussion channel (channel 10): synths play drums on it whatever the note.
pub const DRUM_CHANNEL: u8 = 9;

/// The step of `amplitude` among the 15 melodic channels.
fn level_step(amplitude: f32) -> u8 {
    ((amplitude.clamp(0.0, 1.0) * (CHANNELS - 1) as f32).floor() as u8).min(CHANNELS - 2)
}

/// Louder on higher channels, skipping the drum channel so no note turns into a drum hit.
fn amplitude_to_channel(amplitude: f32) -> u8 {
    let step = level_step(amplitude);
    if step >= DRUM_CHANNEL { step + 1 } else { step }
}

/// MIDI note nearest to `freq`, and the pitch bend for the remainder given a bend range of
/// `range` semitones.
fn freq_to_note_bend(freq: f32, range: f32) -> (u8, i16) {
    if freq <= 0.0 {
        return (0, 0);
    }
    let exact = 69.0 + 12.0 * (freq / 440.0).log2();
    let note = exact.round().clamp(0.0, 127.0) as u8;
    let cents = (exact - note as f32) * 100.0;
    (note, ((cents / (range * 100.0) * 8192.0) as i16).clamp(-8192, 8191))
}

pub fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[n as usize % 12], n as i32 / 12 - 1)
}

/// The detection pipeline, independent of where audio comes from and MIDI goes to.
pub struct Analyzer {
    sample_rate: u32,
    frame: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    spectrum: Vec<f32>,
    detector: McLeodDetector<f32>,
    /// Converts an FFT magnitude to the amplitude of a sine (2 / sum of the window).
    norm: f32,
    gain: f32,
    /// Sine-calibrated RMS of the last frame after gain, in dB.
    level_db: f32,
    /// (note, channel) → time it was (re)triggered.
    active: BTreeMap<(u8, u8), f64>,
}

impl Analyzer {
    pub fn new(sample_rate: u32, frame: usize) -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(frame);
        // Symmetric Hann, like `apodize::hanning_iter`.
        let window: Vec<f32> = (0..frame)
            .map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (frame - 1) as f32).cos())
            .collect();
        let norm = 2.0 / window.iter().sum::<f32>();
        Self {
            norm,
            level_db: -200.0,
            sample_rate,
            frame,
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            spectrum: vec![0.0; frame / 2],
            fft,
            window,
            detector: McLeodDetector::new(frame, frame / 2),
            gain: 1.0,
            active: BTreeMap::new(),
        }
    }

    pub fn frame_len(&self) -> usize {
        self.frame
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    pub fn active_notes(&self) -> impl Iterator<Item = u8> + '_ {
        self.active.keys().map(|(n, _)| *n)
    }

    /// Applies transpose, note range and the velocity floor, then starts (or retriggers) the note.
    fn note_on(&mut self, time: f64, note: u8, vel: u8, ch: u8, cfg: &Config, out: &mut Vec<Msg>) {
        let note = note as i32 + cfg.transpose;
        if vel <= cfg.velocity_gate() || note < cfg.min_note as i32 || note > cfg.max_note as i32 {
            return;
        }
        let note = note as u8;
        // A note that is still sounding is only retriggered (its release is pushed back).
        if let Some(t) = self.active.get_mut(&(note, ch)) {
            *t = time;
            return;
        }
        out.push(Msg::On { ch, note, vel });
        self.active.insert((note, ch), time);
    }

    fn release_expired(&mut self, time: f64, note_secs: f64, out: &mut Vec<Msg>) {
        self.active.retain(|&(note, ch), &mut start| {
            let keep = time - start < note_secs;
            if !keep {
                out.push(Msg::Off { ch, note });
            }
            keep
        });
    }

    pub fn release_all(&mut self, out: &mut Vec<Msg>) {
        for (note, ch) in std::mem::take(&mut self.active).into_keys() {
            out.push(Msg::Off { ch, note });
        }
    }

    /// Analyzes one frame (`frame_len` mono samples) starting at `time` seconds.
    pub fn frame(&mut self, frame: &[f32], time: f64, cfg: &Config, out: &mut Vec<Msg>) {
        let volume = (cfg.volume_percent / 100.0).clamp(0.0, 1.0);
        let peak = frame.iter().fold(0f32, |m, x| m.max(x.abs()));
        if cfg.auto_gain {
            // Adjusted every frame, as in the original.
            let max = 10f32.powf(cfg.max_gain_db / 20.0);
            if peak < 0.1 {
                self.gain = (self.gain * 1.2).min(max);
            } else if peak > 0.8 {
                self.gain = (self.gain * 0.8).max(0.1);
            }
            self.gain = self.gain.min(max);
        } else {
            self.gain = 10f32.powf(cfg.gain_db / 20.0);
        }
        // RMS × √2 reads a sine's amplitude, so 0 dB is a full-scale sine.
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        let loudness = rms * std::f32::consts::SQRT_2 * self.gain;
        self.level_db = 20.0 * loudness.max(1e-10).log10();

        if cfg.amplitude_notes {
            let (vel, pos) = cfg.velocity((peak * self.gain).min(1.0) * volume, loudness);
            let ch = cfg.level_channel.pick(pos);
            // In "by level" mode the note follows the level step, as in the original.
            let offset = if cfg.level_channel == ChannelMode::ByLevel { level_step(pos) } else { 0 };
            let note = cfg.level_base_note.saturating_add(offset).min(127);
            self.note_on(time, note, vel, ch, cfg, out);
        }

        self.release_expired(time, cfg.note_ms as f64 / 1000.0, out);

        if cfg.fft_peaks {
            for (x, (s, w)) in self.input.iter_mut().zip(frame.iter().zip(&self.window)) {
                *x = s * w;
            }
            if self.fft.process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch).is_ok() {
                for (m, c) in self.spectrum.iter_mut().zip(&self.output) {
                    *m = c.norm();
                }
                let bin_hz = self.sample_rate as f32 / self.frame as f32;
                let threshold = 10f32.powf(cfg.peak_threshold_db / 20.0);
                let s = &self.spectrum;
                let mut peaks: Vec<(f32, f32)> = (1..s.len() - 1)
                    .filter(|&i| s[i] > s[i - 1] && s[i] > s[i + 1] && s[i] > threshold)
                    .map(|i| (i as f32 * bin_hz, s[i]))
                    .filter(|(f, _)| *f >= cfg.peak_min_hz && *f <= cfg.peak_max_hz)
                    .collect();
                peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
                for &(freq, amp) in peaks.iter().take(cfg.max_peaks) {
                    let (note, bend) = freq_to_note_bend(freq, cfg.bend_range);
                    let (vel, pos) = cfg.velocity(amp * self.gain * volume, amp * self.norm * self.gain);
                    if vel <= cfg.velocity_gate() {
                        continue;
                    }
                    let ch = cfg.peak_channel.pick(pos);
                    if cfg.pitch_bend {
                        out.push(Msg::Bend { ch, value: bend });
                    }
                    self.note_on(time, note, vel, ch, cfg, out);
                }
            }
        }

        if cfg.pitch_detect {
            let sr = self.sample_rate as usize;
            if let Some(p) = self.detector.get_pitch(frame, sr, cfg.power_threshold, cfg.clarity_threshold) {
                let (note, _) = freq_to_note_bend(p.frequency, cfg.bend_range);
                let (vel, pos) = cfg.velocity(p.clarity * self.gain * volume, loudness);
                self.note_on(time, note, vel, cfg.pitch_channel.pick(pos), cfg, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Settings UI

fn channel_combo(ui: &mut egui::Ui, id: &str, mode: &mut ChannelMode) {
    egui::ComboBox::from_id_salt(ui.id().with(id)).width(80.0).selected_text(mode.label()).show_ui(ui, |ui| {
        ui.selectable_value(mode, ChannelMode::ByLevel, "by level").on_hover_text("louder → higher channel (ch 10, the drums, is skipped)");
        for ch in 0..CHANNELS {
            let label = if ch == DRUM_CHANNEL { format!("ch {} (drums)", ch + 1) } else { format!("ch {}", ch + 1) };
            ui.selectable_value(mode, ChannelMode::Fixed(ch), label);
        }
    });
}

fn note_drag(ui: &mut egui::Ui, note: &mut u8, range: std::ops::RangeInclusive<u8>) {
    ui.add(
        egui::DragValue::new(note)
            .range(range)
            .speed(0.2)
            .custom_formatter(|n, _| format!("{} ({n})", note_name(n as u8))),
    );
}

/// The velocity curve, with the current input level marked.
fn curve_plot(ui: &mut egui::Ui, c: &Config, level_db: f32) {
    use egui::{Color32, Stroke, pos2, vec2};
    let (rect, _) = ui.allocate_exact_size(vec2(220.0, 90.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, Color32::from_gray(18));
    let lo = c.vel_floor_db - 12.0;
    let x_of = |db: f32| rect.left() + ((db - lo) / -lo).clamp(0.0, 1.0) * rect.width();
    let y_of = |v: f32| rect.bottom() - v / 127.0 * rect.height();
    let font = egui::FontId::proportional(8.0);
    let grid = Stroke::new(1.0, Color32::from_gray(45));
    for db in [-96.0, -72.0, -48.0, -24.0, -12.0, 0.0] {
        if db > lo {
            let x = x_of(db);
            p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], grid);
            p.text(pos2(x + 2.0, rect.bottom() - 1.0), egui::Align2::LEFT_BOTTOM, format!("{db:.0}"), font.clone(), Color32::from_gray(100));
        }
    }
    let curve = |db: f32| {
        let amp = 10f32.powf(db / 20.0);
        c.velocity(amp.min(1.0), amp).0 as f32
    };
    let pts: Vec<_> = (0..=110)
        .map(|i| {
            let db = lo - lo * i as f32 / 110.0;
            pos2(x_of(db), y_of(curve(db)))
        })
        .collect();
    p.line(pts, Stroke::new(1.5, Color32::from_rgb(110, 190, 255)));
    // Quieter than the floor sends nothing.
    let x = x_of(c.vel_floor_db);
    p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 120, 80, 140)));
    if level_db > lo {
        let db = level_db.min(0.0);
        p.circle_filled(pos2(x_of(db), y_of(curve(db))), 3.5, Color32::from_rgb(255, 210, 90));
    }
    p.text(rect.left_top() + vec2(3.0, 2.0), egui::Align2::LEFT_TOP, "velocity ↑   input dB →", font, Color32::from_gray(110));
}

/// The detailed settings, shown when the node's ⚙ is open.
pub fn settings_ui(ui: &mut egui::Ui, c: &mut Config, gain: f32, level_db: f32, sample_rate: f32) {
    let grid = |ui: &mut egui::Ui, title: &str, body: &mut dyn FnMut(&mut egui::Ui)| {
        ui.label(egui::RichText::new(title).strong().small());
        egui::Grid::new(ui.id().with(title)).num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| body(ui));
        ui.add_space(2.0);
    };

    grid(ui, "Analysis", &mut |ui| {
        ui.label("Frame");
        egui::ComboBox::from_id_salt(ui.id().with("frame")).selected_text(format!("{}", c.frame)).show_ui(ui, |ui| {
            for n in FRAME_SIZES {
                let ms = n as f32 / sample_rate * 1000.0;
                ui.selectable_value(&mut c.frame, n, format!("{n}  ({ms:.0} ms)"));
            }
        })
        .response
        .on_hover_text("longer = finer pitch, slower response; the lowest detectable pitch rises with shorter frames");
        ui.end_row();
        ui.label("Hop");
        let frame = c.frame;
        egui::ComboBox::from_id_salt(ui.id().with("hop")).selected_text(format!("{}", c.hop)).show_ui(ui, |ui| {
            for div in [8, 4, 2, 1] {
                let n = frame / div;
                let ms = n as f32 / sample_rate * 1000.0;
                ui.selectable_value(&mut c.hop, n, format!("{n}  ({ms:.1} ms, {:.0}/s)", sample_rate / n as f32));
            }
        })
        .response
        .on_hover_text("time between analyses");
        ui.end_row();
        ui.label("Input");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut c.input, Input::Mid, "Mid");
            ui.selectable_value(&mut c.input, Input::Left, "L");
            ui.selectable_value(&mut c.input, Input::Right, "R");
        });
        ui.end_row();
    });

    grid(ui, "Gain", &mut |ui| {
        ui.label("Auto gain");
        ui.horizontal(|ui| {
            ui.checkbox(&mut c.auto_gain, "");
            ui.small(format!("now ×{gain:.2} ({:+.1} dB)", 20.0 * gain.max(1e-6).log10()));
        });
        ui.end_row();
        if c.auto_gain {
            ui.label("Max gain");
            ui.add(egui::Slider::new(&mut c.max_gain_db, 0.0..=40.0).suffix(" dB"));
        } else {
            ui.label("Gain");
            ui.add(egui::Slider::new(&mut c.gain_db, -20.0..=40.0).suffix(" dB"));
        }
        ui.end_row();
    });

    grid(ui, "Velocity", &mut |ui| {
        ui.label("Mode");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut c.velocity_mode, VelocityMode::Decibel, "dB curve")
                .on_hover_text("velocity follows loudness in dB");
            ui.selectable_value(&mut c.velocity_mode, VelocityMode::Original, "Original")
                .on_hover_text("the original formulas; mostly 127");
        });
        ui.end_row();
        if c.velocity_mode == VelocityMode::Decibel {
            ui.label("Floor");
            ui.add(egui::Slider::new(&mut c.vel_floor_db, -96.0..=-12.0).suffix(" dB"))
                .on_hover_text("this level gives the lowest velocity; quieter sends nothing");
            ui.end_row();
            ui.label("Curve");
            ui.add(egui::Slider::new(&mut c.vel_curve, 0.25..=4.0).logarithmic(true))
                .on_hover_text("1 = linear in dB; higher = softer velocities, lower = louder");
            ui.end_row();
            ui.label("Range");
            ui.horizontal(|ui| {
                let max = c.vel_max;
                ui.add(egui::DragValue::new(&mut c.vel_min).range(0..=max).speed(0.3));
                ui.label("–");
                let min = c.vel_min;
                ui.add(egui::DragValue::new(&mut c.vel_max).range(min.max(1)..=127).speed(0.3));
            });
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                curve_plot(ui, c, level_db);
                let amp = 10f32.powf(level_db / 20.0);
                let (v, _) = c.velocity(amp.min(1.0), amp);
                ui.small(if level_db > -150.0 { format!("input {level_db:.1} dB → velocity {v}") } else { "no input".into() });
            });
            ui.end_row();
        }
    });

    grid(ui, "Level notes", &mut |ui| {
        ui.label("Base note");
        note_drag(ui, &mut c.level_base_note, 0..=127);
        ui.end_row();
        ui.label("Channel");
        channel_combo(ui, "lvl-ch", &mut c.level_channel);
        ui.end_row();
    });

    grid(ui, "FFT peaks", &mut |ui| {
        ui.label("Max peaks");
        ui.add(egui::Slider::new(&mut c.max_peaks, 1..=32));
        ui.end_row();
        ui.label("Threshold");
        ui.add(egui::Slider::new(&mut c.peak_threshold_db, -120.0..=40.0).suffix(" dB"))
            .on_hover_text("minimum raw FFT magnitude of a peak (−80 dB = the original's 0.0001)");
        ui.end_row();
        ui.label("Frequency");
        ui.horizontal(|ui| {
            let max = c.peak_max_hz;
            ui.add(egui::DragValue::new(&mut c.peak_min_hz).range(0.0..=max).speed(1.0).suffix(" Hz"));
            ui.label("–");
            let min = c.peak_min_hz;
            ui.add(egui::DragValue::new(&mut c.peak_max_hz).range(min..=sample_rate * 0.5).speed(10.0).suffix(" Hz"));
        });
        ui.end_row();
        ui.label("Channel");
        channel_combo(ui, "peak-ch", &mut c.peak_channel);
        ui.end_row();
        ui.label("Bend range");
        ui.add_enabled(c.pitch_bend, egui::Slider::new(&mut c.bend_range, 1.0..=24.0).step_by(1.0).suffix(" st"))
            .on_hover_text("sent to the synth as RPN 0 so both sides agree");
        ui.end_row();
    });

    grid(ui, "Pitch detection", &mut |ui| {
        ui.label("Power");
        ui.add(egui::Slider::new(&mut c.power_threshold, 0.00001..=0.5).logarithmic(true))
            .on_hover_text("minimum signal power to detect a pitch");
        ui.end_row();
        ui.label("Clarity");
        ui.add(egui::Slider::new(&mut c.clarity_threshold, 0.0..=1.0))
            .on_hover_text("how periodic the signal must be; higher = fewer, surer notes");
        ui.end_row();
        ui.label("Channel");
        channel_combo(ui, "pitch-ch", &mut c.pitch_channel);
        ui.end_row();
    });

    grid(ui, "Output", &mut |ui| {
        ui.label("Transpose");
        ui.add(egui::Slider::new(&mut c.transpose, -48..=48).suffix(" st"));
        ui.end_row();
        ui.label("Note range");
        ui.horizontal(|ui| {
            let max = c.max_note;
            note_drag(ui, &mut c.min_note, 0..=max);
            ui.label("–");
            let min = c.min_note;
            note_drag(ui, &mut c.max_note, min..=127);
        });
        ui.end_row();
        if c.velocity_mode == VelocityMode::Original {
            ui.label("Min velocity");
            ui.add(egui::Slider::new(&mut c.min_velocity, 0..=126)).on_hover_text("quieter notes are dropped");
            ui.end_row();
        }
    });

    ui.horizontal(|ui| {
        if ui.small_button("Reset to defaults").clicked() {
            *c = Config::default();
        }
        if ui.small_button("Original algorithm").on_hover_text("velocity formulas and auto gain as in realtime-audio2midi").clicked() {
            *c = Config::original();
        }
    });
}

// ---------------------------------------------------------------------------
// ALSA sequencer output

pub use crate::midi::Port as Dest;

struct SeqOut {
    seq: Seq,
    port: i32,
    client: i32,
    connected: Option<Addr>,
}

impl SeqOut {
    fn open() -> alsa::Result<Self> {
        let seq = Seq::open(None, Some(alsa::Direction::Playback), false)?;
        seq.set_client_name(&CString::new("nodeplayer").unwrap())?;
        let port = seq.create_simple_port(
            &CString::new("Audio2MIDI").unwrap(),
            PortCap::READ | PortCap::SUBS_READ,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )?;
        let client = seq.client_id()?;
        Ok(Self { seq, port, client, connected: None })
    }

    fn send(&self, m: Msg) -> alsa::Result<()> {
        let mut ev = match m {
            Msg::On { ch, note, vel } => {
                Event::new(EventType::Noteon, &EvNote { channel: ch, note, velocity: vel, off_velocity: 0, duration: 0 })
            }
            Msg::Off { ch, note } => {
                Event::new(EventType::Noteoff, &EvNote { channel: ch, note, velocity: 0, off_velocity: 0, duration: 0 })
            }
            Msg::Bend { ch, value } => Event::new(EventType::Pitchbend, &EvCtrl { channel: ch, param: 0, value: value as i32 }),
            Msg::Ctrl { ch, param, value } => {
                Event::new(EventType::Controller, &EvCtrl { channel: ch, param: param as u32, value: value as i32 })
            }
        };
        ev.set_source(self.port);
        ev.set_subs();
        ev.set_direct();
        self.seq.event_output_direct(&mut ev).map(|_| ())
    }

    /// Ports other clients can receive on.
    fn destinations(&self) -> Vec<Dest> {
        crate::midi::list_ports(&self.seq, self.client, PortCap::WRITE | PortCap::SUBS_WRITE)
    }

    fn connect(&mut self, dest: Option<Addr>) -> alsa::Result<()> {
        let me = Addr { client: self.client, port: self.port };
        if let Some(old) = self.connected.take() {
            let _ = self.seq.unsubscribe_port(me, old);
        }
        if let Some(d) = dest {
            let sub = PortSubscribe::empty()?;
            sub.set_sender(me);
            sub.set_dest(d);
            self.seq.subscribe_port(&sub)?;
            self.connected = Some(d);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Worker thread and UI handle

enum Cmd {
    Config(Config),
    Connect(Option<Dest>),
    /// Connects to the port with this label once it exists.
    ConnectLabel(String),
    Refresh,
    Panic,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    gain: AtomicU32,
    level_db: AtomicU32,
    /// Bitset of notes sounding on any channel.
    notes: [AtomicU64; 2],
    sent: AtomicU64,
    dests: Mutex<Vec<Dest>>,
    connected: Mutex<Option<Dest>>,
    error: Mutex<Option<String>>,
    /// "client:port" of our own port, for connecting from other tools.
    own: Mutex<Option<Addr>>,
}

pub struct Audio2Midi {
    pub tap: Arc<Tap>,
    /// Events for the node's MIDI output in the graph.
    midi_rx: MidiQueue<rtrb::Consumer<MidiEvent>>,
    pub config: Config,
    pub show_settings: bool,
    /// Label of the port the user picked, kept even while that port is missing.
    pub want: Option<String>,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    rate: (u64, Instant, f32),
}

impl Audio2Midi {
    pub fn new(sample_rate: f32) -> Self {
        let tap = Tap::new();
        let shared = Arc::new(Shared::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let config = Config::default();
        let (midi_tx, midi_rx) = midi_queue();
        let thread = {
            let (tap, shared) = (tap.clone(), shared.clone());
            std::thread::Builder::new()
                .name("audio2midi".into())
                .spawn(move || worker(tap, shared, rx, midi_tx, sample_rate as u32, config))
                .ok()
        };
        let _ = tx.send(Cmd::Refresh);
        Self { tap, midi_rx, config, show_settings: false, want: None, shared, tx, thread, rate: (0, Instant::now(), 0.0) }
    }

    /// Passes audio through into the tap and emits the detected notes at the MIDI output.
    pub fn processor(&self) -> MidiBridge {
        MidiBridge { from: Some(self.midi_rx.clone()), to: None, tap: Some(self.tap.clone()), thru: false }
    }

    pub fn set_config(&mut self, c: Config) {
        self.config = c;
        let _ = self.tx.send(Cmd::Config(c));
    }

    pub fn connect(&mut self, dest: Option<Dest>) {
        self.want = dest.as_ref().map(|d| d.label.clone());
        let _ = self.tx.send(Cmd::Connect(dest));
    }

    pub fn connect_label(&mut self, label: String) {
        self.want = Some(label.clone());
        let _ = self.tx.send(Cmd::ConnectLabel(label));
    }

    pub fn dest(&self) -> Option<Dest> {
        self.shared.connected.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let _ = self.tx.send(Cmd::Refresh);
    }

    pub fn panic(&self) {
        let _ = self.tx.send(Cmd::Panic);
    }

    pub fn destinations(&self) -> Vec<Dest> {
        self.shared.dests.lock().unwrap().clone()
    }

    pub fn own_port(&self) -> Option<Addr> {
        *self.shared.own.lock().unwrap()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.shared.gain.load(Ordering::Relaxed))
    }

    /// Input level of the last analyzed frame (sine-calibrated RMS after gain), dB.
    pub fn level_db(&self) -> f32 {
        f32::from_bits(self.shared.level_db.load(Ordering::Relaxed))
    }

    pub fn note_active(&self, note: u8) -> bool {
        let word = self.shared.notes[note as usize / 64].load(Ordering::Relaxed);
        word & (1 << (note % 64)) != 0
    }

    /// MIDI events per second, averaged over about half a second.
    pub fn event_rate(&mut self) -> f32 {
        let sent = self.shared.sent.load(Ordering::Relaxed);
        let (last, at, rate) = &mut self.rate;
        let dt = at.elapsed().as_secs_f32();
        if dt >= 0.5 {
            *rate = (sent - *last) as f32 / dt;
            *last = sent;
            *at = Instant::now();
        }
        *rate
    }
}

impl Drop for Audio2Midi {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn worker(
    tap: Arc<Tap>,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    graph: MidiQueue<rtrb::Producer<MidiEvent>>,
    sample_rate: u32,
    mut cfg: Config,
) {
    let mut out = match SeqOut::open() {
        Ok(s) => {
            *shared.own.lock().unwrap() = Some(Addr { client: s.client, port: s.port });
            Some(s)
        }
        Err(e) => {
            *shared.error.lock().unwrap() = Some(format!("ALSA sequencer: {e}"));
            None
        }
    };
    cfg = cfg.sanitized();
    let mut analyzer = Analyzer::new(sample_rate, cfg.frame);
    let (mut l, mut r, mut mono) = (Vec::new(), Vec::new(), Vec::new());
    let mut msgs = Vec::new();
    // `next_end` is the tap position where the next frame ends.
    let mut next_end = tap.written().max(cfg.frame as u64);
    // Bend range last announced to the synth.
    let mut announced_range = None;

    let send = |out: &Option<SeqOut>, msgs: &mut Vec<Msg>| {
        if let Ok(mut g) = graph.lock() {
            for m in msgs.iter() {
                let _ = g.push(m.to_event());
            }
        }
        if let Some(o) = out {
            for m in msgs.iter() {
                if let Err(e) = o.send(*m) {
                    log::warn!("MIDI send failed: {e}");
                    break;
                }
            }
            shared.sent.fetch_add(msgs.len() as u64, Ordering::Relaxed);
        }
        msgs.clear();
    };
    let panic = |out: &Option<SeqOut>, analyzer: &mut Analyzer, msgs: &mut Vec<Msg>| {
        analyzer.release_all(msgs);
        // All notes off and pitch bend reset on every channel.
        for ch in 0..CHANNELS {
            msgs.push(Msg::Ctrl { ch, param: 123, value: 0 });
            msgs.push(Msg::Bend { ch, value: 0 });
        }
        send(out, msgs);
    };

    while !shared.stop.load(Ordering::Acquire) {
        for cmd in rx.try_iter() {
            match cmd {
                Cmd::Config(c) => {
                    cfg = c.sanitized();
                    if analyzer.frame_len() != cfg.frame {
                        analyzer.release_all(&mut msgs);
                        send(&out, &mut msgs);
                        analyzer = Analyzer::new(sample_rate, cfg.frame);
                    }
                }
                Cmd::Panic => {
                    panic(&out, &mut analyzer, &mut msgs);
                    // Receivers may have reset; announce the bend range again.
                    announced_range = None;
                }
                Cmd::Refresh => {
                    if let Some(o) = &out {
                        *shared.dests.lock().unwrap() = o.destinations();
                    }
                }
                Cmd::Connect(d) => {
                    if let Some(o) = &mut out {
                        let res = o.connect(d.as_ref().map(|d| d.addr));
                        *shared.error.lock().unwrap() = res.as_ref().err().map(|e| format!("connect failed: {e}"));
                        *shared.connected.lock().unwrap() = d.filter(|_| res.is_ok());
                        announced_range = None;
                    }
                }
                Cmd::ConnectLabel(label) => {
                    if let Some(o) = &mut out {
                        let dests = o.destinations();
                        match dests.iter().find(|d| d.label == label) {
                            Some(d) => {
                                let res = o.connect(Some(d.addr));
                                *shared.error.lock().unwrap() = res.as_ref().err().map(|e| format!("connect failed: {e}"));
                                *shared.connected.lock().unwrap() = res.is_ok().then(|| d.clone());
                                announced_range = None;
                            }
                            None => *shared.error.lock().unwrap() = Some(format!("MIDI port \"{label}\" not found")),
                        }
                        *shared.dests.lock().unwrap() = dests;
                    }
                }
            }
        }

        // Tell the synth the pitch bend range (RPN 0) whenever it or the connection changed.
        if cfg.pitch_bend && announced_range != Some(cfg.bend_range) {
            let semis = cfg.bend_range.round() as u8;
            for ch in 0..CHANNELS {
                for (param, value) in [(101, 0), (100, 0), (6, semis), (38, 0), (101, 127), (100, 127)] {
                    msgs.push(Msg::Ctrl { ch, param, value });
                }
            }
            send(&out, &mut msgs);
            announced_range = Some(cfg.bend_range);
        }

        let (frame, hop) = (cfg.frame, cfg.hop as u64);
        l.resize(frame, 0.0);
        r.resize(frame, 0.0);
        mono.resize(frame, 0.0);
        let written = tap.written();
        // Too far behind to catch up from the tap's history: skip to the present.
        if written > next_end + (Tap::FRAMES - frame) as u64 {
            next_end = written;
        }
        next_end = next_end.max(frame as u64);
        let mut analyzed = false;
        while written >= next_end {
            tap.read(next_end, &mut l, &mut r);
            for (m, (a, b)) in mono.iter_mut().zip(l.iter().zip(&r)) {
                *m = match cfg.input {
                    Input::Mid => (a + b) * 0.5,
                    Input::Left => *a,
                    Input::Right => *b,
                };
            }
            let time = (next_end - frame as u64) as f64 / sample_rate as f64;
            analyzer.frame(&mono, time, &cfg, &mut msgs);
            send(&out, &mut msgs);
            next_end += hop;
            analyzed = true;
        }
        if analyzed {
            let mut bits = [0u64; 2];
            for n in analyzer.active_notes() {
                bits[n as usize / 64] |= 1 << (n % 64);
            }
            shared.notes[0].store(bits[0], Ordering::Relaxed);
            shared.notes[1].store(bits[1], Ordering::Relaxed);
            shared.gain.store(analyzer.gain().to_bits(), Ordering::Relaxed);
            shared.level_db.store(analyzer.level_db().to_bits(), Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic(&out, &mut analyzer, &mut msgs);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 2048;

    fn sine(freq: f32, amp: f32, sr: u32, offset: usize, len: usize) -> Vec<f32> {
        (0..len)
            .map(|k| amp * ((offset + k) as f32 * freq * std::f32::consts::TAU / sr as f32).sin())
            .collect()
    }

    fn notes_on(out: &[Msg]) -> Vec<(u8, u8)> {
        out.iter().filter_map(|m| if let Msg::On { note, ch, .. } = m { Some((*note, *ch)) } else { None }).collect()
    }

    fn run(cfg: &Config, freq: f32, frames: usize) -> Vec<Msg> {
        let sr = 48_000;
        let mut a = Analyzer::new(sr, cfg.frame);
        let mut out = Vec::new();
        for k in 0..frames {
            let t = k as f64 * cfg.hop as f64 / sr as f64;
            a.frame(&sine(freq, 0.5, sr, k * cfg.hop, cfg.frame), t, cfg, &mut out);
        }
        out
    }

    #[test]
    fn detects_a440_and_releases() {
        let cfg = Config::default();
        let out = run(&cfg, 440.0, 4);
        let ons: Vec<u8> = notes_on(&out).into_iter().map(|(n, _)| n).collect();
        // McLeod and the strongest FFT peak both find A4.
        assert!(ons.contains(&69), "notes on: {ons:?}");
        // The level detector's note lives in the 48..64 range.
        assert!(ons.iter().any(|n| (48..64).contains(n)));
        // 10 ms notes are released by the next frame (21 ms later) unless retriggered.
        assert!(out.iter().any(|m| matches!(m, Msg::Off { .. })));

        let mut a = Analyzer::new(48_000, FRAME);
        let mut out = Vec::new();
        a.frame(&sine(440.0, 0.5, 48_000, 0, FRAME), 0.0, &cfg, &mut out);
        out.clear();
        a.release_all(&mut out);
        assert!(out.iter().all(|m| matches!(m, Msg::Off { .. })));
        assert_eq!(a.active_notes().count(), 0);
    }

    #[test]
    fn silence_sends_nothing() {
        let mut a = Analyzer::new(48_000, FRAME);
        let mut out = Vec::new();
        for k in 0..10 {
            a.frame(&[0.0; FRAME], k as f64 * 0.02, &Config::default(), &mut out);
        }
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn by_level_skips_the_drum_channel() {
        let chans: Vec<u8> = (0..=1000).map(|i| amplitude_to_channel(i as f32 / 1000.0)).collect();
        assert!(!chans.contains(&DRUM_CHANNEL));
        assert_eq!((chans[0], *chans.last().unwrap()), (0, 15));
        assert!(chans.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!((0..=1000).map(|i| level_step(i as f32 / 1000.0)).max(), Some(14));
    }

    #[test]
    fn note_and_bend_from_frequency() {
        assert_eq!(freq_to_note_bend(440.0, 2.0), (69, 0));
        let quarter = 440.0 * 2f32.powf(0.25 / 12.0);
        let (n, b) = freq_to_note_bend(quarter, 2.0);
        assert_eq!(n, 69);
        assert!((b - 1024).abs() <= 2, "bend {b}");
        // A wider bend range needs a smaller bend for the same offset.
        let (_, b12) = freq_to_note_bend(quarter, 12.0);
        assert!((b12 - 170).abs() <= 2, "bend {b12}");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4");
    }

    #[test]
    fn output_settings_shape_the_notes() {
        // Pitch detection only, on channel 3, an octave up, limited to A5.
        let cfg = Config {
            amplitude_notes: false,
            fft_peaks: false,
            pitch_channel: ChannelMode::Fixed(2),
            transpose: 12,
            min_note: 81,
            max_note: 81,
            ..Config::default()
        };
        let ons = notes_on(&run(&cfg, 440.0, 4));
        assert!(!ons.is_empty());
        assert!(ons.iter().all(|&(n, ch)| n == 81 && ch == 2), "{ons:?}");

        // Out of range notes are dropped.
        let cfg = Config { max_note: 60, ..cfg };
        assert!(notes_on(&run(&cfg, 440.0, 4)).is_empty());
    }

    #[test]
    fn frame_size_and_peak_limits() {
        // A bigger frame still finds A4; with one peak allowed only one FFT note per frame.
        let cfg = Config {
            amplitude_notes: false,
            pitch_detect: false,
            frame: 8192,
            hop: 4096,
            max_peaks: 1,
            note_ms: 1000.0,
            ..Config::default()
        };
        let ons = notes_on(&run(&cfg, 440.0, 3));
        assert_eq!(ons.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![69]);
        // Peaks outside the frequency window are ignored.
        let cfg = Config { peak_min_hz: 1000.0, ..cfg };
        assert!(notes_on(&run(&cfg, 440.0, 3)).is_empty());
    }

    #[test]
    fn manual_gain_sets_velocity() {
        let level_only = Config { auto_gain: false, gain_db: -6.0206, fft_peaks: false, pitch_detect: false, ..Config::default() };
        // Original formula: peak 0.5 at half gain = 0.25 → velocity 31, step 3 of the 15
        // melodic channels.
        let cfg = Config { velocity_mode: VelocityMode::Original, ..level_only };
        let out = run(&cfg, 440.0, 1);
        assert!(out.iter().any(|m| matches!(m, Msg::On { vel: 31, ch: 3, .. })), "{out:?}");
        // dB curve: a 0.25 sine is −12 dB, 48 of the 60 dB range → 1 + 126 × 0.8 = 102; the level (a hair
        // under 0.8) is step 11, channel index 12 past the drum channel.
        let out = run(&level_only, 440.0, 1);
        assert!(out.iter().any(|m| matches!(m, Msg::On { vel: 102, ch: 12, .. })), "{out:?}");
    }

    fn peak_velocity(cfg: &Config, amp: f32) -> u8 {
        let mut a = Analyzer::new(48_000, FRAME);
        let mut out = Vec::new();
        a.frame(&sine(440.0, amp, 48_000, 0, FRAME), 0.0, cfg, &mut out);
        out.iter().find_map(|m| if let Msg::On { vel, .. } = m { Some(*vel) } else { None }).unwrap_or(0)
    }

    #[test]
    fn decibel_mode_ignores_min_velocity() {
        // Settings tuned for the original mode (keep only 127s, 35 % volume) must not silence
        // the dB curve, whose velocities top out at 44 here.
        let cfg = Config { min_velocity: 126, volume_percent: 35.0, amplitude_notes: false, pitch_detect: false, ..Config::default() };
        let v = peak_velocity(&cfg, 0.5);
        assert!(v > 0 && v <= 45, "{v}");
        // In the original mode the same settings keep their meaning.
        let original = Config { velocity_mode: VelocityMode::Original, ..cfg };
        assert_eq!(peak_velocity(&original, 0.5), 127);
    }

    #[test]
    fn velocity_follows_loudness() {
        let peaks = Config { amplitude_notes: false, pitch_detect: false, ..Config::default() };
        let (loud, soft) = (peak_velocity(&peaks, 0.5), peak_velocity(&peaks, 0.05));
        // −6 dB → 1 + 126 × 54/60 = 114, −26 dB → 72 (FFT peak level reads the sine's amplitude).
        assert!((loud as i32 - 114).abs() <= 2 && (soft as i32 - 72).abs() <= 2, "{loud} {soft}");
        // Below the floor nothing is sent.
        assert_eq!(peak_velocity(&peaks, 0.0005), 0);
        // The original formula saturates both.
        let original = Config { velocity_mode: VelocityMode::Original, ..peaks };
        assert_eq!((peak_velocity(&original, 0.5), peak_velocity(&original, 0.05)), (127, 127));
        // A steeper curve gives softer velocities.
        let steep = Config { vel_curve: 2.0, ..peaks };
        assert!(peak_velocity(&steep, 0.05) < soft);
    }
}
