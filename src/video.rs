//! Video panel: shows the picture of the file the player is playing, kept in step with the
//! player's position. Decoding runs on its own thread (FFmpeg, [`crate::av`]); the panel
//! only uploads the frame that's due. The picture can also go into a window of its own.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use egui::{Color32, Rect, Sense, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::av::{Picture, VideoDecoder, VideoInfo};
use crate::player::Player;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// Pictures are scaled down to this height; 0 keeps the file's size.
    pub max_height: u32,
    /// Shows pictures this much later (positive) or earlier than the audio.
    pub offset_ms: f32,
    /// The picture is also shown in a window of its own.
    pub window: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self { max_height: 720, offset_ms: 0.0, window: false }
    }
}

const HEIGHTS: [(u32, &str); 5] = [(360, "360p"), (480, "480p"), (720, "720p"), (1080, "1080p"), (0, "original")];

/// Pictures decoded ahead of the one on screen.
const AHEAD: usize = 4;

enum Cmd {
    Open { generation: u64, path: PathBuf, at: f64, max_height: u32 },
    Seek { generation: u64, at: f64 },
    Close,
}

enum Msg {
    Opened(Option<VideoInfo>),
    Picture(Picture),
    End,
    Failed(String),
}

#[derive(Clone, PartialEq)]
enum State {
    Idle,
    Opening,
    NoVideo,
    Playing(VideoInfo),
    Failed(String),
}

pub struct VideoPanel {
    pub settings: VideoSettings,
    cmd: Sender<Cmd>,
    msgs: Receiver<(u64, Msg)>,
    /// Bumped on every open and seek; older messages are dropped.
    generation: u64,
    track: Option<PathBuf>,
    state: State,
    ahead: VecDeque<Picture>,
    /// Time of the newest picture received for this generation.
    newest: Option<f64>,
    ended: bool,
    texture: Option<egui::TextureHandle>,
    /// Player position at the last update and when that was, to notice seeks.
    last: Option<(f64, Instant)>,
    asked_at: Instant,
    /// The pass the panel was last updated in (the panel and the window both update).
    pass: Option<u64>,
}

impl VideoPanel {
    pub fn new(settings: VideoSettings) -> Self {
        let (cmd, cmd_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msgs) = crossbeam_channel::bounded(AHEAD);
        std::thread::Builder::new()
            .name("video".into())
            .spawn(move || decode_thread(cmd_rx, msg_tx))
            .expect("spawn video thread");
        Self {
            settings,
            cmd,
            msgs,
            generation: 0,
            track: None,
            state: State::Idle,
            ahead: VecDeque::new(),
            newest: None,
            ended: false,
            texture: None,
            last: None,
            asked_at: Instant::now(),
            pass: None,
        }
    }

    fn open(&mut self, at: f64) {
        self.generation += 1;
        self.ahead.clear();
        self.newest = None;
        self.ended = false;
        self.asked_at = Instant::now();
        match &self.track {
            Some(path) => {
                self.state = State::Opening;
                let _ = self.cmd.send(Cmd::Open { generation: self.generation, path: path.clone(), at, max_height: self.settings.max_height });
            }
            None => {
                self.state = State::Idle;
                self.texture = None;
                let _ = self.cmd.send(Cmd::Close);
            }
        }
    }

    fn seek(&mut self, at: f64) {
        if !matches!(self.state, State::Playing(_)) {
            return self.open(at);
        }
        self.generation += 1;
        self.ahead.clear();
        self.newest = None;
        self.ended = false;
        self.asked_at = Instant::now();
        let _ = self.cmd.send(Cmd::Seek { generation: self.generation, at });
    }

