//! The selection loop: a stretch of the track played over and over, without a gap, and its
//! editor, a window drawing the track's waveform like an oscilloscope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use egui::{Color32, CursorIcon, PointerButton, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::library::Repeat;
use crate::player::{AudioReader, MIN_LOOP, Player};

/// The stretch of a track the selection loop plays, in seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub path: PathBuf,
    pub start: f64,
    pub end: f64,
}

impl Selection {
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

/// A selection kept under a name, to come back to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopPreset {
    pub name: String,
    pub start: f64,
    pub end: f64,
}

/// What is kept of a track: its selection and its presets.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackLoops {
    pub selection: Option<(f64, f64)>,
    pub presets: Vec<LoopPreset>,
}

/// Every track's selection and presets, by path (`loops.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopStore {
    pub tracks: BTreeMap<String, TrackLoops>,
    /// Counts changes, for saving.
    #[serde(skip)]
    pub revision: u64,
}

impl LoopStore {
    fn key(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    pub fn selection(&self, path: &Path) -> Option<Selection> {
        let (start, end) = self.tracks.get(&Self::key(path))?.selection?;
        Some(Selection { path: path.to_path_buf(), start, end })
    }

    pub fn presets(&self, path: &Path) -> &[LoopPreset] {
        self.tracks.get(&Self::key(path)).map_or(&[], |t| &t.presets)
    }

    pub fn set_selection(&mut self, path: &Path, range: Option<(f64, f64)>) {
        let key = Self::key(path);
        let t = self.tracks.entry(key.clone()).or_default();
        if t.selection != range {
            t.selection = range;
            self.revision += 1;
        }
        self.tidy(&key);
    }

    /// Keeps `range` as `name`, in place of a preset of the same name.
    pub fn save_preset(&mut self, path: &Path, name: &str, (start, end): (f64, f64)) {
        let presets = &mut self.tracks.entry(Self::key(path)).or_default().presets;
        let preset = LoopPreset { name: name.to_string(), start, end };
        match presets.iter_mut().find(|p| p.name == name) {
            Some(p) => *p = preset,
            None => presets.push(preset),
        }
        self.revision += 1;
    }

    pub fn remove_preset(&mut self, path: &Path, index: usize) {
        let key = Self::key(path);
        if let Some(t) = self.tracks.get_mut(&key)
            && index < t.presets.len()
        {
            t.presets.remove(index);
            self.revision += 1;
        }
        self.tidy(&key);
    }

    /// A track with nothing kept goes.
    fn tidy(&mut self, key: &str) {
        if self.tracks.get(key).is_some_and(|t| t.selection.is_none() && t.presets.is_empty()) {
            self.tracks.remove(key);
        }
    }
}

/// What the player loops: in the selection mode, the selection of the track loaded, or the
/// whole track when it has none.
pub fn loop_region(repeat: Repeat, selection: Option<&Selection>, path: Option<&Path>) -> Option<(f64, f64)> {
    if repeat != Repeat::Selection {
        return None;
    }
    match selection {
        Some(s) if path == Some(s.path.as_path()) && s.len() >= MIN_LOOP => Some((s.start, s.end)),
        _ => Some((0.0, f64::INFINITY)),
    }
}

// ── The waveform ───────────────────────────────────────────────────────

/// Levels of the peak pyramid; each bucket of one holds `FAN` of the level below.
const LEVELS: usize = 8;
const FAN: u64 = 4;

/// The lowest and highest sample of a stretch, left and right.
#[derive(Clone, Copy)]
struct Peak {
    lo: [f32; 2],
    hi: [f32; 2],
}

impl Peak {
    const NONE: Self = Self { lo: [f32::INFINITY; 2], hi: [f32::NEG_INFINITY; 2] };

    fn add(&mut self, l: f32, r: f32) {
        for (c, v) in [l, r].into_iter().enumerate() {
            self.lo[c] = self.lo[c].min(v);
            self.hi[c] = self.hi[c].max(v);
        }
    }

    fn merge(&mut self, o: &Peak) {
        for c in 0..2 {
            self.lo[c] = self.lo[c].min(o.lo[c]);
            self.hi[c] = self.hi[c].max(o.hi[c]);
        }
    }

    fn is_none(&self) -> bool {
        self.lo[0] > self.hi[0]
    }
}

/// The whole track's peaks, filled in as it is read.
struct Peaks {
    rate: u32,
    /// Frames to a bucket of the finest level.
    bucket: u64,
    /// Frames read so far.
    frames: u64,
    levels: Vec<Vec<Peak>>,
    partial: Peak,
    partial_len: u64,
    done: bool,
    error: Option<String>,
}

impl Peaks {
    fn new() -> Self {
        Self {
            rate: 0,
            bucket: 64,
            frames: 0,
            levels: vec![Vec::new(); LEVELS],
            partial: Peak::NONE,
            partial_len: 0,
            done: false,
            error: None,
        }
    }

    fn push(&mut self, stereo: &[f32]) {
        for fr in stereo.as_chunks::<2>().0 {
            self.partial.add(fr[0], fr[1]);
            self.partial_len += 1;
            if self.partial_len == self.bucket {
                self.complete();
            }
        }
        self.frames += (stereo.len() / 2) as u64;
    }

    fn complete(&mut self) {
        let p = std::mem::replace(&mut self.partial, Peak::NONE);
        self.partial_len = 0;
        let mut i = self.levels[0].len();
        self.levels[0].push(p);
        for level in &mut self.levels[1..] {
            i /= FAN as usize;
            match level.get_mut(i) {
                Some(q) => q.merge(&p),
                None => level.push(p),
            }
        }
    }

    fn finish(&mut self) {
        if self.partial_len > 0 {
            self.complete();
        }
        self.done = true;
    }

    /// The peak of frames `a..b`, from the coarsest level fine enough.
    fn peak(&self, a: u64, b: u64) -> Peak {
        let span = b.saturating_sub(a).max(1);
        let (mut k, mut size) = (0, self.bucket);
        while k + 1 < LEVELS && size * FAN <= span {
            k += 1;
            size *= FAN;
        }
        let level = &self.levels[k];
        let (i0, i1) = ((a / size) as usize, (b.div_ceil(size) as usize).min(level.len()));
        let mut p = Peak::NONE;
        for q in level.get(i0..i1).unwrap_or(&[]) {
            p.merge(q);
        }
        p
    }
}

/// The samples of a stretch, read when zoomed in closer than the peaks go.
struct Detail {
    /// The first frame.
    start: u64,
    /// Interleaved stereo.
    samples: Vec<f32>,
}

impl Detail {
    fn frames(&self) -> u64 {
        (self.samples.len() / 2) as u64
    }

