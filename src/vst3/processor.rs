//! The audio-thread half of a VST3 instance.

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use super::host::{EventList, ParamChange, ParamChanges};
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf, Transport};

const MAX_CHANGED_PARAMS: usize = 256;

struct Buses {
    data: Vec<Vec<Vec<f32>>>,
    /// Channel pointer tables referenced by `abb`; never read directly.
    #[allow(dead_code)]
    ptrs: Vec<Vec<*mut f32>>,
    abb: Vec<AudioBusBuffers>,
}

impl Buses {
    fn new(channels: &[usize], max_block: usize) -> Self {
        let mut data: Vec<Vec<Vec<f32>>> =
            channels.iter().map(|&n| vec![vec![0.0; max_block]; n]).collect();
        let mut ptrs: Vec<Vec<*mut f32>> = data
            .iter_mut()
            .map(|bus| bus.iter_mut().map(|ch| ch.as_mut_ptr()).collect())
            .collect();
        let abb = ptrs
            .iter_mut()
            .map(|p| AudioBusBuffers {
                numChannels: p.len() as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: p.as_mut_ptr() },
            })
            .collect();
        Self { data, ptrs, abb }
    }
}

/// Parameters the plugin maps MIDI controllers to (`IMidiMapping`), per channel and controller
/// number (0..=127 CCs, then channel aftertouch and pitch bend).
pub struct CcMap(Vec<Option<ParamID>>);

impl CcMap {
    const PER_CHANNEL: usize = ControllerNumbers_::kCountCtrlNumber as usize;

    pub fn new(mapping: Option<&ComPtr<IMidiMapping>>) -> Self {
        let mut map = vec![None; 16 * Self::PER_CHANNEL];
        if let Some(m) = mapping {
            for ch in 0..16 {
                for cc in 0..Self::PER_CHANNEL {
                    let mut id: ParamID = 0;
                    if unsafe { m.getMidiControllerAssignment(0, ch as i16, cc as i16, &mut id) } == kResultTrue {
                        map[ch * Self::PER_CHANNEL + cc] = Some(id);
                    }
                }
            }
        }
        Self(map)
    }

    fn get(&self, ch: u8, cc: usize) -> Option<ParamID> {
        self.0.get(ch as usize * Self::PER_CHANNEL + cc).copied().flatten()
    }
}

pub struct Vst3Processor {
    audio: ComPtr<IAudioProcessor>,
    from_ui: rtrb::Consumer<ParamChange>,
    to_ui: rtrb::Producer<ParamChange>,
    in_changes: ComWrapper<ParamChanges>,
    out_changes: ComWrapper<ParamChanges>,
    ins: Buses,
    outs: Buses,
    ctx: Box<ProcessContext>,
    max_block: usize,
    /// Event buses, when the plugin has them.
    in_events: Option<ComWrapper<EventList>>,
    out_events: Option<ComWrapper<EventList>>,
    cc_map: CcMap,
    started: bool,
}

