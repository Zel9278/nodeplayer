//! CSynth node: [csynth](https://git.c30.life/ced/csynth-v2) (the MIDI apps' piano) as a MIDI
//! instrument in the graph. It plays the MPP piano samples (found locally or downloaded and
//! cached, as the apps do) or a SoundFont.
//!
//! Sounds load on a background thread; the finished `PianoProcessor` is handed to the audio
//! thread through a slot it polls with `try_lock`, and the one it replaces comes back through
//! another for the UI thread to free.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use csynth::{PianoProcessor, QUANTUM, SampleSet};
use serde::{Deserialize, Serialize};

use crate::engine::nodes::db_to_gain;
use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};

/// Where the apps get the MPP piano samples from.
const SERVER: &str = "wss://mpp.c30.life";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CSynthSettings {
    /// A SoundFont to play; `None` for the MPP piano samples.
    pub font: Option<PathBuf>,
    pub volume_db: f32,
    pub voices: usize,
    /// The shortest release, as the apps' "performance mode".
    pub performance: bool,
    /// The apps' output limiter (-0.5 dB).
    pub limiter: bool,
}

impl Default for CSynthSettings {
    fn default() -> Self {
        Self { font: None, volume_db: 0.0, voices: csynth::DEFAULT_VOICES, performance: false, limiter: true }
    }
}

impl CSynthSettings {
    pub const VOLUME: usize = 0;
    pub const LIMITER: usize = 1;
}

enum Load {
    Loading(String),
    Ready(String),
    Failed(String),
}

#[derive(Default)]
struct Shared {
    /// A new processor for the audio thread to take.
    next: Mutex<Option<Box<PianoProcessor>>>,
    /// The processor it replaced, to be freed off the audio thread.
    old: Mutex<Option<Box<PianoProcessor>>>,
    load: Mutex<Option<Load>>,
    /// Voices sounding, as the audio thread last saw them.
    active: AtomicU32,
    /// Bumped per load so a slow one can't replace a newer one.
    generation: AtomicU32,
}

/// Decoded MPP samples by output rate: decoding takes a while and a lot of memory, so nodes
/// share them.
fn sample_cache() -> &'static Mutex<HashMap<u32, SampleSet>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, SampleSet>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn mpp_samples(rate: u32, status: &(dyn Fn(String) + Sync)) -> Result<SampleSet, String> {
    if let Some(s) = sample_cache().lock().unwrap().get(&rate) {
        return Ok(s.clone());
    }
    let cache = csynth::cache_path("nodeplayer");
    let bundle = csynth::find_bundle(SERVER, cache.as_deref(), status)?;
    status("Decoding piano samples…".into());
    let set = csynth::decode_bundle(&bundle, rate, &|done, all| {
        if done % 16 == 0 {
            status(format!("Decoding piano samples… {done}/{all}"));
        }
    })?;
    sample_cache().lock().unwrap().insert(rate, set.clone());
    Ok(set)
}

pub struct CSynthNode {
    pub settings: CSynthSettings,
    shared: Arc<Shared>,
    rate: u32,
}

impl CSynthNode {
    pub fn new(settings: CSynthSettings, sample_rate: f32) -> Self {
        let mut me = Self { settings, shared: Arc::default(), rate: sample_rate as u32 };
        me.load();
        me
    }

    pub fn processor(&self) -> CSynthProc {
        CSynthProc {
            piano: None,
            limiter: csynth::output_limiter(self.rate),
            use_limiter: self.settings.limiter,
            gain: db_to_gain(self.settings.volume_db),
            shared: self.shared.clone(),
            buf: [[0.0; QUANTUM]; 2],
            interleaved: [0.0; QUANTUM * 2],
            buffered: 0,
            played: 0,
            channels: [ChannelKeys::default(); 16],
            key_down: [0; 128],
        }
    }

