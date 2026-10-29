//! Native plugins: shared libraries written against `nodeplayer-plugin` (see `sdk/`). They
//! provide DSP nodes (hosted here like other formats) and app extensions ([`ext`]).

pub mod ext;
mod ui;

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
use nodeplayer_plugin::abi::*;

use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use crate::plugins::{self, BodyIo, Format, ParamDesc, PluginInfo};

const QUEUE_LEN: usize = 1024;

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

/// `$NODEPLAYER_PLUGIN_PATH` (colon separated), then `~/.local/share/nodeplayer/plugins`.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("NODEPLAYER_PLUGIN_PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join(".local/share/nodeplayer/plugins"));
    }
    v
}

pub fn find_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "so") {
            out.push(p);
        }
    }
}

/// A loaded plugin library and its description.
pub struct Lib {
    desc: *const NpPluginDesc,
    /// Node descriptors in this ABI's layout (older plugins' ones are copied and extended).
    nodes: Vec<NpNodeDesc>,
    pub name: String,
    pub path: PathBuf,
    // Kept loaded for the life of the process, like the other formats.
    _lib: Library,
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

impl Lib {
    pub fn load(path: &Path) -> Result<Self> {
        unsafe {
            let lib = Library::open(Some(path), RTLD_NOW | RTLD_LOCAL)
                .with_context(|| format!("dlopen {}", path.display()))?;
            let entry = *lib.get::<NpEntry>(ENTRY_SYMBOL.as_bytes()).context("not a nodeplayer plugin")?;
            let desc = entry();
            if desc.is_null() {
                bail!("plugin returned no description");
            }
            let d = &*desc;
            if !(MIN_ABI_VERSION..=ABI_VERSION).contains(&d.abi_version) {
                bail!("plugin ABI {} (nodeplayer loads {MIN_ABI_VERSION} to {ABI_VERSION})", d.abi_version);
            }
            let nodes = if d.nodes.is_null() { Vec::new() } else { Self::read_nodes(d) };
            Ok(Self { desc, nodes, name: cstr(d.name), path: path.to_path_buf(), _lib: lib })
        }
    }

    /// Copies the node descriptors; version 1 ones are shorter, so the rest stays zero (no
    /// MIDI).
    unsafe fn read_nodes(d: &NpPluginDesc) -> Vec<NpNodeDesc> {
        let size = match d.abi_version {
            1 => std::mem::offset_of!(NpNodeDesc, midi),
            _ => std::mem::size_of::<NpNodeDesc>(),
        };
        (0..d.node_count as usize)
            .map(|i| unsafe {
                let mut n: std::mem::MaybeUninit<NpNodeDesc> = std::mem::MaybeUninit::zeroed();
                let src = (d.nodes as *const u8).add(i * size);
                std::ptr::copy_nonoverlapping(src, n.as_mut_ptr() as *mut u8, size);
                n.assume_init()
            })
            .collect()
    }

    fn desc(&self) -> &NpPluginDesc {
        unsafe { &*self.desc }
    }

    fn nodes(&self) -> &[NpNodeDesc] {
        &self.nodes
    }

    pub fn extension(&self) -> Option<&NpExtension> {
        unsafe { self.desc().extension.as_ref() }
    }

