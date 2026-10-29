//! MIDI File Player node: plays a standard MIDI file ([`crate::midifile`]) into the graph's
//! MIDI, each message at the frame it is due.
//!
//! The player thread stamps messages with the time they should sound, a little ahead
//! (`PLAYBACK_LATENCY`); the audio thread keeps a clock of when each block starts and puts
//! every message due in the block at its frame.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::engine::nodes::MidiQueue;
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use crate::midifile::{MidiOut, MidiPlayer, MidiSettings, Timed, format_time, group_digits};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiFileSettings {
    pub path: Option<PathBuf>,
    pub speed: f32,
    /// Leave out channel 10 (drums).
    pub skip_drums: bool,
}

impl Default for MidiFileSettings {
    fn default() -> Self {
        Self { path: None, speed: 1.0, skip_drums: false }
    }
}

pub struct MidiFileNode {
    player: MidiPlayer,
    rx: MidiQueue<rtrb::Consumer<Timed>>,
    out: MidiOut,
    sample_rate: f32,
    /// While the position slider is dragged: where it is.
    seek_preview: Option<f64>,
    /// Note-ons the audio thread left out for coming too late.
    late: Arc<AtomicU64>,
}

impl MidiFileNode {
    pub fn new(settings: MidiFileSettings, sample_rate: f32) -> Self {
        let player = MidiPlayer::new(MidiSettings {
            last_path: settings.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            speed: settings.speed,
            skip_drums: settings.skip_drums,
        });
        // black MIDI has a million events a second, 20 ms of them ahead
        let (out, rx) = MidiOut::new(1 << 18);
        player.set_output(Some(out.clone()));
        let mut me = Self { player, rx: Arc::new(Mutex::new(rx)), out, sample_rate, seek_preview: None, late: Arc::default() };
        if let Some(p) = settings.path.filter(|p| p.is_file()) {
            me.player.open(p, false);
        }
        me
    }

    pub fn settings(&self) -> MidiFileSettings {
        let s = &self.player.settings;
        MidiFileSettings {
            path: (!s.last_path.is_empty()).then(|| PathBuf::from(&s.last_path)),
            speed: s.speed,
            skip_drums: s.skip_drums,
        }
    }

    pub fn processor(&self) -> MidiFileProc {
        MidiFileProc { rx: self.rx.clone(), pending: None, next_block: None, sample_rate: self.sample_rate, late: self.late.clone() }
    }

    pub fn open(&mut self, path: PathBuf) {
        self.player.open(path, false);
    }

    pub fn is_midi_file(path: &std::path::Path) -> bool {
        path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi"))
    }

    /// Finishes loads; call every frame (also while the node is collapsed).
    pub fn update(&mut self) {
        self.player.update();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.update();
        let status = self.player.status();
        ui.horizontal(|ui| {
            if ui.button("Open…").clicked() {
                let mut dlg = rfd::FileDialog::new().add_filter("MIDI", &["mid", "midi", "MID", "MIDI"]);
                if let Some(dir) = PathBuf::from(&self.player.settings.last_path).parent().filter(|d| d.is_dir()) {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(p) = dlg.pick_file() {
                    self.open(p);
                }
            }
            match &status.name {
                Some(name) => ui.label(name),
                None => ui.weak("no file"),
            };
        });
        if let Some(p) = self.player.load_progress() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add(egui::ProgressBar::new(status.loading.unwrap_or(0.0)).desired_width(200.0).text(format!("loading {}", p.name)));
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if let Some(e) = self.player.load_error() {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 90), e);
        }
        let Some(info) = self.player.info().cloned() else { return };
        ui.small(format!(
            "{} tracks · {} notes · {} PPQ · {} tempo changes",
            info.tracks,
            group_digits(info.note_on),
            info.division,
            info.tempo_changes
        ));

        let duration = status.duration;
        let position = self.seek_preview.unwrap_or(status.position);
        ui.horizontal(|ui| {
            if ui.button(if status.playing { "⏸" } else { "▶" }).clicked() {
                self.player.toggle();
            }
            if ui.button("⏹").clicked() {
                self.player.stop();
            }
            ui.label(format!("{} / {}", format_time(position), format_time(duration)));
            if self.player.seek_progress().is_some() {
                ui.spinner();
            }
        });
        let mut seek_to = position;
        let slider = ui.add(egui::Slider::new(&mut seek_to, 0.0..=duration.max(0.001)).show_value(false));
        if slider.dragged() {
            self.seek_preview = Some(seek_to);
        }
        if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
            self.seek_preview = None;
            self.player.seek(seek_to);
        }
        ui.horizontal(|ui| {
            let mut speed = self.player.settings.speed;
            if ui.add(egui::Slider::new(&mut speed, 0.25..=4.0).logarithmic(true).suffix("×").text("speed")).changed() {
                self.player.set_speed(speed);
            }
            let mut skip = self.player.settings.skip_drums;
            if ui.checkbox(&mut skip, "no drums").on_hover_text("leave out channel 10").changed() {
                self.player.set_skip_drums(skip);
            }
        });
        self.keys(ui);
        let skipped = self.player.notes_skipped() + self.out.dropped.load(Ordering::Relaxed) + self.late.load(Ordering::Relaxed);
        ui.small(format!(
            "{} of {} notes{}",
            group_digits(status.notes_played),
            group_digits(status.notes_total),
            if skipped > 0 { format!(" · {} left out (too dense)", group_digits(skipped)) } else { String::new() }
        ));
        if status.playing {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
    }

    /// The 128 keys, lit while the file holds them.
    fn keys(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(256.0, 16.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        let w = rect.width() / 128.0;
        for key in 0..128 {
            let black = matches!(key % 12, 1 | 3 | 6 | 8 | 10);
            let x = rect.left() + key as f32 * w;
            let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, rect.height()));
            let color = if self.player.key_down(key) {
                egui::Color32::from_rgb(255, 170, 60)
            } else if black {
                egui::Color32::from_gray(40)
            } else {
                egui::Color32::from_gray(170)
            };
            p.rect_filled(r.shrink2(egui::vec2(0.15, if black { 3.0 } else { 0.0 })), 0.0, color);
        }
    }
}

