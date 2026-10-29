//! The C ABI between nodeplayer and its plugins. `include/nodeplayer_plugin.h` declares the
//! same structures for C and C++.
//!
//! A plugin is a shared library exporting
//! `const NpPluginDesc *nodeplayer_plugin_entry(void)`. It may describe DSP node types, which
//! appear in the graph's plugin list, and one app extension, which nodeplayer creates at
//! startup and which can draw a side panel, add menu items, react to playback events and
//! control the player through [`NpHost`].
//!
//! Threads: a node's `process` and `set_param` run on the audio thread; everything else
//! (including every [`NpHost`] and [`NpUi`] call) happens on the UI thread.

use std::ffi::{c_char, c_void};

/// The version plugins built with this SDK declare. Version 2 added MIDI to nodes; the host
/// still loads version 1 plugins.
pub const ABI_VERSION: u32 = 2;
/// The oldest version the host loads.
pub const MIN_ABI_VERSION: u32 = 1;

pub const ENTRY_SYMBOL: &str = "nodeplayer_plugin_entry";

pub type NpEntry = unsafe extern "C" fn() -> *const NpPluginDesc;

/// UTF-8 text that isn't NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NpStr {
    pub ptr: *const u8,
    pub len: usize,
}

impl NpStr {
    pub fn new(s: &str) -> Self {
        Self { ptr: s.as_ptr(), len: s.len() }
    }

    /// # Safety
    /// `ptr` must point at `len` readable bytes (or be null).
    pub unsafe fn as_str<'a>(self) -> std::borrow::Cow<'a, str> {
        if self.ptr.is_null() || self.len == 0 {
            return std::borrow::Cow::Borrowed("");
        }
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(self.ptr, self.len) })
    }
}

/// A sink for bytes the callee produces (state, strings): `write` may be called repeatedly.
#[repr(C)]
pub struct NpBuf {
    pub ctx: *mut c_void,
    pub write: unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize),
}

// ---------------------------------------------------------------------------
// Plugin description

#[repr(C)]
pub struct NpPluginDesc {
    pub abi_version: u32,
    pub name: *const c_char,
    pub version: *const c_char,
    pub author: *const c_char,
    pub nodes: *const NpNodeDesc,
    pub node_count: u32,
    /// Null when the plugin has no app extension.
    pub extension: *const NpExtension,
}

/// The parameter is shown with a logarithmic slider.
pub const PARAM_LOG: u32 = 1 << 0;
/// The parameter isn't shown in generic UIs.
pub const PARAM_HIDDEN: u32 = 1 << 1;

#[repr(C)]
pub struct NpParamInfo {
    pub id: u32,
    pub name: *const c_char,
    pub unit: *const c_char,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// 0 for continuous values, else the number of steps between `min` and `max`.
    pub steps: u32,
    pub flags: u32,
}

/// A DSP node type. Instances are opaque pointers returned by `create`.
#[repr(C)]
pub struct NpNodeDesc {
    /// Unique within the plugin; saved graphs refer to it.
    pub id: *const c_char,
    pub name: *const c_char,
    /// "|"-separated, like "Fx|Distortion".
    pub category: *const c_char,
    pub params: *const NpParamInfo,
    pub param_count: u32,
    pub create: unsafe extern "C" fn(sample_rate: f32, max_block: u32) -> *mut c_void,
    /// Called once the audio thread no longer uses the instance.
    pub destroy: unsafe extern "C" fn(inst: *mut c_void),
    /// Audio thread. Stereo in, stereo out; `n` is at most `max_block`.
    pub process: unsafe extern "C" fn(
        inst: *mut c_void,
        in_l: *const f32,
        in_r: *const f32,
        out_l: *mut f32,
        out_r: *mut f32,
        n: u32,
    ),
    /// Audio thread, right before `process`.
    pub set_param: unsafe extern "C" fn(inst: *mut c_void, id: u32, value: f64),
    /// UI thread: the value to show.
    pub get_param: unsafe extern "C" fn(inst: *mut c_void, id: u32) -> f64,
    /// UI thread: draws the node's body. Null for generic parameter sliders.
    pub draw: Option<unsafe extern "C" fn(inst: *mut c_void, ui: *const NpUi)>,
    /// UI thread: extra state beyond parameter values (which the host saves itself).
    pub save_state: Option<unsafe extern "C" fn(inst: *mut c_void, out: *mut NpBuf)>,
    pub load_state: Option<unsafe extern "C" fn(inst: *mut c_void, data: *const u8, len: usize) -> bool>,
    pub latency: Option<unsafe extern "C" fn(inst: *mut c_void) -> u32>,

    // ABI 2. A version 1 plugin's descriptors end here.
    /// `MIDI_IN` and/or `MIDI_OUT`: the node gets a MIDI input / output pin.
    pub midi: u32,
    /// Audio thread, before `process` in every block (also without input events, so nodes
    /// can generate MIDI). Events pushed to `out` leave through the node's MIDI output.
    pub process_midi:
        Option<unsafe extern "C" fn(inst: *mut c_void, input: *const NpMidiEvent, count: u32, out: *const NpMidiOut)>,
}

