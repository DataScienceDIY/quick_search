//! Turning "something that must be present" into a SQL narrowing.
//!
//! Three of the cascade's passes would otherwise read the whole index on every
//! keystroke: the fuzzy full-text pass decompresses every stored document, and
//! the two `regex:` passes run the user's regex over every name, every path and
//! every document. In each case there is a set of literal strings of which **at
//! least one must occur** in anything the pass can accept, and that set is
//! enough to make the database do the rejecting instead.
//!
//! Where the sets come from differs; what is done with them does not, which is
//! why they meet here:
//!
//! * the fuzzy pass splits its term into `k + 1` chunks, at most `k` of which
//!   an in-budget match can damage — see
//!   [`crate::search::fuzzy::pigeonhole_chunks`];
//! * a `regex:` query hands its pattern to the same literal analysis the regex
//!   engine uses to build its own prefilter — see
//!   [`crate::query::pattern::RegexQuery`].
//!
//! # The one rule
//!
//! A prefilter must be a **superset** of what the pass accepts. It may admit
//! rows the pass then rejects — every candidate is verified afterwards exactly
//! as before, so ranking and results do not change, only how many rows are
//! looked at. It must never exclude a row the pass would have accepted, because
//! that failure has no symptom: the file simply stops appearing, and no error
//! is raised anywhere.
//!
//! Everything below is guards in service of that rule.

use rusqlite::types::Value;

use crate::query::translator::{escape_like, quote_phrase};

/// Characters in the smallest unit the FTS5 trigram index can be queried for.
///
/// A phrase shorter than this matches no token at all, so a prefilter built
/// from one would return the empty set rather than a superset — the exact
/// failure the module note forbids. It bounds [`Required::fts_expr`] only:
/// `LIKE` has no such floor, which is why the two predicates guard separately.
pub const TRIGRAM_FLOOR: usize = 3;

/// Most literals worth OR-ing together.
///
/// A case-insensitive pattern expands combinatorially — `(?i)FOO` extracts as
/// eight literals — and each one costs a term in the MATCH expression or two
/// `LIKE`s per row. Past some width the filter stops being cheaper than the
/// scan it replaces, and falling back to the scan is always correct.
const MAX_LITERALS: usize = 32;

/// Literals of which at least one occurs in anything the query can match.
#[derive(Debug, Clone)]
pub struct Required(Vec<String>);

impl Required {
    /// `None` when the set cannot constrain anything: empty, too wide to be
    /// worth it, or containing an empty literal.
    ///
    /// The empty-literal case is the one that matters. A literal set containing
    /// `""` says "a match may begin with nothing", which is not a constraint at
    /// all — building a filter from it would narrow to rows containing the
    /// empty string, which is a statement SQL is entitled to answer any way it
    /// likes. Callers see `None` and scan.
    pub fn new(literals: Vec<String>) -> Option<Required> {
        if literals.is_empty() || literals.len() > MAX_LITERALS {
            return None;
        }
        if literals.iter().any(|l| l.is_empty()) {
            return None;
        }
        Some(Required(literals))
    }

    pub fn literals(&self) -> &[String] {
        &self.0
    }

    /// A `searchabletext MATCH` expression: `(text: "a" OR text: "b" …)`.
    ///
    /// `None` when any literal is shorter than [`TRIGRAM_FLOOR`] **characters**.
    /// Characters, not bytes: `café` is five bytes and four characters, and
    /// `日本` is six bytes and two — the tokenizer indexes character triples, so
    /// a byte-length test would admit a phrase that matches no token.
    ///
    /// Every literal goes through [`quote_phrase`], which is what makes this
    /// safe for text the user typed: a literal can contain `"`, `*`, `:`,
    /// `NEAR` and the rest of FTS5's syntax, and unquoted that is a syntax
    /// error rather than a search.
    ///
    /// The index folds case and strips diacritics (`remove_diacritics 1`), so
    /// it matches *more* than the literal as written. That direction is the
    /// harmless one.
    pub fn fts_expr(&self) -> Option<String> {
        if self
            .0
            .iter()
            .any(|l| l.chars().count() < TRIGRAM_FLOOR)
        {
            return None;
        }
        Some(format!(
            "({})",
            self.0
                .iter()
                .map(|l| format!("text: {}", quote_phrase(l)))
                .collect::<Vec<_>>()
                .join(" OR ")
        ))
    }

