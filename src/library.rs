//! The track list: scans folders (with their subfolders) and single files for audio, video
//! and MIDI files in the background, reads their tags for the table (cached in
//! `~/.cache/nodeplayer/library-tags-2.json`), counts plays (in
//! `~/.local/state/nodeplayer/plays.json`), and keeps the play order (sorting, next/previous,
//! repeat, shuffle).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;

pub const AUDIO_EXTS: &[&str] =
    &["flac", "mp3", "wav", "ogg", "opus", "m4a", "aac", "aiff", "aif", "caf", "mkv", "webm"];

pub fn is_media(path: &Path) -> bool {
    crate::av::is_video_ext(path)
        || crate::midi_song::is_midi_file(path)
        || path.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub struct Track {
    pub path: PathBuf,
    /// Heading the track is listed under: the source folder's name and the subfolder, or
    /// "Tracks" for files added one by one.
    pub dir: String,
    pub name: String,
    /// Tags and length, once read.
    pub meta: Option<TrackMeta>,
    /// Position in the scan's order (sources, then folders and names in natural order).
    order: usize,
    /// File size and modification time, to tell whether cached tags still hold.
    stamp: Stamp,
    /// When the file was created (else last modified), in Unix seconds.
    pub added: i64,
}

impl Track {
    /// Title tag, else the file name without its extension.
    pub fn title(&self) -> String {
        self.meta.as_ref().and_then(|m| m.title.clone()).unwrap_or_else(|| {
            Path::new(&self.name).file_stem().map_or(self.name.clone(), |s| s.to_string_lossy().into_owned())
        })
    }
}

/// What the track table shows about a file.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub duration: Option<f64>,
    /// Release date as tagged: "2021", "2021-04-07", ...
    pub date: Option<String>,
}

/// MIDI files up to this size are read through for their length; black MIDI can be
/// gigabytes.
const MIDI_LENGTH_MAX_BYTES: u64 = 16 << 20;

impl TrackMeta {
    fn read(path: &Path) -> Self {
        if crate::midi_song::is_midi_file(path) {
            let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= MIDI_LENGTH_MAX_BYTES);
            let duration = small
                .then(|| crate::midifile::load(path, &Default::default()).ok())
                .flatten()
                .map(|f| f.info.duration);
            return Self { duration, ..Default::default() };
        }
        let Ok(info) = crate::av::track_info(path) else { return Self::default() };
        let get = |k: &str| info.meta.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u32>().ok());
        Self {
            title: get("title"),
            artist: get("artist").or_else(|| get("album artist")),
            album: get("album"),
            track: num("tracknumber"),
            disc: num("discnumber"),
            duration: info.duration,
            date: get("date").or_else(|| get("originaldate")).or_else(|| get("releasedate")),
        }
    }
}

/// Where tracks come from: a folder (scanned with its subfolders) or one file.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Source {
    Folder(PathBuf),
    File(PathBuf),
}

impl Source {
    pub fn path(&self) -> &Path {
        match self {
            Self::Folder(p) | Self::File(p) => p,
        }
    }
}

/// Columns of the track table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Column {
    Track,
    Title,
    Artist,
    Album,
    Length,
    Released,
    Added,
    Plays,
}

impl Column {
    pub const ALL: [Self; 8] =
        [Self::Track, Self::Title, Self::Artist, Self::Album, Self::Length, Self::Released, Self::Added, Self::Plays];

    pub fn label(self) -> &'static str {
        match self {
            Self::Track => "#",
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Length => "Length",
            Self::Released => "Released",
            Self::Added => "Added",
            Self::Plays => "Plays",
        }
    }
}

/// The track table's column widths and which columns are shown, in [`Column::ALL`] order.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(from = "SavedColumns")]
pub struct Columns {
    pub widths: [f32; 8],
    pub shown: [bool; 8],
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            widths: [28.0, 160.0, 110.0, 110.0, 60.0, 82.0, 82.0, 56.0],
            shown: [true, true, true, true, true, false, false, false],
        }
    }
}

