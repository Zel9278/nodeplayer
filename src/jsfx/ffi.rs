//! The part of ysfx's C API (`third_party/ysfx/include/ysfx.h`) nodeplayer uses.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_void};

pub type ysfx_real = f64;

#[repr(C)]
pub struct ysfx_t {
    _private: [u8; 0],
}

#[repr(C)]
pub struct ysfx_config_t {
    _private: [u8; 0],
}

pub const YSFX_MAX_SLIDERS: u32 = 256;

pub const YSFX_LOG_INFO: i32 = 0;
pub const YSFX_LOG_WARNING: i32 = 1;
pub const YSFX_LOG_ERROR: i32 = 2;

pub type ysfx_log_reporter_t = unsafe extern "C" fn(userdata: isize, level: i32, message: *const c_char);

pub const YSFX_SECTION_GFX: u32 = 5;
pub const YSFX_SECTION_SERIALIZE: u32 = 6;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ysfx_slider_curve_t {
    pub def: ysfx_real,
    pub min: ysfx_real,
    pub max: ysfx_real,
    pub inc: ysfx_real,
    pub shape: u8,
    pub modifier: ysfx_real,
}

#[repr(C)]
pub struct ysfx_time_info_t {
    pub tempo: ysfx_real,
    pub playback_state: u32,
    pub time_position: ysfx_real,
    pub beat_position: ysfx_real,
    pub time_signature: [u32; 2],
}

pub const YSFX_PLAYBACK_PLAYING: u32 = 1;

#[repr(C)]
pub struct ysfx_state_slider_t {
    pub index: u32,
    pub value: ysfx_real,
}

#[repr(C)]
pub struct ysfx_state_t {
    pub sliders: *mut ysfx_state_slider_t,
    pub slider_count: u32,
    pub data: *mut u8,
    pub data_size: usize,
}

#[repr(C)]
pub struct ysfx_gfx_config_t {
    pub user_data: *mut c_void,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub pixel_stride: u32,
    pub pixels: *mut u8,
    pub scale_factor: ysfx_real,
    pub show_menu: Option<unsafe extern "C" fn(*mut c_void, *const c_char, i32, i32) -> i32>,
    pub set_cursor: Option<unsafe extern "C" fn(*mut c_void, i32)>,
    pub get_drop_file: Option<unsafe extern "C" fn(*mut c_void, i32) -> *const c_char>,
}

#[repr(C)]
pub struct ysfx_midi_event_t {
    pub bus: u32,
    pub offset: u32,
    pub size: u32,
    pub data: *const u8,
}

pub const YSFX_MENU_ITEM: u32 = 0;
pub const YSFX_MENU_SEPARATOR: u32 = 1;
pub const YSFX_MENU_SUB: u32 = 2;
pub const YSFX_MENU_ENDSUB: u32 = 3;
pub const YSFX_MENU_ITEM_DISABLED: u32 = 1 << 0;
pub const YSFX_MENU_ITEM_CHECKED: u32 = 1 << 1;

#[repr(C)]
pub struct ysfx_menu_insn_t {
    pub opcode: u32,
    pub id: u32,
    pub name: *const c_char,
    pub item_flags: u32,
}

#[repr(C)]
pub struct ysfx_menu_t {
    pub insns: *mut ysfx_menu_insn_t,
    pub insn_count: u32,
}

pub const YSFX_MOD_SHIFT: u32 = 1 << 0;
pub const YSFX_MOD_CTRL: u32 = 1 << 1;
pub const YSFX_MOD_ALT: u32 = 1 << 2;
pub const YSFX_MOD_SUPER: u32 = 1 << 3;

pub const YSFX_BUTTON_LEFT: u32 = 1 << 0;
pub const YSFX_BUTTON_MIDDLE: u32 = 1 << 1;
pub const YSFX_BUTTON_RIGHT: u32 = 1 << 2;

pub const YSFX_KEY_BACKSPACE: u32 = 0x08;
pub const YSFX_KEY_ESCAPE: u32 = 0x1b;
pub const YSFX_KEY_DELETE: u32 = 0x7f;
pub const YSFX_KEY_LEFT: u32 = 0xe000 + 12;
pub const YSFX_KEY_UP: u32 = 0xe000 + 13;
pub const YSFX_KEY_RIGHT: u32 = 0xe000 + 14;
pub const YSFX_KEY_DOWN: u32 = 0xe000 + 15;
pub const YSFX_KEY_PAGE_UP: u32 = 0xe000 + 16;
pub const YSFX_KEY_PAGE_DOWN: u32 = 0xe000 + 17;
pub const YSFX_KEY_HOME: u32 = 0xe000 + 18;
pub const YSFX_KEY_END: u32 = 0xe000 + 19;
pub const YSFX_KEY_INSERT: u32 = 0xe000 + 20;

