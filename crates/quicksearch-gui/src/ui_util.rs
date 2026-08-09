//! Shared UI helpers: bordered widgets, ignore-pattern validation, text
//! eliding, and the "more content below" scroll hint. The colors they paint
//! with live in [`crate::color`].

use quicksearch_core::config::IgnoreSet;
use std::borrow::Cow;

use crate::color::palette;

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
/// `Ui`, so a section whose content changes *renames* every widget below it
/// — and a `DragValue` or `TextEdit` whose id changes loses keyboard focus
/// and in-progress edits. A child `Ui` costs the parent exactly one id no
/// matter what goes inside it. Wrapping does not help the section's *own*
/// widgets: a child `Ui` mixes the parent's counter into its id.
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
    let caution = palette(ui.visuals().dark_mode).orange;
    stable_section(ui, |ui| {
        if let Some(hint) = pattern_hint(pattern) {
            ui.label(hint_colored(hint, caution));
        }
    });
}

/// Border color for a pattern editor holding `text`, or `None` to keep the
/// theme's own border (a blank box stays neutral).
fn pattern_border(text: &str, dark_mode: bool) -> Option<egui::Color32> {
    let p = palette(dark_mode);
    if text.trim().is_empty() {
        None
    } else if ignore_pattern_valid(text) {
        Some(p.green)
    } else {
        Some(p.red)
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
    let border = pattern_border(text, ui.visuals().dark_mode);
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
/// The budget is in pixels, summed from the font's own glyph advances (the
/// same numbers egui's layout adds up), not a character count scaled by one
/// sample glyph — a proportional font makes that estimate wrong in both
/// directions: overshoot and egui elides the result a *second* time,
/// painting two ellipses; undershoot and the column sits visibly short.
///
/// The borrowed/owned distinction is the caller's signal that something was
/// dropped, which is what a "full text on hover" tooltip keys off.
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

        // Grow a head and a tail toward each other, each step feeding
        // whichever side is currently narrower. Indices advance by whole
        // characters, so they always land on UTF-8 boundaries.
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
            // The two halves met without dropping anything.
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
/// never swallow clicks; the bundled fonts have no ▼ glyph, so it is a
/// shape. Painted last in the caller's own layer: above the scrolled
/// content, still below anything stacked over it.
pub fn more_below_hint<R>(ui: &egui::Ui, out: &egui::scroll_area::ScrollAreaOutput<R>) {
    let more_below = out.state.offset.y + out.inner_rect.height() < out.content_size.y - 1.0;
    if !more_below {
        return;
    }
    let painter = ui.ctx().layer_painter(ui.layer_id());
    let cx = out.inner_rect.center().x;
    let tip = out.inner_rect.bottom() - 5.0;
    let (half_width, height) = (7.0, 6.0);
    // A slow opacity pulse; repaints only while visible, at a lazy cadence.
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
/// above the first, fully opaque at and below the second. `wipe` is 1 when
/// the section is entirely covered, 0 when it is entirely on screen;
/// walking it down uncovers the top first.
fn wipe_edges(rect: egui::Rect, wipe: f32) -> (f32, f32) {
    let band = (rect.height() * WIPE_BAND).max(WIPE_BAND_MIN);
    let covered = rect.bottom() + band - wipe * (rect.height() + band);
    (covered - band, covered)
}

/// The scrim hiding the `wipe` of `rect` not yet revealed: a vertical
/// gradient from transparent to solid `fill`, or `None` once nothing is
/// covered. Unlike a per-row opacity it reaches the parts of an
/// `egui_extras` table the caller never gets a `Ui` for — the stripes, the
/// selection fill, the scroll bar.
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
/// Drawn through the layer painter, not `ui.painter()`: the section-wide
/// opacity the caller has set would otherwise scale the scrim along with
/// what it is meant to hide.
pub fn wipe_scrim(ui: &egui::Ui, rect: egui::Rect, wipe: f32) {
    let Some(mesh) = wipe_mesh(rect, wipe, ui.visuals().panel_fill) else {
        return;
    };
    ui.ctx()
        .layer_painter(ui.layer_id())
        .add(egui::Shape::mesh(mesh));
}

/// De-emphasized annotation text: the `.small().weak()` styling every
/// hint and caption in the app uses.
pub fn hint(text: impl Into<egui::RichText>) -> egui::RichText {
    text.into().small().weak()
}

/// Small colored annotation (warnings, "Unsaved changes"): small + color,
/// not weak.
pub fn hint_colored(text: impl Into<egui::RichText>, color: egui::Color32) -> egui::RichText {
    text.into().small().color(color)
}

/// The centered, non-collapsible, non-resizable window every confirmation
/// prompt shares. Returns the closure's value; `None` if egui skipped the
/// window this frame. Width is the body's to set (`ui.set_max_width`).
pub fn centered_modal<R>(
    ctx: &egui::Context,
    title: &str,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, contents)
        .and_then(|r| r.inner)
}

/// Determinate bar, or the animated indeterminate bar when the work has no
/// denominator yet.
pub fn progress_bar(ui: &mut egui::Ui, fraction: Option<f32>, width: f32) {
    let bar = match fraction {
        Some(frac) => egui::ProgressBar::new(frac),
        None => egui::ProgressBar::new(0.0).animate(true),
    };
    ui.add(bar.desired_width(width));
}

#[cfg(test)]
mod tests;
