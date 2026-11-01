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
        "fixture": scenario.plugin.as_ref().map(|p| serde_json::json!({"format":p.format,"class_id":p.class_id,"name_filter":p.name_filter,"path":"[explicit fixture path redacted]"})),
        "sample_rate": scenario.offline.as_ref().map(|f| f.sample_rate),
        "requested_block_sizes": scenario.offline.as_ref().map(|f| &f.block_sizes),
        "actual_device_callback_sizes": serde_json::Value::Null,
        "q": serde_json::Value::Null, "d": serde_json::Value::Null, "h": serde_json::Value::Null,
        "policy": {"normal_profile":false,"native_extensions":false,"network":false,"auto_downloads":false,"library_discovery":false,"device_audio":scenario.audio_output == AudioOutput::Device},
        "limitations": ["plugin-owned OS profile data is not isolated", "external watchdog required for UI/process hangs", "unavailable measurements are null", "screenshots require explicit scenario steps; no detailed trace, OS profiling or uploads"]
    });
    write_new_json(&root.join("manifest.json"), &manifest)?;
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
    /// Idempotent finalization. A claimed pass is downgraded if required events are absent.
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
        let outcome = if requested == Outcome::Passed && !missing.is_empty() {
            Outcome::Failed
        } else {
            requested
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
            &serde_json::json!({"schema_version":1,"run_id":self.run_id,"outcome":outcome,"exit_code":outcome.exit_code(),"reason":reason,"missing_events":missing,"elapsed_ms":self.start.elapsed().as_millis(),"event_error":event_error}),
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
