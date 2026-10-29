//! [`NpUi`] for native plugins, on top of egui.

use std::ffi::c_void;

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Vec2, pos2, vec2};
use nodeplayer_plugin::abi::*;

use crate::plugins::BodyIo;

/// What one [`NpUi`] handle refers to. Nested layouts get their own, sharing the flags.
struct UiCtx {
    ui: *mut egui::Ui,
    /// The last canvas; painting is clipped to it.
    canvas: Option<Rect>,
    /// Mouse wheel taken from the graph view this frame.
    scroll: Vec2,
    wants_scroll: bool,
}

/// Runs `f` with an [`NpUi`] drawing into `ui`.
pub fn with_ui<R>(ui: &mut egui::Ui, io: &mut BodyIo, f: impl FnOnce(*const NpUi) -> R) -> R {
    let mut ctx = UiCtx { ui, canvas: None, scroll: io.scroll, wants_scroll: false };
    let np = NpUi { ctx: &mut ctx as *mut UiCtx as *mut c_void, vt: &VTBL };
    let r = f(&np);
    io.wants_scroll |= ctx.wants_scroll;
    r
}

static VTBL: NpUiVtbl = NpUiVtbl {
    label,
    colored_label,
    button,
    selectable,
    checkbox,
    slider,
    drag_value,
    text_edit,
    combo,
    separator,
    space,
    progress,
    layout,
    available_width,
    canvas,
    paint,
    request_repaint,
};

unsafe fn ctx<'a>(ui: *const NpUi) -> &'a mut UiCtx {
    unsafe { &mut *((*ui).ctx as *mut UiCtx) }
}

unsafe fn eui<'a>(ui: *const NpUi) -> &'a mut egui::Ui {
    unsafe { &mut *ctx(ui).ui }
}

