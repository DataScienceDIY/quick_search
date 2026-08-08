//! Shared UI helpers: emphasis colors, bordered widgets, ignore-pattern
//! validation, text eliding, and the "more content below" scroll hint.

use quicksearch_core::config::IgnoreSet;
use std::borrow::Cow;

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

/// Middle-elide `text` so it fits `max_width` pixels when laid out in
/// `font_id`, returning it borrowed and untouched when it already fits.
///
/// A path's two ends are the informative ones — the head says which volume
/// or home it lives under, the tail names the deepest directories — so a
/// column too narrow for the whole thing should drop out of the middle
/// rather than tail-truncate the way egui does by default.
///
/// The budget is in pixels, summed from the font's own glyph advances (the
/// same numbers egui's layout adds up), not a character count scaled by the
/// width of one sample glyph. The proportional body font makes that estimate
/// wrong in both directions: overshoot and egui elides the result a *second*
/// time, painting two ellipses; undershoot and the column sits visibly short
/// of full.
///
/// The borrowed/owned distinction is also the caller's signal that something
/// was dropped, which is what a "full text on hover" tooltip keys off.
pub fn middle_elide<'a>(
    ui: &egui::Ui,
    text: &'a str,
    max_width: f32,
    font_id: &egui::FontId,
) -> Cow<'a, str> {
    ui.fonts(|f| {
        let width_of = |c: char| f.glyph_width(font_id, c);
        if text.chars().map(width_of).sum::<f32>() <= max_width {
            return Cow::Borrowed(text);
        }
        let budget = max_width - width_of('…');

        // Grow a head and a tail toward each other through the middle,
        // each step feeding whichever side is currently narrower so the cut
        // lands near the middle. Indices advance by whole characters, so
        // they always land on UTF-8 boundaries.
        let (mut head, mut tail) = (0usize, text.len());
        let (mut head_w, mut tail_w) = (0.0f32, 0.0f32);
        while head < tail {
            let rest = &text[head..tail];
            let front = rest.chars().next().expect("head < tail");
            let back = rest.chars().next_back().expect("head < tail");
            let (front_w, back_w) = (width_of(front), width_of(back));
            let used = head_w + tail_w;
            let (front_fits, back_fits) = (used + front_w <= budget, used + back_w <= budget);
            if !front_fits && !back_fits {
                break;
            }
            // The preferred side wins when it fits; otherwise the other one
            // does, since at least one of them just did.
            let take_front = if head_w <= tail_w {
                front_fits
            } else {
                !back_fits
            };
            if take_front {
                head += front.len_utf8();
                head_w += front_w;
            } else {
                tail -= back.len_utf8();
                tail_w += back_w;
            }
        }
        if head >= tail {
            // The two halves met without dropping anything — splicing an
            // ellipsis in now would only lengthen a string that fits.
            return Cow::Borrowed(text);
        }

        let mut out = String::with_capacity(head + '…'.len_utf8() + (text.len() - tail));
        out.push_str(&text[..head]);
        out.push('…');
        out.push_str(&text[tail..]);
        Cow::Owned(out)
    })
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

/// Height of the wipe's soft edge, as a fraction of the section it travels
/// over — a proportional band so the transition reads the same on a tall
/// window as on a short one.
const WIPE_BAND: f32 = 0.45;
/// …but never thinner than this, so a two-row table still gets a gradient
/// rather than a hard cut.
const WIPE_BAND_MIN: f32 = 24.0;

/// The two y-coordinates a wipe's scrim ramps between: fully clear at and
/// above the first, fully opaque at and below the second.
///
/// `wipe` is 1 when the section is entirely covered and 0 when it is
/// entirely on screen, and the edge travels monotonically with it — so
/// walking `wipe` down from 1 slides the covered region off the bottom of
/// the section, uncovering the top first.
fn wipe_edges(rect: egui::Rect, wipe: f32) -> (f32, f32) {
    let band = (rect.height() * WIPE_BAND).max(WIPE_BAND_MIN);
    let covered = rect.bottom() + band - wipe * (rect.height() + band);
    (covered - band, covered)
}

