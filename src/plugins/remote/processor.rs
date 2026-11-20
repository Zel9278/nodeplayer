use super::{
    Core,
    shared::{self, DELAY, FREE, INPUT, MAX_LATENCY, MIDI, OUTPUT, Q, SLOTS},
};
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf, Transport};
use crate::plugins::AudioPorts;
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
    ports: AudioPorts,
    input_layout: shared::WireAudioLayout,
    output_layout: shared::WireAudioLayout,
    input_audio: Vec<[f32; Q]>,
    input_bindings: Vec<(usize, usize)>,
    output_bindings: Vec<(usize, usize)>,
    main_input: Option<usize>,
    main_output: Option<usize>,
    in_midi: [shared::WireMidi; MIDI],
    in_count: usize,
    fill: usize,
    first: u64,
    transport: Transport,
    in_transport: Transport,
    sequence: u64,
    wet: Vec<Vec<f32>>,
    wet_tags: Vec<u64>,
    dry: Vec<Vec<f32>>,
    dry_tags: Vec<u64>,
    future: Vec<Timed>,
    notes_reset: bool,
    child_reset: bool,
    prime_until: u64,
    epoch: u64,
    minimum_input: u64,
    minimum_dry: u64,
}
impl RemoteProcessor {
    pub fn new(core: Arc<Core>) -> Self {
        let ports = core
            .snapshot
            .lock()
            .unwrap()
            .metadata
            .as_ref()
            .map(|metadata| metadata.audio_ports.clone())
            .unwrap_or_default();
        Self::with_ports(core, ports)
    }

