//! Shared UI helpers: emphasis colors, bordered widgets, ignore-pattern
//! validation, and the "more content below" scroll hint.

use quicksearch_core::config::IgnoreSet;

/// Warning/emphasis orange, also used for the fuzzy-edit-distance warning.
pub const ORANGE: egui::Color32 = egui::Color32::from_rgb(220, 150, 40);
/// Emphasis blue for the primary commit controls.
pub const BLUE: egui::Color32 = egui::Color32::from_rgb(90, 150, 250);
/// Border of a pattern editor holding a valid pattern.
pub const VALID_GREEN: egui::Color32 = egui::Color32::from_rgb(80, 180, 100);
/// Border of a pattern editor holding an invalid pattern.
pub const INVALID_RED: egui::Color32 = egui::Color32::from_rgb(220, 80, 80);

/// A standard button with a colored emphasis border.
pub fn bordered_button(
    text: impl Into<egui::WidgetText>,
    color: egui::Color32,
) -> egui::Button<'static> {
    egui::Button::new(text).stroke(egui::Stroke::new(1.5, color))
}

/// Render a section whose widget count changes from frame to frame inside
/// its own child `Ui`.
///
/// egui derives a widget's id from how many widgets precede it in the same
/// `Ui`. A section that emits, say, one label while idle and four rows plus
/// a progress line while working therefore *renames* every widget below it
/// the moment its content changes — and a `DragValue` or `TextEdit` whose
/// id changes loses keyboard focus and whatever the user was typing. A
/// child `Ui` costs the parent exactly one id no matter what goes inside
/// it, so everything below keeps its identity.
///
/// Wrapping does not help the section's *own* widgets: a child `Ui` mixes
/// the parent's counter into its id, so an unstable section cannot be made
/// stable from the inside. Live text and progress belong in a wrapped
/// section; editable fields belong outside one.
pub fn stable_section<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.vertical(contents).inner
}

/// Whether `pattern` is usable as an ignore pattern. `IgnoreSet::compile`
/// silently *skips* patterns that trim to nothing, so emptiness is checked
/// here with the same trimming rules compile applies.
pub fn ignore_pattern_valid(pattern: &str) -> bool {
    let trimmed = pattern.trim().trim_end_matches(['/', '\\']);
    !trimmed.is_empty() && IgnoreSet::compile(&[pattern.to_string()]).is_ok()
}

/// An informational note for a pattern that is valid but likely does not
/// mean what was typed, or `None`. Never an error: everything it fires on
/// compiles and matches exactly as described.
///
/// The dot-leading case fires for `.git` too — "matches only items named
/// exactly `.git`" is both true and the intended behavior there, so the
/// note stays factual rather than guessing intent.
pub fn pattern_hint(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    // ".jpg" is an exact-name pattern, not an extension pattern — the trap
    // behind "my ignore filters don't work" reports.
    if p.len() >= 2 && p.starts_with('.') && !p.contains(['*', '?', '[', '/', '\\']) {
        return Some(format!(
            "Matches only files or folders named exactly \"{p}\". \
             To ignore all {p} files, use \"*{p}\"."
        ));
    }
    // "D:" can only match a component literally named "D:", which nothing
    // ever is; the working spelling keeps the separator.
    let b = p.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some(format!(
            "\"{p}\" never matches anything — use \"{p}\\\" to ignore the whole drive."
        ));
    }
    None
}

/// Render [`pattern_hint`] as a small orange label inside a stable section,
/// so its appearance never shifts the ids of widgets below it.
pub fn pattern_hint_label(ui: &mut egui::Ui, pattern: &str) {
    stable_section(ui, |ui| {
        if let Some(hint) = pattern_hint(pattern) {
            ui.label(egui::RichText::new(hint).small().color(ORANGE));
        }
    });
}

/// Border color for a pattern editor holding `text`, or `None` to keep the
/// theme's own border. A blank box is not wrong yet, just unfilled, so it
/// stays neutral; only text the user actually typed is judged.
fn pattern_border(text: &str) -> Option<egui::Color32> {
    if text.trim().is_empty() {
        None
    } else if ignore_pattern_valid(text) {
        Some(VALID_GREEN)
    } else {
        Some(INVALID_RED)
    }
}

/// Single-line ignore-pattern editor with a green border while the text is
/// a valid pattern, a red one while it is not, and the theme's neutral
/// border while it is empty. Returns the response and the validity of the
/// text as it stands after this frame's edits.
pub fn pattern_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    desired_width: f32,
    hint: &str,
) -> (egui::Response, bool) {
    let mut valid = ignore_pattern_valid(text);
    let border = pattern_border(text);
    let response = ui
        .scope(|ui| {
            // TextEdit frames with widgets.*.bg_stroke when unfocused and
            // selection.stroke when focused; recolor all of them.
            if let Some(color) = border {
                let stroke = egui::Stroke::new(1.0, color);
                let v = ui.visuals_mut();
                v.widgets.inactive.bg_stroke = stroke;
                v.widgets.hovered.bg_stroke = stroke;
                v.widgets.active.bg_stroke = stroke;
                v.selection.stroke = stroke;
            }
            ui.add(
                egui::TextEdit::singleline(text)
                    .desired_width(desired_width)
                    .hint_text(hint),
            )
        })
        .inner;
    if response.changed() {
        // The border catches up next frame; the returned validity is current.
        valid = ignore_pattern_valid(text);
    }
    (response, valid)
}

