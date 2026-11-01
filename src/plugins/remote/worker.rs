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
                    | Command::Zoom(_, _)
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
                        if captured && !matches!(placement, Placement::Embedded(_)) {
                            bail!("captured editor must be presented inside the graph")
                        }
                        surface = None;
                        editor = None;
                        let source = instance.open_editor(if captured {
                            Placement::Window
                        } else {
                            placement
                        })?;
                        editor = Some(if captured {
                            crate::plugins::surface::capture(source, placement)?
                        } else {
                            source
                        });
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
                    Command::Zoom(token, f) => {
                        if !f.is_finite() {
                            bail!("invalid editor zoom")
                        }
                        if token == editor_id
                            && let Some(e) = editor.as_mut()
                            && !e.is_captured()
                        {
                            e.set_zoom(f.clamp(0.1, 8.0));
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
                    map.invalidate();
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
fn request_pending(socket: &mut TcpStream) -> Result<bool> {
    let timeout = socket.read_timeout()?;
    socket.set_read_timeout(Some(Duration::from_millis(8)))?;
    let result = socket.peek(&mut [0]);
    socket.set_read_timeout(timeout)?;
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
        || e.is_some() != cached.editor_open
        || e.as_ref().map(|e| e.size()).unwrap_or((640, 480)) != cached.editor_size
        || editor_id != cached.editor_id
        || e.as_ref().and_then(|e| e.capture_error()) != cached.editor_error.as_deref()
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
    Ok(Metadata {
        params,
        values,
        latency: i.latency(),
        midi: i.midi_ports(),
        instrument: i.info().is_instrument(),
        editor: i.has_editor(),
        editor_size: e.as_ref().map(|e| e.size()).unwrap_or((640, 480)),
        editor_open: e.is_some(),
        editor_id,
        editor_error,
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
                    // Local parameter setters enqueue to DSP. Consume those queues before
                    // saving/restarting, even when no parent input quantum is available.
                    // This child-only silent quantum is discarded, not published as audio.
                    inputs[0].clear(Q);
                    outputs[0].clear(Q);
                    output_midi.clear();
                    proc.process_midi(Q, &mut inputs, &mut outputs, &[], &mut output_midi);
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
                proc.process_midi(Q, &mut inputs, &mut outputs, &input_midi, &mut output_midi);
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
        assert!(request_pending(&mut worker).unwrap());
        let request: Request = protocol::receive(&mut worker).unwrap();
        assert!(matches!(request.command, Command::Poll));
        assert_eq!(request.id, 1);
        assert!(!request_pending(&mut worker).unwrap());
        drop(parent);
        assert!(request_pending(&mut worker).is_err());
    }
}
