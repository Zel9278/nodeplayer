//! Startup-only policy. Parse OS strings so paths need not be valid UTF-8.
use anyhow::{Result, anyhow, bail};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

pub const HELP: &str = "nodeplayer [options] [file-or-folder]\n\
  --experimental-pdc                 Enable experimental delay compensation\n\
  --experimental-plugin-isolation    Request plugin isolation (also enables PDC)\n\
  --experimental-editor-capture      Composite plugin editors in the graph\n\
  --data-root <directory>             Separate config/cache/state/data\n\
  --diagnostic-scenario <json>        Run an explicit, restricted scenario\n\
  --diagnostic-output <directory>     New, exclusive artifact directory\n\
  --diagnostic-duration <seconds>     Overall in-app deadline (default 30)\n\
  --diagnostic-metrics <off|minimal>   Atomic instrumentation (default minimal)\n\
  --help, -h                         Show this help\n\
  --                                 Treat remaining argument as a path\n\
Existing commands: --scan-plugin <format> <path>; --test-plugin <format> <path> [filter];\n\
--test-vst3 <bundle> [filter]; --test-editor <bundle> <name> <out.ppm>\n\
--test-isolated <format> <path>         Explicit isolated fixture bridge test\n\
Diagnostics disable normal-profile restore, Native extensions, external networking,\n\
auto-downloads and broad library/plugin discovery. Plugin-owned OS data is NOT isolated.\n";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricsMode {
    Off,
    #[default]
    Minimal,
}