    /// A predicate over the `files` columns, plus the values it binds:
    /// `(f.name LIKE ? OR f.parent LIKE ? OR …)`.
    ///
    /// `None` when any literal contains a path separator.
    ///
    /// # Why the separator matters
    ///
    /// There is no `path` column — a file's path is `parent || name` — so a
    /// literal has to be looked for in the two columns separately. An
    /// occurrence in the concatenation lies wholly inside `parent`, wholly
    /// inside `name`, or spans the join. A spanning occurrence necessarily
    /// covers the byte before the boundary, and that byte is `parent`'s last,
    /// which is always a separator (see
    /// [`crate::file_handling::dir_to_db_parent`]). So a literal with no
    /// separator in it cannot span the join and the two-column test sees it
    /// wherever it is — while a literal *with* one might sit exactly across the
    /// boundary, be invisible to both `LIKE`s, and take its row with it.
    ///
    /// All-or-nothing on purpose: the set is an OR, so a row whose only present
    /// literal is the untestable one would be dropped. One bad literal
    /// therefore disqualifies the whole predicate rather than being skipped.
    ///
    /// No trigram floor here — `LIKE '%ab%'` is a perfectly good filter.
    pub fn like_predicate(&self) -> Option<(String, Vec<Value>)> {
        if self
            .0
            .iter()
            .any(|l| l.contains(std::path::MAIN_SEPARATOR))
        {
            return None;
        }
        let mut clauses = Vec::with_capacity(self.0.len());
        let mut params = Vec::with_capacity(self.0.len() * 2);
        for literal in &self.0 {
            clauses.push("f.name LIKE ? ESCAPE '\\' OR f.parent LIKE ? ESCAPE '\\'");
            let pattern = format!("%{}%", escape_like(literal));
            params.push(Value::Text(pattern.clone()));
            params.push(Value::Text(pattern));
        }
        Some((format!("({})", clauses.join(" OR ")), params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(lits: &[&str]) -> Option<Required> {
        Required::new(lits.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn a_set_that_cannot_constrain_is_rejected() {
        assert!(req(&[]).is_none(), "nothing to filter on");
        assert!(req(&["abc", ""]).is_none(), "an empty literal is no constraint");
        let wide: Vec<String> = (0..MAX_LITERALS + 1).map(|i| format!("lit{i}")).collect();
        assert!(Required::new(wide).is_none(), "too wide to be worth it");
    }

    #[test]
    fn the_trigram_floor_counts_characters_not_bytes() {
        // Four characters, five bytes: usable.
        assert!(req(&["café"]).unwrap().fts_expr().is_some());
        // Two characters, six bytes: a byte test would wrongly admit this.
        assert!(req(&["日本"]).unwrap().fts_expr().is_none());
        assert!(req(&["ab"]).unwrap().fts_expr().is_none());
        assert!(req(&["abc", "de"]).unwrap().fts_expr().is_none(), "all of them");
    }

    #[test]
    fn fts5_syntax_in_a_literal_is_quoted_inert() {
        let expr = req(&["a\"b", "NEAR", "x*y"]).unwrap().fts_expr().unwrap();
        // The embedded quote is doubled, which is FTS5's own escape.
        assert!(expr.contains(r#""a""b""#), "{expr}");
        assert!(expr.contains(r#""NEAR""#), "{expr}");
        assert!(expr.contains(r#""x*y""#), "{expr}");
        assert_eq!(expr.matches(" OR ").count(), 2);
    }

    #[test]
    fn a_literal_with_a_separator_disqualifies_the_like_predicate() {
        let sep = std::path::MAIN_SEPARATOR;
        assert!(req(&["abc"]).unwrap().like_predicate().is_some());
        assert!(
            req(&["abc", &format!("d{sep}e")])
                .unwrap()
                .like_predicate()
                .is_none(),
            "one untestable literal disqualifies the whole OR"
        );
    }

    #[test]
    fn like_metacharacters_in_a_literal_stay_literal() {
        let (sql, params) = req(&["100%_x"]).unwrap().like_predicate().unwrap();
        assert_eq!(params.len(), 2, "one literal, bound to both columns");
        assert!(sql.contains("ESCAPE '\\'"));
        assert!(
            matches!(&params[0], Value::Text(t) if t == "%100\\%\\_x%"),
            "{:?}",
            params[0]
        );
    }

    #[test]
    fn several_literals_become_one_or_over_both_columns() {
        let (sql, params) = req(&["abc", "def"]).unwrap().like_predicate().unwrap();
        assert_eq!(params.len(), 4);
        assert_eq!(sql.matches("f.name LIKE").count(), 2);
        assert_eq!(sql.matches("f.parent LIKE").count(), 2);
    }
}
