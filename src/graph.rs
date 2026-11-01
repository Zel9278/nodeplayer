//! UI-side graph model and its compilation into an engine [`Schedule`].

use std::collections::{BTreeMap, HashMap};

use crate::engine::timing::{self, EdgeKey, EdgeKind, NodeSpec, NodeTiming, TimingLimits};
use anyhow::Result;
use std::sync::Arc;

use egui_snarl::{NodeId, Snarl};

use crate::engine::eq::{EqSettings, ParamEq};
use crate::engine::nodes::{self, LimiterMeter, LimiterParams, MIXER_INPUTS, Meter};
use crate::engine::{Processor, Schedule, Step};
use crate::audio2midi::Audio2Midi;
use crate::csynth_node::CSynthNode;
use crate::midi::{Monitor, SeqNode};
use crate::eq_editor::EqNode;
use crate::player::Player;
use crate::visual::Visualizer;
use crate::plugins::guard::Tag;
use crate::plugins::{Editor, Instance, Placement, PluginInfo};

/// A graph inside a node. Inside, a [`NodeKind::GroupIn`] node gives out what reaches the
/// group's inputs, and what reaches the [`NodeKind::GroupOut`] node leaves by its outputs.
pub struct GroupNode {
    pub name: String,
    pub snarl: Snarl<GraphNode>,
}

impl GroupNode {
    pub fn new(name: String) -> Self {
        Self { name, snarl: Snarl::new() }
    }

    fn eid_of(&self, f: impl Fn(&NodeKind) -> bool) -> Option<u32> {
        self.snarl.nodes().find(|n| f(&n.kind)).map(|n| n.eid)
    }
}

pub struct PluginNode {
    /// Marks calls into the plugin, for crash reports.
    pub tag: Arc<Tag>,
    // Declared before the instance: an editor must close before its plugin goes away.
    pub editor: Option<Box<dyn Editor>>,
    pub instance: Box<dyn Instance>,
    /// Last control-side timing accepted for the prepared bypass/graph plan.
    pub timing: (u32, u32),
    pub bypass: bool,
    pub show_params: bool,
    pub param_filter: String,
}

