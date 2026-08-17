//! Snippet extraction for search results.
//!
//! Contentless FTS5 doesn't support SQLite's `snippet()` / `highlight()`,
//! so this module finds a window of context around the first match in the
//! stored text and reports every query-term occurrence inside it. Output is
//! *structural* — the window text plus byte ranges of the matches within
//! it — so any frontend can render highlights natively; nothing here
//! produces markup.
//!
//! Matching is ASCII-case-insensitive only: a query for `cafe` still
//! *finds* a file containing `café` (the FTS tokenizer strips diacritics)
//! but the snippet won't mark the accented occurrence.

/// Options controlling snippet extraction.
#[derive(Debug, Clone)]
pub struct Options {
    /// Approximate byte budget for the returned window. Matches expand the
    /// window if needed so a hit is never cut off; the budget is a soft
    /// target, not a hard cap.
    pub approx_chars: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { approx_chars: 200 }
    }
}

/// A context window from a document plus the match positions inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The excerpt, sliced verbatim from the source text on UTF-8 char
    /// boundaries.
    pub window: String,
    /// Byte ranges *into `window`*, sorted, non-overlapping (overlapping
    /// term hits are coalesced), always on char boundaries.
    pub ranges: Vec<(usize, usize)>,
    /// Content exists before/after the window — frontends render their own
    /// ellipsis.
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

