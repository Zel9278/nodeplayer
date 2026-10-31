//! Standard MIDI files: loading and playback, from midi-blocks-rs (`crates/midi`, commit
//! 1a8f873), which got it from mpp-client-gui-rs. Only the file parser and the player are
//! here; tracker modules, Domino files, keyboard input and the drawing helpers are left out,
//! and the messages go to the graph's MIDI (`MidiOut`) instead of a synth.
//!
//! Loading follows `fast-note-count-info` from midi-tools: the file is
//! memory-mapped, the `MTrk` chunks are indexed, and one worker per CPU
//! takes tracks from a shared atomic index, largest first, and scans them.
//! The scan only collects what playback needs up front (note count, tempo
//! map, length), so even multi-gigabyte black MIDI loads without expanding
//! the events in memory.
//!
//! Then every track's channel messages are merged in the order they play (by
//! tick, the lower track first on one) and packed into compressed blocks in
//! memory (`packed`), and the mapping is let go. Playback runs on its own
//! thread and unpacks the blocks one after the other: due events go out with
//! the time they should sound, and which keys are down is published for
//! drawing. A seek unpacks from the last snapshot of the channels' set-up
//! before its place.

// Kept close to the original: the tempo / bar queries and load statistics aren't all used here.
#![allow(dead_code)]

mod packed;
mod prefetch;

use packed::{Event, Packed, Playhead};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ── File ───────────────────────────────────────────────────────────────

enum Data {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Data {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Data::Mapped(m) => m,
            Data::Owned(v) => v,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MidiInfo {
    pub format: u16,
    pub tracks: usize,
    pub division: u16,
    pub note_on: u64,
    /// Channel messages played (notes, controllers, bends, programs).
    pub events: u64,
    /// Of the note-ons, on MIDI channel 10 (drums).
    pub drum_notes: u64,
    /// Of those, above 127 (256-key black MIDI).
    pub high_notes: u64,
    pub tempo_changes: usize,
    pub file_bytes: usize,
    pub duration: f64,
    /// Open, map and index the tracks.
    pub index_ms: f64,
    /// Parallel scan of every track.
    pub scan_ms: f64,
    /// Merging and packing the events, and the bytes they take packed.
    pub pack_ms: f64,
    pub packed_bytes: u64,
}

#[derive(Clone, Copy)]
struct TrackSpan {
    offset: usize,
    len: usize,
}

pub struct MidiFile {
    pub name: String,
    /// Where it was read from (empty for one made in memory).
    pub path: PathBuf,
    /// The channel messages of every track, merged and packed.
    packed: Arc<Packed>,
    tempo: TempoMap,
    meter: MeterMap,
    pub info: MidiInfo,
}

impl Drop for MidiFile {
    fn drop(&mut self) {
        self.packed.close();
    }
}

/// Places in a track reading can be taken up from, saved by the load's scan a stretch of bytes
/// apart: packing cuts the file by ticks, and each stretch takes up every track from the last
/// of them before its start instead of from the track's start.
#[derive(Default)]
struct TrackCheckpoints {
    points: Vec<Checkpoint>,
    /// Each point's controllers, pitch bends and programs of the track so far, one after the
    /// other.
    slots: Vec<SlotValue>,
}

/// A chased slot's value: (chase slot, tick, message).
type SlotValue = (u16, u64, u32);

/// The reader just before an event, the note-ons before it, and where its slots end in
/// `TrackCheckpoints::slots` (they begin where the previous point's end).
#[derive(Clone, Copy)]
struct Checkpoint {
    reader: TrackReader,
    notes: NotesBefore,
    slots_end: u32,
}

/// Checkpoints in a whole file at most, whatever its size; and the fewest bytes between two.
const MAX_CHECKPOINTS: usize = 1 << 19;
const MIN_CHECKPOINT_SPACING: usize = 4 << 10;

impl TrackCheckpoints {
    /// The last point reading up to `tick` can start from: every event before it is earlier
    /// than `tick`. With its slots.
    fn before(&self, tick: u64) -> Option<(&Checkpoint, &[SlotValue])> {
        let i = self.points.partition_point(|p| p.reader.tick < tick).checked_sub(1)?;
        let start = if i == 0 { 0 } else { self.points[i - 1].slots_end as usize };
        let point = &self.points[i];
        Some((point, &self.slots[start..point.slots_end as usize]))
    }
}

fn read_be16(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[0], p[1]])
}

fn read_be32(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[0], p[1], p[2], p[3]])
}

#[inline(always)]
fn read_var_len(data: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0u64;
    while *pos < data.len() {
        let b = data[*pos];
        *pos += 1;
        value = (value << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            break;
        }
    }
    value
}

fn index_tracks(data: &[u8]) -> Result<(u16, u16, Vec<TrackSpan>), String> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return Err("Not a MIDI file (no MThd header)".into());
    }
    let header_size = read_be32(&data[4..8]) as usize;
    let format = read_be16(&data[8..10]);
    let declared = read_be16(&data[10..12]);
    let division = read_be16(&data[12..14]);
    let mut pos = 8usize.saturating_add(header_size);
    let mut tracks = Vec::with_capacity(declared.max(1) as usize);

    while pos.saturating_add(8) <= data.len() {
        if &data[pos..pos + 4] != b"MTrk" {
            pos += 1;
            continue;
        }
        let size = read_be32(&data[pos + 4..pos + 8]) as usize;
        pos += 8;
        let end = data.len().min(pos.saturating_add(size));
        tracks.push(TrackSpan {
            offset: pos,
            len: end - pos,
        });
        pos = end;
    }
    if tracks.is_empty() {
        return Err("No MIDI tracks found".into());
    }
    Ok((format, division, tracks))
}

// ── Track reader ───────────────────────────────────────────────────────

enum Item {
    /// Note on / off, control change or pitch bend, laid out for ksynth:
    /// `status | data1 << 8 | data2 << 16`.
    Channel(u32),
    /// Microseconds per quarter note.
    Tempo(u32),
    /// A time signature: beats a bar, and the beat as a power of two (2 = a quarter).
    Meter(u8, u8),
    /// Anything playback ignores.
    Other,
}

/// Walks one track's events in place.
#[derive(Clone, Copy)]
struct TrackReader {
    pos: usize,
    end: usize,
    tick: u64,
    running: u8,
}

impl TrackReader {
    fn new(span: TrackSpan) -> Self {
        Self {
            pos: span.offset,
            end: span.offset + span.len,
            tick: 0,
            running: 0,
        }
    }

    /// The next event (its tick is `self.tick`), or None at the end.
    fn next(&mut self, file: &[u8]) -> Option<Item> {
        let track = &file[..self.end];
        let len = self.end;
        loop {
            if self.pos >= len {
                return None;
            }
            self.tick += read_var_len(track, &mut self.pos);
            if self.pos >= len {
                return None;
            }
            let b = track[self.pos];
            let status = if b & 0x80 != 0 {
                self.pos += 1;
                if b < 0xf0 {
                    self.running = b;
                } else if b == 0xf0 || b == 0xf7 || b == 0xff {
                    self.running = 0;
                }
                b
            } else {
                // Running status: `b` is already the first data byte.
                if self.running == 0 {
                    self.pos += 1;
                    continue;
                }
                self.running
            };

            match status & 0xf0 {
                0x80 | 0x90 | 0xa0 | 0xb0 | 0xe0 => {
                    if self.pos + 2 > len {
                        self.pos = len;
                        return None;
                    }
                    // a note's number keeps its top bit: notes 128-255 are the "256-key"
                    // black MIDI that Kiva and Zenith show
                    let d1 = if status & 0xe0 == 0x80 { track[self.pos] } else { track[self.pos] & 0x7f };
                    let d2 = track[self.pos + 1] & 0x7f;
                    self.pos += 2;
                    if status & 0xf0 == 0xa0 {
                        return Some(Item::Other); // aftertouch: ksynth ignores it
                    }
                    return Some(Item::Channel(
                        u32::from(status) | u32::from(d1) << 8 | u32::from(d2) << 16,
                    ));
                }
                0xc0 => {
                    // program changes reach the synth (pcmmidi's sign is in the program)
                    let d1 = track[self.pos] & 0x7f;
                    self.pos += 1;
                    return Some(Item::Channel(u32::from(status) | u32::from(d1) << 8));
                }
                0xd0 => {
                    self.pos += 1;
                    return Some(Item::Other);
                }
                _ if status == 0xff => {
                    if self.pos >= len {
                        return None;
                    }
                    let kind = track[self.pos];
                    self.pos += 1;
                    let n = read_var_len(track, &mut self.pos) as usize;
                    if kind == 0x2f || len - self.pos < n {
                        self.pos = len;
                        return None;
                    }
                    let at = self.pos;
                    self.pos += n;
                    if kind == 0x51 && n == 3 {
                        let tempo = u32::from(track[at]) << 16
                            | u32::from(track[at + 1]) << 8
                            | u32::from(track[at + 2]);
                        return Some(Item::Tempo(tempo));
                    }
                    if kind == 0x58 && n >= 2 {
                        return Some(Item::Meter(track[at], track[at + 1]));
                    }
                    return Some(Item::Other);
                }
                _ if status == 0xf0 || status == 0xf7 => {
                    let n = read_var_len(track, &mut self.pos) as usize;
                    if len - self.pos < n {
                        self.pos = len;
                        return None;
                    }
                    self.pos += n;
                    return Some(Item::Other);
                }
                _ => return Some(Item::Other),
            }
        }
    }

    /// The next channel message as (tick, message).
    fn next_channel(&mut self, file: &[u8]) -> Option<(u64, u32)> {
        loop {
            if let Item::Channel(msg) = self.next(file)? {
                return Some((self.tick, msg));
            }
        }
    }
}

// ── Tempo map ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TempoPoint {
    tick: u64,
    seconds: f64,
    /// Seconds per tick from here on.
    tick_seconds: f64,
}

struct TempoMap {
    points: Vec<TempoPoint>,
}

