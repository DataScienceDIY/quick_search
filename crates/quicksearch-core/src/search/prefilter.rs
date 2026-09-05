//! Turning "something that must be present" into a SQL narrowing for the
//! fuzzy and `regex:` passes, which would otherwise scan the whole index.
//!
//! # The one rule
//!
//! A prefilter must be a **superset** of what the pass accepts. It may admit
//! rows the pass then rejects — every candidate is verified afterwards, so
//! results do not change, only how many rows are looked at. It must never
//! exclude a row the pass would have accepted, because that failure has no
//! symptom: the file simply stops appearing, and no error is raised anywhere.

use rusqlite::types::Value;

use crate::query::translator::{escape_like, quote_phrase};

/// Characters in the smallest unit the FTS5 trigram index can be queried for.
///
/// A shorter phrase matches no token at all, so a prefilter built from one
/// would return the empty set rather than a superset. Bounds
/// [`Required::fts_expr`] only; `LIKE` has no such floor.
pub const TRIGRAM_FLOOR: usize = 3;

/// Most literals worth OR-ing together; past this the filter stops being
/// cheaper than the scan it replaces, and falling back is always correct.
const MAX_LITERALS: usize = 32;

/// Literals of which at least one occurs in anything the query can match.
#[derive(Debug, Clone)]
pub struct Required(Vec<String>);

impl Required {
    /// `None` when the set cannot constrain anything: empty, too wide to be
    /// worth it, or containing an empty literal (`""` says "a match may begin
    /// with nothing" — no constraint at all). Callers see `None` and scan.
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
    /// `None` when any literal is shorter than [`TRIGRAM_FLOOR`]
    /// **characters** — the tokenizer indexes character triples, so a
    /// byte-length test would admit a phrase that matches no token.
    /// [`quote_phrase`] renders FTS5 syntax in user text inert; the index
    /// folds case and diacritics, so it matches *more* than the literal as
    /// written — the harmless direction.
    pub fn fts_expr(&self) -> Option<String> {
        if self.0.iter().any(|l| l.chars().count() < TRIGRAM_FLOOR) {
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
    /// `None` when any literal contains a path separator: a path is
    /// `parent || name`, and an occurrence spanning the join would be
    /// invisible to both per-column `LIKE`s and take its row with it. A
    /// separator-free literal cannot span the join, because `parent`'s last
    /// byte is always a separator ([`crate::file_handling::dir_to_db_parent`]).
    /// The set is an OR, so one untestable literal disqualifies the whole
    /// predicate. No trigram floor here — `LIKE '%ab%'` is a fine filter.
    pub fn like_predicate(&self) -> Option<(String, Vec<Value>)> {
        if self.0.iter().any(|l| l.contains(std::path::MAIN_SEPARATOR)) {
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
        assert!(
            req(&["abc", ""]).is_none(),
            "an empty literal is no constraint"
        );
        let wide: Vec<String> = (0..MAX_LITERALS + 1).map(|i| format!("lit{i}")).collect();
        assert!(Required::new(wide).is_none(), "too wide to be worth it");
    }

    #[test]
    fn the_trigram_floor_counts_characters_not_bytes() {
        assert!(req(&["café"]).unwrap().fts_expr().is_some());
        assert!(req(&["日本"]).unwrap().fts_expr().is_none());
        assert!(req(&["ab"]).unwrap().fts_expr().is_none());
        assert!(
            req(&["abc", "de"]).unwrap().fts_expr().is_none(),
            "all of them"
        );
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
