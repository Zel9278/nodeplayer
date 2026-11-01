//! Graph zoom is applied to a captured native editor, never to its GUI layout.

mod platform;
mod plugin;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use egui::emath::TSTransform;
use egui::{Color32, Pos2, Rect, Sense, Vec2};
use platform::{CapturedFrame, NativeEditor, PointerEvent, PointerKind};
use plugin::PluginEditor;
use serde::Serialize;

#[derive(Default)]
struct Options {
    plugin: Option<PathBuf>,
    filter: Option<String>,
    backend_smoke: Option<PathBuf>,
    smoke: Option<PathBuf>,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut options = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--plugin" => {
                    options.plugin =
                        Some(args.next().context("--plugin requires a CLAP path")?.into())
                }
                "--id" => {
                    options.filter = Some(args.next().context("--id requires a plugin name or id")?)
                }
                "--backend-smoke" => {
                    options.backend_smoke = Some(
                        args.next()
                            .context("--backend-smoke requires an output directory")?
                            .into(),
                    )
                }
                "--smoke" => {
                    options.smoke = Some(
                        args.next()
                            .context("--smoke requires an output directory")?
                            .into(),
                    )
                }
                "--help" | "-h" => {
                    println!(
                        "nodeplayer-editor-zoom-poc [--plugin <file.clap>] [--id <name-or-id>] [--smoke <directory>] [--backend-smoke <directory>]"
                    );
                    println!(
                        "With no plugin, an actual native test window is used. Ctrl+wheel/pinch zooms the graph; drag its background to pan, or a node title to move it."
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument {arg}; use --help"),
            }
        }
        ensure!(
            options.backend_smoke.is_none() || options.smoke.is_none(),
            "choose --smoke or --backend-smoke"
        );
        Ok(options)
    }
}

/// Field order is significant: plugin views are destroyed before their native parent.
struct Source {
    plugin: Option<PluginEditor>,
    native: NativeEditor,
    name: String,
}

impl Source {
    fn open(options: &Options) -> Result<Self> {
        let mut native = NativeEditor::new(640, 360, options.plugin.is_none())?;
        let plugin = if let Some(path) = &options.plugin {
            let mut plugin = PluginEditor::load(path, options.filter.as_deref())?;
            let (width, height) = plugin.initial_size();
            native.resize(width, height)?;
            plugin.attach(native.handle())?;
            Some(plugin)
        } else {
            None
        };
        let name = plugin
            .as_ref()
            .map_or_else(|| "Native test editor".to_owned(), |p| p.name().to_owned());
        Ok(Self {
            plugin,
            native,
            name,
        })
    }

    fn pump(&mut self) -> Result<()> {
        self.native.pump()?;
        if let Some(plugin) = &mut self.plugin
            && let Some((width, height)) = plugin.idle()?
        {
            self.native.resize(width, height)?;
        }
        Ok(())
    }

    fn capture(&mut self) -> Result<CapturedFrame> {
        self.pump()?;
        let frame = self.native.capture()?;
        frame.validate()?;
        Ok(frame)
    }
}

fn save_frame(frame: &CapturedFrame, path: &Path) -> Result<()> {
    frame.validate()?;
    image::save_buffer(
        path,
        &frame.rgba,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
    )
    .with_context(|| format!("saving {}", path.display()))
}

