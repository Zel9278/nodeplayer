//! A file's channel messages as playback reads them, packed at load: every track merged in the
//! order they play (by tick, then by track, as reading the tracks side by side gave them), cut
//! into blocks, and each block kept column by column (the ticks, the tracks, the status bytes,
//! the two data bytes), each column compressed with zstd. Black MIDI repeats itself so much
//! that an event takes a fifth to a twentieth of a byte this way against three or four in the
//! file, so the whole of it stays in memory; and playing is unpacking blocks one after the
//! other, not reading thousands of tracks side by side, with a heap to merge them and a sort.
//!
//! Packing reads the tracks once more after the load's scan, split by ticks into stretches that
//! the CPUs pack side by side, each taking up its tracks from the scan's checkpoints.
//!
//! After the load, a thread of its own goes through the packed events once more and keeps the
//! notes held at the start of every so many blocks, packed (`index_held`): a seek then knows
//! the notes still sounding where it lands however long ago they were struck
//! (`Playhead::held_at`). Until it is done a seek looks back a stretch for them.

use super::{CHASE_SLOTS, NotesBefore, TrackCheckpoints, TrackReader, TrackSpan, chase_messages, chase_slot, is_note_on};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

/// An event as played: (tick, track, message).
pub(crate) type Event = (u64, usize, u32);

/// Events a seek looks back over at most for the notes still held where it lands, until the
/// held notes are indexed (`Playhead::held_at`): about 16 ms of unpacking.
const HELD_LOOKBACK: usize = 1 << 21;
/// Places the held notes are kept at at most (`HeldIndex`): a seek reads the blocks from the
/// last one before its place.
const MAX_HELD_POINTS: usize = 1 << 9;

/// The notes held before every `stride`-th block: point k is before block k × `stride`.
struct HeldIndex {
    stride: usize,
    points: Vec<HeldPoint>,
}

/// Notes held, in play order, packed (`pack_held`) and compressed, and their bytes unpacked.
struct HeldPoint {
    data: Box<[u8]>,
    bytes: u32,
}

/// Hashes a source of notes, (channel and key, track): a multiplication a word.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0 ^ u64::from(n)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// Notes struck in play order, and each source's held ones chained through them oldest first:
/// a note-off ends the oldest of its channel, key and track, as the piano roll pairs them. A
/// source with none held is let go. Black MIDI strikes hundreds of millions of notes, a few
/// hundred thousand held at once at most, nearly all let go soon after, while still in the
/// cache.
#[derive(Default)]
struct Chains {
    /// (tick, track, note-on or 0 once let go, the source's next held)
    notes: Vec<(u64, u32, u32, u32)>,
    gone: usize,
    /// (track, channel and key) -> its oldest and newest held
    sources: HashMap<u64, (u32, u32), BuildHasherDefault<KeyHasher>>,
}

impl Chains {
    const END: u32 = u32::MAX;

    #[inline]
    fn play(&mut self, (tick, track, msg): Event) {
        let source = (track as u64) << 16 | u64::from(msg & 0xff0f);
        if is_note_on(msg) {
            let at = self.notes.len() as u32;
            self.notes.push((tick, track as u32, msg, Self::END));
            match self.sources.entry(source) {
                Entry::Occupied(mut held) => {
                    self.notes[held.get().1 as usize].3 = at;
                    held.get_mut().1 = at;
                }
                Entry::Vacant(none) => {
                    none.insert((at, at));
                }
            }
        } else if is_note_off(msg)
            && let Entry::Occupied(mut held) = self.sources.entry(source)
        {
            let oldest = &mut self.notes[held.get().0 as usize];
            oldest.2 = 0;
            self.gone += 1;
            match oldest.3 {
                Self::END => {
                    held.remove();
                }
                next => held.get_mut().0 = next,
            }
        }
    }

    /// The notes held, in play order.
    fn held(&self) -> impl Iterator<Item = Event> {
        self.notes.iter().filter(|n| n.2 != 0).map(|&(tick, track, msg, _)| (tick, track as usize, msg))
    }

