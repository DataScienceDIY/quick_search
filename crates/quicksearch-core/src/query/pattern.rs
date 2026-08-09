//! Compiled matchers for the cascade term and the `regex:` keyword.
//!
//! A term with an unquoted `*` compiles to a small regex (every literal
//! chunk escaped, stars joined with `.*`), so wildcards and `regex:` share
//! one linear-time matching engine. Terms without stars stay on the
//! [`Literal`](TermPattern::Literal) path — plain string operations.
//!
//! `.` never matches `\n`, so a star cannot span lines of extracted text —
//! a `*` bridging a whole document would produce absurd match ranges and
//! page-sized snippets. Names and paths contain no newlines, so the rule
//! only shows up in content matching.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use super::translator::TranslateError;
use crate::snippet;

/// Compile-time memory cap for user-supplied and derived regexes. Keeps a
/// hostile pattern (`a{1000000}{1000}` and friends) from ballooning the
/// compiled program; matching itself is linear-time by construction.
const REGEX_SIZE_LIMIT: usize = 4 << 20;

/// Occurrence counts saturate here, matching `count_frac` in the cascade.
const COUNT_CAP: usize = 1000;

/// The cascade term, compiled once at split time.
#[derive(Debug, Clone, Default)]
pub enum TermPattern {
    /// No matchable content: an empty term, or only stars (`*`, `**`).
    /// Matches nothing — a bare `*` must not become a scan of everything.
    #[default]
    Empty,
    /// A star-free term; plain string operations.
    Literal(LiteralPattern),
    /// A term with at least one active wildcard.
    Wildcard(WildcardPattern),
}

#[derive(Debug, Clone)]
pub struct LiteralPattern {
    text: String,
    folded: String,
}

#[derive(Debug, Clone)]
pub struct WildcardPattern {
    /// Literal chunks between stars, in order. Never empty, and no chunk
    /// is empty: edge stars are folded into the compiled regexes, doubled
    /// stars collapse.
    segments: Vec<String>,
    /// Unanchored search regexes with non-greedy joins — leftmost-shortest
    /// match, which is what a snippet window wants.
    search_cs: Regex,
    search_ci: Regex,
    /// Anchored (`^…$`) regexes for whole-field matching (rank tiers 1/2).
    anchored_cs: Regex,
    anchored_ci: Regex,
}

/// One piece of the search phrase as split out of the token stream.
/// `glob` is true only for plain unquoted words — quoted phrases and
/// reassembled `key:value` text keep their stars literal.
#[derive(Debug, Clone)]
pub struct TermPart {
    pub text: String,
    pub glob: bool,
}

/// A chunk stream: literal text interleaved with active stars.
enum Chunk {
    Lit(String),
    Star,
}

impl TermPattern {
    /// Compile the joined term parts. Parts are joined with a single space,
    /// exactly like the display term (`parts.join(" ")`).
    pub fn build(parts: &[TermPart]) -> Result<TermPattern, TranslateError> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let push_lit = |chunks: &mut Vec<Chunk>, s: &str| {
            if s.is_empty() {
                return;
            }
            if let Some(Chunk::Lit(prev)) = chunks.last_mut() {
                prev.push_str(s);
            } else {
                chunks.push(Chunk::Lit(s.to_string()));
            }
        };
        for (idx, part) in parts.iter().enumerate() {
            if idx > 0 {
                push_lit(&mut chunks, " ");
            }
            if part.glob {
                let mut first = true;
                for piece in part.text.split('*') {
                    if !first && !matches!(chunks.last(), Some(Chunk::Star)) {
                        chunks.push(Chunk::Star);
                    }
                    first = false;
                    push_lit(&mut chunks, piece);
                }
            } else {
                push_lit(&mut chunks, &part.text);
            }
        }

