//! Syntax highlighting for the search box.
//!
//! [`classify`] is a pure token walk over [`tokenize_spanned`] output that
//! mirrors `split_for_cascade` branch for branch — it must never claim
//! something is a filter (or a wildcard) that the engine treats as plain
//! text. The egui layer at the bottom turns its segments into a `Galley`
//! for `TextEdit::layouter`.
//!
//! Color scheme: recognized keywords red, their arguments blue, syntax
//! characters (operators, quotes, live wildcards) green, invalid arguments
//! in the error color, everything else plain. A complete recognized filter
//! additionally gets a tinted background chip.

use std::ops::Range;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Galley, Stroke};
use quicksearch_core::query::ast::Op;
use quicksearch_core::query::lexer::{tokenize_spanned, Token};
use quicksearch_core::query::pattern::RegexQuery;
use quicksearch_core::query::translator::{build_filter, is_filter_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Plain,
    /// The key word of a recognized filter (`type`, `name`, `regex`, …).
    Keyword,
    /// Syntax characters doing work: filter operators (`:`, `:>=`, …),
    /// quote delimiters, and `*` where it is a live wildcard.
    Operator,
    /// The value of a recognized filter.
    Argument,
    /// The value of a recognized filter that the engine would reject
    /// (unknown type name, bad date, invalid regex).
    InvalidArg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub range: Range<usize>,
    pub class: Class,
    /// Part of a complete recognized filter — drawn on the chip tint.
    pub chip: bool,
}

