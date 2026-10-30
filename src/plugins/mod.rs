//! Format-independent plugin hosting: what a scan finds ([`PluginInfo`]), what an instance
//! offers the UI ([`Instance`]) and native editor windows ([`Editor`]). Each format lives in
//! its own module and only has to produce these plus an audio-thread [`Processor`].

pub mod dylib;
pub mod guard;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub mod x11win;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use x11win as window;
#[cfg(target_os = "windows")]
pub mod win32win;
#[cfg(target_os = "windows")]
pub use win32win as window;
#[cfg(target_os = "macos")]
pub mod nowin;
#[cfg(target_os = "macos")]
pub use nowin as window;

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::engine::Processor;

/// A native window: an X11 window ID, a Win32 HWND, an NSView pointer.
pub type WindowId = usize;

/// Where an editor is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Its own top-level window.
    Window,
    /// A child of the given window (the app's main window).
    Embedded(WindowId),
}

/// Pixel rectangle in the parent window's coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// What happened to an editor's window since the last `HostWindow::poll`.
pub enum WinEvent {
    /// The user closed a top-level editor window.
    Close,
    /// The user or window manager resized a top-level window.
    Resized(u32, u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Format {
    Vst3,
    Clap,
    Jsfx,
    Native,
}

impl Format {
    pub const ALL: [Format; 4] = [Self::Vst3, Self::Clap, Self::Jsfx, Self::Native];

    pub fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Clap => "CLAP",
            Self::Jsfx => "JSFX",
            Self::Native => "Native",
        }
    }

    fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.label() == s)
    }
}

#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub format: Format,
    pub name: String,
    pub vendor: String,
    /// "Fx|EQ"-style categories (CLAP features and JSFX tags are joined the same way).
    pub category: String,
    /// The class inside `path`: VST3 class ID as hex, CLAP plugin id, native node id; empty
    /// for JSFX, where the file is the plugin.
    pub id: String,
    pub path: PathBuf,
}

impl PluginInfo {
    pub fn is_instrument(&self) -> bool {
        self.category.split('|').any(|s| s.eq_ignore_ascii_case("instrument"))
    }

    pub fn is_bridged(&self) -> bool {
        self.path.components().any(|c| c.as_os_str() == "yabridge")
    }

    /// One line of the scan cache / scanner output (without the path).
    pub fn to_line(&self) -> String {
        format!("{}\t{}\t{}\t{}\t{}", self.format.label(), self.id, self.name, self.vendor, self.category)
    }

    pub fn from_line(line: &str, path: &Path) -> Option<Self> {
        let mut it = line.split('\t');
        let format = Format::from_label(it.next()?)?;
        Some(Self {
            format,
            id: it.next()?.to_string(),
            name: it.next()?.to_string(),
            vendor: it.next().unwrap_or_default().to_string(),
            category: it.next().unwrap_or_default().to_string(),
            path: path.to_path_buf(),
        })
    }

    /// Same plugin as `other` (what a saved graph refers to).
    pub fn same(&self, format: Format, path: &Path, id: &str) -> bool {
        self.format == format && self.path == path && self.id == id
    }
}

#[derive(Clone, Debug)]
pub struct ParamDesc {
    pub id: u32,
    pub title: String,
    pub units: String,
    /// 0 for continuous parameters, else the number of steps.
    pub step_count: i32,
    /// Normalized default.
    pub default: f64,
    pub hidden: bool,
    pub read_only: bool,
}