impl TempoMap {
    /// `tempos` must be sorted by tick (stable: track order on ties).
    fn new(division: u16, tempos: &[(u64, u32)]) -> Result<Self, String> {
        if division & 0x8000 != 0 {
            // SMPTE: a fixed number of ticks per second, tempo events ignored.
            let fps = match -i32::from((division >> 8) as u8 as i8) {
                29 => 29.97,
                fps => f64::from(fps),
            };
            let ticks_per_frame = f64::from(division & 0xff);
            if fps <= 0.0 || ticks_per_frame <= 0.0 {
                return Err("Invalid SMPTE time division".into());
            }
            return Ok(Self {
                points: vec![TempoPoint {
                    tick: 0,
                    seconds: 0.0,
                    tick_seconds: 1.0 / (fps * ticks_per_frame),
                }],
            });
        }
        if division == 0 {
            return Err("Invalid time division 0".into());
        }
        let ppq = f64::from(division);
        let mut points = vec![TempoPoint {
            tick: 0,
            seconds: 0.0,
            tick_seconds: 0.5 / ppq,
        }];
        for &(tick, tempo) in tempos {
            let last = *points.last().expect("never empty");
            let tick_seconds = f64::from(tempo.max(1)) / 1e6 / ppq;
            if tick == last.tick {
                points.last_mut().expect("never empty").tick_seconds = tick_seconds;
            } else {
                points.push(TempoPoint {
                    tick,
                    seconds: last.seconds + (tick - last.tick) as f64 * last.tick_seconds,
                    tick_seconds,
                });
            }
        }
        Ok(Self { points })
    }

    fn seconds(&self, tick: u64) -> f64 {
        let i = self.points.partition_point(|p| p.tick <= tick).max(1) - 1;
        let p = self.points[i];
        p.seconds + (tick - p.tick) as f64 * p.tick_seconds
    }

    /// The (fractional) tick at `seconds`.
    fn tick_at(&self, seconds: f64) -> f64 {
        let i = self.points.partition_point(|p| p.seconds <= seconds).max(1) - 1;
        let p = self.points[i];
        p.tick as f64 + (seconds - p.seconds).max(0.0) / p.tick_seconds
    }
}

/// Ticks per beat (quarter note). SMPTE files have no beats; a beat is half a second.
fn ticks_per_beat(division: u16, tempo: &TempoMap) -> f64 {
    if division & 0x8000 != 0 {
        0.5 / tempo.points[0].tick_seconds
    } else {
        f64::from(division)
    }
}

/// A time signature in force from `beat` (in quarter notes) on, which is the start of bar
/// `bar` (counted from 0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meter {
    pub beat: f64,
    pub bar: f64,
    /// Beats a bar.
    pub num: u8,
    /// The note a beat is (4 = a quarter, 8 = an eighth).
    pub den: u16,
}

impl Meter {
    /// Quarter notes a bar.
    pub fn bar_beats(&self) -> f64 {
        f64::from(self.num) * self.unit()
    }

    /// Quarter notes a beat.
    pub fn unit(&self) -> f64 {
        4.0 / f64::from(self.den)
    }
}

/// The time signatures of a song, 4/4 until the file says otherwise. A change part way
/// through a bar starts a new bar there.
struct MeterMap {
    points: Vec<Meter>,
}

impl MeterMap {
    /// `meters` (tick, beats a bar, beat note as a power of two) sorted by tick.
    fn new(ticks_per_beat: f64, meters: &[(u64, u8, u8)]) -> Self {
        let mut points = vec![Meter { beat: 0.0, bar: 0.0, num: 4, den: 4 }];
        for &(tick, num, power) in meters {
            if num == 0 || power > 6 {
                continue;
            }
            let (beat, den) = (tick as f64 / ticks_per_beat, 1u16 << power);
            let last = *points.last().expect("never empty");
            if beat - last.beat < 1e-9 {
                let last = points.last_mut().expect("never empty");
                (last.num, last.den) = (num, den);
                continue;
            }
            let bar = last.bar + (beat - last.beat) / last.bar_beats();
            let bar = if bar.fract() < 1e-6 { bar.floor() } else { bar.ceil() };
            points.push(Meter { beat, bar, num, den });
        }
        Self { points }
    }

    fn at(&self, beat: f64) -> &Meter {
        &self.points[self.points.partition_point(|m| m.beat <= beat).max(1) - 1]
    }

    fn bar_at(&self, beat: f64) -> f64 {
        let m = self.at(beat);
        m.bar + (beat - m.beat) / m.bar_beats()
    }
}

// ── Loading ────────────────────────────────────────────────────────────

/// Run `work` on every track, largest first, one worker per CPU, and return
/// the results in track order.
fn for_each_track<T: Send>(
    track_count: usize,
    by_size: &[usize],
    work: impl Fn(usize) -> T + Sync,
) -> Vec<T> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, track_count.max(1));
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<T>> = (0..track_count).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    while let Some(&track) = by_size.get(next.fetch_add(1, Ordering::Relaxed)) {
                        local.push((track, work(track)));
                    }
                    local
                })
            })
            .collect();
        for handle in handles {
            for (track, result) in handle.join().expect("MIDI worker thread panicked") {
                results[track] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|r| r.expect("every track scanned"))
        .collect()
}

struct TrackScan {
    checkpoints: TrackCheckpoints,
    events: u64,
    note_on: u64,
    drum_notes: u64,
    high_notes: u64,
    tempos: Vec<(u64, u32)>,
    meters: Vec<(u64, u8, u8)>,
    end_tick: u64,
}

const PROGRESS_STEP: usize = 4 << 20;

fn is_note_on(msg: u32) -> bool {
    msg & 0xf0 == 0x90 && msg >> 16 & 0x7f > 0
}

/// Note-ons before a seek's position: all of them, and those on channel 10 (drums).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct NotesBefore {
    all: u64,
    drums: u64,
}

impl NotesBefore {
    /// The ones the player plays.
    fn played(self, skip_drums: bool) -> u64 {
        if skip_drums { self.all - self.drums } else { self.all }
    }
}

fn scan_track(data: &[u8], span: TrackSpan, progress: &AtomicU64, spacing: usize) -> TrackScan {
    let mut reader = TrackReader::new(span);
    let mut scan = TrackScan {
        checkpoints: TrackCheckpoints::default(),
        events: 0,
        note_on: 0,
        drum_notes: 0,
        high_notes: 0,
        tempos: Vec::new(),
        meters: Vec::new(),
        end_tick: 0,
    };
    let mut reported = span.offset;
    // the track's chase slots so far, as a seek collects them, and which are set
    let mut slots: Vec<Option<(u64, u32)>> = Vec::new();
    let mut set: Vec<u16> = Vec::new();
    let mut last_point = span.offset;
    loop {
        let before = reader;
        let Some(item) = reader.next(data) else { break };
        if before.pos - last_point >= spacing {
            // just before this event: everything before it is as a seek past it finds it
            last_point = before.pos;
            let points = &mut scan.checkpoints;
            points.slots.extend(set.iter().filter_map(|&slot| slots[usize::from(slot)].map(|(at, msg)| (slot, at, msg))));
            let notes = NotesBefore { all: scan.note_on, drums: scan.drum_notes };
            points.points.push(Checkpoint { reader: before, notes, slots_end: points.slots.len() as u32 });
        }
        scan.events += u64::from(matches!(item, Item::Channel(_)));
        match item {
            Item::Channel(msg) if is_note_on(msg) => {
                scan.note_on += 1;
                scan.drum_notes += u64::from(msg & 0x0f == 9);
                scan.high_notes += u64::from(msg >> 8 & 0xff > 127);
            }
            Item::Channel(msg) => {
                if let Some(slot) = chase_slot(msg) {
                    if slots.is_empty() {
                        slots = vec![None; CHASE_SLOTS];
                    }
                    if slots[slot].is_none() {
                        set.push(slot as u16);
                    }
                    slots[slot] = Some((reader.tick, msg));
                }
            }
            Item::Tempo(tempo) => scan.tempos.push((reader.tick, tempo)),
            Item::Meter(num, den) => scan.meters.push((reader.tick, num, den)),
            _ => {}
        }
        if reader.pos - reported >= PROGRESS_STEP {
            progress.fetch_add((reader.pos - reported) as u64, Ordering::Relaxed);
            reported = reader.pos;
        }
    }
    progress.fetch_add(
        (span.offset + span.len - reported) as u64,
        Ordering::Relaxed,
    );
    scan.end_tick = reader.tick;
    scan
}

/// How far a load has got: the track bytes scanned so far, and when the scan began (after
/// the file is mapped and its tracks indexed); then the track bytes packed, and when packing
/// began.
#[derive(Default)]
pub struct LoadCounter {
    pub bytes: AtomicU64,
    pub scan_started: OnceLock<Instant>,
    pub packed: AtomicU64,
    pub pack_started: OnceLock<Instant>,
}

impl LoadCounter {
    /// Track bytes scanned and packed: each file is read twice.
    fn read(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed) + self.packed.load(Ordering::Relaxed)
    }

    /// The share of a load of a `total` bytes long file done: the scan the first half, packing
    /// the second.
    fn share(&self, total: u64) -> f32 {
        (self.read() as f64 / (2 * total.max(1)) as f64).min(1.0) as f32
    }
}

/// Map and scan `path`, counting the progress in `progress`.
pub fn load(path: &Path, progress: &LoadCounter) -> Result<MidiFile, String> {
    let mut midi = load_midi(path, progress)?;
    midi.name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    midi.path = path.to_path_buf();
    Ok(midi)
}

fn load_midi(path: &Path, progress: &LoadCounter) -> Result<MidiFile, String> {
    let start = Instant::now();
    let file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    if file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(format!("{} is empty", path.display()));
    }
    // SAFETY: the mapping is read-only; the player assumes nobody rewrites
    // or truncates the file while it is open, like any memory-mapped reader.
    let map = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| format!("Failed to map {}: {e}", path.display()))?;
    // from a disk (or FUSE) the scan's reads all over the file are slow one by one: read it
    // through in order alongside, unless it is in memory already
    let _read_through = (!prefetch::mostly_resident(&map)).then(|| prefetch::ReadThrough::start(&file)).flatten();
    parse(Data::Mapped(map), start, progress)
}

fn parse(data: Data, start: Instant, progress: &LoadCounter) -> Result<MidiFile, String> {
    let spacing = (data.len() / MAX_CHECKPOINTS).max(MIN_CHECKPOINT_SPACING);
    parse_sized(data, start, progress, spacing, packed::SIZES)
}

