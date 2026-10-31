//! File playback: a decoder thread (symphonia, or FFmpeg for what it can't open, + rubato) feeding a ring buffer that the
//! `Player` source node drains on the audio thread. MIDI files are played by a
//! [`MidiSong`] instead, out of the node's MIDI pin.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, Async, Fft, FixedAsync, FixedSync, Indexing, Resampler, SincInterpolationParameters};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag, StandardVisualKey};
use symphonia::core::units::{Time, TimeBase};

use crate::engine::{MidiBuf, MidiEvent, Processor, StereoBuf};
use crate::midi_song::{MidiFeed, MidiSong, is_midi_file};

const RING_SECONDS: f32 = 0.5;
const RESAMPLE_CHUNK: usize = 1024;

/// Playback rate shift: speed and pitch change together, like a tape played faster.
pub const RATE_RANGE: RangeInclusive<f32> = 0.5..=2.0;
/// Output frames per varispeed resampler call.
const VARISPEED_CHUNK: usize = 64;
/// Frames played at 1× kept to prime the resampler with when the rate first changes.
const VARISPEED_HISTORY: usize = 512;
/// The shortest selection loop, in seconds.
pub const MIN_LOOP: f64 = 0.01;
/// At most this long (seconds) of what follows the loop's end fades out under its start.
const LOOP_FADE: f64 = 0.005;

#[derive(Default)]
struct Shared {
    playing: AtomicBool,
    /// Set by the decoder after a seek/load; the source discards buffered audio and clears it.
    flush: AtomicBool,
    /// Decoder has pushed everything for the current file.
    eof: AtomicBool,
    /// Set by the source when playback stopped because the file ran out; consumed by
    /// [`Player::take_ended`].
    ended: AtomicBool,
    /// Like `ended`, but stays set until the next load or seek.
    finished: AtomicBool,
    /// Frames of the file played since `base_secs` (at the device rate, before the rate shift).
    frames_played: AtomicU64,
    base_secs: AtomicU64,
    /// Playback rate as f32 bits.
    rate: AtomicU32,
    /// Recently decoded packets: start and length in the file (seconds) and size in bytes,
    /// for the bitrate at the playing position.
    packets: Mutex<VecDeque<(f64, f64, u32)>>,
    /// Where the file time is known to be: the ring frame (counted like `frames_played`) and
    /// the file time from there on. Where the resampler's delay ends after a load or seek,
    /// and where the selection loop went back; the last one played and those to come.
    wraps: Mutex<VecDeque<(u64, f64)>>,
}

impl Shared {
    /// The playing position in the file, in seconds.
    fn position(&self, device_rate: u32) -> f64 {
        let played = self.frames_played.load(Ordering::Acquire);
        let wraps = self.wraps.lock().unwrap();
        match wraps.iter().rev().find(|(at, _)| *at <= played) {
            Some(&(from, secs)) => secs + (played - from) as f64 / device_rate as f64,
            // Still in the resampler's delay.
            None if let Some(&(_, secs)) = wraps.front() => secs,
            None => f64::from_bits(self.base_secs.load(Ordering::Acquire)) + played as f64 / device_rate as f64,
        }
    }
}

#[derive(Clone, Default)]
pub struct Status {
    pub path: Option<PathBuf>,
    pub duration: Option<f64>,
    pub source_rate: u32,
    pub channels: usize,
    pub error: Option<String>,
    pub tags: Tags,
    /// "symphonia", "FFmpeg" or "MIDI".
    pub decoder: &'static str,
}

/// The tags of the loaded file that the UI and the media session show.
#[derive(Clone, Default)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Embedded lyrics (plain, or LRC with timestamps).
    pub lyrics: Option<String>,
    /// Embedded cover art: media type and encoded image.
    pub cover: Option<Arc<(String, Vec<u8>)>>,
}

impl Status {
    /// Title tag, or the file name without extension.
    pub fn title(&self) -> Option<String> {
        self.tags.title.clone().or_else(|| {
            self.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned())
        })
    }
}

enum Cmd {
    Load(PathBuf),
    /// Let go of the file (a MIDI file plays instead).
    Unload,
    Seek(f64),
    /// Play `.0..1` over and over; `None` plays on.
    Loop(Option<(f64, f64)>),
    Attach(rtrb::Producer<f32>),
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    status: Arc<Mutex<Status>>,
    device_rate: u32,
    midi: Mutex<MidiSong>,
    /// The MIDI file loaded, when it is one; the decoder then has nothing.
    midi_path: Mutex<Option<PathBuf>>,
    loop_region: Mutex<Option<(f64, f64)>>,
}

