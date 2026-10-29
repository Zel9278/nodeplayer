//! The EQ node's body: a frequency-response graph with a draggable point per band, the
//! selected band's values below it, and the output spectrum behind the curve.

use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, FontId, Id, Mesh, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

use crate::engine::eq::{EqBand, EqSettings, FilterKind, MAX_BANDS, MAX_GAIN, MAX_Q, MAX_STAGES, MIN_Q};
use crate::engine::nodes::Tap;
use crate::visual::{FftSettings, Spectrum, read_latest};

const COLORS: [Color32; MAX_BANDS] = [
    Color32::from_rgb(255, 110, 110),
    Color32::from_rgb(255, 170, 80),
    Color32::from_rgb(235, 215, 90),
    Color32::from_rgb(130, 220, 110),
    Color32::from_rgb(90, 210, 210),
    Color32::from_rgb(110, 160, 255),
    Color32::from_rgb(180, 130, 255),
    Color32::from_rgb(255, 120, 200),
];
const GRID: Color32 = Color32::from_gray(40);
const LABEL: Color32 = Color32::from_gray(110);
const HANDLE_RADIUS: f32 = 7.0;
/// Space kept above +range and below -range.
const PAD: f32 = 8.0;

pub struct EqNode {
    pub settings: EqSettings,
    pub tap: Arc<Tap>,
    selected: Option<usize>,
    /// Where the graph was right-clicked, for "add band here".
    menu_pos: Option<Pos2>,
    spectrum: Box<Spectrum>,
    l: Vec<f32>,
    r: Vec<f32>,
    last_frame: Instant,
}

/// What a frame of the editor produced.
pub struct EqResponse {
    pub sends: Vec<(usize, f32)>,
    /// The pointer is over a band point, which takes the mouse wheel for Q.
    pub wants_scroll: bool,
}

/// Maps between frequency / gain and positions in the graph.
#[derive(Clone, Copy)]
struct Axis {
    rect: Rect,
    lo: f32,
    hi: f32,
    range: f32,
}

impl Axis {
    fn new(rect: Rect, sample_rate: f32, range: f32) -> Self {
        Self { rect, lo: 20.0, hi: 20_000f32.min(sample_rate * 0.5), range }
    }

    fn x(self, f: f32) -> f32 {
        self.rect.left() + (f / self.lo).ln() / (self.hi / self.lo).ln() * self.rect.width()
    }

    fn freq(self, x: f32) -> f32 {
        self.lo * (self.hi / self.lo).powf((x - self.rect.left()) / self.rect.width())
    }

    fn half(self) -> f32 {
        (self.rect.height() * 0.5 - PAD).max(1.0)
    }

    fn y(self, db: f32) -> f32 {
        // Far-off values stay just outside the graph so lines leaving it keep their direction.
        (self.rect.center().y - db / self.range * self.half()).clamp(self.rect.top() - 4.0, self.rect.bottom() + 4.0)
    }

    fn db(self, y: f32) -> f32 {
        (self.rect.center().y - y) / self.half() * self.range
    }

    fn handle(self, b: &EqBand) -> Pos2 {
        let db = if b.kind.has_gain() { b.gain_db } else { 0.0 };
        let inner = self.rect.shrink(2.0);
        pos2(self.x(b.freq), self.y(db)).clamp(inner.min, inner.max)
    }
}