/// Columns as saved, which may be from a version with fewer of them.
#[derive(serde::Deserialize)]
#[serde(default)]
struct SavedColumns {
    widths: Vec<f32>,
    shown: Vec<bool>,
}

impl Default for SavedColumns {
    fn default() -> Self {
        let d = Columns::default();
        Self { widths: d.widths.to_vec(), shown: d.shown.to_vec() }
    }
}

impl From<SavedColumns> for Columns {
    fn from(s: SavedColumns) -> Self {
        let mut c = Self::default();
        for (to, from) in c.widths.iter_mut().zip(s.widths) {
            *to = from;
        }
        for (to, from) in c.shown.iter_mut().zip(s.shown) {
            *to = from;
        }
        c
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Repeat {
    Off,
    All,
    One,
    /// The selection loop: the selection of the track over and over, or the whole track
    /// without a gap.
    Selection,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Selection,
            Self::Selection => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "🔁 off",
            Self::All => "🔁 all",
            Self::One => "🔂 one",
            Self::Selection => "🔂 selection",
        }
    }
}

/// File size and modification time.
type Stamp = (u64, i64);
/// Cached tags: path → (stamp, tags).
type Cache = HashMap<PathBuf, (Stamp, TrackMeta)>;

pub struct Library {
    pub sources: Vec<Source>,
    pub tracks: Vec<Track>,
    pub current: Option<usize>,
    pub filter: String,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// The table's order: a column and whether it's descending; `None` is the scan's order.
    pub sort: Option<(Column, bool)>,
    pub columns: Columns,
    /// Changes whenever the list or its order does.
    pub revision: u64,
    /// Tracks played in shuffle mode, so "previous" can walk back.
    history: Vec<usize>,
    scan_rx: Option<Receiver<(u64, Vec<Track>)>>,
    /// Bumped by every scan, so an older scan's result is dropped.
    generation: u64,
    meta_rx: Option<Receiver<(PathBuf, Stamp, TrackMeta)>>,
    /// Tags read so far this run plus those from the cache file; `None` until first needed.
    cache: Option<Cache>,
    cache_dirty: bool,
    /// How often and when each file was played; `None` until first needed.
    plays: Option<HashMap<PathBuf, PlayStats>>,
    rng: u64,
}

impl Default for Library {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self {
            sources: Vec::new(),
            tracks: Vec::new(),
            current: None,
            filter: String::new(),
            repeat: Repeat::Off,
            shuffle: false,
            sort: None,
            columns: Columns::default(),
            revision: 0,
            history: Vec::new(),
            scan_rx: None,
            generation: 0,
            meta_rx: None,
            cache: None,
            cache_dirty: false,
            plays: None,
            rng: seed | 1,
        }
    }
}

impl Library {
    pub fn scanning(&self) -> bool {
        self.scan_rx.is_some()
    }

    /// Tags are still being read.
    pub fn reading_tags(&self) -> bool {
        self.meta_rx.is_some()
    }

    /// The first folder, for what only knows one (extensions, the last-folder file).
    pub fn root(&self) -> Option<&Path> {
        self.sources.iter().find_map(|s| match s {
            Source::Folder(p) => Some(p.as_path()),
            Source::File(_) => None,
        })
    }

    /// Makes `root` the only source.
    pub fn open(&mut self, root: PathBuf) {
        self.set_sources(vec![Source::Folder(root)]);
    }

    pub fn set_sources(&mut self, sources: Vec<Source>) {
        self.sources = sources;
        self.rescan();
    }

    /// Adds a folder or file unless it's already there, and scans again.
    pub fn add(&mut self, source: Source) {
        if !self.sources.contains(&source) {
            self.sources.push(source);
            self.rescan();
        }
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.sources.len() {
            self.sources.remove(i);
            self.rescan();
        }
    }