/// The UI-thread side of a plugin instance. Parameter values are normalized to 0..1.
pub trait Instance {
    fn info(&self) -> &PluginInfo;
    fn params(&self) -> &[ParamDesc];
    fn param_value(&self, id: u32) -> f64;
    fn param_display(&self, id: u32, value: f64) -> String;
    fn set_param(&mut self, id: u32, value: f64);
    fn latency(&self) -> u32 {
        0
    }
    /// Whether the plugin takes MIDI in and sends MIDI out.
    fn midi_ports(&self) -> (bool, bool) {
        (false, false)
    }
    /// Housekeeping; called regularly on the UI thread.
    fn idle(&mut self) {}
    /// The plugin asked to be restarted (to apply new ports, latency or the like). The host
    /// then takes its processor off the audio thread and calls [`restart`](Self::restart).
    fn wants_restart(&mut self) -> bool {
        false
    }
    /// Reactivates the plugin once its old processor is gone; returns the new one.
    fn restart(&mut self, _sample_rate: f64, _max_block: usize) -> Result<Box<dyn Processor>> {
        bail!("{} can't restart", self.info().name)
    }
    fn save_state(&self) -> Option<Vec<u8>>;
    fn load_state(&mut self, data: &[u8]) -> Result<()>;

    /// A native editor window the plugin draws itself.
    fn has_editor(&self) -> bool {
        false
    }
    fn open_editor(&mut self, _placement: Placement) -> Result<Box<dyn Editor>> {
        bail!("{} has no editor", self.info().name)
    }

    /// An interface drawn by the host with egui inside the node (JSFX sliders and graphics,
    /// native plugins' UIs).
    fn has_body(&self) -> bool {
        false
    }
    fn body_ui(&mut self, _ui: &mut egui::Ui, _io: &mut BodyIo) {}
    /// The body changes on its own (graphics), so the UI should repaint continuously.
    fn animating(&self) -> bool {
        false
    }
}

/// What a node body exchanges with the graph view besides drawing.
#[derive(Default)]
pub struct BodyIo {
    /// Mouse wheel taken from the graph view (it pans otherwise); see `wants_scroll`.
    pub scroll: egui::Vec2,
    /// Set while the pointer is over something that uses the wheel, so next frame's wheel
    /// movement goes to the body instead of panning the graph.
    pub wants_scroll: bool,
}

/// A native editor window, embedded or on its own.
pub trait Editor {
    fn placement(&self) -> Placement;
    /// Size in physical pixels.
    fn size(&self) -> (u32, u32);
    /// Asks the editor to draw at `factor` (graph zoom × display scale), if it can.
    fn set_zoom(&mut self, _factor: f32) {}
    /// See [`window::HostWindow::place`].
    fn place(&mut self, clip: Option<PxRect>, origin: (i32, i32));
    /// Pumps the editor; returns `false` once the user closed its window.
    fn idle(&mut self) -> bool;
}

pub struct Created {
    pub instance: Box<dyn Instance>,
    pub processor: Box<dyn Processor>,
}

/// Loads plugin binaries once and creates instances of any format.
#[derive(Default)]
pub struct Host {
    pub vst3: crate::vst3::ModuleCache,
    pub clap: crate::clap::ModuleCache,
    pub native: crate::native::LibCache,
    /// Plugin files that crashed nodeplayer; see [`guard`].
    pub disabled: Vec<(Format, PathBuf)>,
}

impl Host {
    pub fn is_disabled(&self, format: Format, path: &Path) -> bool {
        self.disabled.iter().any(|(f, p)| *f == format && p == path)
    }

    pub fn create(&mut self, info: &PluginInfo, sample_rate: f32, max_block: usize) -> Result<Created> {
        if self.is_disabled(info.format, &info.path) {
            bail!("disabled because it crashed nodeplayer; enable it again in the Plugins tab");
        }
        let _g = guard::enter(&guard::Tag::of(info));
        match info.format {
            Format::Vst3 => {
                let module = self.vst3.get(&info.path)?;
                let (plugin, proc) = crate::vst3::Vst3Plugin::create(module, info, sample_rate as f64, max_block)?;
                Ok(Created { instance: Box::new(plugin), processor: Box::new(proc) })
            }
            Format::Clap => {
                let module = self.clap.get(&info.path)?;
                let (plugin, proc) = crate::clap::ClapPlugin::create(module, info, sample_rate as f64, max_block)?;
                Ok(Created { instance: Box::new(plugin), processor: Box::new(proc) })
            }
            Format::Jsfx => {
                let (plugin, proc) = crate::jsfx::JsfxPlugin::create(info, sample_rate, max_block)?;
                Ok(Created { instance: Box::new(plugin), processor: Box::new(proc) })
            }
            Format::Native => {
                let lib = self.native.get(&info.path)?;
                let (node, proc) = crate::native::NativeNode::create(lib, info, sample_rate, max_block)?;
                Ok(Created { instance: Box::new(node), processor: Box::new(proc) })
            }
        }
    }

