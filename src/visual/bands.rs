//! Band analyzer: the spectrum split into bands on a musical or perceptual scale, each band
//! measured by an FFT, a constant-Q transform, a sliding resonator (SWIFT) or an analog-style
//! band-pass filter, and drawn as bars, LED columns, a curve, or around a circle, with peak
//! caps that hold and then fall.

use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Stroke, Vec2, pos2, vec2};
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use super::spectral::{Channel, FFT_SIZES, WindowFn};
use super::{GRID, LABEL};
use crate::engine::nodes::Tap;

// ---------------------------------------------------------------------------
// Settings

/// How each band's level is measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Transform {
    /// One FFT; a band sums the bins inside it.
    Fft,
    /// A windowed DFT per band, its window as long as the band is narrow: fine detail in the
    /// bass and fast highs.
    ConstantQ,
    /// A chain of complex one-pole resonators per band, run on every sample.
    Swift,
    /// Band-pass filters with an RMS detector, as in a hardware analyzer.
    Analog,
}

impl Transform {
    pub const ALL: [Transform; 4] = [Self::ConstantQ, Self::Fft, Self::Swift, Self::Analog];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fft => "FFT",
            Self::ConstantQ => "Constant-Q",
            Self::Swift => "SWIFT",
            Self::Analog => "Analog filters",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Fft => "one FFT for all bands; the bass is coarse unless the FFT is large",
            Self::ConstantQ => "each band analyzed with a window fitting its width: sharp bass, quick treble",
            Self::Swift => "sliding resonators updated on every sample: smooth and very responsive",
            Self::Analog => "band-pass filters and an RMS detector, like a hardware analyzer",
        }
    }
}

/// How the frequency range is divided into bands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Scale {
    /// A fixed number of bands per octave, centered on the notes of the tuning.
    Notes,
    Log,
    Linear,
    /// Critical bands of hearing (Traunmüller's formula).
    Bark,
    /// Equivalent rectangular bandwidths (Glasberg and Moore).
    Erb,
    Mel,
}

impl Scale {
    pub const ALL: [Scale; 6] = [Self::Notes, Self::Log, Self::Linear, Self::Bark, Self::Erb, Self::Mel];

    pub fn label(self) -> &'static str {
        match self {
            Self::Notes => "Octave bands",
            Self::Log => "Logarithmic",
            Self::Linear => "Linear",
            Self::Bark => "Bark",
            Self::Erb => "ERB",
            Self::Mel => "Mel",
        }
    }

    /// Frequency to a position on the scale, where equal steps make equal bands.
    fn unit(self, f: f64) -> f64 {
        match self {
            Self::Notes | Self::Log => f.max(1e-3).ln(),
            Self::Linear => f,
            Self::Bark => 26.81 * f / (1960.0 + f) - 0.53,
            Self::Erb => 21.4 * (1.0 + 0.00437 * f).log10(),
            Self::Mel => 2595.0 * (1.0 + f / 700.0).log10(),
        }
    }

    fn freq_at(self, u: f64) -> f64 {
        match self {
            Self::Notes | Self::Log => u.exp(),
            Self::Linear => u,
            Self::Bark => 1960.0 * (u + 0.53) / (26.28 - u),
            Self::Erb => (10f64.powf(u / 21.4) - 1.0) / 0.00437,
            Self::Mel => 700.0 * (10f64.powf(u / 2595.0) - 1.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Look {
    Bars,
    Leds,
    Curve,
    RadialBars,
    RadialCurve,
}

impl Look {
    pub const ALL: [Look; 5] = [Self::Bars, Self::Leds, Self::Curve, Self::RadialBars, Self::RadialCurve];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bars => "Bars",
            Self::Leds => "LED bars",
            Self::Curve => "Curve",
            Self::RadialBars => "Radial bars",
            Self::RadialCurve => "Radial curve",
        }
    }

    fn radial(self) -> bool {
        matches!(self, Self::RadialBars | Self::RadialCurve)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Gradient {
    Classic,
    Prism,
    Turbo,
    Viridis,
    Magma,
    Gold,
    Ice,
    White,
}

impl Gradient {
    pub const ALL: [Gradient; 8] =
        [Self::Classic, Self::Prism, Self::Turbo, Self::Viridis, Self::Magma, Self::Gold, Self::Ice, Self::White];

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Prism => "Prism",
            Self::Turbo => "Turbo",
            Self::Viridis => "Viridis",
            Self::Magma => "Magma",
            Self::Gold => "Gold",
            Self::Ice => "Ice",
            Self::White => "White",
        }
    }

    fn stops(self) -> &'static [[u8; 3]] {
        match self {
            Self::Classic => &[[30, 170, 70], [60, 210, 80], [200, 220, 50], [250, 170, 40], [240, 60, 40]],
            Self::Prism => &[[150, 60, 230], [60, 90, 255], [40, 200, 230], [60, 220, 90], [240, 220, 50], [250, 120, 40], [235, 50, 60]],
            Self::Turbo => &[
                [48, 18, 59],
                [70, 107, 227],
                [40, 187, 236],
                [50, 241, 152],
                [164, 252, 60],
                [237, 208, 58],
                [251, 128, 34],
                [210, 48, 5],
                [122, 4, 3],
            ],
            Self::Viridis => &[[68, 1, 84], [59, 82, 139], [33, 145, 140], [94, 201, 98], [253, 231, 37]],
            Self::Magma => &[[20, 12, 50], [81, 18, 124], [183, 55, 121], [252, 137, 97], [252, 253, 191]],
            Self::Gold => &[[90, 50, 5], [180, 110, 10], [240, 185, 50], [255, 230, 150], [255, 250, 230]],
            Self::Ice => &[[20, 40, 110], [30, 110, 190], [90, 200, 230], [230, 250, 255]],
            Self::White => &[[150, 150, 150], [245, 245, 245]],
        }
    }

    /// Color at `t` in 0..=1.
    fn at(self, t: f32) -> Color32 {
        let stops = self.stops();
        let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
        let i = (t as usize).min(stops.len() - 2);
        let f = t - i as f32;
        let c = |k: usize| (stops[i][k] as f32 * (1.0 - f) + stops[i + 1][k] as f32 * f).round() as u8;
        Color32::from_rgb(c(0), c(1), c(2))
    }

    /// Where the stops sit in 0..=1, for drawing a gradient in straight pieces.
    fn positions(self) -> impl Iterator<Item = f32> {
        let n = self.stops().len() - 1;
        (0..=n).map(move |k| k as f32 / n as f32)
    }
}

