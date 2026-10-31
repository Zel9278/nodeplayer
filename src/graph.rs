//! UI-side graph model and its compilation into an engine [`Schedule`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use egui_snarl::{NodeId, Snarl};

use crate::engine::eq::{EqSettings, ParamEq};
use crate::engine::nodes::{self, LimiterMeter, LimiterParams, MIXER_INPUTS, Meter};
use crate::engine::{Processor, Schedule, Step};
use crate::audio2midi::Audio2Midi;
use crate::csynth_node::CSynthNode;
use crate::lyrics::LyricsNode;
use crate::video::VideoNode;
use crate::midi::{Monitor, SeqNode};
use crate::eq_editor::EqNode;
use crate::player::Player;
use crate::visual::Visualizer;
use crate::plugins::guard::Tag;
use crate::plugins::{Editor, Instance, Placement, PluginInfo};

pub struct PluginNode {
    /// Marks calls into the plugin, for crash reports.
    pub tag: Arc<Tag>,
    // Declared before the instance: an editor must close before its plugin goes away.
    pub editor: Option<Box<dyn Editor>>,
    pub instance: Box<dyn Instance>,
    pub bypass: bool,
    pub show_params: bool,
    pub param_filter: String,
}

impl PluginNode {
    pub fn new(instance: Box<dyn Instance>) -> Self {
        let tag = Tag::of(instance.info());
        Self { tag, editor: None, instance, bypass: false, show_params: false, param_filter: String::new() }
    }

    pub fn info(&self) -> &PluginInfo {
        self.instance.info()
    }

    /// (Re)opens the plugin's editor at `placement`.
    pub fn open_editor(&mut self, placement: Placement) -> anyhow::Result<()> {
        self.editor = None;
        self.editor = Some(self.instance.open_editor(placement)?);
        Ok(())
    }

    pub fn is_embedded(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.placement() != Placement::Window)
    }
}

pub enum NodeKind {
    /// Plays the current track: audio out, and MIDI out for MIDI files.
    Player,
    Output { volume_db: f32, meter: Arc<Meter>, level: [f32; 2] },
    Gain { db: f32 },
    Pan { pan: f32 },
    /// Playback rate shift in the chain. The rate is applied where the player reads the file
    /// (it changes how fast the file goes by), so the node passes audio through; it counts
    /// while the Player feeds it. `active` is whether it does, as last worked out.
    RateShift { rate: f32, bypass: bool, active: bool },
    Eq(Box<EqNode>),
    Delay { time_ms: f32, feedback: f32, mix: f32 },
    Mixer { gains_db: [f32; MIXER_INPUTS] },
    /// `gain` is the lowest gain applied since the previous UI frame.
    Limiter { p: LimiterParams, meter: Arc<LimiterMeter>, gain: f32 },
    Plugin(Box<PluginNode>),
    Visual(Box<Visualizer>),
    Audio2Midi(Box<Audio2Midi>),
    MidiIn(Box<SeqNode>),
    MidiOut(Box<SeqNode>),
    MidiMonitor(Box<Monitor>),
    CSynth(Box<CSynthNode>),
    Lyrics(Box<LyricsNode>),
    Video(Box<VideoNode>),
}

impl NodeKind {
    pub fn output() -> Self {
        Self::Output { volume_db: 0.0, meter: Arc::default(), level: [0.0; 2] }
    }

    pub fn limiter() -> Self {
        Self::Limiter { p: LimiterParams::default(), meter: Arc::default(), gain: 1.0 }
    }

    pub fn eq(settings: EqSettings) -> Self {
        Self::Eq(Box::new(EqNode::new(settings)))
    }

    pub fn delay() -> Self {
        Self::Delay { time_ms: 350.0, feedback: 0.35, mix: 0.3 }
    }

    /// The plugin's crash-report tag, for plugin nodes.
    pub fn tag(&self) -> Option<Arc<Tag>> {
        match self {
            Self::Plugin(p) => Some(p.tag.clone()),
            _ => None,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Player => "Player".into(),
            Self::Output { .. } => "Output".into(),
            Self::Gain { .. } => "Gain".into(),
            Self::Pan { .. } => "Pan".into(),
            Self::RateShift { .. } => "Playback Rate Shift".into(),
            Self::Eq(_) => "EQ".into(),
            Self::Delay { .. } => "Delay".into(),
            Self::Mixer { .. } => "Mixer".into(),
            Self::Limiter { .. } => "Kazu Limiter".into(),
            Self::Plugin(p) => p.info().name.clone(),
            Self::Visual(v) => v.kind.label().into(),
            Self::Audio2Midi(_) => "Audio → MIDI".into(),
            Self::MidiIn(_) => "MIDI In".into(),
            Self::MidiOut(_) => "MIDI Out".into(),
            Self::MidiMonitor(_) => "MIDI Monitor".into(),
            Self::CSynth(_) => "CSynth".into(),
            Self::Lyrics(_) => "Lyrics".into(),
            Self::Video(_) => "Video".into(),
        }
    }

