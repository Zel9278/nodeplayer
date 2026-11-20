//! The audio-thread half of a VST3 instance.

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use super::host::{EventList, ParamChange, ParamChanges};
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf, Transport};
use crate::plugins::{AudioPort, AudioPorts};

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
    audio_ports: AudioPorts,
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
            audio_ports: AudioPorts {
                inputs: describe_channels(in_channels),
                outputs: describe_channels(out_channels),
            },
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

    pub(super) fn with_audio_ports(mut self, audio_ports: AudioPorts) -> Self {
        self.audio_ports = audio_ports;
        self
    }

    /// Compatibility helper for the command-line plugin checks. Only the main buses are
    /// connected; all auxiliary inputs are explicitly silent for every block.
    pub fn process_stereo(
        &mut self,
        input: [&[f32]; 2],
        output: [&mut [f32]; 2],
        midi_in: &[MidiEvent],
        midi_out: Option<&mut MidiBuf>,
    ) {
        let [out_l, out_r] = output;
        let n = out_l.len().min(out_r.len()).min(input[0].len()).min(input[1].len()).min(self.max_block);
        for bus in &mut self.ins.data {
            for channel in bus {
                channel[..n].fill(0.0);
            }
        }
        let main_input = self.audio_ports.inputs.iter().position(|port| port.main).unwrap_or(0);
        if let Some(bus) = self.ins.data.get_mut(main_input) {
            match bus.len() {
                0 => {}
                1 => {
                    for (index, sample) in bus[0][..n].iter_mut().enumerate() {
                        *sample = 0.5 * (input[0][index] + input[1][index]);
                    }
                }
                _ => {
                    bus[0][..n].copy_from_slice(&input[0][..n]);
                    bus[1][..n].copy_from_slice(&input[1][..n]);
                }
            }
        }
        let success = self.process_block(n, midi_in, midi_out);
        out_l.fill(0.0);
        out_r.fill(0.0);
        let main_output = self.audio_ports.outputs.iter().position(|port| port.main).unwrap_or(0);
        if success && let Some(bus) = self.outs.data.get(main_output) && !bus.is_empty() {
            let flags = self.outs.abb[main_output].silenceFlags;
            if flags & 1 == 0 {
                out_l[..n].copy_from_slice(&bus[0][..n]);
            }
            let channel = usize::from(bus.len() > 1);
            if flags & (1u64 << channel) == 0 {
                out_r[..n].copy_from_slice(&bus[channel][..n]);
            }
        }
    }

    fn run(
        &mut self,
        n: usize,
        inputs: &[StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: Option<&mut MidiBuf>,
    ) {
        let n = n.min(self.max_block);
        for (index, bus) in self.ins.data.iter_mut().enumerate() {
            for (channel, samples) in bus.iter_mut().enumerate() {
                if let Some(input) = inputs.get(index).filter(|input| channel < input.channels()) {
                    samples[..n].copy_from_slice(&input.channel(channel)[..n]);
                } else {
                    samples[..n].fill(0.0);
                }
            }
        }
        let success = self.process_block(n, midi_in, midi_out);
        for (index, output) in outputs.iter_mut().enumerate() {
            output.clear(n);
            if success && let Some(bus) = self.outs.data.get(index) {
                let flags = self.outs.abb[index].silenceFlags;
                for (channel, samples) in bus.iter().enumerate().take(output.channels()) {
                    if flags & (1u64 << channel) == 0 {
                        output.channel_mut(channel)[..n].copy_from_slice(&samples[..n]);
                    }
                }
            }
        }
    }

    /// Processes every bus together once. Buffers are allocated only during activation.
    fn process_block(&mut self, n: usize, midi_in: &[MidiEvent], midi_out: Option<&mut MidiBuf>) -> bool {
        if !self.started {
            self.started = unsafe { self.audio.setProcessing(1) } == kResultOk;
            if !self.started {
                return false;
            }
        }
        for buffer in &mut self.ins.abb {
            buffer.silenceFlags = 0;
        }
        for (bus, buffer) in self.outs.data.iter_mut().zip(&mut self.outs.abb) {
            buffer.silenceFlags = 0;
            for channel in bus {
                channel[..n].fill(0.0);
            }
        }
        self.in_changes.clear();
        self.out_changes.clear();
        while let Ok(ch) = self.from_ui.pop() {
            self.in_changes.push(ch);
        }
        if let Some(list) = &self.in_events { list.clear(); }
        if let Some(list) = &self.out_events { list.clear(); }
        self.queue_midi(midi_in);
        let events = |list: &Option<ComWrapper<EventList>>| {
            list.as_ref().map_or(std::ptr::null_mut(), |list| list.as_com_ref::<IEventList>().unwrap().as_ptr())
        };
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n as i32,
            numInputs: self.ins.abb.len() as i32,
            numOutputs: self.outs.abb.len() as i32,
            inputs: if self.ins.abb.is_empty() { std::ptr::null_mut() } else { self.ins.abb.as_mut_ptr() },
            outputs: if self.outs.abb.is_empty() { std::ptr::null_mut() } else { self.outs.abb.as_mut_ptr() },
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
        let result = unsafe { self.audio.process(&mut data) };
        self.ctx.continousTimeSamples = self.ctx.continousTimeSamples.saturating_add(n as i64);
        for change in self.out_changes.iter() {
            let _ = self.to_ui.push(change);
        }
        if let Some(out) = midi_out { self.take_midi(n, out); }
        result == kResultOk
    }

}

