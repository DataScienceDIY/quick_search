//! Shared query vocabulary.

/// The comparison in a `key op value` filter, as the lexer emits it.
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
