//! VST3 hosting: module loading, scanning and instance lifecycle.
//!
//! On Linux, Windows plugins are expected to be wrapped by yabridge, which makes them look like
//! ordinary Linux VST3 bundles, so nothing here is Wine-specific. On Windows and macOS the
//! platform's own bundles load directly.

pub mod editor;
pub mod host;
mod processor;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

use crate::plugins::dylib::{self, Library};
use crate::plugins::{self, Format, ParamDesc, Placement, PluginInfo};
use host::{
    ComponentHandler, HostApp, MemoryStream, ParamChange, ParamSender, cchar_to_string,
    tchar_to_string,
};
pub use processor::Vst3Processor;

const AUDIO_MODULE_CLASS: &str = "Audio Module Class";
const PARAM_QUEUE_LEN: usize = 1024;

fn cid_hex(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn parse_cid_hex(s: &str) -> Option<TUID> {
    let mut cid = [0; 16];
    if s.len() != 32 {
        return None;
    }
    for (i, c) in cid.iter_mut().enumerate() {
        *c = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()? as i8;
    }
    Some(cid)
}

// ---------------------------------------------------------------------------
// Modules

pub struct Module {
    factory: ComPtr<IPluginFactory>,
    // Plugins frequently misbehave when unloaded, so modules are kept for the process lifetime.
    _lib: Library,
}

unsafe impl Send for Module {}
unsafe impl Sync for Module {}

/// Resolves a bundle directory to the shared library inside it: `Contents/x86_64-linux/X.so`,
/// `Contents/x86_64-win/X.vst3`, `Contents/MacOS/X`. Older Windows plugins are a single DLL
/// named `X.vst3`, which is returned as it is.
pub fn bundle_binary(path: &Path) -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return dylib::mac_bundle_binary(path);
    }
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let (arch, ext) = if cfg!(windows) {
        (format!("{}-win", std::env::consts::ARCH), "vst3")
    } else {
        (format!("{}-linux", std::env::consts::ARCH), "so")
    };
    let dir = path.join("Contents").join(arch);
    let stem = path.file_stem().unwrap_or_default();
    let preferred = dir.join(stem).with_extension(ext);
    if preferred.is_file() {
        return Ok(preferred);
    }
    std::fs::read_dir(&dir)
        .with_context(|| format!("no {} in bundle", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)))
        .ok_or_else(|| anyhow!("no .{ext} in {}", dir.display()))
}

impl Module {
    pub fn load(bundle: &Path) -> Result<Self> {
        let bin = bundle_binary(bundle)?;
        let lib = dylib::open(&bin)?;
        unsafe {
            // Each platform has its own entry point, called once before anything else.
            #[cfg(all(unix, not(target_os = "macos")))]
            let lib = {
                let lib: libloading::os::unix::Library = lib.into();
                let handle = lib.into_raw();
                let lib = Library::from(libloading::os::unix::Library::from_raw(handle));
                if let Ok(entry) = lib.get::<unsafe extern "system" fn(*mut std::ffi::c_void) -> bool>(b"ModuleEntry")
                    && !entry(handle)
                {
                    bail!("ModuleEntry failed");
                }
                lib
            };
            #[cfg(windows)]
            if let Ok(entry) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll")
                && !entry()
            {
                bail!("InitDll failed");
            }
            #[cfg(target_os = "macos")]
            if let Ok(entry) = lib.get::<unsafe extern "C" fn(*mut std::ffi::c_void) -> bool>(b"bundleEntry")
                && !entry(dylib::cf_bundle(bundle)?)
            {
                bail!("bundleEntry failed");
            }
            let get_factory = lib
                .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory")
                .context("GetPluginFactory not exported")?;
            let factory =
                ComPtr::from_raw(get_factory()).ok_or_else(|| anyhow!("null plugin factory"))?;
            Ok(Self { factory, _lib: lib })
        }
    }

