//! Multi-band parametric EQ: up to [`MAX_BANDS`] RBJ cookbook biquads, cut filters optionally
//! cascaded into steeper Butterworth slopes. The editor draws the same coefficients'
//! magnitude response, so the curve on screen is exactly what's applied.

use std::f64::consts::PI;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::nodes::{Smoothed, Tap, db_to_gain};
use super::{Processor, StereoBuf};

pub const MAX_BANDS: usize = 8;
/// Sections per band; each adds 12 dB/oct to a cut filter.
pub const MAX_STAGES: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FilterKind {
    Peak,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

impl FilterKind {
    pub const ALL: [FilterKind; 7] =
        [Self::Peak, Self::LowShelf, Self::HighShelf, Self::LowPass, Self::HighPass, Self::BandPass, Self::Notch];

    pub fn label(self) -> &'static str {
        match self {
            Self::Peak => "Peak",
            Self::LowShelf => "Low shelf",
            Self::HighShelf => "High shelf",
            Self::LowPass => "Low pass",
            Self::HighPass => "High pass",
            Self::BandPass => "Band pass",
            Self::Notch => "Notch",
        }
    }

    pub fn has_gain(self) -> bool {
        matches!(self, Self::Peak | Self::LowShelf | Self::HighShelf)
    }

    /// Cut filters can be cascaded for a steeper slope.
    pub fn has_slope(self) -> bool {
        matches!(self, Self::LowPass | Self::HighPass)
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }

    fn from_index(i: usize) -> Self {
        Self::ALL[i.min(Self::ALL.len() - 1)]
    }
}

/// The single-band EQ node's settings, as older sessions saved them.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EqParams {
    pub kind: FilterKind,
    pub freq: f32,
    pub q: f32,
    pub gain_db: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBand {
    pub enabled: bool,
    pub kind: FilterKind,
    pub freq: f32,
    pub q: f32,
    pub gain_db: f32,
    /// Cascaded sections for cut filters, 12 dB/oct each.
    pub stages: u8,
}

impl Default for EqBand {
    fn default() -> Self {
        Self { enabled: true, kind: FilterKind::Peak, freq: 1000.0, q: 1.0, gain_db: 0.0, stages: 1 }
    }
}

pub const MIN_FREQ: f32 = 10.0;
pub const MAX_Q: f32 = 30.0;
pub const MIN_Q: f32 = 0.1;
pub const MAX_GAIN: f32 = 30.0;

impl EqBand {
    pub fn new(kind: FilterKind, freq: f32, gain_db: f32) -> Self {
        let q = if kind == FilterKind::Peak { 1.0 } else { std::f32::consts::FRAC_1_SQRT_2 };
        Self { kind, freq, gain_db, q, ..Self::default() }
    }

    pub fn sanitized(mut self) -> Self {
        self.freq = if self.freq.is_finite() { self.freq.clamp(MIN_FREQ, 40_000.0) } else { 1000.0 };
        self.q = if self.q.is_finite() { self.q.clamp(MIN_Q, MAX_Q) } else { 1.0 };
        self.gain_db = if self.gain_db.is_finite() { self.gain_db.clamp(-MAX_GAIN, MAX_GAIN) } else { 0.0 };
        self.stages = self.stages.clamp(1, MAX_STAGES as u8);
        self
    }

    fn stage_count(&self) -> usize {
        if self.kind.has_slope() { self.stages.clamp(1, MAX_STAGES as u8) as usize } else { 1 }
    }

    /// Slope of a cut filter in dB/oct.
    pub fn slope_db(&self) -> u32 {
        self.stage_count() as u32 * 12
    }

    /// Normalized `[b0, b1, b2, a1, a2]` per section; returns the section count.
    pub fn sections(&self, sample_rate: f64, out: &mut [[f64; 5]; MAX_STAGES]) -> usize {
        let n = self.stage_count();
        let f = (self.freq as f64).clamp(MIN_FREQ as f64, sample_rate * 0.49);
        let q = self.q as f64;
        if n == 1 {
            out[0] = rbj(self.kind, f, q, self.gain_db as f64, sample_rate);
            return 1;
        }
        // Butterworth cascade; the band's Q scales the most resonant section, so 0.707 is flat.
        for (k, c) in out.iter_mut().take(n).enumerate() {
            let mut qk = 1.0 / (2.0 * ((2 * k + 1) as f64 * PI / (4 * n) as f64).cos());
            if k == n - 1 {
                qk *= q / std::f64::consts::FRAC_1_SQRT_2;
            }
            *c = rbj(self.kind, f, qk, 0.0, sample_rate);
        }
        n
    }

