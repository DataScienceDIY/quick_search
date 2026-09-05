//! The app's own font set. `egui` is built with `default-features = false`,
//! so it embeds no fonts; the two faces below are the same files egui would
//! have bundled, unmodified. Dropped: egui's two emoji faces — 736,668 bytes
//! of `.rodata` for four glyphs, now `↻`, `×` and a colour. The faces have
//! no CJK/Hebrew/Arabic and never did; emoji in filenames now render `◻`
//! too, the one real regression.

use std::sync::Arc;

/// Proportional body text. Ubuntu Font Licence 1.0 — `assets/fonts/UFL.txt`.
const UBUNTU_LIGHT: &[u8] = include_bytes!("../assets/fonts/Ubuntu-Light.ttf");

/// Monospace. MIT/DejaVu/Bitstream Vera — `assets/fonts/Hack-Regular.txt`.
const HACK_REGULAR: &[u8] = include_bytes!("../assets/fonts/Hack-Regular.ttf");

/// Install the two faces on `ctx`. `set_fonts` only queues; applied in
/// `begin_pass`, so it takes effect on frame 1 if called before the first run.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();

    fonts.font_data.insert(
        "Ubuntu-Light".to_owned(),
        Arc::new(egui::FontData::from_static(UBUNTU_LIGHT)),
    );
    fonts.font_data.insert(
        "Hack".to_owned(),
        Arc::new(egui::FontData::from_static(HACK_REGULAR)),
    );

    // Hack trails Ubuntu-Light in *both* families: it is the only remaining
    // source of `◻` (without it proportional text degrades to a bare `?`),
    // and it covers Greek, Cyrillic, arrows and box drawing that
    // Ubuntu-Light lacks, all of which turn up in filenames.
    fonts.families.insert(
        egui::FontFamily::Proportional,
        vec!["Ubuntu-Light".to_owned(), "Hack".to_owned()],
    );
    // Ubuntu-Light second, as epaint has it: "fallback for √ etc".
    fonts.families.insert(
        egui::FontFamily::Monospace,
        vec!["Hack".to_owned(), "Ubuntu-Light".to_owned()],
    );

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    /// The coverage contract the UI depends on. `↻` is the load-bearing one
    /// for Proportional: U+21BB is in Hack and not Ubuntu-Light, so it
    /// passing proves Hack really is in the proportional fallback chain.
    /// `◻` itself cannot be asserted: `has_glyph` reports the replacement
    /// glyph as missing, epaint's own documented quirk.
    #[test]
    fn the_installed_faces_cover_what_the_ui_paints_and_no_more() {
        let ctx = crate::test_ui::ctx();
        // `Context::fonts` panics until the first pass has run.
        let _ = ctx.run(egui::RawInput::default(), |_| {});

        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let id = egui::FontId::new(14.0, family.clone());
            ctx.fonts(|fonts| {
                for c in ['↻', '×', '·', '…', '•', '−', '°', 'é', 'Ω', 'д'] {
                    assert!(fonts.has_glyph(&id, c), "{family:?} lost {c:?}");
                }
                // If these start resolving, the 736,668 emoji bytes are back.
                for c in ['⟳', '🗙', '⚠', '🔥', '📋'] {
                    assert!(!fonts.has_glyph(&id, c), "{family:?} still has {c:?}");
                }
            });
        }
    }

    #[test]
    fn egui_bundles_no_fonts_of_its_own() {
        assert!(
            egui::FontDefinitions::builtin_font_names().is_empty(),
            "egui's `default_fonts` feature is back on: {:?}",
            egui::FontDefinitions::builtin_font_names()
        );
    }
}
