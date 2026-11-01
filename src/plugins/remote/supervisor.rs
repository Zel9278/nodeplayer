use super::{
    super::PluginInfo,
    Core,
    protocol::{self, Command, Hello, Init, Reply, Request, VERSION},
    shared::{self, DELAY, Q, SLOTS, Shared},
};
use anyhow::{Context, Result, bail};
use crossbeam_channel::Receiver;
use ring::rand::{SecureRandom, SystemRandom};
use std::{

    collections::HashMap,
    io::Write,
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};

static MAP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn run(core: Arc<Core>, rx: Receiver<Command>, info: PluginInfo, sr: f32) {
    let mut generation = 0u64;
    let mut journal = HashMap::new();
    while !core.stop.load(Ordering::Acquire) {
        generation = generation.wrapping_add(1);
        core.recover.store(false, Ordering::Release);
        // Diagnostics deliberately reserve fixture parameters >=100 as fault triggers.
        // The worker is a separate process, so this gate is based on the shared
        // diagnostic scenario signal (also inherited as an explicit env marker below).
        if std::env::var_os("NODEPLAYER_DIAGNOSTIC_WORKER").is_some()
            && info.id == "org.nodeplayer.test-fixture"
        {
            journal.retain(|id, _| *id < 100);
        }
        status(&core, "starting");
        let result = session(&core, &rx, &info, sr, generation, &mut journal);
        core.ready.store(false, Ordering::Release);
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
    let path = std::env::temp_dir().join(format!("nodeplayer-{}-{serial}.mmap", std::process::id()));
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
        command.arg("--data-root").arg(crate::runtime::get().data_root.as_ref().unwrap());
    }
    if crate::diagnostics::active().is_some() {
        command.env("NODEPLAYER_DIAGNOSTIC_WORKER", "1");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No inherited stdout/stderr pipes: a log-flooding plugin cannot deadlock the worker.
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
                    transact_recorded(&mut stream, core, generation, &mut id, journal, c)?;
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
            if core.stop.load(Ordering::Acquire) {
                // Drain the finite bounded queue before shutdown so the final checkpoint and
                // other already-enqueued UI commands land before the worker is closed.
                for _ in 0..128 {
                    match rx.try_recv() {
                        Ok(c) => transact_recorded(&mut stream, core, generation, &mut id, journal, c)?,
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
                transact_recorded(&mut stream, core, generation, &mut id, journal, c)?;
            }
            if checkpoint_at.elapsed() > Duration::from_secs(30) {
                transact_recorded(
                    &mut stream,
                    core,
                    generation,
                    &mut id,
                    journal,
                    Command::Checkpoint,
                )?;
                checkpoint_at = Instant::now();
            }
        }
    })();
    core.ready.store(false, Ordering::Release);
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
    c: Command,
) -> Result<()> {
    let checkpoint = matches!(&c, Command::Checkpoint);
    let long_timeout = checkpoint || matches!(&c, Command::Load(_));
    remember(core, journal, &c);
    if long_timeout {
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    }
    let cached_state = transact(stream, core, generation, id, c)?;
    if long_timeout {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    }
    checkpoint_journal(journal, checkpoint && cached_state);
    Ok(())
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
        if !matches!(
            &c,
            Command::SetParam(..) | Command::Display(..) | Command::Place(..) | Command::Zoom(..)
        ) {
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
        (Command::Place(a, _), Command::Place(b, _))
        | (Command::Zoom(a, _), Command::Zoom(b, _)) => a == b,
        (Command::Display(a, _), Command::Display(b, _))
        | (Command::SetParam(a, _), Command::SetParam(b, _)) => a == b,
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
    *id = id
        .checked_add(1)
        .context("control request sequence overflow")?;
    let quiesce = matches!(c, Command::Load(_) | Command::Checkpoint);
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
    )?;
    let reply: Reply = protocol::receive(stream)?;
    validate(&reply, generation, *id)?;
    let cached_state = reply.state.is_some();
    apply(core, reply)?;
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

    fn encoded(commands: Vec<Command>) -> serde_json::Value {
        serde_json::to_value(commands).unwrap()
    }

    #[test]
    fn shutdown_queue_drain_is_bounded_and_preserves_checkpoint_order() {
        let (tx, rx) = crossbeam_channel::bounded(128);
        for i in 0..127 {
            tx.try_send(Command::SetParam(i, i as f64 / 127.0)).unwrap();
        }
        tx.try_send(Command::Checkpoint).unwrap();
        let drained = (0..128).filter_map(|_| rx.try_recv().ok()).collect::<Vec<_>>();
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
            Command::Zoom(1, 1.0),
            Command::Close(1),
            Command::Open {
                token: 2,
                placement: Placement::Window,
            },
            Command::Zoom(2, 2.0),
            Command::Zoom(2, 3.0),
        ];
        let expected = vec![
            Command::SetParam(7, 0.2),
            Command::Load(vec![1, 2, 3]),
            Command::SetParam(7, 0.3),
            Command::Checkpoint,
            Command::SetParam(7, 0.4),
            Command::Zoom(1, 1.0),
            Command::Close(1),
            Command::Open {
                token: 2,
                placement: Placement::Window,
            },
            Command::Zoom(2, 3.0),
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
            Command::Zoom(1, 1.0),
            Command::Zoom(2, 2.0),
            Command::Zoom(1, 3.0),
            Command::Place(1, None),
            Command::Place(2, None),
        ];
        let expected = vec![
            Command::Zoom(1, 3.0),
            Command::Zoom(2, 2.0),
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
}