    pub fn classes(&self) -> Vec<PluginInfo> {
        let author = cstr(self.desc().author);
        self.nodes()
            .iter()
            .map(|n| PluginInfo {
                format: Format::Native,
                name: cstr(n.name),
                vendor: if author.is_empty() { self.name.clone() } else { format!("{} ({author})", self.name) },
                category: cstr(n.category),
                id: cstr(n.id),
                path: self.path.clone(),
            })
            .collect()
    }
}

/// Keeps each library loaded once per process.
#[derive(Default)]
pub struct LibCache {
    libs: HashMap<PathBuf, Arc<Lib>>,
}

impl LibCache {
    pub fn get(&mut self, path: &Path) -> Result<Arc<Lib>> {
        if let Some(l) = self.libs.get(path) {
            return Ok(l.clone());
        }
        let l = Arc::new(Lib::load(path)?);
        self.libs.insert(path.to_path_buf(), l.clone());
        Ok(l)
    }
}

// ---------------------------------------------------------------------------
// Nodes

struct Inst {
    desc: *const NpNodeDesc,
    ptr: *mut c_void,
    _lib: Arc<Lib>,
}

unsafe impl Send for Inst {}
unsafe impl Sync for Inst {}

impl Inst {
    fn desc(&self) -> &NpNodeDesc {
        unsafe { &*self.desc }
    }
}

impl Drop for Inst {
    fn drop(&mut self) {
        unsafe { (self.desc().destroy)(self.ptr) };
    }
}

pub struct NativeNode {
    info: PluginInfo,
    inst: Arc<Inst>,
    params: Vec<ParamDesc>,
    ranges: HashMap<u32, (f64, f64, bool)>,
    to_audio: rtrb::Producer<(u32, f64)>,
    /// Values set from the UI, shown until the plugin has seen them.
    recent: HashMap<u32, (f64, Instant)>,
}

impl NativeNode {
    pub fn create(lib: Arc<Lib>, info: &PluginInfo, sample_rate: f32, max_block: usize) -> Result<(Self, NativeProcessor)> {
        let desc = lib
            .nodes()
            .iter()
            .find(|n| cstr(n.id) == info.id)
            .ok_or_else(|| anyhow!("{} has no node {}", lib.name, info.id))? as *const NpNodeDesc;
        let d = unsafe { &*desc };
        let ptr = unsafe { (d.create)(sample_rate, max_block as u32) };
        if ptr.is_null() {
            bail!("{}: create failed", info.name);
        }
        let inst = Arc::new(Inst { desc, ptr, _lib: lib });
        let infos = if d.params.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(d.params, d.param_count as usize) } };
        let mut params = Vec::new();
        let mut ranges = HashMap::new();
        for p in infos {
            let (min, max) = (p.min, p.max);
            let log = p.flags & PARAM_LOG != 0 && min > 0.0 && max > min;
            ranges.insert(p.id, (min, max, log));
            params.push(ParamDesc {
                id: p.id,
                title: cstr(p.name),
                units: cstr(p.unit),
                step_count: p.steps as i32,
                default: norm((min, max, log), p.default),
                hidden: p.flags & PARAM_HIDDEN != 0,
                read_only: false,
            });
        }
        let (to_audio, from_ui) = rtrb::RingBuffer::new(QUEUE_LEN);
        let me = Self { info: info.clone(), inst: inst.clone(), params, ranges, to_audio, recent: HashMap::new() };
        Ok((me, NativeProcessor { inst, from_ui, midi_in: Vec::with_capacity(MidiBuf::CAPACITY) }))
    }

    fn plain_value(&self, id: u32) -> f64 {
        if let Some((v, t)) = self.recent.get(&id)
            && t.elapsed() < Duration::from_millis(300)
        {
            return *v;
        }
        unsafe { (self.inst.desc().get_param)(self.inst.ptr, id) }
    }

    fn set_plain(&mut self, id: u32, v: f64) {
        self.recent.insert(id, (v, Instant::now()));
        let _ = self.to_audio.push((id, v));
    }

    /// Sliders for every visible parameter, for nodes without their own UI.
    fn generic_ui(&mut self, ui: &mut egui::Ui) {
        let mut changes = Vec::new();
        egui::Grid::new(ui.id().with("params")).num_columns(2).show(ui, |ui| {
            for p in self.params.iter().filter(|p| !p.hidden) {
                let (min, max, log) = self.ranges[&p.id];
                let mut v = self.plain_value(p.id);
                ui.label(&p.title);
                let mut s = egui::Slider::new(&mut v, min..=max).suffix(if p.units.is_empty() { String::new() } else { format!(" {}", p.units) });
                if log {
                    s = s.logarithmic(true);
                }
                if p.step_count > 0 {
                    s = s.step_by((max - min) / p.step_count as f64);
                }
                let r = ui.add(s);
                if r.double_clicked() {
                    changes.push((p.id, denorm((min, max, log), p.default)));
                } else if r.changed() {
                    changes.push((p.id, v));
                }
                ui.end_row();
            }
        });
        for (id, v) in changes {
            self.set_plain(id, v);
        }
    }
}

fn norm((min, max, log): (f64, f64, bool), v: f64) -> f64 {
    if max <= min {
        return 0.0;
    }
    let t = if log { (v.max(min) / min).ln() / (max / min).ln() } else { (v - min) / (max - min) };
    t.clamp(0.0, 1.0)
}

fn denorm((min, max, log): (f64, f64, bool), t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if log { min * (max / min).powf(t) } else { min + t * (max - min) }
}

impl plugins::Instance for NativeNode {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn params(&self) -> &[ParamDesc] {
        &self.params
    }