// The processor is only ever used from one thread at a time (moved to the audio thread once).
unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        audio: ComPtr<IAudioProcessor>,
        from_ui: rtrb::Consumer<ParamChange>,
        to_ui: rtrb::Producer<ParamChange>,
        in_channels: &[usize],
        out_channels: &[usize],
        midi: (bool, bool),
        cc_map: CcMap,
        sample_rate: f64,
        max_block: usize,
    ) -> Self {
        let mut ctx: Box<ProcessContext> = Box::new(unsafe { std::mem::zeroed() });
        update_transport(&mut ctx, Transport::default());
        ctx.sampleRate = sample_rate;
        Self {
            audio,
            from_ui,
            to_ui,
            in_changes: ComWrapper::new(ParamChanges::new(MAX_CHANGED_PARAMS)),
            out_changes: ComWrapper::new(ParamChanges::new(MAX_CHANGED_PARAMS)),
            ins: Buses::new(in_channels, max_block),
            outs: Buses::new(out_channels, max_block),
            ctx,
            max_block,
            in_events: midi.0.then(|| ComWrapper::new(EventList::new())),
            out_events: midi.1.then(|| ComWrapper::new(EventList::new())),
            cc_map,
            started: false,
        }
    }

    /// Turns MIDI into VST3 note events, and controllers into the parameters the plugin
    /// maps them to.
    fn queue_midi(&self, midi: &[MidiEvent]) {
        let Some(list) = &self.in_events else { return };
        for m in midi {
            let [s, a, b] = m.data;
            let ch = m.channel();
            let mut e: Event = unsafe { std::mem::zeroed() };
            e.sampleOffset = m.time as i32;
            let ctrl = |cc: usize, value: f64| {
                if let Some(id) = self.cc_map.get(ch, cc) {
                    self.in_changes.push_at(ParamChange { id, value }, m.time as i32);
                }
            };
            use Event_::EventTypes_::*;
            match s & 0xf0 {
                0x90 if b > 0 => {
                    e.r#type = kNoteOnEvent as u16;
                    e.__field0.noteOn =
                        NoteOnEvent { channel: ch as i16, pitch: a as i16, tuning: 0.0, velocity: b as f32 / 127.0, length: 0, noteId: -1 };
                }
                0x80 | 0x90 => {
                    e.r#type = kNoteOffEvent as u16;
                    e.__field0.noteOff =
                        NoteOffEvent { channel: ch as i16, pitch: a as i16, velocity: b as f32 / 127.0, noteId: -1, tuning: 0.0 };
                }
                0xa0 => {
                    e.r#type = kPolyPressureEvent as u16;
                    e.__field0.polyPressure =
                        PolyPressureEvent { channel: ch as i16, pitch: a as i16, pressure: b as f32 / 127.0, noteId: -1 };
                }
                0xb0 => {
                    ctrl(a as usize, b as f64 / 127.0);
                    continue;
                }
                0xd0 => {
                    ctrl(ControllerNumbers_::kAfterTouch as usize, a as f64 / 127.0);
                    continue;
                }
                0xe0 => {
                    ctrl(ControllerNumbers_::kPitchBend as usize, ((b as u32) << 7 | a as u32) as f64 / 16383.0);
                    continue;
                }
                _ => continue,
            }
            list.push(e);
        }
    }

    /// Converts the plugin's output events back to MIDI.
    fn take_midi(&self, n: usize, out: &mut MidiBuf) {
        let Some(list) = &self.out_events else { return };
        list.for_each(|e| {
            use Event_::EventTypes_::*;
            #[cfg(windows)]
            let (note_on, note_off, poly_pressure, legacy_midi_cc_out) = (
                kNoteOnEvent as u32, kNoteOffEvent as u32, kPolyPressureEvent as u32, kLegacyMIDICCOutEvent as u32,
            );
            #[cfg(not(windows))]
            let (note_on, note_off, poly_pressure, legacy_midi_cc_out) = (
                kNoteOnEvent, kNoteOffEvent, kPolyPressureEvent, kLegacyMIDICCOutEvent,
            );
            #[cfg(windows)]
            let (after_touch, pitch_bend) = (ControllerNumbers_::kAfterTouch as u32, ControllerNumbers_::kPitchBend as u32);
            #[cfg(not(windows))]
            let (after_touch, pitch_bend) = (ControllerNumbers_::kAfterTouch, ControllerNumbers_::kPitchBend);
            let time = (e.sampleOffset.max(0) as u32).min(n.saturating_sub(1) as u32);
            let vel = |v: f32| (v * 127.0).round().clamp(0.0, 127.0) as u8;
            let data = unsafe {
                match e.r#type as u32 {
                    t if t == note_on => {
                        let x = e.__field0.noteOn;
                        [0x90 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.velocity).max(1)]
                    }
                    t if t == note_off => {
                        let x = e.__field0.noteOff;
                        [0x80 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.velocity)]
                    }
                    t if t == poly_pressure => {
                        let x = e.__field0.polyPressure;
                        [0xa0 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.pressure)]
                    }
                    t if t == legacy_midi_cc_out => {
                        let x = e.__field0.midiCCOut;
                        let ch = x.channel as u8 & 15;
                        match x.controlNumber as u32 {
                            c if c < 128 => [0xb0 | ch, c as u8, x.value as u8 & 127],
                            c if c == after_touch => [0xd0 | ch, x.value as u8 & 127, 0],
                            c if c == pitch_bend => [0xe0 | ch, x.value as u8 & 127, x.value2 as u8 & 127],
                            130 => [0xc0 | ch, x.value as u8 & 127, 0],
                            _ => return,
                        }
                    }
                    _ => return,
                }
            };
            out.push(MidiEvent { time, data });
        });
        list.clear();
    }

    /// Processes one stereo block. `input`/`output` are `[left, right]` slices of equal length.
    pub fn process_stereo(
        &mut self,
        input: [&[f32]; 2],
        output: [&mut [f32]; 2],
        midi_in: &[MidiEvent],
        midi_out: Option<&mut MidiBuf>,
    ) {
        let [out_l, out_r] = output;
        let n = out_l.len().min(self.max_block);
        if !self.started {
            unsafe { self.audio.setProcessing(1) };
            self.started = true;
        }

        // Main input bus: stereo -> whatever the bus has.
        if let Some(bus) = self.ins.data.first_mut() {
            match bus.len() {
                0 => {}
                1 => {
                    for (i, s) in bus[0][..n].iter_mut().enumerate() {
                        *s = 0.5 * (input[0][i] + input[1][i]);
                    }
                }
                _ => {
                    bus[0][..n].copy_from_slice(&input[0][..n]);
                    bus[1][..n].copy_from_slice(&input[1][..n]);
                    for ch in &mut bus[2..] {
                        ch[..n].fill(0.0);
                    }
                }
            }
        }

        self.in_changes.clear();
        self.out_changes.clear();
        while let Ok(ch) = self.from_ui.pop() {
            self.in_changes.push(ch);
        }
        if let Some(l) = &self.in_events {
            l.clear();
        }
        if let Some(l) = &self.out_events {
            l.clear();
        }
        self.queue_midi(midi_in);
        let events = |l: &Option<ComWrapper<EventList>>| {
            l.as_ref().map_or(std::ptr::null_mut(), |l| l.as_com_ref::<IEventList>().unwrap().as_ptr())
        };

        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n as i32,
            numInputs: self.ins.abb.len() as i32,
            numOutputs: self.outs.abb.len() as i32,
            inputs: self.ins.abb.as_mut_ptr(),
            outputs: self.outs.abb.as_mut_ptr(),
            inputParameterChanges: self.in_changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
            outputParameterChanges: self
                .out_changes
                .as_com_ref::<IParameterChanges>()
                .unwrap()
                .as_ptr(),
            inputEvents: events(&self.in_events),
            outputEvents: events(&self.out_events),
            processContext: &mut *self.ctx,
        };
        let res = unsafe { self.audio.process(&mut data) };
        self.ctx.continousTimeSamples = self.ctx.continousTimeSamples.saturating_add(n as i64);

        for ch in self.out_changes.iter() {
            let _ = self.to_ui.push(ch);
        }
        if let Some(out) = midi_out {
            self.take_midi(n, out);
        }

        match (res == kResultOk, self.outs.data.first()) {
            (true, Some(bus)) if !bus.is_empty() => {
                let r = if bus.len() > 1 { &bus[1] } else { &bus[0] };
                out_l[..n].copy_from_slice(&bus[0][..n]);
                out_r[..n].copy_from_slice(&r[..n]);
            }
            _ => {
                out_l[..n].fill(0.0);
                out_r[..n].fill(0.0);
            }
        }
    }
}