pub struct MidiFileProc {
    rx: MidiQueue<rtrb::Consumer<Timed>>,
    /// A message taken from the queue that belongs to a later block.
    pending: Option<Timed>,
    /// When the next block starts, by this processor's clock.
    next_block: Option<Instant>,
    sample_rate: f32,
    late: Arc<AtomicU64>,
}

/// A note-on this far behind its time is left out rather than played late, so a file denser
/// than the graph can take doesn't fall further and further behind.
const LATE_LIMIT: Duration = Duration::from_millis(100);

impl Processor for MidiFileProc {
    fn process(&mut self, _n: usize, _inputs: &mut [StereoBuf], _outputs: &mut [StereoBuf]) {}

    fn process_midi(
        &mut self,
        n: usize,
        _inputs: &mut [StereoBuf],
        _outputs: &mut [StereoBuf],
        _midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        // Blocks follow each other at the sample rate; the clock only jumps to the wall
        // clock when it falls behind or runs far ahead (a stall, the stream restarting).
        let now = Instant::now();
        let start = match self.next_block {
            Some(t) if t >= now && t - now < Duration::from_millis(100) => t,
            _ => now,
        };
        let len = Duration::from_secs_f64(n as f64 / self.sample_rate as f64);
        self.next_block = Some(start + len);
        let end = start + len;
        let Ok(mut rx) = self.rx.try_lock() else { return };
        while let Some((at, msg)) = self.pending.take().or_else(|| rx.pop().ok()) {
            let time = match at {
                None => 0,
                Some(at) if at + LATE_LIMIT < start && msg & 0xf0 == 0x90 && msg >> 16 & 0x7f > 0 => {
                    self.late.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Some(at) if at < end => {
                    let secs = at.saturating_duration_since(start).as_secs_f64();
                    ((secs * self.sample_rate as f64) as usize).min(n.saturating_sub(1)) as u32
                }
                Some(_) => {
                    self.pending = Some((at, msg));
                    break;
                }
            };
            let bytes = [msg as u8, (msg >> 8) as u8, (msg >> 16) as u8];
            let Some(e) = MidiEvent::new(time, &bytes) else { continue };
            // Events come in time order; a message already late stays at the block's start.
            let e = MidiEvent { time: e.time.max(midi_out.events().last().map_or(0, |l| l.time)), ..e };
            if !midi_out.push(e) {
                // The block is full (black MIDI): the rest go into the next ones.
                self.pending = Some((at, msg));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_messages_at_their_frame() {
        let (out, rx) = MidiOut::new(64);
        let node_rx = Arc::new(Mutex::new(rx));
        let mut p = MidiFileProc { rx: node_rx, pending: None, next_block: None, sample_rate: 48_000.0, late: Arc::default() };
        let mut buf = MidiBuf::new();
        // Prime the clock, then send a note 5 ms into the next block and one 30 ms out.
        p.process_midi(480, &mut [], &mut [], &[], &mut buf);
        let block = p.next_block.unwrap();
        let times = [block + Duration::from_millis(5), block + Duration::from_millis(30)];
        out.send_for_test(&[0x90 | 60 << 8 | 100 << 16, 0x80 | 60 << 8], &times);
        buf.clear();
        p.process_midi(480, &mut [], &mut [], &[], &mut buf);
        assert_eq!(buf.events().iter().map(|e| (e.time, e.data)).collect::<Vec<_>>(), [(240, [0x90, 60, 100])]);
        // Blocks are 10 ms: 30 ms after that block's start is the first frame of the third
        // block after it, and nothing comes before.
        let mut got = Vec::new();
        for block in 1..=3 {
            buf.clear();
            p.process_midi(480, &mut [], &mut [], &[], &mut buf);
            got.extend(buf.events().iter().map(|e| (block, e.time, e.data)));
        }
        assert_eq!(got, [(3, 0, [0x80, 60, 0])]);
    }
}