    /// Follows the player: opens what it plays, notices seeks, and puts the picture that's
    /// due on the texture. Does nothing the second time in a pass.
    fn update(&mut self, ctx: &egui::Context, player: &Player) {
        let pass = ctx.cumulative_pass_nr();
        if self.pass.replace(pass) == Some(pass) {
            return;
        }
        let playing = player.is_playing();
        let pos = player.position() - self.settings.offset_ms as f64 / 1000.0;
        let now = Instant::now();
        let path = player.status().path;
        if path != self.track {
            self.track = path;
            self.texture = None;
            // The player's position may still be the old track's.
            self.open(0.0);
        } else if let Some((p, at)) = self.last {
            let rate = player.rate() as f64;
            let expected = p + if playing { now.duration_since(at).as_secs_f64() * rate } else { 0.0 };
            let jumped = (pos - expected).abs() > 0.5;
            // Decoding couldn't keep up (or the open took long): skip ahead to the player.
            let behind = playing
                && !self.ended
                && matches!(self.state, State::Playing(_))
                && self.ahead.is_empty()
                && self.newest.is_some_and(|t| t < pos - 1.0)
                && self.asked_at.elapsed() > Duration::from_secs(1);
            if jumped || behind {
                self.seek(pos.max(0.0));
            }
        }
        self.last = Some((pos, now));

        while self.ahead.len() < AHEAD {
            let Ok((generation, msg)) = self.msgs.try_recv() else { break };
            if generation != self.generation {
                continue;
            }
            match msg {
                Msg::Opened(Some(info)) => self.state = State::Playing(info),
                Msg::Opened(None) => {
                    self.state = State::NoVideo;
                    self.texture = None;
                }
                Msg::Picture(p) => {
                    self.newest = Some(p.time);
                    self.ahead.push_back(p);
                }
                Msg::End => self.ended = true,
                Msg::Failed(e) => self.state = State::Failed(e),
            }
        }

        let fps = match &self.state {
            State::Playing(info) => info.fps,
            _ => 30.0,
        };
        let mut due = None;
        while self.ahead.front().is_some_and(|p| p.time <= pos + 0.5 / fps) {
            due = self.ahead.pop_front();
        }
        if let Some(p) = due {
            log::trace!("picture {:.3} at {pos:.3}", p.time);
            match &mut self.texture {
                Some(t) => t.set(p.image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("video", p.image, egui::TextureOptions::LINEAR)),
            }
        }

        let waiting = self.state == State::Opening || (self.newest.is_none() && matches!(self.state, State::Playing(_)) && !self.ended);
        if waiting {
            ctx.request_repaint_after(Duration::from_millis(10));
        } else if playing && let Some(next) = self.ahead.front() {
            let wait = (next.time - pos - 0.5 / fps).clamp(0.0, 0.1);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        } else if playing && !self.ended && matches!(self.state, State::Playing(_)) {
            ctx.request_repaint_after(Duration::from_millis(5));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, player: &Player) {
        self.update(ui.ctx(), player);
        ui.horizontal(|ui| {
            match &self.state {
                State::Idle => ui.small("nothing playing"),
                State::Opening => ui.small("opening…"),
                State::NoVideo => ui.small("no video in this file"),
                State::Playing(i) => ui.small(format!("{}×{} · {:.3} fps", i.width, i.height, i.fps).replace(".000 fps", " fps")),
                State::Failed(e) => ui.colored_label(Color32::from_rgb(230, 120, 90), e),
            };
        });
        ui.horizontal(|ui| {
            let label = HEIGHTS.iter().find(|(h, _)| *h == self.settings.max_height).map_or("custom", |(_, l)| l);
            egui::ComboBox::from_id_salt(ui.id().with("height"))
                .selected_text(label)
                .width(80.0)
                .show_ui(ui, |ui| {
                    for (h, l) in HEIGHTS {
                        if ui.selectable_value(&mut self.settings.max_height, h, l).changed() {
                            let at = player.position();
                            self.open(at);
                        }
                    }
                })
                .response
                .on_hover_text("decode size; smaller is lighter on the CPU");
            ui.add(egui::DragValue::new(&mut self.settings.offset_ms).speed(1.0).range(-2000.0..=2000.0).fixed_decimals(0).prefix("offset ").suffix(" ms"))
                .on_hover_text("positive shows the picture later");
            ui.checkbox(&mut self.settings.window, "window").on_hover_text("also show it in a window of its own (double-click the picture)");
        });

        let size = ui.available_size().max(vec2(80.0, 45.0));
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        self.paint(ui.painter(), rect);
        if resp.double_clicked() {
            self.settings.window = !self.settings.window;
        }
    }

    /// Black box with the picture fitted in, upright.
    fn paint(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        let Some(tex) = &self.texture else {
            if self.state == State::NoVideo {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "no video", egui::FontId::proportional(14.0), Color32::from_gray(120));
            }
            return;
        };
        let (aspect, rotation) = match &self.state {
            State::Playing(info) => (info.aspect(), info.rotation),
            _ => {
                let [tw, th] = tex.size();
                (tw as f32 / th.max(1) as f32, 0)
            }
        };
        let size = if rect.width() / rect.height() > aspect {
            vec2(rect.height() * aspect, rect.height())
        } else {
            vec2(rect.width(), rect.width() / aspect)
        };
        let r = Rect::from_center_size(rect.center(), size);
        // Corners clockwise from the top left; turning the picture moves the texture corners.
        let uv = [pos2(0.0, 0.0), pos2(1.0, 0.0), pos2(1.0, 1.0), pos2(0.0, 1.0)];
        let corners = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()];
        let k = (rotation / 90) as usize;
        let mut mesh = egui::Mesh::with_texture(tex.id());
        for (i, c) in corners.into_iter().enumerate() {
            mesh.vertices.push(egui::epaint::Vertex { pos: c, uv: uv[(i + 4 - k) % 4], color: Color32::WHITE });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        painter.with_clip_rect(rect).add(mesh);
    }

