//! Write nodeplayer plugins in Rust.
//!
//! A plugin is a `cdylib` that implements [`Node`] for its DSP node types and optionally
//! [`Extension`] for app-wide features, then exports them with [`export_plugin!`]:
//!
//! ```ignore
//! nodeplayer_plugin::export_plugin! {
//!     name: "My plugin",
//!     version: "0.1.0",
//!     author: "me",
//!     nodes: [MyNode],
//!     extension: MyExtension,
//! }
//! ```
//!
//! Build it with `cargo build --release` and copy the `.so` to
//! `~/.local/share/nodeplayer/plugins/`. The raw interface is in [`abi`] (and
//! `include/nodeplayer_plugin.h` for C).

pub mod abi;

use std::cell::UnsafeCell;
use std::ffi::{CString, c_char, c_void};
use std::marker::PhantomData;
use std::ops::RangeInclusive;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::*;

/// Runs plugin code so a panic can't unwind into the host.
fn guard<R>(what: &str, default: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("nodeplayer plugin: panic in {what}");
            default
        }
    }
}

fn leak_cstr(s: &str) -> *const c_char {
    Box::leak(CString::new(s.replace('\0', "")).unwrap().into_boxed_c_str()).as_ptr()
}

unsafe extern "C" fn vec_write(ctx: *mut c_void, data: *const u8, len: usize) {
    if len > 0 {
        unsafe { (*(ctx as *mut Vec<u8>)).extend_from_slice(std::slice::from_raw_parts(data, len)) };
    }
}

/// Collects what a callee writes into an [`NpBuf`].
fn collect(f: impl FnOnce(*mut NpBuf)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = NpBuf { ctx: &mut out as *mut Vec<u8> as *mut c_void, write: vec_write };
    f(&mut buf);
    out
}

// ---------------------------------------------------------------------------
// Colors

/// 0xRRGGBBAA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const WHITE: Color = Color(0xffffffff);
    pub const BLACK: Color = Color(0x000000ff);
    pub const GRAY: Color = Color(0x808080ff);
    pub const RED: Color = Color(0xff5050ff);
    pub const GREEN: Color = Color(0x50dc78ff);
    pub const BLUE: Color = Color(0x6eb4ffff);
    pub const YELLOW: Color = Color(0xf0dc5aff);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color((r as u32) << 24 | (g as u32) << 16 | (b as u32) << 8 | a as u32)
    }
}

// ---------------------------------------------------------------------------
// UI

/// Horizontal text alignment for [`Ui::text`].
#[derive(Clone, Copy, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// An area reserved with [`Ui::canvas`] and how the pointer interacts with it.
pub type Canvas = NpCanvas;

/// The host's immediate-mode UI. Call widgets every frame; they return whether the user
/// changed something.
pub struct Ui<'a> {
    raw: *const NpUi,
    _life: PhantomData<&'a mut ()>,
}

impl<'a> Ui<'a> {
    /// # Safety
    /// `raw` must be the UI handle the host passed to the current callback.
    pub unsafe fn from_raw(raw: *const NpUi) -> Self {
        Self { raw, _life: PhantomData }
    }

    fn vt(&self) -> &NpUiVtbl {
        unsafe { &*(*self.raw).vt }
    }

    fn text_styled(&mut self, text: &str, style: u32) {
        unsafe { (self.vt().label)(self.raw, NpStr::new(text), style) }
    }

    pub fn label(&mut self, text: &str) {
        self.text_styled(text, TEXT_NORMAL)
    }

    pub fn heading(&mut self, text: &str) {
        self.text_styled(text, TEXT_HEADING)
    }

    pub fn small(&mut self, text: &str) {
        self.text_styled(text, TEXT_SMALL)
    }

    pub fn weak(&mut self, text: &str) {
        self.text_styled(text, TEXT_WEAK)
    }

    pub fn monospace(&mut self, text: &str) {
        self.text_styled(text, TEXT_MONO)
    }

    pub fn colored_label(&mut self, text: &str, color: Color) {
        unsafe { (self.vt().colored_label)(self.raw, NpStr::new(text), color.0) }
    }

    pub fn button(&mut self, text: &str) -> bool {
        unsafe { (self.vt().button)(self.raw, NpStr::new(text), false) }
    }

    pub fn small_button(&mut self, text: &str) -> bool {
        unsafe { (self.vt().button)(self.raw, NpStr::new(text), true) }
    }

