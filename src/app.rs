use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crossbeam_channel::Receiver;
use egui::{Color32, Pos2, pos2};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlStyle, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, NodeId, OutPin, OutPinId, InPinId, Snarl};

use crate::engine::eq::EqSettings;
use crate::engine::nodes::{Delay, LimiterParams, MIXER_INPUTS, MidiBridge, PluginProc};
use crate::engine::{Command, Engine, Garbage, MAX_BLOCK, NodeSlot};
use crate::audio2midi::Audio2Midi;
use crate::csynth_node::CSynthNode;
use crate::lyrics::LyricsNode;
use crate::video::VideoNode;
use crate::midi_file_node::MidiFileNode;
use crate::graph::{self, GraphNode, IdAlloc, NodeKind, PluginNode};
use crate::library::{self, AUDIO_EXTS, Library};
use crate::midi::{Dir, Monitor, SeqNode};
use crate::misskey::Misskey;
use crate::mpris::{self, Mpris};
use crate::player::Player;
use crate::session::{self, SavedGraph, SavedKind, SavedNode, Session};
use crate::visual::{VisKind, Visualizer};
use crate::native::ext::{Command as ExtCommand, Extensions};
use crate::plugins::guard;
use crate::plugins::{self, Format, Placement, PluginInfo, PxRect};
use crate::yabridge::{self, Yabridge};

enum ScanMsg {
    Progress(String),
    Found(Vec<PluginInfo>),
    Failed(PathBuf, String),
    Done,
}

/// The panels, docked side by side or as tabs as the user arranges them. The graph is always
/// there; the others can be closed (right-click the tab) and opened again from a tab bar's "+".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum Tab {
    Graph,
    Library,
    Presets,
    Plugins,
    Extensions,
    Misskey,
    Discord,
    Info,
}

impl Tab {
    pub(crate) const ALL: [Self; 8] =
        [Self::Graph, Self::Library, Self::Presets, Self::Plugins, Self::Extensions, Self::Misskey, Self::Discord, Self::Info];

    fn title(self) -> &'static str {
        match self {
            Self::Graph => "Graph",
            Self::Library => "🎵 Library",
            Self::Presets => "💾 Presets",
            Self::Plugins => "🔌 Plugins",
            Self::Extensions => "⚙ Extensions",
            Self::Misskey => "📢 Misskey",
            Self::Discord => "🎮 Discord",
            Self::Info => "ℹ Now playing",
        }
    }
}

/// Puts a panel this version added into a layout saved before it: the "Now playing" panel
/// under the library, others with the library's tabs.
fn add_new_tab(dock: &mut egui_dock::DockState<Tab>, tab: Tab) {
    let library = dock.find_main_surface_tab(&Tab::Library).map(|(node, _)| node);
    match (tab, library) {
        (Tab::Info, Some(node)) => {
            dock.main_surface_mut().split_below(node, 0.62, vec![tab]);
        }
        (_, Some(node)) => {
            dock.set_focused_node_and_surface(egui_dock::NodePath { surface: egui_dock::SurfaceIndex::main(), node });
            dock.push_to_focused_leaf(tab);
        }
        _ => dock.push_to_first_leaf(tab),
    }
}

/// The layout before any rearranging: the side tabs left of the graph.
fn default_dock(extensions: bool) -> egui_dock::DockState<Tab> {
    let mut dock = egui_dock::DockState::new(vec![Tab::Graph]);
    let mut side = vec![Tab::Library, Tab::Presets, Tab::Plugins, Tab::Misskey, Tab::Discord];
    if extensions {
        side.push(Tab::Extensions);
    }
    let [_, left] = dock.main_surface_mut().split_left(egui_dock::NodeIndex::root(), 0.35, side);
    dock.main_surface_mut().split_below(left, 0.62, vec![Tab::Info]);
    // The library in front.
    dock.set_focused_node_and_surface(egui_dock::NodePath { surface: egui_dock::SurfaceIndex::main(), node: left });
    dock
}

/// Shows the docked panels: the app's own for each tab.
struct Tabs<'a> {
    app: &'a mut App,
    actions: &'a mut Vec<Action>,
    /// Closed tabs, offered by the "+" buttons.
    closed: Vec<Tab>,
    /// A closed tab picked from a "+" button, and where.
    reopen: Option<(egui_dock::NodePath, Tab)>,
}

impl egui_dock::TabViewer for Tabs<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(("dock tab", *tab))
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let app = &mut *self.app;
        match tab {
            Tab::Graph => app.graph_ui(ui, self.actions),
            Tab::Library => app.library_panel(ui),
            Tab::Presets => app.presets_panel(ui),
            Tab::Plugins => app.plugin_panel(ui, self.actions),
            Tab::Extensions => app.extensions_panel(ui),
            Tab::Misskey => app.misskey.ui(ui, &app.player.status()),
            Tab::Discord => app.discord.ui(ui, &app.player.status()),
            Tab::Info => {
                let status = app.player.status();
                let plays = status.path.as_deref().map(|p| app.library.play_stats(p)).unwrap_or_default();
                let now = crate::info::Now {
                    status: &status,
                    playing: app.player.is_playing(),
                    finished: app.player.finished(),
                    position: app.player.position(),
                    rate: app.player.rate(),
                    output: app.engine.as_ref().map(|e| (e.device_name.as_str(), e.sample_rate, e.channels as u16)),
                    plays,
                };
                app.info.ui(ui, now);
            }
        }
    }

    fn is_closeable(&self, tab: &Tab) -> bool {
        *tab != Tab::Graph
    }

    // Plugin editors are embedded into the graph's place in the main window.
    fn allowed_in_windows(&self, tab: &mut Tab) -> bool {
        *tab != Tab::Graph
    }

    // The panels scroll by themselves where they need to.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    fn add_popup(&mut self, ui: &mut egui::Ui, path: egui_dock::NodePath) {
        if self.closed.is_empty() {
            ui.weak("Every panel is open.");
        }
        for tab in &self.closed {
            if ui.button(tab.title()).clicked() {
                self.reopen = Some((path, *tab));
                ui.close();
            }
        }
    }
}

/// How often the session is written while running (it's also written on exit).
const AUTOSAVE: Duration = Duration::from_secs(30);

enum Action {
    Add(NodeKind, Pos2),
    AddPlugin(PluginInfo, Pos2),
    Remove(NodeId),
    Changed,
    /// Ask for a name, then save the selected nodes as a node preset.
    NameNodePreset,
    AddNodePreset(String, Pos2),
}

pub struct App {
    // Declared first so the audio stream stops before any plugin is torn down.
    engine: Option<Engine>,
    sample_rate: f32,
    player: Player,
    snarl: Snarl<GraphNode>,
    style: SnarlStyle,
    ids: IdAlloc,
    dirty: bool,
    host: plugins::Host,
    plugins: Vec<PluginInfo>,
    plugin_filter: String,
    scan_rx: Option<Receiver<ScanMsg>>,
    scan_progress: String,
    pending_teardown: HashMap<u32, Box<PluginNode>>,
    /// Plugin nodes whose processor is being swapped out so the plugin can restart.
    restarting: HashSet<u32>,
    yabridge: Yabridge,
    messages: Vec<String>,
    seek_drag: Option<f64>,
    /// The transport's playback rate; Rate Shift nodes multiply it.
    rate: f32,
    added: usize,
    /// The main window's X11 id when running under X11; editors are embedded into it.
    host_xid: Option<crate::plugins::WindowId>,
    debug_done: bool,
    library: Library,
    dock: egui_dock::DockState<Tab>,
    /// Where embedded plugin editors go this frame, from the graph tab.
    placements: HashMap<u32, (PxRect, (i32, i32))>,
    /// Scroll the track list to the current track on the next frame.
    reveal_current: bool,
    mpris: Option<Mpris>,
    misskey: Misskey,
    discord: crate::discord::Discord,
    info: crate::info::Info,
    play_counter: library::PlayCounter,
    /// First-run questions: downloading FFmpeg, installing the AppImage.
    setup: crate::setup::Setup,
    /// Whether FFmpeg was found, and when that was checked.
    ffmpeg: (bool, Instant),
    /// Restored plugin nodes whose editor was open; reopened once the window exists.
    pending_editors: Vec<NodeId>,
    last_save: Instant,
    shut_down: bool,
    preset_name: String,
    presets: Vec<String>,
    node_presets: Vec<String>,
    node_preset_name: String,
    /// The nodes being saved as a node preset while its name is asked for.
    naming: Option<Vec<NodeId>>,
    /// Node preset waiting for a second click to be deleted.
    confirm_node: Option<String>,
    /// Nodes selected in the graph (Shift+drag, Shift+click), as of the last frame.
    selected: Vec<NodeId>,
    /// The zoom button was clicked: back to 100% on the next frame.
    reset_zoom: bool,
    /// The graph's zoom last frame.
    zoom: f32,
    /// Preset (or "" for resetting the graph) waiting for a second click to confirm.
    confirm: Option<(String, bool)>,
    /// Track restored from the session, until the player reports it as loaded.
    restored_track: Option<PathBuf>,
    /// The graph view exists (its first frame fits the nodes into view).
    graph_shown: bool,
    /// An EQ point was under the pointer last frame; the mouse wheel goes to it instead of
    /// panning the graph.
    capture_scroll: bool,
    /// App extensions from native plugins.
    extensions: Extensions,
    /// What extensions were last told: current track and whether it played.
    ext_seen: (Option<PathBuf>, bool),
    /// The library revision the extensions' snapshot was taken from.
    ext_library: u64,
    track_ended: bool,
    library_changed: bool,
    /// Shown until dismissed: a plugin crashed the last run and was disabled.
    crash_notice: Option<String>,
}

impl App {
    pub fn new(initial_file: Option<PathBuf>, ctx: &egui::Context) -> Self {
        let mut messages = Vec::new();
        let engine = match Engine::start() {
            Ok(e) => Some(e),
            Err(e) => {
                messages.push(format!("audio output unavailable: {e:#}"));
                None
            }
        };
        let sample_rate = engine.as_ref().map_or(48_000, |e| e.sample_rate);
        let mut app = Self {
            player: Player::new(sample_rate),
            engine,
            sample_rate: sample_rate as f32,
            snarl: Snarl::new(),
            // Editors that support scaling follow the zoom; others stay at their own size.
            style: SnarlStyle { collapsible: Some(true), max_scale: Some(2.0), ..SnarlStyle::new() },
            ids: IdAlloc::default(),
            dirty: true,
            host: plugins::Host::default(),
            plugins: plugins::load_cache(),
            plugin_filter: String::new(),
            scan_rx: None,
            scan_progress: String::new(),
            pending_teardown: HashMap::new(),
            restarting: HashSet::new(),
            yabridge: Yabridge::default(),
            messages,
            seek_drag: None,
            rate: 1.0,
            added: 0,
            host_xid: None,
            debug_done: false,
            library: Library::default(),
            dock: default_dock(false),
            placements: HashMap::new(),
            reveal_current: false,
            // On Windows it needs the window, so it's made with the first frame.
            mpris: if cfg!(windows) { None } else { Mpris::new(ctx.clone(), None) },
            misskey: Misskey::new(ctx.clone()),
            discord: crate::discord::Discord::new(ctx.clone()),
            info: crate::info::Info::new(),
            play_counter: library::PlayCounter::default(),
            setup: crate::setup::Setup::new(),
            ffmpeg: (crate::av::available(), Instant::now()),
            pending_editors: Vec::new(),
            last_save: Instant::now(),
            shut_down: false,
            preset_name: String::new(),
            presets: session::list_presets(),
            node_presets: session::list_node_presets(),
            node_preset_name: String::new(),
            confirm_node: None,
            naming: None,
            selected: Vec::new(),
            reset_zoom: false,
            zoom: 1.0,
            confirm: None,
            restored_track: None,
            graph_shown: false,
            capture_scroll: false,
            extensions: Extensions::default(),
            ext_seen: (None, false),
            ext_library: u64::MAX,
            track_ended: false,
            library_changed: false,
            crash_notice: None,
        };
        app.check_crash();

        let saved = session::load_session().unwrap_or_default();
        match saved.graph.as_ref().filter(|g| !g.nodes.is_empty()) {
            Some(g) => app.load_graph(g),
            None => app.default_graph(),
        }
        app.library.repeat = saved.repeat.unwrap_or(app.library.repeat);
        app.library.shuffle = saved.shuffle;
        app.rate = saved.rate.unwrap_or(1.0);
        app.library.sort = saved.library_sort;
        app.library.columns = saved.library_columns.clone().unwrap_or_default();
        match initial_file {
            Some(f) if f.is_dir() => app.library.open(f),
            Some(f) => app.load_file(&f),
            None => {
                if !saved.library_sources.is_empty() {
                    app.library.set_sources(saved.library_sources.clone());
                } else if let Some(dir) = saved.library_root.clone().filter(|d| d.is_dir()).or_else(library::load_last_folder) {
                    app.library.open(dir);
                }
                // Bring back the last track, paused where it was.
                if let Some(track) = saved.track.filter(|t| t.is_file()) {
                    app.player.load(&track);
                    app.player.set_playing(false);
                    app.restored_track = Some(track);
                    if saved.position > 0.0 {
                        app.player.seek(saved.position);
                    }
                }
            }
        }
        let (extensions, errors) = Extensions::load(&mut app.host.native, &app.host.disabled);
        app.extensions = extensions;
        app.dock = match saved.dock.clone() {
            Some(mut d) if d.find_tab(&Tab::Graph).is_some() => {
                if app.extensions.panels().is_empty() {
                    d.retain_tabs(|t| *t != Tab::Extensions);
                }
                // Panels added since the layout was saved (a panel closed then stays closed).
                for tab in [Tab::Discord, Tab::Info] {
                    if !saved.tabs_seen.contains(&tab) && d.find_tab(&tab).is_none() {
                        add_new_tab(&mut d, tab);
                    }
                }
                // Adding a tab brings it to the front; keep the library there.
                if let Some(path) = d.find_tab(&Tab::Library) {
                    let _ = d.set_active_tab(path);
                }
                d
            }
            _ => default_dock(!app.extensions.panels().is_empty()),
        };
        for e in errors {
            app.log(e);
        }
        app
    }

