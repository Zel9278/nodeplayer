//! Visualizer nodes: audio passes through unchanged while a [`Tap`] records it, and the node
//! body draws from that history every frame.

use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

use crate::engine::nodes::{LimiterMeter, Tap};

mod bands;
mod spectral;
use bands::{BandAnalyzer, BandSettings};
pub use spectral::{FftSettings, Spectrum};
use spectral::Spectrogram;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VisKind {
    Spectrum,
    Bands,
    Spectrogram,
    Scope,
    Vectorscope,
    Meter,
}

impl VisKind {
    pub const ALL: [VisKind; 6] = [Self::Spectrum, Self::Bands, Self::Spectrogram, Self::Scope, Self::Vectorscope, Self::Meter];

    pub fn label(self) -> &'static str {
        match self {
            Self::Spectrum => "Spectrum",
            Self::Bands => "Band analyzer",
            Self::Spectrogram => "Spectrogram",
            Self::Scope => "Oscilloscope",
            Self::Vectorscope => "Vectorscope",
            Self::Meter => "Level meter",
        }
    }

    fn default_size(self) -> Vec2 {
        match self {
            Self::Spectrum | Self::Spectrogram => vec2(360.0, 170.0),
            Self::Bands => vec2(420.0, 200.0),
            Self::Scope => vec2(320.0, 140.0),
            Self::Vectorscope => vec2(200.0, 200.0),
            Self::Meter => vec2(280.0, 64.0),
        }
    }
}

enum View {
    Spectrum(Box<Spectrum>),
    Bands(Box<BandAnalyzer>),
    Spectrogram(Box<Spectrogram>),
    Scope(Scope),
    Vectorscope,
    Meter(Meter),
}

pub struct Visualizer {
    pub kind: VisKind,
    pub tap: Arc<Tap>,
    /// Size of the drawing area in graph units; resizable with the corner handle.
    size: Vec2,
    view: View,
    l: Vec<f32>,
    r: Vec<f32>,
    last_frame: Instant,
}

/// What's saved of a visualizer: its kind, size and settings.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct VisSettings {
    pub kind: VisKind,
    pub size: [f32; 2],
    /// Spectrum / spectrogram settings.
    #[serde(default)]
    pub fft: Option<FftSettings>,
    /// Band analyzer settings.
    #[serde(default)]
    pub bands: Option<BandSettings>,
    // Older spectrum settings, read when `fft` is missing.
    #[serde(default, skip_serializing)]
    pub fft_size: Option<usize>,
    #[serde(default, skip_serializing)]
    pub tilt: Option<bool>,
    #[serde(default, skip_serializing)]
    pub hold: Option<bool>,
    #[serde(default)]
    pub scope_ms: Option<f32>,
    #[serde(default)]
    pub trigger: Option<bool>,
}

impl Visualizer {
    pub fn settings(&self) -> VisSettings {
        let mut s = VisSettings {
            kind: self.kind,
            size: self.size.into(),
            fft: None,
            bands: None,
            fft_size: None,
            tilt: None,
            hold: None,
            scope_ms: None,
            trigger: None,
        };
        match &self.view {
            View::Spectrum(sp) => s.fft = Some(sp.cfg),
            View::Bands(b) => s.bands = Some(b.cfg),
            View::Spectrogram(sg) => s.fft = Some(sg.cfg),
            View::Scope(sc) => {
                s.scope_ms = Some(sc.ms);
                s.trigger = Some(sc.trigger);
            }
            _ => {}
        }
        s
    }

    pub fn from_settings(s: &VisSettings) -> Self {
        let mut v = Self::new(s.kind);
        v.size = Vec2::from(s.size).clamp(vec2(120.0, 48.0), vec2(1600.0, 1000.0));
        match &mut v.view {
            View::Spectrum(sp) => {
                let cfg = s.fft.unwrap_or_else(|| {
                    let d = FftSettings::spectrum();
                    FftSettings {
                        size: s.fft_size.unwrap_or(d.size),
                        tilt: if s.tilt.unwrap_or(true) { 3.0 } else { 0.0 },
                        peak_hold: s.hold.unwrap_or(false),
                        ..d
                    }
                });
                **sp = Spectrum::new(cfg);
            }
            View::Spectrogram(sg) => {
                if let Some(cfg) = s.fft {
                    **sg = Spectrogram::new(cfg);
                }
            }
            View::Bands(b) => {
                if let Some(cfg) = s.bands {
                    b.cfg = cfg;
                }
            }
            View::Scope(sc) => {
                sc.ms = s.scope_ms.unwrap_or(sc.ms).clamp(1.0, 200.0);
                sc.trigger = s.trigger.unwrap_or(sc.trigger);
            }
            _ => {}
        }
        v
    }

