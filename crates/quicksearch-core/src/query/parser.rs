//! Recursive-descent parser: token stream → [`Term`] tree.
//!
//! Grammar (implicit AND between adjacent terms):
//! ```text
//! expr    := or_expr
//! or_expr := and_expr ("OR" and_expr)*
//! and_expr := atom ("AND"? atom)*
//! atom    := "(" expr ")" | property | literal
//! property := ident ":"|"="|"<"|"<="|">"|">=" value
//! literal := WORD | QUOTED
//! ```

use super::ast::{Op, Term};
use super::lexer::{tokenize, LexError, Token};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Lex(LexError),
    Unexpected { at: usize, reason: String },
    Empty,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Lex(e) => write!(f, "{}", e),
            ParseError::Unexpected { at, reason } => {
                write!(f, "parse error at token {}: {}", at, reason)
            }
            ParseError::Empty => write!(f, "empty query"),
        }
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Term, ParseError> {
    let tokens = tokenize(input).map_err(ParseError::Lex)?;
    let trimmed: Vec<_> = tokens.into_iter().collect();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut p = Parser { tokens: trimmed, pos: 0 };
    let term = p.parse_or()?;
    if p.pos < p.tokens.len() {
        return Err(ParseError::Unexpected {
            at: p.pos,
            reason: format!("trailing token {:?}", p.tokens[p.pos]),
        });
    }
    Ok(term)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Result<Term, ParseError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.bump();
            let right = self.parse_and()?;
            left = Term::or(left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Term, ParseError> {
        let mut left = self.parse_atom()?;
        loop {
            match self.peek() {
                Some(Token::And) => {
                    self.bump();
                    let right = self.parse_atom()?;
                    left = Term::and(left, right);
                }
                // Implicit AND: adjacent atoms without an operator.
                Some(Token::Word(_))
                | Some(Token::Quoted(_))
                | Some(Token::LParen) => {
                    let right = self.parse_atom()?;
                    left = Term::and(left, right);
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_atom(&mut self) -> Result<Term, ParseError> {
        match self.bump() {
            Some(Token::LParen) => {
                let inner = self.parse_or()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(inner),
                    other => Err(ParseError::Unexpected {
                        at: self.pos,
                        reason: format!("expected ')' got {:?}", other),
                    }),
                }
            }
            Some(Token::Quoted(s)) => Ok(Term::Literal(s)),
            Some(Token::Word(w)) => {
                // If followed by an operator, this word is a property key.
                if let Some(Token::Op(op)) = self.peek().cloned() {
                    self.bump();
                    // `key:>=value` — an Op followed by another Op becomes the
                    // effective comparator; the original colon is "separator".
                    let effective_op = if op == Op::Contains {
                        if let Some(Token::Op(inner_op)) = self.peek().cloned() {
                            self.bump();
                            inner_op
                        } else {
                            Op::Contains
                        }
                    } else {
                        op
                    };
                    let value = match self.bump() {
                        Some(Token::Word(v)) => v,
                        Some(Token::Quoted(v)) => v,
                        other => {
                            return Err(ParseError::Unexpected {
                                at: self.pos,
                                reason: format!("expected property value got {:?}", other),
                            })
                        }
                    };
                    Ok(Term::Property {
                        key: w,
                        op: effective_op,
                        value,
                    })
                } else {
                    Ok(Term::Literal(w))
                }
            }
            other => Err(ParseError::Unexpected {
                at: self.pos,
                reason: format!("expected term, got {:?}", other),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Term {
        Term::Literal(s.into())
    }

    fn prop(k: &str, op: Op, v: &str) -> Term {
        Term::Property {
            key: k.into(),
            op,
            value: v.into(),
        }
    }

    #[test]
    fn single_word() {
        assert_eq!(parse("foo").unwrap(), lit("foo"));
    }

    #[test]
    fn quoted_phrase() {
        assert_eq!(parse(r#""hello world""#).unwrap(), lit("hello world"));
    }

    #[test]
    fn implicit_and() {
        assert_eq!(
            parse("foo bar").unwrap(),
            Term::And(vec![lit("foo"), lit("bar")])
        );
    }

    #[test]
    fn explicit_and() {
        assert_eq!(
            parse("foo AND bar").unwrap(),
            Term::And(vec![lit("foo"), lit("bar")])
        );
    }

    #[test]
    fn or_has_lower_precedence_than_and() {
        assert_eq!(
            parse("a b OR c").unwrap(),
            Term::Or(vec![
                Term::And(vec![lit("a"), lit("b")]),
                lit("c")
            ])
        );
    }

    #[test]
    fn parens_override_precedence() {
        assert_eq!(
            parse("a (b OR c)").unwrap(),
            Term::And(vec![lit("a"), Term::Or(vec![lit("b"), lit("c")])])
        );
    }

    #[test]
    fn property_contains() {
        assert_eq!(
            parse("type:Audio").unwrap(),
            prop("type", Op::Contains, "Audio")
        );
    }

    #[test]
    fn property_comparator_via_colon() {
        assert_eq!(
            parse("modified:>=2024-01-01").unwrap(),
            prop("modified", Op::Ge, "2024-01-01")
        );
    }

    #[test]
    fn property_comparator_bare() {
        assert_eq!(
            parse("modified>=2024-01-01").unwrap(),
            prop("modified", Op::Ge, "2024-01-01")
        );
    }

    #[test]
    fn mixed_filter_and_literal() {
        assert_eq!(
            parse("type:Audio beatles").unwrap(),
            Term::And(vec![
                prop("type", Op::Contains, "Audio"),
                lit("beatles")
            ])
        );
    }

    #[test]
    fn nested_or() {
        assert_eq!(
            parse("(a OR b) AND (c OR d)").unwrap(),
            Term::And(vec![
                Term::Or(vec![lit("a"), lit("b")]),
                Term::Or(vec![lit("c"), lit("d")])
            ])
        );
    }

    #[test]
    fn empty_query_is_error() {
        assert!(matches!(parse("   "), Err(ParseError::Empty)));
    }

    #[test]
    fn trailing_token_is_error() {
        assert!(parse("a )").is_err());
    }

    #[test]
    fn property_value_may_be_quoted() {
        assert_eq!(
            parse(r#"path:"/tmp with space""#).unwrap(),
            prop("path", Op::Contains, "/tmp with space")
        );
    }
}
