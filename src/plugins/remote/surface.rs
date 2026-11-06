//! A latest-frame image slot, independent of the realtime audio mapping.
//!
//! FREE/READY -> WRITING -> READY -> READING -> FREE. A producer may replace an
//! unread frame, but never a frame being copied by the consumer. All mapped fields
//! and payload words are atomic: even malformed peer metadata cannot create Rust aliases
//! to ordinary mutable memory. This is a native-endian, same-executable ABI.

use anyhow::{Context, Result, bail, ensure};
use memmap2::{MmapOptions, MmapRaw};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use crate::plugins::surface::{EditorFrame, EditorPopup, MAX_DIMENSION, MAX_PIXELS, MAX_POPUPS};

const MAGIC: u64 = 0x4e50_5355_5246_3031;
// Version 4 publishes display density with the pixels and child-popup geometry.
const VERSION: u32 = 4;
const RGBA8: u32 = 1;
const MAX_BYTES: usize = MAX_PIXELS * 4;
const FREE: u32 = 0;
const WRITING: u32 = 1;
const READY: u32 = 2;
const READING: u32 = 3;

#[repr(C, align(64))]
struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    header_bytes: AtomicU32,
    mapping_bytes: AtomicU64,
    capacity: AtomicU64,
    generation: AtomicU64,
    token: AtomicU64,
    state: AtomicU32,
    format: AtomicU32,
    sequence: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    native_width: AtomicU32,
    native_height: AtomicU32,
    native_units_per_point_bits: AtomicU32,
    byte_len: AtomicU64,
    popup_count: AtomicU32,
    popups: [PopupHeader; MAX_POPUPS],
}

const HEADER_BYTES: usize = std::mem::size_of::<Header>();
const MAPPING_BYTES: usize = HEADER_BYTES + MAX_BYTES;
const WORD_BYTES: usize = std::mem::size_of::<u64>();
const _: () = assert!(HEADER_BYTES.is_multiple_of(std::mem::align_of::<AtomicU64>()));
const _: () = assert!(MAX_BYTES.is_multiple_of(WORD_BYTES));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SurfaceDescriptor {
    pub path: PathBuf,
    pub generation: u64,
    pub token: u64,
}

/// Fields are ordered so that the mapping is unmapped before cleanup tries to
/// remove the file. Both successful peers retry cleanup on Drop: on Windows an
/// early unlink can fail until the last process has unmapped the file.
pub struct FrameMap {
    map: MmapRaw,
    cleanup: Cleanup,
    generation: u64,
    token: u64,
    last_sequence: AtomicU64,
}

struct Cleanup {
    path: PathBuf,
    remove: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.remove {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Releases only the state it actually owns, including on an invalid frame.
/// A broken peer changing the state cannot make this guard free its newer write.
struct Lease<'a> {
    state: &'a AtomicU32,
    owned: u32,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let _ = self
            .state
            .compare_exchange(self.owned, FREE, Ordering::Release, Ordering::Relaxed);
    }
}

#[repr(C)]
struct PopupHeader {
    x: AtomicU64,
    y: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    native_width: AtomicU32,
    native_height: AtomicU32,
    interactive: AtomicU32,
}