    fn covers(&self, a: u64, b: u64) -> bool {
        a >= self.start && b <= self.start + self.frames()
    }

    fn sample(&self, frame: u64, ch: usize) -> Option<f32> {
        let i = frame.checked_sub(self.start)? as usize;
        self.samples.get(i * 2 + ch).copied()
    }
}

/// A track's waveform, read in the background.
struct Wave {
    peaks: Arc<Mutex<Peaks>>,
    cancel: Arc<AtomicBool>,
    detail: Option<Detail>,
    /// The stretch last asked for.
    asked: Option<(u64, u64)>,
    detail_tx: Sender<(u64, u64)>,
    detail_rx: Receiver<Detail>,
}

impl Wave {
    fn open(path: PathBuf, ctx: egui::Context, viewport: egui::ViewportId) -> Self {
        let peaks = Arc::new(Mutex::new(Peaks::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let (path, peaks, cancel, ctx) = (path.clone(), peaks.clone(), cancel.clone(), ctx.clone());
            let _ = std::thread::Builder::new()
                .name("waveform".into())
                .spawn(move || read_peaks(&path, &peaks, &cancel, &ctx, viewport));
        }
        let (detail_tx, rx) = crossbeam_channel::unbounded();
        let (tx, detail_rx) = crossbeam_channel::unbounded();
        {
            let _ = std::thread::Builder::new()
                .name("waveform detail".into())
                .spawn(move || read_details(&path, &rx, &tx, &ctx, viewport));
        }
        Self { peaks, cancel, detail: None, asked: None, detail_tx, detail_rx }
    }

    /// Makes sure frames `a..b` will be there to draw sample by sample.
    fn want_detail(&mut self, a: u64, b: u64) {
        if let Ok(d) = self.detail_rx.try_recv() {
            self.detail = Some(d);
        }
        while let Ok(d) = self.detail_rx.try_recv() {
            self.detail = Some(d);
        }
        if self.detail.as_ref().is_some_and(|d| d.covers(a, b)) {
            return;
        }
        if self.asked.is_some_and(|(x, y)| x <= a && b <= y) {
            return;
        }
        // Some on each side, for scrolling.
        let span = b - a;
        let ask = (a.saturating_sub(span), b + span);
        self.asked = Some(ask);
        let _ = self.detail_tx.send(ask);
    }
}

impl Drop for Wave {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn read_peaks(path: &Path, peaks: &Mutex<Peaks>, cancel: &AtomicBool, ctx: &egui::Context, viewport: egui::ViewportId) {
    let mut reader = match AudioReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            peaks.lock().unwrap().error = Some(format!("{e:#}"));
            ctx.request_repaint_of(viewport);
            return;
        }
    };
    {
        let mut p = peaks.lock().unwrap();
        p.rate = reader.sample_rate();
        // Long recordings get coarser buckets, to keep them small.
        if reader.duration().is_some_and(|d| d > 1800.0) {
            p.bucket = 256;
        }
    }
    let mut buf = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        buf.clear();
        let mut more = true;
        while more && buf.len() < 1 << 16 {
            more = match reader.read(&mut buf) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("waveform of {}: {e:#}", path.display());
                    false
                }
            };
        }
        let mut p = peaks.lock().unwrap();
        p.push(&buf);
        if !more {
            p.finish();
            ctx.request_repaint_of(viewport);
            return;
        }
    }
}

fn read_details(path: &Path, rx: &Receiver<(u64, u64)>, tx: &Sender<Detail>, ctx: &egui::Context, viewport: egui::ViewportId) {
    let mut reader: Option<AudioReader> = None;
    while let Ok(mut ask) = rx.recv() {
        // Only the latest matters.
        while let Ok(a) = rx.try_recv() {
            ask = a;
        }
        let (a, b) = ask;
        // A reader that got to the end may not seek any more: open it again then.
        let mut samples = Vec::new();
        for _ in 0..2 {
            let r = match reader.take().map_or_else(|| AudioReader::open(path), Ok) {
                Ok(r) => reader.insert(r),
                Err(e) => {
                    log::warn!("waveform of {}: {e:#}", path.display());
                    return;
                }
            };
            if r.seek(a as f64 / r.sample_rate() as f64).is_ok() {
                break;
            }
            reader = None;
        }
        let Some(r) = &mut reader else { continue };
        let want = ((b - a) * 2) as usize;
        while samples.len() < want {
            match r.read(&mut samples) {
                Ok(true) => {}
                _ => {
                    reader = None;
                    break;
                }
            }
        }
        samples.truncate(want);
        if tx.send(Detail { start: a, samples }).is_err() {
            return;
        }
        ctx.request_repaint_of(viewport);
    }
}

// ── The editor ─────────────────────────────────────────────────────────

const BACKGROUND: Color32 = Color32::from_rgb(8, 12, 10);
const GRID: Color32 = Color32::from_rgb(22, 44, 30);
const GRID_STRONG: Color32 = Color32::from_rgb(34, 70, 46);
const TRACE: Color32 = Color32::from_rgb(90, 255, 140);
const TRACE_DIM: Color32 = Color32::from_rgb(40, 120, 70);
const SELECTION: Color32 = Color32::from_rgba_premultiplied(40, 70, 110, 90);
const EDGE: Color32 = Color32::from_rgb(110, 170, 255);
const PLAYHEAD: Color32 = Color32::from_rgb(255, 90, 80);

/// How close (points) the pointer must be to grab a selection edge, or snap to a beat.
const GRAB: f32 = 6.0;

#[derive(Clone, Default)]
enum Drag {
    #[default]
    None,
    /// Making the selection, from `anchor` to the pointer; `previous` comes back if it ends
    /// up too short (a click).
    Select { anchor: f64, previous: Option<Selection> },
    /// Moving it whole, grabbed at `grab`.
    Move { grab: f64, start: f64, end: f64 },
    /// Scrolling the view (middle button).
    Pan,
    /// Seeking from the ruler, to `at` once let go.
    Seek { at: f64 },
}

