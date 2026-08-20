//! The app's own font set.
//!
//! `egui` is declared with `default-features = false`, so it embeds no fonts
//! at all and `FontDefinitions::default()` is `empty()`. Everything the app
//! paints comes from the two faces below.
//!
//! They are the same two files egui would have bundled, copied verbatim and
//! carrying no `FontTweak` — exactly as epaint registers them — so metrics and
//! rendering are unchanged. What is gone is egui's two *emoji* faces,
//! `NotoEmoji-Regular` (418,804) and `emoji-icon-font` (317,864): 736,668 bytes
//! of `.rodata`, a fifth of the section, for four glyphs the UI used and none
//! it needed. Those four are now `↻`, `×` and a colour (see `search_tab` and
//! `manage_tab`).
//!
//! The faces have no CJK, Hebrew, Arabic, Devanagari or Hangul and never did,
//! so filenames in those scripts render as `◻` here just as they always have.
//! Emoji in filenames now join them; that is the one real regression.

use std::sync::Arc;

/// Proportional body text. Ubuntu Font Licence 1.0, unmodified —
/// `assets/fonts/UFL.txt`.
const UBUNTU_LIGHT: &[u8] = include_bytes!("../assets/fonts/Ubuntu-Light.ttf");

/// Monospace: paths, snippets, keys. MIT over public-domain DejaVu over the
/// Bitstream Vera licence — `assets/fonts/Hack-Regular.txt`.
const HACK_REGULAR: &[u8] = include_bytes!("../assets/fonts/Hack-Regular.ttf");

/// Install the two faces on `ctx`.
///
/// `Context::set_fonts` only queues the definitions; they are applied in
/// `begin_pass`, before any user code of that frame runs. So this takes effect
/// on frame 1 wherever it is called, as long as it is called before the first
/// `Context::run`.
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

    // Hack trails Ubuntu-Light in *both* families, which egui's own defaults do
    // not do — there the proportional fallbacks are the two emoji faces. The
    // bytes are linked either way, and it buys two things. It is the only
    // remaining source of `◻`, the replacement glyph epaint reaches for when
    // nothing has the character; without it proportional text falls back to a
    // bare `?`. And it covers Greek, Cyrillic, Armenian, Georgian, arrows and
    // box drawing that Ubuntu-Light lacks, all of which turn up in a filename.
    // Armenian and Georgian names in fact render here for the first time.
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
    /// A tripwire on the Cargo feature, not on this module.
    ///
    /// `builtin_font_names` is `&[]` exactly when `epaint/default_fonts` is
    /// off, so this fails the moment some dependency edge unions the feature
    /// back on and relinks all 1,407,752 bytes of bundled TTFs. Feature
    /// unification is silent and is precisely how they got in.
    /// The coverage contract the UI now depends on, and the proof that
    /// `assert_no_tofu` can fail: the glyphs the tabs paint resolve, and the
    /// emoji that used to come from the two dropped faces do not.
    ///
    /// `↻` and `×` are the two characters the Search tab was moved onto; the
    /// rest are what the status lines and buttons paint. `↻` is the load-bearing
    /// one for *Proportional*: U+21BB is in Hack and not in Ubuntu-Light, so it
    /// passing here is the proof that Hack really is in the proportional
    /// fallback chain — which is what keeps `◻` available and stops epaint
    /// degrading to a bare `?`.
    ///
    /// `◻` itself cannot be asserted: `has_glyph` is
    /// `glyph_info(c) != replacement_glyph`, so the replacement glyph always
    /// reports as missing. That is epaint's own documented quirk, not ours.
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
                // Dropped with NotoEmoji and emoji-icon-font. If these start
                // resolving, the 736,668 bytes are back.
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