    /// Drop the notes let go once they are as many as those held (and some), moving the
    /// places down.
    fn tidy(&mut self) {
        if self.gone <= self.notes.len() / 2 + (1 << 20) {
            return;
        }
        let mut moved = vec![Self::END; self.notes.len()];
        let mut kept = 0;
        for (at, n) in self.notes.iter().enumerate() {
            if n.2 != 0 {
                moved[at] = kept;
                kept += 1;
            }
        }
        self.notes.retain(|n| n.2 != 0);
        for n in &mut self.notes {
            if n.3 != Self::END {
                n.3 = moved[n.3 as usize];
            }
        }
        for (oldest, newest) in self.sources.values_mut() {
            (*oldest, *newest) = (moved[*oldest as usize], moved[*newest as usize]);
        }
        self.gone = 0;
    }
}

fn is_note_off(msg: u32) -> bool {
    msg & 0xf0 == 0x80 || (msg & 0xf0 == 0x90 && msg >> 16 & 0x7f == 0)
}

/// Notes held, in play order, as bytes for zstd: the step in ticks from the one before and the
/// track as base-128 numbers, then the message's three bytes.
fn pack_held(notes: impl Iterator<Item = Event>, out: &mut Vec<u8>) {
    let mut last = 0;
    for (tick, track, msg) in notes {
        put_varint(out, tick - last);
        put_varint(out, track as u64);
        out.extend_from_slice(&msg.to_le_bytes()[..3]);
        last = tick;
    }
}

fn unpack_held(data: &[u8], out: &mut Vec<Event>) {
    let (mut pos, mut tick) = (0, 0);
    while pos < data.len() {
        tick += take_varint(data, &mut pos);
        let track = take_varint(data, &mut pos) as usize;
        out.push((tick, track, u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], 0])));
        pos += 3;
    }
}

/// Index the notes held through `packed` (`HeldIndex`) on a thread of its own: one pass through
/// it, given up once the file is closed (`Packed::close`).
pub(crate) fn index_held(packed: &Arc<Packed>) {
    let packed = packed.clone();
    let spawned = std::thread::Builder::new().name("midi-held".into()).spawn(move || {
        // behind playing (the thread it starts to unpack ahead takes the same)
        #[cfg(target_os = "linux")]
        // SAFETY: lowers this thread's priority, nothing more
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
        }
        if let Some(index) = HeldIndex::make(&packed, || packed.closed.load(Ordering::Relaxed)) {
            let _ = packed.held.set(index);
        }
    });
    if let Err(e) = spawned {
        eprintln!("Could not start indexing the held notes: {e}");
    }
}

impl HeldIndex {
    /// The notes held before every so many blocks, in a pass through all of them; None once
    /// `give_up`.
    fn make(packed: &Arc<Packed>, give_up: impl Fn() -> bool) -> Option<HeldIndex> {
        let stride = packed.blocks.len().div_ceil(MAX_HELD_POINTS).max(1);
        let mut zstd = zstd::bulk::Compressor::new(LEVEL).expect("a zstd compressor");
        let mut bytes = Vec::new();
        let mut chains = Chains::default();
        let mut points = Vec::with_capacity(packed.blocks.len().div_ceil(stride));
        // the blocks unpacked ahead on a thread of their own
        let mut playhead = Playhead::start(packed);
        for i in 0..packed.blocks.len() {
            if i % stride == 0 {
                if give_up() {
                    return None;
                }
                bytes.clear();
                pack_held(chains.held(), &mut bytes);
                let data = zstd.compress(&bytes).expect("zstd packs the held notes").into_boxed_slice();
                points.push(HeldPoint { data, bytes: bytes.len() as u32 });
            }
            for &e in &playhead.events {
                chains.play(e);
            }
            playhead.advance();
            chains.tidy();
        }
        Some(HeldIndex { stride, points })
    }
}

/// How packing cuts a file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sizes {
    /// Events a block.
    pub block_events: usize,
    /// Stretches of ticks packed side by side, per CPU (more than one, so a CPU that is done
    /// takes another while a dense one is still being packed).
    pub ranges_per_cpu: usize,
    /// Snapshots of the channels' set-up kept at most, a seek's start: it unpacks the blocks
    /// from the last one before its place.
    pub max_snapshots: usize,
}

pub(crate) const SIZES: Sizes = Sizes { block_events: 1 << 17, ranges_per_cpu: 2, max_snapshots: 1 << 10 };