    /// (Re)loads the sound in the background; the audio thread switches over when it's ready.
    fn load(&mut self) {
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let shared = self.shared.clone();
        let (font, rate) = (self.settings.font.clone(), self.rate);
        let (voices, performance) = (self.settings.voices, self.settings.performance);
        let name = font.as_deref().map_or("MPP piano".into(), |f| f.file_name().unwrap_or_default().to_string_lossy().into_owned());
        *shared.load.lock().unwrap() = Some(Load::Loading(format!("Loading {name}…")));
        let spawned = std::thread::Builder::new().name("csynth load".into()).spawn(move || {
            let status = |s: String| {
                if shared.generation.load(Ordering::Acquire) == generation {
                    *shared.load.lock().unwrap() = Some(Load::Loading(s));
                }
            };
            let piano = match &font {
                Some(path) => csynth::Font::load(path).map(|f| PianoProcessor::with_fonts(vec![f], rate)),
                None => mpp_samples(rate, &status).map(|s| PianoProcessor::new(s, rate)),
            };
            if shared.generation.load(Ordering::Acquire) != generation {
                return;
            }
            match piano {
                Ok(mut p) => {
                    p.set_voice_limit(voices);
                    p.set_performance_mode(performance);
                    // Whatever wasn't picked up yet is simply replaced.
                    *shared.next.lock().unwrap() = Some(Box::new(p));
                    *shared.load.lock().unwrap() = Some(Load::Ready(name));
                }
                Err(e) => {
                    log::warn!("csynth: {name}: {e}");
                    *shared.load.lock().unwrap() = Some(Load::Failed(format!("{name}: {e}")));
                }
            }
        });
        if let Err(e) = spawned {
            *self.shared.load.lock().unwrap() = Some(Load::Failed(e.to_string()));
        }
    }

    /// Frees what the audio thread let go of.
    pub fn idle(&mut self) {
        drop(self.shared.old.lock().unwrap().take());
    }

    /// The body; returns parameter changes for the processor.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<(usize, f32)> {
        self.idle();
        let mut sends = Vec::new();
        let mut reload = false;
        ui.horizontal(|ui| {
            ui.label("sound");
            let text = match &self.settings.font {
                Some(f) => f.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                None => "MPP piano".into(),
            };
            egui::ComboBox::from_id_salt(ui.id().with("csynth-sound")).width(170.0).selected_text(text).show_ui(ui, |ui| {
                if ui.selectable_label(self.settings.font.is_none(), "MPP piano").clicked() && self.settings.font.is_some() {
                    self.settings.font = None;
                    reload = true;
                }
                if ui.button("SoundFont (.sf2)…").clicked() {
                    let mut dlg = rfd::FileDialog::new().add_filter("SoundFont", &["sf2", "SF2"]);
                    if let Some(dir) = self.settings.font.as_deref().and_then(Path::parent) {
                        dlg = dlg.set_directory(dir);
                    }
                    if let Some(p) = dlg.pick_file() {
                        self.settings.font = Some(p);
                        reload = true;
                    }
                    ui.close();
                }
            });
        });
        if ui.add(egui::Slider::new(&mut self.settings.volume_db, -40.0..=12.0).suffix(" dB").text("volume")).changed() {
            sends.push((CSynthSettings::VOLUME, self.settings.volume_db));
        }
        let voices = ui.add(egui::Slider::new(&mut self.settings.voices, 16..=csynth::MAX_VOICES).logarithmic(true).text("voices"));
        reload |= voices.drag_stopped() || (voices.changed() && !voices.dragged());
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.settings.performance, "short release").on_hover_text("the apps' performance mode").changed() {
                reload = true;
            }
            if ui.checkbox(&mut self.settings.limiter, "limiter").changed() {
                sends.push((CSynthSettings::LIMITER, self.settings.limiter as u8 as f32));
            }
        });
        match &*self.shared.load.lock().unwrap() {
            Some(Load::Loading(s)) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small(s);
                });
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            Some(Load::Ready(name)) => {
                ui.small(format!("{name} · {} voices sounding", self.shared.active.load(Ordering::Relaxed)));
            }
            Some(Load::Failed(e)) => {
                ui.colored_label(egui::Color32::from_rgb(230, 120, 90), e);
            }
            None => {}
        }
        if reload {
            self.load();
        }
        sends
    }
}

/// A channel's notes and sustain pedal (csynth itself has no pedal).
#[derive(Clone, Copy)]
struct ChannelKeys {
    pedal: bool,
    /// Notes started per key and not ended yet: several tracks often share a channel and
    /// strike the same key, and the key is let go when the last of them ends.
    started: [u16; 128],
    /// Of those ended, the ones the pedal still holds.
    pedal_held: [u16; 128],
}