    pub fn new(kind: VisKind) -> Self {
        let view = match kind {
            VisKind::Spectrum => View::Spectrum(Box::new(Spectrum::new(FftSettings::spectrum()))),
            VisKind::Bands => View::Bands(Box::new(BandAnalyzer::new(BandSettings::default()))),
            VisKind::Spectrogram => View::Spectrogram(Box::new(Spectrogram::new(FftSettings::spectrogram()))),
            VisKind::Scope => View::Scope(Scope { ms: 20.0, trigger: true }),
            VisKind::Vectorscope => View::Vectorscope,
            VisKind::Meter => View::Meter(Meter::default()),
        };
        Self {
            kind,
            tap: Tap::new(),
            size: kind.default_size(),
            view,
            l: Vec::new(),
            r: Vec::new(),
            last_frame: Instant::now(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, sample_rate: f32) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;

        // Per-view settings row.
        match &mut self.view {
            View::Spectrum(s) => spectral::settings_ui(ui, &mut s.cfg, &mut s.show_settings, false, sample_rate),
            View::Spectrogram(s) => spectral::settings_ui(ui, &mut s.cfg, &mut s.show_settings, true, sample_rate),
            View::Bands(b) => bands::settings_ui(ui, &mut b.cfg, &mut b.show_settings, sample_rate),
            View::Scope(s) => {
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut s.ms, 1.0..=200.0).logarithmic(true).suffix(" ms"));
                    ui.checkbox(&mut s.trigger, "trigger");
                });
            }
            _ => {}
        }

        let (rect, resp) = ui.allocate_exact_size(self.size, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::from_gray(12));
        let hover = resp.hover_pos();

        let (l, r, tap) = (&mut self.l, &mut self.r, &*self.tap);
        match &mut self.view {
            View::Spectrum(s) => {
                read_latest(tap, s.size(), l, r);
                s.update(l, r, sample_rate, dt);
                s.draw(&painter, rect, sample_rate, hover);
            }
            View::Bands(b) => {
                b.update(tap, sample_rate, dt);
                b.draw(&painter, rect, sample_rate, hover);
            }
            View::Spectrogram(s) => {
                s.update(tap, sample_rate, l, r);
                s.draw(ui.ctx(), &painter, rect, sample_rate, hover);
            }
            View::Scope(s) => {
                let shown = ((s.ms * 0.001 * sample_rate) as usize).clamp(16, Tap::FRAMES / 2);
                read_latest(tap, shown * 2, l, r);
                draw_scope(&painter, rect, l, r, shown, s.trigger);
            }
            View::Vectorscope => {
                read_latest(tap, 2048, l, r);
                draw_vectorscope(&painter, rect, l, r);
            }
            View::Meter(m) => {
                // 300 ms RMS window.
                read_latest(tap, (sample_rate * 0.3) as usize, l, r);
                if resp.clicked() {
                    *m = Meter::default();
                }
                m.update(l, r, sample_rate, dt);
                m.draw(&painter, rect);
            }
        }

        self.resize_handle(ui, rect);
    }

    fn resize_handle(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let h = Rect::from_min_max(rect.max - vec2(12.0, 12.0), rect.max);
        let resp = ui.interact(h, ui.id().with("resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
        let c = if resp.hovered() || resp.dragged() { Color32::from_gray(200) } else { Color32::from_gray(90) };
        for k in [4.0, 8.0] {
            ui.painter().line_segment([pos2(h.max.x - k, h.max.y - 1.0), pos2(h.max.x - 1.0, h.max.y - k)], Stroke::new(1.0, c));
        }
        if resp.dragged() {
            self.size = (self.size + resp.drag_delta()).clamp(vec2(120.0, 48.0), vec2(1600.0, 1000.0));
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers

/// Fills `l`/`r` with the newest `n` frames.
pub fn read_latest(tap: &Tap, n: usize, l: &mut Vec<f32>, r: &mut Vec<f32>) {
    l.resize(n, 0.0);
    r.resize(n, 0.0);
    tap.read(tap.written(), l, r);
}

const GRID: Color32 = Color32::from_gray(40);
const LABEL: Color32 = Color32::from_gray(110);

// ---------------------------------------------------------------------------
// Oscilloscope

struct Scope {
    ms: f32,
    trigger: bool,
}

const LEFT: Color32 = Color32::from_rgb(110, 190, 255);
const RIGHT: Color32 = Color32::from_rgb(255, 160, 90);

/// `l`/`r` hold `2 * shown` frames; with `trigger` the window starts at the last rising zero
/// crossing of the mid signal in the first half so periodic waveforms stand still.
fn draw_scope(painter: &egui::Painter, rect: Rect, l: &[f32], r: &[f32], shown: usize, trigger: bool) {
    let cy = rect.center().y;
    painter.line_segment([pos2(rect.left(), cy), pos2(rect.right(), cy)], Stroke::new(1.0, GRID));
    let mut start = l.len() - shown;
    if trigger {
        let mid = |k: usize| l[k] + r[k];
        if let Some(k) = (1..=start).rev().find(|&k| mid(k - 1) <= 0.0 && mid(k) > 0.0) {
            start = k;
        }
    }
    let half = rect.height() * 0.5;
    for (buf, color) in [(r, RIGHT), (l, LEFT)] {
        let pts: Vec<Pos2> = buf[start..start + shown]
            .iter()
            .enumerate()
            .map(|(k, v)| {
                let x = rect.left() + k as f32 / (shown - 1) as f32 * rect.width();
                pos2(x, cy - v.clamp(-1.0, 1.0) * half)
            })
            .collect();
        painter.line(pts, Stroke::new(1.0, color.gamma_multiply(0.85)));
    }
    let font = FontId::proportional(9.0);
    painter.text(rect.left_top() + vec2(3.0, 2.0), Align2::LEFT_TOP, "L", font.clone(), LEFT);
    painter.text(rect.left_top() + vec2(12.0, 2.0), Align2::LEFT_TOP, "R", font, RIGHT);
}

// ---------------------------------------------------------------------------
// Vectorscope (goniometer) with a correlation meter

fn draw_vectorscope(painter: &egui::Painter, rect: Rect, l: &[f32], r: &[f32]) {
    let bar_h = 10.0;
    let plot = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - bar_h - 4.0));
    let side = plot.width().min(plot.height());
    let sq = Rect::from_center_size(plot.center(), vec2(side, side));
    let c = sq.center();
    let rad = side * 0.5;
    let stroke = Stroke::new(1.0, GRID);
    painter.circle_stroke(c, rad, stroke);
    painter.line_segment([pos2(c.x, sq.top()), pos2(c.x, sq.bottom())], stroke);
    painter.line_segment([pos2(sq.left(), c.y), pos2(sq.right(), c.y)], stroke);
    let d = rad * std::f32::consts::FRAC_1_SQRT_2;
    painter.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
    painter.line_segment([c + vec2(d, -d), c + vec2(-d, d)], stroke);
    let font = FontId::proportional(9.0);
    painter.text(c + vec2(-d - 2.0, -d - 2.0), Align2::RIGHT_BOTTOM, "L", font.clone(), LABEL);
    painter.text(c + vec2(d + 2.0, -d - 2.0), Align2::LEFT_BOTTOM, "R", font.clone(), LABEL);
    painter.text(pos2(c.x + 3.0, sq.top()), Align2::LEFT_TOP, "M", font.clone(), LABEL);

    // Mono sits on the vertical axis, out-of-phase on the horizontal one.
    let mut mesh = Mesh::default();
    let dot = Color32::from_rgba_unmultiplied(120, 255, 170, 70);
    let s = 0.6;
    let (mut lr, mut ll, mut rr) = (0.0, 0.0, 0.0);
    for (&a, &b) in l.iter().zip(r) {
        lr += a * b;
        ll += a * a;
        rr += b * b;
        let x = (b - a) * std::f32::consts::FRAC_1_SQRT_2;
        let y = (a + b) * std::f32::consts::FRAC_1_SQRT_2;
        let p = c + vec2(x, -y).clamp(vec2(-1.0, -1.0), vec2(1.0, 1.0)) * rad;
        mesh.add_colored_rect(Rect::from_center_size(p, vec2(s * 2.0, s * 2.0)), dot);
    }
    painter.add(egui::Shape::mesh(mesh));

    let corr = if ll * rr > 1e-12 { lr / (ll * rr).sqrt() } else { 0.0 };
    // Inset on the right too so the "+1" label clears the resize handle.
    let bar = Rect::from_min_max(pos2(rect.left() + 16.0, rect.bottom() - bar_h), pos2(rect.right() - 30.0, rect.bottom()));
    painter.rect_filled(bar, 2.0, Color32::from_gray(30));
    let x0 = bar.center().x;
    let x1 = x0 + corr * bar.width() * 0.5;
    let col = if corr < 0.0 { Color32::from_rgb(230, 80, 70) } else { Color32::from_rgb(90, 200, 120) };
    painter.rect_filled(Rect::from_x_y_ranges(x0.min(x1)..=x0.max(x1), bar.y_range()), 0.0, col);
    painter.line_segment([pos2(x0, bar.top()), pos2(x0, bar.bottom())], Stroke::new(1.0, LABEL));
    painter.text(pos2(bar.left() - 2.0, bar.center().y), Align2::RIGHT_CENTER, "-1", font.clone(), LABEL);
    painter.text(pos2(bar.right() + 2.0, bar.center().y), Align2::LEFT_CENTER, "+1", font, LABEL);
}

// ---------------------------------------------------------------------------
// Level meter: RMS bar with a peak overlay, peak hold and clip latch

const METER_MIN_DB: f32 = -60.0;
const METER_MAX_DB: f32 = 6.0;

struct Meter {
    peak: [f32; 2],
    rms: [f32; 2],
    hold: [f32; 2],
    hold_age: [f32; 2],
    clip: [bool; 2],
}

impl Default for Meter {
    fn default() -> Self {
        Self { peak: [-200.0; 2], rms: [-200.0; 2], hold: [-200.0; 2], hold_age: [0.0; 2], clip: [false; 2] }
    }
}

fn to_db(v: f32) -> f32 {
    20.0 * v.max(1e-9).log10()
}

impl Meter {
    fn update(&mut self, l: &[f32], r: &[f32], sample_rate: f32, dt: f32) {
        // Peak over the newest ~frame's worth of audio; RMS over the whole window.
        let recent = ((sample_rate * dt.max(0.005)) as usize).min(l.len());
        for (ch, buf) in [l, r].into_iter().enumerate() {
            let peak = buf[buf.len() - recent..].iter().fold(0f32, |m, v| m.max(v.abs()));
            let rms = (buf.iter().map(|v| v * v).sum::<f32>() / buf.len().max(1) as f32).sqrt();
            let p = to_db(peak);
            self.peak[ch] = p.max(self.peak[ch] - 30.0 * dt);
            self.rms[ch] = to_db(rms);
            self.hold_age[ch] += dt;
            if p >= self.hold[ch] || self.hold_age[ch] > 1.5 {
                self.hold[ch] = p;
                self.hold_age[ch] = 0.0;
            }
            self.clip[ch] |= peak >= 1.0;
        }
    }

    fn draw(&self, painter: &egui::Painter, rect: Rect) {
        let font = FontId::proportional(9.0);
        let label_w = 12.0;
        let value_w = 76.0;
        let scale_h = 11.0;
        let bars = Rect::from_min_max(
            pos2(rect.left() + label_w, rect.top() + 4.0),
            pos2(rect.right() - value_w, rect.bottom() - scale_h),
        );
        let x_of = |db: f32| bars.left() + ((db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)).clamp(0.0, 1.0) * bars.width();
        let bar_h = (bars.height() - 3.0) / 2.0;
        for ch in 0..2 {
            let top = bars.top() + ch as f32 * (bar_h + 3.0);
            let row = Rect::from_min_size(pos2(bars.left(), top), vec2(bars.width(), bar_h));
            painter.rect_filled(row, 1.0, Color32::from_gray(28));
            let zone = |db: f32| {
                if db > 0.0 {
                    Color32::from_rgb(230, 70, 60)
                } else if db > -9.0 {
                    Color32::from_rgb(230, 200, 70)
                } else {
                    Color32::from_rgb(80, 200, 120)
                }
            };
            let peak_r = Rect::from_x_y_ranges(row.left()..=x_of(self.peak[ch]), row.y_range());
            painter.rect_filled(peak_r, 1.0, zone(self.peak[ch]).gamma_multiply(0.45));
            let rms_r = Rect::from_x_y_ranges(row.left()..=x_of(self.rms[ch]), row.y_range());
            painter.rect_filled(rms_r, 1.0, zone(self.rms[ch]));
            let hx = x_of(self.hold[ch]);
            painter.line_segment([pos2(hx, row.top()), pos2(hx, row.bottom())], Stroke::new(1.5, Color32::WHITE));
            painter.text(pos2(rect.left() + 2.0, row.center().y), Align2::LEFT_CENTER, ["L", "R"][ch], font.clone(), LABEL);
            let clip_c = if self.clip[ch] { Color32::from_rgb(240, 60, 50) } else { Color32::from_gray(45) };
            let led = Rect::from_min_size(pos2(bars.right() + 3.0, row.top()), vec2(6.0, row.height()));
            painter.rect_filled(led, 1.0, clip_c);
            let shown = |db: f32| if db < -99.0 { "-inf".to_string() } else { format!("{db:.1}") };
            painter.text(
                pos2(rect.right() - 2.0, row.center().y),
                Align2::RIGHT_CENTER,
                format!("{} / {}", shown(self.hold[ch]), shown(self.rms[ch])),
                FontId::monospace(9.0),
                Color32::from_gray(190),
            );
        }
        for db in [-60.0, -48.0, -36.0, -24.0, -12.0, -6.0, 0.0, 6.0] {
            let x = x_of(db);
            painter.line_segment([pos2(x, bars.bottom()), pos2(x, bars.bottom() + 3.0)], Stroke::new(1.0, LABEL));
            painter.text(pos2(x, rect.bottom()), Align2::CENTER_BOTTOM, format!("{db:.0}"), font.clone(), LABEL);
        }
        painter.text(
            pos2(rect.right() - 2.0, rect.bottom()),
            Align2::RIGHT_BOTTOM,
            "peak / rms",
            font,
            Color32::from_gray(80),
        );
    }
}

// ---------------------------------------------------------------------------
// Kazu Limiter display: gain-reduction meter plus its history, after the JSFX's @gfx

/// `gain` is the lowest gain applied since the last frame.
pub fn draw_limiter(ui: &mut egui::Ui, meter: &LimiterMeter, gain: f32, threshold_db: f32) {
    // `+ 0.0` turns -0.0 into 0.0 so it doesn't print as "-0.0".
    let gr_db = (-to_db(gain.max(1e-6))).max(0.0) + 0.0;
    ui.label(format!("Gain Reduction: {gr_db:.1} dB"));

    let width = 300.0;
    let (bar, _) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar, 1.0, Color32::from_gray(77));
    let frac = (gr_db / 30.0).clamp(0.0, 1.0);
    painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 1.0, Color32::from_rgb(204, 51, 51));
    let font = FontId::proportional(9.0);
    let (mut scale, _) = ui.allocate_exact_size(vec2(width, 11.0), Sense::hover());
    scale.max.x -= 1.0;
    ui.painter().text(scale.left_top(), Align2::LEFT_TOP, "0 dB", font.clone(), LABEL);
    ui.painter().text(scale.right_top(), Align2::RIGHT_TOP, "30 dB", font.clone(), LABEL);

    // History: 0 dB at the top, 24 dB of reduction at the bottom.
    let (rect, _) = ui.allocate_exact_size(vec2(width, 130.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(40));
    let plot = Rect::from_min_max(pos2(rect.left() + 34.0, rect.top() + 6.0), pos2(rect.right() - 4.0, rect.bottom() - 6.0));
    let y_of = |db_down: f32| plot.top() + (db_down / 24.0).clamp(0.0, 1.0) * plot.height();
    for k in 0..=4 {
        let db = k as f32 * 6.0;
        let y = y_of(db);
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, Color32::from_gray(64)));
        painter.text(pos2(plot.left() - 3.0, y), Align2::RIGHT_CENTER, format!("-{db:.0} dB"), font.clone(), LABEL);
    }
    for k in 0..=4 {
        let x = plot.left() + plot.width() * k as f32 / 4.0;
        painter.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, Color32::from_gray(64)));
    }
    let ty = y_of(-threshold_db);
    painter.line_segment([pos2(plot.left(), ty), pos2(plot.right(), ty)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 0, 0, 204)));

    let history = meter.history();
    let n = history.len();
    let pts: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(k, g)| pos2(plot.left() + k as f32 / (n - 1) as f32 * plot.width(), y_of(-to_db(g.max(1e-6)))))
        .collect();
    painter.line(pts, Stroke::new(1.5, Color32::from_rgb(51, 128, 230)));
}
