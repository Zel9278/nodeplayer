//! FFmpeg, run as the `ffmpeg` and `ffprobe` programs: the audio of files symphonia can't open
//! (most video files), the pictures for the video node, and tags and format details. Nothing
//! links to FFmpeg's libraries, so any version works; the programs come from
//! `~/.local/share/nodeplayer/ffmpeg/` (see [`crate::setup`]) or the PATH.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::Receiver;
use serde_json::Value;

use crate::player::Tags;

/// Where nodeplayer keeps its own copy of the programs.
pub fn local_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("nodeplayer/ffmpeg"))
}

/// `ffmpeg` or `ffprobe`: nodeplayer's own copy, else the one on the PATH.
pub fn tool(name: &str) -> Option<PathBuf> {
    if let Some(p) = local_dir().map(|d| d.join(name)).filter(|p| is_executable(p)) {
        return Some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Both programs are there.
pub fn available() -> bool {
    tool("ffmpeg").is_some() && tool("ffprobe").is_some()
}

fn command(name: &str) -> Result<Command> {
    let path = tool(name).ok_or_else(|| anyhow!("FFmpeg isn't installed (no {name})"))?;
    let mut c = Command::new(path);
    if name == "ffmpeg" {
        c.arg("-nostdin");
    }
    c.arg("-hide_banner").stdin(Stdio::null());
    Ok(c)
}

/// `ffprobe`'s view of a file: its format and streams.
struct Probe {
    format: Value,
    streams: Vec<Value>,
}

impl Probe {
    fn new(path: &Path) -> Result<Self> {
        let out = command("ffprobe")?
            .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
            .arg(path)
            .stderr(Stdio::piped())
            .output()
            .context("can't run ffprobe")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let mut v: Value = serde_json::from_slice(&out.stdout)?;
        let streams = v["streams"].as_array().cloned().unwrap_or_default();
        Ok(Self { format: v["format"].take(), streams })
    }

    fn duration(&self) -> Option<f64> {
        num(&self.format["duration"]).filter(|d| *d > 0.0)
    }

    /// The audio stream FFmpeg would pick: the default one, else the first.
    fn audio(&self) -> Option<&Value> {
        let audio = || self.streams.iter().filter(|s| s["codec_type"] == "audio");
        audio().find(|s| s["disposition"]["default"] == 1).or_else(|| audio().next())
    }

    /// The first real video stream (an attached cover picture doesn't count).
    fn video(&self) -> Option<&Value> {
        self.streams.iter().find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
    }

    /// The file's tags, then the stream's.
    fn tags<'a>(&'a self, stream: Option<&'a Value>) -> impl Iterator<Item = (&'a str, &'a str)> {
        let pairs = |v: &'a Value| v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.as_str(), v.as_str()?)));
        pairs(&self.format["tags"]).chain(stream.into_iter().flat_map(move |s| pairs(&s["tags"])))
    }
}

/// A number ffprobe may write as a number or a string.
fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// "30000/1001" → 29.97.
fn ratio(v: &Value) -> Option<f64> {
    let (a, b) = v.as_str()?.split_once(['/', ':'])?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0.0 && b > 0.0).then_some(a / b)
}

fn seconds(t: f64) -> String {
    format!("{:.6}", t.max(0.0))
}

/// The audio of any file FFmpeg can open, as interleaved stereo at the stream's own rate.
/// A file without audio (a silent video) plays silence for its length.
pub struct AudioDecoder {
    path: PathBuf,
    probe: Probe,
    /// The audio stream's index in the file.
    stream: Option<u64>,
    pub sample_rate: u32,
    pub channels: usize,
    duration: Option<f64>,
    /// Frames of silence left, for a file without audio.
    silence: u64,
    child: Option<(Child, ChildStdout)>,
    /// Bytes of a sample frame that was cut between reads.
    partial: Vec<u8>,
    /// The end of ffmpeg's log, for when it fails. It's read as it comes so a chatty ffmpeg
    /// can't stall on a full pipe.
    log: std::sync::Arc<std::sync::Mutex<String>>,
}

