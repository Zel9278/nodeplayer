//! The "Now playing" panel: the playing file's format and what playback does with it (the
//! bitrate of the second just played, with a graph of the last minute, resampling, the
//! decoder, the output), the file, all its tags and its play statistics, like foobar2000's
//! Properties.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use egui::RichText;

use crate::av::TrackInfo;
use crate::library::{self, PlayStats};
use crate::midifile::{MidiInfo, group_digits};
use crate::player::Status;

/// What the panel shows about the file, read off the UI thread.
struct Details {
    info: TrackInfo,
    size: Option<u64>,
    added: i64,
}

impl Details {
    fn read(path: &Path) -> Self {
        // FFmpeg takes MIDI files for audio it would render; the player tells about them.
        let info = if crate::midi_song::is_midi_file(path) { Ok(TrackInfo::default()) } else { crate::av::track_info(path) };
        let info = info.map_err(|e| log::debug!("{}: {e:#}", path.display())).unwrap_or_default();
        Self { info, size: std::fs::metadata(path).ok().map(|m| m.len()), added: library::added(path) }
    }

    fn tech(&self, k: &str) -> Option<&str> {
        self.info.tech.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
}

/// The player's state this frame, as the app sees it.
pub struct Now<'a> {
    pub status: &'a Status,
    pub playing: bool,
    /// Played to the end.
    pub finished: bool,
    pub position: f64,
    /// Playback rate in effect.
    pub rate: f32,
    /// The MIDI file playing, once loaded.
    pub midi: Option<MidiInfo>,
    /// Output device: name, sample rate and channels.
    pub output: Option<(&'a str, u32, u16)>,
    pub plays: PlayStats,
}

pub struct Info {
    details: Option<(PathBuf, Box<Details>)>,
    reading: Option<PathBuf>,
    tx: Sender<(PathBuf, Box<Details>)>,
    rx: Receiver<(PathBuf, Box<Details>)>,
    /// The bitrate shown, taken twice a second so it can be read.
    kbps: Option<f64>,
    /// The bitrate every [`SAMPLE`] while playing, newest last, for the graph; and the track
    /// it's for.
    history: VecDeque<f32>,
    history_path: Option<PathBuf>,
    sampled: Instant,
}

const SAMPLE: Duration = Duration::from_millis(500);
/// A minute of samples.
const HISTORY: usize = 120;

impl Info {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            details: None,
            reading: None,
            tx,
            rx,
            kbps: None,
            history: VecDeque::new(),
            history_path: None,
            sampled: Instant::now(),
        }
    }

    /// Runs every tick, shown or not, so the graph has no gaps. `live` is asked for only when a
    /// sample is due.
    pub fn sample(&mut self, path: Option<&Path>, playing: bool, live: impl FnOnce() -> Option<f64>) {
        if self.history_path.as_deref() != path {
            self.history.clear();
            self.history_path = path.map(Path::to_path_buf);
            self.kbps = None;
        }
        if self.sampled.elapsed() < SAMPLE {
            return;
        }
        self.sampled = Instant::now();
        self.kbps = if playing { live() } else { None };
        if let Some(k) = self.kbps {
            if self.history.len() == HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(k as f32);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, now: Now) {
        for (path, d) in self.rx.try_iter() {
            if self.reading.as_ref() == Some(&path) {
                self.reading = None;
            }
            self.details = Some((path, d));
        }
        let Some(path) = now.status.path.clone() else {
            ui.weak("Nothing is loaded.");
            return;
        };
        if self.details.as_ref().is_none_or(|(p, _)| *p != path) && self.reading.as_ref() != Some(&path) {
            self.reading = Some(path.clone());
            let (tx, ctx, path) = (self.tx.clone(), ui.ctx().clone(), path.clone());
            std::thread::spawn(move || {
                let d = Details::read(&path);
                let _ = tx.send((path, Box::new(d)));
                ctx.request_repaint();
            });
        }
        let d = self.details.as_ref().filter(|(p, _)| Some(p) == now.status.path.as_ref()).map(|(_, d)| &**d);

        egui::ScrollArea::vertical().id_salt("now playing").auto_shrink([false, false]).show(ui, |ui| {
            let title = now.status.title().unwrap_or_default();
            ui.strong(&title);
            if let Some(artist) = &now.status.tags.artist {
                ui.label(artist);
            }
            ui.add_space(4.0);
            let avg = d.and_then(|d| d.tech("bitrate")).and_then(|b| b.parse::<f32>().ok());
            if !self.history.is_empty() {
                bitrate_graph(ui, &self.history, avg);
            } else if now.status.decoder == "FFmpeg" {
                ui.weak("No bitrate graph: FFmpeg plays this file and doesn't tell packet sizes.");
            }
            ui.add_space(2.0);

            section(ui, "Playback", |ui| {
                let state = match (now.playing, now.finished) {
                    (true, _) => "Playing",
                    (false, true) => "Stopped",
                    (false, false) => "Paused",
                };
                row(ui, "State", state);
                let length = now.status.duration.map(fmt_time).unwrap_or_else(|| "?".into());
                row(ui, "Position", format!("{} / {length}", fmt_time(now.position)));
                if let Some(m) = &now.midi {
                    row(ui, "Format", format!("Standard MIDI file, type {}", m.format));
                    row(ui, "Tracks", m.tracks.to_string());
                    row(ui, "Notes", group_digits(m.note_on));
                    row(ui, "Resolution", format!("{} PPQ", m.division));
                    row(ui, "Tempo changes", m.tempo_changes.to_string());
                }
                let audio = now.status.decoder != "MIDI";
                let avg = d.and_then(|d| d.tech("bitrate"));
                let bitrate = match (self.kbps, avg) {
                    (Some(live), Some(avg)) => format!("{live:.0} kbps (average {avg} kbps)"),
                    (Some(live), None) => format!("{live:.0} kbps"),
                    (None, Some(avg)) => format!("{avg} kbps"),
                    (None, None) => "?".into(),
                };
                if audio {
                    row(ui, "Bitrate", bitrate).on_hover_text("of the second just played; the average is over the whole file");
                }
                let codec = match (d.and_then(|d| d.tech("codec")), d.and_then(|d| d.tech("codec_profile"))) {
                    (Some(c), Some(p)) => format!("{c} ({p})"),
                    (Some(c), None) => c.to_string(),
                    _ => "?".into(),
                };
                if audio {
                    row(ui, "Codec", codec);
                }
                if let Some(e) = d.and_then(|d| d.tech("encoding")) {
                    row(ui, "Encoding", if e == "lossless" { "Lossless" } else { "Lossy" });
                }
                if let Some(bits) = d.and_then(|d| d.tech("bitspersample")) {
                    row(ui, "Bit depth", format!("{bits} bit"));
                }
                let src = now.status.source_rate;
                let rate = match now.output.map(|o| o.1) {
                    Some(out) if out != src && src > 0 => format!("{} → {} (resampled)", fmt_rate(src), fmt_rate(out)),
                    _ if src > 0 => fmt_rate(src),
                    _ => "?".into(),
                };
                if audio {
                    row(ui, "Sample rate", rate);
                }
                let ch = now.status.channels;
                let channels = match ch {
                    1 => "1 (mono, played on both sides)".to_string(),
                    2 => "2 (stereo)".to_string(),
                    0 => "?".to_string(),
                    n => format!("{n} (mixed down to stereo)"),
                };
                if audio {
                    row(ui, "Channels", channels);
                }
                if (now.rate - 1.0).abs() > 1e-4 {
                    row(ui, "Rate", format!("{:.2}×", now.rate));
                }
                if !now.status.decoder.is_empty() {
                    row(ui, "Decoder", now.status.decoder);
                }
                if let Some((name, rate, ch)) = now.output {
                    row(ui, "Output", format!("{name} · {} · {ch} ch", fmt_rate(rate)));
                }
            });

            section(ui, "File", |ui| {
                if let Some(name) = path.file_name() {
                    row(ui, "Name", name.to_string_lossy());
                }
                if let Some(dir) = path.parent() {
                    row(ui, "Folder", dir.display().to_string());
                }
                if let Some(c) = d.and_then(|d| d.tech("container")) {
                    row(ui, "Container", c);
                }
                if let Some(size) = d.and_then(|d| d.size) {
                    row(ui, "Size", fmt_size(size));
                }
                if let Some(added) = d.map(|d| library::fmt_datetime(d.added)).filter(|t| !t.is_empty()) {
                    row(ui, "Added", added);
                }
            });

            section(ui, "Statistics", |ui| {
                row(ui, "Plays", now.plays.count.to_string());
                for (label, t) in [("First played", now.plays.first), ("Last played", now.plays.last)] {
                    let t = library::fmt_datetime(t);
                    if !t.is_empty() {
                        row(ui, label, t);
                    }
                }
            });

            match d {
                Some(d) if !d.info.meta.is_empty() => section(ui, "Tags", |ui| {
                    for (k, v) in &d.info.meta {
                        row(ui, k, v.as_str());
                    }
                }),
                Some(_) => {}
                None => {
                    ui.weak("Reading the file…");
                }
            }
        });
    }
}

