//! Misskey NowPlaying: posts the playing track as a note (`/api/notes/create`) to one or more
//! Misskey accounts, from a foobar2000-style template. A port of the fooyin plugin
//! fooyin_misskey_np, whose settings can be imported.
//!
//! A Now Playing card ([`crate::card`]) can be attached: it's uploaded to the account's
//! Drive, in a "nodeplayer" folder, and the note links it.
//!
//! Accounts log in with MiAuth (or a pasted access token). The settings, tokens included, are
//! kept in `~/.config/nodeplayer/misskey.json`, readable only by the user.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use egui::Color32;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::av::TrackInfo;
use crate::card;
use crate::library::{self, PlayStats};
use crate::player::Status;

const USER_AGENT: &str = concat!("nodeplayer/", env!("CARGO_PKG_VERSION"));
/// Misskey's default limit on a note's length.
const MAX_NOTE: usize = 3000;
/// How long MiAuth waits for the app to be allowed in the browser.
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
/// The same note again within this time isn't posted twice.
const REPEAT_GUARD: Duration = Duration::from_secs(5);

pub const DEFAULT_TEMPLATE: &str = "#NowPlaying with nodeplayer :ablobcatmusic:

**%title%**
[**Artist:** %artist%
][**Album:** %album%
][**Track:** %tracknumber%
][**Released:** %date%
]$ifgreater(%play_count%,0,**Plays:** %play_count%
,)
**Technical Info:**
- **Codec:** %codec%[ '('%codec_profile%')']
- **Bitrate:** $if(%bitrate%,%bitrate% kbps,N/A)
- **Samplerate:** $if(%samplerate%,$div(%samplerate%,1000).$left($right(0$mod(%samplerate%,1000),3),1) kHz,N/A)
- **Channels:** $if(%channels%,%channels%,2) ch
- **Bit Depth:** $if($info(bitspersample),$info(bitspersample) bit,N/A)
- **Encoding:** $if($stricmp($info(encoding),lossless),Lossless,Lossy)";

const TEMPLATE_HELP: &str = "foobar2000 title formatting:
%field%  a tag or value, empty when missing
[...]  shown only when a field inside it has a value
'text'  literal text ($, %, [ and , lose their meaning)
$if(cond,then,else) $if2(a,b) $if3(a,b,...,else)
$ifequal(a,b,then,else) $ifgreater(a,b,then,else)
$stricmp(a,b) $strcmp(a,b) $not(x) $and(...) $or(...)
$add $sub $mul $div $mod $min $max
$left(s,n) $right(s,n) $len(s) $upper(s) $lower(s) $caps(s)
$trim(s) $num(n,width) $pad(s,width) $replace(s,from,to)
$info(name) $meta(name) $crlf()

Fields: title, artist, album, album artist, tracknumber, totaltracks,
discnumber, totaldiscs, genre, date, year, composer, performer, comment,
length, length_seconds, play_count, first_played, last_played, added,
filename, filename_ext, directoryname, path, and any other tag by its name.
Info (also as %name%): codec, codec_profile, bitrate, samplerate,
channels, bitspersample, encoding (lossless or lossy).";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Public,
    Home,
    Followers,
}

impl Visibility {
    const ALL: [Self; 3] = [Self::Public, Self::Home, Self::Followers];

