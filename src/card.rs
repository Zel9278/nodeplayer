//! The Now Playing card attached to Misskey notes, as media_info draws it: the cover, the
//! playback state, title, artist, a progress bar and the album, on a dark 820×270 image.

use std::sync::{Arc, OnceLock};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, VariableFont, point};
use image::{DynamicImage, Rgba, RgbaImage};

const CARD_W: u32 = 820;
const CARD_H: u32 = 270;
const PAD: i32 = 20;
const THUMB: u32 = 230;

const BG: Rgba<u8> = Rgba([30, 30, 46, 255]);
const FG: Rgba<u8> = Rgba([235, 235, 245, 255]);
const SUB: Rgba<u8> = Rgba([170, 170, 190, 255]);
const DIM: Rgba<u8> = Rgba([120, 120, 140, 255]);
const ACCENT: Rgba<u8> = Rgba([130, 220, 160, 255]);
const AMBER: Rgba<u8> = Rgba([235, 200, 120, 255]);
const THUMB_BG: Rgba<u8> = Rgba([55, 55, 75, 255]);
const BAR_BG: Rgba<u8> = Rgba([60, 60, 80, 255]);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Playing,
    Paused,
    Stopped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Playing => "再生中",
            Self::Paused => "一時停止中",
            Self::Stopped => "停止中",
        }
    }

    fn color(self) -> Rgba<u8> {
        match self {
            Self::Playing => ACCENT,
            Self::Paused => AMBER,
            Self::Stopped => DIM,
        }
    }
}

pub struct Card {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The cover, from [`thumbnail`].
    pub cover: Option<Arc<RgbaImage>>,
    pub state: State,
    pub position: f64,
    pub duration: Option<f64>,
}

struct Fonts {
    text: FontVec,
    bold: FontVec,
}

/// Japanese-capable faces from fontconfig: Medium for text, Bold for the title. Loaded once.
fn fonts() -> Option<&'static Fonts> {
    static FONTS: OnceLock<Option<Fonts>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let load = |pattern: &str, weight: f32| {
                let (path, index) = crate::fonts::fc_match(pattern)?;
                let bytes = std::fs::read(&path).map_err(|e| log::warn!("{}: {e}", path.display())).ok()?;
                let mut font = FontVec::try_from_vec_and_index(bytes, index).ok()?;
                // Picks the weight of a variable font; does nothing to others.
                font.set_variation(b"wght", weight);
                Some(font)
            };
            let text = load("sans-serif:lang=ja:weight=medium", 500.0)?;
            let bold = load("sans-serif:lang=ja:weight=bold", 700.0)?;
            Some(Fonts { text, bold })
        })
        .as_ref()
}

/// An encoded cover (embedded picture or a file next to the track) cropped and scaled for the
/// card. Done once per track, so the card can be redrawn as the position moves.
pub fn thumbnail(encoded: &[u8]) -> Option<RgbaImage> {
    let cover = image::load_from_memory(encoded).map_err(|e| log::debug!("cover: {e}")).ok()?;
    Some(cover.resize_to_fill(THUMB, THUMB, image::imageops::FilterType::Lanczos3).to_rgba8())
}

