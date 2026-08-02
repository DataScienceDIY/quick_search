//! Structured query parser and SQL translator.
//!
//! Input syntax (a deliberate subset of KDE Baloo's query language — just
//! enough to be useful standalone; full Baloo grammar lives in the Set B
//! compat layer):
//!
//! - plain words: `foo bar` (implicit AND)
//! - quoted phrases: `"hello world"`
//! - boolean operators: `AND`, `OR` (case-sensitive)
//! - grouping: `(a OR b)`
//! - structured filters:
//!   - `type:Audio` / `type:Image` / `type:Document` / `type:Text` / `type:Video`
//!     / `type:Archive` / `type:Spreadsheet` / `type:Presentation` / `type:Folder`
//!   - `modified:>=2024-01-01`, `modified:<2023-12-01`, `modified:=2024-05-20`
//!     (also accepts `modified>=2024-01-01` without the colon)
//!   - `path:/some/dir` — matches files with that directory as their parent
//!     or any ancestor.
//!
//! The entry point is [`parse_and_build`], which accepts a query string and
//! returns a ready-to-execute [`SqlQuery`].

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod pattern;
pub mod split;
pub mod translator;

pub use ast::{Op, Term};
pub use lexer::tokenize_spanned;
pub use pattern::{RegexQuery, TermPattern};
pub use split::{split_for_cascade, CascadeQuery};
pub use translator::{parse_and_build, SqlQuery, TranslateError};