impl Processor for Vst3Processor {
    fn set_transport(&mut self, transport: Transport) {
        update_transport(&mut self.ctx, transport);
    }

    fn process_timed(
        &mut self,
        first_sample: u64,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.ctx.continousTimeSamples = first_sample.min(i64::MAX as u64) as i64;
        self.process_midi(n, inputs, outputs, midi_in, midi_out);
    }

    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        let StereoBuf { l, r } = o;
        self.process_stereo([&i.l[..n], &i.r[..n]], [&mut l[..n], &mut r[..n]], &[], None);
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        let StereoBuf { l, r } = o;
        self.process_stereo([&i.l[..n], &i.r[..n]], [&mut l[..n], &mut r[..n]], midi_in, Some(midi_out));
    }

    fn deactivate(&mut self) {
        if std::mem::take(&mut self.started) {
            unsafe { self.audio.setProcessing(0) };
        }
    }
}

fn update_transport(ctx: &mut ProcessContext, transport: Transport) {
    use ProcessContext_::StatesAndFlags_::*;
    // The host has a seconds timeline, but no inferred musical tempo or time signature.
    #[allow(clippy::unnecessary_cast)] // VST3 enum bindings differ on Windows.
    {
        ctx.state = kContTimeValid as u32
            | if transport.is_playing() {
                kPlaying as u32
            } else {
                0
            };
    }
    ctx.projectTimeSamples = (transport.position_seconds * ctx.sampleRate).round() as i64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PlaybackState;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Capture {
        blocks: Mutex<Vec<(u32, i64, i64)>>,
        processing: Mutex<Vec<bool>>,
    }
    struct Plugin(Arc<Capture>);
    impl vst3::Class for Plugin {
        type Interfaces = (IAudioProcessor,);
    }
    impl IAudioProcessorTrait for Plugin {
        unsafe fn setBusArrangements(
            &self,
            _: *mut SpeakerArrangement,
            _: i32,
            _: *mut SpeakerArrangement,
            _: i32,
        ) -> tresult {
            kResultOk
        }
        unsafe fn getBusArrangement(
            &self,
            _: BusDirection,
            _: i32,
            _: *mut SpeakerArrangement,
        ) -> tresult {
            kResultOk
        }
        unsafe fn canProcessSampleSize(&self, _: i32) -> tresult {
            kResultOk
        }
        unsafe fn getLatencySamples(&self) -> u32 {
            0
        }
        unsafe fn setupProcessing(&self, _: *mut ProcessSetup) -> tresult {
            kResultOk
        }
        unsafe fn setProcessing(&self, state: TBool) -> tresult {
            self.0.processing.lock().unwrap().push(state != 0);
            kResultOk
        }
        unsafe fn process(&self, data: *mut ProcessData) -> tresult {
            let ctx = unsafe { &*(*data).processContext };
            self.0.blocks.lock().unwrap().push((
                ctx.state,
                ctx.projectTimeSamples,
                ctx.continousTimeSamples,
            ));
            kResultOk
        }
        unsafe fn getTailSamples(&self) -> u32 {
            0
        }
    }

    #[test]
    fn process_receives_song_time_separate_from_the_continuous_clock() {
        let capture = Arc::new(Capture::default());
        let audio = ComWrapper::new(Plugin(capture.clone()))
            .to_com_ptr::<IAudioProcessor>()
            .unwrap();
        let (_tx, from_ui) = rtrb::RingBuffer::new(1);
        let (to_ui, _rx) = rtrb::RingBuffer::new(1);
        let mut proc = Vst3Processor::new(
            audio,
            from_ui,
            to_ui,
            &[2],
            &[2],
            (false, false),
            CcMap::new(None),
            48_000.0,
            64,
        );
        let mut input = [StereoBuf::new()];
        let mut output = [StereoBuf::new()];
        let mut midi = MidiBuf::new();
        for (i, state) in [
            PlaybackState::Playing,
            PlaybackState::Paused,
            PlaybackState::Paused,
            PlaybackState::Stopped,
        ]
        .into_iter()
        .enumerate()
        {
            proc.set_transport(Transport {
                state,
                position_seconds: if state == PlaybackState::Stopped {
                    0.0
                } else {
                    1.25
                },
                seconds_per_frame: 1.0 / 48_000.0,
            });
            proc.process_timed(
                1000 + i as u64 * 64,
                64,
                &mut input,
                &mut output,
                &[],
                &mut midi,
            );
        }
        assert_eq!(*capture.processing.lock().unwrap(), [true]);
        let blocks = capture.blocks.lock().unwrap();
        assert_eq!(blocks.len(), 4);
        use ProcessContext_::StatesAndFlags_::*;
        for (i, &(flags, position, continuous)) in blocks.iter().enumerate() {
            #[allow(clippy::unnecessary_cast)]
            let expected_flags = kContTimeValid as u32 | if i == 0 { kPlaying as u32 } else { 0 };
            assert_eq!(flags, expected_flags);
            assert_eq!(position, if i == 3 { 0 } else { 60_000 });
            assert_eq!(continuous, 1000 + i as i64 * 64);
        }
        proc.deactivate();
        assert_eq!(*capture.processing.lock().unwrap(), [true, false]);
    }
}
