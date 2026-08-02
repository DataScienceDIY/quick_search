//! Split raw search-box input into (cascade term, structured filters).
//!
//! The ranked search cascade has no boolean logic by design: everything
//! that isn't a recognized `key:value` filter joins the *term* — the single
//! phrase the cascade matches through its filename/full-text/fuzzy stages.
//! Recognized filters (`type:`, `modified:`, `path:`, `mime:`, `name:`)
//! become parameterized SQL fragments ANDed onto every cascade stage.
//!
//! Robustness rules for search-as-you-type:
//! - A lex error (e.g. a half-typed quote) degrades to "whole input is the
//!   term" — incremental typing must never surface an error.
//! - An *unrecognized* `key:value` (like `12:30`) is reassembled verbatim
//!   into the term.
//! - A recognized key whose value doesn't translate (bad date, unknown type
//!   name) is a real [`TranslateError`] — the caller shows it inline.
//! - `AND`/`OR`/parens are not operators here; the words pass through into
//!   the term, parens are dropped.

use super::ast::Op;
use super::lexer::{tokenize, Token};
use super::pattern::{RegexQuery, TermPart, TermPattern};
use super::translator::{build_filter, is_filter_key, TranslateError};

/// The cascade's parsed input: one term string plus composable filter SQL.
#[derive(Debug, Clone, Default)]
pub struct CascadeQuery {
    /// The ranked search phrase; may be empty when the input was
    /// filter-only or regex-only.
    pub term: String,
    /// `term` compiled for matching: literal, wildcard, or empty.
    pub pattern: TermPattern,
    /// A `regex:` filter, matched in Rust against name, path and content —
    /// never part of the SQL.
    pub regex: Option<RegexQuery>,
    /// Zero or more ` AND (...)` fragments with anonymous `?` placeholders
    /// over alias `f`; appended verbatim to every stage's WHERE clause.
    pub filter_sql: String,
    pub filter_params: Vec<rusqlite::types::Value>,
}

impl CascadeQuery {
    /// Nothing to rank on: no term pattern and no regex. (Filters alone
    /// don't drive a search.)
    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty() && self.regex.is_none()
    }

    fn term_only(term: &str) -> CascadeQuery {
        let term = term.trim().to_string();
        // Un-lexable input is searched verbatim — stars are not wildcards
        // here, mirroring the "whole input is the term" degrade rule.
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

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Contains => ":",
        Op::Eq => "=",
        Op::Lt => "<",
        Op::Le => "<=",
        Op::Gt => ">",
        Op::Ge => ">=",
    }
}