/// zstd's level: 1 packs about as small as 3 here, and faster.
const LEVEL: i32 = 1;
/// Events a stretch of a range reads at about most before putting them in order of their
/// ticks: few enough for the stretch to stay in the CPU's cache while it is sorted and packed
/// (black MIDI's are memory bound, sixteen threads at once), and the ticks a stretch spans at
/// most.
const STRETCH_EVENTS: usize = 1 << 15;
const STRETCH_TICKS: u64 = 1 << 20;
const COLUMNS: usize = 5;

pub(crate) struct Packed {
    blocks: Vec<Block>,
    /// The notes held at the start of every so many blocks, once indexed (`index_held`), and
    /// whether the file is closed, which stops the indexing.
    held: OnceLock<HeldIndex>,
    closed: AtomicBool,
    /// The channels' set-up before some of the blocks, in block order; the first block has one.
    snapshots: Vec<Snapshot>,
    bytes: u64,
}

struct Block {
    first_tick: u64,
    count: u32,
    /// Note-ons before the block.
    notes: NotesBefore,
    /// Each column's packed length, and the tick and track columns' unpacked lengths (the
    /// others have a byte an event).
    packed: [u32; COLUMNS],
    unpacked: [u32; 2],
    data: Box<[u8]>,
}

struct Snapshot {
    block: usize,
    /// The chase slots set before the block, with their messages.
    slots: Vec<(u16, u32)>,
}

impl Packed {
    /// Bytes the packed events take, and the held notes once indexed.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes + self.held.get().map_or(0, |h| h.points.iter().map(|p| p.data.len() as u64).sum())
    }

    /// The file is closed: what is being made for it is not wanted any more.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Whether the held notes are indexed.
    #[cfg(test)]
    pub(crate) fn held_indexed(&self) -> bool {
        self.held.get().is_some()
    }

    /// Events packed.
    #[cfg(test)]
    pub(crate) fn events(&self) -> u64 {
        self.blocks.iter().map(|b| u64::from(b.count)).sum()
    }

    /// Bytes of the packed blocks (without the snapshots).
    #[cfg(test)]
    pub(crate) fn block_bytes(&self) -> u64 {
        self.blocks.iter().map(|b| b.data.len() as u64).sum()
    }
}

/// A count as a little-endian base-128 number.
#[inline(always)]
fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline(always)]
fn take_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

// ── Packing ────────────────────────────────────────────────────────────

/// Pack the channel messages of `tracks` (in `data`, `events` of them, as the scan counted),
/// counting the track bytes read in `progress`.
pub(crate) fn pack(data: &[u8], tracks: &[TrackSpan], checkpoints: &[TrackCheckpoints], events: u64, sizes: Sizes, progress: &AtomicU64) -> Packed {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let bounds = plan(tracks, checkpoints, cpus * sizes.ranges_per_cpu);
    let ranges: Vec<(u64, u64)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();
    let blocks = (events as usize).div_ceil(sizes.block_events) + ranges.len();
    let stride = blocks.div_ceil(sizes.max_snapshots).max(1);
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Option<(Vec<Block>, Vec<Snapshot>)>> = (0..ranges.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..cpus.min(ranges.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(from, to)) = ranges.get(i) else { break };
                        done.push((i, pack_range(data, tracks, checkpoints, from, to, stride, sizes, progress)));
                    }
                    done
                })
            })
            .collect();
        for handle in handles {
            for (i, part) in handle.join().expect("a packing thread") {
                parts[i] = Some(part);
            }
        }
    });
    let mut packed = Packed { blocks: Vec::new(), held: OnceLock::new(), closed: AtomicBool::new(false), snapshots: Vec::new(), bytes: 0 };
    for (blocks, snapshots) in parts.into_iter().map(|p| p.expect("every range packed")) {
        let base = packed.blocks.len();
        packed.snapshots.extend(snapshots.into_iter().map(|s| Snapshot { block: base + s.block, ..s }));
        packed.blocks.extend(blocks);
    }
    packed.bytes = packed.blocks.iter().map(|b| b.data.len() as u64).sum::<u64>()
        + packed.snapshots.iter().map(|s| s.slots.len() as u64 * 8).sum::<u64>();
    packed
}