    fn param_value(&self, id: u32) -> f64 {
        self.ranges.get(&id).map_or(0.0, |r| norm(*r, self.plain_value(id)))
    }

    fn param_display(&self, id: u32, value: f64) -> String {
        let Some(r) = self.ranges.get(&id) else { return String::new() };
        format!("{:.3}", denorm(*r, value))
    }

    fn set_param(&mut self, id: u32, value: f64) {
        if let Some(r) = self.ranges.get(&id).copied() {
            self.set_plain(id, denorm(r, value));
        }
    }

    fn latency(&self) -> u32 {
        self.inst.desc().latency.map_or(0, |l| unsafe { l(self.inst.ptr) })
    }

    fn midi_ports(&self) -> (bool, bool) {
        let m = self.inst.desc().midi;
        (m & MIDI_IN != 0, m & MIDI_OUT != 0)
    }

    fn idle(&mut self) {
        self.recent.retain(|_, (_, t)| t.elapsed() < Duration::from_millis(300));
    }

    /// `NPNA`, parameter count (u32), (id u32, value f64) each, then the plugin's own state
    /// (u64 length and bytes).
    fn save_state(&self) -> Option<Vec<u8>> {
        let mut out = b"NPNA".to_vec();
        out.extend((self.params.len() as u32).to_le_bytes());
        for p in &self.params {
            out.extend(p.id.to_le_bytes());
            out.extend(self.plain_value(p.id).to_le_bytes());
        }
        let extra = match self.inst.desc().save_state {
            Some(save) => {
                let mut v: Vec<u8> = Vec::new();
                let mut buf = NpBuf { ctx: &mut v as *mut Vec<u8> as *mut c_void, write: buf_write };
                unsafe { save(self.inst.ptr, &mut buf) };
                v
            }
            None => Vec::new(),
        };
        out.extend((extra.len() as u64).to_le_bytes());
        out.extend(extra);
        Some(out)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let bad = || anyhow!("not a native plugin state");
        let mut rest = data.strip_prefix(b"NPNA").ok_or_else(bad)?;
        let mut take = |n: usize| -> Result<&[u8]> {
            if rest.len() < n {
                return Err(bad());
            }
            let (a, b) = rest.split_at(n);
            rest = b;
            Ok(a)
        };
        let count = u32::from_le_bytes(take(4)?.try_into()?);
        let mut values = Vec::new();
        for _ in 0..count.min(100_000) {
            let id = u32::from_le_bytes(take(4)?.try_into()?);
            values.push((id, f64::from_le_bytes(take(8)?.try_into()?)));
        }
        let len = u64::from_le_bytes(take(8)?.try_into()?) as usize;
        let extra = take(len)?.to_vec();
        for (id, v) in values {
            if self.ranges.contains_key(&id) {
                self.set_plain(id, v);
            }
        }
        if !extra.is_empty()
            && let Some(load) = self.inst.desc().load_state
            && !unsafe { load(self.inst.ptr, extra.as_ptr(), extra.len()) }
        {
            bail!("plugin rejected its state");
        }
        Ok(())
    }

    fn has_body(&self) -> bool {
        true
    }

    fn body_ui(&mut self, ui: &mut egui::Ui, io: &mut BodyIo) {
        match self.inst.desc().draw {
            Some(draw) => ui::with_ui(ui, io, |np| unsafe { draw(self.inst.ptr, np) }),
            None => self.generic_ui(ui),
        }
    }
}

unsafe extern "C" fn buf_write(ctx: *mut c_void, data: *const u8, len: usize) {
    if len > 0 && !data.is_null() {
        unsafe { (*(ctx as *mut Vec<u8>)).extend_from_slice(std::slice::from_raw_parts(data, len)) };
    }
}

unsafe extern "C" fn midi_push(ctx: *mut c_void, e: *const NpMidiEvent) -> bool {
    let (out, n) = unsafe { &mut *(ctx as *mut (&mut MidiBuf, u32)) };
    let e = unsafe { &*e };
    let len = (e.len as usize).clamp(1, 3);
    match MidiEvent::new(e.time.min(n.saturating_sub(1)), &e.data[..len]) {
        Some(ev) => out.push(ev),
        None => false,
    }
}

pub struct NativeProcessor {
    inst: Arc<Inst>,
    from_ui: rtrb::Consumer<(u32, f64)>,
    /// The block's input in the ABI's layout; preallocated.
    midi_in: Vec<NpMidiEvent>,
}

