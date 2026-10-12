//! Split raw search-box input into (cascade term, structured filters).
//!
//! Everything that isn't a recognized `key:value` filter joins the *term*;
//! recognized filters become SQL fragments ANDed onto every cascade stage.
//!
//! The term keeps the text as typed: whitespace runs become one space, quotes
//! are stripped, parens are dropped and `AND`/`OR` are plain words. Operators
//! glue their neighbours into one literal (`std::string`, `Re: meeting`,
//! `x := 5`), and a filter after whitespace still counts
//! (`Re: type:Document`). A lex error degrades to "the whole input is the
//! term". A recognized key with an invalid value is an error, which the
//! search box marks as you type.

use std::ops::Range;

use super::lexer::{tokenize_spanned, Token};
use super::pattern::{RegexQuery, TermPart, TermPattern};
use super::translator::{build_filter, is_filter_key, is_regex_key, TranslateError};
use super::Op;

/// The cascade's parsed input: one term string plus composable filter SQL.
#[derive(Debug, Clone, Default)]
pub struct CascadeQuery {
    /// The ranked search phrase; empty for filter-only or regex-only input.
    pub term: String,
    pub pattern: TermPattern,
    /// A `regex:` filter, matched in Rust and never part of the SQL.
    pub regex: Option<RegexQuery>,
    /// ` AND (...)` fragments appended verbatim to every stage's WHERE clause.
    pub filter_sql: String,
    pub filter_params: Vec<rusqlite::types::Value>,
}

impl CascadeQuery {
    /// Nothing to rank on: filters alone don't drive a search.
    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty() && self.regex.is_none()
    }

    fn term_only(term: &str) -> CascadeQuery {
        let term = term.trim().to_string();
        // Un-lexable input is searched verbatim; stars are not wildcards here.
        let pattern = TermPattern::build(&[TermPart {
            text: term.clone(),
            glob: false,
        }])
        .expect("literal patterns always compile");
        CascadeQuery {
            term,
            pattern,
            ..CascadeQuery::default()
        }
    }
}

/// The recognized filter starting at `tokens[i]`, if one is there:
/// `Word(key) Op [Op] (Word|Quoted)`, since the lexer emits `modified:>=x` as
/// `Word Op(:) Op(>=) Word`. Returns the key, the effective operator, the
/// value, whether the value is an unquoted word (quoted values keep `*`
/// literal), and the index after the value.
fn filter_at(tokens: &[(Token, Range<usize>)], i: usize) -> Option<(&str, Op, &str, bool, usize)> {
    let (Token::Word(key), Some((Token::Op(op1), _))) = (&tokens[i].0, tokens.get(i + 1)) else {
        return None;
    };
    if !is_filter_key(key) && !is_regex_key(key) {
        return None;
    }
    let (op, value_idx) = match tokens.get(i + 2) {
        Some((Token::Op(op2), _)) => (*op2, i + 3),
        _ => (*op1, i + 2),
    };
    match &tokens.get(value_idx)?.0 {
        Token::Word(value) => Some((key, op, value, true, value_idx + 1)),
        Token::Quoted(value) => Some((key, op, value, false, value_idx + 1)),
        _ => None,
    }
}

/// The literal chain starting at `tokens[i]`, if one starts there: its text
/// and the index after it. Operators glue their neighbours, across whitespace
/// too, so a chain is a run of words, quoted phrases, operators and `AND`/`OR`
/// in which every adjacent pair has an operator. The text joins the tokens'
/// texts with one space wherever the input had whitespace. A chain stops
/// before a word that follows whitespace and starts a recognized filter, so
/// `x ::= type:Audio` keeps its filter and `std::path::Path` gains none.
/// The search box's highlighter takes its plain runs from here too.
pub fn literal_chain(tokens: &[(Token, Range<usize>)], i: usize) -> Option<(String, usize)> {
    let is_op = |j: usize| matches!(tokens.get(j), Some((Token::Op(_), _)));
    // A recognized key and an operator: a filter, complete or still typed.
    let starts_filter = |j: usize| {
        matches!(&tokens[j].0, Token::Word(w) if is_filter_key(w) || is_regex_key(w))
            && is_op(j + 1)
    };
    let text_of = |j: usize| match &tokens.get(j)?.0 {
        Token::Word(w) | Token::Quoted(w) => Some(w.as_str()),
        Token::Op(op) => Some(op.as_str()),
        Token::And => Some("AND"),
        Token::Or => Some("OR"),
        Token::LParen | Token::RParen => None,
    };
    let first = text_of(i)?;
    if !is_op(i) && !is_op(i + 1) {
        return None;
    }
    let mut text = first.to_string();
    let mut j = i + 1;
    while let Some(piece) = text_of(j) {
        let gap = tokens[j - 1].1.end < tokens[j].1.start;
        if !(is_op(j - 1) || is_op(j)) || (gap && starts_filter(j)) {
            break;
        }
        if gap {
            text.push(' ');
        }
        text.push_str(piece);
        j += 1;
    }
    Some((text, j))
}

