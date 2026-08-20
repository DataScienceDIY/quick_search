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
///
/// Returns the window together with **how many occurrences it found**, before
/// touching ranges are coalesced. For a single term that number is exactly
/// what [`count_occurrences`] against `folded` would return, which is what
/// lets the full-text pass take its case-insensitive count from here instead
/// of sweeping the body a second time to compute it.
///
/// Blank terms are dropped, but a term that merely *has* surrounding
/// whitespace is searched as given: trimming it here would count and
/// highlight a different string than the pattern's own counters do.
pub fn extract_folded(
    text: &str,
    folded: &str,
    terms: &[&str],
    opts: &Options,
) -> (Snippet, usize) {
    debug_assert_eq!(folded.len(), text.len(), "ASCII folding preserves length");
    if text.is_empty() {
        return (Snippet::empty(), 0);
    }
    let effective_terms: Vec<&str> = terms
        .iter()
        .copied()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if effective_terms.is_empty() {
        return (head_window(text, opts.approx_chars), 0);
    }

    // `memmem` rather than `str::match_indices`: both find non-overlapping
    // occurrences, but std's Two-Way searcher has no vector prefilter and a
    // full-text row scans a whole document body. See `benches/search.rs`,
    // group `substring`.
    // The count has to see every occurrence, so the walk runs to the end of the
    // body either way. What does *not* have to happen is keeping them: the
    // window is fixed by the first match, and everything starting past its
    // right edge is discarded a few lines below. Storing them all meant a `Vec`
    // proportional to the match count and then a sort over it — for a term
    // occurring thousands of times in one file, which is an ordinary minified
    // bundle or log, that is hundreds of kilobytes and an O(n log n) sort per
    // candidate row, to render a 600-byte window.
    //
    // Bounded only for a single term, which is every caller the cascade makes
    // (`cascade::text_snippet` passes one). With several, two different terms
    // can coalesce across the edge and no single-pass bound sees it, so they
    // are collected in full rather than approximately.
    let pre_pad = opts.approx_chars / 3;
    let bounded = effective_terms.len() == 1;
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut found = 0usize;
    // The right edge, once the first match has fixed it. Computed exactly as
    // the window is computed below, so "kept" and "rendered" cannot disagree.
    let mut keep_below: Option<usize> = None;

    for term in &effective_terms {
        // Borrow when the term is already folded, which is the case for every
        // call the search cascade makes: this runs once per candidate row, and
        // an unconditional `to_ascii_lowercase` is an allocation per row to
        // rebuild a string the pattern already holds.
        let pattern: std::borrow::Cow<'_, str> = if term.bytes().any(|b| b.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(term.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(*term)
        };
        // The end of the last kept match, so a chain of touching occurrences
        // (`abab…` for term `ab`) that starts inside the window and continues
        // past it stays intact — `coalesce_overlapping` merges those, and the
        // expansion step below is entitled to follow the merged range out.
        let mut chain_end = 0usize;
        for at in memchr::memmem::find_iter(folded.as_bytes(), pattern.as_bytes()) {
            // Before any dropping: this is the occurrence count, and two
            // occurrences that happen to abut are two hits for ranking even
            // though they are one highlight for painting.
            found += 1;
            if bounded {
                let bound = *keep_below.get_or_insert_with(|| {
                    let mut end = (at.saturating_sub(pre_pad) + opts.approx_chars).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end += 1;
                    }
                    end
                });
                if at >= bound && at > chain_end {
                    // Past the edge and not chained to anything kept. Keep
                    // counting — that is the whole rest of the walk — but stop
                    // storing.
                    continue;
                }
            }
            chain_end = at + pattern.len();
            matches.push((at, chain_end));
        }
    }

    if matches.is_empty() {
        return (head_window(text, opts.approx_chars), 0);
    }

    matches.sort_by_key(|(a, _)| *a);
    let matches = coalesce_overlapping(matches);

    // Pick the window. Start a third of the budget before the first match
    // so the hit isn't pinned to the left edge; round both ends to char
    // boundaries so we never slice a multi-byte UTF-8 sequence.
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

    (
        Snippet {
            window: text[win_start..win_end].to_string(),
            ranges,
            truncated_start: win_start > 0,
            truncated_end: win_end < text.len(),
        },
        found,
    )
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
        extract_folded(text, &text.to_ascii_lowercase(), terms, opts).0
    }

    /// The occurrence count alongside the window — the half the full-text
    /// pass consumes.
    fn extract_counted(text: &str, terms: &[&str], opts: &Options) -> (Snippet, usize) {
        extract_folded(text, &text.to_ascii_lowercase(), terms, opts)
    }

    fn opts_small() -> Options {
        Options { approx_chars: 40 }
    }

    /// The count `extract_folded` hands back is what the full-text pass ranks
    /// on, so it must be occurrences — not the highlights they coalesce into.
    #[test]
    fn extract_folded_counts_occurrences_not_ranges() {
        let text = "abab and ab";
        let (s, n) = extract_counted(text, &["ab"], &opts_small());
        assert_eq!(n, 3, "three occurrences");
        assert_eq!(
            n,
            count_occurrences(text, "ab", true),
            "must agree with the counter the pass used to call"
        );
        assert_eq!(s.ranges.len(), 2, "the abutting pair paints as one range");
    }

    /// The pass takes `count_folded`'s answer from here now; anything that
    /// made the two disagree would silently change which rows survive.
    #[test]
    fn extract_folded_count_agrees_with_count_occurrences() {
        let cases: &[(&str, &str)] = &[
            ("no hits at all", "zzz"),
            ("one hit here", "hit"),
            ("Hit hit HIT", "hit"),
            ("aaaa", "aa"),
            ("", "x"),
            ("short", "much longer than the haystack"),
            ("ünïcode ünïcode", "ünïcode"),
        ];
        for (text, term) in cases {
            let folded = text.to_ascii_lowercase();
            let (_, n) = extract_folded(text, &folded, &[term], &opts_small());
            assert_eq!(
                n,
                count_occurrences(&folded, &term.to_ascii_lowercase(), true),
                "count mismatch for {:?} in {:?}",
                term,
                text
            );
        }
    }

    /// An uppercase needle takes the owning branch of the fold; a needle that
    /// is already folded takes the borrowing one. Both must find the same
    /// matches, since the whole point of the borrow is that it changes
    /// nothing but the allocation.
    #[test]
    fn extract_folded_needle_case_does_not_change_matches() {
        let text = "The Needle and the needle";
        let folded = text.to_ascii_lowercase();
        let (upper, n_upper) = extract_folded(text, &folded, &["Needle"], &opts_small());
        let (lower, n_lower) = extract_folded(text, &folded, &["needle"], &opts_small());
        assert_eq!(n_upper, 2);
        assert_eq!(n_upper, n_lower);
        assert_eq!(upper.ranges, lower.ranges);
        assert_eq!(upper.window, lower.window);
    }

    /// A blank term is dropped rather than searched: an empty needle matches
    /// at every byte offset, which would rank a document by its length.
    #[test]
    fn extract_folded_ignores_blank_terms() {
        let text = "some text";
        let folded = text.to_ascii_lowercase();
        for term in ["", "   ", "\t"] {
            let (s, n) = extract_folded(text, &folded, &[term], &opts_small());
            assert_eq!(n, 0, "blank term {:?} counted", term);
            assert!(s.ranges.is_empty(), "blank term {:?} highlighted", term);
        }
    }

    /// A term with surrounding space is searched as given. It used to be
    /// trimmed, which counted and highlighted a different string than the
    /// pattern's own counters did.
    #[test]
    fn extract_folded_does_not_trim_a_padded_term() {
        let text = "needle needlework";
        let folded = text.to_ascii_lowercase();
        let (_, n) = extract_folded(text, &folded, &["needle "], &opts_small());
        assert_eq!(n, 1, "only the occurrence followed by a space");
        assert_eq!(n, count_occurrences(&folded, "needle ", true));
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

    /// Occurrences past the window's right edge are counted but not kept, and
    /// that has to be invisible from the outside.
    ///
    /// The count is what the full-text pass ranks on, so it must still see the
    /// whole body; the ranges are what gets painted, so they must still be
    /// exactly the occurrences inside the window. A bound that leaked into
    /// either would be a ranking change or a missing highlight.
    #[test]
    fn occurrences_past_the_window_are_counted_but_not_kept() {
        // 400 occurrences, evenly spread, far more than a 40-byte window holds.
        let unit = "needle filler filler ";
        let text = unit.repeat(400);
        let (snip, found) = extract_counted(&text, &["needle"], &opts_small());

        assert_eq!(found, 400, "every occurrence must still be counted");
        assert_ranges_valid(&snip);
        assert!(!snip.ranges.is_empty());
        assert!(snip.truncated_end, "there is a great deal more body");

        // Every range is a real occurrence, and every occurrence that falls
        // inside the window has a range.
        for &(a, b) in &snip.ranges {
            assert_eq!(&snip.window[a..b], "needle");
        }
        let win_at = text.find(&snip.window).expect("the window is a slice of the text");
        let expected = memchr::memmem::find_iter(text.as_bytes(), b"needle")
            .filter(|at| *at >= win_at && *at < win_at + snip.window.len())
            .count();
        assert_eq!(
            snip.ranges.len(),
            expected,
            "every occurrence inside the window must be marked"
        );
    }

    /// The case the chain rule exists for: touching occurrences coalesce into
    /// one range, and a chain that starts inside the window can run past its
    /// right edge. Dropping the moment the edge is crossed would cut the
    /// highlight short.
    #[test]
    fn a_coalescing_chain_is_not_cut_at_the_window_edge() {
        // One unbroken run of `ab`, far longer than the window.
        let text = "ab".repeat(400);
        let (snip, found) = extract_counted(&text, &["ab"], &opts_small());

        assert_eq!(found, 400, "non-overlapping occurrences, all counted");
        assert_ranges_valid(&snip);
        assert_eq!(snip.ranges.len(), 1, "one chain, one highlight");

        // The load-bearing assertion, and it has to be about the window's
        // *size*. The whole run coalesces into one range, and the expansion
        // step then grows the window to cover it — so an unbroken chain
        // legitimately produces a window the length of the text, not the
        // 40-byte budget. Dropping the chain rule cuts the run at the budget
        // and yields a 40-byte window instead, which asserting only
        // "range == whole window" cannot tell apart, because both are.
        assert_eq!(
            snip.window.len(),
            text.len(),
            "the window must grow to cover the coalesced run"
        );
        assert!(!snip.truncated_end, "nothing is left past a full-length window");
        assert_eq!(snip.ranges[0], (0, text.len()), "one highlight over the lot");
    }

    /// Several terms disable the bound, because two different terms can
    /// coalesce across the edge and a per-term walk cannot see it. Pin that the
    /// multi-term path still produces the full, correct answer.
    #[test]
    fn several_terms_still_coalesce_across_the_edge() {
        let text = format!("{}{}", "x".repeat(10), "abc".repeat(200));
        let (snip, found) = extract_counted(&text, &["ab", "bc"], &opts_small());
        assert_eq!(found, 400, "200 of each term");
        assert_ranges_valid(&snip);
        assert_eq!(snip.ranges.len(), 1, "the two terms interleave into one run");
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