fn describe_channels(channels: &[usize]) -> Vec<AudioPort> {
    channels.iter().enumerate().map(|(index, &channels)| AudioPort {
        id: index.to_string(), name: format!("Audio {}", index + 1), channels,
        main: index == 0, layout: None,
    }).collect()
}

impl Processor for Vst3Processor {
    fn audio_ports(&self) -> Option<AudioPorts> { Some(self.audio_ports.clone()) }

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

    fn flush_params(&mut self) {
        // VST3 permits parameter-only process calls with zero samples and no buses.
        // Flush only the queue snapshot: concurrent UI edits cannot extend this work.
        // https://steinbergmedia.github.io/vst3_dev_portal/pages/FAQ/Processing.html
        let mut pending = self.from_ui.slots();
        while pending != 0 {
            self.in_changes.clear();
            self.out_changes.clear();
            let count = pending.min(MAX_CHANGED_PARAMS);
            for _ in 0..count {
                if let Ok(change) = self.from_ui.pop() {
                    self.in_changes.push(change);
                }
            }
            pending -= count;
            let mut data = ProcessData {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                numSamples: 0,
                numInputs: 0,
                numOutputs: 0,
                inputs: std::ptr::null_mut(),
                outputs: std::ptr::null_mut(),
                inputParameterChanges: self
                    .in_changes
                    .as_com_ref::<IParameterChanges>()
                    .unwrap()
                    .as_ptr(),
                outputParameterChanges: self
                    .out_changes
                    .as_com_ref::<IParameterChanges>()
                    .unwrap()
                    .as_ptr(),
                inputEvents: std::ptr::null_mut(),
                outputEvents: std::ptr::null_mut(),
                processContext: &mut *self.ctx,
            };
            unsafe { self.audio.process(&mut data) };
            for change in self.out_changes.iter() {
                let _ = self.to_ui.push(change);
            }
        }
    }
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.run(n, inputs, outputs, &[], None);
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.run(n, inputs, outputs, midi_in, Some(midi_out));
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
        audio_blocks: Mutex<Vec<Vec<Vec<Vec<f32>>>>>,
        processing: Mutex<Vec<bool>>,
        flushes: Mutex<Vec<Vec<(u32, f64)>>>,
        bus_descriptions: Mutex<Vec<(String, usize)>>,
    }
    struct Plugin(Arc<Capture>);
    impl vst3::Class for Plugin {
        type Interfaces = (IAudioProcessor, IComponent);
    }
    impl IPluginBaseTrait for Plugin {
        unsafe fn initialize(&self, _: *mut FUnknown) -> tresult { kResultOk }
        unsafe fn terminate(&self) -> tresult { kResultOk }
    }
    impl IComponentTrait for Plugin {
        unsafe fn getControllerClassId(&self, _: *mut TUID) -> tresult { kNotImplemented }
        unsafe fn setIoMode(&self, _: IoMode) -> tresult { kResultOk }
        unsafe fn getBusCount(&self, _: MediaType, _: BusDirection) -> i32 {
            self.0.bus_descriptions.lock().unwrap().len() as i32
        }
        unsafe fn getBusInfo(&self, _: MediaType, _: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
            let descriptions = self.0.bus_descriptions.lock().unwrap();
            let Some((name, channels)) = descriptions.get(index as usize) else { return kInvalidArgument; };
            unsafe {
                let bus = &mut *bus;
                bus.channelCount = *channels as i32;
                bus.busType = if index == 0 { BusTypes_::kMain as i32 } else { BusTypes_::kAux as i32 };
                for (target, source) in bus.name.iter_mut().zip(name.encode_utf16()) { *target = source; }
            }
            kResultOk
        }
        unsafe fn getRoutingInfo(&self, _: *mut RoutingInfo, _: *mut RoutingInfo) -> tresult { kNotImplemented }
        unsafe fn activateBus(&self, _: MediaType, _: BusDirection, _: i32, _: TBool) -> tresult { kResultOk }
        unsafe fn setActive(&self, _: TBool) -> tresult { kResultOk }
        unsafe fn setState(&self, _: *mut IBStream) -> tresult { kResultOk }
        unsafe fn getState(&self, _: *mut IBStream) -> tresult { kResultOk }
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
            index: i32,
            arrangement: *mut SpeakerArrangement,
        ) -> tresult {
            if let Some((_, channels)) = self.0.bus_descriptions.lock().unwrap().get(index as usize) {
                unsafe { *arrangement = (1u64 << (*channels).min(63)) - 1; }
            }
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
            let data = unsafe { &*data };
            let ctx = unsafe { &*data.processContext };
            self.0.blocks.lock().unwrap().push((
                ctx.state,
                ctx.projectTimeSamples,
                ctx.continousTimeSamples,
            ));
            if data.numSamples == 0 {
                assert_eq!((data.numInputs, data.numOutputs), (0, 0));
                assert!(data.inputs.is_null() && data.outputs.is_null());
                assert!(data.inputEvents.is_null() && data.outputEvents.is_null());
                let input = unsafe { vst3::ComRef::from_raw(data.inputParameterChanges) }.unwrap();
                let output =
                    unsafe { vst3::ComRef::from_raw(data.outputParameterChanges) }.unwrap();
                let mut changes = Vec::new();
                for index in 0..unsafe { input.getParameterCount() } {
                    let queue =
                        unsafe { vst3::ComRef::from_raw(input.getParameterData(index)) }.unwrap();
                    let id = unsafe { queue.getParameterId() };
                    let mut offset = -1;
                    let mut value = 0.0;
                    assert_eq!(
                        unsafe { queue.getPoint(0, &mut offset, &mut value) },
                        kResultOk
                    );
                    assert_eq!(offset, 0);
                    changes.push((id, value));
                    let echo = unsafe {
                        vst3::ComRef::from_raw(output.addParameterData(&id, std::ptr::null_mut()))
                    }
                    .unwrap();
                    assert_eq!(
                        unsafe { echo.addPoint(0, value, std::ptr::null_mut()) },
                        kResultOk
                    );
                }
                self.0.flushes.lock().unwrap().push(changes);
            } else {
                let n = data.numSamples as usize;
                let mut captured = Vec::new();
                for index in 0..data.numInputs as usize {
                    let bus = unsafe { &*data.inputs.add(index) };
                    let mut channels = Vec::new();
                    for channel in 0..bus.numChannels as usize {
                        let samples = unsafe { *bus.__field0.channelBuffers32.add(channel) };
                        channels.push(unsafe { std::slice::from_raw_parts(samples, n) }.to_vec());
                    }
                    captured.push(channels);
                }
                for index in 0..data.numOutputs as usize {
                    let bus = unsafe { &mut *data.outputs.add(index) };
                    for channel in 0..bus.numChannels as usize {
                        let target = unsafe { std::slice::from_raw_parts_mut(*bus.__field0.channelBuffers32.add(channel), n) };
                        if let Some(source) = captured.get(index).and_then(|bus| bus.get(channel)) {
                            target.copy_from_slice(source);
                        } else {
                            target.fill(0.0);
                        }
                    }
                }
                self.0.audio_blocks.lock().unwrap().push(captured);
            }
            kResultOk
        }
        unsafe fn getTailSamples(&self) -> u32 {
            0
        }
    }

    #[test]
    fn native_bus_descriptors_keep_layouts_and_named_ids_across_reordering() {
        let capture = Arc::new(Capture::default());
        *capture.bus_descriptions.lock().unwrap() = vec![
            ("Program".into(), 6), ("Detector".into(), 1), ("Monitor".into(), 2),
        ];
        let plugin = ComWrapper::new(Plugin(capture.clone()));
        let component = plugin.to_com_ptr::<IComponent>().unwrap();
        let audio = plugin.to_com_ptr::<IAudioProcessor>().unwrap();
        let ports = super::super::read_audio_ports(&component, &audio, BusDirections_::kInput as i32).unwrap();
        assert_eq!(ports.iter().map(|port| port.channels).collect::<Vec<_>>(), [6, 1, 2]);
        assert!(ports[0].main && !ports[1].main);
        assert_eq!(ports[0].layout.as_deref(), Some("0x3f"));
        capture.bus_descriptions.lock().unwrap().swap(1, 2);
        let reordered = super::super::read_audio_ports(&component, &audio, BusDirections_::kInput as i32).unwrap();
        assert_eq!(ports[1].id, reordered[2].id);
        assert_eq!(ports[2].id, reordered[1].id);
        *capture.bus_descriptions.lock().unwrap() = vec![("Too wide".into(), 65)];
        assert!(super::super::read_audio_ports(&component, &audio, BusDirections_::kInput as i32).is_err());
        *capture.bus_descriptions.lock().unwrap() = vec![("Too many".into(), 1); 33];
        assert!(super::super::read_audio_ports(&component, &audio, BusDirections_::kInput as i32).is_err());
        capture.bus_descriptions.lock().unwrap().clear();
        assert!(super::super::read_audio_ports(&component, &audio, BusDirections_::kInput as i32).unwrap().is_empty());
    }

    #[test]
    fn every_bus_preserves_mono_and_surround_channels_and_missing_inputs_are_silent() {
        let capture = Arc::new(Capture::default());
        let audio = ComWrapper::new(Plugin(capture.clone())).to_com_ptr::<IAudioProcessor>().unwrap();
        let (_tx, from_ui) = rtrb::RingBuffer::new(1);
        let (to_ui, _rx) = rtrb::RingBuffer::new(1);
        let widths = [2, 1, 6];
        let mut processor = Vst3Processor::new(audio, from_ui, to_ui, &widths, &widths,
            (false, false), CcMap::new(None), 48_000.0, 64);
        let mut inputs: Vec<_> = widths.iter().map(|&channels| StereoBuf::with_channels(channels)).collect();
        let mut outputs: Vec<_> = widths.iter().map(|&channels| StereoBuf::with_channels(channels)).collect();
        for (bus, input) in inputs.iter_mut().enumerate() {
            for channel in 0..input.channels() {
                input.channel_mut(channel)[..64].fill((bus * 10 + channel + 1) as f32);
            }
        }
        processor.process(64, &mut inputs, &mut outputs);
        assert_eq!(capture.audio_blocks.lock().unwrap().len(), 1, "all buses share one process call");
        for (bus, output) in outputs.iter().enumerate() {
            for channel in 0..output.channels() {
                assert_eq!(&output.channel(channel)[..64], vec![(bus * 10 + channel + 1) as f32; 64]);
            }
        }
        processor.process(64, &mut inputs[..1], &mut outputs);
        for output in &outputs[1..] {
            for channel in 0..output.channels() {
                assert!(output.channel(channel)[..64].iter().all(|&sample| sample == 0.0));
            }
        }
        processor.process_stereo([&[3.0; 64], &[5.0; 64]], [&mut [0.0; 64], &mut [0.0; 64]], &[], None);
        let captured = capture.audio_blocks.lock().unwrap();
        for bus in &captured.last().unwrap()[1..] {
            assert!(bus.iter().flatten().all(|&sample| sample == 0.0), "legacy helper silences auxiliary buses");
        }
    }

    #[test]
    fn zero_bus_plugins_still_process_without_graph_audio_ports() {
        let capture = Arc::new(Capture::default());
        let audio = ComWrapper::new(Plugin(capture.clone())).to_com_ptr::<IAudioProcessor>().unwrap();
        let configurations: [(&[usize], &[usize]); 3] = [(&[], &[]), (&[], &[6]), (&[1], &[])];
        for (in_widths, out_widths) in configurations {
            let (_tx, from_ui) = rtrb::RingBuffer::new(1);
            let (to_ui, _rx) = rtrb::RingBuffer::new(1);
            let mut processor = Vst3Processor::new(audio.clone(), from_ui, to_ui, in_widths, out_widths,
                (false, false), CcMap::new(None), 48_000.0, 64);
            let mut inputs: Vec<_> = in_widths.iter().map(|&channels| StereoBuf::with_channels(channels)).collect();
            let mut outputs: Vec<_> = out_widths.iter().map(|&channels| StereoBuf::with_channels(channels)).collect();
            processor.process(64, &mut inputs, &mut outputs);
            let ports = processor.audio_ports().unwrap();
            assert_eq!((ports.inputs.len(), ports.outputs.len()), (in_widths.len(), out_widths.len()));
        }
        assert_eq!(capture.audio_blocks.lock().unwrap().len(), 3);
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

    #[test]
    fn restart_flush_delivers_all_pending_edits_without_audio_or_clock_advance() {
        for started in [false, true] {
            let capture = Arc::new(Capture::default());
            let audio = ComWrapper::new(Plugin(capture.clone()))
                .to_com_ptr::<IAudioProcessor>()
                .unwrap();
            let count = MAX_CHANGED_PARAMS + 17;
            let (mut tx, from_ui) = rtrb::RingBuffer::new(count);
            let (to_ui, mut rx) = rtrb::RingBuffer::new(count);
            let mut proc = Vst3Processor::new(
                audio,
                from_ui,
                to_ui,
                &[2],
                &[2],
                (true, true),
                CcMap::new(None),
                48_000.0,
                64,
            );
            proc.ctx.projectTimeSamples = 60_000;
            proc.ctx.continousTimeSamples = 4096;
            if started {
                proc.process(64, &mut [StereoBuf::new()], &mut [StereoBuf::new()]);
            }
            let before = (proc.ctx.projectTimeSamples, proc.ctx.continousTimeSamples);
            proc.flush_params();
            assert!(capture.flushes.lock().unwrap().is_empty());
            for id in 0..count as u32 {
                tx.push(ParamChange {
                    id,
                    value: id as f64 / count as f64,
                })
                .unwrap();
            }
            proc.flush_params();
            assert_eq!(
                (proc.ctx.projectTimeSamples, proc.ctx.continousTimeSamples),
                before
            );
            assert_eq!(proc.started, started);
            let expected: Vec<_> = (0..count as u32)
                .map(|id| (id, id as f64 / count as f64))
                .collect();
            let flushes = capture.flushes.lock().unwrap();
            assert_eq!(
                flushes.iter().flatten().copied().collect::<Vec<_>>(),
                expected
            );
            assert!(
                flushes
                    .iter()
                    .all(|batch| batch.len() <= MAX_CHANGED_PARAMS)
            );
            drop(flushes);
            let mut echoed = Vec::new();
            while let Ok(change) = rx.pop() {
                echoed.push((change.id, change.value));
            }
            assert_eq!(echoed, expected);
            proc.deactivate();
            let expected_lifecycle = if started { vec![true, false] } else { vec![] };
            assert_eq!(*capture.processing.lock().unwrap(), expected_lifecycle);
            // A replacement processor receives new queues after the old queue was drained.
            assert!(proc.from_ui.pop().is_err());
            let calls = capture.blocks.lock().unwrap().len();
            proc.flush_params();
            assert_eq!(capture.blocks.lock().unwrap().len(), calls);
        }
    }

}
