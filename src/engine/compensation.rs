//! Prepared delay storage. Constructors run on the control thread; processing and history
//! transfer use bounded storage without locks or allocation.

use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::timing::{EdgeKey, EdgeKind, TimingAnalysis, TimingLimits};
use super::{MidiBuf, MidiEvent, StereoBuf};

pub(super) struct StorageReservation<'a> {
    budget: &'a AtomicUsize,
    bytes: usize,
}

impl<'a> StorageReservation<'a> {
    pub(super) fn reserve_in(budget: &'a AtomicUsize, limit: usize, bytes: usize) -> Result<Self> {
        budget.try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|&next| next <= limit)
        }).map_err(|_| anyhow::anyhow!("prepared plugin bypass memory budget exceeded ({bytes} bytes requested, {limit} byte limit)"))?;
        Ok(Self { budget, bytes })
    }
}

impl Drop for StorageReservation<'_> {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

const AUDIO_PAGE_FRAMES: usize = 1024;

struct AudioPage {
    frames: [[f32; 2]; AUDIO_PAGE_FRAMES],
    // Each allocation keeps its own budget token while moving between delay rings.
    _reservation: Option<StorageReservation<'static>>,
}

pub struct AudioDelay {
    delay: usize,
    pages: Vec<Box<AudioPage>>,
    cursor: u64,
    valid: usize,
}

impl AudioDelay {
    // Storage capacity, never additional signal delay. Zero-delay routes also retain real
    // input, so ordinary latency increases can select an already warm history tap.
    pub const HISTORY_RESERVE: usize = 8192;

    pub fn allocation_frames(frames: u64) -> Result<usize> {
        ensure!(
            frames <= TimingLimits::default().max_latency_frames,
            "audio delay exceeds frame limit"
        );
        Ok(usize::try_from(frames)?
            .max(Self::HISTORY_RESERVE)
            .next_power_of_two())
    }

    pub fn allocation_bytes(frames: u64) -> Result<usize> {
        Ok(Self::allocation_frames(frames)? / AUDIO_PAGE_FRAMES
            * (size_of::<AudioPage>() + size_of::<Box<AudioPage>>()))
    }

    pub fn new(frames: u64) -> Result<Self> {
        Self::prepare(frames, frames, None)
    }

    pub(super) fn with_budget(
        frames: u64,
        capacity: u64,
        budget: &'static AtomicUsize,
        limit: usize,
    ) -> Result<Self> {
        let reservation =
            StorageReservation::reserve_in(budget, limit, Self::allocation_bytes(capacity)?)?;
        Self::prepare(frames, capacity, Some(reservation))
    }

    fn prepare(
        frames: u64,
        capacity: u64,
        mut reservation: Option<StorageReservation<'static>>,
    ) -> Result<Self> {
        ensure!(
            frames <= capacity,
            "audio delay exceeds prepared history capacity"
        );
        let count = Self::allocation_frames(capacity)? / AUDIO_PAGE_FRAMES;
        let mut pages = Vec::with_capacity(count);
        for _ in 0..count {
            let token = reservation.as_mut().map(|total| {
                let bytes = size_of::<AudioPage>() + size_of::<Box<AudioPage>>();
                total.bytes -= bytes;
                StorageReservation {
                    budget: total.budget,
                    bytes,
                }
            });
            pages.push(Box::new(AudioPage {
                frames: [[0.0; 2]; AUDIO_PAGE_FRAMES],
                _reservation: token,
            }));
        }
        Ok(Self {
            delay: usize::try_from(frames)?,
            pages,
            cursor: 0,
            valid: 0,
        })
    }

    #[cfg(test)]
    pub fn frames(&self) -> usize {
        self.delay
    }

    fn allocated_bytes(&self) -> usize {
        self.pages.len() * (size_of::<AudioPage>() + size_of::<Box<AudioPage>>())
    }

    fn capacity(&self) -> usize {
        self.pages.len() * AUDIO_PAGE_FRAMES
    }

    /// Invalidate without zeroing a potentially long allocation on the audio thread.
    pub fn invalidate(&mut self) {
        self.valid = 0;
    }

