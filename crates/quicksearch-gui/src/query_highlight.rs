//! Syntax highlighting for the search box. [`classify`] mirrors
//! `split_for_cascade` branch for branch: it must never claim something is a
//! filter (or a wildcard) that the engine treats as plain text. Literal
//! chains (`std::string`, `Re: meeting`) take their extent from the engine's
//! own `literal_chain`, so a filter after whitespace chips in both. A
//! recognized key with an invalid value is marked as an error, as the engine
//! rejects it.
//!
//! Colors: keywords red, arguments blue, syntax characters green, invalid
//! arguments error-colored; a complete recognized filter gets a chip tint.

use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Galley, Stroke};
use quicksearch_core::query::lexer::{tokenize_spanned, Token};
use quicksearch_core::query::pattern::RegexQuery;
use quicksearch_core::query::split::literal_chain;
use quicksearch_core::query::translator::{check_filter, is_filter_key, is_regex_key};
use quicksearch_core::query::Op;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Plain,
    Keyword,
    /// Operators, quote delimiters, and `*` where it is a live wildcard.
    Operator,
    Argument,
    /// A recognized filter's value that the engine would reject.
    InvalidArg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub range: Range<usize>,
    pub class: Class,
    /// Part of a complete recognized filter, drawn on the chip tint.
    pub chip: bool,
}

type Spanned = (Token, Range<usize>);

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
        if let Some((key, op, op_end, value_idx)) = filter_shape(&tokens, i) {
            let is_regex = is_regex_key(key);
            match tok(value_idx) {
                // A complete recognized filter.
                Some(Token::Word(value) | Token::Quoted(value)) => {
                    let value_is_word = matches!(tok(value_idx), Some(Token::Word(_)));
                    let valid = if is_regex {
                        let first = !regex_seen;
                        regex_seen = true;
                        first && op == Op::Contains && RegexQuery::new(value).is_ok()
                    } else {
                        check_filter(key, op, value, value_is_word).is_ok()
                    };
                    em.emit(span(i), Class::Keyword, true, false);
                    em.emit_all(&tokens, i + 1..=op_end, Class::Operator, true);
                    let vspan = span(value_idx);
                    if !valid {
                        em.emit(vspan, Class::InvalidArg, true, true);
                    } else if !value_is_word {
                        em.emit_quoted(vspan, Class::Argument, true);
                    } else if glob_value_key(key) {
                        em.emit_word(vspan, Class::Argument, true, true);
                    } else {
                        em.emit(vspan, Class::Argument, true, true);
                    }
                    i = value_idx + 1;
                }
                // Mid-typing `type:`: recognized keys color optimistically
                // but earn no chip until complete.
                _ => {
                    em.emit(span(i), Class::Keyword, false, false);
                    em.emit_all(&tokens, i + 1..=op_end, Class::Operator, false);
                    i = op_end + 1;
                }
            }
            continue;
        }
        // The engine searches a chain as one literal (stars included).
        if let Some((_, end)) = literal_chain(&tokens, i) {
            for j in i..end {
                em.emit_glued_value(span(j), tok(j));
            }
            i = end;
            continue;
        }
        match &tokens[i].0 {
            Token::Word(_) => em.emit_word(span(i), Class::Plain, false, false),
            Token::Quoted(_) => em.emit_quoted(span(i), Class::Plain, false),
            Token::And | Token::Or | Token::LParen | Token::RParen | Token::Op(_) => {
                em.emit(span(i), Class::Plain, false, false);
            }
        }
        i += 1;
    }

    // An unterminated quote is a quote-in-progress, not a mistake.
    if let Some(err) = err {
        if err.offset < text.len() && text.as_bytes()[err.offset] == b'"' {
            em.emit(err.offset..err.offset + 1, Class::Operator, false, false);
        }
    }
    em.finish(text.len())
}

/// The `Word(key) Op [Op]` shape of a recognized key at `i`, as
/// `split_for_cascade` sees it: the key, the operator, the index of its last
/// token, and where the value would be.
fn filter_shape(tokens: &[Spanned], i: usize) -> Option<(&str, Op, usize, usize)> {
    let (Token::Word(key), Some((Token::Op(op1), _))) = (&tokens[i].0, tokens.get(i + 1)) else {
        return None;
    };
    if !is_filter_key(key) && !is_regex_key(key) {
        return None;
    }
    Some(match tokens.get(i + 2) {
        Some((Token::Op(op2), _)) => (key, *op2, i + 2, i + 3),
        _ => (key, *op1, i + 1, i + 2),
    })
}

/// Keys whose word-form values interpret `*` as live syntax.
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

    /// `gap_chip` tints the whitespace before the span, so a filter's
    /// interior chips as one run.
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

    /// Every token in `idx` as `class`, the gaps between them chipped alike.
    fn emit_all(
        &mut self,
        tokens: &[Spanned],
        idx: RangeInclusive<usize>,
        class: Class,
        chip: bool,
    ) {
        for (_, span) in &tokens[idx] {
            self.emit(span.clone(), class, chip, chip);
        }
    }

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

    /// Quotes green, content in `content`; inner `""` escapes are just
    /// content bytes, no offset math needed.
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
        // Keyword red and error red are near neighbors; the underline
        // disambiguates at a glance.
        invalid: TextFormat {
            underline: Stroke::new(1.0, error),
            ..base(error)
        },
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

/// Validating a `regex:` argument compiles the regex and the layouter runs
/// every frame, so segments are recomputed only when the text changes. The
/// job is rebuilt each frame (colors follow the live theme); epaint's galley
/// cache dedupes the layout by job hash.
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