const SILENCE_RATE: u32 = 48_000;

impl AudioDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let probe = Probe::new(path)?;
        let duration = probe.duration();
        let (stream, sample_rate, channels) = match probe.audio() {
            Some(s) => (
                s["index"].as_u64(),
                num(&s["sample_rate"]).map_or(44_100, |r| r as u32),
                s["channels"].as_u64().unwrap_or(2) as usize,
            ),
            None if duration.is_some() => (None, SILENCE_RATE, 2),
            None => bail!("no audio track"),
        };
        let mut me = Self {
            path: path.into(),
            probe,
            stream,
            sample_rate,
            channels,
            duration,
            silence: 0,
            child: None,
            partial: Vec::new(),
            log: Default::default(),
        };
        me.seek(0.0)?;
        Ok(me)
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.duration
    }

    pub fn tags(&self) -> Tags {
        let mut tags = Tags::default();
        for (k, v) in self.probe.tags(self.probe.audio()) {
            let slot = match k.to_ascii_lowercase().as_str() {
                "title" => &mut tags.title,
                "artist" | "album_artist" => &mut tags.artist,
                "album" => &mut tags.album,
                k if k.starts_with("lyrics") || k == "unsyncedlyrics" => &mut tags.lyrics,
                _ => continue,
            };
            if !v.trim().is_empty() {
                slot.get_or_insert_with(|| v.to_string());
            }
        }
        tags
    }

    /// The next stretch as interleaved stereo; `false` at the end of the file.
    pub fn next_stereo(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        let Some((_, stdout)) = &mut self.child else {
            if self.silence == 0 {
                return Ok(false);
            }
            let n = self.silence.min(4096);
            out.resize(out.len() + n as usize * 2, 0.0);
            self.silence -= n;
            return Ok(true);
        };
        let mut buf = std::mem::take(&mut self.partial);
        let have = buf.len();
        buf.resize(have + 4096 * 8, 0);
        let n = loop {
            match stdout.read(&mut buf[have..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        };
        if n == 0 {
            self.finish()?;
            return Ok(false);
        }
        buf.truncate(have + n);
        // Mono comes as it is and is doubled here, at full level on both sides.
        let mono = self.channels == 1;
        let frame = if mono { 4 } else { 8 };
        let whole = buf.len() / frame * frame;
        let samples = buf[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b));
        if mono {
            out.extend(samples.flat_map(|v| [v, v]));
        } else {
            out.extend(samples);
        }
        self.partial = buf[whole..].to_vec();
        Ok(true)
    }

    /// Waits for the decoder that just ended, reporting how it failed if it did.
    fn finish(&mut self) -> Result<()> {
        let Some((mut child, _)) = self.child.take() else { return Ok(()) };
        let status = child.wait()?;
        if !status.success() {
            let log = self.log.lock().unwrap();
            bail!("ffmpeg: {}", log.lines().last().unwrap_or("failed").trim());
        }
        Ok(())
    }

    /// Starts decoding again at `secs`.
    pub fn seek(&mut self, secs: f64) -> Result<()> {
        if let Some((mut c, _)) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.partial.clear();
        self.silence = match (self.stream, self.duration) {
            (None, Some(d)) => ((d - secs).max(0.0) * SILENCE_RATE as f64) as u64,
            _ => 0,
        };
        let Some(index) = self.stream else { return Ok(()) };
        let mut c = command("ffmpeg")?;
        c.args(["-v", "error"]);
        if secs > 0.0 {
            c.args(["-ss", &seconds(secs)]);
        }
        c.arg("-i").arg(&self.path);
        c.args(["-map", &format!("0:{index}"), "-vn", "-sn", "-dn", "-ar", &self.sample_rate.to_string()]);
        // More than two channels are mixed down to two.
        if self.channels > 2 {
            c.args(["-ac", "2"]);
        }
        c.args(["-f", "f32le", "pipe:1"]);
        let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("can't run ffmpeg")?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no output from ffmpeg"))?;
        if let Some(stderr) = child.stderr.take() {
            let log = self.log.clone();
            log.lock().unwrap().clear();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut log = log.lock().unwrap();
                    if log.len() > 4096 {
                        log.clear();
                    }
                    log.push_str(&line);
                    log.push('\n');
                }
            });
        }
        self.child = Some((child, stdout));
        Ok(())
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        if let Some((mut c, _)) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// What the video node shows: the frames of a file's first real video stream, as RGBA.
pub struct VideoDecoder {
    path: PathBuf,
    index: u64,
    pub info: VideoInfo,
    /// The running ffmpeg, the size it scales to, where it started and its frame times.
    child: Option<Running>,
    /// Time of the last picture handed out.
    last: f64,
    /// Where to start ffmpeg next: the seek target, or just past the last picture when the
    /// size changes.
    resume: f64,
}