/// Where to cut the file's ticks into about `parts` stretches holding as many events each, by
/// where the tracks' checkpoints lie (a stretch of bytes apart each): from 0 to `u64::MAX`.
fn plan(tracks: &[TrackSpan], checkpoints: &[TrackCheckpoints], parts: usize) -> Vec<u64> {
    let mut weights: Vec<(u64, usize)> = Vec::new();
    for (span, points) in tracks.iter().zip(checkpoints) {
        let mut from = span.offset;
        for p in &points.points {
            weights.push((p.reader.tick, p.reader.pos - from));
            from = p.reader.pos;
        }
    }
    weights.sort_unstable();
    let total: usize = weights.iter().map(|w| w.1).sum();
    let mut bounds = vec![0];
    let mut sum = 0;
    let mut part = 1;
    for &(tick, bytes) in &weights {
        sum += bytes;
        if part < parts && sum as u128 * parts as u128 >= total as u128 * part as u128 {
            if tick > *bounds.last().expect("0 first") {
                bounds.push(tick);
            }
            while part < parts && sum as u128 * parts as u128 >= total as u128 * part as u128 {
                part += 1;
            }
        }
    }
    bounds.push(u64::MAX);
    bounds
}

/// A track taken up at a tick (`seek_track`).
struct TakenUp {
    reader: TrackReader,
    /// The first channel message at the tick or later.
    next: Option<(u64, u32)>,
    /// The track's chase slots before it, as (slot, tick, message), and its note-ons.
    chase: Vec<(usize, u64, u32)>,
    notes: NotesBefore,
}

/// A track's reader taken up at its first channel message at `tick` or later, from the last
/// checkpoint before the place, as the scan found things there.
fn seek_track(data: &[u8], span: TrackSpan, checkpoints: Option<&TrackCheckpoints>, tick: u64) -> TakenUp {
    let mut reader = TrackReader::new(span);
    // the latest (tick, message) per slot, made on first use
    let mut slots: Vec<Option<(u64, u32)>> = Vec::new();
    let mut notes = NotesBefore::default();
    if let Some((point, values)) = checkpoints.and_then(|c| c.before(tick)) {
        reader = point.reader;
        notes = point.notes;
        if !values.is_empty() {
            slots = vec![None; CHASE_SLOTS];
            for &(slot, at, msg) in values {
                slots[usize::from(slot)] = Some((at, msg));
            }
        }
    }
    let next = loop {
        let Some((at, msg)) = reader.next_channel(data) else { break None };
        if at >= tick {
            break Some((at, msg));
        }
        if is_note_on(msg) {
            notes.all += 1;
            notes.drums += u64::from(msg & 0x0f == 9);
        }
        let Some(slot) = chase_slot(msg) else { continue };
        if slots.is_empty() {
            slots = vec![None; CHASE_SLOTS];
        }
        slots[slot] = Some((at, msg));
    };
    let chase = slots.into_iter().enumerate().filter_map(|(slot, v)| v.map(|(at, msg)| (slot, at, msg))).collect();
    TakenUp { reader, next, chase, notes }
}

