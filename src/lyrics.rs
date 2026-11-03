//! Lyrics panel: shows the current track's lyrics, following the playback position when they
//! are timed (LRC).
//!
//! Where they come from ([`Provider`]): the one picked for the track, or ("auto") the first of
//! these that has them:
//! 1. an `.lrc` (or `.txt`) file next to the track with the same name,
//! 2. lyrics embedded in the file's tags,
//! 3. online, when "online" is on: NetEase Cloud Music (music.163.com), Kugou
//!    (krcs.kugou.com), then LRCLIB (lrclib.net), searched by title and artist and matched by
//!    name and length. NetEase and Kugou are the services' unofficial web APIs; LRCLIB's is
//!    public. None needs a key. Each one's answers (found or not) are cached in
//!    `~/.cache/nodeplayer/lyrics/`.
//!
//! The provider picked and the timing offset are kept per track ([`LyricsStore`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use egui::{Align2, Color32, FontId, Rect, Sense, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::player::Player;

/// The services answer browsers; an unknown client may get nothing.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
/// LRCLIB asks clients to name themselves.
const LRCLIB_AGENT: &str = concat!("nodeplayer/", env!("CARGO_PKG_VERSION"), " (https://github.com/Zel9278/nodeplayer)");
/// How long a "not found" answer is trusted before asking again.
const RETRY_MISSING: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsSettings {
    /// Look the lyrics up online when there are none locally.
    pub online: bool,
    pub font_size: f32,
}

impl Default for LyricsSettings {
    fn default() -> Self {
        Self { online: true, font_size: 16.0 }
    }
}

/// Where lyrics come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// An `.lrc` or `.txt` next to the track.
    File,
    /// The file's tags.
    Embedded,
    NetEase,
    Kugou,
    Lrclib,
}

impl Provider {
    /// All of them, in the order "auto" asks them.
    pub const ALL: [Self; 5] = [Self::File, Self::Embedded, Self::NetEase, Self::Kugou, Self::Lrclib];

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Embedded => "embedded",
            Self::NetEase => "NetEase",
            Self::Kugou => "Kugou",
            Self::Lrclib => "LRCLIB",
        }
    }

    /// For cache file names.
    fn id(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Embedded => "embedded",
            Self::NetEase => "netease",
            Self::Kugou => "kugou",
            Self::Lrclib => "lrclib",
        }
    }

    /// The online service behind it.
    fn service(self) -> Option<Service> {
        match self {
            Self::NetEase => Some(netease),
            Self::Kugou => Some(kugou),
            Self::Lrclib => Some(lrclib),
            Self::File | Self::Embedded => None,
        }
    }
}

/// What is kept for one track.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackLyrics {
    /// The provider picked; `None` takes the first that has lyrics.
    pub provider: Option<Provider>,
    /// Added to the playback position, in seconds; positive shows lines earlier.
    pub offset: f32,
}

/// Each track's provider and offset, by path (`lyrics.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsStore {
    pub tracks: BTreeMap<String, TrackLyrics>,
    /// Counts changes, for saving.
    #[serde(skip)]
    pub revision: u64,
}