/// Paint a semitransparent down-arrow near the bottom edge of a scroll
/// area while more content lies below the fold. Painter-only, so it can
/// never swallow clicks. (The bundled fonts have no ▼ glyph — this is a
/// shape, like the sort-header triangles.)
///
/// The hint is painted on the caller's own layer, unclipped: last in that
/// layer, so it sits above the scrolled content, but still below anything
/// stacked over it — a tab's hint stays under the Options window rather
/// than punching through it.
pub fn more_below_hint<R>(ui: &egui::Ui, out: &egui::scroll_area::ScrollAreaOutput<R>) {
    let more_below = out.state.offset.y + out.inner_rect.height() < out.content_size.y - 1.0;
    if !more_below {
        return;
    }
    let painter = ui.ctx().layer_painter(ui.layer_id());
    let cx = out.inner_rect.center().x;
    let tip = out.inner_rect.bottom() - 5.0;
    let (half_width, height) = (7.0, 6.0);
    // A slow opacity pulse so the hint reads as a cue rather than
    // furniture. Repaints are only requested while the arrow is visible,
    // and at a lazy cadence — the fade is too subtle to need 60 fps.
    let t = ui.ctx().input(|i| i.time);
    let pulse = 0.35 + 0.10 * ((t * std::f64::consts::TAU / 2.5).sin() as f32);
    let color = ui.visuals().strong_text_color().gamma_multiply(pulse);
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(50));
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(cx - half_width, tip - height),
            egui::pos2(cx + half_width, tip - height),
            egui::pos2(cx, tip),
        ],
        color,
        egui::Stroke::NONE,
    ));
}

#[cfg(test)]
mod tests {
    use super::{ignore_pattern_valid, pattern_border, pattern_hint, INVALID_RED, VALID_GREEN};

    /// The trap behind "my ignore filters don't work" reports: ".jpg" is an
    /// exact-name pattern, and the hint must say so and offer "*.jpg".
    #[test]
    fn extension_like_patterns_get_a_hint() {
        let hint = pattern_hint(".jpg").expect("hint for .jpg");
        assert!(hint.contains("*.jpg"), "{}", hint);
        assert!(hint.contains("exactly"), "{}", hint);

        // Fires for genuine exact-name patterns too — the statement it
        // makes is just as true for .git, so no allowlist.
        assert!(pattern_hint(".git").is_some());
        assert!(pattern_hint("  .venv  ").is_some(), "trimmed first");

        let targz = pattern_hint(".tar.gz").expect("hint for .tar.gz");
        assert!(targz.contains("*.tar.gz"), "{}", targz);
    }

    #[test]
    fn working_patterns_get_no_hint() {
        for p in [
            "*.jpg",        // the fixed spelling itself
            "node_modules", // plain name
            ".hidden*",     // wildcard: the author knows about globs
            ".[jJ]pg",      // character class counts as a wildcard
            ".git/",        // separator: a path pattern
            r".git\",       // …either flavor
            ".",            // too short to be an extension
            "",
            "   ",
            "D:/",  // working drive-root spelling
            r"D:\", // …either flavor
            "cache-??",
        ] {
            assert_eq!(pattern_hint(p), None, "hinted on {:?}", p);
        }
    }

    #[test]
    fn bare_drive_letters_get_a_hint() {
        let hint = pattern_hint("D:").expect("hint for D:");
        assert!(hint.contains("D:\\"), "{}", hint);
        assert!(pattern_hint("d:").is_some(), "case does not matter");
        assert_eq!(pattern_hint("DD:"), None, "not a drive letter");
        assert_eq!(pattern_hint("4:"), None, "not a drive letter");
    }

    #[test]
    fn blank_patterns_are_invalid() {
        // IgnoreSet::compile would silently skip all of these.
        assert!(!ignore_pattern_valid(""));
        assert!(!ignore_pattern_valid("   "));
        assert!(!ignore_pattern_valid("/"));
        assert!(!ignore_pattern_valid("\\"));
        assert!(!ignore_pattern_valid("  //  "));
    }

    #[test]
    fn malformed_globs_are_invalid() {
        assert!(!ignore_pattern_valid("["));
        assert!(!ignore_pattern_valid("foo[")); // unclosed character class
    }

    #[test]
    fn usual_patterns_are_valid() {
        assert!(ignore_pattern_valid("*.tmp")); // extension
        assert!(ignore_pattern_valid("node_modules")); // name
        assert!(ignore_pattern_valid("Thumbs.db"));
        assert!(ignore_pattern_valid("/home/x/docs/*")); // directory
        assert!(ignore_pattern_valid("C:\\Windows\\Temp\\*"));
        assert!(ignore_pattern_valid("cache-??")); // wildcards
    }

    #[test]
    fn empty_editor_keeps_the_theme_border() {
        // Nothing typed yet is not an error to flag.
        assert_eq!(pattern_border(""), None);
        assert_eq!(pattern_border("   "), None);
        assert_eq!(pattern_border("\t\n"), None);
    }

    #[test]
    fn typed_text_is_judged() {
        assert_eq!(pattern_border("*.tmp"), Some(VALID_GREEN));
        assert_eq!(pattern_border("  node_modules  "), Some(VALID_GREEN));
        assert_eq!(pattern_border("foo["), Some(INVALID_RED));
        // Typed, but trims away to nothing under the pattern rules — still
        // worth flagging, unlike a box the user simply has not filled in.
        assert_eq!(pattern_border("/"), Some(INVALID_RED));
    }
}
