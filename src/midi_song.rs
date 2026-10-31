//! MIDI files as songs: the [`crate::player::Player`] plays them through a [`MidiPlayer`] and
//! sends what they play out of the Player node's MIDI pin, each message at the frame it is
//! due.
//!
//! The player thread stamps messages with the time they should sound, a little ahead
//! (`PLAYBACK_LATENCY`); the audio thread keeps a clock of when each block starts and puts
//! every message due in the block at its frame. There the playback rate also shifts the
//! pitch, as it does audio ([`PitchShift`]).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::nodes::MidiQueue;
use crate::engine::{MidiBuf, MidiEvent};
use crate::midifile::{MidiOut, MidiPlayer, MidiSettings, Timed, group_digits};

pub const MIDI_EXTS: &[&str] = &["mid", "midi"];

pub fn is_midi_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| MIDI_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The UI side: the file player and the queue its messages go out through.
pub struct MidiSong {
    pub player: MidiPlayer,
    rx: MidiQueue<rtrb::Consumer<Timed>>,
    out: MidiOut,
    /// Note-ons the audio thread left out for coming too late.
    late: Arc<AtomicU64>,
}

impl MidiSong {
    pub fn new() -> Self {
        let player = MidiPlayer::new(MidiSettings { last_path: String::new(), speed: 1.0, skip_drums: false });
        // black MIDI has a million events a second, 20 ms of them ahead
        let (out, rx) = MidiOut::new(1 << 18);
        player.set_output(Some(out.clone()));
        Self { player, rx: Arc::new(Mutex::new(rx)), out, late: Arc::default() }
    }

    /// The audio-thread end of the queue.
    pub fn feed(&self, sample_rate: f32) -> MidiFeed {
        MidiFeed {
            rx: self.rx.clone(),
            pending: None,
            next_block: None,
            sample_rate,
            late: self.late.clone(),
            pitch: PitchShift::new(),
        }
    }

    /// The Player node's MIDI section: the file, its keys, and the MIDI-only settings.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let status = self.player.status();
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
            "MIDI · {} tracks · {} notes · {} PPQ · {} tempo changes",
            info.tracks,
            group_digits(info.note_on),
            info.division,
            info.tempo_changes
        ));
        let mut skip = self.player.settings.skip_drums;
        if ui.checkbox(&mut skip, "no drums").on_hover_text("leave out channel 10").changed() {
            self.player.set_skip_drums(skip);
        }
        self.keys(ui);
        let skipped = self.player.notes_skipped() + self.out.dropped.load(Ordering::Relaxed) + self.late.load(Ordering::Relaxed);
        ui.small(format!(
            "{} of {} notes{}",
            group_digits(status.notes_played),
            group_digits(status.notes_total),
            if skipped > 0 { format!(" · {} left out (too dense)", group_digits(skipped)) } else { String::new() }
        ));
        if self.player.seek_progress().is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small("seeking");
            });
        }
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

/// The audio-thread side: puts each message in the block it is due in.
pub struct MidiFeed {
    rx: MidiQueue<rtrb::Consumer<Timed>>,
    /// A message taken from the queue that belongs to a later block.
    pending: Option<Timed>,
    /// When the next block starts, by this processor's clock.
    next_block: Option<Instant>,
    sample_rate: f32,
    late: Arc<AtomicU64>,
    pitch: PitchShift,
}

/// A note-on this far behind its time is left out rather than played late, so a file denser
/// than the graph can take doesn't fall further and further behind.
const LATE_LIMIT: Duration = Duration::from_millis(100);

impl MidiFeed {
    /// The messages queued, with the times the player gave them, taken as they are.
    #[cfg(test)]
    pub fn take_queued(&mut self) -> Vec<Timed> {
        let mut rx = self.rx.lock().unwrap();
        std::iter::from_fn(|| rx.pop().ok()).collect()
    }