    /// Scans every source in the background; the list is replaced when it finishes.
    pub fn rescan(&mut self) {
        self.generation += 1;
        let (generation, sources) = (self.generation, self.sources.clone());
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.scan_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send((generation, scan(&sources)));
        });
    }

    /// Picks up a finished scan and tags read since the last call. `playing` is the file
    /// currently loaded, so it stays selected. True when the list was replaced.
    pub fn poll(&mut self, playing: Option<&Path>) -> bool {
        self.poll_meta();
        let Some(rx) = &self.scan_rx else { return false };
        let Ok((generation, mut tracks)) = rx.try_recv() else { return false };
        self.scan_rx = None;
        if generation != self.generation {
            return false;
        }
        let cache = self.cache.get_or_insert_with(load_cache);
        let mut missing = Vec::new();
        for t in &mut tracks {
            match cache.get(&t.path) {
                Some((stamp, meta)) if *stamp == t.stamp => t.meta = Some(meta.clone()),
                _ => missing.push((t.path.clone(), t.stamp)),
            }
        }
        self.tracks = tracks;
        self.history.clear();
        self.apply_sort(playing);
        if let Some(root) = self.root() {
            save_last_folder(root);
        }
        self.read_tags(missing);
        true
    }

    /// Reads tags of `files` on a few threads; results come in through `poll`.
    fn read_tags(&mut self, files: Vec<(PathBuf, Stamp)>) {
        if files.is_empty() {
            self.meta_rx = None;
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let (jobs_tx, jobs) = crossbeam_channel::unbounded();
        for f in files {
            let _ = jobs_tx.send(f);
        }
        drop(jobs_tx);
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
        for _ in 0..threads {
            let (jobs, tx) = (jobs.clone(), tx.clone());
            std::thread::spawn(move || {
                for (path, stamp) in jobs {
                    let meta = TrackMeta::read(&path);
                    if tx.send((path, stamp, meta)).is_err() {
                        break;
                    }
                }
            });
        }
        self.meta_rx = Some(rx);
    }

    fn poll_meta(&mut self) {
        let Some(rx) = &self.meta_rx else { return };
        let mut got = Vec::new();
        let done = loop {
            match rx.try_recv() {
                Ok(m) => got.push(m),
                Err(crossbeam_channel::TryRecvError::Empty) => break false,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break true,
            }
        };
        if !got.is_empty() {
            let index: HashMap<&Path, usize> = self.tracks.iter().enumerate().map(|(i, t)| (t.path.as_path(), i)).collect();
            let hits: Vec<(usize, TrackMeta)> =
                got.iter().filter_map(|(p, _, m)| Some((*index.get(p.as_path())?, m.clone()))).collect();
            for (i, m) in hits {
                self.tracks[i].meta = Some(m);
            }
            let cache = self.cache.get_or_insert_with(load_cache);
            for (p, stamp, m) in got {
                cache.insert(p, (stamp, m));
            }
            self.cache_dirty = true;
        }
        if done {
            self.meta_rx = None;
            // A column sort may depend on the tags just read.
            if self.sort.is_some() {
                let playing = self.current.map(|i| self.tracks[i].path.clone());
                self.apply_sort(playing.as_deref());
            }
            self.save_cache();
        }
    }

    fn save_cache(&mut self) {
        if !std::mem::take(&mut self.cache_dirty) {
            return;
        }
        let (Some(cache), Some(path)) = (&self.cache, cache_path()) else { return };
        // Only what's still listed, so removed folders don't keep growing the file.
        let listed: HashSet<&Path> = self.tracks.iter().map(|t| t.path.as_path()).collect();
        let keep: Vec<(&PathBuf, &(Stamp, TrackMeta))> = cache.iter().filter(|(p, _)| listed.contains(p.as_path())).collect();
        let write = || -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&keep)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write() {
            log::warn!("can't save the tag cache: {e:#}");
        }
    }

    /// Times `path` was played, once [`Self::load_plays`] has run.
    pub fn plays(&self, path: &Path) -> u32 {
        self.play_stats(path).count
    }

    pub fn play_stats(&self, path: &Path) -> PlayStats {
        self.plays.as_ref().and_then(|p| p.get(path)).copied().unwrap_or_default()
    }

    pub fn load_plays(&mut self) {
        self.plays.get_or_insert_with(load_plays);
    }

    /// Counts a play of `path`. The list isn't re-sorted for it, so the play order stays put.
    pub fn add_play(&mut self, path: &Path) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let plays = self.plays.get_or_insert_with(load_plays);
        let s = plays.entry(path.to_path_buf()).or_default();
        s.count += 1;
        if s.first == 0 {
            s.first = now;
        }
        s.last = now;
        save_plays(plays);
    }

    /// Sorts the list by a column (`None`: the scan's order). The play order follows.
    pub fn set_sort(&mut self, sort: Option<(Column, bool)>) {
        self.sort = sort;
        let playing = self.current.map(|i| self.tracks[i].path.clone());
        self.apply_sort(playing.as_deref());
    }

    fn apply_sort(&mut self, playing: Option<&Path>) {
        let text = |t: &Track, f: fn(&TrackMeta) -> &Option<String>| t.meta.as_ref().and_then(|m| f(m).clone()).unwrap_or_default();
        let num = |t: &Track, f: fn(&TrackMeta) -> Option<u32>| t.meta.as_ref().and_then(f).unwrap_or(0);
        let by_album = |a: &Track, b: &Track| {
            natural_cmp(&text(a, |m| &m.album), &text(b, |m| &m.album))
                .then(num(a, |m| m.disc).cmp(&num(b, |m| m.disc)))
                .then(num(a, |m| m.track).cmp(&num(b, |m| m.track)))
        };
        let plays = match self.sort {
            Some((Column::Plays, _)) => {
                self.plays.get_or_insert_with(load_plays).iter().map(|(p, s)| (p.clone(), s.count)).collect()
            }
            _ => HashMap::new(),
        };
        let played = |t: &Track| plays.get(&t.path).copied().unwrap_or(0);
        let released = |t: &Track| t.meta.as_ref().and_then(|m| m.date.as_deref()).map(date_key).unwrap_or_default();
        match self.sort {
            None => self.tracks.sort_by_key(|t| t.order),
            Some((col, desc)) => {
                self.tracks.sort_by(|a, b| {
                    let ord = match col {
                        Column::Track => num(a, |m| m.track).cmp(&num(b, |m| m.track)),
                        Column::Title => natural_cmp(&a.title(), &b.title()),
                        Column::Artist => natural_cmp(&text(a, |m| &m.artist), &text(b, |m| &m.artist)).then_with(|| by_album(a, b)),
                        Column::Album => by_album(a, b),
                        Column::Length => {
                            let d = |t: &Track| t.meta.as_ref().and_then(|m| m.duration).unwrap_or(0.0);
                            d(a).total_cmp(&d(b))
                        }
                        Column::Released => released(a).cmp(&released(b)).then_with(|| by_album(a, b)),
                        Column::Added => a.added.cmp(&b.added),
                        Column::Plays => played(a).cmp(&played(b)),
                    };
                    let ord = if desc { ord.reverse() } else { ord };
                    ord.then(a.order.cmp(&b.order))
                });
            }
        }
        self.history.clear();
        self.current = playing.and_then(|p| self.index_of(p));
        self.revision += 1;
    }

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.tracks.iter().position(|t| t.path == path)
    }

    pub fn set_current(&mut self, i: usize) {
        if self.shuffle && self.current.is_some_and(|c| c != i) {
            self.history.extend(self.current);
        }
        self.current = Some(i);
    }

    /// The track to play after the current one, honoring repeat and shuffle. `finished` is true
    /// when the current track ended by itself (repeat-one only applies then).
    pub fn next(&mut self, finished: bool) -> Option<usize> {
        let n = self.tracks.len();
        if n == 0 {
            return None;
        }
        let Some(cur) = self.current else { return Some(0) };
        if finished && matches!(self.repeat, Repeat::One | Repeat::Selection) {
            return Some(cur);
        }
        if self.shuffle && n > 1 {
            let r = (self.rand() % (n as u64 - 1)) as usize;
            return Some(if r >= cur { r + 1 } else { r });
        }
        if cur + 1 < n {
            Some(cur + 1)
        } else if self.repeat != Repeat::Off {
            Some(0)
        } else {
            None
        }
    }

    pub fn previous(&mut self) -> Option<usize> {
        if self.shuffle
            && let Some(i) = self.history.pop()
        {
            // set_current would push the track we're leaving; going back shouldn't.
            self.current = None;
            return Some(i);
        }
        let n = self.tracks.len();
        match self.current {
            Some(0) if self.repeat != Repeat::Off && n > 0 => Some(n - 1),
            Some(c) => Some(c.saturating_sub(1)),
            None => (n > 0).then_some(0),
        }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }
}