    /// Disables the plugin the previous run crashed in, and says so.
    fn check_crash(&mut self) {
        self.host.disabled = guard::disabled();
        let Some((format, path)) = guard::take_crash() else { return };
        if !self.host.is_disabled(format, &path) {
            self.host.disabled.push((format, path.clone()));
            guard::set_disabled(&self.host.disabled);
        }
        let name = self
            .plugins
            .iter()
            .find(|p| p.format == format && p.path == path)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().into_owned());
        let msg = format!(
            "nodeplayer crashed inside the {} plugin \"{name}\" last time, so it's disabled for now. You can enable it again in the 🔌 Plugins tab.",
            format.label()
        );
        log::warn!("{msg} ({})", path.display());
        self.crash_notice = Some(msg);
    }

    /// Keeps the extensions' view of the player current, tells them what changed, and
    /// carries out what they asked for.
    fn update_extensions(&mut self) {
        if self.extensions.is_empty() {
            self.track_ended = false;
            self.library_changed = false;
            return;
        }
        let status = self.player.status();
        let playing = self.player.is_playing();
        {
            let mut st = self.extensions.state();
            st.has_track = status.path.is_some();
            st.playing = playing;
            st.position = self.player.position();
            st.duration = status.duration.unwrap_or(0.0);
            st.track_index = self.library.current;
            st.path = status.path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            st.title = status.title().unwrap_or_default();
            st.artist = status.tags.artist.clone().unwrap_or_default();
            st.album = status.tags.album.clone().unwrap_or_default();
            if self.library.revision != self.ext_library {
                st.library_root = self.library.root().map(|p| p.to_string_lossy().into_owned());
                st.library = self.library.tracks.iter().map(|t| t.path.to_string_lossy().into_owned()).collect();
                self.ext_library = self.library.revision;
            }
        }
        use nodeplayer_plugin::abi::*;
        if std::mem::take(&mut self.library_changed) {
            self.extensions.event(EVENT_LIBRARY_CHANGED);
        }
        if std::mem::take(&mut self.track_ended) {
            self.extensions.event(EVENT_TRACK_ENDED);
        }
        if status.path != self.ext_seen.0 {
            self.ext_seen.0 = status.path.clone();
            if status.path.is_some() {
                self.extensions.event(EVENT_TRACK_CHANGED);
            }
        }
        if playing != self.ext_seen.1 {
            self.ext_seen.1 = playing;
            self.extensions.event(if playing { EVENT_PLAYBACK_STARTED } else { EVENT_PLAYBACK_PAUSED });
        }
        self.extensions.idle();
        self.run_extension_commands();
    }

    fn run_extension_commands(&mut self) {
        for c in self.extensions.take_commands() {
            match c {
                ExtCommand::Play => self.play(),
                ExtCommand::Pause => self.player.set_playing(false),
                ExtCommand::Toggle if self.player.is_playing() => self.player.set_playing(false),
                ExtCommand::Toggle => self.play(),
                ExtCommand::Stop => self.stop(),
                ExtCommand::Next => self.next_track(),
                ExtCommand::Previous => self.previous_track(),
                ExtCommand::Seek(t) => self.seek_to(t),
                ExtCommand::LoadFile(p) if p.is_file() => self.load_file(&p),
                ExtCommand::LoadFile(p) => self.log(format!("no such file: {}", p.display())),
                ExtCommand::PlayTrack(i) => self.play_track(i),
                ExtCommand::OpenFolder(p) if p.is_dir() => self.library.open(p),
                ExtCommand::OpenFolder(p) => self.log(format!("no such folder: {}", p.display())),
                ExtCommand::LoadPreset(name) => match session::load_preset(&name) {
                    Ok(g) => {
                        self.load_graph(&g);
                        self.reopen_editors();
                        self.log(format!("loaded preset \"{name}\""));
                    }
                    Err(e) => self.log(format!("can't load preset: {e:#}")),
                },
                ExtCommand::SavePreset(name) => {
                    if let Err(e) = session::save_preset(&name, &self.save_graph()) {
                        self.log(format!("can't save preset: {e:#}"));
                    }
                    self.presets = session::list_presets();
                }
                ExtCommand::SetVolume(db) => {
                    let mut sends = Vec::new();
                    for (_, n) in self.snarl.nodes_ids_mut() {
                        if let NodeKind::Output { volume_db, .. } = &mut n.kind {
                            *volume_db = db;
                            sends.push(n.eid);
                        }
                    }
                    if let Some(e) = &mut self.engine {
                        for eid in sends {
                            e.send(Command::SetParam(eid, 0, db));
                        }
                    }
                }
                ExtCommand::Message(m) => self.log(m),
            }
        }
    }

    fn extensions_panel(&mut self, ui: &mut egui::Ui) {
        let panels = self.extensions.panels();
        if panels.is_empty() {
            ui.weak("No extension draws a panel.");
        }
        egui::ScrollArea::vertical().id_salt("extensions").auto_shrink([false, false]).show(ui, |ui| {
            for (i, name, title) in panels {
                egui::CollapsingHeader::new(&title)
                    .id_salt(("ext", i))
                    .default_open(true)
                    .show(ui, |ui| self.extensions.draw_panel(i, ui))
                    .header_response
                    .on_hover_text(name);
            }
        });
        self.run_extension_commands();
    }

    fn default_graph(&mut self) {
        let p = self.add_node(NodeKind::Player, pos2(0.0, 0.0));
        let o = self.add_node(NodeKind::output(), pos2(260.0, 0.0));
        if let (Some(p), Some(o)) = (p, o) {
            self.snarl.connect(OutPinId { node: p, output: 0 }, InPinId { node: o, input: 0 });
        }
    }

    fn clear_graph(&mut self) {
        self.pending_editors.clear();
        let ids: Vec<NodeId> = self.snarl.node_ids().map(|(id, _)| id).collect();
        for id in ids {
            self.remove_node(id);
        }
    }

    fn save_graph(&self) -> SavedGraph {
        self.save_nodes(None)
    }

    /// The nodes in `only` (all when `None`) and the wires between them.
    fn save_nodes(&self, only: Option<&HashSet<NodeId>>) -> SavedGraph {
        let mut index = HashMap::new();
        let mut nodes = Vec::new();
        for (id, n) in self.snarl.nodes_ids_data() {
            if only.is_some_and(|o| !o.contains(&id)) {
                continue;
            }
            let kind = match &n.value.kind {
                NodeKind::Player => SavedKind::Player,
                NodeKind::Output { volume_db, .. } => SavedKind::Output { volume_db: *volume_db },
                NodeKind::Gain { db } => SavedKind::Gain { db: *db },
                NodeKind::Pan { pan } => SavedKind::Pan { pan: *pan },
                NodeKind::RateShift { rate, bypass, .. } => SavedKind::RateShift { rate: *rate, bypass: *bypass },
                NodeKind::Eq(e) => SavedKind::ParamEq(e.settings.clone()),
                NodeKind::Delay { time_ms, feedback, mix } => {
                    SavedKind::Delay { time_ms: *time_ms, feedback: *feedback, mix: *mix }
                }
                NodeKind::Mixer { gains_db } => SavedKind::Mixer { gains_db: *gains_db },
                NodeKind::Limiter { p, .. } => SavedKind::Limiter(*p),
                NodeKind::Visual(v) => SavedKind::Visual(v.settings()),
                NodeKind::Audio2Midi(a) => SavedKind::Audio2Midi { config: a.config, dest: a.want.clone() },
                NodeKind::MidiIn(m) => SavedKind::MidiIn { port: m.want.clone() },
                NodeKind::MidiOut(m) => SavedKind::MidiOut { port: m.want.clone() },
                NodeKind::MidiMonitor(_) => SavedKind::MidiMonitor,
                NodeKind::CSynth(c) => SavedKind::CSynth(c.settings.clone()),
                NodeKind::Lyrics(l) => SavedKind::Lyrics(l.settings.clone()),
                NodeKind::Video(v) => SavedKind::Video(v.settings.clone()),
                NodeKind::MidiFile(m) => SavedKind::MidiFile(m.settings()),
                NodeKind::Plugin(p) => {
                    let _g = guard::enter(&p.tag);
                    let state = p.instance.save_state().map(|b| BASE64.encode(b));
                    let info = p.info();
                    SavedKind::Plugin {
                        format: info.format,
                        path: info.path.clone(),
                        id: info.id.clone(),
                        name: info.name.clone(),
                        bypass: p.bypass,
                        editor: p.editor.is_some(),
                        state,
                    }
                }
            };
            index.insert(id, nodes.len());
            nodes.push(SavedNode { pos: n.pos.into(), open: n.open, kind });
        }
        let wires = self
            .snarl
            .wires()
            .filter_map(|(o, i)| Some([*index.get(&o.node)?, o.output, *index.get(&i.node)?, i.input]))
            .collect();
        SavedGraph { nodes, wires }
    }

    /// Replaces the graph with `g`. Nodes that can't be recreated (a plugin that's gone) are
    /// skipped along with their wires.
    fn load_graph(&mut self, g: &SavedGraph) {
        self.clear_graph();
        self.insert_graph(g, egui::Vec2::ZERO);
    }

    /// Saves the selected nodes, with the wires between them, as a node preset. Positions are
    /// kept relative to the group's top-left corner.
    fn save_node_preset(&mut self, name: &str, selected: &[NodeId]) {
        let only: HashSet<NodeId> = selected.iter().copied().filter(|id| self.snarl.get_node(*id).is_some()).collect();
        let mut g = self.save_nodes(Some(&only));
        let min = g.nodes.iter().fold([f32::INFINITY; 2], |m, n| [m[0].min(n.pos[0]), m[1].min(n.pos[1])]);
        for n in &mut g.nodes {
            n.pos = [n.pos[0] - min[0], n.pos[1] - min[1]];
        }
        match session::save_node_preset(name, &g) {
            Ok(()) => {
                self.log(format!("saved node preset \"{name}\" ({} nodes)", g.nodes.len()));
                self.node_presets = session::list_node_presets();
            }
            Err(e) => self.log(format!("can't save node preset \"{name}\": {e:#}")),
        }
    }

    /// Adds a node preset's nodes and wires with its top-left corner at `pos`.
    fn add_node_preset(&mut self, name: &str, pos: Pos2) {
        match session::load_node_preset(name) {
            Ok(g) => {
                let had = self.snarl.node_ids().count();
                self.insert_graph(&g, pos.to_vec2());
                let added = self.snarl.node_ids().count() - had;
                let players = g.nodes.iter().filter(|n| matches!(n.kind, SavedKind::Player)).count();
                if added + players < g.nodes.len() {
                    self.log(format!("node preset \"{name}\": {added} of {} nodes added (missing plugins are skipped)", g.nodes.len()));
                }
            }
            Err(e) => self.log(format!("can't load node preset \"{name}\": {e:#}")),
        }
    }

    /// Adds `g`'s nodes, moved by `offset`, and its wires; the new ids in `g`'s order.
    fn insert_graph(&mut self, g: &SavedGraph, offset: egui::Vec2) -> Vec<Option<NodeId>> {
        let mut ids = Vec::with_capacity(g.nodes.len());
        for n in &g.nodes {
            let pos = Pos2::from(n.pos) + offset;
            let id = match &n.kind {
                SavedKind::Plugin { .. } => self.restore_plugin(&n.kind, pos),
                SavedKind::Vst3 { .. } => n.kind.upgraded().and_then(|k| self.restore_plugin(&k, pos)),
                // There's only one Player; a node preset's wires from it go to the one there is.
                SavedKind::Player if self.snarl.nodes().any(|x| matches!(x.kind, NodeKind::Player)) => {
                    let existing = self.snarl.node_ids().find(|(_, n)| matches!(n.kind, NodeKind::Player)).map(|(id, _)| id);
                    ids.push(existing);
                    continue;
                }
                k => {
                    let kind = match k {
                        SavedKind::Player => NodeKind::Player,
                        SavedKind::Output { volume_db } => {
                            NodeKind::Output { volume_db: *volume_db, meter: Arc::default(), level: [0.0; 2] }
                        }
                        SavedKind::Gain { db } => NodeKind::Gain { db: *db },
                        SavedKind::Pan { pan } => NodeKind::Pan { pan: *pan },
                        SavedKind::RateShift { rate, bypass } => NodeKind::RateShift { rate: *rate, bypass: *bypass, active: false },
                        SavedKind::Eq(p) => NodeKind::eq(EqSettings::from_legacy(*p)),
                        SavedKind::ParamEq(s) => NodeKind::eq(s.clone()),
                        SavedKind::Delay { time_ms, feedback, mix } => {
                            NodeKind::Delay { time_ms: *time_ms, feedback: *feedback, mix: *mix }
                        }
                        SavedKind::Mixer { gains_db } => NodeKind::Mixer { gains_db: *gains_db },
                        SavedKind::Limiter(p) => NodeKind::Limiter { p: *p, meter: Arc::default(), gain: 1.0 },
                        SavedKind::Visual(s) => NodeKind::Visual(Box::new(Visualizer::from_settings(s))),
                        SavedKind::Audio2Midi { config, dest } => {
                            let mut a = Audio2Midi::new(self.sample_rate);
                            a.set_config(*config);
                            if let Some(label) = dest {
                                a.connect_label(label.clone());
                            }
                            NodeKind::Audio2Midi(Box::new(a))
                        }
                        SavedKind::MidiIn { port } | SavedKind::MidiOut { port } => {
                            let dir = if matches!(k, SavedKind::MidiIn { .. }) { Dir::In } else { Dir::Out };
                            let mut m = SeqNode::new(dir);
                            if let Some(label) = port {
                                m.connect_label(label.clone());
                            }
                            match dir {
                                Dir::In => NodeKind::MidiIn(Box::new(m)),
                                Dir::Out => NodeKind::MidiOut(Box::new(m)),
                            }
                        }
                        SavedKind::MidiMonitor => NodeKind::MidiMonitor(Box::new(Monitor::new())),
                        SavedKind::CSynth(s) => NodeKind::CSynth(Box::new(CSynthNode::new(s.clone(), self.sample_rate))),
                        SavedKind::Lyrics(s) => NodeKind::Lyrics(Box::new(LyricsNode::new(s.clone()))),
                        SavedKind::Video(s) => NodeKind::Video(Box::new(VideoNode::new(s.clone()))),
                        SavedKind::MidiFile(s) => NodeKind::MidiFile(Box::new(MidiFileNode::new(s.clone(), self.sample_rate))),
                        SavedKind::Plugin { .. } | SavedKind::Vst3 { .. } => unreachable!(),
                    };
                    self.add_node(kind, pos)
                }
            };
            if let Some(info) = id.and_then(|id| self.snarl.get_node_info_mut(id)) {
                info.open = n.open;
            }
            ids.push(id);
        }
        for &[a, out, b, inp] in &g.wires {
            let (Some(&Some(from)), Some(&Some(to))) = (ids.get(a), ids.get(b)) else { continue };
            let (a, b) = (&self.snarl[from].kind, &self.snarl[to].kind);
            if out < a.outputs()
                && inp < b.inputs()
                && a.is_midi_output(out) == b.is_midi_input(inp)
                && !graph::would_cycle(&self.snarl, from, to)
            {
                self.snarl.connect(OutPinId { node: from, output: out }, InPinId { node: to, input: inp });
            }
        }
        self.dirty = true;
        ids
    }

    fn restore_plugin(&mut self, kind: &SavedKind, pos: Pos2) -> Option<NodeId> {
        let SavedKind::Plugin { format, path, id, name, bypass, editor, state } = kind else { return None };
        let info = self.plugins.iter().find(|p| p.same(*format, path, id)).cloned().or_else(|| {
            self.host.classes(*format, path).ok()?.into_iter().find(|c| &c.id == id)
        });
        let Some(info) = info else {
            self.log(format!("plugin not found, skipped: {name} ({})", path.display()));
            return None;
        };
        let id = self.add_plugin(&info, pos)?;
        let eid = self.snarl[id].eid;
        let state = state.as_ref().and_then(|s| BASE64.decode(s).ok());
        if let NodeKind::Plugin(p) = &mut self.snarl[id].kind {
            let _g = guard::enter(&p.tag);
            if let Some(state) = state
                && let Err(e) = p.instance.load_state(&state)
            {
                log::warn!("{name}: can't restore state: {e:#}");
            }
            p.bypass = *bypass;
        }
        if *bypass
            && let Some(e) = &mut self.engine
        {
            e.send(Command::SetParam(eid, 0, 1.0));
        }
        if *editor {
            self.pending_editors.push(id);
        }
        Some(id)
    }

    fn save_session(&mut self) {
        let status = self.player.status();
        let s = Session {
            graph: Some(self.save_graph()),
            library_root: None,
            library_sources: self.library.sources.clone(),
            library_sort: self.library.sort,
            library_columns: Some(self.library.columns.clone()),
            track: status.path,
            position: self.player.position(),
            repeat: Some(self.library.repeat),
            shuffle: self.library.shuffle,
            rate: Some(self.rate),
            dock: Some(self.dock.clone()),
            tabs_seen: Tab::ALL.to_vec(),
        };
        session::save_session(&s);
        self.last_save = Instant::now();
    }

    fn next_track(&mut self) {
        if let Some(i) = self.library.next(false) {
            self.play_track(i);
        }
    }

    /// Like most players: past the first seconds, "previous" restarts the track.
    fn previous_track(&mut self) {
        if self.player.position() > 3.0 && self.library.current.is_some() {
            self.player.seek(0.0);
        } else if let Some(i) = self.library.previous() {
            self.play_track(i);
        }
    }

    /// Starts playback; with nothing loaded, starts the current (or first) track of the list.
    fn play(&mut self) {
        if self.player.status().path.is_some() {
            self.player.set_playing(true);
        } else if !self.library.tracks.is_empty() {
            self.play_track(self.library.current.unwrap_or(0));
        }
    }

    fn stop(&mut self) {
        self.player.set_playing(false);
        self.player.seek(0.0);
        self.misskey.stopped(&self.player.status());
        self.discord.stopped();
    }

    fn seek_to(&self, secs: f64) {
        let dur = self.player.status().duration.unwrap_or(f64::MAX);
        self.player.seek(secs.clamp(0.0, dur));
    }

    fn media_event(&mut self, e: mpris::Event, ctx: &egui::Context) {
        use mpris::{Event, SeekDirection};
        let step = |d: SeekDirection, secs: f64| if d == SeekDirection::Forward { secs } else { -secs };
        match e {
            Event::Play => self.play(),
            Event::Pause => self.player.set_playing(false),
            Event::Toggle if self.player.is_playing() => self.player.set_playing(false),
            Event::Toggle => self.play(),
            Event::Next => self.next_track(),
            Event::Previous => self.previous_track(),
            Event::Stop => self.stop(),
            Event::Seek(d) => self.seek_to(self.player.position() + step(d, 10.0)),
            Event::SeekBy(d, by) => self.seek_to(self.player.position() + step(d, by.as_secs_f64())),
            Event::SetPosition(p) => self.seek_to(p.0.as_secs_f64()),
            Event::OpenUri(uri) => {
                let path = PathBuf::from(uri.strip_prefix("file://").unwrap_or(&uri));
                if path.is_file() {
                    self.load_file(&path);
                }
            }
            Event::Raise => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Event::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Event::SetVolume(_) => {}
        }
    }

    fn reopen_editors(&mut self) {
        let Some(xid) = self.host_xid else { return };
        for id in std::mem::take(&mut self.pending_editors) {
            let Some(n) = self.snarl.get_node_info_mut(id) else { continue };
            if let NodeKind::Plugin(p) = &mut n.value.kind {
                let _g = guard::enter(&p.tag);
                if let Err(e) = p.open_editor(Placement::Embedded(xid)) {
                    log::warn!("{e:#}");
                }
            }
        }
    }

    fn log(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.messages.push(msg);
        if self.messages.len() > 200 {
            self.messages.remove(0);
        }
    }

    /// Loads a file picked outside the track list; it becomes current if the list has it.
    fn load_file(&mut self, path: &Path) {
        self.player.load(path);
        if let Some(i) = self.library.index_of(path) {
            self.library.set_current(i);
            self.reveal_current = true;
        }
    }

    fn play_track(&mut self, i: usize) {
        let Some(t) = self.library.tracks.get(i) else { return };
        self.player.load(&t.path);
        self.library.set_current(i);
        self.reveal_current = true;
    }

    fn open_dropped(&mut self, path: PathBuf) {
        // A MIDI file goes to the first MIDI File Player, if there is one.
        if MidiFileNode::is_midi_file(&path)
            && let Some(m) = self.snarl.nodes_mut().find_map(|n| match &mut n.kind {
                NodeKind::MidiFile(m) => Some(m),
                _ => None,
            })
        {
            m.open(path);
            return;
        }
        if path.is_dir() {
            self.library.add(library::Source::Folder(path));
            self.show_tab(Tab::Library);
        } else {
            self.load_file(&path);
        }
    }

    fn insert(&mut self, kind: NodeKind, proc: Box<dyn crate::engine::Processor>, pos: Pos2) -> Option<NodeId> {
        let Some(eid) = self.ids.alloc() else {
            self.log("too many nodes");
            return None;
        };
        if let Some(e) = &mut self.engine {
            let slot = NodeSlot::new(proc, kind.audio_inputs(), kind.audio_outputs()).with_tag(kind.tag());
            e.send(Command::AddNode(eid, slot));
        }
        self.dirty = true;
        Some(self.snarl.insert_node(pos, GraphNode { eid, kind }))
    }

    fn add_node(&mut self, kind: NodeKind, pos: Pos2) -> Option<NodeId> {
        let proc = kind.make_processor(self.sample_rate, &self.player)?;
        self.insert(kind, proc, pos)
    }

    fn add_plugin(&mut self, info: &PluginInfo, pos: Pos2) -> Option<NodeId> {
        match self.host.create(info, self.sample_rate, MAX_BLOCK) {
            Ok(created) => {
                let i = &created.instance;
                self.log(format!(
                    "loaded {} {} ({} params, latency {} samples)",
                    info.format.label(),
                    info.name,
                    i.params().len(),
                    i.latency()
                ));
                let node = PluginNode::new(created.instance);
                self.insert(NodeKind::Plugin(Box::new(node)), Box::new(PluginProc::new(created.processor)), pos)
            }
            Err(e) => {
                self.log(format!("failed to load {}: {e:#}", info.name));
                None
            }
        }
    }

    /// `NODEPLAYER_DEBUG_PLUGIN="[FORMAT:]<path>|<class name>"` (VST3 when no format is
    /// given) inserts that plugin and opens its editor, for testing without clicking through
    /// the UI.
    fn debug_autoload(&mut self) {
        let Some(spec) = std::env::var_os("NODEPLAYER_DEBUG_PLUGIN") else { return };
        let spec = spec.to_string_lossy().into_owned();
        let Some((path, name)) = spec.split_once('|') else { return };
        let (format, path) = match path.split_once(':') {
            Some((f, p)) => match Format::ALL.into_iter().find(|x| x.label() == f) {
                Some(f) => (f, p),
                None => (Format::Vst3, path),
            },
            None => (Format::Vst3, path),
        };
        let Ok(classes) = self.host.classes(format, Path::new(path)) else { return };
        let Some(info) = classes.into_iter().find(|c| c.name == name) else { return };
        let Some(id) = self.add_plugin(&info, pos2(130.0, 120.0)) else { return };
        let placement = self.host_xid.map_or(Placement::Window, Placement::Embedded);
        if let NodeKind::Plugin(p) = &mut self.snarl[id].kind
            && p.instance.has_editor()
            && let Err(e) = p.open_editor(placement)
        {
            log::error!("{e:#}");
        }
    }

    /// Plugin windows are children of the main window, so they must be closed while it still
    /// exists; then audio stops before the plugins are terminated.
    fn shutdown(&mut self) {
        if std::mem::replace(&mut self.shut_down, true) {
            return;
        }
        // Save first: plugin state has to be read before the plugins go away.
        self.save_session();
        self.extensions = Extensions::default();
        for (_, node) in self.snarl.nodes_ids_mut() {
            if let NodeKind::Plugin(p) = &mut node.kind {
                let _g = guard::enter(&p.tag);
                p.editor = None;
            }
        }
        self.engine = None;
        for (_, p) in self.pending_teardown.drain() {
            let _g = guard::enter(&p.tag.clone());
            drop(p);
        }
        let ids: Vec<NodeId> = self.snarl.node_ids().map(|(id, _)| id).collect();
        for id in ids {
            let node = self.snarl.remove_node(id);
            let _g = node.kind.tag().map(|t| guard::enter(&t));
            drop(node);
        }
    }

    fn remove_node(&mut self, id: NodeId) {
        let node = self.snarl.remove_node(id);
        // Engine must stop scheduling the node before the slot is removed.
        self.recompile();
        let mut plugin = match node.kind {
            NodeKind::Plugin(mut p) => {
                let _g = guard::enter(&p.tag);
                p.editor = None;
                Some(p)
            }
            _ => None,
        };
        match &mut self.engine {
            Some(e) => {
                e.send(Command::RemoveNode(node.eid));
                if let Some(p) = plugin.take() {
                    self.pending_teardown.insert(node.eid, p);
                }
            }
            None => self.ids.free(node.eid),
        }
    }

    fn recompile(&mut self) {
        self.dirty = false;
        let schedule = graph::compile(&self.snarl);
        if let Some(e) = &mut self.engine {
            e.send(Command::SetSchedule(Box::new(schedule)));
        }
    }

    fn collect_garbage(&mut self) {
        let Some(e) = &mut self.engine else { return };
        for g in e.collect_garbage() {
            match g {
                Garbage::Node(eid, slot) => {
                    let _g = slot.tag().map(|t| guard::enter(&t));
                    // Release the audio-side COM refs before the plugin is terminated.
                    drop(slot);
                    self.pending_teardown.remove(&eid);
                    self.ids.free(eid);
                }
                Garbage::Replaced(eid, slot) => {
                    let g = slot.tag().map(|t| guard::enter(&t));
                    drop(slot);
                    drop(g);
                    if self.restarting.remove(&eid) {
                        self.finish_restart(eid);
                    }
                }
                Garbage::Schedule(_) => {}
            }
        }
    }

    /// Takes a plugin's processor off the audio thread (audio passes through meanwhile); the
    /// plugin restarts once it's back, in [`finish_restart`](Self::finish_restart).
    fn begin_restart(&mut self, eid: u32) {
        let Some(e) = &mut self.engine else { return };
        let Some(node) = self.snarl.nodes().find(|n| n.eid == eid) else { return };
        if !self.restarting.insert(eid) {
            return;
        }
        let slot = NodeSlot::new(Box::new(MidiBridge::default()), node.kind.audio_inputs(), node.kind.audio_outputs());
        e.send(Command::ReplaceNode(eid, slot));
    }

    fn finish_restart(&mut self, eid: u32) {
        let Some(id) = self.snarl.node_ids().find(|(_, n)| n.eid == eid).map(|(id, _)| id) else { return };
        let NodeKind::Plugin(p) = &mut self.snarl[id].kind else { return };
        let name = p.info().name.clone();
        let tag = p.tag.clone();
        let _g = guard::enter(&tag);
        match p.instance.restart(self.sample_rate as f64, MAX_BLOCK) {
            Ok(proc) => {
                let mut proc = PluginProc::new(proc);
                if p.bypass {
                    crate::engine::Processor::set_param(&mut proc, 0, 1.0);
                }
                let kind = &self.snarl[id].kind;
                let slot = NodeSlot::new(Box::new(proc), kind.audio_inputs(), kind.audio_outputs()).with_tag(Some(tag));
                if let Some(e) = &mut self.engine {
                    e.send(Command::ReplaceNode(eid, slot));
                }
                self.drop_stale_wires(id);
                self.log(format!("restarted {name}"));
            }
            Err(e) => self.log(format!("{name} failed to restart: {e:#}")),
        }
    }

    /// Removes wires a node's pins no longer match (after a plugin changed its ports).
    fn drop_stale_wires(&mut self, id: NodeId) {
        let stale: Vec<_> = self
            .snarl
            .wires()
            .filter(|(o, i)| {
                let (a, b) = (&self.snarl[o.node].kind, &self.snarl[i.node].kind);
                (o.node == id || i.node == id)
                    && (o.output >= a.outputs() || i.input >= b.inputs() || a.is_midi_output(o.output) != b.is_midi_input(i.input))
            })
            .collect();
        for (o, i) in stale {
            self.snarl.disconnect(o, i);
        }
        self.dirty = true;
    }

    fn start_scan(&mut self) {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.scan_rx = Some(rx);
        self.plugins.clear();
        std::thread::spawn(move || {
            let files = plugins::find_all();
            let total = files.len();
            let next = std::sync::atomic::AtomicUsize::new(0);
            // yabridge-wrapped plugins take a while to start Wine, so scan in parallel.
            std::thread::scope(|s| {
                for _ in 0..4 {
                    s.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((format, path)) = files.get(i) else { break };
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            let _ = tx.send(ScanMsg::Progress(format!("{}/{total} {name}", i + 1)));
                            let _ = tx.send(match plugins::scan_isolated(*format, path) {
                                Ok(v) => ScanMsg::Found(v),
                                Err(e) => ScanMsg::Failed(path.clone(), e.to_string()),
                            });
                        }
                    });
                }
            });
            let _ = tx.send(ScanMsg::Done);
        });
    }

    /// Adds a plugin file picked by hand to the list (and keeps it for later scans).
    fn add_plugin_file(&mut self, path: &Path) {
        let Some(format) = plugins::detect_format(path) else {
            self.log(format!("not a plugin: {}", path.display()));
            return;
        };
        match plugins::scan_isolated(format, path) {
            Ok(found) if !found.is_empty() => {
                plugins::add_extra_file(path);
                self.log(format!("added {} {} plugin(s) from {}", found.len(), format.label(), path.display()));
                self.plugins.retain(|p| p.path != path);
                self.plugins.extend(found);
                self.plugins.sort_by_key(|p| p.name.to_lowercase());
                plugins::save_cache(&self.plugins);
            }
            Ok(_) => self.log(format!("no plugins in {}", path.display())),
            Err(e) => self.log(format!("can't read {}: {e:#}", path.display())),
        }
    }

    fn poll_background(&mut self) {
        self.collect_garbage();

        // The player loads asynchronously, so a just-restored track may not be reported yet.
        let loaded = self.player.status().path.or_else(|| self.restored_track.clone());
        if self.library.poll(loaded.as_deref()) {
            self.reveal_current = true;
            self.library_changed = true;
            self.log(format!("{} tracks in {} source(s)", self.library.tracks.len(), self.library.sources.len()));
        }
        if self.player.take_ended() {
            self.track_ended = true;
            match self.library.next(true) {
                Some(i) => self.play_track(i),
                None => {
                    self.misskey.stopped(&self.player.status());
                    self.discord.stopped();
                }
            }
        }
        if self.last_save.elapsed() > AUTOSAVE {
            self.save_session();
        }

        if let Some(rx) = &self.scan_rx {
            let msgs: Vec<_> = rx.try_iter().collect();
            for m in msgs {
                match m {
                    ScanMsg::Progress(s) => self.scan_progress = s,
                    ScanMsg::Found(v) => self.plugins.extend(v),
                    ScanMsg::Failed(p, e) => self.log(format!("scan failed: {}: {e}", p.display())),
                    ScanMsg::Done => {
                        self.scan_rx = None;
                        self.plugins.sort_by_key(|p| p.name.to_lowercase());
                        plugins::save_cache(&self.plugins);
                        self.log(format!("scan finished: {} plugins", self.plugins.len()));
                    }
                }
            }
        }

        if let Some(r) = self.yabridge.poll() {
            let ok = r.ok;
            self.log(format!("{} {}", r.what, if ok { "ok" } else { "failed" }));
            if ok && r.what.ends_with("sync") {
                self.start_scan();
            }
        }

        let mut closed = Vec::new();
        let mut restart = Vec::new();
        for (_, node) in self.snarl.nodes_ids_mut() {
            match &mut node.kind {
                NodeKind::Plugin(p) => {
                    let _g = guard::enter(&p.tag);
                    p.instance.idle();
                    if p.instance.wants_restart() {
                        restart.push(node.eid);
                    }
                    if let Some(ed) = &mut p.editor
                        && !ed.idle()
                    {
                        closed.push(p.info().name.clone());
                        p.editor = None;
                    }
                }
                NodeKind::Limiter { meter, gain, .. } => *gain = meter.take_gain(),
                NodeKind::CSynth(c) => c.idle(),
                NodeKind::MidiFile(m) => m.update(),
                NodeKind::Output { meter, level, .. } => {
                    let peak = meter.take();
                    for c in 0..2 {
                        level[c] = peak[c].max(level[c] * 0.85);
                    }
                }
                _ => {}
            }
        }
        for eid in restart {
            self.begin_restart(eid);
        }
    }

    fn transport(&mut self, ui: &mut egui::Ui) {
        let status = self.player.status();
        let playing = self.player.is_playing();
        let pos = self.player.position();
        let dur = status.duration.unwrap_or(0.0);
        ui.horizontal(|ui| {
            if ui.button("📂 Open").clicked() {
                let media: Vec<&str> = AUDIO_EXTS.iter().chain(crate::av::VIDEO_EXTS).copied().collect();
                if let Some(p) = rfd::FileDialog::new().add_filter("Audio and video", &media).add_filter("Audio", AUDIO_EXTS).add_filter("Video", crate::av::VIDEO_EXTS).pick_file() {
                    self.load_file(&p);
                }
            }
            let has_tracks = !self.library.tracks.is_empty();
            if ui.add_enabled(has_tracks, egui::Button::new("⏮")).on_hover_text("previous (or restart)").clicked() {
                self.previous_track();
            }
            let label = if playing { "⏸" } else { "▶" };
            if ui.add_enabled(status.path.is_some() || has_tracks, egui::Button::new(label)).clicked() {
                if playing {
                    self.player.set_playing(false);
                } else {
                    self.play();
                }
            }
            if ui.button("⏹").clicked() {
                self.stop();
            }
            if ui.add_enabled(has_tracks, egui::Button::new("⏭")).on_hover_text("next").clicked() {
                self.next_track();
            }
            if ui.button(self.library.repeat.label()).on_hover_text("repeat").clicked() {
                self.library.repeat = self.library.repeat.next();
            }
            ui.toggle_value(&mut self.library.shuffle, "🔀").on_hover_text("shuffle");
            let items = self.extensions.menu_items();
            if !items.is_empty() {
                let mut chosen = None;
                ui.menu_button("⚙", |ui| {
                    for item in &items {
                        if ui.button(&item.label).clicked() {
                            chosen = Some(item.clone());
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("extensions");
                if let Some(item) = chosen {
                    self.extensions.menu(&item);
                    self.run_extension_commands();
                }
            }
            ui.label(format!("{} / {}", fmt_time(self.seek_drag.unwrap_or(pos)), fmt_time(dur)));
            self.rate_control(ui);

            let mut t = self.seek_drag.unwrap_or(pos).min(dur);
            ui.spacing_mut().slider_width = (ui.available_width() - 220.0).max(100.0);
            let r = ui.add_enabled(dur > 0.0, egui::Slider::new(&mut t, 0.0..=dur.max(0.001)).show_value(false));
            if r.dragged() {
                self.seek_drag = Some(t);
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                self.player.seek(t);
                self.seek_drag = None;
            }

            if let Some(p) = &status.path {
                let title = status.title().unwrap_or_default();
                let text = match &status.tags.artist {
                    Some(a) => format!("{a} — {title}"),
                    None => title,
                };
                let album = status.tags.album.as_deref().map(|a| format!("{a}\n")).unwrap_or_default();
                ui.label(text).on_hover_text(format!(
                    "{album}{}\n{} Hz, {} ch",
                    p.display(),
                    status.source_rate,
                    status.channels
                ));
            }
        });
        if let Some(e) = &status.error {
            ui.colored_label(Color32::LIGHT_RED, e);
        }
    }

    /// Playback rate shift: speed and pitch together.
    fn rate_control(&mut self, ui: &mut egui::Ui) {
        let effective = self.player.rate();
        let semitones = 12.0 * effective.log2();
        let r = ui
            .add(
                egui::DragValue::new(&mut self.rate)
                    .range(crate::player::RATE_RANGE)
                    .speed(0.002)
                    .fixed_decimals(2)
                    .suffix("×"),
            )
            .on_hover_text(format!(
                "playback rate: speed and pitch change together ({semitones:+.2} semitones)\nPlayback Rate Shift nodes after the Player multiply it\ndrag or double-click to type; right-click resets to 1×"
            ));
        if r.secondary_clicked() {
            self.rate = 1.0;
        }
        if (effective - self.rate).abs() > 1e-4 {
            ui.weak(format!("→ {effective:.2}×")).on_hover_text("with the Playback Rate Shift nodes (0.5–2× at most)");
        }
        if r.changed() || r.secondary_clicked() {
            let nodes = graph::rate_shift(&mut self.snarl);
            self.player.set_rate(self.rate * nodes);
        }
    }

    fn library_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let start = self.library.sources.last().map(|s| s.path().to_path_buf());
            let start = start.map(|p| if p.is_dir() { p } else { p.parent().map(Path::to_path_buf).unwrap_or(p) });
            if ui.button("📁 Add folder…").on_hover_text("list the audio and video files under a folder, with its subfolders").clicked() {
                let mut dlg = rfd::FileDialog::new();
                if let Some(d) = &start {
                    dlg = dlg.set_directory(d);
                }
                for dir in dlg.pick_folders().unwrap_or_default() {
                    self.library.add(library::Source::Folder(dir));
                }
            }
            if ui.button("🎵 Add files…").on_hover_text("add single tracks").clicked() {
                let media: Vec<&str> = AUDIO_EXTS.iter().chain(crate::av::VIDEO_EXTS).copied().collect();
                let mut dlg = rfd::FileDialog::new().add_filter("Audio and video", &media);
                if let Some(d) = &start {
                    dlg = dlg.set_directory(d);
                }
                for f in dlg.pick_files().unwrap_or_default() {
                    self.library.add(library::Source::File(f));
                }
            }
            let can_rescan = !self.library.sources.is_empty() && !self.library.scanning();
            if ui.add_enabled(can_rescan, egui::Button::new("⟳")).on_hover_text("scan again").clicked() {
                self.library.rescan();
            }
            if self.library.scanning() {
                ui.spinner();
            } else if self.library.reading_tags() {
                ui.spinner().on_hover_text("reading tags");
            }
        });
        if self.library.sources.is_empty() {
            ui.weak("Add a folder or files, or drop a folder onto the window.");
            return;
        }
        let (folders, files) = self.library.sources.iter().fold((0, 0), |(d, f), s| match s {
            library::Source::Folder(_) => (d + 1, f),
            library::Source::File(_) => (d, f + 1),
        });
        let summary = match (folders, files) {
            (d, 0) => format!("{d} folder(s)"),
            (0, f) => format!("{f} file(s)"),
            (d, f) => format!("{d} folder(s), {f} file(s)"),
        };
        let mut remove = None;
        egui::CollapsingHeader::new(egui::RichText::new(summary).small()).id_salt("library sources").show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("sources").max_height(120.0).show(ui, |ui| {
                for (i, src) in self.library.sources.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✖").on_hover_text("remove from the list").clicked() {
                            remove = Some(i);
                        }
                        let icon = if matches!(src, library::Source::Folder(_)) { "📁" } else { "🎵" };
                        let name = src.path().file_name().map_or_else(|| src.path().display().to_string(), |n| n.to_string_lossy().into_owned());
                        ui.add(egui::Label::new(egui::RichText::new(format!("{icon} {name}")).small()).truncate())
                            .on_hover_text(src.path().display().to_string());
                    });
                }
            });
        });
        if let Some(i) = remove {
            self.library.remove(i);
        }
        right_to_left_row(ui, |ui| {
            ui.small(format!("{} tracks", self.library.tracks.len()));
            ui.add(egui::TextEdit::singleline(&mut self.library.filter).hint_text("filter").desired_width(f32::INFINITY));
        });
        self.track_table(ui);
    }

    /// The tracks as a table: number, title, artist, album, length, release date, when the
    /// file was added and how often it was played. Clicking a heading sorts by it (again:
    /// descending, a third time: back to folder order), dragging a heading's edge resizes the
    /// column, and right-clicking the headings picks the columns and sorts by hidden ones.
    fn track_table(&mut self, ui: &mut egui::Ui) {
        use library::Column;
        self.library.load_plays();
        // Folder headings and tracks as rows of one height so the list can be virtualized.
        enum Row {
            Dir(usize),
            Track(usize),
        }
        let lib = &self.library;
        let grouped = lib.sort.is_none();
        let filter = lib.filter.to_lowercase();
        let matches = |t: &library::Track| {
            if filter.is_empty() {
                return true;
            }
            let m = t.meta.as_ref();
            [Some(t.name.as_str()), Some(t.dir.as_str()), m.and_then(|m| m.title.as_deref()), m.and_then(|m| m.artist.as_deref()), m.and_then(|m| m.album.as_deref())]
                .into_iter()
                .flatten()
                .any(|s| s.to_lowercase().contains(&filter))
        };
        let mut rows = Vec::new();
        let mut last_dir: Option<&str> = None;
        for (i, t) in lib.tracks.iter().enumerate() {
            if !matches(t) {
                continue;
            }
            if grouped && last_dir != Some(t.dir.as_str()) {
                last_dir = Some(&t.dir);
                if !t.dir.is_empty() {
                    rows.push(Row::Dir(i));
                }
            }
            rows.push(Row::Track(i));
        }

        let cols: Vec<(usize, Column)> = Column::ALL.into_iter().enumerate().filter(|(k, _)| lib.columns.shown[*k]).collect();
        let pad = 4.0;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        // The title column takes whatever room the others leave, but not less than this.
        const MIN_TITLE: f32 = 100.0;
        let mut widths = lib.columns.widths;
        let others: f32 = cols.iter().filter(|(_, c)| *c != Column::Title).map(|(k, _)| widths[*k]).sum();
        let table_w = (others + MIN_TITLE).max(ui.available_width());
        if let Some((k, _)) = cols.iter().find(|(_, c)| *c == Column::Title) {
            widths[*k] = table_w - others;
        }

        let current = lib.current;
        let mut play = None;
        let mut new_sort = None;
        let mut resize: Option<(usize, f32)> = None;
        let mut toggle = None;
        let reveal = std::mem::take(&mut self.reveal_current);
        egui::ScrollArea::horizontal().id_salt("library h").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_min_width(table_w);
            // Headings.
            let (head, mut head_resp) = ui.allocate_exact_size(egui::vec2(table_w, row_h), egui::Sense::click());
            let painter = ui.painter_at(head);
            painter.rect_filled(head, 0.0, ui.visuals().faint_bg_color);
            let mut x = head.left();
            for (k, col) in &cols {
                let w = widths[*k];
                let cell = egui::Rect::from_min_size(pos2(x, head.top()), egui::vec2(w, row_h));
                let r = ui.interact(cell.shrink2(egui::vec2(3.0, 0.0)), ui.id().with(("col", *k)), egui::Sense::click());
                let arrow = match lib.sort {
                    Some((c, false)) if c == *col => " ⏶",
                    Some((c, true)) if c == *col => " ⏷",
                    _ => "",
                };
                let color = if r.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
                let right = matches!(col, Column::Track | Column::Length | Column::Plays);
                table_cell(ui, &painter, cell, &format!("{}{arrow}", col.label()), color, right, pad);
                // Right-clicking a heading opens the menu below too.
                head_resp = head_resp.union(r.clone());
                if r.clicked() {
                    new_sort = Some(match lib.sort {
                        Some((c, false)) if c == *col => Some((*col, true)),
                        Some((c, true)) if c == *col => None,
                        _ => Some((*col, false)),
                    });
                }
                // The edge between this column and the next.
                let edge = egui::Rect::from_center_size(pos2(x + w, head.center().y), egui::vec2(6.0, row_h));
                let e = ui.interact(edge, ui.id().with(("edge", *k)), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeColumn);
                if e.dragged() && *col != Column::Title {
                    resize = Some((*k, e.drag_delta().x));
                }
                painter.vline(x + w - 0.5, head.y_range().shrink(4.0), ui.visuals().widgets.noninteractive.bg_stroke);
                x += w;
            }
            head_resp.context_menu(|ui| {
                for (k, col) in Column::ALL.into_iter().enumerate() {
                    if col == Column::Title {
                        continue;
                    }
                    let mut on = lib.columns.shown[k];
                    if ui.checkbox(&mut on, col.label()).changed() {
                        toggle = Some(k);
                    }
                }
                ui.separator();
                ui.menu_button("Sort by", |ui| {
                    if ui.radio(lib.sort.is_none(), "Folder order").clicked() {
                        new_sort = Some(None);
                        ui.close();
                    }
                    for col in Column::ALL {
                        if ui.radio(lib.sort.is_some_and(|(c, _)| c == col), col.label()).clicked() {
                            new_sort = Some(Some((col, matches!(col, Column::Released | Column::Added | Column::Plays))));
                            ui.close();
                        }
                    }
                    ui.separator();
                    let mut desc = lib.sort.is_some_and(|(_, d)| d);
                    if ui.add_enabled(lib.sort.is_some(), egui::Checkbox::new(&mut desc, "Descending")).changed() {
                        new_sort = Some(lib.sort.map(|(c, _)| (c, desc)));
                    }
                });
            });

            let mut scroll = egui::ScrollArea::vertical().id_salt("library").auto_shrink([false, false]);
            if reveal
                && let Some(k) = rows.iter().position(|r| matches!(r, Row::Track(i) if Some(*i) == current))
            {
                let visible = ui.available_height();
                scroll = scroll.vertical_scroll_offset((k as f32 * (row_h + ui.spacing().item_spacing.y) - visible * 0.4).max(0.0));
            }
            scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
                for row in &rows[range] {
                    match *row {
                        Row::Dir(i) => {
                            let dir = egui::RichText::new(format!("📁 {}", lib.tracks[i].dir)).small().strong();
                            list_row(ui, row_h, false, dir, egui::Sense::hover());
                        }
                        Row::Track(i) => {
                            let t = &lib.tracks[i];
                            let is_cur = Some(i) == current;
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(table_w, row_h), egui::Sense::click());
                            let visuals = ui.style().interact_selectable(&resp, is_cur);
                            if is_cur || resp.hovered() {
                                ui.painter().rect_filled(rect, 2.0, visuals.weak_bg_fill);
                            }
                            let color = if is_cur { ui.visuals().strong_text_color() } else { visuals.text_color() };
                            let painter = ui.painter_at(rect);
                            let m = t.meta.as_ref();
                            let mut x = rect.left();
                            for (k, col) in &cols {
                                let cell = egui::Rect::from_min_size(pos2(x, rect.top()), egui::vec2(widths[*k], row_h));
                                x += widths[*k];
                                let text = match col {
                                    Column::Track => m.and_then(|m| m.track).map(|n| n.to_string()).unwrap_or_default(),
                                    Column::Title if is_cur => format!("▶ {}", t.title()),
                                    Column::Title => t.title(),
                                    Column::Artist => m.and_then(|m| m.artist.clone()).unwrap_or_default(),
                                    Column::Album => m.and_then(|m| m.album.clone()).unwrap_or_default(),
                                    Column::Length => m.and_then(|m| m.duration).map(fmt_time).unwrap_or_default(),
                                    Column::Released => m.and_then(|m| m.date.clone()).unwrap_or_default(),
                                    Column::Added => library::fmt_date(t.added),
                                    Column::Plays => match lib.plays(&t.path) {
                                        0 => String::new(),
                                        n => n.to_string(),
                                    },
                                };
                                let right = matches!(col, Column::Track | Column::Length | Column::Plays);
                                let weak = *col != Column::Title && *col != Column::Artist && *col != Column::Album && !is_cur;
                                let c = if weak { ui.visuals().weak_text_color() } else { color };
                                table_cell(ui, &painter, cell, &text, c, right, pad);
                            }
                            if resp.clicked() {
                                play = Some(i);
                            }
                            resp.on_hover_text(t.path.display().to_string());
                        }
                    }
                }
            });
        });
        if let Some((k, dx)) = resize {
            self.library.columns.widths[k] = (self.library.columns.widths[k] + dx).clamp(24.0, 600.0);
        }
        if let Some(k) = toggle {
            self.library.columns.shown[k] = !self.library.columns.shown[k];
        }
        if let Some(sort) = new_sort {
            self.library.set_sort(sort);
        }
        if let Some(i) = play {
            self.play_track(i);
        }
    }

    /// The node graph (a tab of the dock).
    fn graph_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let has_player = self.snarl.nodes().any(|n| matches!(n.kind, NodeKind::Player));
        // The graph pans on the wheel before node bodies see it, so take it out first.
        let scroll = if self.capture_scroll {
            ui.ctx().input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta))
        } else {
            egui::Vec2::ZERO
        };
        let mut viewer = Viewer {
            engine: self.engine.as_mut(),
            player: &self.player,
            actions,
            plugins: &self.plugins,
            has_player,
            errors: Vec::new(),
            embed_parent: self.host_xid,
            sample_rate: self.sample_rate,
            ppp: ui.ctx().pixels_per_point(),
            placements: HashMap::new(),
            scroll,
            wants_scroll: false,
            selected: self.selected.len(),
            view: ui.max_rect(),
            zoom: 1.0,
            last_zoom: self.zoom,
            zooming: ui.input(|i| (i.zoom_delta() - 1.0).abs() > 1e-4),
            reset_zoom: std::mem::take(&mut self.reset_zoom),
            node_presets: &self.node_presets,
        };
        // Selectable node titles would swallow drags meant to move the node.
        ui.style_mut().interaction.selectable_labels = false;
        // The first frame fits the graph into view; don't let that zoom in past 1:1.
        let mut style = self.style;
        if !std::mem::replace(&mut self.graph_shown, true) {
            style.max_scale = Some(1.0);
        }
        SnarlWidget::new().id_salt("graph").style(style).show(&mut self.snarl, &mut viewer, ui);
        self.selected = SnarlWidget::new().id_salt("graph").get_selected_nodes(ui);
        // Zoom readout; clicking it goes back to 100% (where text is sharp).
        let zoom = viewer.zoom;
        self.zoom = zoom;
        let corner = ui.max_rect().right_bottom() - egui::vec2(8.0, 8.0);
        egui::Area::new(egui::Id::new("graph zoom"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::RIGHT_BOTTOM)
            .fixed_pos(corner)
            .show(ui.ctx(), |ui| {
                let text = format!("{:.0}%", zoom * 100.0);
                let b = ui.add_enabled(zoom != 1.0, egui::Button::new(egui::RichText::new(text).small()).wrap_mode(egui::TextWrapMode::Extend));
                if b.on_hover_text("zoom (Ctrl+wheel); click for 100%, where text is sharpest").clicked() {
                    self.reset_zoom = true;
                    ui.ctx().request_repaint();
                }
            });
        self.placements = std::mem::take(&mut viewer.placements);
        self.capture_scroll = viewer.wants_scroll;
        let errors = std::mem::take(&mut viewer.errors);
        for e in errors {
            self.log(e);
        }
    }

    /// Brings a panel to the front, opening it again if it was closed.
    fn show_tab(&mut self, tab: Tab) {
        match self.dock.find_tab(&tab) {
            Some(path) => {
                let _ = self.dock.set_active_tab(path);
                self.dock.set_focused_node_and_surface(path.node_path());
            }
            None => self.dock.push_to_focused_leaf(tab),
        }
    }

    /// Asks for the name of the node preset being saved.
    fn node_preset_dialog(&mut self, ctx: &egui::Context) {
        let Some(nodes) = self.naming.clone() else { return };
        let mut done = None;
        let modal = egui::Modal::new(egui::Id::new("node preset name")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.strong(format!("Save {} node(s) as a node preset", nodes.len()));
            let name = self.node_preset_name.trim().to_string();
            let edit = ui.add(egui::TextEdit::singleline(&mut self.node_preset_name).hint_text("name").desired_width(f32::INFINITY));
            if !edit.has_focus() && name.is_empty() {
                edit.request_focus();
            }
            let exists = self.node_presets.contains(&name);
            if exists {
                ui.small("A node preset with this name exists and will be replaced.");
            }
            ui.horizontal(|ui| {
                let label = if exists { "Overwrite" } else { "Save" };
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(!name.is_empty(), egui::Button::new(label)).clicked() || (enter && !name.is_empty()) {
                    done = Some(Some(name.clone()));
                }
                if ui.button("Cancel").clicked() {
                    done = Some(None);
                }
            });
        });
        if modal.should_close() && done.is_none() {
            done = Some(None);
        }
        if let Some(result) = done {
            self.naming = None;
            if let Some(name) = result {
                self.save_node_preset(&name, &nodes);
            }
        }
    }

    fn node_presets_list(&mut self, ui: &mut egui::Ui) {
        ui.strong("Node presets");
        ui.small("A few nodes and their wiring. Select nodes with Shift+drag (or Shift+click), then right-click the graph to save them; add one from the same menu or with \"Add\" here.");
        let mut add = None;
        let mut delete = None;
        egui::ScrollArea::vertical().id_salt("node presets").max_height(ui.available_height() - 40.0).show(ui, |ui| {
            if self.node_presets.is_empty() {
                ui.weak("No node presets yet.");
            }
            for name in &self.node_presets {
                ui.horizontal(|ui| {
                    let armed = self.confirm_node.as_ref() == Some(name);
                    let del = if armed { egui::Button::new(egui::RichText::new("Delete?").color(Color32::LIGHT_RED)) } else { egui::Button::new("✖") };
                    if ui.add(del).on_hover_text("delete (click twice)").clicked() {
                        if armed {
                            delete = Some(name.clone());
                        } else {
                            self.confirm_node = Some(name.clone());
                        }
                    }
                    if ui.button("Add").on_hover_text("add these nodes to the graph").clicked() {
                        add = Some(name.clone());
                    }
                    ui.label(name);
                });
            }
        });
        if let Some(name) = add {
            self.confirm_node = None;
            // Below whatever is there, so it doesn't land on top of other nodes.
            let bottom = self.snarl.nodes_pos().map(|(p, _)| p.y).fold(f32::NEG_INFINITY, f32::max);
            let left = self.snarl.nodes_pos().map(|(p, _)| p.x).fold(f32::INFINITY, f32::min);
            let pos = if bottom.is_finite() { pos2(left, bottom + 220.0) } else { pos2(0.0, 0.0) };
            self.add_node_preset(&name, pos);
        }
        if let Some(name) = delete {
            self.confirm_node = None;
            if let Err(e) = session::delete_node_preset(&name) {
                self.log(format!("can't delete node preset: {e:#}"));
            }
            self.node_presets = session::list_node_presets();
        }
    }

    fn presets_panel(&mut self, ui: &mut egui::Ui) {
        ui.small("A preset is the whole graph: nodes, wiring, settings and plugin states.");
        // Right to left, so the name field takes whatever width the button leaves.
        right_to_left_row(ui, |ui| {
            let name_ok = !self.preset_name.trim().is_empty();
            let exists = self.presets.iter().any(|p| p == self.preset_name.trim());
            let label = if exists { "Overwrite" } else { "Save" };
            let save = ui.add_enabled(name_ok, egui::Button::new(label));
            ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text("preset name").desired_width(f32::INFINITY));
            if save.clicked() {
                let name = self.preset_name.trim().to_string();
                match session::save_preset(&name, &self.save_graph()) {
                    Ok(()) => self.log(format!("saved preset \"{name}\"")),
                    Err(e) => self.log(format!("can't save preset: {e:#}")),
                }
                self.presets = session::list_presets();
            }
        });
        ui.separator();

        let mut load = None;
        let mut delete = None;
        egui::ScrollArea::vertical().id_salt("presets").max_height((ui.available_height() - 40.0) * 0.5).show(ui, |ui| {
            if self.presets.is_empty() {
                ui.weak("No presets yet.");
            }
            for name in &self.presets {
                ui.horizontal(|ui| {
                    let armed = self.confirm.as_ref().is_some_and(|(n, del)| n == name && *del);
                    let del = if armed { egui::Button::new(egui::RichText::new("Delete?").color(Color32::LIGHT_RED)) } else { egui::Button::new("✖") };
                    if ui.add(del).on_hover_text("delete (click twice)").clicked() {
                        if armed {
                            delete = Some(name.clone());
                        } else {
                            self.confirm = Some((name.clone(), true));
                        }
                    }
                    if ui.button("Load").on_hover_text("replace the current graph").clicked() {
                        load = Some(name.clone());
                    }
                    if ui.selectable_label(self.preset_name == *name, name).clicked() {
                        self.preset_name = name.clone();
                    }
                });
            }
        });
        if let Some(name) = load {
            self.confirm = None;
            match session::load_preset(&name) {
                Ok(g) => {
                    self.load_graph(&g);
                    self.reopen_editors();
                    self.preset_name = name.clone();
                    self.log(format!("loaded preset \"{name}\""));
                }
                Err(e) => self.log(format!("can't load preset: {e:#}")),
            }
        }
        if let Some(name) = delete {
            self.confirm = None;
            if let Err(e) = session::delete_preset(&name) {
                self.log(format!("can't delete preset: {e:#}"));
            }
            self.presets = session::list_presets();
        }

        ui.separator();
        self.node_presets_list(ui);

        ui.separator();
        let armed = self.confirm.as_ref().is_some_and(|(n, del)| n.is_empty() && !*del);
        let text = if armed { "Really reset the graph?" } else { "Reset graph (Player → Output)" };
        if ui.button(text).clicked() {
            if armed {
                self.confirm = None;
                self.clear_graph();
                self.default_graph();
            } else {
                self.confirm = Some((String::new(), false));
            }
        }
    }

    fn plugin_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            let scanning = self.scan_rx.is_some();
            if ui.add_enabled(!scanning, egui::Button::new("Scan plugins")).on_hover_text("VST3, CLAP, JSFX and native plugins").clicked() {
                self.start_scan();
            }
            if ui.add_enabled(!scanning, egui::Button::new("Add file…")).on_hover_text("a .clap, .so or JSFX file outside the usual folders").clicked() {
                let mut dlg = rfd::FileDialog::new();
                if let Some(dir) = crate::jsfx::default_search_paths().into_iter().find(|d| d.is_dir()) {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(path) = dlg.pick_file() {
                    self.add_plugin_file(&path);
                }
            }
            if scanning {
                ui.spinner();
                ui.label(&self.scan_progress);
            }
        });
        if !self.host.disabled.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("⚠ Disabled after crashing nodeplayer").color(Color32::from_rgb(240, 170, 80)));
                let mut enable = None;
                for (i, (format, path)) in self.host.disabled.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("Enable").on_hover_text("load it again (restart for extensions)").clicked() {
                            enable = Some(i);
                        }
                        let name = path.file_name().unwrap_or_default().to_string_lossy();
                        ui.small(format!("{name} ({})", format.label())).on_hover_text(path.display().to_string());
                    });
                }
                if let Some(i) = enable {
                    self.host.disabled.remove(i);
                    guard::set_disabled(&self.host.disabled);
                }
            });
        }
        ui.add(egui::TextEdit::singleline(&mut self.plugin_filter).hint_text("filter"));
        let filter = self.plugin_filter.to_lowercase();
        egui::ScrollArea::vertical().id_salt("plugins").max_height(ui.available_height() * 0.55).show(ui, |ui| {
            for info in &self.plugins {
                if !filter.is_empty()
                    && !info.name.to_lowercase().contains(&filter)
                    && !info.vendor.to_lowercase().contains(&filter)
                {
                    continue;
                }
                ui.horizontal(|ui| {
                    if ui.small_button("+").on_hover_text("add to graph").clicked() {
                        let k = self.added as f32;
                        actions.push(Action::AddPlugin(info.clone(), pos2(200.0 + 25.0 * k, 260.0 + 25.0 * k)));
                        self.added = (self.added + 1) % 10;
                    }
                    let mut text = egui::RichText::new(&info.name);
                    if info.is_instrument() {
                        text = text.weak();
                    }
                    ui.label(text).on_hover_text(format!(
                        "{} · {}\n{}\n{}{}",
                        info.format.label(),
                        info.vendor,
                        info.category,
                        info.path.display(),
                        if info.is_bridged() { "\n(Windows plugin via yabridge)" } else { "" }
                    ));
                    if info.format != Format::Vst3 {
                        ui.small(info.format.label());
                    }
                    if info.is_bridged() {
                        ui.small("win");
                    }
                    if self.host.is_disabled(info.format, &info.path) {
                        ui.colored_label(Color32::from_rgb(240, 170, 80), "⚠").on_hover_text("disabled after a crash");
                    }
                });
            }
        });

        ui.separator();
        ui.heading("Windows plugins (yabridge)");
        if !self.yabridge.available() {
            ui.label("yabridgectl not found.");
            ui.hyperlink_to("Install yabridge", "https://github.com/robbert-vdh/yabridge#readme");
            ui.small("Extract it to ~/.local/share/yabridge, then restart.");
            return;
        }
        ui.add_enabled_ui(!self.yabridge.busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Add folder…").clicked() {
                    let start = yabridge::default_windows_plugin_dirs().into_iter().next();
                    let mut dlg = rfd::FileDialog::new();
                    if let Some(s) = start {
                        dlg = dlg.set_directory(s);
                    }
                    if let Some(dir) = dlg.pick_folder() {
                        self.yabridge.add_dir(&dir);
                    }
                }
                if ui.button("Sync").on_hover_text("yabridgectl sync, then rescan").clicked() {
                    self.yabridge.sync();
                }
                if ui.button("Status").clicked() {
                    self.yabridge.status();
                }
                if self.yabridge.busy {
                    ui.spinner();
                }
            });
            let mut remove = None;
            for d in &self.yabridge.dirs {
                ui.horizontal(|ui| {
                    if ui.small_button("✖").on_hover_text("yabridgectl rm").clicked() {
                        remove = Some(d.clone());
                    }
                    ui.small(d);
                });
            }
            if let Some(d) = remove {
                self.yabridge.remove_dir(&d);
            }
            let found: Vec<_> = yabridge::default_windows_plugin_dirs()
                .into_iter()
                .filter(|p| !self.yabridge.dirs.iter().any(|d| Path::new(d) == p))
                .collect();
            for p in found {
                ui.horizontal(|ui| {
                    if ui.small_button("+").clicked() {
                        self.yabridge.add_dir(&p);
                    }
                    ui.small(format!("found: {}", p.display()));
                });
            }
        });
        if !self.yabridge.log.is_empty() {
            egui::CollapsingHeader::new("yabridgectl output").show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("yblog").max_height(200.0).show(ui, |ui| {
                    ui.monospace(&self.yabridge.log);
                });
            });
        }
    }

    fn param_windows(&mut self, ctx: &egui::Context) {
        for (_, node) in self.snarl.nodes_ids_mut() {
            let eid = node.eid;
            let NodeKind::Plugin(p) = &mut node.kind else { continue };
            if !p.show_params {
                continue;
            }
            let PluginNode { instance, show_params, param_filter, tag, .. } = &mut **p;
            let _g = guard::enter(tag);
            egui::Window::new(format!("{} — parameters", instance.info().name))
                .id(egui::Id::new(("params", eid)))
                .open(show_params)
                .default_size([420.0, 480.0])
                .show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(param_filter).hint_text("filter"));
                    let f = param_filter.to_lowercase();
                    let params = instance.params().to_vec();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new(("pgrid", eid)).num_columns(3).striped(true).show(ui, |ui| {
                            for info in &params {
                                if info.hidden || (!f.is_empty() && !info.title.to_lowercase().contains(&f)) {
                                    continue;
                                }
                                let mut v = instance.param_value(info.id);
                                ui.label(&info.title);
                                let mut slider = egui::Slider::new(&mut v, 0.0..=1.0).show_value(false);
                                if info.step_count > 0 {
                                    slider = slider.step_by(1.0 / info.step_count as f64);
                                }
                                let r = ui.add_enabled(!info.read_only, slider);
                                if r.changed() {
                                    instance.set_param(info.id, v);
                                }
                                if r.double_clicked() {
                                    instance.set_param(info.id, info.default);
                                }
                                ui.label(format!("{} {}", instance.param_display(info.id, v), info.units));
                                ui.end_row();
                            }
                        });
                    });
                });
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        self.shutdown();
    }

    /// Runs every tick, also while the window is minimized (when `ui` isn't called), so
    /// playback moves on to the next track and the media session keeps working.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.shut_down {
            return;
        }
        self.poll_background();
        let events = self.mpris.as_ref().map(|m| m.events()).unwrap_or_default();
        for e in events {
            self.media_event(e, ctx);
        }
        let status = self.player.status();
        if let Some(m) = &mut self.mpris {
            m.update(&status, self.player.is_playing(), self.player.position());
        }
        self.library.load_plays();
        let plays = status.path.as_deref().map(|p| self.library.play_stats(p)).unwrap_or_default();
        for msg in self.misskey.update(&status, self.player.is_playing(), self.player.position(), plays) {
            self.log(msg);
        }
        if self.play_counter.tick(status.path.as_deref(), self.player.position(), status.duration)
            && let Some(path) = &status.path
        {
            self.library.add_play(path);
        }
        self.info.sample(status.path.as_deref(), self.player.is_playing(), || self.player.live_bitrate());
        let nodes = graph::rate_shift(&mut self.snarl);
        self.player.set_rate(self.rate * nodes);
        self.discord.update(&status, self.player.is_playing(), self.player.position(), f64::from(self.rate * nodes));
        self.update_extensions();
        if self.extensions.take_repaint() {
            ctx.request_repaint();
        }
        if self.dirty {
            self.recompile();
        }
        ctx.request_repaint_after(Duration::from_millis(30));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            self.shutdown();
            return;
        }
        if self.shut_down {
            return;
        }
        let first_frame = self.host_xid.is_none() && !self.debug_done;
        self.host_xid = native_window_id(frame);
        if first_frame {
            self.debug_done = true;
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            if self.host_xid.is_some() {
                crate::x11_errors::install(xlib_display(frame));
            }
            if cfg!(windows) && self.mpris.is_none() {
                self.mpris = Mpris::new(ui.ctx().clone(), self.host_xid);
            }
            self.debug_autoload();
        }
        if !self.pending_editors.is_empty() && self.host_xid.is_some() {
            self.reopen_editors();
        }
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_dropped(p);
        }
        // Space plays and pauses, unless something is being typed. Taken before any widget
        // sees it, so it doesn't also press the focused button.
        if !ctx.text_edit_focused() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space)) {
            if self.player.is_playing() {
                self.player.set_playing(false);
            } else {
                self.play();
            }
        }

        egui::Panel::top("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.transport(ui);
            ui.add_space(2.0);
        });
        if let Some(notice) = self.crash_notice.clone() {
            egui::Panel::top("crash").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(240, 170, 80), "⚠");
                    ui.label(notice);
                    if ui.small_button("Show").clicked() {
                        self.show_tab(Tab::Plugins);
                    }
                    if ui.small_button("✖").clicked() {
                        self.crash_notice = None;
                    }
                });
            });
        }
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                match &self.engine {
                    Some(e) => ui.small(format!("{} · {} Hz · {} ch", e.device_name, e.sample_rate, e.channels)),
                    None => ui.colored_label(Color32::LIGHT_RED, "no audio device"),
                };
                if self.ffmpeg.1.elapsed() > Duration::from_secs(2) {
                    self.ffmpeg = (crate::av::available(), Instant::now());
                }
                if !self.ffmpeg.0 && !self.setup.busy() {
                    ui.separator();
                    let b = ui.small_button("FFmpeg isn't installed: get it…");
                    if b.on_hover_text("needed for video, some audio formats and library tags").clicked() {
                        self.setup.ask_ffmpeg();
                    }
                }
                if let Some(m) = self.messages.last() {
                    ui.separator();
                    ui.small(m).on_hover_text(self.messages.join("\n"));
                }
            });
        });
        self.param_windows(&ctx);

        let mut dock = std::mem::replace(&mut self.dock, egui_dock::DockState::new(Vec::new()));
        let closed: Vec<Tab> = Tab::ALL
            .into_iter()
            .filter(|t| dock.find_tab(t).is_none())
            .filter(|t| *t != Tab::Extensions || !self.extensions.panels().is_empty())
            .collect();
        self.placements.clear();
        let mut tabs = Tabs { app: self, actions: &mut actions, closed, reopen: None };
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            egui_dock::DockArea::new(&mut dock)
                .id(egui::Id::new("dock"))
                .style(egui_dock::Style::from_egui(ui.style()))
                // Right-click a tab to close it; buttons on every tab would leave no room.
                .show_close_buttons(false)
                .show_add_buttons(true)
                .show_add_popup(true)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, &mut tabs);
        });
        if let Some((path, tab)) = tabs.reopen.take() {
            dock.set_focused_node_and_surface(path);
            dock.push_to_focused_leaf(tab);
        }
        self.dock = dock;
        // Move embedded editors over the space their nodes reserved this frame (none when the
        // graph tab is behind another).
        for (_, node) in self.snarl.nodes_ids_mut() {
            let eid = node.eid;
            let NodeKind::Plugin(p) = &mut node.kind else { continue };
            let Some(ed) = &mut p.editor else { continue };
            let _g = guard::enter(&p.tag);
            match self.placements.get(&eid) {
                Some(&(clip, origin)) => ed.place(Some(clip), origin),
                None => ed.place(None, (0, 0)),
            }
        }
        for node in self.snarl.nodes_mut() {
            if let NodeKind::Video(v) = &mut node.kind {
                v.window(&ctx, &self.player, node.eid);
            }
        }

        for a in actions {
            match a {
                Action::Add(kind, pos) => {
                    self.add_node(kind, pos);
                }
                Action::AddPlugin(info, pos) => {
                    self.add_plugin(&info, pos);
                }
                Action::Remove(id) => self.remove_node(id),
                Action::Changed => self.dirty = true,
                Action::NameNodePreset => self.naming = Some(self.selected.clone()),
                Action::AddNodePreset(name, pos) => self.add_node_preset(&name, pos),
            }
        }
        self.node_preset_dialog(&ctx);
        for msg in self.setup.ui(&ctx) {
            self.log(msg);
        }
        if self.dirty {
            self.recompile();
        }

        // Visualizers want a smooth frame rate; otherwise a slow tick is enough.
        let animate = self
            .snarl
            .nodes()
            .any(|n| match &n.kind {
                NodeKind::Visual(_) | NodeKind::Limiter { .. } | NodeKind::Audio2Midi(_) => true,
                NodeKind::Eq(e) => e.settings.analyzer,
                NodeKind::Plugin(p) => p.instance.animating(),
                _ => false,
            });
        ctx.request_repaint_after(Duration::from_millis(if animate { 16 } else { 30 }));
    }
}