    /// Stereo inputs; a MIDI input, if any, comes after them.
    pub fn audio_inputs(&self) -> usize {
        match self {
            Self::Player
            | Self::MidiIn(_)
            | Self::MidiOut(_)
            | Self::MidiMonitor(_)
            | Self::CSynth(_)
            | Self::Lyrics(_)
            | Self::Video(_) => 0,
            Self::Mixer { .. } => MIXER_INPUTS,
            _ => 1,
        }
    }

    pub fn audio_outputs(&self) -> usize {
        match self {
            Self::Output { .. } | Self::MidiIn(_) | Self::MidiOut(_) | Self::MidiMonitor(_) | Self::Lyrics(_) | Self::Video(_) => 0,
            _ => 1,
        }
    }

    pub fn midi_input(&self) -> bool {
        match self {
            Self::MidiOut(_) | Self::MidiMonitor(_) | Self::CSynth(_) => true,
            Self::Plugin(p) => p.instance.midi_ports().0,
            _ => false,
        }
    }

    pub fn midi_output(&self) -> bool {
        match self {
            Self::Player | Self::MidiIn(_) | Self::MidiMonitor(_) | Self::Audio2Midi(_) => true,
            Self::Plugin(p) => p.instance.midi_ports().1,
            _ => false,
        }
    }

    /// Pins as the graph view shows them: audio first, then MIDI.
    pub fn inputs(&self) -> usize {
        self.audio_inputs() + self.midi_input() as usize
    }

    pub fn outputs(&self) -> usize {
        self.audio_outputs() + self.midi_output() as usize
    }

    pub fn is_midi_input(&self, pin: usize) -> bool {
        pin >= self.audio_inputs()
    }

    pub fn is_midi_output(&self, pin: usize) -> bool {
        pin >= self.audio_outputs()
    }

    /// Builds the audio-thread processor for built-in kinds. Plugin nodes are created separately
    /// because instantiating the plugin can fail.
    pub fn make_processor(&self, sample_rate: f32, player: &Player) -> Option<Box<dyn Processor>> {
        Some(match self {
            Self::Player => Box::new(player.make_source()),
            Self::Output { volume_db, meter, .. } => Box::new(nodes::Output::new(*volume_db, meter.clone())),
            Self::Gain { db } => Box::new(nodes::Gain::new(*db)),
            Self::Pan { pan } => Box::new(nodes::Pan::new(*pan)),
            Self::RateShift { .. } => Box::new(nodes::Through),
            Self::Eq(e) => Box::new(ParamEq::new(sample_rate, &e.settings, e.tap.clone())),
            Self::Delay { time_ms, feedback, mix } => {
                Box::new(nodes::Delay::new(sample_rate, *time_ms, *feedback, *mix))
            }
            Self::Mixer { gains_db } => Box::new(nodes::Mixer::new(*gains_db)),
            Self::Limiter { p, meter, .. } => Box::new(nodes::KazuLimiter::new(sample_rate, *p, meter.clone())),
            Self::Visual(v) => Box::new(nodes::TapNode::new(v.tap.clone())),
            Self::Audio2Midi(a) => Box::new(a.processor()),
            Self::MidiIn(m) | Self::MidiOut(m) => Box::new(m.processor()),
            Self::MidiMonitor(m) => Box::new(m.processor()),
            Self::CSynth(c) => Box::new(c.processor()),
            // Shows what's playing; nothing to do on the audio thread.
            Self::Lyrics(_) | Self::Video(_) => Box::new(nodes::MidiBridge::default()),
            Self::Plugin(_) => return None,
        })
    }
}

pub struct GraphNode {
    /// Slot index in the engine.
    pub eid: u32,
    pub kind: NodeKind,
}

#[derive(Default)]
pub struct IdAlloc {
    free: Vec<u32>,
    next: u32,
}

impl IdAlloc {
    pub fn alloc(&mut self) -> Option<u32> {
        if let Some(id) = self.free.pop() {
            return Some(id);
        }
        if (self.next as usize) < crate::engine::MAX_NODES {
            self.next += 1;
            Some(self.next - 1)
        } else {
            None
        }
    }

    pub fn free(&mut self, id: u32) {
        self.free.push(id);
    }
}

/// The rate the Rate Shift nodes ask for: the product of those the Player's audio goes
/// through and that aren't bypassed. Marks each node with whether it counts.
pub fn rate_shift(snarl: &mut Snarl<GraphNode>) -> f32 {
    let mut fed: std::collections::HashSet<NodeId> = snarl.node_ids().filter(|(_, n)| matches!(n.kind, NodeKind::Player)).map(|(id, _)| id).collect();
    let mut stack: Vec<NodeId> = fed.iter().copied().collect();
    while let Some(n) = stack.pop() {
        for (o, i) in snarl.wires() {
            if o.node == n && !snarl[o.node].kind.is_midi_output(o.output) && fed.insert(i.node) {
                stack.push(i.node);
            }
        }
    }
    let mut rate = 1.0;
    for (id, node) in snarl.nodes_ids_mut() {
        if let NodeKind::RateShift { rate: r, bypass, active } = &mut node.kind {
            *active = fed.contains(&id);
            if *active && !*bypass {
                rate *= *r;
            }
        }
    }
    rate
}

