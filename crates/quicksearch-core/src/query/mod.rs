//! Search-box input → the ranked cascade's term plus its structured filters.
//!
//! The entry point is [`split_for_cascade`], which splits raw input into a
//! single search phrase and zero or more `key:value` filters. There is no
//! boolean grammar by design: the cascade ranks one phrase, so `AND`, `OR` and
//! parentheses are ordinary words rather than operators (see [`split`]).
//!
//! Recognized filters:
//!
//! - `type:Audio` / `Image` / `Video` / `Document` / `Text` / `Archive` /
//!   `Spreadsheet` / `Presentation` / `Folder`
//! - `modified:>=2024-01-01`, `modified:<2023-12-01`, `modified:=2024-05-20`
//!   (`mtime:` is a synonym; `=` matches the whole day)
//! - `path:/some/dir` — that directory or any beneath it (`folder:`,
//!   `includefolder:` are synonyms)
//! - `name:report` / `filename:report` — filename substring, `*` globs
//! - `mime:application/pdf` — exact MIME type
//! - `regex:…` — compiled in [`pattern`] and matched in Rust, never in SQL
//!
//! Each recognized filter becomes a [`translator::FilterFragment`] that the
//! cascade ANDs onto every stage; everything else joins the term.

pub mod ast;
pub mod lexer;
pub mod pattern;
pub mod split;
pub mod translator;

pub use ast::Op;
pub use lexer::tokenize_spanned;
pub use pattern::{RegexQuery, TermPattern};
pub use split::{split_for_cascade, CascadeQuery};
pub use translator::TranslateError;
