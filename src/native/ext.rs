//! App extensions from native plugins, and the [`NpHost`] services they use.
//!
//! Plugins never touch the app directly: queries read a snapshot the app keeps up to date
//! ([`HostState`]), and commands are queued for the app to carry out after the plugin's
//! callback returns.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use nodeplayer_plugin::abi::*;

use super::{Lib, LibCache};
use crate::plugins::guard::{self, Tag};
use crate::plugins::{BodyIo, Format};

/// What extensions can read about the player.
#[derive(Default)]
pub struct HostState {
    pub has_track: bool,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub track_index: Option<usize>,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub library_root: Option<String>,
    pub library: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek(f64),
    LoadFile(PathBuf),
    PlayTrack(usize),
    OpenFolder(PathBuf),
    LoadPreset(String),
    SavePreset(String),
    SetVolume(f32),
    /// A warning or error to show in the status line.
    Message(String),
}

#[derive(Clone)]
pub struct MenuItem {
    pub ext: usize,
    pub id: u32,
    pub label: String,
}

#[derive(Default)]
struct Shared {
    state: HostState,
    commands: Vec<Command>,
    menu: Vec<MenuItem>,
    repaint: bool,
}

/// `host_data` of one extension's [`NpHost`].
struct HostCtx {
    shared: Rc<RefCell<Shared>>,
    ext: usize,
    name: String,
    next_menu: Cell<u32>,
}

unsafe fn hctx<'a>(host: *const NpHost) -> &'a HostCtx {
    unsafe { &*((*host).host_data as *const HostCtx) }
}

unsafe fn write(out: *mut NpBuf, s: &str) {
    if !out.is_null() && !s.is_empty() {
        unsafe { ((*out).write)((*out).ctx, s.as_ptr(), s.len()) };
    }
}

unsafe extern "C" fn host_log(host: *const NpHost, level: u32, msg: NpStr) {
    let c = unsafe { hctx(host) };
    let msg = unsafe { msg.as_str() };
    match level {
        LOG_INFO => log::info!("{}: {msg}", c.name),
        LOG_WARN => {
            log::warn!("{}: {msg}", c.name);
            c.shared.borrow_mut().commands.push(Command::Message(format!("{}: {msg}", c.name)));
        }
        _ => {
            log::error!("{}: {msg}", c.name);
            c.shared.borrow_mut().commands.push(Command::Message(format!("{}: {msg}", c.name)));
        }
    }
}

unsafe extern "C" fn host_status(host: *const NpHost, out: *mut NpStatus) {
    if out.is_null() {
        return;
    }
    let sh = unsafe { hctx(host) }.shared.borrow();
    let s = &sh.state;
    unsafe {
        *out = NpStatus {
            has_track: s.has_track,
            playing: s.playing,
            position: s.position,
            duration: s.duration,
            track_index: s.track_index.map_or(-1, |i| i as i32),
            library_len: s.library.len() as u32,
        };
    }
}

unsafe extern "C" fn host_track_text(host: *const NpHost, field: u32, out: *mut NpBuf) {
    let sh = unsafe { hctx(host) }.shared.borrow();
    let s = &sh.state;
    let text = match field {
        TRACK_PATH => &s.path,
        TRACK_TITLE => &s.title,
        TRACK_ARTIST => &s.artist,
        TRACK_ALBUM => &s.album,
        _ => return,
    };
    unsafe { write(out, text) };
}