/// The file's stamp and when it was added.
fn stamp(path: &Path) -> (Stamp, i64) {
    let Ok(m) = std::fs::metadata(path) else { return ((0, 0), 0) };
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64)
    };
    let mtime = secs(m.modified()).unwrap_or(0);
    ((m.len(), mtime), secs(m.created()).unwrap_or(mtime))
}

/// A release date as (year, month, day) for sorting; parts that are missing are 0. Takes
/// "2021", "2021-04-07", "2021/4/7", "2021.04" and the like.
fn date_key(date: &str) -> (u32, u32, u32) {
    let mut parts = date.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn scan(sources: &[Source]) -> Vec<Track> {
    let mut tracks = Vec::new();
    // Single files go together under one heading, after the folders.
    let mut singles = Vec::new();
    let mut listed = HashSet::new();
    for source in sources {
        let found: Vec<Track> = match source {
            Source::Folder(root) => {
                let mut files = Vec::new();
                walk(root, &mut files, &mut HashSet::new());
                let top = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
                files
                    .into_iter()
                    .map(|path| {
                        let rel = path.strip_prefix(root).unwrap_or(&path);
                        let sub = rel.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                        // Only name the source folder when there are several.
                        let dir = match (sources.len() > 1, sub.is_empty()) {
                            (false, _) => sub,
                            (true, true) => top.clone(),
                            (true, false) => format!("{top}/{sub}"),
                        };
                        let name = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        let (stamp, added) = stamp(&path);
                        Track { stamp, added, path, dir, name, meta: None, order: 0 }
                    })
                    .collect()
            }
            Source::File(path) if path.is_file() => {
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let (stamp, added) = stamp(path);
                vec![Track { stamp, added, path: path.clone(), dir: "Tracks".into(), name, meta: None, order: 0 }]
            }
            Source::File(_) => Vec::new(),
        };
        let found = found.into_iter().filter(|t| listed.insert(t.path.clone()));
        match source {
            Source::Folder(_) => {
                let mut found: Vec<Track> = found.collect();
                found.sort_by(|a, b| natural_cmp(&a.dir, &b.dir).then_with(|| natural_cmp(&a.name, &b.name)));
                tracks.extend(found);
            }
            Source::File(_) => singles.extend(found),
        }
    }
    tracks.extend(singles);
    for (i, t) in tracks.iter_mut().enumerate() {
        t.order = i;
    }
    tracks
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    // Symlinked folders are followed, so guard against loops.
    let Ok(canon) = dir.canonicalize() else { return };
    if !seen.insert(canon) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, out, seen);
        } else if is_media(&path) {
            out.push(path);
        }
    }
}