    pub fn classes(&self, bundle: &Path) -> Vec<PluginInfo> {
        let mut out = Vec::new();
        unsafe {
            let mut finfo: PFactoryInfo = std::mem::zeroed();
            let factory_vendor = if self.factory.getFactoryInfo(&mut finfo) == kResultOk {
                cchar_to_string(&finfo.vendor)
            } else {
                String::new()
            };
            let f2 = self.factory.cast::<IPluginFactory2>();
            for i in 0..self.factory.countClasses() {
                let mut info: PClassInfo = std::mem::zeroed();
                if self.factory.getClassInfo(i, &mut info) != kResultOk
                    || cchar_to_string(&info.category) != AUDIO_MODULE_CLASS
                {
                    continue;
                }
                let mut pi = PluginInfo {
                    format: Format::Vst3,
                    name: cchar_to_string(&info.name),
                    vendor: factory_vendor.clone(),
                    category: String::new(),
                    id: cid_hex(&info.cid),
                    path: bundle.to_path_buf(),
                };
                if let Some(f2) = &f2 {
                    let mut info2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut info2) == kResultOk {
                        pi.category = cchar_to_string(&info2.subCategories);
                        let v = cchar_to_string(&info2.vendor);
                        if !v.is_empty() {
                            pi.vendor = v;
                        }
                    }
                }
                out.push(pi);
            }
        }
        out
    }

    unsafe fn create<I: Interface>(&self, cid: &TUID) -> Option<ComPtr<I>> {
        let mut obj = std::ptr::null_mut();
        let res = unsafe {
            self.factory.createInstance(
                cid.as_ptr() as FIDString,
                I::IID.as_ptr() as FIDString,
                &mut obj,
            )
        };
        if res != kResultOk {
            return None;
        }
        unsafe { ComPtr::from_raw(obj as *mut I) }
    }
}

/// Keeps each module loaded once per process.
#[derive(Default)]
pub struct ModuleCache {
    modules: HashMap<PathBuf, Arc<Module>>,
}

impl ModuleCache {
    pub fn get(&mut self, bundle: &Path) -> Result<Arc<Module>> {
        if let Some(m) = self.modules.get(bundle) {
            return Ok(m.clone());
        }
        let m = Arc::new(Module::load(bundle)?);
        self.modules.insert(bundle.to_path_buf(), m.clone());
        Ok(m)
    }
}

// ---------------------------------------------------------------------------
// Scanning

/// `$VST3_PATH` (separated like PATH) first, then the platform's standard locations.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("VST3_PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    v.extend(plugins::standard_dirs("VST3", ".vst3", &["/usr/lib/vst3", "/usr/lib64/vst3", "/usr/local/lib/vst3"]));
    v
}