        let leading = matches!(chunks.first(), Some(Chunk::Star));
        let trailing = chunks.len() > 1 && matches!(chunks.last(), Some(Chunk::Star));
        let has_star = chunks.iter().any(|c| matches!(c, Chunk::Star));
        let segments: Vec<String> = chunks
            .into_iter()
            .filter_map(|c| match c {
                Chunk::Lit(s) => Some(s),
                Chunk::Star => None,
            })
            .collect();

        if segments.is_empty() {
            // "" or stars only.
            return Ok(TermPattern::Empty);
        }
        if !has_star {
            let Some(text) = segments.into_iter().next() else {
                return Ok(TermPattern::Empty);
            };
            let folded = text.to_ascii_lowercase();
            return Ok(TermPattern::Literal(LiteralPattern { text, folded }));
        }

        let escaped: Vec<String> = segments.iter().map(|s| regex::escape(s)).collect();
        let compile = |src: &str, ci: bool| -> Result<Regex, TranslateError> {
            RegexBuilder::new(src)
                .case_insensitive(ci)
                .size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map_err(|e| TranslateError::BadRegex(e.to_string()))
        };
        // Edge stars are dropped from the search form — under substring
        // semantics a leading/trailing `.*?` adds nothing.
        let search_src = escaped.join(".*?");
        // The anchored form keeps them: `*foo` must whole-match "myfoo".
        let anchored_src = format!(
            "^{}{}{}$",
            if leading { ".*" } else { "" },
            escaped.join(".*"),
            if trailing { ".*" } else { "" },
        );
        Ok(TermPattern::Wildcard(WildcardPattern {
            search_cs: compile(&search_src, false)?,
            search_ci: compile(&search_src, true)?,
            anchored_cs: compile(&anchored_src, false)?,
            anchored_ci: compile(&anchored_src, true)?,
            segments,
        }))
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, TermPattern::Empty)
    }

    pub fn is_wildcard(&self) -> bool {
        matches!(self, TermPattern::Wildcard(_))
    }

    /// The literal text, when the term has no wildcard. SQL builders branch
    /// on this to keep the original single-`LIKE`/phrase-`MATCH` shapes.
    pub fn literal(&self) -> Option<&str> {
        match self {
            TermPattern::Literal(l) => Some(&l.text),
            _ => None,
        }
    }

    /// Literal chunks between wildcards (the whole term when literal).
    pub fn segments(&self) -> &[String] {
        match self {
            TermPattern::Empty => &[],
            TermPattern::Literal(l) => std::slice::from_ref(&l.text),
            TermPattern::Wildcard(w) => &w.segments,
        }
    }

    /// Characters of literal (non-star) content — the trigram floor and
    /// path-tier switch count these.
    pub fn literal_char_count(&self) -> usize {
        self.segments().iter().map(|s| s.chars().count()).sum()
    }

    /// Does the pattern match the entire field?
    pub fn whole_match(&self, text: &str, case_insensitive: bool) -> bool {
        match self {
            TermPattern::Empty => false,
            TermPattern::Literal(l) => {
                if case_insensitive {
                    text.eq_ignore_ascii_case(&l.text)
                } else {
                    text == l.text
                }
            }
            TermPattern::Wildcard(w) => {
                let re = if case_insensitive {
                    &w.anchored_ci
                } else {
                    &w.anchored_cs
                };
                re.is_match(text)
            }
        }
    }

    /// Leftmost match as a byte range. Literal folding is ASCII-only and
    /// byte-length preserving, so folded offsets are valid in the original —
    /// the same invariant the cascade has always relied on.
    pub fn find_first(&self, text: &str, case_insensitive: bool) -> Option<Range<usize>> {
        match self {
            TermPattern::Empty => None,
            TermPattern::Literal(l) => {
                let pos = if case_insensitive {
                    text.to_ascii_lowercase().find(&l.folded)?
                } else {
                    text.find(&l.text)?
                };
                Some(pos..pos + l.text.len())
            }
            TermPattern::Wildcard(w) => {
                let re = if case_insensitive {
                    &w.search_ci
                } else {
                    &w.search_cs
                };
                re.find(text).map(|m| m.range())
            }
        }
    }

    /// Case-insensitive [`TermPattern::find_first`] against an already-folded
    /// haystack. Folding is byte-length preserving, so the returned range is
    /// valid in the unfolded original too.
    pub fn find_first_folded(&self, folded: &str) -> Option<Range<usize>> {
        match self {
            TermPattern::Empty => None,
            TermPattern::Literal(l) => {
                let pos = folded.find(&l.folded)?;
                Some(pos..pos + l.text.len())
            }
            // The regex engine folds as it matches, so it needs no help.
            TermPattern::Wildcard(w) => w.search_ci.find(folded).map(|m| m.range()),
        }
    }

    /// Case-insensitive [`TermPattern::count`] against an already-folded
    /// haystack. See [`TermPattern::find_first_folded`].
    pub fn count_folded(&self, folded: &str) -> usize {
        match self {
            TermPattern::Empty => 0,
            // Both sides are already folded, so an exact scan *is* the
            // case-insensitive one.
            TermPattern::Literal(l) => snippet::count_occurrences(folded, &l.folded, true),
            TermPattern::Wildcard(w) => w.search_ci.find_iter(folded).take(COUNT_CAP).count(),
        }
    }

    /// Non-overlapping occurrence count, capped at 1000 (the cascade's
    /// `count_frac` saturates there anyway).
    pub fn count(&self, text: &str, case_insensitive: bool) -> usize {
        match self {
            TermPattern::Empty => 0,
            TermPattern::Literal(l) => snippet::count_occurrences(text, &l.text, !case_insensitive),
            TermPattern::Wildcard(w) => {
                let re = if case_insensitive {
                    &w.search_ci
                } else {
                    &w.search_cs
                };
                re.find_iter(text).take(COUNT_CAP).count()
            }
        }
    }
}

