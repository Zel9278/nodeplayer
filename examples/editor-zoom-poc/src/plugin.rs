//! A GUI-only CLAP host for the zoom experiment. Graph zoom never changes plugin UI scale.

use std::ffi::{CStr, CString, c_char, c_void};
use std::io::Write;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap_sys::entry::clap_plugin_entry;
use clap_sys::ext::gui::*;
use clap_sys::ext::log::*;
#[cfg(target_os = "linux")]
use clap_sys::ext::posix_fd_support::*;
use clap_sys::ext::thread_check::*;
use clap_sys::ext::timer_support::*;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};
use libloading::Library;

#[cfg(target_os = "macos")]
const API: &CStr = CLAP_WINDOW_API_COCOA;
#[cfg(target_os = "windows")]
const API: &CStr = CLAP_WINDOW_API_WIN32;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const API: &CStr = CLAP_WINDOW_API_X11;

const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

fn trace(message: std::fmt::Arguments<'_>) {
    if std::env::var_os("NODEPLAYER_ZOOM_POC_TRACE").is_some() {
        let _ = writeln!(std::io::stderr().lock(), "CLAP PoC: {message}");
    }
}

fn valid_size(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_PIXELS
}

fn text(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        // SAFETY: CLAP strings are NUL-terminated and live with their descriptor/callback.
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

struct Timer {
    id: clap_id,
    period: Duration,
    next: Instant,
}

struct Timers {
    next_id: clap_id,
    entries: Vec<Timer>,
}

struct HostState {
    main_thread: ThreadId,
    callback: AtomicBool,
    restart: AtomicBool,
    resize: AtomicU64,
    // 0 = no request, 1 = show, 2 = hide.
    visibility: AtomicU8,
    // 0 = open, 1 = closed, 2 = already destroyed by the plugin.
    closed: AtomicU8,
    timers: Mutex<Timers>,
    #[cfg(target_os = "linux")]
    fds: Mutex<Vec<(i32, clap_posix_fd_flags)>>,
}

impl HostState {
    fn new() -> Self {
        Self {
            main_thread: std::thread::current().id(),
            callback: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            resize: AtomicU64::new(0),
            visibility: AtomicU8::new(0),
            closed: AtomicU8::new(0),
            timers: Mutex::new(Timers {
                next_id: 1,
                entries: Vec::new(),
            }),
            #[cfg(target_os = "linux")]
            fds: Mutex::new(Vec::new()),
        }
    }

    fn reset(&self) {
        self.callback.store(false, Ordering::Release);
        self.restart.store(false, Ordering::Release);
        self.resize.store(0, Ordering::Release);
        self.visibility.store(0, Ordering::Release);
        self.closed.store(0, Ordering::Release);
        if let Ok(mut timers) = self.timers.lock() {
            timers.entries.clear();
        }
        #[cfg(target_os = "linux")]
        if let Ok(mut fds) = self.fds.lock() {
            fds.clear();
        }
    }
}

fn host(state: &mut HostState) -> clap_host {
    clap_host {
        clap_version: CLAP_VERSION,
        host_data: (state as *mut HostState).cast(),
        name: c"nodeplayer editor zoom PoC".as_ptr(),
        vendor: c"nodeplayer".as_ptr(),
        url: c"".as_ptr(),
        version: c"0.1.0".as_ptr(),
        get_extension: Some(get_extension),
        request_restart: Some(request_restart),
        request_process: Some(request_process),
        request_callback: Some(request_callback),
    }
}

unsafe fn state<'a>(host: *const clap_host) -> Option<&'a HostState> {
    // SAFETY: these callbacks only receive the host kept alive in PluginEditor.
    unsafe { host.as_ref()?.host_data.cast::<HostState>().as_ref() }
}

unsafe extern "C" fn get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    // SAFETY: extension identifiers follow CLAP's NUL-terminated string contract.
    let id = unsafe { CStr::from_ptr(id) };
    trace(format_args!("host extension {}", id.to_string_lossy()));
    if id == CLAP_EXT_GUI {
        (&HOST_GUI as *const clap_host_gui).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        (&HOST_TIMERS as *const clap_host_timer_support).cast()
    } else if id == CLAP_EXT_THREAD_CHECK {
        (&HOST_THREADS as *const clap_host_thread_check).cast()
    } else if id == CLAP_EXT_LOG {
        (&HOST_LOG as *const clap_host_log).cast()
    } else {
        #[cfg(target_os = "linux")]
        if id == CLAP_EXT_POSIX_FD_SUPPORT {
            return (&HOST_FDS as *const clap_host_posix_fd_support).cast();
        }
        std::ptr::null()
    }
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.restart.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn request_process(_host: *const clap_host) {
    // This experiment never activates DSP; request_process is a scheduling hint only.
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    if let Some(state) = unsafe { state(host) } {
        state.callback.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn resize_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    if !valid_size(width, height) {
        return false;
    }
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    state.resize.store(
        (u64::from(width) << 32) | u64::from(height),
        Ordering::Release,
    );
    true
}

unsafe extern "C" fn request_show(host: *const clap_host) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    state.visibility.store(1, Ordering::Release);
    true
}

unsafe extern "C" fn request_hide(host: *const clap_host) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    state.visibility.store(2, Ordering::Release);
    true
}