/// Finds `.vst3` bundles below `root` without loading them.
pub fn find_bundles(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3")) {
            out.push(p);
        } else if p.is_dir() {
            find_bundles(&p, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Instances

/// The UI-thread half of a plugin instance. The audio half is [`Vst3Processor`].
pub struct Vst3Plugin {
    pub info: PluginInfo,
    pub params: Vec<ParamDesc>,
    pub latency: u32,
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    separate_controller: bool,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    _host: ComWrapper<HostApp>,
    _handler: ComWrapper<ComponentHandler>,
    to_audio: ParamSender,
    from_audio: rtrb::Consumer<ParamChange>,
    restart_flags: Arc<AtomicI32>,
    restart_pending: bool,
    /// Event input and output buses.
    midi: (bool, bool),
    _module: Arc<Module>,
}

impl Vst3Plugin {
    pub fn create(
        module: Arc<Module>,
        info: &PluginInfo,
        sample_rate: f64,
        max_block: usize,
    ) -> Result<(Self, Vst3Processor)> {
        unsafe { Self::create_inner(module, info, sample_rate, max_block) }
    }

    unsafe fn create_inner(
        module: Arc<Module>,
        info: &PluginInfo,
        sample_rate: f64,
        max_block: usize,
    ) -> Result<(Self, Vst3Processor)> {
        unsafe {
            let host = ComWrapper::new(HostApp);
            let host_unk = host.as_com_ref::<FUnknown>().unwrap().as_ptr();

            let cid = parse_cid_hex(&info.id).ok_or_else(|| anyhow!("bad class id {}", info.id))?;
            let component = module
                .create::<IComponent>(&cid)
                .ok_or_else(|| anyhow!("createInstance(IComponent) failed"))?;
            if component.initialize(host_unk) != kResultOk {
                bail!("IComponent::initialize failed");
            }
            let _audio = component
                .cast::<IAudioProcessor>()
                .ok_or_else(|| anyhow!("component has no IAudioProcessor"))?;

            // Controller: either the component itself or a separate class.
            let mut separate_controller = false;
            let controller = match component.cast::<IEditController>() {
                Some(c) => Some(c),
                None => {
                    let mut ccid: TUID = [0; 16];
                    if component.getControllerClassId(&mut ccid) == kResultOk {
                        module.create::<IEditController>(&ccid).filter(|c| {
                            let ok = c.initialize(host_unk) == kResultOk;
                            separate_controller = ok;
                            ok
                        })
                    } else {
                        None
                    }
                }
            };

            let connections = if separate_controller {
                let ctrl = controller.as_ref().unwrap();
                match (component.cast::<IConnectionPoint>(), ctrl.cast::<IConnectionPoint>()) {
                    (Some(a), Some(b)) => {
                        a.connect(b.as_ptr());
                        b.connect(a.as_ptr());
                        Some((a, b))
                    }
                    _ => None,
                }
            } else {
                None
            };

            // Sync controller with the component's initial state.
            if let Some(ctrl) = &controller {
                let stream = ComWrapper::new(MemoryStream::default());
                let sp = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
                if component.getState(sp) == kResultOk {
                    stream.rewind();
                    ctrl.setComponentState(sp);
                }
            }

            let (to_audio_tx, to_audio_rx) = rtrb::RingBuffer::new(PARAM_QUEUE_LEN);
            let (from_audio_tx, from_audio_rx) = rtrb::RingBuffer::new(PARAM_QUEUE_LEN);
            let to_audio: ParamSender = Arc::new(Mutex::new(to_audio_tx));
            let restart_flags = Arc::new(AtomicI32::new(0));
            let handler = ComWrapper::new(ComponentHandler {
                to_audio: to_audio.clone(),
                restart_flags: restart_flags.clone(),
            });
            if let Some(ctrl) = &controller {
                ctrl.setComponentHandler(handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr());
            }

            let mut plugin = Self {
                info: info.clone(),
                params: Vec::new(),
                latency: 0,
                component,
                controller,
                separate_controller,
                connections,
                _host: host,
                _handler: handler,
                to_audio,
                from_audio: from_audio_rx,
                restart_flags,
                restart_pending: false,
                midi: (false, false),
                _module: module,
            };
            drop((to_audio_rx, from_audio_tx));
            plugin.reload_params();
            let proc = plugin.activate(sample_rate, max_block)?;
            Ok((plugin, proc))
        }
    }

    /// Sets up buses and processing, activates the component and returns a processor for it.
    fn activate(&mut self, sample_rate: f64, max_block: usize) -> Result<Vst3Processor> {
        let component = &self.component;
        let audio = component.cast::<IAudioProcessor>().ok_or_else(|| anyhow!("component has no IAudioProcessor"))?;
        unsafe {
            // Buses: ask for stereo on the main buses, keep whatever the rest use.
            use BusDirections_::*;
            use MediaTypes_::*;
            let n_in = component.getBusCount(kAudio as i32, kInput as i32).max(0) as usize;
            let n_out = component.getBusCount(kAudio as i32, kOutput as i32).max(0) as usize;
            let mut in_arr = vec![SpeakerArr::kStereo; n_in];
            let mut out_arr = vec![SpeakerArr::kStereo; n_out];
            for (i, a) in in_arr.iter_mut().enumerate().skip(1) {
                audio.getBusArrangement(kInput as i32, i as i32, a);
            }
            for (i, a) in out_arr.iter_mut().enumerate().skip(1) {
                audio.getBusArrangement(kOutput as i32, i as i32, a);
            }
            if audio.setBusArrangements(
                in_arr.as_mut_ptr(),
                n_in as i32,
                out_arr.as_mut_ptr(),
                n_out as i32,
            ) != kResultOk
            {
                log::info!("{}: plugin rejected stereo bus arrangement", self.info.name);
            }
            let bus_channels = |dir: i32, n: usize| -> Vec<usize> {
                (0..n)
                    .map(|i| {
                        let mut bi: BusInfo = std::mem::zeroed();
                        component.getBusInfo(kAudio as i32, dir, i as i32, &mut bi);
                        bi.channelCount.max(0) as usize
                    })
                    .collect()
            };
            let in_ch = bus_channels(kInput as i32, n_in);
            let out_ch = bus_channels(kOutput as i32, n_out);
            if n_in > 0 {
                component.activateBus(kAudio as i32, kInput as i32, 0, 1);
            }
            if n_out > 0 {
                component.activateBus(kAudio as i32, kOutput as i32, 0, 1);
            }
            // The first event bus of each direction carries the node's MIDI.
            let ev_in = component.getBusCount(kEvent as i32, kInput as i32) > 0;
            let ev_out = component.getBusCount(kEvent as i32, kOutput as i32) > 0;
            if ev_in {
                component.activateBus(kEvent as i32, kInput as i32, 0, 1);
            }
            if ev_out {
                component.activateBus(kEvent as i32, kOutput as i32, 0, 1);
            }
            self.midi = (ev_in, ev_out);

            if audio.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultTrue {
                bail!("plugin can't process 32-bit float");
            }
            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: max_block as i32,
                sampleRate: sample_rate,
            };
            if audio.setupProcessing(&mut setup) != kResultOk {
                bail!("setupProcessing failed");
            }
            if component.setActive(1) != kResultOk {
                bail!("setActive failed");
            }
            self.latency = audio.getLatencySamples();

            let (to_audio_tx, to_audio_rx) = rtrb::RingBuffer::new(PARAM_QUEUE_LEN);
            let (from_audio_tx, from_audio_rx) = rtrb::RingBuffer::new(PARAM_QUEUE_LEN);
            *self.to_audio.lock().unwrap() = to_audio_tx;
            self.from_audio = from_audio_rx;
            let mapping = self.controller.as_ref().and_then(|c| c.cast::<IMidiMapping>());
            let cc_map = processor::CcMap::new(if ev_in { mapping.as_ref() } else { None });
            Ok(Vst3Processor::new(
                audio,
                to_audio_rx,
                from_audio_tx,
                &in_ch,
                &out_ch,
                self.midi,
                cc_map,
                sample_rate,
                max_block,
            ))
        }
    }

    pub fn reload_params(&mut self) {
        self.params.clear();
        let Some(ctrl) = &self.controller else { return };
        unsafe {
            for i in 0..ctrl.getParameterCount() {
                let mut pi: ParameterInfo = std::mem::zeroed();
                if ctrl.getParameterInfo(i, &mut pi) != kResultOk {
                    continue;
                }
                use ParameterInfo_::ParameterFlags_::*;
                self.params.push(ParamDesc {
                    id: pi.id,
                    title: tchar_to_string(&pi.title),
                    units: tchar_to_string(&pi.units),
                    step_count: pi.stepCount,
                    default: pi.defaultNormalizedValue,
                    hidden: pi.flags & kIsHidden != 0,
                    read_only: pi.flags & kIsReadOnly != 0,
                });
            }
        }
    }

    pub fn param_value(&self, id: ParamID) -> f64 {
        self.controller.as_ref().map_or(0.0, |c| unsafe { c.getParamNormalized(id) })
    }

    pub fn param_display(&self, id: ParamID, value: f64) -> String {
        let Some(ctrl) = &self.controller else { return format!("{value:.3}") };
        let mut s: String128 = [0; 128];
        if unsafe { ctrl.getParamStringByValue(id, value, &mut s) } == kResultOk {
            tchar_to_string(&s)
        } else {
            format!("{value:.3}")
        }
    }

    /// Sets a parameter from the host UI: updates the controller and the audio processor.
    pub fn set_param(&self, id: ParamID, value: f64) {
        if let Some(ctrl) = &self.controller {
            unsafe { ctrl.setParamNormalized(id, value) };
        }
        if let Ok(mut tx) = self.to_audio.lock() {
            let _ = tx.push(ParamChange { id, value });
        }
    }

    /// Call regularly on the UI thread: forwards processor output parameters and restart requests.
    pub fn idle(&mut self) {
        while let Ok(ch) = self.from_audio.pop() {
            if let Some(ctrl) = &self.controller {
                unsafe { ctrl.setParamNormalized(ch.id, ch.value) };
            }
        }
        let flags = self.restart_flags.swap(0, Ordering::AcqRel);
        if flags != 0 {
            log::info!("{}: restartComponent({})", self.info.name, host::restart_flags_str(flags));
            use RestartFlags_::*;
            if flags & (kParamTitlesChanged | kReloadComponent | kIoChanged) != 0 {
                self.reload_params();
            }
            // These only take effect after a deactivate / activate cycle.
            if flags & (kReloadComponent | kIoChanged | kLatencyChanged | kMidiCCAssignmentChanged) != 0 {
                self.restart_pending = true;
            }
        }
    }

    #[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
    pub fn controller(&self) -> Option<&ComPtr<IEditController>> {
        self.controller.as_ref()
    }

    /// Component (processor) state and edit-controller state, as the plugin serializes them.
    pub fn save_states(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let get = |f: &dyn Fn(*mut IBStream) -> i32| {
            let stream = ComWrapper::new(MemoryStream::default());
            let sp = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
            (f(sp) == kResultOk).then(|| stream.bytes())
        };
        let component = get(&|sp| unsafe { self.component.getState(sp) });
        let controller = match (&self.controller, self.separate_controller) {
            // A single-object plugin keeps everything in the component state.
            (Some(ctrl), true) => get(&|sp| unsafe { ctrl.getState(sp) }),
            _ => None,
        };
        (component, controller)
    }

    /// Restores what [`save_states`](Self::save_states) returned.
    pub fn load_states(&mut self, component: Option<Vec<u8>>, controller: Option<Vec<u8>>) {
        unsafe {
            if let Some(bytes) = component {
                let stream = ComWrapper::new(MemoryStream::from_bytes(bytes));
                let sp = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
                if self.component.setState(sp) == kResultOk
                    && let Some(ctrl) = &self.controller
                {
                    stream.rewind();
                    ctrl.setComponentState(sp);
                }
            }
            if let (Some(bytes), Some(ctrl)) = (controller, &self.controller) {
                let stream = ComWrapper::new(MemoryStream::from_bytes(bytes));
                ctrl.setState(stream.as_com_ref::<IBStream>().unwrap().as_ptr());
            }
        }
        self.reload_params();
    }
}

/// Both VST3 states in one blob: `NPV3`, then each state as a little-endian u32 length
/// (`u32::MAX` for none) and its bytes.
pub fn pack_states(component: Option<&[u8]>, controller: Option<&[u8]>) -> Vec<u8> {
    let mut out = b"NPV3".to_vec();
    for part in [component, controller] {
        match part {
            Some(b) => {
                out.extend((b.len() as u32).to_le_bytes());
                out.extend(b);
            }
            None => out.extend(u32::MAX.to_le_bytes()),
        }
    }
    out
}

/// Component and controller state, either of which may be missing.
type States = (Option<Vec<u8>>, Option<Vec<u8>>);

fn unpack_states(data: &[u8]) -> Result<States> {
    let mut rest = data.strip_prefix(b"NPV3").ok_or_else(|| anyhow!("not a VST3 state"))?;
    let mut part = || -> Result<Option<Vec<u8>>> {
        let (len, tail) = rest.split_first_chunk::<4>().ok_or_else(|| anyhow!("truncated state"))?;
        let len = u32::from_le_bytes(*len);
        if len == u32::MAX {
            rest = tail;
            return Ok(None);
        }
        let len = len as usize;
        if tail.len() < len {
            bail!("truncated state");
        }
        let (bytes, tail) = tail.split_at(len);
        rest = tail;
        Ok(Some(bytes.to_vec()))
    };
    Ok((part()?, part()?))
}

impl plugins::Instance for Vst3Plugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn params(&self) -> &[ParamDesc] {
        &self.params
    }

    fn param_value(&self, id: u32) -> f64 {
        Vst3Plugin::param_value(self, id)
    }

    fn param_display(&self, id: u32, value: f64) -> String {
        Vst3Plugin::param_display(self, id, value)
    }

    fn set_param(&mut self, id: u32, value: f64) {
        Vst3Plugin::set_param(self, id, value)
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn midi_ports(&self) -> (bool, bool) {
        self.midi
    }

    fn idle(&mut self) {
        Vst3Plugin::idle(self)
    }

    fn wants_restart(&mut self) -> bool {
        std::mem::take(&mut self.restart_pending)
    }

    fn restart(&mut self, sample_rate: f64, max_block: usize) -> Result<Box<dyn crate::engine::Processor>> {
        // The processor has stopped processing on the audio thread already.
        unsafe { self.component.setActive(0) };
        let proc = self.activate(sample_rate, max_block)?;
        self.reload_params();
        Ok(Box::new(proc))
    }

    fn save_state(&self) -> Option<Vec<u8>> {
        let (c, e) = self.save_states();
        Some(pack_states(c.as_deref(), e.as_deref()))
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let (c, e) = unpack_states(data)?;
        self.load_states(c, e);
        Ok(())
    }

    fn has_editor(&self) -> bool {
        self.controller.is_some()
    }

    fn open_editor(&mut self, placement: Placement) -> Result<Box<dyn plugins::Editor>> {
        let ctrl = self.controller.as_ref().ok_or_else(|| anyhow!("{} has no edit controller", self.info.name))?;
        // Wine misplaces mouse input in embedded windows unless told where they are.
        Ok(Box::new(editor::Editor::open(ctrl, &self.info.name, placement, self.info.is_bridged())?))
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // The processor must already be gone from the audio thread at this point.
        unsafe {
            if let Some(audio) = self.component.cast::<IAudioProcessor>() {
                audio.setProcessing(0);
            }
            self.component.setActive(0);
            if let Some((a, b)) = self.connections.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(ctrl) = self.controller.take() {
                ctrl.setComponentHandler(std::ptr::null_mut());
                if self.separate_controller {
                    ctrl.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_pack_and_unpack() {
        let packed = pack_states(Some(b"abc"), None);
        assert_eq!(unpack_states(&packed).unwrap(), (Some(b"abc".to_vec()), None));
        let packed = pack_states(None, Some(b""));
        assert_eq!(unpack_states(&packed).unwrap(), (None, Some(Vec::new())));
        assert!(unpack_states(b"NPV3\x05\x00\x00\x00ab").is_err());
        assert_eq!(parse_cid_hex(&cid_hex(&[1, -2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])).unwrap()[1], -2);
    }
}