// ---------------------------------------------------------------------------
// Node graph viewer

struct Viewer<'a> {
    engine: Option<&'a mut Engine>,
    player: &'a Player,
    actions: &'a mut Vec<Action>,
    plugins: &'a [PluginInfo],
    has_player: bool,
    errors: Vec<String>,
    embed_parent: Option<crate::plugins::WindowId>,
    sample_rate: f32,
    ppp: f32,
    /// Screen placement of embedded editors, filled while drawing node bodies.
    placements: HashMap<u32, (PxRect, (i32, i32))>,
    /// Mouse wheel taken from the graph view for an EQ point or a plugin body.
    scroll: egui::Vec2,
    wants_scroll: bool,
    /// How many nodes are selected.
    selected: usize,
    /// The graph's screen area, the zoom shown this frame, whether the user is zooming now,
    /// and a request to go back to 100%.
    view: egui::Rect,
    zoom: f32,
    /// The zoom of the previous frame.
    last_zoom: f32,
    zooming: bool,
    reset_zoom: bool,
    node_presets: &'a [String],
}

impl Viewer<'_> {
    fn param(&mut self, eid: u32, index: usize, value: f32) {
        if let Some(e) = self.engine.as_deref_mut() {
            e.send(Command::SetParam(eid, index, value));
        }
    }
}

