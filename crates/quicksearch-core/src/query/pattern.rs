//! Compiled matchers for the cascade term and the `regex:` keyword.
//!
//! A term with an unquoted `*` compiles to a small regex; star-free terms
//! stay on the [`Literal`](TermPattern::Literal) path.
//!
//! The two fold case differently. A literal folds ASCII case only: `cafe`
//! finds `CAFE`, and `café` never finds `CAFÉ`. A `*` pattern and `regex:`
//! fold Unicode case by the regex crate's simple folds, so `résumé*` finds
//! `RÉSUMÉ` and `kilo*` a name spelled with U+212A KELVIN SIGN. Every
//! prefilter in front of them must fold at least as much; see
//! [`crate::search::prefilter`], and its exception for file contents.
//!
//! `.` never matches `\n`, so a star cannot span lines of extracted text; a
//! `*` bridging a whole document would produce page-sized snippets.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use super::translator::TranslateError;
use crate::search::prefilter::Required;

/// Keeps a hostile pattern (`a{1000000}{1000}`) from ballooning the program.
const REGEX_SIZE_LIMIT: usize = 4 << 20;

/// Occurrence counts saturate here, matching `count_frac` in the cascade.
const COUNT_CAP: usize = 1000;

/// The cascade term, compiled once at split time.
#[derive(Debug, Clone, Default)]
pub enum TermPattern {
    /// Empty or stars-only. Matches nothing: a bare `*` must not become a scan
    /// of everything.
    #[default]
    Empty,
    Literal(LiteralPattern),
    Wildcard(WildcardPattern),
}

#[derive(Debug, Clone)]
pub struct LiteralPattern {
    text: String,
    folded: String,
}

#[derive(Debug, Clone)]
pub struct WildcardPattern {
    /// Literal chunks between stars, in order. Never empty and no chunk is
    /// empty: edge stars fold into the regexes, doubled stars collapse.
    segments: Vec<String>,
    /// Unanchored, non-greedy joins: leftmost-shortest, what a snippet wants.
    search_cs: Regex,
    search_ci: Regex,
    /// Anchored (`^…$`) regexes for whole-field matching (rank tiers 1/2).
    anchored_cs: Regex,
    anchored_ci: Regex,
}

/// One piece of the search phrase. `glob` is true only for plain unquoted
/// words; quoted phrases and `key:value` text keep their stars literal.
#[derive(Debug, Clone)]
pub struct TermPart {
    pub text: String,
    pub glob: bool,
}

enum Chunk {
    Lit(String),
    Star,
}

impl WildcardPattern {
    fn search(&self, case_insensitive: bool) -> &Regex {
        if case_insensitive {
            &self.search_ci
        } else {
            &self.search_cs
        }
    }

    fn anchored(&self, case_insensitive: bool) -> &Regex {
        if case_insensitive {
            &self.anchored_ci
        } else {
            &self.anchored_cs
        }
    }

    /// Non-overlapping occurrences in `text`, stopping at 1000 where the
    /// cascade's `count_frac` saturates, and the first of them, in one sweep.
    pub fn count_and_first(
        &self,
        text: &str,
        case_insensitive: bool,
    ) -> (usize, Option<Range<usize>>) {
        let mut found = self
            .search(case_insensitive)
            .find_iter(text)
            .take(COUNT_CAP);
        let first = found.next().map(|m| m.range());
        (first.as_ref().map_or(0, |_| 1 + found.count()), first)
    }
}

impl TermPattern {
    /// Compile the term parts, joined with single spaces like the display term.
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
            // Star-free chunks merge into one segment.
            let text = segments.into_iter().next().expect("segments is non-empty");
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
        // Under substring semantics a leading/trailing `.*?` adds nothing.
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

    /// The literal text, when the term has no wildcard.
    pub fn literal(&self) -> Option<&str> {
        match self {
            TermPattern::Literal(l) => Some(&l.text),
            _ => None,
        }
    }

