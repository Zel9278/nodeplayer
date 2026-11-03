//! Explicit, dedicated diagnostic runs. All artifact I/O is control-thread only.
//! This is a reproducibility harness, not a security boundary for third-party plugins.
pub mod bridge;
pub mod metrics;
pub mod scenario;
use crate::runtime::RuntimeOptions;
use anyhow::{Context, Result, anyhow, bail};
pub use scenario::{AudioOutput, Scenario, ScenarioMode, Step};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_EVENTS_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 16 * 1024;
static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE: OnceLock<Weak<DiagnosticRun>> = OnceLock::new();
/// Startup-only registration. Weak ownership preserves finalization on ordinary drop.
/// Control-thread API: audio callbacks must capture the Arc once, never call active().
pub fn set_active(run: Arc<DiagnosticRun>) -> Result<()> {
    ACTIVE
        .set(Arc::downgrade(&run))
        .map_err(|_| anyhow!("active diagnostic already registered"))
}
pub fn active() -> Option<Arc<DiagnosticRun>> {
    ACTIVE.get().and_then(Weak::upgrade)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Passed,
    Failed,
    Timeout,
    Aborted,
}
impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Passed => 0,
            Self::Failed => 2,
            Self::Timeout => 124,
            Self::Aborted => 125,
        }
    }
}
struct EventWriter {
    file: File,
    bytes: usize,
    seen: HashSet<String>,
    sequence: u64,
}

