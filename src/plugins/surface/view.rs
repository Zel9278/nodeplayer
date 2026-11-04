//! The canvas owns editor placement, zoom and input ownership; native sources stay fixed.

use anyhow::Result;
use egui::{Pos2, Rect};
use std::time::Instant;

use super::{EditorFrame, EditorPointer, PointerKind};
use crate::plugins::{Editor, Spot};

#[derive(Default)]
pub struct EditorView {
    texture: Option<egui::TextureHandle>,
    pixels: Option<ImagePixels>,
    native_size: Option<(u32, u32)>,
    native_units_per_point: Option<f32>,
    screen: Option<Rect>,
    buttons: u8,
    modifiers: egui::Modifiers,
    last_position: Option<Pos2>,
    frames: u64,
    first_frame_at: Option<Instant>,
    last_frame_at: Option<Instant>,
    changed_frames: u64,
    last_frame_hash: Option<ring::digest::Digest>,
    popups: Vec<PopupTexture>,
    popup_rects: Vec<Rect>,
    popup_layer: Option<egui::LayerId>,
    popup_anchor: Option<[f64; 2]>,
    popup_offset: Option<egui::Vec2>,
    cancel_popup: bool,
    drag_popup: Option<PopupMapping>,
    clicks: ClickTracker,
}

#[derive(Clone, Copy)]
struct ClickLimits {
    interval: f64,
    distance: egui::Vec2,
}

impl ClickLimits {
    fn current(context: &egui::Context) -> Self {
        #[cfg(target_os = "macos")]
        let interval = objc2_app_kit::NSEvent::doubleClickInterval();
        #[cfg(target_os = "windows")]
        let interval = unsafe {
            f64::from(windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime()) / 1000.0
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let interval = context.options(|options| options.input_options.max_double_click_delay);
        #[cfg(not(target_os = "windows"))]
        let distance =
            egui::Vec2::splat(context.options(|options| options.input_options.max_click_dist));
        #[cfg(target_os = "windows")]
        let distance = unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
            };
            egui::vec2(
                GetSystemMetrics(SM_CXDOUBLECLK) as f32,
                GetSystemMetrics(SM_CYDOUBLECLK) as f32,
            ) / (2.0 * context.pixels_per_point())
        };
        Self { interval, distance }
    }

    fn contains(self, delta: egui::Vec2) -> bool {
        delta.x.abs() <= self.distance.x && delta.y.abs() <= self.distance.y
    }
}

#[derive(Clone, Copy)]
struct ButtonClick {
    position: Pos2,
    time: f64,
    popup: Option<usize>,
    count: u8,
    released: bool,
    dragged: bool,
}

#[derive(Default)]
struct ClickTracker {
    buttons: [Option<ButtonClick>; 3],
    last_button: Option<u8>,
}

impl ClickTracker {
    fn press(
        &mut self,
        button: u8,
        popup: Option<usize>,
        position: Pos2,
        time: f64,
        limits: ClickLimits,
    ) -> u8 {
        let previous = self.buttons[usize::from(button - 1)];
        let count = previous
            .filter(|click| {
                self.last_button == Some(button)
                    && click.released
                    && click.popup == popup
                    && time >= click.time
                    && time - click.time <= limits.interval
                    && limits.contains(position - click.position)
            })
            .map_or(1, |click| click.count.saturating_add(1));
        self.buttons[usize::from(button - 1)] = Some(ButtonClick {
            position,
            time,
            popup,
            count,
            released: false,
            dragged: false,
        });
        self.last_button = Some(button);
        count
    }

    fn moved(&mut self, position: Pos2, limits: ClickLimits) {
        for click in self
            .buttons
            .iter_mut()
            .flatten()
            .filter(|click| !click.released)
        {
            click.dragged |= !limits.contains(position - click.position);
        }
    }

    fn release(&mut self, button: u8, position: Pos2, time: f64, limits: ClickLimits) -> u8 {
        let Some(click) = &mut self.buttons[usize::from(button - 1)] else {
            return 0;
        };
        click.released = !click.dragged
            && time >= click.time
            && time - click.time <= limits.interval
            && limits.contains(position - click.position);
        if click.released { click.count } else { 0 }
    }
}

struct PopupTexture {
    texture: egui::TextureHandle,
    pixels: ImagePixels,
    origin: [f64; 2],
    native_size: (u32, u32),
    interactive: bool,
}

/// Keep the last uploaded image so repeated native captures do not cause another
/// CPU color conversion and GPU upload. Native capture already bounds the bytes.
#[derive(PartialEq, Eq)]
struct ImagePixels {
    size: [usize; 2],
    rgba: Vec<u8>,
}

impl ImagePixels {
    fn image(&self) -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied(self.size, &self.rgba)
    }
}

#[derive(Clone, Copy)]
struct PopupMapping {
    screen: Rect,
    origin: [f64; 2],
    native_size: (u32, u32),
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct EditorFrameStats {
    pub frames: u64,
    /// Age since the newest image reached texture staging, not display presentation.
    pub frame_age_seconds: f64,
    elapsed_seconds: f64,
    frames_per_second: f64,
    changed_frames: Option<u64>,
}

impl EditorView {
    pub fn dimensions(&self) -> Option<((u32, u32), [usize; 2])> {
        Some((self.native_size?, self.texture.as_ref()?.size()))
    }

    pub fn frame_stats(&self) -> Option<EditorFrameStats> {
        let elapsed_seconds = self.first_frame_at?.elapsed().as_secs_f64();
        Some(EditorFrameStats {
            frames: self.frames,
            frame_age_seconds: self.last_frame_at?.elapsed().as_secs_f64(),
            elapsed_seconds,
            frames_per_second: self.frames.saturating_sub(1) as f64
                / elapsed_seconds.max(f64::EPSILON),
            changed_frames: self.last_frame_hash.as_ref().map(|_| self.changed_frames),
        })
    }

