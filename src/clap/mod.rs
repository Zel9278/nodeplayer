//! CLAP hosting: module loading, scanning, instances with their host-side extensions, and
//! the audio-thread processor. Editors are in [`editor`].
//!
//! Like VST3 plugins, Windows CLAP plugins come through yabridge as ordinary Linux ones on
//! Linux; on Windows and macOS the platform's own plugins load directly.

mod editor;

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::posix_fd_support::*;
use clap_sys::ext::state::*;
use clap_sys::ext::thread_check::*;
use clap_sys::ext::timer_support::*;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::*;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use crate::plugins::dylib::{self, Library};
use crate::plugins::{self, Format, ParamDesc, Placement, PluginInfo};

const QUEUE_LEN: usize = 1024;

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

fn carray(a: &[c_char]) -> String {
    let bytes: Vec<u8> = a.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Modules

pub struct Module {
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    // Plugins frequently misbehave when unloaded, so modules are kept for the process lifetime.
    _lib: Library,
}

unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn load(path: &Path) -> Result<Self> {
        let bin = bundle_binary(path)?;
        let lib = dylib::open(&bin)?;
        // init gets the bundle on macOS, the binary elsewhere.
        let init_path = if cfg!(target_os = "macos") { path } else { bin.as_path() };
        unsafe {
            let entry: *const clap_plugin_entry =
                *lib.get::<*const clap_plugin_entry>(b"clap_entry").context("clap_entry not exported")?;
            if entry.is_null() {
                bail!("null clap_entry");
            }
            let e = &*entry;
            if !clap_sys::version::clap_version_is_compatible(e.clap_version) {
                bail!("incompatible CLAP version {}.{}", e.clap_version.major, e.clap_version.minor);
            }
            let cpath = CString::new(init_path.as_os_str().as_encoded_bytes())?;
            if !e.init.is_some_and(|f| f(cpath.as_ptr())) {
                bail!("clap_entry.init failed");
            }
            let factory = e.get_factory.map_or(std::ptr::null(), |f| f(CLAP_PLUGIN_FACTORY_ID.as_ptr()))
                as *const clap_plugin_factory;
            if factory.is_null() {
                bail!("no plugin factory");
            }
            Ok(Self { entry, factory, _lib: lib })
        }
    }

    pub fn classes(&self, path: &Path) -> Vec<PluginInfo> {
        let mut out = Vec::new();
        unsafe {
            let f = &*self.factory;
            let count = f.get_plugin_count.map_or(0, |c| c(self.factory));
            for i in 0..count {
                let Some(d) = f.get_plugin_descriptor.map(|g| g(self.factory, i)).filter(|d| !d.is_null()) else {
                    continue;
                };
                let d = &*d;
                let mut features = Vec::new();
                let mut p = d.features;
                while !p.is_null() && !(*p).is_null() {
                    features.push(cstr(*p));
                    p = p.add(1);
                }
                out.push(PluginInfo {
                    format: Format::Clap,
                    name: cstr(d.name),
                    vendor: cstr(d.vendor),
                    category: features.join("|"),
                    id: cstr(d.id),
                    path: path.to_path_buf(),
                });
            }
        }
        out
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // Only reached when a module was loaded for a scan and nothing was created from it.
        unsafe {
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
    }
}

/// Keeps each module loaded once per process.
#[derive(Default)]
pub struct ModuleCache {
    modules: HashMap<PathBuf, Arc<Module>>,
}

impl ModuleCache {
    pub fn get(&mut self, path: &Path) -> Result<Arc<Module>> {
        if let Some(m) = self.modules.get(path) {
            return Ok(m.clone());
        }
        let m = Arc::new(Module::load(path)?);
        self.modules.insert(path.to_path_buf(), m.clone());
        Ok(m)
    }
}

/// `$CLAP_PATH` (separated like PATH) first, then the platform's standard locations.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        std::env::var_os("CLAP_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    v.extend(plugins::standard_dirs("CLAP", ".clap", &["/usr/lib/clap", "/usr/lib64/clap", "/usr/local/lib/clap"]));
    v
}

/// The binary of the CLAP at `path`: `path` itself when it's a file, otherwise the one inside
/// the bundle folder. That's `Contents/MacOS/` on macOS; elsewhere vendors use the VST3 layout
/// (`X.clap/Contents/x86_64-win/X.clap`, `…/x86_64-linux/X.so`, …), or just put the file
/// somewhere inside.
pub fn bundle_binary(path: &Path) -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return dylib::mac_bundle_binary(path);
    }
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let os = if cfg!(windows) { "win" } else { "linux" };
    let arch_dir = path.join("Contents").join(format!("{}-{os}", std::env::consts::ARCH));
    find_binary(&arch_dir, 0)
        .or_else(|| find_binary(path, 0))
        .ok_or_else(|| anyhow!("no .clap or .{} in {}", dylib::EXT, path.display()))
}

