//! Shared UI helpers: bordered widgets, ignore-pattern validation, text
//! eliding, and the "more content below" scroll hint.

use quicksearch_core::config::IgnoreSet;
use std::borrow::Cow;

use crate::color::palette;

pub fn bordered_button(
    text: impl Into<egui::WidgetText>,
    color: egui::Color32,
) -> egui::Button<'static> {
    egui::Button::new(text).stroke(egui::Stroke::new(1.5, color))
}

/// Renders a section whose widget count changes from frame to frame inside
/// its own child `Ui`. egui derives a widget's id from how many widgets
/// precede it in the same `Ui`, so a section whose content changes renames
/// every widget below it, and a `DragValue` or `TextEdit` whose id changes
/// loses keyboard focus and in-progress edits. A child `Ui` costs the parent
/// exactly one id, whatever goes inside it.
pub fn stable_section<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.vertical(contents).inner
}

/// Glob matching everything under `dir`, spelled with the platform
/// separator. `Path::join` inserts a separator only where one is needed, so
/// a drive root yields `C:\*` rather than the never-matching `C:\/*` a
/// `format!("{}/*")` would produce. Shared by the search tab's ignore dialog
/// and the duplicates tab's exclusions, which speak the same glob syntax.
pub fn dir_ignore_pattern(dir: &std::path::Path) -> String {
    dir.join("*").to_string_lossy().into_owned()
}

/// `IgnoreSet::compile` silently *skips* patterns that trim to nothing, so
/// emptiness is checked here with the same trimming rules.
pub fn ignore_pattern_valid(pattern: &str) -> bool {
    let trimmed = pattern.trim().trim_end_matches(['/', '\\']);
    !trimmed.is_empty() && IgnoreSet::compile(&[pattern.to_string()]).is_ok()
}

/// A note for a valid pattern that likely does not mean what was typed.
pub fn pattern_hint(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    // ".jpg" is an exact-name pattern, not an extension pattern.
    if p.len() >= 2 && p.starts_with('.') && !p.contains(['*', '?', '[', '/', '\\']) {
        return Some(format!(
            "Matches only files or folders named exactly \"{p}\". \
             To ignore all {p} files, use \"*{p}\"."
        ));
    }
    // "D:" can only match a component literally named "D:", which nothing is.
    if matches!(p.as_bytes(), [d, b':'] if d.is_ascii_alphabetic()) {
        return Some(format!(
            "\"{p}\" never matches anything; use \"{p}\\\" to ignore the whole drive."
        ));
    }
    None
}

/// [`pattern_hint`] in the caution colour, inside a [`stable_section`].
pub fn pattern_hint_label(ui: &mut egui::Ui, pattern: &str) {
    let caution = palette(ui.visuals().dark_mode).orange;
    stable_section(ui, |ui| {
        if let Some(hint) = pattern_hint(pattern) {
            ui.label(hint_colored(hint, caution));
        }
    });
}

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

/// Ignore-pattern editor: green border while valid, red while not, neutral
/// while empty. Returns the response and the post-edit validity.
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

/// Grows a left and a right side one character at a time, the narrower side
/// first (the left on a tie), until neither side's next character fits in
/// `budget` pixels on top of `fixed`. `next(l, r)` gives the characters the
/// sides would take after taking `l` and `r` bytes; returns the bytes taken.
pub fn grow_balanced(
    budget: f32,
    fixed: f32,
    width_of: impl Fn(char) -> f32,
    next: impl Fn(usize, usize) -> (Option<char>, Option<char>),
) -> (usize, usize) {
    let (mut l, mut r) = (0, 0);
    let (mut l_w, mut r_w) = (0.0f32, 0.0f32);
    loop {
        let used = l_w + fixed + r_w;
        let fitting = |c: Option<char>| {
            c.map(|c| (c.len_utf8(), width_of(c)))
                .filter(|&(_, w)| used + w <= budget)
        };
        let (left, right) = next(l, r);
        // The preferred side wins when it fits; otherwise the other one.
        match (l_w <= r_w, fitting(left), fitting(right)) {
            (true, Some((n, w)), _) | (false, Some((n, w)), None) => {
                l += n;
                l_w += w;
            }
            (_, _, Some((n, w))) => {
                r += n;
                r_w += w;
            }
            (_, None, None) => return (l, r),
        }
    }
}

/// Byte offsets `(head_end, tail_start)` for eliding the middle of `text` to
/// fit `max_width` pixels with one `…`; `None` if it fits. Measured in real
/// glyph widths: any overshoot and egui elides again, painting two ellipses.
/// Split from [`middle_elide`] for callers that highlight ranges.
pub fn middle_elide_cut(
    ui: &egui::Ui,
    text: &str,
    max_width: f32,
    font_id: &egui::FontId,
) -> Option<(usize, usize)> {
    ui.fonts(|f| {
        let width_of = |c: char| f.glyph_width(font_id, c);
        if text.chars().map(width_of).sum::<f32>() <= max_width {
            return None;
        }
        // A head and a tail grow toward each other.
        let (head, tail_len) = grow_balanced(max_width - width_of('…'), 0.0, width_of, |h, t| {
            let rest = &text[h..text.len() - t];
            (rest.chars().next(), rest.chars().next_back())
        });
        let tail = text.len() - tail_len;
        // Meeting means the two halves dropped nothing.
        (head < tail).then_some((head, tail))
    })
}

