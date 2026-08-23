//! Rendering a [`Snippet`] into egui layout jobs: marked ranges,
//! middle-out elision around the first match, and row budgeting.

use super::*;

struct SnippetFormats {
    normal: TextFormat,
    highlight: TextFormat,
    weak: TextFormat,
}

fn snippet_formats(ui: &egui::Ui) -> SnippetFormats {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    SnippetFormats {
        normal: TextFormat {
            font_id: font_id.clone(),
            color: ui.visuals().text_color(),
            ..Default::default()
        },
        highlight: TextFormat {
            font_id: font_id.clone(),
            color: ui.visuals().strong_text_color(),
            background: ui.visuals().selection.bg_fill.gamma_multiply(0.4),
            ..Default::default()
        },
        weak: TextFormat {
            font_id,
            color: ui.visuals().weak_text_color(),
            ..Default::default()
        },
    }
}

const SNIPPET_LEAD: &str = "… ";

/// Append `window[range]` to `job`, marking the parts of `ranges` inside it.
/// Ranges are clipped to the slice, so a caller rendering a string in pieces
/// can hand each piece the *whole* set.
fn append_marked(
    job: &mut LayoutJob,
    fmt: &SnippetFormats,
    window: &str,
    ranges: &[(usize, usize)],
    range: std::ops::Range<usize>,
) {
    let mut cursor = range.start;
    for &(a, b) in ranges {
        let (a, b) = (a.max(range.start), b.min(range.end));
        if a >= b {
            continue; // wholly before or after the slice
        }
        if a > cursor {
            job.append(&window[cursor..a], 0.0, fmt.normal.clone());
        }
        job.append(&window[a..b], 0.0, fmt.highlight.clone());
        cursor = b;
    }
    if cursor < range.end {
        job.append(&window[cursor..range.end], 0.0, fmt.normal.clone());
    }
}

/// A whole field with its matched spans marked. Wrapping stays at the job's
/// defaults, so this lays out exactly like the plain string it replaces.
pub(super) fn marked_field_job(ui: &egui::Ui, text: &str, ranges: &[(usize, usize)]) -> LayoutJob {
    let fmt = snippet_formats(ui);
    let mut job = LayoutJob::default();
    append_marked(&mut job, &fmt, text, ranges, 0..text.len());
    job
}

/// The byte offset rendering must start at for the first match to land on a
/// row that survives `max_rows`. epaint stops at `wrap.max_rows` and *every*
/// `\n` costs a row, so a ragged lead-in can spend the whole budget before
/// layout reaches the match.
fn first_visible_byte(
    ui: &egui::Ui,
    snip: &Snippet,
    fmt: &SnippetFormats,
    max_rows: usize,
    wrap_width: f32,
) -> usize {
    let Some(&(match_start, _)) = snip.ranges.first() else {
        return 0; // head-of-file window
    };

    // The rendered job pays for a leading mark this probe does not: the
    // probe wraps narrower, so the match cannot drift *down* a row on rebuild.
    let lead_width = ui.fonts(|f| {
        SNIPPET_LEAD
            .chars()
            .map(|c| f.glyph_width(&fmt.normal.font_id, c))
            .sum::<f32>()
    });
    let mut probe = LayoutJob::default();
    probe.wrap.max_width = (wrap_width - lead_width - 1.0).max(1.0);
    probe.append(&snip.window, 0.0, fmt.normal.clone());
    let galley = ui.fonts(|f| f.layout_job(probe));

    // Cursors index characters; snippet ranges are byte offsets.
    let cursor = egui::text::CCursor {
        index: snip.window[..match_start].chars().count(),
        // At a wrap, the character belongs to the row it is drawn on.
        prefer_next_row: true,
    };
    let match_row = galley.layout_from_cursor(cursor).row;

    // epaint trades the end of the last visible row for its own overflow
    // ellipsis, so a match sitting there only counts as visible when there
    // was nothing below it to elide.
    let visible_rows = if galley.rows.len() > max_rows {
        max_rows.saturating_sub(1)
    } else {
        max_rows
    };
    if match_row < visible_rows {
        return 0;
    }

    // A third of the budget as lead-in, so the hit is not pinned to the top.
    let mut cursor = cursor;
    for _ in 0..max_rows / 3 {
        // `Some(0.0)` asks for the row above, not the character above.
        cursor = galley.cursor_up_one_row(&cursor, Some(0.0)).0;
    }
    let start_char = galley.cursor_begin_of_row(&cursor).index;
    snip.window
        .char_indices()
        .nth(start_char)
        .map_or(snip.window.len(), |(i, _)| i)
}

/// Wrapped to `max_rows`, started far enough in that the first match survives.
pub(super) fn snippet_job(ui: &egui::Ui, snip: &Snippet, max_rows: usize) -> LayoutJob {
    let fmt = snippet_formats(ui);
    // `ui.label` overwrites `wrap.max_width` with `ui.available_width()`;
    // setting it anyway lets `first_visible_byte` lay out the real rows.
    let wrap_width = ui.available_width();
    let start = first_visible_byte(ui, snip, &fmt, max_rows, wrap_width);

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.wrap.max_rows = max_rows;
    if start > 0 || snip.truncated_start {
        job.append(SNIPPET_LEAD, 0.0, fmt.weak.clone());
    }
    append_marked(
        &mut job,
        &fmt,
        &snip.window,
        &snip.ranges,
        start..snip.window.len(),
    );
    if snip.truncated_end {
        job.append(" …", 0.0, fmt.weak);
    }
    job
}