    /// Classes in a plugin file, loading it in this process (the scanner subprocess uses this).
    pub fn classes(&mut self, format: Format, path: &Path) -> Result<Vec<PluginInfo>> {
        match format {
            Format::Vst3 => Ok(self.vst3.get(path)?.classes(path)),
            Format::Clap => Ok(self.clap.get(path)?.classes(path)),
            Format::Jsfx => crate::jsfx::classes(path),
            Format::Native => Ok(self.native.get(path)?.classes()),
        }
    }
}

// ---------------------------------------------------------------------------
// Scanning

/// The standard plugin folders of a format: `~/.vst3` and `/usr/lib/vst3`-like `unix` ones
/// on Linux, `Common Files\VST3` on Windows, `Library/Audio/Plug-Ins/VST3` on macOS. `name`
/// is the format's folder name on Windows and macOS, `home` the one in the home folder on
/// Linux.
pub fn standard_dirs(name: &str, home: &str, unix: &[&str]) -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = name;
        v.extend(crate::dirs::home().map(|h| h.join(home)));
        v.extend(unix.iter().map(PathBuf::from));
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (home, unix);
        v.extend(crate::dirs::home().map(|h| h.join("Library/Audio/Plug-Ins").join(name)));
        v.push(Path::new("/Library/Audio/Plug-Ins").join(name));
    }
    #[cfg(windows)]
    {
        let _ = (home, unix);
        // Per-user installs, then the shared folder ("C:\Program Files\Common Files").
        v.extend(std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join("Programs\\Common").join(name)));
        let common = std::env::var_os("CommonProgramFiles").map_or_else(|| PathBuf::from("C:\\Program Files\\Common Files"), PathBuf::from);
        v.push(common.join(name));
    }
    v
}

/// The format of a plugin file picked by hand, from its name (and contents for JSFX).
pub fn detect_format(path: &Path) -> Option<Format> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("vst3") => Some(Format::Vst3),
        Some("clap") => Some(Format::Clap),
        Some(e) if e == dylib::EXT => Some(Format::Native),
        _ => crate::jsfx::is_jsfx_file(path).then_some(Format::Jsfx),
    }
}

fn extra_files_path() -> Option<PathBuf> {
    Some(crate::session::config_dir()?.join("extra_plugins.txt"))
}

/// Plugin files added by hand outside the search paths; scans include them.
pub fn extra_files() -> Vec<PathBuf> {
    let Some(text) = extra_files_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect()
}

pub fn add_extra_file(path: &Path) {
    let mut files = extra_files();
    if files.iter().any(|f| f == path) {
        return;
    }
    files.push(path.to_path_buf());
    let Some(p) = extra_files_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = files.iter().map(|f| format!("{}\n", f.display())).collect();
    if let Err(e) = std::fs::write(&p, text) {
        log::warn!("can't save {}: {e}", p.display());
    }
}

/// Plugin files of every format below the standard locations, without loading them.
pub fn find_all() -> Vec<(Format, PathBuf)> {
    let mut out: Vec<(Format, PathBuf)> =
        extra_files().into_iter().filter(|p| p.exists()).filter_map(|p| Some((detect_format(&p)?, p))).collect();
    for dir in crate::vst3::default_search_paths() {
        let mut v = Vec::new();
        crate::vst3::find_bundles(&dir, &mut v);
        out.extend(v.into_iter().map(|p| (Format::Vst3, p)));
    }
    for dir in crate::clap::default_search_paths() {
        let mut v = Vec::new();
        crate::clap::find_files(&dir, &mut v);
        out.extend(v.into_iter().map(|p| (Format::Clap, p)));
    }
    for dir in crate::jsfx::default_search_paths() {
        let mut v = Vec::new();
        crate::jsfx::find_files(&dir, &mut v);
        out.extend(v.into_iter().map(|p| (Format::Jsfx, p)));
    }
    for dir in crate::native::default_search_paths() {
        let mut v = Vec::new();
        crate::native::find_files(&dir, &mut v);
        out.extend(v.into_iter().map(|p| (Format::Native, p)));
    }
    out
}