/// `parse`, with checkpoints `spacing` bytes apart in each track, packed by `sizes`.
fn parse_sized(data: Data, start: Instant, progress: &LoadCounter, spacing: usize, sizes: packed::Sizes) -> Result<MidiFile, String> {
    let (format, division, tracks) = index_tracks(&data)?;
    let mut by_size: Vec<usize> = (0..tracks.len()).collect();
    by_size.sort_by_key(|&t| Reverse(tracks[t].len));
    let index_ms = start.elapsed().as_secs_f64() * 1000.0;

    let scan_start = Instant::now();
    let _ = progress.scan_started.set(scan_start);
    let mut scans = for_each_track(tracks.len(), &by_size, |t| {
        scan_track(&data, tracks[t], &progress.bytes, spacing)
    });
    let checkpoints: Vec<TrackCheckpoints> = scans.iter_mut().map(|s| std::mem::take(&mut s.checkpoints)).collect();
    let scan_ms = scan_start.elapsed().as_secs_f64() * 1000.0;
    let events = scans.iter().map(|s| s.events).sum();

    let pack_start = Instant::now();
    let _ = progress.pack_started.set(pack_start);
    let packed = Arc::new(packed::pack(&data, &tracks, &checkpoints, events, sizes, &progress.packed));
    packed::index_held(&packed);
    let pack_ms = pack_start.elapsed().as_secs_f64() * 1000.0;

    let mut tempos: Vec<(u64, u32)> = scans
        .iter()
        .flat_map(|s| s.tempos.iter().copied())
        .collect();
    tempos.sort_by_key(|&(tick, _)| tick); // stable: track order on ties
    let tempo = TempoMap::new(division, &tempos)?;
    let mut meters: Vec<(u64, u8, u8)> = scans.iter().flat_map(|s| s.meters.iter().copied()).collect();
    meters.sort_by_key(|&(tick, ..)| tick);
    let meter = MeterMap::new(ticks_per_beat(division, &tempo), &meters);
    let end_tick = scans.iter().map(|s| s.end_tick).max().unwrap_or(0);

    Ok(MidiFile {
        name: String::new(),
        path: PathBuf::new(),
        info: MidiInfo {
            format,
            tracks: tracks.len(),
            division,
            note_on: scans.iter().map(|s| s.note_on).sum(),
            events,
            drum_notes: scans.iter().map(|s| s.drum_notes).sum(),
            high_notes: scans.iter().map(|s| s.high_notes).sum(),
            tempo_changes: tempos.len(),
            file_bytes: data.len(),
            duration: tempo.seconds(end_tick),
            index_ms,
            scan_ms,
            pack_ms,
            packed_bytes: packed.bytes(),
        },
        packed,
        tempo,
        meter,
    })
}

// ── Merged playback position ───────────────────────────────────────────

/// Controller slots chased on seek: 128 controllers and pitch bend per channel.
const CHASE_SLOTS: usize = 16 * CHASE_PER_CHANNEL;
/// Per channel: 128 controllers, pitch bend, and the program.
const CHASE_PER_CHANNEL: usize = 130;

/// The slot a seek chases `msg` in: a controller (not a channel mode message), the pitch
/// bend or the program of its channel. None for the rest.
fn chase_slot(msg: u32) -> Option<usize> {
    let channel = (msg & 0x0f) as usize;
    match msg & 0xf0 {
        0xb0 if !matches!(msg >> 8 & 0x7f, 0x78..=0x7f) => Some(channel * CHASE_PER_CHANNEL + (msg >> 8 & 0x7f) as usize),
        0xe0 => Some(channel * CHASE_PER_CHANNEL + 128),
        0xc0 => Some(channel * CHASE_PER_CHANNEL + 129),
        _ => None,
    }
}

/// The messages of chased slots, in the order to send them: by channel and slot, except
/// that the parameter a data entry (CC 6, 38) sets is chosen by CC 101 and 100, which come
/// later by number: they go first.
fn chase_messages(slots: impl IntoIterator<Item = Option<u32>>) -> Vec<u32> {
    let mut chase: Vec<u32> = slots.into_iter().flatten().collect();
    chase.sort_by_key(|&msg| !(msg & 0xf0 == 0xb0 && matches!(msg >> 8 & 0x7f, 100 | 101)));
    chase
}

// ── Reading ahead ──────────────────────────────────────────────────────

impl MidiFile {
    /// The file's size in bytes.
    pub fn size(&self) -> u64 {
        self.info.file_bytes as u64
    }

    /// Bytes its events take packed in memory, with the held notes once indexed.
    pub fn packed_bytes(&self) -> u64 {
        self.packed.bytes()
    }

    /// Ticks per beat (quarter note). SMPTE files have no beats; a beat is half a second.
    /// Ticks per beat (quarter note); for SMPTE files, ticks per half second.
    pub fn ticks_per_beat(&self) -> f64 {
        ticks_per_beat(self.info.division, &self.tempo)
    }

    /// The (fractional) beat at `seconds`.
    pub fn beat_at(&self, seconds: f64) -> f64 {
        self.tempo.tick_at(seconds) / self.ticks_per_beat()
    }

    /// Quarter notes a minute at `seconds`.
    pub fn bpm_at(&self, seconds: f64) -> f64 {
        let points = &self.tempo.points;
        let p = points[points.partition_point(|p| p.seconds <= seconds).max(1) - 1];
        60.0 / (p.tick_seconds * self.ticks_per_beat())
    }

    /// The time signature in force at `beat`.
    pub fn meter_at(&self, beat: f64) -> Meter {
        *self.meter.at(beat)
    }

    /// The (fractional) bar at `beat`, counted from 0.
    pub fn bar_at(&self, beat: f64) -> f64 {
        self.meter.bar_at(beat)
    }

    /// Bars in the song, the last one counted even when it is not full.
    pub fn bars(&self) -> u64 {
        let end = self.bar_at(self.beat_at(self.info.duration));
        (end - 1e-6).ceil().max(1.0) as u64
    }

    /// The beats of the time signatures from `from` to `to` (in quarter notes), each with
    /// whether a bar starts there, appended to `out`. Stops after `max` lines and then
    /// returns false.
    pub fn beat_lines(&self, from: f64, to: f64, max: usize, out: &mut Vec<(f64, bool)>) -> bool {
        let points = &self.meter.points;
        let first = points.partition_point(|m| m.beat <= from.max(0.0)).max(1) - 1;
        let mut count = 0;
        for (i, m) in points.iter().enumerate().skip(first) {
            let end = points.get(i + 1).map_or(f64::INFINITY, |n| n.beat).min(to + 1e-9);
            let unit = m.unit();
            let mut k = ((from.max(m.beat) - m.beat) / unit - 1e-9).ceil().max(0.0);
            loop {
                let beat = m.beat + k * unit;
                if beat >= end {
                    break;
                }
                if count == max {
                    return false;
                }
                let bar = (beat - m.beat) / m.bar_beats();
                out.push((beat, (bar - bar.round()).abs() < 1e-6));
                count += 1;
                k += 1.0;
            }
            if end >= to {
                break;
            }
        }
        true
    }
}

// ── Output ─────────────────────────────────────────────────────────────

/// A message the player sends (`status | data1 << 8 | data2 << 16`) and when it should sound;
/// `None` for at once (resets, a seek's chased controllers).
pub type Timed = (Option<Instant>, u32);

/// Where the player's messages go: a queue the audio thread reads, playing each at its time.
#[derive(Clone)]
pub struct MidiOut {
    tx: Arc<Mutex<rtrb::Producer<Timed>>>,
    /// Messages that found the queue full.
    pub dropped: Arc<AtomicU64>,
}

impl MidiOut {
    pub fn new(capacity: usize) -> (Self, rtrb::Consumer<Timed>) {
        let (tx, rx) = rtrb::RingBuffer::new(capacity);
        (Self { tx: Arc::new(Mutex::new(tx)), dropped: Arc::default() }, rx)
    }

    fn send(&self, messages: &[u32]) {
        self.send_at(messages, &[]);
    }

    #[cfg(test)]
    pub fn send_for_test(&self, messages: &[u32], times: &[Instant]) {
        self.send_at(messages, times);
    }

    fn send_at(&self, messages: &[u32], times: &[Instant]) {
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        for (i, &m) in messages.iter().enumerate() {
            if tx.push((times.get(i).copied(), m)).is_err() {
                self.dropped.fetch_add((messages.len() - i) as u64, Ordering::Relaxed);
                break;
            }
        }
    }
}

// ── Player thread ──────────────────────────────────────────────────────

/// Note numbers the player keeps track of: 0-255, as 256-key black MIDI uses the top bit.
/// Only 0-127 reach the pianos.
pub const NOTES: usize = 256;

struct Control {
    file: Option<Arc<MidiFile>>,
    playhead: Option<Playhead>,
    playing: bool,
    /// Song position at `anchor`.
    position: f64,
    anchor: Instant,
    speed: f64,
    /// A seek for the player thread to carry out.
    seek_request: Option<f64>,
    seeking: bool,
    /// When the seek being carried out began.
    seek_since: Option<Instant>,
    out: Option<MidiOut>,
    /// Leave out MIDI channel 10 (drums), as Pianofall does.
    skip_drums: bool,
    /// Held notes per (channel, key).
    held: Vec<u16>,
    /// What dispatching did to the keys since the UI's counts were last brought up to date.
    keys: KeyCounts,
    /// The channels' set-up sent so far, by chase slot (`chase_slot`): handed to an output
    /// that comes later, when a file may already have started.
    channels: Vec<Option<u32>>,
    quit: bool,
}

impl Control {
    fn running(&self) -> bool {
        self.playing && !self.seeking && self.seek_request.is_none()
    }

    fn now(&self) -> f64 {
        if self.running() {
            self.position + self.anchor.elapsed().as_secs_f64() * self.speed
        } else {
            self.position
        }
    }

}

/// Changes to the keys' counts (`Shared::down`, `presses`, `channel_of`, `notes_played`)
/// gathered over a round and put there at once: black MIDI dispatches millions of notes a
/// second, and four shared counters for each would cost more than the rest.
struct KeyCounts {
    down: [i32; NOTES],
    presses: [u32; NOTES],
    channel: [u8; NOTES],
    played: u64,
}

impl Default for KeyCounts {
    fn default() -> Self {
        Self { down: [0; NOTES], presses: [0; NOTES], channel: [0; NOTES], played: 0 }
    }
}