impl Player {
    pub fn new(device_rate: u32) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared { rate: AtomicU32::new(1f32.to_bits()), ..Default::default() });
        let status = Arc::new(Mutex::new(Status::default()));
        let worker = Worker {
            rx,
            shared: shared.clone(),
            status: status.clone(),
            device_rate,
            ring: None,
            decoder: None,
            resampler: None,
            acc: Vec::new(),
            pending: Vec::new(),
            decoder_done: true,
            path: None,
            loop_region: None,
            fed: 0,
            stretch: (0, 0.0),
            fade: Vec::new(),
            faded: 0,
        };
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || worker.run())
            .expect("spawn decoder thread");
        Self { tx, shared, status, device_rate, midi: Mutex::new(MidiSong::new()), midi_path: Mutex::new(None), loop_region: Mutex::new(None) }
    }

    /// Creates the audio-thread source. Only the most recently created source receives audio.
    pub fn make_source(&self) -> PlayerSource {
        let len = (self.device_rate as f32 * RING_SECONDS) as usize * 2;
        let (prod, cons) = rtrb::RingBuffer::new(len);
        let _ = self.tx.send(Cmd::Attach(prod));
        let midi = self.midi.lock().unwrap().feed(self.device_rate as f32);
        PlayerSource { ring: cons, shared: self.shared.clone(), varispeed: Varispeed::new(), midi }
    }

    pub fn is_midi(&self) -> bool {
        self.midi_path.lock().unwrap().is_some()
    }

    /// The MIDI player, with its load finished if it is done and its speed following the
    /// rate shift.
    pub fn midi(&self) -> MutexGuard<'_, MidiSong> {
        let mut m = self.midi.lock().unwrap();
        m.player.update();
        let rate = self.rate();
        if m.player.settings.speed != rate {
            m.player.set_speed(rate);
        }
        m
    }

    /// Playback rate shift, within [`RATE_RANGE`]; 1 is normal speed.
    pub fn set_rate(&self, rate: f32) {
        let r = rate.clamp(*RATE_RANGE.start(), *RATE_RANGE.end());
        self.shared.rate.store(r.to_bits(), Ordering::Release);
    }

    pub fn rate(&self) -> f32 {
        f32::from_bits(self.shared.rate.load(Ordering::Acquire))
    }

    /// Plays `start..end` (seconds) over and over, without a gap; an end past the file's
    /// loops the whole file from `start`. `None` plays on. Getting to the end from inside the
    /// range starts it again; a position past it plays on to the end. Loading a file clears it.
    pub fn set_loop(&self, region: Option<(f64, f64)>) {
        let region = region.filter(|(a, b)| *a >= 0.0 && b - a >= MIN_LOOP);
        {
            let mut l = self.loop_region.lock().unwrap();
            if *l == region {
                return;
            }
            *l = region;
        }
        self.midi.lock().unwrap().player.set_loop(region);
        let _ = self.tx.send(Cmd::Loop(region));
    }

    pub fn loop_region(&self) -> Option<(f64, f64)> {
        *self.loop_region.lock().unwrap()
    }

    pub fn load(&self, path: &Path) {
        self.set_loop(None);
        // Clear a stale end-of-file now so the source doesn't stop again before the decoder
        // gets to the command.
        self.shared.eof.store(false, Ordering::Release);
        self.shared.ended.store(false, Ordering::Release);
        self.shared.finished.store(false, Ordering::Release);
        if is_midi_file(path) {
            log::info!("load {}", path.display());
            let _ = self.tx.send(Cmd::Unload);
            self.shared.playing.store(false, Ordering::Release);
            *self.midi_path.lock().unwrap() = Some(path.to_path_buf());
            self.midi().player.open(path.to_path_buf(), true);
            return;
        }
        if self.midi_path.lock().unwrap().take().is_some() {
            self.midi().player.stop();
        }
        let _ = self.tx.send(Cmd::Load(path.to_path_buf()));
        self.shared.playing.store(true, Ordering::Release);
    }

    pub fn seek(&self, secs: f64) {
        if self.is_midi() {
            let mut m = self.midi();
            if !m.player.seek_when_loaded(secs.max(0.0)) {
                m.player.seek(secs.max(0.0));
            }
            return;
        }
        self.shared.eof.store(false, Ordering::Release);
        self.shared.finished.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Seek(secs.max(0.0)));
    }

    /// Starting again after the file played to the end restarts it from the top.
    pub fn set_playing(&self, on: bool) {
        if self.is_midi() {
            let mut m = self.midi();
            if !m.player.play_when_loaded(on) {
                if on { m.player.play() } else { m.player.pause() }
            }
            return;
        }
        if on && self.shared.finished.load(Ordering::Acquire) {
            self.seek(0.0);
        }
        self.shared.playing.store(on, Ordering::Release);
    }

    pub fn is_playing(&self) -> bool {
        if self.is_midi() {
            let m = self.midi();
            return m.player.is_playing() || m.player.plays_when_loaded();
        }
        self.shared.playing.load(Ordering::Acquire)
    }

    pub fn position(&self) -> f64 {
        if self.is_midi() {
            return self.midi().player.status().position;
        }
        self.shared.position(self.device_rate)
    }

    /// The file played to the end and hasn't been started again.
    pub fn finished(&self) -> bool {
        if self.is_midi() {
            let m = self.midi();
            let s = m.player.status();
            return m.player.is_loaded() && !s.playing && !m.player.is_loading() && s.position >= s.duration;
        }
        self.shared.finished.load(Ordering::Acquire)
    }

    /// True once after the current file finished playing to the end.
    pub fn take_ended(&self) -> bool {
        if self.is_midi() {
            return self.midi().player.take_ended();
        }
        self.shared.ended.swap(false, Ordering::AcqRel)
    }

    pub fn status(&self) -> Status {
        let midi_path = self.midi_path.lock().unwrap().clone();
        if let Some(path) = midi_path {
            let m = self.midi();
            return Status {
                path: Some(path),
                // Known once loaded.
                duration: m.player.info().map(|i| i.duration),
                error: m.player.load_error().map(str::to_owned),
                decoder: "MIDI",
                ..Default::default()
            };
        }
        self.status.lock().unwrap().clone()
    }

    /// The bitrate of the second just played, in kbps, from the sizes of its packets; `None`
    /// when the decoder doesn't tell them (FFmpeg) or too little has played.
    pub fn live_bitrate(&self) -> Option<f64> {
        if self.is_midi() {
            return None;
        }
        let pos = self.position();
        let packets = self.shared.packets.lock().unwrap();
        let (mut bytes, mut secs) = (0u64, 0f64);
        for &(start, dur, size) in packets.iter().filter(|(start, ..)| *start >= pos - 1.0 && *start < pos) {
            bytes += u64::from(size);
            secs += dur.min(pos - start);
        }
        (secs > 0.25).then(|| bytes as f64 * 8.0 / secs / 1000.0)
    }
}

/// A file's audio as interleaved stereo at its own rate, read like the player reads it (for
/// drawing it).
pub struct AudioReader(Decoder);

impl AudioReader {
    pub fn open(path: &Path) -> Result<Self> {
        Decoder::open(path).map(Self)
    }

    pub fn sample_rate(&self) -> u32 {
        self.0.sample_rate()
    }

    pub fn duration(&self) -> Option<f64> {
        self.0.duration_secs()
    }

    /// The next stretch of audio, appended to `out`; false at the end.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        self.0.next_stereo(out)
    }

    /// Reading goes on from exactly `secs`.
    pub fn seek(&mut self, secs: f64) -> Result<()> {
        self.0.seek(secs)
    }
}

/// symphonia when it can open the file (gapless, no surprises), FFmpeg otherwise (video
/// files, codecs symphonia lacks).
enum Decoder {
    Sym(SymDecoder),
    Av(Box<crate::av::AudioDecoder>),
}

impl Decoder {
    fn open(path: &Path) -> Result<Self> {
        match SymDecoder::open(path) {
            Ok(d) => Ok(Self::Sym(d)),
            Err(e) => {
                log::debug!("symphonia can't open {}: {e}; trying ffmpeg", path.display());
                crate::av::AudioDecoder::open(path).map(|d| Self::Av(Box::new(d))).map_err(|e2| anyhow!("{e} (ffmpeg: {e2})"))
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            Self::Sym(d) => d.sample_rate,
            Self::Av(d) => d.sample_rate,
        }
    }

    fn channels(&self) -> usize {
        match self {
            Self::Sym(d) => d.channels,
            Self::Av(d) => d.channels,
        }
    }

    fn tags(&mut self) -> Tags {
        match self {
            Self::Sym(d) => d.tags(),
            Self::Av(d) => d.tags(),
        }
    }

    fn duration_secs(&self) -> Option<f64> {
        match self {
            Self::Sym(d) => d.duration_secs(),
            Self::Av(d) => d.duration_secs(),
        }
    }

    fn next_stereo(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        match self {
            Self::Sym(d) => d.next_stereo(out),
            Self::Av(d) => d.next_stereo(out),
        }
    }

    fn seek(&mut self, secs: f64) -> Result<()> {
        match self {
            Self::Sym(d) => d.seek(secs),
            Self::Av(d) => d.seek(secs),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Sym(_) => "symphonia",
            Self::Av(_) => "FFmpeg",
        }
    }

    /// The last packet decoded: start, length (seconds) and size.
    fn take_packet(&mut self) -> Option<(f64, f64, u32)> {
        match self {
            Self::Sym(d) => d.last_packet.take(),
            Self::Av(_) => None,
        }
    }
}

struct SymDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    time_base: Option<TimeBase>,
    last_packet: Option<(f64, f64, u32)>,
    /// After a seek: the time asked for. Seeks land on a packet at or before it; what comes
    /// before it is left out.
    trim_to: Option<f64>,
}

