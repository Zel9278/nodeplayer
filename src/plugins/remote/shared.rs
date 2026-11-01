//! File-backed, fixed-capacity cross-process slots. No addresses or Rust containers in IPC.
//!
//! FREE -> INPUT (parent release) -> WORKING (child acquire CAS) -> OUTPUT (child
//! release) -> FREE (parent release). A stopped/late WORKING slot is NEVER reclaimed
//! while its child is alive. Only 64-bit targets with lock-free 32/64-bit atomics are
//! accepted. This is an OS-coherent native-endian ABI, not a cross-architecture ABI.
use super::protocol::VERSION;
use anyhow::{Result, bail};
use memmap2::{MmapMut, MmapOptions};
use std::{
    cell::UnsafeCell,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

pub const Q: usize = 256;
pub const DELAY: usize = 4096;
pub const SLOTS: usize = 32;
pub const MIDI: usize = 512;
pub const MAX_LATENCY: usize = 65536;
pub const FREE: u32 = 0;
pub const INPUT: u32 = 1;
pub const WORKING: u32 = 2;
pub const OUTPUT: u32 = 3;
const MAGIC: u64 = 0x4e504c4159455231;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WireMidi {
    pub offset: u32,
    pub data: [u8; 3],
    pub reserved: u8,
}
#[repr(C)]
pub struct Payload {
    pub generation: u64,
    pub sequence: u64,
    pub first_sample: u64,
    pub frames: u32,
    pub midi_count: u32,
    pub left: [f32; Q],
    pub right: [f32; Q],
    pub midi: [WireMidi; MIDI],
}
#[repr(C, align(64))]
pub struct Slot {
    pub state: AtomicU32,
    data: UnsafeCell<Payload>,
}
#[repr(C, align(64))]
struct Header {
    magic: u64,
    version: u32,
    quantum: u32,
    slots: u32,
    reserved: u32,
    generation: u64,
    pub progress: AtomicU64,
    epoch: AtomicU64,
}
#[repr(C)]
struct Layout {
    header: Header,
    slots: [Slot; SLOTS],
}

pub struct Shared {
    map: Option<MmapMut>,
    path: PathBuf,
    remove: bool,
}
// Each payload has exactly one owner according to the acquire/release slot state.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}
impl Shared {
    pub fn create(path: PathBuf, generation: u64) -> Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;
        file.set_len(std::mem::size_of::<Layout>() as u64)?;
        let mut map = unsafe { MmapOptions::new().map_mut(&file)? };
        map.fill(0);
        let layout = map.as_mut_ptr().cast::<Layout>();
        unsafe {
            std::ptr::addr_of_mut!((*layout).header).write(Header {
                magic: MAGIC,
                version: VERSION,
                quantum: Q as u32,
                slots: SLOTS as u32,
                reserved: 0,
                generation,
                progress: AtomicU64::new(0),
                epoch: AtomicU64::new(1),
            });
            for i in 0..SLOTS {
                std::ptr::addr_of_mut!((*layout).slots[i].state).write(AtomicU32::new(FREE));
            }
        }
        Ok(Self {
            map: Some(map),
            path,
            remove: true,
        })
    }
    pub fn open(path: &Path, generation: u64) -> Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        if file.metadata()?.len() != std::mem::size_of::<Layout>() as u64 {
            bail!("invalid shared mapping size");
        }
        let map = unsafe { MmapOptions::new().map_mut(&file)? };
        let s = Self {
            map: Some(map),
            path: path.to_owned(),
            remove: false,
        };
        let h = &s.layout().header;
        if h.magic != MAGIC
            || h.version != VERSION
            || h.quantum as usize != Q
            || h.slots as usize != SLOTS
            || h.generation != generation
        {
            bail!("incompatible shared mapping header");
        }
        Ok(s)
    }
    fn layout(&self) -> &Layout {
        unsafe { &*self.map.as_ref().unwrap().as_ptr().cast::<Layout>() }
    }
    pub fn generation(&self) -> u64 {
        self.layout().header.generation
    }
    pub fn epoch(&self) -> u64 {
        self.layout().header.epoch.load(Ordering::Acquire)
    }
    pub fn invalidate(&self) {
        self.layout().header.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn slot(&self, i: usize) -> &Slot {
        &self.layout().slots[i]
    }
    pub fn progress(&self) -> u64 {
        self.layout().header.progress.load(Ordering::Acquire)
    }
    pub fn progressed(&self) {
        self.layout()
            .header
            .progress
            .fetch_add(1, Ordering::Release);
    }
}
impl Slot {
    /// Access the payload using external process-shared state ownership, not a Rust borrow.
    ///
    /// # Safety
    /// The caller must ensure exclusive access to this payload under the acquire/release
    /// slot protocol: FREE/OUTPUT parent, WORKING child. Do not retain the reference
    /// after the next release publication. The state is never reset by a third party.
    #[allow(clippy::mut_from_ref)] // Exclusivity is enforced by the caller via the shared slot state.
    pub unsafe fn payload(&self) -> &mut Payload {
        unsafe { &mut *self.data.get() }
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        // Unmap before unlink: Windows does not allow deleting a mapped file.
        drop(self.map.take());
        if self.remove {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_survives_second_mapping_and_busy_slot_is_not_free() {
        let path = std::env::temp_dir().join(format!(
            "nodeplayer-map-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let parent = Shared::create(path.clone(), 77).unwrap();
        let child = Shared::open(&path, 77).unwrap();
        assert!(Shared::open(&path, 78).is_err());
        let p = parent.slot(0);
        unsafe {
            p.payload().sequence = 42;
        }
        p.state.store(INPUT, Ordering::Release);
        let c = child.slot(0);
        assert!(
            c.state
                .compare_exchange(INPUT, WORKING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        );
        assert_ne!(p.state.load(Ordering::Acquire), FREE);
        unsafe {
            assert_eq!(c.payload().sequence, 42);
            c.payload().left[0] = 0.75;
        }
        c.state.store(OUTPUT, Ordering::Release);
        assert_eq!(p.state.load(Ordering::Acquire), OUTPUT);
        unsafe {
            assert_eq!(p.payload().left[0], 0.75);
        }
        p.state.store(FREE, Ordering::Release);
        drop(child);
        drop(parent);
        assert!(!path.exists());
    }
}