    pub fn paint(
        &mut self,
        ui: &mut egui::Ui,
        editor: &mut dyn Editor,
        display_scale: f32,
    ) -> Rect {
        editor.set_display_scale(display_scale);
        if let Some(frame) = editor.take_frame().filter(|frame| frame.validate().is_ok()) {
            self.update_texture(ui.ctx(), frame);
        }
        let size = self.native_size.unwrap_or_else(|| editor.size());
        // Density belongs to the delivered image. A pending monitor-DPI request must
        // not resize an older texture while the replacement image is still in flight.
        let units = self.native_units_per_point.unwrap_or_else(|| editor.native_units_per_point());
        let desired = egui::vec2(size.0 as f32 / units, size.1 as f32 / units);
        let rect = if let Some(texture) = &self.texture {
            ui.add(
                egui::Image::new((texture.id(), desired))
                    .fit_to_exact_size(desired)
                    .sense(egui::Sense::click_and_drag()),
            )
            .rect
        } else {
            let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_gray(16));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Opening editor…",
                egui::FontId::proportional(14.0),
                egui::Color32::GRAY,
            );
            rect
        };
        let transform = ui
            .ctx()
            .layer_transform_to_global(ui.layer_id())
            .unwrap_or_default();
        self.screen = Some(transform * rect);
        self.paint_popups(ui.ctx(), ui.id().with("captured-editor-popup"));
        if let Some(error) = editor.capture_error() {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        rect
    }

