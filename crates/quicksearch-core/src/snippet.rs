//! Snippet extraction for search results; contentless FTS5 has no
//! `snippet()`. Output is structural (window text plus byte ranges), never
//! markup. Only literal terms are matched here ([`extract_folded`]), folding
//! ASCII case only, as their full-text verification does: `cafe` neither
//! finds nor marks `café`. A `*` pattern or `regex:` finds its own match,
//! folding Unicode case, and hands the range to [`window_around`] or
//! [`whole_field`].

/// A context window from a document plus the match positions inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The excerpt, sliced verbatim from the source text.
    pub window: String,
    /// Byte ranges *into `window`*, sorted, non-overlapping (overlapping
    /// term hits are coalesced), always on char boundaries.
    pub ranges: Vec<(usize, usize)>,
    /// Content exists before/after the window.
    pub truncated_start: bool,
    pub truncated_end: bool,
}

impl Snippet {
    fn empty() -> Snippet {
        Snippet {
            window: String::new(),
            ranges: Vec::new(),
            truncated_start: false,
            truncated_end: false,
        }
    }
}

/// What [`extract_folded`] found in one body.
#[derive(Debug, Clone)]
pub struct Found {
    pub snippet: Snippet,
    /// Occurrences in any ASCII case, counted before touching ones were
    /// coalesced: what the full-text pass ranks on.
    pub count: usize,
    /// How many of those are spelled exactly as typed.
    pub exact: usize,
}

/// Find `term` in a haystack the caller has already ASCII-folded, and cut the
/// window around its first occurrence. `folded` must be
/// `text.to_ascii_lowercase()` and `term` must be `typed` ASCII-lowercased:
/// the fold preserves byte length, so an offset found in `folded` slices
/// `text`, where the occurrence is compared with `typed`. `None` when the
/// term does not occur; a blank term never does, but a padded one is searched
/// as given, never trimmed.
///
/// `exact` comes from the case-insensitive occurrences rather than a second,
/// case-sensitive search: the same count for any term that cannot overlap
/// itself, and at most one short per overlap for one that can (`aA` in `aaA`).
pub fn extract_folded(
    text: &str,
    folded: &str,
    term: &str,
    typed: &str,
    budget: usize,
) -> Option<Found> {
    debug_assert_eq!(folded.len(), text.len(), "ASCII folding preserves length");
    debug_assert_eq!(term, typed.to_ascii_lowercase(), "`term` is `typed` folded");
    if term.trim().is_empty() {
        return None;
    }

    // `memmem`, not `str::match_indices`: measured faster on whole-body
    // scans. The walk runs to the end (the counts must see every occurrence),
    // but matches past the window's right edge are not stored.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let (mut count, mut exact) = (0usize, 0usize);
    // The window's right edge once the first match fixes it, from the same
    // [`window_bounds`] the window itself uses.
    let mut keep_below: Option<usize> = None;
    // The end of the last kept match, so a chain of touching occurrences
    // (`abab…` for `ab`) that starts inside the window and continues past
    // it stays intact for `coalesce_overlapping` and the expansion step.
    let mut chain_end = 0usize;
    for at in memchr::memmem::find_iter(folded.as_bytes(), term.as_bytes()) {
        // Before any dropping: two abutting occurrences are two hits for
        // ranking even though they paint as one highlight.
        count += 1;
        if text.as_bytes()[at..at + term.len()] == *typed.as_bytes() {
            exact += 1;
        }
        let bound = *keep_below.get_or_insert_with(|| window_bounds(text, at, budget).1);
        if at >= bound && at > chain_end {
            // Past the edge and not chained to anything kept: keep counting,
            // stop storing.
            continue;
        }
        chain_end = at + term.len();
        matches.push((at, chain_end));
    }

    if matches.is_empty() {
        return None;
    }

    // One term's occurrences arrive in order and never overlap; only
    // abutting ones merge.
    let matches = coalesce_overlapping(matches);

    let (win_start, mut win_end) = window_bounds(text, matches[0].0, budget);
    // A match straddling the right edge is fully included, not cut mid-hit.
    if let Some((_, end)) = matches.iter().rfind(|(s, _)| *s < win_end) {
        if *end > win_end {
            win_end = text.ceil_char_boundary(*end);
        }
    }

    let ranges = matches
        .iter()
        .filter(|(s, e)| *e > win_start && *s < win_end)
        .map(|(s, e)| {
            (
                (*s).max(win_start) - win_start,
                (*e).min(win_end) - win_start,
            )
        })
        .collect();

    Some(Found {
        snippet: Snippet {
            window: text[win_start..win_end].to_string(),
            ranges,
            truncated_start: win_start > 0,
            truncated_end: win_end < text.len(),
        },
        count,
        exact,
    })
}

