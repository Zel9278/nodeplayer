//! First-run setup, asked once in the window:
//! - FFmpeg, when neither nodeplayer's own copy nor one on the PATH is there: a static build
//!   (BtbN's FFmpeg-Builds on GitHub) can be downloaded into `~/.local/share/nodeplayer/ffmpeg/`
//!   (`%LOCALAPPDATA%\nodeplayer\ffmpeg` on Windows). There are no macOS builds there, so on
//!   macOS this points at Homebrew instead.
//! - When running from an AppImage: copying it to `~/.local/bin/nodeplayer.AppImage` with a
//!   menu entry and an icon.
//!
//! "Don't ask again" is kept in `~/.config/nodeplayer/setup.json`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const ICON: &[u8] = include_bytes!("../assets/icon.png");
const RELEASES: &str = "https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/latest";
const USER_AGENT: &str = concat!("nodeplayer/", env!("CARGO_PKG_VERSION"));

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Answers {
    /// Don't offer to download FFmpeg.
    no_ffmpeg: bool,
    /// Don't offer to install the AppImage.
    no_install: bool,
}

fn answers_path() -> Option<PathBuf> {
    Some(crate::session::config_dir()?.join("setup.json"))
}

impl Answers {
    fn load() -> Self {
        answers_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = answers_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&path, b)?)) {
            log::warn!("can't save {}: {e:#}", path.display());
        }
    }
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

fn bin_home() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/bin"))
}

use crate::dirs::tilde;

fn installed_appimage() -> Option<PathBuf> {
    Some(bin_home()?.join("nodeplayer.AppImage"))
}

/// The AppImage this runs from: `$APPIMAGE`, when this program is the one inside `$APPDIR`
/// (both are passed on to child processes, so that's checked).
pub fn running_appimage() -> Option<PathBuf> {
    let image = PathBuf::from(std::env::var_os("APPIMAGE")?);
    let dir = PathBuf::from(std::env::var_os("APPDIR")?);
    let exe = std::env::current_exe().ok()?;
    exe.starts_with(&dir).then_some(image)
}

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    Ffmpeg,
    Install,
}

#[derive(Clone, Default)]
enum Download {
    #[default]
    Idle,
    /// Bytes so far and in all (0 while unknown).
    Running(u64, u64, &'static str),
    Done,
    Failed(String),
}

pub struct Setup {
    answers: Answers,
    /// The questions still to ask, in order.
    queue: Vec<Ask>,
    download: Arc<Mutex<Download>>,
    /// The outcome of installing the AppImage, to show once.
    note: Option<String>,
}

impl Setup {
    pub fn new() -> Self {
        let answers = Answers::load();
        let mut queue = Vec::new();
        if !crate::av::available() && !answers.no_ffmpeg {
            queue.push(Ask::Ffmpeg);
        }
        if let Some(image) = running_appimage()
            && !answers.no_install
            && installed_appimage().is_some_and(|i| i != image)
        {
            queue.push(Ask::Install);
        }
        Self { answers, queue, download: Arc::default(), note: None }
    }

    /// Offers the FFmpeg download again (from the status bar), even after "don't ask".
    pub fn ask_ffmpeg(&mut self) {
        if !self.queue.contains(&Ask::Ffmpeg) {
            self.queue.insert(0, Ask::Ffmpeg);
        }
    }

    /// Whether FFmpeg is being downloaded.
    pub fn busy(&self) -> bool {
        matches!(*self.download.lock().unwrap(), Download::Running(..))
    }