/// The Content Match column cell: one line with the first matched span
/// centered and equal context on both sides, trimmed to the column width.
pub(super) fn centered_match_job(ui: &egui::Ui, snip: &Snippet, width_px: f32) -> LayoutJob {
    let fmt = snippet_formats(ui);

    // Newlines force breaks even in a one-row LayoutJob; flatten them to
    // spaces — byte-for-byte, so the match ranges stay valid.
    let flattened: Option<String> = snip
        .window
        .contains(['\n', '\r', '\t'])
        .then(|| snip.window.replace(['\n', '\r', '\t'], " "));
    let window = flattened.as_deref().unwrap_or(&snip.window);

    // The budget is in pixels: the centered layout puts egui in Extend mode
    // (infinite width), and `Column::clip` then trims a *centered* overflow
    // from both ends at once — silently, taking the match with it.
    let (start, end, decorate) = ui.fonts(|f| {
        let font_id = &fmt.normal.font_id;
        let width_of = |c: char| f.glyph_width(font_id, c);
        let ellipsis = width_of('…');
        let mut marks = 0.0;
        if snip.truncated_start {
            marks += ellipsis;
        }
        if snip.truncated_end {
            marks += ellipsis;
        }
        if fits_within(window, width_px - marks, width_of) {
            return (0, window.len(), true);
        }
        // Either end may gain a mark; reserve for both.
        let budget = width_px - 2.0 * ellipsis;
        let Some(&(a, b)) = snip.ranges.first() else {
            // No ranges (shouldn't happen for match cells) — head trim.
            return (0, take_forward(window, 0, budget.max(0.0), width_of), true);
        };
        if budget <= 0.0 {
            // Narrower than its own punctuation: spend everything on the hit.
            return (a, take_forward(window, a, width_px, width_of), false);
        }
        if !fits_within(&window[a..b], budget, width_of) {
            // A hit wider than the column: its beginning has to survive.
            return (a, take_forward(window, a, budget, width_of), true);
        }
        let match_w: f32 = window[a..b].chars().map(width_of).sum();

        // Equal context on both sides, grown outward a character at a time;
        // the narrower side is fed first.
        let (mut start, mut end) = (a, b);
        let (mut before_w, mut after_w) = (0.0f32, 0.0f32);
        loop {
            let prev = window[..start].chars().next_back();
            let next = window[end..].chars().next();
            let used = before_w + match_w + after_w;
            let prev_fits = prev.is_some_and(|c| used + width_of(c) <= budget);
            let next_fits = next.is_some_and(|c| used + width_of(c) <= budget);
            if !prev_fits && !next_fits {
                break;
            }
            // The preferred side wins when it fits; otherwise the other just did.
            let take_prev = if before_w <= after_w {
                prev_fits
            } else {
                !next_fits
            };
            if take_prev {
                let c = prev.expect("prev_fits");
                start -= c.len_utf8();
                before_w += width_of(c);
            } else {
                let c = next.expect("next_fits");
                end += c.len_utf8();
                after_w += width_of(c);
            }
        }
        (start, end, true)
    });

    let mut job = LayoutJob::default();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    if decorate && (start > 0 || snip.truncated_start) {
        job.append("…", 0.0, fmt.weak.clone());
    }
    append_marked(&mut job, &fmt, window, &snip.ranges, start..end);
    if decorate && (end < window.len() || snip.truncated_end) {
        job.append("…", 0.0, fmt.weak);
    }
    job
}

/// The Path column cell: middle-elided to `width_px`, with whatever of a
/// path-tier match survives the cut highlighted; only the elision mark is
/// weak. Returns the job and whether anything was elided — the caller's
/// trigger for a full-path tooltip.
pub(super) fn path_cell_job(
    ui: &egui::Ui,
    path: &str,
    ranges: &[(usize, usize)],
    width_px: f32,
    font_id: &egui::FontId,
) -> (LayoutJob, bool) {
    let fmt = snippet_formats(ui);
    let mut job = LayoutJob::default();
    match crate::ui_util::middle_elide_cut(ui, path, width_px, font_id) {
        None => (marked_field_job(ui, path, ranges), false),
        // The surviving ends keep their original offsets: `append_marked`
        // clips the ranges to each end, so a match in the dropped middle
        // drops with it rather than landing on the glyphs that moved in.
        Some((head, tail)) => {
            append_marked(&mut job, &fmt, path, ranges, 0..head);
            job.append("…", 0.0, fmt.weak.clone());
            append_marked(&mut job, &fmt, path, ranges, tail..path.len());
            (job, true)
        }
    }
}

fn fits_within(text: &str, budget: f32, width_of: impl Fn(char) -> f32) -> bool {
    let mut used = 0.0;
    for c in text.chars() {
        used += width_of(c);
        if used > budget {
            return false;
        }
    }
    true
}

fn take_forward(text: &str, from: usize, budget: f32, width_of: impl Fn(char) -> f32) -> usize {
    let mut end = from;
    let mut used = 0.0;
    for c in text[from..].chars() {
        let w = width_of(c);
        if used + w > budget {
            break;
        }
        used += w;
        end += c.len_utf8();
    }
    end
}