fn save_color_image(image: &egui::ColorImage, path: &Path) -> Result<()> {
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();
    image::save_buffer(
        path,
        &bytes,
        image.width() as u32,
        image.height() as u32,
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

fn frame_hash(frame: &CapturedFrame) -> u64 {
    frame.rgba.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn check_nonuniform(frame: &CapturedFrame) -> Result<()> {
    frame.validate()?;
    let (pixels, _) = frame.rgba.as_chunks::<4>();
    let first = pixels[0];
    ensure!(
        pixels.iter().any(|pixel| *pixel != first),
        "native capture is a uniform image"
    );
    ensure!(
        pixels.iter().any(|pixel| pixel[3] != 0),
        "native capture is entirely transparent"
    );
    Ok(())
}

fn pump_for(source: &mut Source, duration: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < duration {
        source.pump()?;
        std::thread::sleep(Duration::from_millis(8));
    }
    Ok(())
}

fn capture_ready(source: &mut Source, timeout: Duration) -> Result<CapturedFrame> {
    let start = Instant::now();
    loop {
        source.pump()?;
        match source.capture().and_then(|frame| {
            check_nonuniform(&frame)?;
            Ok(frame)
        }) {
            Ok(frame) => return Ok(frame),
            Err(error) if start.elapsed() >= timeout => {
                return Err(error).context(
                    "native editor did not paint a usable first frame before the startup deadline",
                );
            }
            Err(_) => std::thread::sleep(Duration::from_millis(16)),
        }
    }
}

fn native_click(source: &mut Source, point: [f64; 2]) -> Result<()> {
    for (kind, buttons) in [
        (PointerKind::Move, 0),
        (PointerKind::Down(1), 1),
        (PointerKind::Up(1), 0),
    ] {
        source.native.pointer(PointerEvent {
            position: point,
            kind,
            buttons,
        })?;
        source.pump()?;
    }
    Ok(())
}

fn backend_smoke(options: &Options, directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let report_path = directory.join("backend-report.json");
    if report_path.exists() {
        std::fs::remove_file(&report_path)?;
    }
    let mut source = Source::open(options)?;
    let before = capture_ready(&mut source, Duration::from_secs(10))?;
    save_frame(&before, &directory.join("native-before.png"))?;
    let previous = source.native.fixture_value();
    if previous.is_some() {
        native_click(&mut source, [80.0, 90.0])?;
        pump_for(&mut source, Duration::from_millis(150))?;
    }
    let after = source.capture()?;
    let current = source.native.fixture_value();
    if let Some(previous) = previous {
        ensure!(
            current == Some(previous + 1),
            "native fixture did not receive the forwarded click"
        );
        ensure!(
            frame_hash(&before) != frame_hash(&after),
            "native click did not change captured pixels"
        );
    }
    save_frame(&after, &directory.join("native-after.png"))?;
    let report = serde_json::json!({
        "success":true, "os":std::env::consts::OS, "source":source.name,
        "real_plugin":source.plugin.is_some(), "native_size":source.native.size(),
        "capture_size":[after.width,after.height], "nonuniform_capture":true,
        "fixture_click_before":previous, "fixture_click_after":current,
        "capture_before_hash":format!("{:016x}",frame_hash(&before)),
        "capture_after_hash":format!("{:016x}",frame_hash(&after)),
        "plugin_scale_requests":0,
    });
    // A capture result is not a successful smoke test until native/plugin teardown finishes.
    drop(source);
    std::fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Convert through the same transform used to paint the editor image.
fn native_from_screen(
    transform: TSTransform,
    image_rect: Rect,
    native_size: (u32, u32),
    screen: Pos2,
) -> [f64; 2] {
    let point = transform.inverse() * screen;
    [
        f64::from((point.x - image_rect.min.x) / image_rect.width()) * f64::from(native_size.0),
        f64::from((point.y - image_rect.min.y) / image_rect.height()) * f64::from(native_size.1),
    ]
}

fn zoom_about(transform: &mut TSTransform, zoom: f32, anchor: Pos2) {
    let local = transform.inverse() * anchor;
    transform.scaling = zoom.clamp(0.2, 3.0);
    transform.translation = anchor.to_vec2() - transform.scaling * local.to_vec2();
}

#[derive(Serialize)]
struct Shot {
    zoom: f32,
    file: String,
    editor_screen_rect: [[f32; 2]; 2],
    native_size: (u32, u32),
    capture_size: [u32; 2],
    inverse_mapping_error: f64,
    fixture_click_value: Option<u64>,
}

struct Smoke {
    directory: PathBuf,
    stage: usize,
    phase_frames: u32,
    awaiting: bool,
    click_done: bool,
    shots: Vec<Shot>,
    started: Instant,
}

impl Smoke {
    const ZOOMS: [f32; 3] = [0.5, 1.0, 1.5];

    fn new(directory: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        let previous = directory.join("graph-report.json");
        if previous.exists() {
            std::fs::remove_file(previous)?;
        }
        Ok(Self {
            directory,
            stage: 0,
            phase_frames: 0,
            awaiting: false,
            click_done: false,
            shots: Vec::new(),
            started: Instant::now(),
        })
    }
}

struct ZoomApp {
    source: Source,
    texture: Option<egui::TextureHandle>,
    capture_size: [u32; 2],
    next_capture: Instant,
    capture_count: u64,
    last_error: Option<String>,
    transform: TSTransform,
    initialized: bool,
    node_position: Pos2,
    image_rect: Rect,
    image_units: Vec2,
    native_size: (u32, u32),
    native_units_per_point: f32,
    pointer_buttons: u8,
    pointer_owned: bool,
    smoke: Option<Smoke>,
}

impl ZoomApp {
    fn new(options: &Options, context: &egui::Context) -> Result<Self> {
        let source = Source::open(options)?;
        let (width, height) = source.native.size();
        let units = if cfg!(target_os = "macos") {
            1.0
        } else {
            context.native_pixels_per_point().unwrap_or(1.0)
        };
        Ok(Self {
            source,
            texture: None,
            capture_size: [0, 0],
            next_capture: Instant::now(),
            capture_count: 0,
            last_error: None,
            transform: TSTransform::IDENTITY,
            initialized: false,
            node_position: egui::pos2(30.0, 30.0),
            image_rect: Rect::NOTHING,
            image_units: egui::vec2(width as f32 / units, height as f32 / units),
            native_size: (width, height),
            native_units_per_point: units,
            pointer_buttons: 0,
            pointer_owned: false,
            smoke: options.smoke.clone().map(Smoke::new).transpose()?,
        })
    }

    fn refresh(&mut self, context: &egui::Context) -> Result<()> {
        self.source.pump()?;
        let size = self.source.native.size();
        if size != self.native_size {
            self.native_size = size;
            self.image_units = egui::vec2(
                size.0 as f32 / self.native_units_per_point,
                size.1 as f32 / self.native_units_per_point,
            );
        }
        if Instant::now() < self.next_capture {
            return Ok(());
        }
        self.next_capture = Instant::now() + Duration::from_millis(33);
        let frame = self.source.capture()?;
        // Some editors paint asynchronously after GUI show. Keep retrying their first frame;
        // a later uniform frame can be legitimate content and should still be displayed.
        if self.texture.is_none() {
            check_nonuniform(&frame)?;
        }
        self.capture_size = [frame.width, frame.height];
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture =
                Some(context.load_texture("native-editor", image, egui::TextureOptions::LINEAR));
        }
        self.capture_count += 1;
        if self
            .last_error
            .as_ref()
            .is_some_and(|error| error.starts_with("Capture: "))
        {
            self.last_error = None;
        }
        Ok(())
    }

    fn graph(&mut self, ui: &mut egui::Ui) {
        let (canvas, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        ui.painter()
            .rect_filled(canvas, 0.0, Color32::from_rgb(23, 26, 32));
        let layer = egui::LayerId::new(ui.layer_id().order, ui.id().with("zoom-graph"));
        ui.ctx().set_sublayer(ui.layer_id(), layer);
        if !self.initialized {
            self.transform.translation = canvas.min.to_vec2() + egui::vec2(25.0, 20.0);
            self.initialized = true;
        }
        if let Some(smoke) = &self.smoke
            && smoke.stage < Smoke::ZOOMS.len()
        {
            self.transform.scaling = Smoke::ZOOMS[smoke.stage];
            self.transform.translation = canvas.min.to_vec2() + egui::vec2(28.0, 20.0);
        }
        let mut scene_ui = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer)
                .max_rect(Rect::from_min_size(Pos2::ZERO, Vec2::splat(10000.0)))
                .sense(Sense::drag()),
        );
        // Fractional local coordinates are intentional while zooming a graph.
        #[cfg(debug_assertions)]
        {
            scene_ui.style_mut().debug.show_unaligned = false;
        }
        let mut response = scene_ui.response();
        let editor_wheel = scene_ui.input(|input| {
            input.modifiers.alt
                && input.pointer.latest_pos().is_some_and(|position| {
                    canvas.contains(position)
                        && self
                            .image_rect
                            .contains(self.transform.inverse() * position)
                })
        });
        if self.smoke.is_none() && !editor_wheel {
            egui::Scene::new()
                .zoom_range(0.2..=3.0)
                .register_pan_and_zoom(&scene_ui, &mut response, &mut self.transform);
        }
        scene_ui.set_clip_rect(self.transform.inverse() * canvas);
        scene_ui.ctx().set_transform_layer(layer, self.transform);
        let viewport = self.transform.inverse() * canvas;
        for x in ((viewport.min.x / 40.0).floor() as i32)..=((viewport.max.x / 40.0).ceil() as i32)
        {
            let x = x as f32 * 40.0;
            scene_ui.painter().line_segment(
                [egui::pos2(x, viewport.min.y), egui::pos2(x, viewport.max.y)],
                egui::Stroke::new(0.5, Color32::from_gray(42)),
            );
        }
        for y in ((viewport.min.y / 40.0).floor() as i32)..=((viewport.max.y / 40.0).ceil() as i32)
        {
            let y = y as f32 * 40.0;
            scene_ui.painter().line_segment(
                [egui::pos2(viewport.min.x, y), egui::pos2(viewport.max.x, y)],
                egui::Stroke::new(0.5, Color32::from_gray(42)),
            );
        }
        let mut node_ui = scene_ui.new_child(
            egui::UiBuilder::new()
                .max_rect(Rect::from_min_size(
                    self.node_position,
                    self.image_units + egui::vec2(24.0, 80.0),
                ))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        egui::Frame::new()
            .fill(Color32::from_rgb(43, 49, 60))
            .stroke(egui::Stroke::new(1.0, Color32::from_gray(95)))
            .corner_radius(8)
            .inner_margin(12)
            .show(&mut node_ui, |ui| {
                ui.set_width(self.image_units.x);
                let title = ui.add(
                    egui::Label::new(egui::RichText::new(&self.source.name).strong())
                        .sense(Sense::drag()),
                );
                if title.dragged() {
                    self.node_position += title.drag_delta();
                }
                ui.add_space(6.0);
                if let Some(texture) = &self.texture {
                    let image = ui.add(
                        egui::Image::new((texture.id(), self.image_units))
                            .fit_to_exact_size(self.image_units)
                            .sense(Sense::click_and_drag()),
                    );
                    self.image_rect = image.rect;
                    self.forward_pointer(ui.ctx(), canvas, image.rect, image.contains_pointer());
                } else {
                    let (rect, _) = ui.allocate_exact_size(self.image_units, Sense::hover());
                    self.image_rect = rect;
                    ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
                }
                ui.add_space(3.0);
                ui.small("Native editor image");
            });
        // An overlapping graph node deliberately uses the same paint layer as the editor image.
        let overlap = Rect::from_min_size(
            self.node_position + egui::vec2(self.image_units.x - 90.0, self.image_units.y - 18.0),
            egui::vec2(170.0, 82.0),
        );
        scene_ui
            .painter()
            .rect_filled(overlap, 6.0, Color32::from_rgb(58, 87, 88));
        scene_ui.painter().text(
            overlap.min + egui::vec2(12.0, 12.0),
            egui::Align2::LEFT_TOP,
            "Output",
            egui::FontId::proportional(16.0),
            Color32::WHITE,
        );
        scene_ui.painter().text(
            overlap.min + egui::vec2(12.0, 42.0),
            egui::Align2::LEFT_TOP,
            "Overlapping node",
            egui::FontId::proportional(12.0),
            Color32::LIGHT_GRAY,
        );
        scene_ui.expand_to_include_rect(viewport);
    }

    fn forward_pointer(
        &mut self,
        context: &egui::Context,
        canvas: Rect,
        image_rect: Rect,
        image_contains_pointer: bool,
    ) {
        let events = context.input(|input| input.events.clone());
        let overlap = Rect::from_min_size(
            self.node_position + egui::vec2(self.image_units.x - 90.0, self.image_units.y - 18.0),
            egui::vec2(170.0, 82.0),
        );
        for event in events {
            let (position, kind) = match event {
                egui::Event::PointerMoved(position) => (position, PointerKind::Move),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let button = match button {
                        egui::PointerButton::Primary => 1,
                        egui::PointerButton::Secondary => 2,
                        egui::PointerButton::Middle => 3,
                        _ => continue,
                    };
                    (
                        pos,
                        if pressed {
                            PointerKind::Down(button)
                        } else {
                            PointerKind::Up(button)
                        },
                    )
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if modifiers.alt => {
                    let Some(position) = context.pointer_latest_pos() else {
                        continue;
                    };
                    let multiplier = match unit {
                        egui::MouseWheelUnit::Point => 1.0,
                        egui::MouseWheelUnit::Line => 20.0,
                        egui::MouseWheelUnit::Page => self.image_units.y,
                    };
                    (
                        position,
                        PointerKind::Wheel {
                            dx: delta.x * multiplier,
                            dy: delta.y * multiplier,
                        },
                    )
                }
                _ => continue,
            };
            let local = self.transform.inverse() * position;
            let hovered = canvas.contains(position)
                && image_rect.contains(local)
                && !overlap.contains(local)
                && image_contains_pointer;
            match kind {
                PointerKind::Down(button) if hovered => {
                    self.pointer_owned = true;
                    self.pointer_buttons |= 1 << (button - 1);
                }
                PointerKind::Down(_) => continue,
                PointerKind::Up(button) if self.pointer_owned => {
                    self.pointer_buttons &= !(1 << (button - 1));
                }
                PointerKind::Up(_) => continue,
                PointerKind::Move if !hovered && !self.pointer_owned => continue,
                PointerKind::Wheel { .. } if !hovered => continue,
                _ => {}
            }
            let native = native_from_screen(
                self.transform,
                image_rect,
                self.source.native.size(),
                position,
            );
            if let Err(error) = self.source.native.pointer(PointerEvent {
                position: native,
                kind,
                buttons: self.pointer_buttons,
            }) {
                self.last_error = Some(format!("Pointer: {error:#}"));
            }
            if matches!(kind, PointerKind::Up(_)) && self.pointer_buttons == 0 {
                self.pointer_owned = false;
            }
        }
    }

    fn smoke_tick(&mut self, context: &egui::Context) -> Result<()> {
        let Some(smoke) = &mut self.smoke else {
            return Ok(());
        };
        ensure!(
            smoke.started.elapsed() < Duration::from_secs(60),
            "GUI smoke timed out"
        );
        for event in context.input(|input| input.events.clone()) {
            if let egui::Event::Screenshot {
                user_data, image, ..
            } = event
                && let Some(stage) = user_data
                    .data
                    .as_ref()
                    .and_then(|data| data.downcast_ref::<usize>())
                && *stage == smoke.stage
                && smoke.awaiting
            {
                let file = format!(
                    "graph-{:03}.png",
                    (Smoke::ZOOMS[smoke.stage] * 100.0) as u32
                );
                save_color_image(&image, &smoke.directory.join(&file))?;
                let native_size = self.source.native.size();
                let target = [80.0_f64, 90.0_f64];
                let graph_point = self.image_rect.min
                    + egui::vec2(
                        target[0] as f32 / native_size.0 as f32 * self.image_rect.width(),
                        target[1] as f32 / native_size.1 as f32 * self.image_rect.height(),
                    );
                let mapped = native_from_screen(
                    self.transform,
                    self.image_rect,
                    native_size,
                    self.transform * graph_point,
                );
                let error = (mapped[0] - target[0])
                    .abs()
                    .max((mapped[1] - target[1]).abs());
                ensure!(error < 0.001, "inverse pointer mapping error {error}");
                let screen = self.transform * self.image_rect;
                ensure!(
                    (screen.width() - self.image_units.x * self.transform.scaling).abs() < 0.01,
                    "editor image did not follow graph zoom"
                );
                smoke.shots.push(Shot {
                    zoom: self.transform.scaling,
                    file,
                    editor_screen_rect: [
                        [screen.min.x, screen.min.y],
                        [screen.max.x, screen.max.y],
                    ],
                    native_size,
                    capture_size: self.capture_size,
                    inverse_mapping_error: error,
                    fixture_click_value: self.source.native.fixture_value(),
                });
                smoke.stage += 1;
                smoke.phase_frames = 0;
                smoke.awaiting = false;
                smoke.click_done = false;
            }
        }
        if smoke.stage == Smoke::ZOOMS.len() {
            let report = serde_json::json!({"success":false,"rendering_success":true,"clean_app_teardown":false,"os":std::env::consts::OS,"source":self.source.name,"real_plugin":self.source.plugin.is_some(),"capture_frames":self.capture_count,"plugin_scale_requests":0,"shots":smoke.shots});
            std::fs::write(
                smoke.directory.join("graph-report.json"),
                serde_json::to_vec_pretty(&report)?,
            )?;
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return Ok(());
        }
        smoke.phase_frames += 1;
        if smoke.phase_frames >= 12 && !smoke.click_done && self.texture.is_some() {
            if let Some(before) = self.source.native.fixture_value() {
                let size = self.source.native.size();
                let graph_point = self.image_rect.min
                    + egui::vec2(
                        80.0 / size.0 as f32 * self.image_rect.width(),
                        90.0 / size.1 as f32 * self.image_rect.height(),
                    );
                let point = native_from_screen(
                    self.transform,
                    self.image_rect,
                    size,
                    self.transform * graph_point,
                );
                native_click(&mut self.source, point)?;
                ensure!(
                    self.source.native.fixture_value() == Some(before + 1),
                    "fixture click at graph zoom {} failed",
                    self.transform.scaling
                );
                self.next_capture = Instant::now();
            }
            smoke.click_done = true;
        }
        if smoke.phase_frames >= 24 && smoke.click_done && !smoke.awaiting && self.capture_count > 2
        {
            context.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                smoke.stage,
            )));
            smoke.awaiting = true;
        }
        Ok(())
    }
}