/// Draws the card; `None` when there's no usable font.
pub fn render(c: &Card) -> Option<RgbaImage> {
    let f = fonts().or_else(|| {
        log::warn!("no Japanese font for the Now Playing card");
        None
    })?;
    let mut img = RgbaImage::from_pixel(CARD_W, CARD_H, BG);

    let thumb_y = (CARD_H as i32 - THUMB as i32) / 2;
    match &c.cover {
        Some(thumb) => image::imageops::overlay(&mut img, &**thumb, PAD as i64, thumb_y as i64),
        None => {
            fill_rect(&mut img, PAD, thumb_y, THUMB, THUMB, THUMB_BG);
            let size = 72.0;
            let w = text_width(&f.text, size, "♪");
            draw_text(&mut img, &f.text, size, PAD + (THUMB as i32 - w) / 2, thumb_y + (THUMB as i32 - size as i32) / 2 - 8, DIM, "♪");
        }
    }

    let x = PAD + THUMB as i32 + 24;
    let max_w = CARD_W as i32 - x - PAD;

    let color = c.state.color();
    let status_y = 28;
    state_icon(&mut img, c.state, x, status_y + 6, 20, color);
    draw_line(&mut img, &f.text, 26.0, x + 30, status_y, color, c.state.label(), max_w - 30);

    let title = if c.title.is_empty() { "(タイトルなし)" } else { &c.title };
    draw_line(&mut img, &f.bold, 40.0, x, 66, FG, title, max_w);
    if let Some(artist) = c.artist.as_deref().filter(|a| !a.is_empty()) {
        draw_line(&mut img, &f.text, 30.0, x, 120, SUB, artist, max_w);
    }
    if let Some(d) = c.duration.filter(|d| *d > 0.0) {
        progress(&mut img, &f.text, x, 180, max_w, color, c.position.clamp(0.0, d), d);
    }
    if let Some(album) = c.album.as_deref().filter(|a| !a.is_empty()) {
        draw_line(&mut img, &f.text, 24.0, x, 220, DIM, album, max_w);
    }
    Some(img)
}

pub fn png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img.clone()).write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// The bar with the position on its left and the length on its right.
#[allow(clippy::too_many_arguments)]
fn progress(img: &mut RgbaImage, font: &FontVec, x: i32, y: i32, max_w: i32, color: Rgba<u8>, pos: f64, dur: f64) {
    const SIZE: f32 = 18.0;
    const GAP: i32 = 10;
    const BAR_H: u32 = 8;
    let (cur, total) = (fmt_time(pos), fmt_time(dur));
    let (cur_w, total_w) = (text_width(font, SIZE, &cur), text_width(font, SIZE, &total));
    let bar_x = x + cur_w + GAP;
    let bar_w = (max_w - cur_w - total_w - GAP * 2).max(20) as u32;
    fill_rect(img, bar_x, y, bar_w, BAR_H, BAR_BG);
    let filled = (bar_w as f64 * (pos / dur).clamp(0.0, 1.0)) as u32;
    if filled > 0 {
        fill_rect(img, bar_x, y, filled, BAR_H, color);
    }
    // Centred on the bar.
    let text_y = y + BAR_H as i32 / 2 - (SIZE * 0.62) as i32;
    draw_text(img, font, SIZE, x, text_y, SUB, &cur);
    draw_text(img, font, SIZE, x + max_w - total_w, text_y, SUB, &total);
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

/// ▶, ❚❚ or ■ in a `size` square at (x, y).
fn state_icon(img: &mut RgbaImage, state: State, x: i32, y: i32, size: u32, color: Rgba<u8>) {
    let s = size as i32;
    match state {
        State::Playing => {
            // A triangle pointing right, filled row by row (antialiased at its slanted edges).
            for row in 0..s {
                let fy = row as f32 + 0.5;
                let half = (s as f32 / 2.0 - (fy - s as f32 / 2.0).abs()).max(0.0);
                let len = half * 2.0 * 0.9;
                for col in 0..=len.ceil() as i32 {
                    let cover = (len - col as f32).clamp(0.0, 1.0);
                    blend(img, x + 2 + col, y + row, color, cover);
                }
            }
        }
        State::Paused => {
            let w = (size * 3 / 10).max(2);
            fill_rect(img, x + 1, y, w, size, color);
            fill_rect(img, x + s - 1 - w as i32, y, w, size, color);
        }
        State::Stopped => fill_rect(img, x + 1, y + 1, size - 2, size - 2, color),
    }
}

fn fill_rect(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, color: Rgba<u8>) {
    for py in y.max(0)..(y + h as i32).min(img.height() as i32) {
        for px in x.max(0)..(x + w as i32).min(img.width() as i32) {
            img.put_pixel(px as u32, py as u32, color);
        }
    }
}

/// Mixes `color` into the pixel by `cover` (0–1).
fn blend(img: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>, cover: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 || cover <= 0.0 {
        return;
    }
    let a = cover.min(1.0) * color[3] as f32 / 255.0;
    let p = img.get_pixel_mut(x as u32, y as u32);
    for i in 0..3 {
        p[i] = (p[i] as f32 * (1.0 - a) + color[i] as f32 * a).round() as u8;
    }
}

/// Draws `text` with its top at `y`; returns the width.
fn draw_text(img: &mut RgbaImage, font: &FontVec, size: f32, x: i32, y: i32, color: Rgba<u8>, text: &str) -> i32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let baseline = y as f32 + scaled.ascent();
    let mut caret = x as f32;
    let mut prev = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(size, point(caret, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            outline.draw(|gx, gy, c| blend(img, b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, c));
        }
        caret += scaled.h_advance(id);
        prev = Some(id);
    }
    (caret - x as f32).ceil() as i32
}