impl Default for PopupHeader {
    fn default() -> Self {
        Self {
            x: AtomicU64::new(0),
            y: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            native_width: AtomicU32::new(0),
            native_height: AtomicU32::new(0),
            interactive: AtomicU32::new(0),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PopupMetadata {
    x: u64,
    y: u64,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    interactive: u32,
}

#[derive(Clone, PartialEq, Eq)]
struct FrameHeader {
    sequence: u64,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    native_units_per_point_bits: u32,
    byte_len: u64,
    popups: Vec<PopupMetadata>,
}

fn image_len(width: u32, height: u32, native_width: u32, native_height: u32) -> Result<usize> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "invalid shared editor image dimensions"
    );
    ensure!(
        native_width > 0
            && native_height > 0
            && native_width <= MAX_DIMENSION
            && native_height <= MAX_DIMENSION,
        "invalid shared native editor dimensions"
    );
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .context("shared editor image size overflow")?;
    ensure!(
        pixels <= MAX_PIXELS,
        "shared editor image exceeds pixel limit"
    );
    pixels
        .checked_mul(4)
        .context("shared editor image byte count overflow")
}

impl FrameHeader {
    fn read(header: &Header) -> Result<Self> {
        let popup_count = header.popup_count.load(Ordering::Relaxed) as usize;
        ensure!(popup_count <= MAX_POPUPS, "too many shared editor popups");
        Ok(Self {
            sequence: header.sequence.load(Ordering::Relaxed),
            width: header.width.load(Ordering::Relaxed),
            height: header.height.load(Ordering::Relaxed),
            native_width: header.native_width.load(Ordering::Relaxed),
            native_height: header.native_height.load(Ordering::Relaxed),
            native_units_per_point_bits: header.native_units_per_point_bits.load(Ordering::Relaxed),
            byte_len: header.byte_len.load(Ordering::Relaxed),
            popups: header.popups[..popup_count]
                .iter()
                .map(|p| PopupMetadata {
                    x: p.x.load(Ordering::Relaxed),
                    y: p.y.load(Ordering::Relaxed),
                    width: p.width.load(Ordering::Relaxed),
                    height: p.height.load(Ordering::Relaxed),
                    native_width: p.native_width.load(Ordering::Relaxed),
                    native_height: p.native_height.load(Ordering::Relaxed),
                    interactive: p.interactive.load(Ordering::Relaxed),
                })
                .collect(),
        })
    }

    fn validated_len(&self) -> Result<usize> {
        ensure!(self.sequence != 0, "invalid editor frame sequence");
        ensure!(crate::plugins::surface::valid_display_scale(f32::from_bits(self.native_units_per_point_bits)),
            "invalid shared editor display density");
        let mut length = image_len(
            self.width,
            self.height,
            self.native_width,
            self.native_height,
        )?;
        for popup in &self.popups {
            ensure!(
                [popup.x, popup.y].iter().all(|v| {
                    let value = f64::from_bits(*v);
                    value.is_finite() && value.abs() <= 1_000_000.0
                }),
                "invalid shared editor popup origin"
            );
            ensure!(
                popup.interactive <= 1,
                "invalid shared editor popup input flag"
            );
            length = length
                .checked_add(image_len(
                    popup.width,
                    popup.height,
                    popup.native_width,
                    popup.native_height,
                )?)
                .context("shared editor popup byte count overflow")?;
        }
        ensure!(
            length <= MAX_BYTES && self.byte_len == length as u64,
            "invalid shared editor image byte count"
        );
        Ok(length)
    }
}

impl FrameMap {
    pub fn create(generation: u64, token: u64) -> Result<Self> {
        ensure!(
            generation != 0 && token != 0,
            "invalid editor surface identity"
        );
        let (file, cleanup) = create_file()?;
        file.set_len(MAPPING_BYTES as u64)
            .context("size editor surface mapping")?;
        // MmapRaw exposes only pointers, so it never creates a Rust byte slice
        // over memory the peer can change. The descriptor is sent after init.
        let map = MmapOptions::new().len(MAPPING_BYTES).map_raw(&file)?;
        let header = Header {
            magic: AtomicU64::new(0),
            version: AtomicU32::new(VERSION),
            header_bytes: AtomicU32::new(HEADER_BYTES as u32),
            mapping_bytes: AtomicU64::new(MAPPING_BYTES as u64),
            capacity: AtomicU64::new(MAX_BYTES as u64),
            generation: AtomicU64::new(generation),
            token: AtomicU64::new(token),
            state: AtomicU32::new(FREE),
            format: AtomicU32::new(RGBA8),
            sequence: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            native_width: AtomicU32::new(0),
            native_height: AtomicU32::new(0),
            native_units_per_point_bits: AtomicU32::new(1.0f32.to_bits()),
            byte_len: AtomicU64::new(0),
            popup_count: AtomicU32::new(0),
            popups: std::array::from_fn(|_| PopupHeader::default()),
        };
        // SAFETY: mmap's page-aligned address meets Header's alignment and the
        // whole Header fits in the mapping. No other process has its descriptor.
        unsafe { map.as_mut_ptr().cast::<Header>().write(header) };
        let result = Self {
            map,
            cleanup,
            generation,
            token,
            last_sequence: AtomicU64::new(0),
        };
        result.header().magic.store(MAGIC, Ordering::Release);
        Ok(result)
    }

