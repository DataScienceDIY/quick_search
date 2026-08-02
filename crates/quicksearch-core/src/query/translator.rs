//! AST → executable SQL.

use super::ast::{Op, Term};
use super::parser::{parse, ParseError};
use crate::mime::FileType;

/// A prepared SQL statement plus its positional parameters. Parameters are
/// rusqlite `Value` for convenience at the call site. Every `?N` in `sql`
/// corresponds to `params[N-1]`.
#[derive(Debug, Clone)]
pub struct SqlQuery {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// Sort strategy for the result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Newest first by file modification time.
    ByMtimeDesc,
    /// FTS5 relevance rank. Only sensible when the query has FTS terms.
    ByRank,
    /// No ORDER BY clause.
    None,
}

impl Default for Sort {
    fn default() -> Self {
        Sort::ByMtimeDesc
    }
}

#[derive(Debug, Clone)]
pub enum TranslateError {
    Parse(ParseError),
    UnknownProperty(String),
    BadDate(String),
    BadRegex(String),
    UnsupportedOp {
        key: String,
        op: Op,
    },
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::Parse(e) => write!(f, "{}", e),
            TranslateError::UnknownProperty(k) => write!(f, "unknown property '{}'", k),
            TranslateError::BadDate(s) => write!(f, "bad date '{}'", s),
            TranslateError::BadRegex(s) => write!(f, "regex error: {}", s),
            TranslateError::UnsupportedOp { key, op } => {
                write!(f, "operator {:?} is not supported for property '{}'", op, key)
            }
        }
    }
}

impl std::error::Error for TranslateError {}

/// One-shot: parse the input string and build the SQL. Limit/offset are
/// applied at the SQL level; `None` means "no limit".
pub fn parse_and_build(
    input: &str,
    limit: Option<u32>,
    offset: u32,
    sort: Sort,
) -> Result<SqlQuery, TranslateError> {
    let ast = parse(input).map_err(TranslateError::Parse)?;
    build(&ast, limit, offset, sort)
}

/// Translate an already-parsed AST into SQL.
pub fn build(
    ast: &Term,
    limit: Option<u32>,
    offset: u32,
    sort: Sort,
) -> Result<SqlQuery, TranslateError> {
    let mut b = Builder::default();
    let where_sql = b.translate(ast)?;

    // The FTS branch joins a CTE of matching rowids; the structured branch
    // stands alone. A query may have both — in that case we intersect at the
    // file.id level.
    let needs_fts_join = !b.fts_parts.is_empty();

    // Structured placeholders were numbered assuming no FTS param. When an
    // FTS MATCH is prepended as ?1, bump every `?N` in the generated WHERE
    // clause by 1 so the positional bindings line up.
    let where_sql = if needs_fts_join {
        shift_placeholders(&where_sql)
    } else {
        where_sql
    };

    let mut sql = String::new();
    if needs_fts_join {
        sql.push_str(
            "WITH fts_hits AS (SELECT rowid FROM searchabletext WHERE searchabletext MATCH ?1) ",
        );
        b.all_params
            .insert(0, rusqlite::types::Value::Text(fts_expr(&b.fts_parts)));
    }
    sql.push_str("SELECT f.id, f.name, f.path FROM files f ");
    if needs_fts_join {
        sql.push_str("JOIN fts_hits ON fts_hits.rowid = f.id ");
    }
    if !where_sql.is_empty() {
        sql.push_str("WHERE ");
        sql.push_str(&where_sql);
        sql.push(' ');
    }
    match sort {
        Sort::ByMtimeDesc => sql.push_str("ORDER BY f.mtime DESC "),
        Sort::ByRank if needs_fts_join => sql.push_str("ORDER BY rank "),
        Sort::ByRank | Sort::None => {}
    }
    if let Some(n) = limit {
        sql.push_str(&format!("LIMIT {} ", n));
    }
    if offset > 0 {
        sql.push_str(&format!("OFFSET {}", offset));
    }

    Ok(SqlQuery {
        sql: sql.trim_end().to_string(),
        params: b.all_params,
    })
}

#[derive(Default)]
struct Builder {
    /// Tokens that will be joined into the single FTS MATCH expression.
    fts_parts: Vec<FtsFragment>,
    /// Everything else (structured filters) as a WHERE clause.
    all_params: Vec<rusqlite::types::Value>,
}

