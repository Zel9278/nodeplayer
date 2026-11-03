//! Capture an existing, mapped X11 editor without taking ownership of its window.
//!
//! XComposite retains the complete hierarchy while its native source is obscured. The
//! capture connection owns its redirection, named pixmap and an unmapped timestamp
//! helper; the editor's backend
//! continues to own mapping, resizing, event pumping, and destruction. A capture-only
//! host remains mapped outside the root window with physical input disabled.

use anyhow::{Context, Result, bail, ensure};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

use super::{EditorFrame, EditorPointer, PointerKind};

const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: usize = 16 * 1024 * 1024;

pub struct NativeCapture {
    conn: RustConnection,
    root: Window,
    window: Window,
    pixmap: Option<Pixmap>,
    masks: [u32; 3],
    pointer_target: Option<Window>,
    timestamp_window: Option<Window>,
}

impl NativeCapture {
    pub fn new(handle: crate::plugins::WindowId) -> Result<Self> {
        let window = u32::try_from(handle).context("editor X11 handle exceeds the XID range")?;
        ensure!(window != x11rb::NONE, "empty editor X11 handle");
        let (conn, _) = x11rb::connect(None).context("connect to the editor's X11 display")?;
        let geometry = conn
            .get_geometry(window)?
            .reply()
            .context("query editor window")?;
        let attributes = conn.get_window_attributes(window)?.reply()?;
        ensure!(
            attributes.map_state == MapState::VIEWABLE,
            "capture source must remain mapped and viewable"
        );
        ensure!(
            attributes.class == WindowClass::INPUT_OUTPUT,
            "capture source has no drawable content"
        );
        validate_size((u32::from(geometry.width), u32::from(geometry.height)))?;
        let visual = conn
            .setup()
            .roots
            .iter()
            .find(|screen| screen.root == geometry.root)
            .into_iter()
            .flat_map(|screen| &screen.allowed_depths)
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == attributes.visual)
            .context("editor X11 visual not found")?;
        ensure!(
            visual.class == VisualClass::TRUE_COLOR,
            "editor capture requires a TrueColor X11 visual"
        );
        let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
        ensure!(
            masks.iter().all(|mask| *mask != 0),
            "editor visual has an empty color mask"
        );
        let version = conn
            .composite_query_version(0, 4)
            .context("XComposite is required for editor capture")?
            .reply()?;
        ensure!(
            version.major_version > 0 || version.minor_version >= 2,
            "XComposite 0.2 or newer is required"
        );
        // Event selection is per client; this does not replace the editor owner's mask.
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        conn.composite_redirect_window(window, Redirect::AUTOMATIC)?
            .check()
            .context("redirect the existing native editor hierarchy")?;
        // Construct immediately after redirecting so failed pixmap acquisition still
        // removes the redirection when this value is dropped.
        let mut capture = Self {
            conn,
            root: geometry.root,
            window,
            pixmap: None,
            masks,
            pointer_target: None,
            timestamp_window: None,
        };
        let timestamp_window = capture.conn.generate_id()?;
        capture.timestamp_window = Some(timestamp_window);
        capture
            .conn
            .create_window(
                0,
                timestamp_window,
                capture.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()
            .context("create editor input timestamp helper")?;
        capture.rename_pixmap()?;
        capture.conn.flush()?;
        Ok(capture)
    }

    fn server_time(&self) -> Result<Timestamp> {
        let helper = self
            .timestamp_window
            .context("editor input timestamp helper missing")?;
        // CurrentTime is a sentinel in requests, but XSendEvent preserves a zero
        // event.time verbatim. Native toolkits use that field for click tracking
        // and may pass it back to focus/grab requests. Ask this display for a real
        // timestamp without changing a plugin property or mapping another window.
        self.conn
            .change_property(
                PropMode::REPLACE,
                helper,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                8,
                1,
                &[0],
            )?
            .check()
            .context("request editor input timestamp")?;
        // The reply ensures the preceding PropertyNotify is already queued. Never
        // wait indefinitely for an event or drain an unbounded resize stream.
        self.conn.get_input_focus()?.reply()?;
        for _ in 0..256 {
            let Some(event) = self.conn.poll_for_event()? else {
                break;
            };
            match event {
                Event::PropertyNotify(event)
                    if event.window == helper && event.atom == u32::from(AtomEnum::WM_NAME) =>
                {
                    ensure!(
                        event.time != x11rb::CURRENT_TIME,
                        "editor input timestamp is zero"
                    );
                    return Ok(event.time);
                }
                Event::Error(error) => bail!("editor input X11 request failed: {error:?}"),
                Event::DestroyNotify(event) if event.window == self.window => {
                    bail!("editor capture source was destroyed")
                }
                _ => {}
            }
        }
        bail!("editor input timestamp notification missing")
    }