impl eframe::App for ZoomApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        if let Err(error) = self.refresh(&context) {
            self.last_error = Some(format!("Capture: {error:#}"));
        }
        egui::Panel::top("controls").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Editor zoom PoC");
                for (label, zoom) in [("50%", 0.5), ("100%", 1.0), ("150%", 1.5)] {
                    if ui.button(label).clicked() {
                        let anchor = context.content_rect().center();
                        zoom_about(&mut self.transform, zoom, anchor);
                    }
                }
                ui.label(format!("{:.0}%", self.transform.scaling * 100.0));
                ui.separator();
                ui.label("Ctrl+wheel / pinch · drag to pan · Alt+wheel for editor");
            });
            if let Some(error) = &self.last_error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.graph(ui));
        if let Err(error) = self.smoke_tick(&context) {
            eprintln!("GUI smoke: {error:#}");
            if let Some(smoke) = &self.smoke {
                let _=std::fs::write(smoke.directory.join("graph-report.json"),serde_json::json!({"success":false,"error":format!("{error:#}"),"os":std::env::consts::OS}).to_string());
            }
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        context.request_repaint_after(Duration::from_millis(16));
    }
}

fn main() -> Result<()> {
    let options = Options::parse()?;
    if let Some(directory) = &options.backend_smoke {
        return backend_smoke(&options, directory);
    }
    let smoke_directory = options.smoke.clone();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("nodeplayer — editor zoom PoC")
            .with_inner_size([1160.0, 820.0])
            .with_position([720.0, 60.0]),
        event_loop_builder: x11_hook(),
        ..Default::default()
    };
    eframe::run_native(
        "nodeplayer editor zoom PoC",
        native_options,
        Box::new(move |creation| {
            creation
                .egui_ctx
                .options_mut(|options| options.zoom_with_keyboard = false);
            Ok(Box::new(ZoomApp::new(&options, &creation.egui_ctx)?))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(directory) = smoke_directory {
        let mut report: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("graph-report.json"))
                .context("GUI smoke did not produce a report")?,
        )?;
        ensure!(
            report["rendering_success"] == true,
            "GUI smoke failed: {}",
            report["error"]
        );
        report["success"] = serde_json::Value::Bool(true);
        report["clean_app_teardown"] = serde_json::Value::Bool(true);
        std::fs::write(
            directory.join("graph-report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn x11_hook() -> Option<eframe::EventLoopBuilderHook> {
    use winit::platform::x11::EventLoopBuilderExtX11;
    Some(Box::new(|builder| {
        builder.with_x11();
    }))
}

#[cfg(not(target_os = "linux"))]
fn x11_hook() -> Option<eframe::EventLoopBuilderHook> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_mapping_survives_zoom_pan_and_retina_capture() {
        let image = Rect::from_min_size(egui::pos2(30.0, 75.0), egui::vec2(640.0, 360.0));
        for native_size in [(640, 360), (1280, 720)] {
            for zoom in [0.2, 0.5, 1.0, 1.5, 3.0] {
                let transform = TSTransform {
                    scaling: zoom,
                    translation: egui::vec2(-123.5, 241.0),
                };
                for point in [
                    egui::pos2(0.0, 0.0),
                    egui::pos2(80.0, 90.0),
                    egui::pos2(native_size.0 as f32, native_size.1 as f32),
                ] {
                    let graph = image.min
                        + egui::vec2(
                            point.x / native_size.0 as f32 * image.width(),
                            point.y / native_size.1 as f32 * image.height(),
                        );
                    let screen = transform * graph;
                    let native = native_from_screen(transform, image, native_size, screen);
                    assert!((native[0] - f64::from(point.x)).abs() < 0.001);
                    assert!((native[1] - f64::from(point.y)).abs() < 0.001);
                }
            }
        }
    }

    #[test]
    fn zoom_preserves_pointer_anchor() {
        let mut transform = TSTransform {
            scaling: 0.6,
            translation: egui::vec2(21.0, -67.0),
        };
        let anchor = egui::pos2(430.0, 270.0);
        let before = transform.inverse() * anchor;
        zoom_about(&mut transform, 1.7, anchor);
        let after = transform.inverse() * anchor;
        assert!(before.distance(after) < 0.001);
    }
}