    pub fn selectable(&mut self, text: &str, selected: bool) -> bool {
        unsafe { (self.vt().selectable)(self.raw, NpStr::new(text), selected) }
    }

    pub fn checkbox(&mut self, text: &str, value: &mut bool) -> bool {
        unsafe { (self.vt().checkbox)(self.raw, NpStr::new(text), value) }
    }

    fn slider_flags(&mut self, text: &str, value: &mut f64, range: RangeInclusive<f64>, flags: u32) -> bool {
        unsafe { (self.vt().slider)(self.raw, NpStr::new(text), value, *range.start(), *range.end(), flags) }
    }

    pub fn slider(&mut self, text: &str, value: &mut f64, range: RangeInclusive<f64>) -> bool {
        self.slider_flags(text, value, range, 0)
    }

    pub fn slider_log(&mut self, text: &str, value: &mut f64, range: RangeInclusive<f64>) -> bool {
        self.slider_flags(text, value, range, SLIDER_LOG)
    }

    pub fn slider_int(&mut self, text: &str, value: &mut f64, range: RangeInclusive<f64>) -> bool {
        self.slider_flags(text, value, range, SLIDER_INTEGER)
    }

    pub fn drag_value(&mut self, prefix: &str, value: &mut f64, speed: f64, range: RangeInclusive<f64>, suffix: &str) -> bool {
        unsafe {
            (self.vt().drag_value)(
                self.raw,
                NpStr::new(prefix),
                value,
                speed,
                *range.start(),
                *range.end(),
                NpStr::new(suffix),
            )
        }
    }

    pub fn text_edit(&mut self, text: &mut String, hint: &str) -> bool {
        let mut buf = text.clone().into_bytes();
        let cap = (buf.len() * 2).max(256);
        let mut len = buf.len();
        buf.resize(cap, 0);
        let changed = unsafe { (self.vt().text_edit)(self.raw, buf.as_mut_ptr(), cap, &mut len, NpStr::new(hint)) };
        if changed {
            buf.truncate(len.min(cap));
            *text = String::from_utf8_lossy(&buf).into_owned();
        }
        changed
    }

    /// A drop-down; `id` keeps its state apart from other combos in the same UI.
    pub fn combo(&mut self, id: &str, selected: &mut usize, items: &[&str]) -> bool {
        let joined = items.join("\n");
        let mut sel = *selected as u32;
        let changed = unsafe { (self.vt().combo)(self.raw, NpStr::new(id), &mut sel, NpStr::new(&joined)) };
        *selected = sel as usize;
        changed
    }

    pub fn separator(&mut self) {
        unsafe { (self.vt().separator)(self.raw) }
    }

    pub fn space(&mut self, amount: f32) {
        unsafe { (self.vt().space)(self.raw, amount) }
    }

    pub fn progress(&mut self, fraction: f32, text: &str) {
        unsafe { (self.vt().progress)(self.raw, fraction, NpStr::new(text)) }
    }

    pub fn available_width(&self) -> f32 {
        unsafe { (self.vt().available_width)(self.raw) }
    }

    fn layout<F: FnOnce(&mut Ui)>(&mut self, kind: u32, title: &str, number: f64, f: F) {
        unsafe extern "C" fn tramp<F: FnOnce(&mut Ui)>(ui: *const NpUi, user: *mut c_void) {
            let f = unsafe { &mut *(user as *mut Option<F>) };
            if let Some(f) = f.take() {
                let mut ui = unsafe { Ui::from_raw(ui) };
                guard("ui", (), || f(&mut ui));
            }
        }
        let mut f = Some(f);
        unsafe {
            (self.vt().layout)(self.raw, kind, NpStr::new(title), number, tramp::<F>, &mut f as *mut Option<F> as *mut c_void)
        }
    }