    fn api(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Home => "home",
            Self::Followers => "followers",
        }
    }

    fn from_api(s: &str) -> Self {
        Self::ALL.into_iter().find(|v| v.api() == s).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// `user@host`, for showing.
    pub name: String,
    /// Base URL, like `https://misskey.io`.
    pub instance: String,
    pub token: String,
    pub visibility: Visibility,
    pub local_only: bool,
    pub enabled: bool,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            name: String::new(),
            instance: String::new(),
            token: String::new(),
            visibility: Visibility::Public,
            local_only: false,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub on_start: bool,
    pub on_pause: bool,
    pub on_stop: bool,
    /// Attach the Now Playing card image.
    pub attach_card: bool,
    pub template: String,
    pub accounts: Vec<Account>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            on_start: true,
            on_pause: false,
            on_stop: false,
            attach_card: true,
            template: DEFAULT_TEMPLATE.into(),
            accounts: Vec::new(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(crate::session::config_dir()?.join("misskey.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = settings_path().filter(|p| p.exists()) else { return Self::default() };
        std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice(&b)?))
            .map_err(|e| log::warn!("can't read {}: {e:#}", path.display()))
            .unwrap_or_default()
    }

    /// Writes through a temporary file, readable only by the user since it holds tokens.
    pub fn save(&self) -> Result<()> {
        let path = settings_path().context("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        crate::dirs::write_private(&tmp, &serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// `misskey.io`, `https://misskey.io/` and the like → `https://misskey.io`; `None` without a
/// host.
pub fn instance_base(url: &str) -> Option<String> {
    let url = url.trim().split(['?', '#']).next().unwrap_or("");
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let rest = rest.trim_end_matches('/');
    let host = rest.split('/').next().unwrap_or("");
    (!host.is_empty() && !host.contains(char::is_whitespace)).then(|| format!("{scheme}://{rest}"))
}

fn host(base: &str) -> &str {
    base.split_once("://").map_or(base, |(_, rest)| rest).split('/').next().unwrap_or(base)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into()
}

/// POSTs JSON to `base/api/endpoint`; on failure, Misskey's error message.
fn api(agent: &ureq::Agent, base: &str, endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut resp = agent
        .post(format!("{base}/api/{endpoint}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if !(200..300).contains(&status) {
        let msg = json["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("HTTP {status}: {msg}"));
    }
    Ok(json)
}

pub fn post_note(account: &Account, text: &str, files: &[String]) -> Result<(), String> {
    let mut body = json!({ "i": account.token, "text": text, "visibility": account.visibility.api() });
    if account.local_only {
        body["localOnly"] = true.into();
    }
    if !files.is_empty() {
        body["fileIds"] = json!(files);
    }
    api(&agent(), &account.instance, "notes/create", &body).map(|_| ())
}

/// The Drive folder cards go into.
const DRIVE_FOLDER: &str = "nodeplayer";

/// The id of the account's "nodeplayer" Drive folder, made when missing; remembered for the
/// run. `None` when it can't be had (the card then goes to the Drive's top).
fn drive_folder(agent: &ureq::Agent, account: &Account) -> Option<String> {
    static FOLDERS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let key = format!("{}\n{}", account.instance, account.token);
    if let Some((_, id)) = FOLDERS.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return Some(id.clone());
    }
    let found = api(agent, &account.instance, "drive/folders/find", &json!({ "i": account.token, "name": DRIVE_FOLDER }))
        .map_err(|e| log::debug!("misskey: finding the Drive folder: {e}"))
        .ok()?;
    let id = match found[0]["id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            let made = api(agent, &account.instance, "drive/folders/create", &json!({ "i": account.token, "name": DRIVE_FOLDER }))
                .map_err(|e| log::debug!("misskey: making the Drive folder: {e}"))
                .ok()?;
            made["id"].as_str()?.to_string()
        }
    };
    FOLDERS.lock().unwrap().push((key, id.clone()));
    Some(id)
}

/// Uploads the card to the account's Drive (`drive/files/create`, multipart); its file id.
fn upload_card(account: &Account, png: &[u8]) -> Result<String, String> {
    let agent = agent();
    let folder = drive_folder(&agent, account);
    let boundary = format!("nodeplayer-{}", session_id());
    let mut body = Vec::with_capacity(png.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    };
    field("i", &account.token);
    if let Some(f) = &folder {
        field("folderId", f);
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"nowplaying-{stamp}.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let mut resp = agent
        .post(format!("{}/api/drive/files/create", account.instance))
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .header("User-Agent", USER_AGENT)
        .send(body)
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if !(200..300).contains(&status) {
        let msg = json["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("HTTP {status}: {msg}"));
    }
    json["id"].as_str().map(str::to_string).ok_or_else(|| "no file id in the answer".into())
}

/// The account a token belongs to.
fn verify_token(base: &str, token: &str) -> Result<Account, String> {
    let me = api(&agent(), base, "i", &json!({ "i": token }))?;
    let user = me["username"].as_str().ok_or("no user in the answer")?;
    Ok(Account { name: format!("{user}@{}", host(base)), instance: base.into(), token: token.into(), ..Default::default() })
}

/// A random UUID (v4), for the MiAuth session.
fn session_id() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b = (t ^ (std::process::id() as u128) << 64).to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn miauth_url(base: &str, session: &str) -> String {
    format!("{base}/miauth/{session}?name=nodeplayer%20NowPlaying&permission=write:notes,write:drive,read:drive")
}

/// Asks the server every couple of seconds whether the user allowed the app yet.
fn miauth_wait(base: &str, session: &str, cancel: &AtomicBool) -> Result<Account, String> {
    let agent = agent();
    let deadline = Instant::now() + AUTH_TIMEOUT;
    let mut last_error = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        match api(&agent, base, &format!("miauth/{session}/check"), &json!({})) {
            Ok(v) => {
                if let Some(token) = v["token"].as_str().filter(|t| !t.is_empty()) {
                    let user = v["user"]["username"].as_str().unwrap_or("?");
                    return Ok(Account {
                        name: format!("{user}@{}", host(base)),
                        instance: base.into(),
                        token: token.into(),
                        ..Default::default()
                    });
                }
            }
            Err(e) => last_error = Some(e),
        }
        if Instant::now() > deadline {
            return Err(match last_error {
                Some(e) => format!("not allowed in time ({e})"),
                None => "not allowed in time".into(),
            });
        }
        for _ in 0..10 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn open_browser(url: &str) {
    if let Err(e) = crate::dirs::open(url) {
        log::warn!("can't open {url}: {e}");
    }
}

/// Everything the template can ask about a track.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    path: PathBuf,
    info: TrackInfo,
    /// From the player, for when FFmpeg can't read the file.
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    samplerate: u32,
    channels: usize,
    /// How often and when the track was played before (nodeplayer's count).
    plays: Option<PlayStats>,
}

impl Fields {
    /// Reads the file's tags and format; slow enough to keep off the UI thread.
    pub(crate) fn read(status: &Status) -> Option<Self> {
        let path = status.path.clone()?;
        let info = crate::av::track_info(&path).map_err(|e| log::debug!("{}: {e:#}", path.display())).unwrap_or_default();
        Some(Self {
            path,
            info,
            title: status.tags.title.clone(),
            artist: status.tags.artist.clone(),
            album: status.tags.album.clone(),
            duration: status.duration,
            samplerate: status.source_rate,
            channels: status.channels,
            plays: None,
        })
    }

    fn meta(&self, k: &str) -> Option<String> {
        self.info.meta.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn tech(&self, k: &str) -> Option<String> {
        let found = self.info.tech.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        found.or_else(|| match k {
            "samplerate" => (self.samplerate > 0).then(|| self.samplerate.to_string()),
            "channels" => (self.channels > 0).then(|| self.channels.to_string()),
            _ => None,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: &str, meta: &[(&str, &str)], duration: Option<f64>) -> Self {
        let meta = meta.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Self { path: path.into(), info: TrackInfo { meta, duration, ..Default::default() }, ..Default::default() }
    }

    pub(crate) fn duration(&self) -> Option<f64> {
        self.info.duration.or(self.duration).filter(|d| *d > 0.0)
    }

    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(k) = name.strip_prefix("__") {
            return self.tech(k);
        }
        let file = |f: fn(&Path) -> Option<&std::ffi::OsStr>| f(&self.path).map(|s| s.to_string_lossy().into_owned());
        match name {
            "title" => self.meta("title").or_else(|| self.title.clone()).or_else(|| file(Path::file_stem)),
            "artist" => self
                .meta("artist")
                .or_else(|| self.artist.clone())
                .or_else(|| self.meta("album artist"))
                .or_else(|| self.meta("composer"))
                .or_else(|| self.meta("performer")),
            "album artist" | "album_artist" | "albumartist" => self.meta("album artist").or_else(|| self.get("artist")),
            "album" => self.meta("album").or_else(|| self.album.clone()),
            // Two digits, as foobar2000 shows it.
            "tracknumber" | "track" => {
                self.meta("tracknumber").map(|t| t.trim().parse::<u32>().map_or(t.clone(), |n| format!("{n:02}")))
            }
            "discnumber" | "disc" => self.meta("discnumber"),
            "totaltracks" | "tracktotal" => self.meta("totaltracks"),
            "totaldiscs" | "disctotal" => self.meta("totaldiscs"),
            "year" => {
                let date = self.meta("date")?;
                let year: String = date.chars().take(4).collect();
                (year.len() == 4 && year.chars().all(|c| c.is_ascii_digit())).then_some(year)
            }
            "filename" => file(Path::file_stem),
            "filename_ext" => file(Path::file_name),
            "directoryname" => self.path.parent().and_then(Path::file_name).map(|s| s.to_string_lossy().into_owned()),
            "path" => Some(self.path.display().to_string()),
            "length" | "duration" => self.duration().map(|d| {
                let s = d.round() as u64;
                match s / 3600 {
                    0 => format!("{}:{:02}", s / 60, s % 60),
                    h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
                }
            }),
            "length_seconds" => self.duration().map(|d| (d.round() as u64).to_string()),
            // Like foo_playcount: nothing for a track never played, so $if(%play_count%,...)
            // leaves it out.
            "play_count" => self.plays.filter(|s| s.count > 0).map(|s| s.count.to_string()),
            "first_played" => self.plays.map(|s| library::fmt_datetime(s.first)).filter(|t| !t.is_empty()),
            "last_played" => self.plays.map(|s| library::fmt_datetime(s.last)).filter(|t| !t.is_empty()),
            "added" => Some(library::fmt_datetime(library::added(&self.path))).filter(|t| !t.is_empty()),
            "codec" | "codec_profile" | "bitrate" | "samplerate" | "channels" | "bitspersample" | "encoding" => self.tech(name),
            _ => self.meta(name),
        }
    }
}

/// The note's text. An `@` in a field gets a zero-width space after it, so a title like
/// "@home" doesn't mention anyone.
pub fn render(template: &str, fields: &Fields) -> String {
    let template = template.replace("\r\n", "\n");
    crate::titleformat::format(&template, &|name| fields.get(name).map(|v| v.replace('@', "@\u{200B}"))).trim().to_string()
}

/// The track's cover, ready for the card: its embedded picture, else an image next to it
/// (cover.jpg, folder.png, ...).
fn card_cover(fields: &Fields, status: &Status) -> Option<Arc<image::RgbaImage>> {
    let thumb = match &status.tags.cover {
        Some(c) => card::thumbnail(&c.1),
        None => card::thumbnail(&std::fs::read(crate::mpris::folder_image(fields.path.parent()?)?).ok()?),
    };
    thumb.map(Arc::new)
}

fn card_for(fields: &Fields, cover: Option<Arc<image::RgbaImage>>, state: card::State, position: f64) -> card::Card {
    card::Card {
        title: fields.get("title").unwrap_or_default(),
        artist: fields.get("artist"),
        album: fields.get("album"),
        cover,
        state,
        position,
        duration: fields.duration(),
    }
}

/// The Now Playing card for a track.
fn render_card(fields: &Fields, status: &Status, state: card::State, position: f64) -> Option<image::RgbaImage> {
    card::render(&card_for(fields, card_cover(fields, status), state, position))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Reason {
    Start,
    Pause,
    Stop,
    Manual,
}

enum Msg {
    /// Posted to the account; why the card couldn't be attached, if it couldn't.
    Posted(String, Option<String>),
    Failed(String, String),
    Skipped,
    /// The current track's fields and its cover for the card.
    Fields(PathBuf, Box<Fields>, Option<Arc<image::RgbaImage>>),
    /// The preview card, drawn for that track, state and second.
    Card(CardKey, Option<egui::ColorImage>),
    Authorized(Account),
    AuthFailed(String),
}

/// Track, state and whole second: what the preview card was drawn for.
type CardKey = (PathBuf, card::State, u64);

struct Auth {
    /// The page to allow the app on; `None` while checking a pasted token.
    url: Option<String>,
    cancel: Arc<AtomicBool>,
}

pub struct Misskey {
    pub settings: Settings,
    /// What's on disk, to notice changes.
    saved: Settings,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    /// The last note posted and when, shared with the posting threads.
    last: Arc<Mutex<Option<(String, Instant)>>>,
    /// The track last seen starting to play.
    started: Option<PathBuf>,
    was_playing: bool,
    plays: PlayStats,
    /// Stopped rather than paused, until playing again.
    halted: bool,
    position: f64,
    /// Playback was stopped (not paused) since the last update.
    stopped: bool,
    auth: Option<Auth>,
    new_instance: String,
    new_token: String,
    /// The playing track's fields and card cover, for the preview.
    preview: Option<Box<Fields>>,
    preview_cover: Option<Arc<image::RgbaImage>>,
    reading: Option<PathBuf>,
    /// The preview card, redrawn as the state and position change, and what it shows.
    card: Option<egui::TextureHandle>,
    card_key: Option<CardKey>,
    drawing: bool,
    /// The outcome of the last post or login, and whether it went well.
    outcome: Option<(String, bool)>,
}

impl Misskey {
    pub fn new(ctx: egui::Context) -> Self {
        let settings = if crate::runtime::get().allow_network() {
            Settings::load()
        } else {
            Settings::default()
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            saved: settings.clone(),
            settings,
            tx,
            rx,
            ctx,
            last: Arc::default(),
            started: None,
            was_playing: false,
            plays: PlayStats::default(),
            halted: false,
            position: 0.0,
            stopped: false,
            auth: None,
            new_instance: String::new(),
            new_token: String::new(),
            preview: None,
            preview_cover: None,
            reading: None,
            card: None,
            card_key: None,
            drawing: false,
            outcome: None,
        }
    }

    /// Posts when a track starts or playback pauses, as set; returns lines for the app's log.
    /// Runs every tick.
    /// `plays`: how often and when the loaded track was played, for `%play_count%` and such.
    pub fn update(&mut self, status: &Status, playing: bool, position: f64, plays: PlayStats) -> Vec<String> {
        self.plays = plays;
        let mut log = Vec::new();
        for m in self.rx.try_iter().collect::<Vec<_>>() {
            match m {
                Msg::Posted(name, None) => self.outcome = Some((format!("Posted to {name}."), true)),
                Msg::Posted(name, Some(e)) => {
                    let msg = format!("Posted to {name} without the card: {}. Logging in again allows uploading it.", e.trim_end_matches('.'));
                    log.push(format!("Misskey: {msg}"));
                    self.outcome = Some((msg, false));
                }
                Msg::Failed(name, e) => {
                    log.push(format!("Misskey: posting to {name} failed: {e}"));
                    self.outcome = Some((format!("Posting to {name} failed: {e}"), false));
                }
                Msg::Skipped => self.outcome = Some(("Not posted: the same note was just posted.".into(), true)),
                Msg::Fields(path, f, cover) => {
                    if self.reading.as_ref() == Some(&path) {
                        self.reading = None;
                    }
                    self.preview = Some(f);
                    self.preview_cover = cover;
                }
                Msg::Card(key, img) => {
                    self.drawing = false;
                    self.card_key = Some(key);
                    match (img, &mut self.card) {
                        (Some(img), Some(tex)) => tex.set(img, egui::TextureOptions::LINEAR),
                        (img, card) => *card = img.map(|i| self.ctx.load_texture("misskey card", i, egui::TextureOptions::LINEAR)),
                    }
                }
                Msg::Authorized(account) => {
                    self.auth = None;
                    self.new_instance.clear();
                    self.new_token.clear();
                    self.outcome = Some((format!("Added {}.", account.name), true));
                    log.push(format!("Misskey: added {}", account.name));
                    // Logging in again replaces the account's token.
                    match self.settings.accounts.iter_mut().find(|a| a.name == account.name && a.instance == account.instance) {
                        Some(a) => a.token = account.token,
                        None => self.settings.accounts.push(account),
                    }
                    self.save();
                }
                Msg::AuthFailed(e) => {
                    self.auth = None;
                    self.outcome = Some((format!("Login failed: {e}"), false));
                }
            }
        }

        let stopped = std::mem::take(&mut self.stopped);
        if playing && status.path.is_some() && status.path != self.started {
            self.started = status.path.clone();
            if self.settings.on_start {
                self.post(status, Reason::Start);
            }
        }
        if self.was_playing && !playing && !stopped && self.settings.on_pause {
            // Playing to the end isn't pausing.
            let at_end = status.duration.is_some_and(|d| position >= d - 0.5);
            if !at_end {
                self.post(status, Reason::Pause);
            }
        }
        self.was_playing = playing;
        self.halted &= !playing;
        self.position = position;
        log
    }

    /// Playback was stopped, or reached the end of the list.
    pub fn stopped(&mut self, status: &Status) {
        self.stopped = true;
        self.halted = true;
        if self.settings.on_stop {
            self.post(status, Reason::Stop);
        }
    }

    fn post(&mut self, status: &Status, reason: Reason) {
        if !self.settings.enabled && reason != Reason::Manual {
            return;
        }
        let accounts: Vec<Account> =
            self.settings.accounts.iter().filter(|a| a.enabled && !a.token.is_empty()).cloned().collect();
        if accounts.is_empty() || status.path.is_none() {
            if reason == Reason::Manual {
                let why = if status.path.is_none() { "nothing is playing" } else { "no account is on" };
                self.outcome = Some((format!("Not posted: {why}."), false));
            }
            return;
        }
        log::debug!("misskey: posting ({reason:?})");
        let state = match reason {
            Reason::Start => card::State::Playing,
            Reason::Pause => card::State::Paused,
            Reason::Stop => card::State::Stopped,
            Reason::Manual if self.was_playing => card::State::Playing,
            Reason::Manual => card::State::Paused,
        };
        let position = if reason == Reason::Start { 0.0 } else { self.position };
        let attach = self.settings.attach_card;
        let (status, template, last, tx, ctx, plays) =
            (status.clone(), self.settings.template.clone(), self.last.clone(), self.tx.clone(), self.ctx.clone(), self.plays);
        std::thread::spawn(move || {
            let Some(mut fields) = Fields::read(&status) else { return };
            // Told apart from the last note without the play count, which goes up each time
            // the same song plays again.
            let same = render(&template, &fields);
            fields.plays = Some(plays);
            let text = render(&template, &fields);
            if text.is_empty() {
                return;
            }
            {
                let mut last = last.lock().unwrap();
                // A new track playing the same note (the same song again) isn't posted again.
                let repeat = last.as_ref().is_some_and(|(t, at)| *t == same && (reason == Reason::Start || at.elapsed() < REPEAT_GUARD));
                if repeat {
                    let _ = tx.send(Msg::Skipped);
                    ctx.request_repaint();
                    return;
                }
                *last = Some((same, Instant::now()));
            }
            let png = attach
                .then(|| render_card(&fields, &status, state, position))
                .flatten()
                .and_then(|img| card::png(&img).map_err(|e| log::warn!("card: {e}")).ok());
            for a in accounts {
                let (files, card_error) = match &png {
                    Some(png) => match upload_card(&a, png) {
                        Ok(id) => (vec![id], None),
                        Err(e) => (Vec::new(), Some(e)),
                    },
                    None => (Vec::new(), None),
                };
                let _ = tx.send(match post_note(&a, &text, &files) {
                    Ok(()) => Msg::Posted(a.name, card_error),
                    Err(e) => Msg::Failed(a.name, e),
                });
                ctx.request_repaint();
            }
        });
    }

    fn save(&mut self) {
        if self.settings == self.saved {
            return;
        }
        match self.settings.save() {
            Ok(()) => self.saved = self.settings.clone(),
            Err(e) => {
                log::warn!("can't save the Misskey settings: {e:#}");
                self.outcome = Some((format!("Can't save the settings: {e:#}"), false));
                // Don't try again every frame.
                self.saved = self.settings.clone();
            }
        }
    }

    fn login(&mut self, base: String) {
        let session = session_id();
        let url = miauth_url(&base, &session);
        let cancel = Arc::new(AtomicBool::new(false));
        self.auth = Some(Auth { url: Some(url.clone()), cancel: cancel.clone() });
        self.outcome = None;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            open_browser(&url);
            let _ = tx.send(match miauth_wait(&base, &session, &cancel) {
                Ok(a) => Msg::Authorized(a),
                Err(e) => Msg::AuthFailed(e),
            });
            ctx.request_repaint();
        });
    }

    fn add_token(&mut self, base: String, token: String) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.auth = Some(Auth { url: None, cancel });
        self.outcome = None;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(match verify_token(&base, &token) {
                Ok(a) => Msg::Authorized(a),
                Err(e) => Msg::AuthFailed(e),
            });
            ctx.request_repaint();
        });
    }

    fn import_fooyin(&mut self, path: &Path) {
        match read_fooyin(path) {
            Ok(imported) => {
                let mut added = 0;
                for a in imported.accounts {
                    if !self.settings.accounts.iter().any(|b| b.instance == a.instance && b.token == a.token) {
                        self.settings.accounts.push(a);
                        added += 1;
                    }
                }
                self.settings.enabled = imported.enabled;
                self.settings.on_start = imported.on_start;
                self.settings.on_pause = imported.on_pause;
                self.settings.on_stop = imported.on_stop;
                self.settings.template = imported.template;
                self.outcome = Some((format!("Imported the template and {added} new account(s) from fooyin."), true));
                self.save();
            }
            Err(e) => self.outcome = Some((format!("Can't import from fooyin: {e:#}"), false)),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, status: &Status) {
        // Keep the preview for the loaded track.
        let shown = self.preview.as_ref().map(|f| &f.path);
        if let Some(path) = status.path.clone().filter(|p| shown != Some(p) && self.reading.as_ref() != Some(p)) {
            self.reading = Some(path.clone());
            let (status, tx, ctx) = (status.clone(), self.tx.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                if let Some(f) = Fields::read(&status) {
                    let cover = card_cover(&f, &status);
                    let _ = tx.send(Msg::Fields(path, Box::new(f), cover));
                    ctx.request_repaint();
                }
            });
        }
        // and its card, redrawn each second and when playback pauses or stops.
        let fields = self.preview.as_ref().filter(|f| Some(&f.path) == status.path.as_ref());
        if let Some(f) = fields.filter(|_| self.settings.attach_card && !self.drawing) {
            let state = match (self.was_playing, self.halted) {
                (true, _) => card::State::Playing,
                (false, true) => card::State::Stopped,
                (false, false) => card::State::Paused,
            };
            let key = (f.path.clone(), state, self.position.max(0.0) as u64);
            if self.card_key.as_ref() != Some(&key) {
                self.drawing = true;
                let card = card_for(f, self.preview_cover.clone(), state, self.position);
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                std::thread::spawn(move || {
                    let img = card::render(&card).map(|img| {
                        egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
                    });
                    let _ = tx.send(Msg::Card(key, img));
                    ctx.request_repaint();
                });
            }
        }

        egui::ScrollArea::vertical().id_salt("misskey").auto_shrink([false, false]).show(ui, |ui| {
            ui.checkbox(&mut self.settings.enabled, "Post what's playing to Misskey");
            ui.add_enabled_ui(self.settings.enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Post when");
                    ui.checkbox(&mut self.settings.on_start, "a track starts");
                    ui.checkbox(&mut self.settings.on_pause, "pausing");
                    ui.checkbox(&mut self.settings.on_stop, "stopping");
                });
                ui.checkbox(&mut self.settings.attach_card, "Attach a card image")
                    .on_hover_text("cover, state, title, artist and progress, uploaded to the Drive's \"nodeplayer\" folder");
            });

            ui.separator();
            ui.strong("Accounts");
            self.accounts_ui(ui);

            ui.separator();
            ui.horizontal(|ui| {
                ui.strong("Note template");
                ui.label("ⓘ").on_hover_text(TEMPLATE_HELP);
                let default = self.settings.template == DEFAULT_TEMPLATE;
                if ui.add_enabled(!default, egui::Button::new("Reset").small()).clicked() {
                    self.settings.template = DEFAULT_TEMPLATE.into();
                }
            });
            egui::ScrollArea::vertical().id_salt("misskey template").max_height(220.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.settings.template)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.strong("Preview");
                let can_post = status.path.is_some() && self.settings.accounts.iter().any(|a| a.enabled);
                if ui.add_enabled(can_post, egui::Button::new("Post now")).on_hover_text("post the current track now").clicked() {
                    self.post(status, Reason::Manual);
                }
            });
            match self.preview.as_ref().filter(|f| Some(&f.path) == status.path.as_ref()) {
                Some(f) => {
                    let text = render(&self.settings.template, &Fields { plays: Some(self.plays), ..(**f).clone() });
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(&text).wrap().selectable(true));
                    });
                    let n = text.chars().count();
                    if n > MAX_NOTE {
                        ui.colored_label(Color32::LIGHT_RED, format!("{n} characters: longer than Misskey usually allows ({MAX_NOTE})"));
                    } else {
                        ui.small(format!("{n} characters"));
                    }
                    let current = self.card_key.as_ref().is_some_and(|k| k.0 == f.path);
                    if let Some(tex) = self.card.as_ref().filter(|_| self.settings.attach_card && current) {
                        let w = ui.available_width();
                        let size = tex.size_vec2();
                        ui.add(egui::Image::new((tex.id(), egui::vec2(w, w * size.y / size.x))));
                    }
                }
                None if status.path.is_some() => {
                    ui.weak("Reading the track…");
                }
                None => {
                    ui.weak("Play something to see the note.");
                }
            }
            if let Some((msg, ok)) = &self.outcome {
                ui.add_space(4.0);
                if *ok {
                    ui.label(msg);
                } else {
                    ui.colored_label(Color32::LIGHT_RED, msg);
                }
            }
        });
        self.save();
    }

    fn accounts_ui(&mut self, ui: &mut egui::Ui) {
        if self.settings.accounts.is_empty() {
            ui.weak("No accounts yet.");
        }
        let mut remove = None;
        for (i, a) in self.settings.accounts.iter_mut().enumerate() {
            crate::app::right_to_left_row(ui, |ui| {
                if ui.small_button("🗑").on_hover_text("remove").clicked() {
                    remove = Some(i);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.checkbox(&mut a.enabled, "").on_hover_text("post to this account");
                    ui.add(egui::Label::new(&a.name).truncate()).on_hover_text(&a.instance);
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                egui::ComboBox::from_id_salt(("visibility", i)).width(90.0).selected_text(a.visibility.api()).show_ui(ui, |ui| {
                    for v in Visibility::ALL {
                        ui.selectable_value(&mut a.visibility, v, v.api());
                    }
                });
                ui.checkbox(&mut a.local_only, "local only").on_hover_text("not federated to other servers");
            });
        }
        if let Some(i) = remove {
            self.settings.accounts.remove(i);
        }

        ui.add_space(4.0);
        if let Some(auth) = &self.auth {
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                ui.spinner();
                match &auth.url {
                    Some(url) => {
                        ui.label("Allow nodeplayer in the browser.");
                        ui.hyperlink_to("Open the page", url).on_hover_text(url);
                    }
                    None => {
                        ui.label("Checking the token…");
                    }
                }
                cancel = ui.button("Cancel").clicked();
            });
            if cancel {
                auth.cancel.store(true, Ordering::Relaxed);
                self.auth = None;
            }
            return;
        }
        let base = instance_base(&self.new_instance);
        crate::app::right_to_left_row(ui, |ui| {
            let login = ui.add_enabled(base.is_some(), egui::Button::new("Log in")).on_hover_text("log in with MiAuth in the browser");
            ui.add(egui::TextEdit::singleline(&mut self.new_instance).hint_text("server, like misskey.io").desired_width(f32::INFINITY));
            if login.clicked()
                && let Some(b) = base.clone()
            {
                self.login(b);
            }
        });
        egui::CollapsingHeader::new("Use an access token instead").id_salt("misskey token").show(ui, |ui| {
            ui.small(
                "Make one in Settings → API on the server, allowing \"Compose or delete notes\" and, for the card, \"Access your Drive files\" and \"Edit or delete your Drive files\".",
            );
            crate::app::right_to_left_row(ui, |ui| {
                let ok = base.is_some() && !self.new_token.trim().is_empty();
                let add = ui.add_enabled(ok, egui::Button::new("Add"));
                ui.add(egui::TextEdit::singleline(&mut self.new_token).password(true).hint_text("token").desired_width(f32::INFINITY));
                if add.clicked()
                    && let Some(b) = base.clone()
                {
                    let token = self.new_token.trim().to_string();
                    self.add_token(b, token);
                }
            });
        });
        if let Some(path) = fooyin_settings().filter(|p| p.is_file())
            && ui.button("Import from fooyin").on_hover_text(format!("accounts and template of fooyin's Misskey NowPlaying\n{}", path.display())).clicked()
        {
            self.import_fooyin(&path);
        }
    }
}