pub const MIDI_IN: u32 = 1 << 0;
pub const MIDI_OUT: u32 = 1 << 1;

/// A short MIDI message (no sysex) at frame `time` of the block.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NpMidiEvent {
    pub time: u32,
    pub data: [u8; 3],
    /// Bytes used in `data` (1 to 3).
    pub len: u8,
}

/// Where a node puts its MIDI output; `push` returns false when the block's buffer is full.
#[repr(C)]
pub struct NpMidiOut {
    pub ctx: *mut c_void,
    pub push: unsafe extern "C" fn(ctx: *mut c_void, event: *const NpMidiEvent) -> bool,
}

/// The app extension; one per plugin, created at startup.
#[repr(C)]
pub struct NpExtension {
    pub create: unsafe extern "C" fn(host: *const NpHost) -> *mut c_void,
    pub destroy: unsafe extern "C" fn(ext: *mut c_void),
    /// Every UI frame (also while the window is minimized, at a slower rate).
    pub idle: Option<unsafe extern "C" fn(ext: *mut c_void)>,
    pub on_event: Option<unsafe extern "C" fn(ext: *mut c_void, event: *const NpEvent)>,
    /// Title of the extension's side panel; null for none.
    pub panel_title: *const c_char,
    pub draw_panel: Option<unsafe extern "C" fn(ext: *mut c_void, ui: *const NpUi)>,
    /// A menu item added with [`NpHost::add_menu_item`] was chosen.
    pub on_menu: Option<unsafe extern "C" fn(ext: *mut c_void, id: u32)>,
}

// ---------------------------------------------------------------------------
// Events

pub const EVENT_TRACK_CHANGED: u32 = 1;
pub const EVENT_PLAYBACK_STARTED: u32 = 2;
pub const EVENT_PLAYBACK_PAUSED: u32 = 3;
/// A track played to its end (before the next one starts).
pub const EVENT_TRACK_ENDED: u32 = 4;
pub const EVENT_LIBRARY_CHANGED: u32 = 5;

#[repr(C)]
pub struct NpEvent {
    pub kind: u32,
}

// ---------------------------------------------------------------------------
// Host

#[repr(C)]
#[derive(Default)]
pub struct NpStatus {
    pub has_track: bool,
    pub playing: bool,
    /// Seconds.
    pub position: f64,
    /// Seconds; 0 when unknown.
    pub duration: f64,
    /// Index of the current track in the library list, or -1.
    pub track_index: i32,
    pub library_len: u32,
}

pub const TRACK_PATH: u32 = 0;
pub const TRACK_TITLE: u32 = 1;
pub const TRACK_ARTIST: u32 = 2;
pub const TRACK_ALBUM: u32 = 3;

pub const CMD_PLAY: u32 = 1;
pub const CMD_PAUSE: u32 = 2;
pub const CMD_TOGGLE: u32 = 3;
pub const CMD_STOP: u32 = 4;
pub const CMD_NEXT: u32 = 5;
pub const CMD_PREVIOUS: u32 = 6;
/// `number`: position in seconds.
pub const CMD_SEEK: u32 = 7;
/// `text`: file path.
pub const CMD_LOAD_FILE: u32 = 8;
/// `number`: library index.
pub const CMD_PLAY_TRACK: u32 = 9;
/// `text`: folder to list in the library.
pub const CMD_OPEN_FOLDER: u32 = 10;
/// `text`: preset name.
pub const CMD_LOAD_PRESET: u32 = 11;
pub const CMD_SAVE_PRESET: u32 = 12;
/// `number`: output volume in dB (every Output node).
pub const CMD_SET_VOLUME: u32 = 13;

pub const LOG_INFO: u32 = 0;
pub const LOG_WARN: u32 = 1;
pub const LOG_ERROR: u32 = 2;

/// Services the host offers an extension. Commands are carried out right after the
/// extension's callback returns.
#[repr(C)]
pub struct NpHost {
    pub abi_version: u32,
    pub host_data: *mut c_void,
    pub log: unsafe extern "C" fn(host: *const NpHost, level: u32, msg: NpStr),
    pub status: unsafe extern "C" fn(host: *const NpHost, out: *mut NpStatus),
    /// Text about the current track (`TRACK_*`); writes nothing when there's none.
    pub track_text: unsafe extern "C" fn(host: *const NpHost, field: u32, out: *mut NpBuf),
    /// Path of a library track; false when `index` is out of range.
    pub library_path: unsafe extern "C" fn(host: *const NpHost, index: u32, out: *mut NpBuf) -> bool,
    pub library_root: unsafe extern "C" fn(host: *const NpHost, out: *mut NpBuf) -> bool,
    /// One of `CMD_*`; false for an unknown command.
    pub command: unsafe extern "C" fn(host: *const NpHost, cmd: u32, number: f64, text: NpStr) -> bool,
    /// Adds an entry to the ⚙ menu; returns the id passed to `on_menu`.
    pub add_menu_item: unsafe extern "C" fn(host: *const NpHost, label: NpStr) -> u32,
    pub request_repaint: unsafe extern "C" fn(host: *const NpHost),
    /// A folder for the plugin's own files (created on demand).
    pub data_dir: unsafe extern "C" fn(host: *const NpHost, out: *mut NpBuf),
}

