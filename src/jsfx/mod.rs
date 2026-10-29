//! JSFX (REAPER's scripted effects) through the bundled ysfx: scanning, instances with their
//! sliders and @gfx graphics drawn inside the node, and the audio-thread processor.

mod ffi;

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use egui::{Color32, Rect, Sense, Vec2, pos2, vec2};

use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use crate::plugins::{self, BodyIo, Format, ParamDesc, PluginInfo};
use ffi::*;

const QUEUE_LEN: usize = 1024;

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

unsafe extern "C" fn log_reporter(_user: isize, level: i32, msg: *const c_char) {
    let msg = cstr(msg);
    match level {
        YSFX_LOG_INFO => log::info!("jsfx: {msg}"),
        YSFX_LOG_WARNING => log::warn!("jsfx: {msg}"),
        _ => log::error!("jsfx: {msg}"),
    }
}

/// `$JSFX_PATH` (colon separated), REAPER's effects folder, and our own.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        std::env::var_os("JSFX_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        v.push(home.join(".config/REAPER/Effects"));
        v.push(home.join(".local/share/nodeplayer/jsfx"));
    }
    v
}

fn reaper_effects() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("HOME")?).join(".config/REAPER/Effects");
    p.is_dir().then_some(p)
}

/// JSFX files are often extensionless (REAPER's own) or `.txt`; those are recognized by a
/// `desc:` line near the top. Imported libraries (`.jsfx-inc`) aren't effects by themselves.
pub fn is_jsfx_file(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("jsfx") => true,
        None | Some("txt") => {
            let Ok(f) = std::fs::File::open(path) else { return false };
            let mut head = Vec::new();
            if f.take(16 * 1024).read_to_end(&mut head).is_err() {
                return false;
            }
            let text = String::from_utf8_lossy(&head);
            text.lines().any(|l| l.trim_start().starts_with("desc:"))
                && text.lines().any(|l| l.trim_start().starts_with('@') || l.trim_start().starts_with("slider"))
        }
        _ => false,
    }
}

/// Finds JSFX files below `root`.
pub fn find_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_files(&p, out);
        } else if is_jsfx_file(&p) {
            out.push(p);
        }
    }
}

/// A loaded effect. The lock keeps the audio thread out while the UI saves or loads state,
/// as ysfx expects.
struct Fx {
    ptr: *mut ysfx_t,
    lock: Mutex<()>,
}

unsafe impl Send for Fx {}
unsafe impl Sync for Fx {}

impl Fx {
    /// Parses `path`; with `compile`, also compiles it so it can run.
    fn load(path: &Path, compile: bool) -> Result<Self> {
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())?;
        unsafe {
            let config = ysfx_config_new();
            ysfx_set_log_reporter(config, Some(log_reporter));
            ysfx_guess_file_roots(config, cpath.as_ptr());
            // Effects kept outside REAPER's tree can still import its libraries.
            if cstr(ysfx_get_import_root(config)).is_empty()
                && let Some(root) = reaper_effects()
            {
                let mut s = root.as_os_str().as_encoded_bytes().to_vec();
                s.push(b'/');
                let root = CString::new(s)?;
                ysfx_set_import_root(config, root.as_ptr());
            }
            ysfx_register_builtin_audio_formats(config);
            let ptr = ysfx_new(config);
            ysfx_config_free(config);
            let fx = Self { ptr, lock: Mutex::new(()) };
            // Scanning doesn't need the imports, which is much faster for big libraries.
            let opts = if compile { 0 } else { 1 };
            if !ysfx_load_file(ptr, cpath.as_ptr(), opts) {
                bail!("can't load {}", path.display());
            }
            if compile && !ysfx_compile(ptr, 0) {
                bail!("can't compile {}", path.display());
            }
            Ok(fx)
        }
    }

    fn info(&self, path: &Path) -> PluginInfo {
        unsafe {
            let mut tags = [std::ptr::null::<c_char>(); 32];
            let n = ysfx_get_tags(self.ptr, tags.as_mut_ptr(), tags.len() as u32) as usize;
            // Tags are space separated, but authors often add commas too.
            let category = tags[..n.min(tags.len())]
                .iter()
                .flat_map(|t| cstr(*t).split(',').map(|s| s.trim().to_string()).collect::<Vec<_>>())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("|");
            let mut name = cstr(ysfx_get_name(self.ptr));
            if name.is_empty() {
                name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            }
            PluginInfo {
                format: Format::Jsfx,
                name,
                vendor: cstr(ysfx_get_author(self.ptr)),
                category,
                id: String::new(),
                path: path.to_path_buf(),
            }
        }
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        unsafe { ysfx_free(self.ptr) };
    }
}