/// Classify `text` into contiguous segments tiling `0..text.len()`.
pub fn classify(text: &str) -> Vec<Seg> {
    let (tokens, err) = tokenize_spanned(text);
    let mut em = Emitter {
        text,
        cursor: 0,
        segs: Vec::new(),
    };

    let tok = |i: usize| tokens.get(i).map(|(t, _)| t);
    let span = |i: usize| tokens[i].1.clone();

    let mut i = 0usize;
    // The engine allows one `regex:` per query; later ones are errors.
    let mut regex_seen = false;

    while i < tokens.len() {
        match &tokens[i].0 {
            Token::Word(word) => {
                if let Some(Token::Op(op1)) = tok(i + 1) {
                    // Candidate filter: Word(key) Op [Op] (Word|Quoted),
                    // exactly as split_for_cascade sees it.
                    let (op, op_end_idx, value_idx) = match tok(i + 2) {
                        Some(Token::Op(op2)) => (*op2, i + 2, i + 3),
                        _ => (*op1, i + 1, i + 2),
                    };
                    let value = match tok(value_idx) {
                        Some(Token::Word(v)) | Some(Token::Quoted(v)) => Some(v.clone()),
                        _ => None,
                    };
                    let is_regex = word.eq_ignore_ascii_case("regex");
                    if let Some(value) = value {
                        let value_is_word = matches!(tok(value_idx), Some(Token::Word(_)));
                        if is_regex || is_filter_key(word) {
                            let valid = if is_regex {
                                let first = !regex_seen;
                                regex_seen = true;
                                first && op == Op::Contains && RegexQuery::new(&value).is_ok()
                            } else {
                                build_filter(word, op, &value, value_is_word).is_ok()
                            };
                            em.emit(span(i), Class::Keyword, true, false);
                            for op_idx in (i + 1)..=op_end_idx {
                                em.emit(span(op_idx), Class::Operator, true, true);
                            }
                            let vspan = span(value_idx);
                            if !valid {
                                // One uniform error run reads better than
                                // error-with-green-sprinkles.
                                em.emit(vspan, Class::InvalidArg, true, true);
                            } else if !value_is_word {
                                em.emit_quoted(vspan, Class::Argument, true);
                            } else if glob_value_key(word) {
                                em.emit_word(vspan, Class::Argument, true, true);
                            } else {
                                // Stars in other filter values are literal
                                // characters — no wildcard color.
                                em.emit(vspan, Class::Argument, true, true);
                            }
                            i = value_idx + 1;
                            continue;
                        }
                        // Unrecognized key: the engine reassembles the whole
                        // chain verbatim (stars stay literal), so everything
                        // renders plain — that absence of color is how the
                        // user learns `foo:` is not a filter.
                        em.emit(span(i), Class::Plain, false, false);
                        for op_idx in (i + 1)..=op_end_idx {
                            em.emit(span(op_idx), Class::Plain, false, false);
                        }
                        em.emit_glued_value(span(value_idx), tok(value_idx));
                        i = value_idx + 1;
                        while let Some(Token::Op(_)) = tok(i) {
                            em.emit(span(i), Class::Plain, false, false);
                            i += 1;
                            if let Some(Token::Word(_)) | Some(Token::Quoted(_)) = tok(i) {
                                em.emit_glued_value(span(i), tok(i));
                                i += 1;
                            }
                        }
                        continue;
                    }
                    // Key + op with no value yet (mid-typing `type:`).
                    // Recognized keys color optimistically — instant
                    // feedback that the key landed — but earn no chip
                    // until the filter is complete.
                    let known = is_regex || is_filter_key(word);
                    let (key_class, op_class) = if known {
                        (Class::Keyword, Class::Operator)
                    } else {
                        (Class::Plain, Class::Plain)
                    };
                    em.emit(span(i), key_class, false, false);
                    for op_idx in (i + 1)..=op_end_idx {
                        em.emit(span(op_idx), op_class, false, false);
                    }
                    i = op_end_idx + 1;
                    continue;
                }
                // A plain word: unquoted stars are live wildcards.
                em.emit_word(span(i), Class::Plain, false, false);
            }
            Token::Quoted(_) => em.emit_quoted(span(i), Class::Plain, false),
            // Demoted to plain text by the live search path — coloring
            // them as operators would lie.
            Token::And | Token::Or | Token::LParen | Token::RParen | Token::Op(_) => {
                em.emit(span(i), Class::Plain, false, false);
            }
        }
        i += 1;
    }

    // Trailing lex error: an unterminated quote is a quote-in-progress,
    // not a mistake — green delimiter, plain tail.
    if let Some(err) = err {
        if err.offset < text.len() && text.as_bytes()[err.offset] == b'"' {
            em.emit(err.offset..err.offset + 1, Class::Operator, false, false);
        }
    }
    em.finish(text.len())
}

/// Keys whose word-form values interpret `*` as a wildcard (or, for
/// `regex`, as live pattern syntax).
fn glob_value_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "name" | "filename" | "regex"
    )
}

struct Emitter<'a> {
    text: &'a str,
    cursor: usize,
    segs: Vec<Seg>,
}

