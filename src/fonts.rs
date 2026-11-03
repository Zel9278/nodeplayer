//! Fonts: the desktop's own sans-serif and monospace faces (from fontconfig; the system's
//! standard ones on Windows and macOS) in place of egui's thin bundled Ubuntu Light, which
//! looks blurry at UI sizes; egui's emoji and icon fonts after them; and a Japanese face last,
//! since egui's fonts have no CJK glyphs.

use std::path::{Path, PathBuf};

use egui::{FontData, FontDefinitions, FontFamily};

/// Used when `fc-match` isn't available.
const KNOWN: &[(&str, u32, u32)] = &[
    ("/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf", 0, 0),
];

/// Font file and face index fontconfig picks for `pattern`. Without fontconfig (Windows,
/// macOS), the first installed of the system's standard faces for it.
pub(crate) fn fc_match(pattern: &str) -> Option<(PathBuf, u32)> {
    if cfg!(any(windows, target_os = "macos")) {
        return system_font(pattern);
    }
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}|%{index}", pattern]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let (file, index) = text.split_once('|')?;
    Some((PathBuf::from(file), index.trim().parse().ok()?))
}

/// The fontconfig patterns nodeplayer uses, answered from the faces Windows and macOS ship.
fn system_font(pattern: &str) -> Option<(PathBuf, u32)> {
    let ja = pattern.contains("lang=ja");
    let mono = pattern.starts_with("monospace");
    let bold = pattern.contains("weight=bold");
    let candidates: &[(&str, u32)] = if cfg!(windows) {
        match (ja, mono, bold) {
            (false, false, _) => &[("segoeui.ttf", 0)],
            (false, true, _) => &[("consola.ttf", 0), ("cour.ttf", 0)],
            // Yu Gothic (Windows 8.1+), else Meiryo.
            (true, false, false) => &[("YuGothM.ttc", 0), ("meiryo.ttc", 0), ("msgothic.ttc", 1)],
            (true, false, true) => &[("YuGothB.ttc", 0), ("meiryob.ttc", 0), ("msgothic.ttc", 1)],
            (true, true, _) => &[("msgothic.ttc", 0)],
        }
    } else {
        match (ja, mono, bold) {
            (false, false, _) => &[("Helvetica.ttc", 0)],
            (false, true, _) => &[("Menlo.ttc", 0), ("Courier.ttc", 0)],
            (true, _, false) => &[("ヒラギノ角ゴシック W4.ttc", 0), ("Hiragino Sans GB.ttc", 0)],
            (true, _, true) => &[("ヒラギノ角ゴシック W6.ttc", 0), ("ヒラギノ角ゴシック W4.ttc", 0)],
        }
    };
    let dirs: Vec<PathBuf> = if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
        vec![windir.join("Fonts")]
    } else {
        vec!["/System/Library/Fonts".into(), "/Library/Fonts".into()]
    };
    candidates
        .iter()
        .flat_map(|&(file, index)| dirs.iter().map(move |d| (d.join(file), index)))
        .find(|(p, _)| p.is_file())
}