    pub fn horizontal(&mut self, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_HORIZONTAL, "", 0.0, f)
    }

    pub fn vertical(&mut self, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_VERTICAL, "", 0.0, f)
    }

    pub fn group(&mut self, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_GROUP, "", 0.0, f)
    }

    pub fn collapsing(&mut self, title: &str, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_COLLAPSING, title, 0.0, f)
    }

    pub fn scroll(&mut self, max_height: f32, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_SCROLL, "", max_height as f64, f)
    }

    /// Scopes widget ids, for repeating the same widgets in a loop.
    pub fn id_scope(&mut self, id: u64, f: impl FnOnce(&mut Ui)) {
        self.layout(LAYOUT_ID_SCOPE, "", id as f64, f)
    }

    /// Reserves a `w`×`h` drawing area; paint into it with the drawing methods.
    pub fn canvas(&mut self, w: f32, h: f32) -> Canvas {
        let mut c = Canvas::default();
        unsafe { (self.vt().canvas)(self.raw, w, h, &mut c) };
        c
    }

    fn paint(&mut self, kind: u32, (x1, y1, x2, y2): (f32, f32, f32, f32), size: f32, color: Color, align: u32, text: &str) {
        let cmd = NpPaint { kind, x1, y1, x2, y2, size, rgba: color.0, align, text: NpStr::new(text) };
        unsafe { (self.vt().paint)(self.raw, &cmd) }
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color) {
        self.paint(PAINT_LINE, (x1, y1, x2, y2), width, color, 0, "")
    }

    pub fn rect(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color) {
        self.paint(PAINT_RECT, (x1, y1, x2, y2), 0.0, color, 0, "")
    }

    pub fn rect_stroke(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color) {
        self.paint(PAINT_RECT_STROKE, (x1, y1, x2, y2), width, color, 0, "")
    }

    pub fn circle(&mut self, x: f32, y: f32, radius: f32, color: Color) {
        self.paint(PAINT_CIRCLE, (x, y, 0.0, 0.0), radius, color, 0, "")
    }

    pub fn circle_stroke(&mut self, x: f32, y: f32, radius: f32, color: Color) {
        self.paint(PAINT_CIRCLE_STROKE, (x, y, 0.0, 0.0), radius, color, 0, "")
    }

    pub fn text(&mut self, x: f32, y: f32, size: f32, color: Color, align: Align, text: &str) {
        let a = match align {
            Align::Left => ALIGN_LEFT,
            Align::Center => ALIGN_CENTER,
            Align::Right => ALIGN_RIGHT,
        };
        self.paint(PAINT_TEXT, (x, y, 0.0, 0.0), size, color, a, text)
    }

    /// Keeps the UI repainting, for animations.
    pub fn request_repaint(&mut self) {
        unsafe { (self.vt().request_repaint)(self.raw) }
    }
}

// ---------------------------------------------------------------------------
// Host

#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    pub has_track: bool,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub track_index: Option<usize>,
    pub library_len: usize,
}

/// Identifies a menu item added with [`Host::add_menu_item`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MenuId(pub u32);

/// The player, as seen by an extension. Only usable on the UI thread (in callbacks).
/// Commands take effect right after the current callback returns.
#[derive(Clone, Copy)]
pub struct Host {
    raw: *const NpHost,
}

impl Host {
    /// # Safety
    /// `raw` must be the host handle passed to the extension's `create`.
    pub unsafe fn from_raw(raw: *const NpHost) -> Self {
        Self { raw }
    }

    fn h(&self) -> &NpHost {
        unsafe { &*self.raw }
    }

    fn text(&self, f: impl FnOnce(*mut NpBuf)) -> String {
        String::from_utf8_lossy(&collect(f)).into_owned()
    }

    pub fn log(&self, msg: &str) {
        unsafe { (self.h().log)(self.raw, LOG_INFO, NpStr::new(msg)) }
    }

    pub fn warn(&self, msg: &str) {
        unsafe { (self.h().log)(self.raw, LOG_WARN, NpStr::new(msg)) }
    }

    pub fn error(&self, msg: &str) {
        unsafe { (self.h().log)(self.raw, LOG_ERROR, NpStr::new(msg)) }
    }

    pub fn status(&self) -> Status {
        let mut s = NpStatus::default();
        unsafe { (self.h().status)(self.raw, &mut s) };
        Status {
            has_track: s.has_track,
            playing: s.playing,
            position: s.position,
            duration: s.duration,
            track_index: (s.track_index >= 0).then_some(s.track_index as usize),
            library_len: s.library_len as usize,
        }
    }

    fn track_field(&self, field: u32) -> Option<String> {
        let t = self.text(|b| unsafe { (self.h().track_text)(self.raw, field, b) });
        (!t.is_empty()).then_some(t)
    }

    pub fn track_path(&self) -> Option<String> {
        self.track_field(TRACK_PATH)
    }

    /// The tag title, or the file name without extension.
    pub fn track_title(&self) -> Option<String> {
        self.track_field(TRACK_TITLE)
    }

