//! Snippet extraction for search results.
//!
//! We store extracted text in the `documents_text` sidecar (zstd-compressed)
//! rather than in FTS5, so SQLite's built-in `snippet()` / `highlight()`
//! auxiliary functions aren't available (contentless FTS5 doesn't support
//! them). This module reproduces the parts we actually need in Rust: find a
//! window of context around the first match and report every query-term
//! occurrence inside that window.
//!
//! Output is *structural* — the window text plus byte ranges of the matches
//! within it — so any frontend can render highlights natively (egui builds
//! a `LayoutJob`, the CLI emits ANSI bold). Nothing here produces markup.
//!
//! Matching is ASCII-case-insensitive. That aligns with the search path,
//! which is already case-insensitive via the trigram tokenizer; Unicode
//! accent folding isn't applied at this layer — a query for `cafe` will
//! still *find* a file containing `café` (the FTS tokenizer strips
//! diacritics) but the snippet won't mark the accented occurrence. The
//! window text is returned verbatim either way.

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

/// Extract a snippet from `text` marking every occurrence of any term in
/// `terms` (ASCII-case-insensitive). With no terms or no matches, returns
/// the head of the text as the window with no ranges.
pub fn extract(text: &str, terms: &[&str], opts: &Options) -> Snippet {
    extract_folded(text, &text.to_ascii_lowercase(), terms, opts)
}

/// [`extract`] against a haystack the caller has already ASCII-folded.
///
/// The search cascade folds each document once per row and then counts,
/// searches and extracts from that one buffer; folding again here would copy
/// up to `maximum_text_size` a second time for every hit. `folded` must be
/// `text.to_ascii_lowercase()` — the fold is byte-length preserving, which is
/// what lets offsets found in it slice the original.
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

    // All positioning happens on the folded buffer; slices come from the
    // original. Both have identical byte layout because `to_ascii_lowercase`
    // only touches ASCII letters.
    let folded_bytes = folded.as_bytes();

    let mut matches: Vec<(usize, usize)> = Vec::new();
    for term in &effective_terms {
        let pattern = term.to_ascii_lowercase();
        let pbytes = pattern.as_bytes();
        let mut start = 0;
        while start + pbytes.len() <= folded_bytes.len() {
            if let Some(rel) = memfind(&folded_bytes[start..], pbytes) {
                let at = start + rel;
                matches.push((at, at + pbytes.len()));
                start = at + pbytes.len();
            } else {
                break;
            }
        }
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

/// Build a snippet window around one known match range in `text` (byte
/// offsets into `text`). Used by fuzzy full-text search, where the match
/// was located by the fuzzy matcher rather than exact term search. The
/// range is clamped and char-boundary-aligned defensively.
pub fn window_around(text: &str, range: (usize, usize), opts: &Options) -> Snippet {
    if text.is_empty() {
        return Snippet::empty();
    }
    let (mut ms, mut me) = range;
    ms = ms.min(text.len());
    me = me.clamp(ms, text.len());
    while ms > 0 && !text.is_char_boundary(ms) {
        ms -= 1;
    }
    while me < text.len() && !text.is_char_boundary(me) {
        me += 1;
    }

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
    let (hay, needle);
    let (hay_ref, needle_ref): (&[u8], &[u8]) = if case_sensitive {
        (text.as_bytes(), term.as_bytes())
    } else {
        hay = text.to_ascii_lowercase();
        needle = term.to_ascii_lowercase();
        (hay.as_bytes(), needle.as_bytes())
    };
    let mut count = 0;
    let mut start = 0;
    while start + needle_ref.len() <= hay_ref.len() {
        match memfind(&hay_ref[start..], needle_ref) {
            Some(rel) => {
                count += 1;
                start += rel + needle_ref.len();
            }
            None => break,
        }
    }
    count
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
fn coalesce_overlapping(mut v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if v.len() < 2 {
        return v;
    }
    let mut out = Vec::with_capacity(v.len());
    let mut cur = v.remove(0);
    for next in v {
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

/// Locate the first occurrence of `needle` in `hay`. A byte-level search;
/// callers have already normalized case where needed.
fn memfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
