//! Crash attribution. Plugins run inside nodeplayer, so a crashing plugin takes the app down.
//! Each thread notes which plugin it is calling into; if the process dies from a fatal signal
//! meanwhile, a signal handler (an unhandled exception filter on Windows) writes that plugin
//! to a file. On the next start it is put on the disabled list ([`disabled`]) and not loaded
//! until the user enables it again.

use std::cell::Cell;
use std::ffi::{CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::{Format, PluginInfo};

/// "FORMAT\tpath" of a plugin library, NUL-terminated for the signal handler.
pub struct Tag(CString);

impl Tag {
    pub fn new(format: Format, path: &Path) -> Arc<Self> {
        let s = format!("{}\t{}", format.label(), path.display()).replace('\0', "");
        Arc::new(Self(CString::new(s).unwrap_or_default()))
    }

    pub fn of(info: &PluginInfo) -> Arc<Self> {
        Self::new(info.format, &info.path)
    }
}

thread_local! {
    // Constant-initialized and without a destructor, so reading it in a signal handler is
    // plain memory access.
    static CURRENT: Cell<*const c_char> = const { Cell::new(std::ptr::null()) };
}

/// Marks the current thread as running inside a plugin until dropped.
#[must_use]
pub struct Entered {
    prev: *const c_char,
    _tag: Option<Arc<Tag>>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

pub fn enter(tag: &Arc<Tag>) -> Entered {
    let prev = CURRENT.with(|c| c.replace(tag.0.as_ptr()));
    Entered { prev, _tag: Some(tag.clone()) }
}

/// [`enter`] for the audio thread: no reference counting. The caller keeps `tag` alive for
/// as long as the guard.
pub fn enter_borrowed(tag: &Tag) -> Entered {
    let prev = CURRENT.with(|c| c.replace(tag.0.as_ptr()));
    Entered { prev, _tag: None }
}

static CRASH_FILE: OnceLock<CString> = OnceLock::new();
#[cfg(unix)]
const SIGNALS: [i32; 5] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];
#[cfg(unix)]
static mut PREVIOUS: [Option<libc::sigaction>; 5] = [None; 5];

pub(crate) fn state_dir() -> Option<PathBuf> {
    crate::dirs::state()
}

fn crash_path() -> Option<PathBuf> {
    Some(state_dir()?.join("crashed-plugin"))
}

/// Installs the signal handlers. Call once at startup, before any plugin is loaded.
pub fn install() {
    let Some(path) = crash_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else { return };
    if CRASH_FILE.set(c).is_err() {
        return;
    }
    #[cfg(windows)]
    unsafe {
        windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(on_exception));
    }
    #[cfg(unix)]
    unsafe {
        for (i, &sig) in SIGNALS.iter().enumerate() {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_signal as *const () as usize;
            act.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut act.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, &act, &mut old) == 0 {
                (*std::ptr::addr_of_mut!(PREVIOUS))[i] = Some(old);
            }
        }
    }
}

/// Notes the plugin the crashing thread was in, then lets Windows end the process as usual.
#[cfg(windows)]
unsafe extern "system" fn on_exception(_info: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    let cur = CURRENT.with(|c| c.get());
    if !cur.is_null()
        && let Some(file) = CRASH_FILE.get()
    {
        // The process is going down anyway; writing through std is fine here.
        let path = std::path::Path::new(std::str::from_utf8(file.to_bytes()).unwrap_or_default());
        let _ = std::fs::write(path, unsafe { std::ffi::CStr::from_ptr(cur) }.to_bytes());
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

#[cfg(unix)]
extern "C" fn on_signal(sig: i32, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // Only async-signal-safe calls in here.
    let cur = CURRENT.with(|c| c.get());
    if !cur.is_null()
        && let Some(file) = CRASH_FILE.get()
    {
        unsafe {
            let fd = libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC, 0o644);
            if fd >= 0 {
                let len = libc::strlen(cur);
                let _ = libc::write(fd, cur as *const libc::c_void, len);
                libc::close(fd);
            }
        }
    }
    // Hand the signal back to whoever had it (Rust's stack overflow handler, or the default
    // action). For a fault, returning re-runs the faulting instruction, which then reaches
    // that handler; a raised signal is raised again.
    unsafe {
        let i = SIGNALS.iter().position(|&s| s == sig).unwrap_or(0);
        match (*std::ptr::addr_of!(PREVIOUS))[i] {
            Some(old) => libc::sigaction(sig, &old, std::ptr::null_mut()),
            None => libc::signal(sig, libc::SIG_DFL) as i32,
        };
        if matches!(sig, libc::SIGABRT | libc::SIGFPE) {
            libc::raise(sig);
        }
    }
}

/// The plugin that crashed the previous run, if any; reading clears it.
pub fn take_crash() -> Option<(Format, PathBuf)> {
    let path = crash_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    parse_line(&text)
}

fn parse_line(line: &str) -> Option<(Format, PathBuf)> {
    let (f, p) = line.trim_end_matches('\n').split_once('\t')?;
    Some((Format::from_label(f)?, PathBuf::from(p)))
}

fn disabled_path() -> Option<PathBuf> {
    Some(crate::session::config_dir()?.join("disabled_plugins.txt"))
}

/// Plugin libraries that crashed nodeplayer and stay off until enabled again.
pub fn disabled() -> Vec<(Format, PathBuf)> {
    let Some(p) = disabled_path() else { return Vec::new() };
    std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(parse_line).collect()
}

pub fn set_disabled(list: &[(Format, PathBuf)]) {
    let Some(p) = disabled_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = list.iter().map(|(f, p)| format!("{}\t{}\n", f.label(), p.display())).collect();
    if let Err(e) = std::fs::write(&p, text) {
        log::warn!("can't write {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_parse_back() {
        let t = Tag::new(Format::Clap, Path::new("/x/y z.clap"));
        assert_eq!(parse_line(t.0.to_str().unwrap()), Some((Format::Clap, PathBuf::from("/x/y z.clap"))));
        assert_eq!(parse_line("Bogus\t/a"), None);
        let _g = enter(&t);
        assert!(!CURRENT.with(|c| c.get()).is_null());
        drop(_g);
        assert!(CURRENT.with(|c| c.get()).is_null());
    }
}
