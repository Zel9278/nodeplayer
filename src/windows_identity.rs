//! Windows resolves the media session's app name through a Start menu shortcut with the
//! same AppUserModelID as the process. A window title alone (or souvlaki's Linux-only
//! display_name) does not identify a portable executable to the shell.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::System::Com::StructuredStorage::{PVCHF_DEFAULT, PropVariantChangeType};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHCNE_UPDATEITEM, SHCNF_PATHW, SHChangeNotify,
    SHGetKnownFolderPath, SetCurrentProcessExplicitAppUserModelID, ShellLink,
};
use windows::core::{BSTR, GUID, HSTRING, Interface, PROPVARIANT, w};

const APP_ID: &str = "Zel9278.nodeplayer";
const APP_ID_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// Run only for the GUI, before creating its first window or media session. Failure to
/// register must not prevent playback (for example on a profile with a read-only menu).
pub fn install() {
    if let Err(e) = register() {
        log::warn!("Windows app identity: {e:#}");
    }
}

fn register() -> Result<()> {
    // Velopack owns installed and portable launcher shortcuts and sets the same AUMID.
    if crate::updater::is_managed() {
        return Ok(());
    }
    unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) }?;
    let _com = ComApartment::new()?;
    let exe = std::env::current_exe()?;
    // Use the Windows known folder API: the Start menu may be redirected.
    let menu = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None) }?;
    let programs = unsafe { menu.to_string() };
    unsafe { CoTaskMemFree(Some(menu.0.cast())) };
    let shortcut = std::path::PathBuf::from(programs?).join("nodeplayer.lnk");
    let data = crate::dirs::data().context("missing local application data folder")?;
    std::fs::create_dir_all(&data)?;
    let icon = data.join("nodeplayer.ico");
    let bytes = icon_bytes();
    if std::fs::read(&icon).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&icon, bytes)?;
    }
    std::fs::create_dir_all(shortcut.parent().context("missing Start menu folder")?)?;
    if ensure_shortcut(&shortcut, &exe, &icon)? {
        let path = HSTRING::from(shortcut.as_path());
        unsafe {
            SHChangeNotify(
                SHCNE_UPDATEITEM,
                SHCNF_PATHW,
                Some(path.as_ptr().cast()),
                None,
            )
        };
    }
    Ok(())
}

struct ComApartment;

impl ComApartment {
    fn new() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Avoid rewriting a correct link each launch; repair its target after an extracted
/// portable copy moves. No arguments or working directory are needed by nodeplayer.
fn ensure_shortcut(path: &Path, exe: &Path, icon: &Path) -> windows::core::Result<bool> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let file: IPersistFile = link.cast()?;
        let properties: IPropertyStore = link.cast()?;
        let path = HSTRING::from(path);
        if file.Load(&path, STGM_READ).is_ok() {
            let mut target = [0u16; 32768];
            // Keep an installer's icon choice when its link already has our target and
            // identity; rewriting it would interfere with MSI repair/uninstall ownership.
            let matches = link.GetPath(&mut target, std::ptr::null_mut(), 0).is_ok()
                && wide_path(&target) == exe
                && properties
                    .GetValue(&APP_ID_KEY)
                    .ok()
                    .and_then(|v| BSTR::try_from(&v).ok())
                    .is_some_and(|id| id.to_string() == APP_ID);
            if matches {
                return Ok(false);
            }
        }
        // Use a fresh link when repairing, so stale arguments cannot survive.
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(exe))?;
        link.SetDescription(w!("nodeplayer"))?;
        link.SetIconLocation(&HSTRING::from(icon), 0)?;
        let properties: IPropertyStore = link.cast()?;
        let mut id = PROPVARIANT::default();
        PropVariantChangeType(
            &mut id,
            &PROPVARIANT::from(APP_ID),
            PVCHF_DEFAULT,
            VT_LPWSTR,
        )?;
        properties.SetValue(&APP_ID_KEY, &id)?;
        properties.Commit()?;
        let file: IPersistFile = link.cast()?;
        file.Save(&path, true)?;
        Ok(true)
    }
}

fn wide_path(value: &[u16]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let end = value.iter().position(|&v| v == 0).unwrap_or(value.len());
    std::ffi::OsString::from_wide(&value[..end]).into()
}

/// ICO supports PNG images directly; wrap the existing 256x256 app icon in one directory
/// entry, rather than adding another image dependency or shipping a separate asset.
fn icon_bytes() -> Vec<u8> {
    let png = include_bytes!("../assets/icon.png");
    let mut ico = vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 32, 0];
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(png);
    ico
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_keeps_identity_and_repairs_a_moved_executable() {
        let _com = ComApartment::new().unwrap();
        let dir = std::env::temp_dir().join(format!("nodeplayer-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nodeplayer.lnk");
        let exe = dir.join("日本語 player.exe");
        let moved = dir.join("moved player.exe");
        let icon = dir.join("nodeplayer.ico");
        std::fs::write(&icon, icon_bytes()).unwrap();
        assert!(ensure_shortcut(&path, &exe, &icon).unwrap());
        assert!(!ensure_shortcut(&path, &exe, &icon).unwrap());
        // MSI owns its icon location; the portable fallback must keep a correct installed
        // shortcut even when its icon differs from our per-user icon cache.
        assert!(!ensure_shortcut(&path, &exe, &dir.join("another-icon.ico")).unwrap());
        assert!(ensure_shortcut(&path, &moved, &icon).unwrap());
        assert!(!ensure_shortcut(&path, &moved, &icon).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