    /// The picture in a window of its own, while that's turned on. Call every pass, also
    /// when the panel isn't shown. Double-click or F11 for full screen, Space to pause,
    /// arrows to seek.
    pub fn window(&mut self, ctx: &egui::Context, player: &Player) {
        if !self.settings.window {
            return;
        }
        self.update(ctx, player);
        let title = player.status().title().map_or_else(|| "Video".to_string(), |t| format!("{t} — Video"));
        let id = egui::ViewportId::from_hash_of("video");
        let builder = egui::ViewportBuilder::default().with_title(title).with_app_id("nodeplayer").with_inner_size([960.0, 540.0]);
        ctx.show_viewport_immediate(id, builder, |ui, _| {
            let ctx = ui.ctx().clone();
            if ctx.input(|i| i.viewport().close_requested()) {
                self.settings.window = false;
                return;
            }
            let rect = ui.max_rect();
            let resp = ui.interact(rect, ui.id().with("video-window"), Sense::click());
            self.paint(ui.painter(), rect);
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            let (toggle_fs, leave_fs, pause, back, forward) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::F11) || i.key_pressed(egui::Key::F),
                    i.key_pressed(egui::Key::Escape),
                    i.key_pressed(egui::Key::Space),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                )
            });
            if resp.double_clicked() || toggle_fs {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
            } else if leave_fs && fullscreen {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            if pause && player.status().path.is_some() {
                player.set_playing(!player.is_playing());
            }
            if back || forward {
                let step = if forward { 5.0 } else { -5.0 };
                let end = player.status().duration.unwrap_or(f64::MAX);
                player.seek((player.position() + step).clamp(0.0, end));
            }
        });
    }
}

impl Drop for VideoPanel {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Close);
    }
}

fn decode_thread(cmds: Receiver<Cmd>, out: Sender<(u64, Msg)>) {
    let mut decoder: Option<VideoDecoder> = None;
    let mut generation = 0;
    let mut max_height = 720;
    // Set when the decoder ran out; then the thread waits for a command.
    let mut idle = true;
    loop {
        let cmd = if idle {
            match cmds.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            match cmds.try_recv() {
                Ok(c) => Some(c),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
                Err(_) => return,
            }
        };
        let send = |generation: u64, msg: Msg| out.send((generation, msg)).is_ok();
        match cmd {
            Some(Cmd::Open { generation: g, path, at, max_height: h }) => {
                generation = g;
                max_height = if h == 0 { u32::MAX } else { h };
                decoder = None;
                idle = true;
                let opened = VideoDecoder::open(&path).and_then(|d| {
                    let Some(mut d) = d else { return Ok(None) };
                    if at > 0.0 {
                        d.seek(at)?;
                    }
                    Ok(Some(d))
                });
                match opened {
                    Ok(d) => {
                        if !send(generation, Msg::Opened(d.as_ref().map(|d| d.info))) {
                            return;
                        }
                        idle = d.is_none();
                        decoder = d;
                    }
                    Err(e) => {
                        log::warn!("video: {}: {e}", path.display());
                        if !send(generation, Msg::Failed(format!("can't open: {e}"))) {
                            return;
                        }
                    }
                }
                continue;
            }
            Some(Cmd::Seek { generation: g, at }) => {
                generation = g;
                if let Some(d) = &mut decoder {
                    match d.seek(at) {
                        Ok(()) => idle = false,
                        Err(e) => log::warn!("video {e}"),
                    }
                }
                continue;
            }
            Some(Cmd::Close) => {
                decoder = None;
                idle = true;
                continue;
            }
            None => {}
        }
        let Some(d) = &mut decoder else {
            idle = true;
            continue;
        };
        match d.next(max_height) {
            Ok(Some(p)) => {
                // Wait for room, but go back to the commands as soon as there are any (a
                // seek makes this picture useless).
                let mut msg = (generation, Msg::Picture(p));
                loop {
                    match out.try_send(msg) {
                        Ok(()) => break,
                        Err(TrySendError::Disconnected(_)) => return,
                        Err(TrySendError::Full(m)) => {
                            if !cmds.is_empty() {
                                break;
                            }
                            msg = m;
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
            }
            Ok(None) => {
                idle = true;
                if !send(generation, Msg::End) {
                    return;
                }
            }
            Err(e) => {
                idle = true;
                decoder = None;
                if !send(generation, Msg::Failed(e.to_string())) {
                    return;
                }
            }
        }
    }
}