/// [`extract`] against a haystack the caller has already ASCII-folded.
/// `folded` must be `text.to_ascii_lowercase()` — the fold is byte-length
/// preserving, which is what lets offsets found in it slice the original.
pub fn extract_folded(text: &str, folded: &str, terms: &[&str], opts: &Options) -> Snippet {
    debug_assert_eq!(folded.len(), text.len(), "ASCII folding preserves length");
    if text.is_empty() {
        return Snippet::empty();
    }
    let effective_terms: Vec<&str> = terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if effective_terms.is_empty() {
        return head_window(text, opts.approx_chars);
    }

    // `memmem` rather than `str::match_indices`: both find non-overlapping
    // occurrences, but std's Two-Way searcher has no vector prefilter and a
    // full-text row scans a whole document body. See `benches/search.rs`,
    // group `substring`.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for term in &effective_terms {
        let pattern = term.to_ascii_lowercase();
        matches.extend(
            memchr::memmem::find_iter(folded.as_bytes(), pattern.as_bytes())
                .map(|at| (at, at + pattern.len())),
        );
    }

    if matches.is_empty() {
        return head_window(text, opts.approx_chars);
    }

    matches.sort_by_key(|(a, _)| *a);
    let matches = coalesce_overlapping(matches);

    // Pick the window. Start a third of the budget before the first match
    // so the hit isn't pinned to the left edge; round both ends to char
    // boundaries so we never slice a multi-byte UTF-8 sequence.
    let pre_pad = opts.approx_chars / 3;
    let mut win_start = matches[0].0.saturating_sub(pre_pad);
    let mut win_end = (win_start + opts.approx_chars).min(text.len());
    while win_start > 0 && !text.is_char_boundary(win_start) {
        win_start -= 1;
    }
    while win_end < text.len() && !text.is_char_boundary(win_end) {
        win_end += 1;
    }

    // Expand the window so a match straddling the right edge is fully
    // included rather than cut mid-hit.
    if let Some((_, end)) = matches.iter().rfind(|(s, _)| *s < win_end) {
        if *end > win_end {
            win_end = *end;
            while win_end < text.len() && !text.is_char_boundary(win_end) {
                win_end += 1;
            }
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

    Snippet {
        window: text[win_start..win_end].to_string(),
        ranges,
        truncated_start: win_start > 0,
        truncated_end: win_end < text.len(),
    }
}

/// Clamp `range` into `text` and widen it to the nearest char boundaries.
///
/// Both callers below take ranges from matchers that work on bytes — bitap
/// over an ASCII-folded copy — so an endpoint can land inside a multi-byte
/// character. Slicing there panics, and `Snippet::ranges` promises boundaries.
fn aligned_range(text: &str, range: (usize, usize)) -> (usize, usize) {
    let (mut start, mut end) = range;
    start = start.min(text.len());
    end = end.clamp(start, text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    (start, end)
}

/// The whole of `text` as the window, with `range` marked.
///
/// This is the shape [`crate::search::SearchHit::snippet`] documents for the
/// name and path tiers, and what lets a frontend highlight the matched span
/// inside its own Name or Path column: `window` is that field verbatim, so the
/// ranges index the field the column is already painting. A filename or a path
/// is short enough to carry whole, so there is nothing to gain by windowing it.
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

/// Build a snippet window around one known match range in `text` (byte
/// offsets into `text`). Used by fuzzy full-text search, where the match
/// was located by the fuzzy matcher rather than exact term search. The
/// range is clamped and char-boundary-aligned defensively.
pub fn window_around(text: &str, range: (usize, usize), opts: &Options) -> Snippet {
    if text.is_empty() {
        return Snippet::empty();
    }
    let (ms, me) = aligned_range(text, range);

    let pre_pad = opts.approx_chars / 3;
    let mut win_start = ms.saturating_sub(pre_pad);
    let mut win_end = (win_start + opts.approx_chars).max(me).min(text.len());
    while win_start > 0 && !text.is_char_boundary(win_start) {
        win_start -= 1;
    }
    while win_end < text.len() && !text.is_char_boundary(win_end) {
        win_end += 1;
    }

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

/// Count non-overlapping occurrences of `term` in `text`. Empty terms count
/// zero. Case-insensitive counting folds ASCII only, matching the rest of
/// the search pipeline.
pub fn count_occurrences(text: &str, term: &str, case_sensitive: bool) -> usize {
    if term.is_empty() || text.len() < term.len() {
        return 0;
    }
    if case_sensitive {
        memchr::memmem::find_iter(text.as_bytes(), term.as_bytes()).count()
    } else {
        memchr::memmem::find_iter(
            text.to_ascii_lowercase().as_bytes(),
            term.to_ascii_lowercase().as_bytes(),
        )
        .count()
    }
}

/// The first `n` bytes of `text` (char-aligned) as a match-less window.
fn head_window(text: &str, n: usize) -> Snippet {
    if text.len() <= n {
        return Snippet {
            window: text.to_string(),
            ranges: Vec::new(),
            truncated_start: false,
            truncated_end: false,
        };
    }
    let mut cut = n;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Snippet {
        window: text[..cut].to_string(),
        ranges: Vec::new(),
        truncated_start: false,
        truncated_end: true,
    }
}

/// Merge adjacent / overlapping (start, end) ranges. Input must be sorted
/// by start.
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

    /// The tests were written against a since-removed `extract` wrapper.
    /// Production always holds a fold buffer already, so the wrapper earned
    /// nothing; folding here keeps its coverage of the window logic.
    fn extract(text: &str, terms: &[&str], opts: &Options) -> Snippet {
        extract_folded(text, &text.to_ascii_lowercase(), terms, opts)
    }

    fn opts_small() -> Options {
        Options { approx_chars: 40 }
    }

    /// Every range must be in-bounds, ordered, non-overlapping, and sit on
    /// char boundaries — the contract egui's LayoutJob sections rely on.
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

    #[test]
    fn empty_text_returns_empty() {
        let s = extract("", &["foo"], &Options::default());
        assert_eq!(s, Snippet::empty());
    }

    #[test]
    fn no_terms_returns_head_marked_truncated() {
        let long = "abcdefghijklmnop".repeat(10);
        let s = extract(&long, &[], &opts_small());
        assert!(s.truncated_end);
        assert!(!s.truncated_start);
        assert!(s.ranges.is_empty());
        assert!(s.window.len() <= 40);
    }

    #[test]
    fn no_terms_untruncated() {
        let s = extract("short text", &[], &opts_small());
        assert_eq!(s.window, "short text");
        assert!(!s.truncated_end && !s.truncated_start);
    }

    #[test]
    fn simple_match_range() {
        let s = extract("the quick brown fox", &["quick"], &opts_small());
        assert_eq!(marked(&s), vec!["quick"]);
        assert_ranges_valid(&s);
    }

    #[test]
    fn case_insensitive_match_reports_original_case() {
        let s = extract("The QUICK brown fox", &["quick"], &opts_small());
        assert_eq!(marked(&s), vec!["QUICK"]);
    }

    #[test]
    fn multiple_terms_both_marked() {
        let s = extract(
            "the quick brown fox jumps over the lazy dog",
            &["quick", "lazy"],
            &Options::default(),
        );
        assert_eq!(marked(&s), vec!["quick", "lazy"]);
        assert_ranges_valid(&s);
    }

    #[test]
    fn window_truncation_flags_on_both_sides() {
        let text = "prefix ".repeat(20) + "MATCH in middle " + &"suffix ".repeat(20);
        let s = extract(&text, &["MATCH"], &opts_small());
        assert!(s.truncated_start);
        assert!(s.truncated_end);
        assert_eq!(marked(&s), vec!["MATCH"]);
    }

    #[test]
    fn match_at_start_not_truncated_left() {
        let s = extract(
            "MATCH right at the start of this paragraph",
            &["match"],
            &opts_small(),
        );
        assert!(!s.truncated_start);
        assert_eq!(s.ranges[0].0, 0);
    }

    #[test]
    fn no_match_returns_head_without_ranges() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let s = extract(text, &["nomatch"], &opts_small());
        assert!(s.ranges.is_empty());
        assert!(s.window.starts_with("alpha"));
    }

    #[test]
    fn overlapping_terms_coalesce() {
        let s = extract("the RUSTC compiler", &["rust", "rustc"], &opts_small());
        assert_eq!(marked(&s), vec!["RUSTC"]);
        assert_ranges_valid(&s);
    }

    #[test]
    fn utf8_boundaries_hold_with_multibyte_text() {
        let text = "café café café café café café café café café café";
        let s = extract(text, &["café"], &opts_small());
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
        let s = extract(&text, &["LONGMATCHTERMTEXT"], &opts_small());
        assert_eq!(marked(&s), vec!["LONGMATCHTERMTEXT"]);
    }

    #[test]
    fn empty_query_term_ignored() {
        let s = extract("hello world", &["", "world"], &opts_small());
        assert_eq!(marked(&s), vec!["world"]);
    }

    #[test]
    fn window_around_basic() {
        let text = "prefix ".repeat(20) + "NEEDLE" + &" suffix".repeat(20);
        let at = text.find("NEEDLE").unwrap();
        let s = window_around(&text, (at, at + 6), &opts_small());
        assert_eq!(marked(&s), vec!["NEEDLE"]);
        assert!(s.truncated_start && s.truncated_end);
        assert_ranges_valid(&s);
    }

    #[test]
    fn window_around_clamps_out_of_bounds() {
        let s = window_around("tiny", (2, 999), &opts_small());
        assert_eq!(s.window, "tiny");
        assert_eq!(s.ranges, vec![(2, 4)]);
        // Fully out-of-range → no ranges, but never a panic.
        let s = window_around("tiny", (999, 1000), &opts_small());
        assert!(s.ranges.is_empty());
    }

    #[test]
    fn window_around_aligns_multibyte_boundaries() {
        let text = "ééééééééé needle ééééééééé";
        // Deliberately mis-aligned offsets inside multi-byte sequences.
        let s = window_around(text, (1, 3), &opts_small());
        assert_ranges_valid(&s);
    }

    #[test]
    fn count_occurrences_cases() {
        assert_eq!(count_occurrences("aaaa", "aaa", true), 1, "non-overlapping");
        assert_eq!(count_occurrences("abcABC", "abc", true), 1);
        assert_eq!(count_occurrences("abcABC", "abc", false), 2);
        assert_eq!(count_occurrences("", "x", true), 0);
        assert_eq!(count_occurrences("xyz", "", true), 0);
        assert_eq!(count_occurrences("no hits here", "zzz", false), 0);
        assert_eq!(count_occurrences("ab ab ab", "ab", true), 3);
    }
}