    fn update_texture(&mut self, context: &egui::Context, frame: EditorFrame) {
        self.frames = self.frames.saturating_add(1);
        self.last_frame_at = Some(Instant::now());
        self.first_frame_at.get_or_insert_with(Instant::now);
        // Diagnostics must distinguish repeated captures from actual plugin redraws.
        // Keep hashing off the ordinary editor path.
        if crate::diagnostics::active().is_some() {
            let hash = ring::digest::digest(&ring::digest::SHA256, &frame.rgba);
            if self
                .last_frame_hash
                .as_ref()
                .is_none_or(|previous| previous.as_ref() != hash.as_ref())
            {
                self.changed_frames = self.changed_frames.saturating_add(1);
                self.last_frame_hash = Some(hash);
            }
        }
        if self.texture.is_none()
            && let Some(run) = crate::diagnostics::active()
        {
            let _ = run.event("editor-frame", serde_json::json!({"native_size":frame.native_size,"capture_size":[frame.width,frame.height]}));
        }
        self.native_size = Some(frame.native_size);
        self.native_units_per_point = Some(frame.native_units_per_point);
        if frame.popups.iter().any(|popup| popup.interactive)
            && !self.popups.iter().any(|popup| popup.interactive)
        {
            self.popup_offset = None;
        }
        self.popups.truncate(frame.popups.len());
        if frame.popups.is_empty() {
            self.cancel_popup = false;
            self.popup_offset = None;
        }
        for (index, popup) in frame.popups.into_iter().enumerate() {
            let pixels = ImagePixels {
                size: [popup.width as usize, popup.height as usize],
                rgba: popup.rgba,
            };
            if let Some(previous) = self.popups.get_mut(index) {
                if previous.pixels != pixels {
                    previous.texture.set(pixels.image(), egui::TextureOptions::LINEAR);
                    previous.pixels = pixels;
                }
                previous.origin = popup.origin;
                previous.native_size = popup.native_size;
                previous.interactive = popup.interactive;
            } else {
                self.popups.push(PopupTexture {
                    texture: context.load_texture(
                        "plugin-popup",
                        pixels.image(),
                        egui::TextureOptions::LINEAR,
                    ),
                    pixels,
                    origin: popup.origin,
                    native_size: popup.native_size,
                    interactive: popup.interactive,
                });
            }
        }
        let pixels = ImagePixels {
            size: [frame.width as usize, frame.height as usize],
            rgba: frame.rgba,
        };
        if self.texture.is_none() || self.pixels.as_ref() != Some(&pixels) {
            let image = pixels.image();
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(context.load_texture(
                    "plugin-editor", image, egui::TextureOptions::LINEAR,
                ));
            }
            self.pixels = Some(pixels);
        }
    }

    fn paint_popups(&mut self, context: &egui::Context, id: egui::Id) {
        self.popup_rects.clear();
        self.popup_layer = None;
        if self.popups.is_empty() || self.cancel_popup {
            return;
        }
        let Some(screen) = self.screen else { return };
        let Some(size) = self.native_size else { return };
        let scale = screen.size() / egui::vec2(size.0 as f32, size.1 as f32);
        for popup in &self.popups {
            self.popup_rects.push(Rect::from_min_size(
                screen.min + egui::vec2(popup.origin[0] as f32, popup.origin[1] as f32) * scale,
                egui::vec2(popup.native_size.0 as f32, popup.native_size.1 as f32) * scale,
            ));
        }
        let mut bounds = self
            .popup_rects
            .iter()
            .copied()
            .reduce(Rect::union)
            .unwrap();
        if self.popup_offset.is_none() {
            // Some plugins clamp a menu to their physical monitor even though
            // their private editor is offscreen. Present that menu by the click
            // that opened it, while retaining its original native input origin.
            self.popup_offset = Some(
                if !bounds.intersects(screen)
                    && self.popups.iter().any(|popup| popup.interactive)
                    && let Some(anchor) = self.popup_anchor
                {
                    egui::vec2(anchor[0] as f32, anchor[1] as f32)
                        - (bounds.min - screen.min) / scale
                } else {
                    egui::Vec2::ZERO
                },
            );
        }
        let offset = self.popup_offset.unwrap_or_default() * scale;
        bounds = bounds.translate(offset);
        for rect in &mut self.popup_rects {
            *rect = rect.translate(offset);
        }
        // The source's virtual desktop differs from the graph viewport. Keep the
        // menu on screen without changing its native coordinates or input mapping.
        let viewport = context.content_rect();
        let min = egui::pos2(
            bounds
                .min
                .x
                .max(viewport.min.x)
                .min((viewport.max.x - bounds.width()).max(viewport.min.x)),
            bounds
                .min
                .y
                .max(viewport.min.y)
                .min((viewport.max.y - bounds.height()).max(viewport.min.y)),
        );
        let shift = min - bounds.min;
        bounds = bounds.translate(shift);
        for rect in &mut self.popup_rects {
            *rect = rect.translate(shift);
        }
        let interactive = self.popups.iter().any(|popup| popup.interactive);
        if !interactive {
            // Tooltips and selection fades ignore native mouse input. Paint without
            // an Area so they also stay out of the graph's editor occlusion regions.
            let painter = context
                .layer_painter(egui::LayerId::new(egui::Order::Tooltip, id))
                .with_clip_rect(viewport);
            for (popup, rect) in self.popups.iter().zip(&self.popup_rects) {
                painter.image(
                    popup.texture.id(),
                    *rect,
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            return;
        }
        let area = egui::Area::new(id)
            .order(egui::Order::Foreground)
            .fixed_pos(bounds.min)
            .movable(false)
            .constrain(false)
            .interactable(true);
        let paint = |ui: &mut egui::Ui| {
            let (actual, _) = ui.allocate_exact_size(bounds.size(), egui::Sense::click_and_drag());
            let offset = actual.min - bounds.min;
            for (popup, rect) in self.popups.iter().zip(&self.popup_rects) {
                ui.painter().image(
                    popup.texture.id(),
                    rect.translate(offset),
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        };
        let response = egui::Modal::new(id)
            .area(area)
            .frame(egui::Frame::NONE)
            .backdrop_color(egui::Color32::TRANSPARENT)
            .show(context, paint);
        self.popup_layer = Some(response.response.layer_id);
        self.cancel_popup |= response.should_close();
    }

    /// Called after every graph node and higher UI layer has established its coverage.
    pub fn dispatch(
        &mut self,
        context: &egui::Context,
        editor: &mut dyn Editor,
        spot: Option<&Spot>,
        native_units_per_point: f32,
    ) -> Result<()> {
        let Some(screen) = self.screen.filter(|rect| rect.is_positive()) else {
            return Ok(());
        };
        let Some(size) = self.native_size.filter(|_| self.texture.is_some()) else {
            return Ok(());
        };
        let focused = context.input(|input| input.focused);
        let top_modal = context.memory(|memory| memory.top_modal_layer());
        let owns_popup = self.popup_layer.is_some() && self.popup_layer == top_modal;
        if !self.popups.is_empty() && (self.cancel_popup || !focused || spot.is_none()) {
            self.dismiss_popups(editor, screen, size)?;
            return Ok(());
        }
        let events = context.input(|input| input.events.clone());
        // Count at the UI boundary, before worker queueing can compress click timing.
        let time = context.input(|input| input.time);
        let click_limits = ClickLimits::current(context);
        let visible = |point: Pos2| -> bool {
            screen.contains(point)
                && spot.is_some_and(|spot| {
                    let point = point.to_vec2() * native_units_per_point;
                    spot.visible.iter().any(|rect| {
                        point.x >= rect.x as f32
                            && point.y >= rect.y as f32
                            && point.x < rect.x as f32 + rect.w as f32
                            && point.y < rect.y as f32 + rect.h as f32
                    })
                })
        };
        for event in events {
            let (position, kind, modifiers) = match event {
                egui::Event::ModifiersChanged(modifiers) => {
                    self.modifiers = modifiers;
                    continue;
                }
                egui::Event::PointerMoved(position) => (
                    position,
                    PointerKind::Move,
                    self.modifiers,
                ),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    self.modifiers = modifiers;
                    let button = match button {
                        egui::PointerButton::Primary => 1,
                        egui::PointerButton::Secondary => 2,
                        egui::PointerButton::Middle => 3,
                        _ => continue,
                    };
                    (
                        pos,
                        if pressed {
                            PointerKind::Down(button)
                        } else {
                            PointerKind::Up(button)
                        },
                        modifiers,
                    )
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if modifiers.alt || owns_popup => {
                    self.modifiers = modifiers;
                    let Some(position) = context.pointer_latest_pos() else {
                        continue;
                    };
                    let multiplier = match unit {
                        egui::MouseWheelUnit::Point => 1.0,
                        egui::MouseWheelUnit::Line => 20.0,
                        egui::MouseWheelUnit::Page => screen.height(),
                    };
                    (
                        position,
                        PointerKind::Wheel {
                            dx: delta.x * multiplier,
                            dy: delta.y * multiplier,
                        },
                        modifiers,
                    )
                }
                _ => continue,
            };
            let popup = owns_popup
                .then(|| {
                    self.popup_rects
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(index, rect)| {
                            (self.popups[index].interactive && rect.contains(position))
                                .then_some(index)
                        })
                })
                .flatten();
            if owns_popup && matches!(kind, PointerKind::Down(_)) && popup.is_none() {
                self.dismiss_popups(editor, screen, size)?;
                break;
            }
            let hovered =
                focused && (popup.is_some() || (top_modal.is_none() && visible(position)));
            match kind {
                PointerKind::Down(_) if !hovered => {
                    self.clicks = ClickTracker::default();
                    continue;
                }
                PointerKind::Up(button) if self.buttons & (1 << (button - 1)) == 0 => continue,
                PointerKind::Move if !hovered && self.buttons == 0 => continue,
                PointerKind::Wheel { .. } if !hovered => continue,
                _ => {}
            }
            let next_buttons = match kind {
                PointerKind::Down(b) => self.buttons | (1 << (b - 1)),
                PointerKind::Up(b) => self.buttons & !(1 << (b - 1)),
                _ => self.buttons,
            };
            if matches!(kind, PointerKind::Down(_)) && self.buttons == 0 {
                self.drag_popup = popup.and_then(|index| self.popup_mapping(index));
            }
            let click_count = match kind {
                PointerKind::Down(button) => {
                    self.clicks
                        .press(button, popup, position, time, click_limits)
                }
                PointerKind::Up(button) => {
                    self.clicks.release(button, position, time, click_limits)
                }
                PointerKind::Move => {
                    self.clicks.moved(position, click_limits);
                    0
                }
                _ => 0,
            };
            if matches!(kind, PointerKind::Down(_)) && popup.is_none() {
                self.popup_anchor = Some(native_from_screen(screen, size, position));
            }
            editor.pointer(EditorPointer {
                timestamp_ms: super::pointer_timestamp_ms(),
                modifiers: modifier_mask(modifiers),
                click_count,
                position: self.native_position(screen, size, position, popup),
                kind,
                buttons: next_buttons,
            })?;
            self.buttons = next_buttons;
            if self.buttons == 0 {
                self.drag_popup = None;
            }
            self.last_position = Some(position);
        }
        // Release ownership on focus loss, hidden graph tabs, or a lost OS pointer-up.
        let down = context.input(|input| input.pointer.any_down());
        if self.buttons != 0 && (!focused || spot.is_none() || !down) {
            let position = self.last_position.unwrap_or(screen.min);
            for button in 1..=3 {
                let bit = 1 << (button - 1);
                if self.buttons & bit != 0 {
                    editor.pointer(EditorPointer {
                        timestamp_ms: super::pointer_timestamp_ms(),
                        modifiers: 0,
                        click_count: 0,
                        position: self.native_position(screen, size, position, None),
                        kind: PointerKind::Up(button),
                        buttons: self.buttons & !bit,
                    })?;
                    self.buttons &= !bit;
                }
            }
            self.drag_popup = None;
        }
        if !focused || spot.is_none() {
            self.clicks = ClickTracker::default();
            self.modifiers = egui::Modifiers::default();
        }
        Ok(())
    }

    fn native_position(
        &self,
        screen: Rect,
        size: (u32, u32),
        position: Pos2,
        popup: Option<usize>,
    ) -> [f64; 2] {
        if let Some(popup) = self.drag_popup.or_else(|| {
            if self.buttons == 0 {
                popup.and_then(|index| self.popup_mapping(index))
            } else {
                None
            }
        }) {
            let local = native_from_screen(popup.screen, popup.native_size, position);
            [popup.origin[0] + local[0], popup.origin[1] + local[1]]
        } else {
            native_from_screen(screen, size, position)
        }
    }

    fn popup_mapping(&self, index: usize) -> Option<PopupMapping> {
        let popup = self.popups.get(index)?;
        Some(PopupMapping {
            screen: *self.popup_rects.get(index)?,
            origin: popup.origin,
            native_size: popup.native_size,
        })
    }

    fn dismiss_popups(
        &mut self,
        editor: &mut dyn Editor,
        screen: Rect,
        size: (u32, u32),
    ) -> Result<()> {
        let position =
            self.native_position(screen, size, self.last_position.unwrap_or(screen.min), None);
        for button in 1..=3 {
            let bit = 1 << (button - 1);
            if self.buttons & bit != 0 {
                editor.pointer(EditorPointer {
                    timestamp_ms: super::pointer_timestamp_ms(),
                    modifiers: 0,
                    click_count: 0,
                    position,
                    kind: PointerKind::Up(button),
                    buttons: self.buttons & !bit,
                })?;
                self.buttons &= !bit;
            }
        }
        editor.pointer(EditorPointer {
            timestamp_ms: super::pointer_timestamp_ms(),
            modifiers: 0,
            click_count: 0,
            position,
            kind: PointerKind::Cancel,
            buttons: 0,
        })?;
        self.cancel_popup = true;
        self.drag_popup = None;
        self.popup_layer = None;
        self.popup_rects.clear();
        self.popups.clear();
        self.popup_offset = None;
        self.clicks = ClickTracker::default();
        Ok(())
    }
}

fn modifier_mask(modifiers: egui::Modifiers) -> u8 {
    (u8::from(modifiers.shift) * super::MOD_SHIFT)
        | (u8::from(modifiers.ctrl) * super::MOD_CONTROL)
        | (u8::from(modifiers.alt) * super::MOD_ALT)
        | (u8::from(modifiers.mac_cmd) * super::MOD_SUPER)
}

fn native_from_screen(screen: Rect, native_size: (u32, u32), position: Pos2) -> [f64; 2] {
    let relative = (position - screen.min) / screen.size();
    [
        f64::from(relative.x) * f64::from(native_size.0),
        f64::from(relative.y) * f64::from(native_size.1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;

    #[test]
    fn repeated_pixels_keep_textures_while_geometry_popups_and_delivery_advance() {
        let context = egui::Context::default();
        context.tex_manager().write().take_delta().clear();
        let uploads = || {
            let mut delta = context.tex_manager().write().take_delta();
            let textures: Vec<_> = delta.set.keys().copied().collect();
            delta.clear();
            textures
        };
        let mut view = EditorView::default();
        let mut frame = EditorFrame {
            width: 2,
            height: 2,
            native_size: (2, 2),
            native_units_per_point: 1.0,
            rgba: [23, 45, 67, 255].repeat(4),
            popups: vec![super::super::EditorPopup {
                origin: [0.0, 1.0],
                width: 2,
                height: 1,
                native_size: (2, 1),
                rgba: [89, 12, 34, 220].repeat(2),
                interactive: true,
            }],
        };
        view.update_texture(&context, frame.clone());
        assert_eq!(uploads().len(), 2);
        let root_texture = view.texture.as_ref().unwrap().id();
        let popup_texture = view.popups[0].texture.id();

        frame.native_size = (4, 4);
        frame.native_units_per_point = 2.0;
        frame.popups[0].origin = [8.0, 12.0];
        frame.popups[0].native_size = (4, 2);
        frame.popups[0].interactive = false;
        view.update_texture(&context, frame.clone());
        assert!(uploads().is_empty());
        assert_eq!(view.native_size, Some((4, 4)));
        assert_eq!(view.native_units_per_point, Some(2.0));
        assert_eq!(view.popups[0].origin, [8.0, 12.0]);
        assert_eq!(view.popups[0].native_size, (4, 2));
        assert!(!view.popups[0].interactive);
        assert_eq!(view.frame_stats().unwrap().frames, 2);

        // A fading menu changes independently of the stationary root editor.
        frame.popups[0].rgba[3] = 100;
        view.update_texture(&context, frame.clone());
        assert_eq!(uploads(), [popup_texture]);

        // The same byte count with a different shape still requires a new upload.
        frame.width = 1;
        frame.height = 4;
        view.update_texture(&context, frame);
        assert_eq!(uploads(), [root_texture]);
        assert_eq!(view.texture.as_ref().unwrap().size(), [1, 4]);
    }

    #[test]
    fn display_density_changes_wait_for_matching_pixels_and_preserve_logical_size() {
        struct DensityEditor {
            frame: Option<EditorFrame>,
            density: f32,
        }
        impl Editor for DensityEditor {
            fn placement(&self) -> crate::plugins::Placement {
                crate::plugins::Placement::Embedded(1)
            }
            fn size(&self) -> (u32, u32) {
                ((320.0 * self.density) as u32, (180.0 * self.density) as u32)
            }
            fn set_display_scale(&mut self, factor: f32) { self.density = factor; }
            fn native_units_per_point(&self) -> f32 { self.density }
            fn take_frame(&mut self) -> Option<EditorFrame> { self.frame.take() }
            fn place(&mut self, _: Option<&Spot>) {}
            fn idle(&mut self) -> bool { true }
        }
        let image = |width, height, density| EditorFrame {
            width,
            height,
            native_size: (width, height),
            native_units_per_point: density,
            rgba: vec![255; width as usize * height as usize * 4],
            popups: Vec::new(),
        };
        let context = egui::Context::default();
        let mut editor = DensityEditor { frame: Some(image(320, 180, 1.0)), density: 1.0 };
        let mut view = EditorView::default();
        for frame in 0..4 {
            if frame == 2 { editor.frame = Some(image(640, 360, 2.0)); }
            if frame == 3 { editor.frame = Some(image(800, 400, 2.0)); }
            let mut size = Vec2::ZERO;
            let mut output = context.run_ui(egui::RawInput::default(), |ui| {
                size = view.paint(ui, &mut editor, if frame == 0 { 1.0 } else { 2.0 }).size();
            });
            output.textures_delta.clear();
            let expected = if frame == 3 { egui::vec2(400.0, 200.0) } else { egui::vec2(320.0, 180.0) };
            assert!((size - expected).length() < 0.01, "density transition resized an older image on frame {frame}");
        }
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn click_sequences_count_double_and_triple_clicks_on_press() {
        let limits = ClickLimits {
            interval: 0.5,
            distance: Vec2::splat(4.0),
        };
        let mut tracker = ClickTracker::default();
        for (time, expected) in [(10.0, 1), (10.2, 2), (10.4, 3)] {
            assert_eq!(tracker.press(1, None, Pos2::ZERO, time, limits), expected);
            assert_eq!(
                tracker.release(1, Pos2::ZERO, time + 0.01, limits),
                expected
            );
        }
    }

    #[test]
    fn click_sequences_reject_slow_distant_dragged_and_different_target_clicks() {
        let limits = ClickLimits {
            interval: 0.5,
            distance: Vec2::splat(4.0),
        };
        for (time, point, button, popup) in [
            (10.6, Pos2::ZERO, 1, None),
            (10.2, egui::pos2(5.0, 0.0), 1, None),
            (10.2, Pos2::ZERO, 2, None),
            (10.2, Pos2::ZERO, 1, Some(0)),
        ] {
            let mut tracker = ClickTracker::default();
            assert_eq!(tracker.press(1, None, Pos2::ZERO, 10.0, limits), 1);
            assert_eq!(tracker.release(1, Pos2::ZERO, 10.01, limits), 1);
            assert_eq!(tracker.press(button, popup, point, time, limits), 1);
        }
        let mut tracker = ClickTracker::default();
        tracker.press(1, None, Pos2::ZERO, 10.0, limits);
        tracker.moved(egui::pos2(20.0, 0.0), limits);
        tracker.moved(Pos2::ZERO, limits);
        assert_eq!(tracker.release(1, Pos2::ZERO, 10.05, limits), 0);
        assert_eq!(tracker.press(1, None, Pos2::ZERO, 10.1, limits), 1);
        // A second down without a release is not a second click.
        assert_eq!(tracker.press(1, None, Pos2::ZERO, 10.2, limits), 1);
    }

    #[test]
    fn mapping_keeps_native_coordinates_at_every_graph_zoom_and_dpi() {
        for zoom in [0.2, 0.5, 1.0, 1.5, 3.0] {
            for dpi in [1.0, 2.0] {
                let size = Vec2::new(640.0, 360.0) * zoom / dpi;
                let screen = Rect::from_min_size(egui::pos2(-123.0, 241.0), size);
                let point = screen.min + Vec2::new(80.0, 90.0) * zoom / dpi;
                let mapped = native_from_screen(screen, (640, 360), point);
                assert!((mapped[0] - 80.0).abs() < 0.001);
                assert!((mapped[1] - 90.0).abs() < 0.001);
            }
        }
    }

    struct RecordingEditor(Vec<EditorPointer>);
    impl Editor for RecordingEditor {
        fn placement(&self) -> crate::plugins::Placement {
            crate::plugins::Placement::Embedded(1)
        }
        fn size(&self) -> (u32, u32) {
            (640, 360)
        }
        fn place(&mut self, _: Option<&Spot>) {}
        fn idle(&mut self) -> bool {
            true
        }
        fn pointer(&mut self, event: EditorPointer) -> Result<()> {
            self.0.push(event);
            Ok(())
        }
    }

    #[test]
    fn pointer_modifiers_are_recorded_before_native_or_worker_delivery() {
        use crate::plugins::PxRect;
        let context = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(200.0, 100.0));
        let mut view = EditorView { screen: Some(screen), ..Default::default() };
        view.update_texture(&context, EditorFrame {
            width: 2,
            height: 1,
            native_size: (400, 200),
            native_units_per_point: 1.0,
            rgba: vec![255; 8],
            popups: Vec::new(),
        });
        let spot = Spot {
            clip: PxRect { x: 0, y: 0, w: 200, h: 100 },
            origin: (0, 0),
            visible: vec![PxRect { x: 0, y: 0, w: 200, h: 100 }],
        };
        let mut editor = RecordingEditor(Vec::new());
        let mut output = context.run_ui(egui::RawInput {
            focused: true,
            events: vec![
                egui::Event::PointerButton {
                    pos: egui::pos2(20.0, 30.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers { ctrl: true, ..Default::default() },
                },
                egui::Event::ModifiersChanged(egui::Modifiers { shift: true, ..Default::default() }),
                egui::Event::PointerMoved(egui::pos2(40.0, 30.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(40.0, 30.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers { alt: true, ..Default::default() },
                },
            ],
            ..Default::default()
        }, |ui| view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0).unwrap());
        output.textures_delta.clear();
        assert_eq!(editor.0.iter().map(|event| event.modifiers).collect::<Vec<_>>(),
            [super::super::MOD_CONTROL, super::super::MOD_SHIFT, super::super::MOD_ALT]);
        assert!((editor.0[1].position[0] - 80.0).abs() < 0.001);
        assert!((editor.0[1].position[1] - 60.0).abs() < 0.001);
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn double_clicks_are_forwarded_and_focus_or_covered_clicks_reset_the_sequence() {
        use crate::plugins::PxRect;
        let context = egui::Context::default();
        let mut view = EditorView {
            screen: Some(Rect::from_min_size(
                egui::pos2(100.0, 100.0),
                egui::vec2(320.0, 180.0),
            )),
            ..Default::default()
        };
        view.update_texture(
            &context,
            EditorFrame {
                width: 2,
                height: 1,
                native_size: (640, 360),
                native_units_per_point: 1.0,
                rgba: vec![255; 8],
                popups: Vec::new(),
            },
        );
        let step = ClickLimits::current(&context).interval / 8.0;
        let mut editor = RecordingEditor(Vec::new());
        let click = |pressed| egui::Event::PointerButton {
            pos: egui::pos2(140.0, 145.0),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut run = |tick, events, focused, covered| {
            let clip = PxRect {
                x: 100,
                y: 100,
                w: 320,
                h: 180,
            };
            let spot = Spot {
                clip,
                origin: (100, 100),
                visible: if covered { Vec::new() } else { vec![clip] },
            };
            let mut output = context.run_ui(
                egui::RawInput {
                    time: Some(10.0 + f64::from(tick) * step),
                    focused,
                    events,
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
                    ..Default::default()
                },
                |ui| {
                    view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                        .unwrap()
                },
            );
            output.textures_delta.clear();
        };
        run(0, vec![click(true)], true, false);
        run(1, vec![click(false)], true, false);
        run(2, vec![click(true)], true, false);
        run(3, vec![click(false)], true, false);
        run(4, Vec::new(), false, false);
        run(5, vec![click(true)], true, false);
        run(6, vec![click(false)], true, false);
        run(7, vec![click(true), click(false)], true, true);
        run(8, vec![click(true), click(false)], true, false);
        assert_eq!(
            editor
                .0
                .iter()
                .map(|event| event.click_count)
                .collect::<Vec<_>>(),
            [1, 1, 2, 2, 1, 1, 1, 1]
        );
        assert!(editor.0.iter().all(|event| event.position == [80.0, 90.0]));
        assert!(
            matches!(editor.0[2].kind, PointerKind::Down(1)),
            "double-click must be known on down"
        );
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn covered_clicks_are_rejected_but_an_owned_drag_keeps_its_release() {
        use crate::plugins::PxRect;
        let context = egui::Context::default();
        let screen = Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(320.0, 180.0));
        let mut view = EditorView {
            native_size: Some((640, 360)),
            screen: Some(screen),
            ..Default::default()
        };
        view.update_texture(
            &context,
            EditorFrame {
                popups: Vec::new(),
                width: 2,
                height: 1,
                native_size: (640, 360),
                native_units_per_point: 1.0,
                rgba: vec![255; 8],
            },
        );
        let spot = Spot {
            clip: PxRect {
                x: 100,
                y: 100,
                w: 320,
                h: 180,
            },
            origin: (100, 100),
            visible: vec![PxRect {
                x: 100,
                y: 100,
                w: 50,
                h: 180,
            }],
        };
        let button = |point, pressed| egui::Event::PointerButton {
            pos: point,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut editor = RecordingEditor(Vec::new());
        let mut output = context.run_ui(
            egui::RawInput {
                focused: true,
                events: vec![
                    button(egui::pos2(170.0, 145.0), true),
                    button(egui::pos2(170.0, 145.0), false),
                ],
                ..Default::default()
            },
            |ui| {
                view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                    .unwrap();
            },
        );
        output.textures_delta.clear();
        assert!(editor.0.is_empty(), "the overlay owns the covered click");
        let mut output = context.run_ui(
            egui::RawInput {
                focused: true,
                events: vec![
                    button(egui::pos2(140.0, 145.0), true),
                    egui::Event::PointerMoved(egui::pos2(500.0, 500.0)),
                    button(egui::pos2(500.0, 500.0), false),
                ],
                ..Default::default()
            },
            |ui| {
                view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                    .unwrap();
            },
        );
        output.textures_delta.clear();
        assert_eq!(editor.0.len(), 3);
        assert!(matches!(editor.0[0].kind, PointerKind::Down(1)));
        assert_eq!(editor.0[0].position, [80.0, 90.0]);
        assert!(matches!(editor.0[2].kind, PointerKind::Up(1)));
        assert_eq!(editor.0[2].buttons, 0);
        assert_eq!(view.buttons, 0);
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn popup_outside_the_editor_receives_clicks_and_escape_dismisses_it() {
        use crate::plugins::{PxRect, surface::EditorPopup};
        let context = egui::Context::default();
        let mut view = EditorView {
            screen: Some(Rect::from_min_size(
                egui::pos2(100.0, 100.0),
                egui::vec2(320.0, 180.0),
            )),
            ..Default::default()
        };
        view.update_texture(
            &context,
            EditorFrame {
                width: 2,
                height: 1,
                native_size: (640, 360),
                native_units_per_point: 1.0,
                rgba: vec![255; 8],
                popups: vec![EditorPopup {
                    origin: [600.0, -40.0],
                    width: 2,
                    height: 1,
                    native_size: (100, 80),
                    rgba: vec![255; 8],
                    interactive: true,
                }],
            },
        );
        // A modal covers the root image; its own popup must still receive input.
        let spot = Spot {
            clip: PxRect {
                x: 100,
                y: 100,
                w: 320,
                h: 180,
            },
            origin: (100, 100),
            visible: Vec::new(),
        };
        let mut editor = RecordingEditor(Vec::new());
        let mut run = |events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    focused: true,
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    view.paint_popups(ui.ctx(), egui::Id::new("popup-regression"));
                    view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                        .unwrap();
                },
            );
            output.textures_delta.clear();
        };
        // egui establishes modal ownership from the previous frame.
        run(Vec::new());
        run(Vec::new());
        let click = |pressed| egui::Event::PointerButton {
            pos: egui::pos2(425.0, 100.0),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(vec![click(true), click(false)]);
        run(vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert_eq!(editor.0.len(), 3);
        assert!(matches!(editor.0[0].kind, PointerKind::Down(1)));
        assert_eq!(editor.0[0].position, [650.0, 0.0]);
        assert!(matches!(editor.0[1].kind, PointerKind::Up(1)));
        assert!(matches!(editor.0[2].kind, PointerKind::Cancel));
        assert_eq!(view.native_size, Some((640, 360)));
        assert!(view.popups.is_empty());
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn clamped_popup_keeps_native_input_and_dismisses_on_outside_focus_or_hidden_graph() {
        use crate::plugins::{PxRect, surface::EditorPopup};
        for dismiss in ["outside", "focus", "hidden"] {
            let context = egui::Context::default();
            let mut view = EditorView {
                screen: Some(Rect::from_min_size(
                    egui::pos2(100.0, 100.0),
                    egui::vec2(320.0, 180.0),
                )),
                ..Default::default()
            };
            view.update_texture(
                &context,
                EditorFrame {
                    width: 2,
                    height: 1,
                    native_size: (640, 360),
                    native_units_per_point: 1.0,
                    rgba: vec![255; 8],
                    popups: vec![EditorPopup {
                        origin: [1000.0, 400.0],
                        width: 2,
                        height: 1,
                        native_size: (100, 80),
                        rgba: vec![255; 8],
                        interactive: true,
                    }],
                },
            );
            let spot = Spot {
                clip: PxRect {
                    x: 100,
                    y: 100,
                    w: 320,
                    h: 180,
                },
                origin: (100, 100),
                visible: Vec::new(),
            };
            let mut editor = RecordingEditor(Vec::new());
            let mut run = |events, focused, shown| {
                let mut output = context.run_ui(
                    egui::RawInput {
                        focused,
                        screen_rect: Some(Rect::from_min_size(
                            Pos2::ZERO,
                            egui::vec2(500.0, 300.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        if shown {
                            view.paint_popups(ui.ctx(), egui::Id::new("clamped-popup"));
                        }
                        view.dispatch(ui.ctx(), &mut editor, shown.then_some(&spot), 1.0)
                            .unwrap();
                    },
                );
                output.textures_delta.clear();
            };
            run(Vec::new(), true, true);
            run(Vec::new(), true, true);
            let click = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let at = egui::pos2(475.0, 280.0);
            run(vec![click(at, true), click(at, false)], true, true);
            run(
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(0.0, -1.0),
                    modifiers: egui::Modifiers::NONE,
                }],
                true,
                true,
            );
            match dismiss {
                "outside" => run(vec![click(egui::pos2(20.0, 20.0), true)], true, true),
                "focus" => run(Vec::new(), false, true),
                _ => run(Vec::new(), true, false),
            }
            assert_eq!(editor.0.len(), 4, "{dismiss}");
            assert_eq!(editor.0[0].position, [1050.0, 440.0], "{dismiss}");
            assert!(matches!(
                editor.0[2].kind,
                PointerKind::Wheel { dy: -20.0, .. }
            ));
            assert!(matches!(editor.0[3].kind, PointerKind::Cancel), "{dismiss}");
            assert!(view.popups.is_empty());
            drop(view);
            context.tex_manager().write().take_delta().clear();
        }
    }

    #[test]
    fn monitor_clamped_menu_stays_at_its_opening_click_and_keeps_native_input() {
        use crate::plugins::surface::EditorPopup;
        let context = egui::Context::default();
        let mut view = EditorView {
            screen: Some(Rect::from_min_size(
                egui::pos2(100.0, 80.0),
                egui::vec2(626.0, 385.0),
            )),
            popup_anchor: Some([24.0, 392.0]),
            ..Default::default()
        };
        view.update_texture(
            &context,
            EditorFrame {
                width: 2,
                height: 1,
                native_size: (1252, 770),
                native_units_per_point: 2.0,
                rgba: vec![255; 8],
                popups: vec![EditorPopup {
                    origin: [1284.0, 802.0],
                    width: 2,
                    height: 1,
                    native_size: (622, 708),
                    rgba: vec![255; 8],
                    interactive: true,
                }],
            },
        );
        let paint = |view: &mut EditorView| {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(2000.0, 1600.0))),
                    ..Default::default()
                },
                |ui| view.paint_popups(ui.ctx(), egui::Id::new("monitor-clamped-menu")),
            );
            output.textures_delta.clear();
        };
        paint(&mut view);
        let first = view.popup_rects[0];
        assert_eq!(first.min, egui::pos2(112.0, 276.0));
        let native = view.native_position(
            view.screen.unwrap(),
            (1252, 770),
            first.min + egui::vec2(10.0, 15.0),
            Some(0),
        );
        assert!((native[0] - 1304.0).abs() < 0.001 && (native[1] - 832.0).abs() < 0.001);
        view.last_position = Some(egui::pos2(900.0, 1000.0));
        paint(&mut view);
        assert_eq!(
            view.popup_rects[0], first,
            "menu must not chase the pointer"
        );
        view.screen = Some(Rect::from_min_size(
            egui::pos2(200.0, 120.0),
            egui::vec2(1252.0, 770.0),
        ));
        paint(&mut view);
        let zoomed = view.popup_rects[0];
        assert_eq!(zoomed.min, egui::pos2(224.0, 512.0));
        let native = view.native_position(
            view.screen.unwrap(),
            (1252, 770),
            zoomed.min + egui::vec2(20.0, 30.0),
            Some(0),
        );
        assert!((native[0] - 1304.0).abs() < 0.001 && (native[1] - 832.0).abs() < 0.001);
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn popup_release_retains_its_mapping_after_the_popup_image_disappears() {
        use crate::plugins::{PxRect, surface::EditorPopup};
        let context = egui::Context::default();
        let mut view = EditorView {
            screen: Some(Rect::from_min_size(
                egui::pos2(100.0, 100.0),
                egui::vec2(320.0, 180.0),
            )),
            ..Default::default()
        };
        let frame = |open| EditorFrame {
            width: 2,
            height: 1,
            native_size: (640, 360),
            native_units_per_point: 1.0,
            rgba: vec![255; 8],
            popups: if open {
                vec![EditorPopup {
                    origin: [1000.0, 400.0],
                    width: 2,
                    height: 1,
                    native_size: (100, 80),
                    rgba: vec![255; 8],
                    interactive: true,
                }]
            } else {
                Vec::new()
            },
        };
        view.update_texture(&context, frame(true));
        let spot = Spot {
            clip: PxRect {
                x: 100,
                y: 100,
                w: 320,
                h: 180,
            },
            origin: (100, 100),
            visible: Vec::new(),
        };
        let mut editor = RecordingEditor(Vec::new());
        let mut run = |events, image| {
            if let Some(image) = image {
                view.update_texture(&context, frame(image));
            }
            let mut output = context.run_ui(
                egui::RawInput {
                    focused: true,
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(500.0, 300.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    view.paint_popups(ui.ctx(), egui::Id::new("popup-release"));
                    view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                        .unwrap();
                },
            );
            output.textures_delta.clear();
        };
        run(Vec::new(), None);
        run(Vec::new(), None);
        let click = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(vec![click(egui::pos2(475.0, 280.0), true)], None);
        // The worker can remove a menu on mouse-down, before its release reaches us.
        let released = egui::pos2(485.0, 275.0);
        run(
            vec![egui::Event::PointerMoved(released), click(released, false)],
            Some(false),
        );
        assert_eq!(editor.0.len(), 3);
        assert_eq!(editor.0[0].position, [1050.0, 440.0]);
        for event in &editor.0[1..] {
            assert!((event.position[0] - 1070.0).abs() < 0.001);
            assert!((event.position[1] - 430.0).abs() < 0.001);
        }
        assert!(matches!(editor.0[2].kind, PointerKind::Up(1)));
        assert_eq!(view.buttons, 0);
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }

    #[test]
    fn inactive_popup_fade_does_not_cover_the_editor_or_consume_clicks() {
        use crate::plugins::{PxRect, surface::EditorPopup};
        let context = egui::Context::default();
        let mut view = EditorView {
            screen: Some(Rect::from_min_size(
                egui::pos2(100.0, 100.0),
                egui::vec2(320.0, 180.0),
            )),
            ..Default::default()
        };
        view.update_texture(
            &context,
            EditorFrame {
                width: 2,
                height: 1,
                native_size: (640, 360),
                native_units_per_point: 1.0,
                rgba: vec![255; 8],
                popups: vec![EditorPopup {
                    origin: [20.0, 20.0],
                    width: 2,
                    height: 1,
                    native_size: (100, 80),
                    rgba: vec![255; 8],
                    interactive: false,
                }],
            },
        );
        let clip = PxRect {
            x: 100,
            y: 100,
            w: 320,
            h: 180,
        };
        let spot = Spot {
            clip,
            origin: (100, 100),
            visible: vec![clip],
        };
        let id = egui::Id::new("inactive-popup");
        let mut editor = RecordingEditor(Vec::new());
        let click = |pressed| egui::Event::PointerButton {
            pos: egui::pos2(130.0, 130.0),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        for events in [Vec::new(), Vec::new(), vec![click(true), click(false)]] {
            let mut output = context.run_ui(
                egui::RawInput {
                    focused: true,
                    events,
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
                    ..Default::default()
                },
                |ui| {
                    view.paint_popups(ui.ctx(), id);
                    view.dispatch(ui.ctx(), &mut editor, Some(&spot), 1.0)
                        .unwrap();
                },
            );
            output.textures_delta.clear();
            assert!(
                context.memory(|memory| memory.area_rect(id)).is_none(),
                "an inactive native popup must not enter editor coverage"
            );
        }
        assert_eq!(editor.0.len(), 2);
        assert!(matches!(editor.0[0].kind, PointerKind::Down(1)));
        assert!(
            editor.0[0]
                .position
                .iter()
                .all(|value| (value - 60.0).abs() < 0.001)
        );
        assert!(matches!(editor.0[1].kind, PointerKind::Up(1)));
        assert!(view.popup_layer.is_none());
        drop(view);
        context.tex_manager().write().take_delta().clear();
    }
}