    pub fn track_artist(&self) -> Option<String> {
        self.track_field(TRACK_ARTIST)
    }

    pub fn track_album(&self) -> Option<String> {
        self.track_field(TRACK_ALBUM)
    }

    pub fn library_path(&self, index: usize) -> Option<String> {
        let mut ok = false;
        let t = self.text(|b| ok = unsafe { (self.h().library_path)(self.raw, index as u32, b) });
        ok.then_some(t)
    }

    pub fn library_root(&self) -> Option<String> {
        let mut ok = false;
        let t = self.text(|b| ok = unsafe { (self.h().library_root)(self.raw, b) });
        ok.then_some(t)
    }

    fn command(&self, cmd: u32, number: f64, text: &str) {
        unsafe { (self.h().command)(self.raw, cmd, number, NpStr::new(text)) };
    }

    pub fn play(&self) {
        self.command(CMD_PLAY, 0.0, "")
    }

    pub fn pause(&self) {
        self.command(CMD_PAUSE, 0.0, "")
    }

    pub fn toggle(&self) {
        self.command(CMD_TOGGLE, 0.0, "")
    }

    pub fn stop(&self) {
        self.command(CMD_STOP, 0.0, "")
    }

    pub fn next(&self) {
        self.command(CMD_NEXT, 0.0, "")
    }

    pub fn previous(&self) {
        self.command(CMD_PREVIOUS, 0.0, "")
    }

    pub fn seek(&self, seconds: f64) {
        self.command(CMD_SEEK, seconds, "")
    }

    pub fn load_file(&self, path: &str) {
        self.command(CMD_LOAD_FILE, 0.0, path)
    }

    pub fn play_track(&self, index: usize) {
        self.command(CMD_PLAY_TRACK, index as f64, "")
    }

    pub fn open_folder(&self, path: &str) {
        self.command(CMD_OPEN_FOLDER, 0.0, path)
    }

    pub fn load_preset(&self, name: &str) {
        self.command(CMD_LOAD_PRESET, 0.0, name)
    }

    pub fn save_preset(&self, name: &str) {
        self.command(CMD_SAVE_PRESET, 0.0, name)
    }

    /// Sets every Output node's volume.
    pub fn set_volume_db(&self, db: f64) {
        self.command(CMD_SET_VOLUME, db, "")
    }

    /// Adds an entry to the ⚙ menu in the transport bar.
    pub fn add_menu_item(&self, label: &str) -> MenuId {
        MenuId(unsafe { (self.h().add_menu_item)(self.raw, NpStr::new(label)) })
    }

    pub fn request_repaint(&self) {
        unsafe { (self.h().request_repaint)(self.raw) }
    }

    /// A folder for this plugin's own files.
    pub fn data_dir(&self) -> PathBuf {
        PathBuf::from(self.text(|b| unsafe { (self.h().data_dir)(self.raw, b) }))
    }
}

// ---------------------------------------------------------------------------
// Parameters

#[derive(Clone, Copy, Debug)]
pub struct Param {
    pub id: u32,
    pub name: &'static str,
    pub unit: &'static str,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub steps: u32,
    pub log: bool,
    pub hidden: bool,
}

impl Param {
    pub const fn new(id: u32, name: &'static str, min: f64, max: f64, default: f64) -> Self {
        Self { id, name, unit: "", min, max, default, steps: 0, log: false, hidden: false }
    }

    pub const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }

    pub const fn log(mut self) -> Self {
        self.log = true;
        self
    }

    /// Whole-number steps between min and max.
    pub const fn steps(mut self, steps: u32) -> Self {
        self.steps = steps;
        self
    }

    pub const fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

/// Current parameter values, readable from both threads.
pub struct Params {
    ids: Vec<u32>,
    values: Vec<AtomicU64>,
    info: Vec<Param>,
}

impl Params {
    fn new(info: Vec<Param>) -> Self {
        Self {
            ids: info.iter().map(|p| p.id).collect(),
            values: info.iter().map(|p| AtomicU64::new(p.default.to_bits())).collect(),
            info,
        }
    }

    fn slot(&self, id: u32) -> Option<usize> {
        self.ids.iter().position(|i| *i == id)
    }

    pub fn get(&self, id: u32) -> f64 {
        self.slot(id).map_or(0.0, |i| f64::from_bits(self.values[i].load(Ordering::Relaxed)))
    }