impl Default for ChannelKeys {
    fn default() -> Self {
        Self { pedal: false, started: [0; 128], pedal_held: [0; 128] }
    }
}

/// Which note-ons a block keeps when there are more than the voices can sound (black MIDI):
/// the loudest, from a histogram of their velocities. Those above `cut` are kept, and of
/// those at `cut` all but the first `drop_at_cut`. From midi-blocks-rs (`synth::audio`).
struct Thinning {
    cut: usize,
    drop_at_cut: usize,
}

impl Thinning {
    fn new(histogram: &[usize; 128], limit: usize) -> Self {
        if histogram.iter().sum::<usize>() <= limit {
            return Self { cut: 0, drop_at_cut: 0 };
        }
        let mut kept = 0;
        let mut cut = 0;
        for v in (1..128).rev() {
            if kept + histogram[v] > limit {
                cut = v;
                break;
            }
            kept += histogram[v];
        }
        Self { cut, drop_at_cut: histogram[cut] - (limit - kept) }
    }

    /// Whether the next note-on, of `velocity`, is kept (they are asked in order).
    fn keeps(&mut self, velocity: usize) -> bool {
        if velocity < self.cut {
            return false;
        }
        if velocity == self.cut && self.drop_at_cut > 0 {
            self.drop_at_cut -= 1;
            return false;
        }
        true
    }
}

pub struct CSynthProc {
    piano: Option<Box<PianoProcessor>>,
    limiter: csynth::limiter::Limiter,
    use_limiter: bool,
    gain: f32,
    shared: Arc<Shared>,
    /// The last rendered quantum, of which the last `buffered` frames aren't out yet.
    buf: [[f32; QUANTUM]; 2],
    interleaved: [f32; QUANTUM * 2],
    buffered: usize,
    /// Frames handed to the graph so far.
    played: u64,
    channels: [ChannelKeys; 16],
    /// Notes started per key over all channels: the MPP piano has one voice a key whatever
    /// the channel, so its key is let go when no channel holds it.
    key_down: [u16; 128],
}

impl CSynthProc {
    fn take_new_piano(&mut self) {
        let Ok(mut next) = self.shared.next.try_lock() else { return };
        if next.is_none() {
            return;
        }
        let Ok(mut old) = self.shared.old.try_lock() else { return };
        if old.is_some() {
            // The UI hasn't freed the previous one yet; try again next block.
            return;
        }
        let mut piano = next.take().unwrap();
        // Continue on the stream's clock (what has been rendered so far).
        piano.frame = self.played + self.buffered as u64;
        *old = self.piano.replace(piano);
    }