pub fn install(ctx: &egui::Context) {
    // Glyphs drawn at whole pixels: sharper, at the cost of slightly less even spacing.
    ctx.all_styles_mut(|s| s.visuals.text_options.subpixel_binning = false);

    let mut defs = FontDefinitions::default();
    // The CJK faces usually live in one (large) collection file, so read each file once and
    // share it; it's needed for the whole run anyway.
    let mut files: Vec<(PathBuf, &'static [u8])> = Vec::new();
    let mut add = |defs: &mut FontDefinitions, name: &str, (path, index): (PathBuf, u32), family: FontFamily, first: bool| {
        let bytes = match files.iter().find(|(p, _)| *p == path) {
            Some((_, b)) => *b,
            None => match std::fs::read(&path) {
                Ok(b) => {
                    let b: &'static [u8] = Box::leak(b.into_boxed_slice());
                    files.push((path.clone(), b));
                    b
                }
                Err(e) => {
                    log::warn!("can't read {}: {e}", path.display());
                    return;
                }
            },
        };
        log::debug!("{name}: {} #{index}", path.display());
        defs.font_data.insert(name.into(), FontData { index, ..FontData::from_static(bytes) }.into());
        let list = defs.families.entry(family).or_default();
        if first {
            list.insert(0, name.into());
        } else {
            list.push(name.into());
        }
    };

    if let Some(f) = fc_match("sans-serif") {
        add(&mut defs, "ui-sans", f, FontFamily::Proportional, true);
    }
    if let Some(f) = fc_match("monospace") {
        add(&mut defs, "ui-mono", f, FontFamily::Monospace, true);
    }

    let sans = fc_match("sans-serif:lang=ja");
    let mono = fc_match("monospace:lang=ja");
    let (sans, mono) = match (sans, mono) {
        (Some(s), Some(m)) => (s, m),
        _ => match KNOWN.iter().find(|(p, ..)| Path::new(p).exists()) {
            Some(&(p, s, m)) => ((p.into(), s), (p.into(), m)),
            None => {
                log::warn!("no Japanese font found; CJK text will show as boxes");
                ctx.set_fonts(defs);
                return;
            }
        },
    };
    add(&mut defs, "cjk-sans", sans, FontFamily::Proportional, false);
    add(&mut defs, "cjk-mono", mono, FontFamily::Monospace, false);
    align_baseline(&mut defs, &FontFamily::Proportional, "cjk-sans");
    align_baseline(&mut defs, &FontFamily::Monospace, "cjk-mono");
    ctx.set_fonts(defs);
}

/// egui places a glyph from a fallback face by centring that face's line box on the primary
/// face's, not by matching baselines. A face whose ascent and descent are balanced differently
/// (Yu Gothic next to Segoe UI, say) then sits above or below the Latin text around it; shift
/// it down by the difference so the baselines meet.
fn align_baseline(defs: &mut FontDefinitions, family: &FontFamily, name: &str) {
    let Some(primary) = defs.families.get(family).and_then(|list| list.first()) else {
        return;
    };
    let (Some(p), Some(f)) = (
        defs.font_data.get(primary).and_then(|d| vertical_metrics(d)),
        defs.font_data.get(name).and_then(|d| vertical_metrics(d)),
    ) else {
        return;
    };
    let shift = baseline_shift(p, f);
    log::debug!("{name}: baseline shifted by {shift:.3} em");
    if let Some(data) = defs.font_data.get_mut(name) {
        std::sync::Arc::make_mut(data).tweak.y_offset_factor = shift;
    }
}

/// Ascent and line height (ascent + descent + line gap) in ems, as egui reads them.
fn vertical_metrics(data: &FontData) -> Option<(f32, f32)> {
    use skrifa::MetadataProvider;
    let font = skrifa::FontRef::from_index(&data.font, data.index).ok()?;
    let m = font.metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
    let em = f32::from(m.units_per_em);
    (em > 0.0).then(|| (m.ascent / em, (m.ascent - m.descent + m.leading) / em))
}

/// How far down (in ems) to move a fallback face so its baseline meets the primary face's.
/// egui puts the fallback baseline at `its ascent + (primary height - its height) / 2`.
fn baseline_shift((p_ascent, p_height): (f32, f32), (f_ascent, f_height): (f32, f32)) -> f32 {
    p_ascent - (f_ascent + 0.5 * (p_height - f_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_lands_the_fallback_baseline_on_the_primary_one() {
        // Segoe UI-like primary; a CJK face with a tall line box and a shallower ascent.
        let primary = (1.08, 1.33);
        let fallback = (0.88, 1.32);
        let shift = baseline_shift(primary, fallback);
        let baseline = fallback.0 + 0.5 * (primary.1 - fallback.1) + shift;
        assert!((baseline - primary.0).abs() < 1e-6);
        assert!(shift > 0.0, "a lower ascent in a similar line box must move down");
        assert_eq!(baseline_shift(primary, primary), 0.0);
    }

    #[test]
    fn system_faces_have_readable_metrics() {
        // Only meaningful where the desktop has the faces; skip quietly elsewhere.
        for pattern in ["sans-serif", "sans-serif:lang=ja"] {
            let Some((path, index)) = fc_match(pattern) else { continue };
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let data = FontData { index, ..FontData::from_owned(bytes) };
            let (ascent, height) = vertical_metrics(&data).expect("metrics");
            assert!(ascent > 0.0 && height > ascent, "{pattern}: {ascent} {height}");        }
    }
}