    /// Changing a value from the UI takes effect on the next processed block.
    pub fn set(&self, id: u32, value: f64) {
        if let Some(i) = self.slot(id) {
            let p = &self.info[i];
            let v = value.clamp(p.min.min(p.max), p.max.max(p.min));
            self.values[i].store(v.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn info(&self) -> &[Param] {
        &self.info
    }
}

// ---------------------------------------------------------------------------
// Nodes

/// A DSP node type.
pub trait Node: Sized + Send + 'static {
    /// Unique within the plugin; saved graphs refer to it, so keep it stable.
    const ID: &'static str;
    const NAME: &'static str;
    /// "|"-separated, like "Fx|Distortion".
    const CATEGORY: &'static str = "Fx";
    /// Draws its own body with [`Node::ui`] instead of generic parameter sliders.
    const HAS_UI: bool = false;
    /// MIDI pins: [`MIDI_IN`](abi::MIDI_IN) and/or [`MIDI_OUT`](abi::MIDI_OUT).
    const MIDI: u32 = 0;
    /// Data the audio thread shares with the UI (meters and the like), e.g. atomics.
    type Shared: Default + Send + Sync + 'static;

    fn params() -> Vec<Param>;
    fn new(sample_rate: f32, max_block: usize) -> Self;
    /// Audio thread.
    fn process(&mut self, input: [&[f32]; 2], output: [&mut [f32]; 2], params: &Params, shared: &Self::Shared);
    /// Audio thread, before [`Node::process`] in every block, for nodes with [`Node::MIDI`]:
    /// the block's incoming events, sorted by time, and where outgoing ones go.
    fn midi(&mut self, _input: &[MidiEvent], _out: &mut MidiOut, _params: &Params, _shared: &Self::Shared) {}
    /// UI thread; only called when [`Node::HAS_UI`] is set.
    fn ui(_ui: &mut Ui, _params: &Params, _shared: &Self::Shared) {}
}

struct NodeInstance<T: Node> {
    /// Only touched by the audio thread.
    dsp: UnsafeCell<T>,
    params: Params,
    shared: T::Shared,
}

unsafe extern "C" fn node_create<T: Node>(sample_rate: f32, max_block: u32) -> *mut c_void {
    guard("create", std::ptr::null_mut(), || {
        let inst = NodeInstance::<T> {
            dsp: UnsafeCell::new(T::new(sample_rate, max_block as usize)),
            params: Params::new(T::params()),
            shared: T::Shared::default(),
        };
        Box::into_raw(Box::new(inst)) as *mut c_void
    })
}

unsafe extern "C" fn node_destroy<T: Node>(inst: *mut c_void) {
    guard("destroy", (), || drop(unsafe { Box::from_raw(inst as *mut NodeInstance<T>) }))
}

unsafe extern "C" fn node_process<T: Node>(
    inst: *mut c_void,
    in_l: *const f32,
    in_r: *const f32,
    out_l: *mut f32,
    out_r: *mut f32,
    n: u32,
) {
    let inst = unsafe { &*(inst as *const NodeInstance<T>) };
    let n = n as usize;
    let (il, ir, ol, or) = unsafe {
        (
            std::slice::from_raw_parts(in_l, n),
            std::slice::from_raw_parts(in_r, n),
            std::slice::from_raw_parts_mut(out_l, n),
            std::slice::from_raw_parts_mut(out_r, n),
        )
    };
    guard("process", (), || unsafe { &mut *inst.dsp.get() }.process([il, ir], [ol, or], &inst.params, &inst.shared))
}

unsafe extern "C" fn node_set_param<T: Node>(inst: *mut c_void, id: u32, value: f64) {
    unsafe { &*(inst as *const NodeInstance<T>) }.params.set(id, value)
}

unsafe extern "C" fn node_get_param<T: Node>(inst: *mut c_void, id: u32) -> f64 {
    unsafe { &*(inst as *const NodeInstance<T>) }.params.get(id)
}

unsafe extern "C" fn node_draw<T: Node>(inst: *mut c_void, ui: *const NpUi) {
    let inst = unsafe { &*(inst as *const NodeInstance<T>) };
    let mut ui = unsafe { Ui::from_raw(ui) };
    guard("ui", (), || T::ui(&mut ui, &inst.params, &inst.shared))
}

/// The descriptor of a node type; used by [`export_plugin!`].
unsafe extern "C" fn node_midi<T: Node>(inst: *mut c_void, input: *const NpMidiEvent, count: u32, out: *const NpMidiOut) {
    let inst = unsafe { &*(inst as *const NodeInstance<T>) };
    let events = if input.is_null() || count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(input as *const MidiEvent, count as usize) }
    };
    let mut out = MidiOut(out);
    guard("midi", (), || unsafe { &mut *inst.dsp.get() }.midi(events, &mut out, &inst.params, &inst.shared))
}

