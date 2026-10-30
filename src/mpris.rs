//! Media session integration (MPRIS on D-Bus on Linux, the System Media Transport Controls on
//! Windows, Now Playing on macOS): shows the current track in the desktop's media controls and
//! forwards their play/pause/next/seek requests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

use crate::player::Status;

pub use souvlaki::{MediaControlEvent as Event, SeekDirection};

pub struct Mpris {
    controls: MediaControls,
    rx: Receiver<MediaControlEvent>,
    shown_path: Option<PathBuf>,
    shown_playing: Option<bool>,
    /// Where the position was last reported, to notice seeks and refresh it now and then.
    reported: (f64, Instant),
    cover_file: Option<PathBuf>,
}

impl Mpris {
    /// `hwnd` is the main window, which Windows ties the media session to; unused elsewhere.
    pub fn new(ctx: egui::Context, hwnd: Option<crate::plugins::WindowId>) -> Option<Self> {
        if cfg!(windows) && hwnd.is_none() {
            return None;
        }
        let hwnd = hwnd.map(|h| h as *mut std::ffi::c_void);
        let config = PlatformConfig { dbus_name: "nodeplayer", display_name: "nodeplayer", hwnd };
        let mut controls = MediaControls::new(config).map_err(|e| log::warn!("media session: {e:?}")).ok()?;
        let (tx, rx) = crossbeam_channel::unbounded();
        controls
            .attach(move |e| {
                let _ = tx.send(e);
                // Wake the app even while its window is hidden.
                ctx.request_repaint();
            })
            .map_err(|e| log::warn!("media session: {e:?}"))
            .ok()?;
        Some(Self {
            controls,
            rx,
            shown_path: None,
            shown_playing: None,
            reported: (0.0, Instant::now()),
            cover_file: None,
        })
    }

    pub fn events(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Pushes metadata when the track changes and the playback state when it changes, when
    /// the position jumps, or every few seconds.
    pub fn update(&mut self, status: &Status, playing: bool, position: f64) {
        if status.path != self.shown_path {
            self.shown_path = status.path.clone();
            self.set_metadata(status);
            self.shown_playing = None;
        }
        let (last_pos, at) = self.reported;
        let expected = if self.shown_playing == Some(true) { last_pos + at.elapsed().as_secs_f64() } else { last_pos };
        let stale = at.elapsed() > Duration::from_secs(5);
        if self.shown_playing != Some(playing) || (position - expected).abs() > 1.0 || stale {
            let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
            let playback = match (&status.path, playing) {
                (None, _) => MediaPlayback::Stopped,
                (Some(_), true) => MediaPlayback::Playing { progress },
                (Some(_), false) => MediaPlayback::Paused { progress },
            };
            if let Err(e) = self.controls.set_playback(playback) {
                log::debug!("media session: {e:?}");
            }
            self.shown_playing = Some(playing);
            self.reported = (position, Instant::now());
        }
    }

    fn set_metadata(&mut self, status: &Status) {
        let cover = status.path.as_deref().and_then(|p| self.cover_url(status, p));
        let title = status.title();
        let meta = MediaMetadata {
            title: title.as_deref(),
            artist: status.tags.artist.as_deref(),
            album: status.tags.album.as_deref(),
            cover_url: cover.as_deref(),
            duration: status.duration.map(Duration::from_secs_f64),
        };
        if let Err(e) = self.controls.set_metadata(meta) {
            log::debug!("media session: {e:?}");
        }
    }

    /// A `file://` URL for the cover: the embedded picture written to the cache, or an image
    /// next to the track (cover.jpg, folder.png, ...).
    fn cover_url(&mut self, status: &Status, track: &Path) -> Option<String> {
        if let Some(old) = self.cover_file.take() {
            let _ = std::fs::remove_file(old);
        }
        let file = match &status.tags.cover {
            Some(cover) => {
                let (mime, data) = &**cover;
                let ext = match mime.as_str() {
                    "image/png" => "png",
                    "image/webp" => "webp",
                    "image/gif" => "gif",
                    _ => "jpg",
                };
                let dir = cache_dir()?;
                std::fs::create_dir_all(&dir).ok()?;
                // A new name per track, since media widgets cache images by URL.
                let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
                let path = dir.join(format!("cover-{stamp}.{ext}"));
                std::fs::write(&path, data).ok()?;
                self.cover_file = Some(path.clone());
                path
            }
            None => folder_image(track.parent()?)?,
        };
        Some(format!("file://{}", file.display()))
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        if let Some(f) = self.cover_file.take() {
            let _ = std::fs::remove_file(f);
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    crate::dirs::cache()
}

pub(crate) fn folder_image(dir: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart"];
    const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        let stem = p.file_stem().and_then(|s| s.to_str()).map(str::to_lowercase);
        let ext = p.extension().and_then(|s| s.to_str()).map(str::to_lowercase);
        matches!((stem, ext), (Some(s), Some(e)) if NAMES.contains(&s.as_str()) && EXTS.contains(&e.as_str()))
    })
}
