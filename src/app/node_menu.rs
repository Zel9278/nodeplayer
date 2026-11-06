//! Type-aware wire-drop menu. Factories are deferred until selected: rendering the menu
//! must not open MIDI devices, load SoundFonts or instantiate plugins.

use super::*;

#[derive(Clone)]
pub(super) enum WireDrag {
    Outputs(Vec<OutPinId>),
    Inputs(Vec<InPinId>),
}

#[derive(Clone, Copy)]
pub(super) struct Ports {
    audio_in: bool,
    audio_out: bool,
    midi_in: bool,
    midi_out: bool,
}

impl Ports {
    fn of(kind: &NodeKind) -> Self {
        Self {
            audio_in: kind.audio_inputs() > 0,
            audio_out: kind.audio_outputs() > 0,
            midi_in: kind.midi_input(),
            midi_out: kind.midi_output(),
        }
    }
}

impl WireDrag {
    pub fn from_pins(pins: AnyPins<'_>) -> Self {
        match pins {
            AnyPins::Out(pins) => Self::Outputs(pins.to_vec()),
            AnyPins::In(pins) => Self::Inputs(pins.to_vec()),
        }
    }

    /// The signal types a newly connected node must accept (or produce for an input drag).
    pub fn requirements(&self, snarl: &Snarl<GraphNode>) -> Option<(bool, bool)> {
        let mut need = (false, false);
        let types: Vec<bool> = match self {
            Self::Outputs(pins) => pins
                .iter()
                .map(|pin| {
                    let kind = &snarl.get_node(pin.node)?.kind;
                    (pin.output < kind.outputs()).then(|| kind.is_midi_output(pin.output))
                })
                .collect::<Option<_>>()?,
            Self::Inputs(pins) => pins
                .iter()
                .map(|pin| {
                    let kind = &snarl.get_node(pin.node)?.kind;
                    (pin.input < kind.inputs()).then(|| kind.is_midi_input(pin.input))
                })
                .collect::<Option<_>>()?,
        };
        if types.is_empty() {
            return None;
        }
        for midi in types {
            if midi {
                need.1 = true;
            } else {
                need.0 = true;
            }
        }
        Some(need)
    }

    fn accepts(&self, need: (bool, bool), ports: Ports) -> bool {
        let available = match self {
            Self::Outputs(_) => (ports.audio_in, ports.midi_in),
            Self::Inputs(_) => (ports.audio_out, ports.midi_out),
        };
        (!need.0 || available.0) && (!need.1 || available.1)
    }

    fn connections(
        &self,
        snarl: &Snarl<GraphNode>,
        node: NodeId,
    ) -> Option<Vec<(OutPinId, InPinId)>> {
        let kind = &snarl.get_node(node)?.kind;
        if !self.accepts(self.requirements(snarl)?, Ports::of(kind)) {
            return None;
        }
        match self {
            Self::Outputs(pins) => pins
                .iter()
                .map(|from| {
                    if graph::would_cycle(snarl, from.node, node) {
                        return None;
                    }
                    let midi = snarl[from.node].kind.is_midi_output(from.output);
                    let matching =
                        || (0..kind.inputs()).filter(|&index| kind.is_midi_input(index) == midi);
                    let input = matching()
                        .find(|&input| snarl.in_pin(InPinId { node, input }).remotes.is_empty())
                        .or_else(|| matching().next())?;
                    Some((*from, InPinId { node, input }))
                })
                .collect(),
            Self::Inputs(pins) => pins
                .iter()
                .map(|to| {
                    if graph::would_cycle(snarl, node, to.node) {
                        return None;
                    }
                    let midi = snarl[to.node].kind.is_midi_input(to.input);
                    let output =
                        (0..kind.outputs()).find(|&index| kind.is_midi_output(index) == midi)?;
                    Some((OutPinId { node, output }, *to))
                })
                .collect(),
        }
    }

    pub fn connect(&self, snarl: &mut Snarl<GraphNode>, node: NodeId) -> bool {
        let Some(wires) = self.connections(snarl, node) else {
            return false;
        };
        for (from, to) in wires {
            snarl.connect(from, to);
        }
        true
    }
}

