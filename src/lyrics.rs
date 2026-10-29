//! Lyrics node: shows the current track's lyrics, following the playback position when they
//! are timed (LRC).
//!
//! Where they come from, first found wins:
//! 1. an `.lrc` (or `.txt`) file next to the track with the same name,
//! 2. lyrics embedded in the file's tags,
//! 3. online, when "online" is on: NetEase Cloud Music (music.163.com), then Kugou
//!    (krcs.kugou.com), searched by title and artist and matched by name and length. Both are
//!    the services' unofficial web APIs, needing no key. Answers (found or not) are cached in
//!    `~/.cache/nodeplayer/lyrics/`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::player::Player;

/// The services answer browsers; an unknown client may get nothing.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
/// How long a "not found" answer is trusted before asking again.
const RETRY_MISSING: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricsSettings {
    /// Look the lyrics up online when there are none locally.
    pub online: bool,
    /// Added to the playback position, in seconds; positive shows lines earlier.
    pub offset: f32,
    pub font_size: f32,
    pub size: [f32; 2],
}

impl Default for LyricsSettings {
    fn default() -> Self {
        Self { online: true, offset: 0.0, font_size: 16.0, size: [360.0, 300.0] }
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
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("nodeplayer/lyrics"))
}

/// FNV-1a, stable across builds (the cache file name).
fn key_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[derive(Serialize, Deserialize)]
struct Cached {
    /// The service that had them; empty when none did.
    source: String,
    lyrics: Option<String>,
    /// Seconds since the epoch of the lookup.
    when: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(12))).http_status_as_error(false).build().into()
}