/// Pack the events from tick `from` to before `to`: every track taken up at `from`, then read a
/// stretch of ticks at a time (as long as gives about `STRETCH_EVENTS`), track by track, the
/// stretch's events put in order of their ticks and packed.
#[allow(clippy::too_many_arguments)]
fn pack_range(data: &[u8], tracks: &[TrackSpan], checkpoints: &[TrackCheckpoints], from: u64, to: u64, stride: usize, sizes: Sizes, progress: &AtomicU64) -> (Vec<Block>, Vec<Snapshot>) {
    let mut writer = Writer::new(sizes.block_events, stride);
    let mut readers = Vec::with_capacity(tracks.len());
    let mut pending = Vec::with_capacity(tracks.len());
    // the set-up at `from`: the latest of each slot, the later track's on one tick
    let mut chase: Vec<Option<(u64, u32)>> = vec![None; CHASE_SLOTS];
    for (t, &span) in tracks.iter().enumerate() {
        let TakenUp { reader, next, chase: values, notes } = seek_track(data, span, checkpoints.get(t), from);
        writer.notes.all += notes.all;
        writer.notes.drums += notes.drums;
        for (slot, at, msg) in values {
            if chase[slot].is_none_or(|(old, _)| at >= old) {
                chase[slot] = Some((at, msg));
            }
        }
        readers.push(reader);
        pending.push(next);
    }
    writer.slots = chase.into_iter().map(|v| v.map(|(_, msg)| msg)).collect();

    let mut span = 64u64;
    let mut stretch: Vec<Stretched> = Vec::new();
    let mut sorter = Sorter::default();
    // every event before `first` is packed
    let mut first = from;
    loop {
            let end = first.saturating_add(span).min(to);
        if first >= end {
            break;
        }
        stretch.clear();
        let mut walked = 0;
        for t in 0..tracks.len() {
            let Some((mut tick, mut msg)) = pending[t].filter(|&(tick, _)| tick < end) else { continue };
            let reader = &mut readers[t];
            let before = reader.pos;
            loop {
                stretch.push(((tick - first) as u32, t as u32, msg));
                match reader.next_channel(data) {
                    Some((next, next_msg)) if next < end => (tick, msg) = (next, next_msg),
                    other => {
                        pending[t] = other;
                        break;
                    }
                }
            }
            walked += reader.pos - before;
        }
        progress.fetch_add(walked as u64, Ordering::Relaxed);
            sorter.sort(&mut stretch, (end - first) as usize);
            for &(offset, track, msg) in &stretch {
            writer.push((first + u64::from(offset), track as usize, msg));
        }
        if stretch.is_empty() {
            // nothing in the stretch: on to the next event, wherever it is
            let Some(next) = pending.iter().filter_map(|p| p.map(|(tick, _)| tick)).min() else { break };
            first = next;
            continue;
        }
        first = end;
        if stretch.len() < STRETCH_EVENTS / 2 {
            span = span.saturating_mul(2).min(STRETCH_TICKS);
        } else if stretch.len() > STRETCH_EVENTS * 2 {
            span = (span / 2).max(1);
        }
    }
    writer.finish()
}

/// An event of a stretch: (ticks since the stretch's start, track, message), small to keep the
/// stretch in the cache.
type Stretched = (u32, u32, u32);

/// Puts a stretch's events in order of their ticks by counting, keeping the order of those on
/// one tick.
#[derive(Default)]
struct Sorter {
    counts: Vec<u32>,
    sorted: Vec<Stretched>,
}

impl Sorter {
    fn sort(&mut self, events: &mut Vec<Stretched>, ticks: usize) {
        let counts = &mut self.counts;
        counts.clear();
        counts.resize(ticks + 1, 0);
        for e in events.iter() {
            counts[e.0 as usize + 1] += 1;
        }
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }
        self.sorted.clear();
        self.sorted.resize(events.len(), (0, 0, 0));
        for &e in events.iter() {
            let slot = &mut counts[e.0 as usize];
            self.sorted[*slot as usize] = e;
            *slot += 1;
        }
        std::mem::swap(events, &mut self.sorted);
    }
}

/// Packs events into blocks as they come, in play order.
struct Writer {
    zstd: zstd::bulk::Compressor<'static>,
    /// The block's columns so far.
    columns: [Vec<u8>; COLUMNS],
    count: usize,
    first_tick: u64,
    last: (u64, usize),
    block_notes: NotesBefore,
    /// Note-ons, and the channels' set-up by chase slot, before the next event.
    notes: NotesBefore,
    slots: Vec<Option<u32>>,
    block_events: usize,
    /// Blocks between two snapshots.
    stride: usize,
    blocks: Vec<Block>,
    snapshots: Vec<Snapshot>,
}

impl Writer {
    fn new(block_events: usize, stride: usize) -> Self {
        Self {
            zstd: zstd::bulk::Compressor::new(LEVEL).expect("a zstd compressor"),
            columns: Default::default(),
            count: 0,
            first_tick: 0,
            last: (0, 0),
            block_notes: NotesBefore::default(),
            notes: NotesBefore::default(),
            slots: vec![None; CHASE_SLOTS],
            block_events,
            stride,
            blocks: Vec::new(),
            snapshots: Vec::new(),
        }
    }