/// A short MIDI message at frame `time` of the block.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiEvent(pub NpMidiEvent);

impl MidiEvent {
    pub fn new(time: u32, bytes: &[u8]) -> Self {
        let len = bytes.len().clamp(1, 3);
        let mut data = [0; 3];
        data[..len.min(bytes.len())].copy_from_slice(&bytes[..len.min(bytes.len())]);
        Self(NpMidiEvent { time, data, len: len as u8 })
    }

    pub fn note_on(time: u32, channel: u8, key: u8, velocity: u8) -> Self {
        Self::new(time, &[0x90 | (channel & 15), key & 127, velocity & 127])
    }

    pub fn note_off(time: u32, channel: u8, key: u8) -> Self {
        Self::new(time, &[0x80 | (channel & 15), key & 127, 0])
    }

    pub fn time(&self) -> u32 {
        self.0.time
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0.data[..(self.0.len as usize).min(3)]
    }

    pub fn status(&self) -> u8 {
        self.0.data[0] & 0xf0
    }

    pub fn channel(&self) -> u8 {
        self.0.data[0] & 0x0f
    }

    /// `(key, velocity)` of a note on (a note on with velocity 0 counts as a note off).
    pub fn as_note_on(&self) -> Option<(u8, u8)> {
        let [_, k, v] = self.0.data;
        (self.status() == 0x90 && v > 0).then_some((k, v))
    }

    /// The key of a note off.
    pub fn as_note_off(&self) -> Option<u8> {
        let [_, k, v] = self.0.data;
        (self.status() == 0x80 || self.status() == 0x90 && v == 0).then_some(k)
    }
}

/// A node's MIDI output for the current block.
pub struct MidiOut(*const NpMidiOut);

impl MidiOut {
    /// Sends an event; false when the block's buffer is full. Push in time order.
    pub fn push(&mut self, e: MidiEvent) -> bool {
        if self.0.is_null() {
            return false;
        }
        unsafe { ((*self.0).push)((*self.0).ctx, &e.0) }
    }
}

pub fn node_desc<T: Node>() -> NpNodeDesc {
    let params: Vec<NpParamInfo> = T::params()
        .iter()
        .map(|p| NpParamInfo {
            id: p.id,
            name: leak_cstr(p.name),
            unit: leak_cstr(p.unit),
            min: p.min,
            max: p.max,
            default: p.default,
            steps: p.steps,
            flags: if p.log { PARAM_LOG } else { 0 } | if p.hidden { PARAM_HIDDEN } else { 0 },
        })
        .collect();
    let param_count = params.len() as u32;
    NpNodeDesc {
        id: leak_cstr(T::ID),
        name: leak_cstr(T::NAME),
        category: leak_cstr(T::CATEGORY),
        params: Box::leak(params.into_boxed_slice()).as_ptr(),
        param_count,
        create: node_create::<T>,
        destroy: node_destroy::<T>,
        process: node_process::<T>,
        set_param: node_set_param::<T>,
        get_param: node_get_param::<T>,
        draw: if T::HAS_UI { Some(node_draw::<T>) } else { None },
        save_state: None,
        load_state: None,
        latency: None,
        midi: T::MIDI,
        process_midi: if T::MIDI != 0 { Some(node_midi::<T>) } else { None },
    }
}

// ---------------------------------------------------------------------------
// Extensions

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    TrackChanged,
    PlaybackStarted,
    PlaybackPaused,
    /// A track played to its end (before the next one starts).
    TrackEnded,
    LibraryChanged,
    Other(u32),
}

/// App-wide features: a side panel, menu items, reacting to playback.
pub trait Extension: Sized + 'static {
    /// Title of the side panel, if the extension draws one.
    const PANEL: Option<&'static str> = None;

    fn new(host: Host) -> Self;
    /// Every UI frame.
    fn idle(&mut self) {}
    fn event(&mut self, _event: Event) {}
    fn panel(&mut self, _ui: &mut Ui) {}
    fn menu(&mut self, _id: MenuId) {}
}

