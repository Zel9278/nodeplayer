use super::{
    protocol::{
        self, Command, Hello, Init, MAX_PARAMS, MAX_STATE, Metadata, Reply, Request, VERSION,
    },
    shared::{self, INPUT, MAX_LATENCY, OUTPUT, Q, SLOTS, Shared, WORKING},
};
use crate::{
    engine::{MidiBuf, MidiEvent, Processor, StereoBuf},
    plugins::{Editor, Host, Instance},
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
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
/// Internal CLI args: <loopback-address> <256-bit-token> <mmap-path> <generation>.
/// This entry must execute before the normal app creates windows/audio/extensions.
pub fn worker_main(args: &[OsString]) -> Result<()> {
    if args.len() != 4 {
        bail!("invalid plugin worker arguments")
    }
    let address: SocketAddr = args[0]
        .to_str()
        .context("invalid worker address")?
        .parse()?;
    if !address.ip().is_loopback() {
        bail!("worker control endpoint must be loopback")
    }
    let token = args[1]
        .to_str()
        .context("invalid worker credential")?
        .to_owned();
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid worker credential")
    }
    let path = PathBuf::from(&args[2]);
    let generation: u64 = args[3].to_str().context("invalid generation")?.parse()?;
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
                    std::process::exit(74);
                }
            }
        })?;
    let map = Arc::new(Shared::open(&path, generation)?);
    #[cfg(windows)]
    let _com = ComInit::new();
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
    let mut editor_id = 0;
    let initial_metadata = metadata(instance.as_ref(), &editor, editor_id)?;
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
        pump_native();
        instance.idle();
        if editor.as_mut().is_some_and(|e| !e.idle()) {
            editor = None;
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
        let result: Result<()> = (|| {
            match req.command {
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
                        .map_err(|_| anyhow::anyhow!("child audio replacement queue timeout"))?;
                    resume(&ctl)?;
                    loaded?;
                }
                Command::Checkpoint => {
                    park(&ctl, true)?;
                    map.invalidate();
                    // save_state may hang: supervisor will kill/reap, UI uses previous cache.
                    reply.state = instance.save_state().filter(|b| b.len() <= MAX_STATE);
                    let proc = instance.restart(init.sr as f64, crate::engine::MAX_BLOCK)?;
                    audio_tx
                        .send_timeout(AudioMessage::Replace(proc), Duration::from_secs(1))
                        .map_err(|_| anyhow::anyhow!("child audio replacement queue timeout"))?;
                    resume(&ctl)?;
                }
                Command::Open { token, placement } => {
                    if token == 0 || token <= editor_id {
                        bail!("invalid editor token")
                    }
                    editor = None;
                    editor = Some(instance.open_editor(placement)?);
                    editor_id = token;
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
                    {
                        e.set_zoom(f.clamp(0.1, 8.0));
                    }
                }
                Command::Close(token) => {
                    if token == editor_id {
                        editor = None;
                    }
                }
                Command::Shutdown => {
                    ctl.stop.store(true, Ordering::Release);
                    let _ = audio_tx.try_send(AudioMessage::Stop);
                    return Ok(());
                }
            }
            if instance.wants_restart() {
                park(&ctl, true)?;
                map.invalidate();
                let proc = instance.restart(init.sr as f64, crate::engine::MAX_BLOCK)?;
                audio_tx
                    .send_timeout(AudioMessage::Replace(proc), Duration::from_secs(1))
                    .map_err(|_| anyhow::anyhow!("child audio replacement queue timeout"))?;
                resume(&ctl)?;
            }
            reply.metadata = Some(metadata(instance.as_ref(), &editor, editor_id)?);
            Ok(())
        })();
        if let Err(e) = result {
            reply.error = Some(format!("{e:#}"));
        }
        protocol::send(&mut socket, &reply)?;
        if ctl.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if ctl.failed.load(Ordering::Acquire) {
            bail!("child audio thread failed")
        }
    }
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
fn pump_native() {
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
}
#[cfg(not(windows))]
fn pump_native() {}
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
