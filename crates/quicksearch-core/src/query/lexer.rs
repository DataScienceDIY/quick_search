//! Tokenizer for the query grammar.

use super::ast::Op;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Unquoted word. `AND`/`OR` are intercepted before emitting a `Word`.
    Word(String),
    Quoted(String),
    LParen,
    RParen,
    /// Binary property operator (`:`, `=`, `>`, `>=`, `<`, `<=`).
    ///
    /// `:` is [`Op::Contains`] by default; the parser re-interprets it when
    /// followed immediately by a comparator (e.g. `modified:>=2024-01-01`).
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

pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
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
                out.push(Token::LParen);
                i += 1;
            }
            b')' => {
                out.push(Token::RParen);
                i += 1;
            }
            b':' => {
                out.push(Token::Op(Op::Contains));
                i += 1;
            }
            b'=' => {
                out.push(Token::Op(Op::Eq));
                i += 1;
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(Op::Le));
                    i += 2;
                } else {
                    out.push(Token::Op(Op::Lt));
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(Op::Ge));
                    i += 2;
                } else {
                    out.push(Token::Op(Op::Gt));
                    i += 1;
                }
            }
            b'"' => {
                // Double-quoted phrase. Supports doubled-quote escape `""`.
                let mut j = i + 1;
                let mut buf = String::new();
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        if bytes.get(j + 1) == Some(&b'"') {
                            buf.push('"');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    buf.push(bytes[j] as char);
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(LexError {
                        message: "unterminated quoted phrase".into(),
                        offset: i,
                    });
                }
                out.push(Token::Quoted(buf));
                i = j + 1;
            }
            _ => {
                // Unquoted word; continues until whitespace, paren, or operator.
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_whitespace()
                        || matches!(c, b'(' | b')' | b':' | b'=' | b'<' | b'>' | b'"')
                    {
                        break;
                    }
                    i += 1;
                }
                let word = std::str::from_utf8(&bytes[start..i])
                    .map_err(|e| LexError {
                        message: format!("invalid UTF-8 in word: {}", e),
                        offset: start,
                    })?
                    .to_string();
                match word.as_str() {
                    "AND" => out.push(Token::And),
                    "OR" => out.push(Token::Or),
                    _ => out.push(Token::Word(word)),
                }
            }
        }
    }

    Ok(out)
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
}