    /// Shows the question at the front of the queue. Returns lines for the app's log.
    pub fn ui(&mut self, ctx: &egui::Context) -> Vec<String> {
        let mut log = Vec::new();
        if let Some(note) = self.note.clone() {
            let modal = egui::Modal::new(egui::Id::new("setup note")).show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(&note);
                ui.button("OK").clicked()
            });
            if modal.inner || modal.should_close() {
                self.note = None;
            }
            return log;
        }
        let Some(&ask) = self.queue.first() else { return log };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new(("setup", ask as u8))).show(ctx, |ui| {
            ui.set_max_width(440.0);
            match ask {
                Ask::Ffmpeg => close = self.ffmpeg_ui(ui, &mut log),
                Ask::Install => close = self.install_ui(ui, &mut log),
            }
        });
        // Only Esc, not a click beside the dialog, counts as "not now".
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.busy() {
            close = true;
        }
        let _ = modal;
        if close {
            self.queue.remove(0);
        }
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        log
    }

    fn ffmpeg_ui(&mut self, ui: &mut egui::Ui, log: &mut Vec<String>) -> bool {
        ui.heading("FFmpeg");
        let state = self.download.lock().unwrap().clone();
        match state {
            Download::Idle => {
                ui.label(
                    "FFmpeg isn't installed. nodeplayer uses it to play video and the audio formats it can't read by itself (Opus in WebM, AC-3, WMA and others), and to read tags for the library and Misskey.",
                );
                ui.add_space(4.0);
                let can_download = build_name().is_ok();
                if can_download {
                    let dir = crate::av::local_dir().map_or_else(String::new, |d| tilde(&d));
                    ui.label(format!("It can download a static build (about 150 MB, from BtbN's FFmpeg-Builds on GitHub) into {dir}. {}", if cfg!(windows) { "An FFmpeg installed some other way (winget, Scoop) works too; nodeplayer finds it on the PATH." } else { "FFmpeg from your distribution works too; nodeplayer finds it once it's installed." }));
                } else {
                    ui.label("Install it with Homebrew (brew install ffmpeg); nodeplayer finds it there.");
                }
                ui.add_space(8.0);
                let mut close = false;
                ui.horizontal(|ui| {
                    if can_download && ui.button("Download").clicked() {
                        self.start_download();
                    }
                    if ui.button("Not now").clicked() {
                        close = true;
                    }
                    if ui.button("Don't ask again").clicked() {
                        self.answers.no_ffmpeg = true;
                        self.answers.save();
                        close = true;
                    }
                });
                close
            }
            Download::Running(got, total, what) => {
                ui.label(what);
                let frac = if total > 0 { got as f32 / total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).text(format!("{:.0} / {:.0} MB", got as f64 / 1e6, total as f64 / 1e6)));
                false
            }
            Download::Done => {
                log.push("FFmpeg installed".into());
                *self.download.lock().unwrap() = Download::Idle;
                self.note = Some(format!(
                    "FFmpeg is installed in {}. Video and the other formats work now.",
                    crate::av::local_dir().map_or_else(String::new, |d| tilde(&d))
                ));
                true
            }
            Download::Failed(e) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("The download failed: {e}"));
                let mut close = false;
                ui.horizontal(|ui| {
                    if ui.button("Try again").clicked() {
                        self.start_download();
                    }
                    if ui.button("Close").clicked() {
                        *self.download.lock().unwrap() = Download::Idle;
                        close = true;
                    }
                });
                close
            }
        }
    }

    fn start_download(&self) {
        let state = self.download.clone();
        *state.lock().unwrap() = Download::Running(0, 0, "Looking for the latest build…");
        std::thread::spawn(move || {
            let result = download_ffmpeg(&state);
            *state.lock().unwrap() = match result {
                Ok(()) => Download::Done,
                Err(e) => {
                    log::warn!("FFmpeg download: {e:#}");
                    Download::Failed(format!("{e:#}"))
                }
            };
        });
    }

    fn install_ui(&mut self, ui: &mut egui::Ui, log: &mut Vec<String>) -> bool {
        ui.heading("Add nodeplayer to the app menu?");
        let target = installed_appimage().unwrap_or_default();
        let exists = target.exists();
        if exists {
            ui.label(format!("Another copy is installed in {}. Replace it with this one and keep the menu entry pointing at it?", tilde(&target)));
        } else {
            ui.label(format!("This copies the AppImage to {} and adds a menu entry and an icon, so it can be started from the app menu.", tilde(&target)));
        }
        ui.add_space(8.0);
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.button(if exists { "Replace" } else { "Install" }).clicked() {
                close = true;
                match install_appimage() {
                    Ok(path) => {
                        log.push(format!("installed {}", path.display()));
                        self.note = Some(format!("Installed. The menu entry starts {}; this file can be deleted.", tilde(&path)));
                    }
                    Err(e) => self.note = Some(format!("Couldn't install: {e:#}")),
                }
            }
            if ui.button("Not now").clicked() {
                close = true;
            }
            if ui.button("Don't ask again").clicked() {
                self.answers.no_install = true;
                self.answers.save();
                close = true;
            }
        });
        close
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_connect(Some(Duration::from_secs(20))).http_status_as_error(true).build().into()
}