impl SymDecoder {
    fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;
        let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("not an audio track"))?;
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())?;
        Ok(Self { format, decoder, track_id, sample_rate, channels, time_base, last_packet: None, trim_to: None })
    }

    fn tags(&mut self) -> Tags {
        let mut tags = Tags::default();
        let track_id = self.track_id as u64;
        let mut meta = self.format.metadata();
        let Some(rev) = meta.skip_to_latest() else { return tags };
        let per_track = rev.per_track.iter().filter(|t| t.track_id == track_id).map(|t| &t.metadata);
        let containers: Vec<_> = std::iter::once(&rev.media).chain(per_track).collect();
        for tag in containers.iter().flat_map(|c| &c.tags) {
            let (slot, v) = match &tag.std {
                Some(StandardTag::TrackTitle(v)) => (&mut tags.title, v),
                Some(StandardTag::Artist(v)) => (&mut tags.artist, v),
                Some(StandardTag::Album(v)) => (&mut tags.album, v),
                Some(StandardTag::Lyrics(v)) => (&mut tags.lyrics, v),
                _ => continue,
            };
            slot.get_or_insert_with(|| v.to_string());
        }
        let visuals: Vec<_> = containers.iter().flat_map(|c| &c.visuals).collect();
        let cover = visuals
            .iter()
            .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
            .or_else(|| visuals.first());
        tags.cover = cover.map(|v| {
            let mime = v.media_type.clone().unwrap_or_else(|| "image/jpeg".into());
            Arc::new((mime, v.data.to_vec()))
        });
        tags
    }

    fn duration_secs(&self) -> Option<f64> {
        let t = self.format.tracks().iter().find(|t| t.id == self.track_id)?;
        Some(t.time_base?.calc_duration(t.duration?)?.as_secs_f64())
    }

    /// Next packet as interleaved stereo; `None` at end of stream.
    fn next_stereo(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        let mut planes: Vec<Vec<f32>> = Vec::new();
        loop {
            let Some(packet) = self.format.next_packet()? else { return Ok(false) };
            if packet.track_id != self.track_id {
                continue;
            }
            let mut start = None;
            if let Some(tb) = self.time_base {
                let secs = tb.calc_time_saturating(packet.pts).as_secs_f64();
                let dur = tb.calc_duration(packet.dur).map_or(0.0, |t| t.as_secs_f64());
                self.last_packet = Some((secs, dur, packet.data.len() as u32));
                start = Some(secs);
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    buf.copy_to_vecs_planar::<f32>(&mut planes);
                    let (l, r) = match planes.len() {
                        0 => return Ok(true),
                        1 => (&planes[0], &planes[0]),
                        _ => (&planes[0], &planes[1]),
                    };
                    let skip = match (self.trim_to, start) {
                        (Some(to), Some(at)) => ((to - at) * self.sample_rate as f64).round().max(0.0) as usize,
                        _ => 0,
                    };
                    if skip >= l.len() {
                        continue;
                    }
                    self.trim_to = None;
                    out.extend(l[skip..].iter().zip(&r[skip..]).flat_map(|(a, b)| [*a, *b]));
                    return Ok(true);
                }
                Err(SymError::DecodeError(e)) => log::warn!("decode error: {e}"),
                Err(SymError::IoError(e)) if e.kind() != std::io::ErrorKind::UnexpectedEof => {
                    log::warn!("io error: {e}")
                }
                Err(SymError::IoError(_)) | Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn seek(&mut self, secs: f64) -> Result<()> {
        let time = Time::try_from_secs_f64(secs).ok_or_else(|| anyhow!("bad seek time"))?;
        let to = self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })?;
        self.decoder.reset();
        self.trim_to = self.time_base.and_then(|tb| tb.calc_time(to.required_ts)).map(|t| t.as_secs_f64());
        Ok(())
    }
}

struct Worker {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    status: Arc<Mutex<Status>>,
    device_rate: u32,
    ring: Option<rtrb::Producer<f32>>,
    decoder: Option<Decoder>,
    resampler: Option<Fft<f32>>,
    /// Decoded interleaved stereo waiting for a full resampler chunk.
    acc: Vec<f32>,
    /// Device-rate interleaved stereo waiting for ring space.
    pending: Vec<f32>,
    decoder_done: bool,
    /// The file currently loaded, for reopening it.
    path: Option<PathBuf>,
    loop_region: Option<(f64, f64)>,
    /// Source frames decoded since the stream was last reset.
    fed: u64,
    /// Where the stretch being decoded began: at which of those frames, and its file time.
    stretch: (u64, f64),
    /// What followed the loop's end (interleaved), fading out under its start; `faded`
    /// frames of it are done.
    fade: Vec<f32>,
    faded: usize,
}

impl Worker {
    fn run(mut self) {
        loop {
            let busy = !self.pending.is_empty() || !self.decoder_done;
            let cmd = if busy {
                match self.rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(crossbeam_channel::TryRecvError::Empty) => None,
                    Err(_) => return,
                }
            } else {
                match self.rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(cmd) = cmd {
                self.handle(cmd);
                continue;
            }
            if busy && !self.produce() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Attach(p) => self.ring = Some(p),
            Cmd::Unload => {
                self.reset_stream(0.0);
                self.decoder = None;
                self.decoder_done = true;
                self.path = None;
                *self.status.lock().unwrap() = Status::default();
                self.shared.base_secs.store(0f64.to_bits(), Ordering::Release);
            }
            Cmd::Load(path) => {
                log::info!("load {}", path.display());
                self.reset_stream(0.0);
                match Decoder::open(&path) {
                    Ok(mut d) => {
                        *self.status.lock().unwrap() = Status {
                            path: Some(path.clone()),
                            duration: d.duration_secs(),
                            source_rate: d.sample_rate(),
                            channels: d.channels(),
                            error: None,
                            tags: d.tags(),
                            decoder: d.name(),
                        };
                        self.resampler = (d.sample_rate() != self.device_rate)
                            .then(|| {
                                Fft::<f32>::new(
                                    d.sample_rate() as usize,
                                    self.device_rate as usize,
                                    RESAMPLE_CHUNK,
                                    2,
                                    FixedSync::Input,
                                )
                                .map_err(|e| log::error!("resampler: {e}"))
                                .ok()
                            })
                            .flatten();
                        self.decoder = Some(d);
                        self.decoder_done = false;
                        self.path = Some(path);
                        self.mark_start(0.0);
                    }
                    Err(e) => {
                        let mut st = self.status.lock().unwrap();
                        st.error = Some(format!("{}: {e}", path.display()));
                        self.decoder = None;
                        self.path = None;
                        self.shared.playing.store(false, Ordering::Release);
                    }
                }
                self.shared.base_secs.store(0f64.to_bits(), Ordering::Release);
            }
            Cmd::Seek(secs) => {
                log::debug!("seek {secs:.2}");
                // Some demuxers (MP4, MKV) can't seek once they've reached the end, so start
                // over from a freshly opened file then, or whenever seeking fails.
                let seeked = !self.decoder_done
                    && self.decoder.as_mut().is_some_and(|d| {
                        d.seek(secs).map_err(|e| log::debug!("seek failed, reopening: {e}")).is_ok()
                    });
                let mut at = secs;
                if !seeked {
                    let Some(path) = &self.path else { return };
                    match Decoder::open(path) {
                        Ok(mut d) => {
                            if secs > 0.0 && d.seek(secs).is_err() {
                                log::warn!("can't seek in {}", path.display());
                                at = 0.0;
                            }
                            self.decoder = Some(d);
                        }
                        Err(e) => {
                            log::warn!("can't reopen {}: {e}", path.display());
                            return;
                        }
                    }
                }
                self.reset_stream(at);
                self.mark_start(at);
                self.decoder_done = false;
                self.shared.base_secs.store(at.to_bits(), Ordering::Release);
            }
            Cmd::Loop(region) => {
                self.loop_region = region;
                // Audio from past a new end may be on its way already: it goes, and playing
                // goes on from where it is.
                let Some((_, end)) = region else { return };
                let pos = self.shared.position(self.device_rate);
                if self.decoder.is_some() && pos < end && self.decoded_to().is_some_and(|t| t > end) {
                    self.handle(Cmd::Seek(pos));
                }
            }
        }
    }