unsafe extern "C" fn closed(host: *const clap_host, was_destroyed: bool) {
    if let Some(state) = unsafe { state(host) } {
        state
            .closed
            .store(if was_destroyed { 2 } else { 1 }, Ordering::Release);
    }
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(resize_hints_changed),
    request_resize: Some(request_resize),
    request_show: Some(request_show),
    request_hide: Some(request_hide),
    closed: Some(closed),
};

unsafe extern "C" fn register_timer(
    host: *const clap_host,
    period_ms: u32,
    output: *mut clap_id,
) -> bool {
    if output.is_null() || period_ms == 0 {
        return false;
    }
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let Ok(mut timers) = state.timers.lock() else {
        return false;
    };
    if timers.entries.len() >= 1024 {
        return false;
    }
    let id = loop {
        let id = timers.next_id;
        timers.next_id = timers.next_id.wrapping_add(1);
        if id != CLAP_INVALID_ID && !timers.entries.iter().any(|timer| timer.id == id) {
            break id;
        }
    };
    let period = Duration::from_millis(u64::from(period_ms));
    timers.entries.push(Timer {
        id,
        period,
        next: Instant::now() + period,
    });
    trace(format_args!("register timer {id}, period {period_ms} ms"));
    // SAFETY: output is the plugin-provided writable timer ID pointer.
    unsafe { output.write(id) };
    true
}

unsafe extern "C" fn unregister_timer(host: *const clap_host, id: clap_id) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let Ok(mut timers) = state.timers.lock() else {
        return false;
    };
    let Some(index) = timers.entries.iter().position(|timer| timer.id == id) else {
        return false;
    };
    timers.entries.swap_remove(index);
    true
}

static HOST_TIMERS: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(register_timer),
    unregister_timer: Some(unregister_timer),
};

