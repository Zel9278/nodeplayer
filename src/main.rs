mod about;
mod app;
mod audio2midi;
mod av;
mod card;
mod clap;
mod csynth_node;
mod dirs;
mod discord;
mod engine;
mod eq_editor;
mod fonts;
mod info;
mod graph;
mod jsfx;
mod library;
mod loop_editor;
mod lyrics;
mod midi;
mod midi_song;
mod midifile;
#[cfg(not(target_os = "linux"))]
mod midir_io;
mod misskey;
mod mpris;
mod native;
mod player;
mod plugins;
mod r2;
mod runtime;
mod session;
mod setup;
mod titleformat;
mod video;
mod visual;
mod vst3;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod x11_errors;
mod yabridge;

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,nodeplayer=info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--scan-plugin") => {
            let (Some(format), Some(path)) = (args.get(1), args.get(2)) else {
                return Err(anyhow!("usage: --scan-plugin <format> <path>"));
            };
            return plugins::scan_main(format, Path::new(path));
        }
        Some("--test-plugin") => {
            let (Some(format), Some(path)) = (args.get(1), args.get(2)) else {
                return Err(anyhow!("usage: --test-plugin <format> <path> [name filter]"));
            };
            return test_plugin(format, Path::new(path), args.get(3).map(String::as_str).unwrap_or(""));
        }
        Some("--test-vst3") => {
            let bundle = args.get(1).ok_or_else(|| anyhow!("usage: --test-vst3 <bundle> [name]"))?;
            return test_vst3(Path::new(bundle), args.get(2).map(String::as_str).unwrap_or(""));
        }
        Some("--test-editor") => {
            let usage = "usage: --test-editor <bundle> <name> <out.ppm>";
            let (Some(b), Some(n), Some(o)) = (args.get(1), args.get(2), args.get(3)) else {
                return Err(anyhow!(usage));
            };
            return test_editor(Path::new(b), n, Path::new(o));
        }
        _ => {}
    }

    plugins::guard::install();
    let initial_file = args.first().map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("nodeplayer")
            .with_app_id("nodeplayer")
            .with_icon(setup::icon().unwrap_or_default())
            .with_inner_size([1280.0, 800.0])
            .with_drag_and_drop(true),
        event_loop_builder: x11_event_loop_hook(),
        ..Default::default()
    };
    eframe::run_native(
        "nodeplayer",
        options,
        Box::new(move |cc| {
            fonts::install(&cc.egui_ctx);
            Ok(Box::new(app::App::new(initial_file, &cc.egui_ctx)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Plugin editors are X11 windows and can only be embedded into an X11 parent, so the app
/// runs through XWayland unless `NODEPLAYER_WAYLAND` is set (editors then get own windows).
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn x11_event_loop_hook() -> Option<eframe::EventLoopBuilderHook> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn x11_event_loop_hook() -> Option<eframe::EventLoopBuilderHook> {
    use winit::platform::x11::EventLoopBuilderExtX11;
    if std::env::var_os("NODEPLAYER_WAYLAND").is_some() || std::env::var_os("DISPLAY").is_none() {
        return None;
    }
    pick_xim_server();
    Some(Box::new(|builder| {
        builder.with_x11();
    }))
}

/// An X11 window reaches the input method (fcitx, ibus) through XIM, named by `XMODIFIERS`.
/// Wayland sessions often leave it unset, as Wayland apps don't need it; then the first server
/// the display lists is used. Runs before any thread is started.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn pick_xim_server() {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    if std::env::var_os("XMODIFIERS").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Ok((conn, screen)) = x11rb::connect(None) else { return };
    let root = conn.setup().roots[screen].root;
    let server = (|| {
        let servers = conn.intern_atom(true, b"XIM_SERVERS").ok()?.reply().ok()?.atom;
        let reply = conn.get_property(false, root, servers, AtomEnum::ATOM, 0, 64).ok()?.reply().ok()?;
        let first = reply.value32()?.next()?;
        let name = conn.get_atom_name(first).ok()?.reply().ok()?.name;
        String::from_utf8(name).ok()?.strip_prefix("@server=").map(str::to_owned)
    })();
    if let Some(server) = server {
        log::info!("XMODIFIERS is not set; using the XIM server {server}");
        // SAFETY: no other thread is running yet.
        unsafe { std::env::set_var("XMODIFIERS", format!("@im={server}")) };
    }
}

/// Headless smoke test: instantiate the classes whose name contains `filter` and run a sine
/// through them.
fn test_vst3(bundle: &Path, filter: &str) -> Result<()> {
    let module = std::sync::Arc::new(vst3::Module::load(bundle)?);
    let sr = 48_000.0;
    let n = 512;
    let input: Vec<f32> = (0..n).map(|i| (i as f32 * 1000.0 * std::f32::consts::TAU / sr as f32).sin() * 0.5).collect();
    let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();

    for info in module.classes(bundle).into_iter().filter(|c| c.name.contains(filter)) {
        println!("class: {} [{}] by {} ({})", info.name, info.id, info.vendor, info.category);
        let (mut plugin, mut proc) = vst3::Vst3Plugin::create(module.clone(), &info, sr, engine::MAX_BLOCK)?;
        println!("  latency {} samples, {} params", plugin.latency, plugin.params.len());
        for p in plugin.params.iter().take(8) {
            let v = plugin.param_value(p.id);
            println!("  param {} '{}' = {:.3} ({} {})", p.id, p.title, v, plugin.param_display(p.id, v), p.units);
        }
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        proc.process_stereo([&input, &input], [&mut l, &mut r], &[], None);
        println!("  in rms {:.4} -> out rms {:.4} / {:.4}", rms(&input), rms(&l), rms(&r));
        let param = (plugin.params.iter().find(|p| !p.read_only && p.step_count == 0))
            .or_else(|| plugin.params.iter().find(|p| !p.read_only))
            .map(|p| (p.id, p.title.clone()));
        if let Some((id, title)) = &param {
            plugin.set_param(*id, 0.25);
            proc.process_stereo([&input, &input], [&mut l, &mut r], &[], None);
            println!("  after '{title}' = 0.25: out rms {:.4} / {:.4}", rms(&l), rms(&r));
        }
        let (component, controller) = plugin.save_states();
        let len = |b: &Option<Vec<u8>>| b.as_ref().map_or(0, |b| b.len());
        println!("  state: component {} bytes, controller {} bytes", len(&component), len(&controller));
        // Round trip: change the parameter, restore, and read it back.
        if let Some((id, title)) = &param {
            plugin.set_param(*id, 0.75);
            plugin.load_states(component, controller);
            println!("  '{title}' after restoring state: {:.3} (saved 0.250)", plugin.param_value(*id));
        }
        drop(proc);
        drop(plugin);
    }
    Ok(())
}

/// Headless smoke test for any format: instantiate the classes whose name contains `filter`,
/// run a sine through them, move a parameter and round-trip the state.
fn test_plugin(format: &str, path: &Path, filter: &str) -> Result<()> {
    use engine::{Processor, StereoBuf};
    let format = plugins::Format::ALL
        .into_iter()
        .find(|f| f.label().eq_ignore_ascii_case(format))
        .ok_or_else(|| anyhow!("unknown format {format}"))?;
    let mut host = plugins::Host::default();
    let sr = 48_000.0;
    let n = 512;
    let rms = |v: &[f32]| (v[..n].iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
    for info in host.classes(format, path)?.into_iter().filter(|c| c.name.contains(filter)) {
        println!("class: {} [{}] by {} ({})", info.name, info.id, info.vendor, info.category);
        let plugins::Created { mut instance, mut processor } = host.create(&info, sr, engine::MAX_BLOCK)?;
        println!("  latency {} samples, {} params, editor {}", instance.latency(), instance.params().len(), instance.has_editor());
        let (mut ins, mut outs) = ([StereoBuf::new()], [StereoBuf::new()]);
        let mut run = |p: &mut Box<dyn Processor>| {
            for _ in 0..8 {
                for k in 0..n {
                    let v = (k as f32 * 1000.0 * std::f32::consts::TAU / sr).sin() * 0.5;
                    ins[0].l[k] = v;
                    ins[0].r[k] = v;
                }
                p.process(n, &mut ins, &mut outs);
            }
            (rms(&ins[0].l), rms(&outs[0].l), rms(&outs[0].r))
        };
        let (i, l, r) = run(&mut processor);
        println!("  in rms {i:.4} -> out rms {l:.4} / {r:.4}");
        let (midi_in, midi_out) = instance.midi_ports();
        println!("  midi in {midi_in}, out {midi_out}");
        if midi_in || midi_out {
            // Middle C for a few blocks of silence, then its note off.
            let (mut ins, mut outs) = ([StereoBuf::new()], [StereoBuf::new()]);
            let mut out_midi = engine::MidiBuf::new();
            let mut seen = Vec::new();
            let mut peak = 0f32;
            for b in 0..16 {
                let ev: Vec<engine::MidiEvent> = match b {
                    0 => vec![engine::MidiEvent::new(10, &[0x90, 60, 100]).unwrap()],
                    8 => vec![engine::MidiEvent::new(0, &[0x80, 60, 0]).unwrap()],
                    _ => vec![],
                };
                out_midi.clear();
                processor.process_midi(n, &mut ins, &mut outs, &ev, &mut out_midi);
                if b < 8 {
                    peak = peak.max(rms(&outs[0].l));
                }
                seen.extend(out_midi.events().iter().map(midi::describe));
            }
            println!("  note C4 -> audio rms {peak:.4}, {} MIDI events out", seen.len());
            for line in seen.iter().take(4) {
                println!("    {line}");
            }
        }
        // A restart as the host does it when a plugin asks: stop, reactivate, new processor.
        let mut processor = if matches!(format, plugins::Format::Vst3 | plugins::Format::Clap) {
            processor.deactivate();
            drop(processor);
            let mut p = instance.restart(sr as f64, engine::MAX_BLOCK)?;
            let (_, l, _) = run(&mut p);
            println!("  restart ok, out rms {l:.4}");
            p
        } else {
            processor
        };
        let param = instance.params().iter().find(|p| !p.read_only && !p.hidden && p.step_count == 0).cloned();
        if let Some(p) = &param {
            let v = instance.param_value(p.id);
            println!("  param '{}' = {v:.3} ({})", p.title, instance.param_display(p.id, v));
        }
        let state = instance.save_state();
        println!("  state: {} bytes", state.as_ref().map_or(0, |s| s.len()));
        if let (Some(p), Some(state)) = (&param, &state) {
            let before = instance.param_value(p.id);
            instance.set_param(p.id, 0.9);
            let _ = run(&mut processor);
            instance.idle();
            std::thread::sleep(std::time::Duration::from_millis(350));
            println!("  '{}' after set 0.9: {:.3}", p.title, instance.param_value(p.id));
            instance.load_state(state)?;
            println!("  after restoring state: {:.3} (saved {before:.3})", instance.param_value(p.id));
        }
        drop(processor);
        drop(instance);
    }
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn test_editor(_bundle: &Path, _name: &str, _out: &Path) -> Result<()> {
    Err(anyhow!("--test-editor captures X11 windows; it only works on Linux"))
}

/// Opens a plugin editor for a few seconds and dumps the window contents as a PPM image.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn test_editor(bundle: &Path, name: &str, out: &Path) -> Result<()> {
    use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

    let module = std::sync::Arc::new(vst3::Module::load(bundle)?);
    let info = module
        .classes(bundle)
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow!("no class named {name}"))?;
    let (plugin, _proc) = vst3::Vst3Plugin::create(module.clone(), &info, 48_000.0, engine::MAX_BLOCK)?;
    let ctrl = plugin.controller().ok_or_else(|| anyhow!("no controller"))?;
    use plugins::Editor as _;
    let mut editor = vst3::editor::Editor::open(ctrl, &info.name, plugins::Placement::Window, info.is_bridged())?;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        if !editor.idle() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    let (conn, _) = x11rb::connect(None)?;
    let win = editor.window_id();
    let geo = conn.get_geometry(win)?.reply()?;
    let img = conn.get_image(ImageFormat::Z_PIXMAP, win, 0, 0, geo.width, geo.height, !0)?.reply()?;
    let mut ppm = format!("P6 {} {} 255\n", geo.width, geo.height).into_bytes();
    for px in img.data.chunks_exact(4) {
        ppm.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::write(out, ppm)?;
    println!("editor {}x{} captured to {}", geo.width, geo.height, out.display());
    drop(editor);
    Ok(())
}
