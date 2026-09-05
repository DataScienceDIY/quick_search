//! Search-box input → the ranked cascade's term plus its structured filters.
//!
//! [`split_for_cascade`] splits raw input into one search phrase and zero or
//! more `key:value` filters; there is no boolean grammar by design.
//!
//! Recognized filters: `type:`, `modified:`/`mtime:` (`=` matches the whole
//! day), `path:`/`folder:`/`includefolder:` (the directory and everything
//! beneath it), `name:`/`filename:` (substring, `*` globs), `mime:` (exact),
//! and `regex:` (matched in Rust, never in SQL).

pub mod lexer;
pub mod pattern;
pub mod split;
pub mod translator;

pub use lexer::tokenize_spanned;
pub use pattern::{RegexQuery, TermPattern};
pub use split::{split_for_cascade, CascadeQuery};
pub use translator::TranslateError;

/// The comparison in a `key op value` filter, as the lexer emits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `key:value` — substring / FTS MATCH semantics.
    Contains,
    /// `key=value` — exact match.
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}
