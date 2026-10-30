//! Loading plugin binaries (shared objects, DLLs, the binary inside a macOS bundle).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
pub use libloading::Library;

/// Extension of a shared library on this platform: `so`, `dll`, `dylib`.
pub const EXT: &str = std::env::consts::DLL_EXTENSION;

/// Loads `path`. On Unix its symbols stay private (`RTLD_LOCAL`), so two plugins that bundle
/// different versions of the same library don't get each other's.
pub fn open(path: &Path) -> Result<Library> {
    #[cfg(unix)]
    let lib = unsafe {
        use libloading::os::unix::{RTLD_LOCAL, RTLD_NOW};
        libloading::os::unix::Library::open(Some(path), RTLD_NOW | RTLD_LOCAL).map(Library::from)
    };
    #[cfg(windows)]
    let lib = unsafe { Library::new(path) };
    lib.with_context(|| format!("can't load {}", path.display()))
}

/// The executable inside a macOS bundle (`X.vst3/Contents/MacOS/X`); `path` itself when it's
/// a file.
pub fn mac_bundle_binary(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let dir = path.join("Contents").join("MacOS");
    let preferred = dir.join(path.file_stem().unwrap_or_default());
    if preferred.is_file() {
        return Ok(preferred);
    }
    std::fs::read_dir(&dir)
        .with_context(|| format!("no {} in bundle", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("nothing in {}", dir.display()))
}

/// A `CFBundleRef` for a macOS bundle, which VST3's `bundleEntry` wants. Never released:
/// modules stay loaded for the life of the process.
#[cfg(target_os = "macos")]
pub fn cf_bundle(path: &Path) -> Result<*mut std::ffi::c_void> {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, is_dir: u8) -> *const c_void;
        fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
    }
    let bytes = path.as_os_str().as_encoded_bytes();
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
        if url.is_null() {
            anyhow::bail!("bad bundle path {}", path.display());
        }
        let bundle = CFBundleCreate(std::ptr::null(), url);
        CFRelease(url);
        if bundle.is_null() {
            anyhow::bail!("{} isn't a bundle", path.display());
        }
        Ok(bundle)
    }
}