#[derive(Debug, Clone)]
enum FtsFragment {
    /// A plain word or phrase to be included as an AND'd token.
    And(String),
    /// Grouped alternation, already rendered with internal ORs.
    OrGroup(Vec<String>),
}

fn fts_expr(parts: &[FtsFragment]) -> String {
    // Render `(a AND b AND (c OR d))` where a/b/c/d are quoted phrases.
    let mut out = String::new();
    let mut first = true;
    for p in parts {
        if !first {
            out.push_str(" AND ");
        }
        first = false;
        match p {
            FtsFragment::And(s) => out.push_str(&quote_phrase(s)),
            FtsFragment::OrGroup(items) => {
                out.push('(');
                let mut first_item = true;
                for item in items {
                    if !first_item {
                        out.push_str(" OR ");
                    }
                    first_item = false;
                    out.push_str(&quote_phrase(item));
                }
                out.push(')');
            }
        }
    }
    out
}

/// Escape a phrase for FTS5 MATCH. FTS5 itself uses doubled quotes for
/// literal quotes inside a quoted phrase; wrapping in quotes renders all
/// other MATCH metacharacters (`( ) * :` etc.) inert. Injection-safe by
/// construction.
pub fn quote_phrase(s: &str) -> String {
    let mut buf = String::with_capacity(s.len() + 2);
    buf.push('"');
    for c in s.chars() {
        if c == '"' {
            buf.push('"');
        }
        buf.push(c);
    }
    buf.push('"');
    buf
}

impl Builder {
    /// Translate a sub-term, returning its contribution to the SQL WHERE
    /// clause. FTS parts are accumulated in `self.fts_parts` (not returned
    /// here) since they collapse into a single MATCH expression at the top.
    fn translate(&mut self, t: &Term) -> Result<String, TranslateError> {
        match t {
            Term::Literal(s) => {
                self.fts_parts.push(FtsFragment::And(s.clone()));
                Ok(String::new())
            }
            Term::Property { key, op, value } => self.translate_property(key, *op, value),
            Term::And(children) => {
                let mut pieces = Vec::new();
                for c in children {
                    let p = self.translate(c)?;
                    if !p.is_empty() {
                        pieces.push(p);
                    }
                }
                Ok(join_with("AND", &pieces))
            }
            Term::Or(children) => {
                // An OR of pure-literal children collapses into one FTS OR-group.
                if children.iter().all(|c| matches!(c, Term::Literal(_))) {
                    let group: Vec<String> = children
                        .iter()
                        .map(|c| match c {
                            Term::Literal(s) => s.clone(),
                            _ => unreachable!(),
                        })
                        .collect();
                    self.fts_parts.push(FtsFragment::OrGroup(group));
                    return Ok(String::new());
                }
                // Mixed OR — each branch becomes a separate sub-Builder whose
                // WHERE fragments we OR together. FTS branches cannot mix
                // with structured branches cleanly at the SQL level here;
                // keep it simple by requiring that mixed-OR branches produce
                // structured-only WHERE fragments.
                let mut pieces = Vec::new();
                for c in children {
                    let before = self.fts_parts.len();
                    let p = self.translate(c)?;
                    if self.fts_parts.len() > before {
                        return Err(TranslateError::UnknownProperty(
                            "OR mixing FTS and structured terms is not supported in Set A".into(),
                        ));
                    }
                    if !p.is_empty() {
                        pieces.push(p);
                    }
                }
                Ok(format!("({})", join_with("OR", &pieces)))
            }
        }
    }

    /// Delegate to the shared [`build_filter`] fragment builder, then
    /// convert its anonymous `?` placeholders to this builder's numbered
    /// scheme (params[0] is reserved for the FTS MATCH when one exists;
    /// `build` shifts numbers afterwards).
    fn translate_property(
        &mut self,
        key: &str,
        op: Op,
        value: &str,
    ) -> Result<String, TranslateError> {
        let frag = build_filter(key, op, value, false)?;
        let mut params = frag.params.into_iter();
        let mut out = String::with_capacity(frag.sql.len() + 8);
        for c in frag.sql.chars() {
            if c == '?' {
                let v = params
                    .next()
                    .expect("FilterFragment placeholder/param counts match");
                self.all_params.push(v);
                out.push('?');
                out.push_str(&self.all_params.len().to_string());
            } else {
                out.push(c);
            }
        }
        Ok(out)
    }
}