/// Middle-elide `text` to fit `max_width` pixels. The borrowed/owned
/// distinction is the caller's signal that something was dropped.
pub fn middle_elide<'a>(
    ui: &egui::Ui,
    text: &'a str,
    max_width: f32,
    font_id: &egui::FontId,
) -> Cow<'a, str> {
    match middle_elide_cut(ui, text, max_width, font_id) {
        None => Cow::Borrowed(text),
        Some((head, tail)) => {
            let mut out = String::with_capacity(head + '…'.len_utf8() + (text.len() - tail));
            out.push_str(&text[..head]);
            out.push('…');
            out.push_str(&text[tail..]);
            Cow::Owned(out)
        }
    }
}

/// A semitransparent down-arrow while more content lies below the fold.
/// Painter-only, so it never swallows clicks; the fonts have no ▼ glyph.
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

/// Height of the wipe's soft edge, as a fraction of the section.
const WIPE_BAND: f32 = 0.45;
/// …but never thinner, so a two-row table still gets a gradient.
const WIPE_BAND_MIN: f32 = 24.0;

/// The two y-coordinates the scrim ramps between: clear above the first,
/// opaque below the second. `wipe` 1 = fully covered, 0 = fully shown.
fn wipe_edges(rect: egui::Rect, wipe: f32) -> (f32, f32) {
    let band = (rect.height() * WIPE_BAND).max(WIPE_BAND_MIN);
    let covered = rect.bottom() + band - wipe * (rect.height() + band);
    (covered - band, covered)
}

/// The scrim hiding the not-yet-revealed part of `rect`. Unlike a per-row
/// opacity it reaches the parts of an `egui_extras` table the caller never
/// gets a `Ui` for: the stripes, the selection fill, the scroll bar.
pub fn wipe_mesh(rect: egui::Rect, wipe: f32, fill: egui::Color32) -> Option<egui::Mesh> {
    if wipe <= 0.0 || rect.height() <= 0.0 || rect.width() <= 0.0 {
        return None;
    }
    let (clear, covered) = wipe_edges(rect, wipe);
    let alpha_at = |y: f32| ((y - clear) / (covered - clear)).clamp(0.0, 1.0);

    // Linear between the edges, flat outside: quads break only at the edges.
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
        if bottom - top < 0.5 || (a_top <= 0.0 && a_bottom <= 0.0) {
            continue;
        }
        let base = mesh.vertices.len() as u32;
        for (y, alpha) in [(top, a_top), (bottom, a_bottom)] {
            // Mesh vertices carry premultiplied colors.
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

/// Paints [`wipe_mesh`] through the layer painter, not `ui.painter()`: the
/// caller's section-wide opacity would otherwise scale the scrim along with
/// what it is meant to hide.
pub fn wipe_scrim(ui: &egui::Ui, rect: egui::Rect, wipe: f32) {
    let Some(mesh) = wipe_mesh(rect, wipe, ui.visuals().panel_fill) else {
        return;
    };
    ui.ctx()
        .layer_painter(ui.layer_id())
        .add(egui::Shape::mesh(mesh));
}

/// The Apply & Save button, orange while there are unsaved edits, with an
/// "Unsaved changes" note beside it that never renames the button.
pub fn apply_save_row(ui: &mut egui::Ui, dirty: bool) -> egui::Response {
    use crate::tips::Tipped;
    let p = palette(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        let color = if dirty { p.orange } else { p.blue };
        let apply = ui
            .add(bordered_button("Apply & Save", color))
            .tip(&crate::tips::APPLY_SAVE);
        stable_section(ui, |ui| {
            if dirty {
                ui.label(hint_colored("Unsaved changes", p.orange));
            }
        });
        apply
    })
    .inner
}

/// Small, weak text for secondary notes.
pub fn hint(text: impl Into<egui::RichText>) -> egui::RichText {
    text.into().small().weak()
}

pub fn hint_colored(text: impl Into<egui::RichText>, color: egui::Color32) -> egui::RichText {
    text.into().small().color(color)
}

/// The centered window every confirmation prompt shares. `None` if egui
/// skipped the window this frame; width is the body's to set.
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

pub fn progress_bar(ui: &mut egui::Ui, fraction: Option<f32>, width: f32) {
    let bar = match fraction {
        Some(frac) => egui::ProgressBar::new(frac),
        None => egui::ProgressBar::new(0.0).animate(true),
    };
    ui.add(bar.desired_width(width));
}

#[cfg(test)]
mod tests;
