//! FFT-based visualizers: the spectrum analyzer and the spectrogram, with their settings.

use std::sync::Arc;

use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Stroke, pos2};
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use super::{GRID, LABEL};
use crate::engine::nodes::Tap;

pub const FFT_SIZES: [usize; 7] = [512, 1024, 2048, 4096, 8192, 16384, 32768];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WindowFn {
    Rectangular,
    Hann,
    Hamming,
    BlackmanHarris,
    FlatTop,
}

impl WindowFn {
    pub const ALL: [WindowFn; 5] = [Self::Hann, Self::Hamming, Self::BlackmanHarris, Self::FlatTop, Self::Rectangular];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rectangular => "Rectangular",
            Self::Hann => "Hann",
            Self::Hamming => "Hamming",
            Self::BlackmanHarris => "Blackman-Harris",
            Self::FlatTop => "Flat top",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Rectangular => "sharpest peaks, heavy leakage",
            Self::Hann => "good all-round choice",
            Self::Hamming => "narrower peak, higher far sidelobes than Hann",
            Self::BlackmanHarris => "very low leakage; good for wide dynamic range",
            Self::FlatTop => "accurate peak levels, wide peaks",
        }
    }

    /// Periodic window coefficient `k` of `n`.
    pub(super) fn coeff(self, k: usize, n: usize) -> f32 {
        let x = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
        let cos_sum = |a: &[f64]| a.iter().enumerate().map(|(i, c)| c * (i as f64 * x).cos()).sum::<f64>();
        (match self {
            Self::Rectangular => 1.0,
            Self::Hann => cos_sum(&[0.5, -0.5]),
            Self::Hamming => cos_sum(&[0.54, -0.46]),
            Self::BlackmanHarris => cos_sum(&[0.35875, -0.48829, 0.14128, -0.01168]),
            Self::FlatTop => cos_sum(&[0.21557895, -0.41663158, 0.277263158, -0.083578947, 0.006947368]),
        }) as f32
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Channel {
    Mid,
    Side,
    Left,
    Right,
    /// Left and right as two traces (spectrum only; the spectrogram shows mid).
    Stereo,
}

impl Channel {
    pub const ALL: [Channel; 5] = [Self::Mid, Self::Side, Self::Left, Self::Right, Self::Stereo];

    pub fn label(self) -> &'static str {
        match self {
            Self::Mid => "Mid (L+R)",
            Self::Side => "Side (L−R)",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Stereo => "L & R",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Self::Mid => "M",
            Self::Side => "S",
            Self::Left => "L",
            Self::Right => "R",
            Self::Stereo => "L/R",
        }
    }

    pub(super) fn mix(self, l: &[f32], r: &[f32], out: &mut [f32]) {
        for (o, (a, b)) in out.iter_mut().zip(l.iter().zip(r)) {
            *o = match self {
                Self::Mid | Self::Stereo => (a + b) * 0.5,
                Self::Side => (a - b) * 0.5,
                Self::Left => *a,
                Self::Right => *b,
            };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Smoothing {
    Off,
    Oct24,
    Oct12,
    Oct6,
    Oct3,
}

impl Smoothing {
    pub const ALL: [Smoothing; 5] = [Self::Off, Self::Oct24, Self::Oct12, Self::Oct6, Self::Oct3];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Oct24 => "1/24 oct",
            Self::Oct12 => "1/12 oct",
            Self::Oct6 => "1/6 oct",
            Self::Oct3 => "1/3 oct",
        }
    }

    fn octaves(self) -> Option<f64> {
        match self {
            Self::Off => None,
            Self::Oct24 => Some(1.0 / 24.0),
            Self::Oct12 => Some(1.0 / 12.0),
            Self::Oct6 => Some(1.0 / 6.0),
            Self::Oct3 => Some(1.0 / 3.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Style {
    Fill,
    Line,
    Bars,
}

impl Style {
    pub const ALL: [Style; 3] = [Self::Fill, Self::Line, Self::Bars];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fill => "Filled",
            Self::Line => "Line",
            Self::Bars => "Bars",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Palette {
    Heat,
    Ice,
    Gray,
    Rainbow,
}

impl Palette {
    pub const ALL: [Palette; 4] = [Self::Heat, Self::Ice, Self::Gray, Self::Rainbow];

    pub fn label(self) -> &'static str {
        match self {
            Self::Heat => "Heat",
            Self::Ice => "Ice",
            Self::Gray => "Gray",
            Self::Rainbow => "Rainbow",
        }
    }

    fn color(self, t: f32) -> Color32 {
        const HEAT: &[[f32; 3]] = &[
            [0.0, 0.0, 0.0],
            [60.0, 10.0, 110.0],
            [180.0, 20.0, 90.0],
            [245.0, 110.0, 20.0],
            [255.0, 220.0, 60.0],
            [255.0, 255.0, 240.0],
        ];
        const ICE: &[[f32; 3]] =
            &[[0.0, 0.0, 0.0], [10.0, 30.0, 90.0], [20.0, 110.0, 180.0], [90.0, 200.0, 230.0], [240.0, 255.0, 255.0]];
        const GRAY: &[[f32; 3]] = &[[0.0, 0.0, 0.0], [255.0, 255.0, 255.0]];
        const RAINBOW: &[[f32; 3]] = &[
            [0.0, 0.0, 0.0],
            [40.0, 0.0, 130.0],
            [0.0, 90.0, 255.0],
            [0.0, 220.0, 120.0],
            [250.0, 230.0, 0.0],
            [255.0, 60.0, 0.0],
            [255.0, 255.0, 255.0],
        ];
        let stops = match self {
            Self::Heat => HEAT,
            Self::Ice => ICE,
            Self::Gray => GRAY,
            Self::Rainbow => RAINBOW,
        };
        let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
        let i = (t as usize).min(stops.len() - 2);
        let f = t - i as f32;
        let c = |k: usize| (stops[i][k] * (1.0 - f) + stops[i + 1][k] * f) as u8;
        Color32::from_rgb(c(0), c(1), c(2))
    }
}

/// Settings of the spectrum and spectrogram. Fields only one of them uses are ignored by the
/// other.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FftSettings {
    pub size: usize,
    pub window: WindowFn,
    pub channel: Channel,
    pub log_freq: bool,
    pub min_hz: f32,
    pub max_hz: f32,
    pub min_db: f32,
    pub max_db: f32,
    /// dB per octave, pivoting at 1 kHz.
    pub tilt: f32,
    pub smoothing: Smoothing,
    /// How fast the trace falls, dB/s; 0 shows each frame as is.
    pub release: f32,
    pub peak_hold: bool,
    /// How fast the peak trace falls, dB/s.
    pub peak_decay: f32,
    pub style: Style,
    /// Seconds of history the spectrogram shows.
    pub history: f32,
    pub palette: Palette,
}

impl Default for FftSettings {
    fn default() -> Self {
        Self::spectrum()
    }
}

impl FftSettings {
    pub fn spectrum() -> Self {
        Self {
            size: 4096,
            window: WindowFn::Hann,
            channel: Channel::Mid,
            log_freq: true,
            min_hz: 20.0,
            max_hz: 20_000.0,
            min_db: -96.0,
            max_db: 6.0,
            tilt: 3.0,
            smoothing: Smoothing::Off,
            release: 40.0,
            peak_hold: false,
            peak_decay: 8.0,
            style: Style::Fill,
            history: 4.0,
            palette: Palette::Heat,
        }
    }

    pub fn spectrogram() -> Self {
        Self { size: 2048, min_db: -110.0, max_db: -10.0, tilt: 0.0, ..Self::spectrum() }
    }

    /// Keeps values usable whatever a saved file contained.
    fn sanitized(mut self, sample_rate: f32) -> Self {
        if !FFT_SIZES.contains(&self.size) {
            self.size = 4096;
        }
        let nyquist = sample_rate * 0.5;
        self.max_hz = self.max_hz.clamp(100.0, nyquist);
        self.min_hz = self.min_hz.clamp(if self.log_freq { 1.0 } else { 0.0 }, self.max_hz * 0.5);
        self.max_db = self.max_db.clamp(-180.0, 40.0);
        self.min_db = self.min_db.clamp(-200.0, self.max_db - 6.0);
        self.history = self.history.clamp(0.5, 60.0);
        self
    }
}

// ---------------------------------------------------------------------------
// Analysis

struct Fft {
    size: usize,
    window_fn: WindowFn,
    plan: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Converts |X| to the amplitude of a sine (2 / sum of the window).
    norm: f32,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Squared amplitude per bin, and its prefix sums for band averages.
    power: Vec<f32>,
    prefix: Vec<f64>,
}

impl Fft {
    fn new(size: usize, window_fn: WindowFn) -> Self {
        let plan = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        let window: Vec<f32> = (0..size).map(|k| window_fn.coeff(k, size)).collect();
        let norm = 2.0 / window.iter().sum::<f32>();
        Self {
            size,
            window_fn,
            input: plan.make_input_vec(),
            output: plan.make_output_vec(),
            scratch: plan.make_scratch_vec(),
            power: vec![0.0; size / 2 + 1],
            prefix: vec![0.0; size / 2 + 2],
            plan,
            window,
            norm,
        }
    }

    /// Analyzes `samples` (`size` long) and writes dBFS per bin into `db` (a full-scale sine
    /// reads 0 dB), smoothed over `smoothing` octaves.
    fn run(&mut self, samples: &[f32], smoothing: Smoothing, bin_hz: f32, db: &mut Vec<f32>) {
        for ((x, s), w) in self.input.iter_mut().zip(samples).zip(&self.window) {
            *x = s * w;
        }
        db.resize(self.power.len(), -200.0);
        if self.plan.process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch).is_err() {
            return;
        }
        let k = self.norm * self.norm;
        for (p, c) in self.power.iter_mut().zip(&self.output) {
            *p = c.norm_sqr() * k;
        }
        let to_db = |p: f64| (10.0 * (p + 1e-20).log10()) as f32;
        match smoothing.octaves() {
            None => {
                for (d, p) in db.iter_mut().zip(&self.power) {
                    *d = to_db(*p as f64);
                }
            }
            Some(oct) => {
                // Average power over a band `oct` octaves wide around each bin.
                for (i, p) in self.power.iter().enumerate() {
                    self.prefix[i + 1] = self.prefix[i] + *p as f64;
                }
                let half = 2f64.powf(oct / 2.0);
                let last = self.power.len() - 1;
                db[0] = to_db(self.power[0] as f64);
                for (i, d) in db.iter_mut().enumerate().skip(1) {
                    let f = i as f64 * bin_hz as f64;
                    let lo = ((f / half / bin_hz as f64).floor() as usize).clamp(1, i);
                    let hi = ((f * half / bin_hz as f64).ceil() as usize).clamp(i, last);
                    *d = to_db((self.prefix[hi + 1] - self.prefix[lo]) / (hi + 1 - lo) as f64);
                }
            }
        }
    }
}

/// Value of a per-bin dB array over the frequency band `f0..f1`: the loudest bin, or a linear
/// interpolation when the band is narrower than a bin.
fn band(db: &[f32], f0: f32, f1: f32, bin_hz: f32) -> f32 {
    let (b0, b1) = (f0 / bin_hz, f1 / bin_hz);
    let last = db.len() - 1;
    if b1 - b0 < 1.0 {
        let c = ((b0 + b1) * 0.5).clamp(0.0, last as f32);
        let i = (c as usize).min(last - 1);
        let t = c - i as f32;
        db[i] * (1.0 - t) + db[i + 1] * t
    } else {
        let (i0, i1) = ((b0.ceil() as usize).min(last), (b1.floor() as usize).min(last));
        db[i0..=i1].iter().copied().fold(f32::MIN, f32::max)
    }
}

fn tilt_db(tilt: f32, freq: f32) -> f32 {
    if tilt == 0.0 { 0.0 } else { tilt * (freq.max(1.0) / 1000.0).log2() }
}

/// Frequency axis between `lo` and `hi`, logarithmic or linear.
#[derive(Clone, Copy)]
pub(super) struct FreqAxis {
    lo: f32,
    hi: f32,
    log: bool,
}

impl FreqAxis {
    fn new(cfg: &FftSettings, sample_rate: f32) -> Self {
        let hi = cfg.max_hz.min(sample_rate * 0.5);
        let lo = if cfg.log_freq { cfg.min_hz.max(1.0) } else { cfg.min_hz.max(0.0) }.min(hi * 0.5);
        Self { lo, hi, log: cfg.log_freq }
    }

    fn freq(self, t: f32) -> f32 {
        if self.log { self.lo * (self.hi / self.lo).powf(t) } else { self.lo + (self.hi - self.lo) * t }
    }

    fn pos(self, f: f32) -> f32 {
        if self.log { (f / self.lo).ln() / (self.hi / self.lo).ln() } else { (f - self.lo) / (self.hi - self.lo) }
    }

    /// Grid frequencies with labels.
    fn lines(self) -> Vec<(f32, String)> {
        let label = |f: f32| if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        let mut out = Vec::new();
        if self.log {
            let mut decade = 1.0;
            while decade <= self.hi {
                for m in [1.0, 2.0, 5.0] {
                    let f = decade * m;
                    if f > self.lo && f <= self.hi {
                        out.push((f, label(f)));
                    }
                }
                decade *= 10.0;
            }
        } else {
            let span = self.hi - self.lo;
            let step = [10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0]
                .into_iter()
                .find(|s| span / s <= 10.0)
                .unwrap_or(20_000.0);
            let mut f = (self.lo / step).ceil() * step;
            while f <= self.hi {
                if f > self.lo {
                    out.push((f, label(f)));
                }
                f += step;
            }
        }
        out
    }
}

fn fmt_freq(f: f32) -> String {
    if f >= 1000.0 { format!("{:.2} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

fn draw_freq_grid(painter: &egui::Painter, rect: Rect, axis: FreqAxis, vertical: bool) {
    let font = FontId::proportional(9.0);
    for (f, label) in axis.lines() {
        let t = axis.pos(f);
        if vertical {
            let x = rect.left() + t * rect.width();
            painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, GRID));
            painter.text(pos2(x + 2.0, rect.bottom() - 1.0), Align2::LEFT_BOTTOM, label, font.clone(), LABEL);
        } else {
            let y = (rect.bottom() - t * rect.height()).max(rect.top() + 6.0);
            painter.text(pos2(rect.left() + 2.0, y), Align2::LEFT_CENTER, label, font.clone(), LABEL);
        }
    }
}

/// Builds the analyzer again when its size or window changed.
fn ensure_fft(fft: &mut Fft, cfg: &FftSettings) {
    if fft.size != cfg.size || fft.window_fn != cfg.window {
        *fft = Fft::new(cfg.size, cfg.window);
    }
}

// ---------------------------------------------------------------------------
// Spectrum analyzer

struct Trace {
    smooth: Vec<f32>,
    peak: Vec<f32>,
}

impl Trace {
    fn new() -> Self {
        Self { smooth: Vec::new(), peak: Vec::new() }
    }
}

pub struct Spectrum {
    pub cfg: FftSettings,
    pub show_settings: bool,
    fft: Fft,
    mono: Vec<f32>,
    db: Vec<f32>,
    traces: [Trace; 2],
}

const TRACE_COLORS: [Color32; 2] = [Color32::from_rgb(110, 190, 255), Color32::from_rgb(255, 160, 90)];

impl Spectrum {
    pub fn new(cfg: FftSettings) -> Self {
        Self {
            fft: Fft::new(cfg.size, cfg.window),
            cfg,
            show_settings: false,
            mono: Vec::new(),
            db: Vec::new(),
            traces: [Trace::new(), Trace::new()],
        }
    }

    pub fn size(&self) -> usize {
        self.cfg.size
    }

    pub fn update(&mut self, l: &[f32], r: &[f32], sample_rate: f32, dt: f32) {
        let cfg = self.cfg.sanitized(sample_rate);
        ensure_fft(&mut self.fft, &cfg);
        let bin_hz = sample_rate / cfg.size as f32;
        let channels: &[Channel] = match cfg.channel {
            Channel::Stereo => &[Channel::Left, Channel::Right],
            c => &[c][..],
        };
        self.mono.resize(cfg.size, 0.0);
        for (ch, trace) in channels.iter().zip(&mut self.traces) {
            ch.mix(l, r, &mut self.mono);
            self.fft.run(&self.mono, cfg.smoothing, bin_hz, &mut self.db);
            let n = self.db.len();
            if trace.smooth.len() != n {
                trace.smooth = self.db.clone();
                trace.peak = self.db.clone();
            }
            let (fall, peak_fall) = (cfg.release * dt, cfg.peak_decay * dt);
            for ((s, p), d) in trace.smooth.iter_mut().zip(&mut trace.peak).zip(&self.db) {
                *s = if cfg.release <= 0.0 { *d } else { d.max(*s - fall) };
                *p = d.max(*p - peak_fall);
            }
        }
    }

    pub fn draw(&self, painter: &egui::Painter, rect: Rect, sample_rate: f32, hover: Option<Pos2>) {
        let cfg = self.cfg.sanitized(sample_rate);
        let axis = FreqAxis::new(&cfg, sample_rate);
        let bin_hz = sample_rate / cfg.size as f32;
        let (min_db, max_db) = (cfg.min_db, cfg.max_db);
        let y_of = |db: f32| {
            let t = ((db - min_db) / (max_db - min_db)).clamp(0.0, 1.0);
            rect.bottom() - t * rect.height()
        };
        let level = |db: &[f32], f0: f32, f1: f32| band(db, f0, f1, bin_hz) + tilt_db(cfg.tilt, (f0 * f1).sqrt());

        // dB grid: about six lines whatever the range.
        let font = FontId::proportional(9.0);
        let range = max_db - min_db;
        let step = [3.0, 6.0, 12.0, 24.0, 48.0].into_iter().find(|s| range / s <= 7.0).unwrap_or(60.0);
        let mut db = (max_db / step).floor() * step;
        while db >= min_db {
            let y = y_of(db);
            painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, GRID));
            painter.text(pos2(rect.right() - 2.0, y + 1.0), Align2::RIGHT_TOP, format!("{db:.0}"), font.clone(), LABEL);
            db -= step;
        }
        draw_freq_grid(painter, rect, axis, true);

        let traces = if cfg.channel == Channel::Stereo { 2 } else { 1 };
        for (k, trace) in self.traces.iter().take(traces).enumerate() {
            if trace.smooth.is_empty() {
                continue;
            }
            let color = TRACE_COLORS[k];
            match cfg.style {
                Style::Bars => {
                    let bars = ((rect.width() / 7.0) as usize).max(4);
                    let w = rect.width() / bars as f32;
                    for b in 0..bars {
                        let (t0, t1) = (b as f32 / bars as f32, (b + 1) as f32 / bars as f32);
                        let y = y_of(level(&trace.smooth, axis.freq(t0), axis.freq(t1)));
                        let x = rect.left() + b as f32 * w + k as f32 * w * 0.5 / traces as f32;
                        let bw = (w - 1.5) / traces as f32;
                        painter.rect_filled(
                            Rect::from_min_max(pos2(x, y), pos2(x + bw, rect.bottom())),
                            0.0,
                            color.gamma_multiply(0.8),
                        );
                    }
                }
                Style::Fill | Style::Line => {
                    let cols = (rect.width().round() as usize).max(2);
                    let line: Vec<Pos2> = (0..cols)
                        .map(|c| {
                            let (t0, t1) = (c as f32 / cols as f32, (c + 1) as f32 / cols as f32);
                            let x = rect.left() + (t0 + t1) * 0.5 * rect.width();
                            pos2(x, y_of(level(&trace.smooth, axis.freq(t0), axis.freq(t1))))
                        })
                        .collect();
                    if cfg.style == Style::Fill {
                        fill_under(painter, &line, rect.bottom(), color.gamma_multiply(0.45), color.gamma_multiply(0.04));
                    }
                    painter.line(line, Stroke::new(1.2, color));
                }
            }
            if cfg.peak_hold {
                let cols = (rect.width().round() as usize).max(2);
                let peaks: Vec<Pos2> = (0..cols)
                    .map(|c| {
                        let (t0, t1) = (c as f32 / cols as f32, (c + 1) as f32 / cols as f32);
                        let x = rect.left() + (t0 + t1) * 0.5 * rect.width();
                        pos2(x, y_of(level(&trace.peak, axis.freq(t0), axis.freq(t1))))
                    })
                    .collect();
                let pc = if traces == 1 { Color32::from_rgba_unmultiplied(255, 190, 90, 160) } else { color.gamma_multiply(0.5) };
                painter.line(peaks, Stroke::new(1.0, pc));
            }
        }

        let hover = hover.filter(|h| rect.contains(*h));
        if hover.is_none() {
            let summary = format!("{} · {} · {}", cfg.size, cfg.window.label(), cfg.channel.short());
            painter.text(rect.left_top() + egui::vec2(4.0, 2.0), Align2::LEFT_TOP, summary, font, Color32::from_gray(90));
        }
        if let Some(h) = hover {
            let f = axis.freq((h.x - rect.left()) / rect.width());
            let values: Vec<String> = self
                .traces
                .iter()
                .take(traces)
                .filter(|t| !t.smooth.is_empty())
                .map(|t| format!("{:.1}", level(&t.smooth, f, f)))
                .collect();
            painter.line_segment([pos2(h.x, rect.top()), pos2(h.x, rect.bottom())], Stroke::new(1.0, Color32::from_gray(90)));
            painter.text(
                pos2(rect.left() + 4.0, rect.top() + 2.0),
                Align2::LEFT_TOP,
                format!("{}  {} dB", fmt_freq(f), values.join(" / ")),
                FontId::proportional(11.0),
                Color32::from_gray(210),
            );
        }
    }
}

impl Spectrum {
    /// Only the first trace, as a faint fill for drawing behind something else, on the same
    /// frequency axis as [`draw`](Self::draw).
    pub fn draw_overlay(&self, painter: &egui::Painter, rect: Rect, sample_rate: f32, color: Color32) {
        let trace = &self.traces[0];
        if trace.smooth.is_empty() {
            return;
        }
        let cfg = self.cfg.sanitized(sample_rate);
        let axis = FreqAxis::new(&cfg, sample_rate);
        let bin_hz = sample_rate / cfg.size as f32;
        let cols = ((rect.width() / 2.0).round() as usize).max(2);
        let line: Vec<Pos2> = (0..cols)
            .map(|c| {
                let (f0, f1) = (axis.freq(c as f32 / cols as f32), axis.freq((c + 1) as f32 / cols as f32));
                let db = band(&trace.smooth, f0, f1, bin_hz) + tilt_db(cfg.tilt, (f0 * f1).sqrt());
                let t = ((db - cfg.min_db) / (cfg.max_db - cfg.min_db)).clamp(0.0, 1.0);
                pos2(rect.left() + (c as f32 + 0.5) / cols as f32 * rect.width(), rect.bottom() - t * rect.height())
            })
            .collect();
        fill_under(painter, &line, rect.bottom(), color, color.gamma_multiply(0.15));
    }
}

/// Fills between `line` and `bottom` with a vertical gradient.
fn fill_under(painter: &egui::Painter, line: &[Pos2], bottom: f32, top_color: Color32, bottom_color: Color32) {
    let mut mesh = Mesh::default();
    for (i, p) in line.iter().enumerate() {
        mesh.colored_vertex(*p, top_color);
        mesh.colored_vertex(pos2(p.x, bottom), bottom_color);
        if i > 0 {
            let v = (i as u32) * 2;
            mesh.add_triangle(v - 2, v - 1, v);
            mesh.add_triangle(v - 1, v + 1, v);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

// ---------------------------------------------------------------------------
// Spectrogram

pub struct Spectrogram {
    pub cfg: FftSettings,
    pub show_settings: bool,
    /// Settings the current image was drawn with; it's cleared when they change.
    drawn: Option<FftSettings>,
    fft: Fft,
    mono: Vec<f32>,
    db: Vec<f32>,
    /// `written` position of the newest analyzed column.
    pos: u64,
    pixels: Vec<Color32>,
    texture: Option<egui::TextureHandle>,
    dirty: bool,
}

impl Spectrogram {
    const COLS: usize = 400;
    const ROWS: usize = 200;

    pub fn new(cfg: FftSettings) -> Self {
        Self {
            fft: Fft::new(cfg.size, cfg.window),
            cfg,
            show_settings: false,
            drawn: None,
            mono: Vec::new(),
            db: Vec::new(),
            pos: 0,
            pixels: vec![Color32::BLACK; Self::COLS * Self::ROWS],
            texture: None,
            dirty: true,
        }
    }

    fn hop(cfg: &FftSettings, sample_rate: f32) -> u64 {
        ((cfg.history * sample_rate / Self::COLS as f32) as u64).max(1)
    }

    pub fn update(&mut self, tap: &Tap, sample_rate: f32, l: &mut Vec<f32>, r: &mut Vec<f32>) {
        let cfg = self.cfg.sanitized(sample_rate);
        if self.drawn != Some(cfg) {
            self.pixels.fill(Color32::BLACK);
            self.drawn = Some(cfg);
            self.dirty = true;
        }
        ensure_fft(&mut self.fft, &cfg);
        let written = tap.written();
        let hop = Self::hop(&cfg, sample_rate);
        // Skip ahead if more than a screenful is pending (e.g. the node was off-screen).
        let span = (Self::COLS as u64) * hop;
        if written > self.pos + span {
            self.pos = written - span;
        }
        if written < self.pos {
            self.pos = written;
        }
        let n = cfg.size;
        l.resize(n, 0.0);
        r.resize(n, 0.0);
        self.mono.resize(n, 0.0);
        let axis = FreqAxis::new(&cfg, sample_rate);
        let bin_hz = sample_rate / n as f32;
        let channel = if cfg.channel == Channel::Stereo { Channel::Mid } else { cfg.channel };
        while self.pos + hop <= written {
            self.pos += hop;
            tap.read(self.pos, l, r);
            channel.mix(l, r, &mut self.mono);
            self.fft.run(&self.mono, cfg.smoothing, bin_hz, &mut self.db);
            for row in 0..Self::ROWS {
                // Row 0 is the top of the image, i.e. the highest frequency.
                let f0 = axis.freq(1.0 - (row + 1) as f32 / Self::ROWS as f32);
                let f1 = axis.freq(1.0 - row as f32 / Self::ROWS as f32);
                let db = band(&self.db, f0, f1, bin_hz) + tilt_db(cfg.tilt, (f0 * f1).sqrt());
                let line = &mut self.pixels[row * Self::COLS..(row + 1) * Self::COLS];
                line.copy_within(1.., 0);
                line[Self::COLS - 1] = cfg.palette.color((db - cfg.min_db) / (cfg.max_db - cfg.min_db));
            }
            self.dirty = true;
        }
    }

    pub fn draw(&mut self, ctx: &egui::Context, painter: &egui::Painter, rect: Rect, sample_rate: f32, hover: Option<Pos2>) {
        if self.dirty || self.texture.is_none() {
            let img = egui::ColorImage::new([Self::COLS, Self::ROWS], self.pixels.clone());
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("spectrogram", img, egui::TextureOptions::LINEAR)),
            }
            self.dirty = false;
        }
        if let Some(t) = &self.texture {
            painter.image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        let cfg = self.cfg.sanitized(sample_rate);
        let axis = FreqAxis::new(&cfg, sample_rate);
        draw_freq_grid(painter, rect, axis, false);
        if let Some(h) = hover.filter(|h| rect.contains(*h)) {
            let f = axis.freq((rect.bottom() - h.y) / rect.height());
            let secs = (Self::COLS as u64 * Self::hop(&cfg, sample_rate)) as f32 / sample_rate;
            let ago = (rect.right() - h.x) / rect.width() * secs;
            painter.text(
                pos2(rect.right() - 4.0, rect.top() + 2.0),
                Align2::RIGHT_TOP,
                format!("{}  -{ago:.1} s", fmt_freq(f)),
                FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Settings UI

/// The row above the display: a settings toggle and a summary; the full settings below it
/// when open. Returns nothing; edits `cfg` in place.
pub fn settings_ui(ui: &mut egui::Ui, cfg: &mut FftSettings, open: &mut bool, spectrogram: bool, sample_rate: f32) {
    ui.horizontal(|ui| {
        ui.toggle_value(open, "⚙").on_hover_text("analyzer settings");
        egui::ComboBox::from_id_salt(ui.id().with("size"))
            .width(64.0)
            .selected_text(format!("{}", cfg.size))
            .show_ui(ui, |ui| {
                for n in FFT_SIZES {
                    let ms = n as f32 / sample_rate * 1000.0;
                    let hz = sample_rate / n as f32;
                    ui.selectable_value(&mut cfg.size, n, format!("{n}  ({ms:.0} ms, {hz:.1} Hz/bin)"));
                }
            })
            .response
            .on_hover_text("FFT size: larger = finer frequency detail, slower response");
        egui::ComboBox::from_id_salt(ui.id().with("ch"))
            .width(70.0)
            .selected_text(cfg.channel.label())
            .show_ui(ui, |ui| {
                for c in Channel::ALL {
                    if c == Channel::Stereo && spectrogram {
                        continue;
                    }
                    ui.selectable_value(&mut cfg.channel, c, c.label());
                }
            });
        if !spectrogram {
            ui.toggle_value(&mut cfg.peak_hold, "peak");
        }
    });
    if !*open {
        return;
    }

    let nyquist = sample_rate * 0.5;
    egui::Grid::new(ui.id().with("grid")).num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| {
        ui.label("Window");
        egui::ComboBox::from_id_salt(ui.id().with("win")).selected_text(cfg.window.label()).show_ui(ui, |ui| {
            for w in WindowFn::ALL {
                ui.selectable_value(&mut cfg.window, w, w.label()).on_hover_text(w.hint());
            }
        });
        ui.end_row();

        ui.label("Frequency");
        ui.horizontal(|ui| {
            let lo_min = if cfg.log_freq { 1.0 } else { 0.0 };
            let max_hz = cfg.max_hz;
            ui.add(egui::DragValue::new(&mut cfg.min_hz).range(lo_min..=max_hz * 0.5).speed(1.0).suffix(" Hz"));
            ui.label("–");
            let min_hz = cfg.min_hz;
            ui.add(egui::DragValue::new(&mut cfg.max_hz).range((min_hz * 2.0).max(100.0)..=nyquist).speed(10.0).suffix(" Hz"));
            ui.checkbox(&mut cfg.log_freq, "log");
        });
        ui.end_row();

        ui.label(if spectrogram { "Color range" } else { "Level range" });
        ui.horizontal(|ui| {
            let max_db = cfg.max_db;
            ui.add(egui::DragValue::new(&mut cfg.min_db).range(-200.0..=max_db - 6.0).speed(0.5).suffix(" dB"));
            ui.label("–");
            let min_db = cfg.min_db;
            ui.add(egui::DragValue::new(&mut cfg.max_db).range(min_db + 6.0..=40.0).speed(0.5).suffix(" dB"));
        });
        ui.end_row();

        ui.label("Tilt");
        ui.add(egui::Slider::new(&mut cfg.tilt, -6.0..=9.0).step_by(0.5).suffix(" dB/oct"))
            .on_hover_text("pivots at 1 kHz; 3 dB/oct shows pink noise flat, 4.5 is common for mixes");
        ui.end_row();

        ui.label("Smoothing");
        egui::ComboBox::from_id_salt(ui.id().with("smooth")).selected_text(cfg.smoothing.label()).show_ui(ui, |ui| {
            for s in Smoothing::ALL {
                ui.selectable_value(&mut cfg.smoothing, s, s.label());
            }
        });
        ui.end_row();

        if spectrogram {
            ui.label("History");
            ui.add(egui::Slider::new(&mut cfg.history, 0.5..=60.0).logarithmic(true).suffix(" s"));
            ui.end_row();

            ui.label("Colors");
            egui::ComboBox::from_id_salt(ui.id().with("pal")).selected_text(cfg.palette.label()).show_ui(ui, |ui| {
                for p in Palette::ALL {
                    ui.selectable_value(&mut cfg.palette, p, p.label());
                }
            });
            ui.end_row();
        } else {
            ui.label("Style");
            ui.horizontal(|ui| {
                for s in Style::ALL {
                    ui.selectable_value(&mut cfg.style, s, s.label());
                }
            });
            ui.end_row();

            ui.label("Release");
            ui.add(egui::Slider::new(&mut cfg.release, 0.0..=300.0).suffix(" dB/s"))
                .on_hover_text("how fast the trace falls; 0 = no smoothing");
            ui.end_row();

            ui.label("Peak decay");
            ui.add_enabled(cfg.peak_hold, egui::Slider::new(&mut cfg.peak_decay, 0.0..=60.0).suffix(" dB/s"))
                .on_hover_text("0 holds peaks until reset");
            ui.end_row();
        }
    });
    if ui.small_button("Reset to defaults").clicked() {
        *cfg = if spectrogram { FftSettings::spectrogram() } else { FftSettings::spectrum() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_sine_reads_zero_db_with_every_window() {
        let (sr, n) = (48_000.0, 4096);
        let bin_hz = sr / n as f32;
        // On a bin center, every window reads the amplitude exactly.
        let freq = 100.0 * bin_hz;
        let s: Vec<f32> = (0..n).map(|k| (k as f32 * freq * std::f32::consts::TAU / sr).sin()).collect();
        let mut db = Vec::new();
        for w in WindowFn::ALL {
            let mut fft = Fft::new(n, w);
            fft.run(&s, Smoothing::Off, bin_hz, &mut db);
            let peak = db.iter().copied().fold(f32::MIN, f32::max);
            assert!(peak.abs() < 0.05, "{w:?}: {peak}");
        }
        // Between two bins only the flat top stays accurate.
        let freq = 100.5 * bin_hz;
        let s: Vec<f32> = (0..n).map(|k| (k as f32 * freq * std::f32::consts::TAU / sr).sin()).collect();
        let mut fft = Fft::new(n, WindowFn::FlatTop);
        fft.run(&s, Smoothing::Off, bin_hz, &mut db);
        let peak = db.iter().copied().fold(f32::MIN, f32::max);
        assert!(peak.abs() < 0.1, "flat top off-bin: {peak}");
    }

    #[test]
    fn octave_smoothing_keeps_flat_spectra_flat() {
        let (sr, n) = (48_000.0, 2048);
        let bin_hz = sr / n as f32;
        // An impulse has a flat spectrum; smoothing must not change its level.
        let mut s = vec![0.0; n];
        s[0] = 1.0;
        let (mut raw, mut smooth) = (Vec::new(), Vec::new());
        let mut fft = Fft::new(n, WindowFn::Rectangular);
        fft.run(&s, Smoothing::Off, bin_hz, &mut raw);
        fft.run(&s, Smoothing::Oct3, bin_hz, &mut smooth);
        for i in [10, 100, 500] {
            assert!((raw[i] - smooth[i]).abs() < 0.01, "bin {i}: {} vs {}", raw[i], smooth[i]);
        }
    }

    #[test]
    fn axes_round_trip() {
        for log in [true, false] {
            let cfg = FftSettings { log_freq: log, min_hz: 30.0, max_hz: 16_000.0, ..FftSettings::spectrum() };
            let axis = FreqAxis::new(&cfg, 48_000.0);
            for f in [30.0, 440.0, 16_000.0] {
                assert!((axis.freq(axis.pos(f)) - f).abs() < 0.5, "log {log}: {f}");
            }
            assert!(!axis.lines().is_empty());
        }
    }
}