    /// [`literal`](Self::literal), ASCII-folded: the form every
    /// case-insensitive scan searches with.
    pub fn literal_folded(&self) -> Option<&str> {
        match self {
            TermPattern::Literal(l) => Some(&l.folded),
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

    /// Characters of literal (non-star) content.
    pub fn literal_char_count(&self) -> usize {
        self.segments().iter().map(|s| s.chars().count()).sum()
    }

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
            TermPattern::Wildcard(w) => w.anchored(case_insensitive).is_match(text),
        }
    }

    /// ASCII-case-insensitive [`str::find`] without folding the haystack;
    /// `needle` must already be lowercased. `memchr2` jumps to each spelling
    /// of the needle's first byte, and only those candidates are compared:
    /// 1.18x faster than testing every byte (`benches/search.rs`,
    /// `filename_ladder`). The returned index is always a `char` boundary: a
    /// `&str`'s first byte is never a continuation byte, and ASCII-only case
    /// differences keep the end a boundary too.
    fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        let Some(&first) = n.first() else {
            return Some(0);
        };
        // Equal to `first` when it has no ASCII case partner.
        let upper = first.to_ascii_uppercase();
        let last = h.len().checked_sub(n.len())?;
        let mut at = 0;
        while at <= last {
            let i = at + memchr::memchr2(first, upper, &h[at..=last])?;
            if h[i..i + n.len()].eq_ignore_ascii_case(n) {
                return Some(i);
            }
            at = i + 1;
        }
        None
    }

    /// Leftmost match as a byte range; folded offsets are valid in the original.
    pub fn find_first(&self, text: &str, case_insensitive: bool) -> Option<Range<usize>> {
        match self {
            TermPattern::Empty => None,
            TermPattern::Literal(l) => {
                let pos = if case_insensitive {
                    Self::find_ascii_ci(text, &l.folded)?
                } else {
                    text.find(&l.text)?
                };
                Some(pos..pos + l.text.len())
            }
            TermPattern::Wildcard(w) => w.search(case_insensitive).find(text).map(|m| m.range()),
        }
    }
}

/// A compiled `regex:` query. Case-insensitive by default (override with
/// `(?-i:…)`); `multi_line` makes `^`/`$` per-line over extracted text.
#[derive(Debug, Clone)]
pub struct RegexQuery {
    pub source: String,
    re: Regex,
    required: Option<Required>,
}

/// Extract a set of literals at least one of which occurs in every match.
/// Sound because every match begins (prefix set) or ends (suffix set) with
/// one of the literals; an unbounded set yields `None`. Parsed
/// case-sensitively, because case-insensitive extraction explodes
/// combinatorially. Sound because FTS folds case (up to the exception in
/// [`crate::search::prefilter`]) and `like_predicate(true)` turns every
/// character with a non-ASCII case partner into `_`. An explicit `(?i)` in
/// the pattern still yields literals: the extractor expands classes of 10
/// characters or fewer, so `(?i)abc` gives its 8 case variants.
fn required_literals(source: &str) -> Option<Required> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};

    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(false)
        .build()
        .parse(source)
        .ok()?;
    // The OR is only as selective as its shortest arm.
    let weakest = |r: &Required| r.literals().iter().map(|l| l.chars().count()).min();
    [ExtractKind::Prefix, ExtractKind::Suffix]
        .into_iter()
        .filter_map(|kind| {
            // `None` when unbounded: no constraint from this direction.
            let seq = Extractor::new().kind(kind).extract(&hir);
            // The extractor can split a multi-byte character, and invalid
            // UTF-8 cannot be handed to FTS5 or bound as SQL text; dropping
            // one literal from an OR would drop rows, so the whole direction
            // goes.
            let strings = seq
                .literals()?
                .iter()
                .map(|l| std::str::from_utf8(l.as_bytes()).ok().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            Required::new(strings)
        })
        // On a tie the earlier direction, the prefix set, wins.
        .reduce(|best, next| {
            if weakest(&next) > weakest(&best) {
                next
            } else {
                best
            }
        })
}