const PIN_COLOR: Color32 = Color32::from_rgb(110, 180, 255);
const MIDI_COLOR: Color32 = Color32::from_rgb(255, 170, 60);

fn midi_pin() -> PinInfo {
    PinInfo::square().with_fill(MIDI_COLOR)
}

impl SnarlViewer<GraphNode> for Viewer<'_> {
    fn title(&mut self, node: &GraphNode) -> String {
        node.kind.title()
    }

    /// The graph is drawn as a scaled picture, so text is only sharp at exactly 100% and on
    /// whole pixels: zoom snaps to 100% when it ends up near it (or when asked), and panning
    /// stays on the pixel grid.
    fn current_transform(&mut self, to_global: &mut egui::emath::TSTransform, _snarl: &mut Snarl<GraphNode>) {
        let near = (to_global.scaling - 1.0).abs() < 0.08;
        // Fitting the graph into view (double-click) doesn't zoom in past 100%.
        let fitted_in = !self.zooming && to_global.scaling > 1.0 && (to_global.scaling - self.last_zoom).abs() > 1e-4;
        if self.reset_zoom || fitted_in || (near && !self.zooming && to_global.scaling != 1.0) {
            // Keep the point at the middle of the view where it is.
            let c = self.view.center().to_vec2();
            let local = (c - to_global.translation) / to_global.scaling;
            to_global.scaling = 1.0;
            to_global.translation = c - local;
        }
        let ppp = self.ppp;
        to_global.translation = (to_global.translation * ppp).round() / ppp;
        self.zoom = to_global.scaling;
    }

    fn inputs(&mut self, node: &GraphNode) -> usize {
        node.kind.inputs()
    }

    fn outputs(&mut self, node: &GraphNode) -> usize {
        node.kind.outputs()
    }

    fn show_input(&mut self, pin: &InPin, ui: &mut egui::Ui, snarl: &mut Snarl<GraphNode>) -> impl SnarlPin + 'static {
        let kind = &snarl[pin.id.node].kind;
        if kind.is_midi_input(pin.id.input) {
            ui.label("MIDI");
            return midi_pin();
        }
        match kind {
            NodeKind::Mixer { .. } => ui.label(format!("in {}", pin.id.input + 1)),
            _ => ui.label("in"),
        };
        PinInfo::circle().with_fill(PIN_COLOR)
    }

    fn show_output(&mut self, pin: &OutPin, ui: &mut egui::Ui, snarl: &mut Snarl<GraphNode>) -> impl SnarlPin + 'static {
        if snarl[pin.id.node].kind.is_midi_output(pin.id.output) {
            ui.label("MIDI");
            return midi_pin();
        }
        ui.label("out");
        PinInfo::circle().with_fill(PIN_COLOR)
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<GraphNode>) {
        let midi = snarl[from.id.node].kind.is_midi_output(from.id.output);
        if midi != snarl[to.id.node].kind.is_midi_input(to.id.input) {
            self.errors.push("MIDI pins only connect to MIDI pins".into());
            return;
        }
        if graph::would_cycle(snarl, from.id.node, to.id.node) {
            self.errors.push("that connection would create a feedback loop".into());
            return;
        }
        snarl.connect(from.id, to.id);
        self.actions.push(Action::Changed);
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<GraphNode>) {
        snarl.disconnect(from.id, to.id);
        self.actions.push(Action::Changed);
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<GraphNode>) {
        snarl.drop_outputs(pin.id);
        self.actions.push(Action::Changed);
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<GraphNode>) {
        snarl.drop_inputs(pin.id);
        self.actions.push(Action::Changed);
    }

    fn has_body(&mut self, node: &GraphNode) -> bool {
        !matches!(node.kind, NodeKind::Player)
    }

    fn show_body(
        &mut self,
        node: NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<GraphNode>,
    ) {
        let eid = snarl[node].eid;
        let mut sends: Vec<(usize, f32)> = Vec::new();
        ui.vertical(|ui| match &mut snarl[node].kind {
            NodeKind::Player => {}
            NodeKind::Visual(v) => v.show(ui, self.sample_rate),
            NodeKind::Audio2Midi(a) => audio2midi_body(ui, a, eid, self.sample_rate),
            NodeKind::MidiIn(m) | NodeKind::MidiOut(m) => m.ui(ui),
            NodeKind::MidiMonitor(m) => m.ui(ui),
            NodeKind::CSynth(c) => sends.extend(c.ui(ui)),
            NodeKind::Lyrics(l) => self.wants_scroll |= l.ui(ui, self.player, self.scroll.y),
            NodeKind::Video(v) => v.ui(ui, self.player),
            NodeKind::MidiFile(m) => m.ui(ui),
            NodeKind::Output { volume_db, level, .. } => {
                if ui.add(egui::Slider::new(volume_db, -60.0..=12.0).suffix(" dB").text("vol")).changed() {
                    sends.push((0, *volume_db));
                }
                for l in level.iter() {
                    let db = 20.0 * l.max(1e-6).log10();
                    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let color = if *l >= 1.0 { Color32::RED } else { Color32::from_rgb(80, 200, 120) };
                    ui.add(egui::ProgressBar::new(frac).desired_width(160.0).desired_height(6.0).fill(color));
                }
            }
            NodeKind::Gain { db } => {
                if ui.add(egui::Slider::new(db, -60.0..=24.0).suffix(" dB")).changed() {
                    sends.push((0, *db));
                }
            }
            NodeKind::Pan { pan } => {
                if ui.add(egui::Slider::new(pan, -1.0..=1.0)).changed() {
                    sends.push((0, *pan));
                }
            }
            NodeKind::RateShift { rate, bypass, active } => {
                let r = ui
                    .add(egui::Slider::new(rate, crate::player::RATE_RANGE).logarithmic(true).fixed_decimals(2).suffix("×"))
                    .on_hover_text("speed and pitch change together; right-click resets to 1×");
                if r.secondary_clicked() {
                    *rate = 1.0;
                }
                ui.horizontal(|ui| {
                    ui.checkbox(bypass, "bypass");
                    ui.small(format!("{:+.2} semitones", 12.0 * rate.log2()));
                });
                if !*active {
                    ui.colored_label(Color32::from_rgb(230, 170, 90), "no effect: the Player's audio doesn't go through here");
                }
            }
            NodeKind::Eq(e) => {
                let r = e.show(ui, eid, self.sample_rate, self.scroll.y);
                sends.extend(r.sends);
                self.wants_scroll |= r.wants_scroll;
            }
            NodeKind::Delay { time_ms, feedback, mix } => {
                let max_ms = Delay::MAX_SECONDS * 1000.0;
                if ui.add(egui::Slider::new(time_ms, 1.0..=max_ms).suffix(" ms").text("time")).changed() {
                    sends.push((Delay::TIME_MS, *time_ms));
                }
                if ui.add(egui::Slider::new(feedback, 0.0..=0.95).text("feedback")).changed() {
                    sends.push((Delay::FEEDBACK, *feedback));
                }
                if ui.add(egui::Slider::new(mix, 0.0..=1.0).text("mix")).changed() {
                    sends.push((Delay::MIX, *mix));
                }
            }
            NodeKind::Mixer { gains_db } => {
                for (i, g) in gains_db.iter_mut().enumerate() {
                    if ui.add(egui::Slider::new(g, -60.0..=12.0).suffix(" dB").text(format!("in {}", i + 1))).changed() {
                        sends.push((i, *g));
                    }
                }
            }
            NodeKind::Limiter { p, meter, gain } => {
                let mut slider = |ui: &mut egui::Ui, v: &mut f32, range, suffix: &str, text: &str, index: usize| {
                    let mut s = egui::Slider::new(v, range).suffix(suffix).text(text);
                    if index == LimiterParams::RELEASE {
                        s = s.logarithmic(true);
                    }
                    if ui.add(s).changed() {
                        sends.push((index, *v));
                    }
                };
                slider(ui, &mut p.threshold_db, -60.0..=0.0, " dB", "threshold", LimiterParams::THRESHOLD);
                slider(ui, &mut p.release_ms, 1.0..=1000.0, " ms", "release", LimiterParams::RELEASE);
                slider(ui, &mut p.lookahead_ms, 5.0..=50.0, " ms", "lookahead", LimiterParams::LOOKAHEAD);
                slider(ui, &mut p.output_db, -60.0..=0.0, " dB", "output", LimiterParams::OUTPUT);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut p.bypass, "Bypass").changed() {
                        sends.push((LimiterParams::BYPASS, if p.bypass { 1.0 } else { 0.0 }));
                    }
                    if ui
                        .add(egui::Slider::new(&mut p.graph_speed, 1.0..=50.0).step_by(1.0).text("graph speed"))
                        .on_hover_text("lower = slower")
                        .changed()
                    {
                        sends.push((LimiterParams::SPEED, p.graph_speed));
                    }
                });
                crate::visual::draw_limiter(ui, meter, *gain, p.threshold_db);
            }
            NodeKind::Plugin(p) => {
                let _g = guard::enter(&p.tag);
                let info = p.info();
                let bridged = if info.is_bridged() { " · yabridge" } else { "" };
                ui.small(format!("{} · {}{bridged}", info.format.label(), info.vendor));
                ui.horizontal(|ui| {
                    let has_editor = p.editor.is_some();
                    if p.instance.has_editor() && ui.selectable_label(has_editor, "Editor").clicked() {
                        if has_editor {
                            p.editor = None;
                        } else {
                            let placement = self.embed_parent.map_or(Placement::Window, Placement::Embedded);
                            if let Err(e) = p.open_editor(placement) {
                                self.errors.push(format!("{e:#}"));
                            }
                        }
                    }
                    if p.is_embedded() && ui.button("Pop out").on_hover_text("open in its own window").clicked() {
                        if let Err(e) = p.open_editor(Placement::Window) {
                            self.errors.push(format!("{e:#}"));
                        }
                    }
                    ui.toggle_value(&mut p.show_params, "Params");
                    if ui.checkbox(&mut p.bypass, "Bypass").changed() {
                        sends.push((0, if p.bypass { 1.0 } else { 0.0 }));
                    }
                });
                if p.instance.latency() > 0 {
                    ui.small(format!("latency {} smp", p.instance.latency()));
                }
                if p.instance.has_body() {
                    let mut io = plugins::BodyIo { scroll: self.scroll, wants_scroll: false };
                    ui.push_id(("body", eid), |ui| p.instance.body_ui(ui, &mut io));
                    self.wants_scroll |= io.wants_scroll;
                }
                let embedded = p.is_embedded();
                if let Some(ed) = p.editor.as_mut().filter(|_| embedded) {
                    // Reserve the editor's actual pixel size on screen; the X11 child window is
                    // moved over this rect after the graph is drawn. The editor is asked to
                    // follow the zoom, which it does if it can scale or resize.
                    let ppp = self.ppp;
                    let xf = ui.ctx().layer_transform_to_global(ui.layer_id()).unwrap_or_default();
                    ed.set_zoom(ppp * xf.scaling);
                    let (w, h) = ed.size();
                    let k = ppp * xf.scaling;
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(w as f32 / k, h as f32 / k), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, Color32::from_gray(16));
                    let screen = xf * rect;
                    let clip = (xf * ui.clip_rect()).intersect(screen);
                    if clip.is_positive() {
                        let px = |v: f32| (v * ppp).round() as i32;
                        let r = PxRect {
                            x: px(clip.min.x),
                            y: px(clip.min.y),
                            w: px(clip.width()).max(0) as u32,
                            h: px(clip.height()).max(0) as u32,
                        };
                        self.placements.insert(eid, (r, (px(screen.min.x), px(screen.min.y))));
                    }
                }
            }
        });
        for (i, v) in sends {
            self.param(eid, i, v);
        }
    }

    fn has_graph_menu(&mut self, _pos: Pos2, _snarl: &mut Snarl<GraphNode>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: Pos2, ui: &mut egui::Ui, _snarl: &mut Snarl<GraphNode>) {
        if self.selected > 0 {
            if ui.button(format!("Save the {} selected node(s) as a node preset…", self.selected)).clicked() {
                self.actions.push(Action::NameNodePreset);
                ui.close();
            }
            ui.separator();
        }
        if !self.node_presets.is_empty() {
            ui.menu_button("Node presets", |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for name in self.node_presets {
                        if ui.button(name).clicked() {
                            self.actions.push(Action::AddNodePreset(name.clone(), pos));
                            ui.close();
                        }
                    }
                });
            });
        }
        ui.label("Add node");
        let mut add = |ui: &mut egui::Ui, label: &str, kind: fn() -> NodeKind| {
            if ui.button(label).clicked() {
                self.actions.push(Action::Add(kind(), pos));
                ui.close();
            }
        };
        if !self.has_player {
            add(ui, "Player", || NodeKind::Player);
        }
        add(ui, "Output", NodeKind::output);
        add(ui, "Gain", || NodeKind::Gain { db: 0.0 });
        add(ui, "Pan", || NodeKind::Pan { pan: 0.0 });
        add(ui, "Playback Rate Shift", || NodeKind::RateShift { rate: 1.0, bypass: false, active: false });
        add(ui, "EQ", || NodeKind::eq(EqSettings::default()));
        add(ui, "Delay", NodeKind::delay);
        add(ui, "Mixer", || NodeKind::Mixer { gains_db: [0.0; MIXER_INPUTS] });
        add(ui, "Kazu Limiter", NodeKind::limiter);
        add(ui, "Lyrics", || NodeKind::Lyrics(Box::new(LyricsNode::new(Default::default()))));
        add(ui, "Video", || NodeKind::Video(Box::new(VideoNode::new(Default::default()))));
        ui.menu_button("MIDI", |ui| {
            let mut add = |ui: &mut egui::Ui, label: &str, make: &dyn Fn() -> NodeKind| {
                if ui.button(label).clicked() {
                    self.actions.push(Action::Add(make(), pos));
                    ui.close();
                }
            };
            add(ui, "MIDI In (ALSA)", &|| NodeKind::MidiIn(Box::new(SeqNode::new(Dir::In))));
            add(ui, "MIDI Out (ALSA)", &|| NodeKind::MidiOut(Box::new(SeqNode::new(Dir::Out))));
            add(ui, "MIDI Monitor", &|| NodeKind::MidiMonitor(Box::new(Monitor::new())));
            let sr = self.sample_rate;
            add(ui, "MIDI File Player", &|| NodeKind::MidiFile(Box::new(MidiFileNode::new(Default::default(), sr))));
            add(ui, "CSynth (piano / SoundFont)", &|| NodeKind::CSynth(Box::new(CSynthNode::new(Default::default(), sr))));
            add(ui, "Audio → MIDI", &|| NodeKind::Audio2Midi(Box::new(Audio2Midi::new(sr))));
        });
        ui.menu_button("Visualizer", |ui| {
            for k in VisKind::ALL {
                if ui.button(k.label()).clicked() {
                    self.actions.push(Action::Add(NodeKind::Visual(Box::new(Visualizer::new(k))), pos));
                    ui.close();
                }
            }
        });
        if !self.plugins.is_empty() {
            ui.menu_button("Plugins", |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for info in self.plugins {
                        if ui.button(&info.name).clicked() {
                            self.actions.push(Action::AddPlugin(info.clone(), pos));
                            ui.close();
                        }
                    }
                });
            });
        }
    }

    fn has_node_menu(&mut self, _node: &GraphNode) -> bool {
        true
    }

    fn show_node_menu(
        &mut self,
        node: NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        _snarl: &mut Snarl<GraphNode>,
    ) {
        if ui.button("Remove").clicked() {
            self.actions.push(Action::Remove(node));
            ui.close();
        }
    }
}