    /// Puts the messages due in the next `n` frames into `midi_out`, at the playback `rate`.
    pub fn run(&mut self, n: usize, midi_out: &mut MidiBuf, rate: f32) {
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
        self.pitch.set_rate(rate);
        while let Some(bend) = self.pitch.next_bend() {
            if !push(midi_out, 0, bend) {
                break;
            }
        }
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
            if midi_out.events().len() + 2 > MidiBuf::CAPACITY {
                // The block is full (black MIDI): the rest go into the next ones.
                self.pending = Some((at, msg));
                break;
            }
            for m in self.pitch.map(msg).into_iter().flatten() {
                push(midi_out, time, m);
            }
        }
    }
}

/// Appends `msg` at `time`, or later if an event is already there (events come in time order;
/// a message already late stays at the block's start). False if the block is full.
fn push(midi_out: &mut MidiBuf, time: u32, msg: u32) -> bool {
    let bytes = [msg as u8, (msg >> 8) as u8, (msg >> 16) as u8];
    let Some(e) = MidiEvent::new(time, &bytes) else { return true };
    let e = MidiEvent { time: e.time.max(midi_out.events().last().map_or(0, |l| l.time)), ..e };
    midi_out.push(e)
}

const DRUMS: usize = 9;
const BEND_CENTRE: u16 = 8192;

/// The playback rate's pitch change for MIDI, as a tape played faster also sounds higher:
/// each channel's notes move by whole semitones and the rest goes on its pitch bend, which
/// follows the rate at once (and glides while it is dragged). A channel's whole-semitone
/// shift only changes while it holds no notes (pedal up too), so every note ends on the key
/// it started on and sounding notes stay in tune. The file's own bends are kept, over the
/// bend range it sets (RPN 0; 2 semitones until then, as synths have it). Channel 10 (drums)
/// is left alone. At 1× everything passes through as it is.
struct PitchShift {
    /// The shift asked for, in semitones.
    semitones: f32,
    channels: [Channel; 16],
}

#[derive(Clone, Copy)]
struct Channel {
    /// Seen a message: bends are only sent to channels in use.
    used: bool,
    /// Whole semitones the notes are moved by.
    shift: i8,
    /// Notes held per key (as the file plays them), and how many keys are held.
    held: [u8; 128],
    keys_held: u8,
    sustain: bool,
    /// The file's bend, and its range in semitones.
    bend: u16,
    range: f32,
    rpn: (u8, u8),
    /// The bend sent no longer matches.
    stale: bool,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            used: false,
            shift: 0,
            held: [0; 128],
            keys_held: 0,
            sustain: false,
            bend: BEND_CENTRE,
            range: 2.0,
            rpn: (127, 127),
            stale: false,
        }
    }
}

impl Channel {
    /// The bend to send: the file's plus what the shift leaves to it.
    fn bend_msg(&self, ch: usize, semitones: f32) -> u32 {
        let rest = semitones - f32::from(self.shift);
        let bend = if rest == 0.0 || self.range <= 0.0 {
            self.bend
        } else {
            (f32::from(self.bend) + rest / self.range * 8192.0).round().clamp(0.0, 16383.0) as u16
        };
        0xe0 | ch as u32 | u32::from(bend & 0x7f) << 8 | u32::from(bend >> 7) << 16
    }

    /// The key `key` sounds on, if there is one.
    fn key(&self, key: u32) -> Option<u32> {
        u32::try_from(key as i32 + i32::from(self.shift)).ok().filter(|k| *k < 128)
    }
}

impl PitchShift {
    fn new() -> Self {
        Self { semitones: 0.0, channels: [Channel::default(); 16] }
    }

    fn set_rate(&mut self, rate: f32) {
        let semitones = 12.0 * rate.max(1e-3).log2();
        let semitones = if semitones.abs() < 1e-4 { 0.0 } else { semitones };
        if semitones != self.semitones {
            self.semitones = semitones;
            for c in self.channels.iter_mut().filter(|c| c.used) {
                c.stale = true;
            }
        }
    }