pub fn classes(path: &Path) -> Result<Vec<PluginInfo>> {
    Ok(vec![Fx::load(path, false)?.info(path)])
}

// ---------------------------------------------------------------------------
// State blob: `NPJS`, slider count (u32), (index u32, value f64) per slider, serialized data
// length (u64) and bytes; little endian.

fn pack_state(st: &ysfx_state_t) -> Vec<u8> {
    let mut out = b"NPJS".to_vec();
    out.extend(st.slider_count.to_le_bytes());
    let sliders = unsafe { std::slice::from_raw_parts(st.sliders, st.slider_count as usize) };
    for s in sliders {
        out.extend(s.index.to_le_bytes());
        out.extend(s.value.to_le_bytes());
    }
    out.extend((st.data_size as u64).to_le_bytes());
    if st.data_size > 0 {
        out.extend(unsafe { std::slice::from_raw_parts(st.data, st.data_size) });
    }
    out
}

fn unpack_state(data: &[u8]) -> Result<(Vec<ysfx_state_slider_t>, Vec<u8>)> {
    let bad = || anyhow!("not a JSFX state");
    let mut rest = data.strip_prefix(b"NPJS").ok_or_else(bad)?;
    let mut take = |n: usize| -> Result<&[u8]> {
        if rest.len() < n {
            return Err(bad());
        }
        let (a, b) = rest.split_at(n);
        rest = b;
        Ok(a)
    };
    let count = u32::from_le_bytes(take(4)?.try_into()?);
    let mut sliders = Vec::new();
    for _ in 0..count.min(YSFX_MAX_SLIDERS) {
        let index = u32::from_le_bytes(take(4)?.try_into()?);
        let value = f64::from_le_bytes(take(8)?.try_into()?);
        sliders.push(ysfx_state_slider_t { index, value });
    }
    let len = u64::from_le_bytes(take(8)?.try_into()?) as usize;
    Ok((sliders, take(len)?.to_vec()))
}

// ---------------------------------------------------------------------------
// Instances

struct Slider {
    index: u32,
    name: String,
    curve: ysfx_slider_curve_t,
    enums: Vec<String>,
}

pub struct JsfxPlugin {
    info: PluginInfo,
    fx: Arc<Fx>,
    sliders: Vec<Slider>,
    params: Vec<ParamDesc>,
    to_audio: rtrb::Producer<(u32, f64)>,
    /// Values set from the UI, shown until the effect has picked them up.
    recent: HashMap<u32, (f64, Instant)>,
    latency: u32,
    gfx: Option<Gfx>,
    show_sliders: bool,
}

impl JsfxPlugin {
    pub fn create(info: &PluginInfo, sample_rate: f32, max_block: usize) -> Result<(Self, JsfxProcessor)> {
        let fx = Arc::new(Fx::load(&info.path, true)?);
        let p = fx.ptr;
        unsafe {
            ysfx_set_sample_rate(p, sample_rate as f64);
            ysfx_set_block_size(p, max_block as u32);
            ysfx_init(p);
        }
        let mut sliders = Vec::new();
        let mut params = Vec::new();
        for i in 0..YSFX_MAX_SLIDERS {
            unsafe {
                if !ysfx_slider_exists(p, i) {
                    continue;
                }
                let mut curve = ysfx_slider_curve_t::default();
                ysfx_slider_get_curve(p, i, &mut curve);
                let mut enums = Vec::new();
                if ysfx_slider_is_enum(p, i) {
                    let mut names = [std::ptr::null::<c_char>(); 256];
                    let n = ysfx_slider_get_enum_names(p, i, names.as_mut_ptr(), names.len() as u32) as usize;
                    enums = names[..n.min(names.len())].iter().map(|s| cstr(*s)).collect();
                }
                let name = cstr(ysfx_slider_get_name(p, i));
                let steps = if !enums.is_empty() {
                    enums.len() as i32 - 1
                } else if curve.inc > 0.0 && (curve.max - curve.min) / curve.inc <= 1000.0 {
                    ((curve.max - curve.min) / curve.inc).round() as i32
                } else {
                    0
                };
                params.push(ParamDesc {
                    id: i,
                    title: name.clone(),
                    units: String::new(),
                    step_count: steps.max(0),
                    default: ysfx_ysfx_value_to_normalized(curve.def, &curve),
                    hidden: ysfx_slider_is_path(p, i) || !ysfx_slider_is_initially_visible(p, i),
                    read_only: false,
                });
                sliders.push(Slider { index: i, name, curve, enums });
            }
        }
        let has_gfx = unsafe { ysfx_has_section(p, ffi::YSFX_SECTION_GFX) };
        let latency = unsafe { ysfx_get_pdc_delay(p) }.max(0.0) as u32;
        let (to_audio, from_ui) = rtrb::RingBuffer::new(QUEUE_LEN);
        let me = Self {
            info: info.clone(),
            fx: fx.clone(),
            sliders,
            params,
            to_audio,
            recent: HashMap::new(),
            latency,
            gfx: has_gfx.then(|| Gfx::new(fx.clone())),
            show_sliders: !has_gfx,
        };
        Ok((me, JsfxProcessor { fx, from_ui, time_set: false }))
    }