    #[inline]
    fn push(&mut self, (tick, track, msg): Event) {
        if self.count == 0 {
            self.first_tick = tick;
            self.last = (tick, track);
            self.block_notes = self.notes;
            if self.blocks.len().is_multiple_of(self.stride) {
                let slots = self.slots.iter().enumerate().filter_map(|(s, m)| m.map(|m| (s as u16, m))).collect();
                self.snapshots.push(Snapshot { block: self.blocks.len(), slots });
            }
        }
        let [ticks, tracks, status, data1, data2] = &mut self.columns;
        let delta = tick - self.last.0;
        put_varint(ticks, delta);
        // on one tick the tracks come in order: the step from the one before
        let same_tick = self.count > 0 && delta == 0;
        debug_assert!(!same_tick || track >= self.last.1);
        put_varint(tracks, if same_tick { track - self.last.1 } else { track } as u64);
        status.push(msg as u8);
        data1.push((msg >> 8) as u8);
        data2.push((msg >> 16) as u8);
        self.last = (tick, track);
        if is_note_on(msg) {
            self.notes.all += 1;
            self.notes.drums += u64::from(msg & 0x0f == 9);
        } else if let Some(slot) = chase_slot(msg) {
            self.slots[slot] = Some(msg);
        }
        self.count += 1;
        if self.count == self.block_events {
            self.end_block();
        }
    }

    fn end_block(&mut self) {
        if self.count == 0 {
            return;
        }
            let mut data = Vec::new();
        let mut packed = [0u32; COLUMNS];
        for (k, column) in self.columns.iter_mut().enumerate() {
            let bytes = self.zstd.compress(column).expect("zstd packs a block");
            packed[k] = bytes.len() as u32;
            data.extend_from_slice(&bytes);
        }
        let unpacked = [self.columns[0].len() as u32, self.columns[1].len() as u32];
        for column in &mut self.columns {
            column.clear();
        }
        self.blocks.push(Block {
            first_tick: self.first_tick,
            count: self.count as u32,
            notes: self.block_notes,
            packed,
            unpacked,
            data: data.into_boxed_slice(),
        });
        self.count = 0;
    }

    fn finish(mut self) -> (Vec<Block>, Vec<Snapshot>) {
        self.end_block();
        (self.blocks, self.snapshots)
    }
}

// ── Playing ────────────────────────────────────────────────────────────

/// Blocks a playhead has unpacked ahead of the one it plays, on a thread of its own.
const AHEAD: usize = 2;

/// Unpacks blocks, keeping its buffers from one to the next.
struct Unpacker {
    zstd: zstd::bulk::Decompressor<'static>,
    columns: [Vec<u8>; COLUMNS],
}

impl Unpacker {
    fn new() -> Self {
        Self { zstd: zstd::bulk::Decompressor::new().expect("a zstd decompressor"), columns: Default::default() }
    }

    /// Block `b`'s events, appended to `out`.
    fn unpack(&mut self, b: &Block, out: &mut Vec<Event>) {
        let n = b.count as usize;
        let mut at = 0;
        for (k, column) in self.columns.iter_mut().enumerate() {
            let len = b.packed[k] as usize;
            column.clear();
            column.reserve(if k < 2 { b.unpacked[k] as usize } else { n });
            self.zstd.decompress_to_buffer(&b.data[at..at + len], column).expect("a block packed here");
            at += len;
        }
        let [ticks, tracks, status, data1, data2] = &self.columns;
        let (mut tick, mut track) = (b.first_tick, 0usize);
        let (mut tp, mut rp) = (0, 0);
        out.reserve(n);
        for i in 0..n {
            let delta = take_varint(ticks, &mut tp);
            let step = take_varint(tracks, &mut rp) as usize;
            track = if i > 0 && delta == 0 { track + step } else { step };
            tick += delta;
            out.push((tick, track, u32::from(status[i]) | u32::from(data1[i]) << 8 | u32::from(data2[i]) << 16));
        }
    }
}

/// A thread unpacking the blocks a playhead comes to next, in order, `AHEAD` of them at a time:
/// unpacking costs as much as all the rest of playing does, which then has the player's thread
/// to itself. Ends when the playhead goes.
struct Ahead {
    requests: mpsc::Sender<(usize, Vec<Event>)>,
    unpacked: mpsc::Receiver<Vec<Event>>,
    /// The next block asked for.
    next: usize,
}