#[derive(Debug, Clone)]
pub struct DiagnosticOptions {
    pub scenario: PathBuf,
    /// Must not exist. This is the run directory, not the user's profile.
    pub output: Option<PathBuf>,
    pub duration: Duration,
    pub metrics: MetricsMode,
    /// Set only by diagnostics::prepare after validating an explicit device-output scenario.
    pub audio_device: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    pub experimental_pdc: bool,
    pub experimental_plugin_isolation: bool,
    pub experimental_editor_capture: bool,
    pub data_root: Option<PathBuf>,
    pub diagnostic: Option<DiagnosticOptions>,
}
impl RuntimeOptions {
    pub fn is_diagnostic(&self) -> bool {
        self.diagnostic.is_some()
    }
    pub fn allow_normal_profile(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_native_extensions(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_network(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_auto_downloads(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_library_discovery(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_plugin_discovery(&self) -> bool {
        !self.is_diagnostic()
    }
    pub fn allow_audio_device(&self) -> bool {
        self.diagnostic.as_ref().is_none_or(|d| d.audio_device)
    }
}

static RUNTIME: OnceLock<RuntimeOptions> = OnceLock::new();
static DEFAULT: OnceLock<RuntimeOptions> = OnceLock::new();
/// Call exactly once, after diagnostics::prepare and before spawning any threads.
pub fn install(mut options: RuntimeOptions) -> Result<()> {
    options.experimental_pdc |= options.experimental_plugin_isolation;
    if options.is_diagnostic() && options.data_root.is_none() {
        bail!("diagnostic runtime must be prepared before installation");
    }
    RUNTIME
        .set(options)
        .map_err(|_| anyhow!("runtime options already installed"))
}
/// Read-only global policy. Tests should construct options/Paths directly, not install globals.
pub fn get() -> &'static RuntimeOptions {
    RUNTIME
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(RuntimeOptions::default))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Gui {
        initial_file: Option<PathBuf>,
    },
    ScanPlugin {
        format: String,
        path: PathBuf,
    },
    TestPlugin {
        format: String,
        path: PathBuf,
        filter: String,
    },
    TestIsolated {
        format: String,
        path: PathBuf,
    },
    TestVst3 {
        bundle: PathBuf,
        filter: String,
    },
    TestEditor {
        bundle: PathBuf,
        name: String,
        output: PathBuf,
    },
    PluginWorker {
        args: Vec<OsString>,
    },
    Help,
}
#[derive(Debug, Clone)]
pub struct ParsedArgs {
    pub mode: Mode,
    pub options: RuntimeOptions,
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<ParsedArgs> {
    let mut args = args.into_iter().peekable();
    // Worker argv belongs to its versioned internal parser, never the GUI parser.
    if args
        .peek()
        .is_some_and(|s| s == OsStr::new("--plugin-worker"))
    {
        args.next();
        let mut worker_args = Vec::new();
        let mut options = RuntimeOptions::default();
        while let Some(arg) = args.next() {
            if arg == OsStr::new("--data-root") {
                if options.data_root.is_some() {
                    bail!("duplicate worker --data-root");
                }
                options.data_root = Some(nonempty_path(args.next().ok_or_else(|| anyhow!("--data-root requires a value"))?)?);
            } else {
                worker_args.push(arg);
            }
        }
        return Ok(ParsedArgs {
            mode: Mode::PluginWorker { args: worker_args },
            options,
        });
    }
    let mut options = RuntimeOptions::default();
    let mut mode: Option<&'static str> = None;
    let mut positional = Vec::new();
    let mut scenario = None;
    let mut output = None;
    let mut duration = None;
    let mut metrics = None;
    let mut help = false;
    let mut literal = false;
    let mut unknown_options = Vec::new();
    while let Some(arg) = args.next() {
        if literal {
            positional.push(arg);
            continue;
        }
        let take = |args: &mut std::iter::Peekable<_>, flag: &str| -> Result<OsString> {
            args.next()
                .ok_or_else(|| anyhow!("{flag} requires a value"))
        };
        match arg.to_str() {
            Some("--") => literal = true,
            Some("--help" | "-h") => help = true,
            Some("--experimental-pdc") => options.experimental_pdc = true,
            Some("--experimental-plugin-isolation") => options.experimental_plugin_isolation = true,
            Some("--experimental-editor-capture") => options.experimental_editor_capture = true,
            Some("--data-root") => {
                if options.data_root.is_some() {
                    bail!("duplicate --data-root");
                }
                options.data_root = Some(nonempty_path(take(&mut args, "--data-root")?)?);
            }
            Some("--diagnostic-scenario") => {
                if scenario.is_some() {
                    bail!("duplicate --diagnostic-scenario");
                }
                scenario = Some(nonempty_path(take(&mut args, "--diagnostic-scenario")?)?);
            }
            Some("--diagnostic-output") => {
                if output.is_some() {
                    bail!("duplicate --diagnostic-output");
                }
                output = Some(nonempty_path(take(&mut args, "--diagnostic-output")?)?);
            }
            Some("--diagnostic-duration") => {
                if duration.is_some() {
                    bail!("duplicate --diagnostic-duration");
                }
                let text = utf8(take(&mut args, "--diagnostic-duration")?, "duration")?;
                let seconds: f64 = text
                    .parse()
                    .map_err(|_| anyhow!("invalid diagnostic duration"))?;
                if !seconds.is_finite() || seconds <= 0.0 || seconds > 86_400.0 {
                    bail!("diagnostic duration must be > 0 and <= 86400 seconds");
                }
                duration = Some(
                    Duration::try_from_secs_f64(seconds)
                        .map_err(|_| anyhow!("invalid diagnostic duration"))?,
                );
            }
            Some("--diagnostic-metrics") => {
                if metrics.is_some() {
                    bail!("duplicate --diagnostic-metrics");
                }
                metrics = Some(match take(&mut args, "--diagnostic-metrics")?.to_str() {
                    Some("off") => MetricsMode::Off,
                    Some("minimal") => MetricsMode::Minimal,
                    _ => bail!(
                        "diagnostic metrics must be off or minimal (detailed tracing is not implemented)"
                    ),
                });
            }
            Some(
                flag @ ("--scan-plugin" | "--test-plugin" | "--test-vst3" | "--test-editor"
                | "--test-isolated"),
            ) => {
                if mode.is_some() {
                    bail!("only one scan/test mode may be specified");
                }
                mode = Some(match flag {
                    "--scan-plugin" => "scan",
                    "--test-plugin" => "plugin",
                    "--test-vst3" => "vst3",
                    "--test-isolated" => "isolated",
                    _ => "editor",
                });
            }
            _ if arg.as_encoded_bytes().starts_with(b"-") => {
                unknown_options.push(arg.clone());
                positional.push(arg);
            }
            _ => positional.push(arg),
        }
    }
    if scenario.is_none() && (output.is_some() || duration.is_some() || metrics.is_some()) {
        bail!("diagnostic options require --diagnostic-scenario");
    }
    if scenario.is_some() && (mode.is_some() || !positional.is_empty()) {
        bail!("diagnostic scenarios cannot be combined with scan/test commands or an initial file");
    }
    if mode == Some("isolated") {
        options.experimental_plugin_isolation = true;
    }
    options.experimental_pdc |= options.experimental_plugin_isolation;
    if !unknown_options.is_empty() && (options.experimental_pdc || options.experimental_plugin_isolation || options.experimental_editor_capture) {
        bail!("unknown option {:?}; use -- before a path beginning with '-'", unknown_options[0]);
    }
    if let Some(scenario) = scenario {
        options.diagnostic = Some(DiagnosticOptions {
            scenario,
            output,
            duration: duration.unwrap_or(Duration::from_secs(30)),
            metrics: metrics.unwrap_or_default(),
            audio_device: false,
        });
    }
    let count = positional.len();
    let mut p = positional.into_iter();
    let mode = if help {
        Mode::Help
    } else {
        match mode {
            None if count <= 1 || (!options.experimental_pdc && !options.experimental_plugin_isolation) => Mode::Gui {
                initial_file: p.next().map(PathBuf::from),
            },
            Some("scan") if count == 2 => Mode::ScanPlugin {
                format: utf8(p.next().unwrap(), "format")?,
                path: p.next().unwrap().into(),
            },
            Some("isolated") if count == 2 => Mode::TestIsolated {
                format: utf8(p.next().unwrap(), "format")?,
                path: p.next().unwrap().into(),
            },
            Some("plugin") if (2..=3).contains(&count) => Mode::TestPlugin {
                format: utf8(p.next().unwrap(), "format")?,
                path: p.next().unwrap().into(),
                filter: utf8(p.next().unwrap_or_default(), "filter")?,
            },
            Some("vst3") if (1..=2).contains(&count) => Mode::TestVst3 {
                bundle: p.next().unwrap().into(),
                filter: utf8(p.next().unwrap_or_default(), "filter")?,
            },
            Some("editor") if count == 3 => Mode::TestEditor {
                bundle: p.next().unwrap().into(),
                name: utf8(p.next().unwrap(), "name")?,
                output: p.next().unwrap().into(),
            },
            _ => bail!("incorrect number of arguments; see --help"),
        }
    };
    Ok(ParsedArgs { mode, options })
}
fn nonempty_path(s: OsString) -> Result<PathBuf> {
    if s == OsStr::new("") {
        bail!("path cannot be empty");
    }
    Ok(s.into())
}
fn utf8(s: OsString, label: &str) -> Result<String> {
    s.into_string()
        .map_err(|_| anyhow!("{label} must be valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(a: &[&str]) -> Result<ParsedArgs> {
        parse_args(a.iter().map(OsString::from))
    }
    #[test]
    fn defaults_and_paths() {
        let p = parse(&["音 楽/file.flac"]).unwrap();
        assert!(!p.options.experimental_pdc);
        assert!(p.options.allow_network());
        assert_eq!(
            p.mode,
            Mode::Gui {
                initial_file: Some("音 楽/file.flac".into())
            }
        );
        assert_eq!(
            parse(&["--", "--song"]).unwrap().mode,
            Mode::Gui {
                initial_file: Some("--song".into())
            }
        );
    }
    #[test]
    fn flags_and_errors() {
        let p = parse(&["--experimental-plugin-isolation", "--data-root", "a b"]).unwrap();
        assert!(p.options.experimental_pdc && p.options.experimental_plugin_isolation);
        let capture = parse(&["--experimental-editor-capture"]).unwrap();
        assert!(capture.options.experimental_editor_capture);
        assert!(!capture.options.experimental_plugin_isolation && !capture.options.experimental_pdc);
        assert!(parse(&["--experimental-editor-capture", "--unknown"]).is_err());
        assert!(matches!(parse(&["--unknown"]).unwrap().mode, Mode::Gui { .. }));
        assert!(matches!(parse(&["a", "b"]).unwrap().mode, Mode::Gui { .. }));
        assert!(parse(&["--experimental-pdc", "--unknown"]).is_err());
        assert!(parse(&["--experimental-plugin-isolation", "a", "b"]).is_err());
        for a in [
            & ["--data-root"][..],
            &["--diagnostic-duration", "NaN"][..],
            &["--diagnostic-output", "out"][..],
        ] {
            assert!(parse(a).is_err());
        }
    }
    #[test]
    fn internal_worker_args_are_opaque() {
        let p = parse(&["--plugin-worker", "--unknown-internal", "space 日本語"]).unwrap();
        assert_eq!(
            p.mode,
            Mode::PluginWorker {
                args: vec!["--unknown-internal".into(), "space 日本語".into()]
            }
        );
        assert!(!p.options.experimental_plugin_isolation);
    }
    #[test]
    fn worker_data_root_is_parsed_separately_from_worker_arguments() {
        let parsed = parse(&["--plugin-worker", "127.0.0.1:1234", "mapping", "2", "--data-root", "worker root"]).unwrap();
        assert_eq!(parsed.options.data_root.as_deref(), Some(std::path::Path::new("worker root")));
        assert_eq!(parsed.mode, Mode::PluginWorker { args: vec!["127.0.0.1:1234".into(), "mapping".into(), "2".into()] });
        assert!(parse(&["--plugin-worker", "--data-root"]).is_err());
    }
    #[test]
    fn isolated_test_enables_required_policy() {
        let p = parse(&["--test-isolated", "CLAP", "fixture.clap"]).unwrap();
        assert!(matches!(p.mode, Mode::TestIsolated { .. }));
        assert!(p.options.experimental_pdc && p.options.experimental_plugin_isolation);
        assert!(parse(&["--test-isolated", "CLAP"]).is_err());
    }
    #[test]
    fn legacy_modes() {
        assert!(matches!(
            parse(&["--scan-plugin", "CLAP", "a b.clap"]).unwrap().mode,
            Mode::ScanPlugin { .. }
        ));
        assert!(matches!(
            parse(&["--test-plugin", "CLAP", "a", "name"]).unwrap().mode,
            Mode::TestPlugin { .. }
        ));
        assert!(matches!(
            parse(&["--test-vst3", "a"]).unwrap().mode,
            Mode::TestVst3 { .. }
        ));
        assert!(matches!(
            parse(&["--test-editor", "a", "b", "c"]).unwrap().mode,
            Mode::TestEditor { .. }
        ));
    }
    #[test]
    fn diagnostic_policy_is_explicit() {
        let p = parse(&[
            "--diagnostic-scenario",
            "s.json",
            "--diagnostic-duration",
            "0.2",
            "--diagnostic-metrics",
            "off",
        ])
        .unwrap();
        assert!(
            !p.options.allow_normal_profile()
                && !p.options.allow_native_extensions()
                && !p.options.allow_network()
        );
        assert!(!p.options.allow_audio_device() && !p.options.allow_library_discovery());
        assert_eq!(
            p.options.diagnostic.unwrap().duration,
            Duration::from_millis(200)
        );
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_preserved() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(vec![b'a', 0xff]);
        assert_eq!(
            parse_args([path.clone()]).unwrap().mode,
            Mode::Gui {
                initial_file: Some(path.into())
            }
        );
    }
}