/// A structured-filter fragment over table alias `f`: SQL with anonymous
/// `?` placeholders plus the values they bind. Anonymous placeholders
/// compose by simple appending — the search cascade tacks fragments onto
/// every stage's WHERE clause with `AND (...)`.
#[derive(Debug, Clone)]
pub struct FilterFragment {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// Whether `key` is a recognized structured-filter property.
pub fn is_filter_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "type" | "modified" | "mtime" | "path" | "folder" | "includefolder" | "name"
            | "filename" | "mime"
    )
}

/// Translate one `key op value` filter into a [`FilterFragment`]. The
/// single source of filter semantics, shared by the legacy numbered
/// [`build`] path and the cascade's [`super::split`].
///
/// `glob` marks a value whose unquoted `*` should act as a wildcard — only
/// `name:`/`filename:` honor it; every other key treats the star literally.
/// The caller decides, because only the tokenizer knows whether the value
/// was quoted (quoted stars are always literal).
pub fn build_filter(
    key: &str,
    op: Op,
    value: &str,
    glob: bool,
) -> Result<FilterFragment, TranslateError> {
    use rusqlite::types::Value;
    let frag = |sql: &str, params: Vec<Value>| FilterFragment {
        sql: sql.to_string(),
        params,
    };
    let eq_like_only = |op: Op| -> Result<(), TranslateError> {
        if op != Op::Contains && op != Op::Eq {
            return Err(TranslateError::UnsupportedOp {
                key: key.into(),
                op,
            });
        }
        Ok(())
    };

    match key.to_ascii_lowercase().as_str() {
        "type" => {
            eq_like_only(op)?;
            let bits = FileType::from_name(value).bits() as i64;
            if bits == 0 {
                return Err(TranslateError::UnknownProperty(format!(
                    "type name '{}'",
                    value
                )));
            }
            Ok(frag("(f.type & ?) != 0", vec![Value::Integer(bits)]))
        }
        "modified" | "mtime" => {
            let unix =
                parse_date_to_unix(value).ok_or_else(|| TranslateError::BadDate(value.into()))?;
            // `modified:=2024-01-01` matches the whole day, not the second.
            if op == Op::Eq || op == Op::Contains {
                return Ok(frag(
                    "(f.mtime >= ? AND f.mtime < ?)",
                    vec![Value::Integer(unix), Value::Integer(unix + 86_400)],
                ));
            }
            let sql_op = match op {
                Op::Lt => "<",
                Op::Le => "<=",
                Op::Gt => ">",
                Op::Ge => ">=",
                Op::Contains | Op::Eq => unreachable!(),
            };
            Ok(frag(
                &format!("f.mtime {} ?", sql_op),
                vec![Value::Integer(unix)],
            ))
        }
        "path" | "folder" | "includefolder" => {
            eq_like_only(op)?;
            let base = normalize_folder_value(value);
            if base.is_empty() {
                // "everything". On Unix the old `parent = '/' OR parent LIKE
                // '/%'` happened to match every absolute path; Windows has no
                // single root, so say it directly rather than by accident.
                return Ok(frag("1=1", Vec::new()));
            }
            // The `=` half needs the collation spelled out: `LIKE` folds ASCII
            // case on its own, so without this the two halves of the same
            // filter disagree about `C:\Users` versus `c:\users`.
            Ok(frag(
                &format!(
                    "(f.parent = ? COLLATE {} OR f.parent LIKE ? ESCAPE '\\')",
                    crate::platform::PATH_COLLATION
                ),
                vec![
                    Value::Text(base.clone()),
                    Value::Text(like_subtree_pattern(&base)),
                ],
            ))
        }
        "name" | "filename" => {
            if op != Op::Contains {
                return Err(TranslateError::UnsupportedOp {
                    key: key.into(),
                    op,
                });
            }
            // With `glob`, each `*` becomes an unescaped `%`; the pieces
            // around it still get `%`/`_`/`\` escaped so user metacharacters
            // stay literal either way.
            let pattern = if glob && value.contains('*') {
                value
                    .split('*')
                    .map(escape_like)
                    .collect::<Vec<_>>()
                    .join("%")
            } else {
                escape_like(value)
            };
            Ok(frag(
                "f.name LIKE ? ESCAPE '\\'",
                vec![Value::Text(format!("%{}%", pattern))],
            ))
        }
        "mime" => {
            eq_like_only(op)?;
            Ok(frag("f.mime = ?", vec![Value::Text(value.into())]))
        }
        _ => Err(TranslateError::UnknownProperty(key.to_string())),
    }
}