#[derive(Clone, Copy, Serialize)]
struct ExpectedFault {
    recover: bool,
    observed: bool,
}
#[derive(Default, Clone, Copy, Serialize)]
struct ExpectedRestart {
    saw_down: bool,
    completed: bool,
}
#[derive(Serialize)]
struct CaptureHealth {
    started_ms: u128,
    observed_ms: u128,
    frames: u64,
    baseline_frames: u64,
    fresh_frame: bool,
    frame_age_seconds: Option<f64>,
    capture_error: Option<String>,
}
#[derive(Serialize)]
struct Health {
    policy: scenario::HealthPolicy,
    worker_status: Option<String>,
    worker_faulted: bool,
    worker_faults: u64,
    expected_worker_faults: u64,
    recovered_worker_faults: u64,
    expected_fault: Option<ExpectedFault>,
    expected_restart: Option<ExpectedRestart>,
    capture: Option<CaptureHealth>,
    failure: Option<&'static str>,
}
impl Health {
    fn new(policy: scenario::HealthPolicy) -> Self {
        Self {
            policy,
            worker_status: None,
            worker_faulted: false,
            worker_faults: 0,
            expected_worker_faults: 0,
            recovered_worker_faults: 0,
            expected_fault: None,
            expected_restart: None,
            capture: None,
            failure: None,
        }
    }
    fn expect_fault(&mut self, recover: bool) -> Result<()> {
        if self.expected_fault.is_some() || self.restarting() {
            bail!("previous diagnostic fault/restart expectation is still pending");
        }
        self.expected_fault = Some(ExpectedFault {
            recover,
            observed: false,
        });
        Ok(())
    }
    fn expect_restart(&mut self) -> Result<()> {
        if self.restarting() {
            bail!("diagnostic worker restart already pending");
        }
        if self.expected_fault.is_some_and(|fault| !fault.observed) {
            bail!("fixture fault must be observed before requesting recovery");
        }
        self.expected_restart = Some(ExpectedRestart {
            saw_down: self
                .worker_status
                .as_deref()
                .is_some_and(|s| s != "running"),
            completed: false,
        });
        Ok(())
    }
    fn restarting(&self) -> bool {
        self.expected_restart
            .is_some_and(|restart| !restart.completed)
    }
    fn expected_outage(&self) -> bool {
        self.expected_fault.is_some() || self.restarting()
    }
    fn observe_status(&mut self, status: &str, now_ms: u128) {
        let faulted = worker_status_faulted(status);
        if faulted && (!self.worker_faulted || self.worker_status.as_deref() != Some(status)) {
            // Older workers report their requested teardown as a fault. Only this
            // exact reason is allowed by a restart expectation; other failures latch.
            let requested_teardown =
                self.restarting() && status == "faulted: explicit worker restart requested";
            if !requested_teardown {
                self.worker_faults = self.worker_faults.saturating_add(1);
                if let Some(fault) = &mut self.expected_fault
                    && !fault.observed
                {
                    fault.observed = true;
                    self.expected_worker_faults = self.expected_worker_faults.saturating_add(1);
                } else {
                    self.failure.get_or_insert("unexpected worker fault");
                }
            }
        }
        self.worker_faulted = faulted;
        self.worker_status = Some(status.to_owned());
        if let Some(restart) = &mut self.expected_restart
            && !restart.completed
        {
            restart.saw_down |= status != "running";
            if restart.saw_down && status == "running" {
                restart.completed = true;
                if self
                    .expected_fault
                    .take()
                    .is_some_and(|fault| fault.observed)
                {
                    self.recovered_worker_faults = self.recovered_worker_faults.saturating_add(1);
                }
                if let Some(capture) = &mut self.capture {
                    capture.started_ms = now_ms;
                    capture.observed_ms = now_ms;
                    capture.baseline_frames = capture.frames;
                    capture.fresh_frame = false;
                    capture.frame_age_seconds = None;
                    capture.capture_error = None;
                }
            }
        }
    }
    fn observe_capture(
        &mut self,
        now_ms: u128,
        frames: u64,
        frame_age_seconds: Option<f64>,
        capture_error: Option<&str>,
    ) {
        let capture = self.capture.get_or_insert(CaptureHealth {
            started_ms: now_ms,
            observed_ms: now_ms,
            frames: 0,
            baseline_frames: 0,
            fresh_frame: false,
            frame_age_seconds: None,
            capture_error: None,
        });
        // Reopening an editor resets its delivered-frame counter.
        if frames < capture.frames {
            capture.started_ms = now_ms;
            capture.baseline_frames = 0;
            capture.fresh_frame = false;
        }
        capture.frames = frames;
        capture.observed_ms = now_ms;
        capture.fresh_frame |= frames > capture.baseline_frames;
        capture.frame_age_seconds = frame_age_seconds;
        capture.capture_error = capture_error.map(|error| error.chars().take(1024).collect());
        self.check_capture(now_ms);
    }
    fn close_capture(&mut self) {
        if !self.expected_outage()
            && self
                .capture
                .as_ref()
                .is_some_and(|capture| !capture.fresh_frame)
        {
            self.failure
                .get_or_insert("captured editor closed without a delivered frame");
        }
        self.capture = None;
    }
    fn check_capture(&mut self, now_ms: u128) {
        if self.expected_outage() {
            return;
        }
        let Some(capture) = &mut self.capture else {
            return;
        };
        if let Some(age) = &mut capture.frame_age_seconds {
            *age += now_ms.saturating_sub(capture.observed_ms) as f64 / 1000.0;
        }
        capture.observed_ms = now_ms;
        let failure = if capture.capture_error.is_some() {
            Some("editor capture reported an error")
        } else if capture.fresh_frame {
            match capture.frame_age_seconds {
                Some(age) if age.is_finite() && age >= 0.0 => (age * 1000.0
                    > self.policy.capture_max_frame_age_ms as f64)
                    .then_some("captured editor frame starvation"),
                _ => Some("captured editor frame age is unavailable"),
            }
        } else {
            (now_ms.saturating_sub(capture.started_ms)
                >= u128::from(self.policy.capture_startup_timeout_ms))
            .then_some("captured editor startup produced no frame")
        };
        if let Some(failure) = failure {
            self.failure.get_or_insert(failure);
        }
    }
    fn finish_failure(&mut self, now_ms: u128) -> Option<&'static str> {
        self.check_capture(now_ms);
        self.failure.or_else(|| {
            if self.restarting() {
                Some("worker restart did not complete")
            } else if let Some(fault) = self.expected_fault {
                if !fault.observed {
                    Some("expected fixture worker fault was not observed")
                } else if fault.recover {
                    Some("fixture worker fault requires an explicit successful restart")
                } else if !self.worker_faulted {
                    Some("terminal fixture fault is no longer present")
                } else {
                    None
                }
            } else if self
                .worker_status
                .as_deref()
                .is_some_and(|s| s != "running" && s != "local")
            {
                Some("worker is not running")
            } else if self
                .capture
                .as_ref()
                .is_some_and(|capture| !capture.fresh_frame)
            {
                Some("captured editor has no delivered frame")
            } else {
                None
            }
        })
    }
}
pub fn worker_status_faulted(status: &str) -> bool {
    let lower = status.to_ascii_lowercase();
    lower.contains("fault") || lower.contains("unresponsive") || lower.contains("fail")
}

