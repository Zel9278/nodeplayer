//! Discord Rich Presence: shows what's playing as a "Listening to" activity, with a progress
//! bar while it plays, through the Discord app's local IPC socket.
//!
//! Discord needs an application to show the activity under: one made in the Developer Portal,
//! whose name is what "Listening to …" says (unless the member list is set to show the song).
//! The texts are foobar2000 title formatting, like the Misskey note. Discord only shows images
//! it can download, so the track's own cover is uploaded to a Cloudflare R2 bucket ([`r2`]),
//! or looked up on the iTunes Search API.

use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use egui::Color32;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::misskey::Fields;
use crate::player::Status;
use crate::r2;

pub const DEFAULT_DETAILS: &str = "%title%";
pub const DEFAULT_STATE: &str = "%artist%";
pub const DEFAULT_LARGE_TEXT: &str = "%album%";

/// What Discord's member list shows after "Listening to".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Display {
    /// The application's name.
    Name,
    /// The second line (the artist, by default).
    State,
    /// The first line (the title, by default).
    #[default]
    Details,
}

impl Display {
    const ALL: [Self; 3] = [Self::Details, Self::State, Self::Name];

    fn label(self) -> &'static str {
        match self {
            Self::Name => "the application's name",
            Self::State => "the second line",
            Self::Details => "the first line",
        }
    }

    fn api(self) -> u8 {
        match self {
            Self::Name => 0,
            Self::State => 1,
            Self::Details => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    /// The Discord application's ID (Developer Portal → the application → General Information).
    pub client_id: String,
    pub details: String,
    pub state: String,
    /// Shown when hovering the cover.
    pub large_text: String,
    pub display: Display,
    /// Keep showing the track while paused (without the bar); otherwise the activity goes away.
    pub show_paused: bool,
    /// Look the cover up on the iTunes Search API by artist and album (when there's no own
    /// cover to show).
    pub cover_lookup: bool,
    /// Upload the track's own cover to R2.
    pub cover_upload: bool,
    pub r2: r2::Config,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            details: DEFAULT_DETAILS.into(),
            state: DEFAULT_STATE.into(),
            large_text: DEFAULT_LARGE_TEXT.into(),
            display: Display::default(),
            show_paused: true,
            cover_lookup: false,
            cover_upload: false,
            r2: r2::Config::default(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(crate::session::config_dir()?.join("discord.json"))
}

impl Settings {
    fn load() -> Self {
        let Some(path) = settings_path().filter(|p| p.exists()) else { return Self::default() };
        std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice(&b)?))
            .map_err(|e| log::warn!("can't read {}: {e:#}", path.display()))
            .unwrap_or_default()
    }

    /// Readable only by the user, since it holds the R2 key.
    fn save(&self) -> Result<()> {
        let path = settings_path().context("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        crate::dirs::write_private(&tmp, &serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn client_id(&self) -> Option<&str> {
        let id = self.client_id.trim();
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then_some(id)
    }
}

// ---------------------------------------------------------------------------
// IPC

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;

/// Where Discord listens on Windows: a named pipe.
#[cfg(windows)]
fn socket_paths() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("NODEPLAYER_DISCORD_IPC") {
        return vec![p.into()];
    }
    (0..10).map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}"))).collect()
}

/// Where Discord (native, Flatpak, Snap, Vesktop; `$TMPDIR` on macOS) listens.
#[cfg(unix)]
fn socket_paths() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("NODEPLAYER_DISCORD_IPC") {
        return vec![p.into()];
    }
    let mut bases: Vec<PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    bases.push("/tmp".into());
    let subs = ["", "app/com.discordapp.Discord", "app/com.discordapp.DiscordCanary", "snap.discord", ".flatpak/dev.vencord.Vesktop/xdg-run"];
    let mut out = Vec::new();
    for base in &bases {
        for sub in subs {
            for i in 0..10 {
                out.push(base.join(sub).join(format!("discord-ipc-{i}")));
            }
        }
    }
    out
}

struct Conn {
    #[cfg(unix)]
    stream: UnixStream,
    /// The named pipe. It has no timeouts, but Discord answers every request, and closing
    /// Discord breaks the pipe.
    #[cfg(windows)]
    stream: std::fs::File,
}

