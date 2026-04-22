//! Query AST shared between parser and translator.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `key:value` — substring / FTS MATCH semantics.
    Contains,
    /// `key=value` — exact match.
    Eq,
    /// `key<value`
    Lt,
    /// `key<=value`
    Le,
    /// `key>value`
    Gt,
    /// `key>=value`
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// An unquoted word or quoted phrase. Feeds the FTS MATCH expression.
    Literal(String),
    /// A structured filter such as `type:Audio` or `modified:>=2024-01-01`.
    Property { key: String, op: Op, value: String },
    And(Vec<Term>),
    Or(Vec<Term>),
}

impl Term {
    /// Combine two terms with AND, flattening to avoid a deeply nested tree.
    pub fn and(a: Term, b: Term) -> Term {
        match (a, b) {
            (Term::And(mut xs), Term::And(ys)) => {
                xs.extend(ys);
                Term::And(xs)
            }
            (Term::And(mut xs), other) | (other, Term::And(mut xs)) => {
                xs.push(other);
                Term::And(xs)
            }
            (a, b) => Term::And(vec![a, b]),
        }
    }

    pub fn or(a: Term, b: Term) -> Term {
        match (a, b) {
            (Term::Or(mut xs), Term::Or(ys)) => {
                xs.extend(ys);
                Term::Or(xs)
            }
            (Term::Or(mut xs), other) | (other, Term::Or(mut xs)) => {
                xs.push(other);
                Term::Or(xs)
            }
            (a, b) => Term::Or(vec![a, b]),
        }
    }
}