    fn value(&self, index: u32) -> f64 {
        if let Some((v, t)) = self.recent.get(&index)
            && t.elapsed() < Duration::from_millis(300)
        {
            return *v;
        }
        unsafe { ysfx_slider_get_value(self.fx.ptr, index) }
    }

    fn set_value(&mut self, index: u32, value: f64) {
        self.recent.insert(index, (value, Instant::now()));
        let _ = self.to_audio.push((index, value));
    }

    /// Sliders the effect currently shows (`slider_show` can change that at runtime).
    fn visible(&self, index: u32) -> bool {
        let mask = unsafe { ysfx_get_slider_visibility(self.fx.ptr, (index / 64) as u8) };
        mask & (1u64 << (index % 64)) != 0
    }

    fn sliders_ui(&mut self, ui: &mut egui::Ui) {
        let mut changes = Vec::new();
        let shown = |s: &Slider| self.visible(s.index) && !unsafe { ysfx_slider_is_path(self.fx.ptr, s.index) };
        if !self.sliders.iter().any(shown) {
            ui.weak("No visible sliders; hidden ones are under Params.");
            return;
        }
        egui::Grid::new(ui.id().with("sliders")).num_columns(2).show(ui, |ui| {
            for s in self.sliders.iter().filter(|s| shown(s)) {
                let mut v = self.value(s.index);
                ui.label(&s.name);
                if s.enums.is_empty() {
                    let c = &s.curve;
                    let mut slider = egui::Slider::new(&mut v, c.min.min(c.max)..=c.max.max(c.min)).clamping(egui::SliderClamping::Never);
                    if c.inc > 0.0 {
                        slider = slider.step_by(c.inc);
                    }
                    let r = ui.add(slider);
                    if r.double_clicked() {
                        changes.push((s.index, c.def));
                    } else if r.changed() {
                        changes.push((s.index, v));
                    }
                } else {
                    let cur = (v.round().max(0.0) as usize).min(s.enums.len() - 1);
                    let mut pick = cur;
                    egui::ComboBox::from_id_salt(ui.id().with(("enum", s.index)))
                        .selected_text(&s.enums[cur])
                        .show_ui(ui, |ui| {
                            for (k, name) in s.enums.iter().enumerate() {
                                ui.selectable_value(&mut pick, k, name);
                            }
                        });
                    if pick != cur {
                        changes.push((s.index, pick as f64));
                    }
                }
                ui.end_row();
            }
        });
        for (i, v) in changes {
            self.set_value(i, v);
        }
    }
}

