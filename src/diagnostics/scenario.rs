//! Versioned, explicit fixture descriptions. No command strings are executed.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioMode {
    #[default]
    Gui,
    Offline,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioOutput {
    #[default]
    Null,
    Device,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginFixture {
    pub format: String,
    pub path: PathBuf,
    #[serde(default)]
    pub class_id: Option<String>,
    #[serde(default)]
    pub name_filter: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Step {
    WaitEvent {
        event: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    OpenEditor {
        #[serde(default = "yes")]
        embedded: bool,
    },
    CloseEditor,
    /// Exercise the ordinary graph transform without changing the plugin GUI scale.
    GraphZoom {
        factor: f32,
    },
    Screenshot {
        name: String,
    },
    SetParam {
        id: u32,
        value: f64,
    },
    RestartHost,
    Wait {
        duration_ms: u64,
    },
    AssertStatus {
        contains: String,
    },
    /// Fixture-only fault request; actual support must be negotiated with the fixture/worker.
    Fault {
        kind: String,
    },
    Finish,
}
fn default_timeout() -> u64 {
    5000
}
fn yes() -> bool {
    true
}

/// Small deterministic offline baseline, NOT a plugin/engine correctness test.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineFixture {
    #[serde(default = "sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "block_sizes")]
    pub block_sizes: Vec<usize>,
    #[serde(default = "frames")]
    pub frames: usize,
    #[serde(default)]
    pub delay_frames: usize,
    #[serde(default = "gain")]
    pub gain: f32,
    #[serde(default = "amplitude")]
    pub amplitude: f32,
    #[serde(default)]
    pub expected_impulse_frame: Option<usize>,
}
fn sample_rate() -> u32 {
    48_000
}
fn block_sizes() -> Vec<usize> {
    vec![1, 127, 256, 511, 512, 1024]
}
fn frames() -> usize {
    48_000
}
fn gain() -> f32 {
    1.0
}
fn amplitude() -> f32 {
    0.01
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub mode: ScenarioMode,
    /// Real device output requires explicit scenario opt-in; default is a null engine.
    #[serde(default)]
    pub audio_output: AudioOutput,
    #[serde(default)]
    pub plugin: Option<PluginFixture>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expected_events: Vec<String>,
    #[serde(default)]
    pub offline: Option<OfflineFixture>,
}
impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        const LIMIT: u64 = 1024 * 1024;
        let mut bytes = Vec::new();
        File::open(path)
            .context("open diagnostic scenario")?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            bail!("diagnostic scenario exceeds 1 MiB");
        }
        let mut scenario: Self =
            serde_json::from_slice(&bytes).context("parse diagnostic scenario")?;
        scenario.validate()?;
        if let Some(plugin) = &mut scenario.plugin {
            if plugin.path.is_relative() {
                plugin.path = path.parent().unwrap_or(Path::new(".")).join(&plugin.path);
            }
            plugin.path = std::fs::canonicalize(&plugin.path)
                .context("explicit plugin fixture does not exist")?;
        }
        Ok(scenario)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported diagnostic scenario version {}", self.version);
        }
        if self.name.is_empty() || self.name.len() > 128 || self.name.chars().any(char::is_control)
        {
            bail!("invalid scenario name");
        }
        if self.steps.len() > 1024 || self.expected_events.len() > 128 {
            bail!("too many scenario steps/events");
        }
        for event in &self.expected_events {
            validate_event_name(event)?;
        }
        for step in &self.steps {
            match step {
                Step::WaitEvent { event, timeout_ms } => {
                    validate_event_name(event)?;
                    if *timeout_ms == 0 || *timeout_ms > 86_400_000 {
                        bail!("invalid event wait timeout");
                    }
                }
                Step::SetParam { value, .. }
                    if !value.is_finite() || !(0.0..=1.0).contains(value) =>
                {
                    bail!("parameter value must be finite and normalized")
                }
                Step::GraphZoom { factor } if !factor.is_finite() || !(0.2..=3.0).contains(factor) => {
                    bail!("graph zoom must be finite and between 0.2 and 3.0");
                }
                Step::Screenshot { name } if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') => {
                    bail!("invalid diagnostic screenshot name");
                }
                Step::Wait { duration_ms } if *duration_ms > 86_400_000 => {
                    bail!("wait duration exceeds limit")
                }
                Step::AssertStatus { contains } if contains.is_empty() || contains.len() > 256 => {
                    bail!("invalid status assertion")
                }
                Step::Fault { kind }
                    if !matches!(
                        kind.as_str(),
                        "crash" | "dsp-hang" | "gui-hang" | "state-hang" | "drop-hang" | "late"
                    ) =>
                {
                    bail!("unsupported fault kind")
                }
                _ => (),
            }
        }
        if let Some(plugin) = &self.plugin {
            if !matches!(plugin.format.to_ascii_lowercase().as_str(), "clap" | "vst3") {
                bail!("diagnostic plugin fixture must explicitly name CLAP or VST3");
            }
            if plugin.path.as_os_str().is_empty() {
                bail!("empty plugin path");
            }
        }
        match (self.mode, &self.offline) {
            (ScenarioMode::Offline, Some(f)) => {
                if self.audio_output != AudioOutput::Null {
                    bail!("offline fixtures cannot use device audio");
                }
                if self.plugin.is_some() || !self.steps.is_empty() {
                    bail!("offline fixture cannot execute plugin or GUI steps");
                }
                if !(8000..=384_000).contains(&f.sample_rate)
                    || f.frames == 0
                    || f.frames > 10_000_000
                {
                    bail!("offline sample rate/frame limit exceeded");
                }
                if f.delay_frames >= f.frames
                    || f.block_sizes.is_empty()
                    || f.block_sizes.len() > 128
                    || f.block_sizes.iter().any(|n| *n == 0 || *n > 65_536)
                {
                    bail!("invalid offline delay/block sizes");
                }
                if !f.amplitude.is_finite()
                    || f.amplitude.abs() > 0.05
                    || !f.gain.is_finite()
                    || f.gain.abs() > 16.0
                {
                    bail!("offline fixture level/gain out of range");
                }
                if f.expected_impulse_frame.is_some_and(|n| n >= f.frames) {
                    bail!("expected impulse is outside fixture");
                }
            }
            (ScenarioMode::Offline, None) => bail!("offline mode requires an offline fixture"),
            (ScenarioMode::Gui, Some(_)) => bail!("GUI mode cannot execute offline fixture"),
            _ => (),
        }
        Ok(())
    }
}
pub fn validate_event_name(event: &str) -> Result<()> {
    if event.is_empty()
        || event.len() > 96
        || !event
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid diagnostic event name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_version_and_fields() {
        assert!(
            serde_json::from_str::<Scenario>(r#"{"version":1,"name":"x","shell":"rm"}"#).is_err()
        );
        let mut s: Scenario = serde_json::from_str(
            r#"{"version":1,"name":"baseline","mode":"offline","offline":{}}"#,
        )
        .unwrap();
        assert!(s.validate().is_ok());
        s.version = 2;
        assert!(s.validate().is_err());
    }
    #[test]
    fn event_names_and_limits() {
        assert!(validate_event_name("app-ready").is_ok());
        assert!(validate_event_name("private/path").is_err());
        let s: Scenario = serde_json::from_str(
            r#"{"version":1,"name":"bad","mode":"offline","offline":{"block_sizes":[0]}}"#,
        )
        .unwrap();
        assert!(s.validate().is_err());
    }
}