fn fmt_freq(f: f32) -> String {
    if f >= 1000.0 { format!("{:.2} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

fn describe(i: usize, b: &EqBand) -> String {
    let mut s = format!("{} {} {}", i + 1, b.kind.label(), fmt_freq(b.freq));
    if b.kind.has_gain() {
        s += &format!(" {:+.1} dB", b.gain_db);
    }
    s += &format!(" Q {:.2}", b.q);
    if b.kind.has_slope() {
        s += &format!(" {} dB/oct", b.slope_db());
    }
    if !b.enabled {
        s += " (off)";
    }
    s
}

impl EqNode {
    pub fn new(settings: EqSettings) -> Self {
        let cfg = FftSettings { min_db: -90.0, max_db: 0.0, ..FftSettings::spectrum() };
        Self {
            settings: settings.sanitized(),
            tap: Tap::new(),
            selected: None,
            menu_pos: None,
            spectrum: Box::new(Spectrum::new(cfg)),
            l: Vec::new(),
            r: Vec::new(),
            last_frame: Instant::now(),
        }
    }

    /// `scroll` is the wheel movement taken from the graph view while a point was hovered.
    pub fn show(&mut self, ui: &mut egui::Ui, eid: u32, sample_rate: f32, scroll: f32) -> EqResponse {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;
        let before = self.settings.clone();
        let id = Id::new(("eq", eid));
        let sr = sample_rate as f64;
        let mut wants_scroll = false;
        let Self { settings: s, selected, menu_pos, spectrum, l, r, tap, .. } = self;

        ui.horizontal(|ui| {
            ui.checkbox(&mut s.bypass, "Bypass");
            ui.checkbox(&mut s.analyzer, "Analyzer");
            egui::ComboBox::from_id_salt(id.with("range"))
                .width(64.0)
                .selected_text(format!("±{} dB", s.range_db))
                .show_ui(ui, |ui| {
                    for v in [3.0, 6.0, 12.0, 18.0, 24.0, 30.0] {
                        ui.selectable_value(&mut s.range_db, v, format!("±{v} dB"));
                    }
                });
            ui.add(egui::DragValue::new(&mut s.output_db).range(-30.0..=30.0).speed(0.1).prefix("out ").suffix(" dB"));
        });

        let (rect, bg) = ui.allocate_exact_size(Vec2::from(s.size), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::from_gray(12));
        let ax = Axis::new(rect, sample_rate, s.range_db);

        if s.analyzer {
            read_latest(tap, spectrum.size(), l, r);
            spectrum.update(l, r, sample_rate, dt);
            spectrum.draw_overlay(&painter, rect, sample_rate, Color32::from_rgba_unmultiplied(110, 130, 160, 60));
        }
        draw_grid(&painter, ax);

        // Band points. Later points sit on top, so they get the pointer first.
        let shift = ui.input(|i| i.modifiers.shift);
        let mut hovered = None;
        let mut remove = None;
        for i in (0..s.bands.len()).rev() {
            let pos = ax.handle(&s.bands[i]);
            let hit = Rect::from_center_size(pos, Vec2::splat(HANDLE_RADIUS * 2.0 + 4.0));
            let resp = ui
                .interact(hit, id.with(("band", i)), Sense::click_and_drag())
                .on_hover_cursor(egui::CursorIcon::Grab);
            let b = &mut s.bands[i];
            if hovered.is_none() && (resp.hovered() || resp.dragged()) {
                hovered = Some(i);
                wants_scroll = true;
                if scroll != 0.0 {
                    b.q = (b.q * (scroll / 200.0).exp2()).clamp(MIN_Q, MAX_Q);
                }
            }
            if resp.clicked() || resp.drag_started() {
                *selected = Some(i);
            }
            if resp.double_clicked() {
                b.enabled = !b.enabled;
            }
            if resp.dragged() {
                let d = resp.drag_delta() * if shift { 0.2 } else { 1.0 };
                b.freq = (b.freq * (ax.hi / ax.lo).powf(d.x / rect.width())).clamp(ax.lo, ax.hi);
                if b.kind.has_gain() {
                    b.gain_db = (b.gain_db - d.y / ax.half() * ax.range).clamp(-MAX_GAIN, MAX_GAIN);
                } else {
                    // Cut / band filters have no gain; up and down set their Q instead.
                    b.q = (b.q * (-d.y / 40.0).exp2()).clamp(MIN_Q, MAX_Q);
                }
            }
            resp.context_menu(|ui| {
                band_menu(ui, b);
                ui.separator();
                if ui.button("Remove").clicked() {
                    remove = Some(i);
                    ui.close();
                }
            });
        }

        if bg.double_clicked() {
            if let Some(p) = bg.interact_pointer_pos()
                && s.bands.len() < MAX_BANDS
            {
                s.bands.push(EqBand::new(FilterKind::Peak, ax.freq(p.x), ax.db(p.y).clamp(-s.range_db, s.range_db)));
                *selected = Some(s.bands.len() - 1);
            }
        } else if bg.clicked() {
            *selected = None;
        }
        if bg.secondary_clicked() {
            *menu_pos = bg.interact_pointer_pos();
        }
        bg.context_menu(|ui| {
            let at = menu_pos.unwrap_or(rect.center());
            let (f, db) = (ax.freq(at.x), ax.db(at.y).clamp(-s.range_db, s.range_db));
            ui.add_enabled_ui(s.bands.len() < MAX_BANDS, |ui| {
                ui.menu_button(format!("Add band at {}", fmt_freq(f)), |ui| {
                    for k in FilterKind::ALL {
                        if ui.button(k.label()).clicked() {
                            s.bands.push(EqBand::new(k, f, if k.has_gain() { db } else { 0.0 }));
                            *selected = Some(s.bands.len() - 1);
                            ui.close();
                        }
                    }
                });
            });
            if ui.button("Flatten all gains").clicked() {
                for b in &mut s.bands {
                    b.gain_db = 0.0;
                }
                ui.close();
            }
            if ui.button("Reset").on_hover_text("back to four flat bands").clicked() {
                *s = EqSettings { size: s.size, range_db: s.range_db, analyzer: s.analyzer, ..EqSettings::default() };
                *selected = None;
                ui.close();
            }
            if ui.button("Remove all bands").clicked() {
                s.bands.clear();
                *selected = None;
                ui.close();
            }
        });

        if let Some(i) = remove {
            s.bands.remove(i);
            *selected = match *selected {
                Some(j) if j == i => None,
                Some(j) if j > i => Some(j - 1),
                other => other,
            };
        }
        if selected.is_some_and(|i| i >= s.bands.len()) {
            *selected = None;
        }

        draw_curves(&painter, ax, s, *selected, sr);
        for (i, b) in s.bands.iter().enumerate() {
            let pos = ax.handle(b);
            let lit = hovered == Some(i) || *selected == Some(i);
            let radius = if lit { HANDLE_RADIUS + 1.0 } else { HANDLE_RADIUS };
            if b.enabled {
                let ring = if *selected == Some(i) { Color32::WHITE } else { Color32::from_gray(20) };
                painter.circle(pos, radius, COLORS[i].gamma_multiply(if lit { 1.0 } else { 0.8 }), Stroke::new(1.5, ring));
                painter.text(pos, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(10.0), Color32::BLACK);
            } else {
                painter.circle(pos, radius, Color32::from_gray(25), Stroke::new(1.5, COLORS[i].gamma_multiply(0.5)));
                painter.text(pos, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(10.0), Color32::GRAY);
            }
        }

        // Readout: the point under the pointer, else the pointer's position.
        let font = FontId::proportional(11.0);
        let corner = rect.left_top() + vec2(4.0, 2.0);
        if let Some(i) = hovered {
            painter.text(corner, Align2::LEFT_TOP, describe(i, &s.bands[i]), font, COLORS[i]);
        } else if let Some(h) = bg.hover_pos() {
            let f = ax.freq(h.x);
            let text = format!("{}  {:+.1} dB  (curve {:+.1})", fmt_freq(f), ax.db(h.y), s.response_db(f as f64, sr));
            painter.text(corner, Align2::LEFT_TOP, text, font, Color32::from_gray(200));
        } else if s.bands.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "double-click to add a band", font, LABEL);
        }
        if s.bypass {
            painter.text(rect.right_top() + vec2(-4.0, 2.0), Align2::RIGHT_TOP, "BYPASS", FontId::proportional(11.0), Color32::from_rgb(255, 170, 80));
        }
        resize_handle(ui, rect, &mut s.size, id);

        match selected.and_then(|i| s.bands.get_mut(i).map(|b| (i, b))) {
            Some((i, b)) => {
                let mut delete = false;
                band_row(ui, id, i, b, sample_rate, &mut delete);
                if delete {
                    s.bands.remove(i);
                    *selected = None;
                }
            }
            None => {
                ui.small("double-click: add · drag (Shift: fine) · wheel: Q · right-click: menu");
            }
        }

        *s = std::mem::take(s).sanitized();
        EqResponse { sends: s.diff(&before), wants_scroll }
    }
}

fn band_menu(ui: &mut egui::Ui, b: &mut EqBand) {
    ui.checkbox(&mut b.enabled, "Enabled");
    ui.separator();
    for k in FilterKind::ALL {
        if ui.radio(b.kind == k, k.label()).clicked() {
            set_kind(b, k);
        }
    }
    if b.kind.has_slope() {
        ui.separator();
        for n in 1..=MAX_STAGES as u8 {
            ui.radio_value(&mut b.stages, n, format!("{} dB/oct", n as u32 * 12));
        }
    }
}

/// Switching to a cut filter starts from a flat Butterworth Q.
fn set_kind(b: &mut EqBand, k: FilterKind) {
    if b.kind != k && !k.has_gain() && b.kind.has_gain() {
        b.q = std::f32::consts::FRAC_1_SQRT_2;
    }
    b.kind = k;
}

/// The selected band's values in one row.
fn band_row(ui: &mut egui::Ui, id: Id, i: usize, b: &mut EqBand, sample_rate: f32, delete: &mut bool) {
    ui.horizontal(|ui| {
        let (chip, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
        ui.painter().circle_filled(chip.center(), 6.0, COLORS[i]);
        ui.painter().text(chip.center(), Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(9.0), Color32::BLACK);
        ui.checkbox(&mut b.enabled, "").on_hover_text("enabled");
        let mut kind = b.kind;
        egui::ComboBox::from_id_salt(id.with("kind")).width(84.0).selected_text(kind.label()).show_ui(ui, |ui| {
            for k in FilterKind::ALL {
                ui.selectable_value(&mut kind, k, k.label());
            }
        });
        set_kind(b, kind);
        if b.kind.has_slope() {
            egui::ComboBox::from_id_salt(id.with("slope"))
                .width(48.0)
                .selected_text(format!("{} dB", b.slope_db()))
                .show_ui(ui, |ui| {
                    for n in 1..=MAX_STAGES as u8 {
                        ui.selectable_value(&mut b.stages, n, format!("{} dB/oct", n as u32 * 12));
                    }
                });
        }
        let nyquist = (sample_rate * 0.49).min(20_000.0);
        let (freq_speed, q_speed) = (b.freq * 0.004, b.q * 0.01);
        ui.add(
            egui::DragValue::new(&mut b.freq)
                .range(10.0..=nyquist)
                .speed(freq_speed)
                .max_decimals(0)
                .suffix(" Hz"),
        );
        ui.add(egui::DragValue::new(&mut b.q).range(MIN_Q..=MAX_Q).speed(q_speed).max_decimals(2).prefix("Q "));
        if b.kind.has_gain() {
            ui.add(egui::DragValue::new(&mut b.gain_db).range(-MAX_GAIN..=MAX_GAIN).speed(0.1).max_decimals(1).suffix(" dB"));
        }
        if ui.small_button("✖").on_hover_text("remove band").clicked() {
            *delete = true;
        }
    });
}

fn draw_grid(painter: &egui::Painter, ax: Axis) {
    let font = FontId::proportional(9.0);
    let rect = ax.rect;
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        if f >= ax.hi {
            continue;
        }
        let x = ax.x(f);
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, GRID));
        let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        painter.text(pos2(x + 2.0, rect.bottom() - 1.0), Align2::LEFT_BOTTOM, label, font.clone(), LABEL);
    }
    let step = [1.0, 2.0, 3.0, 6.0, 12.0].into_iter().find(|s| ax.range / s <= 4.0).unwrap_or(12.0);
    let mut db = (ax.range / step).floor() * step;
    while db >= -ax.range {
        let y = ax.y(db);
        let color = if db == 0.0 { Color32::from_gray(70) } else { GRID };
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, color));
        painter.text(pos2(rect.right() - 2.0, y), Align2::RIGHT_BOTTOM, if db == 0.0 { "0".to_string() } else { format!("{db:+.0}") }, font.clone(), LABEL);
        db -= step;
    }
}