impl Conn {
    /// Connects and says hello; the logged-in user's name.
    fn open(client_id: &str) -> Result<(Self, String)> {
        #[cfg(unix)]
        let stream = {
            let stream = socket_paths().into_iter().find_map(|p| UnixStream::connect(p).ok()).context("Discord isn't running")?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            stream
        };
        #[cfg(windows)]
        let stream = socket_paths()
            .into_iter()
            .find_map(|p| std::fs::OpenOptions::new().read(true).write(true).open(p).ok())
            .context("Discord isn't running")?;
        let mut c = Self { stream };
        c.send(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }))?;
        let (op, reply) = c.recv()?;
        if op == OP_CLOSE || reply["evt"] == "ERROR" {
            let msg = reply["message"].as_str().or(reply["data"]["message"].as_str()).unwrap_or("refused");
            bail!("Discord: {msg}");
        }
        let user = &reply["data"]["user"];
        let name = user["global_name"].as_str().or(user["username"].as_str()).unwrap_or("").to_string();
        Ok((c, name))
    }

    fn send(&mut self, op: u32, payload: &Value) -> Result<()> {
        let body = payload.to_string();
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body.as_bytes());
        self.stream.write_all(&frame)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<(u32, Value)> {
        let mut head = [0u8; 8];
        self.stream.read_exact(&mut head)?;
        let op = u32::from_le_bytes(head[..4].try_into().unwrap());
        let len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
        if len > 1 << 20 {
            bail!("oversized frame from Discord");
        }
        let mut body = vec![0; len];
        self.stream.read_exact(&mut body)?;
        Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// Sets (or with `None`, clears) the activity; Discord's complaint if it has one.
    fn set_activity(&mut self, activity: Option<&Value>) -> Result<Option<String>> {
        let nonce = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        let args = json!({ "pid": std::process::id(), "activity": activity });
        self.send(OP_FRAME, &json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce }))?;
        // Replies to other requests or events may come first.
        loop {
            let (op, reply) = self.recv()?;
            if op == OP_CLOSE {
                bail!("Discord closed the connection: {}", reply["message"].as_str().unwrap_or(""));
            }
            if reply["nonce"].as_str() == Some(nonce.as_str()) {
                return Ok((reply["evt"] == "ERROR").then(|| reply["data"]["message"].as_str().unwrap_or("error").to_string()));
            }
        }
    }
}

/// What the worker should show.
#[derive(Clone, Debug, PartialEq)]
struct Want {
    client_id: String,
    activity: Value,
    /// The track's own cover, to upload.
    image: Option<Cover>,
    r2: Option<r2::Config>,
    /// Artist and album to look the cover up by.
    lookup: Option<(String, String)>,
}

/// A cover ready to upload: a JPEG of at most 512 px, named after the original's hash so the
/// same picture goes up once.
#[derive(Clone, Debug)]
struct Cover {
    key: String,
    jpeg: Arc<Vec<u8>>,
}

impl PartialEq for Cover {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Cover {
    /// The embedded picture, else an image next to the track.
    fn of(status: &Status, fields: &Fields) -> Option<Self> {
        let bytes = match &status.tags.cover {
            Some(c) => c.1.clone(),
            None => std::fs::read(crate::mpris::folder_image(fields.path().parent()?)?).ok()?,
        };
        let img = image::load_from_memory(&bytes).map_err(|e| log::debug!("cover: {e}")).ok()?;
        let img = if img.width() > 512 || img.height() > 512 { img.resize(512, 512, image::imageops::FilterType::Lanczos3) } else { img };
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90).encode_image(&img.to_rgb8()).ok()?;
        let hash: String = ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect();
        Some(Self { key: format!("nodeplayer/covers/{hash}.jpg"), jpeg: Arc::new(jpeg.into_inner()) })
    }
}

/// How the connection is doing, for the tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Link {
    #[default]
    Off,
    /// The user's name (arRPC, in Vesktop, says "arRPC"), and when the activity was last
    /// accepted (Unix seconds).
    Connected(String, Option<i64>),
    Failed(String),
}

/// Discord allows 5 activity updates per 20 seconds.
const MIN_INTERVAL: Duration = Duration::from_secs(4);