fn color(rgba: u32) -> Color32 {
    let [r, g, b, a] = rgba.to_be_bytes();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

unsafe extern "C" fn label(ui: *const NpUi, text: NpStr, style: u32) {
    let (u, t) = unsafe { (eui(ui), text.as_str()) };
    match style {
        TEXT_HEADING => u.heading(t),
        TEXT_SMALL => u.small(t),
        TEXT_WEAK => u.weak(t),
        TEXT_MONO => u.monospace(t),
        _ => u.label(t),
    };
}

unsafe extern "C" fn colored_label(ui: *const NpUi, text: NpStr, rgba: u32) {
    unsafe { eui(ui).colored_label(color(rgba), text.as_str()) };
}

unsafe extern "C" fn button(ui: *const NpUi, text: NpStr, small: bool) -> bool {
    let (u, t) = unsafe { (eui(ui), text.as_str()) };
    if small { u.small_button(t).clicked() } else { u.button(t).clicked() }
}

unsafe extern "C" fn selectable(ui: *const NpUi, text: NpStr, selected: bool) -> bool {
    unsafe { eui(ui).selectable_label(selected, text.as_str()).clicked() }
}

unsafe extern "C" fn checkbox(ui: *const NpUi, text: NpStr, value: *mut bool) -> bool {
    if value.is_null() {
        return false;
    }
    unsafe { eui(ui).checkbox(&mut *value, text.as_str()).changed() }
}

unsafe extern "C" fn slider(ui: *const NpUi, text: NpStr, value: *mut f64, min: f64, max: f64, flags: u32) -> bool {
    if value.is_null() || !min.is_finite() || !max.is_finite() {
        return false;
    }
    let mut s = egui::Slider::new(unsafe { &mut *value }, min..=max).text(unsafe { text.as_str() });
    if flags & SLIDER_LOG != 0 {
        s = s.logarithmic(true);
    }
    if flags & SLIDER_INTEGER != 0 {
        s = s.step_by(1.0);
    }
    unsafe { eui(ui) }.add(s).changed()
}

unsafe extern "C" fn drag_value(
    ui: *const NpUi,
    prefix: NpStr,
    value: *mut f64,
    speed: f64,
    min: f64,
    max: f64,
    suffix: NpStr,
) -> bool {
    if value.is_null() {
        return false;
    }
    let mut d = egui::DragValue::new(unsafe { &mut *value })
        .speed(speed)
        .prefix(unsafe { prefix.as_str() })
        .suffix(unsafe { suffix.as_str() });
    if min.is_finite() && max.is_finite() && min <= max {
        d = d.range(min..=max);
    }
    unsafe { eui(ui) }.add(d).changed()
}

unsafe extern "C" fn text_edit(ui: *const NpUi, buf: *mut u8, cap: usize, len: *mut usize, hint: NpStr) -> bool {
    if buf.is_null() || len.is_null() {
        return false;
    }
    let bytes = unsafe { std::slice::from_raw_parts_mut(buf, cap) };
    let mut s = String::from_utf8_lossy(&bytes[..unsafe { *len }.min(cap)]).into_owned();
    let changed = unsafe { eui(ui) }.add(egui::TextEdit::singleline(&mut s).hint_text(unsafe { hint.as_str() })).changed();
    if changed {
        let mut n = s.len().min(cap);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        bytes[..n].copy_from_slice(&s.as_bytes()[..n]);
        unsafe { *len = n };
    }
    changed
}

unsafe extern "C" fn combo(ui: *const NpUi, id: NpStr, selected: *mut u32, items: NpStr) -> bool {
    if selected.is_null() {
        return false;
    }
    let items = unsafe { items.as_str() };
    let items: Vec<&str> = items.split('\n').collect();
    let cur = unsafe { *selected } as usize;
    let mut pick = cur.min(items.len().saturating_sub(1));
    egui::ComboBox::from_id_salt(unsafe { id.as_str() }.into_owned())
        .selected_text(items.get(pick).copied().unwrap_or(""))
        .show_ui(unsafe { eui(ui) }, |ui| {
            for (k, item) in items.iter().enumerate() {
                ui.selectable_value(&mut pick, k, *item);
            }
        });
    unsafe { *selected = pick as u32 };
    pick != cur
}

unsafe extern "C" fn separator(ui: *const NpUi) {
    unsafe { eui(ui).separator() };
}

unsafe extern "C" fn space(ui: *const NpUi, amount: f32) {
    unsafe { eui(ui).add_space(amount.clamp(0.0, 1000.0)) };
}

unsafe extern "C" fn progress(ui: *const NpUi, fraction: f32, text: NpStr) {
    let t = unsafe { text.as_str() };
    let mut bar = egui::ProgressBar::new(fraction.clamp(0.0, 1.0));
    if !t.is_empty() {
        bar = bar.text(t.into_owned());
    }
    unsafe { eui(ui) }.add(bar);
}

unsafe extern "C" fn layout(ui: *const NpUi, kind: u32, title: NpStr, number: f64, body: NpUiFn, user: *mut c_void) {
    let parent = unsafe { ctx(ui) };
    let scroll = parent.scroll;
    let mut wants = false;
    let mut run = |inner: &mut egui::Ui| {
        let mut c = UiCtx { ui: inner, canvas: None, scroll, wants_scroll: false };
        let np = NpUi { ctx: &mut c as *mut UiCtx as *mut c_void, vt: &VTBL };
        unsafe { body(&np, user) };
        wants |= c.wants_scroll;
    };
    let u = unsafe { &mut *parent.ui };
    match kind {
        LAYOUT_HORIZONTAL => {
            u.horizontal(|ui| run(ui));
        }
        LAYOUT_GROUP => {
            u.group(|ui| run(ui));
        }
        LAYOUT_COLLAPSING => {
            egui::CollapsingHeader::new(unsafe { title.as_str() }.into_owned()).show(u, |ui| run(ui));
        }
        LAYOUT_SCROLL => {
            let id = u.next_auto_id();
            egui::ScrollArea::vertical().id_salt(id).max_height(number.max(16.0) as f32).show(u, |ui| run(ui));
        }
        LAYOUT_ID_SCOPE => {
            u.push_id(number.to_bits(), |ui| run(ui));
        }
        _ => {
            u.vertical(|ui| run(ui));
        }
    }
    parent.wants_scroll |= wants;
}

unsafe extern "C" fn available_width(ui: *const NpUi) -> f32 {
    unsafe { eui(ui).available_width() }
}

unsafe extern "C" fn canvas(ui: *const NpUi, w: f32, h: f32, out: *mut NpCanvas) {
    let c = unsafe { ctx(ui) };
    let u = unsafe { &mut *c.ui };
    let size = vec2(w.clamp(1.0, 4096.0), h.clamp(1.0, 4096.0));
    let (rect, resp) = u.allocate_exact_size(size, Sense::click_and_drag());
    c.canvas = Some(rect);
    let pointer = resp.hover_pos().or(resp.interact_pointer_pos()).unwrap_or(rect.min);
    let hovered = resp.hovered();
    if hovered {
        c.wants_scroll = true;
    }
    if out.is_null() {
        return;
    }
    let d = resp.drag_delta();
    unsafe {
        *out = NpCanvas {
            x: rect.min.x,
            y: rect.min.y,
            w: rect.width(),
            h: rect.height(),
            hovered,
            clicked: resp.clicked(),
            double_clicked: resp.double_clicked(),
            dragged: resp.dragged(),
            drag_dx: d.x,
            drag_dy: d.y,
            pointer_x: pointer.x,
            pointer_y: pointer.y,
            scroll_y: if hovered { c.scroll.y } else { 0.0 },
        };
    }
}

unsafe extern "C" fn paint(ui: *const NpUi, cmd: *const NpPaint) {
    if cmd.is_null() {
        return;
    }
    let c = unsafe { ctx(ui) };
    let u = unsafe { &mut *c.ui };
    let cmd = unsafe { &*cmd };
    let painter = u.painter_at(c.canvas.unwrap_or_else(|| u.max_rect()));
    let col = color(cmd.rgba);
    let (a, b) = (pos2(cmd.x1, cmd.y1), pos2(cmd.x2, cmd.y2));
    match cmd.kind {
        PAINT_LINE => {
            painter.line_segment([a, b], Stroke::new(cmd.size.max(0.1), col));
        }
        PAINT_RECT => {
            painter.rect_filled(Rect::from_two_pos(a, b), 0.0, col);
        }
        PAINT_RECT_STROKE => {
            painter.rect_stroke(Rect::from_two_pos(a, b), 0.0, Stroke::new(cmd.size.max(0.1), col), egui::StrokeKind::Inside);
        }
        PAINT_CIRCLE => {
            painter.circle_filled(a, cmd.size.max(0.0), col);
        }
        PAINT_CIRCLE_STROKE => {
            painter.circle_stroke(a, cmd.size.max(0.0), Stroke::new(1.0, col));
        }
        PAINT_TEXT => {
            let align = match cmd.align {
                ALIGN_CENTER => Align2::CENTER_CENTER,
                ALIGN_RIGHT => Align2::RIGHT_CENTER,
                _ => Align2::LEFT_CENTER,
            };
            let size = if cmd.size > 0.0 { cmd.size } else { 12.0 };
            painter.text(a, align, unsafe { cmd.text.as_str() }, FontId::proportional(size.min(200.0)), col);
        }
        _ => {}
    }
}

unsafe extern "C" fn request_repaint(ui: *const NpUi) {
    unsafe { eui(ui).ctx().request_repaint() };
}