    /// A bend that is due because the rate changed.
    fn next_bend(&mut self) -> Option<u32> {
        let (ch, c) = self.channels.iter_mut().enumerate().find(|(ch, c)| c.stale && *ch != DRUMS)?;
        c.stale = false;
        Some(c.bend_msg(ch, self.semitones))
    }

    /// What to send for the file's `msg` (status, then data bytes from the low end).
    fn map(&mut self, msg: u32) -> [Option<u32>; 2] {
        let (status, ch) = (msg & 0xf0, (msg & 0x0f) as usize);
        if !(0x80..0xf0).contains(&status) || ch == DRUMS {
            return [Some(msg), None];
        }
        let (d1, d2) = (msg >> 8 & 0x7f, msg >> 16 & 0x7f);
        let semitones = self.semitones;
        let c = &mut self.channels[ch];
        if !c.used {
            // A channel coming into use gets its bend before its first note.
            c.used = true;
            c.stale = semitones != 0.0;
        }
        let with_key = |k: u32| msg & !0x7f00 | k << 8;
        match status {
            0x90 if d2 > 0 => {
                let mut bend = None;
                let shift = semitones.round() as i8;
                if c.keys_held == 0 && !c.sustain && shift != c.shift {
                    c.shift = shift;
                    c.stale = true;
                }
                if c.stale {
                    bend = Some(c.bend_msg(ch, semitones));
                    c.stale = false;
                }
                let Some(key) = c.key(d1) else { return [bend, None] };
                let held = &mut c.held[d1 as usize];
                if *held == 0 {
                    c.keys_held += 1;
                }
                *held = held.saturating_add(1);
                [bend, Some(with_key(key))]
            }
            0x80 | 0x90 => {
                let held = &mut c.held[d1 as usize];
                if *held > 0 {
                    *held -= 1;
                    if *held == 0 {
                        c.keys_held -= 1;
                    }
                }
                [c.key(d1).map(with_key), None]
            }
            0xa0 => [c.key(d1).map(with_key), None],
            0xe0 => {
                c.bend = (d1 | d2 << 7) as u16;
                c.stale = false;
                [Some(c.bend_msg(ch, semitones)), None]
            }
            0xb0 => {
                let mut range_changed = false;
                match d1 {
                    6 if c.rpn == (0, 0) => (c.range, range_changed) = (d2 as f32 + c.range.fract(), true),
                    38 if c.rpn == (0, 0) => (c.range, range_changed) = (c.range.floor() + d2 as f32 / 100.0, true),
                    64 => c.sustain = d2 >= 64,
                    100 => c.rpn.1 = d2 as u8,
                    101 => c.rpn.0 = d2 as u8,
                    // reset all controllers: the bend goes back to the centre
                    121 => {
                        (c.bend, c.rpn, c.sustain) = (BEND_CENTRE, (127, 127), false);
                        range_changed = true;
                    }
                    120 | 123 => (c.held, c.keys_held) = ([0; 128], 0),
                    _ => {}
                }
                // The bend sent counts on the range: send it again for the new one.
                let bend = (range_changed && semitones != f32::from(c.shift)).then(|| c.bend_msg(ch, semitones));
                [Some(msg), bend]
            }
            _ => [Some(msg), None],
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
        let mut p = MidiFeed { rx: node_rx, pending: None, next_block: None, sample_rate: 48_000.0, late: Arc::default(), pitch: PitchShift::new() };
        let mut buf = MidiBuf::new();
        // Prime the clock, then send a note 5 ms into the next block and one 30 ms out.
        p.run(480, &mut buf, 1.0);
        let block = p.next_block.unwrap();
        let times = [block + Duration::from_millis(5), block + Duration::from_millis(30)];
        out.send_for_test(&[0x90 | 60 << 8 | 100 << 16, 0x80 | 60 << 8], &times);
        buf.clear();
        p.run(480, &mut buf, 1.0);
        assert_eq!(buf.events().iter().map(|e| (e.time, e.data)).collect::<Vec<_>>(), [(240, [0x90, 60, 100])]);
        // Blocks are 10 ms: 30 ms after that block's start is the first frame of the third
        // block after it, and nothing comes before.
        let mut got = Vec::new();
        for block in 1..=3 {
            buf.clear();
            p.run(480, &mut buf, 1.0);
            got.extend(buf.events().iter().map(|e| (block, e.time, e.data)));
        }
        assert_eq!(got, [(3, 0, [0x80, 60, 0])]);
    }

    fn note_on(ch: u32, key: u32) -> u32 {
        0x90 | ch | key << 8 | 100 << 16
    }

    fn note_off(ch: u32, key: u32) -> u32 {
        0x80 | ch | key << 8
    }

    fn bend(ch: u32, value: u32) -> u32 {
        0xe0 | ch | (value & 0x7f) << 8 | (value >> 7) << 16
    }

    /// The rate that shifts the pitch by `semitones`.
    fn rate(semitones: f32) -> f32 {
        2f32.powf(semitones / 12.0)
    }

    #[test]
    fn pitch_passes_through_at_normal_speed() {
        let mut p = PitchShift::new();
        p.set_rate(1.0);
        for msg in [note_on(0, 60), bend(0, 10_000), 0xb0 | 7 << 8 | 90 << 16, note_off(0, 60)] {
            assert_eq!(p.map(msg).into_iter().flatten().collect::<Vec<_>>(), [msg]);
        }
        assert_eq!(p.next_bend(), None);
    }

    #[test]
    fn pitch_moves_notes_and_bends_for_the_rest() {
        let mut p = PitchShift::new();
        p.set_rate(rate(1.3));
        // 1 semitone up on the key, 0.3 on the bend (2 semitones until the file says).
        assert_eq!(p.map(note_on(0, 60)), [Some(bend(0, 8192 + 1229)), Some(note_on(0, 61))]);
        // Dragged further while the note sounds: only the bend follows, so the note stays in
        // tune and ends on its key.
        p.set_rate(rate(2.6));
        assert_eq!(p.next_bend(), Some(bend(0, 8192 + 6554)));
        assert_eq!(p.next_bend(), None);
        assert_eq!(p.map(note_off(0, 60)), [Some(note_off(0, 61)), None]);
        // The next note takes the new whole semitones.
        assert_eq!(p.map(note_on(0, 60)), [Some(bend(0, 8192 - 1638)), Some(note_on(0, 63))]);
        // The file's own bends are kept on top.
        assert_eq!(p.map(bend(0, 8192 + 4096)), [Some(bend(0, 8192 + 4096 - 1638)), None]);
    }

    #[test]
    fn pitch_follows_the_bend_range_and_leaves_drums() {
        let mut p = PitchShift::new();
        p.set_rate(rate(0.4));
        let cc = |n: u32, v: u32| 0xb0 | 1 | n << 8 | v << 16;
        // Under half a semitone: all on the bend, sent before the channel's first note.
        assert_eq!(p.map(note_on(1, 60)), [Some(bend(1, 8192 + 1638)), Some(note_on(1, 60))]);
        // RPN 0 sets the range to 12 semitones: the bend is sent again for it.
        p.map(cc(101, 0));
        p.map(cc(100, 0));
        assert_eq!(p.map(cc(6, 12)), [Some(cc(6, 12)), Some(bend(1, 8192 + 273))]);
        // Channel 10 is drums: the keys are sounds, not pitches.
        assert_eq!(p.map(note_on(9, 36)), [Some(note_on(9, 36)), None]);
        p.set_rate(rate(3.0));
        assert_eq!(p.next_bend().map(|b| b & 0x0f), Some(1));
        assert_eq!(p.next_bend(), None);
    }
}
