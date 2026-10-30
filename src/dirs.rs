//! Where nodeplayer keeps its files, per platform.
//!
//! Linux (and other Unix systems) follow the XDG base directory spec (`~/.config/nodeplayer`,
//! `~/.cache/nodeplayer`, …). macOS uses `~/Library/Application Support/nodeplayer` and
//! `~/Library/Caches/nodeplayer`. Windows uses the roaming and local app data folders:
//! settings and state go to `%APPDATA%\nodeplayer`, caches and downloads to
//! `%LOCALAPPDATA%\nodeplayer`, so a roaming profile doesn't carry a 150 MB FFmpeg around.

use std::path::{Path, PathBuf};

/// The user's home folder.
pub fn home() -> Option<PathBuf> {
    #[cfg(unix)]
    let var = "HOME";
    #[cfg(windows)]
    let var = "USERPROFILE";
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(fallback)))?;
    Some(base.join("nodeplayer"))
}

#[cfg(target_os = "macos")]
fn library(sub: &str) -> Option<PathBuf> {
    Some(home()?.join("Library").join(sub).join("nodeplayer"))
}

#[cfg(windows)]
fn known(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(fallback)))?;
    Some(base.join("nodeplayer"))
}

/// Settings: `~/.config/nodeplayer`, `~/Library/Application Support/nodeplayer`,
/// `%APPDATA%\nodeplayer`.
pub fn config() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return xdg("XDG_CONFIG_HOME", ".config");
    #[cfg(target_os = "macos")]
    return library("Application Support");
    #[cfg(windows)]
    return known("APPDATA", "AppData\\Roaming");
}

/// Throwaway files: `~/.cache/nodeplayer`, `~/Library/Caches/nodeplayer`,
/// `%LOCALAPPDATA%\nodeplayer\cache`.
pub fn cache() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return xdg("XDG_CACHE_HOME", ".cache");
    #[cfg(target_os = "macos")]
    return library("Caches");
    #[cfg(windows)]
    return Some(known("LOCALAPPDATA", "AppData\\Local")?.join("cache"));
}

/// State worth keeping but not configuration, like play counts: `~/.local/state/nodeplayer`,
/// `~/Library/Application Support/nodeplayer/state`, `%APPDATA%\nodeplayer\state`.
pub fn state() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return xdg("XDG_STATE_HOME", ".local/state");
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    return Some(config()?.join("state"));
}

/// Program data we install ourselves, like our FFmpeg copy and plugins:
/// `~/.local/share/nodeplayer`, `~/Library/Application Support/nodeplayer`,
/// `%LOCALAPPDATA%\nodeplayer`.
pub fn data() -> Option<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    return xdg("XDG_DATA_HOME", ".local/share");
    #[cfg(target_os = "macos")]
    return library("Application Support");
    #[cfg(windows)]
    return known("LOCALAPPDATA", "AppData\\Local");
}

/// `path` with the home folder written as `~` (`%USERPROFILE%` on Windows).
pub fn tilde(path: &Path) -> String {
    let short = if cfg!(windows) { "%USERPROFILE%" } else { "~" };
    match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("{short}{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// Makes a file readable only by its owner. A no-op on Windows, where files under the user's
/// profile aren't readable by other users anyway.
pub fn restrict(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

/// Writes `data` to a new or truncated file that only its owner can read, even for the moment
/// before the permissions could be changed.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    // The file may have existed with wider permissions.
    restrict(path)?;
    f.write_all(data)
}

/// Opens a web address or file in the desktop's default program.
pub fn open(target: &str) -> std::io::Result<()> {
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        // `start` treats its first quoted argument as the window title, hence the empty "".
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    let mut child = cmd.arg(target).stdin(std::process::Stdio::null()).spawn()?;
    // Reap it so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folders_sit_under_the_home_folder() {
        // The test environment always has one; the folders are optional only in theory.
        let home = home().expect("no home folder");
        for d in [config(), cache(), state(), data()] {
            let d = d.expect("no folder");
            assert!(d.starts_with(&home), "{} isn't under {}", d.display(), home.display());
            assert!(d.to_string_lossy().contains("nodeplayer"));
        }
    }

    #[test]
    fn tilde_shortens_only_the_home_folder() {
        let home = home().unwrap();
        assert!(tilde(&home.join("x")).ends_with("x"));
        assert!(!tilde(&home.join("x")).starts_with(home.to_str().unwrap()));
        assert_eq!(tilde(Path::new("/elsewhere/x")), "/elsewhere/x");
    }
}
