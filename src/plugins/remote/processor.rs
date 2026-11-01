use super::{
    Core,
    shared::{self, DELAY, FREE, INPUT, MAX_LATENCY, MIDI, OUTPUT, Q, SLOTS},
};
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use std::sync::{Arc, atomic::Ordering};

const HISTORY: usize = 131072;
#[derive(Clone, Copy)]
struct Timed {
    at: u64,
    sequence: u64,
    index: u32,
    event: MidiEvent,
}
pub(super) struct RemoteProcessor {
    map: Option<Arc<shared::Shared>>,
    core: Arc<Core>,
    cursor: u64,
    active: bool,
    in_l: [f32; Q],
    in_r: [f32; Q],
    in_midi: [shared::WireMidi; MIDI],
    in_count: usize,
    fill: usize,
    first: u64,
    sequence: u64,
    wet_l: Vec<f32>,
    wet_r: Vec<f32>,
    wet_tags: Vec<u64>,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    dry_tags: Vec<u64>,
    future: Vec<Timed>,
    notes_reset: bool,
    child_reset: bool,
    prime_until: u64,
    epoch: u64,
    minimum_input: u64,
}
impl RemoteProcessor {
    pub fn new(core: Arc<Core>) -> Self {
        let cursor = core.cursor.load(Ordering::Acquire);
        Self {
            core,
            map: None,
            cursor,
            active: true,
            in_l: [0.0; Q],
            in_r: [0.0; Q],
            in_midi: [shared::WireMidi::default(); MIDI],
            in_count: 0,
            fill: 0,
            first: cursor,
            sequence: 0,
            wet_l: vec![0.0; HISTORY],
            wet_r: vec![0.0; HISTORY],
            wet_tags: vec![u64::MAX; HISTORY],
            dry_l: vec![0.0; HISTORY],
            dry_r: vec![0.0; HISTORY],
            dry_tags: vec![u64::MAX; HISTORY],
            future: Vec::with_capacity(SLOTS * MIDI),
            notes_reset: false,
            child_reset: false,
            prime_until: cursor.saturating_add(DELAY as u64),
            epoch: 0,
            minimum_input: cursor,
        }
    }
    fn align_cursor(&mut self, first_sample: u64) {
        if first_sample == self.cursor {
            return;
        }
        // Discontinuities include metadata replacement after the engine used a temporary
        // fallback. Never retime queued audio/MIDI onto the new engine sample position.
        self.cursor = first_sample;
        self.minimum_input = first_sample;
        self.fill = 0;
        self.in_count = 0;
        self.future.clear();
        self.notes_reset = true;
        self.child_reset = true;
        self.prime_until = first_sample.saturating_add(DELAY as u64);
        self.core.cursor.store(first_sample, Ordering::Release);
    }
    fn attach(&mut self) {
        if let Ok(m) = self.core.map.try_lock()
            && let Some(m) = m.as_ref()
            && (self
                .map
                .as_ref()
                .is_none_or(|old| old.generation() != m.generation())
                || self.epoch != m.epoch())
        {
            self.map = Some(m.clone());
            self.epoch = m.epoch();
            self.fill = 0;
            self.in_count = 0;
            self.sequence = 0;
            self.future.clear();
            self.minimum_input = self.cursor;
            self.notes_reset = true;
            self.prime_until = self.cursor.saturating_add(DELAY as u64);
        }
    }
    fn collect(&mut self) {
        let Some(map) = &self.map else { return };
        for i in 0..SLOTS {
            let slot = map.slot(i);
            if slot.state.load(Ordering::Acquire) != OUTPUT {
                continue;
            }
            let payload = unsafe { slot.payload() as *const shared::Payload };
            // The child may mutate payload memory even after publishing OUTPUT. Snapshot
            // every field used below once; never create references into the shared payload.
            let (generation, first_sample, frames, midi_count, sequence) = unsafe {
                (
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).generation)),
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).first_sample)),
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).frames)),
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).midi_count)),
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).sequence)),
                )
            };
            let count = midi_count as usize;
            if generation == self.epoch
                && first_sample >= self.minimum_input
                && frames as usize == Q
                && count <= MIDI
                && let Some(at) = first_sample.checked_add(DELAY as u64)
                && at.saturating_add(Q as u64) > self.cursor
                && at < self.cursor.saturating_add((HISTORY - Q) as u64)
            {
                for j in 0..Q {
                    let t = at + j as u64;
                    if t >= self.cursor {
                        let k = t as usize & (HISTORY - 1);
                        let (left, right) = unsafe {
                            (
                                std::ptr::read_volatile(std::ptr::addr_of!((*payload).left[j])),
                                std::ptr::read_volatile(std::ptr::addr_of!((*payload).right[j])),
                            )
                        };
                        self.wet_l[k] = left;
                        self.wet_r[k] = right;
                        self.wet_tags[k] = t;
                    }
                }
                for index in 0..count {
                    let e = unsafe {
                        std::ptr::read_volatile(std::ptr::addr_of!((*payload).midi[index]))
                    };
                    if e.offset as usize >= Q {
                        continue;
                    }
                    let t = at + e.offset as u64;
                    if t >= self.cursor && self.future.len() < self.future.capacity() {
                        self.future.push(Timed {
                            at: t,
                            sequence,
                            index: index as u32,
                            event: MidiEvent { time: 0, data: e.data },
                        });
                    }
                }
            }
            slot.state.store(FREE, Ordering::Release);
        }
    }
    fn publish(&mut self) {
        if !self.core.ready.load(Ordering::Acquire) {
            return;
        }
        let Some(map) = &self.map else { return };
        for i in 0..SLOTS {
            let slot = map.slot(i);
            if slot.state.load(Ordering::Acquire) != FREE {
                continue;
            }
            let p = unsafe { slot.payload() };
            p.generation = self.epoch;
            p.sequence = self.first;
            p.first_sample = self.first;
            p.frames = Q as u32;
            p.midi_count = self.in_count as u32;
            p.left.copy_from_slice(&self.in_l);
            p.right.copy_from_slice(&self.in_r);
            if self.child_reset {
                // A dropped quantum may contain a note-off. Deliver bounded panic to
                // the child itself, not merely downstream; discard this quantum's notes.
                let mut count = 0;
                for channel in 0..16 {
                    for cc in [64, 123, 120] {
                        p.midi[count] = shared::WireMidi {
                            offset: 0,
                            data: [0xb0 | channel, cc, 0],
                            reserved: 0,
                        };
                        count += 1;
                    }
                }
                p.midi_count = count as u32;
                self.child_reset = false;
            } else {
                p.midi[..self.in_count].copy_from_slice(&self.in_midi[..self.in_count]);
            }
            slot.state.store(INPUT, Ordering::Release);
            self.sequence = self.sequence.wrapping_add(1);
            return;
        }
        self.core.overflows.fetch_add(1, Ordering::Relaxed);
        self.child_reset = true;
        self.notes_reset = true;
    }
    fn render(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi: &[MidiEvent],
        mut midi_out: Option<&mut MidiBuf>,
    ) {
        for o in outputs.iter_mut() {
            o.l[..n].fill(0.0);
            o.r[..n].fill(0.0);
        }
        if let Some(out) = midi_out.as_deref_mut() {
            out.clear();
        }
        if !self.active {
            return;
        }
        self.attach();
        self.collect();
        let start = self.cursor;
        let algorithm = self.core.algorithm_latency.load(Ordering::Acquire) as usize;
        let delay = DELAY + algorithm.min(MAX_LATENCY);
        let ready = self.core.ready.load(Ordering::Acquire);
        let mut missed = false;
        let mut midi_index = 0;
        for j in 0..n {
            let t = self.cursor;
            let k = t as usize & (HISTORY - 1);
            let (l, r) = inputs
                .first()
                .map(|b| (b.l[j], b.r[j]))
                .unwrap_or((0.0, 0.0));
            self.dry_l[k] = l;
            self.dry_r[k] = r;
            self.dry_tags[k] = t;
            let mut audio = (0.0, 0.0);
            if ready && t >= self.prime_until && self.wet_tags[k] == t {
                audio = (self.wet_l[k], self.wet_r[k]);
                self.wet_tags[k] = u64::MAX;
            } else {
                missed |= t >= self.prime_until;
                if !self.core.instrument.load(Ordering::Relaxed)
                    && let Some(source) = t.checked_sub(delay as u64)
                {
                    let x = source as usize & (HISTORY - 1);
                    if self.dry_tags[x] == source {
                        audio = (self.dry_l[x], self.dry_r[x]);
                    }
                }
            }
            if let Some(o) = outputs.first_mut() {
                o.l[j] = audio.0;
                o.r[j] = audio.1;
            }
            if self.fill == 0 {
                self.first = t;
            }
            self.in_l[self.fill] = l;
            self.in_r[self.fill] = r;
            // MIDI already has block-relative offsets; never clamp future events into now.
            while midi_index < midi.len() && (midi[midi_index].time as usize) <= j {
                let e = &midi[midi_index];
                midi_index += 1;
                if e.time as usize != j {
                    continue;
                }
                if self.in_count < MIDI {
                    self.in_midi[self.in_count] = shared::WireMidi {
                        offset: self.fill as u32,
                        data: e.data,
                        reserved: 0,
                    };
                    self.in_count += 1;
                } else {
                    self.core.overflows.fetch_add(1, Ordering::Relaxed);
                    self.notes_reset = true;
                    self.child_reset = true;
                }
            }
            self.fill += 1;
            self.cursor = self.cursor.saturating_add(1);
            if self.fill == Q {
                self.publish();
                self.fill = 0;
                self.in_count = 0;
            }
        }
        if missed {
            self.core.underruns.fetch_add(1, Ordering::Relaxed);
            self.notes_reset = true;
        }
        if let Some(out) = midi_out {
            if self.notes_reset {
                for ch in 0..16 {
                    out.push(MidiEvent {
                        time: 0,
                        data: [0xb0 | ch, 123, 0],
                    });
                    out.push(MidiEvent {
                        time: 0,
                        data: [0xb0 | ch, 64, 0],
                    });
                }
                self.notes_reset = false;
            }
            // Bounded stable extraction. Late MIDI is discarded, never shifted to current block.
            self.future
                .sort_unstable_by_key(|e| (e.at, e.sequence, e.index));
            for e in &self.future {
                if e.at >= start && e.at < self.cursor {
                    out.push(MidiEvent {
                        time: (e.at - start) as u32,
                        ..e.event
                    });
                }
            }
        }
        self.future.retain(|e| e.at >= self.cursor);
        self.core.cursor.store(self.cursor, Ordering::Release);
    }
}