/// The fooyin plugin's settings (QSettings, INI format).
fn fooyin_settings() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("fooyin/misskey_nowplaying.conf"))
}

fn read_fooyin(path: &Path) -> Result<Settings> {
    let text = std::fs::read(path).with_context(|| path.display().to_string())?;
    let values = parse_ini(&String::from_utf8_lossy(&text));
    let get = |k: &str| values.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let flag = |k: &str, default: bool| get(k).map_or(default, |v| v == "true");
    let d = Settings::default();
    let mut s = Settings {
        enabled: flag("enabled", d.enabled),
        on_start: flag("postOnStart", d.on_start),
        on_pause: flag("postOnPause", d.on_pause),
        on_stop: flag("postOnStop", d.on_stop),
        attach_card: d.attach_card,
        template: get("template").map_or(d.template, str::to_string),
        accounts: Vec::new(),
    };
    let count: usize = get("accounts/size").and_then(|n| n.parse().ok()).unwrap_or(0);
    for i in 1..=count {
        let field = |k: &str| get(&format!("accounts/{i}/{k}")).unwrap_or("").to_string();
        let Some(instance) = instance_base(&field("instanceUrl")) else { continue };
        let name = field("name");
        s.accounts.push(Account {
            name: if name.is_empty() { host(&instance).to_string() } else if name.contains('@') { name } else { format!("{name}@{}", host(&instance)) },
            token: field("apiToken"),
            visibility: Visibility::from_api(&field("visibility")),
            local_only: field("localOnly") == "true",
            enabled: field("enabled") != "false",
            instance,
        });
    }
    Ok(s)
}