/// The first `.clap` file below `dir`, else the first shared library, looking a few levels deep.
fn find_binary(dir: &Path, depth: u32) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    let has = |p: &PathBuf, ext: &str| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext));
    entries
        .iter()
        .find(|p| has(p, "clap"))
        .or_else(|| entries.iter().find(|p| has(p, dylib::EXT)))
        .cloned()
        .or_else(|| if depth < 3 { entries.iter().filter(|p| p.is_dir()).find_map(|p| find_binary(p, depth + 1)) } else { None })
}

/// Finds `.clap` files and bundle folders below `root` without loading them.
pub fn find_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("clap")) {
            out.push(p);
        } else if p.is_dir() {
            find_files(&p, out);
        }
    }
}

// ---------------------------------------------------------------------------
// The host side handed to a plugin

struct HostTimer {
    id: clap_id,
    period: Duration,
    next: Instant,
}

/// Everything a plugin's host callbacks touch. Boxed so `host.host_data` can point at it.
struct HostShared {
    host: clap_host,
    name: String,
    main_thread: ThreadId,
    callback_requested: AtomicBool,
    restart_requested: AtomicBool,
    rescan_params: AtomicBool,
    latency_changed: AtomicBool,
    /// Size the GUI asked for, as `w << 32 | h`; 0 when none is pending.
    resize_request: AtomicU64,
    gui_closed: AtomicBool,
    timers: Mutex<Vec<HostTimer>>,
    next_timer: AtomicU32,
    fds: Mutex<Vec<(i32, clap_posix_fd_flags)>>,
}

thread_local! {
    static AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

const HOST_NAME: &CStr = c"nodeplayer";
const HOST_VENDOR: &CStr = c"nodeplayer";
const HOST_URL: &CStr = c"https://localhost/nodeplayer";
const HOST_VERSION: &CStr = c"0.1.0";

unsafe fn shared<'a>(host: *const clap_host) -> &'a HostShared {
    unsafe { &*((*host).host_data as *const HostShared) }
}

unsafe extern "C" fn get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_LOG {
        &HOST_LOG as *const _ as *const c_void
    } else if id == CLAP_EXT_THREAD_CHECK {
        &HOST_THREAD_CHECK as *const _ as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_GUI {
        &HOST_GUI as *const _ as *const c_void
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        &HOST_TIMER as *const _ as *const c_void
    } else if cfg!(unix) && id == CLAP_EXT_POSIX_FD_SUPPORT {
        &HOST_FD as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &HOST_LATENCY as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &HOST_STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &HOST_AUDIO_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_NOTE_PORTS {
        &HOST_NOTE_PORTS as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    unsafe { shared(host) }.restart_requested.store(true, Ordering::Release);
}

unsafe extern "C" fn request_process(_host: *const clap_host) {
    // The engine processes every node all the time.
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    unsafe { shared(host) }.callback_requested.store(true, Ordering::Release);
}

static HOST_LOG: clap_host_log = clap_host_log { log: Some(host_log) };

unsafe extern "C" fn host_log(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    let name = &unsafe { shared(host) }.name;
    let msg = cstr(msg);
    match severity {
        CLAP_LOG_DEBUG => log::debug!("{name}: {msg}"),
        CLAP_LOG_INFO => log::info!("{name}: {msg}"),
        CLAP_LOG_WARNING => log::warn!("{name}: {msg}"),
        _ => log::error!("{name}: {msg}"),
    }
}

static HOST_THREAD_CHECK: clap_host_thread_check =
    clap_host_thread_check { is_main_thread: Some(is_main_thread), is_audio_thread: Some(is_audio_thread) };

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    unsafe { shared(host) }.main_thread == std::thread::current().id()
}

unsafe extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    AUDIO_THREAD.with(|a| a.get())
}

static HOST_PARAMS: clap_host_params =
    clap_host_params { rescan: Some(params_rescan), clear: Some(params_clear), request_flush: Some(params_flush) };

unsafe extern "C" fn params_rescan(host: *const clap_host, _flags: clap_param_rescan_flags) {
    unsafe { shared(host) }.rescan_params.store(true, Ordering::Release);
}

unsafe extern "C" fn params_clear(_host: *const clap_host, _id: clap_id, _flags: clap_param_clear_flags) {}

unsafe extern "C" fn params_flush(_host: *const clap_host) {
    // Parameters are flushed by the next process call, which always comes.
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};

unsafe extern "C" fn gui_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn gui_request_resize(host: *const clap_host, w: u32, h: u32) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    unsafe { shared(host) }.resize_request.store((w as u64) << 32 | h as u64, Ordering::Release);
    true
}