impl Emitter<'_> {
    /// Fill the gap (whitespace the lexer skipped) up to `pos`.
    fn gap_to(&mut self, pos: usize, chip: bool) {
        if pos > self.cursor {
            self.segs.push(Seg {
                range: self.cursor..pos,
                class: Class::Plain,
                chip,
            });
            self.cursor = pos;
        }
    }

    /// Emit one span. `gap_chip` tints the whitespace before it — true for
    /// the interior of a filter (`type : Audio` chips as one run).
    fn emit(&mut self, range: Range<usize>, class: Class, chip: bool, gap_chip: bool) {
        self.gap_to(range.start, gap_chip);
        if range.end > range.start {
            self.segs.push(Seg {
                range: range.clone(),
                class,
                chip,
            });
            self.cursor = range.end;
        }
    }

    /// Emit a word span with each `*` as a green wildcard and the pieces
    /// between in `base`.
    fn emit_word(&mut self, range: Range<usize>, base: Class, chip: bool, gap_chip: bool) {
        self.gap_to(range.start, gap_chip);
        let bytes = self.text.as_bytes();
        let mut piece_start = range.start;
        for pos in range.clone() {
            if bytes[pos] == b'*' {
                if pos > piece_start {
                    self.segs.push(Seg {
                        range: piece_start..pos,
                        class: base,
                        chip,
                    });
                }
                self.segs.push(Seg {
                    range: pos..pos + 1,
                    class: Class::Operator,
                    chip,
                });
                piece_start = pos + 1;
            }
        }
        if range.end > piece_start {
            self.segs.push(Seg {
                range: piece_start..range.end,
                class: base,
                chip,
            });
        }
        self.cursor = self.cursor.max(range.end);
    }

    /// Emit a quoted span (delimiters included): quotes green, content in
    /// `content`. Inner `""` escapes are just content bytes — no offset
    /// math needed.
    fn emit_quoted(&mut self, range: Range<usize>, content: Class, chip: bool) {
        self.gap_to(range.start, chip);
        self.segs.push(Seg {
            range: range.start..range.start + 1,
            class: Class::Operator,
            chip,
        });
        if range.end - range.start > 2 {
            self.segs.push(Seg {
                range: range.start + 1..range.end - 1,
                class: content,
                chip,
            });
        }
        if range.end - range.start >= 2 {
            self.segs.push(Seg {
                range: range.end - 1..range.end,
                class: Class::Operator,
                chip,
            });
        }
        self.cursor = self.cursor.max(range.end);
    }

    /// A value inside unrecognized-key glue: plain, except quote
    /// delimiters, which still did real tokenizing work.
    fn emit_glued_value(&mut self, range: Range<usize>, token: Option<&Token>) {
        match token {
            Some(Token::Quoted(_)) => self.emit_quoted(range, Class::Plain, false),
            _ => self.emit(range, Class::Plain, false, false),
        }
    }

    fn finish(mut self, len: usize) -> Vec<Seg> {
        self.gap_to(len, false);
        self.segs
    }
}

// ---------------------------------------------------------------------------
// egui layer
// ---------------------------------------------------------------------------

struct QueryPalette {
    keyword: Color32,
    argument: Color32,
    operator: Color32,
}

/// GitHub Primer syntax colors — readable on egui's near-black and white
/// text-field backgrounds. Same convention as `rank_tier_color`.
fn query_palette(dark_mode: bool) -> QueryPalette {
    if dark_mode {
        QueryPalette {
            keyword: Color32::from_rgb(255, 123, 114),
            argument: Color32::from_rgb(121, 192, 255),
            operator: Color32::from_rgb(126, 231, 135),
        }
    } else {
        QueryPalette {
            keyword: Color32::from_rgb(207, 34, 46),
            argument: Color32::from_rgb(5, 80, 174),
            operator: Color32::from_rgb(26, 127, 55),
        }
    }
}

struct QueryFormats {
    plain: TextFormat,
    keyword: TextFormat,
    operator: TextFormat,
    argument: TextFormat,
    invalid: TextFormat,
    chip_bg: Color32,
}

fn query_formats(ui: &egui::Ui) -> QueryFormats {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let palette = query_palette(ui.visuals().dark_mode);
    let base = |color: Color32| TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };
    let error = ui.visuals().error_fg_color;
    QueryFormats {
        plain: base(ui.visuals().text_color()),
        keyword: base(palette.keyword),
        operator: base(palette.operator),
        argument: base(palette.argument),
        // The keyword red and the error red are near neighbors in dark
        // mode; the underline disambiguates at a glance.
        invalid: TextFormat {
            underline: Stroke::new(1.0, error),
            ..base(error)
        },
        // Slightly weaker than the snippet highlight's 0.4 so the colored
        // text on top stays crisp.
        chip_bg: ui.visuals().selection.bg_fill.gamma_multiply(0.35),
    }
}