#[cfg(target_os = "linux")]
fn valid_fd_flags(flags: clap_posix_fd_flags) -> bool {
    let known = CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE | CLAP_POSIX_FD_ERROR;
    flags & !known == 0
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn register_fd(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    if fd < 0 || !valid_fd_flags(flags) || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return false;
    }
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    let Ok(mut fds) = state.fds.lock() else {
        return false;
    };
    if state.main_thread != std::thread::current().id()
        || fds.len() >= 1024
        || fds.iter().any(|entry| entry.0 == fd)
    {
        return false;
    }
    fds.push((fd, flags));
    trace(format_args!("register fd {fd}, flags {flags:#x}"));
    true
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn modify_fd(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    if !valid_fd_flags(flags) {
        return false;
    }
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    if state.main_thread != std::thread::current().id() {
        return false;
    }
    let Ok(mut fds) = state.fds.lock() else {
        return false;
    };
    let Some(entry) = fds.iter_mut().find(|entry| entry.0 == fd) else {
        return false;
    };
    entry.1 = flags;
    true
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn unregister_fd(host: *const clap_host, fd: i32) -> bool {
    let Some(state) = (unsafe { state(host) }) else {
        return false;
    };
    if state.main_thread != std::thread::current().id() {
        return false;
    }
    let Ok(mut fds) = state.fds.lock() else {
        return false;
    };
    let Some(index) = fds.iter().position(|entry| entry.0 == fd) else {
        return false;
    };
    fds.swap_remove(index);
    true
}

#[cfg(target_os = "linux")]
static HOST_FDS: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(register_fd),
    modify_fd: Some(modify_fd),
    unregister_fd: Some(unregister_fd),
};

#[cfg(target_os = "linux")]
fn ready_fds(state: &HostState) -> Result<Vec<(i32, clap_posix_fd_flags)>> {
    let snapshot = state
        .fds
        .lock()
        .map_err(|_| anyhow!("CLAP fd lock poisoned"))?
        .clone();
    if snapshot.is_empty() {
        return Ok(Vec::new());
    }
    let mut pollfds: Vec<_> = snapshot
        .iter()
        .map(|&(fd, flags)| {
            let mut events = 0;
            if flags & CLAP_POSIX_FD_READ != 0 {
                events |= libc::POLLIN | libc::POLLPRI;
            }
            if flags & CLAP_POSIX_FD_WRITE != 0 {
                events |= libc::POLLOUT;
            }
            libc::pollfd {
                fd,
                events,
                revents: 0,
            }
        })
        .collect();
    // SAFETY: the vector contains this many initialized pollfd values. Timeout zero never
    // blocks the GUI thread; the plugin retains ownership of its file descriptors.
    let result = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 0) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(Vec::new());
        }
        return Err(error).context("poll CLAP GUI file descriptors");
    }
    Ok(pollfds
        .into_iter()
        .filter_map(|event| {
            let mut flags = 0;
            if event.revents & (libc::POLLIN | libc::POLLPRI) != 0 {
                flags |= CLAP_POSIX_FD_READ;
            }
            if event.revents & libc::POLLOUT != 0 {
                flags |= CLAP_POSIX_FD_WRITE;
            }
            if event.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                flags |= CLAP_POSIX_FD_ERROR;
            }
            (flags != 0).then_some((event.fd, flags))
        })
        .collect())
}

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    unsafe { state(host) }.is_some_and(|state| state.main_thread == std::thread::current().id())
}

unsafe extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    false
}

static HOST_THREADS: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(is_main_thread),
    is_audio_thread: Some(is_audio_thread),
};

unsafe extern "C" fn log(
    _host: *const clap_host,
    severity: clap_log_severity,
    message: *const c_char,
) {
    let message: String = text(message).chars().take(2048).collect();
    let _ = writeln!(std::io::stderr().lock(), "CLAP[{severity}]: {message}");
}

static HOST_LOG: clap_host_log = clap_host_log { log: Some(log) };

struct Module {
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    // Native toolkits can retain autoreleased objects/classes after GUI destruction. Keep
    // plugin code mapped for the rest of this PoC process, as the production host does.
    // Module::drop still pairs successful init with deinit; the OS reclaims this handle.
    _library: ManuallyDrop<Library>,
}

impl Module {
    fn load(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("plugin path {}", path.display()))?;
        let binary = binary_path(&path)?;
        #[cfg(unix)]
        let library = unsafe {
            use libloading::os::unix::{Library as UnixLibrary, RTLD_LOCAL, RTLD_NOW};
            UnixLibrary::open(Some(&binary), RTLD_NOW | RTLD_LOCAL).map(Library::from)
        };
        #[cfg(not(unix))]
        let library = unsafe { Library::new(&binary) };
        let library = library.with_context(|| format!("load CLAP binary {}", binary.display()))?;
        // On macOS init receives the bundle path, not its Contents/MacOS executable.
        let init_path = if cfg!(target_os = "macos") {
            &path
        } else {
            &binary
        };
        let init_path = CString::new(
            init_path
                .to_str()
                .context("CLAP plugin path must be UTF-8")?,
        )?;
        unsafe {
            let entry = *library
                .get::<*const clap_plugin_entry>(b"clap_entry")
                .context("CLAP binary does not export clap_entry")?;
            let entry_ref = entry.as_ref().context("null clap_entry")?;
            ensure!(
                clap_version_is_compatible(entry_ref.clap_version),
                "incompatible CLAP version"
            );
            ensure!(
                entry_ref.deinit.is_some(),
                "clap_entry has no deinit callback"
            );
            let init = entry_ref.init.context("clap_entry has no init callback")?;
            ensure!(init(init_path.as_ptr()), "clap_entry.init failed");
            // Construct the owner immediately so every subsequent error pairs init with deinit.
            let mut module = Self {
                entry,
                factory: std::ptr::null(),
                _library: ManuallyDrop::new(library),
            };
            module.factory = entry_ref
                .get_factory
                .map_or(std::ptr::null(), |f| f(CLAP_PLUGIN_FACTORY_ID.as_ptr()))
                .cast();
            ensure!(
                !module.factory.is_null(),
                "CLAP binary has no plugin factory"
            );
            Ok(module)
        }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: Module exists only after successful init; its Library remains loaded here.
        unsafe {
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
    }
}

