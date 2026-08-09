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

/// The mark on a snippet that starts partway into its window;
/// `first_visible_byte` pays for its width in advance.
const SNIPPET_LEAD: &str = "… ";

/// Append `window[range]` to `job`, highlighting whatever parts of `ranges`
/// (byte offsets into `window`) fall inside it.
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

/// The byte offset in `snip.window` that rendering has to start at for the
/// first match to land on a row that survives `max_rows`; `0` when it
/// already does. epaint stops at `wrap.max_rows` and *every* `\n` costs a
/// row, blank line or not, so a ragged lead-in can spend the whole row
/// budget before layout reaches the match.
fn first_visible_byte(
    ui: &egui::Ui,
    snip: &Snippet,
    fmt: &SnippetFormats,
    max_rows: usize,
    wrap_width: f32,
) -> usize {
    let Some(&(match_start, _)) = snip.ranges.first() else {
        return 0; // a head-of-file window, with nothing to keep on screen
    };

    // The rendered job pays for a leading mark this probe does not, so the
    // probe wraps to a narrower width — a point narrower still, since epaint
    // rounds `wrap.max_width` before laying out. Every rendered row then
    // holds at least what the probe row starting at the same character held,
    // so the match cannot drift *down* a row when the job is rebuilt.
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

    // Cursors index characters; snippet ranges are byte offsets. epaint
    // counts the `\n` that ends a row, so the two spaces line up 1:1.
    let cursor = egui::text::CCursor {
        index: snip.window[..match_start].chars().count(),
        // At a wrap, the character belongs to the row it is drawn on, not
        // the one it was pushed off.
        prefer_next_row: true,
    };
    let match_row = galley.layout_from_cursor(cursor).row;

    // epaint trades a glyph or two off the end of the last visible row for
    // its own overflow ellipsis, so a match sitting there only counts as
    // visible when there was nothing below it to elide in the first place.
    let visible_rows = if galley.rows.len() > max_rows {
        max_rows.saturating_sub(1)
    } else {
        max_rows
    };
    if match_row < visible_rows {
        return 0;
    }

    // Keep a third of the budget as lead-in so the hit is not pinned to the
    // top edge.
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

/// Build a highlighted snippet LayoutJob from byte ranges, wrapped to at
/// most `max_rows` and started far enough into the window that the first
/// match survives the cap.
pub(super) fn snippet_job(ui: &egui::Ui, snip: &Snippet, max_rows: usize) -> LayoutJob {
    let fmt = snippet_formats(ui);
    // In a top-down `Ui`, `ui.label` overwrites `wrap.max_width` with exactly
    // `ui.available_width()`; setting it here anyway lets `first_visible_byte`
    // (and a test) lay out the rows the user will see.
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

/// The Match column cell: one line with the (first) matched span centered
/// and an equal amount of context on both sides, trimmed to what fits the
/// column width. Matches on a whole field — a filename or a path — are
/// wrapped in brackets: `[name]`.
pub(super) fn centered_match_job(
    ui: &egui::Ui,
    snip: &Snippet,
    width_px: f32,
    whole_field: bool,
) -> LayoutJob {
    let fmt = snippet_formats(ui);

    // Newlines force line breaks even in a one-row LayoutJob; flatten them
    // to spaces — a byte-for-byte ASCII replacement, so the match ranges
    // stay valid. The mouseover renders the original window untouched.
    let flattened: Option<String> = snip
        .window
        .contains(['\n', '\r', '\t'])
        .then(|| snip.window.replace(['\n', '\r', '\t'], " "));
    let window = flattened.as_deref().unwrap_or(&snip.window);

    // The budget is in pixels, summed from the font's own glyph advances:
    // the centered-and-justified layout puts egui in Extend mode, which lays
    // the job out at infinite width, and `Column::clip` then trims a
    // *centered* overflow from both ends at once — silently, taking the
    // highlighted match with it.
    let (start, end, decorate) = ui.fonts(|f| {
        let font_id = &fmt.normal.font_id;
        let width_of = |c: char| f.glyph_width(font_id, c);
        let ellipsis = width_of('…');
        let brackets = if whole_field {
            width_of('[') + width_of(']')
        } else {
            0.0
        };
        let mut marks = 0.0;
        if snip.truncated_start {
            marks += ellipsis;
        }
        if snip.truncated_end {
            marks += ellipsis;
        }
        if fits_within(window, width_px - brackets - marks, width_of) {
            return (0, window.len(), true);
        }

        // Something has to go, so either end may gain a mark; reserve for
        // both so a cut never overflows the column.
        let budget = width_px - brackets - 2.0 * ellipsis;
        let Some(&(a, b)) = snip.ranges.first() else {
            // No ranges (shouldn't happen for match cells) — head trim.
            return (0, take_forward(window, 0, budget.max(0.0), width_of), true);
        };
        if budget <= 0.0 {
            // A column narrower than its own punctuation: spend every point
            // on the hit and drop the decoration.
            return (a, take_forward(window, a, width_px, width_of), false);
        }
        if !fits_within(&window[a..b], budget, width_of) {
            // A hit wider than the whole column — a greedy regex or wildcard
            // match. Its beginning is the part that has to survive.
            return (a, take_forward(window, a, budget, width_of), true);
        }
        let match_w: f32 = window[a..b].chars().map(width_of).sum();

        // Equal context on both sides, grown outward one character at a
        // time; whichever side is currently narrower is fed first, so the
        // leftover from a short side flows to the other.
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
            // The preferred side wins when it fits; otherwise the other one
            // does, since at least one of them just did.
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
    if whole_field && decorate {
        job.append("[", 0.0, fmt.weak.clone());
    }
    if decorate && (start > 0 || snip.truncated_start) {
        job.append("…", 0.0, fmt.weak.clone());
    }
    append_marked(&mut job, &fmt, window, &snip.ranges, start..end);
    if decorate && (end < window.len() || snip.truncated_end) {
        job.append("…", 0.0, fmt.weak.clone());
    }
    if whole_field && decorate {
        job.append("]", 0.0, fmt.weak);
    }
    job
}

/// Whether the whole of `text` fits in `budget` pixels; stops at the first
/// character that does not.
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

/// The byte offset one past the last character of `text[from..]` that still
/// fits in `budget` pixels.
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