unsafe extern "C" fn gui_request_show(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_request_hide(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_closed(host: *const clap_host, _was_destroyed: bool) {
    unsafe { shared(host) }.gui_closed.store(true, Ordering::Release);
}

static HOST_TIMER: clap_host_timer_support =
    clap_host_timer_support { register_timer: Some(register_timer), unregister_timer: Some(unregister_timer) };

unsafe extern "C" fn register_timer(host: *const clap_host, period_ms: u32, id: *mut clap_id) -> bool {
    if id.is_null() {
        return false;
    }
    let s = unsafe { shared(host) };
    let tid = s.next_timer.fetch_add(1, Ordering::Relaxed);
    let period = Duration::from_millis(period_ms.max(1) as u64);
    s.timers.lock().unwrap().push(HostTimer { id: tid, period, next: Instant::now() + period });
    unsafe { *id = tid };
    true
}

unsafe extern "C" fn unregister_timer(host: *const clap_host, id: clap_id) -> bool {
    let s = unsafe { shared(host) };
    let mut t = s.timers.lock().unwrap();
    let before = t.len();
    t.retain(|x| x.id != id);
    t.len() != before
}

static HOST_FD: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(register_fd),
    modify_fd: Some(modify_fd),
    unregister_fd: Some(unregister_fd),
};

unsafe extern "C" fn register_fd(host: *const clap_host, fd: i32, flags: clap_posix_fd_flags) -> bool {
    let s = unsafe { shared(host) };
    let mut fds = s.fds.lock().unwrap();
    fds.retain(|(f, _)| *f != fd);
    fds.push((fd, flags));
    true
}

unsafe extern "C" fn modify_fd(host: *const clap_host, fd: i32, flags: clap_posix_fd_flags) -> bool {
    let s = unsafe { shared(host) };
    let mut fds = s.fds.lock().unwrap();
    match fds.iter_mut().find(|(f, _)| *f == fd) {
        Some(e) => {
            e.1 = flags;
            true
        }
        None => false,
    }
}

unsafe extern "C" fn unregister_fd(host: *const clap_host, fd: i32) -> bool {
    let s = unsafe { shared(host) };
    let mut fds = s.fds.lock().unwrap();
    let before = fds.len();
    fds.retain(|(f, _)| *f != fd);
    fds.len() != before
}

static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(latency_changed) };

unsafe extern "C" fn latency_changed(host: *const clap_host) {
    unsafe { shared(host) }.latency_changed.store(true, Ordering::Release);
}

static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(state_dirty) };

unsafe extern "C" fn state_dirty(_host: *const clap_host) {}

static HOST_AUDIO_PORTS: clap_host_audio_ports =
    clap_host_audio_ports { is_rescan_flag_supported: Some(ports_flag_supported), rescan: Some(ports_rescan) };

unsafe extern "C" fn ports_flag_supported(_host: *const clap_host, _flag: u32) -> bool {
    false
}

unsafe extern "C" fn ports_rescan(_host: *const clap_host, _flags: u32) {
    // Only allowed while the plugin is deactivated, i.e. during a restart, which reads the
    // ports again anyway.
}

static HOST_NOTE_PORTS: clap_host_note_ports =
    clap_host_note_ports { supported_dialects: Some(note_dialects), rescan: Some(ports_rescan) };