/// A compiled `regex:` query. Case-insensitive by default (override with an
/// inline `(?-i:…)`); `multi_line` makes `^`/`$` per-line over extracted
/// text, which is what they mean in a search box.
#[derive(Debug, Clone)]
pub struct RegexQuery {
    pub source: String,
    re: Regex,
}

impl RegexQuery {
    pub fn new(source: &str) -> Result<RegexQuery, TranslateError> {
        let re = RegexBuilder::new(source)
            .case_insensitive(true)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| TranslateError::BadRegex(e.to_string()))?;
        // The regex analog of the bare-`*` rule, but loud: the user typed an
        // explicit keyword, so tell them instead of matching every file.
        if re.is_match("") {
            return Err(TranslateError::BadRegex(format!(
                "'{}' can match the empty string and would match every file",
                source
            )));
        }
        Ok(RegexQuery {
            source: source.to_string(),
            re,
        })
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.re.is_match(text)
    }

    pub fn find_first(&self, text: &str) -> Option<Range<usize>> {
        self.re.find(text).map(|m| m.range())
    }

    /// Non-overlapping occurrence count, capped at 1000.
    pub fn count(&self, text: &str) -> usize {
        self.re.find_iter(text).take(COUNT_CAP).count()
    }
}

/// Cap a match range at `max_len` bytes (aligned back to a char boundary)
/// before handing it to `snippet::window_around`. A greedy user regex can
/// legitimately match megabytes of a minified file; the snippet window
/// wants the start of that, not all of it.
pub fn clamp_match_range(text: &str, range: Range<usize>, max_len: usize) -> Range<usize> {
    let mut end = range.end.min(range.start + max_len);
    while end > range.start && !text.is_char_boundary(end) {
        end -= 1;
    }
    range.start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(text: &str, glob: bool) -> TermPart {
        TermPart {
            text: text.into(),
            glob,
        }
    }

    fn wildcard(parts: &[TermPart]) -> WildcardPattern {
        match TermPattern::build(parts).unwrap() {
            TermPattern::Wildcard(w) => w,
            other => panic!("expected wildcard, got {:?}", other),
        }
    }

    #[test]
    fn starless_parts_build_a_literal() {
        let p = TermPattern::build(&[part("hello", false), part("world", true)]).unwrap();
        assert_eq!(p.literal(), Some("hello world"));
        assert!(!p.is_wildcard());
    }

    #[test]
    fn empty_and_star_only_terms_match_nothing() {
        for parts in [
            vec![],
            vec![part("", false)],
            vec![part("*", true)],
            vec![part("**", true)],
        ] {
            let p = TermPattern::build(&parts).unwrap();
            assert!(p.is_empty(), "{:?}", parts);
            assert!(!p.whole_match("anything", true));
            assert!(p.find_first("anything", true).is_none());
            assert_eq!(p.count("anything", true), 0);
        }
    }

    #[test]
    fn quoted_star_stays_literal() {
        // A quoted "*" arrives with glob = false.
        let p = TermPattern::build(&[part("a*b", false)]).unwrap();
        assert_eq!(p.literal(), Some("a*b"));
        assert!(p.find_first("xa*by", false).is_some());
        assert!(p.find_first("aXb", false).is_none());
    }

    #[test]
    fn segment_shapes() {
        // Edge stars vanish into the anchors: `*foo` whole-matches any
        // suffix `foo`, `foo*` any prefix.
        let p = TermPattern::build(&[part("*foo", true)]).unwrap();
        assert_eq!(p.segments(), ["foo"]);
        assert!(p.whole_match("myfoo", false));
        assert!(!p.whole_match("foomy", false));

        let p = TermPattern::build(&[part("foo*", true)]).unwrap();
        assert_eq!(p.segments(), ["foo"]);
        assert!(p.whole_match("foomy", false));
        assert!(!p.whole_match("myfoo", false));

        let w = wildcard(&[part("f*o*o", true)]);
        assert_eq!(w.segments, ["f", "o", "o"]);

        // Doubled stars collapse.
        let w = wildcard(&[part("f**o", true)]);
        assert_eq!(w.segments, ["f", "o"]);

        // The implicit joining space is literal content.
        let w = wildcard(&[part("a*", true), part("b", false)]);
        assert_eq!(w.segments, ["a", " b"]);

        // `* *` — the joining space between two stars is interior literal
        // content, so this is a real (if odd) pattern, not Empty.
        let p = TermPattern::build(&[part("*", true), part("*", true)]).unwrap();
        assert_eq!(p.segments(), [" "]);
        assert!(p.whole_match("a b", false));
        assert!(!p.whole_match("ab", false));

        let p = TermPattern::build(&[part("*x", true), part("y*", true)]).unwrap();
        assert_eq!(p.segments(), ["x y"]);
        assert!(p.whole_match("ax yb", false));
    }

    #[test]
    fn whole_match_uses_anchors() {
        let p = TermPattern::build(&[part("*.txt", true)]).unwrap();
        assert!(p.whole_match("notes.txt", false));
        assert!(p.whole_match("NOTES.TXT", true));
        assert!(!p.whole_match("NOTES.TXT", false));
        assert!(!p.whole_match("notes.txt.bak", false));

        let p = TermPattern::build(&[part("rep*rt", true)]).unwrap();
        assert!(p.whole_match("report", false));
        assert!(!p.whole_match("report2024", false));
    }

    #[test]
    fn find_first_is_leftmost_shortest() {
        let p = TermPattern::build(&[part("a*b", true)]).unwrap();
        // Leftmost-first with a lazy join: starts at 0, ends at the first b.
        assert_eq!(p.find_first("aXXbYYb", false), Some(0..4));
        // Case-insensitive variant.
        assert_eq!(p.find_first("AXXB", true), Some(0..4));
        assert_eq!(p.find_first("AXXB", false), None);
    }

    #[test]
    fn star_does_not_cross_newlines() {
        let p = TermPattern::build(&[part("foo*bar", true)]).unwrap();
        assert!(p.find_first("foo bar", false).is_some());
        assert!(p.find_first("foo\nbar", false).is_none());
    }

    #[test]
    fn utf8_boundaries_in_segments_and_haystacks() {
        let p = TermPattern::build(&[part("café*menu", true)]).unwrap();
        let hay = "le café du menu";
        let r = p.find_first(hay, false).unwrap();
        assert_eq!(&hay[r], "café du menu");
        // Case-insensitive over non-ASCII haystack: offsets stay valid.
        let hay = "LE CAFÉ DU MENU";
        let r = p.find_first(hay, true).unwrap();
        assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end));
    }

    #[test]
    fn count_is_nonoverlapping_and_capped() {
        let p = TermPattern::build(&[part("a*b", true)]).unwrap();
        assert_eq!(p.count("ab ab ab", false), 3);
        let many = "ab ".repeat(2000);
        assert_eq!(p.count(&many, false), 1000);
    }

    #[test]
    fn literal_parity_with_string_ops() {
        let p = TermPattern::build(&[part("Report", false)]).unwrap();
        assert!(p.whole_match("Report", false));
        assert!(!p.whole_match("report", false));
        assert!(p.whole_match("report", true));
        assert_eq!(p.find_first("my Report.pdf", false), Some(3..9));
        assert_eq!(p.find_first("my report.pdf", true), Some(3..9));
        assert_eq!(p.find_first("my report.pdf", false), None);
        assert_eq!(p.count("report Report", false), 1);
        assert_eq!(p.count("report Report", true), 2);
    }

    #[test]
    fn regex_defaults_case_insensitive_with_optout() {
        let r = RegexQuery::new("foo\\d+").unwrap();
        assert!(r.is_match("FOO123"));
        let r = RegexQuery::new("(?-i:FOO)\\d+").unwrap();
        assert!(r.is_match("FOO1"));
        assert!(!r.is_match("foo1"));
    }

    #[test]
    fn regex_multiline_anchors() {
        let r = RegexQuery::new("^total:").unwrap();
        assert!(r.is_match("line one\ntotal: 5"));
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_panic() {
        for src in ["[", "(", "a{2,1}", "(?P<)"] {
            assert!(
                matches!(RegexQuery::new(src), Err(TranslateError::BadRegex(_))),
                "{:?}",
                src
            );
        }
    }

    #[test]
    fn empty_matchable_regexes_are_rejected() {
        for src in ["", ".*", "a*", "x|", "()", "(a+)*"] {
            assert!(
                matches!(RegexQuery::new(src), Err(TranslateError::BadRegex(_))),
                "{:?} should be rejected",
                src
            );
        }
    }

    #[test]
    fn hostile_regexes_fail_fast_or_run_linear() {
        // Deep nesting: rejected cleanly by the parser's nest limit.
        assert!(RegexQuery::new(&"(".repeat(2000)).is_err());
        // Huge counted repetition: rejected by size_limit, not compiled.
        assert!(RegexQuery::new("a{1000000}{1000}").is_err());
        // Classic backtracking bomb: the linear engine answers immediately
        // (a backtracker would take exponential time here).
        let r = RegexQuery::new("(a+)+$").unwrap();
        let hay = format!("{}b", "a".repeat(10_000));
        assert!(!r.is_match(&hay));
    }

    #[test]
    fn find_and_count_on_regex() {
        let r = RegexQuery::new("b[aeiou]d").unwrap();
        let hay = "bad bed bodkin";
        assert_eq!(r.find_first(hay), Some(0..3));
        assert_eq!(r.count(hay), 3);
    }

    #[test]
    fn clamp_respects_char_boundaries() {
        let text = "aééééb";
        let r = clamp_match_range(text, 0..text.len(), 4);
        assert!(text.is_char_boundary(r.end));
        assert!(r.end <= 4);
        // No-op when already short enough.
        assert_eq!(clamp_match_range(text, 1..3, 100), 1..3);
    }
}
