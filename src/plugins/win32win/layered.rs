//! Retain layered-popup uploads before the compositor takes ownership of their pixels.
//!
//! WM_PRINT does not expose UpdateLayeredWindow content. Capture sources therefore
//! observe these two upload APIs in their own process, and park owned popup uploads
//! before Windows can present them. All other HWNDs retain the original API behavior.

use std::cell::RefCell;

use anyhow::{Result, anyhow, bail, ensure};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

const MAX_WINDOWS: usize = 16;
const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: usize = 16 * 1024 * 1024;

/// Top-left, tightly packed, straight-alpha RGBA; the caller supplies popup geometry.
pub(crate) struct LayeredImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
struct Bitmap {
    width: u32,
    height: u32,
    // Keep the original premultiplied pixels: an opacity-only update must not
    // multiply already attenuated alpha, and dirty updates retain untouched pixels.
    bgra: Vec<u8>,
    blend: BLENDFUNCTION,
    flags: u32,
    key: COLORREF,
}

thread_local! {
    static IMAGES: RefCell<Vec<(HWND, std::result::Result<Bitmap, String>)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn initialize() -> Result<()> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    return hooks::initialize();
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    Ok(())
}

pub(crate) fn image(hwnd: HWND) -> Result<Option<LayeredImage>> {
    if super::CaptureInput::for_window(hwnd).is_none() {
        forget(hwnd);
        return Ok(None);
    }
    IMAGES.with(|images| {
        let images = images.borrow();
        let Some((_, bitmap)) = images.iter().find(|(window, _)| *window == hwnd) else {
            return Ok(None);
        };
        let bitmap = bitmap.as_ref().map_err(|error| anyhow!(error.clone()))?;
        let mut rgba = allocate(bitmap.bgra.len())?;
        for (source, target) in bitmap
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .zip(rgba.as_chunks_mut::<4>().0.iter_mut())
        {
            *target = rgba_pixel(source, bitmap.blend, bitmap.flags, bitmap.key);
        }
        Ok(Some(LayeredImage {
            width: bitmap.width,
            height: bitmap.height,
            rgba,
        }))
    })
}

pub(crate) fn forget(hwnd: HWND) {
    let _ = IMAGES.try_with(|images| {
        if let Ok(mut images) = images.try_borrow_mut() {
            images.retain(|(window, _)| *window != hwnd);
        }
    });
}

fn allocate(bytes: usize) -> Result<Vec<u8>> {
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(bytes)
        .map_err(|_| anyhow!("layered popup allocation failed"))?;
    pixels.resize(bytes, 0);
    Ok(pixels)
}

fn dimensions(size: SIZE) -> Result<(u32, u32, usize)> {
    let (width, height) = (u32::try_from(size.cx)?, u32::try_from(size.cy)?);
    ensure!(
        width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "layered popup exceeds capture dimensions"
    );
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .filter(|pixels| *pixels <= MAX_PIXELS)
        .ok_or_else(|| anyhow!("layered popup exceeds capture pixel budget"))?;
    Ok((width, height, pixels))
}