impl RegexQuery {
    pub fn new(source: &str) -> Result<RegexQuery, TranslateError> {
        let re = RegexBuilder::new(source)
            .case_insensitive(true)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| TranslateError::BadRegex(e.to_string()))?;
        // The regex analog of the bare-`*` rule, but loud: the user typed a
        // keyword, so tell them instead of matching every file.
        if re.is_match("") {
            return Err(TranslateError::BadRegex(format!(
                "'{}' can match the empty string and would match every file",
                source
            )));
        }
        Ok(RegexQuery {
            source: source.to_string(),
            re,
            required: required_literals(source),
        })
    }

    /// Literals of which at least one occurs in anything this matches; `None`
    /// means nothing usable (`\d+`) and the pass scans everything. See
    /// [`crate::search::prefilter`] for the rule to obey.
    pub fn required(&self) -> Option<&Required> {
        self.required.as_ref()
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.re.is_match(text)
    }

    pub fn find_first(&self, text: &str) -> Option<Range<usize>> {
        self.re.find(text).map(|m| m.range())
    }

    /// Non-overlapping occurrences, capped at 1000, and the first of them,
    /// in one sweep. Against a count and then a second search for the first
    /// (`searchtime`, cargo registry): within noise to 3% faster.
    pub fn count_and_first(&self, text: &str) -> (usize, Option<Range<usize>>) {
        let mut found = self.re.find_iter(text).take(COUNT_CAP);
        let first = found.next().map(|m| m.range());
        (first.as_ref().map_or(0, |_| 1 + found.count()), first)
    }
}

