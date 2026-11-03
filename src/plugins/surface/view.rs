//! The canvas owns editor placement, zoom and input ownership; native sources stay fixed.

use anyhow::Result;
use egui::{Pos2, Rect};
use std::time::Instant;

use super::{EditorFrame, EditorPointer, PointerKind};
use crate::plugins::{Editor, Spot};

#[derive(Default)]
pub struct EditorView {
    texture: Option<egui::TextureHandle>,
    native_size: Option<(u32, u32)>,
    screen: Option<Rect>,
    buttons: u8,
    last_position: Option<Pos2>,
    frames: u64,
    first_frame_at: Option<Instant>,
    changed_frames: u64,
    last_frame_hash: Option<ring::digest::Digest>,
    popups: Vec<PopupTexture>,
    popup_rects: Vec<Rect>,
    popup_layer: Option<egui::LayerId>,
    cancel_popup: bool,
    drag_popup: Option<PopupMapping>,
}

struct PopupTexture {
    texture: egui::TextureHandle,
    origin: [f64; 2],
    native_size: (u32, u32),
    interactive: bool,
}

#[derive(Clone, Copy)]
struct PopupMapping {
    screen: Rect,
    origin: [f64; 2],
    native_size: (u32, u32),
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct EditorFrameStats {
    frames: u64,
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
        native_units_per_point: f32,
    ) -> Rect {
        if let Some(frame) = editor.take_frame().filter(|frame| frame.validate().is_ok()) {
            self.update_texture(ui.ctx(), frame);
        }
        let size = self.native_size.unwrap_or_else(|| editor.size());
        let units = native_units_per_point.max(f32::EPSILON);
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
        self.popups.truncate(frame.popups.len());
        if frame.popups.is_empty() {
            self.cancel_popup = false;
        }
        for (index, popup) in frame.popups.into_iter().enumerate() {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [popup.width as usize, popup.height as usize],
                &popup.rgba,
            );
            if let Some(previous) = self.popups.get_mut(index) {
                previous.texture.set(image, egui::TextureOptions::LINEAR);
                previous.origin = popup.origin;
                previous.native_size = popup.native_size;
                previous.interactive = popup.interactive;
            } else {
                self.popups.push(PopupTexture {
                    texture: context.load_texture(
                        "plugin-popup",
                        image,
                        egui::TextureOptions::LINEAR,
                    ),
                    origin: popup.origin,
                    native_size: popup.native_size,
                    interactive: popup.interactive,
                });
            }
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture =
                Some(context.load_texture("plugin-editor", image, egui::TextureOptions::LINEAR));
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
            let (position, kind) = match event {
                egui::Event::PointerMoved(position) => (position, PointerKind::Move),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
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
                    )
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if modifiers.alt || owns_popup => {
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
                PointerKind::Down(_) if !hovered => continue,
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
            editor.pointer(EditorPointer {
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
                        position: self.native_position(screen, size, position, None),
                        kind: PointerKind::Up(button),
                        buttons: self.buttons & !bit,
                    })?;
                    self.buttons &= !bit;
                }
            }
            self.drag_popup = None;
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
                    position,
                    kind: PointerKind::Up(button),
                    buttons: self.buttons & !bit,
                })?;
                self.buttons &= !bit;
            }
        }
        editor.pointer(EditorPointer {
            position,
            kind: PointerKind::Cancel,
            buttons: 0,
        })?;
        self.cancel_popup = true;
        self.drag_popup = None;
        self.popup_layer = None;
        self.popup_rects.clear();
        self.popups.clear();
        Ok(())
    }
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