/// The window cut around a match that starts at `first`: from a third of
/// `budget` before it, rounded down to a char boundary, to `budget` past that
/// unrounded start, rounded up. The third keeps the hit off the left edge.
fn window_bounds(text: &str, first: usize, budget: usize) -> (usize, usize) {
    let start = first.saturating_sub(budget / 3);
    let end = (start + budget).min(text.len());
    (
        text.floor_char_boundary(start),
        text.ceil_char_boundary(end),
    )
}

/// Clamp `range` into `text` and widen it to the nearest char boundaries.
/// Callers take ranges from byte-level matchers, so an endpoint can land
/// inside a multi-byte character, where slicing panics.
fn aligned_range(text: &str, range: (usize, usize)) -> (usize, usize) {
    let start = range.0.min(text.len());
    let end = range.1.clamp(start, text.len());
    (
        text.floor_char_boundary(start),
        text.ceil_char_boundary(end),
    )
}

/// The whole of `text` as the window, with `range` marked: the shape the name
/// and path tiers use. `window` is the field verbatim, so the ranges index the
/// field the column is already painting.
pub fn whole_field(text: &str, range: (usize, usize)) -> Snippet {
    if text.is_empty() {
        return Snippet::empty();
    }
    let (start, end) = aligned_range(text, range);
    Snippet {
        window: text.to_string(),
        ranges: if end > start {
            vec![(start, end)]
        } else {
            Vec::new()
        },
        truncated_start: false,
        truncated_end: false,
    }
}

/// Build a snippet window around one known match range in `text`. The range
/// is clamped into `text` and widened to char boundaries.
pub fn window_around(text: &str, range: (usize, usize), budget: usize) -> Snippet {
    if text.is_empty() {
        return Snippet::empty();
    }
    let (ms, me) = aligned_range(text, range);
    let (win_start, win_end) = window_bounds(text, ms, budget);
    // The whole match stays inside the window.
    let win_end = win_end.max(me);

    let ranges = if me > ms {
        vec![(ms - win_start, me - win_start)]
    } else {
        Vec::new()
    };
    Snippet {
        window: text[win_start..win_end].to_string(),
        ranges,
        truncated_start: win_start > 0,
        truncated_end: win_end < text.len(),
    }
}