impl plugins::Instance for JsfxPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn params(&self) -> &[ParamDesc] {
        &self.params
    }

    fn param_value(&self, id: u32) -> f64 {
        let Some(s) = self.sliders.iter().find(|s| s.index == id) else { return 0.0 };
        unsafe { ysfx_ysfx_value_to_normalized(self.value(id), &s.curve) }
    }

    fn param_display(&self, id: u32, value: f64) -> String {
        let Some(s) = self.sliders.iter().find(|s| s.index == id) else { return String::new() };
        let v = unsafe { ysfx_normalized_to_ysfx_value(value, &s.curve) };
        if !s.enums.is_empty() {
            return s.enums.get(v.round().max(0.0) as usize).cloned().unwrap_or_default();
        }
        let decimals = if s.curve.inc >= 1.0 { 0 } else if s.curve.inc > 0.0 { (-s.curve.inc.log10()).ceil() as usize } else { 3 };
        format!("{v:.decimals$}")
    }

    fn set_param(&mut self, id: u32, value: f64) {
        let Some(s) = self.sliders.iter().find(|s| s.index == id) else { return };
        let v = unsafe { ysfx_normalized_to_ysfx_value(value, &s.curve) };
        self.set_value(id, v);
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn idle(&mut self) {
        self.recent.retain(|_, (_, t)| t.elapsed() < Duration::from_millis(300));
    }

    fn midi_ports(&self) -> (bool, bool) {
        (true, true)
    }

    fn save_state(&self) -> Option<Vec<u8>> {
        let _gfx = self.gfx.as_ref().map(|g| g.shared.pause());
        let _g = self.fx.lock.lock().ok()?;
        unsafe {
            let st = ysfx_save_state(self.fx.ptr);
            if st.is_null() {
                return None;
            }
            let out = pack_state(&*st);
            ysfx_state_free(st);
            Some(out)
        }
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let (mut sliders, mut bytes) = unpack_state(data)?;
        let mut st = ysfx_state_t {
            sliders: sliders.as_mut_ptr(),
            slider_count: sliders.len() as u32,
            data: if bytes.is_empty() { std::ptr::null_mut() } else { bytes.as_mut_ptr() },
            data_size: bytes.len(),
        };
        let _gfx = self.gfx.as_ref().map(|g| g.shared.pause());
        let _g = self.fx.lock.lock().map_err(|_| anyhow!("lock poisoned"))?;
        if !unsafe { ysfx_load_state(self.fx.ptr, &mut st) } {
            bail!("effect rejected its state");
        }
        self.recent.clear();
        Ok(())
    }

    fn has_body(&self) -> bool {
        true
    }

    fn body_ui(&mut self, ui: &mut egui::Ui, io: &mut BodyIo) {
        if let Some(g) = &mut self.gfx {
            ui.horizontal(|ui| {
                let mut v = g.visible;
                if ui.toggle_value(&mut v, "Graphics").changed() {
                    g.set_visible(v);
                }
                ui.toggle_value(&mut self.show_sliders, "Sliders");
            });
        }
        if self.show_sliders {
            self.sliders_ui(ui);
        }
        if let Some(g) = &mut self.gfx
            && g.visible
        {
            g.show(ui, io);
        }
    }

    fn animating(&self) -> bool {
        self.gfx.as_ref().is_some_and(|g| g.visible)
    }
}

// ---------------------------------------------------------------------------
// @gfx
//
// ysfx wants @gfx on a dedicated thread, which also lets `gfx_showmenu` block until the
// user picks from the menu the UI thread shows meanwhile.

#[derive(Clone, Copy, Default, PartialEq)]
struct GfxInput {
    mods: u32,
    x: i32,
    y: i32,
    buttons: u32,
    focus: bool,
    over: bool,
}

/// A `gfx_showmenu` call waiting for the UI.
struct MenuRequest {
    spec: String,
    pos: (i32, i32),
    answer: Option<i32>,
}

#[derive(Default)]
struct GfxState {
    visible: bool,
    size: (u32, u32),
    input: GfxInput,
    wheel: (f64, f64),
    keys: Vec<(u32, u32, bool)>,
    /// Something changed that @gfx should see right away.
    poke: bool,
    /// Latest frame as RGBA, with its size and a counter bumped per new frame.
    frame: Vec<u8>,
    frame_size: (u32, u32),
    serial: u64,
    menu: Option<MenuRequest>,
}

struct GfxShared {
    state: Mutex<GfxState>,
    wake: Condvar,
    stop: AtomicBool,
    /// Held while @gfx runs, so state can be saved or loaded between frames.
    running: Mutex<()>,
}

