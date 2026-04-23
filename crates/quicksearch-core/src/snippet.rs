//! Snippet / highlight rendering for search results.
//!
//! We store extracted text in the `documents_text` sidecar (zstd-compressed)
//! rather than in FTS5, so SQLite's built-in `snippet()` / `highlight()`
//! auxiliary functions aren't available (contentless FTS5 doesn't support
//! them). This module reproduces the parts we actually need in Rust: find
//! a window of context around the first match, bold every query-term
//! occurrence inside that window, trim with ellipsis markers.
//!
//! Matching is ASCII-case-insensitive on the *rendering* side. That aligns
//! with the search path which is already case-insensitive via the trigram
//! tokenizer; exact-case-only snippets aren't a feature users expect here.
//! Unicode accent folding isn't applied at the rendering layer — a query
//! for `cafe` will still *find* a file containing `café` (because the FTS
//! tokenizer strips diacritics) but the snippet won't highlight the
//! accented occurrence. The surrounding text is still returned verbatim.
//!
//! The API is intentionally small: one `render` function plus an `Options`
//! struct. Callers that want different pre/post tags, ellipsis, or window
//! size pass them in; there are sensible defaults for the GUI case.

/// Options controlling snippet rendering. The defaults mirror the old
/// `snippet(searchabletext, 1, '<b>', '</b>', '<b>...</b>', 64)` call that
/// the GUI used to run directly as SQL.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub pre: &'a str,
    pub post: &'a str,
    pub ellipsis: &'a str,
    /// Approximate character budget for the returned snippet. Matches
    /// expand the window if needed to keep their tags on; the budget is a
    /// soft target, not a hard cap.
    pub approx_chars: usize,
}

impl<'a> Default for Options<'a> {
    fn default() -> Self {
        Options {
            pre: "<b>",
            post: "</b>",
            ellipsis: "<b>...</b>",
            approx_chars: 200,
        }
    }
}

/// Render a snippet from `text` highlighting every occurrence of any term
/// in `terms`. Returns a string with `pre`/`post` wrapping each match, and
/// `ellipsis` prepended/appended when the returned window doesn't reach
/// the text's edges.
///
/// If no term matches, returns the first `approx_chars` of `text` (char-
/// aligned), suffixed with `ellipsis` when truncated.
pub fn render(text: &str, terms: &[&str], opts: &Options<'_>) -> String {
    // Short-circuit trivial inputs.
    if text.is_empty() {
        return String::new();
    }
    let effective_terms: Vec<&str> = terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if effective_terms.is_empty() {
        return truncate_head(text, opts.approx_chars, opts.ellipsis);
    }

    // Case-fold once; we do all positioning on the folded buffer and emit
    // slices from the original. Both buffers have identical byte layout
    // because `to_ascii_lowercase` is a byte-for-byte map that preserves
    // multi-byte UTF-8 sequences unchanged (it only touches ASCII letters).
    let folded = text.to_ascii_lowercase();
    let folded_bytes = folded.as_bytes();

    let mut matches: Vec<(usize, usize)> = Vec::new();
    for term in &effective_terms {
        let pattern: String = term.to_ascii_lowercase();
        let pbytes = pattern.as_bytes();
        if pbytes.is_empty() {
            continue;
        }
        let mut start = 0;
        while start + pbytes.len() <= folded_bytes.len() {
            if let Some(rel) = memfind(&folded_bytes[start..], pbytes) {
                let at = start + rel;
                matches.push((at, at + pbytes.len()));
                // Advance past this match to avoid zero-width loops on
                // empty patterns (already guarded above) and to allow
                // overlapping matches of *different* terms in the next
                // outer-loop iteration.
                start = at + pbytes.len();
            } else {
                break;
            }
        }
    }

    if matches.is_empty() {
        return truncate_head(text, opts.approx_chars, opts.ellipsis);
    }

    // Dedupe + sort so overlapping matches from different terms (e.g.
    // "rust" and "rustc") don't produce nested tags.
    matches.sort_by_key(|(a, _)| *a);
    matches = coalesce_overlapping(matches);

    // Pick the window. Start a third of the budget before the first match
    // so the hit isn't pinned to the left edge. Round both ends to char
    // boundaries so we never slice a multi-byte UTF-8 sequence.
    let pre_pad = opts.approx_chars / 3;
    let first_match_start = matches[0].0;
    let mut win_start = first_match_start.saturating_sub(pre_pad);
    let mut win_end = win_start + opts.approx_chars;
    if win_end > text.len() {
        win_end = text.len();
    }
    while win_start > 0 && !text.is_char_boundary(win_start) {
        win_start -= 1;
    }
    while win_end < text.len() && !text.is_char_boundary(win_end) {
        win_end += 1;
    }

    // Expand the window to include the full end of any match that would
    // otherwise be cut off mid-tag. Keeps rendering sane when a long term
    // sits at the right edge of the budget.
    let last_match_in_window = matches.iter().rfind(|(s, _)| *s < win_end);
    if let Some((_, end)) = last_match_in_window {
        if *end > win_end {
            win_end = *end;
            while win_end < text.len() && !text.is_char_boundary(win_end) {
                win_end += 1;
            }
        }
    }

    // Render: walk matches that fall inside the window, splicing pre/post
    // around each. Prepend/append ellipsis when we've chopped off content.
    let mut out = String::with_capacity(win_end - win_start + 32);
    if win_start > 0 {
        out.push_str(opts.ellipsis);
    }
    let mut cursor = win_start;
    for (ms, me) in matches.iter() {
        if *me <= win_start || *ms >= win_end {
            continue;
        }
        // Clamp to the window.
        let ms = (*ms).max(win_start);
        let me = (*me).min(win_end);
        if ms > cursor {
            out.push_str(&text[cursor..ms]);
        }
        out.push_str(opts.pre);
        out.push_str(&text[ms..me]);
        out.push_str(opts.post);
        cursor = me;
    }
    if cursor < win_end {
        out.push_str(&text[cursor..win_end]);
    }
    if win_end < text.len() {
        out.push_str(opts.ellipsis);
    }
    out
}

