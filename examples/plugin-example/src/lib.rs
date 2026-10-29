//! An example nodeplayer plugin:
//! - **Bitcrusher**: a node with parameters only; nodeplayer draws sliders for it.
//! - **Stereo Width**: a node with its own UI (a meter you can drag to set the width).
//! - **Transpose**: a MIDI effect (MIDI in, MIDI out).
//! - **Toolbox**: an extension with a side panel (now playing, sleep timer, history) and
//!   menu items (random track, sleep in 30 minutes).
//!
//! Build with `cargo build --release -p np-example` and copy `target/release/libnp_example.so`
//! to `~/.local/share/nodeplayer/plugins/`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use nodeplayer_plugin::abi::{MIDI_IN, MIDI_OUT};
use nodeplayer_plugin::{
    Align, Color, Event, Extension, Host, MenuId, MidiEvent, MidiOut, Node, Param, Params, Ui, export_plugin,
};

// ---------------------------------------------------------------------------
// Bitcrusher

struct Bitcrusher {
    hold: [f32; 2],
    counter: u32,
}

const BITS: u32 = 0;
const RATE: u32 = 1;
const MIX: u32 = 2;

impl Node for Bitcrusher {
    const ID: &'static str = "bitcrusher";
    const NAME: &'static str = "Bitcrusher";
    const CATEGORY: &'static str = "Fx|Distortion";
    type Shared = ();

    fn params() -> Vec<Param> {
        vec![
            Param::new(BITS, "Bits", 1.0, 16.0, 8.0).steps(15),
            Param::new(RATE, "Downsample", 1.0, 32.0, 4.0).steps(31).unit("×"),
            Param::new(MIX, "Mix", 0.0, 1.0, 1.0),
        ]
    }

    fn new(_sample_rate: f32, _max_block: usize) -> Self {
        Self { hold: [0.0; 2], counter: 0 }
    }

    fn process(&mut self, input: [&[f32]; 2], output: [&mut [f32]; 2], params: &Params, _: &()) {
        let levels = 2f32.powf(params.get(BITS) as f32 - 1.0);
        let rate = params.get(RATE).round().max(1.0) as u32;
        let mix = params.get(MIX) as f32;
        let [out_l, out_r] = output;
        for k in 0..out_l.len() {
            if self.counter == 0 {
                self.hold = [input[0][k], input[1][k]].map(|x| (x * levels).round() / levels);
            }
            self.counter = (self.counter + 1) % rate;
            out_l[k] = input[0][k] + (self.hold[0] - input[0][k]) * mix;
            out_r[k] = input[1][k] + (self.hold[1] - input[1][k]) * mix;
        }
    }
}

// ---------------------------------------------------------------------------
// Stereo width with its own UI

#[derive(Default)]
struct Levels {
    /// Mid and side RMS of the last block, as f32 bits.
    mid: AtomicU32,
    side: AtomicU32,
}

struct Width;

const WIDTH: u32 = 0;

impl Node for Width {
    const ID: &'static str = "width";
    const NAME: &'static str = "Stereo Width";
    const CATEGORY: &'static str = "Fx|Stereo";
    const HAS_UI: bool = true;
    type Shared = Levels;

    fn params() -> Vec<Param> {
        vec![Param::new(WIDTH, "Width", 0.0, 2.0, 1.0)]
    }

    fn new(_sample_rate: f32, _max_block: usize) -> Self {
        Width
    }

    fn process(&mut self, input: [&[f32]; 2], output: [&mut [f32]; 2], params: &Params, levels: &Levels) {
        let w = params.get(WIDTH) as f32;
        let [out_l, out_r] = output;
        let (mut m2, mut s2) = (0.0f32, 0.0f32);
        for k in 0..out_l.len() {
            let mid = (input[0][k] + input[1][k]) * 0.5;
            let side = (input[0][k] - input[1][k]) * 0.5 * w;
            out_l[k] = mid + side;
            out_r[k] = mid - side;
            m2 += mid * mid;
            s2 += side * side;
        }
        let n = out_l.len().max(1) as f32;
        levels.mid.store((m2 / n).sqrt().to_bits(), Ordering::Relaxed);
        levels.side.store((s2 / n).sqrt().to_bits(), Ordering::Relaxed);
    }