// ---------------------------------------------------------------------------
// UI: an immediate-mode interface over the host's toolkit. Nested layouts take a callback
// that receives the inner UI.

pub type NpUiFn = unsafe extern "C" fn(ui: *const NpUi, user: *mut c_void);

#[repr(C)]
pub struct NpUi {
    pub ctx: *mut c_void,
    pub vt: *const NpUiVtbl,
}

pub const TEXT_NORMAL: u32 = 0;
pub const TEXT_HEADING: u32 = 1;
pub const TEXT_SMALL: u32 = 2;
pub const TEXT_WEAK: u32 = 3;
pub const TEXT_MONO: u32 = 4;

pub const SLIDER_LOG: u32 = 1 << 0;
pub const SLIDER_INTEGER: u32 = 1 << 1;

pub const LAYOUT_HORIZONTAL: u32 = 0;
pub const LAYOUT_VERTICAL: u32 = 1;
/// A framed group.
pub const LAYOUT_GROUP: u32 = 2;
/// A collapsible section titled `title`.
pub const LAYOUT_COLLAPSING: u32 = 3;
/// A vertical scroll area at most `number` high.
pub const LAYOUT_SCROLL: u32 = 4;
/// Same layout, with ids scoped by `number` (for repeated widgets in loops).
pub const LAYOUT_ID_SCOPE: u32 = 5;

/// Interaction with an area reserved by `canvas`; coordinates are in UI points.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct NpCanvas {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub hovered: bool,
    pub clicked: bool,
    pub double_clicked: bool,
    pub dragged: bool,
    pub drag_dx: f32,
    pub drag_dy: f32,
    pub pointer_x: f32,
    pub pointer_y: f32,
    /// Mouse wheel over the canvas this frame (the graph view doesn't pan meanwhile).
    pub scroll_y: f32,
}

pub const PAINT_LINE: u32 = 0;
pub const PAINT_RECT: u32 = 1;
pub const PAINT_RECT_STROKE: u32 = 2;
pub const PAINT_CIRCLE: u32 = 3;
pub const PAINT_CIRCLE_STROKE: u32 = 4;
pub const PAINT_TEXT: u32 = 5;

pub const ALIGN_LEFT: u32 = 0;
pub const ALIGN_CENTER: u32 = 1;
pub const ALIGN_RIGHT: u32 = 2;

/// One drawing command, clipped to the last canvas. Colors are 0xRRGGBBAA.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NpPaint {
    pub kind: u32,
    /// Line ends, rectangle corners, or circle center / text anchor in (x1, y1).
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    /// Line / outline width, circle radius, or text size.
    pub size: f32,
    pub rgba: u32,
    pub align: u32,
    pub text: NpStr,
}

#[repr(C)]
pub struct NpUiVtbl {
    pub label: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, style: u32),
    pub colored_label: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, rgba: u32),
    pub button: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, small: bool) -> bool,
    pub selectable: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, selected: bool) -> bool,
    pub checkbox: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, value: *mut bool) -> bool,
    pub slider: unsafe extern "C" fn(ui: *const NpUi, text: NpStr, value: *mut f64, min: f64, max: f64, flags: u32) -> bool,
    pub drag_value: unsafe extern "C" fn(
        ui: *const NpUi,
        prefix: NpStr,
        value: *mut f64,
        speed: f64,
        min: f64,
        max: f64,
        suffix: NpStr,
    ) -> bool,
    /// Edits UTF-8 text in `buf` (capacity `cap`, current length `*len`).
    pub text_edit: unsafe extern "C" fn(ui: *const NpUi, buf: *mut u8, cap: usize, len: *mut usize, hint: NpStr) -> bool,
    /// `items` separated by '\n'; `id` keeps the widget's state apart from others.
    pub combo: unsafe extern "C" fn(ui: *const NpUi, id: NpStr, selected: *mut u32, items: NpStr) -> bool,
    pub separator: unsafe extern "C" fn(ui: *const NpUi),
    pub space: unsafe extern "C" fn(ui: *const NpUi, amount: f32),
    pub progress: unsafe extern "C" fn(ui: *const NpUi, fraction: f32, text: NpStr),
    pub layout: unsafe extern "C" fn(ui: *const NpUi, kind: u32, title: NpStr, number: f64, body: NpUiFn, user: *mut c_void),
    pub available_width: unsafe extern "C" fn(ui: *const NpUi) -> f32,
    /// Reserves a `w`×`h` area for drawing with `paint`, and reports interaction with it.
    pub canvas: unsafe extern "C" fn(ui: *const NpUi, w: f32, h: f32, out: *mut NpCanvas),
    pub paint: unsafe extern "C" fn(ui: *const NpUi, cmd: *const NpPaint),
    /// The UI should keep repainting (animations).
    pub request_repaint: unsafe extern "C" fn(ui: *const NpUi),
}