fn rgba_pixel(pixel: &[u8], blend: BLENDFUNCTION, flags: u32, key: COLORREF) -> [u8; 4] {
    let [blue, green, red, source_alpha] = [pixel[0], pixel[1], pixel[2], pixel[3]];
    if flags & 1 != 0
        && [red, green, blue] == [key.0 as u8, (key.0 >> 8) as u8, (key.0 >> 16) as u8]
    {
        return [0; 4];
    }
    let per_pixel = flags & 2 != 0 && blend.AlphaFormat & AC_SRC_ALPHA as u8 != 0;
    let alpha = if per_pixel { source_alpha } else { 255 };
    let constant = if flags & 2 != 0 {
        blend.SourceConstantAlpha
    } else {
        255
    };
    let output_alpha = ((u16::from(alpha) * u16::from(constant) + 127) / 255) as u8;
    if output_alpha == 0 {
        return [0; 4];
    }
    let straight = |channel: u8| {
        if per_pixel {
            ((u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255) as u8
        } else {
            channel
        }
    };
    [straight(red), straight(green), straight(blue), output_alpha]
}

#[derive(Clone, Copy)]
struct Upload {
    source: HDC,
    size: SIZE,
    origin: POINT,
    blend: Option<BLENDFUNCTION>,
    flags: u32,
    key: COLORREF,
    dirty: Option<RECT>,
}

fn snapshot(hwnd: HWND, upload: Upload) -> Result<Bitmap> {
    let (width, height, pixels) = dimensions(upload.size)?;
    let (existing_pixels, windows) = IMAGES.with(|images| {
        let images = images.borrow();
        (
            images
                .iter()
                .filter(|(window, _)| *window != hwnd)
                .filter_map(|(_, bitmap)| bitmap.as_ref().ok())
                .map(|bitmap| bitmap.bgra.len() / 4)
                .sum::<usize>(),
            images.iter().filter(|(window, _)| *window != hwnd).count(),
        )
    });
    ensure!(
        windows < MAX_WINDOWS && existing_pixels <= MAX_PIXELS - pixels,
        "layered popups exceed capture cache budget"
    );
    let blend = upload.blend.unwrap_or(BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 0,
    });
    ensure!(
        upload.flags & 2 == 0
            || (blend.BlendOp == AC_SRC_OVER as u8
                && blend.BlendFlags == 0
                && blend.AlphaFormat & !(AC_SRC_ALPHA as u8) == 0),
        "unsupported layered popup blend"
    );
    let mut bitmap = Bitmap {
        width,
        height,
        bgra: allocate(pixels * 4)?,
        blend,
        flags: upload.flags,
        key: upload.key,
    };
    let dirty = upload.dirty.unwrap_or(RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    });
    ensure!(
        dirty.left >= 0
            && dirty.top >= 0
            && dirty.right >= dirty.left
            && dirty.bottom >= dirty.top
            && dirty.right <= width as i32
            && dirty.bottom <= height as i32,
        "invalid layered popup dirty rectangle"
    );
    if upload.dirty.is_some() {
        // Cache access ends before any GDI operation, including GetObject/GdiFlush.
        IMAGES.with(|images| {
            if let Some((_, Ok(previous))) =
                images.borrow().iter().find(|(window, _)| *window == hwnd)
            {
                let columns = width.min(previous.width) as usize * 4;
                for row in 0..height.min(previous.height) as usize {
                    bitmap.bgra[row * width as usize * 4..row * width as usize * 4 + columns]
                        .copy_from_slice(
                            &previous.bgra[row * previous.width as usize * 4
                                ..row * previous.width as usize * 4 + columns],
                        );
                }
            }
        });
    }
    if dirty.right > dirty.left && dirty.bottom > dirty.top {
        let origin = POINT {
            x: upload
                .origin
                .x
                .checked_add(dirty.left)
                .ok_or_else(|| anyhow!("layered source coordinate overflow"))?,
            y: upload
                .origin
                .y
                .checked_add(dirty.top)
                .ok_or_else(|| anyhow!("layered source coordinate overflow"))?,
        };
        let region = read_source(
            upload.source,
            origin,
            (dirty.right - dirty.left) as u32,
            (dirty.bottom - dirty.top) as u32,
        )?;
        let stride = (dirty.right - dirty.left) as usize * 4;
        for row in 0..(dirty.bottom - dirty.top) as usize {
            let offset = ((row + dirty.top as usize) * width as usize + dirty.left as usize) * 4;
            bitmap.bgra[offset..offset + stride]
                .copy_from_slice(&region[row * stride..(row + 1) * stride]);
        }
    }
    Ok(bitmap)
}