/// Owns a single plugin GUI and all host memory borrowed by that plugin.
///
/// Create, pump, and drop this value on the native UI thread. The caller owns the native
/// parent and must keep it alive until after this value has been dropped.
pub struct PluginEditor {
    plugin: *const clap_plugin,
    gui: Option<clap_plugin_gui>,
    gui_created: bool,
    shown: bool,
    attached: bool,
    size: (u32, u32),
    name: String,
    host: Box<clap_host>,
    state: Box<HostState>,
    // Last field: deinit happens after the plugin and its borrowed host data are done.
    module: Module,
}

impl PluginEditor {
    pub fn load(path: &Path, filter: Option<&str>) -> Result<Self> {
        let module = Module::load(path)?;
        let mut state = Box::new(HostState::new());
        let host = Box::new(host(&mut state));
        let mut editor = Self {
            plugin: std::ptr::null(),
            gui: None,
            gui_created: false,
            shown: false,
            attached: false,
            size: (0, 0),
            name: String::new(),
            host,
            state,
            module,
        };
        let filter = filter.unwrap_or("").to_lowercase();
        let mut failures = Vec::new();
        unsafe {
            let factory = &*editor.module.factory;
            let count = factory
                .get_plugin_count
                .context("CLAP factory has no get_plugin_count")?(
                editor.module.factory
            );
            ensure!(
                count <= 16384,
                "CLAP factory returned an excessive descriptor count"
            );
            let descriptor = factory
                .get_plugin_descriptor
                .context("CLAP factory has no get_plugin_descriptor")?;
            let create = factory
                .create_plugin
                .context("CLAP factory has no create_plugin")?;
            for index in 0..count {
                let Some(info) = descriptor(editor.module.factory, index).as_ref() else {
                    continue;
                };
                if info.id.is_null() || !clap_version_is_compatible(info.clap_version) {
                    continue;
                }
                let name = text(info.name);
                let id = text(info.id);
                if !filter.is_empty()
                    && !name.to_lowercase().contains(&filter)
                    && !id.to_lowercase().contains(&filter)
                {
                    continue;
                }
                editor.state.reset();
                editor.name = if name.is_empty() { id } else { name };
                editor.plugin = create(editor.module.factory, &*editor.host, info.id);
                let attempt = editor.create_gui();
                match attempt {
                    Ok(()) => return Ok(editor),
                    Err(error) => {
                        if failures.len() < 16 {
                            failures.push(format!("{}: {error:#}", editor.name));
                        }
                        editor.destroy_plugin();
                    }
                }
            }
        }
        if failures.is_empty() {
            bail!("no CLAP plugin matches {:?} in {}", filter, path.display());
        }
        bail!(
            "no usable embedded {} CLAP GUI: {}",
            API.to_string_lossy(),
            failures.join("; ")
        )
    }

