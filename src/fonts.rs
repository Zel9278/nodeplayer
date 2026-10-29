//! Fonts: the desktop's own sans-serif and monospace faces (from fontconfig) in place of egui's
//! thin bundled Ubuntu Light, which looks blurry at UI sizes; egui's emoji and icon fonts after
//! them; and a Japanese face last, since egui's fonts have no CJK glyphs.

use std::path::{Path, PathBuf};

use egui::{FontData, FontDefinitions, FontFamily};

/// Used when `fc-match` isn't available.
const KNOWN: &[(&str, u32, u32)] = &[
    ("/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 0, 5),
    ("/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf", 0, 0),
];

/// Font file and face index fontconfig picks for `pattern`.
pub(crate) fn fc_match(pattern: &str) -> Option<(PathBuf, u32)> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}|%{index}", pattern]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let (file, index) = text.split_once('|')?;
    Some((PathBuf::from(file), index.trim().parse().ok()?))
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
    ctx.set_fonts(defs);
}