pub fn split_for_cascade(input: &str) -> Result<CascadeQuery, TranslateError> {
    // NUL bytes can't occur in filenames or extracted text, but they do
    // break SQLite text binding and the FTS5 query parser — strip them.
    let input = input.replace('\0', "");
    let input = input.as_str();
    let tokens = match tokenize(input) {
        Ok(t) => t,
        // Half-typed input (unterminated quote, invalid word): the whole
        // raw string is the term. Never an error mid-keystroke.
        Err(_) => return Ok(CascadeQuery::term_only(input)),
    };

    let mut out = CascadeQuery::default();
    let mut term_parts: Vec<TermPart> = Vec::new();
    let mut i = 0usize;
    // Only plain unquoted words are wildcard-eligible; everything else
    // (quoted phrases, demoted AND/OR, reassembled key:value glue) is
    // searched verbatim.
    let literal = |text: &str| TermPart {
        text: text.to_string(),
        glob: false,
    };

    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(word) => {
                // Candidate filter: Word(key) Op [Op] (Word|Quoted).
                // The lexer emits `modified:>=x` as Word Op(:) Op(>=) Word.
                if let Some(Token::Op(op1)) = tokens.get(i + 1) {
                    let (op, value_idx) = match tokens.get(i + 2) {
                        Some(Token::Op(op2)) => (*op2, i + 3),
                        _ => (*op1, i + 2),
                    };
                    let value = match tokens.get(value_idx) {
                        Some(Token::Word(v)) | Some(Token::Quoted(v)) => Some(v.clone()),
                        _ => None,
                    };
                    if let Some(value) = value {
                        // Quoted values keep `*` literal; only a bare word's
                        // stars act as wildcards (`name:` honors this too).
                        let value_is_word =
                            matches!(tokens.get(value_idx), Some(Token::Word(_)));
                        if word.eq_ignore_ascii_case("regex") {
                            // Not a SQL filter: compiled here, matched in
                            // Rust against name, path and content.
                            if op != Op::Contains {
                                return Err(TranslateError::UnsupportedOp {
                                    key: word.clone(),
                                    op,
                                });
                            }
                            if out.regex.is_some() {
                                return Err(TranslateError::BadRegex(
                                    "only one regex: per query".into(),
                                ));
                            }
                            out.regex = Some(RegexQuery::new(&value)?);
                            i = value_idx + 1;
                            continue;
                        }
                        if is_filter_key(word) {
                            let frag = build_filter(word, op, &value, value_is_word)?;
                            out.filter_sql.push_str(" AND (");
                            out.filter_sql.push_str(&frag.sql);
                            out.filter_sql.push(')');
                            out.filter_params.extend(frag.params);
                            i = value_idx + 1;
                            continue;
                        }
                        // Unrecognized key — reassemble verbatim (`12:30`,
                        // `foo:bar`), gluing any further `:value` chains
                        // (`foo:bar:baz`).
                        let mut glued = format!("{}{}{}", word, op_str(op), value);
                        i = value_idx + 1;
                        while let Some(Token::Op(next_op)) = tokens.get(i) {
                            glued.push_str(op_str(*next_op));
                            i += 1;
                            if let Some(Token::Word(v)) | Some(Token::Quoted(v)) = tokens.get(i)
                            {
                                glued.push_str(v);
                                i += 1;
                            }
                        }
                        term_parts.push(literal(&glued));
                        continue;
                    }
                    // Key + op with no value yet (mid-typing "type:"):
                    // pass through as literal text.
                    term_parts.push(literal(&format!("{}{}", word, op_str(*op1))));
                    i += 2;
                    continue;
                }
                term_parts.push(TermPart {
                    text: word.clone(),
                    glob: word.contains('*'),
                });
            }
            Token::Quoted(q) => term_parts.push(literal(q)),
            // Not operators in the cascade grammar — plain words.
            Token::And => term_parts.push(literal("AND")),
            Token::Or => term_parts.push(literal("OR")),
            // Grouping has no meaning without boolean logic.
            Token::LParen | Token::RParen => {}
            // Dangling operator (e.g. "a > b" typed literally).
            Token::Op(op) => term_parts.push(literal(op_str(*op))),
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

    /// The end-to-end shape of the bug: before the lexer fix this produced
    /// the filter `parent = "C"` plus a junk term, and returned nothing.
    #[test]
    fn a_windows_drive_path_reaches_the_filter_intact() {
        let q = split_for_cascade(r"path:C:\Users\me\docs").unwrap();
        assert_eq!(q.term, "", "the whole input is a filter");
        assert!(matches!(
            &q.filter_params[0],
            Value::Text(t) if t == r"C:\Users\me\docs"
        ), "{:?}", q.filter_params);
    }

    #[test]
    fn quoted_value_for_filter() {
        let q = split_for_cascade("path:\"/home/me/My Documents\"").unwrap();
        assert_eq!(q.term, "");
        assert_eq!(
            q.filter_params[0],
            Value::Text("/home/me/My Documents".into())
        );
    }

    #[test]
    fn quoted_phrase_joins_term() {
        let q = split_for_cascade("\"exact phrase\" extra").unwrap();
        assert_eq!(q.term, "exact phrase extra");
    }

    #[test]
    fn injection_shapes_stay_bound() {
        // Everything lands either in the term (never interpolated into
        // SQL by the cascade — bound as parameters there too) or in
        // filter_params. filter_sql must never contain user text.
        let q = split_for_cascade("mime:application/x-foo'; DROP TABLE files; --").unwrap();
        assert!(!q.filter_sql.contains("DROP"), "{}", q.filter_sql);
        // The value went into params (term got the trailing junk words).
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
        // Empty-matchable patterns are rejected loudly.
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

        // path: values never glob.
        let q = split_for_cascade("path:/da*ta").unwrap();
        assert!(matches!(&q.filter_params[0], Value::Text(t) if t == "/da*ta"));
    }

    #[test]
    fn nul_bytes_stripped_and_long_terms_pass_through() {
        // NULs would break SQLite binding / FTS5 parsing downstream.
        let q = split_for_cascade("abc\0def").unwrap();
        assert_eq!(q.term, "abcdef");

        let long = "x".repeat(10_240);
        let q = split_for_cascade(&long).unwrap();
        assert_eq!(q.term.len(), 10_240);
    }
}