struct Running {
    child: Child,
    stdout: ChildStdout,
    size: (u32, u32),
    max_height: u32,
    from: f64,
    /// Each frame's time from `from`, read off `showinfo`'s log.
    times: Receiver<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoInfo {
    /// Coded size.
    pub width: u32,
    pub height: u32,
    /// Width of a pixel over its height.
    pub pixel_aspect: f32,
    pub fps: f64,
    /// Clockwise turn the picture needs to stand upright: 0, 90, 180 or 270.
    pub rotation: u32,
}

impl VideoInfo {
    /// Width over height as shown, after the pixel aspect and rotation.
    pub fn aspect(&self) -> f32 {
        let a = self.width as f32 * self.pixel_aspect / self.height.max(1) as f32;
        if self.rotation % 180 == 90 { 1.0 / a } else { a }
    }

    /// The picture's size when scaled to square pixels, `max_height` tall at most (before
    /// rotation), with even sides.
    fn scaled(&self, max_height: u32) -> (u32, u32) {
        let h = self.height.min(max_height.max(2)) & !1;
        let w = ((h as f32 * self.width as f32 * self.pixel_aspect / self.height.max(1) as f32).round() as u32).max(2) & !1;
        (w, h.max(2))
    }
}

/// One decoded picture and when it is due.
pub struct Picture {
    pub time: f64,
    pub image: egui::ColorImage,
}

impl VideoDecoder {
    /// `None` when the file has no video (an attached cover doesn't count).
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let probe = Probe::new(path)?;
        let Some(s) = probe.video() else { return Ok(None) };
        let fps = ratio(&s["avg_frame_rate"]).or_else(|| ratio(&s["r_frame_rate"])).filter(|r| r.is_finite()).unwrap_or(30.0);
        let info = VideoInfo {
            width: s["width"].as_u64().unwrap_or(0) as u32,
            height: s["height"].as_u64().unwrap_or(0) as u32,
            pixel_aspect: ratio(&s["sample_aspect_ratio"]).unwrap_or(1.0) as f32,
            fps: fps.min(240.0),
            rotation: rotation(s),
        };
        if info.width == 0 || info.height == 0 {
            bail!("the video has no size");
        }
        Ok(Some(Self { path: path.into(), index: s["index"].as_u64().unwrap_or(0), info, child: None, last: 0.0, resume: 0.0 }))
    }

    pub fn seek(&mut self, secs: f64) -> Result<()> {
        self.stop();
        self.resume = secs.max(0.0);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut r) = self.child.take() {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
    }