    fn with_ports(core: Arc<Core>, ports: AudioPorts) -> Self {
        // Called only on the control thread. Metadata has already passed validation.
        debug_assert!(ports.validate().is_ok());
        let cursor = core.cursor.load(Ordering::Acquire);
        let bindings = |ports: &[crate::plugins::AudioPort]| -> Vec<(usize, usize)> {
            ports
                .iter()
                .enumerate()
                .flat_map(|(bus, port)| (0..port.channels).map(move |channel| (bus, channel)))
                .collect()
        };
        let input_bindings = bindings(&ports.inputs);
        let output_bindings = bindings(&ports.outputs);
        let main_input = ports.inputs.iter().position(|port| port.main);
        let main_output = ports.outputs.iter().position(|port| port.main);
        let dry_channels = main_output.map_or(0, |index| ports.outputs[index].channels);
        Self {
            core,
            map: None,
            cursor,
            active: true,
            input_layout: shared::WireAudioLayout::new(&ports.inputs),
            output_layout: shared::WireAudioLayout::new(&ports.outputs),
            input_audio: vec![[0.0; Q]; input_bindings.len()],
            wet: vec![vec![0.0; HISTORY]; output_bindings.len()],
            dry: vec![vec![0.0; HISTORY]; dry_channels],
            input_bindings,
            output_bindings,
            main_input,
            main_output,
            ports,
            in_midi: [shared::WireMidi::default(); MIDI],
            in_count: 0,
            fill: 0,
            first: cursor,
            transport: Transport::default(),
            in_transport: Transport::default(),
            sequence: 0,
            wet_tags: vec![u64::MAX; HISTORY],
            dry_tags: vec![u64::MAX; HISTORY],
            future: Vec::with_capacity(SLOTS * MIDI),
            notes_reset: false,
            child_reset: false,
            prime_until: cursor.saturating_add(DELAY as u64),
            epoch: 0,
            minimum_input: cursor,
            minimum_dry: cursor,
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
        self.minimum_dry = first_sample;
        self.fill = 0;
        self.in_count = 0;
        self.future.clear();
        self.notes_reset = true;
        self.child_reset = true;
        self.prime_until = first_sample.saturating_add(DELAY as u64);
        self.core.cursor.store(first_sample, Ordering::Release);
    }
    fn attach(&mut self) {
        // Ordinary warm plugin activation keeps the same mapping/epoch. A new worker or
        // explicit state-load epoch is a real old-output boundary: discard queued wet/MIDI,
        // while retaining actual dry-input history for the continuously running fallback.
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
            let (input_layout, output_layout) = unsafe {
                (
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).input_layout)),
                    std::ptr::read_volatile(std::ptr::addr_of!((*payload).output_layout)),
                )
            };
            let count = midi_count as usize;
            if input_layout == self.input_layout
                && output_layout == self.output_layout
                && generation == self.epoch
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
                        for (channel, history) in self.wet.iter_mut().enumerate() {
                            history[k] = unsafe {
                                std::ptr::read_volatile(
                                    shared::Payload::channel_ptr(payload, channel).add(j),
                                )
                            };
                        }
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
                            event: MidiEvent {
                                time: 0,
                                data: e.data,
                            },
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
            p.transport = self.in_transport.into();
            p.input_layout = self.input_layout;
            p.output_layout = self.output_layout;
            for (channel, data) in self.input_audio.iter().enumerate() {
                p.channel_mut(channel).copy_from_slice(data);
            }
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
            o.clear(n);
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
            let input = self.main_input.and_then(|index| inputs.get(index));
            let dry_channels = self.dry.len();
            for (channel, history) in self.dry.iter_mut().enumerate() {
                history[k] =
                    input.map_or(0.0, |input| input.mapped_sample(channel, dry_channels, j));
            }
            self.dry_tags[k] = t;
            let wet = ready && t >= self.prime_until && self.wet_tags[k] == t;
            let source = t.checked_sub(delay as u64).filter(|&source| {
                source >= self.minimum_dry
                    && self.dry_tags[source as usize & (HISTORY - 1)] == source
            });
            if !wet {
                missed |= t >= self.prime_until;
            }
            for (flat, &(bus, channel)) in self.output_bindings.iter().enumerate() {
                let audio = if wet {
                    self.wet[flat][k]
                } else if Some(bus) == self.main_output
                    && !self.core.instrument.load(Ordering::Relaxed)
                {
                    source.map_or(0.0, |source| {
                        self.dry[channel][source as usize & (HISTORY - 1)]
                    })
                } else {
                    0.0
                };
                if let Some(output) = outputs
                    .get_mut(bus)
                    .filter(|output| channel < output.channels())
                {
                    output.channel_mut(channel)[j] = audio;
                }
            }
            if wet {
                self.wet_tags[k] = u64::MAX;
            }
            if self.fill == 0 {
                self.first = t;
                // Preserve the song clock of a partial quantum across host callbacks.
                self.in_transport = self.transport.at_offset(j);
            }
            for (flat, &(bus, channel)) in self.input_bindings.iter().enumerate() {
                self.input_audio[flat][self.fill] = inputs
                    .get(bus)
                    .filter(|input| channel < input.channels())
                    .map_or(0.0, |input| input.channel(channel)[j]);
            }
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
        self.transport = self.transport.at_offset(n);
    }
}

impl Processor for RemoteProcessor {
    fn audio_ports(&self) -> Option<AudioPorts> {
        Some(self.ports.clone())
    }
    fn set_transport(&mut self, transport: Transport) {
        self.transport = transport;
    }

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

    fn ports(inputs: &[(usize, bool)], outputs: &[(usize, bool)]) -> AudioPorts {
        let side = |values: &[(usize, bool)]| {
            values
                .iter()
                .enumerate()
                .map(|(index, &(channels, main))| crate::plugins::AudioPort {
                    id: index.to_string(),
                    name: format!("Bus {index}"),
                    channels,
                    main,
                    layout: None,
                })
                .collect()
        };
        AudioPorts {
            inputs: side(inputs),
            outputs: side(outputs),
        }
    }