/// The last minute's bitrate as a filled line, newest at the right, with the file's average
/// dashed across it.
fn bitrate_graph(ui: &mut egui::Ui, history: &VecDeque<f32>, avg: Option<f32>) {
    use egui::{Color32, Pos2, Shape, Stroke, pos2, vec2};
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 3.0, v.extreme_bg_color);
    let peak = history.iter().copied().fold(avg.unwrap_or(0.0), f32::max);
    let top = nice_top(peak * 1.1);
    let dx = rect.width() / (HISTORY - 1) as f32;
    let at = |i: usize, kbps: f32| {
        let x = rect.right() - (history.len() - 1 - i) as f32 * dx;
        pos2(x, rect.bottom() - (kbps / top).clamp(0.0, 1.0) * (rect.height() - 4.0))
    };
    let line: Vec<Pos2> = history.iter().enumerate().map(|(i, k)| at(i, *k)).collect();
    let accent = v.selection.bg_fill;
    if line.len() > 1 {
        let fill = accent.gamma_multiply(0.35);
        let mut mesh = egui::Mesh::default();
        for (i, p) in line.iter().enumerate() {
            mesh.colored_vertex(*p, fill);
            mesh.colored_vertex(pos2(p.x, rect.bottom()), fill);
            if i > 0 {
                let n = (i * 2) as u32;
                mesh.add_triangle(n - 2, n - 1, n);
                mesh.add_triangle(n - 1, n + 1, n);
            }
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::line(line.clone(), Stroke::new(1.5, accent)));
    }
    if let Some(avg) = avg {
        let y = rect.bottom() - (avg / top).clamp(0.0, 1.0) * (rect.height() - 4.0);
        painter.add(Shape::dashed_line(&[pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, v.weak_text_color()), 4.0, 3.0));
    }
    let small = egui::FontId::proportional(10.0);
    painter.text(rect.left_top() + vec2(4.0, 2.0), egui::Align2::LEFT_TOP, format!("{top:.0} kbps"), small.clone(), v.weak_text_color());
    painter.text(rect.left_bottom() + vec2(4.0, -2.0), egui::Align2::LEFT_BOTTOM, "1 min", small, v.weak_text_color());
    // The value under the pointer.
    if let Some(pos) = resp.hover_pos() {
        let back = ((rect.right() - pos.x) / dx).round() as usize;
        if back < history.len() {
            let i = history.len() - 1 - back;
            let p = at(i, history[i]);
            painter.circle_filled(p, 3.0, accent);
            painter.vline(p.x, rect.y_range(), Stroke::new(1.0, Color32::from_white_alpha(40)));
            let ago = back as f32 * SAMPLE.as_secs_f32();
            resp.on_hover_text(format!("{:.0} kbps, {ago:.1} s ago", history[i]));
        }
    }
}