/// Time to x and back, over `rect`.
#[derive(Clone, Copy)]
struct Axis {
    t0: f64,
    t1: f64,
    rect: Rect,
}

impl Axis {
    fn x(&self, t: f64) -> f32 {
        self.rect.left() + ((t - self.t0) / (self.t1 - self.t0)) as f32 * self.rect.width()
    }

    fn t(&self, x: f32) -> f64 {
        self.t0 + ((x - self.rect.left()) / self.rect.width()) as f64 * (self.t1 - self.t0)
    }

    fn per_point(&self) -> f64 {
        (self.t1 - self.t0) / self.rect.width().max(1.0) as f64
    }
}

pub struct LoopEditor {
    pub open: bool,
    wave: Option<Wave>,
    /// The track the view is of.
    track: Option<PathBuf>,
    /// What is shown, in seconds; fitted to the track once its length is known.
    view: (f64, f64),
    fitted: bool,
    pub follow: bool,
    /// Vertical zoom.
    gain: f32,
    /// Selection edges snap to the beats (MIDI files).
    pub snap: bool,
    drag: Drag,
    /// The repeat mode turning the loop off here goes back to.
    repeat_before: Repeat,
    /// The name typed for a new preset.
    preset_name: String,
}

impl Default for LoopEditor {
    fn default() -> Self {
        Self {
            open: false,
            wave: None,
            track: None,
            view: (0.0, 1.0),
            fitted: false,
            follow: true,
            gain: 1.0,
            snap: true,
            drag: Drag::None,
            repeat_before: Repeat::Off,
            preset_name: String::new(),
        }
    }
}

impl LoopEditor {
    /// The selection is being dragged: the player gets it once let go.
    pub fn dragging(&self) -> bool {
        matches!(self.drag, Drag::Select { .. } | Drag::Move { .. })
    }

    /// Turns the selection loop on or off; off goes back to the repeat mode before.
    pub fn toggle_loop(&mut self, repeat: &mut Repeat) {
        if *repeat == Repeat::Selection {
            *repeat = if self.repeat_before == Repeat::Selection { Repeat::Off } else { self.repeat_before };
        } else {
            self.repeat_before = *repeat;
            *repeat = Repeat::Selection;
        }
    }