unsafe extern "C" fn note_dialects(_host: *const clap_host) -> clap_note_dialect {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

// ---------------------------------------------------------------------------
// Event lists

/// A parameter change between the UI and the audio thread (plain, not normalized, value).
#[derive(Clone, Copy)]
struct ParamChange {
    id: clap_id,
    value: f64,
}

fn param_event(id: clap_id, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

/// Any input event we send; they all start with the header.
#[repr(C)]
#[derive(Clone, Copy)]
union AnyEvent {
    header: clap_event_header,
    param: clap_event_param_value,
    note: clap_event_note,
    midi: clap_event_midi,
}

fn header<T>(type_: u16, time: u32) -> clap_event_header {
    clap_event_header { size: std::mem::size_of::<T>() as u32, time, space_id: CLAP_CORE_EVENT_SPACE_ID, type_, flags: 0 }
}

/// How a plugin wants its notes, if it takes any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dialect {
    Midi,
    Clap,
}

struct InEvents {
    events: Vec<AnyEvent>,
    list: clap_input_events,
}

impl InEvents {
    fn new() -> Box<Self> {
        let mut b = Box::new(Self {
            events: Vec::with_capacity(QUEUE_LEN),
            list: clap_input_events { ctx: std::ptr::null_mut(), size: Some(in_size), get: Some(in_get) },
        });
        b.list.ctx = &mut *b as *mut Self as *mut c_void;
        b
    }
}

unsafe extern "C" fn in_size(list: *const clap_input_events) -> u32 {
    unsafe { (*((*list).ctx as *const InEvents)).events.len() as u32 }
}

unsafe extern "C" fn in_get(list: *const clap_input_events, i: u32) -> *const clap_event_header {
    let ev = unsafe { &(*((*list).ctx as *const InEvents)).events };
    ev.get(i as usize).map_or(std::ptr::null(), |e| unsafe { &e.header })
}

struct OutEvents {
    to_ui: rtrb::Producer<ParamChange>,
    /// Where note output goes during `process`, and the block length.
    midi: *mut MidiBuf,
    frames: u32,
    list: clap_output_events,
}

impl OutEvents {
    fn new(to_ui: rtrb::Producer<ParamChange>) -> Box<Self> {
        let mut b = Box::new(Self {
            to_ui,
            midi: std::ptr::null_mut(),
            frames: 0,
            list: clap_output_events { ctx: std::ptr::null_mut(), try_push: Some(out_push) },
        });
        b.list.ctx = &mut *b as *mut Self as *mut c_void;
        b
    }
}

unsafe extern "C" fn out_push(list: *const clap_output_events, ev: *const clap_event_header) -> bool {
    unsafe {
        let me = &mut *((*list).ctx as *mut OutEvents);
        let h = &*ev;
        if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
            return true;
        }
        let time = h.time.min(me.frames.saturating_sub(1));
        let data = match h.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let p = &*(ev as *const clap_event_param_value);
                let _ = me.to_ui.push(ParamChange { id: p.param_id, value: p.value });
                return true;
            }
            CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF => {
                let n = &*(ev as *const clap_event_note);
                if !(0..128).contains(&n.key) {
                    return true;
                }
                let ch = n.channel.clamp(0, 15) as u8;
                let vel = (n.velocity * 127.0).round().clamp(0.0, 127.0) as u8;
                if h.type_ == CLAP_EVENT_NOTE_ON { [0x90 | ch, n.key as u8, vel.max(1)] } else { [0x80 | ch, n.key as u8, vel] }
            }
            CLAP_EVENT_MIDI => (*(ev as *const clap_event_midi)).data,
            _ => return true,
        };
        if let (Some(out), Some(e)) = (me.midi.as_mut(), MidiEvent::new(time, &data)) {
            out.push(e);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Instances

struct Ext {
    params: *const clap_plugin_params,
    state: *const clap_plugin_state,
    gui: *const clap_plugin_gui,
    latency: *const clap_plugin_latency,
    timer: *const clap_plugin_timer_support,
    #[cfg_attr(not(unix), allow(dead_code))]
    fd: *const clap_plugin_posix_fd_support,
    ports: *const clap_plugin_audio_ports,
    notes: *const clap_plugin_note_ports,
}

unsafe fn ext<T>(plugin: *const clap_plugin, id: &CStr) -> *const T {
    unsafe { (*plugin).get_extension.map_or(std::ptr::null(), |g| g(plugin, id.as_ptr())) as *const T }
}

pub struct ClapPlugin {
    info: PluginInfo,
    params: Vec<ParamDesc>,
    /// Plain-value range of each parameter.
    ranges: HashMap<clap_id, (f64, f64)>,
    plugin: *const clap_plugin,
    ext: Ext,
    shared: Box<HostShared>,
    to_audio: rtrb::Producer<ParamChange>,
    from_audio: rtrb::Consumer<ParamChange>,
    /// Values set from the host UI, shown until the plugin has seen them.
    recent: HashMap<clap_id, (f64, Instant)>,
    latency: u32,
    /// Note dialects of the first note input and output port.
    notes: (Option<Dialect>, Option<Dialect>),
    active: bool,
    _module: Arc<Module>,
}

impl ClapPlugin {
    pub fn create(module: Arc<Module>, info: &PluginInfo, sample_rate: f64, max_block: usize) -> Result<(Self, ClapProcessor)> {
        unsafe {
            let mut shared = Box::new(HostShared {
                host: clap_host {
                    clap_version: CLAP_VERSION,
                    host_data: std::ptr::null_mut(),
                    name: HOST_NAME.as_ptr(),
                    vendor: HOST_VENDOR.as_ptr(),
                    url: HOST_URL.as_ptr(),
                    version: HOST_VERSION.as_ptr(),
                    get_extension: Some(get_extension),
                    request_restart: Some(request_restart),
                    request_process: Some(request_process),
                    request_callback: Some(request_callback),
                },
                name: info.name.clone(),
                main_thread: std::thread::current().id(),
                callback_requested: AtomicBool::new(false),
                restart_requested: AtomicBool::new(false),
                rescan_params: AtomicBool::new(false),
                latency_changed: AtomicBool::new(false),
                resize_request: AtomicU64::new(0),
                gui_closed: AtomicBool::new(false),
                timers: Mutex::new(Vec::new()),
                next_timer: AtomicU32::new(1),
                fds: Mutex::new(Vec::new()),
            });
            shared.host.host_data = &mut *shared as *mut HostShared as *mut c_void;

            let f = &*module.factory;
            let id = CString::new(info.id.as_str())?;
            let plugin = f
                .create_plugin
                .map_or(std::ptr::null(), |c| c(module.factory, &shared.host, id.as_ptr()));
            if plugin.is_null() {
                bail!("create_plugin failed");
            }
            if !(*plugin).init.is_some_and(|i| i(plugin)) {
                if let Some(d) = (*plugin).destroy {
                    d(plugin);
                }
                bail!("plugin init failed");
            }
            let ext = Ext {
                params: ext(plugin, CLAP_EXT_PARAMS),
                state: ext(plugin, CLAP_EXT_STATE),
                gui: ext(plugin, CLAP_EXT_GUI),
                latency: ext(plugin, CLAP_EXT_LATENCY),
                timer: ext(plugin, CLAP_EXT_TIMER_SUPPORT),
                fd: ext(plugin, CLAP_EXT_POSIX_FD_SUPPORT),
                ports: ext(plugin, CLAP_EXT_AUDIO_PORTS),
                notes: ext(plugin, CLAP_EXT_NOTE_PORTS),
            };
            let (to_audio, _) = rtrb::RingBuffer::new(1);
            let (_, from_audio) = rtrb::RingBuffer::new(1);
            let mut me = Self {
                info: info.clone(),
                params: Vec::new(),
                ranges: HashMap::new(),
                plugin,
                ext,
                shared,
                to_audio,
                from_audio,
                recent: HashMap::new(),
                latency: 0,
                notes: (None, None),
                active: false,
                _module: module,
            };
            me.reload_params();
            // Dropping `me` on failure destroys the plugin.
            let proc = me.activate(sample_rate, max_block)?;
            Ok((me, proc))
        }
    }

    /// Reads the ports, activates the plugin and returns a processor for it.
    fn activate(&mut self, sample_rate: f64, max_block: usize) -> Result<ClapProcessor> {
        let plugin = self.plugin;
        unsafe {
            let channels = |input: bool| -> Vec<usize> {
                let Some(p) = self.ext.ports.as_ref() else { return Vec::new() };
                let n = p.count.map_or(0, |c| c(plugin, input));
                (0..n)
                    .map(|i| {
                        let mut info: clap_audio_port_info = std::mem::zeroed();
                        if p.get.is_some_and(|g| g(plugin, i, input, &mut info)) { info.channel_count as usize } else { 0 }
                    })
                    .collect()
            };
            let (in_ch, out_ch) = (channels(true), channels(false));
            let dialect = |input: bool| -> Option<Dialect> {
                let p = self.ext.notes.as_ref()?;
                if p.count.map_or(0, |c| c(plugin, input)) == 0 {
                    return None;
                }
                let mut info: clap_note_port_info = std::mem::zeroed();
                if !p.get.is_some_and(|g| g(plugin, 0, input, &mut info)) {
                    return None;
                }
                let d = info.supported_dialects;
                if d & CLAP_NOTE_DIALECT_MIDI != 0 && (info.preferred_dialect == CLAP_NOTE_DIALECT_MIDI || d & CLAP_NOTE_DIALECT_CLAP == 0) {
                    Some(Dialect::Midi)
                } else if d & CLAP_NOTE_DIALECT_CLAP != 0 {
                    Some(Dialect::Clap)
                } else {
                    None
                }
            };
            self.notes = (dialect(true), dialect(false));

            if !(*plugin).activate.is_some_and(|a| a(plugin, sample_rate, 1, max_block as u32)) {
                bail!("activate failed");
            }
            self.active = true;
            self.latency = self.ext.latency.as_ref().and_then(|l| l.get).map_or(0, |g| g(plugin));
            let (to_audio, from_ui) = rtrb::RingBuffer::new(QUEUE_LEN);
            let (to_ui, from_audio) = rtrb::RingBuffer::new(QUEUE_LEN);
            self.to_audio = to_audio;
            self.from_audio = from_audio;
            Ok(ClapProcessor::new(plugin, from_ui, to_ui, &in_ch, &out_ch, self.notes.0, max_block))
        }
    }

    fn reload_params(&mut self) {
        self.params.clear();
        self.ranges.clear();
        let Some(p) = (unsafe { self.ext.params.as_ref() }) else { return };
        unsafe {
            let n = p.count.map_or(0, |c| c(self.plugin));
            for i in 0..n {
                let mut info: clap_param_info = std::mem::zeroed();
                if !p.get_info.is_some_and(|g| g(self.plugin, i, &mut info)) {
                    continue;
                }
                let (min, max) = (info.min_value, info.max_value);
                let span = max - min;
                let stepped = info.flags & CLAP_PARAM_IS_STEPPED != 0;
                self.ranges.insert(info.id, (min, max));
                let module = carray(&info.module);
                let name = carray(&info.name);
                self.params.push(ParamDesc {
                    id: info.id,
                    title: if module.is_empty() { name } else { format!("{module}/{name}") },
                    units: String::new(),
                    step_count: if stepped { span.round().max(1.0) as i32 } else { 0 },
                    default: if span > 0.0 { (info.default_value - min) / span } else { 0.0 },
                    hidden: info.flags & CLAP_PARAM_IS_HIDDEN != 0,
                    read_only: info.flags & CLAP_PARAM_IS_READONLY != 0,
                });
            }
        }
    }

    fn plain(&self, id: u32, norm: f64) -> f64 {
        let (min, max) = self.ranges.get(&id).copied().unwrap_or((0.0, 1.0));
        min + norm.clamp(0.0, 1.0) * (max - min)
    }

    fn norm(&self, id: u32, plain: f64) -> f64 {
        let (min, max) = self.ranges.get(&id).copied().unwrap_or((0.0, 1.0));
        if max > min { ((plain - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 }
    }

    /// Timers and file descriptors the plugin registered, and main-thread callbacks.
    fn pump(&mut self) {
        let s = &self.shared;
        if let Some(on_timer) = unsafe { self.ext.timer.as_ref() }.and_then(|t| t.on_timer) {
            let now = Instant::now();
            let due: Vec<clap_id> = s
                .timers
                .lock()
                .unwrap()
                .iter_mut()
                .filter(|t| t.next <= now)
                .map(|t| {
                    t.next = now + t.period;
                    t.id
                })
                .collect();
            for id in due {
                unsafe { on_timer(self.plugin, id) };
            }
        }
        #[cfg(unix)]
        if let Some(on_fd) = unsafe { self.ext.fd.as_ref() }.and_then(|f| f.on_fd) {
            let fds = s.fds.lock().unwrap().clone();
            if !fds.is_empty() {
                let mut pfds: Vec<libc::pollfd> = fds
                    .iter()
                    .map(|&(fd, fl)| {
                        let mut events = 0;
                        if fl & CLAP_POSIX_FD_READ != 0 {
                            events |= libc::POLLIN;
                        }
                        if fl & CLAP_POSIX_FD_WRITE != 0 {
                            events |= libc::POLLOUT;
                        }
                        libc::pollfd { fd, events, revents: 0 }
                    })
                    .collect();
                if unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 0) } > 0 {
                    for p in pfds.iter().filter(|p| p.revents != 0) {
                        let mut fl = 0;
                        if p.revents & libc::POLLIN != 0 {
                            fl |= CLAP_POSIX_FD_READ;
                        }
                        if p.revents & libc::POLLOUT != 0 {
                            fl |= CLAP_POSIX_FD_WRITE;
                        }
                        if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                            fl |= CLAP_POSIX_FD_ERROR;
                        }
                        unsafe { on_fd(self.plugin, p.fd, fl) };
                    }
                }
            }
        }
        if s.callback_requested.swap(false, Ordering::AcqRel) {
            unsafe {
                if let Some(cb) = (*self.plugin).on_main_thread {
                    cb(self.plugin);
                }
            }
        }
    }
}