fn read_source(source: HDC, origin: POINT, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut pixels = allocate(width as usize * height as usize * 4)?;
    // GetObject reports positive heights even for top-down source DIBs. Copy
    // through GDI into our own top-down 32-bit DIB so row order and DC mapping
    // follow the upload source, without modifying its selected bitmap. SRCCOPY
    // between these 32-bit DIBs retains the premultiplied alpha channel too.
    let readback = Readback::new(source, width, height)?;
    unsafe {
        BitBlt(
            readback.dc,
            0,
            0,
            width as i32,
            height as i32,
            source,
            origin.x,
            origin.y,
            SRCCOPY,
        )
    }?;
    ensure!(
        unsafe { GdiFlush() }.as_bool(),
        "flush layered source readback"
    );
    unsafe {
        std::ptr::copy_nonoverlapping(
            readback.bits.cast::<u8>(),
            pixels.as_mut_ptr(),
            pixels.len(),
        )
    };
    Ok(pixels)
}

struct Readback {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut std::ffi::c_void,
}

impl Readback {
    fn new(source: HDC, width: u32, height: u32) -> Result<Self> {
        let mut result = Self {
            dc: unsafe { CreateCompatibleDC(source) },
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
        };
        ensure!(!result.dc.is_invalid(), "create layered readback DC");
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        result.bitmap = unsafe {
            CreateDIBSection(result.dc, &info, DIB_RGB_COLORS, &mut result.bits, None, 0)
        }?;
        result.previous = unsafe { SelectObject(result.dc, result.bitmap) };
        ensure!(
            !result.previous.is_invalid(),
            "select layered readback bitmap"
        );
        Ok(result)
    }
}