    /// The file time decoding has got to.
    fn decoded_to(&self) -> Option<f64> {
        let rate = self.decoder.as_ref()?.sample_rate();
        Some(self.stretch.1 + (self.fed - self.stretch.0) as f64 / rate as f64)
    }

    /// Through the resampler, the file starts (at `at`) once its delay is out.
    fn mark_start(&self, at: f64) {
        if self.resampler.is_some() {
            self.shared.wraps.lock().unwrap().push_back((self.ring_frame(0), at));
        }
    }

    /// The ring frame that source frame `fed` comes out at.
    fn ring_frame(&self, fed: u64) -> u64 {
        match (&self.resampler, &self.decoder) {
            (Some(rs), Some(d)) => {
                (fed as f64 * self.device_rate as f64 / d.sample_rate() as f64).round() as u64 + rs.output_delay() as u64
            }
            _ => fed,
        }
    }

    /// Goes back to `start` for the selection loop; false if the file can't be read from
    /// there.
    fn wrap(&mut self, start: f64) -> bool {
        let Some(d) = &mut self.decoder else { return false };
        if let Err(e) = d.seek(start) {
            log::debug!("loop: seek failed, reopening: {e}");
            let Some(path) = &self.path else { return false };
            match Decoder::open(path).and_then(|mut d| d.seek(start).map(|_| d)) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => {
                    log::warn!("loop: can't go back in {}: {e}", path.display());
                    return false;
                }
            }
        }
        self.stretch = (self.fed, start);
        let mark = (self.ring_frame(self.fed), start);
        let played = self.shared.frames_played.load(Ordering::Acquire);
        let mut wraps = self.shared.wraps.lock().unwrap();
        while wraps.len() > 1 && wraps[1].0 <= played {
            wraps.pop_front();
        }
        wraps.push_back(mark);
        true
    }

    /// Fades the end of the last loop out under the frames from `acc[from..]` on.
    fn crossfade(&mut self, from: usize) {
        let len = self.fade.len() / 2;
        if self.faded >= len {
            return;
        }
        for fr in self.acc[from..].chunks_exact_mut(2) {
            if self.faded >= len {
                break;
            }
            let t = (self.faded as f32 + 0.5) / len as f32 * std::f32::consts::FRAC_PI_2;
            let (fade_in, fade_out) = t.sin_cos();
            fr[0] = fr[0] * fade_in + self.fade[self.faded * 2] * fade_out;
            fr[1] = fr[1] * fade_in + self.fade[self.faded * 2 + 1] * fade_out;
            self.faded += 1;
        }
    }

    /// Drops everything buffered between the decoder and the speakers; decoding goes on from
    /// file time `at`.
    fn reset_stream(&mut self, at: f64) {
        self.fed = 0;
        self.stretch = (0, at);
        self.fade.clear();
        self.faded = 0;
        self.shared.wraps.lock().unwrap().clear();
        self.acc.clear();
        self.pending.clear();
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
        self.shared.eof.store(false, Ordering::Release);
        if self.ring.is_some() {
            self.shared.flush.store(true, Ordering::Release);
            let deadline = Instant::now() + Duration::from_millis(300);
            while self.shared.flush.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            // If the source isn't running, stale samples are simply played later.
            self.shared.flush.store(false, Ordering::Release);
        }
        self.shared.frames_played.store(0, Ordering::Release);
        self.shared.packets.lock().unwrap().clear();
    }

    /// Moves audio one step down the pipeline. Returns false when nothing could be done.
    fn produce(&mut self) -> bool {
        if !self.pending.is_empty() {
            let Some(ring) = &mut self.ring else { return false };
            let n = ring.slots().min(self.pending.len()) & !1;
            if n == 0 {
                return false;
            }
            if let Ok(chunk) = ring.write_chunk_uninit(n) {
                chunk.fill_from_iter(self.pending.drain(..n));
            }
            if self.pending.is_empty() && self.decoder_done {
                self.shared.eof.store(true, Ordering::Release);
            }
            return true;
        }
        let Some(d) = &mut self.decoder else {
            self.decoder_done = true;
            return false;
        };
        let before = self.acc.len();
        let more = match d.next_stereo(&mut self.acc) {
            Ok(m) => m,
            Err(e) => {
                log::error!("decode failed: {e}");
                false
            }
        };
        let rate = d.sample_rate() as f64;
        if let Some(p) = d.take_packet() {
            let mut packets = self.shared.packets.lock().unwrap();
            // A few seconds behind is plenty: the ring holds well under one.
            while packets.front().is_some_and(|f| f.0 < p.0 - 5.0) {
                packets.pop_front();
            }
            packets.push_back(p);
        }
        let got = (self.acc.len() - before) / 2;
        let from = self.decoded_to().unwrap_or(0.0);
        self.fed += got as u64;
        self.crossfade(before);
        let mut wrapped = false;
        if let Some((start, end)) = self.loop_region {
            let to = from + got as f64 / rate;
            // The end passed in these frames, or the file ended before it (inside the range,
            // and not straight after going back, for a start past the end).
            let crossed = from < end && to >= end;
            let file_ended = !more && from < end && to > start && self.fed > self.stretch.0;
            if crossed || file_ended {
                let fade = (LOOP_FADE * rate) as usize;
                self.fade.clear();
                self.faded = 0;
                if crossed {
                    let keep = (((end - from) * rate).round() as usize).min(got);
                    let tail = before + keep * 2;
                    self.fade.extend_from_slice(&self.acc[tail..(tail + fade * 2).min(self.acc.len())]);
                    self.acc.truncate(tail);
                    self.fed -= (got - keep) as u64;
                    // The end fell on the end of a packet: what follows is in the next ones.
                    if let Some(d) = &mut self.decoder {
                        let mut next = Vec::new();
                        while self.fade.len() < fade * 2 && matches!(d.next_stereo(&mut next), Ok(true)) {
                            self.fade.append(&mut next);
                        }
                        self.fade.truncate(fade * 2);
                    }
                }
                if self.fade.len() < fade * 2 {
                    // Nothing (or too little) after the end to fade out under the start, as
                    // at the end of the file: the end fades out, and the start in.
                    let n = fade.min(self.acc.len() / 2);
                    let at = self.acc.len() - n * 2;
                    for (i, fr) in self.acc[at..].chunks_exact_mut(2).enumerate() {
                        let g = ((i as f32 + 0.5) / n as f32 * std::f32::consts::FRAC_PI_2).cos();
                        fr[0] *= g;
                        fr[1] *= g;
                    }
                    self.fade.clear();
                    self.fade.resize(fade * 2, 0.0);
                }
                wrapped = self.wrap(start);
            }
        }
        let more = more || wrapped;
        // Looping, the last frames wait here: if the file ends next, they fade out.
        let hold = if self.loop_region.is_some() { (LOOP_FADE * rate) as usize } else { 0 };
        self.resample(!more, hold);
        if !more {
            self.decoder_done = true;
            if self.pending.is_empty() {
                self.shared.eof.store(true, Ordering::Release);
            }
        }
        true
    }

    /// Moves decoded frames on, through the resampler if there is one; all of them if
    /// `last`, else all but the last `hold`.
    fn resample(&mut self, last: bool, hold: usize) {
        let hold = if last { 0 } else { hold * 2 };
        let Some(rs) = &mut self.resampler else {
            let n = self.acc.len().saturating_sub(hold);
            self.pending.extend(self.acc.drain(..n));
            return;
        };
        let mut out = vec![0.0f32; rs.output_frames_max() * 2];
        let mut run = |input: &[f32], partial: Option<usize>, pending: &mut Vec<f32>| {
            let n_in = input.len() / 2;
            let (Ok(inp), Ok(mut outp)) = (
                InterleavedSlice::new(input, 2, n_in),
                InterleavedSlice::new_mut(&mut out, 2, rs.output_frames_max()),
            ) else {
                return;
            };
            let idx = partial.map(|p| Indexing::new().partial_len(p));
            match rs.process_into_buffer(&inp, &mut outp, idx.as_ref()) {
                Ok((_, written)) => pending.extend_from_slice(&out[..written * 2]),
                Err(e) => log::error!("resample: {e}"),
            }
        };
        let chunk = RESAMPLE_CHUNK * 2;
        while self.acc.len() >= chunk + hold {
            run(&self.acc[..chunk], None, &mut self.pending);
            self.acc.drain(..chunk);
        }
        if last {
            let rem = self.acc.len() / 2;
            self.acc.resize(chunk, 0.0);
            run(&self.acc, Some(rem), &mut self.pending);
            // Flush the resampler's internal delay.
            self.acc.fill(0.0);
            run(&self.acc, Some(0), &mut self.pending);
            self.acc.clear();
        }
    }
}

