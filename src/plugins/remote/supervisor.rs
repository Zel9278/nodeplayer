use super::{
    super::PluginInfo,
    Core,
    protocol::{self, Command, Hello, Init, Reply, Request, VERSION},
    shared::{self, DELAY, Q, SLOTS, Shared},
    surface::FrameMap,
};
use crate::plugins::surface::PointerKind;
use anyhow::{Context, Result, bail};
use crossbeam_channel::Receiver;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    collections::HashMap,
    io::Write,
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static MAP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn run(core: Arc<Core>, rx: Receiver<Command>, info: PluginInfo, sr: f32) {
    let mut generation = 0u64;
    let mut journal = HashMap::new();
    while !core.stop.load(Ordering::Acquire) {
        generation = generation.wrapping_add(1);
        core.recover.store(false, Ordering::Release);
        omit_diagnostic_fault_triggers(
            &mut journal,
            crate::diagnostics::active().is_some(),
            &info.id,
        );
        status(&core, "starting");
        clear_surface(&core, None);
        let result = session(&core, &rx, &info, sr, generation, &mut journal);
        core.ready.store(false, Ordering::Release);
        clear_surface(&core, None);
        if core.stop.load(Ordering::Acquire) {
            break;
        }
        status(
            &core,
            &format!(
                "faulted: {}",
                result
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_else(|| "worker stopped".into())
            ),
        );
        // Explicit user restart only. Retain most recent state/parameter requests, bounded.
        while !core.stop.load(Ordering::Acquire) && !core.recover.swap(false, Ordering::AcqRel) {
            if let Ok(c @ (Command::SetParam(..) | Command::Load(..))) =
                rx.recv_timeout(Duration::from_millis(25))
            {
                remember(&core, &mut journal, &c)
            }
        }
    }
    status(&core, "stopped");
    // Stay as final owner until UI, editor and processor proxies retire. Their Drop only
    // decrements Arc; the supervisor performs mmap destruction and file unlink off RT.
    loop {
        reclaim(&core);
        if Arc::strong_count(&core) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn omit_diagnostic_fault_triggers(
    journal: &mut HashMap<u32, f64>,
    diagnostic: bool,
    plugin_id: &str,
) {
    // Only diagnostic fixtures reserve >=100 for one-shot fault triggers. The
    // parent's active run is authoritative; the env marker is child-only.
    if diagnostic && plugin_id == "org.nodeplayer.test-fixture" {
        journal.retain(|id, _| *id < 100);
    }
}
fn reclaim(core: &Core) {
    let mut retired = core.retired.lock().unwrap();
    retired.retain(|map| Arc::strong_count(map) > 1);
}
fn status(core: &Core, s: &str) {
    if let Ok(mut snapshot) = core.snapshot.lock() {
        snapshot.status = s.to_owned();
    }
    if let Some(run) = crate::diagnostics::active() {
        let _ = run.event("worker-status",serde_json::json!({"status":s,"underruns":core.underruns.load(Ordering::Relaxed),"overflows":core.overflows.load(Ordering::Relaxed)}));
    }
}
struct OwnedChild {
    child: Child,
    #[cfg(windows)]
    _job: Option<WinJob>,
}
impl OwnedChild {
    fn new(child: Child) -> Self {
        #[cfg(windows)]
        let job = WinJob::assign(&child).ok();
        Self {
            child,
            #[cfg(windows)]
            _job: job,
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
/// Retain cleanup paths until the worker has been reaped, including failed surface setup.
/// FrameMap still retries on its own Drop if a parent reader outlives that worker.
#[derive(Default)]
struct SurfaceFiles(Vec<PathBuf>);
impl SurfaceFiles {
    fn reclaim(&mut self) {
        self.0.retain(|path| match std::fs::remove_file(path) {
            Ok(()) => false,
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        });
    }
}
impl Drop for SurfaceFiles {
    fn drop(&mut self) {
        self.reclaim();
    }
}
fn session(
    core: &Arc<Core>,
    rx: &Receiver<Command>,
    info: &PluginInfo,
    sr: f32,
    generation: u64,
    journal: &mut HashMap<u32, f64>,
) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let mut entropy = [0u8; 32];
    SystemRandom::new()
        .fill(&mut entropy)
        .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    let token: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
    let serial = MAP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("nodeplayer-{}-{serial}.mmap", std::process::id()));
    let map = Arc::new(Shared::create(path.clone(), generation)?);
    // Every mapping remains supervisor-owned through processor retirement. Old proxies may
    // be pending in the engine garbage queue; they cannot unmap on the audio thread.
    {
        let mut current = core.map.lock().unwrap();
        if let Some(old) = current.replace(map.clone()) {
            core.retired.lock().unwrap().push(old);
        }
    }
    let executable = std::env::current_exe()?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--plugin-worker")
        .arg(listener.local_addr()?.to_string())
        .arg(&path)
        .arg(generation.to_string());
    if crate::runtime::get().data_root.is_some() {
        command
            .arg("--data-root")
            .arg(crate::runtime::get().data_root.as_ref().unwrap());
    }
    if crate::diagnostics::active().is_some() {
        command.env("NODEPLAYER_DIAGNOSTIC_WORKER", "1");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No inherited stdout/stderr pipes: a log-flooding plugin cannot deadlock the worker.
    // Declared before child so cleanup runs after kill/reap on every exit path.
    let mut surface_files = SurfaceFiles::default();
    let mut child = OwnedChild::new(command.spawn().context("spawn isolated plugin worker")?);
    if let Some(mut stdin) = child.child.stdin.take() {
        writeln!(stdin, "{token}")?;
    }
    let operation = (|| -> Result<()> {
        let startup = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            if core.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(exit) = child.child.try_wait()? {
                bail!("worker exited during startup ({exit})")
            }
            if Instant::now() > startup {
                bail!("worker startup timeout")
            }
            match listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    // Windows accepts inherit FIONBIO from the listener. Authentication
                    // and the framed control protocol require blocking, deadline-bound IO.
                    stream.set_nonblocking(false)?;
                    stream.set_nodelay(true)?;
                    // protocol::receive applies one absolute deadline to the entire hello frame.
                    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    let hello = protocol::receive::<Hello>(&mut stream);
                    if hello.is_ok_and(|h| {
                        h.version == VERSION
                            && constant_time_equal(h.token.as_bytes(), token.as_bytes())
                    }) {
                        break stream;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        protocol::send(
            &mut stream,
            &Init {
                version: VERSION,
                info: info.clone(),
                sr,
                quantum: Q,
                delay: DELAY,
                slots: SLOTS,
                generation,
            },
        )?;
        let first: Reply = protocol::receive(&mut stream)?;
        validate(&first, generation, 0)?;
        // Child opens the mapping before connecting; after its authenticated hello and
        // initial reply, unlink while retaining mapped handles until all proxies retire.
        let _ = std::fs::remove_file(&path);
        apply(core, first)?;
        let mut id = 0;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        status(core, "restoring");
        let state = core.snapshot.lock().unwrap().state.clone();
        if let Some(data) = state.as_ref() {
            transact(
                &mut stream,
                core,
                generation,
                &mut id,
                Command::Load(data.clone()),
            )?;
        }
        for (&pid, &v) in journal.iter() {
            transact(
                &mut stream,
                core,
                generation,
                &mut id,
                Command::SetParam(pid, v),
            )?;
        }
        // The initial app restore may enqueue state while child is loading; drain prior to
        // publishing ready. At most bounded queue length, never an unbounded producer loop.
        for _ in 0..128 {
            match rx.try_recv() {
                Ok(c) => {
                    // Even an identical Load is an ordering barrier: replayed journal writes or
                    // earlier queued parameter commands may have modified the restored state.
                    transact_recorded(
                        &mut stream,
                        core,
                        generation,
                        &mut id,
                        journal,
                        &mut surface_files,
                        c,
                    )?;
                }
                Err(_) => break,
            }
        }
        core.ready.store(true, Ordering::Release);
        status(core, "running");
        let mut last_progress = map.progress();
        let mut progress_at = Instant::now();
        let mut progress_cursor = core.cursor.load(Ordering::Acquire);
        let mut checkpoint_at = Instant::now();
        loop {
            reclaim(core);
            surface_files.reclaim();
            let close = core.close_editor.swap(0, Ordering::AcqRel);
            if close != 0 {
                transact_recorded(
                    &mut stream,
                    core,
                    generation,
                    &mut id,
                    journal,
                    &mut surface_files,
                    Command::Close(close),
                )?;
            }
            if core.stop.load(Ordering::Acquire) {
                // Drain the finite bounded queue before shutdown so the final checkpoint and
                // other already-enqueued UI commands land before the worker is closed.
                for _ in 0..128 {
                    match rx.try_recv() {
                        Ok(c) => transact_recorded(
                            &mut stream,
                            core,
                            generation,
                            &mut id,
                            journal,
                            &mut surface_files,
                            c,
                        )?,
                        Err(_) => break,
                    }
                }
                let _ = transact(&mut stream, core, generation, &mut id, Command::Shutdown);
                return Ok(());
            }
            if core.recover.load(Ordering::Acquire) {
                bail!("explicit worker restart requested")
            }
            if let Some(exit) = child.child.try_wait()? {
                bail!("worker exited ({exit})")
            }
            let cursor = core.cursor.load(Ordering::Acquire);
            let progress = map.progress();
            if progress != last_progress || cursor == progress_cursor {
                last_progress = progress;
                progress_cursor = cursor;
                progress_at = Instant::now();
            } else if progress_at.elapsed() > Duration::from_secs(2) {
                bail!("DSP progress timeout")
            }
            // UI thread handles each command sequentially; a main-thread hang is bounded by
            // socket timeout here. DSP progress watchdog above is independent of UI replies.
            let first = rx.recv_timeout(Duration::from_millis(8)).ok();
            let mut commands = Vec::new();
            if let Some(c) = first {
                commands.push(c);
            }
            for _ in 0..31 {
                match rx.try_recv() {
                    Ok(c) => commands.push(c),
                    Err(_) => break,
                }
            }
            let mut compact = coalesce(commands);
            if compact.is_empty() {
                compact.push(Command::Poll);
            }
            for c in compact {
                transact_recorded(
                    &mut stream,
                    core,
                    generation,
                    &mut id,
                    journal,
                    &mut surface_files,
                    c,
                )?;
            }
            if checkpoint_at.elapsed() > Duration::from_secs(30) {
                transact_recorded(
                    &mut stream,
                    core,
                    generation,
                    &mut id,
                    journal,
                    &mut surface_files,
                    Command::Checkpoint,
                )?;
                checkpoint_at = Instant::now();
            }
        }
    })();
    core.ready.store(false, Ordering::Release);
    clear_surface(core, None);
    drop(child); // kill + reap here, never on parent UI/audio thread
    // Shared::Drop unmaps then removes its file; reclaim() retains busy generations until
    // all transient processors have retired. No per-generation cleanup thread is needed.
    operation
}
fn remember(core: &Core, journal: &mut HashMap<u32, f64>, c: &Command) {
    remember_param(journal, c);
    if let Command::Load(data) = c
        && let Ok(mut s) = core.snapshot.lock()
    {
        s.state = Some(data.clone());
    }
}
fn remember_param(journal: &mut HashMap<u32, f64>, c: &Command) {
    match c {
        Command::SetParam(id, v) => {
            if journal.len() < protocol::MAX_PARAMS || journal.contains_key(id) {
                journal.insert(*id, *v);
            }
        }
        // Requested states are retained even if the worker fails before acknowledging them;
        // their restore must not be overwritten by writes preceding that request.
        Command::Load(_) => journal.clear(),
        _ => {}
    }
}
fn transact_recorded(
    stream: &mut TcpStream,
    core: &Core,
    generation: u64,
    id: &mut u64,
    journal: &mut HashMap<u32, f64>,
    surface_files: &mut SurfaceFiles,
    c: Command,
) -> Result<()> {
    let captured_open = match &c {
        Command::Open {
            token, captured, ..
        } => {
            clear_surface(core, None);
            (*captured).then_some(*token)
        }
        Command::Close(token) => {
            clear_surface(core, Some(*token));
            None
        }
        _ => None,
    };
    let checkpoint = matches!(&c, Command::Checkpoint);
    let long_timeout = checkpoint || matches!(&c, Command::Load(_));
    remember(core, journal, &c);
    if long_timeout {
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    }
    let cached_state = transact(stream, core, generation, id, c)?;
    let capture_attached = captured_open.is_some_and(|token| {
        let snapshot = core.snapshot.lock().unwrap();
        snapshot.metadata.as_ref()
            .is_some_and(|metadata| metadata.editor_id == token && metadata.editor_open)
    });
    if let Some(token) = captured_open.filter(|_| capture_attached) {
        // Native attachment already succeeded. Create image storage away from the UI and
        // RT threads, then publish it only after the matching worker has opened it.
        let surface = Arc::new(FrameMap::create(generation, token)?);
        surface_files.0.push(surface.descriptor().path);
        transact(
            stream,
            core,
            generation,
            id,
            Command::SetSurface(token, surface.descriptor()),
        )?;
        let _ = surface.unlink(); // Windows may retain the pathname until final unmapping.
        *core.surface.lock().unwrap() = Some(surface);
    }
    if long_timeout {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    }
    checkpoint_journal(journal, checkpoint && cached_state);
    Ok(())
}
fn clear_surface(core: &Core, token: Option<u64>) {
    if let Ok(mut surface) = core.surface.lock()
        && surface
            .as_ref()
            .is_some_and(|map| token.is_none_or(|token| map.token() == token))
    {
        *surface = None;
    }
}
fn checkpoint_journal(journal: &mut HashMap<u32, f64>, cached_state: bool) {
    // Without a new blob, the old cached state still needs its parameter overlay.
    if cached_state {
        journal.clear();
    }
}
fn coalesce(commands: Vec<Command>) -> Vec<Command> {
    let mut compact: Vec<Command> = Vec::with_capacity(commands.len());
    let mut segment = 0;
    for c in commands {
        // Never move a write across state/editor lifecycle operations. In particular,
        // SetParam(A), Load(state), SetParam(B) must apply B after the state restore.
        let mergeable = matches!(
            &c,
            Command::SetParam(..) | Command::Display(..) | Command::Place(..) | Command::DisplayScale(..)
        ) || matches!(&c, Command::Pointer { event, .. } if matches!(event.kind, PointerKind::Move));
        if !mergeable {
            compact.push(c);
            segment = compact.len();
            continue;
        }
        if let Some(at) = compact[segment..].iter().position(|old| same_key(old, &c)) {
            compact[segment + at] = c;
        } else {
            compact.push(c);
        }
    }
    compact
}
fn same_key(a: &Command, b: &Command) -> bool {
    match (a, b) {
        (Command::Place(a, _), Command::Place(b, _)) => a == b,
        (Command::DisplayScale(a, _), Command::DisplayScale(b, _)) => a == b,
        (Command::Display(a, _), Command::Display(b, _))
        | (Command::SetParam(a, _), Command::SetParam(b, _)) => a == b,
        (
            Command::Pointer {
                generation: ga,
                token: a,
                event: ea,
            },
            Command::Pointer {
                generation: gb,
                token: b,
                event: eb,
            },
        ) => {
            ga == gb
                && a == b
                && matches!(ea.kind, PointerKind::Move)
                && matches!(eb.kind, PointerKind::Move)
        }
        _ => false,
    }
}
fn transact(
    stream: &mut TcpStream,
    core: &Core,
    generation: u64,
    id: &mut u64,
    c: Command,
) -> Result<bool> {
    let command = match &c {
        Command::Poll => "poll",
        Command::SetParam(..) => "set-param",
        Command::Display(..) => "display",
        Command::Load(..) => "load",
        Command::Checkpoint => "checkpoint",
        Command::Open { .. } => "open-editor",
        Command::SetSurface(..) => "set-surface",
        Command::Pointer { .. } => "pointer",
        Command::Keyboard { .. } => "keyboard",
        Command::Place(..) => "place",
        Command::DisplayScale(..) => "display-scale",
        Command::Close(..) => "close-editor",
        Command::Shutdown => "shutdown",
    };
    let started = Instant::now();
    *id = id
        .checked_add(1)
        .context("control request sequence overflow")?;
    // Checkpoints serialize the live instance while child DSP keeps running.
    // Gating the parent here would discard wet output and stop audio publication.
    let quiesce = matches!(c, Command::Load(_));
    if quiesce {
        core.ready.store(false, Ordering::Release);
    }
    protocol::send(
        stream,
        &Request {
            version: VERSION,
            generation,
            id: *id,
            command: c,
        },
    )
    .with_context(|| {
        format!(
            "worker generation={generation} request={} command={command} stage=send",
            *id
        )
    })?;
    let reply: Reply = protocol::receive(stream).with_context(|| {
        format!(
            "worker generation={generation} request={} command={command} stage=reply elapsed_ms={}",
            *id,
            started.elapsed().as_millis()
        )
    })?;
    validate(&reply, generation, *id).with_context(|| {
        format!(
            "worker generation={generation} request={} command={command} stage=validate",
            *id
        )
    })?;
    let cached_state = reply.state.is_some();
    apply(core, reply).with_context(|| {
        format!(
            "worker generation={generation} request={} command={command} stage=apply",
            *id
        )
    })?;
    if quiesce {
        core.ready.store(true, Ordering::Release);
    }
    Ok(cached_state)
}
fn validate(r: &Reply, generation: u64, id: u64) -> Result<()> {
    if r.version != VERSION || r.generation != generation || r.id != id {
        bail!("invalid worker reply generation/sequence")
    }
    if let Some(e) = &r.error {
        bail!("worker command failed: {e}")
    }
    if r.state
        .as_ref()
        .is_some_and(|s| s.len() > protocol::MAX_STATE)
    {
        bail!("oversized worker state")
    }
    if r.metadata.as_ref().is_some_and(|m| {
        m.params.len() > protocol::MAX_PARAMS
            || m.values.len() > protocol::MAX_PARAMS
            || m.latency as usize > shared::MAX_LATENCY
            || m.editor_error
                .as_ref()
                .is_some_and(|error| error.len() > 4096)
    }) {
        bail!("invalid worker metadata")
    }
    Ok(())
}
fn apply(core: &Core, r: Reply) -> Result<()> {
    let mut s = core.snapshot.lock().unwrap();
    if let Some(m) = r.metadata {
        core.algorithm_latency.store(m.latency, Ordering::Release);
        core.instrument.store(m.instrument, Ordering::Release);
        s.metadata = Some(m);
        s.revision = s.revision.wrapping_add(1);
    }
    if let Some(state) = r.state {
        s.state = Some(state);
        // Signal that a checkpoint reply landed.
        s.checkpoint_seq = s.checkpoint_seq.wrapping_add(1);
    }
    if let Some((id, v, d)) = r.display
        && (s.displays.len() < protocol::MAX_PARAMS || s.displays.contains_key(&id))
    {
        s.displays.insert(id, (v, d));
    }
    Ok(())
}
fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0;
    for (&a, &b) in a.iter().zip(b) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(windows)]
struct WinJob(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl WinJob {
    fn assign(child: &Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };
        unsafe {
            let job = CreateJobObjectW(None, None)?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Err(e) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )
            .and_then(|_| AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())))
            {
                let _ = CloseHandle(job);
                return Err(e.into());
            }
            Ok(Self(job))
        }
    }
}
#[cfg(windows)]
impl Drop for WinJob {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn killed_surface_worker_retries_cleanup_after_reaping() -> Result<()> {
        let map = FrameMap::create(99, 88)?;
        let descriptor = map.descriptor();
        let mut files = SurfaceFiles(vec![descriptor.path.clone()]);
        let mut child = OwnedChild::new(
            ProcessCommand::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "plugins::remote::supervisor::tests::surface_worker_holds_mapping_until_killed",
                ])
                .env("NODEPLAYER_KILLED_SURFACE_TEST_PATH", &descriptor.path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while map.take_latest()?.is_none() {
            if child.child.try_wait()?.is_some() || Instant::now() >= deadline {
                bail!("surface worker did not open its mapping");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // On Windows this early cleanup is denied while the worker retains the mapping.
        // Termination skips the child's Rust destructors; the supervisor must retry.
        drop(map);
        drop(child);
        files.reclaim();
        assert!(files.0.is_empty());
        assert!(!descriptor.path.exists());
        Ok(())
    }

    #[test]
    fn surface_worker_holds_mapping_until_killed() -> Result<()> {
        let Some(path) = std::env::var_os("NODEPLAYER_KILLED_SURFACE_TEST_PATH") else {
            return Ok(());
        };
        let map = FrameMap::open(&super::super::surface::SurfaceDescriptor {
            path: path.into(),
            generation: 99,
            token: 88,
        })?;
        map.publish(&crate::plugins::surface::EditorFrame {
            native_units_per_point: 1.0,
            popups: Vec::new(),
            width: 1,
            height: 1,
            native_size: (1, 1),
            rgba: vec![1, 2, 3, 255],
        })?;
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn encoded(commands: Vec<Command>) -> serde_json::Value {
        serde_json::to_value(commands).unwrap()
    }

    fn checkpoint_core(map: Arc<Shared>, state: Vec<u8>) -> Arc<Core> {
        use super::super::{AtomicBool, AtomicU32, Mutex, Snapshot};
        let (tx, _rx) = crossbeam_channel::bounded(1);
        Arc::new(Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            map: Mutex::new(Some(map)),
            surface: Mutex::new(None),
            close_editor: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            cursor: AtomicU64::new(0),
            algorithm_latency: AtomicU32::new(0),
            instrument: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            next_editor: AtomicU64::new(0),
            snapshot: Mutex::new(Snapshot {
                metadata: None,
                state: Some(state),
                checkpoint_seq: 0,
                displays: HashMap::new(),
                status: "running".into(),
                revision: 0,
            }),
            tx,
        })
    }

    fn process_checkpoint_audio(
        processor: &mut super::super::processor::RemoteProcessor,
        map: &Shared,
    ) -> crate::engine::StereoBuf {
        use crate::engine::{Processor, StereoBuf};
        let mut input = [StereoBuf::new()];
        input[0].l[..Q].fill(0.25);
        input[0].r[..Q].fill(-0.125);
        let mut output = [StereoBuf::new()];
        processor.process(Q, &mut input, &mut output);
        for i in 0..SLOTS {
            let slot = map.slot(i);
            if slot
                .state
                .compare_exchange(
                    shared::INPUT,
                    shared::WORKING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // A deterministic child applies gain, distinguishing wet from dry fallback.
                // SAFETY: this fake child owns the slot until releasing OUTPUT below.
                let payload = unsafe { slot.payload() };
                for sample in payload.left.iter_mut().chain(&mut payload.right) {
                    *sample *= 2.0;
                }
                payload.midi_count = 0;
                slot.state.store(shared::OUTPUT, Ordering::Release);
                map.progressed();
            }
        }
        output.into_iter().next().unwrap()
    }

    #[test]
    fn delayed_state_replies_keep_checkpoint_audio_live_but_quiesce_load() -> Result<()> {
        #[derive(Clone, Copy)]
        enum Outcome {
            Accepted,
            Rejected,
            Disconnected,
        }
        const GENERATION: u64 = 73;
        let old_state = vec![3, 1, 4, 1, 5];
        let saved_state = (0..=255).cycle().take(4097).collect::<Vec<u8>>();
        for (command, live) in [
            (Command::Checkpoint, true),
            (Command::Load(saved_state.clone()), false),
        ] {
            for outcome in [Outcome::Accepted, Outcome::Rejected, Outcome::Disconnected] {
                let mut random = [0; 16];
                SystemRandom::new().fill(&mut random).unwrap();
                let suffix = random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let map = Arc::new(Shared::create(
                    std::env::temp_dir().join(format!("np-checkpoint-test-{suffix}")),
                    GENERATION,
                )?);
                let core = checkpoint_core(map.clone(), old_state.clone());
                let mut processor = super::super::processor::RemoteProcessor::new(core.clone());
                for _ in 0..=DELAY / Q {
                    process_checkpoint_audio(&mut processor, &map);
                }
                assert_eq!(core.underruns.load(Ordering::Relaxed), 0);
                let progress_before = map.progress();
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                let mut stream = TcpStream::connect(listener.local_addr()?)?;
                stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                let (peer, _) = listener.accept()?;
                peer.set_read_timeout(Some(Duration::from_secs(3)))?;
                peer.set_write_timeout(Some(Duration::from_secs(3)))?;
                let transaction_core = core.clone();
                let requested = command.clone();
                std::thread::scope(|scope| -> Result<()> {
                    // Keep the peer inside the scope so failures close it before joining.
                    let mut peer = peer;
                    let transaction = scope.spawn(move || {
                        let mut journal = HashMap::from([(7, 0.25)]);
                        let result = transact_recorded(
                            &mut stream,
                            &transaction_core,
                            GENERATION,
                            &mut 0,
                            &mut journal,
                            &mut SurfaceFiles::default(),
                            requested,
                        );
                        (result, journal)
                    });
                    let request: Request = protocol::receive(&mut peer)?;
                    assert_eq!(
                        encoded(vec![request.command]),
                        encoded(vec![command.clone()])
                    );
                    // The transaction is waiting for this test's reply throughout these
                    // callbacks. Run beyond the bridge delay to expose publication holes.
                    for _ in 0..DELAY / Q + 4 {
                        assert_eq!(core.ready.load(Ordering::Acquire), live);
                        let output = process_checkpoint_audio(&mut processor, &map);
                        assert_eq!(&output.l[..Q], &[if live { 0.5 } else { 0.25 }; Q]);
                        assert_eq!(&output.r[..Q], &[if live { -0.25 } else { -0.125 }; Q]);
                    }
                    assert_eq!(
                        map.progress() - progress_before,
                        if live { (DELAY / Q + 4) as u64 } else { 0 }
                    );
                    if live {
                        assert_eq!(core.underruns.load(Ordering::Relaxed), 0);
                    }
                    {
                        let snapshot = core.snapshot.lock().unwrap();
                        assert_eq!(snapshot.checkpoint_seq, 0);
                        assert_eq!(
                            snapshot.state.as_ref(),
                            Some(if live { &old_state } else { &saved_state })
                        );
                    }
                    if !matches!(outcome, Outcome::Disconnected) {
                        protocol::send(
                            &mut peer,
                            &Reply {
                                version: VERSION,
                                generation: request.generation,
                                id: request.id,
                                metadata: None,
                                // A rejected reply must not overwrite the previous checkpoint.
                                state: live.then(|| saved_state.clone()),
                                display: None,
                                error: matches!(outcome, Outcome::Rejected)
                                    .then(|| "snapshot failed".into()),
                            },
                        )?;
                    }
                    drop(peer);
                    let (result, journal) = transaction.join().unwrap();
                    let accepted = matches!(outcome, Outcome::Accepted);
                    assert_eq!(result.is_ok(), accepted);
                    let snapshot = core.snapshot.lock().unwrap();
                    let saved = live && accepted;
                    assert_eq!(snapshot.checkpoint_seq, u64::from(saved));
                    assert_eq!(
                        snapshot.state.as_ref(),
                        Some(if live && !accepted {
                            &old_state
                        } else {
                            &saved_state
                        })
                    );
                    assert_eq!(journal.is_empty(), !live || accepted);
                    if accepted {
                        assert!(core.ready.load(Ordering::Acquire));
                    } else if !live {
                        assert!(!core.ready.load(Ordering::Acquire));
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    #[test]
    fn diagnostic_recovery_drops_only_reserved_fixture_fault_parameters() {
        let parameters =
            HashMap::from([(7, 0.25), (99, 0.5), (100, 1.0), (102, 1.0), (4096, 0.75)]);
        for (diagnostic, plugin_id, filtered) in [
            (true, "org.nodeplayer.test-fixture", true),
            (false, "org.nodeplayer.test-fixture", false),
            (true, "org.vendor.production-plugin", false),
            (false, "org.vendor.production-plugin", false),
        ] {
            let mut journal = parameters.clone();
            omit_diagnostic_fault_triggers(&mut journal, diagnostic, plugin_id);
            if filtered {
                assert_eq!(journal, HashMap::from([(7, 0.25), (99, 0.5)]));
            } else {
                assert_eq!(journal, parameters);
            }
        }
    }

    #[test]
    fn unchanged_checkpoint_advances_wait_sequence() {
        use super::super::{AtomicBool, AtomicU32, Mutex, Snapshot};

        let (tx, _rx) = crossbeam_channel::bounded(1);
        let blob = vec![1, 2, 3];
        let core = Core {
            stop: AtomicBool::new(false),
            recover: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            map: Mutex::new(None),
            surface: Mutex::new(None),
            close_editor: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            cursor: AtomicU64::new(0),
            algorithm_latency: AtomicU32::new(0),
            instrument: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            next_editor: AtomicU64::new(0),
            snapshot: Mutex::new(Snapshot {
                metadata: None,
                state: Some(blob.clone()),
                checkpoint_seq: 0,
                displays: HashMap::new(),
                status: "running".into(),
                revision: 0,
            }),
            tx,
        };
        let seq_before = core.snapshot.lock().unwrap().checkpoint_seq;
        apply(
            &core,
            Reply {
                version: VERSION,
                generation: 0,
                id: 0,
                metadata: None,
                state: Some(blob.clone()),
                display: None,
                error: None,
            },
        )
        .unwrap();
        let snapshot = core.snapshot.lock().unwrap();
        assert!(snapshot.checkpoint_seq > seq_before);
        assert_eq!(snapshot.state, Some(blob));
    }

    #[test]
    fn shutdown_queue_drain_is_bounded_and_preserves_checkpoint_order() {
        let (tx, rx) = crossbeam_channel::bounded(128);
        for i in 0..127 {
            tx.try_send(Command::SetParam(i, i as f64 / 127.0)).unwrap();
        }
        tx.try_send(Command::Checkpoint).unwrap();
        let drained = (0..128)
            .filter_map(|_| rx.try_recv().ok())
            .collect::<Vec<_>>();
        assert_eq!(drained.len(), 128);
        assert!(matches!(drained.last(), Some(Command::Checkpoint)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn coalescing_preserves_state_and_editor_barriers() {
        use super::super::super::Placement;
        let commands = vec![
            Command::SetParam(7, 0.1),
            Command::SetParam(7, 0.2),
            Command::Load(vec![1, 2, 3]),
            Command::SetParam(7, 0.3),
            Command::Checkpoint,
            Command::SetParam(7, 0.4),
            Command::Place(1, None),
            Command::Close(1),
            Command::Open {
                token: 2,
                placement: Placement::Window,
                captured: false,
            },
            Command::Place(2, None),
            Command::Place(2, None),
        ];
        let expected = vec![
            Command::SetParam(7, 0.2),
            Command::Load(vec![1, 2, 3]),
            Command::SetParam(7, 0.3),
            Command::Checkpoint,
            Command::SetParam(7, 0.4),
            Command::Place(1, None),
            Command::Close(1),
            Command::Open {
                token: 2,
                placement: Placement::Window,
                captured: false,
            },
            Command::Place(2, None),
        ];
        assert_eq!(encoded(coalesce(commands)), encoded(expected));
    }

    #[test]
    fn state_requests_and_successful_checkpoints_reset_parameter_overlay() {
        let mut journal = HashMap::new();
        remember_param(&mut journal, &Command::SetParam(1, 0.1));
        remember_param(&mut journal, &Command::Load(vec![1]));
        assert!(journal.is_empty());
        remember_param(&mut journal, &Command::SetParam(1, 0.2));
        checkpoint_journal(&mut journal, false);
        assert_eq!(journal.get(&1), Some(&0.2));
        checkpoint_journal(&mut journal, true);
        assert!(journal.is_empty());
        remember_param(&mut journal, &Command::SetParam(1, 0.3));
        assert_eq!(journal.get(&1), Some(&0.3));
    }

    #[test]
    fn editor_geometry_only_coalesces_for_its_own_token() {
        let commands = vec![
            Command::Place(1, None),
            Command::Place(2, None),
            Command::Place(1, None),
        ];
        let expected = vec![
            Command::Place(1, None),
            Command::Place(2, None),
        ];
        assert_eq!(encoded(coalesce(commands)), encoded(expected));
    }

    #[test]
    fn coalescing_keeps_distinct_parameter_keys() {
        let commands = vec![
            Command::SetParam(1, 0.1),
            Command::SetParam(2, 0.2),
            Command::SetParam(1, 0.3),
        ];
        let expected = vec![Command::SetParam(1, 0.3), Command::SetParam(2, 0.2)];
        assert_eq!(encoded(coalesce(commands)), encoded(expected));
    }

    #[test]
    fn pointer_move_coalescing_preserves_buttons_wheel_and_generation() {
        use crate::plugins::surface::EditorPointer;
        let event = |generation, kind, x, buttons| Command::Pointer {
            generation,
            token: 3,
            event: EditorPointer {
                timestamp_ms: None,
                modifiers: 0,
                click_count: if matches!(kind, PointerKind::Down(_) | PointerKind::Up(_)) {
                    1
                } else {
                    0
                },
                position: [x, 20.0],
                kind,
                buttons,
            },
        };
        let commands = vec![
            event(1, PointerKind::Move, 1.0, 0),
            event(1, PointerKind::Move, 2.0, 0),
            event(1, PointerKind::Down(1), 3.0, 1),
            event(1, PointerKind::Move, 4.0, 1),
            event(1, PointerKind::Move, 5.0, 1),
            event(1, PointerKind::Wheel { dx: 0.0, dy: 12.0 }, 6.0, 1),
            event(1, PointerKind::Move, 7.0, 1),
            event(1, PointerKind::Up(1), 8.0, 0),
            event(1, PointerKind::Move, 9.0, 0),
            event(2, PointerKind::Move, 10.0, 0),
        ];
        let expected = vec![
            event(1, PointerKind::Move, 2.0, 0),
            event(1, PointerKind::Down(1), 3.0, 1),
            event(1, PointerKind::Move, 5.0, 1),
            event(1, PointerKind::Wheel { dx: 0.0, dy: 12.0 }, 6.0, 1),
            event(1, PointerKind::Move, 7.0, 1),
            event(1, PointerKind::Up(1), 8.0, 0),
            event(1, PointerKind::Move, 9.0, 0),
            event(2, PointerKind::Move, 10.0, 0),
        ];
        assert_eq!(encoded(coalesce(commands)), encoded(expected));
    }
}