    /// Schedules the block's MIDI on csynth's clock, one quantum late so every event is still
    /// in the future there and keeps its frame. As the MIDI apps play files
    /// (midi-blocks-rs `CSynthEngine::play_midi`): note-ons too quiet to hear are left out, and
    /// of the rest only the loudest the voices can take, so black MIDI costs no more than a
    /// look at each event; a key ends when the last note holding it does; the sustain pedal
    /// holds the ends.
    fn schedule(&mut self, midi: &[MidiEvent]) {
        let Some(p) = self.piano.as_deref_mut() else { return };
        let min_velocity = (csynth::MIN_VOLUME * 127.0).floor() as u8;
        let mut histogram = [0usize; 128];
        for e in midi {
            let [s, _, v] = e.data;
            if s & 0xf0 == 0x90 && v > min_velocity {
                histogram[v as usize] += 1;
            }
        }
        let mut thinning = Thinning::new(&histogram, csynth::MAX_VOICES);
        let fonts = p.plays_fonts();
        for e in midi {
            let t = (self.played + e.time as u64 + QUANTUM as u64) as f64;
            let [s, a, b] = e.data;
            let ch = e.channel();
            let key = a as usize & 127;
            let keys = &mut self.channels[ch as usize];
            // A file's note ends on its channel's key; the MPP piano's key once no channel
            // holds it.
            let key_up = |p: &mut PianoProcessor, keys: &ChannelKeys, key_down: &mut [u16; 128], note: usize| {
                key_down[note] = key_down[note].saturating_sub(1);
                if if fonts { keys.started[note] == 0 } else { key_down[note] == 0 } {
                    p.release_on_channel(t, note as u8, 0, ch);
                }
            };
            match s & 0xf0 {
                0x90 if b > 0 => {
                    if b <= min_velocity || !thinning.keeps(b as usize) {
                        continue;
                    }
                    keys.started[key] = keys.started[key].saturating_add(1);
                    self.key_down[key] = self.key_down[key].saturating_add(1);
                    p.press_on_channel(t, a, csynth::velocity_gain(b as f32 / 127.0), 0, ch);
                }
                0x80 | 0x90 => {
                    // notes left out above have nothing to end
                    if keys.started[key] == 0 {
                        continue;
                    }
                    keys.started[key] -= 1;
                    if keys.pedal {
                        keys.pedal_held[key] += 1;
                    } else {
                        let keys = *keys;
                        key_up(p, &keys, &mut self.key_down, key);
                    }
                }
                0xb0 if a == 64 || a == 121 => {
                    let down = a == 64 && b >= 64;
                    if keys.pedal && !down {
                        let held = std::mem::replace(&mut keys.pedal_held, [0; 128]);
                        let keys = *keys;
                        for (note, &n) in held.iter().enumerate() {
                            for _ in 0..n {
                                key_up(p, &keys, &mut self.key_down, note);
                            }
                        }
                    }
                    self.channels[ch as usize].pedal = down;
                    if a == 121 {
                        p.control(t, ch, a, b);
                    }
                }
                0xb0 => {
                    if matches!(a, 120 | 123) {
                        // everything on the channel lets go
                        for note in 0..128 {
                            let n = keys.started[note] + keys.pedal_held[note];
                            self.key_down[note] = self.key_down[note].saturating_sub(n);
                        }
                        keys.started = [0; 128];
                        keys.pedal_held = [0; 128];
                    }
                    p.control(t, ch, a, b);
                }
                0xc0 => p.program(t, ch, a),
                0xe0 => p.bend(t, ch, (b as u16) << 7 | a as u16),
                _ => {}
            }
        }
    }

    fn render_quantum(&mut self) {
        let [l, r] = &mut self.buf;
        match self.piano.as_deref_mut() {
            Some(p) => {
                p.process(l, r);
            }
            None => {
                l.fill(0.0);
                r.fill(0.0);
            }
        }
        if self.use_limiter {
            for (k, (a, b)) in l.iter().zip(r.iter()).enumerate() {
                self.interleaved[2 * k] = *a;
                self.interleaved[2 * k + 1] = *b;
            }
            self.limiter.process(&mut self.interleaved);
            for k in 0..QUANTUM {
                l[k] = self.interleaved[2 * k];
                r[k] = self.interleaved[2 * k + 1];
            }
        }
        self.buffered = QUANTUM;
    }
}

impl Processor for CSynthProc {
    fn process(&mut self, n: usize, _inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.run(n, outputs, &[]);
    }

    fn process_midi(
        &mut self,
        n: usize,
        _inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        _midi_out: &mut MidiBuf,
    ) {
        self.run(n, outputs, midi_in);
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match index {
            CSynthSettings::VOLUME => self.gain = db_to_gain(value),
            CSynthSettings::LIMITER => self.use_limiter = value > 0.5,
            _ => {}
        }
    }
}