    /// Move prepared pages, never copy an arbitrary-length audio history. Ordinary edits
    /// reuse the entire ring in O(1). Capacity growth swaps at most 4096 page pointers (the
    /// explicit latency limit) and copies at most one fixed 1024-frame boundary page. All
    /// unused allocations stay in `previous` for control-thread retirement.
    pub fn inherit(&mut self, previous: &mut Self) {
        let retained = previous.valid.min(self.capacity());
        if previous.capacity() == self.capacity() {
            std::mem::swap(&mut self.pages, &mut previous.pages);
        } else if retained != 0 {
            let first_sample = previous.cursor - retained as u64;
            let mut first_page = first_sample / AUDIO_PAGE_FRAMES as u64;
            let last_page = (previous.cursor - 1) / AUDIO_PAGE_FRAMES as u64;
            let old_mask = previous.pages.len() - 1;
            let new_mask = self.pages.len() - 1;
            // A full circular ring's first and last partial pages share one allocation.
            // Growth splits the old boundary, while shrink joins the new one. All other
            // pages move by pointer swap. The owned capacity stays exactly as prepared.
            let split = first_page != last_page
                && first_page as usize & old_mask == last_page as usize & old_mask;
            let join = first_page != last_page
                && first_page as usize & new_mask == last_page as usize & new_mask;
            let boundary = (
                first_page as usize & old_mask,
                first_sample as usize & (AUDIO_PAGE_FRAMES - 1),
            );
            if split {
                let offset = first_sample as usize & (AUDIO_PAGE_FRAMES - 1);
                self.pages[first_page as usize & new_mask].frames[offset..].copy_from_slice(
                    &previous.pages[first_page as usize & old_mask].frames[offset..],
                );
            }
            if split || join {
                first_page += 1;
            }
            for page in first_page..=last_page {
                std::mem::swap(
                    &mut self.pages[page as usize & new_mask],
                    &mut previous.pages[page as usize & old_mask],
                );
            }
            if join {
                self.pages[last_page as usize & new_mask].frames[boundary.1..]
                    .copy_from_slice(&previous.pages[boundary.0].frames[boundary.1..]);
            }
        }
        self.cursor = previous.cursor;
        self.valid = retained;
        previous.valid = 0;
    }

    fn next(&mut self, sample: [f32; 2]) -> [f32; 2] {
        let mask = self.capacity() - 1;
        let read = self.cursor.wrapping_sub(self.delay as u64) as usize & mask;
        let delayed = if self.delay == 0 {
            sample
        } else if self.delay <= self.valid {
            self.pages[read / AUDIO_PAGE_FRAMES].frames[read & (AUDIO_PAGE_FRAMES - 1)]
        } else {
            [0.0; 2]
        };
        let write = self.cursor as usize & mask;
        self.pages[write / AUDIO_PAGE_FRAMES].frames[write & (AUDIO_PAGE_FRAMES - 1)] = sample;
        self.cursor = self.cursor.wrapping_add(1);
        self.valid = (self.valid + 1).min(self.capacity());
        delayed
    }

    /// A missing source feeds silence but still advances history.
    pub fn add(&mut self, source: Option<&StereoBuf>, target: &mut StereoBuf, n: usize) {
        for i in 0..n {
            let input = source.map(|b| [b.l[i], b.r[i]]).unwrap_or([0.0; 2]);
            let value = self.next(input);
            target.l[i] += value[0];
            target.r[i] += value[1];
        }
    }

    pub fn write(&mut self, source: Option<&StereoBuf>, target: &mut StereoBuf, n: usize) {
        target.clear(n);
        self.add(source, target, n);
    }