    /// The editor's window, while open. Call every pass.
    pub fn show(&mut self, ctx: &egui::Context, player: &Player, selection: &mut Option<Selection>, repeat: &mut Repeat, store: &mut LoopStore) {
        if !self.open {
            self.wave = None;
            self.track = None;
            return;
        }
        let id = egui::ViewportId::from_hash_of("loop editor");
        let status = player.status();
        let midi = player.is_midi();
        let duration = status.duration.unwrap_or(0.0);
        if status.path != self.track {
            self.track = status.path.clone();
            self.wave = self.track.clone().filter(|_| !midi).map(|p| Wave::open(p, ctx.clone(), id));
            self.fitted = false;
        }
        if !self.fitted && duration > 0.0 {
            self.fitted = true;
            self.view = match selection.as_ref().filter(|s| Some(&s.path) == self.track.as_ref()) {
                Some(s) => around(s.start, s.end, duration),
                None => (0.0, duration),
            };
        }
        let title = status.title().map_or_else(|| "Loop editor".to_string(), |t| format!("{t} — Loop editor"));
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id("nodeplayer")
            .with_inner_size([1000.0, 460.0])
            .with_min_inner_size([480.0, 260.0]);
        ctx.show_viewport_immediate(id, builder, |ui, _| {
            let ctx = ui.ctx().clone();
            if ctx.input(|i| i.viewport().close_requested()) {
                self.open = false;
                return;
            }
            let path = self.track.clone();
            self.keys(&ctx, player, selection, repeat, path.as_deref(), duration, store);
            egui::Panel::top("loop tools").show(ui, |ui| {
                ui.add_space(4.0);
                self.toolbar(ui, player, selection, repeat, path.as_deref(), duration, midi);
                ui.add_space(2.0);
            });
            if let Some(path) = path.as_deref() {
                egui::Panel::right("loop presets").resizable(true).default_size(230.0).min_size(170.0).show(ui, |ui| {
                    self.presets(ui, selection, path, duration, store);
                });
            }
            egui::Panel::bottom("loop hints").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(
                        "Drag to select · drag an edge to change it · Ctrl+drag moves the selection · click to seek · \
                         wheel zooms · Shift+wheel or middle-drag scrolls · Ctrl+wheel scales the height · \
                         Space play/pause · L loop · A/B start/end at the playhead · Z zoom to the selection · F fit · Esc clear · 1–9 presets",
                    );
                });
            });
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BACKGROUND)).show(ui, |ui| {
                self.scope(ui, player, selection, path.as_deref(), duration, midi);
            });
            if player.is_playing() || self.wave.as_ref().is_some_and(|w| !w.peaks.lock().unwrap().done) {
                ctx.request_repaint_after(Duration::from_millis(16));
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn keys(&mut self, ctx: &egui::Context, player: &Player, selection: &mut Option<Selection>, repeat: &mut Repeat, path: Option<&Path>, duration: f64, store: &LoopStore) {
        if ctx.text_edit_focused() {
            return;
        }
        let pressed = |key| ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
        use egui::Key;
        if pressed(Key::Space) && path.is_some() {
            player.set_playing(!player.is_playing());
        }
        if pressed(Key::L) {
            self.toggle_loop(repeat);
        }
        let Some(path) = path else { return };
        let pos = player.position();
        if pressed(Key::A) {
            set_start(selection, path, pos, duration);
        }
        if pressed(Key::B) {
            set_end(selection, path, pos);
        }
        if pressed(Key::Escape) || pressed(Key::Delete) {
            *selection = None;
        }
        if pressed(Key::F) && duration > 0.0 {
            self.view = (0.0, duration);
        }
        if pressed(Key::Z)
            && let Some(s) = selection.as_ref().filter(|s| s.path == path)
        {
            self.view = around(s.start, s.end, duration);
        }
        const NUMBERS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
        for (i, key) in NUMBERS.into_iter().enumerate() {
            if pressed(key)
                && let Some(p) = store.presets(path).get(i)
            {
                self.apply(selection, path, p, duration);
            }
        }
    }

    /// Makes `preset` the selection, and shows it.
    fn apply(&mut self, selection: &mut Option<Selection>, path: &Path, preset: &LoopPreset, duration: f64) {
        let end = preset.end.min(duration.max(preset.start + MIN_LOOP));
        *selection = Some(Selection { path: path.to_path_buf(), start: preset.start, end });
        let span = self.view.1 - self.view.0;
        // Shown whole: the view moves only when it doesn't show it already, or by far.
        if preset.start < self.view.0 || end > self.view.1 || (end - preset.start) < span * 0.05 {
            self.view = around(preset.start, end, duration);
        }
    }

    /// The track's presets: click one to select it; the selection is kept under a name.
    fn presets(&mut self, ui: &mut egui::Ui, selection: &mut Option<Selection>, path: &Path, duration: f64, store: &mut LoopStore) {
        ui.add_space(4.0);
        ui.strong("Presets for this track");
        ui.add_space(2.0);
        let current = selection.as_ref().filter(|s| s.path == path).map(|s| (s.start, s.end));
        let presets = store.presets(path).to_vec();
        let (mut chosen, mut remove) = (None, None);
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 70.0).show(ui, |ui| {
            if presets.is_empty() {
                ui.weak("None yet. Select a stretch, name it below and press Save.");
            }
            for (i, p) in presets.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text("delete").clicked() {
                        remove = Some(i);
                    }
                    let on = current.is_some_and(|(a, b)| (a - p.start).abs() < 1e-6 && (b - p.end).abs() < 1e-6);
                    let key = if i < 9 { format!("{} ", i + 1) } else { String::new() };
                    let r = ui.selectable_label(on, format!("{key}{}", p.name)).on_hover_text(format!(
                        "{} – {} ({})\nclick to select it{}",
                        precise_time(p.start),
                        precise_time(p.end),
                        precise_time(p.end - p.start),
                        if i < 9 { format!("; key {}", i + 1) } else { String::new() }
                    ));
                    if r.clicked() {
                        chosen = Some(i);
                    }
                });
                ui.weak(format!("    {} – {}", precise_time(p.start), precise_time(p.end)));
            }
        });
        if let Some(i) = chosen {
            self.apply(selection, path, &presets[i], duration);
        }
        if let Some(i) = remove {
            store.remove_preset(path, i);
        }
        ui.separator();
        let fallback = format!("Loop {}", presets.len() + 1);
        let r = ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text(fallback.as_str()).desired_width(f32::INFINITY));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let replaces = presets.iter().any(|p| p.name == self.preset_name.trim());
        let label = if replaces { "Save (replace)" } else { "Save selection" };
        let save = ui.add_enabled(current.is_some(), egui::Button::new(label)).on_disabled_hover_text("select a stretch first");
        if let Some(range) = current
            && (save.clicked() || enter)
        {
            let name = self.preset_name.trim();
            store.save_preset(path, if name.is_empty() { &fallback } else { name }, range);
            self.preset_name.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn toolbar(&mut self, ui: &mut egui::Ui, player: &Player, selection: &mut Option<Selection>, repeat: &mut Repeat, path: Option<&Path>, duration: f64, midi: bool) {
        ui.horizontal_wrapped(|ui| {
            let playing = player.is_playing();
            if ui.add_enabled(path.is_some(), egui::Button::new(if playing { "⏸" } else { "▶" })).on_hover_text("Space").clicked() {
                player.set_playing(!playing);
            }
            let looping = *repeat == Repeat::Selection;
            let r = ui.selectable_label(looping, "🔂 Loop selection").on_hover_text(
                "play the selection over and over (L); without one, the whole track\nthe same as the repeat button's \"selection\" mode",
            );
            if r.clicked() {
                self.toggle_loop(repeat);
            }
            ui.separator();
            let Some(path) = path else {
                ui.weak("Nothing is loaded.");
                return;
            };
            let pos = player.position();
            let span = self.view.1 - self.view.0;
            match selection.as_mut().filter(|s| s.path == path) {
                Some(s) => {
                    let end = s.end;
                    ui.label("Start");
                    ui.add(time_value(&mut s.start, 0.0..=(end - MIN_LOOP).max(0.0), span));
                    let start = s.start;
                    ui.label("End");
                    ui.add(time_value(&mut s.end, (start + MIN_LOOP)..=duration.max(start + MIN_LOOP), span));
                    ui.weak(format!("length {}", precise_time(s.len())));
                }
                None => {
                    ui.weak("No selection");
                }
            }
            if ui.button("Set start").on_hover_text("to the playhead (A)").clicked() {
                set_start(selection, path, pos, duration);
            }
            if ui.button("Set end").on_hover_text("to the playhead (B)").clicked() {
                set_end(selection, path, pos);
            }
            let has = selection.as_ref().is_some_and(|s| s.path == path);
            if ui.add_enabled(has, egui::Button::new("Clear")).on_hover_text("Esc").clicked() {
                *selection = None;
            }
            ui.separator();
            if ui.add_enabled(duration > 0.0, egui::Button::new("Fit")).on_hover_text("the whole track (F)").clicked() {
                self.view = (0.0, duration);
            }
            if ui.add_enabled(has, egui::Button::new("Zoom to selection")).on_hover_text("Z").clicked()
                && let Some(s) = selection.as_ref()
            {
                self.view = around(s.start, s.end, duration);
            }
            ui.checkbox(&mut self.follow, "Follow playback");
            if midi {
                ui.checkbox(&mut self.snap, "Snap to beats");
            }
        });
    }

    fn scope(&mut self, ui: &mut egui::Ui, player: &Player, selection: &mut Option<Selection>, path: Option<&Path>, duration: f64, midi: bool) {
        let full = ui.available_rect_before_wrap().shrink2(vec2(6.0, 4.0));
        let overview_rect = Rect::from_min_size(full.min, vec2(full.width(), 40.0));
        let ruler_rect = Rect::from_min_size(pos2(full.left(), overview_rect.bottom() + 6.0), vec2(full.width(), 20.0));
        let scope_rect = Rect::from_min_max(pos2(full.left(), ruler_rect.bottom()), full.max);
        if scope_rect.height() < 20.0 || full.width() < 50.0 {
            return;
        }
        let painter = ui.painter_at(full.expand(4.0));
        let Some(path) = path else {
            painter.text(scope_rect.center(), egui::Align2::CENTER_CENTER, "Play a track to edit its loop.", egui::FontId::proportional(15.0), TRACE_DIM);
            return;
        };
        let duration = duration.max(MIN_LOOP);
        self.clamp_view(duration);
        let pos = player.position();
        let playing = player.is_playing();
        let current = selection.as_ref().filter(|s| s.path == path).map(|s| (s.start, s.end));

        if self.follow && playing && matches!(self.drag, Drag::None) {
            let span = self.view.1 - self.view.0;
            if pos < self.view.0 || pos > self.view.1 - span * 0.02 {
                self.view = (pos - span * 0.1, pos + span * 0.9);
                self.clamp_view(duration);
            }
        }

        // ── Overview: the whole track and where the view is.
        let whole = Axis { t0: 0.0, t1: duration, rect: overview_rect };
        painter.rect_filled(overview_rect, 2.0, Color32::from_rgb(14, 20, 17));
        if let Some((a, b)) = current {
            painter.rect_filled(Rect::from_x_y_ranges(whole.x(a)..=whole.x(b), overview_rect.y_range()), 0.0, SELECTION);
        }
        let peaks = self.wave.as_ref().map(|w| w.peaks.clone());
        if let Some(peaks) = &peaks {
            let p = peaks.lock().unwrap();
            draw_columns(&painter, overview_rect, &whole, &p, None, None, 1.0, TRACE_DIM);
        }
        let view_rect = Rect::from_x_y_ranges(whole.x(self.view.0)..=whole.x(self.view.1).max(whole.x(self.view.0) + 2.0), overview_rect.y_range());
        painter.rect_stroke(view_rect, 1.0, Stroke::new(1.0, Color32::from_gray(200)), egui::StrokeKind::Inside);
        painter.vline(whole.x(pos), overview_rect.y_range(), Stroke::new(1.0, PLAYHEAD));
        let ov = ui.interact(overview_rect, ui.id().with("overview"), Sense::click_and_drag());
        if (ov.clicked() || ov.dragged()) && let Some(p) = ov.interact_pointer_pos() {
            let span = self.view.1 - self.view.0;
            let t = whole.t(p.x);
            self.view = (t - span / 2.0, t + span / 2.0);
            self.clamp_view(duration);
        }
        ov.on_hover_cursor(CursorIcon::PointingHand);

        let axis = Axis { t0: self.view.0, t1: self.view.1, rect: scope_rect };

        // ── Ruler.
        let step = grid_step(axis.per_point() * 90.0);
        painter.rect_filled(ruler_rect, 0.0, Color32::from_rgb(14, 20, 17));
        let mut t = (axis.t0 / step).ceil() * step;
        while t <= axis.t1 {
            let x = axis.x(t);
            painter.vline(x, ruler_rect.bottom() - 6.0..=ruler_rect.bottom(), Stroke::new(1.0, GRID_STRONG));
            painter.vline(x, scope_rect.y_range(), Stroke::new(1.0, GRID));
            painter.text(pos2(x + 3.0, ruler_rect.top() + 2.0), egui::Align2::LEFT_TOP, ruler_label(t, step), egui::FontId::monospace(10.0), Color32::from_gray(150));
            t += step;
        }

        // ── Beats of a MIDI file.
        let mut beats = Vec::new();
        if midi && let Some(file) = player.midi().player.file() {
            let mut lines = Vec::new();
            let (b0, b1) = (file.beat_at(axis.t0), file.beat_at(axis.t1));
            if file.beat_lines(b0, b1, 2000, &mut lines) {
                for (beat, bar) in lines {
                    let t = file.seconds_at_beat(beat);
                    beats.push(t);
                    // Too dense to tell apart: bars only.
                    if bar || (b1 - b0) < scope_rect.width() as f64 / 8.0 {
                        let color = if bar { GRID_STRONG } else { GRID };
                        painter.vline(axis.x(t), scope_rect.y_range(), Stroke::new(1.0, color));
                    }
                }
            }
            painter.text(scope_rect.center(), egui::Align2::CENTER_CENTER, "MIDI files have no waveform: the lines are bars and beats", egui::FontId::proportional(13.0), TRACE_DIM);
        }

        // ── Selection, waveform, playhead.
        if let Some((a, b)) = current {
            painter.rect_filled(Rect::from_x_y_ranges(axis.x(a)..=axis.x(b), scope_rect.y_range()), 0.0, SELECTION);
        }
        let lanes = [
            Rect::from_min_max(scope_rect.min, pos2(scope_rect.right(), scope_rect.center().y)),
            Rect::from_min_max(pos2(scope_rect.left(), scope_rect.center().y), scope_rect.max),
        ];
        for (ch, lane) in lanes.iter().enumerate() {
            let c = lane.center().y;
            painter.hline(lane.x_range(), c, Stroke::new(1.0, GRID_STRONG));
            for v in [-0.5f32, 0.5] {
                painter.hline(lane.x_range(), c - v * lane.height() / 2.0 * self.gain, Stroke::new(1.0, GRID));
            }
            if !midi {
                painter.text(lane.left_top() + vec2(4.0, 2.0), egui::Align2::LEFT_TOP, ["L", "R"][ch], egui::FontId::monospace(10.0), TRACE_DIM);
            }
        }
        painter.hline(scope_rect.x_range(), scope_rect.center().y, Stroke::new(1.0, GRID_STRONG));
        if let Some(wave) = &mut self.wave {
            let p = wave.peaks.lock().unwrap();
            if let Some(e) = &p.error {
                painter.text(scope_rect.center(), egui::Align2::CENTER_CENTER, format!("Can't read the waveform: {e}"), egui::FontId::proportional(13.0), Color32::LIGHT_RED);
            } else if p.rate > 0 {
                let rate = p.rate as f64;
                let frames_per_point = axis.per_point() * rate;
                let (a, b) = ((axis.t0 * rate).floor().max(0.0) as u64, (axis.t1 * rate).ceil() as u64 + 1);
                let close = frames_per_point < p.bucket as f64;
                drop(p);
                if close {
                    wave.want_detail(a, b);
                }
                let p = wave.peaks.lock().unwrap();
                let detail = wave.detail.as_ref().filter(|d| close && d.covers(a, b.min(p.frames.max(d.start + d.frames()))));
                for (ch, lane) in lanes.iter().enumerate() {
                    let lane = lane.shrink2(vec2(0.0, 2.0));
                    // Bright inside the selection, dimmer outside.
                    for (clip, color) in trace_colors(lane, &axis, current) {
                        let painter = painter.with_clip_rect(clip.intersect(lane.expand2(vec2(0.0, 1.0))));
                        match detail {
                            Some(d) if frames_per_point < 1.5 => draw_samples(&painter, lane, &axis, d, rate, ch, self.gain, color),
                            _ => draw_columns(&painter, lane, &axis, &p, detail, Some(ch), self.gain, color),
                        }
                    }
                }
                if !p.done {
                    let read = p.frames as f64 / rate;
                    painter.text(scope_rect.right_top() + vec2(-6.0, 4.0), egui::Align2::RIGHT_TOP, format!("reading… {}", crate::midifile::format_time(read)), egui::FontId::proportional(11.0), TRACE_DIM);
                }
            }
        }
        if let Some((a, b)) = current {
            for (t, label) in [(a, "A"), (b, "B")] {
                let x = axis.x(t);
                if scope_rect.x_range().contains(x) {
                    painter.vline(x, scope_rect.y_range(), Stroke::new(1.5, EDGE));
                    let flag = Rect::from_min_size(pos2(x - 7.0, scope_rect.top()), vec2(14.0, 14.0));
                    painter.rect_filled(flag, 2.0, EDGE);
                    painter.text(flag.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::monospace(10.0), Color32::BLACK);
                }
            }
        }
        if scope_rect.x_range().contains(axis.x(pos)) {
            painter.vline(axis.x(pos), ruler_rect.top()..=scope_rect.bottom(), Stroke::new(1.5, PLAYHEAD));
        }

        // ── Ruler: click or drag to seek.
        let ruler = ui.interact(ruler_rect, ui.id().with("ruler"), Sense::click_and_drag());
        if let Some(p) = ruler.interact_pointer_pos() {
            let at = axis.t(p.x).clamp(0.0, duration);
            if ruler.dragged() {
                self.drag = Drag::Seek { at };
            }
            if ruler.clicked() {
                player.seek(at);
            }
        }
        if ruler.drag_stopped() && let Drag::Seek { at } = std::mem::take(&mut self.drag) {
            player.seek(at);
        }
        if let Drag::Seek { at } = self.drag {
            painter.vline(axis.x(at), ruler_rect.top()..=scope_rect.bottom(), Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 90, 80, 120)));
        }
        let ruler = ruler.on_hover_cursor(CursorIcon::PointingHand);
        ruler.on_hover_text_at_pointer("click or drag to seek");

        // ── Scope: select, zoom, scroll.
        let resp = ui.interact(scope_rect, ui.id().with("scope"), Sense::click_and_drag());
        let snapping = midi && self.snap;
        let snap = |t: f64| -> f64 {
            if !snapping {
                return t;
            }
            let near = beats.iter().copied().min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()));
            match near {
                Some(b) if ((b - t) / axis.per_point()).abs() < GRAB as f64 => b,
                _ => t,
            }
        };
        let edge_at = |x: f32| -> Option<f64> {
            let (a, b) = current?;
            let (da, db) = ((axis.x(a) - x).abs(), (axis.x(b) - x).abs());
            // The edge nearer the pointer; the other one stays put.
            (da.min(db) < GRAB).then_some(if da <= db { b } else { a })
        };
        let ctrl = ui.input(|i| i.modifiers.command);
        if let Some(h) = resp.hover_pos() {
            let inside = current.is_some_and(|(a, b)| (a..b).contains(&axis.t(h.x)));
            if matches!(self.drag, Drag::Move { .. }) || (ctrl && inside) {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            } else if matches!(self.drag, Drag::Select { .. }) || edge_at(h.x).is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            // The time under the pointer.
            let t = axis.t(h.x);
            painter.text(pos2(h.x + 6.0, scope_rect.bottom() - 4.0), egui::Align2::LEFT_BOTTOM, precise_time(t), egui::FontId::monospace(10.0), Color32::from_gray(170));

            let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta(), i.zoom_delta()));
            if scroll.y != 0.0 {
                let factor = (-scroll.y as f64 * 0.003).exp();
                let min_span = (64.0 / self.wave_rate().unwrap_or(48_000.0)).max(0.0005);
                let span = ((self.view.1 - self.view.0) * factor).clamp(min_span, duration * 1.05);
                let k = (t - self.view.0) / (self.view.1 - self.view.0);
                self.view = (t - span * k, t - span * k + span);
                self.clamp_view(duration);
            }
            if scroll.x != 0.0 {
                let dt = -scroll.x as f64 * axis.per_point();
                self.view = (self.view.0 + dt, self.view.1 + dt);
                self.clamp_view(duration);
            }
            if zoom != 1.0 {
                self.gain = (self.gain * zoom).clamp(0.25, 64.0);
            }
        }
        if resp.drag_started_by(PointerButton::Primary) {
            let origin = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or_default();
            let t = axis.t(origin.x).clamp(0.0, duration);
            self.drag = match (current, edge_at(origin.x)) {
                (Some((a, b)), _) if ctrl && (a..b).contains(&t) => Drag::Move { grab: t, start: a, end: b },
                (_, Some(other)) => Drag::Select { anchor: other, previous: selection.clone() },
                _ => Drag::Select { anchor: snap(t), previous: selection.clone() },
            };
        }
        if resp.drag_started_by(PointerButton::Middle) {
            self.drag = Drag::Pan;
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let t = axis.t(p.x).clamp(0.0, duration);
            match &self.drag {
                Drag::Select { anchor, .. } => {
                    let t = snap(t);
                    let (start, end) = if t < *anchor { (t, *anchor) } else { (*anchor, t) };
                    *selection = Some(Selection { path: path.to_path_buf(), start, end });
                }
                Drag::Move { grab, start, end } => {
                    let d = (t - grab).clamp(-start, duration - end);
                    let (a, b) = (snap(start + d), snap(end + d));
                    // Snapped, the length stays as it was.
                    let (a, b) = if a != start + d { (a, a + (end - start)) } else if b != end + d { (b - (end - start), b) } else { (a, b) };
                    *selection = Some(Selection { path: path.to_path_buf(), start: a.max(0.0), end: b.min(duration) });
                }
                Drag::Pan => {
                    let dt = -resp.drag_delta().x as f64 * axis.per_point();
                    self.view = (self.view.0 + dt, self.view.1 + dt);
                    self.clamp_view(duration);
                }
                _ => {}
            }
        }
        if resp.drag_stopped() {
            if let Drag::Select { anchor, previous } = std::mem::take(&mut self.drag)
                && selection.as_ref().is_none_or(|s| s.len() < MIN_LOOP)
            {
                *selection = previous;
                player.seek(anchor);
            }
            self.drag = Drag::None;
        }
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            player.seek(axis.t(p.x).clamp(0.0, duration));
        }
    }

    fn wave_rate(&self) -> Option<f64> {
        let w = self.wave.as_ref()?;
        let rate = w.peaks.lock().unwrap().rate;
        (rate > 0).then_some(rate as f64)
    }

    fn clamp_view(&mut self, duration: f64) {
        let span = (self.view.1 - self.view.0).clamp(0.0005, duration.max(0.0005) * 1.05);
        let t0 = self.view.0.clamp(-span * 0.02, (duration - span * 0.98).max(-span * 0.02));
        self.view = (t0, t0 + span);
    }
}

