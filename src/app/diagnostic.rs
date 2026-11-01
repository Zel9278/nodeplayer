//! UI/control-thread scenario driver. Plugin code always stays in the selected host.
use super::*;
use crate::diagnostics::{DiagnosticRun, Outcome, Step};
use anyhow::{Result, anyhow, bail};
use serde_json::json;

pub(super) struct GuiDiagnostic {
    run: Arc<DiagnosticRun>,
    pub(super) node: Option<NodeId>,
    next: usize,
    step_started: Instant,
    last_status: String,
    editor_reported: bool,
    scan: Option<std::sync::mpsc::Receiver<Result<PluginInfo>>>,
    started: bool,
    next_telemetry: Instant,
    step_requested: bool,
    restart_saw_down: bool,
    pub(super) editor_zoom: Option<f32>,
}

impl App {
    pub fn attach_diagnostic(&mut self, run: Arc<DiagnosticRun>) {
        let scan = run.scenario.plugin.clone().map(|fixture| {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let result = (|| {
                    let format = match fixture.format.to_ascii_lowercase().as_str() {
                        "clap" => Format::Clap,
                        "vst3" => Format::Vst3,
                        _ => bail!("unsupported diagnostic format"),
                    };
                    let classes = plugins::scan_isolated(format, &fixture.path)?;
                    classes
                        .into_iter()
                        .find(|info| {
                            fixture.class_id.as_ref().is_none_or(|id| *id == info.id)
                                && fixture
                                    .name_filter
                                    .as_ref()
                                    .is_none_or(|filter| info.name.contains(filter))
                        })
                        .ok_or_else(|| anyhow!("explicit diagnostic class was not found"))
                })();
                let _ = tx.send(result);
            });
            rx
        });
        self.diagnostic = Some(GuiDiagnostic {
            run,
            node: None,
            next: 0,
            step_started: Instant::now(),
            last_status: String::new(),
            editor_reported: false,
            scan,
            started: false,
            next_telemetry: Instant::now(),
            step_requested: false,
            restart_saw_down: false,
            editor_zoom: None,
        });
    }

    pub(super) fn tick_diagnostic(&mut self, ctx: &egui::Context) {
        let Some(mut diagnostic) = self.diagnostic.take() else {
            return;
        };
        if diagnostic.run.outcome().is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.diagnostic = Some(diagnostic);
            return;
        }
        let result = self.advance_diagnostic(&mut diagnostic, ctx);
        let outcome = if let Err(error) = result {
            log::error!("diagnostic scenario failed: {error:#}");
            Some((Outcome::Failed, "scenario operation failed"))
        } else if diagnostic.run.expired() {
            Some((Outcome::Timeout, "scenario overall deadline"))
        } else {
            None
        };
        if let Some((outcome, reason)) = outcome {
            if let Err(error) = diagnostic.run.finish(outcome, reason) {
                log::error!("cannot finish diagnostic: {error:#}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.diagnostic = Some(diagnostic);
    }

    fn advance_diagnostic(&mut self, diagnostic: &mut GuiDiagnostic, ctx: &egui::Context) -> Result<()> {
        let run = &diagnostic.run;
        if !diagnostic.started {
            diagnostic.started = true;
            run.event(
                "app-ready",
                json!({"audio_backend": self.engine.as_ref().map(|e| &e.device_name)}),
            )?;
            if self.engine.is_none() {
                bail!("diagnostic audio engine did not start");
            }
        }
        if let Some(rx) = &diagnostic.scan {
            match rx.try_recv() {
                Ok(result) => {
                    let info = result?;
                    diagnostic.node = self.add_plugin(&info, pos2(120.0, 100.0));
                    let id = diagnostic
                        .node
                        .ok_or_else(|| anyhow!("diagnostic plugin insertion failed"))?;
                    // Use the ordinary graph and processor path; no special DSP shortcut.
                    if let Some(output) = self
                        .snarl
                        .node_ids()
                        .find(|(_, n)| matches!(n.kind, NodeKind::Output { .. }))
                        .map(|(id, _)| id)
                    {
                        let inputs: Vec<_> = self
                            .snarl
                            .wires()
                            .filter(|(_, i)| i.node == output)
                            .collect();
                        for (o, i) in inputs {
                            self.snarl.disconnect(o, i);
                        }
                        self.snarl.connect(
                            OutPinId {
                                node: id,
                                output: 0,
                            },
                            InPinId {
                                node: output,
                                input: 0,
                            },
                        );
                    }
                    diagnostic.scan = None;
                    self.dirty = true;
                    run.event("plugin-added", json!({"format":info.format.label()}))?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    bail!("diagnostic scan worker disconnected")
                }
            }
        }
        if Instant::now() >= diagnostic.next_telemetry {
            let counters = diagnostic.node.and_then(|id| match &self.snarl[id].kind {
                NodeKind::Plugin(plugin) => plugin.instance.remote_counters(),
                _ => None,
            });
            run.event(
                "engine-progress",
                json!({
                    "sample_clock":self.engine.as_ref().map(Engine::sample_clock),
                    "plan_epoch":self.engine.as_ref().map(Engine::applied_epoch),
                    "midi_overflows":self.engine.as_ref().map(Engine::midi_overflows),
                    "bridge_underruns":counters.map(|c| c.0),
                    "bridge_overflows":counters.map(|c| c.1),
                }),
            )?;
            diagnostic.next_telemetry = Instant::now() + Duration::from_millis(250);
        }
        if let Some(id) = diagnostic.node {
            let NodeKind::Plugin(plugin) = &mut self.snarl[id].kind else {
                bail!("diagnostic plugin disappeared")
            };
            let status = plugin
                .instance
                .remote_status()
                .unwrap_or("local")
                .to_string();
            if status != diagnostic.last_status {
                run.event("host-status", json!({"status":status,"latency_frames":plugin.instance.latency(),"bridge_frames":plugin.instance.bridge_latency()}))?;
                let lower = status.to_ascii_lowercase();
                if lower.contains("running") || lower == "local" {
                    run.event("worker-ready", json!({}))?;
                }
                if lower.contains("fault")
                    || lower.contains("unresponsive")
                    || lower.contains("fail")
                {
                    run.event("worker-faulted", json!({}))?;
                }
                diagnostic.last_status = status;
            }
            if !diagnostic.editor_reported && plugin.editor.as_ref().is_some_and(|e| e.ready()) {
                diagnostic.editor_reported = true;
                run.event("editor-attached", json!({"embedded":plugin.is_embedded()}))?;
            }
        }
        let Some(step) = run.scenario.steps.get(diagnostic.next).cloned() else {
            run.finish(Outcome::Passed, "all scenario steps completed")?;
            return Ok(());
        };
        let mut complete = true;
        match step {
            Step::GraphZoom { factor } => {
                diagnostic.editor_zoom = Some(factor);
                run.event("graph-zoom", json!({"factor":factor}))?;
            }
            Step::Screenshot { name } => {
                complete = false;
                if !diagnostic.step_requested {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(name.clone())));
                    diagnostic.step_requested = true;
                }
                for event in ctx.input(|input| input.events.clone()) {
                    if let egui::Event::Screenshot { user_data, image, .. } = event
                        && user_data.data.as_ref().and_then(|data| data.downcast_ref::<String>()) == Some(&name)
                    {
                        let bytes: Vec<u8> = image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect();
                        image::save_buffer(run.root.join(format!("{name}.png")), &bytes, image.width() as u32, image.height() as u32, image::ColorType::Rgba8)?;
                        let dimensions = diagnostic.node.and_then(|id| match &self.snarl[id].kind {
                            NodeKind::Plugin(plugin) => plugin.editor_view.dimensions(),
                            _ => None,
                        });
                        run.event("screenshot", json!({"name":name,"graph_zoom":self.zoom,"editor_dimensions":dimensions}))?;
                        complete = true;
                    }
                }
                if !complete && diagnostic.step_started.elapsed() >= Duration::from_secs(5) {
                    bail!("diagnostic screenshot deadline");
                }
            }
            Step::WaitEvent { event, timeout_ms } => {
                complete = run.event_seen(&event);
                if !complete
                    && diagnostic.step_started.elapsed() >= Duration::from_millis(timeout_ms)
                {
                    bail!("event wait expired: {event}");
                }
            }
            Step::Wait { duration_ms } => {
                complete = diagnostic.step_started.elapsed() >= Duration::from_millis(duration_ms)
            }
            Step::Finish => {
                run.finish(Outcome::Passed, "explicit scenario finish")?;
            }
            Step::OpenEditor { embedded } => {
                if embedded && self.host_xid.is_none() {
                    complete = false;
                } else {
                    let placement = if embedded {
                        Placement::Embedded(self.host_xid.unwrap())
                    } else {
                        Placement::Window
                    };
                    let plugin = diagnostic_plugin(&mut self.snarl, diagnostic.node)?;
                    if !diagnostic.step_requested {
                        plugin.open_editor(placement)?;
                        diagnostic.editor_reported = false;
                        diagnostic.step_requested = true;
                        run.event("editor-requested", json!({"embedded":embedded}))?;
                    }
                    complete = plugin.editor.as_ref().is_some_and(|editor| editor.ready());
                    if !complete && diagnostic.step_started.elapsed() >= Duration::from_secs(5) {
                        bail!("editor open acknowledgement deadline");
                    }
                }
            }
            Step::CloseEditor => {
                diagnostic_plugin(&mut self.snarl, diagnostic.node)?.editor = None;
                diagnostic.editor_reported = false;
                run.event("editor-closed", json!({}))?;
            }
            Step::SetParam { id, value } => diagnostic_plugin(&mut self.snarl, diagnostic.node)?
                .instance
                .set_param(id, value),
            Step::RestartHost => {
                let plugin = diagnostic_plugin(&mut self.snarl, diagnostic.node)?;
                if !diagnostic.step_requested {
                    diagnostic.restart_saw_down =
                        plugin.instance.remote_status() != Some("running");
                    if !plugin.instance.request_remote_restart() {
                        bail!("selected plugin has no external host to restart");
                    }
                    diagnostic.step_requested = true;
                    run.event("restart-requested", json!({}))?;
                    complete = false;
                } else {
                    let running = plugin.instance.remote_status() == Some("running");
                    diagnostic.restart_saw_down |= !running;
                    complete = diagnostic.restart_saw_down && running;
                    if complete {
                        run.event("worker-restarted", json!({}))?;
                    }
                    if !complete && diagnostic.step_started.elapsed() >= Duration::from_secs(15) {
                        bail!("worker restart acknowledgement deadline");
                    }
                }
            }
            Step::AssertStatus { contains } => {
                let status = diagnostic_plugin(&mut self.snarl, diagnostic.node)?
                    .instance
                    .remote_status()
                    .unwrap_or("local");
                if !status
                    .to_ascii_lowercase()
                    .contains(&contains.to_ascii_lowercase())
                {
                    bail!("host status assertion failed");
                }
                run.event("status-asserted", json!({}))?;
            }
            Step::Fault { kind } => {
                let plugin = diagnostic_plugin(&mut self.snarl, diagnostic.node)?;
                if plugin.info().id != "org.nodeplayer.test-fixture" {
                    bail!("fault injection only supports the explicit nodeplayer test fixture");
                }
                let id = match kind.as_str() {
                    "gui-hang" => 100,
                    "dsp-hang" => 101,
                    "crash" => 102,
                    "state-hang" => 103,
                    "drop-hang" => 105,
                    _ => bail!("this fixture fault is not implemented"),
                };
                let value = if kind == "crash" { 1.0 / 3.0 } else { 1.0 };
                plugin.instance.set_param(id, value);
                run.event("fault-requested", json!({"kind":kind}))?;
            }
        }
        if complete {
            diagnostic.next += 1;
            diagnostic.step_started = Instant::now();
            diagnostic.step_requested = false;
        }
        Ok(())
    }
}

fn diagnostic_plugin(
    snarl: &mut Snarl<GraphNode>,
    node: Option<NodeId>,
) -> Result<&mut PluginNode> {
    let id = node.ok_or_else(|| anyhow!("scenario operation requires an explicit plugin"))?;
    match &mut snarl[id].kind {
        NodeKind::Plugin(plugin) => Ok(plugin),
        _ => bail!("diagnostic target is not a plugin"),
    }
}