/// True if wiring `from -> to` would close a loop.
pub fn would_cycle(snarl: &Snarl<GraphNode>, from: NodeId, to: NodeId) -> bool {
    if from == to {
        return true;
    }
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (o, i) in snarl.wires() {
        adj.entry(o.node).or_default().push(i.node);
    }
    let mut stack = vec![to];
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = stack.pop() {
        if n == from {
            return true;
        }
        if seen.insert(n) {
            stack.extend(adj.get(&n).into_iter().flatten().copied());
        }
    }
    false
}

/// Topologically sorts every node. Unconnected nodes are still scheduled so sources keep
/// draining their queues.
pub fn compile(snarl: &Snarl<GraphNode>) -> Schedule {
    let mut indeg: HashMap<NodeId, usize> = snarl.node_ids().map(|(id, _)| (id, 0)).collect();
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut sources: HashMap<(NodeId, usize), Vec<(u32, u32)>> = HashMap::new();
    let mut midi: HashMap<NodeId, Vec<u32>> = HashMap::new();
    for (o, i) in snarl.wires() {
        *indeg.entry(i.node).or_default() += 1;
        adj.entry(o.node).or_default().push(i.node);
        let (from, to) = (&snarl[o.node].kind, &snarl[i.node].kind);
        match (from.is_midi_output(o.output), to.is_midi_input(i.input)) {
            (false, false) => sources.entry((i.node, i.input)).or_default().push((snarl[o.node].eid, o.output as u32)),
            (true, true) => midi.entry(i.node).or_default().push(snarl[o.node].eid),
            // Mismatched wires are refused when made; one could only survive a plugin
            // changing its ports.
            _ => {}
        }
    }

    let mut queue: VecDeque<NodeId> = indeg.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
    let mut order = Vec::with_capacity(indeg.len());
    while let Some(n) = queue.pop_front() {
        order.push(n);
        for m in adj.get(&n).into_iter().flatten() {
            let d = indeg.get_mut(m).unwrap();
            *d -= 1;
            if *d == 0 {
                queue.push_back(*m);
            }
        }
    }

    let mut schedule = Schedule::default();
    for id in order {
        let node = &snarl[id];
        let inputs = (0..node.kind.audio_inputs())
            .map(|p| sources.remove(&(id, p)).unwrap_or_default())
            .collect();
        let midi = midi.remove(&id).unwrap_or_default();
        schedule.steps.push(Step { node: node.eid, inputs, midi });
        if matches!(node.kind, NodeKind::Output { .. }) {
            schedule.sinks.push(node.eid);
        }
    }
    schedule
}

#[cfg(test)]
mod tests {
    use egui_snarl::{InPinId, OutPinId};

    use super::*;

    fn rate_node(rate: f32, bypass: bool) -> GraphNode {
        GraphNode { eid: 0, kind: NodeKind::RateShift { rate, bypass, active: false } }
    }

    fn active(snarl: &Snarl<GraphNode>, id: NodeId) -> bool {
        matches!(snarl[id].kind, NodeKind::RateShift { active: true, .. })
    }

    #[test]
    fn rate_shift_counts_nodes_the_player_feeds() {
        let mut snarl = Snarl::new();
        let at = egui::pos2(0.0, 0.0);
        let player = snarl.insert_node(at, GraphNode { eid: 0, kind: NodeKind::Player });
        let a = snarl.insert_node(at, rate_node(1.5, false));
        let b = snarl.insert_node(at, rate_node(1.2, false));
        let bypassed = snarl.insert_node(at, rate_node(0.5, true));
        let loose = snarl.insert_node(at, rate_node(2.0, false));
        let wire = |snarl: &mut Snarl<GraphNode>, from, to| {
            snarl.connect(OutPinId { node: from, output: 0 }, InPinId { node: to, input: 0 });
        };
        wire(&mut snarl, player, a);
        wire(&mut snarl, a, b);
        wire(&mut snarl, b, bypassed);
        assert!((rate_shift(&mut snarl) - 1.8).abs() < 1e-6);
        assert!(active(&snarl, a) && active(&snarl, b) && active(&snarl, bypassed));
        assert!(!active(&snarl, loose));
        // Cut off from the Player, a node stops counting.
        snarl.disconnect(OutPinId { node: a, output: 0 }, InPinId { node: b, input: 0 });
        assert!((rate_shift(&mut snarl) - 1.5).abs() < 1e-6);
        assert!(!active(&snarl, b));
    }
}
