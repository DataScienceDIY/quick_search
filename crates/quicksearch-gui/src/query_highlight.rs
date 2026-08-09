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
                                // One uniform error run; no wildcard color.
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
                        // renders plain.
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
                    // Key + op with no value yet (mid-typing `type:`):
                    // recognized keys color optimistically but earn no chip
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
            // Demoted to plain text by the live search path.
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

// --- egui layer ---

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
    let palette = crate::color::palette(ui.visuals().dark_mode);
    let base = |color: Color32| TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };
    let error = ui.visuals().error_fg_color;
    QueryFormats {
        plain: base(ui.visuals().text_color()),
        keyword: base(palette.red),
        operator: base(palette.green),
        argument: base(palette.blue),
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
mod tests;
