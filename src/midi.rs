//! MIDI between the graph and the outside world: ALSA sequencer input and output nodes, and a
//! monitor listing what passes through a MIDI wire.
//!
//! Each sequencer node owns a thread with its own sequencer client. Events cross between that
//! thread and the audio thread through lock-free queues ([`MidiBridge`]).

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alsa::seq::{Addr, ClientIter, EventType, PortCap, PortIter, PortSubscribe, PortType, Seq};
use crossbeam_channel::{Receiver, Sender};

use crate::audio2midi::note_name;
use crate::engine::MidiEvent;
use crate::engine::nodes::{MidiBridge, MidiQueue, midi_queue};

/// A sequencer port of another client.
#[derive(Clone, Debug)]
pub struct Port {
    pub addr: Addr,
    /// "client: port" — stable across restarts, unlike the numeric address.
    pub label: String,
    pub name: String,
}

/// Ports of other clients having all of `caps` (READ|SUBS_READ for sources, WRITE|SUBS_WRITE
/// for destinations).
pub fn list_ports(seq: &Seq, own_client: i32, caps: PortCap) -> Vec<Port> {
    let mut out = Vec::new();
    for c in ClientIter::new(seq) {
        let client = c.get_client();
        if client == own_client || client == 0 {
            continue;
        }
        let cname = c.get_name().unwrap_or("?").to_string();
        for p in PortIter::new(seq, client) {
            let pc = p.get_capability();
            if pc.contains(caps) && !pc.contains(PortCap::NO_EXPORT) {
                let addr = Addr { client, port: p.get_port() };
                let label = format!("{cname}: {}", p.get_name().unwrap_or("?"));
                out.push(Port { addr, name: format!("{label} ({client}:{})", addr.port), label });
            }
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Sequencer → graph.
    In,
    /// Graph → sequencer.
    Out,
}

impl Dir {
    fn port_name(self) -> &'static str {
        match self {
            Self::In => "MIDI In",
            Self::Out => "MIDI Out",
        }
    }

    /// Capabilities of the ports we can connect to.
    fn peer_caps(self) -> PortCap {
        match self {
            Self::In => PortCap::READ | PortCap::SUBS_READ,
            Self::Out => PortCap::WRITE | PortCap::SUBS_WRITE,
        }
    }
}

enum Cmd {
    Connect(Option<Port>),
    /// Connects to the port with this label once it exists.
    ConnectLabel(String),
    Refresh,
    /// All notes off on every channel (output only).
    Panic,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    ports: Mutex<Vec<Port>>,
    connected: Mutex<Option<Port>>,
    error: Mutex<Option<String>>,
    own: Mutex<Option<Addr>>,
    count: AtomicU64,
}

/// An ALSA sequencer port as a graph node.
pub struct SeqNode {
    pub dir: Dir,
    /// Label of the port the user picked, kept even while that port is missing.
    pub want: Option<String>,
    graph_rx: Option<MidiQueue<rtrb::Consumer<MidiEvent>>>,
    graph_tx: Option<MidiQueue<rtrb::Producer<MidiEvent>>>,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    activity: (u64, Instant),
}

impl SeqNode {
    pub fn new(dir: Dir) -> Self {
        let (to_graph, graph_rx) = midi_queue();
        let (graph_tx, from_graph) = midi_queue();
        let shared = Arc::new(Shared::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("alsa {}", dir.port_name()))
                .spawn(move || match dir {
                    Dir::In => worker(dir, shared, rx, Some(to_graph), None),
                    Dir::Out => worker(dir, shared, rx, None, Some(from_graph)),
                })
                .ok()
        };
        let _ = tx.send(Cmd::Refresh);
        let (graph_rx, graph_tx) = match dir {
            Dir::In => (Some(graph_rx), None),
            Dir::Out => (None, Some(graph_tx)),
        };
        Self { dir, want: None, graph_rx, graph_tx, shared, tx, thread, activity: (0, Instant::now()) }
    }

    pub fn processor(&self) -> MidiBridge {
        MidiBridge { from: self.graph_rx.clone(), to: self.graph_tx.clone(), tap: None, thru: false }
    }

    pub fn connect(&mut self, port: Option<Port>) {
        self.want = port.as_ref().map(|p| p.label.clone());
        let _ = self.tx.send(Cmd::Connect(port));
    }

    pub fn connect_label(&mut self, label: String) {
        self.want = Some(label.clone());
        let _ = self.tx.send(Cmd::ConnectLabel(label));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let connected = self.shared.connected.lock().unwrap().clone();
        let ports = self.shared.ports.lock().unwrap().clone();
        let text = match (&connected, &self.want) {
            (Some(p), _) => p.label.clone(),
            (None, Some(w)) => format!("{w} (missing)"),
            (None, None) => "not connected".into(),
        };
        let mut pick: Option<Option<Port>> = None;
        ui.horizontal(|ui| {
            let label = match self.dir {
                Dir::In => "from",
                Dir::Out => "to",
            };
            ui.label(label);
            egui::ComboBox::from_id_salt(ui.id().with("seq-port")).width(200.0).selected_text(text).show_ui(ui, |ui| {
                if ui.selectable_label(connected.is_none(), "none").clicked() {
                    pick = Some(None);
                }
                for p in &ports {
                    let on = connected.as_ref().is_some_and(|c| c.addr == p.addr);
                    if ui.selectable_label(on, &p.name).clicked() {
                        pick = Some(Some(p.clone()));
                    }
                }
            });
            if ui.small_button("⟳").on_hover_text("refresh the port list").clicked() {
                let _ = self.tx.send(Cmd::Refresh);
            }
        });
        if let Some(p) = pick {
            self.connect(p);
        }
        ui.horizontal(|ui| {
            // Lights up for a moment whenever events pass.
            let count = self.shared.count.load(Ordering::Relaxed);
            if count != self.activity.0 {
                self.activity = (count, Instant::now());
            }
            let lit = self.activity.1.elapsed() < Duration::from_millis(150) && count > 0;
            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            let color = if lit { egui::Color32::from_rgb(255, 170, 60) } else { egui::Color32::from_gray(60) };
            ui.painter().circle_filled(r.center(), 4.0, color);
            ui.small(format!("{count} events"));
            if let Some(own) = *self.shared.own.lock().unwrap() {
                ui.weak(format!("· port {}:{}", own.client, own.port));
            }
            if self.dir == Dir::Out && ui.small_button("panic").on_hover_text("all notes off").clicked() {
                let _ = self.tx.send(Cmd::Panic);
            }
        });
        if let Some(e) = self.shared.error.lock().unwrap().clone() {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 90), e);
        }
        if self.activity.1.elapsed() < Duration::from_millis(200) {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

impl Drop for SeqNode {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Client {
    seq: Seq,
    me: Addr,
    connected: Option<Addr>,
}

impl Client {
    fn open(dir: Dir) -> alsa::Result<Self> {
        let (sdir, caps) = match dir {
            Dir::In => (alsa::Direction::Capture, PortCap::WRITE | PortCap::SUBS_WRITE),
            Dir::Out => (alsa::Direction::Playback, PortCap::READ | PortCap::SUBS_READ),
        };
        let seq = Seq::open(None, Some(sdir), true)?;
        seq.set_client_name(&CString::new("nodeplayer").unwrap())?;
        let port = seq.create_simple_port(
            &CString::new(dir.port_name()).unwrap(),
            caps,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )?;
        let me = Addr { client: seq.client_id()?, port };
        Ok(Self { seq, me, connected: None })
    }

    fn connect(&mut self, dir: Dir, peer: Option<Addr>) -> alsa::Result<()> {
        let pair = |p: Addr| match dir {
            Dir::In => (p, self.me),
            Dir::Out => (self.me, p),
        };
        if let Some(old) = self.connected.take() {
            let (s, d) = pair(old);
            let _ = self.seq.unsubscribe_port(s, d);
        }
        if let Some(p) = peer {
            let (s, d) = pair(p);
            let sub = PortSubscribe::empty()?;
            sub.set_sender(s);
            sub.set_dest(d);
            self.seq.subscribe_port(&sub)?;
            self.connected = Some(p);
        }
        Ok(())
    }
}

fn worker(
    dir: Dir,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    to_graph: Option<MidiQueue<rtrb::Producer<MidiEvent>>>,
    from_graph: Option<MidiQueue<rtrb::Consumer<MidiEvent>>>,
) {
    let mut client = match Client::open(dir) {
        Ok(c) => c,
        Err(e) => {
            *shared.error.lock().unwrap() = Some(format!("ALSA sequencer: {e}"));
            return;
        }
    };
    *shared.own.lock().unwrap() = Some(client.me);
    let Ok(mut codec) = alsa::seq::MidiEvent::new(256) else { return };
    codec.enable_running_status(false);
    let mut out = Vec::new();

    let set_connection = |client: &mut Client, port: Option<Port>| {
        let res = client.connect(dir, port.as_ref().map(|p| p.addr));
        *shared.error.lock().unwrap() = res.as_ref().err().map(|e| format!("connect failed: {e}"));
        *shared.connected.lock().unwrap() = port.filter(|_| res.is_ok());
    };

    while !shared.stop.load(Ordering::Acquire) {
        for cmd in rx.try_iter() {
            match cmd {
                Cmd::Refresh => *shared.ports.lock().unwrap() = list_ports(&client.seq, client.me.client, dir.peer_caps()),
                Cmd::Connect(p) => set_connection(&mut client, p),
                Cmd::ConnectLabel(label) => {
                    let ports = list_ports(&client.seq, client.me.client, dir.peer_caps());
                    match ports.iter().find(|p| p.label == label) {
                        Some(p) => set_connection(&mut client, Some(p.clone())),
                        None => *shared.error.lock().unwrap() = Some(format!("MIDI port \"{label}\" not found")),
                    }
                    *shared.ports.lock().unwrap() = ports;
                }
                Cmd::Panic => {
                    for ch in 0..16u8 {
                        for (cc, v) in [(123, 0), (121, 0)] {
                            out.push(MidiEvent { time: 0, data: [0xb0 | ch, cc, v] });
                        }
                    }
                    send(&client, &mut codec, &mut out);
                }
            }
        }

        match dir {
            Dir::In => {
                let mut fds = alsa::poll::Descriptors::get(&(&client.seq, Some(alsa::Direction::Capture))).unwrap_or_default();
                if alsa::poll::poll(&mut fds, 20).unwrap_or(0) == 0 {
                    continue;
                }
                let mut input = client.seq.input();
                let mut buf = [0u8; 16];
                while input.event_input_pending(true).unwrap_or(0) > 0 {
                    let Ok(mut ev) = input.event_input() else { break };
                    if matches!(ev.get_type(), EventType::PortSubscribed | EventType::PortUnsubscribed) {
                        continue;
                    }
                    let Ok(len) = codec.decode(&mut buf, &mut ev) else { continue };
                    if let Some(e) = MidiEvent::new(0, &buf[..len])
                        && let Some(Ok(mut tx)) = to_graph.as_ref().map(|t| t.lock())
                    {
                        let _ = tx.push(e);
                        shared.count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Dir::Out => {
                if let Some(Ok(mut rx)) = from_graph.as_ref().map(|r| r.lock()) {
                    while let Ok(e) = rx.pop() {
                        out.push(e);
                    }
                }
                shared.count.fetch_add(out.len() as u64, Ordering::Relaxed);
                send(&client, &mut codec, &mut out);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    if dir == Dir::Out {
        for ch in 0..16u8 {
            out.push(MidiEvent { time: 0, data: [0xb0 | ch, 123, 0] });
        }
        send(&client, &mut codec, &mut out);
    }
}

fn send(client: &Client, codec: &mut alsa::seq::MidiEvent, out: &mut Vec<MidiEvent>) {
    for e in out.drain(..) {
        codec.reset_encode();
        if let Ok((_, Some(mut ev))) = codec.encode(e.bytes()) {
            ev.set_source(client.me.port);
            ev.set_subs();
            ev.set_direct();
            if let Err(err) = client.seq.event_output_direct(&mut ev) {
                log::warn!("MIDI send failed: {err}");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Monitor

/// Lists recent events passing through; MIDI is passed on unchanged.
pub struct Monitor {
    rx: rtrb::Consumer<MidiEvent>,
    tx: MidiQueue<rtrb::Producer<MidiEvent>>,
    lines: VecDeque<(Instant, MidiEvent)>,
    pub paused: bool,
}

impl Monitor {
    const LINES: usize = 12;

    pub fn new() -> Self {
        let (tx, rx) = rtrb::RingBuffer::new(4096);
        Self { rx, tx: Arc::new(Mutex::new(tx)), lines: VecDeque::new(), paused: false }
    }

    pub fn processor(&self) -> MidiBridge {
        MidiBridge { from: None, to: Some(self.tx.clone()), tap: None, thru: true }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        while let Ok(e) = self.rx.pop() {
            if !self.paused {
                self.lines.push_back((Instant::now(), e));
            }
        }
        while self.lines.len() > Self::LINES {
            self.lines.pop_front();
        }
        ui.horizontal(|ui| {
            ui.toggle_value(&mut self.paused, "pause");
            if ui.small_button("clear").clicked() {
                self.lines.clear();
            }
        });
        let w = 230.0;
        egui::Frame::new().fill(ui.visuals().extreme_bg_color).inner_margin(4.0).show(ui, |ui| {
            ui.set_min_size(egui::vec2(w, Self::LINES as f32 * 14.0));
            if self.lines.is_empty() {
                ui.weak("no MIDI yet");
            }
            for (t, e) in self.lines.iter().rev() {
                let fresh = t.elapsed() < Duration::from_millis(300);
                let text = egui::RichText::new(describe(e)).monospace().size(11.0);
                ui.label(if fresh { text.strong() } else { text.weak() });
            }
        });
        if self.lines.back().is_some_and(|(t, _)| t.elapsed() < Duration::from_millis(400)) {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// One line for the monitor: "ch 1  note on  C4  vel 100".
pub fn describe(e: &MidiEvent) -> String {
    let [s, a, b] = e.data;
    let ch = e.channel() + 1;
    match s & 0xf0 {
        0x90 if b > 0 => format!("ch{ch:<2} note on   {:<4} vel {b}", note_name(a)),
        0x80 | 0x90 => format!("ch{ch:<2} note off  {:<4} vel {b}", note_name(a)),
        0xa0 => format!("ch{ch:<2} poly AT   {:<4} {b}", note_name(a)),
        0xb0 => format!("ch{ch:<2} CC {a:<3}    {b}"),
        0xc0 => format!("ch{ch:<2} program   {a}"),
        0xd0 => format!("ch{ch:<2} chan AT   {a}"),
        0xe0 => format!("ch{ch:<2} bend      {}", ((b as i32) << 7 | a as i32) - 8192),
        _ => e.bytes().iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_events() {
        let on = MidiEvent::new(0, &[0x91, 60, 100]).unwrap();
        assert_eq!(describe(&on), "ch2  note on   C4   vel 100");
        let bend = MidiEvent::new(0, &[0xe0, 0, 0x40]).unwrap();
        assert!(describe(&bend).ends_with(" 0"));
        assert!(MidiEvent::new(0, &[0xf0, 1, 2]).is_none());
        assert_eq!(MidiEvent::new(0, &[0xc0, 5]).unwrap().bytes(), &[0xc0, 5]);
    }
}
