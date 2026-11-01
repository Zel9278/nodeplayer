//! Control-thread-only weighted DAG analysis. Audio and MIDI entering a node share one
//! alignment group. Audio inherits algorithm + bridge latency; MIDI inherits bridge only.
//! Plugin-specific MIDI lookahead is not advertised by today's plugin API.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{Result, bail, ensure};

use super::MAX_NODES;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeTiming {
    pub algorithm_latency_frames: u64,
    pub bridge_latency_frames: u64,
    pub revision: u64,
}

impl NodeTiming {
    /// `total` includes the bridge; never count it twice.
    pub fn from_total(total: u32, bridge: u32) -> Result<Self> {
        ensure!(
            bridge <= total,
            "bridge latency {bridge} exceeds total latency {total}"
        );
        Ok(Self {
            algorithm_latency_frames: (total - bridge) as u64,
            bridge_latency_frames: bridge as u64,
            revision: 0,
        })
    }

    pub fn total(self) -> Result<u64> {
        self.algorithm_latency_frames
            .checked_add(self.bridge_latency_frames)
            .ok_or_else(|| anyhow::anyhow!("node latency overflow"))
    }
}

#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub id: u32,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub midi_input: bool,
    pub midi_output: bool,
    pub sink: bool,
    pub timing: NodeTiming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    Audio { output: u32, input: usize },
    Midi,
}

/// Stable identity includes the occurrence of a parallel wire. It is independent of the
/// topological order and plan epoch, allowing unchanged connections to retain history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeKey {
    pub from: u32,
    pub to: u32,
    pub kind: EdgeKind,
    pub occurrence: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct TimingLimits {
    pub max_latency_frames: u64,
    pub max_delay_bytes: usize,
    pub midi_capacity: usize,
}

