//! Tokenizer for the query grammar.

use super::Op;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(String),
    Quoted(String),
    LParen,
    RParen,
    /// Binary property operator. `:` is [`Op::Contains`] by default; the
    /// parser re-interprets it when a comparator follows (`modified:>=x`).
    Op(Op),
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "query lex error at {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for LexError {}

/// Whether the `:` at `colon` is a drive letter's, not a property operator:
/// true only when the word so far is exactly one ASCII letter *and* a path
/// separator follows — narrow enough to leave `12:30` and `a:b` untouched.
fn is_drive_letter_colon(bytes: &[u8], start: usize, colon: usize) -> bool {
    colon == start + 1
        && bytes[start].is_ascii_alphabetic()
        && matches!(bytes.get(colon + 1), Some(b'\\') | Some(b'/'))
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let (tokens, err) = tokenize_spanned(input);
    match err {
        Some(e) => Err(e),
        None => Ok(tokens.into_iter().map(|(t, _)| t).collect()),
    }
}

/// [`tokenize`], but each token carries its byte range and a trailing error
/// is returned alongside the tokens lexed before it — the GUI's highlighter
/// must color the intact prefix of a half-typed query. `Quoted` spans
/// include both quote characters.
pub fn tokenize_spanned(input: &str) -> (Vec<(Token, std::ops::Range<usize>)>, Option<LexError>) {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b'(' => {
                out.push((Token::LParen, i..i + 1));
                i += 1;
            }
            b')' => {
                out.push((Token::RParen, i..i + 1));
                i += 1;
            }
            b':' => {
                out.push((Token::Op(Op::Contains), i..i + 1));
                i += 1;
            }
            b'=' => {
                out.push((Token::Op(Op::Eq), i..i + 1));
                i += 1;
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push((Token::Op(Op::Le), i..i + 2));
                    i += 2;
                } else {
                    out.push((Token::Op(Op::Lt), i..i + 1));
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push((Token::Op(Op::Ge), i..i + 2));
                    i += 2;
                } else {
                    out.push((Token::Op(Op::Gt), i..i + 1));
                    i += 1;
                }
            }
            b'"' => {
                // Doubled-quote escape. Segments are copied as UTF-8 slices —
                // `bytes[j] as char` would decode Latin-1 and mangle non-ASCII.
                let mut j = i + 1;
                let mut buf = String::new();
                let mut segment_start = j;
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        buf.push_str(&input[segment_start..j]);
                        if bytes.get(j + 1) == Some(&b'"') {
                            buf.push('"');
                            j += 2;
                            segment_start = j;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    return (
                        out,
                        Some(LexError {
                            message: "unterminated quoted phrase".into(),
                            offset: i,
                        }),
                    );
                }
                out.push((Token::Quoted(buf), i..j + 1));
                i = j + 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == b':' && is_drive_letter_colon(bytes, start, i) {
                        // Otherwise `path:C:\Users\me` parses as `path` = `C`.
                        i += 1;
                        continue;
                    }
                    if c.is_ascii_whitespace()
                        || matches!(c, b'(' | b')' | b':' | b'=' | b'<' | b'>' | b'"')
                    {
                        break;
                    }
                    i += 1;
                }
                let word = match std::str::from_utf8(&bytes[start..i]) {
                    Ok(w) => w.to_string(),
                    Err(e) => {
                        return (
                            out,
                            Some(LexError {
                                message: format!("invalid UTF-8 in word: {}", e),
                                offset: start,
                            }),
                        )
                    }
                };
                let tok = match word.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    _ => Token::Word(word),
                };
                out.push((tok, start..i));
            }
        }
    }

    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_words() {
        let t = tokenize("foo bar").unwrap();
        assert_eq!(
            t,
            vec![Token::Word("foo".into()), Token::Word("bar".into())]
        );
    }

    #[test]
    fn and_or_keywords() {
        let t = tokenize("a AND b OR c").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Word("a".into()),
                Token::And,
                Token::Word("b".into()),
                Token::Or,
                Token::Word("c".into()),
            ]
        );
    }

    #[test]
    fn quoted_phrase() {
        let t = tokenize(r#""hello world""#).unwrap();
        assert_eq!(t, vec![Token::Quoted("hello world".into())]);
    }

    #[test]
    fn doubled_quote_is_escape() {
        let t = tokenize(r#""a""b""#).unwrap();
        assert_eq!(t, vec![Token::Quoted(r#"a"b"#.into())]);
    }

    #[test]
    fn property_operators() {
        let t = tokenize("type:Audio modified>=2024-01-01 size<100").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Word("type".into()),
                Token::Op(Op::Contains),
                Token::Word("Audio".into()),
                Token::Word("modified".into()),
                Token::Op(Op::Ge),
                Token::Word("2024-01-01".into()),
                Token::Word("size".into()),
                Token::Op(Op::Lt),
                Token::Word("100".into()),
            ]
        );
    }

    #[test]
    fn parens() {
        let t = tokenize("(a OR b)").unwrap();
        assert_eq!(
            t,
            vec![
                Token::LParen,
                Token::Word("a".into()),
                Token::Or,
                Token::Word("b".into()),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn unterminated_quote_is_error() {
        assert!(tokenize(r#""oops"#).is_err());
    }

    #[test]
    fn non_ascii_survives_a_quoted_phrase() {
        // Byte-wise copying decoded this as Latin-1 (`JosÃ©`).
        let t = tokenize(r#""C:\Users\José\docs""#).unwrap();
        assert_eq!(t, vec![Token::Quoted(r"C:\Users\José\docs".into())]);

        let t = tokenize(r#""ü""ö""#).unwrap();
        assert_eq!(t, vec![Token::Quoted(r#"ü"ö"#.into())]);
    }

    #[test]
    fn a_drive_letter_colon_does_not_split_the_word() {
        let t = tokenize(r"path:C:\Users\me\docs").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Word("path".into()),
                Token::Op(Op::Contains),
                Token::Word(r"C:\Users\me\docs".into()),
            ]
        );

        let t = tokenize("path:D:/data").unwrap();
        assert_eq!(
            t,
            vec![
                Token::Word("path".into()),
                Token::Op(Op::Contains),
                Token::Word("D:/data".into()),
            ]
        );
    }

    #[test]
    fn spans_cover_every_token_shape() {
        let input = r#"type:Audio "a b" (x) size<=5"#;
        let (toks, err) = tokenize_spanned(input);
        assert!(err.is_none());
        let spanned: Vec<(&str, Token)> = toks
            .iter()
            .map(|(t, r)| (&input[r.clone()], t.clone()))
            .collect();
        assert_eq!(
            spanned,
            vec![
                ("type", Token::Word("type".into())),
                (":", Token::Op(Op::Contains)),
                ("Audio", Token::Word("Audio".into())),
                (r#""a b""#, Token::Quoted("a b".into())),
                ("(", Token::LParen),
                ("x", Token::Word("x".into())),
                (")", Token::RParen),
                ("size", Token::Word("size".into())),
                ("<=", Token::Op(Op::Le)),
                ("5", Token::Word("5".into())),
            ]
        );
    }

    #[test]
    fn quoted_span_includes_quotes_and_escapes() {
        let input = r#"x "a""b" y"#;
        let (toks, err) = tokenize_spanned(input);
        assert!(err.is_none());
        assert_eq!(toks[1].0, Token::Quoted(r#"a"b"#.into()));
        assert_eq!(&input[toks[1].1.clone()], r#""a""b""#);
    }

    #[test]
    fn spans_are_byte_offsets_around_non_ascii() {
        let input = r#"José "café" naïve"#;
        let (toks, err) = tokenize_spanned(input);
        assert!(err.is_none());
        assert_eq!(&input[toks[0].1.clone()], "José");
        assert_eq!(&input[toks[1].1.clone()], r#""café""#);
        assert_eq!(&input[toks[2].1.clone()], "naïve");
        assert_eq!(toks[2].1.end, input.len());
    }

    #[test]
    fn unterminated_quote_keeps_prefix_tokens() {
        let (toks, err) = tokenize_spanned(r#"type:Audio "oops"#);
        let err = err.expect("should report the unterminated quote");
        assert_eq!(err.offset, 11, "offset of the opening quote");
        assert_eq!(
            toks.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            vec![
                Token::Word("type".into()),
                Token::Op(Op::Contains),
                Token::Word("Audio".into()),
            ]
        );
    }

    #[test]
    fn stars_stay_inside_words() {
        let (toks, err) = tokenize_spanned("foo*bar *");
        assert!(err.is_none());
        assert_eq!(
            toks.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            vec![Token::Word("foo*bar".into()), Token::Word("*".into())]
        );
    }

    #[test]
    fn tokenize_matches_span_stripped_tokenize_spanned() {
        for input in ["a AND (b:c)", r#""q" x>=2"#, "path:C:\\U foo*"] {
            let plain = tokenize(input).unwrap();
            let (spanned, err) = tokenize_spanned(input);
            assert!(err.is_none());
            let stripped: Vec<Token> = spanned.into_iter().map(|(t, _)| t).collect();
            assert_eq!(plain, stripped, "input {:?}", input);
        }
    }

    #[test]
    fn the_drive_letter_rule_stays_narrow() {
        // Two digits before the colon: still a time, not a drive.
        assert_eq!(
            tokenize("12:30").unwrap(),
            vec![
                Token::Word("12".into()),
                Token::Op(Op::Contains),
                Token::Word("30".into()),
            ]
        );
        assert_eq!(
            tokenize("a:b").unwrap(),
            vec![
                Token::Word("a".into()),
                Token::Op(Op::Contains),
                Token::Word("b".into()),
            ]
        );
        assert_eq!(
            tokenize("type:/Audio").unwrap(),
            vec![
                Token::Word("type".into()),
                Token::Op(Op::Contains),
                Token::Word("/Audio".into()),
            ]
        );
    }
}