/// A round number a little above `kbps` for the top of the graph.
fn nice_top(kbps: f32) -> f32 {
    const STEPS: [f32; 16] = [64.0, 96.0, 128.0, 160.0, 192.0, 256.0, 320.0, 384.0, 512.0, 768.0, 1024.0, 1536.0, 2048.0, 3072.0, 4608.0, 6144.0];
    STEPS.into_iter().find(|s| *s >= kbps).unwrap_or_else(|| (kbps / 1000.0).ceil() * 1000.0)
}

/// A titled grid of label / value rows.
fn section(ui: &mut egui::Ui, title: &str, rows: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong()).id_salt(("now playing", title)).default_open(true).show(ui, |ui| {
        egui::Grid::new(("now playing grid", title)).num_columns(2).spacing([12.0, 3.0]).striped(true).show(ui, rows);
    });
}

fn row(ui: &mut egui::Ui, label: &str, value: impl Into<String>) -> egui::Response {
    ui.weak(label);
    let r = ui.add(egui::Label::new(value.into()).wrap().selectable(true));
    ui.end_row();
    r
}

fn fmt_rate(hz: u32) -> String {
    if hz.is_multiple_of(1000) { format!("{} kHz", hz / 1000) } else { format!("{:.1} kHz", hz as f64 / 1000.0) }
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

fn fmt_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.0} KB", b as f64 / 1024.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_rate(44_100), "44.1 kHz");
        assert_eq!(fmt_rate(48_000), "48 kHz");
        assert_eq!(fmt_size(31_457_280), "30.0 MB");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(nice_top(100.0), 128.0);
        assert_eq!(nice_top(1100.0), 1536.0);
        assert_eq!(nice_top(10_000.0), 10_000.0);
    }

    #[test]
    fn samples_while_playing() {
        let mut info = Info::new();
        let a = Path::new("a.mp3");
        info.sampled = Instant::now() - SAMPLE;
        info.sample(Some(a), true, || Some(128.0));
        assert_eq!(info.history.len(), 1);
        // Not due yet.
        info.sample(Some(a), true, || panic!("asked too early"));
        // Paused: nothing added.
        info.sampled = Instant::now() - SAMPLE;
        info.sample(Some(a), false, || Some(1.0));
        assert_eq!(info.history.len(), 1);
        // Another track starts over.
        info.sample(Some(Path::new("b.mp3")), true, || Some(1.0));
        assert!(info.history.is_empty());
    }
}