impl NativeProcessor {
    fn run(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf], midi: Option<(&[MidiEvent], &mut MidiBuf)>) {
        let d = self.inst.desc();
        while let Ok((id, v)) = self.from_ui.pop() {
            unsafe { (d.set_param)(self.inst.ptr, id, v) };
        }
        if let (Some(f), Some((input, output))) = (d.process_midi, midi) {
            self.midi_in.clear();
            self.midi_in.extend(input.iter().map(|e| NpMidiEvent { time: e.time, data: e.data, len: e.bytes().len() as u8 }));
            let mut ctx: (&mut MidiBuf, u32) = (output, n as u32);
            let out = NpMidiOut { ctx: &mut ctx as *mut (&mut MidiBuf, u32) as *mut c_void, push: midi_push };
            unsafe { f(self.inst.ptr, self.midi_in.as_ptr(), self.midi_in.len() as u32, &out) };
        }
        let (i, o) = (&inputs[0], &mut outputs[0]);
        unsafe { (d.process)(self.inst.ptr, i.l.as_ptr(), i.r.as_ptr(), o.l.as_mut_ptr(), o.r.as_mut_ptr(), n as u32) };
    }
}

impl Processor for NativeProcessor {
    fn process(&mut self, n: usize, inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        self.run(n, inputs, outputs, None);
    }

    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.run(n, inputs, outputs, Some((midi_in, midi_out)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn no_create(_: f32, _: u32) -> *mut c_void {
        std::ptr::null_mut()
    }
    unsafe extern "C" fn no_destroy(_: *mut c_void) {}
    unsafe extern "C" fn no_process(_: *mut c_void, _: *const f32, _: *const f32, _: *mut f32, _: *mut f32, _: u32) {}
    unsafe extern "C" fn no_set(_: *mut c_void, _: u32, _: f64) {}
    unsafe extern "C" fn no_get(_: *mut c_void, _: u32) -> f64 {
        0.0
    }

    #[test]
    fn reads_version_1_node_tables() {
        let node = |id: &'static CStr| NpNodeDesc {
            id: id.as_ptr(),
            name: id.as_ptr(),
            category: c"Fx".as_ptr(),
            params: std::ptr::null(),
            param_count: 0,
            create: no_create,
            destroy: no_destroy,
            process: no_process,
            set_param: no_set,
            get_param: no_get,
            draw: None,
            save_state: None,
            load_state: None,
            latency: None,
            midi: MIDI_IN,
            process_midi: None,
        };
        // A version 1 table: descriptors packed at the old, shorter stride.
        let v1 = std::mem::offset_of!(NpNodeDesc, midi);
        let mut bytes = vec![0u8; v1 * 2];
        for (i, n) in [node(c"a"), node(c"b")].iter().enumerate() {
            let src = unsafe { std::slice::from_raw_parts(n as *const NpNodeDesc as *const u8, v1) };
            bytes[i * v1..(i + 1) * v1].copy_from_slice(src);
        }
        let mut desc = NpPluginDesc {
            abi_version: 1,
            name: c"t".as_ptr(),
            version: c"1".as_ptr(),
            author: c"".as_ptr(),
            nodes: bytes.as_ptr() as *const NpNodeDesc,
            node_count: 2,
            extension: std::ptr::null(),
        };
        let nodes = unsafe { Lib::read_nodes(&desc) };
        assert_eq!(nodes.iter().map(|n| cstr(n.id)).collect::<Vec<_>>(), ["a", "b"]);
        assert!(nodes.iter().all(|n| n.midi == 0 && n.process_midi.is_none()));

        let v2 = [node(c"c")];
        desc.abi_version = 2;
        desc.nodes = v2.as_ptr();
        desc.node_count = 1;
        assert_eq!(unsafe { Lib::read_nodes(&desc) }[0].midi, MIDI_IN);
    }

    #[test]
    fn normalization_round_trips() {
        for r in [(0.0, 1.0, false), (-24.0, 24.0, false), (20.0, 20_000.0, true)] {
            for v in [r.0, r.1, (r.0 + r.1) / 2.0] {
                assert!((denorm(r, norm(r, v)) - v).abs() < 1e-9 * v.abs().max(1.0), "{r:?} {v}");
            }
        }
        // On a log range the geometric mean sits in the middle.
        assert!((norm((20.0, 20_000.0, true), (20.0f64 * 20_000.0).sqrt()) - 0.5).abs() < 1e-9);
    }
}