/// The platform name of BtbN's builds for this machine and their archive type.
fn build_name() -> Result<(&'static str, &'static str)> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("linux64", ".tar.xz"),
        ("linux", "aarch64") => ("linuxarm64", ".tar.xz"),
        ("windows", "x86_64") => ("win64", ".zip"),
        ("windows", "aarch64") => ("winarm64", ".zip"),
        (os, a) => bail!("no FFmpeg build for {os} on {a}"),
    })
}

/// The newest release build for this machine from the release list: "ffmpeg-n9.0-latest-
/// linux64-gpl-9.0.tar.xz" and the like, else the master build.
fn pick_asset(release: &serde_json::Value, arch: &str, ext: &str) -> Option<(String, u64)> {
    let assets = release["assets"].as_array()?;
    let version = |name: &str| -> Option<(u32, u32)> {
        let rest = name.strip_prefix("ffmpeg-n")?;
        let (v, tail) = rest.split_once("-latest-")?;
        if !tail.starts_with(&format!("{arch}-gpl-")) || !tail.ends_with(ext) || tail.contains("shared") {
            return None;
        }
        let (a, b) = v.split_once('.')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    let url = |a: &serde_json::Value| Some((a["browser_download_url"].as_str()?.to_string(), a["size"].as_u64().unwrap_or(0)));
    let best = assets.iter().filter_map(|a| Some((version(a["name"].as_str()?)?, a))).max_by_key(|(v, _)| *v);
    match best {
        Some((_, a)) => url(a),
        None => assets.iter().find(|a| a["name"].as_str() == Some(&format!("ffmpeg-master-latest-{arch}-gpl{ext}"))).and_then(url),
    }
}

fn download_ffmpeg(state: &Mutex<Download>) -> Result<()> {
    let (arch, ext) = build_name()?;
    let dir = crate::av::local_dir().context("no home folder")?;
    let parent = dir.parent().context("no data folder")?.to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let agent = agent();
    // NODEPLAYER_FFMPEG_RELEASES points at another release list, for testing.
    let releases = std::env::var("NODEPLAYER_FFMPEG_RELEASES").unwrap_or_else(|_| RELEASES.into());
    let release: serde_json::Value = agent
        .get(&releases)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .with_config()
        .limit(16 << 20)
        .read_to_string()
        .map_err(anyhow::Error::from)
        .and_then(|t| Ok(serde_json::from_str(&t)?))?;
    let (url, size) = pick_asset(&release, arch, ext).ok_or_else(|| anyhow!("no {arch} build in the latest release"))?;
    log::info!("downloading {url}");

    let archive = parent.join(format!("ffmpeg-download{ext}"));
    let mut resp = agent.get(&url).header("User-Agent", USER_AGENT).call()?;
    let total = resp.body().content_length().unwrap_or(size);
    let mut body = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut file = std::fs::File::create(&archive)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        got += n as u64;
        *state.lock().unwrap() = Download::Running(got, total, "Downloading FFmpeg…");
    }
    drop(file);
    if total > 0 && got < total {
        bail!("the download stopped at {got} of {total} bytes");
    }

    *state.lock().unwrap() = Download::Running(got, total, "Unpacking…");
    let result = unpack(&archive, &dir);
    let _ = std::fs::remove_file(&archive);
    result
}