impl QueryFormats {
    fn format_for(&self, seg: &Seg) -> TextFormat {
        let mut fmt = match seg.class {
            Class::Plain => self.plain.clone(),
            Class::Keyword => self.keyword.clone(),
            Class::Operator => self.operator.clone(),
            Class::Argument => self.argument.clone(),
            Class::InvalidArg => self.invalid.clone(),
        };
        if seg.chip {
            fmt.background = self.chip_bg;
        }
        fmt
    }
}

/// Classification cache: tokenizing is cheap but validating a `regex:`
/// argument compiles the regex, and the layouter runs every frame — so
/// segments are recomputed only when the text changes. The `LayoutJob` is
/// rebuilt each frame (colors follow the live theme) and epaint's own
/// galley cache dedupes the actual layout work by job hash.
#[derive(Default)]
pub struct HighlightCache {
    text: String,
    segs: Vec<Seg>,
}

pub fn galley(ui: &egui::Ui, cache: &mut HighlightCache, text: &str) -> Arc<Galley> {
    if cache.text != text {
        cache.text = text.to_owned();
        cache.segs = classify(text);
    }
    let fmts = query_formats(ui);
    let mut job = LayoutJob::default();
    for seg in &cache.segs {
        job.append(&text[seg.range.clone()], 0.0, fmts.format_for(seg));
    }
    ui.fonts(|f| f.layout_job(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readable projection: (slice, class, chip) per segment.
    fn segs(text: &str) -> Vec<(String, Class, bool)> {
        assert_tiles(text);
        classify(text)
            .into_iter()
            .map(|s| (text[s.range.clone()].to_string(), s.class, s.chip))
            .collect()
    }

    /// Segments must tile 0..len exactly: contiguous, ascending, complete.
    fn assert_tiles(text: &str) {
        let segs = classify(text);
        let mut cursor = 0usize;
        for s in &segs {
            assert_eq!(
                s.range.start, cursor,
                "gap or overlap in {:?}: {:?}",
                text, segs
            );
            assert!(s.range.end > s.range.start, "empty seg in {:?}", text);
            cursor = s.range.end;
        }
        assert_eq!(cursor, text.len(), "segments must cover {:?}", text);
    }

    use Class::*;

    fn owned(v: Vec<(&str, Class, bool)>) -> Vec<(String, Class, bool)> {
        v.into_iter()
            .map(|(s, c, b)| (s.to_string(), c, b))
            .collect()
    }

    #[test]
    fn empty_input_yields_no_segments() {
        assert!(classify("").is_empty());
    }

    #[test]
    fn plain_words_stay_plain() {
        assert_eq!(
            segs("budget report"),
            owned(vec![
                ("budget", Plain, false),
                (" ", Plain, false),
                ("report", Plain, false),
            ])
        );
    }

    #[test]
    fn every_recognized_filter_chips() {
        for input in [
            "type:Audio",
            "modified:>=2024-01-01",
            "mtime:<2023-12-01",
            "path:/home/me",
            "folder:/x",
            "includefolder:/x",
            "name:report",
            "filename:report",
            "mime:application/pdf",
            "regex:foo",
        ] {
            let all = segs(input);
            assert!(
                all.iter().all(|(_, _, chip)| *chip),
                "{:?}: whole filter must chip: {:?}",
                input,
                all
            );
            assert_eq!(all[0].1, Keyword, "{:?}", input);
            assert_eq!(all[1].1, Operator, "{:?}", input);
            assert!(
                all[2..]
                    .iter()
                    .all(|(_, c, _)| *c == Argument || *c == Operator),
                "{:?}: {:?}",
                input,
                all
            );
        }
    }

    #[test]
    fn keys_are_case_insensitive() {
        assert_eq!(segs("TYPE:Audio")[0], ("TYPE".to_string(), Keyword, true));
        assert_eq!(
            segs("Modified:>=2024-01-01")[0],
            ("Modified".to_string(), Keyword, true)
        );
    }

    #[test]
    fn colon_comparator_runs_are_one_green_stretch() {
        assert_eq!(
            segs("modified:>=2024-01-01"),
            owned(vec![
                ("modified", Keyword, true),
                (":", Operator, true),
                (">=", Operator, true),
                ("2024-01-01", Argument, true),
            ])
        );
    }

    #[test]
    fn unrecognized_keys_stay_plain() {
        for input in ["foo:bar", "12:30", "foo:bar:baz"] {
            assert!(
                segs(input).iter().all(|(_, c, chip)| *c == Plain && !chip),
                "{:?}: {:?}",
                input,
                segs(input)
            );
        }
        // Stars in glue are literal to the engine — no green.
        assert!(segs("foo:ba*r").iter().all(|(_, c, _)| *c == Plain));
    }

    #[test]
    fn drive_letters_do_not_split() {
        assert_eq!(
            segs(r"path:C:\Users\me"),
            owned(vec![
                ("path", Keyword, true),
                (":", Operator, true),
                (r"C:\Users\me", Argument, true),
            ])
        );
        assert_eq!(segs(r"C:\data"), owned(vec![(r"C:\data", Plain, false)]));
    }

    #[test]
    fn quoted_phrases_get_green_delimiters() {
        assert_eq!(
            segs("\"exact phrase\""),
            owned(vec![
                ("\"", Operator, false),
                ("exact phrase", Plain, false),
                ("\"", Operator, false),
            ])
        );
        // Inner "" escapes are content bytes.
        assert_eq!(
            segs("\"a\"\"b\""),
            owned(vec![
                ("\"", Operator, false),
                ("a\"\"b", Plain, false),
                ("\"", Operator, false),
            ])
        );
        // Quoted stars are literal — content stays plain.
        assert!(segs("\"a*b\"")
            .iter()
            .all(|(s, c, _)| s == "\"" || *c == Plain));
    }

    #[test]
    fn quoted_filter_values_are_blue_with_green_quotes() {
        assert_eq!(
            segs("path:\"/home/me/My Docs\""),
            owned(vec![
                ("path", Keyword, true),
                (":", Operator, true),
                ("\"", Operator, true),
                ("/home/me/My Docs", Argument, true),
                ("\"", Operator, true),
            ])
        );
        // Empty quoted value: two delimiters, no content seg, no panic.
        assert_eq!(
            segs("path:\"\""),
            owned(vec![
                ("path", Keyword, true),
                (":", Operator, true),
                ("\"", Operator, true),
                ("\"", Operator, true),
            ])
        );
    }

    #[test]
    fn unterminated_quote_is_a_quote_in_progress() {
        assert_eq!(
            segs("\"unclosed phrase"),
            owned(vec![
                ("\"", Operator, false),
                ("unclosed phrase", Plain, false),
            ])
        );
        // Filters before the open quote keep their colors.
        let all = segs("type:Audio \"x");
        assert_eq!(all[0], ("type".to_string(), Keyword, true));
        assert_eq!(all[4], ("\"".to_string(), Operator, false));
        assert_eq!(all[5], ("x".to_string(), Plain, false));
    }

    #[test]
    fn trailing_bare_keys_color_optimistically_without_chip() {
        assert_eq!(
            segs("type:"),
            owned(vec![("type", Keyword, false), (":", Operator, false)])
        );
        assert_eq!(
            segs("modified:>="),
            owned(vec![
                ("modified", Keyword, false),
                (":", Operator, false),
                (">=", Operator, false),
            ])
        );
        assert_eq!(
            segs("foo:"),
            owned(vec![("foo", Plain, false), (":", Plain, false)])
        );
    }

    #[test]
    fn stars_in_words_and_name_values_go_green() {
        assert_eq!(
            segs("rep*ort"),
            owned(vec![
                ("rep", Plain, false),
                ("*", Operator, false),
                ("ort", Plain, false),
            ])
        );
        assert_eq!(
            segs("name:re*.txt"),
            owned(vec![
                ("name", Keyword, true),
                (":", Operator, true),
                ("re", Argument, true),
                ("*", Operator, true),
                (".txt", Argument, true),
            ])
        );
        // Edge and doubled stars keep tiling intact.
        assert_tiles("*foo");
        assert_tiles("foo*");
        assert_tiles("**");
        assert_tiles("*");
        // In non-glob filter values the star is a literal character.
        assert_eq!(
            segs("path:/da*ta")[2],
            ("/da*ta".to_string(), Argument, true)
        );
    }

    #[test]
    fn invalid_arguments_go_error_uniformly() {
        // (`regex:(` is not here: `(` lexes as a paren, so that input is an
        // *incomplete* filter — bare-key optimism applies, not an error.)
        for input in [
            "type:NotAThing",
            "modified:>=tomorrow",
            "regex:[",
            "type:Doc*",
        ] {
            let all = segs(input);
            assert_eq!(all[0].1, Keyword, "{:?}", input);
            let last = all.last().unwrap();
            assert_eq!(last.1, InvalidArg, "{:?}: {:?}", input, all);
            assert!(last.2, "invalid values keep the chip: {:?}", input);
        }
        // name:= is an unsupported op → its value is invalid too.
        let all = segs("name=x");
        assert_eq!(all.last().unwrap().1, InvalidArg);
    }

    #[test]
    fn valid_regex_argument_is_blue_with_green_stars() {
        assert_eq!(
            segs("regex:foo.*bar"),
            owned(vec![
                ("regex", Keyword, true),
                (":", Operator, true),
                ("foo.", Argument, true),
                ("*", Operator, true),
                ("bar", Argument, true),
            ])
        );
    }

    #[test]
    fn a_second_regex_filter_is_invalid() {
        let all = segs("regex:foo regex:bar");
        assert_eq!(all[2], ("foo".to_string(), Argument, true));
        assert_eq!(all.last().unwrap(), &("bar".to_string(), InvalidArg, true));
    }

    #[test]
    fn multi_filter_queries_chip_separately() {
        let all = segs("type:Document budget modified:>=2024-01-01");
        // The word and the whitespace around it stay un-chipped.
        assert_eq!(
            all.iter()
                .filter(|(_, _, chip)| !chip)
                .map(|(s, _, _)| s.as_str())
                .collect::<Vec<_>>(),
            vec![" ", "budget", " "]
        );
    }

    #[test]
    fn spaced_filters_chip_their_interior_gaps() {
        // `type : Audio` is still a filter to the lexer/splitter.
        assert_eq!(
            segs("type : Audio"),
            owned(vec![
                ("type", Keyword, true),
                (" ", Plain, true),
                (":", Operator, true),
                (" ", Plain, true),
                ("Audio", Argument, true),
            ])
        );
    }

    #[test]
    fn demoted_operators_stay_plain() {
        assert!(segs("(alpha AND beta) OR gamma")
            .iter()
            .all(|(_, c, chip)| *c == Plain && !chip));
        // Dangling comparators are literal text.
        assert!(segs("a > b").iter().all(|(_, c, _)| *c == Plain));
        // Leading operator, nothing else.
        assert!(segs(">foo").iter().all(|(_, c, _)| *c == Plain));
    }

    #[test]
    fn adjacency_between_filter_and_quote() {
        // `Audio"q"`: the word ends at the quote; the filter is complete
        // and the quoted phrase stands alone.
        let all = segs("type:Audio\"q\"");
        assert_eq!(all[2], ("Audio".to_string(), Argument, true));
        assert_eq!(all[3], ("\"".to_string(), Operator, false));
    }

    #[test]
    fn unicode_offsets_hold_up() {
        assert_tiles("\"José\" type:Audio naïve*file");
        let all = segs("naïve*café");
        assert_eq!(
            all,
            owned(vec![
                ("naïve", Plain, false),
                ("*", Operator, false),
                ("café", Plain, false),
            ])
        );
    }
}
