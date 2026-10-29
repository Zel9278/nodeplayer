//! File playback: a decoder thread (symphonia, or FFmpeg for what it can't open, + rubato) feeding a ring buffer that the
//! `Player` source node drains on the audio thread.

use std::path::{Path, PathBuf};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
use symphonia::core::units::Time;

use crate::engine::{Processor, StereoBuf};

const RING_SECONDS: f32 = 0.5;
const RESAMPLE_CHUNK: usize = 1024;

/// Playback rate shift: speed and pitch change together, like a tape played faster.
pub const RATE_RANGE: RangeInclusive<f32> = 0.5..=2.0;
/// Output frames per varispeed resampler call.
const VARISPEED_CHUNK: usize = 64;
/// Frames played at 1× kept to prime the resampler with when the rate first changes.
const VARISPEED_HISTORY: usize = 512;

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
}

#[derive(Clone, Default)]
pub struct Status {
    pub path: Option<PathBuf>,
    pub duration: Option<f64>,
    pub source_rate: u32,
    pub channels: usize,
    pub error: Option<String>,
    pub tags: Tags,
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
    Seek(f64),
    Attach(rtrb::Producer<f32>),
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    status: Arc<Mutex<Status>>,
    device_rate: u32,
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
        };
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || worker.run())
            .expect("spawn decoder thread");
        Self { tx, shared, status, device_rate }
    }

    /// Creates the audio-thread source. Only the most recently created source receives audio.
    pub fn make_source(&self) -> PlayerSource {
        let len = (self.device_rate as f32 * RING_SECONDS) as usize * 2;
        let (prod, cons) = rtrb::RingBuffer::new(len);
        let _ = self.tx.send(Cmd::Attach(prod));
        PlayerSource { ring: cons, shared: self.shared.clone(), varispeed: Varispeed::new() }
    }

    /// Playback rate shift, within [`RATE_RANGE`]; 1 is normal speed.
    pub fn set_rate(&self, rate: f32) {
        let r = rate.clamp(*RATE_RANGE.start(), *RATE_RANGE.end());
        self.shared.rate.store(r.to_bits(), Ordering::Release);
    }

    pub fn rate(&self) -> f32 {
        f32::from_bits(self.shared.rate.load(Ordering::Acquire))
    }

    pub fn load(&self, path: &Path) {
        // Clear a stale end-of-file now so the source doesn't stop again before the decoder
        // gets to the command.
        self.shared.eof.store(false, Ordering::Release);
        self.shared.ended.store(false, Ordering::Release);
        self.shared.finished.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Load(path.to_path_buf()));
        self.shared.playing.store(true, Ordering::Release);
    }

    pub fn seek(&self, secs: f64) {
        self.shared.eof.store(false, Ordering::Release);
        self.shared.finished.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Seek(secs.max(0.0)));
    }

    /// Starting again after the file played to the end restarts it from the top.
    pub fn set_playing(&self, on: bool) {
        if on && self.shared.finished.load(Ordering::Acquire) {
            self.seek(0.0);
        }
        self.shared.playing.store(on, Ordering::Release);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire)
    }

    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.base_secs.load(Ordering::Acquire))
            + self.shared.frames_played.load(Ordering::Acquire) as f64 / self.device_rate as f64
    }

    /// True once after the current file finished playing to the end.
    pub fn take_ended(&self) -> bool {
        self.shared.ended.swap(false, Ordering::AcqRel)
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
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
}

struct SymDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
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
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("not an audio track"))?;
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())?;
        Ok(Self { format, decoder, track_id, sample_rate, channels })
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
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    buf.copy_to_vecs_planar::<f32>(&mut planes);
                    let (l, r) = match planes.len() {
                        0 => return Ok(true),
                        1 => (&planes[0], &planes[0]),
                        _ => (&planes[0], &planes[1]),
                    };
                    out.extend(l.iter().zip(r).flat_map(|(a, b)| [*a, *b]));
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
        self.format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })?;
        self.decoder.reset();
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
            Cmd::Load(path) => {
                log::info!("load {}", path.display());
                self.reset_stream();
                match Decoder::open(&path) {
                    Ok(mut d) => {
                        *self.status.lock().unwrap() = Status {
                            path: Some(path.clone()),
                            duration: d.duration_secs(),
                            source_rate: d.sample_rate(),
                            channels: d.channels(),
                            error: None,
                            tags: d.tags(),
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
                self.reset_stream();
                self.decoder_done = false;
                self.shared.base_secs.store(at.to_bits(), Ordering::Release);
            }
        }
    }

    /// Drops everything buffered between the decoder and the speakers.
    fn reset_stream(&mut self) {
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
        let more = match d.next_stereo(&mut self.acc) {
            Ok(m) => m,
            Err(e) => {
                log::error!("decode failed: {e}");
                false
            }
        };
        self.resample(!more);
        if !more {
            self.decoder_done = true;
            if self.pending.is_empty() {
                self.shared.eof.store(true, Ordering::Release);
            }
        }
        true
    }

    fn resample(&mut self, last: bool) {
        let Some(rs) = &mut self.resampler else {
            self.pending.append(&mut self.acc);
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
        while self.acc.len() >= chunk {
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

/// Audio-thread source node: 0 inputs, 1 stereo output.
pub struct PlayerSource {
    ring: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    varispeed: Varispeed,
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
        let mut src = PlayerSource { ring: cons, shared: shared.clone(), varispeed: Varispeed::new() };
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
}