pub struct DiagnosticRun {
    pub scenario: Scenario,
    pub root: PathBuf,
    pub run_id: String,
    pub metrics: metrics::Metrics,
    start: Instant,
    duration: Duration,
    events: Mutex<EventWriter>,
    finished: AtomicBool,
    outcome: Mutex<Option<Outcome>>,
    health: Mutex<Health>,
}

/// Create an exclusively owned run directory and a fresh profile. Existing paths are
/// rejected, even when explicitly requested, so a diagnostic never restores user state.
/// Call before runtime::install; keep this object alive until after the GUI exits.
pub fn prepare(options: &mut RuntimeOptions) -> Result<Option<DiagnosticRun>> {
    let Some(diagnostic) = options.diagnostic.as_ref() else {
        return Ok(None);
    };
    let scenario = Scenario::load(&diagnostic.scenario)?;
    let duration = diagnostic.duration;
    let metrics_mode = diagnostic.metrics;
    let run_id = new_run_id();
    let requested_root = diagnostic.output.clone().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("nodeplayer-diagnostics")
            .join(&run_id)
    });
    if let Some(parent) = requested_root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("create diagnostics output parent")?;
    }
    create_private_dir(&requested_root)
        .context("diagnostic output must be a new exclusive directory")?;
    let root = fs::canonicalize(&requested_root)?;
    let requested_profile = options
        .data_root
        .clone()
        .unwrap_or_else(|| root.join("profile"));
    // No create_dir_all here: neither an existing profile nor a shared normal profile
    // can silently become a diagnostic data root.
    create_private_dir(&requested_profile).context(
        "diagnostic data root must be a new exclusive directory with an existing parent",
    )?;
    let data_root = fs::canonicalize(&requested_profile)?;
    let paths = crate::dirs::Paths::under(&data_root);
    for dir in [&paths.config, &paths.cache, &paths.state, &paths.data] {
        create_private_dir(dir)?;
    }
    write_new_json(
        &data_root.join("diagnostic-owner.json"),
        &serde_json::json!({"run_id":run_id,"pid":std::process::id(),"warning":"plugin-owned OS profile data is not isolated"}),
    )?;
    let events = open_new(&root.join("events.jsonl"))?;
    let manifest = serde_json::json!({
        "schema_version": 1, "run_id": run_id, "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"), "build_profile": option_env!("NODEPLAYER_BUILD_PROFILE"),
        "commit": option_env!("NODEPLAYER_BUILD_COMMIT"), "dirty": option_env!("NODEPLAYER_BUILD_DIRTY"),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "target": option_env!("TARGET"), "started_unix_ms": unix_ms(),
        "experimental_pdc": options.experimental_pdc || options.experimental_plugin_isolation,
        "experimental_plugin_isolation": options.experimental_plugin_isolation,
        "experimental_editor_capture": options.experimental_editor_capture,
        "deadline_ms": duration.as_millis(), "metrics_mode": metrics_mode,
        "scenario": scenario.name, "scenario_mode": scenario.mode,
        "health_policy": scenario.health,
        "fixture": scenario.plugin.as_ref().map(|p| serde_json::json!({"format":p.format,"class_id":p.class_id,"name_filter":p.name_filter,"path":"[explicit fixture path redacted]"})),
        "sample_rate": scenario.offline.as_ref().map(|f| f.sample_rate),
        "requested_block_sizes": scenario.offline.as_ref().map(|f| &f.block_sizes),
        "actual_device_callback_sizes": serde_json::Value::Null,
        "q": serde_json::Value::Null, "d": serde_json::Value::Null, "h": serde_json::Value::Null,
        "policy": {"normal_profile":false,"native_extensions":false,"network":false,"auto_downloads":false,"library_discovery":false,"device_audio":scenario.audio_output == AudioOutput::Device},
        "limitations": ["plugin-owned OS profile data is not isolated", "external watchdog required for UI/process hangs", "unavailable measurements are null", "screenshots require explicit scenario steps; no detailed trace, OS profiling or uploads"]
    });
    write_new_json(&root.join("manifest.json"), &manifest)?;
    let health = Health::new(scenario.health);
    let run = DiagnosticRun {
        scenario,
        root,
        run_id,
        metrics: metrics::Metrics::new(metrics_mode),
        start: Instant::now(),
        duration,
        events: Mutex::new(EventWriter {
            file: events,
            bytes: 0,
            seen: HashSet::new(),
            sequence: 0,
        }),
        finished: AtomicBool::new(false),
        outcome: Mutex::new(None),
        health: Mutex::new(health),
    };
    options.data_root = Some(data_root);
    if let Some(diagnostic) = &mut options.diagnostic {
        diagnostic.audio_device = run.scenario.audio_output == AudioOutput::Device;
    }
    run.event("run-started", serde_json::json!({}))?;
    Ok(Some(run))
}
impl DiagnosticRun {
    /// Not RT-safe: bounded JSON encoding + mutex + file write, intended for UI/control only.
    /// Payload is the caller's explicit sanitized data: never pass credentials, states or media tags.
    pub fn event(&self, name: &str, payload: serde_json::Value) -> Result<()> {
        scenario::validate_event_name(name)?;
        // Observe supervisor events before writing: a transient fault cannot be
        // missed between GUI ticks, or erased by a later successful restart.
        let mut worker_fault = false;
        if name == "worker-status" {
            if let Some(status) = payload.get("status").and_then(serde_json::Value::as_str) {
                let mut health = self
                    .health
                    .lock()
                    .map_err(|_| anyhow!("diagnostic health lock poisoned"))?;
                let faults_before = health.worker_faults;
                health.observe_status(status, self.start.elapsed().as_millis());
                worker_fault = health.worker_faults > faults_before;
            }
        } else if name == "worker-faulted" {
            let mut health = self
                .health
                .lock()
                .map_err(|_| anyhow!("diagnostic health lock poisoned"))?;
            if !health.worker_faulted {
                let status = payload
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("faulted");
                health.observe_status(status, self.start.elapsed().as_millis());
            }
        }
        let mut writer = self
            .events
            .lock()
            .map_err(|_| anyhow!("diagnostic event lock poisoned"))?;
        let event: serde_json::Value = serde_json::json!({"run_id":self.run_id,"pid":std::process::id(),"thread_role":"control","instance_id":null,"generation":null,"plan_epoch":null,"request_id":null,"sequence":writer.sequence,"elapsed_ms":self.start.elapsed().as_millis(),"event":name,"payload":payload});
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_EVENT_BYTES
            || writer.bytes.saturating_add(bytes.len()) > MAX_EVENTS_BYTES
        {
            self.metrics.telemetry_overflow();
            bail!("diagnostic event log size limit reached");
        }
        writer.file.write_all(&bytes)?;
        writer.file.flush()?;
        writer.bytes += bytes.len();
        writer.sequence += 1;
        writer.seen.insert(name.to_owned());
        drop(writer);
        if worker_fault {
            // The supervisor owns this event, so even short faults remain visible
            // to WaitEvent and expected_events without waiting for a GUI poll.
            self.event("worker-faulted", payload)?;
        }
        Ok(())
    }
    pub fn event_seen(&self, name: &str) -> bool {
        self.events.lock().is_ok_and(|w| w.seen.contains(name))
    }
    pub fn expired(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.lock().ok().and_then(|o| *o)
    }
    /// Call only after verifying the explicit diagnostic fault fixture.
    pub fn expect_fixture_fault(&self, recover: bool) -> Result<()> {
        self.health
            .lock()
            .map_err(|_| anyhow!("diagnostic health lock poisoned"))?
            .expect_fault(recover)
    }
    pub fn expect_worker_restart(&self) -> Result<()> {
        self.health
            .lock()
            .map_err(|_| anyhow!("diagnostic health lock poisoned"))?
            .expect_restart()
    }
    pub fn worker_restart_complete(&self) -> bool {
        self.health.lock().is_ok_and(|health| {
            health
                .expected_restart
                .is_some_and(|restart| restart.completed)
        })
    }
    pub fn observe_editor_capture(
        &self,
        frames: u64,
        frame_age_seconds: Option<f64>,
        capture_error: Option<&str>,
    ) -> Result<()> {
        self.health
            .lock()
            .map_err(|_| anyhow!("diagnostic health lock poisoned"))?
            .observe_capture(
                self.start.elapsed().as_millis(),
                frames,
                frame_age_seconds,
                capture_error,
            );
        self.check_health()
    }
    pub fn close_editor_capture(&self) -> Result<()> {
        self.health
            .lock()
            .map_err(|_| anyhow!("diagnostic health lock poisoned"))?
            .close_capture();
        self.check_health()
    }
    pub fn check_health(&self) -> Result<()> {
        let mut health = self
            .health
            .lock()
            .map_err(|_| anyhow!("diagnostic health lock poisoned"))?;
        health.check_capture(self.start.elapsed().as_millis());
        if let Some(failure) = health.failure {
            bail!("{failure}");
        }
        Ok(())
    }
    /// Idempotent finalization. A claimed pass requires expected events and healthy
    /// worker/capture evidence, including recovery after an injected fixture fault.
    /// Returns the machine outcome actually written to summary.json.
    pub fn finish(&self, requested: Outcome, reason: &str) -> Result<Outcome> {
        if reason.len() > 1024 {
            bail!("diagnostic summary reason exceeds 1024 bytes");
        }
        let mut result = self
            .outcome
            .lock()
            .map_err(|_| anyhow!("diagnostic outcome lock poisoned"))?;
        if let Some(outcome) = *result {
            return Ok(outcome);
        }
        let missing: Vec<_> = self
            .scenario
            .expected_events
            .iter()
            .filter(|event| !self.event_seen(event))
            .cloned()
            .collect();
        let (health_failure, health_snapshot) = {
            let mut health = self
                .health
                .lock()
                .map_err(|_| anyhow!("diagnostic health lock poisoned"))?;
            let failure = health.finish_failure(self.start.elapsed().as_millis());
            (failure, serde_json::to_value(&*health)?)
        };
        let (outcome, final_reason) = if requested == Outcome::Passed {
            if let Some(failure) = health_failure {
                (Outcome::Failed, failure)
            } else if !missing.is_empty() {
                (
                    Outcome::Failed,
                    "required diagnostic events were not observed",
                )
            } else if self.expired() {
                (Outcome::Timeout, "scenario overall deadline")
            } else {
                (requested, reason)
            }
        } else {
            (
                requested,
                if requested == Outcome::Failed {
                    health_failure.unwrap_or(reason)
                } else {
                    reason
                },
            )
        };
        let metrics = self.metrics.snapshot();
        write_new_json(&self.root.join("metrics.json"), &metrics)?;
        // Failure to record final event must not prevent writing the authoritative summary.
        let event_error = self
            .event("run-finished", serde_json::json!({"outcome":outcome}))
            .err()
            .map(|_| "final event unavailable");
        write_new_json(
            &self.root.join("summary.json"),
            &serde_json::json!({"schema_version":1,"run_id":self.run_id,"outcome":outcome,"exit_code":outcome.exit_code(),"reason":final_reason,"requested_outcome":requested,"requested_reason":reason,"missing_events":missing,"health_failure":health_failure,"health":health_snapshot,"elapsed_ms":self.start.elapsed().as_millis(),"event_error":event_error}),
        )?;
        *result = Some(outcome);
        self.finished.store(true, Ordering::Release);
        Ok(outcome)
    }
    /// Deterministic, no-device/no-plugin gain+delay baseline. Not an engine integration test.
    pub fn run_offline(&self) -> Result<Outcome> {
        if self.scenario.mode != ScenarioMode::Offline {
            bail!("not an offline scenario");
        }
        let f = self
            .scenario
            .offline
            .as_ref()
            .ok_or_else(|| anyhow!("missing offline fixture"))?;
        self.event(
            "offline-ready",
            serde_json::json!({"sample_rate":f.sample_rate}),
        )?;
        let mut output = vec![0.0f32; f.frames];
        let mut position = 0;
        let mut block = 0;
        while position < f.frames {
            if self.expired() {
                return self.finish(Outcome::Timeout, "offline fixture deadline");
            }
            let n = f.block_sizes[block % f.block_sizes.len()].min(f.frames - position);
            let start = Instant::now();
            for (offset, value) in output[position..position + n].iter_mut().enumerate() {
                let sample = position + offset;
                *value = if sample == f.delay_frames {
                    f.amplitude * f.gain
                } else {
                    0.0
                };
            }
            self.metrics
                .record_dsp(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            // Intentionally not record_callback: this is not a device callback or a real xrun measurement.
            position += n;
            block += 1;
        }
        let impulse = output.iter().position(|v| v.abs() > f32::EPSILON);
        let expected = f.expected_impulse_frame.unwrap_or(f.delay_frames);
        let valid = if f.amplitude * f.gain == 0.0 {
            impulse.is_none()
        } else {
            impulse == Some(expected)
        };
        self.event("offline-complete", serde_json::json!({"frames":f.frames,"blocks":block,"impulse_frame":impulse,"expected_impulse_frame":expected,"assertion_passed":valid}))?;
        self.finish(
            if valid {
                Outcome::Passed
            } else {
                Outcome::Failed
            },
            "deterministic fixture impulse assertion",
        )
    }
}
impl Drop for DiagnosticRun {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            let _ = self.finish(
                Outcome::Aborted,
                "run dropped without explicit finalization",
            );
        }
    }
}
fn new_run_id() -> String {
    format!(
        "run-{}-{}-{}",
        unix_ms(),
        std::process::id(),
        RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}
fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[allow(unused_mut)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn open_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = open_new(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_run() -> DiagnosticRun {
        let scenario: Scenario =
            serde_json::from_str(r#"{"version":1,"name":"health-test"}"#).unwrap();
        let run_id = new_run_id();
        let root = std::env::temp_dir().join(format!("nodeplayer-{run_id}"));
        create_private_dir(&root).unwrap();
        let file = open_new(&root.join("events.jsonl")).unwrap();
        let health = Health::new(scenario.health);
        DiagnosticRun {
            scenario,
            root,
            run_id,
            metrics: metrics::Metrics::new(crate::runtime::MetricsMode::Minimal),
            start: Instant::now(),
            duration: Duration::from_secs(60),
            events: Mutex::new(EventWriter {
                file,
                bytes: 0,
                seen: HashSet::new(),
                sequence: 0,
            }),
            finished: AtomicBool::new(false),
            outcome: Mutex::new(None),
            health: Mutex::new(health),
        }
    }

    fn summary_and_cleanup(run: DiagnosticRun) -> serde_json::Value {
        let summary =
            serde_json::from_slice(&fs::read(run.root.join("summary.json")).unwrap()).unwrap();
        let root = run.root.clone();
        drop(run);
        fs::remove_dir_all(root).unwrap();
        summary
    }

    #[test]
    fn worker_fault_event_cannot_produce_a_pass() {
        let run = test_run();
        run.event(
            "worker-faulted",
            serde_json::json!({"stage":"checkpoint","request_id":42}),
        )
        .unwrap();
        assert_eq!(
            run.finish(Outcome::Passed, "all steps completed").unwrap(),
            Outcome::Failed
        );
        let summary = summary_and_cleanup(run);
        assert_eq!(summary["reason"], "unexpected worker fault");
        assert_eq!(summary["health"]["worker_faults"], 1);
    }

    #[test]
    fn transient_supervisor_fault_retains_evidence_and_failure_after_recovery() {
        let run = test_run();
        run.event("worker-status", serde_json::json!({"status":"running"}))
            .unwrap();
        let evidence = serde_json::json!({"status":"faulted: read timeout","request_id":42,"stage":"reply-header","generation":3});
        run.event("worker-status", evidence.clone()).unwrap();
        run.event("worker-status", serde_json::json!({"status":"running"}))
            .unwrap();
        assert!(run.event_seen("worker-faulted"));
        assert!(run.check_health().is_err());
        assert_eq!(
            run.finish(Outcome::Passed, "completed").unwrap(),
            Outcome::Failed
        );
        let events: Vec<serde_json::Value> = fs::read_to_string(run.root.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "worker-status" && event["payload"] == evidence)
        );
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "worker-faulted" && event["payload"] == evidence)
        );
        summary_and_cleanup(run);
    }

    #[test]
    fn capture_startup_deadline_and_short_false_pass_are_rejected() {
        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_capture(100, 0, None, None);
        health.check_capture(5099);
        assert_eq!(health.failure, None);
        assert_eq!(
            health.finish_failure(5099),
            Some("captured editor has no delivered frame")
        );
        health.check_capture(5100);
        assert_eq!(
            health.failure,
            Some("captured editor startup produced no frame")
        );
    }

    #[test]
    fn capture_starvation_and_errors_latch_even_if_frames_resume() {
        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_capture(100, 1, Some(0.0), None);
        health.check_capture(2100);
        assert_eq!(health.failure, None);
        health.check_capture(2101);
        assert_eq!(health.failure, Some("captured editor frame starvation"));
        health.observe_capture(2102, 2, Some(0.0), None);
        assert_eq!(
            health.finish_failure(2102),
            Some("captured editor frame starvation")
        );

        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_capture(100, 0, None, Some("readback failed"));
        health.observe_capture(101, 1, Some(0.0), None);
        assert_eq!(
            health.finish_failure(101),
            Some("editor capture reported an error")
        );
    }

    #[test]
    fn fixture_fault_requires_observation_and_explicit_recovery() {
        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_status("running", 0);
        health.expect_fault(true).unwrap();
        assert_eq!(
            health.finish_failure(10),
            Some("expected fixture worker fault was not observed")
        );
        assert!(health.expect_restart().is_err());
        health.observe_status("faulted: DSP progress timeout", 20);
        assert_eq!(health.failure, None);
        assert_eq!(
            health.finish_failure(20),
            Some("fixture worker fault requires an explicit successful restart")
        );
        health.expect_restart().unwrap();
        health.observe_status("starting", 30);
        health.observe_status("running", 40);
        assert_eq!(health.finish_failure(40), None);
        assert_eq!(health.worker_faults, 1);
        assert_eq!(health.expected_worker_faults, 1);
        assert_eq!(health.recovered_worker_faults, 1);
    }

    #[test]
    fn recovered_capture_requires_a_new_delivered_frame() {
        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_status("running", 0);
        health.observe_capture(0, 4, Some(0.0), None);
        health.expect_fault(true).unwrap();
        health.observe_status("faulted: fixture crash", 1);
        health.observe_capture(10_000, 4, Some(10.0), Some("worker closed"));
        assert_eq!(health.failure, None);
        health.expect_restart().unwrap();
        health.observe_status("starting", 10_001);
        health.observe_status("running", 10_002);
        health.observe_capture(10_003, 4, Some(10.0), None);
        assert_eq!(
            health.finish_failure(10_003),
            Some("captured editor has no delivered frame")
        );
        health.observe_capture(10_004, 5, Some(0.0), None);
        assert_eq!(health.finish_failure(10_004), None);
    }

    #[test]
    fn recovered_explicit_fixture_restart_can_finish_passed() {
        let run = test_run();
        run.event("worker-status", serde_json::json!({"status":"running"}))
            .unwrap();
        run.expect_fixture_fault(true).unwrap();
        run.event(
            "worker-status",
            serde_json::json!({"status":"faulted: fixture crash"}),
        )
        .unwrap();
        run.expect_worker_restart().unwrap();
        run.event("worker-status", serde_json::json!({"status":"starting"}))
            .unwrap();
        run.event("worker-status", serde_json::json!({"status":"running"}))
            .unwrap();
        assert!(run.worker_restart_complete());
        assert_eq!(
            run.finish(Outcome::Passed, "recovered fixture").unwrap(),
            Outcome::Passed
        );
        let summary = summary_and_cleanup(run);
        assert_eq!(summary["health"]["expected_worker_faults"], 1);
        assert_eq!(summary["health"]["recovered_worker_faults"], 1);
        assert!(summary["health_failure"].is_null());
    }

    #[test]
    fn no_delivered_capture_cannot_finish_passed_during_startup_grace() {
        let run = test_run();
        run.observe_editor_capture(0, None, None).unwrap();
        assert_eq!(
            run.finish(Outcome::Passed, "short scenario").unwrap(),
            Outcome::Failed
        );
        let summary = summary_and_cleanup(run);
        assert_eq!(
            summary["health_failure"],
            "captured editor has no delivered frame"
        );
    }

    #[test]
    fn explicit_restart_only_exempts_its_own_teardown_reason() {
        let mut health = Health::new(scenario::HealthPolicy::default());
        health.observe_status("running", 0);
        health.expect_restart().unwrap();
        health.observe_status("running", 1);
        assert!(!health.expected_restart.unwrap().completed);
        health.observe_status("faulted: explicit worker restart requested", 2);
        health.observe_status("starting", 3);
        health.observe_status("running", 4);
        assert_eq!(health.finish_failure(4), None);
        assert_eq!(health.worker_faults, 0);
        health.expect_restart().unwrap();
        health.observe_status("faulted: read timeout", 5);
        health.observe_status("starting", 6);
        health.observe_status("running", 7);
        assert_eq!(health.finish_failure(7), Some("unexpected worker fault"));
    }

    #[test]
    fn terminal_fixture_fault_must_be_explicit_and_observed() {
        let run = test_run();
        run.expect_fixture_fault(false).unwrap();
        run.event(
            "worker-status",
            serde_json::json!({"status":"faulted: fixture crash"}),
        )
        .unwrap();
        assert_eq!(
            run.finish(Outcome::Passed, "expected terminal fixture fault")
                .unwrap(),
            Outcome::Passed
        );
        let summary = summary_and_cleanup(run);
        assert_eq!(summary["health"]["expected_worker_faults"], 1);
        assert_eq!(summary["health"]["expected_fault"]["recover"], false);
    }

    #[test]
    fn elapsed_overall_deadline_cannot_finish_passed() {
        let mut run = test_run();
        run.duration = Duration::ZERO;
        assert_eq!(
            run.finish(Outcome::Passed, "completed").unwrap(),
            Outcome::Timeout
        );
        summary_and_cleanup(run);
    }

    #[test]
    fn run_ids_are_distinct_without_global_runtime_mutation() {
        assert_ne!(new_run_id(), new_run_id());
    }
    #[test]
    fn outcome_codes_are_unambiguous() {
        assert_eq!(Outcome::Passed.exit_code(), 0);
        assert_eq!(Outcome::Failed.exit_code(), 2);
        assert_eq!(Outcome::Timeout.exit_code(), 124);
        assert_eq!(Outcome::Aborted.exit_code(), 125);
    }
}
