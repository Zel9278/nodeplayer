//! The X11 window a plugin editor draws into (XWayland on Wayland sessions), shared by the
//! VST3 and CLAP hosts.
//!
//! An editor is either a top-level window of its own, or embedded as a child of the app's
//! main window. Embedded editors live in a clipping "container" window so they can be
//! scrolled and partially covered by the host's layout.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{Placement, Spot, WinEvent, WindowId};

pub struct HostWindow {
    pub conn: RustConnection,
    /// The window handed to the plugin.
    pub id: u32,
    /// Clipping parent of `id` when embedded.
    pub container: Option<u32>,
    pub placement: Placement,
    size: Cell<(u32, u32)>,
    /// Content scale the plugin currently draws at, and its size at scale 1.
    pub scale: Cell<f32>,
    pub base: Cell<(f32, f32)>,
    wm_delete: u32,
    /// Where the embedded editor was last put; None = hidden.
    shown: RefCell<Option<Spot>>,
    /// Keep the plugin's windows told where they are (needed for Wine/yabridge editors).
    announce: bool,
    announced: Cell<Option<((i16, i16), Instant)>>,
}

impl HostWindow {
    pub fn create(placement: Placement, w: u32, h: u32, title: &str, announce: bool) -> Result<Self> {
        let (w, h) = (w.clamp(16, 8192) as u16, h.clamp(16, 8192) as u16);
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| anyhow!("X11 connect failed (is XWayland running?): {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth, visual, colormap, black) =
            (screen.root, screen.root_depth, screen.root_visual, screen.default_colormap, screen.black_pixel);
        // The host window may use a 32-bit visual, so always spell out ours.
        let aux = CreateWindowAux::new().background_pixel(black).border_pixel(black).colormap(colormap);

        let mut wm_delete = 0;
        let (id, container) = match placement {
            Placement::Window => {
                let id = conn.generate_id()?;
                conn.create_window(
                    depth,
                    id,
                    root,
                    0,
                    0,
                    w,
                    h,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    visual,
                    &aux.event_mask(EventMask::STRUCTURE_NOTIFY),
                )?;
                conn.change_property8(PropMode::REPLACE, id, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?;
                let net_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
                let utf8 = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
                conn.change_property8(PropMode::REPLACE, id, net_name, utf8, title.as_bytes())?;
                let protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
                wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
                conn.change_property32(PropMode::REPLACE, id, protocols, AtomEnum::ATOM, &[wm_delete])?;
                (id, None)
            }
            Placement::Embedded(parent) => {
                let container = conn.generate_id()?;
                conn.create_window(depth, container, parent as u32, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)?;
                let id = conn.generate_id()?;
                conn.create_window(depth, id, container, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)?;
                (id, Some(container))
            }
        };
        // Embedded containers stay unmapped until the host places them.
        conn.map_window(id)?;
        conn.flush()?;
        Ok(Self {
            conn,
            id,
            container,
            placement,
            size: Cell::new((w as u32, h as u32)),
            scale: Cell::new(1.0),
            base: Cell::new((w as f32, h as f32)),
            wm_delete,
            shown: RefCell::new(None),
            announce,
            announced: Cell::new(None),
        })
    }

    /// The window handed to the plugin.
    pub fn handle(&self) -> WindowId {
        self.id as WindowId
    }

    /// Size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size.get()
    }

    pub fn resize(&self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        self.size.set((w, h));
        let _ = self.conn.configure_window(self.id, &ConfigureWindowAux::new().width(w).height(h));
        let _ = self.conn.flush();
    }

    /// Lets the window manager resize a top-level editor, or pins it to `w`×`h`.
    pub fn set_fixed_size(&self, w: u32, h: u32, fixed: bool) {
        if self.container.is_some() {
            return;
        }
        // WM_NORMAL_HINTS: flags, pad*4, min w/h, max w/h, ...
        let mut hints = [0u32; 18];
        if fixed {
            hints[0] = (1 << 4) | (1 << 5); // PMinSize | PMaxSize
            hints[5] = w;
            hints[6] = h;
            hints[7] = w;
            hints[8] = h;
        }
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            self.id,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        );
    }