/// Merge adjacent/overlapping ranges. Input must be sorted by start.
fn coalesce_overlapping(v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(v.len());
    let mut it = v.into_iter();
    let Some(mut cur) = it.next() else {
        return out;
    };
    for next in it {
        if next.0 <= cur.1 {
            cur.1 = cur.1.max(next.1);
        } else {
            out.push(cur);
            cur = next;
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`extract_folded`] with the folding done here; `typed` as given.
    fn find(text: &str, typed: &str, budget: usize) -> Option<Found> {
        let term = typed.to_ascii_lowercase();
        extract_folded(text, &text.to_ascii_lowercase(), &term, typed, budget)
    }

    fn extract(text: &str, term: &str, budget: usize) -> Snippet {
        find(text, term, budget).expect("the term occurs").snippet
    }

    fn extract_counted(text: &str, term: &str, budget: usize) -> (Snippet, usize) {
        let found = find(text, term, budget).expect("the term occurs");
        (found.snippet, found.count)
    }

    /// Non-overlapping occurrences, byte for byte.
    fn memmem_count(text: &str, term: &str) -> usize {
        memchr::memmem::find_iter(text.as_bytes(), term.as_bytes()).count()
    }

    /// A window budget small enough that tests see truncation.
    const SMALL: usize = 40;

    /// The count is occurrences, not the highlights they coalesce into.
    #[test]
    fn extract_folded_counts_occurrences_not_ranges() {
        let text = "abab and ab";
        let (s, n) = extract_counted(text, "ab", SMALL);
        assert_eq!(n, 3, "three occurrences");
        assert_eq!(n, memmem_count(text, "ab"));
        assert_eq!(s.ranges.len(), 2, "the abutting pair paints as one range");
    }

    /// A disagreement here would change which rows survive the pass, or
    /// rank them: both counts must match separate searches, one over the
    /// folded body and one over the body as stored.
    #[test]
    fn extract_folded_counts_agree_with_separate_searches() {
        let cases: &[(&str, &str)] = &[
            ("one hit here", "hit"),
            ("Hit hit HIT", "hit"),
            ("Hit hit HIT", "HIT"),
            ("Hit hit HIT", "hIT"),
            ("aaaa", "aa"),
            ("ünïcode ÜNÏCODE ünïcode", "ünïcode"),
            ("Needle and needle and NEEDLE", "Needle"),
        ];
        for (text, typed) in cases {
            let found = find(text, typed, SMALL).expect("the term occurs");
            let folded = text.to_ascii_lowercase();
            let term = typed.to_ascii_lowercase();
            assert_eq!(
                found.count,
                memmem_count(&folded, &term),
                "{typed:?} in {text:?}"
            );
            assert_eq!(
                found.exact,
                memmem_count(text, typed),
                "{typed:?} in {text:?}"
            );
        }
    }

    #[test]
    fn a_term_that_does_not_occur_finds_nothing() {
        for (text, term) in [
            ("no hits at all", "zzz"),
            ("", "x"),
            ("short", "much longer than the haystack"),
            ("alpha beta gamma", "nomatch"),
        ] {
            assert!(find(text, term, SMALL).is_none(), "{term:?} in {text:?}");
        }
    }

    /// An empty needle would match at every byte offset.
    #[test]
    fn extract_folded_ignores_blank_terms() {
        for term in ["", "   ", "\t"] {
            assert!(find("some text", term, SMALL).is_none(), "{term:?}");
        }
    }

    /// A term with surrounding space is searched as given, never trimmed.
    #[test]
    fn extract_folded_does_not_trim_a_padded_term() {
        let text = "needle needlework";
        let (_, n) = extract_counted(text, "needle ", SMALL);
        assert_eq!(n, 1, "only the occurrence followed by a space");
    }

    /// The `Snippet::ranges` contract egui's LayoutJob sections rely on.
    fn assert_ranges_valid(s: &Snippet) {
        let mut prev_end = 0;
        for &(a, b) in &s.ranges {
            assert!(a < b, "empty/inverted range {:?}", (a, b));
            assert!(b <= s.window.len(), "range {:?} beyond window", (a, b));
            assert!(a >= prev_end, "overlapping ranges");
            assert!(s.window.is_char_boundary(a) && s.window.is_char_boundary(b));
            prev_end = b;
        }
    }

    fn marked(s: &Snippet) -> Vec<&str> {
        s.ranges.iter().map(|&(a, b)| &s.window[a..b]).collect()
    }

    /// Occurrences past the window's right edge are counted but not kept; a
    /// bound leaking into either side would be a ranking change or a missing
    /// highlight.
    #[test]
    fn occurrences_past_the_window_are_counted_but_not_kept() {
        let unit = "needle filler filler ";
        let text = unit.repeat(400);
        let (snip, found) = extract_counted(&text, "needle", SMALL);

        assert_eq!(found, 400, "every occurrence must still be counted");
        assert_ranges_valid(&snip);
        assert!(!snip.ranges.is_empty());
        assert!(snip.truncated_end, "there is a great deal more body");

        for &(a, b) in &snip.ranges {
            assert_eq!(&snip.window[a..b], "needle");
        }
        let win_at = text
            .find(&snip.window)
            .expect("the window is a slice of the text");
        let expected = memchr::memmem::find_iter(text.as_bytes(), b"needle")
            .filter(|at| *at >= win_at && *at < win_at + snip.window.len())
            .count();
        assert_eq!(
            snip.ranges.len(),
            expected,
            "every occurrence inside the window must be marked"
        );
    }

    /// The case the chain rule exists for: a chain that starts inside the
    /// window can run past its right edge, and dropping at the edge would
    /// cut the highlight short.
    #[test]
    fn a_coalescing_chain_is_not_cut_at_the_window_edge() {
        let text = "ab".repeat(400);
        let (snip, found) = extract_counted(&text, "ab", SMALL);

        assert_eq!(found, 400, "non-overlapping occurrences, all counted");
        assert_ranges_valid(&snip);
        assert_eq!(snip.ranges.len(), 1, "one chain, one highlight");

        // An unbroken chain grows the window to the whole text; without the
        // chain rule the window is 40 bytes, which "range == whole window"
        // alone cannot tell apart.
        assert_eq!(
            snip.window.len(),
            text.len(),
            "the window must grow to cover the coalesced run"
        );
        assert!(
            !snip.truncated_end,
            "nothing is left past a full-length window"
        );
        assert_eq!(
            snip.ranges[0],
            (0, text.len()),
            "one highlight over the lot"
        );
    }

    #[test]
    fn simple_match_range() {
        let s = extract("the quick brown fox", "quick", SMALL);
        assert_eq!(marked(&s), vec!["quick"]);
        assert_ranges_valid(&s);
    }

    #[test]
    fn case_insensitive_match_reports_original_case() {
        let s = extract("The QUICK brown fox", "quick", SMALL);
        assert_eq!(marked(&s), vec!["QUICK"]);
    }

    #[test]
    fn window_truncation_flags_on_both_sides() {
        let text = "prefix ".repeat(20) + "MATCH in middle " + &"suffix ".repeat(20);
        let s = extract(&text, "MATCH", SMALL);
        assert!(s.truncated_start);
        assert!(s.truncated_end);
        assert_eq!(marked(&s), vec!["MATCH"]);
    }

    #[test]
    fn match_at_start_not_truncated_left() {
        let s = extract("MATCH right at the start of this paragraph", "match", SMALL);
        assert!(!s.truncated_start);
        assert_eq!(s.ranges[0].0, 0);
    }

    #[test]
    fn utf8_boundaries_hold_with_multibyte_text() {
        let text = "café café café café café café café café café café";
        let s = extract(text, "café", SMALL);
        assert!(!s.ranges.is_empty());
        assert_ranges_valid(&s);
        for m in marked(&s) {
            assert_eq!(m, "café");
        }
    }

    #[test]
    fn match_near_right_edge_is_fully_included() {
        let prefix = "x".repeat(30);
        let text = format!("{}{}", prefix, "LONGMATCHTERMTEXT");
        let s = extract(&text, "LONGMATCHTERMTEXT", SMALL);
        assert_eq!(marked(&s), vec!["LONGMATCHTERMTEXT"]);
    }

    #[test]
    fn window_around_basic() {
        let text = "prefix ".repeat(20) + "NEEDLE" + &" suffix".repeat(20);
        let at = text.find("NEEDLE").unwrap();
        let s = window_around(&text, (at, at + 6), SMALL);
        assert_eq!(marked(&s), vec!["NEEDLE"]);
        assert!(s.truncated_start && s.truncated_end);
        assert_ranges_valid(&s);
    }

    #[test]
    fn window_around_clamps_out_of_bounds() {
        let s = window_around("tiny", (2, 999), SMALL);
        assert_eq!(s.window, "tiny");
        assert_eq!(s.ranges, vec![(2, 4)]);
        // Fully out-of-range → no ranges, but never a panic.
        let s = window_around("tiny", (999, 1000), SMALL);
        assert!(s.ranges.is_empty());
    }

    #[test]
    fn window_around_aligns_multibyte_boundaries() {
        let text = "ééééééééé needle ééééééééé";
        let s = window_around(text, (1, 3), SMALL);
        assert_ranges_valid(&s);
    }
}
