use super::{
    protocol::{
        self, Command, Hello, Init, MAX_PARAMS, MAX_STATE, Metadata, Reply, Request, VERSION,
    },
    shared::{self, INPUT, MAX_LATENCY, OUTPUT, Q, SLOTS, Shared, WORKING},
    surface::FrameMap,
};
use crate::{
    engine::{MidiBuf, MidiEvent, Processor, StereoBuf},
    plugins::{Editor, Host, Instance, Placement},
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    io::BufRead,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

struct AudioControl {
    park: AtomicBool,
    parked: AtomicBool,
    stop: AtomicBool,
    failed: AtomicBool,
    gate: AtomicU32,
}
enum AudioMessage {
    Replace(Box<dyn Processor>),
    Stop,
}
struct AudioOwner {
    handle: Option<std::thread::JoinHandle<()>>,
    ctl: Arc<AudioControl>,
}
impl Drop for AudioOwner {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // Child-only join is allowed: parent supervisor kills a wedged child on its deadline.
    }
}
/// Internal CLI args: <loopback-address> <mmap-path> <generation>; token is first stdin line.
/// This entry must execute before the normal app creates windows/audio/extensions.
pub fn worker_main(args: &[OsString]) -> Result<()> {
    // This entry bypasses winit's event loop and its autorelease pool management.
    with_native_pool(|| worker_main_inner(args))
}
fn with_native_pool<T>(operation: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    {
        objc2::rc::autoreleasepool(|_| operation())
    }
    #[cfg(not(target_os = "macos"))]
    {
        operation()
    }
}
fn worker_main_inner(args: &[OsString]) -> Result<()> {
    #[cfg(windows)]
    crate::plugins::window::initialize_worker_dpi()?;
    if args.len() != 3 {
        bail!("invalid plugin worker arguments")
    }
    let address: SocketAddr = args[0]
        .to_str()
        .context("invalid worker address")?
        .parse()?;
    if !address.ip().is_loopback() {
        bail!("worker control endpoint must be loopback")
    }
    let mut token = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut token)
        .context("read worker credential")?;
    let token = token.trim_end_matches(['\r', '\n']).to_owned();
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid worker credential")
    }
    let path = PathBuf::from(&args[1]);
    let generation: u64 = args[2].to_str().context("invalid generation")?.parse()?;
    // Open before connecting so parent can unlink immediately after handshake.
    let map = Arc::new(Shared::open(&path, generation)?);
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    protocol::send(
        &mut socket,
        &Hello {
            version: VERSION,
            token,
        },
    )?;
    let init: Init = protocol::receive(&mut socket)?;
    if init.version != VERSION
        || init.quantum != Q
        || init.delay != shared::DELAY
        || init.slots != SLOTS
        || init.generation != generation
        || !init.sr.is_finite()
        || init.sr <= 0.0
    {
        bail!("incompatible worker init")
    }
    // Install before any plugin call: a hung create/initial metadata must not orphan
    // the child if its parent dies and OS job assignment was unavailable.
    let heartbeat = Arc::new(std::sync::Mutex::new(Instant::now()));
    let startup_done = Arc::new(AtomicBool::new(false));
    let watch = heartbeat.clone();
    let watch_started = startup_done.clone();
    std::thread::Builder::new()
        .name("worker-parent-watch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let timeout = if watch_started.load(Ordering::Acquire) {
                    Duration::from_secs(20)
                } else {
                    Duration::from_secs(35)
                };
                if watch.lock().map_or(true, |t| t.elapsed() > timeout) {
                    unsafe { libc::_exit(74) };
                }
            }
        })?;
    #[cfg(windows)]
    let _com = ComInit::new();
    #[cfg(target_os = "macos")]
    crate::plugins::surface::initialize_worker_app()?;
    let mut host = Host::default();
    // Resolve scan-cache-missing identity only inside child, never in the parent.
    let mut info = init.info;
    if info.category.is_empty()
        && let Ok(classes) = host.classes(info.format, &info.path)
        && let Some(actual) = classes.into_iter().find(|x| x.id == info.id)
    {
        info = actual;
    }
    let created = host.create_local(&info, init.sr, crate::engine::MAX_BLOCK)?;
    let mut instance = created.instance;
    let ctl = Arc::new(AudioControl {
        park: AtomicBool::new(false),
        parked: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        gate: AtomicU32::new(0),
    });
    let (audio_tx, audio_rx) = crossbeam_channel::bounded(2);
    let audio_ctl = ctl.clone();
    let audio_map = map.clone();
    let audio = std::thread::Builder::new()
        .name("isolated-audio".into())
        .spawn(move || audio_loop(created.processor, audio_map, audio_ctl, audio_rx))?;
    // Declared after instance: audio joins/deactivates before Instance unloads plugin code.
    let _audio_owner = AudioOwner {
        handle: Some(audio),
        ctl: ctl.clone(),
    };
    let mut editor: Option<Box<dyn Editor>> = None;
    // Drop the image mapping before its native source editor on every exit path.
    let mut surface: Option<FrameMap> = None;
    let mut editor_id = 0;
    let initial_metadata = metadata(instance.as_ref(), &editor, editor_id)?;
    let mut metadata_cache = initial_metadata.clone();
    protocol::send(
        &mut socket,
        &Reply {
            version: VERSION,
            generation,
            id: 0,
            metadata: Some(initial_metadata),
            state: None,
            display: None,
            error: None,
        },
    )?;
    let mut last_id = 0;
    *heartbeat.lock().unwrap() = Instant::now();
    startup_done.store(true, Ordering::Release);
    loop {
        // Drain native temporaries on quiet polls, command replies, and error paths.
        if with_native_pool(|| -> Result<bool> {
            pump_native()?;
            instance.idle();
            if editor.as_mut().is_some_and(|e| !e.idle()) {
                surface = None;
                editor = None;
            }
            if let (Some(editor), Some(surface)) = (editor.as_mut(), surface.as_ref())
                && let Some(frame) = editor.take_frame()
            {
                surface.publish(&frame)?;
            }
            // Poll only for the start of a control frame. A quiet parent must not stop the
            // worker's AppKit/native event pump or its independently paced capture updates.
            if !request_pending(&mut socket)? {
                // Keep native input/timers and capture responsive between commands. This
                // wait is below the 240 Hz capture period and avoids an idle busy loop.
                std::thread::sleep(Duration::from_micros(500));
                return Ok(false);
            }
            let req: Request = match protocol::receive(&mut socket) {
                Ok(r) => r,
                Err(e) => {
                    ctl.stop.store(true, Ordering::Release);
                    return Err(e);
                }
            };
            *heartbeat.lock().unwrap() = Instant::now();
            if req.version != VERSION || req.generation != generation || req.id <= last_id {
                bail!("invalid worker request generation/sequence")
            }
            last_id = req.id;
            let mut reply = Reply {
                version: VERSION,
                generation,
                id: req.id,
                metadata: None,
                state: None,
                display: None,
                error: None,
            };
            let command = req.command.clone();
            let refresh_metadata = matches!(
                &command,
                Command::SetParam(_, _)
                    | Command::Load(_)
                    | Command::Open { .. }
                    | Command::Place(_, _)
                    | Command::DisplayScale(_, _)
                    | Command::Close(_)
            );
            let result: Result<()> = (|| {
                match command {
                    Command::Poll => {}
                    Command::SetParam(id, v) => {
                        if !v.is_finite() {
                            bail!("invalid parameter value")
                        }
                        instance.set_param(id, v.clamp(0.0, 1.0));
                    }
                    Command::Display(id, v) => {
                        if !v.is_finite() {
                            bail!("invalid display value")
                        }
                        let mut s = instance.param_display(id, v);
                        truncate(&mut s, 4096);
                        reply.display = Some((id, v, s));
                    }
                    Command::Load(data) => {
                        if data.len() > MAX_STATE {
                            bail!("state exceeds limit")
                        }
                        park(&ctl, false)?;
                        map.invalidate();
                        let loaded = instance.load_state(&data);
                        let proc = instance.restart(init.sr as f64, crate::engine::MAX_BLOCK)?;
                        audio_tx
                            .send_timeout(AudioMessage::Replace(proc), Duration::from_secs(1))
                            .map_err(|_| {
                                anyhow::anyhow!("child audio replacement queue timeout")
                            })?;
                        resume(&ctl)?;
                        loaded?;
                    }
                    Command::Checkpoint => {
                        // The command loop is the plugin main thread; CLAP/VST3 state APIs
                        // are main-thread interfaces and do not require taking DSP offline.
                        reply.state = instance.save_state();
                        if reply.state.as_ref().is_some_and(|b| b.len() > MAX_STATE) {
                            reply.state = None;
                            bail!("state exceeds limit")
                        }
                    }
                    Command::Open {
                        token,
                        placement,
                        captured,
                    } => {
                        if token == 0 || token <= editor_id {
                            bail!("invalid editor token")
                        }
                        if captured != matches!(placement, Placement::Embedded(_)) {
                            bail!("embedded editor requires captured presentation")
                        }
                        surface = None;
                        drop(editor.take());
                        // An unsupported renderer or native GUI setup error belongs to
                        // this editor, not to the healthy audio processor. Retain an
                        // error proxy so metadata and Pop out remain usable.
                        editor = Some(editor_or_error(placement, captured, || {
                            let source = instance.open_editor(if captured {
                                Placement::CaptureSource
                            } else {
                                placement
                            })?;
                            if captured {
                                crate::plugins::surface::capture(source, placement)
                            } else {
                                Ok(source)
                            }
                        }));
                        editor_id = token;
                    }
                    Command::SetSurface(token, descriptor) => {
                        if token != editor_id
                            || descriptor.token != token
                            || descriptor.generation != generation
                        {
                            bail!("invalid editor surface generation/token")
                        }
                        if !editor.as_ref().is_some_and(|editor| editor.is_captured()) {
                            bail!("editor does not support captured presentation")
                        }
                        surface = Some(FrameMap::open(&descriptor)?);
                    }
                    Command::Pointer {
                        generation: input_generation,
                        token,
                        event,
                    } => {
                        event.validate()?;
                        if input_generation == generation
                            && token == editor_id
                            && let Some(editor) =
                                editor.as_mut().filter(|editor| editor.is_captured())
                        {
                            editor.pointer(event)?;
                        }
                    }
                    Command::Place(token, spot) => {
                        if spot.as_ref().is_some_and(|s| s.visible.len() > 256) {
                            bail!("too many clipping rectangles")
                        }
                        if token == editor_id
                            && let Some(e) = editor.as_mut()
                        {
                            e.place(spot.as_ref());
                        }
                    }
                    Command::DisplayScale(token, factor) => {
                        if !crate::plugins::surface::valid_display_scale(factor) {
                            bail!("invalid editor display density")
                        }
                        if token == editor_id && let Some(editor) = editor.as_mut() {
                            editor.set_display_scale(factor);
                        }
                    }
                    Command::Close(token) => {
                        if token == editor_id {
                            surface = None;
                            editor = None;
                        }
                    }
                    Command::Shutdown => {
                        ctl.stop.store(true, Ordering::Release);
                        let _ = audio_tx.try_send(AudioMessage::Stop);
                        return Ok(());
                    }
                }
                let restart = instance.wants_restart();
                if restart {
                    park(&ctl, true)?;
                    // A warm activation retains the plugin state and its sample timeline.
                    // Keep queued bridge audio; only destructive Load invalidates an epoch.
                    let proc = instance.restart(init.sr as f64, crate::engine::MAX_BLOCK)?;
                    audio_tx
                        .send_timeout(AudioMessage::Replace(proc), Duration::from_secs(1))
                        .map_err(|_| anyhow::anyhow!("child audio replacement queue timeout"))?;
                    resume(&ctl)?;
                }
                // Poll replies reuse the last snapshot. Commands that can change
                // parameter displays, latency, or editor state refresh it once.
                if restart || refresh_metadata || matches!(&req.command, Command::Poll) {
                    let changed = if matches!(&req.command, Command::Poll) {
                        metadata_changed(instance.as_ref(), &editor, editor_id, &metadata_cache)
                    } else {
                        true
                    };
                    if changed {
                        let next_metadata = metadata(instance.as_ref(), &editor, editor_id)?;
                        if serde_json::to_vec(&next_metadata)?
                            != serde_json::to_vec(&metadata_cache)?
                        {
                            metadata_cache = next_metadata;
                            reply.metadata = Some(metadata_cache.clone());
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                reply.error = Some(format!("{e:#}"));
            }
            protocol::send(&mut socket, &reply)?;
            if ctl.stop.load(Ordering::Acquire) {
                return Ok(true);
            }
            if ctl.failed.load(Ordering::Acquire) {
                bail!("child audio thread failed")
            }
            Ok(false)
        })? {
            return Ok(());
        }
    }
}

fn editor_or_error(
    placement: Placement,
    captured: bool,
    open: impl FnOnce() -> Result<Box<dyn Editor>>,
) -> Box<dyn Editor> {
    open().unwrap_or_else(|error| {
        Box::new(EditorOpenError {
            placement,
            captured,
            message: format!("Unable to open editor: {error:#}; try Pop out")
                .chars().take(1024).collect(),
        })
    })
}

struct EditorOpenError {
    placement: Placement,
    captured: bool,
    message: String,
}

impl Editor for EditorOpenError {
    fn placement(&self) -> Placement { self.placement }
    fn size(&self) -> (u32, u32) { (640, 480) }
    fn ready(&self) -> bool { false }
    fn is_captured(&self) -> bool { self.captured }
    fn capture_error(&self) -> Option<&str> { Some(&self.message) }
    fn place(&mut self, _: Option<&crate::plugins::Spot>) {}
    fn idle(&mut self) -> bool { true }
}

fn request_pending(socket: &mut TcpStream) -> Result<bool> {
    // Readiness is separate from the framed request deadline. A blocking peek here used
    // to stall every native UI/capture service pass for 8 ms with a quiet parent.
    socket.set_nonblocking(true)?;
    let result = socket.peek(&mut [0]);
    socket.set_nonblocking(false)?;
    match result {
        Ok(0) => bail!("control connection closed"),
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}
fn metadata_changed(
    i: &dyn Instance,
    e: &Option<Box<dyn Editor>>,
    editor_id: u64,
    cached: &Metadata,
) -> bool {
    if i.params().len() != cached.values.len()
        || i.latency() != cached.latency
        || i.midi_ports() != cached.midi
        || i.has_editor() != cached.editor
        || e.as_ref().is_some_and(|editor| editor.ready()) != cached.editor_open
        || e.as_ref().map(|e| e.size()).unwrap_or((640, 480)) != cached.editor_size
        || editor_id != cached.editor_id
        || e.as_ref().and_then(|e| e.capture_error()) != cached.editor_error.as_deref()
        || e.as_ref().and_then(|e| e.first_capture_error()) != cached.editor_first_error.as_deref()
    {
        return true;
    }
    i.params()
        .iter()
        .zip(&cached.values)
        .any(|(p, (id, value, _))| p.id != *id || i.param_value(p.id) != *value)
}
fn metadata(i: &dyn Instance, e: &Option<Box<dyn Editor>>, editor_id: u64) -> Result<Metadata> {
    if i.params().len() > MAX_PARAMS {
        bail!("parameter count exceeds remote limit")
    }
    if i.latency() as usize > MAX_LATENCY {
        bail!("plugin latency exceeds remote fallback budget")
    }
    let mut params = i.params().to_vec();
    for p in &mut params {
        truncate(&mut p.title, 4096);
        truncate(&mut p.units, 256);
    }
    let values = params
        .iter()
        .map(|p| {
            let v = i.param_value(p.id);
            let mut d = i.param_display(p.id, v);
            truncate(&mut d, 4096);
            (p.id, v, d)
        })
        .collect();
    let mut editor_error = e
        .as_ref()
        .and_then(|e| e.capture_error())
        .map(str::to_owned);
    if let Some(error) = editor_error.as_mut() {
        truncate(error, 4096);
    }
    let editor_first_error = e
        .as_ref()
        .and_then(|e| e.first_capture_error())
        .map(|error| error.chars().take(1024).collect());
    Ok(Metadata {
        params,
        values,
        latency: i.latency(),
        midi: i.midi_ports(),
        instrument: i.info().is_instrument(),
        editor: i.has_editor(),
        editor_size: e.as_ref().map(|e| e.size()).unwrap_or((640, 480)),
        editor_open: e.as_ref().is_some_and(|editor| editor.ready()),
        editor_id,
        editor_error,
        editor_first_error,
    })
}
fn truncate(s: &mut String, max: usize) {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s.truncate(n);
}
fn resume(c: &AudioControl) -> Result<()> {
    c.park.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(2);
    while c.parked.load(Ordering::Acquire) {
        if Instant::now() > end {
            bail!("child DSP resume timeout")
        }
        std::thread::sleep(Duration::from_micros(250));
    }
    Ok(())
}
fn park(c: &AudioControl, flush_pending: bool) -> Result<()> {
    // Publish the drain request before park so its acquire observes both flags.
    c.gate.store(u32::from(flush_pending), Ordering::Relaxed);
    c.park.store(true, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(2);
    while !c.parked.load(Ordering::Acquire) {
        if Instant::now() > end {
            bail!("child DSP failed to quiesce")
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn audio_loop(
    mut proc: Box<dyn Processor>,
    map: Arc<Shared>,
    ctl: Arc<AudioControl>,
    rx: crossbeam_channel::Receiver<AudioMessage>,
) {
    let mut inputs = vec![StereoBuf::new()];
    let mut outputs = vec![StereoBuf::new()];
    let mut input_midi = Vec::with_capacity(shared::MIDI);
    let mut output_midi = MidiBuf::new();
    let mut stopped = false;
    loop {
        if ctl.stop.load(Ordering::Acquire) {
            if !stopped {
                proc.deactivate();
            }
            return;
        }
        if ctl.park.load(Ordering::Acquire) || stopped {
            if !stopped {
                if ctl.gate.swap(0, Ordering::AcqRel) != 0 {
                    // Consume active parameter changes on their legal thread without
                    // inventing input samples or advancing/overlapping process clocks.
                    proc.flush_params();
                }
                proc.deactivate();
                stopped = true;
            }
            ctl.parked.store(true, Ordering::Release);
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(AudioMessage::Replace(next)) => {
                    proc = next;
                    stopped = false;
                    while ctl.park.load(Ordering::Acquire) && !ctl.stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_micros(250));
                    }
                    ctl.parked.store(false, Ordering::Release);
                }
                Ok(AudioMessage::Stop) => return,
                _ => {}
            }
            continue;
        }
        ctl.parked.store(false, Ordering::Release);
        let mut did_work = false;
        // Sequence order, not array order: parent can reuse an earlier free slot while later
        // slots remain occupied. Find oldest input in a bounded scan.
        let mut selected = None;
        let mut oldest = u64::MAX;
        for i in 0..SLOTS {
            let s = map.slot(i);
            if s.state.load(Ordering::Acquire) == INPUT {
                let p = unsafe { s.payload() };
                if p.sequence < oldest {
                    oldest = p.sequence;
                    selected = Some(i);
                }
            }
        }
        if let Some(i) = selected {
            let slot = map.slot(i);
            if slot
                .state
                .compare_exchange(INPUT, WORKING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let p = unsafe { slot.payload() };
                if p.generation != map.epoch() {
                    slot.state.store(shared::FREE, Ordering::Release);
                    continue;
                }
                if p.frames as usize != Q || p.midi_count as usize > shared::MIDI {
                    ctl.failed.store(true, Ordering::Release);
                    return;
                }
                let Some(transport) = p.transport.decode() else {
                    ctl.failed.store(true, Ordering::Release);
                    return;
                };
                inputs[0].l[..Q].copy_from_slice(&p.left);
                inputs[0].r[..Q].copy_from_slice(&p.right);
                outputs[0].clear(Q);
                input_midi.clear();
                output_midi.clear();
                for e in &p.midi[..p.midi_count as usize] {
                    if (e.offset as usize) < Q {
                        input_midi.push(MidiEvent {
                            time: e.offset,
                            data: e.data,
                        });
                    }
                }
                proc.set_transport(transport);
                proc.process_timed(
                    p.first_sample,
                    Q,
                    &mut inputs,
                    &mut outputs,
                    &input_midi,
                    &mut output_midi,
                );
                p.left.copy_from_slice(&outputs[0].l[..Q]);
                p.right.copy_from_slice(&outputs[0].r[..Q]);
                let mut count = 0;
                for e in output_midi
                    .events()
                    .iter()
                    .filter(|e| (e.time as usize) < Q)
                    .take(shared::MIDI)
                {
                    p.midi[count] = shared::WireMidi {
                        offset: e.time,
                        data: e.data,
                        reserved: 0,
                    };
                    count += 1;
                }
                p.midi_count = count as u32;
                slot.state.store(OUTPUT, Ordering::Release);
                map.progressed();
                did_work = true;
            }
        }
        if !did_work {
            std::thread::sleep(Duration::from_micros(250));
        }
    }
}
#[cfg(windows)]
fn pump_native() -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    unsafe {
        let mut msg = MSG::default();
        for _ in 0..256 {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn pump_native() -> Result<()> {
    crate::plugins::surface::pump_worker_app()
}
#[cfg(not(any(windows, target_os = "macos")))]
fn pump_native() -> Result<()> {
    Ok(())
}
#[cfg(windows)]
struct ComInit(bool);
#[cfg(windows)]
impl ComInit {
    fn new() -> Self {
        Self(
            unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                )
            }
            .is_ok(),
        )
    }
}
#[cfg(windows)]
impl Drop for ComInit {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_editor_open_stays_available_as_a_bounded_error() {
        let mut editor = editor_or_error(Placement::Embedded(1), true, || {
            anyhow::bail!("capture backend unavailable")
        });
        assert!(editor.is_captured());
        assert!(!editor.ready());
        assert!(editor.idle(), "an editor setup error must not fault its instance");
        assert!(editor.take_frame().is_none());
        assert!(editor.capture_error().unwrap().contains("capture backend unavailable"));
        let long = editor_or_error(Placement::Embedded(1), true, || {
            anyhow::bail!("界".repeat(4096))
        });
        assert!(long.capture_error().unwrap().len() <= 4096);
    }

    #[test]
    fn metadata_reports_capture_failure_that_recovers_before_polling() {
        use crate::plugins::surface::CaptureErrors;
        use crate::plugins::{Format, ParamDesc, PluginInfo};
        struct TestInstance(PluginInfo);
        impl Instance for TestInstance {
            fn info(&self) -> &PluginInfo {
                &self.0
            }
            fn params(&self) -> &[ParamDesc] {
                &[]
            }
            fn param_value(&self, _: u32) -> f64 {
                0.0
            }
            fn param_display(&self, _: u32, _: f64) -> String {
                String::new()
            }
            fn set_param(&mut self, _: u32, _: f64) {}
            fn save_state(&self) -> Option<Vec<u8>> {
                None
            }
            fn load_state(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        #[derive(Default)]
        struct FlakyEditor {
            errors: CaptureErrors,
            ticks: usize,
        }
        impl Editor for FlakyEditor {
            fn placement(&self) -> Placement {
                Placement::CaptureSource
            }
            fn size(&self) -> (u32, u32) {
                (640, 480)
            }
            fn capture_error(&self) -> Option<&str> {
                self.errors.current()
            }
            fn first_capture_error(&self) -> Option<&str> {
                self.errors.first()
            }
            fn place(&mut self, _: Option<&crate::plugins::Spot>) {}
            fn idle(&mut self) -> bool {
                self.ticks += 1;
                if self.ticks == 1 {
                    self.errors
                        .record("transient native readback failure".into());
                } else {
                    self.errors.recovered();
                }
                true
            }
        }
        let instance = TestInstance(PluginInfo {
            format: Format::Vst3,
            id: "metadata-test".into(),
            name: "metadata-test".into(),
            vendor: String::new(),
            category: String::new(),
            path: PathBuf::new(),
        });
        let mut editor: Option<Box<dyn Editor>> = Some(Box::<FlakyEditor>::default());
        let cached = metadata(&instance, &editor, 21).unwrap();
        assert!(!metadata_changed(&instance, &editor, 21, &cached));
        // Native capture runs twice before the supervisor's next metadata poll.
        assert!(editor.as_mut().unwrap().idle());
        assert!(editor.as_mut().unwrap().idle());
        assert!(editor.as_ref().unwrap().capture_error().is_none());
        assert!(metadata_changed(&instance, &editor, 21, &cached));
        let recovered = metadata(&instance, &editor, 21).unwrap();
        let wire = serde_json::to_vec(&recovered).unwrap();
        let recovered: Metadata = serde_json::from_slice(&wire).unwrap();
        assert!(recovered.editor_error.is_none());
        assert_eq!(
            recovered.editor_first_error.as_deref(),
            Some("transient native readback failure")
        );
        assert!(!metadata_changed(&instance, &editor, 21, &recovered));
        let reopened: Option<Box<dyn Editor>> = Some(Box::<FlakyEditor>::default());
        let fresh = metadata(&instance, &reopened, 22).unwrap();
        assert!(fresh.editor_first_error.is_none());
    }

    #[test]
    fn request_poll_keeps_native_pump_live_without_consuming_control_bytes() {
        use std::net::{Ipv4Addr, TcpListener};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut parent = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut worker, _) = listener.accept().unwrap();
        let timeout = Some(Duration::from_secs(3));
        worker.set_read_timeout(timeout).unwrap();
        assert!(!request_pending(&mut worker).unwrap());
        assert_eq!(worker.read_timeout().unwrap(), timeout);
        protocol::send(
            &mut parent,
            &Request {
                version: VERSION,
                generation: 1,
                id: 1,
                command: Command::Poll,
            },
        )
        .unwrap();
        // TCP delivery is asynchronous even on loopback. Readiness polling must keep
        // returning promptly until the sent frame actually reaches this socket.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !request_pending(&mut worker).unwrap() {
            assert!(Instant::now() < deadline, "sent control frame was not delivered");
            std::thread::yield_now();
        }
        let request: Request = protocol::receive(&mut worker).unwrap();
        assert!(matches!(request.command, Command::Poll));
        assert_eq!(request.id, 1);
        assert!(!request_pending(&mut worker).unwrap());
        drop(parent);
        // FIN delivery is asynchronous even on loopback. Native servicing must remain
        // nonblocking while the OS makes the closed connection observable.
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Ok(pending) = request_pending(&mut worker) {
            assert!(!pending, "the consumed control frame must not reappear");
            assert!(
                Instant::now() < deadline,
                "closed control socket was not detected"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn audio_worker_applies_transport_from_each_queued_quantum() {
        use crate::engine::{PlaybackState, Transport};
        struct Capture(crossbeam_channel::Sender<(u64, Transport)>, Transport);
        impl Processor for Capture {
            fn set_transport(&mut self, transport: Transport) {
                self.1 = transport;
            }
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {}
            fn process_timed(
                &mut self,
                clock: u64,
                _: usize,
                _: &mut [StereoBuf],
                _: &mut [StereoBuf],
                _: &[MidiEvent],
                _: &mut MidiBuf,
            ) {
                self.0.send((clock, self.1)).unwrap();
            }
        }
        let path = std::env::temp_dir().join(format!(
            "nodeplayer-worker-transport-{}",
            std::process::id()
        ));
        let map = Arc::new(Shared::create(path, 1).unwrap());
        let states = [
            Transport {
                state: PlaybackState::Playing,
                position_seconds: 2.0,
                seconds_per_frame: 1.0 / 48_000.0,
            },
            Transport {
                state: PlaybackState::Paused,
                position_seconds: 2.5,
                seconds_per_frame: 0.0,
            },
            Transport::default(),
        ];
        // Physical slot order differs from the queued processing order.
        for (i, index) in [2, 0, 1].into_iter().enumerate() {
            let slot = map.slot(index);
            let p = unsafe { slot.payload() };
            p.generation = map.epoch();
            p.sequence = i as u64;
            p.first_sample = 1000 + i as u64 * Q as u64;
            p.frames = Q as u32;
            p.midi_count = 0;
            p.transport = states[i].into();
            slot.state.store(INPUT, Ordering::Release);
        }
        let ctl = Arc::new(AudioControl {
            park: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            gate: AtomicU32::new(0),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let (_control_tx, control_rx) = crossbeam_channel::unbounded();
        let child_map = map.clone();
        let child_ctl = ctl.clone();
        let handle = std::thread::spawn(move || {
            audio_loop(
                Box::new(Capture(tx, Transport::default())),
                child_map,
                child_ctl,
                control_rx,
            );
        });
        let received: Vec<_> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)))
            .collect();
        ctl.stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(!ctl.failed.load(Ordering::Acquire));
        for (i, received) in received.into_iter().enumerate() {
            assert_eq!(received.unwrap(), (1000 + i as u64 * Q as u64, states[i]));
        }
    }

    #[test]
    fn audio_worker_restart_flushes_controls_without_fabricating_samples() {
        #[derive(Debug, PartialEq)]
        enum Observation {
            Process(u8, u64, f32, f32),
            Flush(u8),
            Stop(u8),
        }
        struct Probe(u8, crossbeam_channel::Sender<Observation>);
        impl Processor for Probe {
            fn process(&mut self, _: usize, _: &mut [StereoBuf], _: &mut [StereoBuf]) {
                panic!("worker must supply an actual input sample clock");
            }
            fn process_timed(
                &mut self,
                clock: u64,
                n: usize,
                inputs: &mut [StereoBuf],
                outputs: &mut [StereoBuf],
                _: &[MidiEvent],
                _: &mut MidiBuf,
            ) {
                assert_eq!(n, Q);
                outputs[0].l[..n].copy_from_slice(&inputs[0].l[..n]);
                outputs[0].r[..n].copy_from_slice(&inputs[0].r[..n]);
                self.1
                    .send(Observation::Process(
                        self.0,
                        clock,
                        inputs[0].l[0],
                        inputs[0].r[0],
                    ))
                    .unwrap();
            }
            fn flush_params(&mut self) {
                self.1.send(Observation::Flush(self.0)).unwrap();
            }
            fn deactivate(&mut self) {
                self.1.send(Observation::Stop(self.0)).unwrap();
            }
        }
        let path = std::env::temp_dir().join(format!(
            "nodeplayer-worker-warm-restart-{}",
            std::process::id()
        ));
        let map = Arc::new(Shared::create(path, 1).unwrap());
        let publish = |index: usize, sample: u64, value: f32| {
            let slot = map.slot(index);
            let payload = unsafe { slot.payload() };
            payload.generation = map.epoch();
            payload.sequence = sample / Q as u64;
            payload.first_sample = sample;
            payload.frames = Q as u32;
            payload.midi_count = 0;
            payload.left.fill(value);
            payload.right.fill(-value);
            slot.state.store(INPUT, Ordering::Release);
        };
        publish(0, 1024, 0.25);
        let ctl = Arc::new(AudioControl {
            park: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            gate: AtomicU32::new(0),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let (control_tx, control_rx) = crossbeam_channel::bounded(2);
        let child_map = map.clone();
        let child_ctl = ctl.clone();
        let probe = Probe(0, tx.clone());
        let handle = std::thread::spawn(move || {
            audio_loop(Box::new(probe), child_map, child_ctl, control_rx);
        });
        let result = (|| -> Result<Vec<Observation>> {
            let mut seen = vec![rx.recv_timeout(Duration::from_secs(2))?];
            park(&ctl, true)?;
            // Actual queued audio survives parking and resumes at the next sample.
            publish(1, 1024 + Q as u64, 0.75);
            control_tx
                .send(AudioMessage::Replace(Box::new(Probe(1, tx))))
                .map_err(|_| anyhow::anyhow!("worker replacement channel closed"))?;
            resume(&ctl)?;
            for _ in 0..3 {
                seen.push(rx.recv_timeout(Duration::from_secs(2))?);
            }
            Ok(seen)
        })();
        ctl.stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert_eq!(
            result.unwrap(),
            vec![
                Observation::Process(0, 1024, 0.25, -0.25),
                Observation::Flush(0),
                Observation::Stop(0),
                Observation::Process(1, 1024 + Q as u64, 0.75, -0.75),
            ]
        );
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![Observation::Stop(1)]
        );
        assert!(!ctl.failed.load(Ordering::Acquire));
    }
}
