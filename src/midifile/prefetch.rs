//! Reading a mapped MIDI file in ahead of the load's scan: in order from front to back in large
//! blocks, instead of the page faults one at a time all over the file that the scan's readers,
//! one per track, would wait for, the more so through FUSE (NTFS by ntfs-3g) or from a hard
//! disk.

use std::sync::Arc;

/// Whether most of the mapping `map` is in memory already (the page cache), by the pages
/// `mincore` finds there.
#[cfg(unix)]
pub(crate) fn mostly_resident(map: &[u8]) -> bool {
    if map.is_empty() {
        return true;
    }
    // SAFETY: sysconf only reads a configuration value.
    let Some(page) = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok().filter(|&p| p > 0) else { return false };
    let mut pages = vec![0u8; map.len().div_ceil(page)];
    // SAFETY: a mapping starts on a page boundary and spans `map.len()` bytes; `pages` has a
    // byte for each of its pages.
    let ok = unsafe { libc::mincore(map.as_ptr() as *mut libc::c_void, map.len(), pages.as_mut_ptr().cast()) } == 0;
    ok && pages.iter().filter(|&&b| b & 1 != 0).count() * 10 >= pages.len() * 9
}

#[cfg(not(unix))]
pub(crate) fn mostly_resident(_: &[u8]) -> bool {
    false
}

/// While a file loads, read it through once from front to back in large blocks on a thread of
/// its own, so that the pages its scan touches all over the file (the tracks' headers, then the
/// tracks side by side) come from one sequential read instead of a seek each. Stops when
/// dropped. Not worth it for a file already in memory.
pub(crate) struct ReadThrough {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ReadThrough {
    pub(crate) fn start(file: &std::fs::File) -> Option<ReadThrough> {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut file = file.try_clone().ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new()
            .name("midi-read-through".into())
            .spawn(move || {
                let mut block = vec![0u8; 8 << 20];
                while !stopped.load(Ordering::Relaxed) && file.read(&mut block).is_ok_and(|n| n > 0) {}
            })
            .ok()?;
        Some(ReadThrough { stop })
    }
}

impl Drop for ReadThrough {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