fn cache_path() -> Option<PathBuf> {
    // -2: since release dates are read; the older file is removed so they get read.
    let dir = crate::dirs::cache()?;
    let _ = std::fs::remove_file(dir.join("library-tags.json"));
    Some(dir.join("library-tags-2.json"))
}

fn load_cache() -> Cache {
    let Some(path) = cache_path() else { return Cache::new() };
    let Ok(bytes) = std::fs::read(&path) else { return Cache::new() };
    match serde_json::from_slice::<Vec<(PathBuf, (Stamp, TrackMeta))>>(&bytes) {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            log::warn!("{}: {e}", path.display());
            Cache::new()
        }
    }
}

/// Case-insensitive comparison that orders digit runs by value ("2 x" before "10 x").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().filter(|c| c.is_ascii_digit()) {
                        s.push(*c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn plays_path() -> Option<PathBuf> {
    Some(crate::plugins::guard::state_dir()?.join("plays.json"))
}

/// How often a file was played, and when first and last (Unix seconds; 0: never).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayStats {
    pub count: u32,
    pub first: i64,
    pub last: i64,
}

fn load_plays() -> HashMap<PathBuf, PlayStats> {
    let Some(path) = plays_path() else { return HashMap::new() };
    let Ok(bytes) = std::fs::read(&path) else { return HashMap::new() };
    serde_json::from_slice::<Vec<(PathBuf, PlayStats)>>(&bytes).map(|v| v.into_iter().collect()).unwrap_or_else(|e| {
        log::warn!("{}: {e}", path.display());
        HashMap::new()
    })
}