    /// Gain in dB at `freq`.
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        let mut c = [[0.0; 5]; MAX_STAGES];
        let n = self.sections(sample_rate, &mut c);
        let w = 2.0 * PI * freq / sample_rate;
        c[..n].iter().map(|c| magnitude_db(c, w)).sum()
    }
}

fn rbj(kind: FilterKind, freq: f64, q: f64, gain_db: f64, sample_rate: f64) -> [f64; 5] {
    let w0 = 2.0 * PI * freq / sample_rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q.max(0.01));
    let a = 10f64.powf(gain_db / 40.0);
    let (b0, b1, b2, a0, a1, a2) = match kind {
        FilterKind::Peak => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
        FilterKind::LowShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - s),
                (a + 1.0) + (a - 1.0) * cos + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - s,
            )
        }
        FilterKind::HighShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - s),
                (a + 1.0) - (a - 1.0) * cos + s,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - s,
            )
        }
        FilterKind::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        FilterKind::HighPass => {
            ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
        }
        // Constant 0 dB peak gain.
        FilterKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        FilterKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
    };
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

/// `20 log10 |H(e^jw)|` of one section.
fn magnitude_db(c: &[f64; 5], w: f64) -> f64 {
    let (s1, c1) = w.sin_cos();
    let (s2, c2) = (2.0 * w).sin_cos();
    let (nr, ni) = (c[0] + c[1] * c1 + c[2] * c2, -(c[1] * s1 + c[2] * s2));
    let (dr, di) = (1.0 + c[3] * c1 + c[4] * c2, -(c[3] * s1 + c[4] * s2));
    10.0 * ((nr * nr + ni * ni).max(1e-30) / (dr * dr + di * di).max(1e-30)).log10()
}

/// Everything saved of an EQ node.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EqSettings {
    pub bands: Vec<EqBand>,
    pub output_db: f32,
    pub bypass: bool,
    /// Spectrum of the output behind the curve.
    pub analyzer: bool,
    /// Size of the graph in graph units.
    pub size: [f32; 2],
    /// Gain shown above and below 0 dB.
    pub range_db: f32,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            // Flat to start with, but with points ready to grab.
            bands: vec![
                EqBand::new(FilterKind::LowShelf, 100.0, 0.0),
                EqBand::new(FilterKind::Peak, 500.0, 0.0),
                EqBand::new(FilterKind::Peak, 2000.0, 0.0),
                EqBand::new(FilterKind::HighShelf, 8000.0, 0.0),
            ],
            output_db: 0.0,
            bypass: false,
            analyzer: true,
            size: [380.0, 180.0],
            range_db: 12.0,
        }
    }
}

/// Parameter indices: band `b`'s field `f` is `b * FIELDS + f`.
pub const FIELDS: usize = 8;
const ENABLED: usize = 0;
const KIND: usize = 1;
const FREQ: usize = 2;
const Q: usize = 3;
const GAIN: usize = 4;
const STAGES: usize = 5;
pub const OUTPUT: usize = MAX_BANDS * FIELDS;
pub const BYPASS: usize = OUTPUT + 1;

impl EqSettings {
    pub fn from_legacy(p: EqParams) -> Self {
        let band = EqBand { enabled: true, kind: p.kind, freq: p.freq, q: p.q, gain_db: p.gain_db, stages: 1 };
        Self { bands: vec![band.sanitized()], ..Self::default() }
    }

    pub fn sanitized(mut self) -> Self {
        self.bands.truncate(MAX_BANDS);
        for b in &mut self.bands {
            *b = b.sanitized();
        }
        self.output_db = if self.output_db.is_finite() { self.output_db.clamp(-30.0, 30.0) } else { 0.0 };
        self.range_db = if self.range_db.is_finite() { self.range_db.clamp(3.0, 36.0) } else { 12.0 };
        let [w, h] = self.size;
        self.size = [w.clamp(200.0, 1600.0), h.clamp(100.0, 1000.0)];
        self
    }