/// Audio-thread source node: 0 inputs, 1 stereo output, and MIDI out for MIDI files.
pub struct PlayerSource {
    ring: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    varispeed: Varispeed,
    midi: MidiFeed,
}

/// The rate shift: a sinc resampler between the ring and the output whose ratio follows the
/// rate, so a change is heard at once and glides (no rebuild). It stays out of the path
/// until the rate first leaves 1, so normal playback stays bit-exact, and is taken out again
/// at the next seek or track while the rate is 1. Coming in mid-play, it is first run over
/// the audio just played, so it carries on from there without a gap.
struct Varispeed {
    resampler: Async<f32>,
    active: bool,
    rate: f32,
    input: Vec<f32>,
    /// Resampled frames, interleaved; `pos..len` are still to be played.
    output: Vec<f32>,
    pos: usize,
    len: usize,
    /// Resampled frames still to be dropped (what priming produced of audio already heard).
    skip: usize,
    /// The last frames played at 1×, interleaved, oldest first once full.
    history: Vec<f32>,
    history_at: usize,
    history_len: usize,
    /// History frames priming left over (it feeds whole chunks); they go in before the ring.
    carry: usize,
}

impl Varispeed {
    fn new() -> Self {
        let max = *RATE_RANGE.end() as f64;
        let resampler = Async::<f32>::new_sinc(1.0, max, &SincInterpolationParameters::default(), VARISPEED_CHUNK, 2, FixedAsync::Output)
            .expect("varispeed resampler");
        let input = vec![0.0; resampler.input_frames_max() * 2];
        let output = vec![0.0; resampler.output_frames_max() * 2];
        let history = vec![0.0; VARISPEED_HISTORY * 2];
        Self { resampler, active: false, rate: 1.0, input, output, pos: 0, len: 0, skip: 0, history, history_at: 0, history_len: 0, carry: 0 }
    }

    fn reset(&mut self) {
        self.resampler.reset();
        self.active = false;
        self.rate = 1.0;
        self.pos = 0;
        self.len = 0;
        self.skip = 0;
        self.history_len = 0;
        self.carry = 0;
    }

    /// Notes frames played without the resampler.
    fn record(&mut self, l: &[f32], r: &[f32]) {
        for (a, b) in l.iter().zip(r) {
            self.history[self.history_at * 2] = *a;
            self.history[self.history_at * 2 + 1] = *b;
            self.history_at = (self.history_at + 1) % VARISPEED_HISTORY;
        }
        self.history_len = (self.history_len + l.len()).min(VARISPEED_HISTORY);
    }

    /// Brings the resampler in: runs it (at 1×) over the frames just played and drops what
    /// that produces, plus its delay, so the next frame out follows the last one played.
    fn activate(&mut self) {
        self.active = true;
        self.resampler.reset();
        if self.history_len < VARISPEED_HISTORY {
            return;
        }
        let (mut fed, mut produced) = (0, 0);
        while fed < VARISPEED_HISTORY {
            let need = self.resampler.input_frames_next();
            if fed + need > VARISPEED_HISTORY {
                break;
            }
            for i in 0..need {
                let k = (self.history_at + fed + i) % VARISPEED_HISTORY;
                self.input[i * 2] = self.history[k * 2];
                self.input[i * 2 + 1] = self.history[k * 2 + 1];
            }
            let frames = self.output.len() / 2;
            let (Ok(inp), Ok(mut out)) =
                (InterleavedSlice::new(&self.input[..need * 2], 2, need), InterleavedSlice::new_mut(&mut self.output, 2, frames))
            else {
                break;
            };
            let Ok((read, written)) = self.resampler.process_into_buffer(&inp, &mut out, None) else { break };
            fed += read;
            produced += written;
        }
        // The rest of the history goes in ahead of the ring. Frame `i` in comes out at
        // `i + delay`; the next one wanted is the first from the ring.
        self.carry = VARISPEED_HISTORY - fed;
        // (Its first chunk takes one frame more, which the delay leaves out.)
        self.skip = (VARISPEED_HISTORY + self.resampler.output_delay()).saturating_sub(produced + 1);

        self.pos = 0;
        self.len = 0;
    }

