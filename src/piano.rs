//! The 128 MIDI keys drawn as a piano keyboard, as the MIDI player and Audio → MIDI show what
//! sounds: each key lit in the colour of what plays it (a channel of a track, or of a
//! detector), with the octaves named under their C keys.

use egui::{Color32, Pos2, Rect, Sense, Ui, pos2, vec2};

/// A white key's width and the keys' height, in points: 75 white keys (C-1 to G9).
const WHITE_WIDTH: f32 = 5.0;
const WHITE_KEYS: usize = 75;
const KEYS_HEIGHT: f32 = 34.0;
/// Room under the keys for the octave names.
const LABELS_HEIGHT: f32 = 10.0;
/// A black key's size, against a white key's.
const BLACK_WIDTH: f32 = 0.64;
const BLACK_HEIGHT: f32 = 0.62;

/// A key that sounds: its colour, and what plays it, for the tooltip ("channel 3 · track 2").
pub struct Lit {
    pub color: Color32,
    pub by: String,
}

/// Draws the keyboard; `lit` tells the keys that sound. Hovering a key names it, and what
/// plays it while it sounds.
pub fn keyboard(ui: &mut Ui, lit: impl Fn(usize) -> Option<Lit>) {
    let size = vec2(WHITE_KEYS as f32 * WHITE_WIDTH, KEYS_HEIGHT + LABELS_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let keys = Rect::from_min_size(rect.min, vec2(rect.width(), KEYS_HEIGHT));
    let p = ui.painter_at(rect);
    p.rect_filled(keys, 0.0, Color32::from_gray(20));
    // White keys, then the black ones over them.
    for black in [false, true] {
        for key in (0..128).filter(|&k| is_black(k) == black) {
            let color = match (lit(key), black) {
                (Some(l), false) => l.color,
                // Lit black keys a little darker, so they still read as black keys.
                (Some(l), true) => l.color.lerp_to_gamma(Color32::BLACK, 0.25),
                (None, false) => Color32::from_gray(225),
                (None, true) => Color32::from_gray(28),
            };
            p.rect_filled(key_rect(keys, key), if black { 1.0 } else { 0.0 }, color);
        }
    }
    for c in (0..128).step_by(12) {
        let at = pos2(key_rect(keys, c).left(), keys.bottom() + 1.0);
        p.text(at, egui::Align2::LEFT_TOP, key_name(c), egui::FontId::proportional(8.0), Color32::from_gray(120));
    }
    if let Some(key) = response.hover_pos().and_then(|pos| key_at(keys, pos)) {
        let text = match lit(key) {
            Some(l) => format!("{} · {}", key_name(key), l.by),
            None => key_name(key),
        };
        response.on_hover_text_at_pointer(text);
    }
}

/// The colour of notes from `channel` of `source` (a file's track, or a detector): each pair
/// its own hue, spread by the golden ratio so that neighbouring channels and sources differ.
pub fn note_color(channel: u8, source: u32) -> Color32 {
    let index = f64::from(source) * 16.0 + f64::from(channel);
    let hue = (index * 0.618_033_988_75).fract() as f32;
    egui::ecolor::Hsva::new(hue, 0.72, 1.0, 1.0).into()
}

fn is_black(key: usize) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

/// White keys left of `key`; for a black key, that's the white key right of it.
fn whites_before(key: usize) -> usize {
    const BEFORE: [usize; 12] = [0, 1, 1, 2, 2, 3, 4, 4, 5, 5, 6, 6];
    key / 12 * 7 + BEFORE[key % 12]
}

/// Where `key` is drawn: a white key fills its column (less a gap), a black key straddles the
/// line between the two white keys around it, from the top.
fn key_rect(keys: Rect, key: usize) -> Rect {
    let x = keys.left() + whites_before(key) as f32 * WHITE_WIDTH;
    if is_black(key) {
        let w = WHITE_WIDTH * BLACK_WIDTH;
        Rect::from_min_size(pos2(x - w / 2.0, keys.top()), vec2(w, keys.height() * BLACK_HEIGHT))
    } else {
        Rect::from_min_size(pos2(x, keys.top()), vec2(WHITE_WIDTH - 1.0, keys.height()))
    }
}

/// The key under `pos`: a black key where one covers the white ones.
fn key_at(keys: Rect, pos: Pos2) -> Option<usize> {
    let hit = |key: &usize| key_rect(keys, *key).expand2(vec2(0.5, 0.0)).contains(pos);
    (0..128).filter(|&k| is_black(k)).find(hit).or_else(|| (0..128).filter(|&k| !is_black(k)).find(hit))
}

/// "C4" for key 60, as most players name them. Plain "#": the fonts may not have "♯".
fn key_name(key: usize) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[key % 12], key as i32 / 12 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_is_laid_out_like_a_piano() {
        // 128 keys, C-1 to G9: 75 white keys.
        assert_eq!(whites_before(127) + 1, WHITE_KEYS);
        assert_eq!((0..128).filter(|&k| !is_black(k)).count(), WHITE_KEYS);
        let kb = Rect::from_min_size(pos2(10.0, 0.0), vec2(WHITE_KEYS as f32 * WHITE_WIDTH, KEYS_HEIGHT));
        for key in (0..128).filter(|&k| is_black(k)) {
            let (r, left, right) = (key_rect(kb, key), key_rect(kb, key - 1), key_rect(kb, key + 1));
            // A black key straddles its two white neighbours, shorter than them.
            assert!(r.left() < left.right() && r.right() > right.left(), "key {key}");
            assert!(r.height() < left.height());
            // Where it covers the white keys, it's the one under the pointer.
            assert_eq!(key_at(kb, r.center()), Some(key));
        }
        // Below the black keys, the white key is hit.
        let c4 = key_rect(kb, 60);
        assert_eq!(key_at(kb, pos2(c4.center().x, c4.bottom() - 1.0)), Some(60));
        assert_eq!(key_at(kb, pos2(kb.right() + 5.0, 1.0)), None);
        assert_eq!((key_name(60).as_str(), key_name(0).as_str(), key_name(127).as_str()), ("C4", "C-1", "G9"));
    }

    #[test]
    fn channels_and_sources_get_their_own_colours() {
        let mut seen = std::collections::HashSet::new();
        for source in 0..8 {
            for channel in 0..16 {
                assert!(seen.insert(note_color(channel, source)), "channel {channel} of {source}");
            }
        }
        // The same pair always looks the same, also on a track of black MIDI.
        assert_eq!(note_color(3, 5000), note_color(3, 5000));
        assert_ne!(note_color(0, 0), note_color(1, 0));
        assert_ne!(note_color(0, 0), note_color(0, 1));
    }
}