    /// Combined gain of the enabled bands at `freq`, without the output gain.
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        self.bands.iter().filter(|b| b.enabled).map(|b| b.response_db(freq, sample_rate)).sum()
    }

    /// Parameter changes that turn a processor built from `old` into one matching `self`.
    pub fn diff(&self, old: &EqSettings) -> Vec<(usize, f32)> {
        let mut out = Vec::new();
        for slot in 0..MAX_BANDS {
            let (new, prev) = (self.bands.get(slot), old.bands.get(slot));
            if new == prev {
                continue;
            }
            let base = slot * FIELDS;
            match new {
                Some(b) => {
                    out.push((base + KIND, b.kind.index() as f32));
                    out.push((base + FREQ, b.freq));
                    out.push((base + Q, b.q));
                    out.push((base + GAIN, b.gain_db));
                    out.push((base + STAGES, b.stages as f32));
                    out.push((base + ENABLED, if b.enabled { 1.0 } else { 0.0 }));
                }
                None => out.push((base + ENABLED, 0.0)),
            }
        }
        if self.output_db != old.output_db {
            out.push((OUTPUT, self.output_db));
        }
        if self.bypass != old.bypass {
            out.push((BYPASS, if self.bypass { 1.0 } else { 0.0 }));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Processor

#[derive(Clone, Copy, Default)]
struct Section {
    c: [f32; 5],
    z: [[f32; 2]; 2],
}

impl Section {
    #[inline]
    fn tick(&mut self, ch: usize, x: f32) -> f32 {
        let (c, z) = (&self.c, &mut self.z[ch]);
        let y = c[0] * x + z[0];
        z[0] = c[1] * x - c[3] * y + z[1];
        z[1] = c[2] * x - c[4] * y;
        y
    }
}

#[derive(Clone, Copy)]
struct BandState {
    band: EqBand,
    sections: [Section; MAX_STAGES],
    n: usize,
}

pub struct ParamEq {
    sample_rate: f64,
    bands: [BandState; MAX_BANDS],
    output: Smoothed,
    bypass: bool,
    tap: Arc<Tap>,
}

impl ParamEq {
    pub fn new(sample_rate: f32, s: &EqSettings, tap: Arc<Tap>) -> Self {
        let off = BandState {
            band: EqBand { enabled: false, ..EqBand::default() },
            sections: [Section::default(); MAX_STAGES],
            n: 0,
        };
        let mut eq = Self {
            sample_rate: sample_rate as f64,
            bands: [off; MAX_BANDS],
            output: Smoothed::new(db_to_gain(s.output_db)),
            bypass: s.bypass,
            tap,
        };
        for (slot, b) in s.bands.iter().take(MAX_BANDS).enumerate() {
            eq.bands[slot].band = b.sanitized();
            eq.update(slot);
        }
        eq
    }

    fn update(&mut self, slot: usize) {
        let st = &mut self.bands[slot];
        if !st.band.enabled {
            st.n = 0;
            return;
        }
        let mut c = [[0.0; 5]; MAX_STAGES];
        let n = st.band.sections(self.sample_rate, &mut c);
        for (k, (sec, c)) in st.sections.iter_mut().zip(&c).enumerate().take(n) {
            // Sections that weren't running start from silence rather than stale state.
            if k >= st.n {
                sec.z = [[0.0; 2]; 2];
            }
            sec.c = c.map(|v| v as f32);
        }
        st.n = n;
    }
}

impl Processor for ParamEq {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        o.l[..n].copy_from_slice(&i.l[..n]);
        o.r[..n].copy_from_slice(&i.r[..n]);
        if !self.bypass {
            for st in &mut self.bands {
                for sec in &mut st.sections[..st.n] {
                    for v in &mut o.l[..n] {
                        *v = sec.tick(0, *v);
                    }
                    for v in &mut o.r[..n] {
                        *v = sec.tick(1, *v);
                    }
                }
            }
            for k in 0..n {
                let g = self.output.next();
                o.l[k] *= g;
                o.r[k] *= g;
            }
        }
        self.tap.push(&o.l[..n], &o.r[..n]);
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            OUTPUT => self.output.target = db_to_gain(value),
            BYPASS => self.bypass = value > 0.5,
            _ if index < OUTPUT => {
                let (slot, field) = (index / FIELDS, index % FIELDS);
                let b = &mut self.bands[slot].band;
                match field {
                    ENABLED => b.enabled = value > 0.5,
                    KIND => b.kind = FilterKind::from_index(value as usize),
                    FREQ => b.freq = value,
                    Q => b.q = value,
                    GAIN => b.gain_db = value,
                    STAGES => b.stages = value as u8,
                    _ => return,
                }
                *b = b.sanitized();
                self.update(slot);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MAX_BLOCK;

    const SR: f64 = 48_000.0;

    #[test]
    fn response_matches_band_settings() {
        let peak = EqBand::new(FilterKind::Peak, 1000.0, 6.0);
        assert!((peak.response_db(1000.0, SR) - 6.0).abs() < 1e-6);
        assert!(peak.response_db(50.0, SR).abs() < 0.1);

        let shelf = EqBand::new(FilterKind::LowShelf, 200.0, -9.0);
        assert!((shelf.response_db(20.0, SR) + 9.0).abs() < 0.2);
        assert!(shelf.response_db(10_000.0, SR).abs() < 0.1);

        // Butterworth: -3 dB at the cutoff whatever the order, 12 dB/oct per section beyond it.
        for stages in 1..=MAX_STAGES as u8 {
            let lp = EqBand { stages, ..EqBand::new(FilterKind::LowPass, 100.0, 0.0) };
            assert!((lp.response_db(100.0, SR) + 3.01).abs() < 0.05, "{stages}");
            let oct = lp.response_db(1600.0, SR) - lp.response_db(3200.0, SR);
            assert!((oct - 12.0 * stages as f64).abs() < 1.5, "{stages}: {oct}");
        }
        let notch = EqBand::new(FilterKind::Notch, 3000.0, 0.0);
        assert!(notch.response_db(3000.0, SR) < -60.0);
    }

    #[test]
    fn processor_applies_the_drawn_curve() {
        let mut s = EqSettings { bands: vec![EqBand::new(FilterKind::Peak, 1000.0, 6.0)], ..EqSettings::default() };
        let mut eq = ParamEq::new(SR as f32, &s, Tap::new());
        let amplitude = |eq: &mut ParamEq| {
            let (mut ins, mut outs) = ([StereoBuf::new()], [StereoBuf::new()]);
            let mut peak = 0f32;
            for b in 0..40 {
                for k in 0..MAX_BLOCK {
                    let t = (b * MAX_BLOCK + k) as f32 / SR as f32;
                    let v = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.25;
                    ins[0].l[k] = v;
                    ins[0].r[k] = v;
                }
                eq.process(MAX_BLOCK, &mut ins, &mut outs);
                if b > 30 {
                    peak = outs[0].l.iter().fold(peak, |m, v| m.max(v.abs()));
                }
            }
            peak
        };
        assert!((amplitude(&mut eq) - 0.25 * 10f32.powf(6.0 / 20.0)).abs() < 0.01);

        // Edits go through `diff`: removing the band leaves the signal untouched.
        let old = s.clone();
        s.bands.clear();
        for (i, v) in s.diff(&old) {
            eq.set_param(i, v);
        }
        assert!((amplitude(&mut eq) - 0.25).abs() < 0.005);
    }

    #[test]
    fn diff_only_touches_changed_bands() {
        let a = EqSettings::default();
        let mut b = a.clone();
        assert!(b.diff(&a).is_empty());
        b.bands[2].gain_db = 3.0;
        let d = b.diff(&a);
        assert!(d.iter().all(|(i, _)| i / FIELDS == 2));
        assert!(d.contains(&(2 * FIELDS + GAIN, 3.0)));
        b.bands.remove(0);
        // Later bands shift down a slot; the last slot is switched off.
        assert!(b.diff(&a).contains(&(3 * FIELDS + ENABLED, 0.0)));
    }
}