pub fn split_for_cascade(input: &str) -> Result<CascadeQuery, TranslateError> {
    // NULs break SQLite text binding and the FTS5 query parser, so strip them.
    let input = input.replace('\0', "");
    let input = input.as_str();
    let (tokens, err) = tokenize_spanned(input);
    if err.is_some() {
        // Half-typed input: the whole raw string is the term.
        return Ok(CascadeQuery::term_only(input));
    }

    let mut out = CascadeQuery::default();
    let mut term_parts: Vec<TermPart> = Vec::new();
    let mut i = 0usize;
    // Only plain unquoted words are wildcard-eligible.
    let literal = |text: &str| TermPart {
        text: text.to_string(),
        glob: false,
    };

    while i < tokens.len() {
        if let Some((key, op, value, value_is_word, next)) = filter_at(&tokens, i) {
            i = next;
            if is_regex_key(key) {
                // Not a SQL filter: matched in Rust.
                if op != Op::Contains {
                    return Err(TranslateError::UnsupportedOp {
                        key: key.to_string(),
                        op,
                    });
                }
                if out.regex.is_some() {
                    return Err(TranslateError::BadRegex("only one regex: per query".into()));
                }
                out.regex = Some(RegexQuery::new(value)?);
            } else {
                let frag = build_filter(key, op, value, value_is_word)?;
                out.filter_sql.push_str(" AND (");
                out.filter_sql.push_str(&frag.sql);
                out.filter_sql.push(')');
                out.filter_params.extend(frag.params);
            }
            continue;
        }
        if let Some((text, next)) = literal_chain(&tokens, i) {
            // Stars in a chain stay literal (`a:::b*`).
            term_parts.push(literal(&text));
            i = next;
            continue;
        }
        match &tokens[i].0 {
            Token::Word(word) => term_parts.push(TermPart {
                text: word.clone(),
                glob: word.contains('*'),
            }),
            Token::Quoted(q) => term_parts.push(literal(q)),
            Token::And => term_parts.push(literal("AND")),
            Token::Or => term_parts.push(literal("OR")),
            // An operator always starts a chain.
            Token::LParen | Token::RParen | Token::Op(_) => {}
        }
        i += 1;
    }

    out.term = term_parts
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    out.pattern = TermPattern::build(&term_parts)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    /// The `path:` param for `dir` in the stored spelling: Windows respells
    /// `/` as `\`.
    fn subtree_param(dir: &str) -> Value {
        Value::Text(crate::query::translator::like_subtree_pattern(
            &dir.replace('/', std::path::MAIN_SEPARATOR_STR),
        ))
    }

    #[test]
    fn plain_words_join_in_order() {
        let q = split_for_cascade("hello brave world").unwrap();
        assert_eq!(q.term, "hello brave world");
        assert!(q.filter_sql.is_empty());
        assert!(q.filter_params.is_empty());
    }

    #[test]
    fn empty_input() {
        let q = split_for_cascade("").unwrap();
        assert_eq!(q.term, "");
        assert!(q.filter_sql.is_empty());
    }

    #[test]
    fn each_recognized_filter_key_extracts() {
        for input in [
            "type:Audio",
            "modified:>=2024-01-01",
            "mtime:<2023-12-01",
            "path:/home/me",
            "folder:/home/me",
            "includefolder:/home/me",
            "name:report",
            "filename:report",
            "mime:application/pdf",
        ] {
            let q = split_for_cascade(input).unwrap();
            assert_eq!(q.term, "", "input {:?} should be pure filter", input);
            assert!(
                q.filter_sql.starts_with(" AND ("),
                "input {:?} → {:?}",
                input,
                q.filter_sql
            );
            assert!(!q.filter_params.is_empty(), "input {:?}", input);
        }
    }

    #[test]
    fn filters_and_term_mix() {
        let q = split_for_cascade("type:Document budget report modified:>=2024-01-01").unwrap();
        assert_eq!(q.term, "budget report");
        assert_eq!(q.filter_sql.matches(" AND (").count(), 2);
        assert_eq!(q.filter_params.len(), 2); // type bits + mtime bound
    }

    #[test]
    fn unknown_key_stays_literal() {
        let q = split_for_cascade("meeting 12:30 notes").unwrap();
        assert_eq!(q.term, "meeting 12:30 notes");
        assert!(q.filter_sql.is_empty());
    }

    #[test]
    fn unknown_key_chain_reassembles() {
        let q = split_for_cascade("foo:bar:baz").unwrap();
        assert_eq!(q.term, "foo:bar:baz");
    }

    #[test]
    fn half_typed_quote_is_whole_term() {
        let q = split_for_cascade("\"unclosed phrase").unwrap();
        assert_eq!(q.term, "\"unclosed phrase");
        assert!(q.filter_sql.is_empty());
    }

    #[test]
    fn half_typed_filter_key_is_literal() {
        let q = split_for_cascade("type:").unwrap();
        assert_eq!(q.term, "type:");
        assert!(q.filter_sql.is_empty());
    }

    #[test]
    fn recognized_key_bad_value_errors() {
        assert!(matches!(
            split_for_cascade("modified:>=not-a-date"),
            Err(TranslateError::BadDate(_))
        ));
        assert!(matches!(
            split_for_cascade("type:NotAThing"),
            Err(TranslateError::UnknownProperty(_))
        ));
    }

    #[test]
    fn and_or_parens_are_plain_text() {
        let q = split_for_cascade("(alpha AND beta) OR gamma").unwrap();
        assert_eq!(q.term, "alpha AND beta OR gamma");
    }

    #[test]
    fn a_windows_drive_path_reaches_the_filter_intact() {
        let q = split_for_cascade(r"path:C:\Users\me\docs").unwrap();
        assert_eq!(q.term, "", "the whole input is a filter");
        assert_eq!(
            q.filter_params[0],
            Value::Text(crate::query::translator::like_subtree_pattern(
                r"C:\Users\me\docs"
            )),
            "{:?}",
            q.filter_params
        );
    }

    #[test]
    fn quoted_value_for_filter() {
        let q = split_for_cascade("path:\"/home/me/My Documents\"").unwrap();
        assert_eq!(q.term, "");
        assert_eq!(q.filter_params[0], subtree_param("/home/me/My Documents"));
    }

    #[test]
    #[cfg(windows)]
    fn a_forward_slash_drive_path_reaches_the_filter_as_stored() {
        let q = split_for_cascade("path:D:/data").unwrap();
        assert_eq!(q.term, "", "the whole input is a filter");
        assert_eq!(
            q.filter_params[0],
            Value::Text(crate::query::translator::like_subtree_pattern(r"D:\data"))
        );
    }

    #[test]
    fn quoted_phrase_joins_term() {
        let q = split_for_cascade("\"exact phrase\" extra").unwrap();
        assert_eq!(q.term, "exact phrase extra");
    }

    #[test]
    fn injection_shapes_stay_bound() {
        // filter_sql must never contain user text.
        let q = split_for_cascade("mime:application/x-foo'; DROP TABLE files; --").unwrap();
        assert!(!q.filter_sql.contains("DROP"), "{}", q.filter_sql);
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t.contains("x-foo'")));

        let q = split_for_cascade("name:%_\\").unwrap();
        // LIKE-escaped inside the bound param, not the SQL.
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t == "%\\%\\_\\\\%"));
    }

    #[test]
    fn unquoted_star_builds_a_wildcard_pattern() {
        let q = split_for_cascade("foo*").unwrap();
        assert_eq!(q.term, "foo*");
        assert!(q.pattern.is_wildcard());
        assert!(q.pattern.whole_match("foobar", false));
    }

    #[test]
    fn quoted_star_stays_literal() {
        let q = split_for_cascade("\"foo*\"").unwrap();
        assert_eq!(q.pattern.literal(), Some("foo*"));
    }

    #[test]
    fn bare_star_matches_nothing() {
        for input in ["*", "**", "* *"] {
            let q = split_for_cascade(input).unwrap();
            assert!(
                q.pattern.find_first("anything", true).is_none(),
                "{:?}",
                input
            );
        }
        // "* *" has an interior literal space; plain stars are Empty.
        assert!(split_for_cascade("*").unwrap().pattern.is_empty());
        assert!(split_for_cascade("*").unwrap().is_empty());
    }

    #[test]
    fn glued_unknown_keys_keep_stars_literal() {
        let q = split_for_cascade("foo:ba*r").unwrap();
        assert_eq!(q.pattern.literal(), Some("foo:ba*r"));
    }

    #[test]
    fn lex_error_degrade_keeps_stars_literal() {
        let q = split_for_cascade("re*port \"unclosed").unwrap();
        assert_eq!(q.pattern.literal(), Some("re*port \"unclosed"));
    }

    #[test]
    fn regex_keyword_compiles_out_of_band() {
        let q = split_for_cascade("regex:foo\\d+").unwrap();
        assert_eq!(q.term, "");
        assert!(q.pattern.is_empty());
        assert!(!q.is_empty(), "a regex-only query still searches");
        let re = q.regex.unwrap();
        assert!(re.is_match("FOO12"));
        assert!(q.filter_sql.is_empty(), "regex is not a SQL filter");
    }

    #[test]
    fn regex_value_may_be_quoted_and_key_is_case_insensitive() {
        let q = split_for_cascade("REGEX:\"foo (bar|baz)\"").unwrap();
        assert!(q.regex.unwrap().is_match("foo bar"));
    }

    #[test]
    fn regex_mixes_with_filters_and_term() {
        let q = split_for_cascade("regex:\\d+ type:Text budget").unwrap();
        assert_eq!(q.term, "budget");
        assert!(q.regex.is_some());
        assert_eq!(q.filter_sql.matches(" AND (").count(), 1);
    }

    #[test]
    fn regex_error_shapes() {
        assert!(matches!(
            split_for_cascade("regex:["),
            Err(TranslateError::BadRegex(_))
        ));
        assert!(matches!(
            split_for_cascade("regex=x"),
            Err(TranslateError::UnsupportedOp { .. })
        ));
        assert!(matches!(
            split_for_cascade("regex:a regex:b"),
            Err(TranslateError::BadRegex(_))
        ));
        assert!(matches!(
            split_for_cascade("regex:.*"),
            Err(TranslateError::BadRegex(_))
        ));
    }

    #[test]
    fn dangling_regex_key_is_literal_text() {
        let q = split_for_cascade("regex:").unwrap();
        assert_eq!(q.term, "regex:");
        assert!(q.regex.is_none());
    }

    #[test]
    fn name_filter_star_becomes_like_wildcard() {
        let q = split_for_cascade("name:foo*bar").unwrap();
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t == "%foo%bar%"));

        // Quoted value: star stays a literal character.
        let q = split_for_cascade("name:\"fo*o\"").unwrap();
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t == "%fo*o%"));

        // User LIKE metacharacters stay escaped even in glob values.
        let q = split_for_cascade("name:%*_").unwrap();
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t == "%\\%%\\_%"));

        // path: values never glob; the star reaches the pattern as a literal.
        let q = split_for_cascade("path:/da*ta").unwrap();
        assert_eq!(q.filter_params[0], subtree_param("/da*ta"));
    }

    #[test]
    fn nul_bytes_stripped_and_long_terms_pass_through() {
        let q = split_for_cascade("abc\0def").unwrap();
        assert_eq!(q.term, "abcdef");

        let long = "x".repeat(10_240);
        let q = split_for_cascade(&long).unwrap();
        assert_eq!(q.term.len(), 10_240);
    }

    #[test]
    fn a_bare_regex_value_keeps_its_operators() {
        let q = split_for_cascade(r"regex:(foo|bar)\d").unwrap();
        assert_eq!(q.term, "");
        assert!(q.regex.unwrap().is_match("BAR7"));

        let q = split_for_cascade(r"regex:\d+:\d+ budget").unwrap();
        assert_eq!(q.term, "budget");
        assert!(q.regex.unwrap().is_match("at 12:30"));

        let q = split_for_cascade(r"regex:café\d").unwrap();
        assert!(q.regex.unwrap().is_match("Café9"));

        // Whitespace behind the colon is skipped, as for every key.
        let q = split_for_cascade("regex: (a|b)c next").unwrap();
        assert_eq!(q.term, "next");
        assert!(q.regex.unwrap().is_match("bc"));
    }

    #[test]
    fn a_broken_bare_regex_is_an_error() {
        let deep = format!("regex:{}", "(".repeat(2000));
        for input in ["regex:(", "(regex:foo)", deep.as_str()] {
            assert!(
                matches!(split_for_cascade(input), Err(TranslateError::BadRegex(_))),
                "{:?}",
                &input[..input.len().min(20)]
            );
        }
        assert!(matches!(
            split_for_cascade("regex<a"),
            Err(TranslateError::UnsupportedOp { .. })
        ));
    }

    #[test]
    fn operator_chains_keep_the_text_as_typed() {
        for input in [
            "Re: meeting",
            "#include <stdio.h>",
            "std::string",
            "std::vector<int>",
            "std::path::Path",
            "fe80::1",
            "::1",
            ":foo",
            "x := 5",
            "a:>b",
            "a:::b",
            "a:AND",
            "a: AND b",
            "meeting notes: draft",
            "12:30",
            "foo:bar:baz",
            "type:",
            "std::",
            "x:type:Audio",
        ] {
            let q = split_for_cascade(input).unwrap();
            assert_eq!(q.term, input);
            assert_eq!(q.pattern.literal(), Some(input), "{:?}", input);
            assert!(q.filter_sql.is_empty(), "{:?}: {}", input, q.filter_sql);
            assert!(q.regex.is_none(), "{:?}", input);
        }
    }

    #[test]
    fn chains_collapse_whitespace_and_strip_quotes() {
        for (input, term) in [
            ("Re:\t\tmeeting", "Re: meeting"),
            ("foo: \"a b\"", "foo: a b"),
            ("\"a b\":c", "a b:c"),
            ("x  :=   5", "x := 5"),
            ("foo:(bar)", "foo: bar"),
        ] {
            assert_eq!(split_for_cascade(input).unwrap().term, term, "{:?}", input);
        }
    }

    #[test]
    fn a_chain_is_never_a_wildcard() {
        let q = split_for_cascade("a:::b*").unwrap();
        assert_eq!(q.pattern.literal(), Some("a:::b*"));
        let q = split_for_cascade("x:y b*").unwrap();
        assert!(q.pattern.is_wildcard(), "a lone word still globs");
    }

    #[test]
    fn a_filter_after_whitespace_ends_a_chain() {
        for (input, term) in [
            ("x ::= type:Audio", "x ::="),
            ("Re: type:Document", "Re:"),
            ("Re: type : Audio", "Re:"),
            ("type: Audio", ""),
            (": type:Audio", ":"),
        ] {
            let q = split_for_cascade(input).unwrap();
            assert_eq!(q.term, term, "{:?}", input);
            assert_eq!(q.filter_sql.matches(" AND (").count(), 1, "{:?}", input);
        }
        let q = split_for_cascade(r"Re: regex:(a|b)\d").unwrap();
        assert_eq!(q.term, "Re:");
        assert!(q.regex.unwrap().is_match("a1"));
        // A key still being typed after the chain stays text.
        assert_eq!(split_for_cascade("Re: type:").unwrap().term, "Re: type:");
    }
}