fn draw_curves(painter: &egui::Painter, ax: Axis, s: &EqSettings, selected: Option<usize>, sr: f64) {
    let cols = ((ax.rect.width() / 2.0).round() as usize).max(2);
    let xs: Vec<f32> = (0..=cols).map(|c| ax.rect.left() + c as f32 / cols as f32 * ax.rect.width()).collect();
    let freqs: Vec<f64> = xs.iter().map(|x| ax.freq(*x) as f64).collect();
    let zero = ax.y(0.0);
    let mut total = vec![0.0f64; xs.len()];
    for (i, b) in s.bands.iter().enumerate().filter(|(_, b)| b.enabled) {
        let db: Vec<f64> = freqs.iter().map(|f| b.response_db(*f, sr)).collect();
        for (t, d) in total.iter_mut().zip(&db) {
            *t += d;
        }
        let line: Vec<Pos2> = xs.iter().zip(&db).map(|(x, d)| pos2(*x, ax.y(*d as f32))).collect();
        if selected == Some(i) {
            let mut mesh = Mesh::default();
            let fill = COLORS[i].gamma_multiply(0.25);
            for (k, p) in line.iter().enumerate() {
                mesh.colored_vertex(*p, fill);
                mesh.colored_vertex(pos2(p.x, zero), fill);
                if k > 0 {
                    let v = (k as u32) * 2;
                    mesh.add_triangle(v - 2, v - 1, v);
                    mesh.add_triangle(v - 1, v + 1, v);
                }
            }
            painter.add(egui::Shape::mesh(mesh));
        }
        painter.line(line, Stroke::new(1.0, COLORS[i].gamma_multiply(0.6)));
    }
    let line: Vec<Pos2> = xs.iter().zip(&total).map(|(x, d)| pos2(*x, ax.y(*d as f32))).collect();
    let color = if s.bypass { Color32::from_gray(110) } else { Color32::from_rgb(235, 240, 255) };
    painter.line(line, Stroke::new(2.0, color));
}