impl CSynthProc {
    fn run(&mut self, n: usize, outputs: &mut [StereoBuf], midi_in: &[MidiEvent]) {
        self.take_new_piano();
        self.schedule(midi_in);
        let o = &mut outputs[0];
        let mut done = 0;
        while done < n {
            if self.buffered == 0 {
                self.render_quantum();
            }
            let from = QUANTUM - self.buffered;
            let take = self.buffered.min(n - done);
            for k in 0..take {
                o.l[done + k] = self.buf[0][from + k] * self.gain;
                o.r[done + k] = self.buf[1][from + k] * self.gain;
            }
            self.buffered -= take;
            done += take;
        }
        self.played += n as u64;
        if let Some(p) = &self.piano {
            self.shared.active.store(p.stats.active, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_set(len: usize) -> SampleSet {
        let mut set: SampleSet = vec![None; 128];
        set[60] = Some(Arc::new(csynth::Sample::new(vec![vec![0.5; len], vec![0.5; len]])));
        set
    }

    /// Black-MIDI-like load: `per_block` note-ons and their note-offs every 10 ms block, on
    /// many channels and keys. `cargo test --release csynth_dense -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn csynth_dense_load() {
        let shared = Arc::new(Shared::default());
        let mut set: SampleSet = vec![None; 128];
        for k in 21..109 {
            set[k] = Some(Arc::new(csynth::Sample::new(vec![vec![0.3; 96_000], vec![0.3; 96_000]])));
        }
        *shared.next.lock().unwrap() = Some(Box::new(PianoProcessor::new(set, 48_000)));
        let node = CSynthNode { settings: CSynthSettings::default(), shared, rate: 48_000 };
        let mut p = node.processor();
        let (mut ins, mut outs) = ([], [StereoBuf::new()]);
        let mut out = MidiBuf::new();
        for per_block in [100usize, 500, 2000, 8000] {
            let mut midi = Vec::new();
            let start = std::time::Instant::now();
            let blocks = 300;
            for b in 0..blocks {
                midi.clear();
                for i in 0..per_block {
                    let (ch, key) = ((i % 16) as u8, (21 + (i * 7 + b) % 88) as u8);
                    let t = (i * 480 / per_block) as u32;
                    if let Some(e) = MidiEvent::new(t, &[0x90 | ch, key, 40 + (i % 80) as u8]) {
                        midi.push(e);
                    }
                    // the note started 20 blocks ago ends: notes last 200 ms
                    let old = (21 + (i * 7 + b + blocks - 20) % 88) as u8;
                    if b >= 20
                        && let Some(e) = MidiEvent::new(t, &[0x80 | ch, old, 0])
                    {
                        midi.push(e);
                    }
                }
                midi.truncate(MidiBuf::CAPACITY);
                p.process_midi(480, &mut ins, &mut outs, &midi, &mut out);
            }
            let secs = start.elapsed().as_secs_f64();
            println!("{per_block} note-ons a block ({} k/s): {:.2} s for 3 s of audio ({:.0}% of real time)", per_block * 100 / 1000, secs, secs / 3.0 * 100.0);
        }
    }

    #[test]
    fn plays_notes_at_their_frame_and_holds_with_the_pedal() {
        let shared = Arc::new(Shared::default());
        *shared.next.lock().unwrap() = Some(Box::new(PianoProcessor::new(tone_set(48_000), 48_000)));
        let node = CSynthNode { settings: CSynthSettings { limiter: false, ..Default::default() }, shared, rate: 48_000 };
        let mut p = node.processor();
        let (mut ins, mut outs) = ([], [StereoBuf::new()]);
        let mut out = MidiBuf::new();
        let mut block = |p: &mut CSynthProc, midi: &[MidiEvent]| {
            p.process_midi(300, &mut ins, &mut outs, midi, &mut out);
            outs[0].l[..300].to_vec()
        };
        let ev = |t, d: &[u8]| MidiEvent::new(t, d).unwrap();
        assert!(block(&mut p, &[]).iter().all(|x| *x == 0.0));
        // A note at frame 100 starts one quantum later (its first frame is the fade-in's 0).
        let a = block(&mut p, &[ev(100, &[0x90, 60, 127])]);
        let b = block(&mut p, &[]);
        let first = a.iter().chain(&b).position(|x| *x != 0.0).unwrap();
        assert!((100 + QUANTUM..=100 + QUANTUM + 1).contains(&first), "started at {first}");
        // With the pedal down, a note off doesn't stop it; lifting the pedal does.
        block(&mut p, &[ev(0, &[0xb0, 64, 127]), ev(10, &[0x80, 60, 0])]);
        let held: Vec<f32> = (0..20).flat_map(|_| block(&mut p, &[])).collect();
        assert!(held[held.len() - 300..].iter().any(|x| x.abs() > 0.01), "pedal should hold the note");
        block(&mut p, &[ev(0, &[0xb0, 64, 0])]);
        let after: Vec<f32> = (0..80).flat_map(|_| block(&mut p, &[])).collect();
        assert!(after[after.len() - 300..].iter().all(|x| x.abs() < 1e-4), "released after the pedal");
        assert!(node.shared.old.lock().unwrap().is_none());
    }
}
