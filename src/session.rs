//! Saved state: the session restored at startup and named graph presets, as JSON under
//! `~/.config/nodeplayer/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio2midi;
use crate::engine::eq::{EqParams, EqSettings};
use crate::engine::nodes::{LimiterParams, MIXER_INPUTS};
use crate::library::Repeat;
use crate::plugins::Format;
use crate::visual::VisSettings;

/// A node graph: nodes with their settings and the wires between them.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SavedGraph {
    pub nodes: Vec<SavedNode>,
    /// `[from node, output, to node, input]`, node being an index into `nodes`.
    pub wires: Vec<[usize; 4]>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedNode {
    pub pos: [f32; 2],
    #[serde(default = "yes")]
    pub open: bool,
    pub kind: SavedKind,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SavedKind {
    Player,
    Output { volume_db: f32 },
    Gain { db: f32 },
    Pan { pan: f32 },
    RateShift { rate: f32, bypass: bool },
    /// The single-band EQ of older versions; loaded as a one-band [`SavedKind::ParamEq`].
    Eq(EqParams),
    ParamEq(EqSettings),
    Delay { time_ms: f32, feedback: f32, mix: f32 },
    Mixer { gains_db: [f32; MIXER_INPUTS] },
    Limiter(LimiterParams),
    Visual(VisSettings),
    Audio2Midi {
        config: audio2midi::Config,
        /// Label of the connected sequencer port.
        dest: Option<String>,
    },
    /// ALSA sequencer ports; `port` is the label of the connected peer.
    MidiIn { port: Option<String> },
    MidiOut { port: Option<String> },
    MidiMonitor,
    CSynth(crate::csynth_node::CSynthSettings),
    Lyrics(crate::lyrics::LyricsSettings),
    Video(crate::video::VideoSettings),
    /// The MIDI File Player node of older versions; the Player plays MIDI files now, so only
    /// its wires are kept (see `insert_graph`).
    MidiFile(serde_json::Value),
    /// A hosted plugin of any format.
    Plugin {
        format: Format,
        path: PathBuf,
        /// The class inside `path`; see [`crate::plugins::PluginInfo::id`].
        #[serde(default)]
        id: String,
        name: String,
        bypass: bool,
        /// Whether the editor was open.
        #[serde(default)]
        editor: bool,
        /// Base64 of the plugin's state.
        state: Option<String>,
    },
    /// VST3 plugins as saved before other formats existed.
    Vst3 {
        path: PathBuf,
        /// Class ID as hex, to pick the class inside the bundle.
        cid: String,
        name: String,
        bypass: bool,
        /// Whether the editor was open.
        #[serde(default)]
        editor: bool,
        /// Base64 of the component / edit controller state.
        component: Option<String>,
        controller: Option<String>,
    },
}

impl SavedKind {
    /// Rewrites the VST3-only form as [`SavedKind::Plugin`].
    pub fn upgraded(&self) -> Option<SavedKind> {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;
        let SavedKind::Vst3 { path, cid, name, bypass, editor, component, controller } = self else { return None };
        let decode = |s: &Option<String>| s.as_ref().and_then(|s| BASE64.decode(s).ok());
        let (c, e) = (decode(component), decode(controller));
        let state = (c.is_some() || e.is_some())
            .then(|| BASE64.encode(crate::vst3::pack_states(c.as_deref(), e.as_deref())));
        Some(SavedKind::Plugin {
            format: Format::Vst3,
            path: path.clone(),
            id: cid.clone(),
            name: name.clone(),
            bypass: *bypass,
            editor: *editor,
            state,
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub graph: Option<SavedGraph>,
    /// The library's single folder, as saved before several sources were possible.
    pub library_root: Option<PathBuf>,
    pub library_sources: Vec<crate::library::Source>,
    pub library_sort: Option<(crate::library::Column, bool)>,
    pub library_columns: Option<crate::library::Columns>,
    pub track: Option<PathBuf>,
    pub position: f64,
    pub repeat: Option<Repeat>,
    pub shuffle: bool,
    /// Playback rate shift; absent means 1.
    #[serde(default)]
    pub rate: Option<f32>,
    /// Leave out channel 10 (drums) when playing MIDI files.
    pub midi_no_drums: bool,
    /// How the panels are docked; dropped (back to the default) if it can't be read.
    #[serde(deserialize_with = "dock_layout")]
    pub dock: Option<egui_dock::DockState<crate::app::Tab>>,
    /// The panels there were when the layout was saved; newer ones are added to it.
    #[serde(deserialize_with = "known_tabs")]
    pub tabs_seen: Vec<crate::app::Tab>,
}

/// The tabs this version knows, skipping any it doesn't.
fn known_tabs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<crate::app::Tab>, D::Error> {
    let v = <Vec<serde_json::Value> as Deserialize>::deserialize(d)?;
    Ok(v.into_iter().filter_map(|t| serde_json::from_value(t).ok()).collect())
}

/// `None` instead of failing the whole session, e.g. for a tab a later version dropped.
fn dock_layout<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<egui_dock::DockState<crate::app::Tab>>, D::Error> {
    // Areas not laid out yet are infinite, which JSON writes as null; they're worked out
    // again on the first frame anyway.
    fn zero_nulls(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if x.is_null() && (k == "x" || k == "y") {
                        *x = 0.0.into();
                    } else {
                        zero_nulls(x);
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(zero_nulls),
            _ => {}
        }
    }
    let mut v = <serde_json::Value as Deserialize>::deserialize(d)?;
    zero_nulls(&mut v);
    Ok(serde_json::from_value(v).map_err(|e| log::warn!("session layout: {e}")).ok())
}

pub fn config_dir() -> Option<PathBuf> {
    crate::dirs::config()
}

fn session_path() -> Option<PathBuf> {
    Some(config_dir()?.join("session.json"))
}

fn presets_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("presets"))
}

/// Node presets: a few nodes and the wires between them, added into the graph as a group.
fn node_presets_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("node-presets"))
}