unsafe extern "C" fn host_library_path(host: *const NpHost, index: u32, out: *mut NpBuf) -> bool {
    let sh = unsafe { hctx(host) }.shared.borrow();
    match sh.state.library.get(index as usize) {
        Some(p) => {
            unsafe { write(out, p) };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_library_root(host: *const NpHost, out: *mut NpBuf) -> bool {
    let sh = unsafe { hctx(host) }.shared.borrow();
    match &sh.state.library_root {
        Some(r) => {
            unsafe { write(out, r) };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_command(host: *const NpHost, cmd: u32, number: f64, text: NpStr) -> bool {
    let text = unsafe { text.as_str() }.into_owned();
    let c = match cmd {
        CMD_PLAY => Command::Play,
        CMD_PAUSE => Command::Pause,
        CMD_TOGGLE => Command::Toggle,
        CMD_STOP => Command::Stop,
        CMD_NEXT => Command::Next,
        CMD_PREVIOUS => Command::Previous,
        CMD_SEEK if number.is_finite() => Command::Seek(number.max(0.0)),
        CMD_LOAD_FILE => Command::LoadFile(text.into()),
        CMD_PLAY_TRACK if number >= 0.0 => Command::PlayTrack(number as usize),
        CMD_OPEN_FOLDER => Command::OpenFolder(text.into()),
        CMD_LOAD_PRESET => Command::LoadPreset(text),
        CMD_SAVE_PRESET => Command::SavePreset(text),
        CMD_SET_VOLUME if number.is_finite() => Command::SetVolume(number.clamp(-60.0, 12.0) as f32),
        _ => return false,
    };
    unsafe { hctx(host) }.shared.borrow_mut().commands.push(c);
    true
}

unsafe extern "C" fn host_add_menu_item(host: *const NpHost, label: NpStr) -> u32 {
    let c = unsafe { hctx(host) };
    let id = c.next_menu.get();
    c.next_menu.set(id + 1);
    let label = unsafe { label.as_str() }.into_owned();
    c.shared.borrow_mut().menu.push(MenuItem { ext: c.ext, id, label });
    id
}

unsafe extern "C" fn host_request_repaint(host: *const NpHost) {
    unsafe { hctx(host) }.shared.borrow_mut().repaint = true;
}

unsafe extern "C" fn host_data_dir(host: *const NpHost, out: *mut NpBuf) {
    let c = unsafe { hctx(host) };
    let safe: String = c.name.chars().map(|ch| if ch.is_alphanumeric() || "-_ .".contains(ch) { ch } else { '_' }).collect();
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = PathBuf::from(home).join(".local/share/nodeplayer/plugin-data").join(safe.trim());
    let _ = std::fs::create_dir_all(&dir);
    unsafe { write(out, &dir.to_string_lossy()) };
}

pub struct Extension {
    pub name: String,
    tag: Arc<Tag>,
    pub panel_title: Option<String>,
    vt: *const NpExtension,
    ptr: *mut c_void,
    // Referenced by the plugin through `ptr`'s host pointer, so they outlive it.
    _host: Box<NpHost>,
    _ctx: Box<HostCtx>,
    _lib: Arc<Lib>,
}

impl Extension {
    fn vt(&self) -> &NpExtension {
        unsafe { &*self.vt }
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        let _g = guard::enter(&self.tag);
        unsafe { (self.vt().destroy)(self.ptr) };
    }
}

/// Every loaded extension.
#[derive(Default)]
pub struct Extensions {
    list: Vec<Extension>,
    shared: Rc<RefCell<Shared>>,
}

impl Extensions {
    /// Loads the extensions of every native plugin found. Returns the problems found too.
    pub fn load(cache: &mut LibCache, disabled: &[(Format, PathBuf)]) -> (Self, Vec<String>) {
        let mut me = Self::default();
        let mut errors = Vec::new();
        let mut files = Vec::new();
        for dir in super::default_search_paths() {
            super::find_files(&dir, &mut files);
        }
        for path in files {
            if disabled.iter().any(|(f, p)| *f == Format::Native && *p == path) {
                errors.push(format!("{}: disabled because it crashed nodeplayer", path.display()));
                continue;
            }
            let tag = Tag::new(Format::Native, &path);
            let _g = guard::enter(&tag);
            let lib = match cache.get(&path) {
                Ok(l) => l,
                Err(e) => {
                    errors.push(format!("plugin {}: {e:#}", path.display()));
                    continue;
                }
            };
            let Some(vt) = lib.extension().map(|e| e as *const NpExtension) else { continue };
            let ctx = Box::new(HostCtx {
                shared: me.shared.clone(),
                ext: me.list.len(),
                name: lib.name.clone(),
                next_menu: Cell::new(1),
            });
            let host = Box::new(NpHost {
                abi_version: ABI_VERSION,
                host_data: &*ctx as *const HostCtx as *mut c_void,
                log: host_log,
                status: host_status,
                track_text: host_track_text,
                library_path: host_library_path,
                library_root: host_library_root,
                command: host_command,
                add_menu_item: host_add_menu_item,
                request_repaint: host_request_repaint,
                data_dir: host_data_dir,
            });
            let v = unsafe { &*vt };
            let ptr = unsafe { (v.create)(&*host) };
            if ptr.is_null() {
                errors.push(format!("{}: extension failed to start", lib.name));
                continue;
            }
            let panel_title = (!v.panel_title.is_null() && v.draw_panel.is_some())
                .then(|| unsafe { CStr::from_ptr(v.panel_title) }.to_string_lossy().into_owned());
            log::info!("extension {} loaded from {}", lib.name, path.display());
            me.list.push(Extension { name: lib.name.clone(), tag: tag.clone(), panel_title, vt, ptr, _host: host, _ctx: ctx, _lib: lib });
        }
        (me, errors)
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn state(&self) -> std::cell::RefMut<'_, HostState> {
        std::cell::RefMut::map(self.shared.borrow_mut(), |s| &mut s.state)
    }

    pub fn event(&mut self, kind: u32) {
        let ev = NpEvent { kind };
        for e in &self.list {
            if let Some(f) = e.vt().on_event {
                let _g = guard::enter(&e.tag);
                unsafe { f(e.ptr, &ev) };
            }
        }
    }

    pub fn idle(&mut self) {
        for e in &self.list {
            if let Some(f) = e.vt().idle {
                let _g = guard::enter(&e.tag);
                unsafe { f(e.ptr) };
            }
        }
    }

    pub fn menu_items(&self) -> Vec<MenuItem> {
        self.shared.borrow().menu.clone()
    }

    pub fn menu(&mut self, item: &MenuItem) {
        let Some(e) = self.list.get(item.ext) else { return };
        if let Some(f) = e.vt().on_menu {
            let _g = guard::enter(&e.tag);
            unsafe { f(e.ptr, item.id) };
        }
    }

    /// Extensions that draw a side panel: (index, extension name, panel title).
    pub fn panels(&self) -> Vec<(usize, String, String)> {
        self.list
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, e.name.clone(), e.panel_title.clone()?)))
            .collect()
    }

    pub fn draw_panel(&mut self, index: usize, ui: &mut egui::Ui) {
        let Some(e) = self.list.get(index) else { return };
        let Some(draw) = e.vt().draw_panel else { return };
        let mut io = BodyIo::default();
        let _g = guard::enter(&e.tag);
        super::ui::with_ui(ui, &mut io, |np| unsafe { draw(e.ptr, np) });
    }

    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.shared.borrow_mut().commands)
    }

    pub fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.shared.borrow_mut().repaint)
    }
}