    /// Fills `o[..n]` from the ring at `rate`; returns how many output frames were filled
    /// and how many ring frames that used.
    fn run(&mut self, ring: &mut rtrb::Consumer<f32>, rate: f32, o: &mut StereoBuf, n: usize) -> (usize, usize) {
        let (mut done, mut used) = (0, 0);
        while done < n {
            if self.pos == self.len {
                // Priming's leftovers are audio already heard, at 1×; the new rate starts
                // after them so the join lines up.
                if rate != self.rate && self.skip == 0 {
                    self.rate = rate;
                    // The ratio is output over input, relative to the 1:1 it was made with.
                    let _ = self.resampler.set_resample_ratio_relative(1.0 / rate as f64, true);
                }
                let need = self.resampler.input_frames_next();
                let carried = self.carry.min(need);
                let Ok(chunk) = ring.read_chunk((need - carried) * 2) else { break };
                for i in 0..carried {
                    let k = (self.history_at + VARISPEED_HISTORY - self.carry + i) % VARISPEED_HISTORY;
                    self.input[i * 2] = self.history[k * 2];
                    self.input[i * 2 + 1] = self.history[k * 2 + 1];
                }
                self.carry -= carried;
                let (a, b) = chunk.as_slices();
                let at = carried * 2;
                self.input[at..at + a.len()].copy_from_slice(a);
                self.input[at + a.len()..at + a.len() + b.len()].copy_from_slice(b);
                chunk.commit_all();
                let frames = self.output.len() / 2;
                let (Ok(inp), Ok(mut out)) =
                    (InterleavedSlice::new(&self.input[..need * 2], 2, need), InterleavedSlice::new_mut(&mut self.output, 2, frames))
                else {
                    break;
                };
                match self.resampler.process_into_buffer(&inp, &mut out, None) {
                    Ok((read, written)) => {
                        used += read.saturating_sub(carried);
                        self.pos = 0;
                        self.len = written;
                    }
                    Err(_) => break,
                }
            }
            if self.skip > 0 {
                let k = self.skip.min(self.len - self.pos);
                self.pos += k;
                self.skip -= k;
                continue;
            }
            let k = (n - done).min(self.len - self.pos);
            for (i, fr) in self.output[self.pos * 2..(self.pos + k) * 2].as_chunks::<2>().0.iter().enumerate() {
                o.l[done + i] = fr[0];
                o.r[done + i] = fr[1];
            }
            self.pos += k;
            done += k;
        }
        (done, used)
    }
}

impl Processor for PlayerSource {
    fn process_midi(
        &mut self,
        n: usize,
        inputs: &mut [StereoBuf],
        outputs: &mut [StereoBuf],
        _midi_in: &[MidiEvent],
        midi_out: &mut MidiBuf,
    ) {
        self.process(n, inputs, outputs);
        let rate = f32::from_bits(self.shared.rate.load(Ordering::Acquire));
        self.midi.run(n, midi_out, rate);
    }