/// Cap a match range at `max_len` bytes, aligned back to a char boundary but
/// never below its start: a greedy regex can match megabytes, and the snippet
/// window wants the start.
pub fn clamp_match_range(text: &str, range: Range<usize>, max_len: usize) -> Range<usize> {
    let end = range.end.min(range.start + max_len);
    let end = if end > range.start {
        text.floor_char_boundary(end).max(range.start)
    } else {
        end
    };
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
        }
    }

    #[test]
    fn quoted_star_stays_literal() {
        let p = TermPattern::build(&[part("a*b", false)]).unwrap();
        assert_eq!(p.literal(), Some("a*b"));
        assert!(p.find_first("xa*by", false).is_some());
        assert!(p.find_first("aXb", false).is_none());
    }

    #[test]
    fn segment_shapes() {
        // Edge stars vanish into the anchors.
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

        let w = wildcard(&[part("f**o", true)]);
        assert_eq!(w.segments, ["f", "o"]);

        // The implicit joining space is literal content.
        let w = wildcard(&[part("a*", true), part("b", false)]);
        assert_eq!(w.segments, ["a", " b"]);

        // `* *`: the joining space is interior literal content, not Empty.
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
        assert_eq!(p.find_first("aXXbYYb", false), Some(0..4));
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
        let hay = "LE CAFÉ DU MENU";
        let r = p.find_first(hay, true).unwrap();
        assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end));
    }

    #[test]
    fn count_is_nonoverlapping_and_capped() {
        let TermPattern::Wildcard(w) = TermPattern::build(&[part("a*b", true)]).unwrap() else {
            panic!("a star makes a wildcard");
        };
        assert_eq!(w.count_and_first("xx ab ab ab", false), (3, Some(3..5)));
        let many = "ab ".repeat(2000);
        assert_eq!(w.count_and_first(&many, false).0, 1000);
        assert_eq!(w.count_and_first("AB aB", false), (0, None), "exact case");
        assert_eq!(w.count_and_first("AB aB", true), (2, Some(0..2)));
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
    }

    /// What a literal's `find_first(text, true)` must return: the leftmost
    /// match in the ASCII-folded haystack. Folding keeps byte length, so the
    /// offsets carry over.
    fn folded_reference(folded_hay: &str, folded_needle: &str) -> Option<Range<usize>> {
        folded_hay
            .find(folded_needle)
            .map(|at| at..at + folded_needle.len())
    }

    fn literal(term: &str) -> TermPattern {
        TermPattern::build(&[part(term, false)]).unwrap()
    }

    /// Every string of up to `max` characters over `alphabet`.
    fn strings_over(alphabet: &[char], max: usize) -> Vec<String> {
        let mut all = vec![String::new()];
        let mut level = vec![String::new()];
        for _ in 0..max {
            level = level
                .iter()
                .flat_map(|s| alphabet.iter().map(move |c| format!("{s}{c}")))
                .collect();
            all.extend(level.iter().cloned());
        }
        all
    }

    /// The candidate scan against the reference over every short string: both
    /// cases of letters, separators, `é`/`É` (one lead byte, different
    /// continuations), and KELVIN SIGN and long s, which fold to `k` and `s`
    /// under Unicode rules only and so must not match here.
    #[test]
    fn ascii_fold_find_agrees_with_a_folded_reference() {
        const ALPHABET: [char; 12] = [
            'a', 'A', 'b', 'B', 'k', 's', '-', '/', 'é', 'É', '\u{212A}', 'ſ',
        ];
        let patterns: Vec<(TermPattern, String)> = strings_over(&ALPHABET, 2)
            .into_iter()
            .skip(1)
            .map(|n| (literal(&n), n.to_ascii_lowercase()))
            .collect();
        for hay in strings_over(&ALPHABET, 4) {
            let folded = hay.to_ascii_lowercase();
            for (p, needle) in &patterns {
                assert_eq!(
                    p.find_first(&hay, true),
                    folded_reference(&folded, needle),
                    "{needle:?} in {hay:?}"
                );
            }
        }
    }

    #[test]
    fn ascii_fold_find_edges() {
        let find = |term: &str, hay: &str| literal(term).find_first(hay, true);
        // An empty field, and a term longer than the field.
        assert_eq!(find("a", ""), None);
        assert_eq!(find("quartzite", "QUARTZ"), None);
        assert_eq!(find("quartz", "QuArTz"), Some(0..6));
        // The last start a scan may take.
        assert_eq!(find("txt", "notes.TXT"), Some(6..9));
        assert_eq!(find("ab", "aaaa"), None);
        // A failed candidate resumes one byte on, inside the repeat.
        assert_eq!(find("aab", "aaaab"), Some(2..5));
        assert_eq!(find("AAB", "aAaAb"), Some(2..5));
        // The leftmost of several spellings.
        assert_eq!(find("Report", "x-REPORT-report"), Some(2..8));
        // First bytes with no case partner.
        assert_eq!(find("-01", "x-A-01"), Some(3..6));
        assert_eq!(find(".txt", "a.TXT"), Some(1..5));
        assert_eq!(find("/q", r"C:\Users/Q"), Some(8..10));
        // ASCII folding only: `É` is not `é`, and KELVIN SIGN is not `k`.
        assert_eq!(find("été", "ÉTÉ été"), Some(6..11));
        assert_eq!(find("café", "CAFÉ"), None);
        assert_eq!(find("café", "CAFé"), Some(0..5));
        assert_eq!(find("kilo", "\u{212A}ILO"), None);
        assert_eq!(find("é", "aÉé"), Some(3..5));
        assert_eq!(find("b", "éB"), Some(2..3));
        // Unreachable through `find_first` (an empty term is `Empty`).
        assert_eq!(TermPattern::find_ascii_ci("abc", ""), Some(0));
    }

    /// The ladder in `search/cascade/passes.rs` `pass_filename` (ranks 1–4 on
    /// the name, 9–10 on the path), with the case-insensitive find swappable.
    fn ladder(
        p: &TermPattern,
        name: &str,
        path: &str,
        find_ci: &dyn Fn(&str) -> Option<Range<usize>>,
    ) -> Option<(u8, Range<usize>)> {
        let with_paths = p.literal_char_count() >= crate::search::prefilter::TRIGRAM_FLOOR;
        if p.whole_match(name, false) {
            Some((1, 0..name.len()))
        } else if p.whole_match(name, true) {
            Some((2, 0..name.len()))
        } else if let Some(r) = p.find_first(name, false) {
            Some((3, r))
        } else if let Some(r) = find_ci(name) {
            Some((4, r))
        } else if !with_paths {
            None
        } else if let Some(r) = p.find_first(path, false) {
            Some((9, r))
        } else {
            find_ci(path).map(|r| (10, r))
        }
    }

    #[test]
    fn filename_tiers_agree_with_a_folded_reference() {
        let rows = [
            ("report.txt", "/home/u/report.txt"),
            ("REPORT.TXT", "/home/u/REPORT.TXT"),
            ("Report", "/home/u/Report"),
            ("my-report-2024.pdf", "/data/my-report-2024.pdf"),
            ("MY_REPORT.pdf", "/data/MY_REPORT.pdf"),
            ("notes.txt", "/srv/Report/notes.txt"),
            ("notes.txt", "/srv/REPORT/notes.txt"),
            ("notes.txt", r"C:\Users\u\Report\notes.txt"),
            ("résumé-report.odt", "/home/u/résumé-report.odt"),
            ("RÉSUMÉ.odt", "/home/u/RÉSUMÉ.odt"),
            ("rrreport", "/x/rrreport"),
            ("rep", "/rep"),
            ("", "/"),
        ];
        let terms = [
            "report",
            "Report",
            "REPORT",
            "rep",
            "rrep",
            "résumé",
            "Résumé",
            "RÉSUMÉ",
            "report/notes",
            r"Report\notes",
            ".txt",
            "t",
        ];
        let mut ranks = std::collections::BTreeSet::new();
        for term in terms {
            let p = literal(term);
            let needle = term.to_ascii_lowercase();
            for (name, path) in rows {
                let shipped = ladder(&p, name, path, &|f| p.find_first(f, true));
                let reference = ladder(&p, name, path, &|f| {
                    folded_reference(&f.to_ascii_lowercase(), &needle)
                });
                assert_eq!(shipped, reference, "{term:?} against {path:?}");
                ranks.extend(shipped.map(|(rank, _)| rank));
            }
        }
        assert_eq!(ranks.into_iter().collect::<Vec<_>>(), [1, 2, 3, 4, 9, 10]);
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
        // Deep nesting: rejected by the parser's nest limit.
        assert!(RegexQuery::new(&"(".repeat(2000)).is_err());
        // Huge counted repetition: rejected by size_limit, not compiled.
        assert!(RegexQuery::new("a{1000000}{1000}").is_err());
        // Backtracking bomb: the linear engine answers immediately.
        let r = RegexQuery::new("(a+)+$").unwrap();
        let hay = format!("{}b", "a".repeat(10_000));
        assert!(!r.is_match(&hay));
    }

    #[test]
    fn an_explicit_case_flag_still_yields_literals() {
        let r = RegexQuery::new("(?i)abc").unwrap();
        let mut literals = r.required().expect("a bounded set").literals().to_vec();
        literals.sort();
        assert_eq!(literals.len(), 8, "{literals:?}");
        assert_eq!((literals[0].as_str(), literals[7].as_str()), ("ABC", "abc"));
        // Without the flag the set is the literal as typed; the folding is
        // the consumers' job.
        let r = RegexQuery::new("Café").unwrap();
        assert_eq!(r.required().unwrap().literals(), ["Café"]);
    }

    #[test]
    fn find_and_count_on_regex() {
        let r = RegexQuery::new("b[aeiou]d").unwrap();
        let hay = "bad bed bodkin";
        assert_eq!(r.find_first(hay), Some(0..3));
        assert_eq!(r.count_and_first(hay), (3, Some(0..3)));
        assert_eq!(r.count_and_first("nothing here"), (0, None));
    }

    #[test]
    fn clamp_respects_char_boundaries() {
        let text = "aééééb";
        let r = clamp_match_range(text, 0..text.len(), 4);
        assert!(text.is_char_boundary(r.end));
        assert!(r.end <= 4);
        assert_eq!(clamp_match_range(text, 1..3, 100), 1..3);
    }
}