    fn rename_pixmap(&mut self) -> Result<()> {
        if let Some(pixmap) = self.pixmap.take() {
            self.conn.free_pixmap(pixmap)?.check()?;
        }
        let pixmap = self.conn.generate_id()?;
        self.conn
            .composite_name_window_pixmap(self.window, pixmap)?
            .check()
            .context("name the editor pixmap (its native source must remain mapped)")?;
        self.pixmap = Some(pixmap);
        Ok(())
    }

    fn check_source(&mut self) -> Result<()> {
        // Only source-structure events are selected on this connection. A bound keeps a
        // continuously resizing source from monopolizing the worker's main thread.
        for _ in 0..256 {
            let Some(event) = self.conn.poll_for_event()? else {
                break;
            };
            match event {
                Event::Error(error) => bail!("editor capture X11 request failed: {error:?}"),
                Event::DestroyNotify(event) if event.window == self.window => {
                    bail!("editor capture source was destroyed")
                }
                _ => {}
            }
        }
        let attributes = self.conn.get_window_attributes(self.window)?.reply()?;
        ensure!(
            attributes.map_state == MapState::VIEWABLE,
            "capture source must remain mapped and viewable"
        );
        Ok(())
    }

    pub fn capture(&mut self, native_size: (u32, u32)) -> Result<EditorFrame> {
        validate_size(native_size)?;
        self.check_source()?;
        let geometry = self.conn.get_geometry(self.window)?.reply()?;
        let size = (u32::from(geometry.width), u32::from(geometry.height));
        let pixels = validate_size(size)?;
        // A compositor can replace offscreen storage while taking over redirection
        // without resizing or remapping this window. A retained name then keeps
        // reading the abandoned image forever, even while the editor repaints.
        // Resolve the current storage for every frame rather than relying on
        // source-structure events to invalidate the previous pixmap.
        self.rename_pixmap()?;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.pixmap.context("editor has no XComposite pixmap")?,
                0,
                0,
                geometry.width,
                geometry.height,
                u32::MAX,
            )?
            .reply()
            .context("read redirected editor pixels")?;
        let format = self
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .context("editor X11 pixel format is unavailable")?;
        ensure!(
            matches!(format.bits_per_pixel, 8 | 16 | 24 | 32),
            "unsupported editor X11 pixel size {}",
            format.bits_per_pixel
        );
        ensure!(
            format.scanline_pad > 0 && format.scanline_pad % 8 == 0,
            "invalid editor X11 scanline padding"
        );
        let width = size.0 as usize;
        let height = size.1 as usize;
        let bytes_per_pixel = usize::from(format.bits_per_pixel) / 8;
        let pad = usize::from(format.scanline_pad) / 8;
        let stride = width
            .checked_mul(bytes_per_pixel)
            .context("editor row size overflow")?
            .div_ceil(pad)
            * pad;
        let expected = stride
            .checked_mul(height)
            .context("editor image size overflow")?;
        ensure!(
            reply.data.len() >= expected && reply.data.len() - expected < 4,
            "invalid editor X11 image size: {} instead of {expected}",
            reply.data.len()
        );
        let little = self.conn.setup().image_byte_order == ImageOrder::LSB_FIRST;
        let mut rgba = Vec::with_capacity(pixels * 4);
        for row in reply.data[..expected].chunks_exact(stride) {
            for bytes in row[..width * bytes_per_pixel].chunks_exact(bytes_per_pixel) {
                let mut pixel = 0u32;
                if little {
                    for (index, byte) in bytes.iter().enumerate() {
                        pixel |= u32::from(*byte) << (index * 8);
                    }
                } else {
                    for byte in bytes {
                        pixel = (pixel << 8) | u32::from(*byte);
                    }
                }
                rgba.extend_from_slice(&[
                    unpack_channel(pixel, self.masks[0]),
                    unpack_channel(pixel, self.masks[1]),
                    unpack_channel(pixel, self.masks[2]),
                    255,
                ]);
            }
        }
        // Source geometry wins if the plugin resized between the owner's metadata
        // snapshot and this read. On X11 native input units are source pixels.
        Ok(EditorFrame {
            popups: Vec::new(),
            width: size.0,
            height: size.1,
            native_size: size,
            rgba,
        })
    }

    /// Synthetic delivery does not move the user's real pointer out of the graph. Some
    /// toolkits and Wine bridges reject XSendEvent or insist on a real pointer grab.
    pub fn pointer(&mut self, event: EditorPointer) -> Result<()> {
        ensure!(
            event.position.iter().all(|value| value.is_finite()),
            "non-finite editor pointer position"
        );
        ensure!(event.buttons & !7 == 0, "invalid editor pointer buttons");
        let x = event.position[0]
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        let y = event.position[1]
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        let root_position = self
            .conn
            .translate_coordinates(self.window, self.root, x, y)?
            .reply()?;
        let root = (root_position.dst_x, root_position.dst_y);
        let (target, local) = match self.pointer_destination(root) {
            Ok(destination) => destination,
            Err(error) => {
                // A plugin can destroy its dragged child. Do not keep routing later
                // gestures to that stale XID after reporting the failed delivery.
                self.pointer_target = None;
                return Err(error);
            }
        };
        let mut state = button_mask(event.buttons);
        for (bit, mask) in [
            (super::MOD_SHIFT, KeyButMask::SHIFT),
            (super::MOD_CONTROL, KeyButMask::CONTROL),
            (super::MOD_ALT, KeyButMask::MOD1),
            (super::MOD_SUPER, KeyButMask::MOD4),
        ] {
            if event.modifiers & bit != 0 {
                state |= mask;
            }
        }
        match event.kind {
            PointerKind::Cancel => {
                self.pointer_target = None;
            }
            PointerKind::Move => {
                self.conn
                    .send_event(
                        false,
                        target,
                        EventMask::POINTER_MOTION,
                        MotionNotifyEvent {
                            response_type: MOTION_NOTIFY_EVENT,
                            detail: Motion::NORMAL,
                            sequence: 0,
                            time: self.server_time()?,
                            root: self.root,
                            event: target,
                            child: x11rb::NONE,
                            root_x: root.0,
                            root_y: root.1,
                            event_x: local.0,
                            event_y: local.1,
                            state,
                            same_screen: true,
                        },
                    )?
                    .check()?;
            }
            PointerKind::Down(button) | PointerKind::Up(button) => {
                let pressed = matches!(event.kind, PointerKind::Down(_));
                let detail = x_button(button)?;
                let bit = button_bit(button);
                // X11 button-event state describes the buttons before this transition;
                // the shared pointer event describes the buttons after it.
                let state = if pressed {
                    KeyButMask::from(u16::from(state) & !u16::from(bit))
                } else {
                    state | bit
                };
                self.send_button(target, local, root, detail, pressed, state)?;
                if pressed {
                    self.pointer_target = Some(target);
                } else if event.buttons == 0 {
                    self.pointer_target = None;
                }
            }
            PointerKind::Wheel { dx, dy } => {
                ensure!(
                    dx.is_finite() && dy.is_finite(),
                    "non-finite editor wheel delta"
                );
                for (delta, negative, positive) in [(dy, 5, 4), (dx, 7, 6)] {
                    if delta.abs() < f32::EPSILON {
                        continue;
                    }
                    let detail = if delta > 0.0 { positive } else { negative };
                    let steps = (delta.abs() / 40.0).ceil().clamp(1.0, 12.0) as usize;
                    for _ in 0..steps {
                        self.send_button(target, local, root, detail, true, state)?;
                        self.send_button(target, local, root, detail, false, state)?;
                    }
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn pointer_destination(&self, root: (i16, i16)) -> Result<(Window, (i16, i16))> {
        let mut target = self.pointer_target.unwrap_or(self.window);
        for _ in 0..64 {
            let at = self
                .conn
                .translate_coordinates(self.root, target, root.0, root.1)?
                .reply()?;
            if self.pointer_target.is_some() || at.child == x11rb::NONE {
                return Ok((target, (at.dst_x, at.dst_y)));
            }
            target = at.child;
        }
        bail!("editor native child hierarchy exceeds capture input limit")
    }

    fn send_button(
        &self,
        target: Window,
        local: (i16, i16),
        root: (i16, i16),
        detail: u8,
        pressed: bool,
        state: KeyButMask,
    ) -> Result<()> {
        self.conn
            .send_event(
                false,
                target,
                if pressed {
                    EventMask::BUTTON_PRESS
                } else {
                    EventMask::BUTTON_RELEASE
                },
                ButtonPressEvent {
                    response_type: if pressed {
                        BUTTON_PRESS_EVENT
                    } else {
                        BUTTON_RELEASE_EVENT
                    },
                    detail,
                    sequence: 0,
                    time: self.server_time()?,
                    root: self.root,
                    event: target,
                    child: x11rb::NONE,
                    root_x: root.0,
                    root_y: root.1,
                    event_x: local.0,
                    event_y: local.1,
                    state,
                    same_screen: true,
                },
            )?
            .check()?;
        Ok(())
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        if let Some(window) = self.timestamp_window.take() {
            let _ = self.conn.destroy_window(window);
        }
        if let Some(pixmap) = self.pixmap.take() {
            let _ = self.conn.free_pixmap(pixmap);
        }
        let _ = self
            .conn
            .composite_unredirect_window(self.window, Redirect::AUTOMATIC);
        let _ = self.conn.flush();
        // The editor backend owns the source, its descendants, and all geometry.
    }
}

fn validate_size(size: (u32, u32)) -> Result<usize> {
    ensure!(
        (1..=MAX_DIMENSION).contains(&size.0) && (1..=MAX_DIMENSION).contains(&size.1),
        "editor capture dimensions must be 1..{MAX_DIMENSION}"
    );
    let pixels = (size.0 as usize)
        .checked_mul(size.1 as usize)
        .context("editor pixel count overflow")?;
    ensure!(
        pixels <= MAX_PIXELS,
        "editor capture exceeds the pixel budget"
    );
    Ok(pixels)
}

fn unpack_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = u64::from(mask >> shift);
    let value = u64::from((pixel & mask) >> shift);
    ((value * 255 + max / 2) / max) as u8
}

fn x_button(button: u8) -> Result<u8> {
    match button {
        1 => Ok(1),
        2 => Ok(3),
        3 => Ok(2),
        _ => bail!("unsupported editor pointer button {button}"),
    }
}

fn button_bit(button: u8) -> KeyButMask {
    match button {
        1 => KeyButMask::BUTTON1,
        2 => KeyButMask::BUTTON3,
        3 => KeyButMask::BUTTON2,
        _ => KeyButMask::default(),
    }
}

fn button_mask(buttons: u8) -> KeyButMask {
    let mut mask = KeyButMask::default();
    for button in 1..=3 {
        if buttons & (1 << (button - 1)) != 0 {
            mask |= button_bit(button);
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_channel(value: u8, mask: u32) -> u32 {
        let shift = mask.trailing_zeros();
        let max = u64::from(mask >> shift);
        ((((u64::from(value) * max + 127) / 255) as u32) << shift) & mask
    }

    #[test]
    fn color_masks_cover_rgb565_and_rgb888() {
        for masks in [[0xff0000, 0xff00, 0xff], [0xf800, 0x7e0, 0x1f]] {
            for value in [0u8, 41, 128, 219, 255] {
                for mask in masks {
                    assert!(value.abs_diff(unpack_channel(pack_channel(value, mask), mask)) <= 4);
                }
            }
        }
    }

    #[test]
    #[ignore = "requires an X11 display with XComposite"]
    fn forwarded_clicks_have_server_timestamps_and_release_the_private_helper() -> Result<()> {
        let source = Source::new()?;
        source
            .conn
            .change_window_attributes(
                source.window,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
            )?
            .check()?;
        let mut capture = NativeCapture::new(source.window as usize)?;
        let helper = capture.timestamp_window.unwrap();
        let attributes = source.conn.get_window_attributes(helper)?.reply()?;
        assert_eq!(attributes.class, WindowClass::INPUT_ONLY);
        assert_eq!(attributes.map_state, MapState::UNMAPPED);
        let mut times = Vec::new();
        for (kind, buttons, click_count) in [
            (PointerKind::Down(1), 1, 1),
            (PointerKind::Up(1), 0, 1),
            (PointerKind::Down(1), 1, 2),
            (PointerKind::Up(1), 0, 2),
        ] {
            capture.pointer(EditorPointer {
                position: [32.0, 24.0],
                kind,
                buttons,
                modifiers: 0,
                click_count,
            })?;
            source.conn.get_input_focus()?.reply()?;
            for _ in 0..256 {
                let Some(event) = source.conn.poll_for_event()? else {
                    break;
                };
                match event {
                    Event::ButtonPress(event) | Event::ButtonRelease(event) => {
                        times.push(event.time);
                    }
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        assert_eq!(times.len(), 4);
        assert!(times.iter().all(|time| *time != x11rb::CURRENT_TIME));
        assert!(times.windows(2).all(|pair| {
            let elapsed = pair[1].wrapping_sub(pair[0]);
            elapsed > 0 && elapsed < 400
        }));
        // A toolkit's normal X11 double-click detector now sees a valid recent
        // first press. These are server times, suitable for native grab requests.
        assert!(times[2].wrapping_sub(times[0]) < 400);
        drop(capture);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while source.conn.get_window_attributes(helper)?.reply().is_ok() {
            ensure!(
                std::time::Instant::now() < deadline,
                "timestamp helper leaked"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(source.conn.get_geometry(source.window)?.reply()?.width, 640);
        Ok(())
    }

    struct Source {
        conn: RustConnection,
        root: Window,
        window: Window,
        gc: Gcontext,
        masks: [u32; 3],
    }

    impl Source {
        fn new() -> Result<Self> {
            let (conn, screen) = x11rb::connect(None)?;
            let screen = &conn.setup().roots[screen];
            let visual = screen
                .allowed_depths
                .iter()
                .flat_map(|depth| &depth.visuals)
                .find(|visual| visual.visual_id == screen.root_visual)
                .context("fixture visual")?;
            let root = screen.root;
            let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
            let window = conn.generate_id()?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                -16384,
                -16384,
                640,
                360,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .override_redirect(1)
                    .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
            let gc = conn.generate_id()?;
            conn.create_gc(gc, window, &CreateGCAux::new())?.check()?;
            conn.map_window(window)?.check()?;
            Ok(Self {
                conn,
                root,
                window,
                gc,
                masks,
            })
        }

        fn fill(&self, window: Window, rgb: [u8; 3], rect: Rectangle) -> Result<()> {
            let color = rgb
                .into_iter()
                .zip(self.masks)
                .fold(0, |pixel, (value, mask)| pixel | pack_channel(value, mask));
            self.conn
                .change_gc(self.gc, &ChangeGCAux::new().foreground(color))?
                .check()?;
            self.conn
                .poly_fill_rectangle(window, self.gc, &[rect])?
                .check()?;
            Ok(())
        }

        fn paint(&self, width: u16, height: u16) -> Result<()> {
            self.fill(
                self.window,
                [24, 28, 38],
                Rectangle {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            )?;
            self.fill(
                self.window,
                [43, 125, 198],
                Rectangle {
                    x: 40,
                    y: 60,
                    width: 160,
                    height: 70,
                },
            )
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            let _ = self.conn.free_gc(self.gc);
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
        }
    }

    fn pixel(frame: &EditorFrame, x: usize, y: usize) -> &[u8] {
        let at = (y * frame.width as usize + x) * 4;
        &frame.rgba[at..at + 4]
    }

    #[test]
    #[ignore = "requires an X11 display with XComposite"]
    fn capture_survives_obscuration_resize_and_remap_without_owning_source() -> Result<()> {
        let source = Source::new()?;
        let mut capture = NativeCapture::new(source.window as usize)?;
        source.paint(640, 360)?;
        let first = capture.capture((640, 360))?;
        assert_eq!(pixel(&first, 45, 65), [43, 125, 198, 255]);
        let cover = source.conn.generate_id()?;
        source
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                cover,
                source.root,
                -16384,
                -16384,
                1600,
                1000,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(0xff0000)
                    .override_redirect(1),
            )?
            .check()?;
        source.conn.map_window(cover)?.check()?;
        let obscured = capture.capture((640, 360))?;
        assert_eq!(pixel(&obscured, 45, 65), pixel(&first, 45, 65));
        source
            .conn
            .configure_window(
                source.window,
                &ConfigureWindowAux::new().width(720).height(420),
            )?
            .check()?;
        source.paint(720, 420)?;
        // Metadata can lag an asynchronous native resize; the frame must still
        // publish the dimensions that match its pixels and input coordinates.
        let resized = capture.capture((640, 360))?;
        assert_eq!(
            (resized.width, resized.height, resized.native_size),
            (720, 420, (720, 420))
        );
        assert_eq!(pixel(&resized, 45, 65), pixel(&first, 45, 65));
        let previous_pixmap = capture.pixmap;
        source.conn.unmap_window(source.window)?.check()?;
        assert!(capture.capture((720, 420)).is_err());
        source.conn.map_window(source.window)?.check()?;
        source.paint(720, 420)?;
        let remapped = capture.capture((720, 420))?;
        assert_ne!(
            capture.pixmap, previous_pixmap,
            "mapping requires a new backing pixmap"
        );
        assert_eq!(pixel(&remapped, 45, 65), pixel(&first, 45, 65));
        drop(capture);
        assert_eq!(
            source.conn.get_geometry(source.window)?.reply()?.width,
            720,
            "capture must not destroy its source"
        );
        source.conn.destroy_window(cover)?.check()?;
        Ok(())
    }

    #[test]
    #[ignore = "requires an X11 display with XComposite"]
    fn captures_descendants_and_retains_drag_target_until_release() -> Result<()> {
        let source = Source::new()?;
        let mut capture = NativeCapture::new(source.window as usize)?;
        source.paint(640, 360)?;
        let child = source.conn.generate_id()?;
        source
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                child,
                source.window,
                20,
                30,
                200,
                160,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(0x00cc88)
                    .event_mask(
                        EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            | EventMask::POINTER_MOTION,
                    ),
            )?
            .check()?;
        source.conn.map_window(child)?.check()?;
        source.fill(
            child,
            [0, 204, 136],
            Rectangle {
                x: 0,
                y: 0,
                width: 200,
                height: 160,
            },
        )?;
        let frame = capture.capture((640, 360))?;
        assert_eq!(pixel(&frame, 80, 90), [0, 204, 136, 255]);
        for event in [
            EditorPointer {
                modifiers: 0,
                click_count: 1,
                position: [80.0, 90.0],
                kind: PointerKind::Down(1),
                buttons: 1,
            },
            EditorPointer {
                modifiers: 0,
                click_count: 0,
                position: [300.0, 90.0],
                kind: PointerKind::Move,
                buttons: 1,
            },
            EditorPointer {
                modifiers: 0,
                click_count: 1,
                position: [300.0, 90.0],
                kind: PointerKind::Up(1),
                buttons: 0,
            },
        ] {
            capture.pointer(event)?;
        }
        source.conn.get_input_focus()?.reply()?;
        let (mut pressed, mut dragged, mut released) = (false, false, false);
        while let Some(event) = source.conn.poll_for_event()? {
            match event {
                Event::ButtonPress(event) => {
                    assert_eq!((event.event, event.event_x, event.event_y), (child, 60, 60));
                    assert!(!event.state.contains(KeyButMask::BUTTON1));
                    pressed = true;
                }
                Event::MotionNotify(event) => {
                    assert_eq!(
                        (event.event, event.event_x, event.event_y),
                        (child, 280, 60)
                    );
                    assert!(event.state.contains(KeyButMask::BUTTON1));
                    dragged = true;
                }
                Event::ButtonRelease(event) => {
                    assert_eq!(
                        (event.event, event.event_x, event.event_y),
                        (child, 280, 60)
                    );
                    assert!(event.state.contains(KeyButMask::BUTTON1));
                    released = true;
                }
                _ => {}
            }
        }
        assert!(pressed && dragged && released);
        assert!(capture.pointer_target.is_none());
        Ok(())
    }

    #[test]
    #[ignore = "requires an X11 display with XComposite and Shape"]
    fn capture_source_stays_offscreen_without_focus_or_physical_input() -> Result<()> {
        use crate::plugins::{Placement, window::HostWindow};
        use x11rb::protocol::shape::{ConnectionExt as _, SK};

        let (observer, screen) = x11rb::connect(None)?;
        observer.composite_query_version(0, 4)?.reply()?;
        let screen = &observer.setup().roots[screen];
        let root = screen.root;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|visual| visual.visual_id == screen.root_visual)
            .context("capture host fixture visual")?;
        let masks = [visual.red_mask, visual.green_mask, visual.blue_mask];
        let focus = observer.get_input_focus()?.reply()?.focus;
        let pointer = observer.query_pointer(root)?.reply()?;
        let client_list = observer
            .intern_atom(false, b"_NET_CLIENT_LIST")?
            .reply()?
            .atom;

        // Simulate a compositor only on this fixture's source. Taking manual
        // redirection of the desktop root would conflict with a real compositor
        // and change unrelated windows during a native-session test.
        for compositor in [false, true] {
            let background = observer
                .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 32, 32, u32::MAX)?
                .reply()
                .ok()
                .map(|reply| reply.data);
            if background.is_none() {
                eprintln!(
                    "X11 root pixels unavailable; checking source geometry, input, and focus directly"
                );
            }
            for _ in 0..2 {
                let host = HostWindow::create(
                    Placement::CaptureSource,
                    256,
                    144,
                    "capture source",
                    false,
                )?;
                let simulated_compositor = if compositor {
                    match observer
                        .composite_redirect_window(host.id, Redirect::MANUAL)?
                        .check()
                    {
                        Ok(()) => true,
                        // A real compositor may already own manual redirection of
                        // root children. Its source redirection is the same case
                        // this fixture simulates; leave that ownership intact.
                        Err(x11rb::errors::ReplyError::X11Error(error))
                            if error.error_kind == x11rb::protocol::ErrorKind::Access =>
                        {
                            false
                        }
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    false
                };
                let attributes = observer.get_window_attributes(host.id)?.reply()?;
                assert!(attributes.override_redirect);
                assert_eq!(attributes.map_state, MapState::VIEWABLE);
                let geometry = observer.get_geometry(host.id)?.reply()?;
                assert!(i32::from(geometry.x) + i32::from(geometry.width) < 0);
                assert!(i32::from(geometry.y) + i32::from(geometry.height) < 0);
                assert!(
                    observer
                        .shape_get_rectangles(host.id, SK::INPUT)?
                        .reply()?
                        .rectangles
                        .is_empty()
                );
                assert_eq!(observer.get_input_focus()?.reply()?.focus, focus);
                let clients = observer
                    .get_property(false, root, client_list, AtomEnum::WINDOW, 0, u32::MAX)?
                    .reply()?;
                assert!(
                    !clients
                        .value32()
                        .into_iter()
                        .flatten()
                        .any(|id| id == host.id)
                );
                let xembed = host.conn.intern_atom(false, b"_XEMBED")?.reply()?.atom;
                host.conn
                    .send_event(
                        false,
                        host.id,
                        EventMask::NO_EVENT,
                        ClientMessageEvent::new(32, host.id, xembed, [0u32; 5]),
                    )?
                    .check()?;
                assert!(
                    host.poll().is_empty(),
                    "an unrelated zero-timestamp ClientMessage must not close the source"
                );

                let gc = host.conn.generate_id()?;
                host.conn
                    .create_gc(gc, host.id, &CreateGCAux::new())?
                    .check()?;
                let paint = |width: u16, height: u16, green: u8| -> Result<()> {
                    for (rgb, y) in [([255, green, 0], 0), ([0, green, 255], height / 2)] {
                        let color = rgb
                            .into_iter()
                            .zip(masks)
                            .fold(0, |pixel, (value, mask)| pixel | pack_channel(value, mask));
                        host.conn
                            .change_gc(gc, &ChangeGCAux::new().foreground(color))?
                            .check()?;
                        host.conn
                            .poly_fill_rectangle(
                                host.id,
                                gc,
                                &[Rectangle {
                                    x: 0,
                                    y: y as i16,
                                    width,
                                    height: height / 2,
                                }],
                            )?
                            .check()?;
                    }
                    Ok(())
                };
                let mut capture = NativeCapture::new(host.handle())?;
                let paint_and_capture = |capture: &mut NativeCapture,
                                         width: u16,
                                         height: u16,
                                         green: u8,
                                         metadata: (u32, u32)|
                 -> Result<EditorFrame> {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    loop {
                        // Taking over redirection can discard a just-painted
                        // backing store. A real editor repaints on its next tick.
                        paint(width, height, green)?;
                        let frame = capture.capture(metadata)?;
                        if frame.width == u32::from(width)
                            && frame.height == u32::from(height)
                            && pixel(&frame, 4, 4) == [255, green, 0, 255]
                            && pixel(&frame, 4, usize::from(height) - 4) == [0, green, 255, 255]
                        {
                            return Ok(frame);
                        }
                        ensure!(
                            std::time::Instant::now() < deadline,
                            "offscreen source did not publish its repainted backing store"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                };
                let first = paint_and_capture(&mut capture, 256, 144, 0, host.size())?;
                assert_eq!(pixel(&first, 4, 4), [255, 0, 0, 255]);
                assert_eq!(pixel(&first, 4, 140), [0, 0, 255, 255]);
                for green in [64, 128, 192] {
                    let frame = paint_and_capture(&mut capture, 256, 144, green, host.size())?;
                    assert!(
                        frame.rgba != first.rgba,
                        "offscreen source must continue producing new pixels"
                    );
                    assert_eq!(pixel(&frame, 4, 4), [255, green, 0, 255]);
                }

                let child = host.conn.generate_id()?;
                host.conn
                    .create_window(
                        x11rb::COPY_DEPTH_FROM_PARENT,
                        child,
                        host.id,
                        20,
                        30,
                        80,
                        60,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        x11rb::COPY_FROM_PARENT,
                        &CreateWindowAux::new()
                            .background_pixel(0)
                            .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
                    )?
                    .check()?;
                host.conn.map_window(child)?.check()?;
                for (kind, buttons) in [(PointerKind::Down(1), 1), (PointerKind::Up(1), 0)] {
                    capture.pointer(EditorPointer {
                        modifiers: 0,
                        click_count: if matches!(kind, PointerKind::Down(_) | PointerKind::Up(_)) {
                            1
                        } else {
                            0
                        },
                        position: [40.0, 50.0],
                        kind,
                        buttons,
                    })?;
                }
                host.conn.get_input_focus()?.reply()?;
                let mut transitions = 0;
                while let Some(event) = host.conn.poll_for_event()? {
                    if let Event::ButtonPress(event) | Event::ButtonRelease(event) = event {
                        assert_eq!((event.event, event.event_x, event.event_y), (child, 20, 20));
                        transitions += 1;
                    }
                }
                assert_eq!(
                    transitions, 2,
                    "synthetic graph input must reach the native child"
                );
                host.resize(320, 180);
                let resized = paint_and_capture(&mut capture, 320, 180, 80, (256, 144))?;
                assert_eq!(
                    (resized.width, resized.height, resized.native_size),
                    (320, 180, (320, 180))
                );
                assert_eq!(pixel(&resized, 4, 176), [0, 80, 255, 255]);
                host.resize(u32::MAX, 180);
                let bounded = host.conn.get_geometry(host.id)?.reply()?;
                assert_eq!(bounded.width, 8192);
                assert!(i32::from(bounded.x) + i32::from(bounded.width) < 0);
                assert_eq!(observer.get_input_focus()?.reply()?.focus, focus);
                let still = observer.query_pointer(root)?.reply()?;
                assert_eq!(
                    (still.root_x, still.root_y),
                    (pointer.root_x, pointer.root_y)
                );
                if let Some(background) = &background {
                    assert_eq!(
                        observer
                            .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 32, 32, u32::MAX)?
                            .reply()?
                            .data,
                        *background
                    );
                }
                host.conn.free_gc(gc)?.check()?;
                let id = host.id;
                drop(capture);
                assert_eq!(
                    observer.get_window_attributes(id)?.reply()?.map_state,
                    MapState::VIEWABLE
                );
                if simulated_compositor {
                    observer
                        .composite_unredirect_window(id, Redirect::MANUAL)?
                        .check()?;
                }
                drop(host);
                // Destruction is flushed on the owner's connection; the observer
                // may briefly run first on the server's other client queue.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while observer.get_geometry(id)?.reply().is_ok() {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "capture source was not destroyed"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert!(observer.get_geometry(child)?.reply().is_err());
            }
        }
        Ok(())
    }
}