    pub fn open(descriptor: &SurfaceDescriptor) -> Result<Self> {
        ensure!(
            descriptor.generation != 0 && descriptor.token != 0,
            "invalid editor surface identity"
        );
        let file = File::options()
            .read(true)
            .write(true)
            .open(&descriptor.path)
            .context("open editor surface mapping")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() == MAPPING_BYTES as u64,
            "invalid editor surface mapping size"
        );
        // The fixed file length was checked. MmapRaw creates no references to
        // peer-written data, and every offset below uses local bounds.
        let map = MmapOptions::new().len(MAPPING_BYTES).map_raw(&file)?;
        let mut result = Self {
            map,
            cleanup: Cleanup {
                path: descriptor.path.clone(),
                remove: false,
            },
            generation: descriptor.generation,
            token: descriptor.token,
            last_sequence: AtomicU64::new(0),
        };
        result.validate_header()?;
        // Invalid descriptors never gain the right to remove someone else's file.
        result.cleanup.remove = true;
        Ok(result)
    }

    pub fn descriptor(&self) -> SurfaceDescriptor {
        SurfaceDescriptor {
            path: self.cleanup.path.clone(),
            generation: self.generation,
            token: self.token,
        }
    }

    pub fn token(&self) -> u64 {
        self.token
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Capture only after consumption, unless input requires a newer unread image.
    /// A reader's copy is never interrupted, even for an interactive refresh.
    pub fn needs_frame(&self, refresh: bool) -> Result<bool> {
        self.validate_header()?;
        match self.header().state.load(Ordering::Acquire) {
            FREE => Ok(true),
            READY => Ok(refresh),
            WRITING | READING => Ok(false),
            _ => bail!("invalid editor surface ownership state"),
        }
    }

    /// False means the reader is copying (or another publisher owns the slot).
    /// This call never waits for the reader or queues an additional frame.
    pub fn publish(&self, frame: &EditorFrame) -> Result<bool> {
        frame.validate()?;
        self.validate_header()?;
        let h = self.header();
        let state = h.state.load(Ordering::Acquire);
        match state {
            WRITING | READING => return Ok(false),
            FREE | READY => {}
            _ => bail!("invalid editor surface ownership state"),
        }
        if h.state
            .compare_exchange(state, WRITING, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return Ok(false);
        }
        let _lease = Lease {
            state: &h.state,
            owned: WRITING,
        };
        let sequence = h
            .sequence
            .load(Ordering::Relaxed)
            .checked_add(1)
            .context("editor frame sequence exhausted")?;
        h.width.store(frame.width, Ordering::Relaxed);
        h.height.store(frame.height, Ordering::Relaxed);
        h.native_width.store(frame.native_size.0, Ordering::Relaxed);
        h.native_height
            .store(frame.native_size.1, Ordering::Relaxed);
        h.native_units_per_point_bits.store(frame.native_units_per_point.to_bits(), Ordering::Relaxed);
        // Keep the common no-popup path allocation-free. Popup images share the same
        // publication lease so geometry and pixels can never come from different frames.
        let payload = if frame.popups.is_empty() {
            std::borrow::Cow::Borrowed(frame.rgba.as_slice())
        } else {
            let length =
                frame.rgba.len() + frame.popups.iter().map(|p| p.rgba.len()).sum::<usize>();
            let mut bytes = Vec::with_capacity(length);
            bytes.extend_from_slice(&frame.rgba);
            for popup in &frame.popups {
                bytes.extend_from_slice(&popup.rgba);
            }
            std::borrow::Cow::Owned(bytes)
        };
        h.popup_count
            .store(frame.popups.len() as u32, Ordering::Relaxed);
        for (header, popup) in h.popups.iter().zip(&frame.popups) {
            header.x.store(popup.origin[0].to_bits(), Ordering::Relaxed);
            header.y.store(popup.origin[1].to_bits(), Ordering::Relaxed);
            header.width.store(popup.width, Ordering::Relaxed);
            header.height.store(popup.height, Ordering::Relaxed);
            header
                .native_width
                .store(popup.native_size.0, Ordering::Relaxed);
            header
                .native_height
                .store(popup.native_size.1, Ordering::Relaxed);
            header
                .interactive
                .store(u32::from(popup.interactive), Ordering::Relaxed);
        }
        h.byte_len.store(payload.len() as u64, Ordering::Relaxed);
        let (words, tail) = payload.as_chunks::<WORD_BYTES>();
        for (index, bytes) in words.iter().enumerate() {
            self.word(index)
                .store(u64::from_ne_bytes(*bytes), Ordering::Relaxed);
        }
        if !tail.is_empty() {
            let mut bytes = [0; WORD_BYTES];
            bytes[..tail.len()].copy_from_slice(tail);
            self.word(words.len())
                .store(u64::from_ne_bytes(bytes), Ordering::Relaxed);
        }
        self.validate_header()?;
        h.sequence.store(sequence, Ordering::Relaxed);
        ensure!(
            h.state
                .compare_exchange(WRITING, READY, Ordering::Release, Ordering::Relaxed)
                .is_ok(),
            "editor surface ownership changed during publication"
        );
        Ok(true)
    }

    pub fn take_latest(&self) -> Result<Option<EditorFrame>> {
        self.validate_header()?;
        let h = self.header();
        match h
            .state
            .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => {}
            Err(FREE | WRITING | READING) => return Ok(None),
            Err(_) => bail!("invalid editor surface ownership state"),
        }
        let _lease = Lease {
            state: &h.state,
            owned: READING,
        };
        let metadata = FrameHeader::read(h)?;
        let length = metadata.validated_len()?;
        ensure!(
            metadata.sequence > self.last_sequence.load(Ordering::Acquire),
            "stale editor frame sequence"
        );
        let mut rgba: Vec<u8> = Vec::new();
        rgba.try_reserve_exact(length)
            .context("allocate captured editor image")?;
        // SAFETY: the fallible reserve provides at least `length` writable bytes owned
        // solely by this Vec. Initialize them before publishing its length. A bulk fill
        // avoids Vec::resize's per-byte initialization loop in unoptimized builds.
        unsafe {
            rgba.as_mut_ptr().write_bytes(0, length);
            rgba.set_len(length);
        }
        let (words, tail) = rgba.as_chunks_mut::<WORD_BYTES>();
        for (index, bytes) in words.iter_mut().enumerate() {
            *bytes = self.word(index).load(Ordering::Relaxed).to_ne_bytes();
        }
        if !tail.is_empty() {
            let bytes = self.word(words.len()).load(Ordering::Relaxed).to_ne_bytes();
            tail.copy_from_slice(&bytes[..tail.len()]);
        }
        self.validate_header()?;
        ensure!(
            h.state.load(Ordering::Acquire) == READING && metadata == FrameHeader::read(h)?,
            "editor frame changed during read"
        );
        let root_len = image_len(
            metadata.width,
            metadata.height,
            metadata.native_width,
            metadata.native_height,
        )?;
        let mut cursor = root_len;
        let mut popups = Vec::with_capacity(metadata.popups.len());
        for popup in metadata.popups {
            let length = image_len(
                popup.width,
                popup.height,
                popup.native_width,
                popup.native_height,
            )?;
            popups.push(EditorPopup {
                origin: [f64::from_bits(popup.x), f64::from_bits(popup.y)],
                width: popup.width,
                height: popup.height,
                native_size: (popup.native_width, popup.native_height),
                rgba: rgba[cursor..cursor + length].to_vec(),
                interactive: popup.interactive != 0,
            });
            cursor += length;
        }
        rgba.truncate(root_len);
        let frame = EditorFrame {
            popups,
            width: metadata.width,
            height: metadata.height,
            native_size: (metadata.native_width, metadata.native_height),
            native_units_per_point: f32::from_bits(metadata.native_units_per_point_bits),
            rgba,
        };
        frame.validate()?;
        self.last_sequence
            .store(metadata.sequence, Ordering::Release);
        Ok(Some(frame))
    }

