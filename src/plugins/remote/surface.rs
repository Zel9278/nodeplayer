//! A latest-frame image slot, independent of the realtime audio mapping.
//!
//! FREE/READY -> WRITING -> READY -> READING -> FREE. A producer may replace an
//! unread frame, but never a frame being copied by the consumer. All mapped fields
//! and bytes are atomic: even malformed peer metadata cannot create Rust aliases
//! to ordinary mutable memory. This is a native-endian, same-executable ABI.

use anyhow::{Context, Result, bail, ensure};
use memmap2::{MmapOptions, MmapRaw};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use crate::plugins::surface::{EditorFrame, MAX_DIMENSION, MAX_PIXELS};

const MAGIC: u64 = 0x4e50_5355_5246_3031;
const VERSION: u32 = 1;
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
    byte_len: AtomicU64,
}

const HEADER_BYTES: usize = std::mem::size_of::<Header>();
const MAPPING_BYTES: usize = HEADER_BYTES + MAX_BYTES;

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

#[derive(Clone, Copy, PartialEq, Eq)]
struct FrameHeader {
    sequence: u64,
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    byte_len: u64,
}

impl FrameHeader {
    fn read(header: &Header) -> Self {
        Self {
            sequence: header.sequence.load(Ordering::Relaxed),
            width: header.width.load(Ordering::Relaxed),
            height: header.height.load(Ordering::Relaxed),
            native_width: header.native_width.load(Ordering::Relaxed),
            native_height: header.native_height.load(Ordering::Relaxed),
            byte_len: header.byte_len.load(Ordering::Relaxed),
        }
    }

    fn validated_len(self) -> Result<usize> {
        ensure!(self.sequence != 0, "invalid editor frame sequence");
        ensure!(
            self.width > 0
                && self.height > 0
                && self.width <= MAX_DIMENSION
                && self.height <= MAX_DIMENSION,
            "invalid shared editor image dimensions"
        );
        ensure!(
            self.native_width > 0
                && self.native_height > 0
                && self.native_width <= MAX_DIMENSION
                && self.native_height <= MAX_DIMENSION,
            "invalid shared native editor dimensions"
        );
        let pixels = (self.width as usize)
            .checked_mul(self.height as usize)
            .context("shared editor image size overflow")?;
        ensure!(
            pixels <= MAX_PIXELS,
            "shared editor image exceeds pixel limit"
        );
        let length = pixels
            .checked_mul(4)
            .context("shared editor image byte count overflow")?;
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
            byte_len: AtomicU64::new(0),
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
        h.byte_len.store(frame.rgba.len() as u64, Ordering::Relaxed);
        for (index, byte) in frame.rgba.iter().enumerate() {
            self.byte(index).store(*byte, Ordering::Relaxed);
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
        let metadata = FrameHeader::read(h);
        let length = metadata.validated_len()?;
        ensure!(
            metadata.sequence > self.last_sequence.load(Ordering::Acquire),
            "stale editor frame sequence"
        );
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(length)
            .context("allocate captured editor image")?;
        for index in 0..length {
            rgba.push(self.byte(index).load(Ordering::Relaxed));
        }
        self.validate_header()?;
        ensure!(
            h.state.load(Ordering::Acquire) == READING && metadata == FrameHeader::read(h),
            "editor frame changed during read"
        );
        let frame = EditorFrame {
            width: metadata.width,
            height: metadata.height,
            native_size: (metadata.native_width, metadata.native_height),
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

    fn byte(&self, index: usize) -> &AtomicU8 {
        debug_assert!(index < MAX_BYTES);
        // SAFETY: callers use only lengths validated against MAX_BYTES. AtomicU8
        // has byte alignment; the OS initializes the extended file to zero. Atomic
        // access is used by both peers, including when the peer violates ownership.
        unsafe { AtomicU8::from_ptr(self.map.as_ptr().add(HEADER_BYTES + index).cast_mut()) }
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
            width: 32,
            height: 16,
            native_size: (16, 8),
            rgba: vec![value; 32 * 16 * 4],
        }
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