/// GETs `url` and parses the JSON answer.
fn get_json(agent: &ureq::Agent, url: &str, query: &[(&str, String)], referer: Option<&str>) -> Result<serde_json::Value, String> {
    let mut req = agent.get(url).header("User-Agent", USER_AGENT);
    if let Some(r) = referer {
        req = req.header("Referer", r);
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
    let found = get_json(&agent, "https://music.163.com/api/cloudsearch/pc", &[("s", query), ("type", "1".into()), ("limit", "10".into())], Some(REFERER))?;
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
        let l = get_json(&agent, "https://music.163.com/api/song/lyric", &[("id", id.to_string()), ("lv", "1".into()), ("tv", "-1".into())], Some(REFERER))?;
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
        None,
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
            None,
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

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// An online service: the track and the title to search for → LRC text, if it has any.
type Service = fn(&Track, &str) -> Result<Option<String>, String>;

/// The online services, asked in this order.
const SERVICES: [(&str, Service); 2] = [("NetEase", netease), ("Kugou", kugou)];

fn lookup(t: &Track, online: bool, refresh: bool) -> State {
    if let Some((text, p)) = sidecar(&t.path)
        && let Some(l) = Lyrics::from_text(&text)
    {
        return State::Found(l, p.file_name().unwrap_or_default().to_string_lossy().into_owned());
    }
    if let Some(l) = t.embedded.as_deref().and_then(Lyrics::from_text) {
        return State::Found(l, "embedded".into());
    }
    if !online {
        return State::Missing("no lyrics file or tag".into());
    }
    let title = t.title.clone().unwrap_or_else(|| t.path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    let key = format!("{}\n{}\n{}", title.to_lowercase(), t.artist.as_deref().unwrap_or("").to_lowercase(), t.album.as_deref().unwrap_or("").to_lowercase());
    let file = cache_dir().map(|d| d.join(format!("{:016x}.json", key_hash(&key))));
    if !refresh
        && let Some(c) = file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice::<Cached>(&b).ok())
    {
        match c.lyrics.as_deref().and_then(Lyrics::from_text) {
            Some(l) => return State::Found(l, format!("{} (cached)", c.source)),
            None if now_secs().saturating_sub(c.when) < RETRY_MISSING.as_secs() => {
                return State::Missing("not found online".into());
            }
            None => {}
        }
    }
    let mut errors = Vec::new();
    let mut found = None;
    for (name, service) in SERVICES {
        match service(t, &title) {
            Ok(Some(text)) if Lyrics::from_text(&text).is_some() => {
                found = Some((name, text));
                break;
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("lyrics lookup for {title} on {name}: {e}");
                errors.push(format!("{name}: {e}"));
            }
        }
    }
    // Don't remember "not found" when a service couldn't be asked.
    if found.is_some() || errors.is_empty() {
        let c = Cached { source: found.as_ref().map_or(String::new(), |f| f.0.to_string()), lyrics: found.as_ref().map(|f| f.1.clone()), when: now_secs() };
        if let Some(f) = &file {
            let _ = std::fs::create_dir_all(f.parent().unwrap());
            let _ = std::fs::write(f, serde_json::to_vec(&c).unwrap_or_default());
        }
    }
    match found {
        Some((name, text)) => State::Found(Lyrics::from_text(&text).unwrap(), name.to_string()),
        None if errors.is_empty() => State::Missing("not found online".into()),
        None => State::Missing(errors.join(", ")),
    }
}

pub struct LyricsNode {
    pub settings: LyricsSettings,
    track: Option<PathBuf>,
    state: Arc<Mutex<Option<State>>>,
    /// The lookup in flight, so an old one finishing late is ignored.
    generation: Arc<Mutex<u64>>,
    /// Manual scrolling: offset in points and when it happened; following the song resumes
    /// a few seconds later.
    manual: Option<(f32, Instant)>,
}

impl LyricsNode {
    pub fn new(settings: LyricsSettings) -> Self {
        Self { settings, track: None, state: Arc::default(), generation: Arc::default(), manual: None }
    }

    fn start_lookup(&mut self, player: &Player, refresh: bool) {
        let status = player.status();
        let Some(path) = status.path.clone() else {
            *self.state.lock().unwrap() = None;
            return;
        };
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
            let found = lookup(&track, online, refresh);
            if *gen_now.lock().unwrap() == generation {
                *state.lock().unwrap() = Some(found);
            }
        });
    }

    /// The body. `scroll` is the wheel movement the graph view handed over; the return value
    /// says whether the pointer is over the lyrics (so the next wheel movement comes here).
    pub fn ui(&mut self, ui: &mut egui::Ui, player: &Player, scroll: f32) -> bool {
        let path = player.status().path;
        if path != self.track {
            self.track = path;
            self.start_lookup(player, false);
        }
        let state = self.state.lock().unwrap().clone();

        ui.horizontal(|ui| {
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
            if ui.checkbox(&mut self.settings.online, "online").on_hover_text("look up NetEase, then Kugou (sends title, artist and length to music.163.com and kugou.com)").changed()
                && self.settings.online
                && matches!(state, Some(State::Missing(_)))
            {
                self.start_lookup(player, false);
            }
        });
        let synced = matches!(state, Some(State::Found(Lyrics::Synced(_), _)));
        if synced {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.settings.offset).speed(0.05).range(-30.0..=30.0).fixed_decimals(2).prefix("offset ").suffix(" s"))
                    .on_hover_text("positive shows lines earlier");
                ui.add(egui::DragValue::new(&mut self.settings.font_size).speed(0.2).range(9.0..=48.0).fixed_decimals(0).prefix("size "));
            });
        } else {
            ui.add(egui::DragValue::new(&mut self.settings.font_size).speed(0.2).range(9.0..=48.0).fixed_decimals(0).prefix("size "));
        }

        let [w, h] = self.settings.size;
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        let hovered = resp.hovered();

        if let Some(State::Found(lyrics, _)) = &state {
            self.draw(ui, &painter, rect, &resp, lyrics, player, if hovered { scroll } else { 0.0 });
        }

        // Resize from the corner, like the visualizers.
        let handle = Rect::from_min_max(rect.max - vec2(12.0, 12.0), rect.max);
        let hr = ui.interact(handle, ui.id().with("lyrics-resize"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
        let c = if hr.hovered() || hr.dragged() { Color32::from_gray(220) } else { Color32::from_gray(120) };
        for k in [4.0, 8.0] {
            painter.line_segment([pos2(handle.max.x - k, handle.max.y - 1.0), pos2(handle.max.x - 1.0, handle.max.y - k)], Stroke::new(1.0, c));
        }
        if hr.dragged() {
            let d = hr.drag_delta();
            self.settings.size = [(w + d.x).clamp(160.0, 1600.0), (h + d.y).clamp(80.0, 1600.0)];
        }
        hovered
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
                let t = player.position() + self.settings.offset as f64;
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
                player.seek((timed[i].0 - self.settings.offset as f64).max(0.0));
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
        for (name, service) in SERVICES {
            let text = service(&t, "Bohemian Rhapsody").unwrap().unwrap_or_else(|| panic!("{name}: nothing"));
            let l = parse_lrc(&text).unwrap_or_else(|| panic!("{name}: not timed"));
            assert!(l.iter().any(|(_, s)| s.contains("real life")), "{name}: {:?}", &l[..3]);
            assert!(!l[0].1.contains("Freddie"), "{name}: credits left: {:?}", &l[..3]);
        }
        let jp = Track { title: Some("夜に駆ける".into()), artist: Some("YOASOBI".into()), duration: Some(261.0), ..Default::default() };
        assert!(netease(&jp, "夜に駆ける").unwrap().is_some());
    }

    #[test]
    fn finds_sidecar_lrc_first() {
        let dir = std::env::temp_dir().join(format!("np-lyrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.flac");
        std::fs::write(dir.join("song.lrc"), "[00:00.10]from file").unwrap();
        let t = Track { path: song.clone(), embedded: Some("embedded words".into()), ..Default::default() };
        match lookup(&t, false, false) {
            State::Found(Lyrics::Synced(l), src) => assert_eq!((l[0].1.as_str(), src.as_str()), ("from file", "song.lrc")),
            s => panic!("{s:?}"),
        }
        std::fs::remove_file(dir.join("song.lrc")).unwrap();
        match lookup(&t, false, false) {
            State::Found(Lyrics::Plain(p), src) => assert_eq!((p.as_str(), src.as_str()), ("embedded words", "embedded")),
            s => panic!("{s:?}"),
        }
        let none = Track { path: song, ..Default::default() };
        assert!(matches!(lookup(&none, false, false), State::Missing(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