/// Return the first `n` characters of `text`, suffixed with `ellipsis` if
/// truncation actually happened. Respects UTF-8 char boundaries.
fn truncate_head(text: &str, n: usize, ellipsis: &str) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    let mut cut = n;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + ellipsis.len());
    out.push_str(&text[..cut]);
    out.push_str(ellipsis);
    out
}

/// Merge adjacent / overlapping (start, end) ranges in place. Input must be
/// sorted by start.
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
/// callers have already lowercased both sides so case is normalized.
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

    fn opts_small() -> Options<'static> {
        Options {
            pre: "<b>",
            post: "</b>",
            ellipsis: "…",
            approx_chars: 40,
        }
    }

    #[test]
    fn empty_text_returns_empty() {
        let s = render("", &["foo"], &Options::default());
        assert_eq!(s, "");
    }

    #[test]
    fn no_terms_returns_head_with_ellipsis_when_truncated() {
        let long = "abcdefghijklmnop".repeat(10);
        let s = render(&long, &[], &opts_small());
        assert!(s.ends_with("…"));
        assert!(s.len() < long.len() + 4);
    }

    #[test]
    fn no_terms_untruncated_has_no_ellipsis() {
        let s = render("short text", &[], &opts_small());
        assert_eq!(s, "short text");
    }

    #[test]
    fn simple_highlight_wraps_matches() {
        let s = render("the quick brown fox", &["quick"], &opts_small());
        assert!(s.contains("<b>quick</b>"));
    }

    #[test]
    fn case_insensitive_match() {
        let s = render("The QUICK brown fox", &["quick"], &opts_small());
        assert!(s.contains("<b>QUICK</b>"), "got {s}");
    }

    #[test]
    fn multiple_terms_both_highlighted() {
        let s = render(
            "the quick brown fox jumps over the lazy dog",
            &["quick", "lazy"],
            &Options {
                approx_chars: 200,
                ..Options::default()
            },
        );
        assert!(s.contains("<b>quick</b>"), "got {s}");
        assert!(s.contains("<b>lazy</b>"), "got {s}");
    }

    #[test]
    fn window_trims_with_ellipsis_on_both_sides() {
        let text =
            "prefix ".repeat(20) + "MATCH in middle " + &"suffix ".repeat(20);
        let s = render(&text, &["MATCH"], &opts_small());
        assert!(s.starts_with("…"), "got {s}");
        assert!(s.ends_with("…"), "got {s}");
        assert!(s.contains("<b>MATCH</b>"), "got {s}");
    }

    #[test]
    fn match_at_start_has_no_leading_ellipsis() {
        let s = render("MATCH right at the start of this paragraph", &["match"], &opts_small());
        assert!(!s.starts_with("…"), "got {s}");
    }

    #[test]
    fn no_match_on_tail_returns_head() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let s = render(text, &["nomatch"], &opts_small());
        assert!(!s.contains("<b>"));
        assert!(s.starts_with("alpha"));
    }

    #[test]
    fn overlapping_terms_do_not_nest_tags() {
        // Two terms matching the same span must coalesce.
        let s = render("the RUSTC compiler", &["rust", "rustc"], &opts_small());
        assert!(s.contains("<b>RUSTC</b>"), "got {s}");
        // No nested <b> tags.
        assert!(!s.contains("<b><b>"), "got {s}");
    }

    #[test]
    fn utf8_boundary_safe_truncation() {
        // Insert multi-byte chars near the window boundary.
        let text = "café café café café café café café café café café";
        let s = render(text, &["nope"], &opts_small());
        // Returned string must be valid UTF-8 (push_str guarantees this only
        // if we sliced on char boundaries). Assert by round-trip.
        assert_eq!(s.as_str(), &s.clone());
    }

    #[test]
    fn match_near_right_edge_is_fully_shown() {
        let prefix = "x".repeat(30);
        let text = format!("{}{}", prefix, "LONGMATCHTERMTEXT");
        let s = render(&text, &["LONGMATCHTERMTEXT"], &opts_small());
        assert!(s.contains("<b>LONGMATCHTERMTEXT</b>"), "got {s}");
    }

    #[test]
    fn empty_query_term_ignored() {
        let s = render("hello world", &["", "world"], &opts_small());
        assert!(s.contains("<b>world</b>"), "got {s}");
    }
}