impl Ahead {
    /// Unpacking from block `first` on, or None when no thread can be had.
    fn start(packed: &Arc<Packed>, first: usize) -> Option<Self> {
        let (requests, todo) = mpsc::channel::<(usize, Vec<Event>)>();
        let (done, unpacked) = mpsc::channel();
        let shared = packed.clone();
        std::thread::Builder::new()
            .name("midi-unpack".into())
            .spawn(move || {
                let mut unpacker = Unpacker::new();
                for (i, mut events) in todo {
                    events.clear();
                    unpacker.unpack(&shared.blocks[i], &mut events);
                    if done.send(events).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        let mut ahead = Self { requests, unpacked, next: first };
        for _ in 0..AHEAD {
            ahead.ask(packed.blocks.len(), Vec::new());
        }
        Some(ahead)
    }

    /// Ask for the next block, unpacked into `events`, if there is one before `blocks`.
    fn ask(&mut self, blocks: usize, events: Vec<Event>) {
        if self.next < blocks && self.requests.send((self.next, events)).is_ok() {
            self.next += 1;
        }
    }
}

/// How a playhead unpacks: on its thread until it first plays, then ahead on another.
enum Unpacking {
    NotYet,
    Ahead(Ahead),
    /// No thread to be had.
    Here,
}

/// Where playback is in the packed events: the block being played, unpacked, and how far into it.
pub(crate) struct Playhead {
    packed: Arc<Packed>,
    unpacker: Unpacker,
    unpacking: Unpacking,
    /// The next block to play, the one unpacked and the next of its events.
    next_block: usize,
    events: Vec<Event>,
    at: usize,
    /// Packed bytes the seek that made it unpacked, and those unpacked since.
    pub scanned: u64,
    pub walked: u64,
}

impl Playhead {
    fn new(packed: &Arc<Packed>) -> Self {
        Self {
            packed: packed.clone(),
            unpacker: Unpacker::new(),
            unpacking: Unpacking::NotYet,
            next_block: 0,
            events: Vec::new(),
            at: 0,
            scanned: 0,
            walked: 0,
        }
    }

    /// Before the first event.
    pub(crate) fn start(packed: &Arc<Packed>) -> Self {
        Self::seek(packed, 0).0
    }

    /// Before the first event at `tick` or later. Also returns the controller, bend and program
    /// messages in effect there, and the note-ons before it.
    pub(crate) fn seek(packed: &Arc<Packed>, tick: u64) -> (Self, Vec<u32>, NotesBefore) {
        let mut p = Self::new(packed);
        // the target lies in the last block that starts before it, or at the next one's start
        let Some(block) = packed.blocks.partition_point(|b| b.first_tick < tick).checked_sub(1) else {
            p.unpack_here();
            p.scanned = std::mem::take(&mut p.walked);
            return (p, Vec::new(), NotesBefore::default());
        };
        let snapshot = &packed.snapshots[packed.snapshots.partition_point(|s| s.block <= block) - 1];
        let mut slots: Vec<Option<u32>> = vec![None; CHASE_SLOTS];
        for &(slot, msg) in &snapshot.slots {
            slots[usize::from(slot)] = Some(msg);
        }
        let mut notes = packed.blocks[block].notes;
        p.next_block = snapshot.block;
        loop {
            p.unpack_here();
            let last = p.next_block == block + 1;
            let before = if last { p.events.partition_point(|e| e.0 < tick) } else { p.events.len() };
            for &(_, _, msg) in &p.events[..before] {
                if is_note_on(msg) {
                    if last {
                        notes.all += 1;
                        notes.drums += u64::from(msg & 0x0f == 9);
                    }
                } else if let Some(slot) = chase_slot(msg) {
                    slots[slot] = Some(msg);
                }
            }
            if last {
                p.at = before;
                break;
            }
        }
        if p.at == p.events.len() {
            p.unpack_here();
        }
        p.scanned = std::mem::take(&mut p.walked);
        (p, chase_messages(slots), notes)
    }

    /// The note-ons still held at `tick`: struck before it, their note-offs at it or later, a
    /// note-off ending the oldest held note of its channel, key and track (as the piano roll
    /// pairs them). In play order, with the packed bytes unpacked to find them: the notes held
    /// at the last indexed place before it (`HeldIndex`), and the blocks from there read up to
    /// it. Not indexed yet, the blocks of `HELD_LOOKBACK` events before it: a note struck
    /// earlier is not found.
    pub(crate) fn held_at(packed: &Packed, tick: u64) -> (Vec<Event>, u64) {
        Self::held_from(packed, tick, packed.held.get())
    }

    fn held_from(packed: &Packed, tick: u64, index: Option<&HeldIndex>) -> (Vec<Event>, u64) {
        let Some(last) = packed.blocks.partition_point(|b| b.first_tick < tick).checked_sub(1) else { return (Vec::new(), 0) };
        let mut notes = Vec::new();
        let (first, mut bytes) = match index {
            Some(index) => {
                let point = &index.points[last / index.stride];
                let raw = zstd::bulk::decompress(&point.data, point.bytes as usize).expect("held notes unpack");
                unpack_held(&raw, &mut notes);
                (last / index.stride * index.stride, point.data.len() as u64)
            }
            None => {
                let mut first = last;
                let mut events = packed.blocks[last].count as usize;
                while first > 0 && events < HELD_LOOKBACK {
                    first -= 1;
                    events += packed.blocks[first].count as usize;
                }
                (first, 0)
            }
        };
        let mut chains = Chains::default();
        for e in notes {
            chains.play(e);
        }
        let (mut unpacker, mut events) = (Unpacker::new(), Vec::new());
        for b in &packed.blocks[first..=last] {
            events.clear();
            unpacker.unpack(b, &mut events);
            bytes += b.data.len() as u64;
            for &e in events.iter().take_while(|e| e.0 < tick) {
                chains.play(e);
            }
            chains.tidy();
        }
        (chains.held().collect(), bytes)
    }

    /// `held_at` at each of `ticks`, with the held notes indexed (made here) and not.
    #[cfg(test)]
    pub(crate) fn held_both_ways(packed: &Arc<Packed>, ticks: impl Iterator<Item = u64>) -> Vec<(u64, [Vec<Event>; 2])> {
        let index = HeldIndex::make(packed, || false);
        ticks.map(|t| (t, [Self::held_from(packed, t, index.as_ref()).0, Self::held_from(packed, t, None).0])).collect()
    }

    pub(crate) fn peek_tick(&self) -> Option<u64> {
        self.events.get(self.at).map(|e| e.0)
    }

    /// Every event at `due` (a tick) or before, appended to `out` in play order.
    pub(crate) fn drain_until(&mut self, due: u64, out: &mut Vec<Event>) {
        loop {
            let rest = &self.events[self.at..];
            let n = rest.partition_point(|e| e.0 <= due);
            out.extend_from_slice(&rest[..n]);
            self.at += n;
            if self.at < self.events.len() || !self.advance() {
                return;
            }
        }
    }

    /// The next event: what `drain_until` gives a round at a time.
    #[cfg(test)]
    pub(crate) fn pop(&mut self) -> Option<Event> {
        let e = *self.events.get(self.at)?;
        self.at += 1;
        if self.at == self.events.len() {
            self.advance();
        }
        Some(e)
    }

    /// Unpack the next block in place of the one played, on this thread (seeking); false at
    /// the end.
    fn unpack_here(&mut self) -> bool {
        self.events.clear();
        self.at = 0;
        let Some(b) = self.packed.blocks.get(self.next_block) else { return false };
        self.next_block += 1;
        self.unpacker.unpack(b, &mut self.events);
        self.walked += b.data.len() as u64;
        true
    }

    /// The next block in place of the one played, unpacked ahead (playing); false at the end.
    fn advance(&mut self) -> bool {
        let blocks = self.packed.blocks.len();
        if matches!(self.unpacking, Unpacking::NotYet) && self.next_block < blocks {
            self.unpacking = Ahead::start(&self.packed, self.next_block).map_or(Unpacking::Here, Unpacking::Ahead);
        }
        let Unpacking::Ahead(ahead) = &mut self.unpacking else { return self.unpack_here() };
        let Some(b) = self.packed.blocks.get(self.next_block) else {
            self.events.clear();
            self.at = 0;
            return false;
        };
        let Ok(events) = ahead.unpacked.recv() else {
            self.unpacking = Unpacking::Here;
            return self.unpack_here();
        };
        let played = std::mem::replace(&mut self.events, events);
        ahead.ask(blocks, played);
        self.at = 0;
        self.next_block += 1;
        self.walked += b.data.len() as u64;
        true
    }
}
