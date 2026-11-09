//! Persistent usage statistics. Audio callbacks only increment the Player's atomic frame
//! counter; aggregation, graph drawing and storage happen on the application thread.
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const SECOND: u64 = 1_000_000;
const DAY: u64 = 86_400 * SECOND;

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct Totals {
    runtime_us: u64,
    listening_us: u64,
}

impl Totals {
    fn add(&mut self, other: Self) {
        self.runtime_us = self.runtime_us.saturating_add(other.runtime_us);
        self.listening_us = self.listening_us.saturating_add(other.listening_us);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Track {
    path: PathBuf,
    title: String,
    artist: String,
    listening_us: u64,
    days: BTreeMap<i64, u64>,
}

impl Track {
    fn label(&self) -> String {
        let title = if self.title.is_empty() {
            self.path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            self.title.clone()
        };
        if self.artist.is_empty() {
            title
        } else {
            format!("{} — {title}", self.artist)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u32,
    since_us: u64,
    total: Totals,
    days: BTreeMap<i64, Totals>,
    tracks: Vec<Track>,
    #[serde(skip)]
    index: HashMap<PathBuf, usize>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: 1,
            since_us: unix_us(),
            total: Totals::default(),
            days: BTreeMap::new(),
            tracks: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl Store {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let mut store: Self = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(
            store.version == 1,
            "unsupported statistics version {}",
            store.version
        );
        for (i, track) in store.tracks.iter().enumerate() {
            anyhow::ensure!(
                store.index.insert(track.path.clone(), i).is_none(),
                "duplicate statistics track"
            );
        }
        Ok(store)
    }

    fn track(&mut self, path: &Path) -> &mut Track {
        let i = *self.index.entry(path.to_path_buf()).or_insert_with(|| {
            self.tracks.push(Track {
                path: path.to_path_buf(),
                ..Default::default()
            });
            self.tracks.len() - 1
        });
        &mut self.tracks[i]
    }

    /// Split at local calendar midnight, preserving the exact sum across all buckets.
    /// Wall-clock adjustments affect the bucket date, never the monotonic runtime total.
    fn record(&mut self, end: u64, elapsed: u64, listening: u64, path: Option<&Path>) {
        self.record_using(end, elapsed, listening, path, local_day);
    }

    fn record_using(
        &mut self,
        end: u64,
        elapsed: u64,
        listening: u64,
        path: Option<&Path>,
        day_at: impl Fn(u64) -> i64,
    ) {
        let listening = if path.is_some() { listening } else { 0 };
        self.total.add(Totals {
            runtime_us: elapsed,
            listening_us: listening,
        });
        let start = end.saturating_sub(elapsed);
        let mut at = start;
        let mut left = listening;
        loop {
            let day = day_at(at);
            let next = if at == end || day_at(end - 1) == day {
                end
            } else {
                // Find the first instant on another calendar date. Do not add 24 hours:
                // local days can be 23/25 hours, and some zones change at midnight.
                let (mut low, mut high) = (at, end - 1);
                while high - low > 1 {
                    let middle = low + (high - low) / 2;
                    if day_at(middle) == day {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                high
            };
            let runtime = next.saturating_sub(at);
            let heard = if next == end {
                left
            } else {
                ((listening as u128 * runtime as u128) / elapsed.max(1) as u128) as u64
            };
            left -= heard;
            self.days.entry(day).or_default().add(Totals {
                runtime_us: runtime,
                listening_us: heard,
            });
            if heard > 0
                && let Some(path) = path
            {
                let track = self.track(path);
                track.listening_us = track.listening_us.saturating_add(heard);
                let bucket = track.days.entry(day).or_default();
                *bucket = bucket.saturating_add(heard);
            }
            if next == end {
                break;
            }
            at = next;
        }
    }

    fn merge(&mut self, delta: &Self) {
        self.since_us = self.since_us.min(delta.since_us);
        self.total.add(delta.total);
        for (&day, &total) in &delta.days {
            self.days.entry(day).or_default().add(total);
        }
        for source in &delta.tracks {
            let track = self.track(&source.path);
            if !source.title.is_empty() {
                track.title.clone_from(&source.title);
            }
            if !source.artist.is_empty() {
                track.artist.clone_from(&source.artist);
            }
            track.listening_us = track.listening_us.saturating_add(source.listening_us);
            for (&day, &time) in &source.days {
                let bucket = track.days.entry(day).or_default();
                *bucket = bucket.saturating_add(time);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Runtime,
    Listening,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chart {
    Bars,
    Line,
}

pub struct Statistics {
    data: Store,
    pending: Store,
    path: Option<PathBuf>,
    load_failed: bool,
    error: Option<String>,
    last_tick: Instant,
    runtime_fraction_ns: u64,
    listening_fraction_us: f64,
    last_save: Instant,
    current: Option<PathBuf>,
    metric: Metric,
    chart: Chart,
    range: u64,
    cumulative: bool,
    selected: Option<PathBuf>,
    filter: String,
    ranking: Vec<usize>,
    ranked_at: Option<Instant>,
}

impl Statistics {
    pub fn new() -> Self {
        let path = (!crate::runtime::get().is_diagnostic())
            .then(crate::dirs::state)
            .flatten()
            .map(|p| p.join("statistics.json"));
        Self::open(path)
    }

    fn open(path: Option<PathBuf>) -> Self {
        let result = path.as_deref().map(Store::read).transpose();
        let load_failed = result.is_err();
        let (data, error) = match result {
            Ok(data) => (data.unwrap_or_default(), None),
            Err(e) => {
                log::warn!("statistics: {e:#}");
                (
                    Store::default(),
                    Some(format!(
                        "Could not load statistics: {e:#}. The existing file will not be overwritten."
                    )),
                )
            }
        };
        Self {
            data,
            pending: Store::default(),
            path,
            load_failed,
            error,
            last_tick: Instant::now(),
            runtime_fraction_ns: 0,
            listening_fraction_us: 0.0,
            last_save: Instant::now(),
            current: None,
            metric: Metric::Listening,
            chart: Chart::Bars,
            range: 30,
            cumulative: false,
            selected: None,
            filter: String::new(),
            ranking: Vec::new(),
            ranked_at: None,
        }
    }

    pub fn set_track(&mut self, path: &Path) {
        self.current = Some(path.to_path_buf());
    }

    pub fn tick(&mut self, listening_seconds: f64, status: &crate::player::Status) {
        let now = Instant::now();
        let nanos =
            now.duration_since(self.last_tick).as_nanos() + self.runtime_fraction_ns as u128;
        let elapsed = (nanos / 1000) as u64;
        self.runtime_fraction_ns = (nanos % 1000) as u64;
        self.last_tick = now;
        let listening = if listening_seconds.is_finite() && listening_seconds > 0.0 {
            let micros = listening_seconds * SECOND as f64 + self.listening_fraction_us;
            self.listening_fraction_us = micros.fract();
            micros as u64
        } else {
            0
        };
        let end = unix_us();
        for store in [&mut self.data, &mut self.pending] {
            store.record(end, elapsed, listening, self.current.as_deref());
            if self.current.is_some()
                && self.current == status.path
                && let Some(i) = self
                    .current
                    .as_ref()
                    .and_then(|p| store.index.get(p))
                    .copied()
            {
                let track = &mut store.tracks[i];
                if let Some(title) = &status.tags.title {
                    track.title.clone_from(title);
                }
                if let Some(artist) = &status.tags.artist {
                    track.artist.clone_from(artist);
                }
            }
        }
        if now.duration_since(self.last_save) >= Duration::from_secs(60) {
            self.save();
        }
    }

    /// Merge this instance's unsaved delta under an OS file lock. Separate app instances
    /// cannot overwrite each other's totals. Atomic replacement protects against crashes.
    pub fn save(&mut self) {
        self.last_save = Instant::now();
        if self.load_failed || self.path.is_none() || self.pending.total.runtime_us == 0 {
            return;
        }
        let result = (|| -> anyhow::Result<Store> {
            let path = self.path.as_ref().unwrap();
            std::fs::create_dir_all(path.parent().unwrap())?;
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_extension("lock"))?;
            lock.lock()?;
            let mut latest = Store::read(path)?;
            latest.merge(&self.pending);
            let tmp = path.with_extension("json.tmp");
            crate::dirs::write_private(&tmp, &serde_json::to_vec(&latest)?)?;
            std::fs::rename(&tmp, path)?;
            Ok(latest)
        })();
        match result {
            Ok(data) => {
                self.data = data;
                self.pending = Store::default();
                self.error = None;
                self.ranked_at = None;
            }
            Err(e) => {
                log::warn!("saving statistics: {e:#}");
                self.error = Some(format!("Could not save statistics: {e:#}"));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("statistics dock").show(ui, |ui| {
            ui.heading("Statistics");
            ui.horizontal_wrapped(|ui| {
                ui.group(|ui| { ui.small("Total running time"); ui.heading(duration(self.data.total.runtime_us)); });
                ui.group(|ui| { ui.small("Total listening time"); ui.heading(duration(self.data.total.listening_us)); });
            });
            if let Some(track) = self.current.as_ref().and_then(|p| self.data.index.get(p)).map(|&i| &self.data.tracks[i]) {
                ui.label(format!("Current track: {} · {}", track.label(), duration(track.listening_us)));
            }
            ui.small(format!("Recorded since {} (PC time zone). Earlier usage cannot be reconstructed.", date(local_day(self.data.since_us))));
            if let Some(error) = &self.error { ui.colored_label(ui.visuals().error_fg_color, error); }
            if self.path.is_none() { ui.weak("Statistics are not saved in this mode."); }
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_value(&mut self.metric, Metric::Runtime, "Running").clicked() { self.selected = None; }
                ui.selectable_value(&mut self.metric, Metric::Listening, "Listening");
                egui::ComboBox::from_id_salt("statistics range").selected_text(format!("{} days", self.range)).show_ui(ui, |ui| {
                    for days in [7, 30, 90, 365] { ui.selectable_value(&mut self.range, days, format!("{days} days")); }
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.chart, Chart::Bars, "Bars");
                ui.selectable_value(&mut self.chart, Chart::Line, "Line");
                ui.checkbox(&mut self.cumulative, "Cumulative");
                if self.selected.is_some() && ui.button("All tracks").clicked() { self.selected = None; }
            });
            let track = self.selected.as_ref().and_then(|p| self.data.index.get(p)).map(|&i| &self.data.tracks[i]);
            ui.label(track.map_or_else(|| if self.metric == Metric::Runtime { "Daily running time (PC time zone)".into() } else { "Daily listening time (PC time zone)".into() }, |t| format!("{} (PC time zone)", t.label())));
            let points = series(&self.data, track, self.metric, local_day(unix_us()), self.range, self.cumulative);
            plot(ui, &points, self.chart);
            ui.small("Listening counts processed playback time, including repeats; paused time and seeking are excluded. Times are independent of playback speed.");
            ui.separator();
            ui.label("Listening by track");
            if ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter tracks").desired_width(f32::INFINITY)).changed() { self.ranked_at = None; }
            if self.ranked_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                let filter = self.filter.to_lowercase();
                self.ranking = self.data.tracks.iter().enumerate().filter(|(_, t)| filter.is_empty() || t.label().to_lowercase().contains(&filter) || t.path.to_string_lossy().to_lowercase().contains(&filter)).map(|(i, _)| i).collect();
                self.ranking.sort_by(|&a, &b| self.data.tracks[b].listening_us.cmp(&self.data.tracks[a].listening_us).then_with(|| self.data.tracks[a].path.cmp(&self.data.tracks[b].path)));
                self.ranked_at = Some(Instant::now());
            }
            if self.ranking.is_empty() { ui.weak("No listening time recorded for matching tracks yet."); }
            egui::ScrollArea::vertical().id_salt("statistics tracks").max_height(320.0).show_rows(ui, 44.0, self.ranking.len(), |ui, rows| {
                for row in rows {
                    let track = &self.data.tracks[self.ranking[row]];
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::selectable(self.selected.as_ref() == Some(&track.path), format!("{}  {}", duration(track.listening_us), track.label())).truncate()).on_hover_text(track.path.display().to_string()).clicked() {
                            self.selected = Some(track.path.clone()); self.metric = Metric::Listening;
                        }
                    });
                    let share = track.listening_us as f32 / self.data.total.listening_us.max(1) as f32;
                    ui.add(egui::ProgressBar::new(share).desired_height(5.0));
                    ui.add_space(5.0);
                }
            });
        });
    }
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn duration(us: u64) -> String {
    let secs = us / SECOND;
    format!("{}h {:02}m {:02}s", secs / 3600, secs / 60 % 60, secs % 60)
}

/// A civil-day ordinal, independent of the length of that local day in seconds.
fn civil_day(year: i32, month: i32, day: i32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let y = year - era * 400;
    let m = i64::from(month) + if month > 2 { -3 } else { 9 };
    era * 146_097 + y * 365 + y / 4 - y / 100 + (153 * m + 2) / 5 + i64::from(day) - 1 - 719_468
}

fn local_day(us: u64) -> i64 {
    let secs = (us / SECOND) as libc::time_t;
    // SAFETY: localtime writes only this tm. No process-global TZ overrides are used.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        let valid = !libc::localtime_r(&secs, &mut tm).is_null();
        #[cfg(windows)]
        let valid = libc::localtime_s(&mut tm, &secs) == 0;
        if valid {
            civil_day(tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
        } else {
            (us / DAY) as i64
        }
    }
}

fn date(day: i64) -> String {
    let secs = (day * 86_400) as libc::time_t;
    // Day keys encode a calendar date, not a UTC midnight to be shifted again. Decode
    // the ordinal with gmtime so timezone changes never relabel historical dates.
    // SAFETY: gmtime only writes to this local tm; the reentrant API is used on Unix.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        let valid = !libc::gmtime_r(&secs, &mut tm).is_null();
        #[cfg(windows)]
        let valid = libc::gmtime_s(&mut tm, &secs) == 0;
        if valid {
            format!(
                "{}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            )
        } else {
            "?".into()
        }
    }
}

fn series(
    store: &Store,
    track: Option<&Track>,
    metric: Metric,
    today: i64,
    days: u64,
    cumulative: bool,
) -> Vec<(i64, u64)> {
    let start = today.saturating_sub(days.saturating_sub(1) as i64);
    let daily = |day: i64| {
        track.map_or_else(
            || {
                store.days.get(&day).map_or(0, |v| {
                    if metric == Metric::Runtime {
                        v.runtime_us
                    } else {
                        v.listening_us
                    }
                })
            },
            |t| t.days.get(&day).copied().unwrap_or(0),
        )
    };
    let mut total = if cumulative {
        track.map_or_else(
            || {
                store
                    .days
                    .range(..start)
                    .map(|(_, v)| {
                        if metric == Metric::Runtime {
                            v.runtime_us
                        } else {
                            v.listening_us
                        }
                    })
                    .sum()
            },
            |t| t.days.range(..start).map(|(_, v)| *v).sum(),
        )
    } else {
        0u64
    };
    (start..=today)
        .map(|day| {
            let value = daily(day);
            total = total.saturating_add(value);
            (day, if cumulative { total } else { value })
        })
        .collect()
}

fn plot(ui: &mut egui::Ui, points: &[(i64, u64)], chart: Chart) {
    use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, pos2, vec2};
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width().max(1.0), 220.0), Sense::hover());
    if rect.width() < 150.0 || points.is_empty() {
        return;
    }
    let painter = ui.painter_at(rect);
    let area = Rect::from_min_max(rect.min + vec2(48.0, 12.0), rect.max - vec2(8.0, 26.0));
    let max = points.iter().map(|p| p.1).max().unwrap_or(0).max(SECOND) as f64 * 1.1;
    let (unit, suffix) = if max < (60 * SECOND) as f64 {
        (SECOND as f64, "s")
    } else if max < (3600 * SECOND) as f64 {
        ((60 * SECOND) as f64, "m")
    } else {
        ((3600 * SECOND) as f64, "h")
    };
    let color = ui.visuals().selection.bg_fill;
    let text = ui.visuals().text_color();
    let step = area.width() / points.len() as f32;
    let position = |i: usize, value: u64| {
        pos2(
            area.left() + (i as f32 + 0.5) * step,
            area.bottom() - (value as f64 / max) as f32 * area.height(),
        )
    };
    for i in 0..=4 {
        let y = area.bottom() - area.height() * i as f32 / 4.0;
        painter.line_segment(
            [pos2(area.left(), y), pos2(area.right(), y)],
            ui.visuals().widgets.noninteractive.bg_stroke,
        );
        painter.text(
            pos2(area.left() - 5.0, y),
            Align2::RIGHT_CENTER,
            format!("{:.1}{suffix}", max / unit * i as f64 / 4.0),
            FontId::monospace(10.0),
            text,
        );
    }
    match chart {
        Chart::Bars => {
            for (i, &(_, value)) in points.iter().enumerate() {
                let top = position(i, value);
                if value > 0 {
                    painter.rect_filled(
                        Rect::from_min_max(
                            pos2(top.x - step * 0.36, top.y),
                            pos2(top.x + step * 0.36, area.bottom()),
                        ),
                        1.0,
                        color,
                    );
                }
            }
        }
        Chart::Line => {
            let vertices = points
                .iter()
                .enumerate()
                .map(|(i, p)| position(i, p.1))
                .collect::<Vec<_>>();
            if vertices.len() == 1 {
                painter.circle_filled(vertices[0], 3.0, color);
            } else {
                painter.add(egui::Shape::line(vertices, Stroke::new(2.0, color)));
            }
        }
    }
    for (i, align) in [(0, Align2::LEFT_TOP), (points.len() - 1, Align2::RIGHT_TOP)] {
        painter.text(
            pos2(
                if i == 0 { area.left() } else { area.right() },
                area.bottom() + 7.0,
            ),
            align,
            date(points[i].0),
            FontId::monospace(10.0),
            text,
        );
    }
    if let Some(mouse) = response.hover_pos().filter(|p| area.contains(*p)) {
        let i = (((mouse.x - area.left()) / step) as usize).min(points.len() - 1);
        let at = position(i, points[i].1);
        painter.line_segment(
            [pos2(at.x, area.top()), pos2(at.x, area.bottom())],
            Stroke::new(1.0, text.gamma_multiply(0.5)),
        );
        painter.circle_filled(at, 3.5, Color32::from_rgb(100, 180, 240));
        response.on_hover_text(format!(
            "{} (PC time zone)\n{}",
            date(points[i].0),
            duration(points[i].1)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Store {
        // Existing fixtures use civil-day ordinals directly, independent of the PC
        // running the tests. Production always uses the OS local timezone instead.
        fn record_utc(&mut self, end: u64, elapsed: u64, listening: u64, path: Option<&Path>) {
            self.record_using(end, elapsed, listening, path, |us| (us / DAY) as i64);
        }
    }

    #[test]
    fn local_date_matches_the_pc_and_civil_dates_survive_calendar_boundaries() {
        for us in [
            unix_us(),
            unix_us().saturating_sub(12 * 3600 * SECOND),
            1_709_164_800 * SECOND,
        ] {
            assert_eq!(
                date(local_day(us)),
                crate::library::fmt_date((us / SECOND) as i64)
            );
        }
        assert_eq!(civil_day(1970, 1, 1), 0);
        assert_eq!(civil_day(2000, 3, 1) - civil_day(2000, 2, 28), 2);
        assert_eq!(civil_day(2100, 3, 1) - civil_day(2100, 2, 28), 1);
        assert_eq!(date(civil_day(2026, 12, 31) + 1), "2027-01-01");
    }

    #[test]
    fn local_midnight_splits_before_utc_midnight_and_handles_negative_offsets() {
        let hour = 3600 * SECOND;
        let mut data = Store::default();
        let path = Path::new("a.wav");
        let midnight = 10 * DAY + 15 * hour; // UTC+09:00: midnight is 15:00 UTC.
        data.record_using(
            midnight + 3 * SECOND,
            5 * SECOND,
            3 * SECOND,
            Some(path),
            |us| ((us + 9 * hour) / DAY) as i64,
        );
        assert_eq!(data.days[&10].runtime_us, 2 * SECOND);
        assert_eq!(data.days[&11].runtime_us, 3 * SECOND);
        assert_eq!(data.days[&10].listening_us, 1_200_000);
        assert_eq!(data.days[&11].listening_us, 1_800_000);
        assert_eq!(data.tracks[0].days.values().sum::<u64>(), 3 * SECOND);
        let midnight = 12 * DAY + 8 * hour; // UTC-08:00.
        data.record_using(midnight + SECOND, 2 * SECOND, SECOND, Some(path), |us| {
            (us as i64 - 8 * hour as i64).div_euclid(DAY as i64)
        });
        assert_eq!(data.days[&11].runtime_us, 4 * SECOND);
        assert_eq!(data.days[&12].runtime_us, SECOND);
    }

    #[test]
    fn daylight_saving_days_have_23_or_25_hours_without_shifting_their_dates() {
        let hour = 3600 * SECOND;
        let mut spring = Store::default();
        // At 07:00 UTC the local clock jumps from 02:00 to 03:00 (UTC-5 -> UTC-4).
        let spring_day = |us: u64| {
            let offset = if us < 100 * DAY + 7 * hour { -5 } else { -4 };
            (us as i64 + offset * hour as i64).div_euclid(DAY as i64)
        };
        spring.record_using(
            101 * DAY + 4 * hour,
            23 * hour,
            11 * hour,
            Some(Path::new("a.wav")),
            spring_day,
        );
        assert_eq!(spring.days.len(), 1);
        assert_eq!(spring.days[&100].runtime_us, 23 * hour);
        assert_eq!(spring.days[&100].listening_us, 11 * hour);
        spring.record_using(
            101 * DAY + 4 * hour + SECOND,
            2 * SECOND,
            2 * SECOND,
            Some(Path::new("a.wav")),
            spring_day,
        );
        assert_eq!(spring.days[&100].runtime_us, 23 * hour + SECOND);
        assert_eq!(spring.days[&101].runtime_us, SECOND);
        let autumn_day = |us: u64| {
            let offset = if us < 200 * DAY + 6 * hour { -4 } else { -5 };
            (us as i64 + offset * hour as i64).div_euclid(DAY as i64)
        };
        let mut autumn = Store::default();
        autumn.record_using(
            201 * DAY + 5 * hour,
            25 * hour,
            12 * hour,
            Some(Path::new("a.wav")),
            autumn_day,
        );
        assert_eq!(autumn.days.len(), 1);
        assert_eq!(autumn.days[&200].runtime_us, 25 * hour);
        assert_eq!(autumn.total.listening_us, 12 * hour);
    }

    fn record(stats: &mut Statistics, runtime: u64, listening: u64, path: &str) {
        for store in [&mut stats.data, &mut stats.pending] {
            store.record_utc(
                10 * DAY + runtime,
                runtime,
                listening,
                Some(Path::new(path)),
            );
        }
    }

    fn temp_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nodeplayer-statistics-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("statistics.json")
    }

    fn cleanup(path: &Path) {
        for file in [
            path.to_path_buf(),
            path.with_extension("lock"),
            path.with_extension("json.tmp"),
        ] {
            if file.exists() {
                std::fs::remove_file(file).unwrap();
            }
        }
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn midnight_split_preserves_totals_and_track_time() {
        let mut data = Store::default();
        let path = Path::new("music/a.wav");
        data.record_utc(11 * DAY + 3 * SECOND, 5 * SECOND, 3 * SECOND, Some(path));
        assert_eq!(data.total.runtime_us, 5 * SECOND);
        assert_eq!(data.total.listening_us, 3 * SECOND);
        assert_eq!(data.days[&10].runtime_us, 2 * SECOND);
        assert_eq!(data.days[&11].runtime_us, 3 * SECOND);
        assert_eq!(data.days[&10].listening_us, 1_200_000);
        assert_eq!(data.days[&11].listening_us, 1_800_000);
        assert_eq!(data.tracks[0].listening_us, 3 * SECOND);
        assert_eq!(data.tracks[0].days.values().sum::<u64>(), 3 * SECOND);
        // A tick exactly on midnight belongs entirely to the preceding day.
        data.record_utc(12 * DAY, SECOND, SECOND, Some(path));
        assert!(!data.days.contains_key(&12));
        // Uptime without a loaded track does not invent listening history.
        data.record_utc(12 * DAY + SECOND, SECOND, 99 * SECOND, None);
        assert_eq!(data.total.listening_us, 4 * SECOND);
    }

    #[test]
    fn graph_fills_missing_days_and_keeps_lifetime_cumulative_baseline() {
        let mut data = Store::default();
        data.record_utc(
            3 * DAY + SECOND,
            SECOND,
            2 * SECOND,
            Some(Path::new("a.wav")),
        );
        data.record_utc(
            7 * DAY + SECOND,
            SECOND,
            3 * SECOND,
            Some(Path::new("a.wav")),
        );
        data.record_utc(8 * DAY + SECOND, SECOND, SECOND, Some(Path::new("b.wav")));
        assert_eq!(
            series(&data, None, Metric::Listening, 8, 3, false),
            [(6, 0), (7, 3 * SECOND), (8, SECOND)]
        );
        assert_eq!(
            series(&data, None, Metric::Listening, 8, 3, true),
            [(6, 2 * SECOND), (7, 5 * SECOND), (8, 6 * SECOND)]
        );
        assert_eq!(
            series(&data, Some(&data.tracks[0]), Metric::Listening, 8, 3, true),
            [(6, 2 * SECOND), (7, 5 * SECOND), (8, 5 * SECOND)]
        );
        assert_eq!(
            series(&data, None, Metric::Runtime, 8, 3, false),
            [(6, 0), (7, SECOND), (8, SECOND)]
        );
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(duration(3_661 * SECOND), "1h 01m 01s");
    }

    #[test]
    fn saves_reload_and_merge_multiple_instances_without_double_counting() {
        let path = temp_file();
        let mut a = Statistics::open(Some(path.clone()));
        let mut b = Statistics::open(Some(path.clone()));
        record(&mut a, 10 * SECOND, 8 * SECOND, "a.wav");
        record(&mut b, 5 * SECOND, 2 * SECOND, "b.wav");
        a.save();
        assert!(a.error.is_none());
        b.save();
        assert!(b.error.is_none());
        record(&mut a, 4 * SECOND, SECOND, "a.wav");
        a.save();
        a.save(); // Saving again cannot replay the already-committed delta.
        let loaded = Statistics::open(Some(path.clone()));
        assert!(loaded.error.is_none());
        assert_eq!(loaded.data.total.runtime_us, 19 * SECOND);
        assert_eq!(loaded.data.total.listening_us, 11 * SECOND);
        assert_eq!(
            loaded.data.tracks[loaded.data.index[Path::new("a.wav")]].listening_us,
            9 * SECOND
        );
        assert_eq!(
            loaded.data.tracks[loaded.data.index[Path::new("b.wav")]].listening_us,
            2 * SECOND
        );
        assert_eq!(loaded.pending.total.runtime_us, 0);
        cleanup(&path);
    }

    #[test]
    fn corrupt_or_future_store_is_never_overwritten() {
        let path = temp_file();
        for contents in [
            b"broken JSON".to_vec(),
            serde_json::to_vec(&Store {
                version: 2,
                ..Default::default()
            })
            .unwrap(),
        ] {
            std::fs::write(&path, &contents).unwrap();
            let mut stats = Statistics::open(Some(path.clone()));
            assert!(stats.load_failed && stats.error.is_some());
            record(&mut stats, SECOND, SECOND, "a.wav");
            stats.save();
            assert_eq!(std::fs::read(&path).unwrap(), contents);
        }
        cleanup(&path);
    }

    #[test]
    fn ticking_tracks_time_metadata_and_fractional_samples_without_a_profile() {
        let mut stats = Statistics::open(None);
        let path = Path::new("a.wav");
        stats.set_track(path);
        let status = crate::player::Status {
            path: Some(path.to_path_buf()),
            tags: crate::player::Tags {
                title: Some("A title".into()),
                artist: Some("Artist".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        for _ in 0..48_000 {
            stats.tick(1.0 / 48_000.0, &status);
        }
        assert!((stats.data.total.listening_us as i64 - SECOND as i64).abs() <= 1);
        assert_eq!(stats.data.tracks[0].label(), "Artist — A title");
        let before = stats.data.total.listening_us;
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            stats.tick(invalid, &status);
        }
        assert_eq!(stats.data.total.listening_us, before);
        assert!(stats.data.total.runtime_us > 0);
        stats.save();
        assert!(stats.path.is_none() && stats.error.is_none());
    }

    #[test]
    fn dock_and_plots_render_with_zero_data_narrow_sizes_and_both_themes() {
        let context = egui::Context::default();
        let mut stats = Statistics::open(None);
        for width in [100.0, 480.0] {
            for dark in [false, true] {
                context.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
                for chart in [Chart::Bars, Chart::Line] {
                    stats.chart = chart;
                    let mut output = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 800.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            stats.ui(ui);
                        },
                    );
                    output.textures_delta.clear();
                }
            }
            record(
                &mut stats,
                SECOND,
                SECOND,
                "a very long track name that must not make rows wrap.wav",
            );
            stats.selected = Some(PathBuf::from(
                "a very long track name that must not make rows wrap.wav",
            ));
            stats.cumulative = true;
        }
    }
}