impl GfxShared {
    fn lock(&self) -> MutexGuard<'_, GfxState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cancel_menu(&self) {
        if let Some(m) = &mut self.lock().menu {
            m.answer.get_or_insert(0);
        }
        self.wake.notify_all();
    }

    /// Waits until @gfx is between frames (closing any menu it's waiting on) and keeps it
    /// there while the guard lives.
    fn pause(&self) -> MutexGuard<'_, ()> {
        loop {
            self.cancel_menu();
            if let Ok(g) = self.running.try_lock() {
                return g;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

unsafe extern "C" fn show_menu(user: *mut c_void, spec: *const c_char, x: i32, y: i32) -> i32 {
    let shared = unsafe { &*(user as *const GfxShared) };
    let mut st = shared.lock();
    st.menu = Some(MenuRequest { spec: cstr(spec), pos: (x, y), answer: None });
    loop {
        if shared.stop.load(Ordering::Acquire) {
            st.menu = None;
            return 0;
        }
        if let Some(a) = st.menu.as_ref().and_then(|m| m.answer) {
            st.menu = None;
            return a;
        }
        st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
    }
}

fn gfx_thread(fx: Arc<Fx>, shared: Arc<GfxShared>) {
    let p = fx.ptr;
    let mut pixels: Vec<u8> = Vec::new();
    let mut setup = (0, 0);
    let mut last_run = Instant::now() - Duration::from_secs(1);
    let mut rgba = Vec::new();
    loop {
        let fps = unsafe { ysfx_get_requested_framerate(p) }.clamp(1, 60);
        let period = Duration::from_secs_f32(1.0 / fps as f32);
        let (input, wheel, keys, size) = {
            let mut st = shared.lock();
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                let due = last_run.elapsed() >= period;
                if st.visible && (st.poke || due) {
                    break;
                }
                let wait = if st.visible { period.saturating_sub(last_run.elapsed()) } else { Duration::from_millis(200) };
                st = shared.wake.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner()).0;
            }
            st.poke = false;
            (st.input, std::mem::take(&mut st.wheel), std::mem::take(&mut st.keys), st.size)
        };
        last_run = Instant::now();

        let _running = shared.running.lock().unwrap_or_else(|e| e.into_inner());
        if setup != size {
            let (w, h) = size;
            pixels = vec![0; (w * h * 4) as usize];
            let mut gc = ysfx_gfx_config_t {
                user_data: Arc::as_ptr(&shared) as *mut c_void,
                pixel_width: w,
                pixel_height: h,
                pixel_stride: w * 4,
                pixels: pixels.as_mut_ptr(),
                scale_factor: 1.0,
                show_menu: Some(show_menu),
                set_cursor: None,
                get_drop_file: None,
            };
            unsafe { ysfx_gfx_setup(p, &mut gc) };
        }
        let fresh = setup != size;
        setup = size;
        let changed = unsafe {
            ysfx_gfx_update_mouse(p, input.mods, input.x, input.y, input.buttons, wheel.0, wheel.1);
            ysfx_gfx_set_window_state(p, input.focus, true, input.over);
            for (mods, key, press) in keys {
                ysfx_gfx_add_key(p, mods, key, press);
            }
            ysfx_gfx_run(p)
        };
        drop(_running);
        if changed || fresh {
            rgba.clear();
            rgba.extend(pixels.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], 255]));
            let mut st = shared.lock();
            std::mem::swap(&mut st.frame, &mut rgba);
            st.frame_size = size;
            st.serial += 1;
        }
    }
}

/// The UI side of an effect's graphics.
struct Gfx {
    visible: bool,
    size: (u32, u32),
    shared: Arc<GfxShared>,
    thread: Option<std::thread::JoinHandle<()>>,
    texture: Option<egui::TextureHandle>,
    shown_serial: u64,
    last_input: GfxInput,
    /// The open menu has seen all buttons released, so the next click outside closes it.
    menu_armed: bool,
    /// Open submenus, as item indices from the top level down, and each level's size.
    menu_path: Vec<usize>,
    menu_sizes: Vec<egui::Vec2>,
}

const MAX_GFX: u32 = 2048;