/// Escape `%`, `_` and `\` for use inside a `LIKE ... ESCAPE '\'` pattern.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Tidy a user-supplied folder value into the spelling `files.parent` stores.
///
/// Trailing separators are how people naturally write directories, and either
/// separator may show up on Windows. A bare drive (`C:`) is *not* a path — the
/// stored parent is `C:\` — so the separator goes back on.
fn normalize_folder_value(value: &str) -> String {
    let base = value.trim().trim_end_matches(['/', '\\']);
    if base.len() == 2 && base.ends_with(':') && base.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return format!("{}{}", base, std::path::MAIN_SEPARATOR);
    }
    base.to_string()
}

/// A `LIKE ... ESCAPE '\'` pattern matching every path strictly beneath `dir`.
///
/// The separator is escaped along with the base, because on Windows the
/// separator *is* the escape character — a hand-written `format!("{}/%", dir)`
/// is wrong twice over there: wrong separator, and the one it emits would be
/// swallowed as an escape.
///
/// SQLite's `patternCompare` takes the character after the escape literally
/// whatever it is, so a doubled `\` is well defined here; the folklore that an
/// escape must be followed by `%`, `_` or itself does not apply.
pub fn like_subtree_pattern(dir: &str) -> String {
    format!(
        "{}{}%",
        escape_like(dir.trim_end_matches(['/', '\\'])),
        escape_like(std::path::MAIN_SEPARATOR_STR)
    )
}

fn join_with(sep: &str, pieces: &[String]) -> String {
    pieces
        .iter()
        .map(|p| p.clone())
        .collect::<Vec<_>>()
        .join(&format!(" {} ", sep))
}

/// Parse a date string. Accepts `YYYY-MM-DD`. Returns unix seconds at 00:00 UTC.
fn parse_date_to_unix(s: &str) -> Option<i64> {
    // Minimal parser: split on '-' into y/m/d integers.
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    // Compute unix seconds using the days-since-epoch formula.
    Some(civil_to_unix(y, m as i64, d as i64))
}

/// Howard Hinnant's civil-from-days algorithm, converting (year, month, day)
/// in the Gregorian calendar to days since 1970-01-01.
fn civil_to_unix(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400
}