    #[test]
    fn multibus_bridge_preserves_surround_sidechain_and_aux_outputs() {
        let (map, core) = fixture();
        let ports = ports(&[(4, true), (1, false)], &[(4, true), (2, false)]);
        let mut processor = RemoteProcessor::with_ports(core.clone(), ports);
        let mut input = [StereoBuf::with_channels(4), StereoBuf::with_channels(1)];
        let mut output = [StereoBuf::with_channels(4), StereoBuf::with_channels(2)];
        for channel in 0..4 {
            input[0].channel_mut(channel)[..Q].fill(channel as f32 + 1.0);
        }
        input[1].channel_mut(0)[..Q].fill(0.5);
        for quantum in 0..=DELAY / Q + 2 {
            processor.process(Q, &mut input, &mut output);
            if quantum >= DELAY / Q {
                for (channel, expected) in [1.5, 2.0, 3.0, 4.0].into_iter().enumerate() {
                    assert!(
                        output[0].channel(channel)[..Q]
                            .iter()
                            .all(|&sample| sample == expected)
                    );
                }
                assert!(
                    output[1].channel(0)[..Q]
                        .iter()
                        .all(|&sample| sample == 1.0)
                );
                assert!(
                    output[1].channel(1)[..Q]
                        .iter()
                        .all(|&sample| sample == 12.0)
                );
            }
            for index in 0..SLOTS {
                let slot = map.slot(index);
                if slot
                    .state
                    .compare_exchange(INPUT, shared::WORKING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let payload = unsafe { slot.payload() };
                    for frame in 0..Q {
                        let sidechain = payload.channel(4)[frame];
                        let room = payload.channel(3)[frame];
                        payload.channel_mut(0)[frame] += sidechain;
                        payload.channel_mut(4)[frame] = sidechain * 2.0;
                        payload.channel_mut(5)[frame] = room * 3.0;
                    }
                    slot.state.store(OUTPUT, Ordering::Release);
                }
            }
        }
        // Failed effects return only the main stream; sidechain audio must never
        // appear on an auxiliary output just because the bus indices line up.
        core.ready.store(false, Ordering::Release);
        processor.process(Q, &mut input, &mut output);
        for channel in 0..4 {
            assert!(
                output[0].channel(channel)[..Q]
                    .iter()
                    .all(|&sample| sample == channel as f32 + 1.0)
            );
        }
        assert!(
            output[1].channel(0)[..Q]
                .iter()
                .all(|&sample| sample == 0.0)
        );
        assert!(
            output[1].channel(1)[..Q]
                .iter()
                .all(|&sample| sample == 0.0)
        );
    }

    #[test]
    fn zero_input_instrument_and_zero_output_effect_keep_valid_timelines() {
        let (map, core) = fixture();
        core.instrument.store(true, Ordering::Release);
        let mut instrument = RemoteProcessor::with_ports(core.clone(), ports(&[], &[(6, true)]));
        let mut output = [StereoBuf::with_channels(6)];
        for quantum in 0..=DELAY / Q {
            instrument.process(Q, &mut [], &mut output);
            if quantum == DELAY / Q {
                for channel in 0..6 {
                    assert!(
                        output[0].channel(channel)[..Q]
                            .iter()
                            .all(|&sample| sample == channel as f32 + 1.0)
                    );
                }
            }
            for index in 0..SLOTS {
                let slot = map.slot(index);
                if slot
                    .state
                    .compare_exchange(INPUT, shared::WORKING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let payload = unsafe { slot.payload() };
                    for channel in 0..6 {
                        payload.channel_mut(channel).fill(channel as f32 + 1.0);
                    }
                    slot.state.store(OUTPUT, Ordering::Release);
                }
            }
        }
        core.ready.store(false, Ordering::Release);
        instrument.process(Q, &mut [], &mut output);
        for channel in 0..6 {
            assert!(
                output[0].channel(channel)[..Q]
                    .iter()
                    .all(|&sample| sample == 0.0)
            );
        }

        let (map, core) = fixture();
        let mut sink = RemoteProcessor::with_ports(core, ports(&[(1, true)], &[]));
        let mut input = [StereoBuf::with_channels(1)];
        for _ in 0..=DELAY / Q {
            sink.process(Q, &mut input, &mut []);
            echo(&map);
        }
        assert_eq!(sink.cursor, (DELAY + Q) as u64);
    }