#[derive(Clone, Copy)]
pub(super) enum Builtin {
    Player,
    Output,
    Gain,
    Pan,
    RateShift,
    Eq,
    Delay,
    Mixer,
    Limiter,
    Group,
    GroupIn,
    GroupOut,
    Send,
    Receive,
    MidiIn,
    MidiOut,
    MidiMonitor,
    CSynth,
    Audio2Midi,
    Visual(VisKind),
}

impl Builtin {
    fn label(self) -> &'static str {
        match self {
            Self::Player => "Player",
            Self::Output => "Output",
            Self::Gain => "Gain",
            Self::Pan => "Pan",
            Self::RateShift => "Playback Rate Shift",
            Self::Eq => "EQ",
            Self::Delay => "Delay",
            Self::Mixer => "Mixer",
            Self::Limiter => "Kazu Limiter",
            Self::Group => "Group",
            Self::GroupIn => "Group input",
            Self::GroupOut => "Group output",
            Self::Send => "Send",
            Self::Receive => "Receive",
            Self::MidiIn => "MIDI In",
            Self::MidiOut => "MIDI Out",
            Self::MidiMonitor => "MIDI Monitor",
            Self::CSynth => "CSynth",
            Self::Audio2Midi => "Audio → MIDI",
            Self::Visual(kind) => kind.label(),
        }
    }

    fn ports(self) -> Ports {
        use Builtin::*;
        Ports {
            audio_in: !matches!(
                self,
                Player | GroupIn | Receive | MidiIn | MidiOut | MidiMonitor | CSynth
            ),
            audio_out: !matches!(
                self,
                Output | GroupOut | Send | MidiIn | MidiOut | MidiMonitor
            ),
            midi_in: matches!(
                self,
                Group | GroupOut | Send | MidiOut | MidiMonitor | CSynth
            ),
            midi_out: matches!(
                self,
                Player | Group | GroupIn | Receive | MidiIn | MidiMonitor | Audio2Midi
            ),
        }
    }
}

pub(super) enum NewNode {
    Builtin(Builtin),
    Plugin(PluginInfo),
}

impl NewNode {
    pub fn create(self, app: &mut App, pos: Pos2) -> Option<NodeId> {
        let choice = match self {
            Self::Builtin(choice) => choice,
            Self::Plugin(info) => return app.add_plugin(&info, pos),
        };
        let sr = app.sample_rate;
        let kind = match choice {
            Builtin::Group => return app.add_group(pos),
            Builtin::Player => NodeKind::Player,
            Builtin::Output => NodeKind::output(),
            Builtin::Gain => NodeKind::Gain { db: 0.0 },
            Builtin::Pan => NodeKind::Pan { pan: 0.0 },
            Builtin::RateShift => NodeKind::RateShift {
                rate: 1.0,
                bypass: false,
                active: false,
            },
            Builtin::Eq => NodeKind::eq(EqSettings::default()),
            Builtin::Delay => NodeKind::delay(),
            Builtin::Mixer => NodeKind::Mixer {
                gains_db: [0.0; MIXER_INPUTS],
            },
            Builtin::Limiter => NodeKind::limiter(),
            Builtin::GroupIn => NodeKind::GroupIn,
            Builtin::GroupOut => NodeKind::GroupOut,
            Builtin::MidiIn => NodeKind::MidiIn(Box::new(SeqNode::new(Dir::In))),
            Builtin::MidiOut => NodeKind::MidiOut(Box::new(SeqNode::new(Dir::Out))),
            Builtin::MidiMonitor => NodeKind::MidiMonitor(Box::new(Monitor::new())),
            Builtin::CSynth => NodeKind::CSynth(Box::new(CSynthNode::new(Default::default(), sr))),
            Builtin::Audio2Midi => NodeKind::Audio2Midi(Box::new(Audio2Midi::new(sr))),
            Builtin::Visual(kind) => NodeKind::Visual(Box::new(Visualizer::new(kind))),
            Builtin::Send | Builtin::Receive => {
                let mut names = std::collections::BTreeSet::new();
                graph::send_names(&app.snarl, &mut names);
                for level in &app.outer {
                    graph::send_names(&level.snarl, &mut names);
                }
                if matches!(choice, Builtin::Send) {
                    NodeKind::Send {
                        name: (1..)
                            .map(|i| format!("bus {i}"))
                            .find(|name| !names.contains(name))
                            .unwrap(),
                    }
                } else {
                    NodeKind::Receive {
                        name: names.into_iter().next().unwrap_or_else(|| "bus 1".into()),
                    }
                }
            }
        };
        app.add_node(kind, pos)
    }
}