    /// Runs ffmpeg from `from`, scaling to `max_height`. The picture stays as stored: the
    /// node turns it upright.
    fn start(&mut self, from: f64, max_height: u32) -> Result<()> {
        let size = self.info.scaled(max_height);
        let mut c = command("ffmpeg")?;
        c.args(["-v", "info", "-noautorotate"]);
        if from > 0.0 {
            c.args(["-ss", &seconds(from)]);
        }
        c.arg("-i").arg(&self.path);
        let filter = format!("scale={}:{}:flags=bilinear,setsar=1,format=rgba,showinfo", size.0, size.1);
        c.args(["-map", &format!("0:{}", self.index), "-an", "-sn", "-dn", "-vf", &filter, "-fps_mode", "passthrough", "-f", "rawvideo", "pipe:1"]);
        let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("can't run ffmpeg")?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no output from ffmpeg"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no log from ffmpeg"))?;
        let (tx, times) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                match line.split_once("pts_time:") {
                    Some((_, rest)) => {
                        let t = rest.split_whitespace().next().and_then(|t| t.parse::<f64>().ok());
                        if tx.send(t.unwrap_or(f64::NAN)).is_err() {
                            return;
                        }
                    }
                    None if line.contains("Error") || line.contains("error") => log::debug!("ffmpeg: {line}"),
                    None => {}
                }
            }
        });
        self.child = Some(Running { child, stdout, size, max_height, from, times });
        Ok(())
    }

    /// The next picture, at most `max_height` pixels tall; `None` at the end.
    pub fn next(&mut self, max_height: u32) -> Result<Option<Picture>> {
        if self.child.as_ref().is_some_and(|r| r.max_height != max_height) {
            self.stop();
        }
        if self.child.is_none() {
            self.start(self.resume, max_height)?;
        }
        let r = self.child.as_mut().unwrap();
        let (w, h) = r.size;
        let mut pixels = vec![0u8; w as usize * h as usize * 4];
        match r.stdout.read_exact(&mut pixels) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.stop();
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        // showinfo logs a frame as it passes, before it's written out.
        let since = r.times.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(f64::NAN);
        let time = if since.is_finite() { r.from + since } else { self.last + 1.0 / self.info.fps };
        self.last = time;
        self.resume = time + 0.5 / self.info.fps;
        // Video has no transparency, so premultiplied and straight alpha are the same.
        Ok(Some(Picture { time, image: egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &pixels) }))
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The clockwise turn from the stream's display matrix (phone videos shot upright).
fn rotation(s: &Value) -> u32 {
    let from_matrix = s["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|d| num(&d["rotation"]))
        // The matrix turns counter-clockwise by this much.
        .map(|ccw| -ccw);
    let from_tag = num(&s["tags"]["rotate"]);
    let cw = from_matrix.or(from_tag).unwrap_or(0.0);
    (cw.round() as i64).rem_euclid(360) as u32 / 90 * 90
}

/// What the Misskey note template and the library table can show about a file.
#[derive(Clone, Debug, Default)]
pub struct TrackInfo {
    /// Tags, under foobar2000-style lowercase names ("album artist", "tracknumber", ...).
    pub meta: Vec<(String, String)>,
    /// Technical info, as `$info(name)` gives it: codec, bitrate (kbps), samplerate, ...
    pub tech: Vec<(String, String)>,
    pub duration: Option<f64>,
}

/// Codecs that keep every bit, by FFmpeg's names (besides the PCM family).
const LOSSLESS: &[&str] = &["flac", "alac", "ape", "wavpack", "tta", "tak", "mlp", "truehd", "wmalossless", "shorten", "mp4als", "ralf", "osq"];

/// Reads a file's tags and its audio stream's format without decoding anything.
pub fn track_info(path: &Path) -> Result<TrackInfo> {
    let probe = Probe::new(path)?;
    let mut info = TrackInfo { duration: probe.duration(), ..Default::default() };
    let stream = probe.audio();
    for (k, v) in probe.tags(stream) {
        let v = v.trim();
        let key = k.to_ascii_lowercase();
        if v.is_empty() || key.starts_with("lyrics") || key == "unsyncedlyrics" {
            continue;
        }
        let mut put = |k: &str, v: &str| {
            if !v.is_empty() && !info.meta.iter().any(|(have, _)| have == k) {
                info.meta.push((k.to_string(), v.to_string()));
            }
        };
        // "3/12" as tracknumber 3 of 12.
        let split = |v: &str| v.split_once('/').map(|(a, b)| (a.trim().to_string(), b.trim().to_string()));
        match key.as_str() {
            "track" | "tracknumber" => match split(v) {
                Some((n, total)) => {
                    put("tracknumber", &n);
                    put("totaltracks", &total);
                }
                None => put("tracknumber", v),
            },
            "disc" | "discnumber" => match split(v) {
                Some((n, total)) => {
                    put("discnumber", &n);
                    put("totaldiscs", &total);
                }
                None => put("discnumber", v),
            },
            "album_artist" | "albumartist" | "album artist" => put("album artist", v),
            "tracktotal" => put("totaltracks", v),
            "disctotal" => put("totaldiscs", v),
            "year" => put("date", v),
            _ => put(&key, v),
        }
    }
    if let Some(s) = stream {
        let codec = s["codec_name"].as_str().unwrap_or("");
        let lossless = codec.starts_with("pcm_") || codec.starts_with("dsd_") || LOSSLESS.contains(&codec);
        let mut tech = |k: &str, v: String| {
            if !v.is_empty() {
                info.tech.push((k.into(), v));
            }
        };
        tech("codec", codec_label(codec));
        tech("codec_profile", s["profile"].as_str().unwrap_or("").to_string());
        let bit_rate = num(&s["bit_rate"]).or_else(|| num(&probe.format["bit_rate"])).unwrap_or(0.0);
        if bit_rate > 0.0 {
            tech("bitrate", ((bit_rate / 1000.0).round() as u64).to_string());
        }
        if let Some(rate) = num(&s["sample_rate"]).filter(|r| *r > 0.0) {
            tech("samplerate", (rate as u64).to_string());
        }
        if let Some(ch) = s["channels"].as_u64().filter(|c| *c > 0) {
            tech("channels", ch.to_string());
        }
        let raw = num(&s["bits_per_raw_sample"]).unwrap_or(0.0);
        let coded = num(&s["bits_per_sample"]).unwrap_or(0.0);
        let bits = if raw > 0.0 { raw } else if lossless { coded } else { 0.0 };
        if bits > 0.0 {
            tech("bitspersample", (bits as u64).to_string());
        }
        tech("encoding", if lossless { "lossless" } else { "lossy" }.into());
    }
    if let Some(name) = probe.format["format_long_name"].as_str().filter(|n| !n.is_empty()) {
        info.tech.push(("container".into(), name.to_string()));
    }
    Ok(info)
}

/// How foobar2000 names codecs, for FFmpeg's codec names.
fn codec_label(name: &str) -> String {
    match name {
        n if n.starts_with("pcm_") => "PCM".into(),
        n if n.starts_with("adpcm_") => "ADPCM".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        "wavpack" => "WavPack".into(),
        "ape" => "Monkey's Audio".into(),
        "musepack7" | "musepack8" => "Musepack".into(),
        "wmav1" | "wmav2" | "wmapro" | "wmavoice" => "WMA".into(),
        "wmalossless" => "WMA Lossless".into(),
        "eac3" => "E-AC3".into(),
        "ac3" => "AC3".into(),
        n => n.to_uppercase(),
    }
}

pub fn is_video_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Common video file extensions, for the library and the open dialog.
pub const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "ts", "m2ts", "mts", "mpg", "mpeg", "ogv", "3gp"];

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes a short test clip with the ffmpeg command line, if it's installed.
    fn clip(dir: &Path, name: &str, args: &[&str]) -> Option<std::path::PathBuf> {
        let path = dir.join(name);
        let ok = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        ok.then_some(path)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nodeplayer-av-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn decodes_video_and_its_audio() {
        let dir = tmp("clip");
        let Some(path) = clip(
            &dir,
            "clip.mkv",
            &["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=3", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=3", "-c:v", "libvpx-vp9", "-c:a", "libopus", "-metadata", "title=Clip"],
        ) else {
            eprintln!("no ffmpeg, skipping");
            return;
        };
        let mut a = AudioDecoder::open(&path).unwrap();
        assert_eq!(a.channels, 1);
        assert_eq!(a.tags().title.as_deref(), Some("Clip"));
        let mut out = Vec::new();
        while a.next_stereo(&mut out).unwrap() {}
        let secs = out.len() as f64 / 2.0 / a.sample_rate as f64;
        assert!((secs - 3.0).abs() < 0.1, "{secs}");
        assert!(out.iter().any(|v| v.abs() > 0.1));

        let mut v = VideoDecoder::open(&path).unwrap().unwrap();
        assert_eq!((v.info.width, v.info.height, v.info.fps), (320, 240, 25.0));
        let first = v.next(120).unwrap().unwrap();
        assert_eq!((first.time, first.image.size), (0.0, [160, 120]));
        v.seek(2.0).unwrap();
        let p = v.next(720).unwrap().unwrap();
        assert!((p.time - 2.0).abs() < 0.03, "{}", p.time);
        assert_eq!(p.image.size, [320, 240]);
        let mut n = 1;
        while v.next(120).unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 25);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn silent_video_plays_silence() {
        let dir = tmp("silent");
        let Some(path) = clip(&dir, "silent.mp4", &["-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=2", "-c:v", "libx264"]) else {
            return;
        };
        let mut a = AudioDecoder::open(&path).unwrap();
        let mut out = Vec::new();
        while a.next_stereo(&mut out).unwrap() {}
        assert!((out.len() as f64 / 2.0 / SILENCE_RATE as f64 - 2.0).abs() < 0.05);
        assert!(out.iter().all(|v| *v == 0.0));
        a.seek(1.5).unwrap();
        out.clear();
        while a.next_stereo(&mut out).unwrap() {}
        assert!((out.len() as f64 / 2.0 / SILENCE_RATE as f64 - 0.5).abs() < 0.05);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn audio_only_files_have_no_video() {
        let dir = tmp("audio");
        let Some(path) = clip(&dir, "tone.mp3", &["-f", "lavfi", "-i", "sine=duration=1"]) else { return };
        assert!(VideoDecoder::open(&path).unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn track_info_reads_tags_and_format() {
        let dir = tmp("info");
        let Some(flac) = clip(
            &dir,
            "t.flac",
            &["-f", "lavfi", "-i", "sine=sample_rate=48000:duration=1", "-sample_fmt", "s32", "-metadata", "TRACKNUMBER=3/12", "-metadata", "ALBUMARTIST=Various", "-metadata", "GENRE=Test"],
        ) else {
            return;
        };
        let info = track_info(&flac).unwrap();
        let get = |list: &[(String, String)], k: &str| list.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get(&info.meta, "tracknumber").as_deref(), Some("3"));
        assert_eq!(get(&info.meta, "totaltracks").as_deref(), Some("12"));
        assert_eq!(get(&info.meta, "album artist").as_deref(), Some("Various"));
        assert_eq!(get(&info.meta, "genre").as_deref(), Some("Test"));
        assert_eq!(get(&info.tech, "codec").as_deref(), Some("FLAC"));
        assert_eq!(get(&info.tech, "encoding").as_deref(), Some("lossless"));
        assert_eq!(get(&info.tech, "bitspersample").as_deref(), Some("24"));
        assert_eq!(get(&info.tech, "samplerate").as_deref(), Some("48000"));

        let Some(m4a) = clip(&dir, "t.m4a", &["-f", "lavfi", "-i", "sine=duration=1", "-c:a", "aac", "-b:a", "128k", "-ac", "2"]) else { return };
        let info = track_info(&m4a).unwrap();
        assert_eq!(get(&info.tech, "codec").as_deref(), Some("AAC"));
        assert_eq!(get(&info.tech, "codec_profile").as_deref(), Some("LC"));
        assert_eq!(get(&info.tech, "encoding").as_deref(), Some("lossy"));
        assert_eq!(get(&info.tech, "channels").as_deref(), Some("2"));
        assert!(get(&info.tech, "bitspersample").is_none());
        assert!(get(&info.tech, "bitrate").is_some_and(|b| b.parse::<u32>().unwrap() > 50));
        std::fs::remove_dir_all(dir).ok();
    }
}