/// Scans one plugin file in a child process so a crashing plugin can't take the app down.
pub fn scan_isolated(format: Format, path: &Path) -> Result<Vec<PluginInfo>> {
    // Parsing a script can't crash the way loading a binary can.
    if format == Format::Jsfx {
        return crate::jsfx::classes(path);
    }
    let exe = std::env::current_exe()?;
    let out = std::process::Command::new(exe)
        .arg("--scan-plugin")
        .arg(format.label())
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("scanner exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| PluginInfo::from_line(l, path)).collect())
}

/// Entry point for `--scan-plugin <format> <path>`: one line per class on stdout.
pub fn scan_main(format: &str, path: &Path) -> Result<()> {
    let format = Format::from_label(format).ok_or_else(|| anyhow!("unknown format {format}"))?;
    for info in Host::default().classes(format, path)? {
        println!("{}", info.to_line());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scan cache: `path \t format \t id \t name \t vendor \t category` per line.

fn cache_dir() -> Option<PathBuf> {
    crate::dirs::cache()
}

pub fn load_cache() -> Vec<PluginInfo> {
    let Some(dir) = cache_dir() else { return Vec::new() };
    let parse = |text: &str, legacy: bool| -> Vec<PluginInfo> {
        text.lines()
            .filter_map(|l| {
                let (path, rest) = l.split_once('\t')?;
                let path = Path::new(path);
                if !path.exists() {
                    return None;
                }
                // The VST3-only cache had no format column.
                if legacy { PluginInfo::from_line(&format!("VST3\t{rest}"), path) } else { PluginInfo::from_line(rest, path) }
            })
            .collect()
    };
    match std::fs::read_to_string(dir.join("plugins.tsv")) {
        Ok(text) => parse(&text, false),
        Err(_) => std::fs::read_to_string(dir.join("vst3.tsv")).map(|t| parse(&t, true)).unwrap_or_default(),
    }
}

pub fn save_cache(plugins: &[PluginInfo]) {
    let Some(dir) = cache_dir() else { return };
    let text: String = plugins.iter().map(|p| format!("{}\t{}\n", p.path.display(), p.to_line())).collect();
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = std::fs::write(dir.join("plugins.tsv"), text) {
        log::warn!("can't write plugin cache: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_lines_round_trip() {
        let i = PluginInfo {
            format: Format::Clap,
            name: "Comp".into(),
            vendor: "LSP".into(),
            category: "audio-effect|compressor".into(),
            id: "in.lsp-plug.comp".into(),
            path: "/x/lsp.clap".into(),
        };
        let back = PluginInfo::from_line(&i.to_line(), &i.path).unwrap();
        assert!(back.same(Format::Clap, Path::new("/x/lsp.clap"), "in.lsp-plug.comp"));
        assert_eq!(back.category, i.category);
        assert!(PluginInfo::from_line("LV2\tx\ty", Path::new("/x")).is_none());
    }

    #[test]
    fn formats_from_file_names() {
        assert_eq!(detect_format(Path::new("/a/b.clap")), Some(Format::Clap));
        assert_eq!(detect_format(Path::new("/a/b.vst3")), Some(Format::Vst3));
        assert_eq!(detect_format(Path::new(&format!("/a/libx.{}", dylib::EXT))), Some(Format::Native));
        assert_eq!(detect_format(Path::new("/a/missing.png")), None);
    }
}