    fn ui(ui: &mut Ui, params: &Params, levels: &Levels) {
        let w = params.get(WIDTH);
        let c = ui.canvas(220.0, 60.0);
        ui.rect(c.x, c.y, c.x + c.w, c.y + c.h, Color::rgb(20, 22, 28));
        // Mid and side meters, in dB from -60.
        let bar = |v: f32| ((20.0 * v.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
        let mid = bar(f32::from_bits(levels.mid.load(Ordering::Relaxed)));
        let side = bar(f32::from_bits(levels.side.load(Ordering::Relaxed)));
        ui.rect(c.x + 4.0, c.y + 6.0, c.x + 4.0 + mid * (c.w - 8.0), c.y + 16.0, Color::BLUE);
        ui.rect(c.x + 4.0, c.y + 20.0, c.x + 4.0 + side * (c.w - 8.0), c.y + 30.0, Color::rgb(255, 160, 90));
        ui.text(c.x + 6.0, c.y + 11.0, 9.0, Color::WHITE, Align::Left, "mid");
        ui.text(c.x + 6.0, c.y + 25.0, 9.0, Color::WHITE, Align::Left, "side");
        // The width handle: drag it, or use the mouse wheel.
        let x = c.x + 4.0 + (w / 2.0) as f32 * (c.w - 8.0);
        ui.line(c.x + 4.0, c.y + 46.0, c.x + c.w - 4.0, c.y + 46.0, 1.0, Color::GRAY);
        ui.circle(x, c.y + 46.0, if c.hovered { 7.0 } else { 6.0 }, Color::YELLOW);
        ui.text(c.x + c.w - 6.0, c.y + 46.0, 11.0, Color::WHITE, Align::Right, &format!("{:.0}%", w * 100.0));
        if c.dragged {
            params.set(WIDTH, w + (c.drag_dx / (c.w - 8.0) * 2.0) as f64);
        } else if c.scroll_y != 0.0 {
            params.set(WIDTH, w + c.scroll_y as f64 / 500.0);
        }
        if c.double_clicked {
            params.set(WIDTH, 1.0);
        }
        // Keep the meters moving.
        ui.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Toolbox extension

struct Toolbox {
    host: Host,
    random: MenuId,
    sleep_30: MenuId,
    sleep_minutes: f64,
    sleep_at: Option<Instant>,
    history: Vec<(String, String)>,
    rng: u64,
}

impl Toolbox {
    fn start_sleep(&mut self, minutes: f64) {
        self.sleep_at = Some(Instant::now() + Duration::from_secs_f64(minutes * 60.0));
        self.host.log(&format!("sleep timer: {minutes:.0} min"));
    }

    fn random_track(&mut self) {
        let n = self.host.status().library_len;
        if n == 0 {
            self.host.warn("the library is empty");
            return;
        }
        // xorshift; good enough for picking a song.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.host.play_track((self.rng % n as u64) as usize);
    }
}

impl Extension for Toolbox {
    const PANEL: Option<&'static str> = Some("Toolbox");

    fn new(host: Host) -> Self {
        let random = host.add_menu_item("Play a random track");
        let sleep_30 = host.add_menu_item("Sleep in 30 minutes");
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Self { host, random, sleep_30, sleep_minutes: 30.0, sleep_at: None, history: Vec::new(), rng: seed | 1 }
    }

    fn idle(&mut self) {
        if let Some(at) = self.sleep_at
            && Instant::now() >= at
        {
            self.sleep_at = None;
            self.host.pause();
            self.host.log("sleep timer: paused");
        }
    }

    fn event(&mut self, event: Event) {
        if event == Event::TrackChanged
            && let Some(path) = self.host.track_path()
        {
            let title = self.host.track_title().unwrap_or_default();
            let line = match self.host.track_artist() {
                Some(a) => format!("{a} — {title}"),
                None => title,
            };
            self.history.retain(|(p, _)| *p != path);
            self.history.insert(0, (path, line));
            self.history.truncate(20);
        }
    }

    fn panel(&mut self, ui: &mut Ui) {
        let s = self.host.status();
        match self.host.track_title() {
            Some(t) => {
                ui.label(&t);
                if let Some(a) = self.host.track_artist() {
                    ui.weak(&a);
                }
                let frac = if s.duration > 0.0 { (s.position / s.duration) as f32 } else { 0.0 };
                ui.progress(frac, &format!("{:.0}:{:02.0}", (s.position / 60.0).floor(), s.position % 60.0));
            }
            None => ui.weak("Nothing playing"),
        }
        ui.horizontal(|ui| {
            if ui.button("⏮") {
                self.host.previous();
            }
            if ui.button(if s.playing { "⏸" } else { "▶" }) {
                self.host.toggle();
            }
            if ui.button("⏭") {
                self.host.next();
            }
            if ui.button("🎲") {
                self.random_track();
            }
        });
        ui.separator();

        ui.label("Sleep timer");
        ui.horizontal(|ui| {
            ui.drag_value("", &mut self.sleep_minutes, 0.5, 1.0..=600.0, " min");
            match self.sleep_at {
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now()).as_secs();
                    ui.label(&format!("{}:{:02} left", left / 60, left % 60));
                    if ui.small_button("Cancel") {
                        self.sleep_at = None;
                    }
                }
                None => {
                    if ui.button("Start") {
                        self.start_sleep(self.sleep_minutes);
                    }
                }
            }
        });
        if self.sleep_at.is_some() {
            ui.request_repaint();
        }
        ui.separator();

        ui.collapsing("History", |ui| {
            if self.history.is_empty() {
                ui.weak("Tracks you play show up here.");
            }
            let mut play = None;
            for (i, (path, line)) in self.history.iter().enumerate() {
                ui.id_scope(i as u64, |ui| {
                    if ui.selectable(line, false) {
                        play = Some(path.clone());
                    }
                });
            }
            if let Some(p) = play {
                self.host.load_file(&p);
                self.host.play();
            }
        });
    }

    fn menu(&mut self, id: MenuId) {
        if id == self.random {
            self.random_track();
        } else if id == self.sleep_30 {
            self.start_sleep(30.0);
        }
    }
}

// ---------------------------------------------------------------------------
// A MIDI effect: shifts notes, passes everything else on

struct Transpose {
    /// Shift each sounding note was started with, so note offs match after a change.
    started: [i8; 128],
}

const SEMITONES: u32 = 0;

impl Node for Transpose {
    const ID: &'static str = "transpose";
    const NAME: &'static str = "Transpose";
    const CATEGORY: &'static str = "MIDI";
    const MIDI: u32 = MIDI_IN | MIDI_OUT;
    type Shared = ();

    fn params() -> Vec<Param> {
        vec![Param::new(SEMITONES, "Semitones", -24.0, 24.0, 12.0).steps(48)]
    }

    fn new(_sample_rate: f32, _max_block: usize) -> Self {
        Self { started: [0; 128] }
    }

    fn midi(&mut self, input: &[MidiEvent], out: &mut MidiOut, params: &Params, _: &()) {
        let shift = params.get(SEMITONES).round() as i8;
        for e in input {
            let mut b = [0u8; 3];
            b[..e.bytes().len()].copy_from_slice(e.bytes());
            let key = b[1] as usize & 127;
            let applied = if e.as_note_on().is_some() {
                self.started[key] = shift;
                shift
            } else if e.as_note_off().is_some() || e.status() == 0xa0 {
                self.started[key]
            } else {
                out.push(*e);
                continue;
            };
            let moved = b[1] as i32 + applied as i32;
            if (0..128).contains(&moved) {
                b[1] = moved as u8;
                out.push(MidiEvent::new(e.time(), &b[..e.bytes().len()]));
            }
        }
    }

    fn process(&mut self, input: [&[f32]; 2], output: [&mut [f32]; 2], _: &Params, _: &()) {
        let [l, r] = output;
        l.copy_from_slice(input[0]);
        r.copy_from_slice(input[1]);
    }
}

export_plugin! {
    name: "Example plugin",
    version: "0.1.0",
    author: "nodeplayer",
    nodes: [Bitcrusher, Width, Transpose],
    extension: Toolbox,
}