/// Takes `bin/ffmpeg` and `bin/ffprobe` out of the archive into `dir`, and checks they run.
fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    let tmp = dir.with_extension("unpacking");
    let _ = std::fs::remove_dir_all(&tmp);
    let result = unpack_into(archive, &tmp);
    if result.is_ok() {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::rename(tmp.join("bin"), dir)?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn unpack_into(archive: &Path, tmp: &Path) -> Result<()> {
    let all = tmp.join("all");
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&all)?;
    std::fs::create_dir_all(&bin)?;
    if archive.extension().is_some_and(|e| e == "zip") {
        unzip_programs(archive, &all)?;
    } else {
        let out = std::process::Command::new("tar")
            .arg("-xJf")
            .arg(archive)
            .arg("-C")
            .arg(&all)
            .args(["--wildcards", "*bin/ffmpeg", "*bin/ffprobe"])
            .output()
            .context("can't run tar")?;
        if !out.status.success() {
            bail!("tar: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    for name in ["ffmpeg", "ffprobe"].map(crate::av::exe_name) {
        let name = name.as_str();
        let found = find_file(&all, name).ok_or_else(|| anyhow!("no {name} in the archive"))?;
        std::fs::rename(&found, bin.join(name))?;
        let ok = std::process::Command::new(bin.join(name)).arg("-version").output().is_ok_and(|o| o.status.success());
        if !ok {
            bail!("the downloaded {name} doesn't run");
        }
    }
    Ok(())
}

/// Takes `bin/ffmpeg.exe` and `bin/ffprobe.exe` out of a Windows build's zip into `dir`, keeping
/// their folders like tar does.
fn unzip_programs(archive: &Path, dir: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?).context("not a zip file")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let wanted = ["ffmpeg.exe", "ffprobe.exe"].iter().any(|n| rel.file_name().is_some_and(|f| f == *n))
            && rel.parent().and_then(Path::file_name).is_some_and(|d| d == "bin");
        if !wanted || !entry.is_file() {
            continue;
        }
        let out = dir.join(rel);
        std::fs::create_dir_all(out.parent().unwrap_or(dir))?;
        std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
    }
    Ok(())
}

/// The first file called `name` under `dir`, in a `bin` folder.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) && p.parent().and_then(Path::file_name).is_some_and(|d| d == "bin") {
            return Some(p);
        }
    }
    None
}

/// Copies the running AppImage to `~/.local/bin` and writes its menu entry and icon.
fn install_appimage() -> Result<PathBuf> {
    let image = running_appimage().context("not running from an AppImage")?;
    let target = installed_appimage().context("no home folder")?;
    let data = data_home().context("no home folder")?;
    std::fs::create_dir_all(target.parent().unwrap())?;
    // Through a temporary name, in case the installed copy is running.
    let tmp = target.with_extension("AppImage.new");
    std::fs::copy(&image, &tmp).with_context(|| format!("copying {}", image.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &target)?;

    let icon_dir = data.join("icons/hicolor/256x256/apps");
    std::fs::create_dir_all(&icon_dir)?;
    std::fs::write(icon_dir.join("nodeplayer.png"), ICON)?;
    let apps = data.join("applications");
    std::fs::create_dir_all(&apps)?;
    std::fs::write(apps.join("nodeplayer.desktop"), desktop_entry(&target))?;
    // Menus pick the new entry up by themselves; this just makes it quicker where it exists.
    let _ = std::process::Command::new("update-desktop-database").arg(&apps).stderr(std::process::Stdio::null()).status();
    Ok(target)
}

fn desktop_entry(exe: &Path) -> String {
    // Exec quoting: a path with spaces or special characters goes in double quotes, with ",
    // `, $ and \ escaped by a backslash, which itself is written doubled in the file.
    let path = exe.display().to_string();
    let exec = if path.contains(|c: char| c.is_whitespace() || "\"'`$\\;&|<>()*?#~=%".contains(c)) {
        let mut q = String::new();
        for c in path.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push_str("\\\\");
            }
            q.push(c);
        }
        format!("\"{q}\"")
    } else {
        path
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=nodeplayer\n\
         Comment=Node-graph audio player with DSP nodes and VST3, CLAP and JSFX plugins\n\
         Exec={exec} %f\n\
         Icon=nodeplayer\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Player;\n\
         MimeType=audio/flac;audio/mpeg;audio/ogg;audio/opus;audio/x-wav;audio/wav;audio/mp4;audio/aac;audio/x-aiff;video/mp4;video/x-matroska;video/webm;video/quicktime;inode/directory;\n\
         StartupWMClass=nodeplayer\n"
    )
}