fn save_plays(plays: &HashMap<PathBuf, PlayStats>) {
    let Some(path) = plays_path() else { return };
    let write = || -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&plays.iter().collect::<Vec<_>>())?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::warn!("can't save play counts: {e:#}");
    }
}

/// Decides when the loaded track counts as played: after half of it, or four minutes,
/// has actually been listened to (seeking doesn't count). Playing it again from the start
/// counts again.
#[derive(Default)]
pub struct PlayCounter {
    path: Option<PathBuf>,
    position: f64,
    listened: f64,
    counted: bool,
}

impl PlayCounter {
    /// Called every tick; true when the track just reached a play.
    pub fn tick(&mut self, path: Option<&Path>, position: f64, duration: Option<f64>) -> bool {
        if self.path.as_deref() != path {
            *self = Self { path: path.map(Path::to_path_buf), position, ..Self::default() };
            return false;
        }
        let step = position - self.position;
        self.position = position;
        if step > 0.0 && step < 1.0 {
            self.listened += step;
        } else if step < 0.0 && position < 1.0 {
            // Back at the start (repeat one, or "previous").
            self.listened = 0.0;
            self.counted = false;
        }
        let need = duration.filter(|d| *d > 0.0).map_or(60.0, |d| (d / 2.0).min(240.0));
        if path.is_some() && !self.counted && self.listened >= need {
            self.counted = true;
            return true;
        }
        false
    }
}

/// When the file was added: created, else last modified (Unix seconds).
pub fn added(path: &Path) -> i64 {
    stamp(path).1
}

/// Unix seconds in local time; `None` for 0 (never).
fn local(secs: i64) -> Option<libc::tm> {
    if secs <= 0 {
        return None;
    }
    // SAFETY: localtime_r and localtime_s only write the struct they're given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        let ok = !libc::localtime_r(&(secs as libc::time_t), &mut tm).is_null();
        #[cfg(windows)]
        let ok = libc::localtime_s(&mut tm, &(secs as libc::time_t)) == 0;
        ok.then_some(tm)
    }
}

