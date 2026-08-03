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

/// Whether `pattern` is usable as an ignore pattern. `IgnoreSet::compile`
/// silently *skips* patterns that trim to nothing, so emptiness is checked
/// here with the same trimming rules compile applies.
pub fn ignore_pattern_valid(pattern: &str) -> bool {
    let trimmed = pattern.trim().trim_end_matches(['/', '\\']);
    !trimmed.is_empty() && IgnoreSet::compile(&[pattern.to_string()]).is_ok()
}

/// Single-line ignore-pattern editor with a green border while the text is
/// a valid pattern and a red one otherwise. Returns the response and the
/// validity of the text as it stands after this frame's edits.
pub fn pattern_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    desired_width: f32,
    hint: &str,
) -> (egui::Response, bool) {
    let mut valid = ignore_pattern_valid(text);
    let stroke = egui::Stroke::new(1.0, if valid { VALID_GREEN } else { INVALID_RED });
    let response = ui
        .scope(|ui| {
            // TextEdit frames with widgets.*.bg_stroke when unfocused and
            // selection.stroke when focused; recolor all of them.
            let v = ui.visuals_mut();
            v.widgets.inactive.bg_stroke = stroke;
            v.widgets.hovered.bg_stroke = stroke;
            v.widgets.active.bg_stroke = stroke;
            v.selection.stroke = stroke;
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
/// area while more content lies below the fold. Painter-only on the
/// foreground layer, so it can never swallow clicks. (The bundled fonts
/// have no ▼ glyph — this is a shape, like the sort-header triangles.)
pub fn more_below_hint<R>(ui: &egui::Ui, out: &egui::scroll_area::ScrollAreaOutput<R>) {
    let more_below = out.state.offset.y + out.inner_rect.height() < out.content_size.y - 1.0;
    if !more_below {
        return;
    }
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("qs-more-below-hint"),
    ));
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
    use super::ignore_pattern_valid;

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
}
