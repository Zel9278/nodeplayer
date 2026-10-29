//! Thin wrapper around `yabridgectl`, which turns Windows VST3/VST2/CLAP plugins installed
//! in a Wine prefix into Linux plugins under `~/.vst3/yabridge` (and friends).

use std::path::{Path, PathBuf};
use std::process::Command;

use crossbeam_channel::{Receiver, Sender};

pub fn find_yabridgectl() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("yabridgectl")).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/share/yabridge/yabridgectl"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Common places Windows plugins end up inside the default Wine prefix.
pub fn default_windows_plugin_dirs() -> Vec<PathBuf> {
    let prefix = std::env::var_os("WINEPREFIX").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".wine"))
    });
    let Some(prefix) = prefix else { return Vec::new() };
    let c = prefix.join("drive_c");
    [
        "Program Files/Common Files/VST3",
        "Program Files/Common Files/CLAP",
        "Program Files/VstPlugins",
        "Program Files/Steinberg/VstPlugins",
    ]
    .iter()
    .map(|p| c.join(p))
    .filter(|p| p.is_dir())
    .collect()
}

pub struct JobResult {
    pub what: String,
    pub ok: bool,
    pub output: String,
}

pub struct Yabridge {
    pub exe: Option<PathBuf>,
    /// Directories yabridgectl is currently managing.
    pub dirs: Vec<String>,
    pub log: String,
    pub busy: bool,
    tx: Sender<JobResult>,
    rx: Receiver<JobResult>,
}

impl Default for Yabridge {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut y = Self { exe: find_yabridgectl(), dirs: Vec::new(), log: String::new(), busy: false, tx, rx };
        y.refresh_dirs();
        y
    }
}

impl Yabridge {
    pub fn available(&self) -> bool {
        self.exe.is_some()
    }

    fn run_sync(exe: &Path, args: &[&str]) -> (bool, String) {
        match Command::new(exe).args(args).output() {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.success(), strip_ansi(&s))
            }
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn refresh_dirs(&mut self) {
        let Some(exe) = &self.exe else { return };
        let (ok, out) = Self::run_sync(exe, &["list"]);
        if ok {
            self.dirs = out.lines().map(str::trim).filter(|l| l.starts_with('/')).map(String::from).collect();
        }
    }

    /// Runs a yabridgectl command in the background; results arrive via [`Yabridge::poll`].
    pub fn spawn(&mut self, args: Vec<String>) {
        let Some(exe) = self.exe.clone() else { return };
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let (ok, output) = Self::run_sync(&exe, &refs);
            let _ = tx.send(JobResult { what: format!("yabridgectl {}", args.join(" ")), ok, output });
        });
    }

    pub fn add_dir(&mut self, dir: &Path) {
        self.spawn(vec!["add".into(), dir.display().to_string()]);
    }

    pub fn remove_dir(&mut self, dir: &str) {
        self.spawn(vec!["rm".into(), dir.to_string()]);
    }

    pub fn sync(&mut self) {
        self.spawn(vec!["sync".into()]);
    }

    pub fn status(&mut self) {
        self.spawn(vec!["status".into()]);
    }

    /// Returns the result of a finished job, if any (e.g. to rescan plugins after a sync).
    pub fn poll(&mut self) -> Option<JobResult> {
        let r = self.rx.try_recv().ok()?;
        self.busy = false;
        self.log = format!("$ {}\n{}", r.what, r.output);
        self.refresh_dirs();
        Some(r)
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