    fn process(&mut self, n: usize, _inputs: &mut [StereoBuf], outputs: &mut [StereoBuf]) {
        let s = &self.shared;
        if s.flush.load(Ordering::Acquire) {
            let avail = self.ring.slots();
            if let Ok(c) = self.ring.read_chunk(avail) {
                c.commit_all();
            }
            s.frames_played.store(0, Ordering::Release);
            s.flush.store(false, Ordering::Release);
            self.varispeed.reset();
        }
        let o = &mut outputs[0];
        if !s.playing.load(Ordering::Acquire) {
            o.clear(n);
            return;
        }
        let rate = f32::from_bits(s.rate.load(Ordering::Acquire));
        if rate != 1.0 && !self.varispeed.active {
            self.varispeed.activate();
        }
        if self.varispeed.active {
            let (m, used) = self.varispeed.run(&mut self.ring, rate, o, n);
            o.l[m..n].fill(0.0);
            o.r[m..n].fill(0.0);
            s.frames_played.fetch_add(used as u64, Ordering::AcqRel);
            if m < n && s.eof.load(Ordering::Acquire) {
                s.playing.store(false, Ordering::Release);
                s.ended.store(true, Ordering::Release);
                s.finished.store(true, Ordering::Release);
            }
            return;
        }
        let m = (self.ring.slots() / 2).min(n);
        if let Ok(chunk) = self.ring.read_chunk(m * 2) {
            let (a, b) = chunk.as_slices();
            for (k, fr) in a.chunks_exact(2).chain(b.chunks_exact(2)).enumerate() {
                o.l[k] = fr[0];
                o.r[k] = fr[1];
            }
            chunk.commit_all();
        }
        self.varispeed.record(&o.l[..m], &o.r[..m]);
        o.l[m..n].fill(0.0);
        o.r[m..n].fill(0.0);
        s.frames_played.fetch_add(m as u64, Ordering::AcqRel);
        if m < n && s.eof.load(Ordering::Acquire) {
            s.playing.store(false, Ordering::Release);
            s.ended.store(true, Ordering::Release);
            s.finished.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source playing a 1 kHz sine from a full ring at `rate`; returns the output and the
    /// position it reports.
    fn play(rate: f32, blocks: usize) -> (Vec<f32>, f64) {
        play_changing(rate, rate, 0, blocks)
    }

    /// Like [`play`], switching from `from` to `to` after `switch` blocks.
    fn play_changing(from: f32, to: f32, switch: usize, blocks: usize) -> (Vec<f32>, f64) {
        play_with(blocks, |b| if b < switch { from } else { to })
    }

    fn play_with(blocks: usize, rate_at: impl Fn(usize) -> f32) -> (Vec<f32>, f64) {
        let rate = rate_at(0);
        let sr = 48_000.0;
        let (mut prod, cons) = rtrb::RingBuffer::new(sr as usize * 4);
        for i in 0..sr as usize * 2 {
            let v = (i as f32 * 1000.0 * std::f32::consts::TAU / sr).sin() * 0.5;
            prod.push(v).unwrap();
            prod.push(v).unwrap();
        }
        let shared = Arc::new(Shared { rate: AtomicU32::new(rate.to_bits()), ..Default::default() });
        shared.playing.store(true, Ordering::Release);
        let mut src = PlayerSource { ring: cons, shared: shared.clone(), varispeed: Varispeed::new(), midi: MidiSong::new().feed(sr) };
        let mut out = vec![StereoBuf::new()];
        let mut l = Vec::new();
        for b in 0..blocks {
            shared.rate.store(rate_at(b).to_bits(), Ordering::Release);
            src.process(512, &mut [], &mut out);
            l.extend_from_slice(&out[0].l[..512]);
        }
        (l, shared.frames_played.load(Ordering::Acquire) as f64 / sr as f64)
    }

    /// Upward zero crossings per second, skipping the resampler's start.
    fn frequency(l: &[f32]) -> f32 {
        let l = &l[4800..];
        let ups = l.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f32 / (l.len() as f32 / 48_000.0)
    }

    #[test]
    fn rate_one_is_untouched() {
        let (l, pos) = play(1.0, 50);
        let expect: Vec<f32> = (0..l.len()).map(|i| (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5).collect();
        assert_eq!(l, expect);
        assert_eq!(pos, 50.0 * 512.0 / 48_000.0);
    }

    /// The first change away from 1× brings the resampler in without a gap or a jump: right
    /// after the switch the output still matches the sine played at 1×.
    #[test]
    fn switching_the_rate_on_is_seamless() {
        for rate in [1.0001f32, 1.01, 1.5, 0.5] {
            let (l, _) = play_changing(1.0, rate, 10, 20);
            let expect = |i: usize| (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            let worst = (5100..=5121).map(|i| (l[i] - expect(i)).abs()).fold(0.0f32, f32::max);
            assert!(worst < 0.002, "rate {rate}: off by {worst} at the switch");
            // From there the pitch glides to the new rate without a step.
            let slope = 0.5 * std::f32::consts::TAU * 1000.0 * rate.max(1.0) / 48_000.0;
            let jump = l[5100..5600].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(jump < slope * 1.05, "rate {rate}: step of {jump}");
            let quiet = l[5120..5120 + 512].iter().filter(|v| v.abs() < 1e-3).count();
            assert!(quiet < 8, "rate {rate}: {quiet} silent samples after the switch");
        }
    }

    /// Dragging the rate: it changes every block, and the sound glides.
    #[test]
    fn rate_glides_while_dragged() {
        let (l, pos) = play_with(100, |b| if b < 10 { 1.0 } else { (1.0 + (b - 10) as f32 * 0.01).min(1.8) });
        // At 1.8× the sine is 1.8 kHz: at most 0.118 per sample.
        let worst = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.125, "jump of {worst}");
        assert!(pos > 100.0 * 512.0 / 48_000.0 * 1.3);
    }

    #[test]
    fn rate_shifts_speed_and_pitch_together() {
        for rate in [0.5f32, 1.25, 2.0] {
            let (l, pos) = play(rate, 80);
            let played = 80.0 * 512.0 / 48_000.0;
            assert!((pos / played - rate as f64).abs() < 0.02, "rate {rate}: position {pos} after {played} s");
            let f = frequency(&l);
            assert!((f / 1000.0 - rate).abs() < 0.02, "rate {rate}: {f} Hz");
            let peak = l[4800..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((peak - 0.5).abs() < 0.02, "rate {rate}: peak {peak}");
        }
    }

    /// Waits up to 5 s for `f` to hold.
    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A 32-bit float stereo WAV at 48 kHz, `secs` long, each sample the time it is at.
    fn ramp_wav(name: &str, secs: f64) -> PathBuf {
        ramp_wav_at(name, secs, 48_000)
    }

    fn ramp_wav_at(name: &str, secs: f64, rate: u32) -> PathBuf {
        let frames = (secs * rate as f64) as u32;
        let mut b = Vec::new();
        let data = frames * 8;
        b.extend(b"RIFF");
        b.extend((36 + data).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(3u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend((rate * 8).to_le_bytes());
        b.extend(8u16.to_le_bytes());
        b.extend(32u16.to_le_bytes());
        b.extend(b"data");
        b.extend(data.to_le_bytes());
        for i in 0..frames {
            let v = i as f32 / rate as f32;
            b.extend(v.to_le_bytes());
            b.extend(v.to_le_bytes());
        }
        let dir = std::env::temp_dir().join(format!("nodeplayer-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ramp.wav");
        std::fs::write(&path, b).unwrap();
        path
    }

    /// Plays `frames` frames from the source as the audio thread would, waiting for the
    /// decoder to keep up; returns the left channel and the position after each block.
    fn pull(src: &mut PlayerSource, player: &Player, frames: usize) -> (Vec<f32>, Vec<f64>) {
        let mut out = vec![StereoBuf::new()];
        let (mut l, mut pos) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + Duration::from_secs(10);
        while l.len() < frames {
            assert!(Instant::now() < deadline, "the decoder keeps up");
            if src.shared.flush.load(Ordering::Acquire) {
                // The audio thread lets the old audio go.
                src.process(512, &mut [], &mut out);
                continue;
            }
            if src.ring.slots() < 4096 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            src.process(512, &mut [], &mut out);
            l.extend_from_slice(&out[0].l[..512]);
            pos.push(player.position());
        }
        (l, pos)
    }

    #[test]
    fn seeking_lands_on_the_exact_time() {
        let path = ramp_wav("seek", 1.0);
        let player = Player::new(48_000);
        let mut src = player.make_source();
        player.load(&path);
        player.seek(0.3);
        // The seek is carried out after the load: skip what came before it.
        assert!(wait_for(|| player.position() == 0.3 && src.ring.slots() > 4096));
        let (l, _) = pull(&mut src, &player, 512);
        assert!((l[0] - 0.3).abs() < 1.0 / 48_000.0, "starts at {}", l[0]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The selection loop goes back to its start without a gap, and the position follows.
    #[test]
    fn the_selection_loops_seamlessly() {
        let path = ramp_wav("loop", 1.0);
        let player = Player::new(48_000);
        let mut src = player.make_source();
        player.load(&path);
        player.set_loop(Some((0.25, 0.5)));
        assert!(wait_for(|| player.status().path.is_some()));
        let (l, pos) = pull(&mut src, &player, 48_000 + 512);
        let step = 1.0 / 48_000.0;
        // Up to the end as it is, then from the start again: one frame to the next, the
        // ramp goes on or jumps back by the loop's length. The fade is over in 5 ms.
        let fade = 240;
        for (i, w) in l.windows(2).enumerate() {
            let j = i + 1;
            let in_fade = j >= 24_000 && (j - 24_000) % 12_000 <= fade;
            if !in_fade {
                assert!((w[1] - w[0] - step).abs() < 1e-5, "frame {j}: {} → {}", w[0], w[1]);
            }
        }
        // The fade starts from what followed the end.
        assert!((l[24_000] - (0.25 * 0.0 + 0.5 * 1.0)).abs() < 0.01, "first after the end: {}", l[24_000]);
        assert!((l[24_000 + fade] - (0.25 + fade as f32 * step)).abs() < 1e-4);
        // Played 1 s: 0.5 s to the end, then twice round the 0.25 s loop.
        let last = *pos.last().unwrap();
        assert!((0.25..0.5).contains(&last), "position {last}");
        assert!(pos.iter().all(|p| *p < 0.5 + 0.02), "the position stays inside");
        // Each sample is its time: the position is that of the last one played.
        // (Not in the fade, where it is the two mixed.)
        for (b, p) in pos.iter().enumerate().filter(|(_, p)| !(0.25..0.256).contains(*p)) {
            let heard = l[b * 512 + 511] as f64 + step as f64;
            assert!((p - heard).abs() < 1e-4, "block {b}: position {p}, playing {heard}");
        }
        assert!(player.is_playing() && !player.take_ended());

        // Whole: an end past the file's loops it from the start.
        player.set_loop(Some((0.0, f64::INFINITY)));
        // (What was decoded ahead for the old range plays first: up to half a second more.)
        let (l, pos) = pull(&mut src, &player, 120_000);
        let backs = pos.windows(2).filter(|w| w[1] < w[0] - 0.5).count();
        assert!((2..=3).contains(&backs), "loops round each second: {backs} times in 2.5 s");
        // Nothing follows the file's end: it fades out, and the start fades in, so there is
        // no step.
        let worst = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.01, "a step of {worst}");
        assert!(!player.take_ended());

        // Off, it plays to the end.
        player.set_loop(None);
        assert!(wait_for(|| {
            let mut out = vec![StereoBuf::new()];
            src.process(512, &mut [], &mut out);
            player.take_ended()
        }));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Through the resampler (a 44.1 kHz file on a 48 kHz device), too.
    #[test]
    fn the_selection_loops_through_the_resampler() {
        let path = ramp_wav_at("loop-44k", 1.0, 44_100);
        let player = Player::new(48_000);
        let mut src = player.make_source();
        player.load(&path);
        player.set_loop(Some((0.25, 0.5)));
        assert!(wait_for(|| player.status().path.is_some()));
        let (l, pos) = pull(&mut src, &player, 48_000 * 2);
        // 0.5 s to the end, then round the 0.25 s loop (23.4 blocks of 512 frames).
        let backs: Vec<usize> = pos.windows(2).enumerate().filter(|(_, w)| w[1] < w[0]).map(|(b, _)| b).collect();
        assert_eq!(backs[0], 46, "first back after 0.5 s");
        assert!(backs.len() >= 6 && backs.windows(2).all(|w| (23..=24).contains(&(w[1] - w[0]))), "goes back each 0.25 s: {backs:?}");
        // Skip the resampler's start, and the fades (the sinc's ringing at the joins too).
        let mut worst = 0.0f64;
        for (b, p) in pos.iter().enumerate().skip(4).filter(|(_, p)| !(0.25..0.256).contains(*p) && **p < 0.499) {
            let heard = l[b * 512 + 511] as f64;
            worst = worst.max((p - heard).abs());
            assert!((p - heard).abs() < 2e-4, "block {b}: position {p}, playing {heard}");
        }
        assert!(pos.iter().all(|p| *p < 0.5 + 0.001), "the position stays inside");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A new end before what is already decoded takes effect at once.
    #[test]
    fn moving_the_end_back_drops_what_was_decoded_past_it() {
        let path = ramp_wav("loop-end", 2.0);
        let player = Player::new(48_000);
        let mut src = player.make_source();
        player.load(&path);
        assert!(wait_for(|| player.status().path.is_some() && src.ring.slots() > 20_000));
        let _ = pull(&mut src, &player, 4096);
        // The ring holds half a second ahead of the position.
        player.set_loop(Some((0.0, 0.2)));
        // The audio thread plays in real time: it can't run ahead of the decoder getting to
        // the command, as this would. The decoder starts over from the position: it asks
        // for what is buffered to go.
        assert!(wait_for(|| src.shared.flush.load(Ordering::Acquire)));
        let (l, _) = pull(&mut src, &player, 24_000);
        assert!(l.iter().all(|v| *v < 0.2 + 1e-3), "nothing from past the end: {}", l.iter().fold(0.0f32, |m, v| m.max(*v)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn midi_files_loop_too() {
        // A note every 0.1 s for 1 s at the default 120 BPM (480 ticks a beat: 96 ticks).
        let mut track = Vec::new();
        for i in 0..10 {
            track.extend([0x00, 0x90, 60 + i, 100, 0x60, 0x80, 60 + i, 0]);
        }
        track.extend([0x00, 0xff, 0x2f, 0x00]);
        let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk".to_vec();
        bytes.extend((track.len() as u32).to_be_bytes());
        bytes.extend(track);
        let dir = std::env::temp_dir().join(format!("nodeplayer-midi-loop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.mid");
        std::fs::write(&path, bytes).unwrap();

        let player = Player::new(48_000);
        let mut src = player.make_source();
        player.load(&path);
        assert!(wait_for(|| player.status().duration.is_some()), "the file loads");
        assert!((player.status().duration.unwrap() - 1.0).abs() < 1e-6);
        player.set_loop(Some((0.2, 0.4)));
        // What the player sends, with the times it gives each message: the audio thread
        // plays each at its time. (Read straight from the queue, so a busy machine stalling
        // the threads now and then changes nothing here.)
        let (mut seen, mut notes, mut at) = (Vec::new(), Vec::new(), Vec::new());
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            for (time, msg) in src.midi.take_queued() {
                if msg & 0xf0 == 0x90 && msg >> 16 & 0x7f > 0 {
                    notes.push((msg >> 8 & 0x7f) as u8);
                    at.push(time.expect("notes are timed"));
                }
            }
            seen.push(player.position());
            std::thread::sleep(Duration::from_millis(10));
        }
        // The notes up to the end, then those from the start again: 62 and 63 (at 0.2 and
        // 0.3 s) over and over, never 64 (on the end).
        assert_eq!(notes[..4], [60, 61, 62, 63], "{notes:?}");
        assert!(notes[4..].chunks(2).all(|c| c[0] == 62 && c.get(1).is_none_or(|k| *k == 63)), "{notes:?}");
        assert!(notes.len() >= 4 + 2 * 4, "goes round again and again: {notes:?}");
        // A note every 0.1 s all along, the jumps back included.
        let gaps: Vec<f64> = at.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
        assert!(gaps.iter().all(|g| (g - 0.1).abs() < 1e-6), "seconds between notes: {gaps:?}");
        let last = *seen.last().unwrap();
        assert!((0.2..0.45).contains(&last), "position {last}");
        assert!(seen.iter().all(|p| *p < 0.45), "stays inside: {:?}", seen.iter().fold(0.0f64, |m, p| m.max(*p)));
        assert!(player.is_playing() && !player.take_ended());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plays_midi_files_as_songs() {
        // One note, half a second long at the default 120 BPM.
        let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk\0\0\0\x0d".to_vec();
        bytes.extend([0x00, 0x90, 60, 100, 0x83, 0x60, 0x80, 60, 0, 0x00, 0xff, 0x2f, 0x00]);
        let dir = std::env::temp_dir().join(format!("nodeplayer-midi-song-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.mid");
        std::fs::write(&path, bytes).unwrap();

        let player = Player::new(48_000);
        // As when the last track is brought back: paused, where it was.
        player.load(&path);
        player.set_playing(false);
        player.seek(0.25);
        assert!(wait_for(|| player.status().duration.is_some()), "the file loads");
        let status = player.status();
        assert_eq!((status.decoder, status.path.as_deref()), ("MIDI", Some(path.as_path())));
        assert!((status.duration.unwrap() - 0.5).abs() < 1e-6);
        assert!(!player.is_playing());
        assert!((player.position() - 0.25).abs() < 1e-6);

        player.set_playing(true);
        assert!(player.is_playing());
        assert!(wait_for(|| player.take_ended()), "the end is told");
        assert!(player.finished() && !player.is_playing());
        // Played again, it starts over.
        player.set_playing(true);
        assert!(player.is_playing() && player.position() < 0.25);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