impl PluginNode {
    pub fn new(instance: Box<dyn Instance>) -> Self {
        let tag = Tag::of(instance.info());
        let timing = (instance.latency(), instance.bridge_latency());
        Self {
            tag,
            editor: None,
            instance,
            timing,
            bypass: false,
            show_params: false,
            param_filter: String::new(),
        }
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
    Group(Box<GroupNode>),
    /// Inside a group: what comes into the group.
    GroupIn,
    /// Inside a group: what leaves the group.
    GroupOut,
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
            Self::Group(g) => g.name.clone(),
            Self::GroupIn => "Group input".into(),
            Self::GroupOut => "Group output".into(),
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
            | Self::GroupIn => 0,
            Self::Mixer { .. } => MIXER_INPUTS,
            _ => 1,
        }
    }

    pub fn audio_outputs(&self) -> usize {
        match self {
            Self::Output { .. }
            | Self::MidiIn(_)
            | Self::MidiOut(_)
            | Self::MidiMonitor(_)
            | Self::GroupOut => 0,
            _ => 1,
        }
    }

    pub fn midi_input(&self) -> bool {
        match self {
            Self::MidiOut(_) | Self::MidiMonitor(_) | Self::CSynth(_) | Self::Group(_) | Self::GroupOut => true,
            Self::Plugin(p) => p.instance.midi_ports().0,
            _ => false,
        }
    }

    pub fn midi_output(&self) -> bool {
        match self {
            Self::Player | Self::MidiIn(_) | Self::MidiMonitor(_) | Self::Audio2Midi(_) | Self::Group(_) | Self::GroupIn => true,
            Self::Plugin(p) => p.instance.midi_ports().1,
            _ => false,
        }
    }

    /// Stereo inputs and outputs of the node's engine slot. The group's input and output
    /// nodes have one more than they show: the group feeds the first, the second feeds it.
    pub fn engine_ports(&self) -> (usize, usize) {
        match self {
            Self::GroupIn => (1, 1),
            Self::GroupOut => (1, 1),
            k => (k.audio_inputs(), k.audio_outputs()),
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
            // A group's own slot takes what comes in; the nodes inside pass it on.
            Self::Group(_) | Self::GroupIn | Self::GroupOut => Box::new(nodes::MidiBridge { thru: true, ..Default::default() }),
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

/// Every node in `snarl` and in the groups inside it, groups' own nodes left out.
pub fn collect_mut<'a>(snarl: &'a mut Snarl<GraphNode>, out: &mut Vec<&'a mut GraphNode>) {
    for node in snarl.nodes_mut() {
        if matches!(node.kind, NodeKind::Group(_)) {
            let NodeKind::Group(g) = &mut node.kind else { unreachable!() };
            collect_mut(&mut g.snarl, out);
        } else {
            out.push(node);
        }
    }
}

/// The groups to go into, from `snarl`, to reach the node with engine slot `eid`, and its id
/// there.
pub fn path_to(snarl: &Snarl<GraphNode>, eid: u32) -> Option<(Vec<NodeId>, NodeId)> {
    for (id, node) in snarl.node_ids() {
        if node.eid == eid {
            return Some((Vec::new(), id));
        }
        if let NodeKind::Group(g) = &node.kind
            && let Some((mut path, found)) = path_to(&g.snarl, eid)
        {
            path.insert(0, id);
            return Some((path, found));
        }
    }
    None
}

/// The graph with its groups opened up: engine slots and what connects them.
#[derive(Default)]
struct Flat {
    nodes: Vec<NodeSpec>,
    /// `(from slot, output) -> (to slot, input)`.
    audio: Vec<((u32, u32), (u32, usize))>,
    /// MIDI from one slot to another.
    midi: Vec<(u32, u32)>,
}

impl Flat {
    fn add(&mut self, snarl: &Snarl<GraphNode>, enabled: bool) -> Result<()> {
        for node in snarl.nodes() {
            let (audio_inputs, audio_outputs) = node.kind.engine_ports();
            let timing = if enabled {
                if let NodeKind::Plugin(plugin) = &node.kind {
                    // Control-side getter only. latency() is TOTAL, including future bridges.
                    NodeTiming::from_total(
                        plugin.instance.latency(),
                        plugin.instance.bridge_latency(),
                    )?
                } else {
                    NodeTiming::default()
                }
            } else {
                NodeTiming::default()
            };
            self.nodes.push(NodeSpec {
                id: node.eid,
                audio_inputs,
                audio_outputs,
                midi_input: node.kind.midi_input() || matches!(node.kind, NodeKind::GroupIn),
                midi_output: node.kind.midi_output() || matches!(node.kind, NodeKind::GroupOut),
                sink: matches!(node.kind, NodeKind::Output { .. }),
                midi_sink: matches!(node.kind, NodeKind::MidiOut(_)),
                timing,
            });
            if let NodeKind::Group(g) = &node.kind {
                self.add(&g.snarl, enabled)?;
                // The group's slot takes what comes in and hands it to the input node inside.
                if let Some(input) = g.eid_of(|k| matches!(k, NodeKind::GroupIn)) {
                    self.audio.push(((node.eid, 0), (input, 0)));
                    self.midi.push((node.eid, input));
                }
            }
        }
        for (o, i) in snarl.wires() {
            let (from, to) = (&snarl[o.node], &snarl[i.node]);
            if o.output >= from.kind.outputs() || i.input >= to.kind.inputs() {
                if enabled {
                    anyhow::bail!("invalid graph pin on {} -> {}", from.eid, to.eid);
                }
                continue;
            }
            // What leaves a group comes from the output node inside it.
            let src = match &from.kind {
                NodeKind::Group(g) => match g.eid_of(|k| matches!(k, NodeKind::GroupOut)) {
                    Some(eid) => eid,
                    None => continue,
                },
                _ => from.eid,
            };
            match (from.kind.is_midi_output(o.output), to.kind.is_midi_input(i.input)) {
                (false, false) => self.audio.push(((src, o.output as u32), (to.eid, i.input))),
                (true, true) => self.midi.push((src, to.eid)),
                // A plugin port change must fail compilation, not silently change the plan.
                _ if enabled => anyhow::bail!("audio/MIDI port mismatch on {} -> {}", from.eid, to.eid),
                _ => continue,
            }
        }
        Ok(())
    }

    /// Topologically sorts every slot, including the latest upstream group expansion.
    /// Unconnected nodes still run so sources continue draining queues.
    fn schedule(self, enabled: bool, limits: TimingLimits) -> Result<Schedule> {
        let mut counts = BTreeMap::new();
        let mut edges = Vec::new();
        for ((from, output), (to, input)) in &self.audio {
            let kind = EdgeKind::Audio {
                output: *output,
                input: *input,
            };
            let occurrence = counts.entry((*from, *to, kind)).or_insert(0);
            edges.push(EdgeKey {
                from: *from,
                to: *to,
                kind,
                occurrence: *occurrence,
            });
            *occurrence += 1;
        }
        for &(from, to) in &self.midi {
            let kind = EdgeKind::Midi;
            let occurrence = counts.entry((from, to, kind)).or_insert(0);
            edges.push(EdgeKey {
                from,
                to,
                kind,
                occurrence: *occurrence,
            });
            *occurrence += 1;
        }
        let analysis = if enabled {
            Some(timing::analyze(&self.nodes, &edges, true, limits)?)
        } else {
            None
        };
        let order = if let Some(analysis) = &analysis {
            analysis.order.clone()
        } else {
            // Match the pre-PDC compiler: Kahn's sort simply leaves cyclic nodes out.
            let mut indegree: HashMap<u32, usize> = self.nodes.iter().map(|n| (n.id, 0)).collect();
            let mut outgoing: HashMap<u32, Vec<u32>> = HashMap::new();
            for edge in &edges {
                if indegree.contains_key(&edge.from) && indegree.contains_key(&edge.to) {
                    *indegree.get_mut(&edge.to).unwrap() += 1;
                    outgoing.entry(edge.from).or_default().push(edge.to);
                }
            }
            let mut queue: std::collections::VecDeque<u32> = self.nodes.iter().map(|n| n.id).filter(|id| indegree[id] == 0).collect();
            let mut order = Vec::with_capacity(self.nodes.len());
            while let Some(id) = queue.pop_front() {
                order.push(id);
                for to in outgoing.get(&id).into_iter().flatten() {
                    let count = indegree.get_mut(to).unwrap();
                    *count -= 1;
                    if *count == 0 { queue.push_back(*to); }
                }
            }
            order
        };
        let info: HashMap<u32, &NodeSpec> = self.nodes.iter().map(|node| (node.id, node)).collect();
        let mut schedule = Schedule::default();
        for &eid in &order {
            let node = info[&eid];
            let mut inputs = vec![Vec::new(); node.audio_inputs];
            for (src, (_, port)) in self.audio.iter().filter(|(_, (to, _))| *to == eid) {
                inputs[*port].push(*src);
            }
            let midi = self
                .midi
                .iter()
                .filter(|(_, to)| *to == eid)
                .map(|(from, _)| *from)
                .collect();
            schedule.steps.push(Step {
                node: eid,
                inputs,
                midi,
            });
            if node.sink {
                schedule.sinks.push(eid);
            }
        }
        if enabled {
            schedule.timing = Some(crate::engine::compensation::TimingPlan::prepare(
                analysis.as_ref().unwrap(),
                limits,
                &schedule.steps,
            )?);
        }
        Ok(schedule)
    }
}

/// The engine's schedule for the whole graph, groups included.
#[cfg(test)]
pub fn compile(snarl: &Snarl<GraphNode>) -> Schedule {
    // Compatibility entry point. Existing callers cannot report a compile error; fail closed
    // instead of returning the old compiler's dangerous partial cyclic schedule.
    compile_with_timing(snarl, false).unwrap_or_else(|error| {
        log::error!("graph compile: {error:#}");
        Schedule::default()
    })
}

/// Prepare an entire plan on the control thread. On Err callers MUST retain the last good
/// schedule. Enabling PDC does not change the editing graph or session format.
pub fn compile_with_timing(snarl: &Snarl<GraphNode>, enabled: bool) -> Result<Schedule> {
    compile_with_timing_limits(snarl, enabled, TimingLimits::default())
}

pub fn compile_with_timing_limits(
    snarl: &Snarl<GraphNode>,
    enabled: bool,
    limits: TimingLimits,
) -> Result<Schedule> {
    let mut flat = Flat::default();
    flat.add(snarl, enabled)?;
    flat.schedule(enabled, limits)
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

    fn wire(snarl: &mut Snarl<GraphNode>, from: NodeId, out: usize, to: NodeId, inp: usize) {
        snarl.connect(OutPinId { node: from, output: out }, InPinId { node: to, input: inp });
    }

    #[test]
    fn groups_open_up_into_one_schedule() {
        let at = egui::pos2(0.0, 0.0);
        // Inside: In → Gain → Out, and In's MIDI → Out's MIDI.
        let mut inside = Snarl::new();
        let i = inside.insert_node(at, GraphNode { eid: 10, kind: NodeKind::GroupIn });
        let g = inside.insert_node(at, GraphNode { eid: 11, kind: NodeKind::Gain { db: 0.0 } });
        let o = inside.insert_node(at, GraphNode { eid: 12, kind: NodeKind::GroupOut });
        wire(&mut inside, i, 0, g, 0);
        wire(&mut inside, g, 0, o, 0);
        wire(&mut inside, i, 1, o, 1);
        // Outside: Player → group → Output, the Player's MIDI into the group.
        let mut snarl = Snarl::new();
        let p = snarl.insert_node(at, GraphNode { eid: 0, kind: NodeKind::Player });
        let group = GroupNode { name: "G".into(), snarl: inside };
        let gr = snarl.insert_node(at, GraphNode { eid: 1, kind: NodeKind::Group(Box::new(group)) });
        let out = snarl.insert_node(at, GraphNode { eid: 2, kind: NodeKind::output() });
        wire(&mut snarl, p, 0, gr, 0);
        wire(&mut snarl, p, 1, gr, 1);
        wire(&mut snarl, gr, 0, out, 0);

        let s = compile(&snarl);
        let step = |eid: u32| s.steps.iter().find(|st| st.node == eid).unwrap_or_else(|| panic!("no step for {eid}"));
        let pos = |eid: u32| s.steps.iter().position(|st| st.node == eid).unwrap();
        assert_eq!(step(1).inputs, vec![vec![(0, 0)]], "the group's slot takes the Player");
        assert_eq!(step(1).midi, vec![0]);
        assert_eq!(step(10).inputs, vec![vec![(1, 0)]], "the input node takes the group's slot");
        assert_eq!(step(10).midi, vec![1]);
        assert_eq!(step(11).inputs, vec![vec![(10, 0)]]);
        assert_eq!(step(12).inputs, vec![vec![(11, 0)]]);
        assert_eq!(step(12).midi, vec![10]);
        assert_eq!(step(2).inputs, vec![vec![(12, 0)]], "the Output takes the output node");
        assert_eq!(s.sinks, vec![2]);
        for (a, b) in [(0, 1), (1, 10), (10, 11), (11, 12), (12, 2)] {
            assert!(pos(a) < pos(b), "{a} before {b}");
        }
        assert_eq!(path_to(&snarl, 11), Some((vec![gr], g)));
    }

    #[test]
    fn strict_compile_rejects_cycles_and_invalid_ports_even_with_pdc_off() {
        let at = egui::pos2(0.0, 0.0);
        let mut snarl = Snarl::new();
        let a = snarl.insert_node(
            at,
            GraphNode {
                eid: 0,
                kind: NodeKind::Gain { db: 0.0 },
            },
        );
        let b = snarl.insert_node(
            at,
            GraphNode {
                eid: 1,
                kind: NodeKind::Gain { db: 0.0 },
            },
        );
        wire(&mut snarl, a, 0, b, 0);
        wire(&mut snarl, b, 0, a, 0);
        assert!(compile_with_timing(&snarl, false).is_ok());
        assert!(compile_with_timing(&snarl, true).err().unwrap().to_string().contains("cycle"));
        snarl.disconnect(
            OutPinId { node: b, output: 0 },
            InPinId { node: a, input: 0 },
        );
        wire(&mut snarl, a, 9, b, 0);
        assert!(compile_with_timing(&snarl, true).is_err());
    }

    #[test]
    fn pdc_compile_preserves_latest_group_expansion() {
        let at = egui::pos2(0.0, 0.0);
        let mut inside = Snarl::new();
        let i = inside.insert_node(
            at,
            GraphNode {
                eid: 10,
                kind: NodeKind::GroupIn,
            },
        );
        let o = inside.insert_node(
            at,
            GraphNode {
                eid: 11,
                kind: NodeKind::GroupOut,
            },
        );
        wire(&mut inside, i, 0, o, 0);
        wire(&mut inside, i, 1, o, 1);
        let mut snarl = Snarl::new();
        let p = snarl.insert_node(
            at,
            GraphNode {
                eid: 0,
                kind: NodeKind::Player,
            },
        );
        let group = snarl.insert_node(
            at,
            GraphNode {
                eid: 1,
                kind: NodeKind::Group(Box::new(GroupNode {
                    name: "G".into(),
                    snarl: inside,
                })),
            },
        );
        let out = snarl.insert_node(
            at,
            GraphNode {
                eid: 2,
                kind: NodeKind::output(),
            },
        );
        wire(&mut snarl, p, 0, group, 0);
        wire(&mut snarl, p, 1, group, 1);
        wire(&mut snarl, group, 0, out, 0);
        let legacy = compile(&snarl);
        let pdc = compile_with_timing(&snarl, true).unwrap();
        assert!(legacy.timing.is_none());
        assert_eq!(
            pdc.steps.iter().map(|s| s.node).collect::<Vec<_>>(),
            legacy.steps.iter().map(|s| s.node).collect::<Vec<_>>()
        );
        for (a, b) in pdc.steps.iter().zip(&legacy.steps) {
            assert_eq!(a.inputs, b.inputs);
            assert_eq!(a.midi, b.midi);
        }
        assert_eq!(pdc.steps.len(), 5);
        assert_eq!(pdc.sinks, legacy.sinks);
        assert!(pdc.epoch() > 0);
        assert_eq!(pdc.total_latency_frames(), 0);
    }

    #[test]
    fn intentional_delay_is_not_compensated_away() {
        let at = egui::pos2(0.0, 0.0);
        let mut snarl = Snarl::new();
        let p = snarl.insert_node(
            at,
            GraphNode {
                eid: 0,
                kind: NodeKind::Player,
            },
        );
        let d = snarl.insert_node(
            at,
            GraphNode {
                eid: 1,
                kind: NodeKind::delay(),
            },
        );
        let o = snarl.insert_node(
            at,
            GraphNode {
                eid: 2,
                kind: NodeKind::output(),
            },
        );
        wire(&mut snarl, p, 0, d, 0);
        wire(&mut snarl, d, 0, o, 0);
        assert_eq!(
            compile_with_timing(&snarl, true)
                .unwrap()
                .total_latency_frames(),
            0
        );
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