    /// Moves an embedded editor to `spot`, cut to its visible parts, or hides it (`None`).
    pub fn place(&self, spot: Option<&Spot>) {
        let Some(container) = self.container else { return };
        let conn = &self.conn;
        let next = spot.filter(|s| !s.visible.is_empty());
        let mut shown = self.shown.borrow_mut();
        if next == shown.as_ref() {
            return;
        }
        match next {
            Some(s) => {
                let (c, (ox, oy)) = (s.clip, s.offset());
                if shown.as_ref().is_none_or(|o| (o.clip, o.offset()) != (c, (ox, oy))) {
                    let _ = conn.configure_window(
                        container,
                        &ConfigureWindowAux::new().x(c.x).y(c.y).width(c.w).height(c.h).stack_mode(StackMode::ABOVE),
                    );
                    let _ = conn.configure_window(self.id, &ConfigureWindowAux::new().x(ox).y(oy));
                }
                if shown.as_ref().is_none_or(|o| o.local().ne(s.local())) {
                    self.shape(container, s);
                }
                if shown.is_none() {
                    let _ = conn.map_window(container);
                }
            }
            None => {
                let _ = conn.unmap_window(container);
            }
        }
        let _ = conn.flush();
        *shown = next.cloned();
    }

    /// Cuts the container to the parts of it nothing covers, for drawing (which clips the
    /// plugin's windows inside too) and for the pointer, which reaches the app's window
    /// through the holes.
    fn shape(&self, container: u32, spot: &Spot) {
        for kind in [shape::SK::BOUNDING, shape::SK::INPUT] {
            let _ = if spot.whole() {
                self.conn.shape_mask(shape::SO::SET, kind, container, 0, 0, x11rb::NONE)
            } else {
                let rects: Vec<Rectangle> = spot
                    .local()
                    .map(|r| Rectangle { x: r.x as i16, y: r.y as i16, width: r.w as u16, height: r.h as u16 })
                    .collect();
                self.conn.shape_rectangles(shape::SO::SET, kind, ClipOrdering::UNSORTED, container, 0, 0, &rects)
            };
        }
    }

    /// Tells every window the plugin put inside ours where it really is on screen, the way a
    /// window manager informs reparented clients (ICCCM synthetic ConfigureNotify). Wine 10+
    /// otherwise assumes embedded windows sit at the screen origin and misplaces mouse input.
    fn announce_position(&self, origin: (i16, i16)) {
        let conn = &self.conn;
        let mut stack = vec![(self.id, origin.0, origin.1)];
        while let Some((parent, px, py)) = stack.pop() {
            let Ok(Ok(tree)) = conn.query_tree(parent).map(|c| c.reply()) else { continue };
            for child in tree.children {
                let Ok(Ok(g)) = conn.get_geometry(child).map(|c| c.reply()) else { continue };
                let (x, y) = (px + g.x, py + g.y);
                let ev = ConfigureNotifyEvent {
                    response_type: CONFIGURE_NOTIFY_EVENT,
                    sequence: 0,
                    event: child,
                    window: child,
                    above_sibling: x11rb::NONE,
                    x,
                    y,
                    width: g.width,
                    height: g.height,
                    border_width: 0,
                    override_redirect: false,
                };
                let _ = conn.send_event(false, child, EventMask::STRUCTURE_NOTIFY, ev);
                stack.push((child, x, y));
            }
        }
        let _ = conn.flush();
    }

    /// Re-announces when our window moved on screen, and once a second regardless since the
    /// plugin may have created or resized windows since.
    fn maybe_announce(&self) {
        let conn = &self.conn;
        let Ok(Ok(origin)) = conn.translate_coordinates(self.id, conn.setup().roots[0].root, 0, 0).map(|c| c.reply())
        else {
            return;
        };
        let origin = (origin.dst_x, origin.dst_y);
        let stale = self.announced.get().is_none_or(|(o, t)| o != origin || t.elapsed() > Duration::from_secs(1));
        if stale {
            self.announce_position(origin);
            self.announced.set(Some((origin, Instant::now())));
        }
    }

    /// Handles our window's events; call regularly from the UI thread.
    pub fn poll(&self) -> Vec<WinEvent> {
        if self.announce {
            self.maybe_announce();
        }
        let mut out = Vec::new();
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                Event::ClientMessage(e) if e.data.as_data32()[0] == self.wm_delete => out.push(WinEvent::Close),
                Event::ConfigureNotify(e) if e.window == self.id && self.container.is_none() => {
                    let size = (e.width as u32, e.height as u32);
                    if size != self.size.get() {
                        self.size.set(size);
                        out.push(WinEvent::Resized(size.0, size.1));
                    }
                }
                _ => {}
            }
        }
        out
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.container.unwrap_or(self.id));
        let _ = self.conn.flush();
    }
}
