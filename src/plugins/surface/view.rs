//! The canvas owns editor placement, zoom and input ownership; native sources stay fixed.

use anyhow::Result;
use egui::{Pos2, Rect};

use super::{EditorFrame, EditorPointer, PointerKind};
use crate::plugins::{Editor, Spot};

#[derive(Default)]
pub struct EditorView {
    texture: Option<egui::TextureHandle>,
    native_size: Option<(u32, u32)>,
    screen: Option<Rect>,
    buttons: u8,
    last_position: Option<Pos2>,
}

impl EditorView {
    pub fn dimensions(&self) -> Option<((u32, u32), [usize; 2])> {
        Some((self.native_size?, self.texture.as_ref()?.size()))
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
        if let Some(error) = editor.capture_error() {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        rect
    }

    fn update_texture(&mut self, context: &egui::Context, frame: EditorFrame) {
        if self.texture.is_none()
            && let Some(run) = crate::diagnostics::active()
        {
            let _ = run.event("editor-frame", serde_json::json!({"native_size":frame.native_size,"capture_size":[frame.width,frame.height]}));
        }
        self.native_size = Some(frame.native_size);
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
                } if modifiers.alt => {
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
            let hovered = focused && visible(position);
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
            editor.pointer(EditorPointer {
                position: native_from_screen(screen, size, position),
                kind,
                buttons: next_buttons,
            })?;
            self.buttons = next_buttons;
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
                        position: native_from_screen(screen, size, position),
                        kind: PointerKind::Up(button),
                        buttons: self.buttons & !bit,
                    })?;
                    self.buttons &= !bit;
                }
            }
        }
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
}