/// The window icon.
pub fn icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(ICON).ok()?.to_rgba8();
    Some(egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_newest_release_build() {
        let release = serde_json::json!({ "assets": [
            { "name": "ffmpeg-master-latest-linux64-gpl.tar.xz", "browser_download_url": "m", "size": 1 },
            { "name": "ffmpeg-n8.1-latest-linux64-gpl-8.1.tar.xz", "browser_download_url": "a", "size": 2 },
            { "name": "ffmpeg-n9.0-latest-linux64-gpl-9.0.tar.xz", "browser_download_url": "b", "size": 3 },
            { "name": "ffmpeg-n9.0-latest-linux64-gpl-shared-9.0.tar.xz", "browser_download_url": "c", "size": 4 },
            { "name": "ffmpeg-n9.0-latest-linux64-lgpl-9.0.tar.xz", "browser_download_url": "d", "size": 5 },
            { "name": "ffmpeg-n10.0-latest-win64-gpl-10.0.zip", "browser_download_url": "e", "size": 6 },
        ]});
        assert_eq!(pick_asset(&release, "linux64", ".tar.xz"), Some(("b".into(), 3)));
        assert_eq!(pick_asset(&release, "linuxarm64", ".tar.xz"), None);
        assert_eq!(pick_asset(&release, "win64", ".zip"), Some(("e".into(), 6)));
        let master_only = serde_json::json!({ "assets": [{ "name": "ffmpeg-master-latest-linuxarm64-gpl.tar.xz", "browser_download_url": "m", "size": 1 }] });
        assert_eq!(pick_asset(&master_only, "linuxarm64", ".tar.xz"), Some(("m".into(), 1)));
    }

    #[test]
    fn desktop_entry_quotes_odd_paths() {
        assert!(desktop_entry(Path::new("/home/a/.local/bin/nodeplayer.AppImage")).contains("Exec=/home/a/.local/bin/nodeplayer.AppImage %f\n"));
        assert!(desktop_entry(Path::new("/home/a b/x.AppImage")).contains("Exec=\"/home/a b/x.AppImage\" %f\n"));
    }

    #[test]
    #[cfg(unix)]
    fn unpacks_the_two_programs() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("nodeplayer-unpack-{}", std::process::id()));
        let bin = base.join("src/ffmpeg-n9.0-latest-linux64-gpl-9.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["ffmpeg", "ffprobe", "ffplay"] {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\necho {name} version test\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let archive = base.join("a.tar.xz");
        let ok = std::process::Command::new("tar").arg("-cJf").arg(&archive).arg("-C").arg(base.join("src")).arg(".").status();
        if !ok.is_ok_and(|s| s.success()) {
            return;
        }
        let dir = base.join("out/ffmpeg");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        unpack(&archive, &dir).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["ffmpeg", "ffprobe"]);
        // Unpacking again replaces the old copy.
        unpack(&archive, &dir).unwrap();
        assert!(!dir.with_extension("unpacking").exists());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn unzips_the_two_programs() {
        use std::io::Write;
        let base = std::env::temp_dir().join(format!("nodeplayer-unzip-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let archive = base.join("a.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for name in ["ffmpeg.exe", "ffprobe.exe", "ffplay.exe"] {
            zip.start_file(format!("ffmpeg-n9.0-latest-win64-gpl-9.0/bin/{name}"), opts).unwrap();
            zip.write_all(name.as_bytes()).unwrap();
        }
        zip.start_file("ffmpeg-n9.0-latest-win64-gpl-9.0/doc/ffmpeg.exe", opts).unwrap();
        zip.write_all(b"not this one").unwrap();
        zip.finish().unwrap();

        let out = base.join("all");
        unzip_programs(&archive, &out).unwrap();
        let bin = out.join("ffmpeg-n9.0-latest-win64-gpl-9.0/bin");
        assert_eq!(std::fs::read(bin.join("ffmpeg.exe")).unwrap(), b"ffmpeg.exe");
        assert_eq!(std::fs::read(bin.join("ffprobe.exe")).unwrap(), b"ffprobe.exe");
        assert!(!bin.join("ffplay.exe").exists());
        assert!(!out.join("ffmpeg-n9.0-latest-win64-gpl-9.0/doc").exists());
        assert_eq!(find_file(&out, "ffmpeg.exe"), Some(bin.join("ffmpeg.exe")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn icon_decodes() {
        let icon = icon().unwrap();
        assert_eq!((icon.width, icon.height), (256, 256));
    }
}
