//! Main-thread ownership of the native drawable readback adapter.
use std::{ffi::c_void, ptr::NonNull};

use anyhow::{Result, ensure};
use objc2_app_kit::NSView;

use super::{EditorFrame, MAX_CAPTURE_PIXELS, convert_rgba8};

#[repr(C)]
#[derive(Default)]
struct FrameInfo {
    width: u32,
    height: u32,
    native_width: u32,
    native_height: u32,
    bgra: u32,
    sequence: u64,
}

unsafe extern "C" {
    fn np_metal_capture_create(root: *mut c_void) -> *mut c_void;
    fn np_metal_capture_current(handle: *mut c_void, root: *mut c_void) -> bool;
    fn np_metal_capture_copy(
        handle: *mut c_void,
        info: *mut FrameInfo,
        bytes: *mut u8,
        capacity: usize,
    ) -> usize;
    fn np_metal_capture_failure(handle: *mut c_void) -> u32;
    fn np_metal_capture_destroy(handle: *mut c_void);
}

pub(super) struct MetalCapture {
    handle: NonNull<c_void>,
}

impl MetalCapture {
    pub fn attach(view: &NSView) -> Option<Self> {
        // SAFETY: NativeCapture retains this live NSView and confines use/drop to its owner thread.
        let handle = NonNull::new(unsafe {
            np_metal_capture_create((view as *const NSView).cast_mut().cast())
        })?;
        log::debug!("attached completed-drawable Metal editor capture");
        Some(Self { handle })
    }

    pub fn is_current(&self, root: &NSView) -> bool {
        // SAFETY: both pointers remain live on the owner thread.
        unsafe {
            np_metal_capture_current(
                self.handle.as_ptr(),
                (root as *const NSView).cast_mut().cast(),
            )
        }
    }

    pub fn frame(&mut self) -> Result<Option<EditorFrame>> {
        // SAFETY: handle remains owned by this adapter; native publication uses immutable snapshots.
        let failure = unsafe { np_metal_capture_failure(self.handle.as_ptr()) };
        ensure!(
            failure == 0,
            "Metal editor readback failed ({failure}); reopen the editor or use Pop out"
        );
        let mut info = FrameInfo::default();
        let needed = unsafe {
            np_metal_capture_copy(self.handle.as_ptr(), &mut info, std::ptr::null_mut(), 0)
        };
        if needed == 0 {
            return Ok(None);
        }
        ensure!(
            needed <= MAX_CAPTURE_PIXELS as usize * 4,
            "Metal editor exceeds capture pixel budget"
        );
        let mut bytes = vec![0; needed];
        // A resize/publication may race the allocation. Skip that frame if its byte count changed.
        let copied = unsafe {
            np_metal_capture_copy(
                self.handle.as_ptr(),
                &mut info,
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        };
        if copied != bytes.len() {
            return Ok(None);
        }
        ensure!(
            u64::from(info.width) * u64::from(info.height) * 4 == copied as u64,
            "invalid Metal editor image geometry"
        );
        let map = if info.bgra != 0 {
            [2, 1, 0, 3]
        } else {
            [0, 1, 2, 3]
        };
        let rgba = convert_rgba8(
            &bytes,
            info.width as usize,
            info.height as usize,
            info.width as usize * 4,
            map,
            true,
        )?;
        Ok(Some(EditorFrame {
            width: info.width,
            height: info.height,
            native_size: (info.native_width, info.native_height),
            native_units_per_point: 1.0,
            rgba,
            popups: Vec::new(),
        }))
    }
}

impl Drop for MetalCapture {
    fn drop(&mut self) {
        // SAFETY: exactly one destroy, on the owner thread, before the plugin view is torn down.
        unsafe { np_metal_capture_destroy(self.handle.as_ptr()) };
    }
}