pub(crate) fn resize_handle(ui: &mut egui::Ui, rect: Rect, size: &mut [f32; 2], id: Id) {
    let h = Rect::from_min_max(rect.max - vec2(12.0, 12.0), rect.max);
    let resp = ui.interact(h, id.with("resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
    let c = if resp.hovered() || resp.dragged() { Color32::from_gray(200) } else { Color32::from_gray(90) };
    for k in [4.0, 8.0] {
        ui.painter().line_segment([pos2(h.max.x - k, h.max.y - 1.0), pos2(h.max.x - 1.0, h.max.y - k)], Stroke::new(1.0, c));
    }
    if resp.dragged() {
        let v = (Vec2::from(*size) + resp.drag_delta()).clamp(vec2(200.0, 100.0), vec2(1600.0, 1000.0));
        *size = v.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_round_trips() {
        let ax = Axis::new(Rect::from_min_size(pos2(10.0, 20.0), vec2(400.0, 200.0)), 48_000.0, 12.0);
        for f in [20.0, 100.0, 1234.0, 20_000.0] {
            assert!((ax.freq(ax.x(f)) - f).abs() / f < 1e-4);
        }
        for db in [-12.0, -3.5, 0.0, 7.0, 12.0] {
            assert!((ax.db(ax.y(db)) - db).abs() < 1e-4);
        }
        assert!(ax.y(12.0) > ax.rect.top() && ax.y(-12.0) < ax.rect.bottom());
    }
}