    /// Call after the child acknowledges opening the mapping. A Windows sharing
    /// failure may be retried on Drop after both processes unmap their views.
    pub fn unlink(&self) -> Result<()> {
        match std::fs::remove_file(&self.cleanup.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove editor surface mapping"),
        }
    }

    fn header(&self) -> &Header {
        // SAFETY: all FrameMaps have a fixed-size, page-aligned mapping. Header
        // contains only atomics and is initialized before its descriptor is sent.
        unsafe { &*self.map.as_ptr().cast::<Header>() }
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        debug_assert!(index < MAX_BYTES / WORD_BYTES);
        // SAFETY: the page-aligned mapping and Header size align every word. Validated
        // image lengths, including a padded final partial word, fit inside MAX_BYTES.
        // Both peers use AtomicU64 exclusively; ordinary references never span peer
        // writes, even if its ownership or metadata is malformed.
        unsafe {
            AtomicU64::from_ptr(
                self.map
                    .as_ptr()
                    .add(HEADER_BYTES + index * WORD_BYTES)
                    .cast_mut()
                    .cast(),
            )
        }
    }

    fn validate_header(&self) -> Result<()> {
        let h = self.header();
        ensure!(
            h.magic.load(Ordering::Acquire) == MAGIC
                && h.version.load(Ordering::Relaxed) == VERSION
                && h.header_bytes.load(Ordering::Relaxed) == HEADER_BYTES as u32
                && h.mapping_bytes.load(Ordering::Relaxed) == MAPPING_BYTES as u64
                && h.capacity.load(Ordering::Relaxed) == MAX_BYTES as u64
                && h.format.load(Ordering::Relaxed) == RGBA8
                && h.generation.load(Ordering::Relaxed) == self.generation
                && h.token.load(Ordering::Relaxed) == self.token,
            "incompatible editor surface header"
        );
        Ok(())
    }
}