/// Rewrite placeholder indices after an FTS MATCH parameter was inserted at
/// position 0. Called automatically in [`build`] when needed — but since we
/// append placeholders numerically during translation and prepend the FTS
/// param afterwards, every explicit `?N` in the generated SQL is off by one
/// when FTS is present.
fn shift_placeholders(sql: &str) -> String {
    // Find every `?N` and bump N by 1. Only ASCII digits after `?`; skip
    // anonymous `?` (which rusqlite won't mix with numbered anyway).
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'?' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            out.push('?');
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let n: u64 = std::str::from_utf8(&bytes[i + 1..j])
                .unwrap()
                .parse()
                .unwrap();
            out.push_str(&(n + 1).to_string());
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    fn build_q(input: &str) -> SqlQuery {
        parse_and_build(input, None, 0, Sort::None).expect("build")
    }

    #[test]
    fn single_literal() {
        let q = build_q("foo");
        assert!(q.sql.contains("searchabletext MATCH ?"));
        assert_eq!(
            q.params,
            vec![rusqlite::types::Value::Text("\"foo\"".into())]
        );
    }

    #[test]
    fn implicit_and_joins_as_fts_and() {
        let q = build_q("foo bar");
        assert!(q.sql.contains("searchabletext MATCH ?"));
        let m: &rusqlite::types::Value = &q.params[0];
        if let rusqlite::types::Value::Text(s) = m {
            assert_eq!(s, "\"foo\" AND \"bar\"");
        } else {
            panic!();
        }
    }

    #[test]
    fn or_of_literals_becomes_fts_group() {
        let q = build_q("a OR b");
        if let rusqlite::types::Value::Text(s) = &q.params[0] {
            assert_eq!(s, "(\"a\" OR \"b\")");
        } else {
            panic!();
        }
    }

    #[test]
    fn type_filter_only() {
        let q = build_q("type:Audio");
        assert!(q.sql.contains("(f.type & ?"));
        assert_eq!(
            q.params,
            vec![rusqlite::types::Value::Integer(FileType::AUDIO.bits() as i64)]
        );
    }

    #[test]
    fn modified_ge() {
        let q = build_q("modified:>=2024-01-01");
        assert!(q.sql.contains("f.mtime >= ?"));
        assert_eq!(q.params.len(), 1);
    }

    #[test]
    fn modified_eq_is_day_range() {
        let q = build_q("modified:=2024-05-20");
        assert!(q.sql.contains("f.mtime >= ?") && q.sql.contains("f.mtime < ?"));
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn path_filter() {
        let q = build_q("path:/home/me/docs");
        assert!(q.sql.contains("f.parent = ?"));
        assert!(q.sql.contains("f.parent LIKE ?"));
        // The LIKE half must be escaped and declare its escape character;
        // without the clause a Windows separator would be eaten as an escape.
        assert!(q.sql.contains("ESCAPE '\\'"), "{}", q.sql);
    }

    /// The subtree pattern is the one place the separator and the LIKE escape
    /// character collide (on Windows they are the same byte), so it is checked
    /// against real SQLite rather than by string comparison.
    #[test]
    fn like_subtree_pattern_matches_only_the_subtree() {
        use std::path::MAIN_SEPARATOR as SEP;

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (parent TEXT NOT NULL)", [])
            .unwrap();

        let base = format!("{}a{}b", root_prefix(), SEP);
        let rows = [
            format!("{}{}sub", base, SEP),          // inside
            format!("{}{}sub{}deep", base, SEP, SEP), // deeper
            base.clone(),                            // the folder itself
            format!("{}a{}bc", root_prefix(), SEP),  // prefix sibling: outside
            format!("{}a", root_prefix()),           // parent: outside
        ];
        for r in &rows {
            conn.execute("INSERT INTO files (parent) VALUES (?1)", [r])
                .unwrap();
        }

        let matched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE parent LIKE ?1 ESCAPE '\\'",
                [like_subtree_pattern(&base)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(matched, 2, "only the two rows strictly beneath {}", base);
    }

    #[test]
    fn like_subtree_pattern_escapes_metacharacters() {
        use std::path::MAIN_SEPARATOR as SEP;

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (parent TEXT NOT NULL)", [])
            .unwrap();

        let base = format!("{}a_b", root_prefix());
        for r in [
            format!("{}{}inside", base, SEP),         // real child
            format!("{}axb{}bait", root_prefix(), SEP), // `_` must not glob to `x`
            format!("{}100%_done{}x", root_prefix(), SEP),
        ] {
            conn.execute("INSERT INTO files (parent) VALUES (?1)", [&r])
                .unwrap();
        }

        let matched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE parent LIKE ?1 ESCAPE '\\'",
                [like_subtree_pattern(&base)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(matched, 1, "`_` and `%` are literals, not wildcards");
    }

    #[test]
    fn folder_value_normalization() {
        use std::path::MAIN_SEPARATOR as SEP;
        // Trailing separators of either flavour are stripped.
        assert_eq!(normalize_folder_value("/home/me/"), "/home/me");
        assert_eq!(normalize_folder_value(r"C:\Users\me\"), r"C:\Users\me");
        // A bare drive is not a path; the stored parent is `C:\`.
        assert_eq!(normalize_folder_value("C:"), format!("C:{}", SEP));
        assert_eq!(normalize_folder_value(r"C:\"), format!("C:{}", SEP));
        // Empty means "everywhere".
        assert_eq!(normalize_folder_value("/"), "");
        assert_eq!(normalize_folder_value("  "), "");
    }

    #[test]
    fn empty_folder_value_matches_everything() {
        let frag = build_filter("path", Op::Contains, "/", false).unwrap();
        assert_eq!(frag.sql, "1=1");
        assert!(frag.params.is_empty(), "no placeholders to renumber");
    }

    /// An absolute-path prefix for the running platform.
    fn root_prefix() -> String {
        if cfg!(windows) {
            r"C:\".to_string()
        } else {
            "/".to_string()
        }
    }

    #[test]
    fn combined_fts_and_type() {
        let q = build_q("type:Audio beatles");
        assert!(q.sql.contains("searchabletext MATCH ?"));
        assert!(q.sql.contains("(f.type & ?"));
        // FTS param first, then type bits.
        assert_eq!(q.params.len(), 2);
        assert_eq!(
            q.params[0],
            rusqlite::types::Value::Text("\"beatles\"".into())
        );
    }

    #[test]
    fn unknown_property_errors() {
        let err = parse_and_build("artist:beatles", None, 0, Sort::None).unwrap_err();
        assert!(matches!(err, TranslateError::UnknownProperty(_)));
    }

    #[test]
    fn bad_date_errors() {
        let err = parse_and_build("modified:>=not-a-date", None, 0, Sort::None).unwrap_err();
        assert!(matches!(err, TranslateError::BadDate(_)));
    }

    #[test]
    fn civil_conversion() {
        // 2024-01-01 → 1704067200 unix
        assert_eq!(civil_to_unix(2024, 1, 1), 1_704_067_200);
        // 1970-01-01 → 0
        assert_eq!(civil_to_unix(1970, 1, 1), 0);
        // 2000-02-29 is valid (leap year)
        assert_eq!(civil_to_unix(2000, 2, 29), 951_782_400);
    }

    #[test]
    fn limit_offset_sort_render() {
        let q = parse_and_build("foo", Some(10), 20, Sort::ByMtimeDesc).unwrap();
        assert!(q.sql.contains("ORDER BY f.mtime DESC"));
        assert!(q.sql.contains("LIMIT 10"));
        assert!(q.sql.contains("OFFSET 20"));
    }

    // End-to-end: run a generated query against a real SQLite DB to verify
    // placeholder shifting after FTS prepending is correct.
    #[test]
    fn end_to_end_combined_filter_executes() {
        use crate::db::{
            open_or_recreate,
            repo::{insert_file, set_content_done, NewFile},
        };
        use crate::mime::FileType;
        use rusqlite::types::ToSql;

        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-query-e2e-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();

        // Two audio files, one document. Only the "audio with beatles" file
        // should be returned for `type:Audio beatles`.
        let ids: Vec<i64> = {
            let tx = conn.transaction().unwrap();
            let a = insert_file(
                &tx,
                &NewFile {
                    name: "beatles-track.mp3",
                    path: "/music/beatles-track.mp3",
                    parent: "/music",
                    size: 100,
                    mtime: 1_700_000_000,
                    inode: None,
                    device_id: None,
                    mime: Some("audio/mpeg"),
                    ftype: FileType::AUDIO,
                    hash: None,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(
                &tx,
                a,
                "beatles-track.mp3",
                "beatles hey jude",
                &[("artist".into(), "The Beatles".into())],
                true,
            )
            .unwrap();
            let b = insert_file(
                &tx,
                &NewFile {
                    name: "bach.flac",
                    path: "/music/bach.flac",
                    parent: "/music",
                    size: 100,
                    mtime: 1_700_000_000,
                    inode: None,
                    device_id: None,
                    mime: Some("audio/flac"),
                    ftype: FileType::AUDIO,
                    hash: None,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(&tx, b, "bach.flac", "bach prelude", &[], true).unwrap();
            let c = insert_file(
                &tx,
                &NewFile {
                    name: "notes.txt",
                    path: "/docs/notes.txt",
                    parent: "/docs",
                    size: 50,
                    mtime: 1_700_000_000,
                    inode: None,
                    device_id: None,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(&tx, c, "notes.txt", "beatles biography", &[], true).unwrap();
            tx.commit().unwrap();
            vec![a, b, c]
        };

        let q = parse_and_build("type:Audio beatles", None, 0, Sort::ByMtimeDesc).unwrap();
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare(&q.sql).expect("prepare");
            let bind: Vec<&dyn ToSql> = q.params.iter().map(|v| v as &dyn ToSql).collect();
            stmt.query_map(bind.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
        };

        assert_eq!(rows.len(), 1, "expected only beatles-track.mp3; got {:?}", rows);
        assert_eq!(rows[0].0, ids[0]);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