/// A view of `a..b` with some room around it.
fn around(a: f64, b: f64, duration: f64) -> (f64, f64) {
    let pad = (b - a) * 0.1;
    ((a - pad).max(0.0), (b + pad).min(duration.max(b)))
}

/// Sets the selection's start, or starts one from `pos` to the end.
fn set_start(selection: &mut Option<Selection>, path: &Path, pos: f64, duration: f64) {
    match selection.as_mut().filter(|s| s.path == path) {
        Some(s) if pos < s.end - MIN_LOOP => s.start = pos.max(0.0),
        _ if pos < duration - MIN_LOOP => *selection = Some(Selection { path: path.to_path_buf(), start: pos.max(0.0), end: duration }),
        _ => {}
    }
}

/// Sets the selection's end, or makes one from the start to `pos`.
fn set_end(selection: &mut Option<Selection>, path: &Path, pos: f64) {
    match selection.as_mut().filter(|s| s.path == path) {
        Some(s) if pos > s.start + MIN_LOOP => s.end = pos,
        _ if pos > MIN_LOOP => *selection = Some(Selection { path: path.to_path_buf(), start: 0.0, end: pos }),
        _ => {}
    }
}

fn lane_y(lane: Rect, v: f32, gain: f32) -> f32 {
    (lane.center().y - v * lane.height() / 2.0 * gain).clamp(lane.top(), lane.bottom())
}

