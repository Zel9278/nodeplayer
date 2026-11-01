//! The audio-thread half of a VST3 instance.

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use super::host::{EventList, ParamChange, ParamChanges};
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};

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
        use ProcessContext_::StatesAndFlags_::*;
        ctx.state = (kPlaying | kTempoValid | kTimeSigValid | kContTimeValid) as u32;
        ctx.sampleRate = sample_rate;
        ctx.tempo = 120.0;
        ctx.timeSigNumerator = 4;
        ctx.timeSigDenominator = 4;
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
            let time = (e.sampleOffset.max(0) as u32).min(n.saturating_sub(1) as u32);
            let vel = |v: f32| (v * 127.0).round().clamp(0.0, 127.0) as u8;
            let data = unsafe {
                match e.r#type as u32 {
                    t if t == kNoteOnEvent as u32 => {
                        let x = e.__field0.noteOn;
                        [0x90 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.velocity).max(1)]
                    }
                    t if t == kNoteOffEvent as u32 => {
                        let x = e.__field0.noteOff;
                        [0x80 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.velocity)]
                    }
                    t if t == kPolyPressureEvent as u32 => {
                        let x = e.__field0.polyPressure;
                        [0xa0 | (x.channel as u8 & 15), x.pitch as u8 & 127, vel(x.pressure)]
                    }
                    t if t == kLegacyMIDICCOutEvent as u32 => {
                        let x = e.__field0.midiCCOut;
                        let ch = x.channel as u8 & 15;
                        match x.controlNumber as u32 {
                            c if c < 128 => [0xb0 | ch, c as u8, x.value as u8 & 127],
                            c if c == ControllerNumbers_::kAfterTouch as u32 => [0xd0 | ch, x.value as u8 & 127, 0],
                            c if c == ControllerNumbers_::kPitchBend as u32 => [0xe0 | ch, x.value as u8 & 127, x.value2 as u8 & 127],
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
        self.ctx.projectTimeSamples += n as i64;
        self.ctx.continousTimeSamples += n as i64;

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