/// Writes through a temporary file so a crash mid-write can't leave a truncated file.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read(path).with_context(|| path.display().to_string())?;
    serde_json::from_slice(&text).with_context(|| path.display().to_string())
}

pub fn load_session() -> Option<Session> {
    let path = session_path()?;
    if !path.exists() {
        return None;
    }
    read_json(&path).map_err(|e| log::warn!("can't read session: {e:#}")).ok()
}

pub fn save_session(s: &Session) {
    let Some(path) = session_path() else { return };
    if let Err(e) = write_json(&path, s) {
        log::warn!("can't save session: {e:#}");
    }
}

/// Preset names, sorted.
pub fn list_presets() -> Vec<String> {
    list_in(presets_dir())
}

pub fn list_node_presets() -> Vec<String> {
    list_in(node_presets_dir())
}

fn list_in(dir: Option<PathBuf>) -> Vec<String> {
    let Some(dir) = dir else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "json" {
                return None;
            }
            Some(p.file_stem()?.to_string_lossy().into_owned())
        })
        .collect();
    names.sort_by(|a, b| crate::library::natural_cmp(a, b));
    names
}

fn preset_path(name: &str) -> Option<PathBuf> {
    path_in(presets_dir(), name)
}

fn path_in(dir: Option<PathBuf>, name: &str) -> Option<PathBuf> {
    // Keep the name usable as a file name.
    let safe: String = name.trim().chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
    (!safe.is_empty() && safe != "." && safe != "..").then(|| dir.map(|d| d.join(format!("{safe}.json"))))?
}

pub fn save_node_preset(name: &str, g: &SavedGraph) -> Result<()> {
    write_json(&path_in(node_presets_dir(), name).context("invalid preset name")?, g)
}

pub fn load_node_preset(name: &str) -> Result<SavedGraph> {
    read_json(&path_in(node_presets_dir(), name).context("invalid preset name")?)
}

pub fn delete_node_preset(name: &str) -> Result<()> {
    std::fs::remove_file(path_in(node_presets_dir(), name).context("invalid preset name")?)?;
    Ok(())
}