impl Drop for Readback {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

fn prune() {
    let windows = IMAGES.with(|images| {
        images
            .borrow()
            .iter()
            .map(|(window, _)| *window)
            .collect::<Vec<_>>()
    });
    let live = windows
        .into_iter()
        .filter(|window| unsafe { IsWindow(*window) }.as_bool())
        .collect::<Vec<_>>();
    IMAGES.with(|images| {
        images
            .borrow_mut()
            .retain(|(window, _)| live.contains(window))
    });
}

fn publish(hwnd: HWND, bitmap: std::result::Result<Bitmap, String>) {
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        // The original upload can synchronously invoke another popup's upload.
        // Recheck after it returns; that nested call may have consumed the budget
        // that was available when this bitmap was read.
        let bitmap = bitmap.and_then(|bitmap| {
            let other_pixels = images
                .iter()
                .filter(|(window, _)| *window != hwnd)
                .filter_map(|(_, bitmap)| bitmap.as_ref().ok())
                .map(|bitmap| bitmap.bgra.len() / 4)
                .sum::<usize>();
            if other_pixels <= MAX_PIXELS - bitmap.bgra.len() / 4 {
                Ok(bitmap)
            } else {
                Err("layered popups exceed capture cache budget".into())
            }
        });
        if let Some((_, previous)) = images.iter_mut().find(|(window, _)| *window == hwnd) {
            *previous = bitmap;
        } else if images.len() < MAX_WINDOWS {
            images.push((hwnd, bitmap));
        }
    });
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod hooks {
    use super::*;
    use minhook::MinHook;
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::rc::Rc;
    use std::sync::{
        OnceLock,
        atomic::{AtomicPtr, Ordering},
    };
    use windows::Win32::Foundation::{BOOL, ERROR_INVALID_FUNCTION, GetLastError, SetLastError};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetWindowLongW, GetWindowRect, UPDATELAYEREDWINDOWINFO, WS_CHILD,
    };
    use windows::core::{PCSTR, w};

    type Update = unsafe extern "system" fn(
        HWND,
        HDC,
        *const POINT,
        *const SIZE,
        HDC,
        *const POINT,
        COLORREF,
        *const BLENDFUNCTION,
        u32,
    ) -> BOOL;
    type Indirect = unsafe extern "system" fn(HWND, *const UPDATELAYEREDWINDOWINFO) -> BOOL;
    static DIRECT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static INDIRECT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    thread_local! { static UPDATING: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) }; }

    pub(super) fn initialize() -> Result<()> {
        INSTALLED
            .get_or_init(|| {
                std::panic::catch_unwind(install)
                    .unwrap_or_else(|_| Err(anyhow!("layered upload hook initialization panicked")))
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|error| anyhow!(error.clone()))
            .copied()
    }

    fn install() -> Result<()> {
        let module = unsafe { GetModuleHandleW(w!("user32.dll")) }?;
        let address = |name: &'static [u8]| -> Result<*mut c_void> {
            unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }
                .map(|address| address as *const () as *mut c_void)
                .ok_or_else(|| anyhow!("layered upload API unavailable"))
        };
        let direct = address(b"UpdateLayeredWindow\0")?;
        let indirect = address(b"UpdateLayeredWindowIndirect\0")?;
        let original =
            unsafe { MinHook::create_hook(direct, direct_hook as *const () as *mut c_void) }
                .map_err(|error| anyhow!("hook layered upload: {error:?}"))?;
        DIRECT.store(original, Ordering::Release);
        let original = match unsafe {
            MinHook::create_hook(indirect, indirect_hook as *const () as *mut c_void)
        } {
            Ok(original) => original,
            Err(error) => {
                let _ = unsafe { MinHook::remove_hook(direct) };
                bail!("hook indirect layered upload: {error:?}")
            }
        };
        INDIRECT.store(original, Ordering::Release);
        // Queue only our upload hooks so activation scans and suspends the process
        // threads once, with both trampolines available before any callback can run.
        let enabled = (|| -> Result<()> {
            for address in [direct, indirect] {
                unsafe { MinHook::queue_enable_hook(address) }
                    .map_err(|error| anyhow!("queue layered upload hook: {error:?}"))?;
            }
            unsafe { MinHook::apply_queued() }
                .map_err(|error| anyhow!("enable layered upload hooks: {error:?}"))
        })();
        if let Err(error) = enabled {
            for address in [direct, indirect] {
                let _ = unsafe { MinHook::queue_disable_hook(address) };
            }
            let _ = unsafe { MinHook::apply_queued() };
            // Activation may partially succeed. Keep trampolines alive for callbacks
            // already in flight, including if rollback fails.
            bail!(error);
        }
        Ok(())
    }

    struct Scope {
        input: Rc<super::super::CaptureInput>,
        previous: HWND,
    }
    impl Scope {
        fn enter(hwnd: HWND) -> Option<Self> {
            if UPDATING
                .try_with(|current| current.get() == hwnd)
                .unwrap_or(true)
            {
                return None;
            }
            let input = super::super::CaptureInput::for_window(hwnd)?;
            if hwnd == input.root
                || unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32 & WS_CHILD.0 != 0
            {
                return None;
            }
            let previous = UPDATING.try_with(|current| current.replace(hwnd)).ok()?;
            Some(Self { input, previous })
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            let _ = UPDATING.try_with(|current| current.set(self.previous));
        }
    }

    struct Prepared {
        destination: POINT,
        logical: Option<POINT>,
        bitmap: Option<std::result::Result<Bitmap, String>>,
        blend: Option<BLENDFUNCTION>,
    }

    fn prepare(
        scope: &Scope,
        hwnd: HWND,
        requested: *const POINT,
        mut upload: Upload,
        has_size: bool,
    ) -> Prepared {
        let mut bounds = RECT::default();
        let known_bounds = unsafe { GetWindowRect(hwnd, &mut bounds) }.is_ok();
        if !has_size {
            upload.size = SIZE {
                cx: (bounds.right - bounds.left).max(1),
                cy: (bounds.bottom - bounds.top).max(1),
            };
        }
        let (x, y) = super::super::capture_origin((upload.size.cx, upload.size.cy));
        let destination = POINT { x, y };
        let logical = unsafe { requested.as_ref() }
            .filter(|requested| {
                (requested.x, requested.y) != (destination.x, destination.y)
                    && (!known_bounds || (requested.x, requested.y) != (bounds.left, bounds.top))
            })
            .copied();
        let hooked = scope.input.hook(hwnd);
        prune();
        let bitmap = (!upload.source.is_invalid()).then(|| {
            hooked
                .and_then(|_| snapshot(hwnd, upload))
                .map_err(|error| format!("{error:#}"))
        });
        Prepared {
            destination,
            logical,
            bitmap,
            blend: upload.blend,
        }
    }

    unsafe fn intercept(
        hwnd: HWND,
        destination: *const POINT,
        upload: Upload,
        has_size: bool,
        forward: impl FnOnce(*const POINT) -> BOOL,
    ) -> BOOL {
        let _physical = super::super::cursor::coordinates(false);
        let incoming_error = unsafe { GetLastError() };
        let scope = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Scope::enter(hwnd)))
            .ok()
            .flatten();
        let Some(scope) = scope else {
            unsafe { SetLastError(incoming_error) };
            return forward(destination);
        };
        let mut prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            prepare(&scope, hwnd, destination, upload, has_size)
        }))
        .ok();
        let target = prepared.as_ref().map_or(destination, |prepared| {
            std::ptr::from_ref(&prepared.destination)
        });
        unsafe { SetLastError(incoming_error) };
        let result = forward(target);
        let outgoing_error = unsafe { GetLastError() };
        if result.as_bool() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(prepared) = prepared.take() {
                    if let Some(logical) = prepared.logical {
                        scope.input.remember_popup(hwnd, logical);
                    }
                    if let Some(bitmap) = prepared.bitmap {
                        publish(hwnd, bitmap);
                    } else if let Some(blend) = prepared.blend {
                        IMAGES.with(|images| {
                            if let Some((_, Ok(bitmap))) = images
                                .borrow_mut()
                                .iter_mut()
                                .find(|(window, _)| *window == hwnd)
                            {
                                bitmap.blend = blend;
                            }
                        });
                    }
                }
            }));
        }
        drop(prepared);
        drop(scope);
        unsafe { SetLastError(outgoing_error) };
        result
    }

    unsafe extern "system" fn direct_hook(
        hwnd: HWND,
        target: HDC,
        destination: *const POINT,
        size: *const SIZE,
        source: HDC,
        origin: *const POINT,
        key: COLORREF,
        blend: *const BLENDFUNCTION,
        flags: u32,
    ) -> BOOL {
        let original = DIRECT.load(Ordering::Acquire);
        if original.is_null() {
            unsafe { SetLastError(ERROR_INVALID_FUNCTION) };
            return BOOL(0);
        }
        let original = unsafe { std::mem::transmute::<*mut c_void, Update>(original) };
        let upload = Upload {
            source,
            size: unsafe { size.as_ref() }.copied().unwrap_or_default(),
            origin: unsafe { origin.as_ref() }.copied().unwrap_or_default(),
            blend: unsafe { blend.as_ref() }.copied(),
            flags,
            key,
            dirty: None,
        };
        unsafe {
            intercept(hwnd, destination, upload, !size.is_null(), |destination| {
                original(
                    hwnd,
                    target,
                    destination,
                    size,
                    source,
                    origin,
                    key,
                    blend,
                    flags,
                )
            })
        }
    }

    unsafe extern "system" fn indirect_hook(
        hwnd: HWND,
        info: *const UPDATELAYEREDWINDOWINFO,
    ) -> BOOL {
        let original = INDIRECT.load(Ordering::Acquire);
        if original.is_null() {
            unsafe { SetLastError(ERROR_INVALID_FUNCTION) };
            return BOOL(0);
        }
        let original = unsafe { std::mem::transmute::<*mut c_void, Indirect>(original) };
        let Some(info_value) = (unsafe { info.as_ref() })
            .filter(|info| info.cbSize as usize == std::mem::size_of::<UPDATELAYEREDWINDOWINFO>())
        else {
            return unsafe { original(hwnd, info) };
        };
        let upload = Upload {
            source: info_value.hdcSrc,
            size: unsafe { info_value.psize.as_ref() }
                .copied()
                .unwrap_or_default(),
            origin: unsafe { info_value.pptSrc.as_ref() }
                .copied()
                .unwrap_or_default(),
            blend: unsafe { info_value.pblend.as_ref() }.copied(),
            flags: info_value.dwFlags.0,
            key: info_value.crKey,
            dirty: unsafe { info_value.prcDirty.as_ref() }.copied(),
        };
        unsafe {
            intercept(
                hwnd,
                info_value.pptDst,
                upload,
                !info_value.psize.is_null(),
                |destination| {
                    if destination == info_value.pptDst {
                        original(hwnd, info)
                    } else {
                        let mut changed = *info_value;
                        changed.pptDst = destination;
                        original(hwnd, &changed)
                    }
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_rows_and_alpha_survive_both_dib_orientations_and_subrectangles() -> Result<()> {
        let top = [16u8, 32, 64, 128, 0, 0, 0, 0];
        let bottom = [80u8, 40, 20, 160, 8, 16, 24, 32];
        let expected = [top.as_slice(), bottom.as_slice()].concat();
        for top_down in [true, false] {
            let mut source = Readback::new(HDC::default(), 2, 2)?;
            if !top_down {
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: 2,
                        biHeight: 2,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits = std::ptr::null_mut();
                let bitmap = unsafe {
                    CreateDIBSection(source.dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)
                }?;
                unsafe {
                    SelectObject(source.dc, bitmap);
                    let _ = DeleteObject(source.bitmap);
                }
                source.bitmap = bitmap;
                source.bits = bits;
            }
            let bytes = if top_down {
                expected.clone()
            } else {
                [bottom.as_slice(), top.as_slice()].concat()
            };
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), source.bits.cast(), bytes.len());
            }
            assert_eq!(read_source(source.dc, POINT::default(), 2, 2)?, expected);
            assert_eq!(
                read_source(source.dc, POINT { x: 1, y: 1 }, 1, 1)?,
                bottom[4..]
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires native Win32 windows and API hooks"]
    fn native_uploads_retain_alpha_dirty_pixels_and_position_without_moving_unrelated_windows()
    -> Result<()> {
        use windows::Win32::Foundation::{GetLastError, HINSTANCE, SetLastError, WIN32_ERROR};
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;
        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }
        let source_window = super::super::HostWindow::create(
            crate::plugins::Placement::CaptureSource,
            64,
            64,
            "layered capture regression",
            false,
        )?;
        let root = HWND(source_window.handle() as *mut _);
        let input = super::super::CaptureInput::new(root)?;
        let popup = |owner: HWND| -> Result<Window> {
            Ok(Window(unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    w!("layered regression"),
                    WS_POPUP,
                    10,
                    20,
                    2,
                    2,
                    owner,
                    None,
                    HINSTANCE::default(),
                    None,
                )
            }?))
        };
        let owned = popup(root)?;
        let other = popup(HWND::default())?;
        let source = Readback::new(HDC::default(), 2, 2)?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                [16u8, 32, 64, 128].repeat(4).as_ptr(),
                source.bits.cast(),
                16,
            )
        };
        let size = SIZE { cx: 2, cy: 2 };
        let origin = POINT::default();
        let mut position = POINT { x: 180, y: 240 };
        let mut blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 128,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let upload = |hwnd, position: &POINT, flags| unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(position),
                Some(&size),
                source.dc,
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                flags,
            )
        };
        upload(owned.0, &position, ULW_ALPHA)?;
        let _ = unsafe { ShowWindow(owned.0, SW_SHOWNOACTIVATE) };
        let captured = image(owned.0)?.ok_or_else(|| anyhow!("owned upload was not retained"))?;
        assert_eq!((captured.width, captured.height), (2, 2));
        assert_eq!(captured.rgba, [128u8, 64, 32, 64].repeat(4));
        let mut actual = RECT::default();
        unsafe { GetWindowRect(owned.0, &mut actual) }?;
        assert!(actual.right < unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) });
        assert!(actual.bottom < unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) });
        // Capture interception must not turn a failed update into a successful one
        // or replace the last successful pixels with the rejected source image.
        unsafe { SetLastError(WIN32_ERROR(0x1234)) };
        assert!(upload(owned.0, &position, UPDATE_LAYERED_WINDOW_FLAGS(0x80000000)).is_err());
        assert_ne!(unsafe { GetLastError() }, WIN32_ERROR(0x1234));
        assert_eq!(image(owned.0)?.unwrap().rgba, captured.rgba);
        upload(other.0, &position, ULW_ALPHA)?;
        unsafe { GetWindowRect(other.0, &mut actual) }?;
        assert_eq!((actual.left, actual.top), (position.x, position.y));
        assert!(image(other.0)?.is_none());
        position = POINT { x: 260, y: 300 };
        unsafe {
            UpdateLayeredWindow(
                owned.0,
                None,
                Some(&position),
                None,
                None,
                None,
                COLORREF(0),
                None,
                UPDATE_LAYERED_WINDOW_FLAGS(0),
            )
        }?;
        assert_eq!(image(owned.0)?.unwrap().rgba, captured.rgba);
        unsafe {
            std::ptr::copy_nonoverlapping(
                [0u8, 0, 255, 255].repeat(4).as_ptr(),
                source.bits.cast(),
                16,
            )
        };
        blend.SourceConstantAlpha = 64;
        let dirty = RECT {
            left: 1,
            top: 0,
            right: 2,
            bottom: 1,
        };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            pptDst: &position,
            psize: &size,
            hdcSrc: source.dc,
            pptSrc: &origin,
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: &dirty,
            ..Default::default()
        };
        ensure!(
            unsafe { UpdateLayeredWindowIndirect(owned.0, &info) }.as_bool(),
            "dirty native upload failed"
        );
        let pixels = image(owned.0)?.unwrap().rgba;
        assert_eq!(&pixels[0..4], &[128, 64, 32, 32]);
        assert_eq!(&pixels[4..8], &[255, 0, 0, 64]);
        assert_eq!(&pixels[8..12], &[128, 64, 32, 32]);
        assert_eq!(&pixels[12..16], &[128, 64, 32, 32]);
        assert_eq!(input.popups()?.len(), 1);
        drop(owned);
        assert!(
            IMAGES.with(|images| images.borrow().is_empty()),
            "destroyed popup pixels were retained"
        );
        Ok(())
    }

    #[test]
    fn source_alpha_is_unpremultiplied_once_and_constant_opacity_is_applied_once() {
        let mut blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 128,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        assert_eq!(
            rgba_pixel(&[16, 32, 64, 128], blend, 2, COLORREF(0)),
            [128, 64, 32, 64]
        );
        assert_eq!(
            rgba_pixel(&[255, 255, 255, 0], blend, 2, COLORREF(0)),
            [0; 4]
        );
        blend.AlphaFormat = 0;
        assert_eq!(
            rgba_pixel(&[16, 32, 64, 0], blend, 2, COLORREF(0)),
            [64, 32, 16, 128]
        );
        assert_eq!(
            rgba_pixel(&[16, 32, 64, 0], blend, 4, COLORREF(0)),
            [64, 32, 16, 255]
        );
        assert_eq!(
            rgba_pixel(&[16, 32, 64, 255], blend, 1, COLORREF(0x102040)),
            [0; 4]
        );
        assert_eq!(
            rgba_pixel(&[17, 32, 64, 0], blend, 1, COLORREF(0x102040)),
            [64, 32, 17, 255]
        );
    }

    #[test]
    fn popup_dimensions_bound_cache_allocations() {
        assert!(dimensions(SIZE { cx: 0, cy: 10 }).is_err());
        assert!(dimensions(SIZE { cx: -1, cy: 10 }).is_err());
        assert!(dimensions(SIZE { cx: 8193, cy: 10 }).is_err());
        assert!(dimensions(SIZE { cx: 8192, cy: 8192 }).is_err());
        assert_eq!(
            dimensions(SIZE { cx: 4096, cy: 4096 }).unwrap().2,
            MAX_PIXELS
        );
    }
}