unsafe extern "C" fn ext_create<T: Extension>(host: *const NpHost) -> *mut c_void {
    guard("create", std::ptr::null_mut(), || {
        Box::into_raw(Box::new(T::new(unsafe { Host::from_raw(host) }))) as *mut c_void
    })
}

unsafe extern "C" fn ext_destroy<T: Extension>(ext: *mut c_void) {
    guard("destroy", (), || drop(unsafe { Box::from_raw(ext as *mut T) }))
}

unsafe extern "C" fn ext_idle<T: Extension>(ext: *mut c_void) {
    guard("idle", (), || unsafe { &mut *(ext as *mut T) }.idle())
}

unsafe extern "C" fn ext_event<T: Extension>(ext: *mut c_void, event: *const NpEvent) {
    let e = match unsafe { (*event).kind } {
        EVENT_TRACK_CHANGED => Event::TrackChanged,
        EVENT_PLAYBACK_STARTED => Event::PlaybackStarted,
        EVENT_PLAYBACK_PAUSED => Event::PlaybackPaused,
        EVENT_TRACK_ENDED => Event::TrackEnded,
        EVENT_LIBRARY_CHANGED => Event::LibraryChanged,
        k => Event::Other(k),
    };
    guard("event", (), || unsafe { &mut *(ext as *mut T) }.event(e))
}

unsafe extern "C" fn ext_panel<T: Extension>(ext: *mut c_void, ui: *const NpUi) {
    let mut ui = unsafe { Ui::from_raw(ui) };
    guard("panel", (), || unsafe { &mut *(ext as *mut T) }.panel(&mut ui))
}

unsafe extern "C" fn ext_menu<T: Extension>(ext: *mut c_void, id: u32) {
    guard("menu", (), || unsafe { &mut *(ext as *mut T) }.menu(MenuId(id)))
}

/// The descriptor of an extension; used by [`export_plugin!`].
pub fn extension_desc<T: Extension>() -> NpExtension {
    NpExtension {
        create: ext_create::<T>,
        destroy: ext_destroy::<T>,
        idle: Some(ext_idle::<T>),
        on_event: Some(ext_event::<T>),
        panel_title: T::PANEL.map_or(std::ptr::null(), leak_cstr),
        draw_panel: T::PANEL.map(|_| ext_panel::<T> as unsafe extern "C" fn(*mut c_void, *const NpUi)),
        on_menu: Some(ext_menu::<T>),
    }
}

/// A plugin descriptor kept alive for the life of the process.
pub struct DescPtr(pub *const NpPluginDesc);

unsafe impl Send for DescPtr {}
unsafe impl Sync for DescPtr {}

/// Builds the plugin descriptor; used by [`export_plugin!`].
pub fn plugin_desc(name: &str, version: &str, author: &str, nodes: Vec<NpNodeDesc>, extension: Option<NpExtension>) -> DescPtr {
    let node_count = nodes.len() as u32;
    let desc = NpPluginDesc {
        abi_version: ABI_VERSION,
        name: leak_cstr(name),
        version: leak_cstr(version),
        author: leak_cstr(author),
        nodes: Box::leak(nodes.into_boxed_slice()).as_ptr(),
        node_count,
        extension: extension.map_or(std::ptr::null(), |e| Box::leak(Box::new(e)) as *const NpExtension),
    };
    DescPtr(Box::leak(Box::new(desc)))
}

/// Exports the plugin's entry point.
#[macro_export]
macro_rules! export_plugin {
    (
        name: $name:expr,
        version: $version:expr,
        author: $author:expr,
        nodes: [$($node:ty),* $(,)?]
        $(, extension: $ext:ty)?
        $(,)?
    ) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn nodeplayer_plugin_entry() -> *const $crate::abi::NpPluginDesc {
            static DESC: ::std::sync::OnceLock<$crate::DescPtr> = ::std::sync::OnceLock::new();
            DESC.get_or_init(|| {
                let nodes = ::std::vec![$($crate::node_desc::<$node>()),*];
                let ext: ::std::option::Option<$crate::abi::NpExtension> = None $(.or(Some($crate::extension_desc::<$ext>())))?;
                $crate::plugin_desc($name, $version, $author, nodes, ext)
            })
            .0
        }
    };
}
