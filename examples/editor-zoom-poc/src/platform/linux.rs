//! X11/XWayland editor capture for the graph-zoom experiment.
//!
//! The source stays mapped as a separate reference window. XComposite redirects its
//! complete child hierarchy to a pixmap, so covering that window does not erase the
//! image used by the graph. This intentionally uses CPU readback before texture upload.

use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{CapturedFrame, PointerEvent, PointerKind};

struct Fixture {
    clicks: u64,
    slider: f64,
    dragging: bool,
    dirty: bool,
    started: Instant,
    pulse: u8,
}

pub struct NativeEditor {
    conn: RustConnection,
    root: Window,
    window: Window,
    pixmap: Option<Pixmap>,
    size: (u32, u32),
    masks: [u32; 3],
    gc: Gcontext,
    font: Font,
    wm_delete: Atom,
    fixture: Option<Fixture>,
    closed: bool,
    pointer_target: Option<Window>,
}

impl NativeEditor {
    pub fn new(width: u32, height: u32, fixture: bool) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "editor dimensions must be 1..8192"
        );
        let (conn, screen_num) = x11rb::connect(None)
            .context("connect to X11 (set DISPLAY; XWayland or Xvfb is supported)")?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth, visual) = (screen.root, screen.root_depth, screen.root_visual);
        let visual_info = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == visual)
            .context("X11 root visual not found")?;
        ensure!(
            visual_info.class == VisualClass::TRUE_COLOR,
            "the PoC needs a TrueColor X11 visual"
        );
        let masks = [
            visual_info.red_mask,
            visual_info.green_mask,
            visual_info.blue_mask,
        ];
        let version = conn
            .composite_query_version(0, 4)
            .context(
                "XComposite is unavailable; window captures must not depend on screen visibility",
            )?
            .reply()
            .context("query XComposite version")?;
        ensure!(
            version.major_version > 0 || version.minor_version >= 2,
            "XComposite 0.2 or newer is required"
        );

        let window = conn.generate_id()?;
        conn.create_window(
            depth,
            window,
            root,
            80,
            80,
            width as u16,
            height as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().background_pixel(0).event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            ),
        )?
        .check()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            if fixture {
                b"nodeplayer zoom source - X11 fixture"
            } else {
                b"nodeplayer zoom source - CLAP"
            },
        )?
        .check()?;
        let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            wm_protocols,
            AtomEnum::ATOM,
            &[wm_delete],
        )?
        .check()?;

        let font = conn.generate_id()?;
        conn.open_font(font, b"fixed")?
            .check()
            .context("open X11 fixed font")?;
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            window,
            &CreateGCAux::new()
                .font(font)
                .foreground(0xffffff)
                .background(0),
        )?
        .check()?;
        // Automatic mode keeps the native reference visible while retaining complete offscreen
        // storage for our graph image. Unlike manual redirection it can coexist with a WM.
        conn.composite_redirect_window(window, Redirect::AUTOMATIC)?
            .check()
            .context("redirect native editor hierarchy")?;
        conn.map_window(window)?.check()?;
        conn.flush()?;
        let mut this = Self {
            conn,
            root,
            window,
            pixmap: None,
            size: (width, height),
            masks,
            gc,
            font,
            wm_delete,
            fixture: fixture.then(|| Fixture {
                clicks: 0,
                slider: 0.4,
                dragging: false,
                dirty: true,
                started: Instant::now(),
                pulse: 0,
            }),
            closed: false,
            pointer_target: None,
        };
        this.rename_pixmap()?;
        this.pump()?;
        Ok(this)
    }

    pub fn handle(&self) -> usize {
        self.window as usize
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "editor dimensions must be 1..8192"
        );
        if self.size != (width, height) {
            self.conn
                .configure_window(
                    self.window,
                    &ConfigureWindowAux::new().width(width).height(height),
                )?
                .check()?;
            self.size = (width, height);
            self.rename_pixmap()?;
            if let Some(fixture) = &mut self.fixture {
                fixture.dirty = true;
            }
            self.conn.flush()?;
        }
        Ok(())
    }

    /// Reacquire after every map/resize: XComposite allocates a new backing pixmap.
    fn rename_pixmap(&mut self) -> Result<()> {
        if let Some(old) = self.pixmap.take() {
            self.conn.free_pixmap(old)?.check()?;
        }
        let pixmap = self.conn.generate_id()?;
        self.conn
            .composite_name_window_pixmap(self.window, pixmap)?
            .check()
            .context("name redirected window pixmap (the source must remain mapped)")?;
        self.pixmap = Some(pixmap);
        Ok(())
    }

    pub fn pump(&mut self) -> Result<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::Error(error) => bail!("X11 request failed: {error:?}"),
                Event::ClientMessage(e)
                    if e.window == self.window && e.data.as_data32()[0] == self.wm_delete =>
                {
                    self.closed = true
                }
                Event::DestroyNotify(e) if e.window == self.window => self.closed = true,
                Event::Expose(e) if e.window == self.window => {
                    if let Some(fixture) = &mut self.fixture {
                        fixture.dirty = true;
                    }
                }
                Event::ConfigureNotify(e) if e.window == self.window => {
                    let next = (e.width as u32, e.height as u32);
                    if next != self.size && next.0 > 0 && next.1 > 0 {
                        self.size = next;
                        self.rename_pixmap()?;
                        if let Some(fixture) = &mut self.fixture {
                            fixture.dirty = true;
                        }
                    }
                }
                Event::ButtonPress(e) if e.event == self.window && e.detail == 1 => {
                    if let Some(fixture) = &mut self.fixture {
                        let (x, y) = (e.event_x as f64, e.event_y as f64);
                        if (40.0..200.0).contains(&x) && (60.0..130.0).contains(&y) {
                            fixture.clicks += 1;
                            fixture.dirty = true;
                        }
                        if (40.0..self.size.0.saturating_sub(40) as f64).contains(&x)
                            && (200.0..260.0).contains(&y)
                        {
                            fixture.dragging = true;
                            fixture.slider = ((x - 40.0)
                                / self.size.0.saturating_sub(80).max(1) as f64)
                                .clamp(0.0, 1.0);
                            fixture.dirty = true;
                        }
                    }
                }
                Event::ButtonRelease(e) if e.event == self.window && e.detail == 1 => {
                    if let Some(fixture) = &mut self.fixture {
                        fixture.dragging = false;
                    }
                }
                Event::MotionNotify(e) if e.event == self.window => {
                    if let Some(fixture) = &mut self.fixture
                        && fixture.dragging
                    {
                        fixture.slider = ((e.event_x as f64 - 40.0)
                            / self.size.0.saturating_sub(80).max(1) as f64)
                            .clamp(0.0, 1.0);
                        fixture.dirty = true;
                    }
                }
                _ => {}
            }
        }
        ensure!(!self.closed, "the native reference window was closed");
        if let Some(fixture) = &mut self.fixture {
            let pulse = ((fixture.started.elapsed().as_millis() / 80) % 24) as u8;
            if fixture.pulse != pulse {
                fixture.pulse = pulse;
                fixture.dirty = true;
            }
        }
        if self.fixture.as_ref().is_some_and(|f| f.dirty) {
            self.paint_fixture()?;
            if let Some(fixture) = &mut self.fixture {
                fixture.dirty = false;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    pub fn capture(&mut self) -> Result<CapturedFrame> {
        self.pump()?;
        let geometry = self.conn.get_geometry(self.window)?.reply()?;
        let next = (geometry.width as u32, geometry.height as u32);
        ensure!(
            next.0 > 0 && next.1 > 0,
            "native source has no drawable area"
        );
        if next != self.size {
            self.size = next;
            self.rename_pixmap()?;
        }
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.pixmap.context("no XComposite pixmap")?,
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
            .find(|f| f.depth == reply.depth)
            .context("no X11 pixel format for capture depth")?;
        ensure!(
            matches!(format.bits_per_pixel, 8 | 16 | 24 | 32),
            "unsupported X11 pixel size {}",
            format.bits_per_pixel
        );
        ensure!(
            format.scanline_pad > 0 && format.scanline_pad % 8 == 0,
            "invalid X11 scanline padding"
        );
        let width = geometry.width as usize;
        let height = geometry.height as usize;
        let bpp = format.bits_per_pixel as usize;
        let pad = format.scanline_pad as usize;
        let stride = (width * bpp).div_ceil(pad) * (pad / 8);
        ensure!(
            reply.data.len() >= stride * height,
            "short X11 image: {} < {} bytes",
            reply.data.len(),
            stride * height
        );
        let little = self.conn.setup().image_byte_order == ImageOrder::LSB_FIRST;
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let at = y * stride + x * (bpp / 8);
                let mut pixel = 0u32;
                if little {
                    for byte in 0..bpp / 8 {
                        pixel |= u32::from(reply.data[at + byte]) << (byte * 8);
                    }
                } else {
                    for byte in 0..bpp / 8 {
                        pixel = (pixel << 8) | u32::from(reply.data[at + byte]);
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
        let frame = CapturedFrame {
            width: width as u32,
            height: height as u32,
            rgba,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// XSendEvent keeps the user's physical pointer in the graph. Some toolkits or Wine
    /// plugins reject synthetic events or grab the real pointer; that remains a PoC limit.
    pub fn pointer(&mut self, event: PointerEvent) -> Result<()> {
        ensure!(
            event.position.iter().all(|v| v.is_finite()),
            "non-finite editor pointer position"
        );
        let x = event.position[0]
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let y = event.position[1]
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let root_position = self
            .conn
            .translate_coordinates(self.window, self.root, x, y)?
            .reply()?;
        let mut target = self.pointer_target.unwrap_or(self.window);
        let mut local = (x, y);
        for _ in 0..64 {
            let at = self
                .conn
                .translate_coordinates(self.root, target, root_position.dst_x, root_position.dst_y)?
                .reply()?;
            local = (at.dst_x, at.dst_y);
            if self.pointer_target.is_some() || at.child == x11rb::NONE {
                break;
            }
            target = at.child;
        }
        let state = button_mask(event.buttons);
        match event.kind {
            PointerKind::Move => {
                let motion = MotionNotifyEvent {
                    response_type: MOTION_NOTIFY_EVENT,
                    detail: Motion::NORMAL,
                    sequence: 0,
                    time: x11rb::CURRENT_TIME,
                    root: self.root,
                    event: target,
                    child: x11rb::NONE,
                    root_x: root_position.dst_x,
                    root_y: root_position.dst_y,
                    event_x: local.0,
                    event_y: local.1,
                    state,
                    same_screen: true,
                };
                self.conn
                    .send_event(false, target, EventMask::POINTER_MOTION, motion)?
                    .check()?;
            }
            PointerKind::Down(button) | PointerKind::Up(button) => {
                let pressed = matches!(event.kind, PointerKind::Down(_));
                let detail = x_button(button)?;
                let bit = button_bit(button);
                let state = if pressed {
                    KeyButMask::from(u16::from(state) & !u16::from(bit))
                } else {
                    state | bit
                };
                self.send_button(
                    target,
                    local,
                    (root_position.dst_x, root_position.dst_y),
                    detail,
                    pressed,
                    state,
                )?;
                if pressed {
                    self.pointer_target = Some(target);
                } else if event.buttons == 0 {
                    self.pointer_target = None;
                }
            }
            PointerKind::Wheel { dx, dy } => {
                for (delta, negative, positive) in [(dy, 5, 4), (dx, 7, 6)] {
                    if !delta.is_finite() || delta.abs() < f32::EPSILON {
                        continue;
                    }
                    let detail = if delta > 0.0 { positive } else { negative };
                    let steps = (delta.abs() / 40.0).ceil().clamp(1.0, 12.0) as usize;
                    for _ in 0..steps {
                        self.send_button(
                            target,
                            local,
                            (root_position.dst_x, root_position.dst_y),
                            detail,
                            true,
                            state,
                        )?;
                        self.send_button(
                            target,
                            local,
                            (root_position.dst_x, root_position.dst_y),
                            detail,
                            false,
                            state,
                        )?;
                    }
                }
            }
        }
        self.conn.flush()?;
        Ok(())
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
        let button = ButtonPressEvent {
            response_type: if pressed {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            },
            detail,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            root: self.root,
            event: target,
            child: x11rb::NONE,
            root_x: root.0,
            root_y: root.1,
            event_x: local.0,
            event_y: local.1,
            state,
            same_screen: true,
        };
        self.conn
            .send_event(
                false,
                target,
                if pressed {
                    EventMask::BUTTON_PRESS
                } else {
                    EventMask::BUTTON_RELEASE
                },
                button,
            )?
            .check()?;
        Ok(())
    }

    pub fn fixture_value(&self) -> Option<u64> {
        self.fixture.as_ref().map(|f| f.clicks)
    }

    fn paint_fixture(&self) -> Result<()> {
        let fixture = self.fixture.as_ref().context("no native fixture")?;
        let (width, height) = self.size;
        self.fill([0, 0, width as i16, height as i16], [24, 28, 38])?;
        for x in (0..width).step_by(40) {
            self.fill([x as i16, 0, 1, height as i16], [38, 45, 58])?;
        }
        for y in (0..height).step_by(40) {
            self.fill([0, y as i16, width as i16, 1], [38, 45, 58])?;
        }
        self.text(
            24,
            30,
            "Native X11 source - graph zoom does not resize this window",
        )?;
        self.fill(
            [40, 60, 160, 70],
            if fixture.clicks % 2 == 0 {
                [43, 125, 198]
            } else {
                [167, 84, 186]
            },
        )?;
        self.text(54, 101, &format!("Clicks: {}", fixture.clicks))?;
        self.text(40, 166, "Click the blue button through the zoomed image")?;
        let track = width.saturating_sub(80).max(1);
        self.fill([40, 210, track as i16, 28], [57, 63, 76])?;
        self.fill(
            [
                40,
                210,
                ((track as f64 * fixture.slider).round() as i16).max(1),
                28,
            ],
            [54, 179, 138],
        )?;
        self.text(
            40,
            280,
            &format!(
                "Slider: {:.2}   Source: {} x {} pixels",
                fixture.slider, width, height
            ),
        )?;
        let pulse_width = width.saturating_sub(48).min(24 * 12);
        for i in 0..24 {
            let on = i <= fixture.pulse;
            let x = 24 + i as u32 * pulse_width / 24;
            self.fill(
                [
                    x as i16,
                    height.saturating_sub(34) as i16,
                    (pulse_width / 24).saturating_sub(2).max(1) as i16,
                    12,
                ],
                if on { [238, 175, 64] } else { [69, 58, 40] },
            )?;
        }
        Ok(())
    }

    fn fill(&self, rect: [i16; 4], rgb: [u8; 3]) -> Result<()> {
        if rect[2] <= 0 || rect[3] <= 0 {
            return Ok(());
        }
        let pixel = pack_channel(rgb[0], self.masks[0])
            | pack_channel(rgb[1], self.masks[1])
            | pack_channel(rgb[2], self.masks[2]);
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(pixel))?;
        self.conn.poly_fill_rectangle(
            self.window,
            self.gc,
            &[Rectangle {
                x: rect[0],
                y: rect[1],
                width: rect[2] as u16,
                height: rect[3] as u16,
            }],
        )?;
        Ok(())
    }

    fn text(&self, x: i16, y: i16, text: &str) -> Result<()> {
        let white = pack_channel(237, self.masks[0])
            | pack_channel(241, self.masks[1])
            | pack_channel(247, self.masks[2]);
        let background = pack_channel(24, self.masks[0])
            | pack_channel(28, self.masks[1])
            | pack_channel(38, self.masks[2]);
        self.conn.change_gc(
            self.gc,
            &ChangeGCAux::new().foreground(white).background(background),
        )?;
        self.conn.image_text8(
            self.window,
            self.gc,
            x,
            y,
            &text.as_bytes()[..text.len().min(255)],
        )?;
        Ok(())
    }
}

impl Drop for NativeEditor {
    fn drop(&mut self) {
        if let Some(pixmap) = self.pixmap.take() {
            let _ = self.conn.free_pixmap(pixmap);
        }
        let _ = self.conn.free_gc(self.gc);
        let _ = self.conn.close_font(self.font);
        let _ = self
            .conn
            .composite_unredirect_window(self.window, Redirect::AUTOMATIC);
        if !self.closed {
            let _ = self.conn.destroy_window(self.window);
        }
        let _ = self.conn.flush();
    }
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

fn pack_channel(value: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = u64::from(mask >> shift);
    ((((u64::from(value) * max + 127) / 255) as u32) << shift) & mask
}

fn x_button(button: u8) -> Result<u8> {
    match button {
        1 => Ok(1),
        2 => Ok(3),
        3 => Ok(2),
        _ => bail!("unsupported pointer button {button}"),
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
    let mut state = KeyButMask::default();
    for button in 1..=3 {
        if buttons & (1 << (button - 1)) != 0 {
            state |= button_bit(button);
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_masks_round_trip_rgb565_and_rgb888() {
        for masks in [[0xff0000, 0xff00, 0xff], [0xf800, 0x7e0, 0x1f]] {
            for value in [0u8, 41, 128, 219, 255] {
                for mask in masks {
                    let actual = unpack_channel(pack_channel(value, mask), mask);
                    assert!(
                        value.abs_diff(actual) <= 4,
                        "{value} -> {actual} with {mask:x}"
                    );
                }
            }
        }
    }

    /// Run under Xvfb or an X11 desktop with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires an X11 display with XComposite"]
    fn capture_retains_obscured_content_and_survives_resize() -> Result<()> {
        let mut editor = NativeEditor::new(640, 360, true)?;
        let first = editor.capture()?;
        let pixel = |image: &CapturedFrame, x: usize, y: usize| {
            let at = (y * image.width as usize + x) * 4;
            image.rgba[at..at + 4].to_vec()
        };
        let button = pixel(&first, 45, 65);
        ensure!(
            button == [43, 125, 198, 255],
            "unexpected native button pixel {button:?}"
        );
        let cover = editor.conn.generate_id()?;
        editor
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                cover,
                editor.root,
                0,
                0,
                1600,
                1000,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().background_pixel(0xff0000),
            )?
            .check()?;
        editor.conn.map_window(cover)?.check()?;
        let obscured = editor.capture()?;
        ensure!(
            pixel(&obscured, 45, 65) == button,
            "capture changed when its reference window was covered"
        );
        editor.resize(720, 420)?;
        let resized = editor.capture()?;
        ensure!(
            (resized.width, resized.height) == (720, 420),
            "capture retained the old XComposite pixmap after resize"
        );
        ensure!(
            pixel(&resized, 45, 65) == button,
            "resized capture lost the button pixels"
        );
        editor.conn.destroy_window(cover)?.check()?;
        Ok(())
    }

    #[test]
    #[ignore = "requires an X11 display with XComposite"]
    fn pointer_routes_to_native_descendant_coordinates() -> Result<()> {
        let mut editor = NativeEditor::new(640, 360, false)?;
        let child = editor.conn.generate_id()?;
        editor
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                child,
                editor.window,
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
                            | EventMask::POINTER_MOTION
                            | EventMask::BUTTON_RELEASE,
                    ),
            )?
            .check()?;
        editor.conn.map_window(child)?.check()?;
        editor.pump()?;
        let image = editor.capture()?;
        let at = (90 * image.width as usize + 80) * 4;
        ensure!(
            image.rgba[at..at + 4] == [0, 204, 136, 255],
            "capture omitted the plugin child window"
        );
        editor.pointer(PointerEvent {
            position: [80.0, 90.0],
            kind: PointerKind::Down(1),
            buttons: 1,
        })?;
        // A drag stays with its original plugin child even after leaving its rectangle.
        editor.pointer(PointerEvent {
            position: [300.0, 90.0],
            kind: PointerKind::Move,
            buttons: 1,
        })?;
        editor.pointer(PointerEvent {
            position: [300.0, 90.0],
            kind: PointerKind::Up(1),
            buttons: 0,
        })?;
        let mut found = false;
        let mut dragged = false;
        let mut released = false;
        while let Some(event) = editor.conn.poll_for_event()? {
            match event {
                Event::ButtonPress(event) => {
                    ensure!(
                        event.event == child,
                        "pointer reached the parent instead of its plugin child"
                    );
                    ensure!(
                        (event.event_x, event.event_y) == (60, 60),
                        "wrong child-local pointer coordinates"
                    );
                    found = true;
                }
                Event::MotionNotify(event) => {
                    ensure!(
                        event.event == child && (event.event_x, event.event_y) == (280, 60),
                        "drag left its original child"
                    );
                    dragged = true;
                }
                Event::ButtonRelease(event) => {
                    ensure!(
                        event.event == child && (event.event_x, event.event_y) == (280, 60),
                        "release left its original child"
                    );
                    released = true;
                }
                _ => {}
            }
        }
        ensure!(found, "synthetic pointer press did not arrive");
        ensure!(dragged && released, "synthetic drag/release did not arrive");
        ensure!(
            editor.pointer_target.is_none(),
            "pointer capture was not released"
        );
        Ok(())
    }
}