    #[test]
    fn topology_mismatch_does_not_reinterpret_queued_channels() {
        let (map, core) = fixture();
        let mut processor = RemoteProcessor::with_ports(core, ports(&[(2, true)], &[(4, true)]));
        let mut input = [StereoBuf::new()];
        let mut output = [StereoBuf::with_channels(4)];
        processor.process(Q, &mut input, &mut output);
        let slot = map.slot(0);
        let payload = unsafe { slot.payload() };
        payload.output_layout = shared::WireAudioLayout::new(&AudioPorts::default().outputs);
        payload.left.fill(99.0);
        slot.state.store(OUTPUT, Ordering::Release);
        processor.collect();
        assert!(processor.wet_tags.iter().all(|&tag| tag == u64::MAX));
        assert_eq!(slot.state.load(Ordering::Acquire), FREE);
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
    fn transport_stays_with_the_first_sample_of_each_quantum() {
        use crate::engine::PlaybackState;
        let (map, c) = fixture();
        let mut p = RemoteProcessor::new(c);
        let mut input = [StereoBuf::new()];
        let mut output = [StereoBuf::new()];
        let playing = Transport {
            state: PlaybackState::Playing,
            position_seconds: 5.0,
            seconds_per_frame: 1.0 / 48_000.0,
        };
        p.set_transport(playing);
        p.process(100, &mut input, &mut output);
        let paused = Transport {
            state: PlaybackState::Paused,
            ..playing.at_offset(100)
        };
        p.set_transport(paused);
        p.process(Q - 100, &mut input, &mut output);
        assert_eq!(
            unsafe { map.slot(0).payload().transport }.decode(),
            Some(playing)
        );
        p.process(Q, &mut input, &mut output);
        assert_eq!(
            unsafe { map.slot(1).payload().transport }.decode(),
            Some(paused)
        );
        let resumed = Transport {
            position_seconds: 10.0,
            seconds_per_frame: 2.0 / 48_000.0,
            ..playing
        };
        p.set_transport(resumed);
        p.process(3 * Q, &mut input, &mut output);
        for i in 0..3 {
            assert_eq!(
                unsafe { map.slot(2 + i).payload().transport }.decode(),
                Some(resumed.at_offset(i * Q))
            );
        }
        p.set_transport(Transport::default());
        p.process(2 * Q, &mut input, &mut output);
        for i in 5..7 {
            assert_eq!(
                unsafe { map.slot(i).payload().transport }.decode(),
                Some(Transport::default())
            );
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

    fn signal(sample: usize, rate: f32) -> [f32; 2] {
        let phase = sample as f32 * std::f32::consts::TAU / rate;
        [
            0.2 + 0.04 * (997.0 * phase).sin(),
            -0.3 + 0.03 * (1301.0 * phase).cos(),
        ]
    }

    #[test]
    fn final_pcm_bridge_keeps_wet_and_dry_history_through_warm_restart_latency_edits() {
        use crate::engine::{compensation::AudioDelay, nodes::Output};
        fn render_final(
            processor: &mut RemoteProcessor,
            sink: &mut Output,
            device_delay: &mut AudioDelay,
            format: (f32, usize),
            input: &mut [StereoBuf; 1],
            output: &mut [StereoBuf; 1],
            pcm: &mut [f32],
        ) {
            let (rate, block) = format;
            let first = processor.cursor as usize;
            for i in 0..block {
                let value = signal(first + i, rate);
                input[0].l[i] = value[0];
                input[0].r[i] = value[1];
            }
            processor.process(block, input, output);
            sink.process(block, output, &mut []);
            pcm.fill(0.0);
            device_delay.mix_device(output.first(), pcm, 2);
        }
        for (rate, block) in [
            (48_000.0, 64),
            (48_000.0, 256),
            (48_000.0, 512),
            (96_000.0, 256),
        ] {
            let (map, core) = fixture();
            core.algorithm_latency.store(192, Ordering::Release);
            let mut processor = RemoteProcessor::new(core.clone());
            let mut sink = Output::new(0.0, Arc::default());
            let mut device_delay = AudioDelay::new(0).unwrap();
            let mut input = [StereoBuf::new()];
            let mut output = [StereoBuf::new()];
            let mut pcm = vec![0.0; block * 2];
            for _ in 0..(HISTORY / block) {
                render_final(
                    &mut processor,
                    &mut sink,
                    &mut device_delay,
                    (rate, block),
                    &mut input,
                    &mut output,
                    &mut pcm,
                );
                echo(&map);
            }
            for latency in [230, 208, 96, 0, MAX_LATENCY as u32, 111, 131, 0] {
                // The same worker/plugin remains alive. Missing worker output during its
                // parked interval uses the warmed algorithm+bridge dry reference.
                core.ready.store(false, Ordering::Release);
                core.algorithm_latency.store(latency, Ordering::Release);
                for _ in 0..(DELAY * 2 / block) {
                    let first = processor.cursor as usize;
                    render_final(
                        &mut processor,
                        &mut sink,
                        &mut device_delay,
                        (rate, block),
                        &mut input,
                        &mut output,
                        &mut pcm,
                    );
                    for (i, frame) in pcm.as_chunks::<2>().0.iter().enumerate() {
                        assert_eq!(
                            frame,
                            &signal(first + i - DELAY - latency as usize, rate),
                            "dry timing: rate={rate} block={block} latency={latency} at={}",
                            first + i
                        );
                    }
                    echo(&map);
                }
                core.ready.store(true, Ordering::Release);
                for _ in 0..(DELAY * 2 / block) {
                    render_final(
                        &mut processor,
                        &mut sink,
                        &mut device_delay,
                        (rate, block),
                        &mut input,
                        &mut output,
                        &mut pcm,
                    );
                    assert!(
                        pcm.iter()
                            .all(|sample| sample.is_finite() && *sample != 0.0),
                        "cold bridge after warm activation: rate={rate} block={block} latency={latency}"
                    );
                    echo(&map);
                }
            }
        }
    }

    #[test]
    fn generation_and_state_epochs_discard_wet_midi_but_keep_aligned_dry_input() {
        let (map, core) = fixture();
        let mut processor = RemoteProcessor::new(core.clone());
        let mut input = [StereoBuf::new()];
        let mut output = [StereoBuf::new()];
        let mut midi = MidiBuf::new();
        input[0].l[..Q].fill(0.25);
        input[0].r[..Q].fill(-0.5);
        for _ in 0..(DELAY * 2 / Q) {
            processor.process_timed(processor.cursor, Q, &mut input, &mut output, &[], &mut midi);
            echo(&map);
        }
        let cursor = processor.cursor;
        processor.wet[0][cursor as usize & (HISTORY - 1)] = 99.0;
        processor.wet_tags[cursor as usize & (HISTORY - 1)] = cursor;
        processor.future.push(Timed {
            at: cursor,
            sequence: 1,
            index: 0,
            event: MidiEvent {
                time: 0,
                data: [0x90, 60, 127],
            },
        });
        map.invalidate();
        processor.process_timed(cursor, Q, &mut input, &mut output, &[], &mut midi);
        assert!(output[0].l[..Q].iter().all(|sample| *sample == 0.25));
        assert!(
            midi.events()
                .iter()
                .all(|event| event.data[0] & 0xf0 != 0x90)
        );
        let path = std::env::temp_dir().join(format!(
            "np-slot-generation-{}-{}",
            std::process::id(),
            cursor
        ));
        let replacement = Arc::new(shared::Shared::create(path, map.generation() + 1).unwrap());
        *core.map.lock().unwrap() = Some(replacement);
        let cursor = processor.cursor;
        processor.process_timed(cursor, Q, &mut input, &mut output, &[], &mut midi);
        assert!(output[0].l[..Q].iter().all(|sample| *sample == 0.25));
        assert!(
            midi.events()
                .iter()
                .all(|event| event.data[0] & 0xf0 != 0x90)
        );
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