pub fn save_preset(name: &str, g: &SavedGraph) -> Result<()> {
    let path = preset_path(name).context("invalid preset name")?;
    write_json(&path, g)
}

pub fn load_preset(name: &str) -> Result<SavedGraph> {
    read_json(&preset_path(name).context("invalid preset name")?)
}

pub fn delete_preset(name: &str) -> Result<()> {
    let path = preset_path(name).context("invalid preset name")?;
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_layout_is_kept_or_dropped() {
        use crate::app::Tab;
        let mut dock = egui_dock::DockState::new(vec![Tab::Graph]);
        dock.main_surface_mut().split_left(egui_dock::NodeIndex::root(), 0.3, vec![Tab::Library, Tab::Misskey]);
        let s = Session { dock: Some(dock), shuffle: true, ..Default::default() };
        // Never shown, so its areas are still infinite (written as null).
        let back: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        let tabs: Vec<Tab> = back.dock.unwrap().iter_all_tabs().map(|(_, t)| *t).collect();
        assert_eq!(tabs.len(), 3);
        assert!(tabs.contains(&Tab::Misskey));
        // A layout that can't be read (a tab that no longer exists) doesn't lose the rest.
        let old = r#"{"shuffle": true, "dock": {"surfaces": [{"Main": {"nodes": [{"Leaf": {"tabs": ["Gone"]}}]}}]}}"#;
        let back: Session = serde_json::from_str(old).unwrap();
        assert!(back.dock.is_none() && back.shuffle);
        // Sessions from before the dock.
        let back: Session = serde_json::from_str(r#"{"shuffle": true}"#).unwrap();
        assert!(back.dock.is_none() && back.shuffle);
    }

    #[test]
    fn graph_round_trips_through_json() {
        let g = SavedGraph {
            nodes: vec![
                SavedNode { pos: [0.0, 0.0], open: true, kind: SavedKind::Player },
                SavedNode { pos: [100.0, 5.0], open: false, kind: SavedKind::Limiter(LimiterParams::default()) },
                SavedNode {
                    pos: [200.0, 0.0],
                    open: true,
                    kind: SavedKind::Vst3 {
                        path: "/x/OTT.vst3".into(),
                        cid: "00".into(),
                        name: "OTT".into(),
                        bypass: true,
                        editor: true,
                        component: Some("AAEC".into()),
                        controller: None,
                    },
                },
            ],
            wires: vec![[0, 0, 1, 0], [1, 0, 2, 0]],
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: SavedGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(back.nodes.len(), 3);
        assert_eq!(back.wires, g.wires);
        assert!(!back.nodes[1].open);
        assert!(matches!(&back.nodes[2].kind, SavedKind::Vst3 { bypass: true, name, .. } if name == "OTT"));
        let up = back.nodes[2].kind.upgraded().unwrap();
        assert!(matches!(&up, SavedKind::Plugin { format: Format::Vst3, id, state: Some(_), .. } if id == "00"));
    }

    #[test]
    fn single_band_eq_still_loads() {
        let json = r#"{"pos":[0,0],"kind":{"type":"Eq","kind":"LowShelf","freq":150.0,"q":0.7,"gain_db":-4.0}}"#;
        let n: SavedNode = serde_json::from_str(json).unwrap();
        let SavedKind::Eq(p) = n.kind else { panic!("not the old EQ") };
        let s = EqSettings::from_legacy(p);
        assert_eq!(s.bands.len(), 1);
        assert_eq!(s.bands[0].gain_db, -4.0);

        let back: SavedKind = serde_json::from_str(&serde_json::to_string(&SavedKind::ParamEq(s.clone())).unwrap()).unwrap();
        assert!(matches!(back, SavedKind::ParamEq(b) if b == s));
    }

    #[test]
    fn preset_names_stay_in_the_presets_dir() {
        assert!(preset_path("").is_none());
        assert!(preset_path("..").is_none());
        let p = preset_path("a/../b").unwrap();
        assert_eq!(p.parent(), presets_dir().as_deref());
    }
}