fn create_file() -> Result<(File, Cleanup)> {
    for _ in 0..8 {
        let mut random = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
            .map_err(|_| anyhow::anyhow!("OS random source unavailable for editor surface"))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("nodeplayer-editor-{suffix}.map"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, Cleanup { path, remove: true })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create editor surface mapping"),
        }
    }
    bail!("could not create a unique editor surface mapping")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    fn frame(value: u8) -> EditorFrame {
        EditorFrame {
            popups: Vec::new(),
            width: 32,
            height: 16,
            native_size: (16, 8),
            native_units_per_point: 1.0,
            rgba: vec![value; 32 * 16 * 4],
        }
    }

    #[test]
    fn patterned_pixels_roundtrip_including_a_partial_word() -> Result<()> {
        for (width, height) in [(1, 1), (2, 1), (3, 1), (3, 3), (17, 19)] {
            let image = EditorFrame {
                popups: Vec::new(),
                width,
                height,
                native_size: (width, height),
                native_units_per_point: 1.0,
                rgba: (0..width as usize * height as usize * 4)
                    .map(|index| index.wrapping_mul(37) as u8)
                    .collect(),
            };
            let map = FrameMap::create(101, 27)?;
            let peer = FrameMap::open(&map.descriptor())?;
            assert!(peer.publish(&image)?);
            assert_eq!(map.take_latest()?.unwrap().rgba, image.rgba);
        }
        Ok(())
    }

    #[test]
    fn popups_roundtrip_with_their_pixels_geometry_and_dismissal() -> Result<()> {
        let map = FrameMap::create(101, 27)?;
        let peer = FrameMap::open(&map.descriptor())?;
        let mut image = frame(19);
        for (origin, interactive, value) in
            [([-40.5, 700.25], true, 37), ([120.0, -80.0], false, 91)]
        {
            image.popups.push(EditorPopup {
                origin,
                width: 3,
                height: 1,
                native_size: (6, 2),
                rgba: vec![value; 12],
                interactive,
            });
        }
        assert!(peer.publish(&image)?);
        let received = map.take_latest()?.unwrap();
        assert_eq!(received.rgba, image.rgba);
        assert_eq!(received.native_size, image.native_size);
        assert_eq!(received.popups.len(), 2);
        for (actual, expected) in received.popups.iter().zip(&image.popups) {
            assert_eq!(actual.origin, expected.origin);
            assert_eq!(
                (actual.width, actual.height),
                (expected.width, expected.height)
            );
            assert_eq!(actual.native_size, expected.native_size);
            assert_eq!(actual.rgba, expected.rgba);
            assert_eq!(actual.interactive, expected.interactive);
        }
        image.popups.clear();
        assert!(peer.publish(&image)?);
        let received = map.take_latest()?.unwrap();
        assert!(received.popups.is_empty());
        assert_eq!(received.rgba, image.rgba);
        Ok(())
    }

    #[test]
    fn frame_density_roundtrips_with_its_pixels_and_rejects_invalid_values() -> Result<()> {
        let map = FrameMap::create(101, 27)?;
        let peer = FrameMap::open(&map.descriptor())?;
        let mut image = frame(19);
        for density in [1.0, 1.25, 1.5, 2.0] {
            image.native_units_per_point = density;
            assert!(peer.publish(&image)?);
            let received = map.take_latest()?.unwrap();
            assert_eq!(received.native_units_per_point, density);
            assert_eq!(received.rgba, image.rgba);
        }
        for density in [f32::NAN, f32::INFINITY, 0.0, -1.0, 100.0] {
            image.native_units_per_point = density;
            assert!(peer.publish(&image).is_err());
        }
        Ok(())
    }

    #[test]
    fn malformed_popup_metadata_is_rejected_and_releases_the_frame() -> Result<()> {
        let map = FrameMap::create(101, 27)?;
        let mut image = frame(19);
        image.popups.push(EditorPopup {
            origin: [0.0, 0.0],
            width: 1,
            height: 1,
            native_size: (1, 1),
            rgba: vec![17; 4],
            interactive: true,
        });
        for corrupt in 0..5 {
            assert!(map.publish(&image)?);
            let h = map.header();
            match corrupt {
                0 => h
                    .popup_count
                    .store(MAX_POPUPS as u32 + 1, Ordering::Relaxed),
                1 => h.popups[0].x.store(f64::NAN.to_bits(), Ordering::Relaxed),
                2 => h.popups[0].width.store(u32::MAX, Ordering::Relaxed),
                3 => h.popups[0].interactive.store(2, Ordering::Relaxed),
                _ => {
                    h.width.store(4096, Ordering::Relaxed);
                    h.height.store(4096, Ordering::Relaxed);
                    h.byte_len.store(MAX_BYTES as u64 + 4, Ordering::Relaxed);
                }
            }
            assert!(map.take_latest().is_err());
            assert_eq!(h.state.load(Ordering::Acquire), FREE);
        }
        Ok(())
    }

    #[test]
    #[ignore = "reports capture-sized image transfer timings; run with --nocapture"]
    fn frame_transfer_timings() -> Result<()> {
        use std::time::{Duration, Instant};
        const FRAMES: u32 = 4;
        for (width, height) in [(640, 360), (1252, 770), (2560, 1440)] {
            let map = FrameMap::create(102, 28)?;
            let peer = FrameMap::open(&map.descriptor())?;
            let image = EditorFrame {
                popups: Vec::new(),
                width,
                height,
                native_size: (width, height),
                native_units_per_point: 1.0,
                rgba: vec![123; width as usize * height as usize * 4],
            };
            let mut publish = Duration::ZERO;
            let mut read = Duration::ZERO;
            for _ in 0..FRAMES {
                let start = Instant::now();
                assert!(peer.publish(std::hint::black_box(&image))?);
                publish += start.elapsed();
                let start = Instant::now();
                let received = map.take_latest()?.unwrap();
                read += start.elapsed();
                assert_eq!(received.rgba.len(), image.rgba.len());
                std::hint::black_box(received);
            }
            println!(
                "frame-transfer-benchmark {}",
                serde_json::json!({
                    "debug": cfg!(debug_assertions), "width": width, "height": height,
                    "frames": FRAMES, "publish_ms": publish.as_secs_f64() * 1000.0 / f64::from(FRAMES),
                    "read_ms": read.as_secs_f64() * 1000.0 / f64::from(FRAMES),
                })
            );
        }
        Ok(())
    }

    #[test]
    fn capture_demand_tracks_consumption_and_preserves_reader_ownership() -> Result<()> {
        let parent = FrameMap::create(103, 29)?;
        let child = FrameMap::open(&parent.descriptor())?;
        assert!(child.needs_frame(false)?);
        assert!(child.publish(&frame(1))?);
        assert!(!child.needs_frame(false)?);
        assert!(child.needs_frame(true)?, "input may replace an unread image");
        assert!(child.publish(&frame(2))?);
        assert_eq!(parent.take_latest()?.unwrap().rgba, frame(2).rgba);
        assert!(child.needs_frame(false)?);
        for state in [WRITING, READING] {
            parent.header().state.store(state, Ordering::Release);
            assert!(!child.needs_frame(false)?);
            assert!(!child.needs_frame(true)?, "refresh must respect an active copy");
            assert_eq!(parent.header().state.load(Ordering::Acquire), state);
        }
        parent.header().state.store(99, Ordering::Release);
        assert!(child.needs_frame(false).is_err());
        parent.header().state.store(FREE, Ordering::Release);
        Ok(())
    }

    #[test]
    fn roundtrip_replaces_unread_frames_and_consumes_once() -> Result<()> {
        let parent = FrameMap::create(91, 7)?;
        let descriptor = parent.descriptor();
        let child = FrameMap::open(&descriptor)?;
        assert_eq!(child.generation(), 91);
        assert_eq!(child.token(), 7);
        assert!(parent.take_latest()?.is_none());
        assert!(child.publish(&frame(17))?);
        assert!(child.publish(&frame(42))?);
        let image = parent.take_latest()?.context("expected latest frame")?;
        assert_eq!(
            (image.width, image.height, image.native_size),
            (32, 16, (16, 8))
        );
        assert_eq!(image.rgba, frame(42).rgba);
        assert!(parent.take_latest()?.is_none());
        assert!(child.publish(&frame(17))?);
        assert_eq!(parent.take_latest()?.unwrap().rgba, frame(17).rgba);
        Ok(())
    }

    #[test]
    fn reading_slot_never_waits_or_gets_overwritten() -> Result<()> {
        let map = FrameMap::create(92, 8)?;
        map.header().state.store(READING, Ordering::Release);
        assert!(!map.publish(&frame(1))?);
        assert!(map.take_latest()?.is_none());
        assert_eq!(map.header().state.load(Ordering::Acquire), READING);
        map.header().state.store(FREE, Ordering::Release);
        assert!(map.publish(&frame(2))?);
        Ok(())
    }

    #[test]
    fn concurrent_mappings_do_not_mix_frames() -> Result<()> {
        let parent = FrameMap::create(93, 9)?;
        let child = FrameMap::open(&parent.descriptor())?;
        let done = Arc::new(AtomicBool::new(false));
        let writer_done = done.clone();
        let writer = std::thread::spawn(move || -> Result<()> {
            for value in 0..500 {
                let _ = child.publish(&frame((value % 251) as u8))?;
                std::thread::yield_now();
            }
            while !child.publish(&frame(253))? {
                std::thread::yield_now();
            }
            writer_done.store(true, Ordering::Release);
            Ok(())
        });
        let mut last = None;
        loop {
            let complete = done.load(Ordering::Acquire);
            if let Some(frame) = parent.take_latest()? {
                assert!(frame.rgba.iter().all(|byte| *byte == frame.rgba[0]));
                last = Some(frame.rgba[0]);
            } else if complete {
                break;
            }
            std::thread::yield_now();
        }
        writer.join().unwrap()?;
        assert_eq!(last, Some(253));
        Ok(())
    }

    #[test]
    fn rejects_wrong_identity_header_and_truncated_file() -> Result<()> {
        let map = FrameMap::create(94, 10)?;
        let mut descriptor = map.descriptor();
        descriptor.token += 1;
        assert!(FrameMap::open(&descriptor).is_err());
        descriptor.token -= 1;
        descriptor.generation += 1;
        assert!(FrameMap::open(&descriptor).is_err());
        descriptor.generation -= 1;
        map.header().version.store(VERSION + 1, Ordering::Relaxed);
        assert!(FrameMap::open(&descriptor).is_err());
        assert!(
            descriptor.path.exists(),
            "an invalid open must not remove the existing file"
        );
        map.header().version.store(VERSION, Ordering::Relaxed);
        let (truncated, cleanup) = create_file()?;
        truncated.set_len(HEADER_BYTES as u64)?;
        descriptor.path = cleanup.path.clone();
        assert!(FrameMap::open(&descriptor).is_err());
        Ok(())
    }

    #[test]
    fn mapping_is_shared_with_a_real_child_process() -> Result<()> {
        let parent = FrameMap::create(99, 15)?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "plugins::remote::surface::tests::publishes_from_child_process",
                "--nocapture",
            ])
            .env("NODEPLAYER_SURFACE_MAP_TEST_PATH", parent.descriptor().path)
            .output()?;
        ensure!(output.status.success(), "editor frame child test failed");
        let image = parent
            .take_latest()?
            .context("child did not publish a frame")?;
        assert_eq!(image.rgba, frame(123).rgba);
        Ok(())
    }

    #[test]
    fn publishes_from_child_process() -> Result<()> {
        let Some(path) = std::env::var_os("NODEPLAYER_SURFACE_MAP_TEST_PATH") else {
            return Ok(());
        };
        let child = FrameMap::open(&SurfaceDescriptor {
            path: path.into(),
            generation: 99,
            token: 15,
        })?;
        ensure!(child.publish(&frame(123))?, "child frame was not accepted");
        Ok(())
    }

    #[test]
    fn rejects_invalid_publications_without_taking_slot() -> Result<()> {
        let map = FrameMap::create(95, 11)?;
        let mut invalid = frame(1);
        invalid.width = MAX_DIMENSION + 1;
        assert!(map.publish(&invalid).is_err());
        invalid = frame(1);
        invalid.native_size = (0, 8);
        assert!(map.publish(&invalid).is_err());
        invalid = frame(1);
        invalid.rgba.pop();
        assert!(map.publish(&invalid).is_err());
        assert_eq!(map.header().state.load(Ordering::Acquire), FREE);
        Ok(())
    }

    #[test]
    fn rejects_peer_bounds_and_stale_sequences_before_copying() -> Result<()> {
        let map = FrameMap::create(96, 12)?;
        assert!(map.publish(&frame(1))?);
        map.header().width.store(u32::MAX, Ordering::Relaxed);
        assert!(map.take_latest().is_err());
        assert_eq!(map.header().state.load(Ordering::Acquire), FREE);
        assert!(map.publish(&frame(2))?);
        map.header().byte_len.store(u64::MAX, Ordering::Relaxed);
        assert!(map.take_latest().is_err());
        assert!(map.publish(&frame(3))?);
        map.header().native_width.store(0, Ordering::Relaxed);
        assert!(map.take_latest().is_err());
        assert!(map.publish(&frame(4))?);
        let sequence = map.header().sequence.load(Ordering::Relaxed);
        assert!(map.take_latest()?.is_some());
        assert!(map.publish(&frame(5))?);
        map.header().sequence.store(sequence, Ordering::Relaxed);
        assert!(map.take_latest().is_err());
        Ok(())
    }

    #[test]
    fn cleanup_removes_file_after_both_peers_close() -> Result<()> {
        let parent = FrameMap::create(97, 13)?;
        let descriptor = parent.descriptor();
        let child = FrameMap::open(&descriptor)?;
        let _ = parent.unlink();
        drop(parent);
        assert!(child.publish(&frame(9))?);
        assert!(child.take_latest()?.is_some());
        drop(child);
        assert!(!descriptor.path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn mapping_file_is_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let map = FrameMap::create(98, 14)?;
        assert_eq!(
            std::fs::metadata(map.descriptor().path)?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Ok(())
    }
}