/// "2026-10-28"; empty for 0.
pub fn fmt_date(secs: i64) -> String {
    local(secs).map_or_else(String::new, |t| format!("{}-{:02}-{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday))
}

/// "2026-10-28 21:05:09", as foobar2000 shows dates; empty for 0.
pub fn fmt_datetime(secs: i64) -> String {
    local(secs).map_or_else(String::new, |t| {
        format!("{}-{:02}-{:02} {:02}:{:02}:{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
    })
}

fn state_path() -> Option<PathBuf> {
    Some(crate::dirs::config()?.join("last_folder"))
}

pub fn load_last_folder() -> Option<PathBuf> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let p = PathBuf::from(text.trim_end_matches('\n'));
    p.is_dir().then_some(p)
}

fn save_last_folder(root: &Path) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, root.as_os_str().as_encoded_bytes()) {
        log::warn!("can't save last folder: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["10 b", "2 a", "01 c", "Track 1", "track 02"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["01 c", "2 a", "10 b", "Track 1", "track 02"]);
    }

    #[test]
    fn several_sources_and_sorting() {
        let dir = std::env::temp_dir().join(format!("nodeplayer-lib-{}", std::process::id()));
        let (a, b) = (dir.join("A"), dir.join("B"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for f in [a.join("2.flac"), a.join("10.flac"), a.join("sub/1.mp3"), b.join("x.ogg"), b.join("notes.txt"), dir.join("single.wav")] {
            std::fs::write(f, b"").unwrap();
        }
        let sources = vec![Source::File(dir.join("single.wav")), Source::Folder(a.clone()), Source::Folder(b.clone()), Source::Folder(a.clone())];
        let tracks = scan(&sources);
        let names: Vec<(&str, &str)> = tracks.iter().map(|t| (t.dir.as_str(), t.name.as_str())).collect();
        // Folders in order (subfolders after files, numbers by value), the duplicate folder
        // once, single files last.
        assert_eq!(names, [("A", "2.flac"), ("A", "10.flac"), ("A/sub", "1.mp3"), ("B", "x.ogg"), ("Tracks", "single.wav")]);

        let mut lib = Library { tracks, ..Library::default() };
        let meta = |artist: &str, secs: f64| Some(TrackMeta { artist: Some(artist.into()), duration: Some(secs), ..Default::default() });
        lib.tracks[0].meta = meta("b", 30.0);
        lib.tracks[1].meta = meta("a", 10.0);
        lib.tracks[3].meta = meta("c", 20.0);
        lib.current = Some(1);
        lib.set_sort(Some((Column::Length, true)));
        let order: Vec<&str> = lib.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(order, ["2.flac", "x.ogg", "10.flac", "1.mp3", "single.wav"]);
        // The playing track is followed to its new place.
        assert_eq!(lib.current, Some(2));
        lib.set_sort(Some((Column::Artist, false)));
        assert_eq!(lib.tracks[2].name, "10.flac");
        lib.tracks.iter_mut().find(|t| t.name == "x.ogg").unwrap().meta =
            Some(TrackMeta { date: Some("2019-04".into()), ..Default::default() });
        lib.tracks.iter_mut().find(|t| t.name == "1.mp3").unwrap().meta =
            Some(TrackMeta { date: Some("2019/3/30".into()), ..Default::default() });
        lib.set_sort(Some((Column::Released, true)));
        assert_eq!(lib.tracks[0].name, "x.ogg");
        assert_eq!(lib.tracks[1].name, "1.mp3");
        lib.set_sort(None);
        assert_eq!(lib.tracks[0].name, "2.flac");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_columns_load() {
        let c: Columns = serde_json::from_str(r#"{"widths":[30,160,100,100,70],"shown":[false,true,true,true,true]}"#).unwrap();
        assert_eq!(c.widths[..5], [30.0, 160.0, 100.0, 100.0, 70.0]);
        assert!(!c.shown[0] && !c.shown[7]);
        assert_eq!(c.widths[7], Columns::default().widths[7]);
    }

    #[test]
    fn counting_plays() {
        let mut c = PlayCounter::default();
        let a = Path::new("a");
        let mut played = 0;
        let mut run = |c: &mut PlayCounter, from: f64, to: f64| {
            let mut t = from;
            while t <= to {
                played += c.tick(Some(a), t, Some(100.0)) as u32;
                t += 0.1;
            }
            played
        };
        // A seek past the middle isn't listening.
        c.tick(Some(a), 0.0, Some(100.0));
        c.tick(Some(a), 80.0, Some(100.0));
        assert_eq!(run(&mut c, 80.0, 100.0), 0);
        // Listening on to half of it: one play, however long it goes on.
        assert_eq!(run(&mut c, 0.0, 49.0), 0);
        assert_eq!(run(&mut c, 49.0, 100.0), 1);
        // Again from the start.
        assert_eq!(run(&mut c, 0.0, 60.0), 2);
    }

    #[test]
    fn next_and_repeat() {
        let tracks = (0..3)
            .map(|i| Track { path: PathBuf::from(format!("{i}")), dir: String::new(), name: format!("{i}"), meta: None, order: i, stamp: (0, 0), added: 0 })
            .collect();
        let mut lib = Library { tracks, current: Some(2), ..Library::default() };
        assert_eq!(lib.next(true), None);
        lib.repeat = Repeat::All;
        assert_eq!(lib.next(true), Some(0));
        lib.repeat = Repeat::One;
        assert_eq!(lib.next(true), Some(2));
        assert_eq!(lib.next(false), Some(0));
        lib.shuffle = true;
        for _ in 0..50 {
            assert_ne!(lib.next(false), Some(2));
        }
    }
}