pub(super) fn show(
    viewer: &mut Viewer<'_>,
    pos: Pos2,
    ui: &mut egui::Ui,
    wires: WireDrag,
    snarl: &Snarl<GraphNode>,
) {
    let Some(need) = wires.requirements(snarl) else {
        return;
    };
    ui.label(match need {
        (true, true) => "Audio + MIDI connection",
        (false, true) => "MIDI connection",
        _ => "Audio connection",
    });
    let filter_id = ui.id().with("wire candidate filter");
    let mut filter = ui
        .data(|data| data.get_temp::<String>(filter_id))
        .unwrap_or_default();
    ui.add(egui::TextEdit::singleline(&mut filter).hint_text("Find a node"));
    ui.data_mut(|data| data.insert_temp(filter_id, filter.clone()));
    let filter = filter.to_lowercase();
    ui.label("Add and connect");
    egui::ScrollArea::vertical()
        .id_salt("new wire nodes")
        .max_height(280.0)
        .show(ui, |ui| {
            use Builtin::*;
            let mut builtins = vec![
                Player,
                Output,
                Gain,
                Pan,
                RateShift,
                Eq,
                Delay,
                Mixer,
                Limiter,
                Group,
                GroupIn,
                GroupOut,
                Send,
                Receive,
                MidiIn,
                MidiOut,
                MidiMonitor,
                CSynth,
                Audio2Midi,
            ];
            builtins.extend(VisKind::ALL.into_iter().map(Visual));
            for choice in builtins {
                if matches!(choice, Player) && viewer.has_player {
                    continue;
                }
                if matches!(choice, GroupIn) && (!viewer.in_group || viewer.group_io.0) {
                    continue;
                }
                if matches!(choice, GroupOut) && (!viewer.in_group || viewer.group_io.1) {
                    continue;
                }
                if wires.accepts(need, choice.ports())
                    && choice.label().to_lowercase().contains(&filter)
                    && ui.button(choice.label()).clicked()
                {
                    viewer.actions.push(Action::AddWired(
                        NewNode::Builtin(choice),
                        pos,
                        wires.clone(),
                    ));
                    ui.close();
                }
            }
            // All plugin nodes expose stereo audio pins. MIDI is known for JSFX and for
            // plugins already instantiated in this graph; don't guess an unprobed event bus.
            let loaded_ports: HashMap<_, _> = snarl
                .nodes()
                .filter_map(|node| {
                    let NodeKind::Plugin(plugin) = &node.kind else {
                        return None;
                    };
                    let info = plugin.info();
                    Some((
                        (info.format, info.path.as_path(), info.id.as_str()),
                        Ports::of(&node.kind),
                    ))
                })
                .collect();
            let mut unprobed = Vec::new();
            for info in viewer.plugins {
                let loaded = loaded_ports
                    .get(&(info.format, info.path.as_path(), info.id.as_str()))
                    .copied();
                let ports = loaded.unwrap_or(Ports {
                    audio_in: true,
                    audio_out: true,
                    midi_in: info.format == Format::Jsfx,
                    midi_out: info.format == Format::Jsfx,
                });
                if need.1
                    && loaded.is_none()
                    && info.format != Format::Jsfx
                    && info.name.to_lowercase().contains(&filter)
                {
                    unprobed.push(info);
                }
                if wires.accepts(need, ports)
                    && info.name.to_lowercase().contains(&filter)
                    && ui
                        .button(format!("{} ({})", info.name, info.format.label()))
                        .clicked()
                {
                    viewer.actions.push(Action::AddWired(
                        NewNode::Plugin(info.clone()),
                        pos,
                        wires.clone(),
                    ));
                    ui.close();
                }
            }
            if !unprobed.is_empty() {
                ui.menu_button("Other plugins…", |ui| {
                    ui.weak("MIDI ports are checked when the plugin is loaded.");
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for info in unprobed {
                                if ui
                                    .button(format!("{} ({})", info.name, info.format.label()))
                                    .clicked()
                                {
                                    viewer.actions.push(Action::AddWired(
                                        NewNode::Plugin(info.clone()),
                                        pos,
                                        wires.clone(),
                                    ));
                                    ui.close();
                                }
                            }
                        });
                });
            }
        });
    ui.separator();
    ui.label(if matches!(wires, WireDrag::Outputs(_)) {
        "Connect to an existing node"
    } else {
        "Connect from an existing node"
    });
    egui::ScrollArea::vertical()
        .id_salt("existing wire nodes")
        .max_height(160.0)
        .show(ui, |ui| {
            for (id, node) in snarl.node_ids() {
                if node.kind.title().to_lowercase().contains(&filter)
                    && wires.connections(snarl, id).is_some()
                    && ui
                        .button(format!("{} · #{}", node.kind.title(), node.eid))
                        .clicked()
                {
                    viewer.actions.push(Action::ConnectWired(id, wires.clone()));
                    ui.close();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(snarl: &mut Snarl<GraphNode>, kind: NodeKind) -> NodeId {
        let eid = snarl.nodes().count() as u32;
        snarl.insert_node(pos2(0.0, 0.0), GraphNode { eid, kind })
    }

    #[test]
    fn wire_drop_uses_audio_and_midi_pin_indices_in_both_directions() {
        let mut snarl = Snarl::new();
        let player = node(&mut snarl, NodeKind::Player);
        let gain = node(&mut snarl, NodeKind::Gain { db: 0.0 });
        let midi = node(&mut snarl, NodeKind::GroupOut);
        let audio_wire = WireDrag::Outputs(vec![OutPinId {
            node: player,
            output: 0,
        }]);
        assert!(audio_wire.connect(&mut snarl, gain));
        let midi_wire = WireDrag::Outputs(vec![OutPinId {
            node: player,
            output: 1,
        }]);
        assert!(!midi_wire.connect(&mut snarl, gain));
        assert!(midi_wire.connect(&mut snarl, midi));
        assert!(
            snarl
                .wires()
                .any(|(from, to)| from.output == 1 && to.node == midi && to.input == 1)
        );
        let reverse = WireDrag::Inputs(vec![InPinId {
            node: midi,
            input: 1,
        }]);
        assert!(!reverse.connect(&mut snarl, gain));
        assert!(reverse.connect(&mut snarl, player));
    }

    #[test]
    fn wire_drop_rejects_cycles_invalid_pins_and_partial_mixed_connections() {
        let mut snarl = Snarl::new();
        let player = node(&mut snarl, NodeKind::Player);
        let a = node(&mut snarl, NodeKind::Gain { db: 0.0 });
        let b = node(&mut snarl, NodeKind::Gain { db: 0.0 });
        snarl.connect(
            OutPinId { node: a, output: 0 },
            InPinId { node: b, input: 0 },
        );
        assert!(!WireDrag::Outputs(vec![OutPinId { node: b, output: 0 }]).connect(&mut snarl, a));
        assert!(
            !WireDrag::Outputs(vec![OutPinId {
                node: player,
                output: 99
            }])
            .connect(&mut snarl, a)
        );
        let count = snarl.wires().count();
        let mixed = WireDrag::Outputs(vec![
            OutPinId {
                node: player,
                output: 0,
            },
            OutPinId {
                node: player,
                output: 1,
            },
        ]);
        assert!(!mixed.connect(&mut snarl, a));
        assert_eq!(snarl.wires().count(), count);
        let group_out = node(&mut snarl, NodeKind::GroupOut);
        assert!(mixed.connect(&mut snarl, group_out));
        assert_eq!(snarl.wires().count(), count + 2);
    }
}