/// What picks a band's color from the gradient.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ColorBy {
    Level,
    Frequency,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Labels {
    Notes,
    Hz,
    Off,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BandSettings {
    pub transform: Transform,
    pub scale: Scale,
    /// Bands per octave on the octave scale.
    pub per_octave: u32,
    /// Number of bands on the other scales.
    pub bands: u32,
    /// Tuning of A4, which the octave bands are centered on.
    pub a4: f32,
    pub min_hz: f32,
    pub max_hz: f32,
    pub channel: Channel,
    pub min_db: f32,
    pub max_db: f32,
    /// dB per octave, pivoting at 1 kHz.
    pub tilt: f32,
    pub fft_size: usize,
    pub window: WindowFn,
    /// How wide each band listens, relative to its own width (constant-Q, SWIFT, analog).
    pub width: f32,
    /// Resonators or filters in a row per band (SWIFT, analog): more = steeper skirts.
    pub order: u32,
    /// Detector time of the analog filters.
    pub analog_ms: f32,
    /// Time for bars to rise; 0 = at once.
    pub attack_ms: f32,
    /// How fast bars fall, dB/s.
    pub release: f32,
    pub peaks: bool,
    pub peak_hold_ms: f32,
    /// How fast a peak cap speeds up once it falls, dB/s².
    pub peak_gravity: f32,
    pub look: Look,
    pub gradient: Gradient,
    pub color_by: ColorBy,
    /// Space between bars, as a share of their pitch.
    pub gap: f32,
    /// Height of one LED, dB.
    pub led_db: f32,
    pub labels: Labels,
}

impl Default for BandSettings {
    fn default() -> Self {
        Self {
            transform: Transform::ConstantQ,
            scale: Scale::Notes,
            per_octave: 6,
            bands: 64,
            a4: 440.0,
            min_hz: 20.0,
            max_hz: 20_000.0,
            channel: Channel::Mid,
            min_db: -72.0,
            max_db: 0.0,
            tilt: 3.0,
            fft_size: 8192,
            window: WindowFn::Hann,
            width: 1.0,
            order: 2,
            analog_ms: 60.0,
            attack_ms: 0.0,
            release: 48.0,
            peaks: true,
            peak_hold_ms: 400.0,
            peak_gravity: 120.0,
            look: Look::Bars,
            gradient: Gradient::Classic,
            color_by: ColorBy::Level,
            gap: 0.25,
            led_db: 3.0,
            labels: Labels::Notes,
        }
    }
}

impl BandSettings {
    /// Keeps values usable whatever a saved file contained.
    fn sanitized(mut self, sample_rate: f32) -> Self {
        let nyquist = sample_rate * 0.5;
        self.max_hz = self.max_hz.clamp(200.0, nyquist * 0.999);
        let floor = if self.scale == Scale::Linear { 0.0 } else { 10.0 };
        self.min_hz = self.min_hz.clamp(floor, self.max_hz * 0.25);
        self.per_octave = self.per_octave.clamp(1, 48);
        self.bands = self.bands.clamp(4, 512);
        self.a4 = self.a4.clamp(400.0, 480.0);
        self.max_db = self.max_db.clamp(-120.0, 24.0);
        self.min_db = self.min_db.clamp(-180.0, self.max_db - 12.0);
        if !FFT_SIZES.contains(&self.fft_size) {
            self.fft_size = 8192;
        }
        self.width = self.width.clamp(0.25, 4.0);
        self.order = self.order.clamp(1, 4);
        self.analog_ms = self.analog_ms.clamp(5.0, 1000.0);
        self.gap = self.gap.clamp(0.0, 0.8);
        self.led_db = self.led_db.clamp(1.0, 12.0);
        if self.channel == Channel::Stereo {
            self.channel = Channel::Mid;
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Bands

#[derive(Clone, Copy, Debug)]
struct Band {
    lo: f64,
    hi: f64,
    center: f64,
}

const MAX_BANDS: usize = 512;

fn make_bands(cfg: &BandSettings, sample_rate: f32) -> Vec<Band> {
    let (min, max) = (cfg.min_hz as f64, cfg.max_hz as f64);
    let nyquist = sample_rate as f64 * 0.5;
    if cfg.scale == Scale::Notes {
        let ppo = cfg.per_octave as f64;
        let a4 = cfg.a4 as f64;
        let k0 = (ppo * (min / a4).log2()).ceil() as i64;
        let k1 = (ppo * (max / a4).log2()).floor() as i64;
        let half = 2f64.powf(0.5 / ppo);
        return (k0..=k1)
            .map(|k| {
                let center = a4 * 2f64.powf(k as f64 / ppo);
                Band { lo: center / half, hi: (center * half).min(nyquist), center }
            })
            .take(MAX_BANDS)
            .collect();
    }
    let (u0, u1) = (cfg.scale.unit(min), cfg.scale.unit(max));
    let n = cfg.bands as usize;
    let edge = |i: usize| cfg.scale.freq_at(u0 + (u1 - u0) * i as f64 / n as f64);
    (0..n)
        .map(|i| {
            let (lo, hi) = (edge(i), edge(i + 1));
            let center = cfg.scale.freq_at((cfg.scale.unit(lo) + cfg.scale.unit(hi)) * 0.5);
            Band { lo, hi, center }
        })
        .collect()
}

const NOTE_NAMES: [&str; 12] = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];

/// The nearest note to `f` and how far off it is in cents.
fn note_of(f: f64, a4: f64) -> (String, f64) {
    let midi = 69.0 + 12.0 * (f / a4).log2();
    let n = midi.round();
    let name = NOTE_NAMES[(n as i64).rem_euclid(12) as usize];
    (format!("{name}{}", (n as i64).div_euclid(12) - 1), (midi - n) * 100.0)
}

fn fmt_hz(f: f64) -> String {
    if f >= 1000.0 { format!("{:.2} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

// ---------------------------------------------------------------------------
// Analysis

/// What changes the analysis itself (rather than how it's shown).
#[derive(Clone, Copy, PartialEq, Debug)]
struct AnalysisKey {
    transform: Transform,
    scale: Scale,
    per_octave: u32,
    bands: u32,
    a4: f32,
    min_hz: f32,
    max_hz: f32,
    fft_size: usize,
    window: WindowFn,
    width: f32,
    order: u32,
    analog_ms: f32,
    sample_rate: f32,
}

impl AnalysisKey {
    fn of(cfg: &BandSettings, sample_rate: f32) -> Self {
        Self {
            transform: cfg.transform,
            scale: cfg.scale,
            per_octave: cfg.per_octave,
            bands: cfg.bands,
            a4: cfg.a4,
            min_hz: cfg.min_hz,
            max_hz: cfg.max_hz,
            fft_size: cfg.fft_size,
            window: cfg.window,
            width: cfg.width,
            order: cfg.order,
            analog_ms: cfg.analog_ms,
            sample_rate,
        }
    }
}

struct FftState {
    plan: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Scales |X|² so a full-scale sine's bin reads 1.
    norm: f32,
    /// Equivalent noise bandwidth in bins: a sine's power summed over its bins, which the
    /// band sums are divided by.
    enbw: f32,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    power: Vec<f32>,
}

impl FftState {
    fn new(size: usize, window_fn: WindowFn) -> Self {
        let plan = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        let window: Vec<f32> = (0..size).map(|k| window_fn.coeff(k, size)).collect();
        let sum: f32 = window.iter().sum();
        let sum_sq: f32 = window.iter().map(|w| w * w).sum();
        Self {
            input: plan.make_input_vec(),
            output: plan.make_output_vec(),
            scratch: plan.make_scratch_vec(),
            power: vec![0.0; size / 2 + 1],
            norm: (2.0 / sum).powi(2),
            enbw: size as f32 * sum_sq / (sum * sum),
            plan,
            window,
        }
    }

    /// Band powers (a full-scale sine reads 1) from the newest `window.len()` samples.
    fn run(&mut self, samples: &[f32], bands: &[Band], bin_hz: f64, out: &mut [f64]) {
        for ((x, s), w) in self.input.iter_mut().zip(samples).zip(&self.window) {
            *x = s * w;
        }
        if self.plan.process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch).is_err() {
            return;
        }
        for (p, c) in self.power.iter_mut().zip(&self.output) {
            *p = c.norm_sqr() * self.norm;
        }
        let last = self.power.len() - 1;
        for (b, o) in bands.iter().zip(out.iter_mut()) {
            // The bins inside the band, summed; and the spectrum read at the band's center,
            // for bands narrower than a bin.
            let i0 = ((b.lo / bin_hz).ceil() as usize).min(last + 1);
            let i1 = ((b.hi / bin_hz).ceil() as usize).min(last + 1);
            let sum: f32 = self.power[i0..i1.max(i0)].iter().sum();
            let c = (b.center / bin_hz).min(last as f64 - 1e-6);
            let i = c as usize;
            let t = (c - i as f64) as f32;
            let at = self.power[i] * (1.0 - t) + self.power[(i + 1).min(last)] * t;
            *o = (sum / self.enbw).max(at) as f64;
        }
    }
}

/// One band of the constant-Q transform: a Hann-windowed DFT at the band's center over the
/// newest `len` samples.
struct CqBand {
    len: usize,
    /// e^-iω per sample.
    step: Complex<f64>,
    /// e^i2π/len, which the window's cosine turns by.
    wstep: Complex<f64>,
}

impl CqBand {
    fn new(b: &Band, width: f64, sample_rate: f64) -> Self {
        // A Hann window of n samples is 1.44·fs/n wide at -3 dB.
        let bw = ((b.hi - b.lo) * width).max(0.1);
        let len = ((1.44 * sample_rate / bw).round() as usize).clamp(32, Tap::FRAMES);
        let w = TAU * b.center / sample_rate;
        Self { len, step: Complex::new(w.cos(), -w.sin()), wstep: Complex::new((TAU / len as f64).cos(), (TAU / len as f64).sin()) }
    }

    /// Power (a full-scale sine reads 1) of the newest `len` samples at the end of `x`.
    fn run(&self, x: &[f32]) -> f64 {
        let x = &x[x.len() - self.len..];
        let mut ph = Complex::new(1.0, 0.0);
        // The window's phasor starts half a sample in, so it is symmetric.
        let half = (PI / self.len as f64).sin_cos();
        let mut wph = Complex::new(half.1, half.0);
        let mut acc = Complex::new(0.0, 0.0);
        for (k, &s) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * wph.re;
            acc += ph * (s as f64 * w);
            ph *= self.step;
            wph *= self.wstep;
            // Keep the phasors on the unit circle over long windows.
            if k & 1023 == 1023 {
                ph /= ph.norm();
                wph /= wph.norm();
            }
        }
        // The window sums to len/2; a sine's amplitude is 2|X|/sum.
        let amp = 4.0 * acc.norm() / self.len as f64;
        amp * amp
    }
}

/// A band's chain of complex one-pole resonators, each with unity gain at the center.
struct Resonator {
    coef: Complex<f64>,
    gain: f64,
    state: [Complex<f64>; 4],
}

impl Resonator {
    fn new(b: &Band, width: f64, order: usize, sample_rate: f64) -> Self {
        // n stages narrow the -3 dB width by sqrt(2^(1/n) - 1); widen each to make up for it.
        let bw = ((b.hi - b.lo) * width).max(0.1) / (2f64.powf(1.0 / order as f64) - 1.0).sqrt();
        let r = (-PI * bw / sample_rate).exp();
        let w = TAU * b.center / sample_rate;
        Self { coef: Complex::from_polar(r, w), gain: 1.0 - r, state: [Complex::new(0.0, 0.0); 4] }
    }

    fn run(&mut self, x: &[f32], order: usize) -> f64 {
        for &s in x {
            let mut v = Complex::new(s as f64, 0.0);
            for st in &mut self.state[..order] {
                *st = v * self.gain + *st * self.coef;
                v = *st;
            }
        }
        // The resonators keep the positive-frequency half of a real sine.
        let amp = 2.0 * self.state[order - 1].norm();
        amp * amp
    }
}

/// A band's band-pass filters (constant 0 dB peak) and its RMS detector.
struct AnalogBand {
    b0: f64,
    a1: f64,
    a2: f64,
    /// Per stage: the last two inputs and outputs.
    z: [[f64; 4]; 4],
    /// Detector smoothing per sample, and the mean square it tracks.
    k: f64,
    ms: f64,
}

impl AnalogBand {
    fn new(b: &Band, width: f64, order: usize, detector_ms: f64, sample_rate: f64) -> Self {
        let bw = ((b.hi - b.lo) * width).max(0.1) / (2f64.powf(1.0 / order as f64) - 1.0).sqrt();
        let q = (b.center / bw).max(0.3);
        let w0 = TAU * b.center / sample_rate;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        // Low bands need a few periods to settle, whatever the detector time.
        let tau = (detector_ms / 1000.0).max(2.0 / b.center);
        Self {
            b0: alpha / a0,
            a1: -2.0 * w0.cos() / a0,
            a2: (1.0 - alpha) / a0,
            z: [[0.0; 4]; 4],
            k: 1.0 - (-1.0 / (tau * sample_rate)).exp(),
            ms: 0.0,
        }
    }

    fn run(&mut self, x: &[f32], order: usize) -> f64 {
        for &s in x {
            let mut v = s as f64;
            for z in &mut self.z[..order] {
                // y = b0·(x - x2) - a1·y1 - a2·y2
                let y = self.b0 * (v - z[1]) - self.a1 * z[2] - self.a2 * z[3];
                *z = [v, z[0], y, z[2]];
                v = y;
            }
            self.ms += (v * v - self.ms) * self.k;
        }
        // A sine's mean square is half its peak squared.
        2.0 * self.ms
    }
}

enum Analysis {
    Fft(Box<FftState>),
    ConstantQ(Vec<CqBand>),
    Swift(Vec<Resonator>),
    Analog(Vec<AnalogBand>),
}

// ---------------------------------------------------------------------------
// The analyzer

pub struct BandAnalyzer {
    pub cfg: BandSettings,
    pub show_settings: bool,
    key: Option<AnalysisKey>,
    bands: Vec<Band>,
    analysis: Analysis,
    /// Tap position the streaming analyses have got to.
    pos: u64,
    l: Vec<f32>,
    r: Vec<f32>,
    mono: Vec<f32>,
    power: Vec<f64>,
    /// Shown level per band, dB, and its peak cap: level, time held, fall speed.
    shown: Vec<f32>,
    peak: Vec<(f32, f32, f32)>,
}

impl BandAnalyzer {
    pub fn new(cfg: BandSettings) -> Self {
        Self {
            cfg,
            show_settings: false,
            key: None,
            bands: Vec::new(),
            analysis: Analysis::ConstantQ(Vec::new()),
            pos: 0,
            l: Vec::new(),
            r: Vec::new(),
            mono: Vec::new(),
            power: Vec::new(),
            shown: Vec::new(),
            peak: Vec::new(),
        }
    }

    fn rebuild(&mut self, cfg: &BandSettings, sample_rate: f32) {
        let key = AnalysisKey::of(cfg, sample_rate);
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.bands = make_bands(cfg, sample_rate);
        let (sr, width, order) = (sample_rate as f64, cfg.width as f64, cfg.order as usize);
        self.analysis = match cfg.transform {
            Transform::Fft => Analysis::Fft(Box::new(FftState::new(cfg.fft_size, cfg.window))),
            Transform::ConstantQ => Analysis::ConstantQ(self.bands.iter().map(|b| CqBand::new(b, width, sr)).collect()),
            Transform::Swift => Analysis::Swift(self.bands.iter().map(|b| Resonator::new(b, width, order, sr)).collect()),
            Transform::Analog => Analysis::Analog(
                self.bands.iter().map(|b| AnalogBand::new(b, width, order, cfg.analog_ms as f64, sr)).collect(),
            ),
        };
        let n = self.bands.len();
        self.power = vec![0.0; n];
        self.shown = vec![cfg.min_db; n];
        self.peak = vec![(cfg.min_db, 0.0, 0.0); n];
        self.pos = 0;
    }

    /// Reads what the tap recorded since the last frame and moves the bars and peak caps on.
    pub fn update(&mut self, tap: &Tap, sample_rate: f32, dt: f32) {
        let cfg = self.cfg.sanitized(sample_rate);
        self.rebuild(&cfg, sample_rate);
        let written = tap.written();
        let channel = cfg.channel;
        match &mut self.analysis {
            Analysis::Fft(fft) => {
                let n = fft.window.len();
                read_mono(tap, written, n, channel, &mut self.l, &mut self.r, &mut self.mono);
                fft.run(&self.mono, &self.bands, sample_rate as f64 / n as f64, &mut self.power);
            }
            Analysis::ConstantQ(cq) => {
                let n = cq.iter().map(|b| b.len).max().unwrap_or(0);
                read_mono(tap, written, n, channel, &mut self.l, &mut self.r, &mut self.mono);
                for (b, p) in cq.iter().zip(&mut self.power) {
                    *p = b.run(&self.mono);
                }
            }
            Analysis::Swift(_) | Analysis::Analog(_) => {
                // Every sample goes through; after a pause, half a second is enough to settle.
                let most = (sample_rate as u64 / 2).min(Tap::FRAMES as u64 / 2);
                if written < self.pos || written - self.pos > most {
                    self.pos = written.saturating_sub(most);
                }
                let n = (written - self.pos) as usize;
                self.pos = written;
                if n > 0 {
                    read_mono(tap, written, n, channel, &mut self.l, &mut self.r, &mut self.mono);
                    let order = cfg.order as usize;
                    match &mut self.analysis {
                        Analysis::Swift(res) => {
                            for (b, p) in res.iter_mut().zip(&mut self.power) {
                                *p = b.run(&self.mono, order);
                            }
                        }
                        Analysis::Analog(filters) => {
                            for (b, p) in filters.iter_mut().zip(&mut self.power) {
                                *p = b.run(&self.mono, order);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Bars rise over the attack time and fall at the release rate; peak caps hold, then
        // fall faster and faster.
        let rise = if cfg.attack_ms <= 0.0 { 1.0 } else { 1.0 - (-dt * 1000.0 / cfg.attack_ms).exp() };
        let hold = cfg.peak_hold_ms / 1000.0;
        for ((b, p), (s, pk)) in self.bands.iter().zip(&self.power).zip(self.shown.iter_mut().zip(&mut self.peak)) {
            let db = (10.0 * (p + 1e-20).log10()) as f32 + tilt_db(cfg.tilt, b.center);
            let db = db.max(cfg.min_db - 1.0);
            *s = if db > *s { *s + (db - *s) * rise } else { db.max(*s - cfg.release * dt) };
            if *s >= pk.0 {
                *pk = (*s, 0.0, 0.0);
            } else {
                pk.1 += dt;
                if pk.1 > hold {
                    pk.2 += cfg.peak_gravity * dt;
                    pk.0 = (pk.0 - pk.2 * dt).max(*s);
                }
            }
        }
    }

    pub fn draw(&self, painter: &egui::Painter, rect: Rect, sample_rate: f32, hover: Option<Pos2>) {
        let cfg = self.cfg.sanitized(sample_rate);
        let n = self.bands.len().min(self.shown.len());
        if n == 0 {
            return;
        }
        let range = cfg.max_db - cfg.min_db;
        let level = |db: f32| ((db - cfg.min_db) / range).clamp(0.0, 1.0);
        let levels: Vec<f32> = self.shown[..n].iter().map(|d| level(*d)).collect();
        let peaks: Vec<f32> = self.peak[..n].iter().map(|p| level(p.0)).collect();
        let freq_t = |i: usize| if n > 1 { i as f32 / (n - 1) as f32 } else { 0.5 };
        let look = Drawing { cfg: &cfg, painter, n };
        if cfg.look.radial() {
            look.radial(rect, &levels, &peaks, freq_t);
        } else {
            let plot = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - if cfg.labels == Labels::Off { 0.0 } else { 12.0 }));
            look.db_grid(plot, false);
            match cfg.look {
                Look::Bars | Look::Leds => look.bars(plot, &levels, &peaks, freq_t),
                _ => look.curve(plot, &levels, &peaks, freq_t),
            }
            look.db_grid(plot, true);
            self.axis(painter, rect, plot, &cfg);
        }
        if let Some(h) = hover.filter(|h| rect.contains(*h))
            && let Some(i) = self.band_at(rect, h, &cfg)
        {
            let b = &self.bands[i];
            let (note, cents) = note_of(b.center, cfg.a4 as f64);
            let cents = if cents.abs() < 0.5 { String::new() } else { format!(" {cents:+.0}¢") };
            painter.text(
                pos2(rect.right() - 4.0, rect.top() + 2.0),
                Align2::RIGHT_TOP,
                format!("{note}{cents}  {}  {:.1} dB", fmt_hz(b.center), self.shown[i]),
                FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
    }

    /// The band under `p`.
    fn band_at(&self, rect: Rect, p: Pos2, cfg: &BandSettings) -> Option<usize> {
        let n = self.bands.len();
        if cfg.look.radial() {
            let d = p - rect.center();
            if d.length() < 4.0 {
                return None;
            }
            // Clockwise from the top.
            let a = (d.x.atan2(-d.y) / std::f32::consts::TAU).rem_euclid(1.0);
            Some(((a * n as f32) as usize).min(n - 1))
        } else {
            let t = (p.x - rect.left()) / rect.width();
            Some(((t * n as f32) as usize).min(n - 1))
        }
    }

    /// Note or frequency labels under the bars, placed through the bands.
    fn axis(&self, painter: &egui::Painter, rect: Rect, plot: Rect, cfg: &BandSettings) {
        let n = self.bands.len();
        let x_of = |f: f64| {
            let i = self.bands.iter().position(|b| f < b.hi)?;
            let b = &self.bands[i];
            if f < b.lo {
                return None;
            }
            let t = cfg.scale.unit(f) - cfg.scale.unit(b.lo);
            let t = t / (cfg.scale.unit(b.hi) - cfg.scale.unit(b.lo));
            Some(plot.left() + (i as f32 + t as f32) / n as f32 * plot.width())
        };
        let marks: Vec<(f64, String)> = match cfg.labels {
            Labels::Off => return,
            Labels::Notes => (0..=9)
                .map(|oct| (cfg.a4 as f64 * 2f64.powf((12.0 * oct as f64 - 57.0) / 12.0), format!("C{oct}")))
                .collect(),
            Labels::Hz => [20.0, 50.0, 100.0, 200.0, 500.0, 1e3, 2e3, 5e3, 1e4, 2e4]
                .into_iter()
                .map(|f| (f, if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") }))
                .collect(),
        };
        let font = FontId::proportional(9.0);
        let mut last = f32::MIN;
        for (f, label) in marks {
            let Some(x) = x_of(f) else { continue };
            if x - last < 22.0 {
                continue;
            }
            last = x;
            painter.line_segment([pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)], Stroke::new(1.0, LABEL));
            painter.text(pos2(x, rect.bottom()), Align2::CENTER_BOTTOM, label, font.clone(), LABEL);
        }
    }
}

fn tilt_db(tilt: f32, freq: f64) -> f32 {
    if tilt == 0.0 { 0.0 } else { tilt * (freq.max(1.0) / 1000.0).log2() as f32 }
}

/// Fills `mono` with the `n` frames before `end`, mixed down to `channel`.
fn read_mono(tap: &Tap, end: u64, n: usize, channel: Channel, l: &mut Vec<f32>, r: &mut Vec<f32>, mono: &mut Vec<f32>) {
    l.resize(n, 0.0);
    r.resize(n, 0.0);
    mono.resize(n, 0.0);
    tap.read(end, l, r);
    channel.mix(l, r, mono);
}

// ---------------------------------------------------------------------------
// Drawing

/// What drawing needs, gathered.
struct Drawing<'a> {
    cfg: &'a BandSettings,
    painter: &'a egui::Painter,
    n: usize,
}

const UNLIT: Color32 = Color32::from_gray(26);
const PEAK: Color32 = Color32::from_gray(235);

impl Drawing<'_> {
    fn color(&self, level: f32, band_t: f32) -> Color32 {
        match self.cfg.color_by {
            ColorBy::Level => self.cfg.gradient.at(level),
            ColorBy::Frequency => self.cfg.gradient.at(band_t),
        }
    }

    /// The dB lines, behind the bars, or their labels, in front.
    fn db_grid(&self, plot: Rect, labels: bool) {
        let (min, max) = (self.cfg.min_db, self.cfg.max_db);
        let range = max - min;
        let step = [6.0, 12.0, 24.0, 48.0].into_iter().find(|s| range / s <= 7.0).unwrap_or(60.0);
        let font = FontId::proportional(9.0);
        let mut db = (max / step).floor() * step;
        while db > min {
            let y = plot.bottom() - (db - min) / range * plot.height();
            if labels {
                self.painter.text(pos2(plot.right() - 2.0, y + 1.0), Align2::RIGHT_TOP, format!("{db:.0}"), font.clone(), LABEL);
            } else {
                self.painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, GRID));
            }
            db -= step;
        }
    }

    /// A column from `bottom` up to `level` of `full`: one color by frequency, or the
    /// gradient's lower part by level, drawn in straight pieces between its stops.
    fn column(&self, mesh: &mut Mesh, (x0, x1): (f32, f32), bottom: f32, full: f32, level: f32, band_t: f32) {
        if level <= 0.0 {
            return;
        }
        let y_of = |t: f32| bottom - t * full;
        match self.cfg.color_by {
            ColorBy::Frequency => {
                let c = self.cfg.gradient.at(band_t);
                quad(mesh, [pos2(x0, y_of(level)), pos2(x1, y_of(level)), pos2(x1, bottom), pos2(x0, bottom)], [c, c, c, c]);
            }
            ColorBy::Level => {
                let mut a = 0.0;
                for b in self.cfg.gradient.positions().skip(1).chain([level]) {
                    let b = b.min(level);
                    if b > a {
                        let (ca, cb) = (self.cfg.gradient.at(a), self.cfg.gradient.at(b));
                        quad(mesh, [pos2(x0, y_of(b)), pos2(x1, y_of(b)), pos2(x1, y_of(a)), pos2(x0, y_of(a))], [cb, cb, ca, ca]);
                        a = b;
                    }
                }
            }
        }
    }

    fn bars(&self, plot: Rect, levels: &[f32], peaks: &[f32], band_t: impl Fn(usize) -> f32) {
        let pitch = plot.width() / self.n as f32;
        let gap = (pitch * self.cfg.gap).min(pitch - 1.0).max(0.0);
        let mut mesh = Mesh::default();
        let leds = self.cfg.look == Look::Leds;
        let segs = ((self.cfg.max_db - self.cfg.min_db) / self.cfg.led_db).round().max(1.0) as usize;
        let seg_h = plot.height() / segs as f32;
        for (i, (&lv, &pk)) in levels.iter().zip(peaks).enumerate() {
            let x0 = plot.left() + i as f32 * pitch + gap * 0.5;
            let x1 = x0 + pitch - gap;
            let bt = band_t(i);
            if leds {
                // Lit segments take the color of their own height.
                for s in 0..segs {
                    let lo = s as f32 / segs as f32;
                    let y1 = plot.bottom() - s as f32 * seg_h;
                    let y0 = y1 - seg_h + seg_h.min(3.0) * 0.4;
                    let c = if lv > lo + 0.5 / segs as f32 { self.color(lo + 0.5 / segs as f32, bt) } else { UNLIT };
                    quad(&mut mesh, [pos2(x0, y0), pos2(x1, y0), pos2(x1, y1), pos2(x0, y1)], [c; 4]);
                }
            } else {
                self.column(&mut mesh, (x0, x1), plot.bottom(), plot.height(), lv, bt);
            }
            if self.cfg.peaks && pk > 0.0 {
                let y = plot.bottom() - pk * plot.height();
                let y = if leds { plot.bottom() - (pk * segs as f32).floor().min(segs as f32 - 1.0) * seg_h - seg_h * 0.5 } else { y };
                let h = if leds { seg_h * 0.6 } else { 2.0 };
                quad(&mut mesh, [pos2(x0, y - h * 0.5), pos2(x1, y - h * 0.5), pos2(x1, y + h * 0.5), pos2(x0, y + h * 0.5)], [PEAK; 4]);
            }
        }
        self.painter.add(egui::Shape::mesh(mesh));
    }

    fn curve(&self, plot: Rect, levels: &[f32], peaks: &[f32], band_t: impl Fn(usize) -> f32) {
        let pitch = plot.width() / self.n as f32;
        let at = |i: usize, t: f32| pos2(plot.left() + (i as f32 + 0.5) * pitch, plot.bottom() - t * plot.height());
        let pts: Vec<Pos2> = levels.iter().enumerate().map(|(i, t)| at(i, *t)).collect();
        let smooth = catmull_rom(&pts, 4);
        // Fill under the curve, fading toward the bottom: by level, the gradient runs up the
        // plot (in pieces between its stops); by frequency, each piece takes its band's color.
        let mut mesh = Mesh::default();
        let y_of = |t: f32| plot.bottom() - t * plot.height();
        let fill = |t: f32| self.cfg.gradient.at(t).gamma_multiply(0.08 + 0.5 * t);
        for w in smooth.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (ta, tb) = ((plot.bottom() - a.y) / plot.height(), (plot.bottom() - b.y) / plot.height());
            match self.cfg.color_by {
                ColorBy::Level => {
                    let top = ta.min(tb).max(0.0);
                    let mut lo = 0.0;
                    for s in self.cfg.gradient.positions().skip(1).chain([top]) {
                        let s = s.min(top);
                        if s > lo {
                            quad(&mut mesh, [pos2(a.x, y_of(s)), pos2(b.x, y_of(s)), pos2(b.x, y_of(lo)), pos2(a.x, y_of(lo))], [fill(s), fill(s), fill(lo), fill(lo)]);
                            lo = s;
                        }
                    }
                    quad(&mut mesh, [a, b, pos2(b.x, y_of(top)), pos2(a.x, y_of(top))], [fill(ta), fill(tb), fill(top), fill(top)]);
                }
                ColorBy::Frequency => {
                    let band = |x: f32| band_t((((x - plot.left()) / pitch - 0.5).round().max(0.0) as usize).min(self.n - 1));
                    let (ca, cb) = (self.cfg.gradient.at(band(a.x)), self.cfg.gradient.at(band(b.x)));
                    let fade = |c: Color32| c.gamma_multiply(0.08);
                    quad(&mut mesh, [a, b, pos2(b.x, plot.bottom()), pos2(a.x, plot.bottom())], [ca.gamma_multiply(0.55), cb.gamma_multiply(0.55), fade(cb), fade(ca)]);
                }
            }
        }
        self.painter.add(egui::Shape::mesh(mesh));
        for w in smooth.windows(2) {
            let t = (plot.bottom() - (w[0].y + w[1].y) * 0.5) / plot.height();
            let i = (((w[0].x - plot.left()) / pitch - 0.5).round().max(0.0) as usize).min(self.n - 1);
            self.painter.line_segment([w[0], w[1]], Stroke::new(1.5, self.color(t, band_t(i))));
        }
        if self.cfg.peaks {
            let peak_pts: Vec<Pos2> = peaks.iter().enumerate().map(|(i, t)| at(i, *t)).collect();
            self.painter.add(egui::Shape::line(catmull_rom(&peak_pts, 4), Stroke::new(1.0, PEAK.gamma_multiply(0.6))));
        }
    }

    fn radial(&self, rect: Rect, levels: &[f32], peaks: &[f32], band_t: impl Fn(usize) -> f32) {
        let c = rect.center();
        let outer = (rect.width().min(rect.height()) * 0.5 - 4.0).max(8.0);
        let inner = outer * 0.25;
        let span = outer - inner;
        let n = self.n as f32;
        let dir = |a: f32| {
            let a = a * std::f32::consts::TAU;
            vec2(a.sin(), -a.cos())
        };
        // Rings at a few levels, and the inner circle.
        for k in 0..=3 {
            self.painter.circle_stroke(c, inner + span * k as f32 / 3.0, Stroke::new(1.0, GRID));
        }
        let mut mesh = Mesh::default();
        match self.cfg.look {
            Look::RadialBars => {
                let half = 0.5 * (1.0 - self.cfg.gap) / n;
                for (i, (&lv, &pk)) in levels.iter().zip(peaks).enumerate() {
                    let mid = (i as f32 + 0.5) / n;
                    let (d0, d1) = (dir(mid - half), dir(mid + half));
                    let bt = band_t(i);
                    // In pieces along the radius so a level gradient reads outward.
                    let pieces: Vec<f32> = match self.cfg.color_by {
                        ColorBy::Frequency => vec![0.0, lv],
                        ColorBy::Level => std::iter::once(0.0)
                            .chain(self.cfg.gradient.positions().skip(1).filter(|p| *p < lv))
                            .chain([lv])
                            .collect(),
                    };
                    for w in pieces.windows(2) {
                        let (ra, rb) = (inner + w[0] * span, inner + w[1] * span);
                        let (ca, cb) = (self.color(w[0], bt), self.color(w[1], bt));
                        quad(&mut mesh, [c + d0 * rb, c + d1 * rb, c + d1 * ra, c + d0 * ra], [cb, cb, ca, ca]);
                    }
                    if self.cfg.peaks && pk > 0.0 {
                        let (ra, rb) = (inner + pk * span - 1.0, inner + pk * span + 1.0);
                        quad(&mut mesh, [c + d0 * rb, c + d1 * rb, c + d1 * ra, c + d0 * ra], [PEAK; 4]);
                    }
                }
            }
            _ => {
                let at = |i: usize, t: f32| c + dir((i as f32 + 0.5) / n) * (inner + t * span);
                let mut pts: Vec<Pos2> = levels.iter().enumerate().map(|(i, t)| at(i, *t)).collect();
                pts.push(pts[0]);
                for (i, w) in pts.windows(2).enumerate() {
                    let (ia, ib) = (i, (i + 1) % self.n);
                    let (ca, cb) = (self.color(levels[ia], band_t(ia)), self.color(levels[ib], band_t(ib)));
                    let (ra, rb) = (c + dir((ia as f32 + 0.5) / n) * inner, c + dir((ib as f32 + 0.5) / n) * inner);
                    quad(&mut mesh, [w[0], w[1], rb, ra], [ca.gamma_multiply(0.6), cb.gamma_multiply(0.6), cb.gamma_multiply(0.08), ca.gamma_multiply(0.08)]);
                    self.painter.line_segment([w[0], w[1]], Stroke::new(1.5, ca));
                }
                if self.cfg.peaks {
                    let mut pk: Vec<Pos2> = peaks.iter().enumerate().map(|(i, t)| at(i, *t)).collect();
                    pk.push(pk[0]);
                    self.painter.add(egui::Shape::line(pk, Stroke::new(1.0, PEAK.gamma_multiply(0.6))));
                }
            }
        }
        self.painter.add(egui::Shape::mesh(mesh));
    }
}

fn quad(mesh: &mut Mesh, p: [Pos2; 4], c: [Color32; 4]) {
    let i = mesh.vertices.len() as u32;
    for (p, c) in p.into_iter().zip(c) {
        mesh.colored_vertex(p, c);
    }
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i, i + 2, i + 3);
}

/// A smooth line through `pts` (centripetal-free uniform Catmull-Rom), `steps` pieces per span.
fn catmull_rom(pts: &[Pos2], steps: usize) -> Vec<Pos2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out = Vec::with_capacity(pts.len() * steps);
    let p = |i: isize| pts[i.clamp(0, pts.len() as isize - 1) as usize].to_vec2();
    for i in 0..pts.len() as isize - 1 {
        let (p0, p1, p2, p3) = (p(i - 1), p(i), p(i + 1), p(i + 2));
        for s in 0..steps {
            let t = s as f32 / steps as f32;
            let (t2, t3) = (t * t, t * t * t);
            let v: Vec2 = 0.5
                * ((2.0 * p1) + (p2 - p0) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2 + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3);
            out.push(v.to_pos2());
        }
    }
    out.push(*pts.last().unwrap());
    out
}

// ---------------------------------------------------------------------------
// Settings UI

pub fn settings_ui(ui: &mut egui::Ui, cfg: &mut BandSettings, open: &mut bool, sample_rate: f32) {
    let combo = |ui: &mut egui::Ui, id: &str, width: f32, text: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
        egui::ComboBox::from_id_salt(ui.id().with(id)).width(width).selected_text(text).show_ui(ui, |ui| add(ui));
    };
    ui.horizontal(|ui| {
        ui.toggle_value(open, "⚙").on_hover_text("analyzer settings");
        combo(ui, "transform", 92.0, cfg.transform.label(), &mut |ui| {
            for t in Transform::ALL {
                ui.selectable_value(&mut cfg.transform, t, t.label()).on_hover_text(t.hint());
            }
        });
        combo(ui, "look", 84.0, cfg.look.label(), &mut |ui| {
            for l in Look::ALL {
                ui.selectable_value(&mut cfg.look, l, l.label());
            }
        });
        ui.toggle_value(&mut cfg.peaks, "peaks");
    });
    if !*open {
        return;
    }
    let nyquist = sample_rate * 0.5;
    egui::Grid::new(ui.id().with("grid")).num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| {
        ui.label("Bands");
        ui.horizontal(|ui| {
            combo(ui, "scale", 90.0, cfg.scale.label(), &mut |ui| {
                for s in Scale::ALL {
                    ui.selectable_value(&mut cfg.scale, s, s.label());
                }
            });
            if cfg.scale == Scale::Notes {
                ui.add(egui::DragValue::new(&mut cfg.per_octave).range(1..=48).prefix("1/").suffix(" oct"))
                    .on_hover_text("bands per octave; 12 gives one band per semitone");
                ui.add(egui::DragValue::new(&mut cfg.a4).range(400.0..=480.0).speed(0.1).prefix("A4 ").suffix(" Hz"));
            } else {
                ui.add(egui::DragValue::new(&mut cfg.bands).range(4..=512).suffix(" bands"));
            }
        });
        ui.end_row();

        ui.label("Frequency");
        ui.horizontal(|ui| {
            let max_hz = cfg.max_hz;
            ui.add(egui::DragValue::new(&mut cfg.min_hz).range(0.0..=max_hz * 0.25).speed(1.0).suffix(" Hz"));
            ui.label("–");
            let min_hz = cfg.min_hz;
            ui.add(egui::DragValue::new(&mut cfg.max_hz).range((min_hz * 4.0).max(200.0)..=nyquist).speed(10.0).suffix(" Hz"));
        });
        ui.end_row();

        ui.label("Channel");
        combo(ui, "ch", 90.0, cfg.channel.label(), &mut |ui| {
            for c in Channel::ALL.into_iter().filter(|c| *c != Channel::Stereo) {
                ui.selectable_value(&mut cfg.channel, c, c.label());
            }
        });
        ui.end_row();

        match cfg.transform {
            Transform::Fft => {
                ui.label("FFT");
                ui.horizontal(|ui| {
                    combo(ui, "size", 64.0, &cfg.fft_size.to_string(), &mut |ui| {
                        for n in FFT_SIZES {
                            ui.selectable_value(&mut cfg.fft_size, n, format!("{n}  ({:.1} Hz/bin)", sample_rate / n as f32));
                        }
                    });
                    combo(ui, "win", 100.0, cfg.window.label(), &mut |ui| {
                        for w in WindowFn::ALL {
                            ui.selectable_value(&mut cfg.window, w, w.label());
                        }
                    });
                });
                ui.end_row();
            }
            t => {
                ui.label("Band width");
                ui.add(egui::Slider::new(&mut cfg.width, 0.25..=4.0).logarithmic(true).suffix("×"))
                    .on_hover_text("how wide each band listens: narrower is sharper but slower");
                ui.end_row();
                if t != Transform::ConstantQ {
                    ui.label("Order");
                    ui.add(egui::Slider::new(&mut cfg.order, 1..=4))
                        .on_hover_text("filters per band in a row: higher keeps neighboring bands apart");
                    ui.end_row();
                }
                if t == Transform::Analog {
                    ui.label("Detector");
                    ui.add(egui::Slider::new(&mut cfg.analog_ms, 5.0..=1000.0).logarithmic(true).suffix(" ms"));
                    ui.end_row();
                }
            }
        }

        ui.label("Level range");
        ui.horizontal(|ui| {
            let max_db = cfg.max_db;
            ui.add(egui::DragValue::new(&mut cfg.min_db).range(-180.0..=max_db - 12.0).speed(0.5).suffix(" dB"));
            ui.label("–");
            let min_db = cfg.min_db;
            ui.add(egui::DragValue::new(&mut cfg.max_db).range(min_db + 12.0..=24.0).speed(0.5).suffix(" dB"));
        });
        ui.end_row();

        ui.label("Tilt");
        ui.add(egui::Slider::new(&mut cfg.tilt, -6.0..=9.0).step_by(0.5).suffix(" dB/oct"))
            .on_hover_text("pivots at 1 kHz; 3 dB/oct shows pink noise flat");
        ui.end_row();

        ui.label("Rise / fall");
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut cfg.attack_ms).range(0.0..=500.0).speed(1.0).suffix(" ms"))
                .on_hover_text("time to rise; 0 = at once");
            ui.add(egui::DragValue::new(&mut cfg.release).range(1.0..=500.0).speed(1.0).suffix(" dB/s"))
                .on_hover_text("how fast bars fall");
        });
        ui.end_row();

        ui.label("Peak caps");
        ui.add_enabled_ui(cfg.peaks, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut cfg.peak_hold_ms).range(0.0..=5000.0).speed(5.0).prefix("hold ").suffix(" ms"));
                ui.add(egui::DragValue::new(&mut cfg.peak_gravity).range(1.0..=2000.0).speed(2.0).prefix("fall ").suffix(" dB/s²"));
            });
        });
        ui.end_row();

        ui.label("Colors");
        ui.horizontal(|ui| {
            combo(ui, "grad", 70.0, cfg.gradient.label(), &mut |ui| {
                for g in Gradient::ALL {
                    ui.selectable_value(&mut cfg.gradient, g, g.label());
                }
            });
            ui.selectable_value(&mut cfg.color_by, ColorBy::Level, "by level");
            ui.selectable_value(&mut cfg.color_by, ColorBy::Frequency, "by frequency");
        });
        ui.end_row();

        ui.label("Bars");
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut cfg.gap, 0.0..=0.8).text("gap"));
            if cfg.look == Look::Leds {
                ui.add(egui::DragValue::new(&mut cfg.led_db).range(1.0..=12.0).speed(0.1).suffix(" dB/LED"));
            }
        });
        ui.end_row();

        ui.label("Labels");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut cfg.labels, Labels::Notes, "notes");
            ui.selectable_value(&mut cfg.labels, Labels::Hz, "Hz");
            ui.selectable_value(&mut cfg.labels, Labels::Off, "off");
        });
        ui.end_row();
    });
    if ui.small_button("Reset to defaults").clicked() {
        *cfg = BandSettings::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(freq: f64, n: usize) -> Vec<f32> {
        (0..n).map(|k| (TAU * freq * k as f64 / SR as f64).sin() as f32).collect()
    }

    /// The level each band reads for a steady full-scale sine, through a tap.
    fn levels(cfg: BandSettings, freq: f64) -> (Vec<Band>, Vec<f32>) {
        let tap = Tap::new();
        let mut an = BandAnalyzer::new(BandSettings { tilt: 0.0, release: 1e6, attack_ms: 0.0, ..cfg });
        let s = sine(freq, Tap::FRAMES);
        for chunk in s.chunks(4096) {
            tap.push(chunk, chunk);
            an.update(&tap, SR, 0.02);
        }
        (an.bands.clone(), an.shown.clone())
    }

    #[test]
    fn octave_bands_sit_on_the_notes() {
        let cfg = BandSettings { per_octave: 12, ..BandSettings::default() }.sanitized(SR);
        let bands = make_bands(&cfg, SR);
        let a4 = bands.iter().find(|b| (b.center - 440.0).abs() < 1e-6).expect("a band on A4");
        assert!((a4.hi / a4.lo - 2f64.powf(1.0 / 12.0)).abs() < 1e-9);
        assert_eq!(note_of(261.6256, 440.0).0, "C4");
        // Bands follow each other without gaps.
        assert!(bands.windows(2).all(|w| (w[0].hi - w[1].lo).abs() < 1e-6));
    }

    #[test]
    fn scales_round_trip() {
        for s in Scale::ALL {
            for f in [30.0, 440.0, 12_000.0] {
                assert!((s.freq_at(s.unit(f)) - f).abs() < 1e-6 * f, "{s:?} {f}");
            }
            let cfg = BandSettings { scale: s, bands: 40, ..BandSettings::default() }.sanitized(SR);
            let bands = make_bands(&cfg, SR);
            assert_eq!(bands.len(), if s == Scale::Notes { 60 } else { 40 }, "{s:?}");
            assert!(bands.iter().all(|b| b.lo < b.center && b.center < b.hi), "{s:?}");
        }
    }

    #[test]
    fn every_transform_reads_a_full_scale_sine_at_zero_db_in_its_band() {
        for transform in Transform::ALL {
            let cfg = BandSettings { transform, per_octave: 3, ..BandSettings::default() };
            let (bands, shown) = levels(cfg, 880.0);
            let i = bands.iter().position(|b| b.lo <= 880.0 && 880.0 < b.hi).unwrap();
            assert!(shown[i].abs() < 1.0, "{transform:?}: {} dB in its band", shown[i]);
            // An octave away it's far down.
            assert!(shown[i + 3] < -15.0 && shown[i - 3] < -15.0, "{transform:?}: {:?}", &shown[i - 3..=i + 3]);
        }
    }

    #[test]
    fn peak_caps_hold_then_fall_faster_and_faster() {
        let cfg = BandSettings { peak_hold_ms: 100.0, peak_gravity: 100.0, release: 1e6, ..BandSettings::default() }.sanitized(SR);
        let mut an = BandAnalyzer::new(cfg);
        an.rebuild(&cfg, SR);
        an.shown[0] = -10.0;
        an.peak[0] = (-10.0, 0.0, 0.0);
        let tap = Tap::new();
        let mut caps = Vec::new();
        for _ in 0..20 {
            an.update(&tap, SR, 0.02);
            caps.push(an.peak[0].0);
        }
        assert!(caps[..4].iter().all(|c| *c == -10.0), "held: {caps:?}");
        let falls: Vec<f32> = caps[5..].windows(2).map(|w| w[0] - w[1]).collect();
        assert!(falls.windows(2).all(|w| w[1] >= w[0]) && falls[falls.len() - 1] > falls[0], "speeds up: {falls:?}");
    }

    #[test]
    fn catmull_rom_passes_through_its_points() {
        let pts = [pos2(0.0, 0.0), pos2(10.0, 5.0), pos2(20.0, -3.0), pos2(30.0, 1.0)];
        let line = catmull_rom(&pts, 4);
        for (k, p) in pts.iter().enumerate() {
            assert!((line[k * 4] - *p).length() < 1e-4);
        }
    }
}