impl LyricsStore {
    fn key(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    pub fn get(&self, path: &Path) -> TrackLyrics {
        self.tracks.get(&Self::key(path)).cloned().unwrap_or_default()
    }

    /// Keeps `kept` for the track; a track left at the defaults goes.
    pub fn set(&mut self, path: &Path, kept: TrackLyrics) {
        if self.get(path) == kept {
            return;
        }
        let key = Self::key(path);
        if kept == TrackLyrics::default() {
            self.tracks.remove(&key);
        } else {
            self.tracks.insert(key, kept);
        }
        self.revision += 1;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lyrics {
    /// Lines with their start time in seconds, in order.
    Synced(Vec<(f64, String)>),
    Plain(String),
}

impl Lyrics {
    /// LRC when the text has timestamps, else plain text.
    pub fn from_text(text: &str) -> Option<Self> {
        if let Some(lines) = parse_lrc(text) {
            return Some(Self::Synced(lines));
        }
        let plain = text.trim();
        (!plain.is_empty()).then(|| Self::Plain(plain.replace("\r\n", "\n")))
    }

    fn lines(&self) -> Vec<&str> {
        match self {
            Self::Synced(l) => l.iter().map(|(_, s)| s.as_str()).collect(),
            Self::Plain(t) => t.lines().collect(),
        }
    }
}

/// `[mm:ss.xx]` (also `[m:ss]`, `[mm:ss:xx]`) → seconds.
fn parse_stamp(s: &str) -> Option<f64> {
    let (m, rest) = s.split_once(':')?;
    let m: f64 = m.trim().parse().ok()?;
    let rest = rest.trim().replacen(':', ".", 1);
    let sec: f64 = rest.parse().ok()?;
    (m >= 0.0 && (0.0..60.0).contains(&sec)).then_some(m * 60.0 + sec)
}

/// Lines of an LRC text, or `None` when it has no timed lines. Handles several stamps on one
/// line, the `[offset:ms]` header and word timings (`<mm:ss.xx>`, dropped).
pub fn parse_lrc(text: &str) -> Option<Vec<(f64, String)>> {
    let mut offset = 0.0;
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        let mut stamps = Vec::new();
        while let Some(rest) = line.strip_prefix('[') {
            let Some(end) = rest.find(']') else { break };
            let tag = &rest[..end];
            match parse_stamp(tag) {
                Some(t) => stamps.push(t),
                None => {
                    if let Some(v) = tag.strip_prefix("offset:") {
                        offset = v.trim().parse::<f64>().unwrap_or(0.0) / 1000.0;
                    }
                }
            }
            line = rest[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        // Enhanced LRC: drop the per-word timings.
        let mut text = String::new();
        let mut rest = line;
        while let Some(i) = rest.find('<') {
            text.push_str(&rest[..i]);
            match rest[i..].find('>') {
                Some(j) if parse_stamp(&rest[i + 1..i + j]).is_some() => rest = &rest[i + j + 1..],
                _ => {
                    text.push('<');
                    rest = &rest[i + 1..];
                }
            }
        }
        text.push_str(rest);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for t in stamps {
            out.push(((t - offset).max(0.0), text.clone()));
        }
    }
    if out.is_empty() {
        return None;
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(out)
}

/// What a lookup needs to know about the track.
#[derive(Clone, Debug, Default)]
struct Track {
    path: PathBuf,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    embedded: Option<String>,
}

#[derive(Clone, Debug)]
enum State {
    Searching,
    Found(Lyrics, String),
    Missing(String),
}

fn sidecar(path: &Path) -> Option<(String, PathBuf)> {
    for ext in ["lrc", "LRC", "txt"] {
        let p = path.with_extension(ext);
        if p != path
            && let Ok(bytes) = std::fs::read(&p)
        {
            return Some((String::from_utf8_lossy(&bytes).into_owned(), p));
        }
    }
    None
}

fn cache_dir() -> Option<PathBuf> {
    Some(crate::dirs::cache()?.join("lyrics"))
}

/// FNV-1a, stable across builds (the cache file name).
fn key_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[derive(Serialize, Deserialize)]
struct Cached {
    /// The provider asked (its label). Earlier versions kept one answer for all of them: the
    /// one that had the lyrics, or empty when none did.
    source: String,
    lyrics: Option<String>,
    /// Seconds since the epoch of the lookup.
    when: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(12))).http_status_as_error(false).build().into()
}

/// GETs `url` and parses the JSON answer. A browser's User-Agent goes along unless `headers`
/// has one.
fn get_json(agent: &ureq::Agent, url: &str, query: &[(&str, String)], headers: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let mut req = agent.get(url);
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("User-Agent")) {
        req = req.header("User-Agent", USER_AGENT);
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    for (k, v) in query {
        req = req.query(*k, v);
    }
    let mut resp = req.call().map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Lowercase letters and digits only, without bracketed parts ("(Remastered 2011)"), for
/// comparing names.
fn norm(s: &str) -> String {
    let mut depth = 0;
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' | '（' | '[' | '【' | '「' if depth < 4 => depth += 1,
            ')' | '）' | ']' | '】' | '」' if depth > 0 => depth -= 1,
            c if depth == 0 && c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => {}
        }
    }
    if out.is_empty() { s.to_lowercase() } else { out }
}

/// How well a search result matches the track; `None` when the title doesn't.
fn match_score(t: &Track, title: &str, name: &str, artists: &[String], secs: Option<f64>) -> Option<i32> {
    let (want, got) = (norm(title), norm(name));
    let mut score = if want == got {
        4
    } else if !want.is_empty() && (got.contains(&want) || want.contains(&got)) {
        2
    } else {
        return None;
    };
    if let Some(artist) = &t.artist {
        let a = norm(artist);
        if artists.iter().any(|x| {
            let x = norm(x);
            !x.is_empty() && (a.contains(&x) || x.contains(&a))
        }) {
            score += 3;
        }
    }
    if let (Some(d), Some(s)) = (t.duration, secs) {
        let diff = (d - s).abs();
        score += if diff < 3.0 {
            3
        } else if diff < 10.0 {
            1
        } else {
            -3
        };
    }
    Some(score)
}

/// Drops the credit lines the services put first ("作词 : …", "Lyrics by：…", the
/// "Title - Artist" line), which aren't sung.
fn strip_credits(lrc: &str, title: &str) -> String {
    const WORDS: [&str; 14] =
        ["作词", "作曲", "编曲", "制作", "作詞", "編曲", "词", "曲", "lyrics by", "composed by", "produced by", "arranged by", "written by", "混音"];
    let mut out = String::new();
    let mut sung = false;
    for line in lrc.lines() {
        let body = line.rsplit(']').next().unwrap_or("").trim();
        let credit = !sung && {
            let lower = body.to_lowercase();
            let head = lower.split([':', '：']).next().unwrap_or("").trim();
            let named = (lower.contains(':') || lower.contains('：')) && WORDS.iter().any(|w| head.contains(w)) && head.chars().count() <= 16;
            named || body.contains(" - ") && norm(body).contains(&norm(title))
        };
        if !credit && !body.is_empty() && line.starts_with('[') && parse_stamp(&line[1..line.find(']').unwrap_or(1)]).is_some() {
            sung = true;
        }
        if !credit {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn netease(t: &Track, title: &str) -> Result<Option<String>, String> {
    const REFERER: &str = "https://music.163.com/";
    let agent = agent();
    let query = match &t.artist {
        Some(a) => format!("{title} {a}"),
        None => title.to_string(),
    };
    let found = get_json(&agent, "https://music.163.com/api/cloudsearch/pc", &[("s", query), ("type", "1".into()), ("limit", "10".into())], &[("Referer", REFERER)])?;
    let mut picks: Vec<(i32, u64)> = found["result"]["songs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let artists: Vec<String> = s["ar"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str().map(String::from)).collect();
            let secs = s["dt"].as_f64().map(|ms| ms / 1000.0);
            let score = match_score(t, title, s["name"].as_str()?, &artists, secs)?;
            Some((score, s["id"].as_u64()?))
        })
        .collect();
    picks.sort_by_key(|(score, _)| -score);
    // The best match may be an instrumental or lack lyrics; try a few.
    for (_, id) in picks.into_iter().take(3) {
        let l = get_json(&agent, "https://music.163.com/api/song/lyric", &[("id", id.to_string()), ("lv", "1".into()), ("tv", "-1".into())], &[("Referer", REFERER)])?;
        if l["nolyric"].as_bool() == Some(true) || l["pureMusic"].as_bool() == Some(true) {
            continue;
        }
        if let Some(text) = l["lrc"]["lyric"].as_str().filter(|s| !s.trim().is_empty()) {
            return Ok(Some(strip_credits(text, title)));
        }
    }
    Ok(None)
}

fn kugou(t: &Track, title: &str) -> Result<Option<String>, String> {
    use base64::Engine as _;
    let agent = agent();
    let keyword = match &t.artist {
        Some(a) => format!("{a} - {title}"),
        None => title.to_string(),
    };
    let ms = t.duration.map_or(String::new(), |d| ((d * 1000.0) as u64).to_string());
    let found = get_json(
        &agent,
        "https://krcs.kugou.com/search",
        &[("ver", "1".into()), ("man", "yes".into()), ("client", "mobi".into()), ("keyword", keyword), ("duration", ms), ("hash", String::new())],
        &[],
    )?;
    let mut picks: Vec<(i32, String, String)> = found["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let artists = vec![c["singer"].as_str().unwrap_or("").to_string()];
            let secs = c["duration"].as_f64().map(|ms| ms / 1000.0);
            let score = match_score(t, title, c["song"].as_str()?, &artists, secs)?;
            Some((score, c["id"].as_str()?.to_string(), c["accesskey"].as_str()?.to_string()))
        })
        .collect();
    picks.sort_by_key(|(score, ..)| -score);
    for (_, id, key) in picks.into_iter().take(2) {
        let d = get_json(
            &agent,
            "https://krcs.kugou.com/download",
            &[("ver", "1".into()), ("client", "pc".into()), ("id", id), ("accesskey", key), ("fmt", "lrc".into()), ("charset", "utf8".into())],
            &[],
        )?;
        let Some(b64) = d["content"].as_str() else { continue };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else { continue };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !text.trim().is_empty() {
            return Ok(Some(strip_credits(&text, title)));
        }
    }
    Ok(None)
}

/// LRCLIB's public API (lrclib.net/docs): one search answers with the lyrics themselves.
fn lrclib(t: &Track, title: &str) -> Result<Option<String>, String> {
    let agent = agent();
    let mut query = vec![("track_name", title.to_string())];
    if let Some(a) = &t.artist {
        query.push(("artist_name", a.clone()));
    }
    let found = get_json(&agent, "https://lrclib.net/api/search", &query, &[("User-Agent", LRCLIB_AGENT)])?;
    let mut picks: Vec<(i32, &serde_json::Value)> = found
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["instrumental"].as_bool() != Some(true))
        .filter_map(|r| {
            let artists = vec![r["artistName"].as_str().unwrap_or("").to_string()];
            let score = match_score(t, title, r["trackName"].as_str()?, &artists, r["duration"].as_f64())?;
            // Between equal matches, timed lyrics first.
            let synced = r["syncedLyrics"].as_str().is_some_and(|s| !s.trim().is_empty());
            Some((score * 2 + i32::from(synced), r))
        })
        .collect();
    picks.sort_by_key(|(score, _)| -score);
    Ok(picks.into_iter().find_map(|(_, r)| {
        [r["syncedLyrics"].as_str(), r["plainLyrics"].as_str()].into_iter().flatten().find(|s| !s.trim().is_empty()).map(String::from)
    }))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// An online service: the track and the title to search for → LRC text, if it has any.
type Service = fn(&Track, &str) -> Result<Option<String>, String>;

/// The lyrics from `choice`, or ("auto", `None`) from the first provider that has them.
fn lookup(t: &Track, choice: Option<Provider>, online: bool, refresh: bool) -> State {
    let title = t.title.clone().unwrap_or_else(|| t.path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    let key = format!("{}\n{}\n{}", title.to_lowercase(), t.artist.as_deref().unwrap_or("").to_lowercase(), t.album.as_deref().unwrap_or("").to_lowercase());
    let picked = choice.is_some();
    let providers = match &choice {
        Some(p) => std::slice::from_ref(p),
        None => &Provider::ALL[..],
    };
    // Why there are none: errors, and (for the one picked) what was missing.
    let mut why = Vec::new();
    for &p in providers {
        match p {
            Provider::File => {
                if let Some((text, path)) = sidecar(&t.path)
                    && let Some(l) = Lyrics::from_text(&text)
                {
                    return State::Found(l, path.file_name().unwrap_or_default().to_string_lossy().into_owned());
                }
                if picked {
                    why.push("no .lrc or .txt next to the track".to_string());
                }
            }
            Provider::Embedded => {
                if let Some(l) = t.embedded.as_deref().and_then(Lyrics::from_text) {
                    return State::Found(l, p.label().into());
                }
                if picked {
                    why.push("none in the tags".to_string());
                }
            }
            _ if !online => {
                if picked {
                    why.push("online is off".to_string());
                }
            }
            _ => match online_lookup(t, &title, &key, p, refresh) {
                Ok(Some((l, true))) => return State::Found(l, format!("{} (cached)", p.label())),
                Ok(Some((l, false))) => return State::Found(l, p.label().into()),
                Ok(None) if picked => why.push(format!("not found on {}", p.label())),
                Ok(None) => {}
                Err(e) => {
                    log::warn!("lyrics lookup for {title} on {}: {e}", p.label());
                    why.push(format!("{}: {e}", p.label()));
                }
            },
        }
    }
    State::Missing(match why.is_empty() {
        false => why.join(", "),
        true if online => "not found online".into(),
        true => "no lyrics file or tag".into(),
    })
}

/// An online provider's lyrics for the track, and whether they came from the cache. Its
/// answer is cached, "not found" too (asked again after a while), but not a failure to ask.
fn online_lookup(t: &Track, title: &str, key: &str, p: Provider, refresh: bool) -> Result<Option<(Lyrics, bool)>, String> {
    let Some(service) = p.service() else { return Ok(None) };
    let file = cache_dir().map(|d| d.join(format!("{:016x}-{}.json", key_hash(key), p.id())));
    if !refresh && let Some(c) = cached(file.as_deref(), key, p) {
        match c.lyrics.as_deref().and_then(Lyrics::from_text) {
            Some(l) => return Ok(Some((l, true))),
            None if now_secs().saturating_sub(c.when) < RETRY_MISSING.as_secs() => return Ok(None),
            None => {}
        }
    }
    let found = service(t, title)?.filter(|text| Lyrics::from_text(text).is_some());
    if let Some(f) = &file {
        let c = Cached { source: p.label().into(), lyrics: found.clone(), when: now_secs() };
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, serde_json::to_vec(&c).unwrap_or_default());
    }
    Ok(found.as_deref().and_then(Lyrics::from_text).map(|l| (l, false)))
}

/// `p`'s cached answer; or lyrics an earlier version cached for the track as a whole, when
/// `p` had them.
fn cached(file: Option<&Path>, key: &str, p: Provider) -> Option<Cached> {
    let read = |f: &Path| std::fs::read(f).ok().and_then(|b| serde_json::from_slice::<Cached>(&b).ok());
    file.and_then(read).or_else(|| {
        let legacy = cache_dir()?.join(format!("{:016x}.json", key_hash(key)));
        read(&legacy).filter(|c| c.source == p.label() && c.lyrics.is_some())
    })
}

pub struct LyricsPanel {
    pub settings: LyricsSettings,
    /// Each track's provider and offset.
    pub store: LyricsStore,
    track: Option<PathBuf>,
    state: Arc<Mutex<Option<State>>>,
    /// The lookup in flight, so an old one finishing late is ignored.
    generation: Arc<Mutex<u64>>,
    /// Manual scrolling: offset in points and when it happened; following the song resumes
    /// a few seconds later.
    manual: Option<(f32, Instant)>,
}

impl LyricsPanel {
    pub fn new(settings: LyricsSettings, store: LyricsStore) -> Self {
        Self { settings, store, track: None, state: Arc::default(), generation: Arc::default(), manual: None }
    }

    /// The current track's offset, in seconds.
    fn offset(&self) -> f64 {
        self.track.as_deref().map_or(0.0, |p| f64::from(self.store.get(p).offset))
    }

    fn start_lookup(&mut self, player: &Player, refresh: bool) {
        let status = player.status();
        let Some(path) = status.path.clone() else {
            *self.state.lock().unwrap() = None;
            return;
        };
        let choice = self.store.get(&path).provider;
        let track = Track {
            path,
            title: status.tags.title.clone(),
            artist: status.tags.artist.clone(),
            album: status.tags.album.clone(),
            duration: status.duration,
            embedded: status.tags.lyrics.clone(),
        };
        let generation = {
            let mut g = self.generation.lock().unwrap();
            *g += 1;
            *g
        };
        *self.state.lock().unwrap() = Some(State::Searching);
        self.manual = None;
        let (state, gen_now, online) = (self.state.clone(), self.generation.clone(), self.settings.online);
        let _ = std::thread::Builder::new().name("lyrics".into()).spawn(move || {
            let found = lookup(&track, choice, online, refresh);
            if *gen_now.lock().unwrap() == generation {
                *state.lock().unwrap() = Some(found);
            }
        });
    }

    /// The panel; the lyrics take the room below the controls, and the wheel scrolls them.
    pub fn ui(&mut self, ui: &mut egui::Ui, player: &Player) {
        let path = player.status().path;
        if path != self.track {
            self.track = path;
            self.start_lookup(player, false);
        }
        let state = self.state.lock().unwrap().clone();
        let mut kept = self.track.as_deref().map(|p| self.store.get(p));

        ui.horizontal(|ui| {
            if let Some(kept) = &mut kept {
                let before = kept.provider;
                egui::ComboBox::from_id_salt("lyrics provider")
                    .width(90.0)
                    .selected_text(kept.provider.map_or("auto", Provider::label))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut kept.provider, None, "auto").on_hover_text("the first that has them");
                        for p in Provider::ALL {
                            ui.selectable_value(&mut kept.provider, Some(p), p.label());
                        }
                    })
                    .response
                    .on_hover_text("where this track's lyrics come from (kept for the track)");
                if kept.provider != before
                    && let Some(path) = self.track.clone()
                {
                    self.store.set(&path, kept.clone());
                    self.start_lookup(player, false);
                }
            }
            match &state {
                Some(State::Searching) => {
                    ui.spinner();
                    ui.small("looking for lyrics…");
                }
                Some(State::Found(_, source)) => {
                    ui.small(format!("from {source}"));
                }
                Some(State::Missing(why)) => {
                    ui.small(format!("no lyrics ({why})"));
                }
                None => {
                    ui.small("nothing playing");
                }
            }
            if ui.small_button("⟳").on_hover_text("look again (skips the cache)").clicked() {
                self.start_lookup(player, true);
            }
            if ui
                .checkbox(&mut self.settings.online, "online")
                .on_hover_text("look up NetEase, Kugou and LRCLIB (sends title, artist and length to music.163.com, kugou.com and lrclib.net)")
                .changed()
                && self.settings.online
                && matches!(state, Some(State::Missing(_)))
            {
                self.start_lookup(player, false);
            }
        });
        let synced = matches!(state, Some(State::Found(Lyrics::Synced(_), _)));
        if synced && let (Some(mut kept), Some(path)) = (kept, self.track.clone()) {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut kept.offset).speed(0.05).range(-30.0..=30.0).fixed_decimals(2).prefix("offset ").suffix(" s"))
                    .on_hover_text("positive shows lines earlier; kept for this track");
                if kept.offset != 0.0 && ui.small_button("↺").on_hover_text("offset back to 0").clicked() {
                    kept.offset = 0.0;
                }
                ui.add(egui::DragValue::new(&mut self.settings.font_size).speed(0.2).range(9.0..=48.0).fixed_decimals(0).prefix("size "));
            });
            self.store.set(&path, kept);
        } else {
            ui.add(egui::DragValue::new(&mut self.settings.font_size).speed(0.2).range(9.0..=48.0).fixed_decimals(0).prefix("size "));
        }

        let size = ui.available_size().max(vec2(80.0, 40.0));
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        let scroll = if resp.hovered() { ui.input(|i| i.smooth_scroll_delta.y) } else { 0.0 };

        if let Some(State::Found(lyrics, _)) = &state {
            self.draw(ui, &painter, rect, &resp, lyrics, player, scroll);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(&mut self, ui: &egui::Ui, painter: &egui::Painter, rect: Rect, resp: &egui::Response, lyrics: &Lyrics, player: &Player, scroll: f32) {
        let font = FontId::proportional(self.settings.font_size);
        let wrap = rect.width() - 24.0;
        let gap = self.settings.font_size * 0.45;
        let visuals = ui.visuals();
        let lines = lyrics.lines();
        // Lay every line out to find where each one sits.
        let galleys: Vec<_> = lines
            .iter()
            .map(|l| {
                let text = if l.is_empty() { "♪".to_string() } else { l.to_string() };
                let mut job = egui::text::LayoutJob::simple(text, font.clone(), Color32::WHITE, wrap);
                // Wrapped lines are centered too; the galley's x = 0 is then its middle.
                job.halign = egui::Align::Center;
                painter.layout_job(job)
            })
            .collect();
        let mut tops = Vec::with_capacity(galleys.len());
        let mut y = 0.0;
        for g in &galleys {
            tops.push(y);
            y += g.size().y + gap;
        }
        let total = y;

        let (current, follow_to) = match lyrics {
            Lyrics::Synced(timed) => {
                let t = player.position() + self.offset();
                let cur = timed.partition_point(|(s, _)| *s <= t).checked_sub(1);
                // Keep the current line a little above the middle.
                let target = cur.map_or(0.0, |i| tops[i] + galleys[i].size().y / 2.0 - rect.height() * 0.42);
                (cur, Some(target))
            }
            Lyrics::Plain(_) => (None, None),
        };

        // The wheel scrolls by hand; timed lyrics follow the song again after a while.
        let max_off = (total - rect.height() * 0.5).max(0.0);
        if scroll != 0.0 {
            let base = self.manual.map(|(o, _)| o).unwrap_or_else(|| {
                ui.ctx().memory(|m| m.data.get_temp::<f32>(resp.id.with("off"))).unwrap_or(0.0)
            });
            self.manual = Some(((base - scroll).clamp(-rect.height() * 0.4, max_off), Instant::now()));
        }
        if follow_to.is_some() && self.manual.is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4)) {
            self.manual = None;
        }
        let off = match (self.manual, follow_to) {
            (Some((o, _)), _) => o,
            (None, Some(target)) => ui.ctx().animate_value_with_time(resp.id.with("follow"), target, 0.3),
            (None, None) => 0.0,
        };
        ui.ctx().memory_mut(|m| m.data.insert_temp(resp.id.with("off"), off));

        let clicked = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let strong = visuals.strong_text_color();
        let accent = visuals.selection.bg_fill.lerp_to_gamma(Color32::WHITE, 0.35);
        for (i, g) in galleys.into_iter().enumerate() {
            let top = rect.top() + 8.0 + tops[i] - off;
            let height = g.size().y;
            if top + height < rect.top() || top > rect.bottom() {
                continue;
            }
            let color = match current {
                Some(c) if c == i => accent,
                Some(c) if i < c => visuals.weak_text_color(),
                Some(_) => visuals.text_color(),
                None => strong,
            };
            let x = rect.center().x;
            if let (Some(p), Lyrics::Synced(timed)) = (clicked, lyrics)
                && p.y >= top
                && p.y < top + height + gap
            {
                // Jump to the clicked line.
                player.seek((timed[i].0 - self.offset()).max(0.0));
                self.manual = None;
            }
            painter.galley_with_override_text_color(pos2(x, top), g, color);
        }
        if current.is_some() && player.is_playing() || self.manual.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        if lines.is_empty() {
            painter.text(rect.center(), Align2::CENTER_CENTER, "(empty)", font, visuals.weak_text_color());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc() {
        let text = "[ti:Song]\n[ar:Someone]\n[offset:500]\n[00:01.00]first\n[00:03.50][01:00.00]twice\n\n[00:02.00]<00:02.00>wo<00:02.50>rds\nno stamp\n[00:04:20]colon form";
        let l = parse_lrc(text).unwrap();
        let got: Vec<(f64, &str)> = l.iter().map(|(t, s)| ((t * 100.0).round() / 100.0, s.as_str())).collect();
        assert_eq!(got, [(0.5, "first"), (1.5, "words"), (3.0, "twice"), (3.7, "colon form"), (59.5, "twice")]);
        assert!(parse_lrc("just\nwords").is_none());
        assert_eq!(Lyrics::from_text("  la la\r\nla  "), Some(Lyrics::Plain("la la\nla".into())));
        assert!(Lyrics::from_text("   ").is_none());
    }

    #[test]
    fn matches_search_results() {
        let t = Track { artist: Some("Queen".into()), duration: Some(355.0), ..Default::default() };
        let q = |name: &str, artist: &str, secs: f64| match_score(&t, "Bohemian Rhapsody", name, &[artist.to_string()], Some(secs));
        assert!(q("Bohemian Rhapsody (Remastered 2011)", "Queen", 355.9) > q("Bohemian Rhapsody", "Someone", 355.0));
        assert!(q("Bohemian Rhapsody", "Queen", 355.0) > q("Bohemian Rhapsody", "Queen", 240.0));
        assert_eq!(q("Another One Bites the Dust", "Queen", 355.0), None);
        assert_eq!(norm("夜に駆ける (Yoru ni Kakeru)"), "夜に駆ける");
    }

    #[test]
    fn strips_credit_lines() {
        let lrc = "[ti:x]\n[00:00.00]Bohemian Rhapsody (Remastered 2011) - Queen\n[00:00.09]Lyrics by：Freddie Mercury\n[00:00.10] 作词 : Freddie Mercury\n[00:00.20] 编曲 : Queen\n[00:00.30]\n[00:00.37]Is this the real life\n[00:05.00]Mama: just killed a man";
        let out = strip_credits(lrc, "Bohemian Rhapsody");
        let lines: Vec<String> = parse_lrc(&out).unwrap().into_iter().map(|(_, s)| s).collect();
        assert_eq!(lines, ["", "Is this the real life", "Mama: just killed a man"]);
    }

    /// Asks the real services; run with `cargo test -- --ignored online`.
    #[test]
    #[ignore]
    fn online_services_answer() {
        let t = Track { title: Some("Bohemian Rhapsody".into()), artist: Some("Queen".into()), duration: Some(355.0), ..Default::default() };
        for p in Provider::ALL {
            let Some(service) = p.service() else { continue };
            let name = p.label();
            let text = service(&t, "Bohemian Rhapsody").unwrap().unwrap_or_else(|| panic!("{name}: nothing"));
            let l = parse_lrc(&text).unwrap_or_else(|| panic!("{name}: not timed"));
            assert!(l.iter().any(|(_, s)| s.contains("real life")), "{name}: {:?}", &l[..3]);
            assert!(!l[0].1.contains("Freddie"), "{name}: credits left: {:?}", &l[..3]);
        }
        let jp = Track { title: Some("夜に駆ける".into()), artist: Some("YOASOBI".into()), duration: Some(261.0), ..Default::default() };
        assert!(netease(&jp, "夜に駆ける").unwrap().is_some());
        assert!(lrclib(&jp, "夜に駆ける").unwrap().is_some());
    }

    #[test]
    fn finds_sidecar_lrc_first() {
        let dir = std::env::temp_dir().join(format!("np-lyrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.flac");
        std::fs::write(dir.join("song.lrc"), "[00:00.10]from file").unwrap();
        let t = Track { path: song.clone(), embedded: Some("embedded words".into()), ..Default::default() };
        match lookup(&t, None, false, false) {
            State::Found(Lyrics::Synced(l), src) => assert_eq!((l[0].1.as_str(), src.as_str()), ("from file", "song.lrc")),
            s => panic!("{s:?}"),
        }
        // Picking a provider skips the ones before it.
        match lookup(&t, Some(Provider::Embedded), false, false) {
            State::Found(Lyrics::Plain(p), src) => assert_eq!((p.as_str(), src.as_str()), ("embedded words", "embedded")),
            s => panic!("{s:?}"),
        }
        match lookup(&t, Some(Provider::NetEase), false, false) {
            State::Missing(why) => assert_eq!(why, "online is off"),
            s => panic!("{s:?}"),
        }
        std::fs::remove_file(dir.join("song.lrc")).unwrap();
        match lookup(&t, None, false, false) {
            State::Found(Lyrics::Plain(p), src) => assert_eq!((p.as_str(), src.as_str()), ("embedded words", "embedded")),
            s => panic!("{s:?}"),
        }
        match lookup(&t, Some(Provider::File), false, false) {
            State::Missing(why) => assert_eq!(why, "no .lrc or .txt next to the track"),
            s => panic!("{s:?}"),
        }
        let none = Track { path: song, ..Default::default() };
        assert!(matches!(lookup(&none, None, false, false), State::Missing(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_provider_and_offset_per_track() {
        let (a, b) = (Path::new("/music/a.flac"), Path::new("/music/b.flac"));
        let mut store = LyricsStore::default();
        store.set(a, TrackLyrics { provider: Some(Provider::Lrclib), offset: 0.5 });
        assert_eq!(store.get(a), TrackLyrics { provider: Some(Provider::Lrclib), offset: 0.5 });
        assert_eq!(store.get(b), TrackLyrics::default());
        let revision = store.revision;
        store.set(a, store.get(a));
        assert_eq!(store.revision, revision, "setting the same doesn't count as a change");

        let json = serde_json::to_string(&store).unwrap();
        assert!(json.contains("\"lrclib\""), "{json}");
        let back: LyricsStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get(a), store.get(a));

        // Back at the defaults, the track goes.
        store.set(a, TrackLyrics::default());
        assert!(store.tracks.is_empty());
        // Settings saved by earlier versions (with a global offset) still load.
        let old: LyricsSettings = serde_json::from_str(r#"{"online":false,"offset":1.5,"font_size":20.0}"#).unwrap();
        assert_eq!(old, LyricsSettings { online: false, font_size: 20.0 });
    }
}