impl Gfx {
    fn new(fx: Arc<Fx>) -> Self {
        let mut dim = [0u32; 2];
        unsafe { ysfx_get_gfx_dim(fx.ptr, dim.as_mut_ptr()) };
        let size = (if dim[0] > 0 { dim[0] } else { 400 }, if dim[1] > 0 { dim[1] } else { 300 });
        let size = (size.0.clamp(32, MAX_GFX), size.1.clamp(32, MAX_GFX));
        let shared = Arc::new(GfxShared {
            state: Mutex::new(GfxState { visible: true, size, ..Default::default() }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            running: Mutex::new(()),
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new().name("jsfx gfx".into()).spawn(move || gfx_thread(fx, shared)).ok()
        };
        Self {
            visible: true,
            size,
            shared,
            thread,
            texture: None,
            shown_serial: 0,
            last_input: GfxInput::default(),
            menu_armed: false,
            menu_path: Vec::new(),
            menu_sizes: Vec::new(),
        }
    }

    fn set_visible(&mut self, v: bool) {
        self.visible = v;
        if !v {
            self.shared.cancel_menu();
        }
        self.shared.lock().visible = v;
        self.shared.wake.notify_all();
    }

    fn show(&mut self, ui: &mut egui::Ui, io: &mut BodyIo) {
        let (w, h) = self.size;
        let (rect, resp) = ui.allocate_exact_size(vec2(w as f32, h as f32), Sense::click_and_drag());
        let scale = w as f32 / rect.width().max(1.0);

        // Mouse and keys, in framebuffer pixels.
        let hovered = resp.hovered() || resp.dragged();
        let (mods, buttons, pos) = ui.input(|i| {
            let m = i.modifiers;
            let mut mods = 0;
            if m.shift {
                mods |= YSFX_MOD_SHIFT;
            }
            if m.ctrl {
                mods |= YSFX_MOD_CTRL;
            }
            if m.alt {
                mods |= YSFX_MOD_ALT;
            }
            let mut b = 0;
            if i.pointer.primary_down() {
                b |= YSFX_BUTTON_LEFT;
            }
            if i.pointer.secondary_down() {
                b |= YSFX_BUTTON_RIGHT;
            }
            if i.pointer.middle_down() {
                b |= YSFX_BUTTON_MIDDLE;
            }
            (mods, b, i.pointer.latest_pos())
        });
        let rel = pos.map(|p| (p - rect.min) * scale).unwrap_or_default();
        let mut keys = Vec::new();
        if resp.hovered() {
            for e in ui.input(|i| i.events.clone()) {
                match e {
                    egui::Event::Text(t) => {
                        for c in t.chars() {
                            keys.push((mods, c as u32, true));
                            keys.push((mods, c as u32, false));
                        }
                    }
                    egui::Event::Key { key, pressed, .. } => {
                        if let Some(k) = map_key(key) {
                            keys.push((mods, k, pressed));
                        }
                    }
                    _ => {}
                }
            }
        }
        let input = GfxInput {
            mods,
            x: rel.x.round() as i32,
            y: rel.y.round() as i32,
            // Buttons only count when they went down over the graphics.
            buttons: if hovered { buttons } else { 0 },
            focus: hovered,
            over: resp.hovered(),
        };
        let wheel = if resp.hovered() { (io.scroll.y as f64 / 50.0, io.scroll.x as f64 / 50.0) } else { (0.0, 0.0) };
        io.wants_scroll |= resp.hovered();

        let menu = {
            let mut st = self.shared.lock();
            st.size = self.size;
            let poke = input != self.last_input || wheel != (0.0, 0.0) || !keys.is_empty();
            st.input = input;
            st.wheel.0 += wheel.0;
            st.wheel.1 += wheel.1;
            st.keys.extend(keys);
            st.poke |= poke;
            if st.serial != self.shown_serial && st.frame_size.0 * st.frame_size.1 * 4 == st.frame.len() as u32 {
                self.shown_serial = st.serial;
                let (fw, fh) = st.frame_size;
                let img = egui::ColorImage::from_rgba_unmultiplied([fw as usize, fh as usize], &st.frame);
                match &mut self.texture {
                    Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                    None => self.texture = Some(ui.ctx().load_texture("jsfx-gfx", img, egui::TextureOptions::NEAREST)),
                }
            }
            st.menu.as_ref().filter(|m| m.answer.is_none()).map(|m| (m.spec.clone(), m.pos))
        };
        self.last_input = input;
        self.shared.wake.notify_all();

        if let Some(t) = &self.texture {
            ui.painter().image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }

        match menu {
            Some((spec, (x, y))) => {
                let at = rect.min + vec2(x as f32, y as f32) / scale;
                if let Some(answer) = self.menu_ui(ui, &spec, at) {
                    if let Some(m) = &mut self.shared.lock().menu {
                        m.answer = Some(answer);
                    }
                    self.shared.wake.notify_all();
                }
            }
            None => self.menu_armed = false,
        }

        // Resize from the corner, like the visualizers.
        let handle = Rect::from_min_max(rect.max - vec2(12.0, 12.0), rect.max);
        let hr = ui.interact(handle, ui.id().with("gfx-resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
        let c = if hr.hovered() || hr.dragged() { Color32::from_gray(220) } else { Color32::from_gray(120) };
        for k in [4.0, 8.0] {
            ui.painter().line_segment(
                [pos2(handle.max.x - k, handle.max.y - 1.0), pos2(handle.max.x - 1.0, handle.max.y - k)],
                egui::Stroke::new(1.0, c),
            );
        }
        if hr.dragged() {
            let v = (Vec2::new(w as f32, h as f32) + hr.drag_delta()).clamp(vec2(64.0, 48.0), vec2(MAX_GFX as f32, MAX_GFX as f32));
            self.size = (v.x.round() as u32, v.y.round() as u32);
        }
    }

    /// Shows the menu `gfx_showmenu` asked for at `at`; returns the picked item id, or 0 when
    /// the menu was dismissed. Submenus open on hover, each in its own area.
    fn menu_ui(&mut self, ui: &mut egui::Ui, spec: &str, at: egui::Pos2) -> Option<i32> {
        let root = parse_menu(spec);
        let mut answer = None;
        let mut inside = false;
        let mut level_items = &root;
        let mut pos = at;
        let mut level = 0;
        loop {
            let mut open_sub: Option<(usize, f32)> = None;
            // Keep it on screen using last frame's size (egui's own constraining moves the
            // drawing but not the hit testing of a fixed-position area).
            let size = self.menu_sizes.get(level).copied().unwrap_or_default();
            let screen = ui.ctx().content_rect();
            let at = egui::pos2(
                pos.x.min(screen.right() - size.x).max(screen.left()),
                pos.y.min(screen.bottom() - size.y).max(screen.top()),
            );
            let area = egui::Area::new(ui.id().with(("jsfx-menu", level)))
                .order(egui::Order::Foreground)
                .fixed_pos(at)
                .constrain(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_min_width(140.0);
                        for (i, item) in level_items.iter().enumerate() {
                            match item {
                                MenuItem::Item { id, name, checked, enabled } => {
                                    let text = if *checked { format!("✔ {name}") } else { name.clone() };
                                    let r = ui.add_enabled(*enabled, egui::Button::new(text).frame(false));
                                    if r.clicked() {
                                        answer = Some(*id);
                                    }
                                    if r.hovered() {
                                        self.menu_path.truncate(level);
                                    }
                                }
                                MenuItem::Separator => {
                                    ui.separator();
                                }
                                MenuItem::Sub { name, enabled, .. } => {
                                    let open = self.menu_path.get(level) == Some(&i);
                                    let r = ui.add_enabled(*enabled, egui::Button::new(format!("{name}  ⏵")).frame(false).selected(open));
                                    if r.hovered() || r.clicked() {
                                        self.menu_path.truncate(level);
                                        self.menu_path.push(i);
                                    }
                                    if self.menu_path.get(level) == Some(&i) {
                                        open_sub = Some((i, r.rect.top()));
                                    }
                                }
                            }
                        }
                    });
                });
            inside |= area.response.contains_pointer();
            if self.menu_sizes.len() <= level {
                self.menu_sizes.resize(level + 1, egui::Vec2::ZERO);
            }
            self.menu_sizes[level] = area.response.rect.size();
            let right = area.response.rect.right();
            match open_sub.and_then(|(i, top)| match &level_items[i] {
                MenuItem::Sub { items, .. } => Some((items, egui::pos2(right, top))),
                _ => None,
            }) {
                Some((items, p)) => {
                    level_items = items;
                    pos = p;
                    level += 1;
                }
                None => break,
            }
        }
        let (down, clicked, escape) =
            ui.input(|i| (i.pointer.any_down(), i.pointer.any_click(), i.key_pressed(egui::Key::Escape)));
        if !down {
            self.menu_armed = true;
        }
        if answer.is_none() && (escape || (self.menu_armed && clicked && !inside)) {
            answer = Some(0);
        }
        if answer.is_some() {
            self.menu_path.clear();
        }
        answer
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.cancel_menu();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum MenuItem {
    Item { id: i32, name: String, checked: bool, enabled: bool },
    Separator,
    Sub { name: String, enabled: bool, items: Vec<MenuItem> },
}

fn parse_menu(spec: &str) -> Vec<MenuItem> {
    let Ok(c) = CString::new(spec) else { return Vec::new() };
    let menu = unsafe { ysfx_parse_menu(c.as_ptr()) };
    if menu.is_null() {
        return Vec::new();
    }
    let insns = unsafe { std::slice::from_raw_parts((*menu).insns, (*menu).insn_count as usize) };
    // A stack of open submenus; the root is at the bottom.
    let mut stack: Vec<(String, bool, Vec<MenuItem>)> = vec![(String::new(), true, Vec::new())];
    for i in insns {
        let enabled = i.item_flags & YSFX_MENU_ITEM_DISABLED == 0;
        match i.opcode {
            YSFX_MENU_ITEM => stack.last_mut().unwrap().2.push(MenuItem::Item {
                id: i.id as i32,
                name: cstr(i.name),
                checked: i.item_flags & YSFX_MENU_ITEM_CHECKED != 0,
                enabled,
            }),
            YSFX_MENU_SEPARATOR => stack.last_mut().unwrap().2.push(MenuItem::Separator),
            YSFX_MENU_SUB => stack.push((cstr(i.name), enabled, Vec::new())),
            YSFX_MENU_ENDSUB if stack.len() > 1 => {
                let (name, enabled, items) = stack.pop().unwrap();
                stack.last_mut().unwrap().2.push(MenuItem::Sub { name, enabled, items });
            }
            _ => {}
        }
    }
    unsafe { ysfx_menu_free(menu) };
    while stack.len() > 1 {
        let (name, enabled, items) = stack.pop().unwrap();
        stack.last_mut().unwrap().2.push(MenuItem::Sub { name, enabled, items });
    }
    stack.pop().unwrap().2
}

fn map_key(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    Some(match key {
        Backspace => YSFX_KEY_BACKSPACE,
        Escape => YSFX_KEY_ESCAPE,
        Delete => YSFX_KEY_DELETE,
        ArrowLeft => YSFX_KEY_LEFT,
        ArrowUp => YSFX_KEY_UP,
        ArrowRight => YSFX_KEY_RIGHT,
        ArrowDown => YSFX_KEY_DOWN,
        PageUp => YSFX_KEY_PAGE_UP,
        PageDown => YSFX_KEY_PAGE_DOWN,
        Home => YSFX_KEY_HOME,
        End => YSFX_KEY_END,
        Insert => YSFX_KEY_INSERT,
        Enter => 13,
        Tab => 9,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Audio thread

pub struct JsfxProcessor {
    fx: Arc<Fx>,
    from_ui: rtrb::Consumer<(u32, f64)>,
    time_set: bool,
}

impl JsfxProcessor {
    fn run(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf], midi_in: &[MidiEvent], midi_out: Option<&mut MidiBuf>) {
        let (i, o) = (&inputs[0], &mut outputs[0]);
        // The UI holds the lock while saving or loading state; let audio through meanwhile.
        let Ok(_g) = self.fx.lock.try_lock() else {
            o.l[..n].copy_from_slice(&i.l[..n]);
            o.r[..n].copy_from_slice(&i.r[..n]);
            if let Some(out) = midi_out {
                for e in midi_in {
                    out.push(*e);
                }
            }
            return;
        };
        let fx = self.fx.ptr;
        unsafe {
            if !self.time_set {
                let info = ysfx_time_info_t {
                    tempo: 120.0,
                    playback_state: YSFX_PLAYBACK_PLAYING,
                    time_position: 0.0,
                    beat_position: 0.0,
                    time_signature: [4, 4],
                };
                ysfx_set_time_info(fx, &info);
                self.time_set = true;
            }
            while let Ok((index, value)) = self.from_ui.pop() {
                ysfx_slider_set_value(fx, index, value, true);
            }
            for e in midi_in {
                let b = e.bytes();
                let ev = ysfx_midi_event_t { bus: 0, offset: e.time, size: b.len() as u32, data: b.as_ptr() };
                ysfx_send_midi(fx, &ev);
            }
            let ins = [i.l.as_ptr(), i.r.as_ptr()];
            let outs = [o.l.as_mut_ptr(), o.r.as_mut_ptr()];
            ysfx_process_float(fx, ins.as_ptr(), outs.as_ptr(), 2, 2, n as u32);
            let mut ev = ysfx_midi_event_t { bus: 0, offset: 0, size: 0, data: std::ptr::null() };
            let mut out = midi_out;
            while ysfx_receive_midi(fx, &mut ev) {
                if ev.bus != 0 || ev.data.is_null() {
                    continue;
                }
                let data = std::slice::from_raw_parts(ev.data, ev.size as usize);
                let time = ev.offset.min(n.saturating_sub(1) as u32);
                if let (Some(out), Some(e)) = (out.as_deref_mut(), MidiEvent::new(time, data)) {
                    out.push(e);
                }
            }
        }
    }
}

impl Processor for JsfxProcessor {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.run(n, inputs, outputs, &[], None);
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.run(n, inputs, outputs, midi_in, Some(midi_out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gfx_menus() {
        let items = parse_menu("First|!Checked|#Disabled||>Sub|Inner|<Last inner|After");
        let names: Vec<String> = items
            .iter()
            .map(|i| match i {
                MenuItem::Item { id, name, checked, enabled } => format!("{id}:{name}:{checked}:{enabled}"),
                MenuItem::Separator => "-".into(),
                MenuItem::Sub { name, items, .. } => format!("{name}[{}]", items.len()),
            })
            .collect();
        assert_eq!(names, ["1:First:false:true", "2:Checked:true:true", "3:Disabled:false:false", "-", "Sub[2]", "6:After:false:true"]);
    }

    #[test]
    fn state_blob_round_trips() {
        let mut sliders = [ysfx_state_slider_t { index: 3, value: -6.5 }, ysfx_state_slider_t { index: 10, value: 1.0 }];
        let mut data = b"hello".to_vec();
        let st = ysfx_state_t { sliders: sliders.as_mut_ptr(), slider_count: 2, data: data.as_mut_ptr(), data_size: 5 };
        let (s, d) = unpack_state(&pack_state(&st)).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].index, s[0].value), (3, -6.5));
        assert_eq!(d, b"hello");
        assert!(unpack_state(b"NPJS\x01").is_err());
    }
}