/// The scrim hiding the `wipe` of `rect` not yet revealed: a vertical
/// gradient from transparent to solid `fill`, or `None` once nothing is
/// covered.
///
/// Painting the background colour over the results is equivalent to fading
/// them into it, and unlike a per-row opacity it reaches the parts of an
/// `egui_extras` table that the caller never gets a `Ui` for — the stripes,
/// the selection fill, the scroll bar.
pub fn wipe_mesh(rect: egui::Rect, wipe: f32, fill: egui::Color32) -> Option<egui::Mesh> {
    if wipe <= 0.0 || rect.height() <= 0.0 || rect.width() <= 0.0 {
        return None;
    }
    let (clear, covered) = wipe_edges(rect, wipe);
    let alpha_at = |y: f32| ((y - clear) / (covered - clear)).clamp(0.0, 1.0);

    // The gradient is linear between the two edges and flat outside them, so
    // the quads only have to break where an edge falls inside the rect.
    let mut stops = vec![rect.top(), rect.bottom()];
    stops.extend(
        [clear, covered]
            .into_iter()
            .filter(|&y| rect.y_range().contains(y)),
    );
    stops.sort_by(f32::total_cmp);

    let mut mesh = egui::Mesh::default();
    for pair in stops.windows(2) {
        let (top, bottom) = (pair[0], pair[1]);
        let (a_top, a_bottom) = (alpha_at(top), alpha_at(bottom));
        // Sub-point slivers and the still-clear stretch above the edge would
        // contribute nothing but vertices.
        if bottom - top < 0.5 || (a_top <= 0.0 && a_bottom <= 0.0) {
            continue;
        }
        let base = mesh.vertices.len() as u32;
        for (y, alpha) in [(top, a_top), (bottom, a_bottom)] {
            // Mesh vertices carry premultiplied colors, which is exactly
            // what scaling an opaque one by `gamma_multiply` produces.
            let color = fill.gamma_multiply(alpha);
            for x in [rect.left(), rect.right()] {
                mesh.colored_vertex(egui::pos2(x, y), color);
            }
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 1, base + 2, base + 3);
    }
    (!mesh.is_empty()).then_some(mesh)
}