    unsafe fn create_gui(&mut self) -> Result<()> {
        unsafe {
            let plugin = self
                .plugin
                .as_ref()
                .context("factory.create_plugin returned null")?;
            ensure!(plugin.destroy.is_some(), "plugin has no destroy callback");
            let init = plugin.init.context("plugin has no init callback")?;
            ensure!(init(self.plugin), "plugin.init failed");
            let get = plugin
                .get_extension
                .context("plugin has no get_extension callback")?;
            let gui = get(self.plugin, CLAP_EXT_GUI.as_ptr())
                .cast::<clap_plugin_gui>()
                .as_ref()
                .context("plugin has no GUI extension")?;
            ensure!(
                gui.destroy.is_some()
                    && gui.get_size.is_some()
                    && gui.set_parent.is_some()
                    && gui.show.is_some(),
                "plugin GUI is missing a required callback"
            );
            ensure!(
                gui.is_api_supported
                    .is_some_and(|f| f(self.plugin, API.as_ptr(), false)),
                "plugin does not support embedded {}",
                API.to_string_lossy()
            );
            self.gui = Some(*gui);
            ensure!(
                gui.create
                    .is_some_and(|f| f(self.plugin, API.as_ptr(), false)),
                "plugin GUI creation failed"
            );
            self.gui_created = true;
            let (mut width, mut height) = (0, 0);
            ensure!(
                gui.get_size
                    .is_some_and(|f| f(self.plugin, &mut width, &mut height)),
                "plugin GUI get_size failed"
            );
            ensure!(
                valid_size(width, height),
                "unsupported plugin GUI size {width}x{height}"
            );
            self.size = (width, height);
            Ok(())
        }
    }