fn audio2midi_body(ui: &mut egui::Ui, a: &mut Audio2Midi, eid: u32, sample_rate: f32) {
    let dests = a.destinations();
    let dest = a.dest();
    let current = dest.as_ref().map_or_else(|| "— subscribers only —".to_string(), |d| d.name.clone());
    ui.horizontal(|ui| {
        ui.label("MIDI out");
        let mut pick = None;
        let r = egui::ComboBox::from_id_salt(("a2m-dest", eid)).width(220.0).selected_text(current).show_ui(ui, |ui| {
            if ui.selectable_label(dest.is_none(), "— subscribers only —").clicked() {
                pick = Some(None);
            }
            for d in &dests {
                let selected = dest.as_ref().is_some_and(|x| x.addr == d.addr);
                if ui.selectable_label(selected, &d.name).clicked() {
                    pick = Some(Some(d.clone()));
                }
            }
        });
        if r.response.clicked() {
            a.refresh();
        }
        if let Some(p) = pick {
            a.connect(p);
        }
        if ui.small_button("⟳").on_hover_text("rescan ALSA sequencer ports").clicked() {
            a.refresh();
        }
    });
    if let Some(own) = a.own_port() {
        ui.small(format!("port nodeplayer:Audio2MIDI ({}:{}) — connectable with aconnect / qpwgraph", own.client, own.port));
    }
    if let Some(e) = a.error() {
        ui.colored_label(Color32::LIGHT_RED, e);
    }

    let mut c = a.config;
    ui.horizontal(|ui| {
        ui.toggle_value(&mut a.show_settings, "⚙").on_hover_text("detailed settings");
        ui.checkbox(&mut c.amplitude_notes, "level").on_hover_text("peak level → note (base note + channel)");
        ui.checkbox(&mut c.fft_peaks, "FFT peaks").on_hover_text("10 strongest spectral peaks");
        ui.checkbox(&mut c.pitch_detect, "pitch").on_hover_text("McLeod pitch detection");
        ui.add_enabled(c.fft_peaks, egui::Checkbox::new(&mut c.pitch_bend, "bend"))
            .on_hover_text("pitch bend (±2 semitones) for FFT peak notes");
    });
    ui.add(egui::Slider::new(&mut c.volume_percent, 0.0..=100.0).suffix(" %").text("volume"));
    ui.add(egui::Slider::new(&mut c.note_ms, 1.0..=1000.0).logarithmic(true).suffix(" ms").text("note length"));
    if a.show_settings {
        // Spacing rather than separators, which would stretch the node to the graph's width.
        ui.add_space(4.0);
        crate::audio2midi::settings_ui(ui, &mut c, a.gain(), a.level_db(), sample_rate);
        ui.add_space(4.0);
    }
    if c != a.config {
        a.set_config(c);
    }
    ui.horizontal(|ui| {
        if ui.button("Panic").on_hover_text("all notes off on every channel").clicked() {
            a.panic();
        }
        let rate = a.event_rate();
        ui.small(format!("auto gain ×{:.2} · {rate:.0} events/s", a.gain()));
    });

    // Sounding notes on a 128-key strip; C notes are marked.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(384.0, 26.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_gray(20));
    let w = rect.width() / 128.0;
    for n in 0..128u8 {
        let black = matches!(n % 12, 1 | 3 | 6 | 8 | 10);
        let x = rect.left() + n as f32 * w;
        let key = egui::Rect::from_min_max(pos2(x, rect.top()), pos2(x + w - 0.5, rect.bottom()));
        let color = if a.note_active(n) {
            Color32::from_rgb(90, 200, 255)
        } else if black {
            Color32::from_gray(35)
        } else {
            Color32::from_gray(150)
        };
        p.rect_filled(key, 0.0, color);
        if n % 12 == 0 {
            p.text(pos2(x, rect.bottom() + 1.0), egui::Align2::LEFT_TOP, format!("C{}", n as i32 / 12 - 1), egui::FontId::proportional(8.0), Color32::from_gray(120));
        }
    }
    ui.add_space(10.0);
}