/// Where the trace is bright (the selection) and where dim.
fn trace_colors(lane: Rect, axis: &Axis, selection: Option<(f64, f64)>) -> Vec<(Rect, Color32)> {
    let Some((a, b)) = selection else { return vec![(lane, TRACE)] };
    let (xa, xb) = (axis.x(a).clamp(lane.left(), lane.right()), axis.x(b).clamp(lane.left(), lane.right()));
    [(lane.left(), xa, TRACE_DIM), (xa, xb, TRACE), (xb, lane.right(), TRACE_DIM)]
        .into_iter()
        .filter(|(l, r, _)| r > l)
        .map(|(l, r, c)| (Rect::from_x_y_ranges(l..=r, lane.y_range()), c))
        .collect()
}

/// The waveform as a column a point wide for each point, from its lowest sample to its
/// highest: from the samples when there, else from the peaks. `ch` `None` draws both
/// channels together.
#[allow(clippy::too_many_arguments)]
fn draw_columns(painter: &egui::Painter, lane: Rect, axis: &Axis, peaks: &Peaks, detail: Option<&Detail>, ch: Option<usize>, gain: f32, color: Color32) {
    let rate = peaks.rate as f64;
    if rate == 0.0 {
        return;
    }
    let clip = painter.clip_rect();
    let x0 = lane.left().max(clip.left()).max(axis.x(0.0)).floor() as i32;
    let x1 = lane.right().min(clip.right()).min(axis.x(peaks.frames as f64 / rate)).ceil() as i32;
    let mut mesh = egui::Mesh::default();
    for x in x0..x1 {
        let fa = (axis.t(x as f32) * rate).max(0.0) as u64;
        let fb = ((axis.t(x as f32 + 1.0) * rate).max(0.0) as u64).max(fa + 1);
        let p = match detail {
            Some(d) if d.covers(fa, fb) => {
                let mut p = Peak::NONE;
                for f in fa..fb {
                    if let (Some(l), Some(r)) = (d.sample(f, 0), d.sample(f, 1)) {
                        p.add(l, r);
                    }
                }
                p
            }
            _ => peaks.peak(fa, fb),
        };
        if p.is_none() {
            continue;
        }
        let (lo, hi) = match ch {
            Some(c) => (p.lo[c], p.hi[c]),
            None => (p.lo[0].min(p.lo[1]), p.hi[0].max(p.hi[1])),
        };
        let (top, bottom) = (lane_y(lane, hi, gain), lane_y(lane, lo, gain));
        mesh.add_colored_rect(Rect::from_min_max(pos2(x as f32, top), pos2(x as f32 + 1.0, bottom.max(top + 1.0))), color);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Close up: the samples joined by a line, and as dots when far apart.
#[allow(clippy::too_many_arguments)]
fn draw_samples(painter: &egui::Painter, lane: Rect, axis: &Axis, detail: &Detail, rate: f64, ch: usize, gain: f32, color: Color32) {
    let clip = painter.clip_rect();
    let (t0, t1) = (axis.t(clip.left()).max(axis.t0), axis.t(clip.right()).min(axis.t1));
    // One sample beyond each side, so the line runs to the edges.
    let (fa, fb) = (((t0 * rate).floor() - 1.0).max(0.0) as u64, (t1 * rate).ceil() as u64 + 2);
    let points: Vec<_> = (fa..fb)
        .filter_map(|f| detail.sample(f, ch).map(|v| pos2(axis.x(f as f64 / rate), lane_y(lane, v, gain))))
        .collect();
    if axis.per_point() * rate < 0.2 {
        for &pt in &points {
            painter.circle_filled(pt, 2.2, color);
        }
    }
    painter.add(egui::Shape::line(points, Stroke::new(1.2, color)));
}

/// A round time step for grid lines at least `min` seconds apart.
fn grid_step(min: f64) -> f64 {
    const STEPS: [f64; 22] = [
        0.0001, 0.0002, 0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ];
    STEPS.iter().copied().find(|s| *s >= min).unwrap_or(1800.0)
}

fn ruler_label(t: f64, step: f64) -> String {
    let t = t.max(0.0);
    let (m, s) = ((t / 60.0).floor(), t % 60.0);
    let decimals = if step >= 1.0 { 0 } else { (-step.log10()).ceil() as usize };
    if decimals == 0 {
        format!("{}:{:02}", m as u64, s.round() as u64)
    } else {
        format!("{}:{:0w$.p$}", m as u64, s, w = decimals + 3, p = decimals)
    }
}

/// `m:ss.mmm`.
pub fn precise_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// Reads `m:ss.mmm`, `h:mm:ss.mmm` or seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let mut t = 0.0;
    for part in s.trim().split(':') {
        let v: f64 = part.trim().parse().ok().filter(|v: &f64| *v >= 0.0)?;
        t = t * 60.0 + v;
    }
    Some(t)
}

/// A time to drag or type, shown as `m:ss.mmm`.
fn time_value(value: &mut f64, range: std::ops::RangeInclusive<f64>, span: f64) -> egui::DragValue<'_> {
    egui::DragValue::new(value)
        .range(range)
        .speed((span / 800.0).max(0.0001))
        .custom_formatter(|v, _| precise_time(v))
        .custom_parser(parse_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_back() {
        assert_eq!(precise_time(83.0456), "1:23.046");
        assert!((parse_time("1:23.046").unwrap() - 83.046).abs() < 1e-9);
        assert_eq!(parse_time("1:00:00"), Some(3600.0));
        assert_eq!(parse_time("12.5"), Some(12.5));
        assert_eq!(parse_time("-1"), None);
        assert_eq!(parse_time("a:b"), None);
    }

    #[test]
    fn peaks_from_every_level_agree() {
        let mut p = Peaks::new();
        p.rate = 48_000;
        let frames: Vec<f32> = (0..100_000).flat_map(|i| {
            let v = ((i as f32) * 0.001).sin();
            [v, -v * 0.5]
        }).collect();
        // Pushed in uneven pieces, as packets come.
        for piece in frames.chunks(2 * 1153) {
            p.push(piece);
        }
        p.finish();
        assert_eq!(p.frames, 100_000);
        for (a, b) in [(0u64, 64u64), (1000, 9000), (0, 100_000), (65_536, 99_999)] {
            let got = p.peak(a, b);
            // Bucket edges round outwards: at least the exact range, at most a bucket each
            // way more of it.
            let exact = |a: u64, b: u64| {
                let mut e = Peak::NONE;
                for f in a as usize..b as usize {
                    e.add(frames[f * 2], frames[f * 2 + 1]);
                }
                e
            };
            let (inner, outer) = (exact(a, b), exact(a.saturating_sub(a % p.bucket), b.next_multiple_of(p.bucket * FAN.pow(LEVELS as u32 - 1)).min(100_000)));
            for c in 0..2 {
                assert!(got.hi[c] >= inner.hi[c] && got.hi[c] <= outer.hi[c], "{a}..{b} ch {c}: high {} not in {}..{}", got.hi[c], inner.hi[c], outer.hi[c]);
                assert!(got.lo[c] <= inner.lo[c] && got.lo[c] >= outer.lo[c], "{a}..{b} ch {c}: low");
            }
        }
    }

    #[test]
    fn the_loop_is_the_selection_of_the_track_playing() {
        let sel = Selection { path: "/a.flac".into(), start: 1.0, end: 2.0 };
        let a = Path::new("/a.flac");
        assert_eq!(loop_region(Repeat::One, Some(&sel), Some(a)), None);
        assert_eq!(loop_region(Repeat::Selection, Some(&sel), Some(a)), Some((1.0, 2.0)));
        // Another track, or none selected: the whole of it.
        assert_eq!(loop_region(Repeat::Selection, Some(&sel), Some(Path::new("/b.flac"))), Some((0.0, f64::INFINITY)));
        assert_eq!(loop_region(Repeat::Selection, None, Some(a)), Some((0.0, f64::INFINITY)));
    }

    #[test]
    fn each_track_keeps_its_selection_and_presets() {
        let (a, b) = (Path::new("/a.flac"), Path::new("/b.flac"));
        let mut store = LoopStore::default();
        store.set_selection(a, Some((1.0, 2.0)));
        store.set_selection(b, Some((3.0, 4.0)));
        store.save_preset(a, "Intro", (0.0, 8.0));
        store.save_preset(a, "Solo", (60.0, 75.5));
        // The same name replaces.
        store.save_preset(a, "Intro", (0.5, 8.0));
        assert_eq!(store.selection(a).map(|s| (s.start, s.end)), Some((1.0, 2.0)));
        assert_eq!(store.selection(b).map(|s| (s.start, s.end)), Some((3.0, 4.0)));
        let names: Vec<_> = store.presets(a).iter().map(|p| (p.name.as_str(), p.start)).collect();
        assert_eq!(names, [("Intro", 0.5), ("Solo", 60.0)]);
        assert!(store.presets(b).is_empty());

        // Through the file and back.
        let back: LoopStore = serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(back.tracks, store.tracks);

        // A track with nothing left goes.
        store.set_selection(b, None);
        assert!(!store.tracks.contains_key("/b.flac"));
        store.set_selection(a, None);
        store.remove_preset(a, 0);
        store.remove_preset(a, 0);
        assert!(store.tracks.is_empty());
    }

    #[test]
    fn setting_an_edge_at_the_playhead() {
        let a = Path::new("/a.flac");
        let mut sel = None;
        set_end(&mut sel, a, 5.0);
        assert_eq!(sel.as_ref().map(|s| (s.start, s.end)), Some((0.0, 5.0)));
        set_start(&mut sel, a, 2.0, 10.0);
        assert_eq!(sel.as_ref().map(|s| (s.start, s.end)), Some((2.0, 5.0)));
        // Past the end: a new one from there to the end of the track.
        set_start(&mut sel, a, 7.0, 10.0);
        assert_eq!(sel.as_ref().map(|s| (s.start, s.end)), Some((7.0, 10.0)));
        // Before the start: from the start of the track.
        set_end(&mut sel, a, 3.0);
        assert_eq!(sel.as_ref().map(|s| (s.start, s.end)), Some((0.0, 3.0)));
    }
}