unsafe extern "C" fn ostream_write(stream: *const clap_ostream, buf: *const c_void, size: u64) -> i64 {
    unsafe {
        let out = &mut *((*stream).ctx as *mut Vec<u8>);
        out.extend_from_slice(std::slice::from_raw_parts(buf as *const u8, size as usize));
    }
    size as i64
}

unsafe extern "C" fn istream_read(stream: *const clap_istream, buf: *mut c_void, size: u64) -> i64 {
    unsafe {
        let (data, pos) = &mut *((*stream).ctx as *mut (&[u8], usize));
        let n = (size as usize).min(data.len() - *pos);
        std::ptr::copy_nonoverlapping(data[*pos..].as_ptr(), buf as *mut u8, n);
        *pos += n;
        n as i64
    }
}

impl plugins::Instance for ClapPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn params(&self) -> &[ParamDesc] {
        &self.params
    }

    fn param_value(&self, id: u32) -> f64 {
        if let Some((v, t)) = self.recent.get(&id)
            && t.elapsed() < Duration::from_millis(300)
        {
            return self.norm(id, *v);
        }
        let mut v = 0.0;
        let ok = unsafe { self.ext.params.as_ref() }
            .and_then(|p| p.get_value)
            .is_some_and(|g| unsafe { g(self.plugin, id, &mut v) });
        if ok { self.norm(id, v) } else { 0.0 }
    }

    fn param_display(&self, id: u32, value: f64) -> String {
        let plain = self.plain(id, value);
        let mut buf = [0 as c_char; 256];
        let ok = unsafe { self.ext.params.as_ref() }
            .and_then(|p| p.value_to_text)
            .is_some_and(|f| unsafe { f(self.plugin, id, plain, buf.as_mut_ptr(), buf.len() as u32) });
        if ok { carray(&buf) } else { format!("{plain:.3}") }
    }

    fn set_param(&mut self, id: u32, value: f64) {
        let plain = self.plain(id, value);
        self.recent.insert(id, (plain, Instant::now()));
        let _ = self.to_audio.push(ParamChange { id, value: plain });
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn idle(&mut self) {
        // Values the plugin reported from the audio thread are readable through get_value;
        // only drop our overrides once it has caught up.
        while let Ok(ch) = self.from_audio.pop() {
            self.recent.remove(&ch.id);
        }
        self.recent.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(1));
        self.pump();
        if self.shared.rescan_params.swap(false, Ordering::AcqRel) {
            self.reload_params();
        }
        let s = &self.shared;
        if s.latency_changed.swap(false, Ordering::AcqRel) {
            self.latency = unsafe { self.ext.latency.as_ref() }.and_then(|l| l.get).map_or(0, |g| unsafe { g(self.plugin) });
        }
    }

    fn midi_ports(&self) -> (bool, bool) {
        (self.notes.0.is_some(), self.notes.1.is_some())
    }

    fn wants_restart(&mut self) -> bool {
        self.shared.restart_requested.swap(false, Ordering::AcqRel)
    }

    fn restart(&mut self, sample_rate: f64, max_block: usize) -> Result<Box<dyn Processor>> {
        // The processor has stopped processing on the audio thread already.
        unsafe {
            if let Some(d) = (*self.plugin).deactivate.filter(|_| std::mem::take(&mut self.active)) {
                d(self.plugin);
            }
        }
        let proc = self.activate(sample_rate, max_block)?;
        self.reload_params();
        log::info!("{}: restarted (latency {})", self.info.name, self.latency);
        Ok(Box::new(proc))
    }

    fn save_state(&self) -> Option<Vec<u8>> {
        let st = unsafe { self.ext.state.as_ref() }?;
        let mut out: Vec<u8> = Vec::new();
        let stream = clap_ostream { ctx: &mut out as *mut Vec<u8> as *mut c_void, write: Some(ostream_write) };
        st.save.is_some_and(|s| unsafe { s(self.plugin, &stream) }).then_some(out)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let st = unsafe { self.ext.state.as_ref() }.ok_or_else(|| anyhow!("plugin has no state"))?;
        let mut ctx: (&[u8], usize) = (data, 0);
        let stream = clap_istream { ctx: &mut ctx as *mut (&[u8], usize) as *mut c_void, read: Some(istream_read) };
        if !st.load.is_some_and(|l| unsafe { l(self.plugin, &stream) }) {
            bail!("plugin rejected its state");
        }
        self.recent.clear();
        self.reload_params();
        Ok(())
    }

    fn has_editor(&self) -> bool {
        unsafe { editor::is_supported(self.plugin, self.ext.gui) }
    }

    fn open_editor(&mut self, placement: Placement) -> Result<Box<dyn plugins::Editor>> {
        let gui = self.ext.gui;
        if gui.is_null() {
            bail!("{} has no GUI", self.info.name);
        }
        self.shared.gui_closed.store(false, Ordering::Release);
        self.shared.resize_request.store(0, Ordering::Release);
        let shared: *const HostShared = &*self.shared;
        let ed = unsafe { editor::ClapEditor::open(self.plugin, gui, shared, &self.info, placement)? };
        Ok(Box::new(ed))
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        // The processor is gone from the audio thread (and stopped) by now.
        unsafe {
            if let Some(d) = (*self.plugin).deactivate.filter(|_| self.active) {
                d(self.plugin);
            }
            if let Some(d) = (*self.plugin).destroy {
                d(self.plugin);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Audio thread

struct Buses {
    data: Vec<Vec<Vec<f32>>>,
    /// Channel pointer tables referenced by `bufs`; never read directly.
    #[allow(dead_code)]
    ptrs: Vec<Vec<*mut f32>>,
    bufs: Vec<clap_audio_buffer>,
}

impl Buses {
    fn new(channels: &[usize], max_block: usize) -> Self {
        let mut data: Vec<Vec<Vec<f32>>> = channels.iter().map(|&n| vec![vec![0.0; max_block]; n]).collect();
        let mut ptrs: Vec<Vec<*mut f32>> =
            data.iter_mut().map(|bus| bus.iter_mut().map(|ch| ch.as_mut_ptr()).collect()).collect();
        let bufs = ptrs
            .iter_mut()
            .map(|p| clap_audio_buffer {
                data32: p.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: p.len() as u32,
                latency: 0,
                constant_mask: 0,
            })
            .collect();
        Self { data, ptrs, bufs }
    }
}

pub struct ClapProcessor {
    plugin: *const clap_plugin,
    started: bool,
    from_ui: rtrb::Consumer<ParamChange>,
    in_events: Box<InEvents>,
    out_events: Box<OutEvents>,
    ins: Buses,
    outs: Buses,
    steady_time: i64,
    max_block: usize,
    note_in: Option<Dialect>,
}

// Moved to the audio thread once and used only there.
unsafe impl Send for ClapProcessor {}

impl ClapProcessor {
    fn new(
        plugin: *const clap_plugin,
        from_ui: rtrb::Consumer<ParamChange>,
        to_ui: rtrb::Producer<ParamChange>,
        in_ch: &[usize],
        out_ch: &[usize],
        note_in: Option<Dialect>,
        max_block: usize,
    ) -> Self {
        Self {
            plugin,
            started: false,
            from_ui,
            in_events: InEvents::new(),
            out_events: OutEvents::new(to_ui),
            ins: Buses::new(in_ch, max_block),
            outs: Buses::new(out_ch, max_block),
            steady_time: 0,
            max_block,
            note_in,
        }
    }

    fn queue_midi(&mut self, midi: &[MidiEvent]) {
        let Some(dialect) = self.note_in else { return };
        for m in midi {
            if self.in_events.events.len() >= QUEUE_LEN {
                break;
            }
            let [s, key, vel] = m.data;
            let ev = match dialect {
                Dialect::Midi => AnyEvent {
                    midi: clap_event_midi { header: header::<clap_event_midi>(CLAP_EVENT_MIDI, m.time), port_index: 0, data: m.data },
                },
                Dialect::Clap => {
                    let on = s & 0xf0 == 0x90 && vel > 0;
                    let panic = crate::runtime::get().experimental_pdc && s & 0xf0 == 0xb0 && matches!(key, 120 | 123);
                    if !on && !panic && !matches!(s & 0xf0, 0x80 | 0x90) {
                        continue;
                    }
                    let type_ = if panic {
                        CLAP_EVENT_NOTE_CHOKE
                    } else if on {
                        CLAP_EVENT_NOTE_ON
                    } else {
                        CLAP_EVENT_NOTE_OFF
                    };
                    AnyEvent {
                        note: clap_event_note {
                            header: header::<clap_event_note>(type_, m.time),
                            note_id: -1,
                            port_index: 0,
                            channel: m.channel() as i16,
                            key: if panic { -1 } else { key as i16 },
                            velocity: vel as f64 / 127.0,
                        },
                    }
                }
            };
            self.in_events.events.push(ev);
        }
    }

    fn run(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf], midi_in: &[MidiEvent], midi_out: *mut MidiBuf) {
        AUDIO_THREAD.with(|a| a.set(true));
        let n = n.min(self.max_block);
        let (i, o) = (&inputs[0], &mut outputs[0]);
        if !self.started {
            self.started = unsafe { (*self.plugin).start_processing.is_none_or(|s| s(self.plugin)) };
            if !self.started {
                o.clear(n);
                return;
            }
        }

        if let Some(bus) = self.ins.data.first_mut() {
            match bus.len() {
                0 => {}
                1 => {
                    for (k, s) in bus[0][..n].iter_mut().enumerate() {
                        *s = 0.5 * (i.l[k] + i.r[k]);
                    }
                }
                _ => {
                    bus[0][..n].copy_from_slice(&i.l[..n]);
                    bus[1][..n].copy_from_slice(&i.r[..n]);
                    for ch in &mut bus[2..] {
                        ch[..n].fill(0.0);
                    }
                }
            }
        }

        self.in_events.events.clear();
        while let Ok(ch) = self.from_ui.pop() {
            if self.in_events.events.len() < QUEUE_LEN {
                self.in_events.events.push(AnyEvent { param: param_event(ch.id, ch.value) });
            }
        }
        self.queue_midi(midi_in);
        self.out_events.midi = midi_out;
        self.out_events.frames = n as u32;

        let process = clap_process {
            steady_time: self.steady_time,
            frames_count: n as u32,
            transport: std::ptr::null(),
            audio_inputs: self.ins.bufs.as_ptr(),
            audio_outputs: self.outs.bufs.as_mut_ptr(),
            audio_inputs_count: self.ins.bufs.len() as u32,
            audio_outputs_count: self.outs.bufs.len() as u32,
            in_events: &self.in_events.list,
            out_events: &self.out_events.list,
        };
        let status = unsafe { (*self.plugin).process.map_or(CLAP_PROCESS_ERROR, |p| p(self.plugin, &process)) };
        self.out_events.midi = std::ptr::null_mut();
        self.steady_time += n as i64;

        match (status != CLAP_PROCESS_ERROR, self.outs.data.first()) {
            (true, Some(bus)) if !bus.is_empty() => {
                let r = if bus.len() > 1 { &bus[1] } else { &bus[0] };
                o.l[..n].copy_from_slice(&bus[0][..n]);
                o.r[..n].copy_from_slice(&r[..n]);
            }
            _ => o.clear(n),
        }
    }
}

impl Processor for ClapProcessor {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.run(n, inputs, outputs, &[], std::ptr::null_mut());
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.run(n, inputs, outputs, midi_in, midi_out);
    }

    fn deactivate(&mut self) {
        if std::mem::take(&mut self.started) {
            unsafe {
                if let Some(s) = (*self.plugin).stop_processing {
                    s(self.plugin);
                }
            }
        }
    }
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        // Normally stopped by `deactivate` on the audio thread; when the whole engine goes
        // away it's stopped here, after the audio thread has let go of it.
        if self.started {
            unsafe {
                if let Some(s) = (*self.plugin).stop_processing {
                    s(self.plugin);
                }
            }
        }
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn finds_the_binary_in_a_bundle_folder() {
        let root = std::env::temp_dir().join(format!("np-clap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let os = if cfg!(windows) { "win" } else { "linux" };
        let arch = format!("{}-{os}", std::env::consts::ARCH);

        // The VST3-like layout, with a binary for another platform beside it.
        let a = root.join("A.clap");
        std::fs::create_dir_all(a.join("Contents").join(&arch)).unwrap();
        std::fs::create_dir_all(a.join("Contents/other-arch")).unwrap();
        std::fs::write(a.join("Contents/other-arch/A.clap"), b"").unwrap();
        std::fs::write(a.join("Contents").join(&arch).join("A.clap"), b"").unwrap();
        std::fs::write(a.join("readme.txt"), b"").unwrap();
        assert_eq!(bundle_binary(&a).unwrap(), a.join("Contents").join(&arch).join("A.clap"));

        // A shared library somewhere inside.
        let b = root.join("B.clap");
        std::fs::create_dir_all(b.join("bin")).unwrap();
        let lib = b.join("bin").join(format!("B.{}", dylib::EXT));
        std::fs::write(&lib, b"").unwrap();
        assert_eq!(bundle_binary(&b).unwrap(), lib);

        // A plain file, and both bundles counted once each.
        let c = root.join("C.clap");
        std::fs::write(&c, b"").unwrap();
        assert_eq!(bundle_binary(&c).unwrap(), c);
        let mut found = Vec::new();
        find_files(&root, &mut found);
        found.sort();
        assert_eq!(found, vec![a, b, c]);

        assert!(bundle_binary(&root.join("missing.clap")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