    pub fn initial_size(&self) -> (u32, u32) {
        self.size
    }
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attach(&mut self, parent: usize) -> Result<()> {
        self.ensure_ui_thread()?;
        ensure!(parent != 0, "CLAP GUI parent handle is null");
        ensure!(!self.attached, "CLAP GUI is already attached");
        let gui = self.gui.context("CLAP GUI is not created")?;
        #[cfg(target_os = "macos")]
        let specific = clap_window_handle {
            cocoa: parent as *mut c_void,
        };
        #[cfg(target_os = "windows")]
        let specific = clap_window_handle {
            win32: parent as *mut c_void,
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let specific = clap_window_handle { x11: parent as _ };
        let window = clap_window {
            api: API.as_ptr(),
            specific,
        };
        unsafe {
            ensure!(
                gui.set_parent.is_some_and(|f| f(self.plugin, &window)),
                "CLAP GUI set_parent failed"
            );
            self.attached = true;
            ensure!(
                gui.show.is_some_and(|f| f(self.plugin)),
                "CLAP GUI show failed"
            );
        }
        self.shown = true;
        Ok(())
    }

    pub fn idle(&mut self) -> Result<Option<(u32, u32)>> {
        self.ensure_ui_thread()?;
        self.check_closed()?;
        if self.state.callback.swap(false, Ordering::AcqRel) {
            trace(format_args!("on_main_thread"));
            unsafe {
                if let Some(callback) = (*self.plugin).on_main_thread {
                    callback(self.plugin);
                }
            }
        }
        let now = Instant::now();
        let due = {
            let mut timers = self
                .state
                .timers
                .lock()
                .map_err(|_| anyhow!("CLAP timer lock poisoned"))?;
            timers
                .entries
                .iter_mut()
                .filter_map(|timer| {
                    if timer.next <= now {
                        timer.next = now + timer.period;
                        Some(timer.id)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        if !due.is_empty() {
            let extension = unsafe {
                (*self.plugin)
                    .get_extension
                    .map_or(std::ptr::null(), |get| {
                        get(self.plugin, CLAP_EXT_TIMER_SUPPORT.as_ptr())
                    })
                    .cast::<clap_plugin_timer_support>()
                    .as_ref()
            };
            if let Some(on_timer) = extension.and_then(|extension| extension.on_timer) {
                for id in due {
                    let registered = self
                        .state
                        .timers
                        .lock()
                        .is_ok_and(|timers| timers.entries.iter().any(|timer| timer.id == id));
                    if registered {
                        trace(format_args!("on_timer {id}"));
                        unsafe {
                            on_timer(self.plugin, id);
                        }
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        self.pump_fds()?;
        self.check_closed()?;
        if self.state.restart.swap(false, Ordering::AcqRel) {
            let _ = writeln!(
                std::io::stderr().lock(),
                "CLAP {} requested a DSP restart; the zoom PoC hosts its GUI only",
                self.name
            );
        }
        let visibility = self.state.visibility.swap(0, Ordering::AcqRel);
        if self.attached && visibility != 0 {
            let gui = self.gui.context("CLAP GUI is not created")?;
            unsafe {
                if visibility == 1 {
                    ensure!(
                        gui.show.is_some_and(|show| show(self.plugin)),
                        "CLAP GUI show request failed"
                    );
                    self.shown = true;
                } else {
                    ensure!(
                        gui.hide.is_some_and(|hide| hide(self.plugin)),
                        "CLAP GUI hide request failed"
                    );
                    self.shown = false;
                }
            }
        }
        let requested = self.state.resize.swap(0, Ordering::AcqRel);
        if requested != 0 {
            self.size = ((requested >> 32) as u32, requested as u32);
            Ok(Some(self.size))
        } else {
            Ok(None)
        }
    }

    fn ensure_ui_thread(&self) -> Result<()> {
        ensure!(
            self.state.main_thread == std::thread::current().id(),
            "CLAP GUI called from another thread"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn pump_fds(&self) -> Result<()> {
        let ready = ready_fds(&self.state)?;
        if ready.is_empty() {
            return Ok(());
        }
        let extension = unsafe {
            (*self.plugin)
                .get_extension
                .map_or(std::ptr::null(), |get| {
                    get(self.plugin, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr())
                })
                .cast::<clap_plugin_posix_fd_support>()
                .as_ref()
        };
        let Some(on_fd) = extension.and_then(|extension| extension.on_fd) else {
            bail!("CLAP GUI registered file descriptors without an on_fd callback");
        };
        // No registry lock spans the callback: toolkits may modify/unregister descriptors
        // re-entrantly while handling X11 events. Recheck watches after earlier callbacks.
        for (fd, mut flags) in ready {
            let requested = self
                .state
                .fds
                .lock()
                .map_err(|_| anyhow!("CLAP fd lock poisoned"))?
                .iter()
                .find(|entry| entry.0 == fd)
                .map(|entry| entry.1);
            if let Some(requested) = requested {
                flags &= requested | CLAP_POSIX_FD_ERROR;
                if flags != 0 {
                    trace(format_args!("on_fd {fd}, flags {flags:#x}"));
                    unsafe {
                        on_fd(self.plugin, fd, flags);
                    }
                }
            }
        }
        Ok(())
    }

    fn check_closed(&mut self) -> Result<()> {
        match self.state.closed.load(Ordering::Acquire) {
            0 => Ok(()),
            status => {
                if status == 2 {
                    self.gui_created = false;
                }
                self.shown = false;
                bail!("CLAP plugin closed its editor");
            }
        }
    }

    unsafe fn destroy_plugin(&mut self) {
        unsafe {
            if self.gui_created
                && self.state.closed.load(Ordering::Acquire) != 2
                && let Some(gui) = &self.gui
            {
                if self.shown
                    && let Some(hide) = gui.hide
                {
                    hide(self.plugin);
                }
                if let Some(destroy) = gui.destroy {
                    destroy(self.plugin);
                }
            }
            self.gui_created = false;
            self.shown = false;
            self.attached = false;
            self.gui = None;
            if let Some(plugin) = self.plugin.as_ref()
                && let Some(destroy) = plugin.destroy
            {
                destroy(self.plugin);
            }
            self.plugin = std::ptr::null();
        }
    }
}

impl Drop for PluginEditor {
    fn drop(&mut self) {
        // The caller keeps this value on its UI thread and drops it before the native parent.
        unsafe {
            self.destroy_plugin();
        }
    }
}

fn binary_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    ensure!(path.is_dir(), "CLAP path is neither a file nor a bundle");
    #[cfg(target_os = "macos")]
    {
        let directory = path.join("Contents/MacOS");
        let mut candidates: Vec<_> = std::fs::read_dir(&directory)
            .with_context(|| format!("read CLAP bundle executables in {}", directory.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        candidates.sort();
        // Bundles normally contain one executable. Honor the bundle's name when helpers are
        // present; reject remaining ambiguity rather than choosing an arbitrary helper.
        if let Some(stem) = path.file_stem() {
            let named = directory.join(stem);
            if candidates.contains(&named) {
                return Ok(named);
            }
        }
        ensure!(
            candidates.len() == 1,
            "CLAP bundle must identify a single executable in {}",
            directory.display()
        );
        Ok(candidates.remove(0))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let os = if cfg!(target_os = "windows") {
            "win"
        } else {
            "linux"
        };
        let native = path
            .join("Contents")
            .join(format!("{}-{os}", std::env::consts::ARCH));
        let mut candidates = binary_candidates(if native.is_dir() { &native } else { path }, 0)?;
        candidates.sort();
        ensure!(
            candidates.len() == 1,
            "CLAP bundle must contain a single native binary, found {} in {}",
            candidates.len(),
            path.display()
        );
        Ok(candidates.remove(0))
    }
}

#[cfg(not(target_os = "macos"))]
fn binary_candidates(path: &Path, depth: u8) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() && depth < 3 {
            candidates.extend(binary_candidates(&path, depth + 1)?);
        } else if path.is_file()
            && path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("clap")
                    || extension.eq_ignore_ascii_case(std::env::consts::DLL_EXTENSION)
            })
        {
            candidates.push(path);
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_requests_reject_invalid_allocations_and_preserve_valid_request() {
        let mut state = HostState::new();
        let host = host(&mut state);
        unsafe {
            assert!(request_resize(&host, 600, 340));
            for (width, height) in [(0, 340), (600, 0), (8193, 1), (8192, 8192)] {
                assert!(!request_resize(&host, width, height));
            }
        }
        let requested = state.resize.load(Ordering::Acquire);
        assert_eq!((requested >> 32, requested as u32), (600, 340));
    }

    #[test]
    fn timer_registration_assigns_distinct_ids_and_unregisters() {
        let mut state = HostState::new();
        let host = host(&mut state);
        let (mut first, mut second) = (CLAP_INVALID_ID, CLAP_INVALID_ID);
        unsafe {
            assert!(!register_timer(&host, 0, &mut first));
            assert!(!register_timer(&host, 10, std::ptr::null_mut()));
            assert!(register_timer(&host, 10, &mut first));
            assert!(register_timer(&host, 10, &mut second));
            assert_ne!(first, second);
            assert_ne!(first, CLAP_INVALID_ID);
            assert!(unregister_timer(&host, first));
            assert!(!unregister_timer(&host, first));
        }
        assert_eq!(state.timers.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn thread_check_distinguishes_the_ui_thread() {
        let mut state = HostState::new();
        let host = host(&mut state);
        assert!(unsafe { is_main_thread(&host) });
        assert!(!unsafe { is_audio_thread(&host) });
        std::thread::scope(|scope| {
            scope
                .spawn(|| assert!(!unsafe { is_main_thread(&host) }))
                .join()
                .unwrap();
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_registration_validates_and_modifies_existing_watches() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (socket, _peer) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let mut state = HostState::new();
        let host = host(&mut state);
        unsafe {
            assert!(!register_fd(&host, -1, CLAP_POSIX_FD_READ));
            assert!(!register_fd(&host, fd, 1 << 15));
            assert!(register_fd(&host, fd, CLAP_POSIX_FD_READ));
            assert!(!register_fd(&host, fd, CLAP_POSIX_FD_WRITE));
            assert!(!modify_fd(&host, fd + 100_000, CLAP_POSIX_FD_READ));
            assert!(modify_fd(&host, fd, 0));
            assert_eq!(state.fds.lock().unwrap().as_slice(), &[(fd, 0)]);
            assert!(unregister_fd(&host, fd));
            assert!(!unregister_fd(&host, fd));
        }
        assert!(ready_fds(&state).unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_polling_delivers_level_triggered_readiness_without_consuming_data() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let mut state = HostState::new();
        let host = host(&mut state);
        assert!(unsafe { register_fd(&host, fd, CLAP_POSIX_FD_READ) });
        assert!(ready_fds(&state).unwrap().is_empty());
        peer.write_all(&[42]).unwrap();
        assert_eq!(ready_fds(&state).unwrap(), vec![(fd, CLAP_POSIX_FD_READ)]);
        assert_eq!(ready_fds(&state).unwrap(), vec![(fd, CLAP_POSIX_FD_READ)]);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [42]);
        assert!(ready_fds(&state).unwrap().is_empty());
        assert!(unsafe { modify_fd(&host, fd, CLAP_POSIX_FD_WRITE) });
        assert_eq!(ready_fds(&state).unwrap(), vec![(fd, CLAP_POSIX_FD_WRITE)]);
        assert!(unsafe { unregister_fd(&host, fd) });
    }
}