fn text_width(font: &FontVec, size: f32, text: &str) -> i32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, id);
        }
        w += scaled.h_advance(id);
        prev = Some(id);
    }
    w.ceil() as i32
}

/// One line, cut with "…" when wider than `max_w`.
#[allow(clippy::too_many_arguments)]
fn draw_line(img: &mut RgbaImage, font: &FontVec, size: f32, x: i32, y: i32, color: Rgba<u8>, text: &str, max_w: i32) {
    let text = fit(font, size, text, max_w);
    draw_text(img, font, size, x, y, color, &text);
}

fn fit(font: &FontVec, size: f32, text: &str, max_w: i32) -> String {
    if text_width(font, size, text) <= max_w {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let candidate = format!("{out}{ch}…");
        if text_width(font, size, &candidate) > max_w {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_card() {
        let mut cover = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 48, Rgba([200, 40, 40, 255])))
            .write_to(&mut cover, image::ImageFormat::Png)
            .unwrap();
        let card = Card {
            title: "とても長いタイトルの曲 (feat. 誰か) とても長いタイトルの曲 とても長い".into(),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            cover: thumbnail(&cover.into_inner()).map(Arc::new),
            state: State::Paused,
            position: 61.0,
            duration: Some(245.0),
        };
        let Some(img) = render(&card) else {
            eprintln!("no Japanese font, skipping");
            return;
        };
        assert_eq!(img.dimensions(), (CARD_W, CARD_H));
        // The cover, cropped to fill the square.
        assert_eq!(*img.get_pixel(PAD as u32 + 100, 120), Rgba([200, 40, 40, 255]));
        // Some text was drawn right of the cover, and nothing past the right padding.
        let x0 = PAD as u32 + THUMB + 24;
        let inked = |x: u32, y: u32| *img.get_pixel(x, y) != BG;
        assert!((x0..CARD_W).any(|x| (66..110).any(|y| inked(x, y))));
        assert!(!(CARD_W - PAD as u32 + 2..CARD_W).any(|x| (0..CARD_H).any(|y| inked(x, y))));
        assert!(png(&img).unwrap().starts_with(b"\x89PNG"));
        if let Ok(dir) = std::env::var("NODEPLAYER_CARD_OUT") {
            std::fs::write(format!("{dir}/card.png"), png(&img).unwrap()).unwrap();
        }
    }

    #[test]
    fn long_text_is_cut() {
        let Some(f) = fonts() else { return };
        let s = fit(&f.text, 30.0, &"あ".repeat(100), 200);
        assert!(s.ends_with('…'));
        assert!(text_width(&f.text, 30.0, &s) <= 200);
        assert_eq!(fit(&f.text, 30.0, "short", 200), "short");
    }
}