/// One full-width row laid out from the right, so a widget added last with infinite desired
/// width fills the space left over.
pub(crate) fn right_to_left_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let height = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), height),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    );
}

/// A full-width, left-aligned, truncated list row that highlights when hovered or selected.
/// One cell of the track table: `text` on a line, cut with "…" to fit.
fn table_cell(ui: &egui::Ui, painter: &egui::Painter, cell: egui::Rect, text: &str, color: Color32, right: bool, pad: f32) {
    if text.is_empty() {
        return;
    }
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: egui::TextStyle::Body.resolve(ui.style()), color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((cell.width() - 2.0 * pad).max(0.0));
    let galley = painter.layout_job(job);
    let y = cell.center().y - galley.size().y * 0.5;
    let x = if right { cell.right() - pad - galley.size().x } else { cell.left() + pad };
    painter.with_clip_rect(cell).galley(pos2(x, y), galley, color);
}

fn list_row(ui: &mut egui::Ui, height: f32, selected: bool, text: egui::RichText, sense: egui::Sense) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), sense);
    let visuals = ui.style().interact_selectable(&resp, selected);
    if selected || (resp.hovered() && sense.senses_click()) {
        ui.painter().rect_filled(rect, 2.0, visuals.weak_bg_fill);
    }
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        rect.width() - 8.0,
        egui::TextStyle::Body,
    );
    let pos = pos2(rect.left() + 4.0, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley, visuals.text_color());
    resp
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn xlib_display(frame: &eframe::Frame) -> *mut std::ffi::c_void {
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    match frame.display_handle().map(|h| h.as_raw()) {
        Ok(RawDisplayHandle::Xlib(h)) => h.display.map_or(std::ptr::null_mut(), |d| d.as_ptr()),
        _ => std::ptr::null_mut(),
    }
}

/// The main window as plugin editors can be embedded into it: an X11 window (not a Wayland
/// surface), a Win32 HWND, an NSView.
fn native_window_id(frame: &eframe::Frame) -> Option<crate::plugins::WindowId> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as _),
        RawWindowHandle::Xcb(h) => Some(h.window.get() as _),
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as _),
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr() as _),
        _ => None,
    }
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