/// Paint [`wipe_mesh`] over `rect` in the panel's own background color.
///
/// Drawn through the layer painter, like [`more_below_hint`]: last in the
/// caller's layer so it covers the content painted before it, and — the
/// reason it cannot use `ui.painter()` — outside the section-wide opacity
/// the caller has already set, which would otherwise scale the scrim along
/// with what it is meant to hide.
pub fn wipe_scrim(ui: &egui::Ui, rect: egui::Rect, wipe: f32) {
    let Some(mesh) = wipe_mesh(rect, wipe, ui.visuals().panel_fill) else {
        return;
    };
    ui.ctx()
        .layer_painter(ui.layer_id())
        .add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::{
        ignore_pattern_valid, middle_elide, pattern_border, pattern_hint, wipe_mesh, Cow,
        INVALID_RED, VALID_GREEN, WIPE_BAND_MIN,
    };
    use crate::test_ui::with_ui;

    fn body_font(ui: &egui::Ui) -> egui::FontId {
        egui::TextStyle::Body.resolve(ui.style())
    }

    const DEEP: &str = "/media/shared/QuickSearch/crates/quicksearch-gui/src/search_tab.rs";

    #[test]
    fn text_that_fits_comes_back_untouched() {
        with_ui(|ui| {
            let font = body_font(ui);
            let out = middle_elide(ui, DEEP, 10_000.0, &font);
            assert!(matches!(out, Cow::Borrowed(_)), "borrowed when it fits");
            assert_eq!(out, DEEP);
            // Nothing to elide, however little room there is.
            assert!(matches!(middle_elide(ui, "", 0.0, &font), Cow::Borrowed(_)));
        });
    }

    #[test]
    fn eliding_keeps_both_ends_and_cuts_once() {
        with_ui(|ui| {
            let out = middle_elide(ui, DEEP, 200.0, &body_font(ui));
            assert!(matches!(out, Cow::Owned(_)), "{out}");
            assert_eq!(out.matches('…').count(), 1, "{out}");
            let (head, tail) = out.split_once('…').expect("one ellipsis");
            assert!(
                !head.is_empty() && !tail.is_empty(),
                "both ends survive: {out}"
            );
            assert!(DEEP.starts_with(head), "{out}");
            assert!(DEEP.ends_with(tail), "{out}");
            assert!(
                tail.ends_with("search_tab.rs"),
                "the filename survives: {out}"
            );
        });
    }

    /// The load-bearing property. If the result laid out in the same font
    /// were wider than the budget, egui would elide it a *second* time and
    /// paint two ellipses.
    #[test]
    fn the_result_fits_the_budget_it_was_given() {
        with_ui(|ui| {
            let font = body_font(ui);
            for width in [40.0f32, 60.0, 121.5, 200.0, 337.5, 480.0] {
                let out = middle_elide(ui, DEEP, width, &font);
                let painted = ui.fonts(|f| {
                    f.layout_no_wrap(out.to_string(), font.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                });
                assert!(
                    painted <= width,
                    "at {width}: {out:?} lays out at {painted}"
                );
            }
        });
    }

    /// An ellipsis is the least that can stand for the text; a column too
    /// narrow even for that gets it anyway rather than a panic.
    #[test]
    fn degenerate_widths_never_panic() {
        with_ui(|ui| {
            let font = body_font(ui);
            for width in [f32::NEG_INFINITY, -50.0, 0.0] {
                assert_eq!(middle_elide(ui, DEEP, width, &font), "…", "at {width}");
            }
        });
    }

    /// Indices walk by whole characters, so a path of multi-byte glyphs
    /// slices cleanly instead of panicking mid-codepoint.
    #[test]
    fn multi_byte_paths_split_on_character_boundaries() {
        with_ui(|ui| {
            let font = body_font(ui);
            let path = "/srv/données/日本語/архив/файл-très-long.txt";
            for width in [30.0f32, 55.0, 90.0, 140.0, 210.0, 400.0] {
                let out = middle_elide(ui, path, width, &font);
                let (head, tail) = out.split_once('…').unwrap_or((out.as_ref(), ""));
                assert!(path.starts_with(head), "at {width}: {out}");
                assert!(path.ends_with(tail), "at {width}: {out}");
            }
        });
    }

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

    // --- The results wipe ---------------------------------------------------

    const SECTION: egui::Rect = egui::Rect {
        min: egui::pos2(10.0, 100.0),
        max: egui::pos2(410.0, 500.0),
    };
    const FILL: egui::Color32 = egui::Color32::from_rgb(27, 27, 27);

    /// Every vertex of `mesh` as (y, alpha), in paint order.
    fn ramp(mesh: &egui::Mesh) -> Vec<(f32, u8)> {
        mesh.vertices
            .iter()
            .map(|v| (v.pos.y, v.color.a()))
            .collect()
    }

    /// The y ranges the mesh's quads cover, merged where they touch.
    fn covered_spans(mesh: &egui::Mesh) -> Vec<(f32, f32)> {
        let mut spans: Vec<(f32, f32)> = Vec::new();
        for quad in mesh.vertices.chunks(4) {
            let (top, bottom) = (quad[0].pos.y, quad[3].pos.y);
            match spans.last_mut() {
                Some(last) if (last.1 - top).abs() < 1e-3 => last.1 = bottom,
                _ => spans.push((top, bottom)),
            }
        }
        spans
    }

    #[test]
    fn a_revealed_section_paints_no_scrim() {
        // The steady state is the common one: no shape, no vertices, no cost.
        assert!(wipe_mesh(SECTION, 0.0, FILL).is_none());
        assert!(wipe_mesh(SECTION, -0.5, FILL).is_none());
    }

    #[test]
    fn a_degenerate_section_paints_no_scrim() {
        let flat = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(410.0, 100.0));
        assert!(wipe_mesh(flat, 0.5, FILL).is_none());
        let sliver = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(10.0, 500.0));
        assert!(wipe_mesh(sliver, 0.5, FILL).is_none());
    }

    #[test]
    fn an_unstarted_wipe_covers_the_whole_section() {
        let mesh = wipe_mesh(SECTION, 1.0, FILL).expect("fully hidden");
        assert!(
            ramp(&mesh).iter().all(|&(_, a)| a == 255),
            "nothing may show through before the reveal starts: {:?}",
            ramp(&mesh)
        );
        assert_eq!(
            covered_spans(&mesh),
            vec![(SECTION.top(), SECTION.bottom())],
            "the quads must tile the section with no gap"
        );
    }

    #[test]
    fn the_scrim_ramps_clear_at_the_top_to_solid_at_the_bottom() {
        let mesh = wipe_mesh(SECTION, 0.5, FILL).expect("mid-travel");
        let ramp = ramp(&mesh);
        assert_eq!(ramp.first().expect("vertices").1, 0, "the top is untouched");
        assert_eq!(ramp.last().expect("vertices").1, 255, "the bottom is gone");
        // Alpha only ever increases downward, and the quads stay contiguous:
        // a gradient, not a stack of steps.
        for pair in ramp.windows(2) {
            assert!(
                pair[1].0 >= pair[0].0 && pair[1].1 >= pair[0].1,
                "vertices run down the section, clear to opaque: {ramp:?}"
            );
        }
        assert_eq!(covered_spans(&mesh).len(), 1, "one contiguous scrim");
    }

    #[test]
    fn less_of_the_section_shows_the_further_the_wipe_is_from_done() {
        // How much of the section's height the scrim swallows: the integral
        // of alpha down it, so a widening gradient counts as well as a
        // growing solid block.
        let hidden_height = |wipe: f32| {
            let mesh = wipe_mesh(SECTION, wipe, FILL).expect("travelling");
            mesh.vertices
                .chunks(4)
                .map(|quad| {
                    let alpha = |v: &egui::epaint::Vertex| v.color.a() as f32 / 255.0;
                    (quad[3].pos.y - quad[0].pos.y) * (alpha(&quad[0]) + alpha(&quad[3])) / 2.0
                })
                .sum::<f32>()
        };
        let mut previous = 0.0;
        for step in 1..=10 {
            let hidden = hidden_height(step as f32 / 10.0);
            assert!(
                hidden > previous,
                "each step hides more than the last: {hidden} after {previous}"
            );
            previous = hidden;
        }
        assert!(
            (previous - SECTION.height()).abs() < 0.5,
            "and the section is entirely gone by the end: {previous} of {}",
            SECTION.height()
        );
    }

    #[test]
    fn a_short_section_still_gets_a_gradient() {
        // Two rows tall: the proportional band would be a few points, small
        // enough to read as a hard cut, so the floor takes over.
        let short = egui::Rect::from_min_max(egui::pos2(10.0, 100.0), egui::pos2(410.0, 130.0));
        let mesh = wipe_mesh(short, 0.5, FILL).expect("mid-travel");
        let gradient = mesh
            .vertices
            .chunks(4)
            .any(|quad| quad[0].color.a() < quad[3].color.a());
        assert!(
            gradient,
            "the edge ramps rather than cutting: {:?}",
            ramp(&mesh)
        );
        let (clear, covered) = super::wipe_edges(short, 0.5);
        assert!(
            (covered - clear - WIPE_BAND_MIN).abs() < 1e-3,
            "the band is held at its floor: {}",
            covered - clear
        );
    }
}