unsafe extern "C" {
    pub fn ysfx_config_new() -> *mut ysfx_config_t;
    pub fn ysfx_config_free(config: *mut ysfx_config_t);
    pub fn ysfx_set_import_root(config: *mut ysfx_config_t, root: *const c_char);
    pub fn ysfx_set_data_root(config: *mut ysfx_config_t, root: *const c_char);
    pub fn ysfx_get_import_root(config: *mut ysfx_config_t) -> *const c_char;
    pub fn ysfx_guess_file_roots(config: *mut ysfx_config_t, sourcepath: *const c_char);
    pub fn ysfx_register_builtin_audio_formats(config: *mut ysfx_config_t);
    pub fn ysfx_set_log_reporter(config: *mut ysfx_config_t, reporter: Option<ysfx_log_reporter_t>);
    pub fn ysfx_set_user_data(config: *mut ysfx_config_t, userdata: isize);

    pub fn ysfx_new(config: *mut ysfx_config_t) -> *mut ysfx_t;
    pub fn ysfx_free(fx: *mut ysfx_t);
    pub fn ysfx_load_file(fx: *mut ysfx_t, filepath: *const c_char, loadopts: u32) -> bool;
    pub fn ysfx_get_name(fx: *mut ysfx_t) -> *const c_char;
    pub fn ysfx_get_author(fx: *mut ysfx_t) -> *const c_char;
    pub fn ysfx_get_tags(fx: *mut ysfx_t, dest: *mut *const c_char, destsize: u32) -> u32;
    pub fn ysfx_get_num_inputs(fx: *mut ysfx_t) -> u32;
    pub fn ysfx_get_num_outputs(fx: *mut ysfx_t) -> u32;
    pub fn ysfx_get_gfx_dim(fx: *mut ysfx_t, dim: *mut u32) -> bool;
    pub fn ysfx_has_section(fx: *mut ysfx_t, ty: u32) -> bool;

    pub fn ysfx_slider_exists(fx: *mut ysfx_t, index: u32) -> bool;
    pub fn ysfx_slider_get_name(fx: *mut ysfx_t, index: u32) -> *const c_char;
    pub fn ysfx_slider_get_curve(fx: *mut ysfx_t, index: u32, curve: *mut ysfx_slider_curve_t) -> bool;
    pub fn ysfx_slider_is_enum(fx: *mut ysfx_t, index: u32) -> bool;
    pub fn ysfx_slider_get_enum_names(fx: *mut ysfx_t, index: u32, dest: *mut *const c_char, destsize: u32) -> u32;
    pub fn ysfx_slider_is_path(fx: *mut ysfx_t, index: u32) -> bool;
    pub fn ysfx_slider_is_initially_visible(fx: *mut ysfx_t, index: u32) -> bool;
    pub fn ysfx_slider_get_value(fx: *mut ysfx_t, index: u32) -> ysfx_real;
    pub fn ysfx_slider_set_value(fx: *mut ysfx_t, index: u32, value: ysfx_real, notify: bool);
    pub fn ysfx_get_slider_visibility(fx: *mut ysfx_t, group: u8) -> u64;
    pub fn ysfx_normalized_to_ysfx_value(value: ysfx_real, curve: *const ysfx_slider_curve_t) -> ysfx_real;
    pub fn ysfx_ysfx_value_to_normalized(value: ysfx_real, curve: *const ysfx_slider_curve_t) -> ysfx_real;

    pub fn ysfx_compile(fx: *mut ysfx_t, compileopts: u32) -> bool;
    pub fn ysfx_set_block_size(fx: *mut ysfx_t, blocksize: u32);
    pub fn ysfx_set_sample_rate(fx: *mut ysfx_t, samplerate: ysfx_real);
    pub fn ysfx_init(fx: *mut ysfx_t);
    pub fn ysfx_get_pdc_delay(fx: *mut ysfx_t) -> ysfx_real;
    pub fn ysfx_set_time_info(fx: *mut ysfx_t, info: *const ysfx_time_info_t);
    pub fn ysfx_process_float(
        fx: *mut ysfx_t,
        ins: *const *const f32,
        outs: *const *mut f32,
        num_ins: u32,
        num_outs: u32,
        num_frames: u32,
    );

    pub fn ysfx_send_midi(fx: *mut ysfx_t, event: *const ysfx_midi_event_t) -> bool;
    pub fn ysfx_receive_midi(fx: *mut ysfx_t, event: *mut ysfx_midi_event_t) -> bool;
    pub fn ysfx_parse_menu(text: *const c_char) -> *mut ysfx_menu_t;
    pub fn ysfx_menu_free(menu: *mut ysfx_menu_t);

    pub fn ysfx_load_state(fx: *mut ysfx_t, state: *mut ysfx_state_t) -> bool;
    pub fn ysfx_save_state(fx: *mut ysfx_t) -> *mut ysfx_state_t;
    pub fn ysfx_state_free(state: *mut ysfx_state_t);

    pub fn ysfx_gfx_setup(fx: *mut ysfx_t, gc: *mut ysfx_gfx_config_t);
    pub fn ysfx_gfx_add_key(fx: *mut ysfx_t, mods: u32, key: u32, press: bool);
    pub fn ysfx_gfx_update_mouse(
        fx: *mut ysfx_t,
        mods: u32,
        xpos: i32,
        ypos: i32,
        buttons: u32,
        wheel: ysfx_real,
        hwheel: ysfx_real,
    );
    pub fn ysfx_gfx_set_window_state(fx: *mut ysfx_t, has_focus: bool, visible: bool, mouse_over: bool);
    pub fn ysfx_gfx_run(fx: *mut ysfx_t) -> bool;
    pub fn ysfx_get_requested_framerate(fx: *mut ysfx_t) -> u32;
}