/// QSettings' INI files: `section/key` (with `\` in keys as `/`, and `General` as no section)
/// and the unescaped value.
fn parse_ini(text: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = if name == "General" { String::new() } else { format!("{name}/") };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        out.push((format!("{section}{}", key.trim().replace('\\', "/")), ini_value(value)));
    }
    out
}

fn ini_value(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {}
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('a') => out.push('\x07'),
                Some('b') => out.push('\x08'),
                Some('f') => out.push('\x0c'),
                Some('v') => out.push('\x0b'),
                Some('0') => out.push('\0'),
                Some('x') => {
                    let mut code = 0u32;
                    while let Some(d) = chars.peek().and_then(|c| c.to_digit(16)) {
                        code = code.saturating_mul(16).saturating_add(d);
                        chars.next();
                    }
                    out.extend(char::from_u32(code));
                }
                Some(other) => out.push(other),
                None => {}
            },
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_urls() {
        assert_eq!(instance_base("misskey.io").as_deref(), Some("https://misskey.io"));
        assert_eq!(instance_base(" https://misskey.io/?x#y ").as_deref(), Some("https://misskey.io"));
        assert_eq!(instance_base("http://127.0.0.1:8080/").as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(instance_base(""), None);
        assert_eq!(instance_base("https://"), None);
        assert_eq!(host("https://misskey.io"), "misskey.io");
    }

    #[test]
    fn session_ids_are_uuids() {
        let a = session_id();
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert_ne!(a, session_id());
    }

    fn fields() -> Fields {
        Fields {
            path: "/music/Some Album/01 - Song.flac".into(),
            info: TrackInfo {
                meta: vec![
                    ("title".into(), "Song".into()),
                    ("artist".into(), "Band".into()),
                    ("tracknumber".into(), "1".into()),
                    ("date".into(), "2011-04-01".into()),
                ],
                tech: vec![
                    ("codec".into(), "FLAC".into()),
                    ("bitrate".into(), "912".into()),
                    ("samplerate".into(), "44100".into()),
                    ("channels".into(), "2".into()),
                    ("bitspersample".into(), "16".into()),
                    ("encoding".into(), "lossless".into()),
                ],
                duration: Some(245.4),
            },
            ..Default::default()
        }
    }

    #[test]
    fn default_template() {
        let text = render(DEFAULT_TEMPLATE, &fields());
        assert!(text.starts_with("#NowPlaying with nodeplayer"), "{text}");
        assert!(text.contains("**Song**\n**Artist:** Band\n**Track:** 01\n**Released:** 2011-04-01\n\n**Technical Info:**"), "{text}");
        let again = render(DEFAULT_TEMPLATE, &Fields { plays: Some(PlayStats { count: 3, ..Default::default() }), ..fields() });
        assert!(again.contains("**Released:** 2011-04-01\n**Plays:** 3\n\n**Technical Info:**"), "{again}");
        let first = render(DEFAULT_TEMPLATE, &Fields { plays: Some(PlayStats::default()), ..fields() });
        assert!(!first.contains("Plays"), "{first}");
        assert!(!text.contains("Album"), "{text}");
        assert!(text.contains("- **Codec:** FLAC\n"), "{text}");
        assert!(text.contains("- **Samplerate:** 44.1 kHz"), "{text}");
        assert!(text.contains("- **Bit Depth:** 16 bit"), "{text}");
        assert!(text.ends_with("- **Encoding:** Lossless"), "{text}");
    }

    #[test]
    fn fields_dont_mention_anyone() {
        let mut f = fields();
        f.info.meta[1].1 = "@band@example.com".into();
        assert_eq!(render("%artist% @me", &f), "@\u{200B}band@\u{200B}example.com @me");
    }

    #[test]
    fn field_names() {
        let f = fields();
        assert_eq!(f.get("album artist").as_deref(), Some("Band"));
        assert_eq!(f.get("year").as_deref(), Some("2011"));
        assert_eq!(f.get("length").as_deref(), Some("4:05"));
        assert_eq!(f.get("filename_ext").as_deref(), Some("01 - Song.flac"));
        assert_eq!(f.get("directoryname").as_deref(), Some("Some Album"));
        assert_eq!(f.get("__samplerate").as_deref(), Some("44100"));
        assert_eq!(f.get("album"), None);
        assert_eq!(f.get("date").as_deref(), Some("2011-04-01"));
        assert_eq!(render("[%play_count% plays]", &f), "");
        assert_eq!(render("[%play_count% plays]", &Fields { plays: Some(PlayStats::default()), ..fields() }), "");
        let played = Fields { plays: Some(PlayStats { count: 3, first: 1_700_000_000, last: 1_800_000_000 }), ..fields() };
        assert_eq!(render("[%play_count% plays]", &played), "3 plays");
        // fooyin's play stats lines.
        let stats = "$if(%play_count%,**Play Stats:**\n- **Play Count:** %play_count%\n$if(%last_played%,- **Last Played:** %last_played%,),)";
        let text = render(stats, &played);
        assert!(text.starts_with("**Play Stats:**\n- **Play Count:** 3\n- **Last Played:** 2027-01-"), "{text}");
        assert_eq!(text.len(), "**Play Stats:**\n- **Play Count:** 3\n- **Last Played:** 2027-01-15 08:00:00".len());
        assert_eq!(render(stats, &fields()), "");
        let untagged = Fields { path: "/x/Name.mp3".into(), ..Default::default() };
        assert_eq!(untagged.get("title").as_deref(), Some("Name"));
    }

    #[test]
    fn fooyin_settings_import() {
        let ini = "[General]\nenabled=true\npostOnPause=true\npostOnStart=true\npostOnStop=false\n\
                   template=\"#NowPlaying \\\"x\\\"\\n%title%, $if(%a%,b,c) \u{266a}\\x3042\"\n\n\
                   [accounts]\n1\\apiToken=abc\n1\\enabled=true\n1\\instanceUrl=https://misskey.example/\n\
                   1\\localOnly=true\n1\\name=me\n1\\visibility=home\nsize=1\n";
        let dir = std::env::temp_dir().join(format!("nodeplayer-fooyin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("misskey_nowplaying.conf");
        std::fs::write(&path, ini).unwrap();
        let s = read_fooyin(&path).unwrap();
        std::fs::remove_dir_all(dir).ok();
        assert!(s.enabled && s.on_pause && s.on_start && !s.on_stop);
        assert_eq!(s.template, "#NowPlaying \"x\"\n%title%, $if(%a%,b,c) \u{266a}\u{3042}");
        assert_eq!(
            s.accounts,
            vec![Account {
                name: "me@misskey.example".into(),
                instance: "https://misskey.example".into(),
                token: "abc".into(),
                visibility: Visibility::Home,
                local_only: true,
                enabled: true,
            }]
        );
    }
}