impl Default for TimingLimits {
    fn default() -> Self {
        Self {
            max_latency_frames: 1 << 22,
            max_delay_bytes: 64 * 1024 * 1024,
            midi_capacity: 1 << 14,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeTiming {
    pub key: EdgeKey,
    pub delay_frames: u64,
}

#[derive(Clone, Debug, Default)]
pub struct TimingAnalysis {
    pub order: Vec<u32>,
    pub edges: Vec<EdgeTiming>,
    pub sinks: Vec<(u32, u64)>,
    pub output_audio: BTreeMap<u32, u64>,
    pub output_midi: BTreeMap<u32, u64>,
    pub total_latency_frames: u64,
    pub delay_bytes: usize,
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| anyhow::anyhow!("cumulative graph latency overflow"))
}

fn charge(bytes: &mut usize, amount: usize, limits: TimingLimits) -> Result<()> {
    *bytes = bytes
        .checked_add(amount)
        .ok_or_else(|| anyhow::anyhow!("delay allocation size overflow"))?;
    ensure!(
        *bytes <= limits.max_delay_bytes,
        "PDC delay memory budget exceeded ({} > {} bytes)",
        *bytes,
        limits.max_delay_bytes
    );
    Ok(())
}

pub fn analyze(
    nodes: &[NodeSpec],
    edges: &[EdgeKey],
    enabled: bool,
    limits: TimingLimits,
) -> Result<TimingAnalysis> {
    let mut specs = BTreeMap::new();
    for node in nodes {
        ensure!(
            (node.id as usize) < MAX_NODES,
            "engine node {} is out of range",
            node.id
        );
        ensure!(
            specs.insert(node.id, node).is_none(),
            "duplicate engine node {}",
            node.id
        );
    }
    let mut indegree: BTreeMap<u32, usize> = specs.keys().map(|id| (*id, 0)).collect();
    let mut outgoing: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    let mut incoming: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (index, edge) in edges.iter().enumerate() {
        let from = specs
            .get(&edge.from)
            .ok_or_else(|| anyhow::anyhow!("missing source node {}", edge.from))?;
        let to = specs
            .get(&edge.to)
            .ok_or_else(|| anyhow::anyhow!("missing destination node {}", edge.to))?;
        match edge.kind {
            EdgeKind::Audio { output, input } => ensure!(
                (output as usize) < from.audio_outputs && input < to.audio_inputs,
                "invalid audio port on edge {} -> {}",
                edge.from,
                edge.to
            ),
            EdgeKind::Midi => ensure!(
                from.midi_output && to.midi_input,
                "invalid MIDI port on edge {} -> {}",
                edge.from,
                edge.to
            ),
        }
        *indegree.get_mut(&edge.to).unwrap() += 1;
        outgoing.entry(edge.from).or_default().push(index);
        incoming.entry(edge.to).or_default().push(index);
    }
    let mut ready: VecDeque<u32> = nodes
        .iter()
        .map(|n| n.id)
        .filter(|id| indegree[id] == 0)
        .collect();
    let mut analysis = TimingAnalysis::default();
    while let Some(id) = ready.pop_front() {
        analysis.order.push(id);
        let node = specs[&id];
        let mut arrival = 0;
        for &i in incoming.get(&id).into_iter().flatten() {
            let edge = edges[i];
            let time = match edge.kind {
                EdgeKind::Audio { .. } => analysis.output_audio[&edge.from],
                EdgeKind::Midi => analysis.output_midi[&edge.from],
            };
            arrival = arrival.max(time);
        }
        let audio = if enabled {
            add(arrival, node.timing.total()?)?
        } else {
            0
        };
        let midi = if enabled {
            add(arrival, node.timing.bridge_latency_frames)?
        } else {
            0
        };
        ensure!(
            audio <= limits.max_latency_frames && midi <= limits.max_latency_frames,
            "node {id} cumulative latency exceeds {} frames",
            limits.max_latency_frames
        );
        analysis.output_audio.insert(id, audio);
        analysis.output_midi.insert(id, midi);
        for &i in incoming.get(&id).into_iter().flatten() {
            let key = edges[i];
            let time = match key.kind {
                EdgeKind::Audio { .. } => analysis.output_audio[&key.from],
                EdgeKind::Midi => analysis.output_midi[&key.from],
            };
            analysis.edges.push(EdgeTiming {
                key,
                delay_frames: arrival - time,
            });
        }
        for &i in outgoing.get(&id).into_iter().flatten() {
            let to = edges[i].to;
            let count = indegree.get_mut(&to).unwrap();
            *count -= 1;
            if *count == 0 {
                ready.push_back(to);
            }
        }
    }
    if analysis.order.len() != nodes.len() {
        bail!("graph contains a cycle; previous execution plan must be retained");
    }
    // Only device sinks set the final reference; disconnected slow branches do not.
    analysis.total_latency_frames = nodes
        .iter()
        .filter(|n| n.sink)
        .map(|n| analysis.output_audio[&n.id])
        .max()
        .unwrap_or(0);
    for node in nodes.iter().filter(|n| n.sink) {
        analysis.sinks.push((
            node.id,
            analysis.total_latency_frames - analysis.output_audio[&node.id],
        ));
    }
    analysis.edges.sort_by_key(|edge| edge.key);
    if enabled {
        ensure!(
            limits.midi_capacity > 0,
            "MIDI delay capacity must be nonzero"
        );
        for edge in &analysis.edges {
            let bytes = match edge.key.kind {
                EdgeKind::Audio { .. } => usize::try_from(edge.delay_frames)?
                    .checked_mul(2 * size_of::<f32>())
                    .ok_or_else(|| anyhow::anyhow!("audio delay size overflow"))?,
                EdgeKind::Midi => limits
                    .midi_capacity
                    .checked_mul(size_of::<super::compensation::TimedMidi>())
                    .ok_or_else(|| anyhow::anyhow!("MIDI delay size overflow"))?,
            };
            charge(&mut analysis.delay_bytes, bytes, limits)?;
        }
        for &(_, delay) in &analysis.sinks {
            let bytes = usize::try_from(delay)?
                .checked_mul(2 * size_of::<f32>())
                .ok_or_else(|| anyhow::anyhow!("sink delay size overflow"))?;
            charge(&mut analysis.delay_bytes, bytes, limits)?;
        }
    }
    Ok(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, latency: u64, sink: bool) -> NodeSpec {
        NodeSpec {
            id,
            audio_inputs: 2,
            audio_outputs: 1,
            midi_input: true,
            midi_output: true,
            sink,
            timing: NodeTiming {
                algorithm_latency_frames: latency,
                ..Default::default()
            },
        }
    }
    fn audio(from: u32, to: u32, input: usize) -> EdgeKey {
        EdgeKey {
            from,
            to,
            kind: EdgeKind::Audio { output: 0, input },
            occurrence: 0,
        }
    }
    fn run(nodes: &[NodeSpec], edges: &[EdgeKey]) -> Result<TimingAnalysis> {
        analyze(nodes, edges, true, TimingLimits::default())
    }
    #[test]
    fn diamond_and_fanout_are_per_connection() {
        let result = run(
            &[
                node(0, 0, false),
                node(1, 128, false),
                node(2, 512, false),
                node(3, 0, true),
                node(4, 0, true),
            ],
            &[
                audio(0, 1, 0),
                audio(0, 2, 0),
                audio(1, 3, 0),
                audio(2, 3, 1),
                audio(1, 4, 0),
            ],
        )
        .unwrap();
        assert_eq!(result.total_latency_frames, 512);
        assert_eq!(
            result
                .edges
                .iter()
                .find(|e| e.key == audio(1, 3, 0))
                .unwrap()
                .delay_frames,
            384
        );
        assert_eq!(
            result
                .edges
                .iter()
                .find(|e| e.key == audio(1, 4, 0))
                .unwrap()
                .delay_frames,
            0
        );
        assert_eq!(result.sinks, [(3, 0), (4, 384)]);
    }
    #[test]
    fn disconnected_latency_does_not_affect_device() {
        let result = run(
            &[node(0, 0, false), node(1, 128, true), node(2, 4096, false)],
            &[audio(0, 1, 0)],
        )
        .unwrap();
        assert_eq!(result.total_latency_frames, 128);
        assert_eq!(result.order.len(), 3);
    }
    #[test]
    fn midi_bridge_not_algorithm_is_inherited() {
        let mut first = node(0, 128, false);
        first.timing.bridge_latency_frames = 256;
        let key = EdgeKey {
            from: 0,
            to: 1,
            kind: EdgeKind::Midi,
            occurrence: 0,
        };
        let result = run(&[first, node(1, 0, true)], &[audio(0, 1, 0), key]).unwrap();
        assert_eq!(result.output_audio[&0], 384);
        assert_eq!(result.output_midi[&0], 256);
        assert_eq!(
            result
                .edges
                .iter()
                .find(|e| e.key == key)
                .unwrap()
                .delay_frames,
            128
        );
    }
    #[test]
    fn cycle_is_an_explicit_error() {
        assert!(
            run(
                &[node(0, 0, false), node(1, 0, false)],
                &[audio(0, 1, 0), audio(1, 0, 0)]
            )
            .unwrap_err()
            .to_string()
            .contains("cycle")
        );
    }
    #[test]
    fn invalid_port_duplicate_and_missing_nodes_are_errors() {
        assert!(run(&[node(0, 0, false)], &[audio(0, 0, 9)]).is_err());
        assert!(run(&[node(0, 0, false), node(0, 0, false)], &[]).is_err());
        assert!(run(&[node(0, 0, false)], &[audio(0, 1, 0)]).is_err());
    }
    #[test]
    fn checked_overflow_and_frame_limit() {
        let limits = TimingLimits {
            max_latency_frames: u64::MAX,
            ..Default::default()
        };
        assert!(
            analyze(
                &[node(0, u64::MAX, false), node(1, 1, true)],
                &[audio(0, 1, 0)],
                true,
                limits
            )
            .unwrap_err()
            .to_string()
            .contains("overflow")
        );
        assert!(run(&[node(0, 1 << 23, true)], &[]).is_err());
        assert!(NodeTiming::from_total(2, 3).is_err());
    }
    #[test]
    fn memory_budget_is_checked_before_allocation() {
        let limits = TimingLimits {
            max_delay_bytes: 8,
            ..Default::default()
        };
        assert!(
            analyze(
                &[node(0, 0, false), node(1, 128, false), node(2, 0, true)],
                &[audio(0, 2, 0), audio(1, 2, 1)],
                true,
                limits
            )
            .unwrap_err()
            .to_string()
            .contains("budget")
        );
    }
    #[test]
    fn off_is_zero_latency_even_with_invalid_latency_metadata() {
        let result = analyze(
            &[node(0, u64::MAX, false), node(1, 128, true)],
            &[audio(0, 1, 0)],
            false,
            TimingLimits::default(),
        )
        .unwrap();
        assert_eq!(result.total_latency_frames, 0);
        assert_eq!(result.delay_bytes, 0);
        assert!(result.edges.iter().all(|e| e.delay_frames == 0));
    }
    #[test]
    fn default_midi_capacity_matches_port_buffer() {
        assert_eq!(TimingLimits::default().midi_capacity, 1 << 14);
        assert_eq!(TimingLimits::default().midi_capacity, crate::engine::MidiBuf::CAPACITY);
    }

    #[test]
    fn parallel_wires_are_independent_and_sorted() {
        let a = audio(0, 1, 0);
        let b = EdgeKey { occurrence: 1, ..a };
        let result = run(&[node(0, 0, false), node(1, 0, true)], &[b, a]).unwrap();
        assert_eq!(result.edges.len(), 2);
        assert_eq!(result.edges[0].key, a);
    }
}