/// Keep `msg` as its channel's set-up, if it is part of it.
fn remember(c: &mut Control, msg: u32) {
    let channel = (msg & 0x0f) as usize;
    if msg & 0xf0 == 0xb0 && msg >> 8 & 0x7f == 0x79 {
        // reset all controllers: the channel's controllers and bend go back to their defaults
        c.channels[channel * CHASE_PER_CHANNEL..channel * CHASE_PER_CHANNEL + 129].fill(None);
    } else if let Some(slot) = chase_slot(msg) {
        c.channels[slot] = Some(msg);
    }
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    /// Notes held per key (all channels), for drawing.
    down: [AtomicU32; NOTES],
    /// Note-ons per key, so the UI sees presses shorter than a frame.
    presses: [AtomicU32; NOTES],
    /// Channel of each key's latest note-on, for its colour.
    channel_of: [AtomicU8; NOTES],
    notes_played: AtomicU64,
    /// Seeks asked for so far, so a view that reads ahead can tell a seek from a slow frame.
    seeks: AtomicU64,
    /// Note-ons left out for coming too late to be heard in time.
    notes_skipped: AtomicU64,
    /// Packed bytes unpacked playing, and by seeks (each from a snapshot before its place).
    bytes_played: AtomicU64,
    bytes_scanned: AtomicU64,
    /// Set when the file plays to its end; taken by [`MidiPlayer::take_ended`].
    ended: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn note_on(&self, c: &mut Control, msg: u32, out: &mut Vec<u32>) {
        let (channel, key) = ((msg & 0x0f) as usize, (msg >> 8 & 0xff) as usize);
        let held = &mut c.held[channel * NOTES + key];
        *held = held.saturating_add(1);
        let keys = &mut c.keys;
        keys.down[key] += 1;
        keys.presses[key] += 1;
        keys.channel[key] = channel as u8;
        keys.played += 1;
        // notes above 127 (256-key black MIDI) are counted but not sent: MIDI has no such keys
        if key < 128 {
            out.push(msg);
        }
    }

    fn note_off(&self, c: &mut Control, channel: usize, key: usize, out: &mut Vec<u32>) {
        let held = &mut c.held[channel * NOTES + key];
        if *held == 0 {
            return;
        }
        *held -= 1;
        if key < 128 {
            out.push(0x80 | channel as u32 | (key as u32) << 8);
        }
        c.keys.down[key] -= 1;
    }

    /// Put what the notes dispatched since the last call did to the keys into the counts the
    /// UI reads.
    fn flush_keys(&self, c: &mut Control) {
        let keys = &mut c.keys;
        for key in 0..NOTES {
            match keys.down[key] {
                0 => {}
                d if d > 0 => {
                    self.down[key].fetch_add(d as u32, Ordering::Relaxed);
                }
                d => {
                    self.down[key].fetch_sub(d.unsigned_abs(), Ordering::Relaxed);
                }
            }
            if keys.presses[key] > 0 {
                self.presses[key].fetch_add(keys.presses[key], Ordering::Relaxed);
                self.channel_of[key].store(keys.channel[key], Ordering::Relaxed);
            }
        }
        self.notes_played.fetch_add(keys.played, Ordering::Relaxed);
        *keys = KeyCounts::default();
    }

    fn dispatch(&self, c: &mut Control, msg: u32, out: &mut Vec<u32>) {
        let status = msg as u8;
        let (channel, data1, data2) = (
            (status & 0x0f) as usize,
            (msg >> 8 & 0xff) as usize,
            msg >> 16 & 0x7f,
        );
        if c.skip_drums && channel == 9 && matches!(status & 0xf0, 0x80 | 0x90) {
            return;
        }
        match status & 0xf0 {
            0x90 if data2 > 0 => self.note_on(c, msg, out),
            0x80 | 0x90 => self.note_off(c, channel, data1, out),
            // All Sound Off / All Notes Off on this channel
            0xb0 if data1 == 0x78 || data1 == 0x7b => {
                for key in 0..NOTES {
                    while c.held[channel * NOTES + key] > 0 {
                        self.note_off(c, channel, key, out);
                    }
                }
                out.push(msg);
            }
            _ => {
                remember(c, msg);
                out.push(msg);
            }
        }
    }

    fn send(&self, c: &Control, out: &[u32]) {
        if let Some(o) = c.out.as_ref().filter(|_| !out.is_empty()) {
            o.send(out);
        }
    }

    /// Let the engine's notes go and leave the keys as they are (a pause): the notes still
    /// held come up with their note-offs once playing again.
    fn silence(&self, c: &mut Control) {
        let mut out = Vec::new();
        for channel in 0..16 {
            for key in 0..128 {
                for _ in 0..c.held[channel * NOTES + key] {
                    out.push(0x80 | channel as u32 | (key as u32) << 8);
                }
            }
        }
        self.send(c, &out);
    }

    fn release_notes(&self, c: &mut Control) {
        self.flush_keys(c);
        let mut out = Vec::new();
        for channel in 0..16 {
            for key in 0..NOTES {
                while c.held[channel * NOTES + key] > 0 {
                    self.note_off(c, channel, key, &mut out);
                }
            }
        }
        // every key is up: the releases' counts go, the keys are set to none held
        c.keys = KeyCounts::default();
        for down in &self.down {
            down.store(0, Ordering::Relaxed);
        }
        self.send(c, &out);
    }

    /// Release everything and put the channels back to their defaults: the controllers, and
    /// what a reset of all controllers leaves (GM RP-015) and a file without its own set-up
    /// would otherwise take from the last one: bank, program, reverb, chorus, the bend
    /// range and tuning.
    fn reset(&self, c: &mut Control) {
        self.release_notes(c);
        c.channels.fill(None);
        let mut out = Vec::new();
        for ch in 0..16u32 {
            let cc = |n: u32, v: u32| 0xb0 | ch | n << 8 | v << 16;
            out.push(cc(0x79, 0)); // reset all controllers (and the RPN chosen)
            out.push(cc(0x40, 0)); // sustain off
            out.push(cc(0x07, 100)); // volume
            out.push(cc(0x0b, 127)); // expression
            out.push(cc(0x01, 0)); // modulation
            out.push(cc(0x0a, 64)); // pan centre
            out.push(cc(0x5b, 40)); // reverb
            out.push(cc(0x5d, 0)); // chorus
            out.push(0xe0 | ch | 0x40 << 16); // pitch bend centre
            // RPN 0, 1, 2: a bend range of 2 semitones, no fine or coarse tuning; then none chosen
            for (rpn, msb, lsb) in [(0, 2, 0), (1, 64, 0), (2, 64, 0)] {
                out.extend([cc(101, 0), cc(100, rpn), cc(6, msb), cc(38, lsb)]);
            }
            out.extend([cc(101, 127), cc(100, 127)]);
            out.extend([cc(0x00, 0), cc(0x20, 0), 0xc0 | ch]); // bank 0, program 0
        }
        self.send(c, &out);
    }

    fn run(&self) {
        let mut out = Vec::new();
        let mut stamps: Vec<Instant> = Vec::new();
        let mut due_ticks: Vec<Event> = Vec::new();
        // seconds of the song a round takes at most (`ROUND_EVENTS`)
        let mut round_span = MAX_ROUND_SPAN;
        let mut c = self.lock();
        loop {
            if c.quit {
                c.playing = false;
                self.reset(&mut c);
                return;
            }
            if let Some(target) = c.seek_request.take() {
                let Some(file) = c.file.clone() else { continue };
                self.reset(&mut c);
                c.seeking = true;
                c.seek_since.get_or_insert_with(Instant::now);
                // Scan without the lock so the UI keeps drawing.
                drop(c);
                let target = target.clamp(0.0, file.info.duration);
                let tick = file.tempo.tick_at(target).ceil() as u64;
                let (playhead, chase, notes) = Playhead::seek(&file.packed, tick);
                let (held, held_bytes) = Playhead::held_at(&file.packed, tick);
                self.bytes_scanned.fetch_add(playhead.scanned + held_bytes, Ordering::Relaxed);
                c = self.lock();
                c.seeking = false;
                if c.seek_request.is_none() {
                    c.seek_since = None;
                }
                if c.seek_request.is_none()
                    && c.file.as_ref().is_some_and(|f| Arc::ptr_eq(f, &file))
                {
                    c.playhead = Some(playhead);
                    c.position = target;
                    c.anchor = Instant::now();
                    // the notes up to here count as played, after a seek back too
                    self.notes_played.store(notes.played(c.skip_drums), Ordering::Relaxed);
                    for &msg in &chase {
                        remember(&mut c, msg);
                    }
                    self.send(&c, &chase);
                    // the notes struck before here and still held hold their keys down; they
                    // are not struck again, and come up with their note-offs
                    for &(_, _, msg) in &held {
                        let (channel, key) = ((msg & 0x0f) as usize, (msg >> 8 & 0xff) as usize);
                        if !(c.skip_drums && channel == 9) {
                            c.held[channel * NOTES + key] = c.held[channel * NOTES + key].saturating_add(1);
                            c.keys.down[key] += 1;
                        }
                    }
                    self.flush_keys(&mut c);
                }
                continue;
            }
            // Paused, the playhead stays put: taking it would lose the position.
            let Some(file) = c.file.clone().filter(|_| c.playing && c.playhead.is_some()) else {
                c = self.wake.wait(c).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let mut playhead = c.playhead.take().expect("checked above");

            let now = c.now();
            let speed = c.speed;
            let (anchor, origin) = (c.anchor, c.position);
            let due = file.tempo.tick_at(now);
            // Read the due events without the lock: black MIDI has millions a
            // second, and the UI takes the lock every frame.
            drop(c);
            // Behind (a file denser than can be played, or a stall), a round takes a stretch
            // of the song at a time, sized to keep it to about `ROUND_EVENTS`: all that is due
            // at once would need gigabytes. The rounds then follow each other without a wait.
            let target = due.floor().max(0.0) as u64;
            let round_due = match playhead.peek_tick() {
                Some(next) => target.min(file.tempo.tick_at(file.tempo.seconds(next) + round_span).ceil() as u64),
                None => target,
            };
            let behind = round_due < target;
            // When each message is due, so the engine can start it at its
            // own time inside an audio buffer. The times count from the
            // clock's anchor, not from when this round woke up, so they keep
            // their exact spacing from one round to the next (pcmmidi plays
            // a recording one sample a tick).
            due_ticks.clear();
            playhead.drain_until(round_due, &mut due_ticks);
            if due_ticks.len() > ROUND_EVENTS {
                round_span = (round_span / 2.0).max(MIN_ROUND_SPAN);
            } else if due_ticks.len() < ROUND_EVENTS / 4 {
                round_span = (round_span * 2.0).min(MAX_ROUND_SPAN);
            }
            self.bytes_played.fetch_add(std::mem::take(&mut playhead.walked), Ordering::Relaxed);
            c = self.lock();
            if !c.file.as_ref().is_some_and(|f| Arc::ptr_eq(f, &file)) || c.playhead.is_some() {
                // another file came in meanwhile
                continue;
            }
            // notes due so long ago they would be heard late are left out (still counted)
            let too_late = Instant::now().checked_sub(LATE_LIMIT);
            let mut skipped = 0;
            // black MIDI has many events on a tick: each tick's time is worked out once
            let mut last: Option<(u64, Instant)> = None;
            for &(tick, _track, msg) in &due_ticks {
                let at = match last {
                    Some((t, at)) if t == tick => at,
                    _ => {
                        let offset = (file.tempo.seconds(tick) - origin) / speed;
                        let at = if offset >= 0.0 {
                            anchor + Duration::from_secs_f64(offset)
                        } else {
                            anchor.checked_sub(Duration::from_secs_f64(-offset)).unwrap_or(anchor)
                        } + PLAYBACK_LATENCY;
                        last = Some((tick, at));
                        at
                    }
                };
                if is_note_on(msg) && too_late.is_some_and(|t| at < t) {
                    skipped += 1;
                    continue;
                }
                let before = out.len();
                self.dispatch(&mut c, msg, &mut out);
                stamps.resize(out.len().max(before), at);
            }
            c.keys.played += skipped;
            self.notes_skipped.fetch_add(skipped, Ordering::Relaxed);
            self.flush_keys(&mut c);
            if let Some(o) = c.out.clone().filter(|_| !out.is_empty()) {
                // and send them without it
                drop(c);
                o.send_at(&out, &stamps);
                c = self.lock();
            }
            out.clear();
            stamps.clear();
            if !c.file.as_ref().is_some_and(|f| Arc::ptr_eq(f, &file)) || c.playhead.is_some() {
                continue;
            }

            let next = playhead.peek_tick();
            c.playhead = Some(playhead);
            if behind && next.is_some() {
                // the next round at once, but first a moment without the lock: taken back
                // straight away, it would keep the UI waiting for frames on end
                drop(c);
                std::thread::sleep(Duration::from_micros(250));
                c = self.lock();
                continue;
            }
            let Some(next) = next else {
                // Finished.
                c.playing = false;
                c.position = file.info.duration;
                self.release_notes(&mut c);
                self.ended.store(true, Ordering::Relaxed);
                continue;
            };
            // At least 2 ms between rounds: dense files would otherwise spin
            // on the lock the UI needs (ksynth takes events per buffer anyway).
            let wait = ((file.tempo.seconds(next) - now) / c.speed).clamp(0.002, 0.05);
            c = self
                .wake
                .wait_timeout(c, Duration::from_secs_f64(wait))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Events a round of the player reads at about most, and the stretches of the song (seconds)
/// it takes between; a note-on due longer ago than `LATE_LIMIT` is left out.
const ROUND_EVENTS: usize = 1 << 19;
const MIN_ROUND_SPAN: f64 = 1e-4;
const MAX_ROUND_SPAN: f64 = 0.25;
#[cfg(not(test))]
const LATE_LIMIT: Duration = Duration::from_millis(100);
/// CI machines can stall the player's thread for longer than that, and the tests check what
/// gets played, not this.
#[cfg(test)]
const LATE_LIMIT: Duration = Duration::from_secs(10);

/// File notes are sent this long before they are due, so each lands in an audio block that is
/// not rendered yet and starts at its own frame (the player wakes every 2 ms or more).
const PLAYBACK_LATENCY: Duration = Duration::from_millis(20);

// ── UI side ────────────────────────────────────────────────────────────

/// Settings kept between runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiSettings {
    pub last_path: String,
    pub speed: f32,
    pub skip_drums: bool,
}

impl Default for MidiSettings {
    fn default() -> Self {
        Self {
            last_path: String::new(),
            speed: 1.0,
            skip_drums: true,
        }
    }
}

/// What the HUD shows about the player.
pub struct Status {
    pub name: Option<String>,
    pub position: f64,
    pub duration: f64,
    pub playing: bool,
    pub notes_played: u64,
    pub notes_total: u64,
    pub loading: Option<f32>,
}

enum LoadState {
    Idle,
    Loading {
        name: String,
        rx: mpsc::Receiver<Result<MidiFile, String>>,
        play: bool,
        /// Where to start once loaded.
        seek: Option<f64>,
        progress: Arc<LoadCounter>,
        total: u64,
        started: Instant,
    },
    Failed(String),
}

/// A MIDI file being loaded: how many of its bytes have been scanned, then packed, and for how
/// long.
#[derive(Clone, Debug)]
pub struct LoadProgress {
    pub name: String,
    /// Track bytes scanned so far; 0 while the file is mapped and its tracks indexed.
    pub done: u64,
    /// The file's size.
    pub total: u64,
    pub elapsed: Duration,
    /// How long the scan has run, once it has begun.
    pub scanning: Option<Duration>,
    /// Once the scan is over: track bytes packed so far, and for how long.
    pub packed: u64,
    pub packing: Option<Duration>,
}

/// A seek being carried out: the position it goes to, and for how long it has taken.
#[derive(Clone, Copy, Debug)]
pub struct SeekProgress {
    pub target: f64,
    pub elapsed: Duration,
}

pub struct MidiPlayer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub settings: MidiSettings,
    load: LoadState,
    loaded: Option<(String, MidiInfo)>,
    /// Bytes the loads that are over scanned and packed.
    bytes_loaded: u64,
}

/// Bytes of MIDI files read so far, by what: loading reads the file, the rest unpack it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BytesRead {
    /// Loading: each file is scanned whole, then packed.
    pub loading: u64,
    /// Packed bytes unpacked playing.
    pub playing: u64,
    /// Packed bytes unpacked by seeks: each from a snapshot of the set-up before its place.
    pub seeks: u64,
}

impl MidiPlayer {
    pub fn new(settings: MidiSettings) -> Self {
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                file: None,
                playhead: None,
                playing: false,
                position: 0.0,
                anchor: Instant::now(),
                speed: f64::from(settings.speed),
                seek_request: None,
                seeking: false,
                seek_since: None,
                out: None,
                skip_drums: settings.skip_drums,
                held: vec![0; 16 * NOTES],
                keys: KeyCounts::default(),
                channels: vec![None; CHASE_SLOTS],
                quit: false,
            }),
            wake: Condvar::new(),
            down: std::array::from_fn(|_| AtomicU32::new(0)),
            presses: std::array::from_fn(|_| AtomicU32::new(0)),
            channel_of: std::array::from_fn(|_| AtomicU8::new(0)),
            notes_played: AtomicU64::new(0),
            seeks: AtomicU64::new(0),
            notes_skipped: AtomicU64::new(0),
            bytes_played: AtomicU64::new(0),
            bytes_scanned: AtomicU64::new(0),
            ended: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("midi-player".into())
            .spawn(move || worker.run())
            .expect("MIDI player thread");
        Self { shared, thread: Some(thread), settings, load: LoadState::Idle, loaded: None, bytes_loaded: 0 }
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.shared.lock()
    }

    /// Load `path` in the background; `play` starts it once loaded.
    pub fn open(&mut self, path: PathBuf, play: bool) {
        self.settings.last_path = path.display().to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.settings.last_path.clone());
        log::info!("Loading MIDI file {}", path.display());
        let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let progress = Arc::new(LoadCounter::default());
        let (tx, rx) = mpsc::channel();
        let counter = progress.clone();
        std::thread::Builder::new()
            .name("midi-load".into())
            .spawn(move || {
                let _ = tx.send(load(&path, &counter));
            })
            .expect("MIDI loader thread");
        self.count_load();
        self.shared.ended.store(false, Ordering::Relaxed);
        self.load = LoadState::Loading {
            name,
            rx,
            play,
            seek: None,
            progress,
            total,
            started: Instant::now(),
        };
    }

    fn install(&mut self, file: MidiFile, play: bool, seek: Option<f64>) {
        let info = &file.info;
        log::info!(
            "MIDI loaded: {} ({} tracks, {} notes, {}) in {:.0} ms (index {:.0}, scan {:.0}, pack {:.0}); {} events packed in {:.1} MB",
            file.name,
            info.tracks,
            group_digits(info.note_on),
            format_time(info.duration),
            info.index_ms + info.scan_ms + info.pack_ms,
            info.index_ms,
            info.scan_ms,
            info.pack_ms,
            group_digits(info.events),
            info.packed_bytes as f64 / 1e6
        );
        self.loaded = Some((file.name.clone(), file.info.clone()));
        let playhead = Playhead::start(&file.packed);
        {
            let mut c = self.control();
            c.playing = false;
            self.shared.reset(&mut c);
            c.file = Some(Arc::new(file));
            c.playhead = Some(playhead);
            c.seek_request = None;
            c.position = 0.0;
        }
        self.shared.notes_played.store(0, Ordering::Relaxed);
        self.shared.ended.store(false, Ordering::Relaxed);
        if let Some(at) = seek {
            self.seek(at);
        }
        if play {
            self.play();
        }
    }

    /// True once after the file played to its end.
    pub fn take_ended(&self) -> bool {
        self.shared.ended.swap(false, Ordering::Relaxed)
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.load, LoadState::Loading { .. })
    }

    /// While a file loads: whether it starts playing once loaded (else false).
    pub fn plays_when_loaded(&self) -> bool {
        matches!(self.load, LoadState::Loading { play: true, .. })
    }

    /// While a file loads: start it once loaded, or not. Returns false if nothing is loading.
    pub fn play_when_loaded(&mut self, on: bool) -> bool {
        match &mut self.load {
            LoadState::Loading { play, .. } => {
                *play = on;
                true
            }
            _ => false,
        }
    }

    /// While a file loads: start it at `position` once loaded. Returns false if nothing is
    /// loading.
    pub fn seek_when_loaded(&mut self, position: f64) -> bool {
        match &mut self.load {
            LoadState::Loading { seek, .. } => {
                *seek = Some(position);
                true
            }
            _ => false,
        }
    }

    /// Send to `out` from now on, starting with the channels' set-up so far (the file may
    /// have started before it was there).
    pub fn set_output(&self, out: Option<MidiOut>) {
        let mut c = self.control();
        if let Some(o) = &out {
            o.send(&chase_messages(c.channels.iter().copied()));
        }
        c.out = out;
    }

    /// Notes held by the file, per key, for drawing.
    pub fn key_down(&self, key: usize) -> bool {
        self.shared.down[key].load(Ordering::Relaxed) > 0
    }

    /// MIDI channel of the latest note-on on `key`.
    pub fn channel(&self, key: usize) -> u8 {
        self.shared.channel_of[key].load(Ordering::Relaxed)
    }

    /// Note-ons per key so far (also catches presses shorter than a frame).
    pub fn presses(&self, key: usize) -> u32 {
        self.shared.presses[key].load(Ordering::Relaxed)
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// The file being played.
    pub fn file(&self) -> Option<Arc<MidiFile>> {
        self.control().file.clone()
    }

    pub fn status(&self) -> Status {
        let (playing, now) = {
            let c = self.control();
            (c.playing, c.now())
        };
        Status {
            name: self.loaded.as_ref().map(|(n, _)| n.clone()),
            position: now.min(self.duration()),
            duration: self.duration(),
            playing,
            notes_played: self.shared.notes_played.load(Ordering::Relaxed),
            notes_total: self.loaded.as_ref().map_or(0, |(_, i)| {
                if self.settings.skip_drums { i.note_on - i.drum_notes } else { i.note_on }
            }),
            loading: match &self.load {
                LoadState::Loading { progress, total, .. } => Some(progress.share(*total)),
                _ => None,
            },
        }
    }

    /// Seeks asked for so far (a new file starts at 0 again only by its own seek count).
    pub fn seeks(&self) -> u64 {
        self.shared.seeks.load(Ordering::Relaxed)
    }

    /// Note-ons left out so far for coming too late (a file denser than this machine plays).
    pub fn notes_skipped(&self) -> u64 {
        self.shared.notes_skipped.load(Ordering::Relaxed)
    }

    /// Bytes of MIDI files walked through so far: loading, playing and seeking.
    pub fn bytes_read(&self) -> BytesRead {
        let loading = match &self.load {
            LoadState::Loading { progress, .. } => progress.read(),
            _ => 0,
        };
        BytesRead {
            loading: self.bytes_loaded + loading,
            playing: self.shared.bytes_played.load(Ordering::Relaxed),
            seeks: self.shared.bytes_scanned.load(Ordering::Relaxed),
        }
    }

    /// The load under way is over (or another takes its place): count what it scanned.
    fn count_load(&mut self) {
        if let LoadState::Loading { progress, .. } = &self.load {
            self.bytes_loaded += progress.read();
        }
    }

    /// The file being loaded, if any.
    pub fn load_progress(&self) -> Option<LoadProgress> {
        match &self.load {
            LoadState::Loading { name, progress, total, started, .. } => Some(LoadProgress {
                name: name.clone(),
                done: progress.bytes.load(Ordering::Relaxed),
                total: *total,
                elapsed: started.elapsed(),
                scanning: progress.scan_started.get().map(Instant::elapsed),
                packed: progress.packed.load(Ordering::Relaxed),
                packing: progress.pack_started.get().map(Instant::elapsed),
            }),
            _ => None,
        }
    }

    /// The seek being carried out, if any. Black MIDI takes a while to seek, as every
    /// track is read up to the new position.
    pub fn seek_progress(&self) -> Option<SeekProgress> {
        let c = self.control();
        (c.seeking || c.seek_request.is_some()).then(|| SeekProgress {
            target: c.position,
            elapsed: c.seek_since.map_or(Duration::ZERO, |t| t.elapsed()),
        })
    }

    pub fn toggle(&self) {
        if self.control().playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn set_skip_drums(&mut self, on: bool) {
        self.settings.skip_drums = on;
        let mut c = self.control();
        c.skip_drums = on;
        self.shared.release_notes(&mut c);
    }

    fn duration(&self) -> f64 {
        self.loaded.as_ref().map_or(0.0, |(_, info)| info.duration)
    }

    pub fn play(&self) {
        let mut c = self.control();
        if c.file.is_none() {
            return;
        }
        if c.seek_request.is_none() && c.playhead.as_ref().is_none_or(|p| p.peek_tick().is_none()) {
            c.seek_request = Some(0.0);
            self.shared.seeks.fetch_add(1, Ordering::Relaxed);
            c.position = 0.0;
        }
        c.anchor = Instant::now();
        c.playing = true;
        self.shared.wake.notify_all();
    }

    /// Stop where it is: the sound stops, the keys stay as they were.
    pub fn pause(&self) {
        let mut c = self.control();
        c.position = c.now();
        c.playing = false;
        self.shared.silence(&mut c);
    }

    pub fn stop(&self) {
        let mut c = self.control();
        c.playing = false;
        if c.file.is_some() {
            c.seek_request = Some(0.0);
            self.shared.seeks.fetch_add(1, Ordering::Relaxed);
            c.position = 0.0;
        }
        self.shared.wake.notify_all();
    }

    pub fn seek(&self, position: f64) {
        let mut c = self.control();
        if c.file.is_none() {
            return;
        }
        c.seek_request = Some(position);
        self.shared.seeks.fetch_add(1, Ordering::Relaxed);
        c.position = position;
        self.shared.wake.notify_all();
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.settings.speed = speed;
        let mut c = self.control();
        c.position = c.now();
        c.anchor = Instant::now();
        c.speed = f64::from(speed);
        self.shared.wake.notify_all();
    }

    /// Per frame: finish loads.
    pub fn update(&mut self) {
        if let LoadState::Loading { rx, play, seek, .. } = &self.load
            && let Ok(result) = rx.try_recv()
        {
            let (play, seek) = (*play, *seek);
            self.count_load();
            self.load = LoadState::Idle;
            match result {
                Ok(file) => self.install(file, play, seek),
                Err(e) => {
                    log::error!("MIDI load failed: {e}");
                    self.load = LoadState::Failed(e);
                }
            }
        }
    }

    /// Why the last load failed, if it did.
    pub fn load_error(&self) -> Option<&str> {
        match &self.load {
            LoadState::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn info(&self) -> Option<&MidiInfo> {
        self.loaded.as_ref().map(|(_, i)| i)
    }

    pub fn is_playing(&self) -> bool {
        self.control().playing
    }
}

impl MidiPlayer {
    /// Stop the player thread; it lets go of every note on the way out.
    pub fn shutdown(&mut self) {
        self.control().quit = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for MidiPlayer {
    fn drop(&mut self) {
        self.control().quit = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn format_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var_len(mut v: u32) -> Vec<u8> {
        let mut bytes = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            bytes.insert(0, (v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        bytes
    }

    fn track(events: &[(u32, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (delta, bytes) in events {
            body.extend(var_len(*delta));
            body.extend_from_slice(bytes);
        }
        body.extend([0x00, 0xff, 0x2f, 0x00]);
        let mut chunk = b"MTrk".to_vec();
        chunk.extend((body.len() as u32).to_be_bytes());
        chunk.extend(body);
        chunk
    }

    fn file(division: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
        let mut data = b"MThd".to_vec();
        data.extend(6u32.to_be_bytes());
        data.extend(1u16.to_be_bytes());
        data.extend((tracks.len() as u16).to_be_bytes());
        data.extend(division.to_be_bytes());
        for t in tracks {
            data.extend(t);
        }
        data
    }

    fn parse_bytes(bytes: Vec<u8>) -> Result<MidiFile, String> {
        parse(Data::Owned(bytes), Instant::now(), &LoadCounter::default())
    }

    /// Packed in small blocks, cut in many stretches, with checkpoints `spacing` bytes apart:
    /// all the seams packing has, in a small file.
    fn parse_small(bytes: &[u8], spacing: usize) -> MidiFile {
        let sizes = packed::Sizes { block_events: 37, ranges_per_cpu: 3, max_snapshots: 5 };
        parse_sized(Data::Owned(bytes.to_vec()), Instant::now(), &LoadCounter::default(), spacing, sizes).unwrap()
    }

    /// The file's channel messages merged the plain way: each track read whole, all of them
    /// put in order of their ticks, keeping the tracks' order on one tick.
    fn merged(bytes: &[u8]) -> Vec<Event> {
        let (_, _, tracks) = index_tracks(bytes).unwrap();
        let mut all = Vec::new();
        for (t, &span) in tracks.iter().enumerate() {
            let mut reader = TrackReader::new(span);
            while let Some((tick, msg)) = reader.next_channel(bytes) {
                all.push((tick, t, msg));
            }
        }
        all.sort_by_key(|e| e.0);
        all
    }

    /// A small generator, so the tests need no crate.
    fn generator(mut seed: u64) -> impl FnMut(u32) -> u32 {
        move |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % u64::from(n)) as u32
        }
    }

    /// Tracks of notes, controllers, programs, bends and other events at random steps: many on
    /// one tick, in one track and across them.
    fn random_tracks(seed: u64, tracks: usize, events: usize) -> Vec<u8> {
        let mut next = generator(seed);
        let tracks: Vec<Vec<u8>> = (0..tracks)
            .map(|t| {
                let ch = (t % 16) as u8;
                let events: Vec<(u32, Vec<u8>)> = (0..events)
                    .map(|i| {
                        let event = match next(10) {
                            0 => vec![0xb0 | ch, [7, 10, 11, 64][next(4) as usize], next(128) as u8],
                            1 => vec![0xc0 | ch, next(128) as u8],
                            2 => vec![0xe0 | ch, next(128) as u8, next(128) as u8],
                            3 => vec![0xff, 0x01, 0x02, b'h', b'i'],
                            _ => vec![0x90 | ch, (i % 128) as u8, next(128) as u8],
                        };
                        (next(4) * next(3), event)
                    })
                    .collect();
                let refs: Vec<(u32, &[u8])> = events.iter().map(|(d, e)| (*d, &e[..])).collect();
                track(&refs)
            })
            .collect();
        file(480, &tracks)
    }

    fn all_events(midi: &MidiFile, mut playhead: Playhead) -> Vec<(f64, u32)> {
        std::iter::from_fn(|| playhead.pop())
            .map(|(tick, _, msg)| (midi.tempo.seconds(tick), msg))
            .collect()
    }

    fn msg(status: u32, a: u32, b: u32) -> u32 {
        status | a << 8 | b << 16
    }

    fn song() -> Vec<u8> {
        // 480 ticks per quarter; 120 BPM, then 60 BPM from tick 960.
        let conductor = track(&[
            (0, &[0xff, 0x51, 0x03, 0x07, 0xa1, 0x20]),
            (960, &[0xff, 0x51, 0x03, 0x0f, 0x42, 0x40]),
        ]);
        // Running status after the first note on, note on velocity 0 as off,
        // aftertouch that is skipped and a program change that is not.
        let piano = track(&[
            (0, &[0x90, 60, 100]),
            (480, &[60, 0]),
            (0, &[0xa0, 60, 10]),
            (0, &[0xc0, 5]),
            (480, &[0x90, 64, 90]),
            (480, &[0x80, 64, 0]),
        ]);
        let bass = track(&[
            (0, &[0x91, 36, 80]),
            (0, &[0xb1, 0x07, 90]),
            (960, &[0xb1, 0x40, 127]),
            (480, &[0x81, 36, 0]),
        ]);
        file(480, &[conductor, piano, bass])
    }

    #[test]
    fn merges_tracks_and_follows_tempo_changes() {
        let midi = parse_bytes(song()).unwrap();
        assert_eq!(
            all_events(&midi, Playhead::start(&midi.packed)),
            vec![
                (0.0, msg(0x90, 60, 100)),
                (0.0, msg(0x91, 36, 80)),
                (0.0, msg(0xb1, 0x07, 90)),
                (0.5, msg(0x90, 60, 0)),
                (0.5, msg(0xc0, 5, 0)),
                (1.0, msg(0x90, 64, 90)),
                (1.0, msg(0xb1, 0x40, 127)),
                (2.0, msg(0x80, 64, 0)),
                (2.0, msg(0x81, 36, 0)),
            ]
        );
        assert_eq!(midi.info.tracks, 3);
        assert_eq!(midi.info.note_on, 3);
        assert_eq!(midi.info.tempo_changes, 2);
        assert_eq!(midi.info.duration, 2.0);
        assert_eq!(midi.tempo.tick_at(1.5), 1200.0);
    }

    #[test]
    fn seeking_skips_events_and_chases_controllers() {
        let midi = parse_bytes(song()).unwrap();
        let (playhead, chase, notes) = Playhead::seek(&midi.packed, midi.tempo.tick_at(1.5).ceil() as u64);
        // the piano's program, then the bass's controllers
        assert_eq!(chase, vec![msg(0xc0, 5, 0), msg(0xb1, 0x07, 90), msg(0xb1, 0x40, 127)]);
        assert_eq!(
            all_events(&midi, playhead),
            vec![(2.0, msg(0x80, 64, 0)), (2.0, msg(0x81, 36, 0))]
        );
        assert_eq!(notes, NotesBefore { all: 3, drums: 0 });
    }

    /// Packing gives the events of all the tracks merged in play order, cut anywhere (blocks,
    /// stretches packed side by side, checkpoints); popped one at a time or drained a round at
    /// a time (rounds that end on a tick, between, far ahead).
    #[test]
    fn packing_merges_the_tracks_in_play_order() {
        let bytes = random_tracks(0x2545_f491_4f6c_dd1d, 40, 300);
        let expected = merged(&bytes);
        for midi in [parse_bytes(bytes.clone()).unwrap(), parse_small(&bytes, 64)] {
            assert_eq!(midi.packed.events(), expected.len() as u64);
            assert_eq!(midi.info.events, expected.len() as u64);
            let mut playhead = Playhead::start(&midi.packed);
            let popped: Vec<Event> = std::iter::from_fn(|| playhead.pop()).collect();
            assert!(popped == expected, "popped in play order");
            let mut next = generator(7);
            let mut drained = Vec::new();
            let mut playhead = Playhead::start(&midi.packed);
            let mut due = 0;
            while playhead.peek_tick().is_some() {
                playhead.drain_until(due, &mut drained);
                assert!(drained.last().is_none_or(|e| e.0 <= due));
                due += u64::from(next(40));
            }
            assert!(drained == expected, "drained in play order");
        }
        assert!(parse_small(&bytes, 64).packed.bytes() < bytes.len() as u64 * 2, "not much bigger, even in small blocks");
    }

    /// A seek finds what reading from the start finds: the controllers, programs and bends in
    /// effect, the notes before and the events on; from the snapshot before its place, which
    /// reads less than from the start.
    #[test]
    fn seeking_finds_what_reading_from_the_start_finds() {
        let bytes = random_tracks(0x9e37_79b9_7f4a_7c15, 12, 400);
        let all = merged(&bytes);
        let midi = parse_small(&bytes, 64);
        let end = all.last().unwrap().0;
        for target in (0..=end + 2).step_by(37) {
            let (playhead, chase, notes) = Playhead::seek(&midi.packed, target);
            let before = all.partition_point(|e| e.0 < target);
            let mut slots = vec![None; CHASE_SLOTS];
            let mut expected = NotesBefore::default();
            for &(_, _, msg) in &all[..before] {
                if is_note_on(msg) {
                    expected.all += 1;
                    expected.drums += u64::from(msg & 0x0f == 9);
                } else if let Some(slot) = chase_slot(msg) {
                    slots[slot] = Some(msg);
                }
            }
            assert_eq!(chase, chase_messages(slots), "at {target}");
            assert_eq!(notes, expected, "at {target}");
            let rest: Vec<(f64, u32)> = all[before..].iter().map(|&(tick, _, msg)| (midi.tempo.seconds(tick), msg)).collect();
            assert_eq!(all_events(&midi, playhead), rest, "at {target}");
        }
        let (half, _, _) = Playhead::seek(&midi.packed, end / 2);
        let all = midi.packed.block_bytes();
        assert!(half.scanned * 2 < all, "{} of {all}", half.scanned);
    }

    /// The channels' set-up played so far is kept for an output that comes later: the latest of each controller and program, a
    /// reset of all controllers clears the channel's controllers, and a reset forgets it all.
    #[test]
    fn the_channels_set_up_is_kept_for_a_synth_that_comes_later() {
        let player = MidiPlayer::new(MidiSettings::default());
        let shared = &player.shared;
        let mut c = shared.lock();
        let mut out = Vec::new();
        for m in [
            msg(0xc1, 74, 0),
            msg(0xb1, 10, 1),
            msg(0xb1, 10, 127),
            msg(0xb0, 7, 100),
            msg(0x91, 60, 100),
            msg(0xc0, 3, 0),
            msg(0xb0, 0x79, 0),
        ] {
            shared.dispatch(&mut c, m, &mut out);
        }
        assert_eq!(chase_messages(c.channels.iter().copied()), [msg(0xc0, 3, 0), msg(0xb1, 10, 127), msg(0xc1, 74, 0)]);
        shared.reset(&mut c);
        assert!(c.channels.iter().all(Option::is_none));
    }

    /// A seek chases a pitch bend range so it still lands on RPN 0: the RPN chosen first,
    /// then the data entry, though the numbers come the other way round.
    #[test]
    fn a_seek_chases_the_bend_range_in_order() {
        let t = track(&[
            (0, &[0xb0, 101, 0]),
            (0, &[0xb0, 100, 0]),
            (0, &[0xb0, 6, 12]),
            (0, &[0xb0, 38, 0]),
            (960, &[0x90, 60, 100]),
            (480, &[0x80, 60, 0]),
        ]);
        let midi = parse_bytes(file(480, &[t])).unwrap();
        let (_, chase, _) = Playhead::seek(&midi.packed, 900);
        let order: Vec<u32> = chase.iter().map(|m| m >> 8 & 0x7f).collect();
        assert_eq!(order, [100, 101, 6, 38]);
    }

    /// After a seek, the notes played are the ones before the new position, back or forth;
    /// left-out drums count in neither the played nor the total.
    #[test]
    fn a_seek_recounts_the_notes_played() {
        let mut player = MidiPlayer::new(MidiSettings { skip_drums: false, ..Default::default() });
        player.install(parse_bytes(song()).unwrap(), false, None);
        let played = |p: &MidiPlayer| p.status().notes_played;
        player.seek(1.5);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(played(&player), 3);
        player.seek(0.2);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(played(&player), 2, "seeking back counts back");

        let drums = track(&[(0, &[0x99, 36, 100]), (480, &[0x89, 36, 0]), (0, &[0x90, 60, 100]), (480, &[0x80, 60, 0])]);
        let mut player = MidiPlayer::new(MidiSettings { skip_drums: true, ..Default::default() });
        player.install(parse_bytes(file(480, &[drums])).unwrap(), false, None);
        assert_eq!(player.status().notes_total, 1);
        player.seek(0.9);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(played(&player), 1);
    }

    /// The notes held where a seek lands are those reading from the start finds held there:
    /// cut anywhere (blocks, stretches packed side by side, checkpoints, held notes indexed
    /// every block). Without the index, as far as it looks back.
    #[test]
    fn seeking_finds_the_notes_held_there() {
        let bytes = random_tracks(0x51_7cc1_b727_220a, 24, 500);
        let all = merged(&bytes);
        let end = all.last().unwrap().0;
        for midi in [parse_bytes(bytes.clone()).unwrap(), parse_small(&bytes, 64)] {
            for (target, [indexed, looked_back]) in Playhead::held_both_ways(&midi.packed, (0..=end + 2).step_by(23)) {
                let mut held: Vec<Option<Event>> = Vec::new();
                for &e @ (_, track, msg) in all.iter().take_while(|e| e.0 < target) {
                    if is_note_on(msg) {
                        held.push(Some(e));
                    } else if (msg & 0xf0 == 0x80 || msg & 0xf0 == 0x90)
                        && let Some(h) = held.iter_mut().find(|h| h.is_some_and(|h| h.1 == track && (h.2 ^ msg) & 0xff0f == 0))
                    {
                        *h = None;
                    }
                }
                let expected: Vec<Event> = held.into_iter().flatten().collect();
                assert_eq!(indexed, expected, "at {target}");
                // these files are shorter than the look back
                assert_eq!(looked_back, expected, "at {target}, looked back");
            }
        }
    }

    /// A seek into a note holds its key, without striking it again; its note-off lets it up.
    /// A note-off ends the oldest held note of its channel, key and track.
    #[test]
    fn a_seek_into_a_note_holds_its_key() {
        let piano = track(&[
            (0, &[0x90, 60, 100]),
            (0, &[0x90, 62, 100]),
            (0, &[0x90, 62, 100]),
            (480, &[0x80, 62, 0]),
            (480, &[0x90, 64, 100]),
            (480, &[0x80, 64, 0]),
            (480, &[0x80, 60, 0]),
            (0, &[0x80, 62, 0]),
        ]);
        let bytes = file(480, &[piano]);
        let midi = parse_bytes(bytes.clone()).unwrap();
        // the second 62 is left held, from the start
        let held = |tick| Playhead::held_at(&midi.packed, tick).0.iter().map(|e| (e.0, e.2 >> 8 & 0xff)).collect::<Vec<_>>();
        assert_eq!(held(900), [(0, 60), (0, 62)]);
        assert_eq!(held(1200), [(0, 60), (0, 62), (960, 64)]);
        assert_eq!(held(1440), [(0, 60), (0, 62), (960, 64)], "a note-off at the place itself is still to come");
        assert_eq!(held(1921), []);
        // 1.25 s at 120 BPM: tick 1200
        let mut player = MidiPlayer::new(MidiSettings::default());
        player.install(parse_bytes(bytes).unwrap(), false, None);
        player.seek(1.25);
        std::thread::sleep(Duration::from_millis(100));
        assert!(player.key_down(60) && player.key_down(62) && player.key_down(64));
        assert_eq!(player.shared.presses[60].load(Ordering::Relaxed), 0, "not struck again");
        player.play();
        std::thread::sleep(Duration::from_millis(900));
        assert!(!player.key_down(60) && !player.key_down(62) && !player.key_down(64));
    }

    #[test]
    fn smpte_division_and_bad_files() {
        // 25 fps, 40 ticks per frame = 1000 ticks per second.
        let division = ((-25i8 as u8 as u16) << 8) | 40;
        let midi = parse_bytes(file(division, &[track(&[(1000, &[0x90, 60, 1])])])).unwrap();
        assert_eq!(midi.info.duration, 1.0);
        assert!(parse_bytes(b"RIFF....".to_vec()).is_err());
        assert!(parse_bytes(file(480, &[])).is_err());
    }

    /// `MIDI_BENCH=file.mid cargo test --release bench -- --ignored --nocapture`:
    /// load time, how small the events pack and how fast they unpack.
    #[test]
    #[ignore]
    fn bench_load_and_merge() {
        let path = std::env::var("MIDI_BENCH").expect("set MIDI_BENCH");
        let progress = LoadCounter::default();
        let midi = load(Path::new(&path), &progress).unwrap();
        let info = &midi.info;
        println!(
            "{}: {:.1} MB, {} tracks, {} notes, {} events, {} | index {:.0} ms, scan {:.0} ms, pack {:.0} ms | packed {:.1} MB ({:.3} B/event)",
            midi.name,
            info.file_bytes as f64 / 1e6,
            info.tracks,
            group_digits(info.note_on),
            group_digits(info.events),
            format_time(info.duration),
            info.index_ms,
            info.scan_ms,
            info.pack_ms,
            info.packed_bytes as f64 / 1e6,
            info.packed_bytes as f64 / info.events.max(1) as f64
        );
        let start = Instant::now();
        let mut playhead = Playhead::start(&midi.packed);
        let mut events = 0u64;
        let mut round = Vec::with_capacity(1 << 20);
        let mut due = 0;
        while playhead.peek_tick().is_some() {
            due += 64;
            round.clear();
            playhead.drain_until(due, &mut round);
            events += round.len() as u64;
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "unpacked {} events in {:.2} s ({:.1} M/s), {:.0}x realtime",
            group_digits(events),
            secs,
            events as f64 / secs / 1e6,
            info.duration / secs
        );
        let start = Instant::now();
        let (_, chase, _) = Playhead::seek(&midi.packed, midi.tempo.tick_at(info.duration / 2.0) as u64);
        println!(
            "seek to middle: {:.1} ms ({} controllers chased)",
            start.elapsed().as_secs_f64() * 1000.0,
            chase.len()
        );
        let start = Instant::now();
        let before = midi.packed.bytes();
        while !midi.packed.held_indexed() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let held: Vec<(f64, usize)> = (1..10)
            .map(|k| {
                let start = Instant::now();
                let (held, _) = Playhead::held_at(&midi.packed, midi.tempo.tick_at(info.duration * f64::from(k) / 10.0) as u64);
                (start.elapsed().as_secs_f64() * 1000.0, held.len())
            })
            .collect();
        println!(
            "held notes indexed {:.1} s after the unpacking above ({:.1} MB); held at 10-90 %: {:?}, found in {:.1} ms at most",
            start.elapsed().as_secs_f64(),
            (midi.packed.bytes() - before) as f64 / 1e6,
            held.iter().map(|h| h.1).collect::<Vec<_>>(),
            held.iter().map(|h| h.0).fold(0.0, f64::max)
        );
    }

    /// 256-key black MIDI: a note number with its top bit set stays above 127, is counted and
    /// held like any other, but isn't sent.
    #[test]
    fn notes_above_127() {
        let notes = track(&[(0, &[0x90, 200, 100]), (480, &[0x80, 200, 0]), (0, &[0x90, 60, 100]), (480, &[0x80, 60, 0])]);
        let midi = Arc::new(parse_bytes(file(480, &[notes])).unwrap());
        assert_eq!((midi.info.note_on, midi.info.high_notes), (2, 1));
        let mut player = MidiPlayer::new(MidiSettings { skip_drums: false, ..Default::default() });
        player.install(parse_bytes(file(480, &[track(&[(0, &[0x90, 200, 100]), (4800, &[0x80, 200, 0])])])).unwrap(), true, None);
        std::thread::sleep(Duration::from_millis(100));
        assert!(player.key_down(200));
        assert_eq!(player.status().notes_played, 1);
    }

    /// Time signatures and tempo: 4/4 for a bar, 3/4 for two, then 7/8 from the middle of a
    /// bar, which starts a new one.
    #[test]
    fn bars_beats_and_tempo() {
        let conductor = track(&[
            (0, &[0xff, 0x58, 0x04, 4, 2, 24, 8]),
            (0, &[0xff, 0x51, 0x03, 0x07, 0xa1, 0x20]),
            // 3/4 after one bar (4 beats of 480 ticks), at 60 BPM
            (1920, &[0xff, 0x58, 0x04, 3, 2, 24, 8]),
            (0, &[0xff, 0x51, 0x03, 0x0f, 0x42, 0x40]),
            // 7/8 after two bars of 3/4 and one more beat
            (3360, &[0xff, 0x58, 0x04, 7, 3, 24, 8]),
            (3360, &[0x90, 60, 1]),
        ]);
        let midi = parse_bytes(file(480, &[conductor])).unwrap();
        assert_eq!(midi.meter_at(0.0), Meter { beat: 0.0, bar: 0.0, num: 4, den: 4 });
        assert_eq!((midi.meter_at(5.0).num, midi.meter_at(5.0).den), (3, 4));
        assert!((midi.bar_at(4.0 + 1.5) - 1.5).abs() < 1e-9);
        // 7/8 starts bar 4 at beat 11 (bar 3 was cut short)
        assert_eq!(midi.meter_at(11.0), Meter { beat: 11.0, bar: 4.0, num: 7, den: 8 });
        assert!((midi.bpm_at(0.5) - 120.0).abs() < 1e-6);
        assert!((midi.bpm_at(3.0) - 60.0).abs() < 1e-6);
        // the file ends at beat 18 = 7 eighths (3.5 beats) twice after beat 11
        assert_eq!(midi.bars(), 6);
        let mut lines = Vec::new();
        assert!(midi.beat_lines(3.0, 12.0, 100, &mut lines));
        let bars: Vec<f64> = lines.iter().filter(|l| l.1).map(|l| l.0).collect();
        assert_eq!(bars, [4.0, 7.0, 10.0, 11.0]);
        assert_eq!(lines.iter().filter(|l| l.0 > 11.0).map(|l| l.0).collect::<Vec<_>>(), [11.5, 12.0]);
        assert!(!midi.beat_lines(0.0, 18.0, 5, &mut Vec::new()), "stops at the limit");
    }

    /// The player thread wakes now and then while paused; resuming carries on from where it
    /// stopped rather than from the start.
    #[test]
    fn resuming_carries_on_from_the_pause() {
        let piano = track(&[(0, &[0x90, 60, 100]), (48_000, &[0x80, 60, 0])]);
        let mut player = MidiPlayer::new(MidiSettings::default());
        player.install(parse_bytes(file(480, &[piano])).unwrap(), true, None);
        // However late a busy machine starts the player's thread.
        let until = Instant::now() + Duration::from_secs(5);
        while player.control().now() <= 0.15 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        player.pause();
        let paused = player.control().now();
        assert!(paused > 0.1, "{paused}");
        // past the thread's longest wait (50 ms)
        std::thread::sleep(Duration::from_millis(200));
        player.play();
        std::thread::sleep(Duration::from_millis(100));
        let now = player.control().now();
        assert!(now > paused && now < paused + 0.5, "paused at {paused}, now {now}");
    }

    /// What the player sends: the reset first, then the notes, each stamped with when it
    /// should sound, in time order and spaced as in the file.
    #[test]
    fn player_sends_timed_messages() {
        let piano = track(&[(480, &[0x90, 60, 100]), (240, &[0x80, 60, 0]), (0, &[0x90, 62, 90]), (240, &[0x80, 62, 0])]);
        let mut player = MidiPlayer::new(MidiSettings::default());
        let (out, mut rx) = MidiOut::new(1 << 12);
        player.set_output(Some(out));
        player.install(parse_bytes(file(480, &[piano])).unwrap(), true, None);
        // The notes end 1 s in; give a busy machine's late thread a while longer.
        let mut sent: Vec<Timed> = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while sent.iter().filter(|s| s.0.is_some()).count() < 4 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
            sent.extend(std::iter::from_fn(|| rx.pop().ok()));
        }
        let notes: Vec<(Instant, u32)> = sent.iter().filter_map(|&(at, m)| Some((at?, m))).collect();
        let msgs: Vec<u32> = notes.iter().map(|n| n.1).collect();
        assert_eq!(msgs, [msg(0x90, 60, 100), msg(0x80, 60, 0), msg(0x90, 62, 90), msg(0x80, 62, 0)]);
        // 240 ticks at 120 BPM and 480 PPQ: 0.25 s apart
        let gap = (notes[2].0 - notes[0].0).as_secs_f64();
        assert!((gap - 0.25).abs() < 1e-6, "{gap}");
        assert!(sent.iter().take_while(|s| s.0.is_none()).count() > 16, "the reset goes first, at once");
    }

    #[test]
    fn player_plays_seeks_and_releases() {
        let piano = track(&[
            (480, &[0x90, 60, 100]),
            (48, &[0x80, 60, 0]),
            (0, &[0x90, 62, 100]),
            (4800, &[0x80, 62, 0]),
        ]);
        let mut player = MidiPlayer::new(MidiSettings::default());
        player.install(parse_bytes(file(480, &[piano])).unwrap(), true, None);
        let down = |p: &MidiPlayer, k: usize| p.shared.down[k].load(Ordering::Relaxed);
        // The player's thread runs late on a busy machine, so wait for what it should do rather
        // than a fixed time.
        let settles = |what: &dyn Fn() -> bool| {
            let until = Instant::now() + Duration::from_secs(5);
            while !what() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            what()
        };
        assert!(settles(&|| down(&player, 62) == 1), "the second note plays");
        assert_eq!((down(&player, 60), down(&player, 62)), (0, 1));
        assert_eq!(player.shared.presses[60].load(Ordering::Relaxed), 1);

        player.update();
        assert!(player.key_down(62) && !player.key_down(60));

        player.pause();
        assert_eq!(down(&player, 62), 1, "pausing keeps the keys as they were");
        assert!(player.key_down(62));
        player.seek(0.0);
        assert!(settles(&|| down(&player, 62) == 0), "seeking before the note lets it go");
        player.seek(4.5);
        assert!(settles(&|| down(&player, 62) == 1), "seeking into the note holds it");
        assert!((player.control().now() - 4.5).abs() < 1e-9);
        player.stop();
        assert!(settles(&|| !player.is_playing() && down(&player, 62) == 0), "stopping lets everything go");
        assert_eq!(player.control().now(), 0.0);
        player.update();
        assert!(!player.key_down(62));
    }
}