    pub fn mix_device(&mut self, source: Option<&StereoBuf>, out: &mut [f32], channels: usize) {
        for (i, frame) in out.chunks_exact_mut(channels).enumerate() {
            let input = source.map(|b| [b.l[i], b.r[i]]).unwrap_or([0.0; 2]);
            let value = self.next(input);
            frame[0] += value[0];
            if channels > 1 {
                frame[1] += value[1];
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimedMidi {
    pub sample: u64,
    pub data: [u8; 3],
}

pub struct MidiDelay {
    delay: u64,
    cursor: u64,
    future: Vec<TimedMidi>,
    capacity: usize,
    cleanup: bool,
    // Per-channel note bitmaps are exact and fixed-size (2048 bits), so tracking cannot
    // overflow; cleanup is scoped to channels with notes that passed this edge.
    live_notes: [u128; 16],
    pub overflow_count: u64,
}

impl MidiDelay {
    pub fn new(delay: u64, capacity: usize) -> Result<Self> {
        ensure!(
            delay <= TimingLimits::default().max_latency_frames,
            "MIDI delay exceeds frame limit"
        );
        ensure!(
            capacity > 0 && capacity <= MidiBuf::CAPACITY,
            "invalid MIDI delay capacity"
        );
        Ok(Self {
            delay,
            cursor: 0,
            future: Vec::with_capacity(capacity),
            capacity,
            cleanup: false,
            live_notes: [0; 16],
            overflow_count: 0,
        })
    }

    pub fn invalidate(&mut self) {
        self.future.clear();
        self.cleanup = true;
    }

    /// Keep pending events on the new timing reference without allocating. If shortening
    /// the delay would move an event into already rendered time, cleanup replaces that
    /// stale queue; note-offs must never silently disappear or replay late.
    pub fn inherit(&mut self, previous: &mut Self) {
        self.cursor = previous.cursor;
        self.live_notes = previous.live_notes;
        self.cleanup = previous.cleanup;
        self.overflow_count = previous.overflow_count;
        if self.capacity != previous.capacity {
            self.cleanup = true;
            return;
        }
        std::mem::swap(&mut self.future, &mut previous.future);
        for event in &mut self.future {
            match event
                .sample
                .checked_sub(previous.delay)
                .and_then(|sample| sample.checked_add(self.delay))
            {
                Some(sample) if sample >= self.cursor => event.sample = sample,
                _ => self.cleanup = true,
            }
        }
        if self.cleanup {
            self.future.clear();
        }
    }

    fn overflow(&mut self) {
        self.overflow_count = self.overflow_count.saturating_add(1);
        self.cleanup = true;
    }

    fn track(&mut self, data: [u8; 3]) {
        let status = data[0];
        if !(0x80..=0x9f).contains(&status) || data[1] >= 128 {
            return;
        }
        let channel = (status & 0x0f) as usize;
        let note = data[1] as u32;
        let bit = 1u128 << (note % 128);
        match status & 0xf0 {
            0x90 if data[2] != 0 => self.live_notes[channel] |= bit,
            0x80 | 0x90 => self.live_notes[channel] &= !bit,
            _ => {}
        }
    }

    fn enqueue_cleanup(&mut self, output: &mut MidiBuf) -> bool {
        self.future.clear();
        let mut complete = true;
        for (channel, notes) in self.live_notes.iter().enumerate() {
            if *notes != 0 {
                for cc in [64, 123, 120] {
                    complete &= output.push_sorted(MidiEvent {
                        time: 0,
                        data: [0xb0 | channel as u8, cc, 0],
                    });
                }
            }
        }
        complete
    }

    fn deliver_due(&mut self, output: &mut MidiBuf, first_sample: u64, end: u64) {
        let due = self.future.partition_point(|event| event.sample < end);
        for i in 0..due {
            let event = self.future[i];
            if event.sample < first_sample
                || !output.push_sorted(MidiEvent {
                    time: (event.sample - first_sample) as u32,
                    data: event.data,
                })
            {
                self.overflow();
            } else {
                self.track(event.data);
            }
        }
        self.future.drain(..due);
    }

    /// Absolute timestamps survive arbitrary callback partitioning. Out-of-block events are
    /// retained, not clamped to this block's last sample. Equal-time arrival order is stable.
    pub fn process(
        &mut self,
        input: &[MidiEvent],
        output: &mut MidiBuf,
        first_sample: u64,
        n: usize,
    ) {
        let Some(end) = first_sample.checked_add(n as u64) else {
            self.overflow();
            return;
        };
        self.cursor = end;
        if self.cleanup && n > 0 {
            // Losing note-off or sustain is unsafe: discard stale events and send bounded
            // channel-wide sustain-off, all-notes-off, all-sound-off before accepting notes.
            let complete = self.enqueue_cleanup(output);
            if complete {
                self.live_notes = [0; 16];
            }
            self.cleanup = !complete;
            if !complete {
                return;
            }
        }
        if self.delay == 0 {
            self.deliver_due(output, first_sample, end);
            for event in input {
                if output.push_sorted(*event) {
                    self.track(event.data);
                } else {
                    self.overflow();
                }
            }
            return;
        }
        for event in input {
            let sample = first_sample
                .checked_add(event.time as u64)
                .and_then(|t| t.checked_add(self.delay));
            let Some(sample) = sample else {
                self.overflow();
                continue;
            };
            if self.future.len() == self.capacity {
                self.overflow();
                continue;
            }
            let at = self.future.partition_point(|event| event.sample <= sample);
            self.future.insert(
                at,
                TimedMidi {
                    sample,
                    data: event.data,
                },
            );
        }
        self.deliver_due(output, first_sample, end);
    }
}

pub enum EdgeDelay {
    Audio(AudioDelay),
    Midi(Box<MidiDelay>),
}

pub struct PreparedEdge {
    pub key: EdgeKey,
    pub delay: EdgeDelay,
    reusable: bool,
}

pub struct PreparedSink {
    pub node: u32,
    pub delay: AudioDelay,
    reusable: bool,
}

/// Per-step route indices mirror the legacy schedule. Runtime never traverses the graph or
/// calculates timing. The routes and their buffers move together in one SetSchedule epoch.
#[derive(Default)]
pub struct StepRoutes {
    pub node: u32,
    pub audio: Vec<Vec<usize>>,
    pub midi: Vec<usize>,
    /// A retired/replaced route may have delivered a note-on but retained its note-off.
    /// Emit cleanup at the destination even if it no longer has any MIDI routes.
    pub midi_cleanup: bool,
}

pub struct TimingPlan {
    pub epoch: u64,
    pub total_latency_frames: u64,
    pub allocated_delay_bytes: usize,
    pub edges: Vec<PreparedEdge>,
    pub sinks: Vec<PreparedSink>,
    pub midi_outputs: Vec<(u32, MidiDelay, MidiBuf)>,
    pub steps: Vec<StepRoutes>,
}

impl TimingPlan {
    pub fn prepare(
        analysis: &TimingAnalysis,
        limits: TimingLimits,
        steps: &[super::Step],
    ) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static EPOCH: AtomicU64 = AtomicU64::new(1);
        let epoch = EPOCH
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("plan epoch exhausted"))?;
        // Timing analysis charges the requested signal delay. Charge the continuously
        // warmed reserve as well, before any route allocation takes place.
        let mut allocated_delay_bytes = analysis.delay_bytes;
        for delay in analysis
            .edges
            .iter()
            .filter_map(|edge| {
                matches!(edge.key.kind, EdgeKind::Audio { .. }).then_some(edge.delay_frames)
            })
            .chain(analysis.sinks.iter().map(|&(_, delay)| delay))
        {
            let reserve = AudioDelay::allocation_bytes(delay)?
                - usize::try_from(delay)? * size_of::<[f32; 2]>();
            allocated_delay_bytes = allocated_delay_bytes
                .checked_add(reserve)
                .ok_or_else(|| anyhow::anyhow!("audio history allocation size overflow"))?;
        }
        for _ in analysis
            .midi_outputs
            .iter()
            .filter(|(_, delay)| *delay == 0)
        {
            allocated_delay_bytes = allocated_delay_bytes
                .checked_add(limits.midi_capacity * size_of::<TimedMidi>())
                .ok_or_else(|| anyhow::anyhow!("MIDI history allocation size overflow"))?;
        }
        ensure!(
            allocated_delay_bytes <= limits.max_delay_bytes,
            "PDC delay memory budget exceeded ({} > {} bytes)",
            allocated_delay_bytes,
            limits.max_delay_bytes
        );
        let mut edges = Vec::with_capacity(analysis.edges.len());
        for edge in &analysis.edges {
            let delay = match edge.key.kind {
                EdgeKind::Audio { .. } => EdgeDelay::Audio(AudioDelay::new(edge.delay_frames)?),
                EdgeKind::Midi => EdgeDelay::Midi(Box::new(MidiDelay::new(
                    edge.delay_frames,
                    limits.midi_capacity,
                )?)),
            };
            edges.push(PreparedEdge {
                key: edge.key,
                delay,
                reusable: true,
            });
        }
        let mut routes = Vec::with_capacity(steps.len());
        for step in steps {
            let mut route = StepRoutes {
                node: step.node,
                audio: vec![Vec::new(); step.inputs.len()],
                midi: Vec::new(),
                midi_cleanup: false,
            };
            for (i, edge) in edges
                .iter()
                .enumerate()
                .filter(|(_, edge)| edge.key.to == step.node)
            {
                match edge.key.kind {
                    EdgeKind::Audio { input, .. } => route.audio[input].push(i),
                    EdgeKind::Midi => route.midi.push(i),
                }
            }
            routes.push(route);
        }
        let midi_outputs = analysis
            .midi_outputs
            .iter()
            .map(|&(node, delay)| {
                Ok((
                    node,
                    MidiDelay::new(delay, limits.midi_capacity)?,
                    MidiBuf::new(),
                ))
            })
            .collect::<Result<_>>()?;
        let sinks = analysis
            .sinks
            .iter()
            .map(|&(node, delay)| {
                Ok(PreparedSink {
                    node,
                    delay: AudioDelay::new(delay)?,
                    reusable: true,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            epoch,
            total_latency_frames: analysis.total_latency_frames,
            allocated_delay_bytes,
            edges,
            sinks,
            midi_outputs,
            steps: routes,
        })
    }

    /// Swap unchanged history, leaving the unused fresh allocation in the retired plan for
    /// control-thread reclamation. O(E log E), no allocation, no copying/zeroing long delays.
    pub fn inherit(&mut self, previous: &mut Self) {
        // Preserve pending cleanup through multiple schedule changes in one callback.
        for step in &mut self.steps {
            step.midi_cleanup |= previous
                .steps
                .iter()
                .any(|old| old.node == step.node && old.midi_cleanup);
        }
        for old in &previous.edges {
            let EdgeDelay::Midi(old_delay) = &old.delay else {
                continue;
            };
            let reusable = old.reusable && self.edges.binary_search_by_key(&old.key, |edge| edge.key).ok().is_some_and(|index| {
                matches!(&self.edges[index].delay, EdgeDelay::Midi(new) if new.capacity == old_delay.capacity)
            });
            if !reusable
                && let Some(step) = self.steps.iter_mut().find(|step| step.node == old.key.to)
            {
                step.midi_cleanup = true;
            }
        }
        for edge in &mut self.edges {
            if let Ok(index) = previous.edges.binary_search_by_key(&edge.key, |e| e.key) {
                let old = &mut previous.edges[index];
                if !old.reusable {
                    continue;
                }
                match (&mut edge.delay, &mut old.delay) {
                    (EdgeDelay::Audio(new), EdgeDelay::Audio(old)) => new.inherit(old),
                    (EdgeDelay::Midi(new), EdgeDelay::Midi(old)) => {
                        new.inherit(old);
                        if new.cleanup
                            && let Some(step) =
                                self.steps.iter_mut().find(|step| step.node == edge.key.to)
                        {
                            // Route timing can discard a queued note-off. The destination
                            // receives the same bounded panic as a retired route before
                            // any other route supplies first-sample notes.
                            step.midi_cleanup = true;
                            new.cleanup = false;
                            new.live_notes = [0; 16];
                        }
                    }
                    _ => {}
                }
            }
        }
        for sink in &mut self.sinks {
            if let Some(old) = previous
                .sinks
                .iter_mut()
                .find(|old| old.node == sink.node && old.reusable)
            {
                sink.delay.inherit(&mut old.delay);
            }
        }
        for (node, delay, _) in &mut self.midi_outputs {
            if let Some((_, old, _)) = previous
                .midi_outputs
                .iter_mut()
                .find(|(old_node, _, _)| old_node == node)
            {
                delay.inherit(old);
            }
        }
        // Report full prepared page storage, including the warm reserve and metadata.
        self.allocated_delay_bytes = self
            .edges
            .iter()
            .map(|edge| match &edge.delay {
                EdgeDelay::Audio(delay) => delay.allocated_bytes(),
                EdgeDelay::Midi(delay) => delay.capacity * size_of::<TimedMidi>(),
            })
            .sum::<usize>()
            + self
                .sinks
                .iter()
                .map(|sink| sink.delay.allocated_bytes())
                .sum::<usize>()
            + self
                .midi_outputs
                .iter()
                .map(|(_, delay, _)| delay.capacity * size_of::<TimedMidi>())
                .sum::<usize>();
    }

    /// A recycled slot must never inherit the previous instance's buffered events/audio.
    pub fn invalidate_node(&mut self, node: u32) {
        for edge in &mut self.edges {
            if edge.key.from == node || edge.key.to == node {
                edge.reusable = false;
                match &mut edge.delay {
                    EdgeDelay::Audio(delay) => delay.invalidate(),
                    EdgeDelay::Midi(delay) => delay.invalidate(),
                }
            }
        }
        for sink in &mut self.sinks {
            if sink.node == node {
                sink.reusable = false;
                sink.delay.invalidate();
            }
        }
        for (output, delay, _) in &mut self.midi_outputs {
            if *output == node {
                delay.invalidate();
            }
        }
    }

    pub fn midi_overflows(&self) -> u64 {
        self.edges
            .iter()
            .filter_map(|edge| match &edge.delay {
                EdgeDelay::Midi(delay) => Some(delay.overflow_count),
                _ => None,
            })
            .fold(0u64, u64::saturating_add)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_impulse_with_irregular_blocks_and_fanout() {
        for delay in [0, 1, 128, 512, 1024] {
            let mut d = AudioDelay::new(delay).unwrap();
            let mut source = StereoBuf::new();
            let mut out = StereoBuf::new();
            let mut got = Vec::new();
            let mut cursor = 0;
            for n in [1, 127, 256, 511, 512, 1024] {
                source.clear(n);
                if cursor == 0 {
                    source.l[0] = 1.0;
                    source.r[0] = -1.0;
                }
                d.write(Some(&source), &mut out, n);
                got.extend_from_slice(&out.l[..n]);
                cursor += n;
            }
            assert_eq!(got.iter().position(|v| *v != 0.0), Some(delay as usize));
            assert_eq!(got.iter().sum::<f32>(), 1.0);
        }
    }
    #[test]
    fn missing_source_flushes_then_silences() {
        let mut d = AudioDelay::new(2).unwrap();
        let mut source = StereoBuf::new();
        let mut out = StereoBuf::new();
        source.l[0] = 1.0;
        d.write(Some(&source), &mut out, 1);
        d.write(None, &mut out, 4);
        assert_eq!(&out.l[..4], &[0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn latency_edits_preserve_exact_audio_history_across_callback_sizes() {
        for block in [64, 256, 512] {
            let mut delay = AudioDelay::new(0).unwrap();
            let mut cursor = 0;
            let mut source = StereoBuf::new();
            let mut output = StereoBuf::new();
            for latency in [0, 192, 230, 208, 96, 0, 111, 131, 4096, 4352, 0] {
                let mut next = AudioDelay::new(latency).unwrap();
                next.inherit(&mut delay);
                delay = next;
                for _ in 0..(2 * AudioDelay::HISTORY_RESERVE / block) {
                    for i in 0..block {
                        source.l[i] = (cursor + i + 1) as f32;
                        source.r[i] = -(cursor as f32 + i as f32 + 1.0);
                    }
                    delay.write(Some(&source), &mut output, block);
                    for i in 0..block {
                        if cursor + i >= latency as usize {
                            let expected = (cursor + i - latency as usize + 1) as f32;
                            assert_eq!(
                                [output.l[i], output.r[i]],
                                [expected, -expected],
                                "block={block} latency={latency} at={}",
                                cursor + i
                            );
                        }
                    }
                    cursor += block;
                }
            }
        }
    }

    #[test]
    fn consecutive_capacity_growth_keeps_all_available_history() {
        let mut delay = AudioDelay::new(0).unwrap();
        let mut cursor = 0;
        for _ in 0..AudioDelay::HISTORY_RESERVE * 2 {
            delay.next([cursor as f32 + 1.0; 2]);
            cursor += 1;
        }
        for (latency, count) in [(9000, 17), (10_000, 19), (9500, 12_000), (128, 12_000)] {
            let retained = delay.valid;
            let mut next = AudioDelay::new(latency as u64).unwrap();
            next.inherit(&mut delay);
            delay = next;
            for elapsed in 0..count {
                let got = delay.next([cursor as f32 + 1.0; 2]);
                let expected = if elapsed + retained >= latency {
                    (cursor + 1 - latency) as f32
                } else {
                    // The previous ring never held this older input. Its absence must not
                    // be disguised as generated audio or a shifted/replayed sample.
                    0.0
                };
                assert_eq!(got, [expected; 2], "latency={latency} at={cursor}");
                cursor += 1;
            }
        }
    }

    #[test]
    fn midi_retiming_keeps_order_and_cleans_notes_when_events_would_be_late() {
        let mut previous = MidiDelay::new(10, 16).unwrap();
        let mut output = MidiBuf::new();
        previous.process(
            &[
                MidiEvent {
                    time: 0,
                    data: [0x90, 60, 1],
                },
                MidiEvent {
                    time: 15,
                    data: [0x80, 60, 0],
                },
                MidiEvent {
                    time: 15,
                    data: [0x90, 61, 1],
                },
            ],
            &mut output,
            0,
            8,
        );
        let mut next = MidiDelay::new(20, 16).unwrap();
        next.inherit(&mut previous);
        next.process(&[], &mut output, 8, 32);
        assert_eq!(
            output.events(),
            &[
                MidiEvent {
                    time: 12,
                    data: [0x90, 60, 1]
                },
                MidiEvent {
                    time: 27,
                    data: [0x80, 60, 0]
                },
                MidiEvent {
                    time: 27,
                    data: [0x90, 61, 1]
                },
            ]
        );
        output.clear();
        next.process(
            &[MidiEvent {
                time: 0,
                data: [0x80, 61, 0],
            }],
            &mut output,
            40,
            1,
        );
        let mut shorter = MidiDelay::new(0, 16).unwrap();
        shorter.inherit(&mut next);
        shorter.process(
            &[MidiEvent {
                time: 0,
                data: [0x90, 62, 1],
            }],
            &mut output,
            41,
            1,
        );
        assert_eq!(
            output.events(),
            &[
                MidiEvent {
                    time: 0,
                    data: [0xb0, 64, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0xb0, 123, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0xb0, 120, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0x90, 62, 1]
                },
            ]
        );
    }

    #[test]
    fn plan_charges_warm_history_before_allocating_zero_delay_routes() {
        let analysis = TimingAnalysis {
            sinks: vec![(0, 0)],
            ..TimingAnalysis::default()
        };
        let limits = TimingLimits {
            max_delay_bytes: AudioDelay::allocation_bytes(0).unwrap() - 1,
            ..TimingLimits::default()
        };
        assert!(TimingPlan::prepare(&analysis, limits, &[]).is_err());
        let plan = TimingPlan::prepare(&analysis, TimingLimits::default(), &[]).unwrap();
        assert_eq!(
            plan.allocated_delay_bytes,
            AudioDelay::allocation_bytes(0).unwrap()
        );
    }

    #[test]
    fn hardware_midi_delay_to_zero_keeps_cleanup_through_queued_plans() {
        let limits = TimingLimits {
            midi_capacity: 16,
            ..TimingLimits::default()
        };
        let prepare = |delay| {
            TimingPlan::prepare(
                &TimingAnalysis {
                    midi_outputs: vec![(1, delay)],
                    delay_bytes: if delay == 0 {
                        0
                    } else {
                        limits.midi_capacity * size_of::<TimedMidi>()
                    },
                    ..TimingAnalysis::default()
                },
                limits,
                &[],
            )
            .unwrap()
        };
        let mut previous = prepare(128);
        let (_, delay, out) = &mut previous.midi_outputs[0];
        delay.process(
            &[
                MidiEvent {
                    time: 0,
                    data: [0x90, 60, 1],
                },
                MidiEvent {
                    time: 100,
                    data: [0x80, 60, 0],
                },
            ],
            out,
            0,
            200,
        );
        assert_eq!(
            out.events(),
            &[MidiEvent {
                time: 128,
                data: [0x90, 60, 1]
            }]
        );
        let mut next = prepare(0);
        next.inherit(&mut previous);
        let mut final_plan = prepare(0);
        final_plan.inherit(&mut next);
        let (_, delay, out) = &mut final_plan.midi_outputs[0];
        delay.process(
            &[MidiEvent {
                time: 0,
                data: [0x90, 61, 1],
            }],
            out,
            200,
            1,
        );
        assert_eq!(
            out.events(),
            &[
                MidiEvent {
                    time: 0,
                    data: [0xb0, 64, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0xb0, 123, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0xb0, 120, 0]
                },
                MidiEvent {
                    time: 0,
                    data: [0x90, 61, 1]
                },
            ]
        );
    }
    #[test]
    fn midi_q_minus_q_q_plus_and_equal_order() {
        let mut d = MidiDelay::new(256, 16).unwrap();
        let mut out = MidiBuf::new();
        let event = |time, note| MidiEvent {
            time,
            data: [0x90, note, 1],
        };
        d.process(
            &[event(255, 1), event(256, 2), event(257, 3), event(256, 4)],
            &mut out,
            0,
            256,
        );
        assert!(out.events().is_empty());
        d.process(&[], &mut out, 256, 256);
        assert_eq!(out.events(), &[event(255, 1)]);
        out.clear();
        d.process(&[], &mut out, 512, 1);
        assert_eq!(out.events(), &[event(0, 2), event(0, 4)]);
        out.clear();
        d.process(&[], &mut out, 513, 127);
        assert_eq!(out.events(), &[event(0, 3)]);
    }
    #[test]
    fn zero_delay_midi_passthrough_is_ordered_and_tracks_notes() {
        let mut d = MidiDelay::new(0, 2).unwrap();
        let mut out = MidiBuf::new();
        let events = [
            MidiEvent {
                time: 1,
                data: [0x90, 60, 1],
            },
            MidiEvent {
                time: 3,
                data: [0x90, 61, 1],
            },
        ];
        d.process(&events, &mut out, 0, 8);
        assert_eq!(out.events(), &events);
        assert!(d.future.is_empty());
        d.invalidate();
        out.clear();
        d.process(&[], &mut out, 8, 8);
        assert_eq!(out.events().len(), 3);
        assert!(out.events().iter().all(|event| event.data[0] == 0xb0));
    }

    #[test]
    fn midi_queue_overflow_schedules_bounded_cleanup() {
        let mut d = MidiDelay::new(1024, 1).unwrap();
        let mut out = MidiBuf::new();
        d.process(
            &[
                MidiEvent {
                    time: 0,
                    data: [0x90, 60, 1],
                },
                MidiEvent {
                    time: 1,
                    data: [0x80, 60, 0],
                },
            ],
            &mut out,
            0,
            128,
        );
        assert_eq!(d.overflow_count, 1);
        d.process(&[], &mut out, 128, 128);
        // Nothing was ever delivered through this edge, so no destination state is live
        // and the scoped cleanup emits nothing.
        assert_eq!(out.events().len(), 0);
        assert!(d.future.is_empty());
    }
    #[test]
    fn absolute_time_overflow_and_late_events_do_not_replay() {
        let mut d = MidiDelay::new(8, 16).unwrap();
        let mut out = MidiBuf::new();
        d.process(
            &[MidiEvent {
                time: 0,
                data: [0x90, 60, 1],
            }],
            &mut out,
            0,
            1,
        );
        d.process(&[], &mut out, 100, 1);
        assert!(out.events().is_empty());
        assert_eq!(d.overflow_count, 1);
        d.process(&[], &mut out, u64::MAX, 1);
        assert_eq!(d.overflow_count, 2);
    }
    #[test]
    fn invalidated_history_cannot_replay_before_next_plan() {
        let mut audio = AudioDelay::new(2).unwrap();
        let mut source = StereoBuf::new();
        let mut output = StereoBuf::new();
        source.l[0] = 1.0;
        audio.write(Some(&source), &mut output, 1);
        audio.invalidate();
        audio.write(None, &mut output, 4);
        assert_eq!(&output.l[..4], &[0.0; 4]);
        let mut midi = MidiDelay::new(128, 8).unwrap();
        let mut out = MidiBuf::new();
        midi.process(
            &[MidiEvent {
                time: 0,
                data: [0x90, 60, 1],
            }],
            &mut out,
            0,
            1,
        );
        midi.invalidate();
        midi.process(&[], &mut out, 1, 256);
        assert_eq!(out.events().len(), 0);
    }

    #[test]
    fn allocation_bounds_are_explicit() {
        assert!(AudioDelay::new(u64::MAX).is_err());
        assert!(MidiDelay::new(0, 0).is_err());
        assert!(MidiDelay::new(0, MidiBuf::CAPACITY + 1).is_err());
    }
}
