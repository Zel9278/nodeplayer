//! Prepared delay storage. Constructors run on the control thread; processing and history
//! transfer use bounded storage without locks or allocation.

use anyhow::{Result, ensure};

use super::timing::{EdgeKey, EdgeKind, TimingAnalysis, TimingLimits};
use super::{MidiBuf, MidiEvent, StereoBuf};

pub struct AudioDelay {
    frames: Vec<[f32; 2]>,
    cursor: usize,
    invalid_frames: usize,
}

impl AudioDelay {
    pub fn new(frames: u64) -> Result<Self> {
        ensure!(
            frames <= TimingLimits::default().max_latency_frames,
            "audio delay exceeds frame limit"
        );
        Ok(Self {
            frames: vec![[0.0; 2]; usize::try_from(frames)?],
            cursor: 0,
            invalid_frames: 0,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// Invalidate without zeroing a potentially long allocation on the audio thread.
    pub fn invalidate(&mut self) {
        self.invalid_frames = self.frames.len();
    }

    fn next(&mut self, sample: [f32; 2]) -> [f32; 2] {
        if self.frames.is_empty() {
            return sample;
        }
        let old = std::mem::replace(&mut self.frames[self.cursor], sample);
        let delayed = if self.invalid_frames > 0 {
            self.invalid_frames -= 1;
            [0.0; 2]
        } else {
            old
        };
        self.cursor += 1;
        if self.cursor == self.frames.len() {
            self.cursor = 0;
        }
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
            future: Vec::with_capacity(capacity),
            capacity,
            cleanup: false,
            live_notes: [0; 16],
            overflow_count: 0,
        })
    }

    pub fn frames(&self) -> u64 {
        self.delay
    }

    pub fn invalidate(&mut self) {
        self.future.clear();
        self.cleanup = true;
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
        let due = self.future.partition_point(|event| event.sample < end);
        for i in 0..due {
            let event = self.future[i];
            // Past events may arise only at a discontinuity: discard, never replay late.
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
        let mut edges = Vec::with_capacity(analysis.edges.len());
        for edge in &analysis.edges {
            let delay = match edge.key.kind {
                EdgeKind::Audio { .. } => EdgeDelay::Audio(AudioDelay::new(edge.delay_frames)?),
                EdgeKind::Midi => {
                    EdgeDelay::Midi(Box::new(MidiDelay::new(edge.delay_frames, limits.midi_capacity)?))
                }
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
            .filter(|(_, delay)| *delay > 0)
            .map(|&(node, delay)| Ok((node, MidiDelay::new(delay, limits.midi_capacity)?, MidiBuf::new())))
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
            allocated_delay_bytes: analysis.delay_bytes,
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
                matches!(&self.edges[index].delay, EdgeDelay::Midi(new) if new.frames() == old_delay.frames() && new.capacity == old_delay.capacity)
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
                    (EdgeDelay::Audio(new), EdgeDelay::Audio(old))
                        if new.frames() == old.frames() =>
                    {
                        std::mem::swap(new, old)
                    }
                    (EdgeDelay::Midi(new), EdgeDelay::Midi(old))
                        if new.frames() == old.frames() && new.capacity == old.capacity =>
                    {
                        std::mem::swap(new, old)
                    }
                    _ => {}
                }
            }
        }
        for sink in &mut self.sinks {
            if let Some(old) = previous.sinks.iter_mut().find(|old| {
                old.node == sink.node && old.reusable && old.delay.frames() == sink.delay.frames()
            }) {
                std::mem::swap(&mut sink.delay, &mut old.delay);
            }
        }
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
        let events = [MidiEvent { time: 1, data: [0x90, 60, 1] }, MidiEvent { time: 3, data: [0x90, 61, 1] }];
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