/// Owns the socket: shows the latest wanted activity, reconnecting while Discord isn't there.
fn worker(rx: Receiver<Option<Want>>, link: Arc<Mutex<Link>>, cover_note: Arc<Mutex<Option<String>>>, ctx: egui::Context) {
    let mut conn: Option<(Conn, String)> = None;
    let mut want: Option<Want> = None;
    let mut shown: Option<Value> = None;
    let mut dirty = false;
    let mut last_send = Instant::now() - MIN_INTERVAL;
    let mut retry_at = Instant::now();
    let mut covers: HashMap<(String, String), Option<String>> = HashMap::new();
    // Covers put into the bucket (or that couldn't be), for the R2 settings they went to.
    let mut uploaded: HashMap<String, Option<String>> = HashMap::new();
    let mut uploaded_for: Option<r2::Config> = None;
    let set_link = |l: Link| {
        let mut cur = link.lock().unwrap();
        if *cur != l {
            *cur = l;
            ctx.request_repaint();
        }
    };
    loop {
        let wait = if !dirty {
            Duration::from_secs(3600)
        } else if want.is_some() && conn.is_none() {
            retry_at.saturating_duration_since(Instant::now())
        } else {
            (last_send + MIN_INTERVAL).saturating_duration_since(Instant::now())
        };
        match rx.recv_timeout(wait) {
            Ok(w) => {
                // Only the newest matters.
                let w = rx.try_iter().last().unwrap_or(w);
                if w.as_ref().map(|w| &w.client_id) != want.as_ref().map(|w| &w.client_id) {
                    conn = None;
                    shown = None;
                    retry_at = Instant::now();
                }
                if w.is_none() {
                    // Closing the connection clears the activity.
                    conn = None;
                    shown = None;
                    set_link(Link::Off);
                }
                dirty |= w != want || (w.is_some() && conn.is_none());
                want = w;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let Some(w) = want.clone() else {
            dirty = false;
            continue;
        };
        if conn.is_none() {
            match Conn::open(&w.client_id) {
                Ok((c, user)) => {
                    set_link(Link::Connected(user.clone(), None));
                    conn = Some((c, user));
                }
                Err(e) => {
                    set_link(Link::Failed(format!("{e:#}")));
                    retry_at = Instant::now() + Duration::from_secs(15);
                    continue;
                }
            }
        }
        let mut activity = w.activity.clone();
        if uploaded_for != w.r2 {
            uploaded.clear();
            uploaded_for = w.r2.clone();
        }
        let own = match (&w.image, &w.r2) {
            (Some(img), Some(cfg)) => uploaded
                .entry(img.key.clone())
                .or_insert_with(|| {
                    let r = upload(cfg, img);
                    let note = r.as_ref().err().map(|e| format!("Couldn't upload the cover: {e:#}"));
                    *cover_note.lock().unwrap() = note;
                    ctx.request_repaint();
                    r.ok()
                })
                .clone(),
            _ => None,
        };
        let mut found = || {
            let key = w.lookup.as_ref()?;
            covers.entry(key.clone()).or_insert_with(|| look_up_cover(&key.0, &key.1)).clone()
        };
        if let Some(url) = own.or_else(&mut found) {
            activity["assets"]["large_image"] = url.into();
        }
        if activity["assets"].as_object().is_some_and(|a| a.is_empty()) {
            activity.as_object_mut().unwrap().remove("assets");
        }
        dirty = false;
        if shown.as_ref() == Some(&activity) {
            continue;
        }
        let (c, user) = conn.as_mut().unwrap();
        last_send = Instant::now();
        match c.set_activity(Some(&activity)) {
            Ok(None) => {
                set_link(Link::Connected(user.clone(), Some(now_ms() / 1000)));
                shown = Some(activity);
            }
            Ok(Some(e)) => {
                log::warn!("Discord: {e}");
                set_link(Link::Failed(format!("Discord: {e}")));
                shown = Some(activity);
            }
            Err(e) => {
                log::debug!("Discord: {e:#}");
                conn = None;
                shown = None;
                dirty = true;
                retry_at = Instant::now() + Duration::from_secs(5);
                set_link(Link::Failed(format!("{e:#}")));
            }
        }
    }
}

/// Puts the cover into the bucket unless it's there; its public address.
fn upload(cfg: &r2::Config, img: &Cover) -> Result<String> {
    if !cfg.exists(&img.key)? {
        cfg.put(&img.key, &img.jpeg, "image/jpeg")?;
        log::info!("uploaded {} to R2", img.key);
    }
    Ok(cfg.public(&img.key))
}

/// The album's cover on the iTunes Search API, 512 px.
fn look_up_cover(artist: &str, album: &str) -> Option<String> {
    let base = std::env::var("NODEPLAYER_ITUNES_URL").unwrap_or_else(|_| "https://itunes.apple.com".into());
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let text = agent
        .get(format!("{base}/search"))
        .query("term", format!("{artist} {album}"))
        .query("entity", "album")
        .query("limit", "5")
        .call()
        .map_err(|e| log::debug!("cover lookup: {e}"))
        .ok()?
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_to_string()
        .ok()?;
    pick_cover(&serde_json::from_str(&text).ok()?, artist, album)
}

/// The best match among search results: the same album name if there is one, else the first.
fn pick_cover(results: &Value, artist: &str, album: &str) -> Option<String> {
    let list = results["results"].as_array()?;
    let same = |a: &Value, k: &str, want: &str| a[k].as_str().is_some_and(|s| s.eq_ignore_ascii_case(want));
    let hit = list
        .iter()
        .find(|a| same(a, "collectionName", album) && same(a, "artistName", artist))
        .or_else(|| list.iter().find(|a| same(a, "collectionName", album)))
        .or_else(|| list.first())?;
    let url = hit["artworkUrl100"].as_str()?;
    Some(url.replace("100x100bb", "512x512bb"))
}

// ---------------------------------------------------------------------------
// The activity

/// Discord wants 2 to 128 characters (UTF-16 units) in each text.
fn fit(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > 127 {
            out.push('…');
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    if out.chars().count() < 2 {
        // A blank Braille pattern: not whitespace, so Discord doesn't trim it.
        out.push('\u{2800}');
    }
    Some(out)
}

fn render(template: &str, fields: &Fields) -> String {
    crate::titleformat::format(&template.replace("\r\n", "\n"), &|name| fields.get(name)).replace('\n', " ")
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The activity for a track. Playing, it has start and end times, which Discord turns into the
/// progress bar (so they're in real time: a rate shift makes the track shorter or longer).
fn activity(s: &Settings, fields: &Fields, playing: bool, position: f64, rate: f64, now: i64) -> Value {
    let mut a = json!({ "type": 2, "status_display_type": s.display.api() });
    let details = fit(&render(&s.details, fields)).unwrap_or_else(|| fit(&fields.get("title").unwrap_or_default()).unwrap_or_else(|| "nodeplayer".into()));
    a["details"] = details.into();
    let state = render(&s.state, fields);
    let state = if playing { fit(&state) } else { fit(&format!("⏸ {state}")) };
    if let Some(state) = state {
        a["state"] = state.into();
    }
    if playing && let Some(d) = fields.duration() {
        let rate = if rate > 0.0 { rate } else { 1.0 };
        let start = now - (position / rate * 1000.0) as i64;
        a["timestamps"] = json!({ "start": start, "end": start + (d / rate * 1000.0) as i64 });
    }
    a["assets"] = json!({});
    if let Some(t) = fit(&render(&s.large_text, fields)) {
        a["assets"]["large_text"] = t.into();
    }
    a
}

// ---------------------------------------------------------------------------

pub struct Discord {
    pub settings: Settings,
    saved: Settings,
    tx: Sender<Option<Want>>,
    link: Arc<Mutex<Link>>,
    /// Why the cover couldn't be uploaded, if it couldn't.
    cover_note: Arc<Mutex<Option<String>>>,
    ctx: egui::Context,
    /// The loaded track's fields and own cover, read off the UI thread.
    fields: Option<(Box<Fields>, Option<Cover>)>,
    reading: Option<PathBuf>,
    fields_rx: Receiver<(PathBuf, Box<Fields>, Option<Cover>)>,
    fields_tx: Sender<(PathBuf, Box<Fields>, Option<Cover>)>,
    /// Stopped rather than paused, until playing again.
    halted: bool,
    /// The last activity handed to the worker (with its times), and what it was made from.
    sent: Option<(Want, bool, i64)>,
}

impl Discord {
    pub fn new(ctx: egui::Context) -> Self {
        let settings = if crate::runtime::get().allow_network() {
            Settings::load()
        } else {
            Settings::default()
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let link = Arc::new(Mutex::new(Link::Off));
        let cover_note = Arc::new(Mutex::new(None));
        if crate::runtime::get().allow_network() {
            let (link, cover_note, ctx) = (link.clone(), cover_note.clone(), ctx.clone());
            std::thread::Builder::new().name("discord".into()).spawn(move || worker(rx, link, cover_note, ctx)).ok();
        }
        let (fields_tx, fields_rx) = crossbeam_channel::unbounded();
        Self {
            saved: settings.clone(),
            settings,
            tx,
            link,
            cover_note,
            ctx,
            fields: None,
            reading: None,
            fields_rx,
            fields_tx,
            halted: false,
            sent: None,
        }
    }

    /// Playback was stopped, or reached the end of the list.
    pub fn stopped(&mut self) {
        self.halted = true;
    }

    /// Runs every tick: keeps the activity in step with the player. `rate` is the playback
    /// rate in effect.
    pub fn update(&mut self, status: &Status, playing: bool, position: f64, rate: f64) {
        self.save();
        for (path, f, cover) in self.fields_rx.try_iter().collect::<Vec<_>>() {
            if self.reading.as_ref() == Some(&path) {
                self.reading = None;
            }
            self.fields = Some((f, cover));
        }
        self.halted &= !playing;
        let s = &self.settings;
        let Some(client_id) = s.client_id().filter(|_| s.enabled) else {
            self.clear();
            return;
        };
        let Some(path) = status.path.clone() else {
            self.clear();
            return;
        };
        if self.fields.as_ref().is_none_or(|(f, _)| f.path() != path.as_path()) {
            if self.reading.as_ref() != Some(&path) {
                self.reading = Some(path.clone());
                let (status, tx, ctx) = (status.clone(), self.fields_tx.clone(), self.ctx.clone());
                std::thread::spawn(move || {
                    if let Some(f) = Fields::read(&status) {
                        let cover = Cover::of(&status, &f);
                        let _ = tx.send((path, Box::new(f), cover));
                        ctx.request_repaint();
                    }
                });
            }
            return;
        }
        if self.halted || (!playing && !s.show_paused) {
            self.clear();
            return;
        }
        let (fields, image) = self.fields.as_ref().unwrap();
        let now = now_ms();
        let activity = activity(s, fields, playing, position, rate, now);
        let lookup = s.cover_lookup.then(|| (fields.get("artist").unwrap_or_default(), fields.get("album").unwrap_or_default()));
        let lookup = lookup.filter(|(a, b)| !a.is_empty() && !b.is_empty());
        let r2 = (s.cover_upload && s.r2.ready()).then(|| s.r2.clone());
        let image = image.clone().filter(|_| r2.is_some());
        let want = Want { client_id: client_id.to_string(), activity, image, r2, lookup };
        // Sent again when anything but the times changed, or the times moved by more than a
        // moment (a seek, a rate change).
        let start = want.activity["timestamps"]["start"].as_i64().unwrap_or(0);
        let untimed = |a: &Value| {
            let mut a = a.clone();
            a.as_object_mut().map(|o| o.remove("timestamps"));
            a
        };
        let same = self.sent.as_ref().is_some_and(|(w, p, st)| {
            *p == playing
                && untimed(&w.activity) == untimed(&want.activity)
                && w.image == want.image
                && w.r2 == want.r2
                && w.lookup == want.lookup
                && w.client_id == want.client_id
                && (st - start).abs() < 1500
        });
        if !same {
            let _ = self.tx.send(Some(want.clone()));
            self.sent = Some((want, playing, start));
        }
    }

    fn clear(&mut self) {
        if self.sent.take().is_some() {
            let _ = self.tx.send(None);
        }
    }

    fn save(&mut self) {
        if self.settings == self.saved {
            return;
        }
        if let Err(e) = self.settings.save() {
            log::warn!("can't save the Discord settings: {e:#}");
        }
        self.saved = self.settings.clone();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, status: &Status) {
        egui::ScrollArea::vertical().id_salt("discord").auto_shrink([false, false]).show(ui, |ui| {
            ui.checkbox(&mut self.settings.enabled, "Show what's playing on Discord");
            ui.horizontal(|ui| {
                ui.label("Application ID");
                ui.add(egui::TextEdit::singleline(&mut self.settings.client_id).hint_text("123456789012345678").desired_width(170.0));
            });
            if !self.settings.client_id.trim().is_empty() && self.settings.client_id().is_none() {
                ui.colored_label(Color32::LIGHT_RED, "The ID is a number.");
            }
            ui.small("Make an application at discord.com/developers/applications; its name is what \"Listening to\" shows. Copy its Application ID here.");
            ui.horizontal(|ui| {
                if ui.link("Open the Developer Portal").clicked() {
                    let _ = crate::dirs::open("https://discord.com/developers/applications");
                }
            });

            ui.add_space(4.0);
            match self.link.lock().unwrap().clone() {
                Link::Off if self.settings.enabled && self.settings.client_id().is_some() => {
                    ui.weak(if status.path.is_some() { "Connecting…" } else { "Shown once something plays." });
                }
                Link::Off => {
                    ui.weak("Off");
                }
                Link::Connected(user, updated) => {
                    ui.label(match user.as_str() {
                        "" => "✔ Connected to Discord".to_string(),
                        // Vesktop's built-in server passes activities on to its Discord tab.
                        "arRPC" => "✔ Connected to arRPC (Vesktop)".to_string(),
                        u => format!("✔ Connected to Discord as {u}"),
                    });
                    match updated {
                        Some(t) => ui.small(format!("Activity last accepted at {}", crate::library::fmt_datetime(t))),
                        None => ui.small("No activity sent yet."),
                    };
                }
                Link::Failed(e) => {
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                ui.strong("Texts");
                ui.label("ⓘ").on_hover_text("foobar2000 title formatting, as in the Misskey tab (%title%, %artist%, %album%, %date%, %play_count%, $if(...), ...)");
            });
            egui::Grid::new("discord texts").num_columns(2).show(ui, |ui| {
                for (label, text, default) in [
                    ("First line", &mut self.settings.details, DEFAULT_DETAILS),
                    ("Second line", &mut self.settings.state, DEFAULT_STATE),
                    ("Cover tooltip", &mut self.settings.large_text, DEFAULT_LARGE_TEXT),
                ] {
                    ui.label(label);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(text).font(egui::TextStyle::Monospace).desired_width(220.0));
                        if ui.add_enabled(text.as_str() != default, egui::Button::new("↺").small()).on_hover_text("default").clicked() {
                            *text = default.into();
                        }
                    });
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Member list shows");
                egui::ComboBox::from_id_salt("discord display").selected_text(self.settings.display.label()).show_ui(ui, |ui| {
                    for d in Display::ALL {
                        ui.selectable_value(&mut self.settings.display, d, d.label());
                    }
                });
            });
            ui.checkbox(&mut self.settings.show_paused, "Keep showing while paused (without the bar)");

            ui.separator();
            ui.strong("Cover");
            ui.small("Discord only shows images from the web.");
            ui.checkbox(&mut self.settings.cover_upload, "Upload the track's own cover to Cloudflare R2")
                .on_hover_text("the embedded picture, or cover.jpg and the like next to the track, as a JPEG of up to 512 px; each picture goes up once");
            if self.settings.cover_upload {
                egui::Grid::new("discord r2").num_columns(2).show(ui, |ui| {
                    let r2 = &mut self.settings.r2;
                    for (label, text, hint, secret) in [
                        ("Account ID", &mut r2.account_id, "in the R2 overview", false),
                        ("Access key ID", &mut r2.access_key_id, "from an R2 API token", false),
                        ("Secret access key", &mut r2.secret_access_key, "", true),
                        ("Bucket", &mut r2.bucket, "", false),
                        ("Public URL", &mut r2.public_url, "https://pub-….r2.dev or your domain", false),
                    ] {
                        ui.label(label);
                        ui.add(egui::TextEdit::singleline(text).hint_text(hint).password(secret).desired_width(230.0));
                        ui.end_row();
                    }
                });
                if !self.settings.r2.ready() {
                    ui.weak("Fill in all five to upload.");
                }
                if let Some(note) = self.cover_note.lock().unwrap().clone() {
                    ui.colored_label(Color32::LIGHT_RED, note);
                }
            }
            ui.checkbox(&mut self.settings.cover_lookup, "Otherwise, look the cover up online")
                .on_hover_text("for tracks without a cover of their own (or with R2 off): searches the iTunes Search API for the artist and album and shows the cover it finds");

            ui.separator();
            ui.strong("Preview");
            match self.fields.as_ref().map(|(f, _)| f).filter(|f| status.path.as_deref() == Some(f.path())) {
                Some(f) => {
                    let a = activity(&self.settings, f, true, 0.0, 1.0, 0);
                    // Without the padding fit() adds, which the UI fonts don't have.
                    let text = |k: &str, k2: Option<&str>| {
                        let v = match k2 {
                            Some(k2) => &a[k][k2],
                            None => &a[k],
                        };
                        v.as_str().map(|s| s.trim_end_matches('\u{2800}').to_string())
                    };
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let shown = match self.settings.display {
                            Display::Name => Some("(application name)".to_string()),
                            Display::State => text("state", None),
                            Display::Details => text("details", None),
                        };
                        ui.small(format!("Listening to {}", shown.unwrap_or_default()));
                        ui.strong(text("details", None).unwrap_or_default());
                        if let Some(s) = text("state", None) {
                            ui.label(s);
                        }
                        if let Some(t) = text("assets", Some("large_text")) {
                            ui.weak(t);
                        }
                    });
                }
                None => {
                    ui.weak("Play something to see it.");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_fit_discord() {
        assert_eq!(fit("  "), None);
        assert_eq!(fit("A").as_deref(), Some("A\u{2800}"));
        let long = fit(&"あ".repeat(300)).unwrap();
        assert!(long.encode_utf16().count() <= 128 && long.ends_with('…'));
        // Emoji are two UTF-16 units each.
        assert!(fit(&"🎵".repeat(100)).unwrap().encode_utf16().count() <= 128);
    }

    #[test]
    fn activity_has_the_bar_while_playing() {
        let f = Fields::for_test("/m/Song.flac", &[("title", "Song"), ("artist", "Band"), ("album", "LP")], Some(200.0));
        let s = Settings::default();
        let a = activity(&s, &f, true, 50.0, 1.0, 1_000_000);
        assert_eq!(a["type"], 2);
        assert_eq!(a["details"], "Song");
        assert_eq!(a["state"], "Band");
        assert_eq!(a["assets"]["large_text"], "LP");
        assert_eq!(a["timestamps"]["start"], 1_000_000 - 50_000);
        assert_eq!(a["timestamps"]["end"], 1_000_000 + 150_000);
        // Twice as fast: half the time left.
        let a = activity(&s, &f, true, 50.0, 2.0, 1_000_000);
        assert_eq!(a["timestamps"]["end"], 1_000_000 + 75_000);
        // Paused: no bar.
        let a = activity(&s, &f, false, 50.0, 1.0, 1_000_000);
        assert!(a.get("timestamps").is_none());
        assert_eq!(a["state"], "⏸ Band");
    }

    #[test]
    fn covers_from_search_results() {
        let r = json!({ "results": [
            { "collectionName": "Other", "artistName": "Band", "artworkUrl100": "https://x/a/100x100bb.jpg" },
            { "collectionName": "LP", "artistName": "Band", "artworkUrl100": "https://x/b/100x100bb.jpg" },
        ]});
        assert_eq!(pick_cover(&r, "band", "lp").as_deref(), Some("https://x/b/512x512bb.jpg"));
        assert_eq!(pick_cover(&r, "Band", "Nope").as_deref(), Some("https://x/a/512x512bb.jpg"));
        assert_eq!(pick_cover(&json!({ "results": [] }), "a", "b"), None);
    }
}