impl Processor for RemoteProcessor {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.render(n, inputs, outputs, &[], None)
    }
    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi: &[MidiEvent],
        out: &mut MidiBuf,
    ) {
        self.render(n, inputs, outputs, midi, Some(out))
    }
    fn process_timed(
        &mut self,
        first_sample: u64,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi: &[MidiEvent],
        out: &mut MidiBuf,
    ) {
        self.align_cursor(first_sample);
        self.render(n, inputs, outputs, midi, Some(out));
    }
    fn deactivate(&mut self) {
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Snapshot, protocol::Command};
    use super::*;
    use std::{
        collections::HashMap,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU32, AtomicU64},
        },
    };
    fn core(map: Arc<shared::Shared>) -> Arc<Core> {
        let (tx, _rx) = crossbeam_channel::bounded::<Command>(1);
        Arc::new(Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            map: Mutex::new(Some(map)),
            retired: Mutex::new(Vec::new()),
            surface: Mutex::new(None),
            close_editor: AtomicU64::new(0),
            cursor: AtomicU64::new(0),
            next_editor: AtomicU64::new(0),
            algorithm_latency: AtomicU32::new(0),
            instrument: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            tx,
            snapshot: Mutex::new(Snapshot {
                metadata: None,
                state: None,
                displays: HashMap::new(),
                status: "running".into(),
                revision: 0,
                checkpoint_seq: 0,
            }),
        })
    }
    fn fixture() -> (Arc<shared::Shared>, Arc<Core>) {
        let mut bytes = [0u8; 16];
        use ring::rand::SecureRandom;
        ring::rand::SystemRandom::new().fill(&mut bytes).unwrap();
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let map = Arc::new(
            shared::Shared::create(
                std::env::temp_dir().join(format!("np-slot-test-{suffix}")),
                1,
            )
            .unwrap(),
        );
        let core = core(map.clone());
        (map, core)
    }
    fn echo(map: &shared::Shared) {
        for i in 0..SLOTS {
            let s = map.slot(i);
            if s.state
                .compare_exchange(INPUT, shared::WORKING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                s.state.store(OUTPUT, Ordering::Release);
            }
        }
    }
    #[test]
    fn irregular_blocks_deliver_audio_midi_at_fixed_cursor() {
        let (map, c) = fixture();
        let mut p = RemoteProcessor::new(c.clone());
        let mut input = vec![StereoBuf::new()];
        let mut output = vec![StereoBuf::new()];
        let mut midi = MidiBuf::new();
        let mut cursor = 0usize;
        let mut found = Vec::new();
        let mut found_midi = Vec::new();
        for n in [
            1, 127, 256, 511, 512, 1024, 127, 256, 511, 512, 1024, 511, 511,
        ] {
            input[0].clear(n);
            if cursor == 0 {
                input[0].l[0] = 1.0;
                input[0].r[0] = 1.0;
            }
            let events = if cursor == 0 {
                vec![MidiEvent {
                    time: 0,
                    data: [0x90, 60, 100],
                }]
            } else {
                Vec::new()
            };
            output[0].l[..n].fill(99.0);
            p.process_midi(n, &mut input, &mut output, &events, &mut midi);
            for (j, &x) in output[0].l[..n].iter().enumerate() {
                assert_ne!(x, 99.0);
                if x != 0.0 {
                    found.push((cursor + j, x));
                }
            }
            for e in midi.events() {
                if e.data[0] == 0x90 {
                    found_midi.push(cursor + e.time as usize);
                }
            }
            cursor += n;
            echo(&map);
        }
        assert_eq!(found, vec![(DELAY, 1.0)]);
        assert_eq!(found_midi, vec![DELAY]);
    }
    #[test]
    fn bogus_child_midi_count_is_dropped_without_panicking() {
        let (map, c) = fixture();
        let mut p = RemoteProcessor::new(c);
        p.attach();
        let slot = map.slot(0);
        let payload = unsafe { slot.payload() };
        payload.generation = p.epoch;
        payload.first_sample = 0;
        payload.frames = Q as u32;
        payload.midi_count = (MIDI as u32).saturating_add(1);
        slot.state.store(OUTPUT, Ordering::Release);
        p.collect();
        assert_eq!(slot.state.load(Ordering::Acquire), FREE);
        assert!(p.future.is_empty());
    }
    #[test]
    fn late_completion_never_replays_and_working_slot_never_reused() {
        let (map, c) = fixture();
        c.instrument.store(true, Ordering::Release);
        let mut p = RemoteProcessor::new(c.clone());
        let mut input = vec![StereoBuf::new()];
        let mut output = vec![StereoBuf::new()];
        input[0].l[0] = 1.0;
        p.process(Q, &mut input, &mut output);
        let s = map.slot(0);
        assert_eq!(s.state.load(Ordering::Acquire), INPUT);
        s.state.store(shared::WORKING, Ordering::Release);
        let first = unsafe { s.payload().first_sample };
        input[0].clear(Q);
        for _ in 0..40 {
            p.process(Q, &mut input, &mut output);
            assert!(output[0].l[..Q].iter().all(|x| *x == 0.0));
        }
        assert_eq!(unsafe { s.payload().first_sample }, first);
        assert_eq!(s.state.load(Ordering::Acquire), shared::WORKING);
        s.state.store(OUTPUT, Ordering::Release);
        p.process(Q, &mut input, &mut output);
        assert!(output[0].l[..Q].iter().all(|x| *x == 0.0));
        assert!(c.overflows.load(Ordering::Acquire) > 0);
    }
    #[test]
    fn timed_cursor_jump_discards_old_outputs_and_uses_engine_clock() {
        let (map, c) = fixture();
        let mut p = RemoteProcessor::new(c.clone());
        let mut input = vec![StereoBuf::new()];
        let mut output = vec![StereoBuf::new()];
        let mut midi = MidiBuf::new();
        input[0].l[0] = 1.0;
        p.process_timed(10_000, Q, &mut input, &mut output, &[], &mut midi);
        echo(&map);
        input[0].clear(Q);
        let resume = 20_000;
        for i in 0..20 {
            p.process_timed(
                resume + i * Q as u64,
                Q,
                &mut input,
                &mut output,
                &[],
                &mut midi,
            );
            echo(&map);
            assert!(output[0].l[..Q].iter().all(|v| *v == 0.0));
        }
        assert_eq!(c.cursor.load(Ordering::Acquire), resume + 20 * Q as u64);
    }
    #[test]
    fn dropped_quantum_sends_cleanup_to_child_before_new_notes() {
        let (map, c) = fixture();
        let mut p = RemoteProcessor::new(c);
        for i in 0..SLOTS {
            map.slot(i).state.store(shared::WORKING, Ordering::Release);
        }
        let mut input = vec![StereoBuf::new()];
        let mut output = vec![StereoBuf::new()];
        p.process(Q, &mut input, &mut output);
        map.slot(0).state.store(FREE, Ordering::Release);
        p.process(Q, &mut input, &mut output);
        assert_eq!(map.slot(0).state.load(Ordering::Acquire), INPUT);
        let payload = unsafe { map.slot(0).payload() };
        assert_eq!(payload.midi_count, 48);
        assert_eq!(payload.midi[0].data, [0xb0, 64, 0]);
        assert_eq!(payload.midi[47].data, [0xbf, 120, 0]);
    }
    #[test]
    fn epoch_change_discards_queued_previous_state_output() {
        let (map, c) = fixture();
        c.instrument.store(true, Ordering::Release);
        let mut p = RemoteProcessor::new(c);
        let mut input = vec![StereoBuf::new()];
        let mut output = vec![StereoBuf::new()];
        input[0].l[0] = 1.0;
        p.process(Q, &mut input, &mut output);
        echo(&map);
        map.invalidate();
        input[0].clear(Q);
        for _ in 0..20 {
            p.process(Q, &mut input, &mut output);
            echo(&map);
            assert!(output[0].l[..Q].iter().all(|x| *x == 0.0));
        }
    }
}
