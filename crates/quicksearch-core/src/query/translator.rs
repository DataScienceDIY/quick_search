//! `key op value` filters → composable SQL fragments.
//!
//! One filter becomes one [`FilterFragment`]: a WHERE fragment over table alias
//! `f` with anonymous `?` placeholders, plus the values to bind. The search
//! cascade ANDs those fragments onto every stage's query, so they have to
//! compose by plain appending — which anonymous placeholders do and numbered
//! ones would not.

use super::ast::Op;
use crate::mime::FileType;

#[derive(Debug, Clone)]
pub enum TranslateError {
    UnknownProperty(String),
    BadDate(String),
    BadRegex(String),
    UnsupportedOp { key: String, op: Op },
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::UnknownProperty(k) => write!(f, "unknown property '{}'", k),
            TranslateError::BadDate(s) => write!(f, "bad date '{}'", s),
            TranslateError::BadRegex(s) => write!(f, "regex error: {}", s),
            TranslateError::UnsupportedOp { key, op } => {
                write!(
                    f,
                    "operator {:?} is not supported for property '{}'",
                    op, key
                )
            }
        }
    }
}

impl std::error::Error for TranslateError {}

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

/// A structured-filter fragment over table alias `f`: SQL with anonymous
/// `?` placeholders plus the values they bind.
#[derive(Debug, Clone)]
pub struct FilterFragment {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

/// Whether `key` is a recognized structured-filter property.
pub fn is_filter_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "type"
            | "modified"
            | "mtime"
            | "path"
            | "folder"
            | "includefolder"
            | "name"
            | "filename"
            | "mime"
    )
}

/// Translate one `key op value` filter into a [`FilterFragment`]. The single
/// source of filter semantics, reached through [`super::split`].
///
/// `glob` marks a value whose unquoted `*` should act as a wildcard — only
/// `name:`/`filename:` honor it; every other key treats the star literally.
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
            // One `LIKE`, where this used to need `parent = ? OR parent LIKE ?`
            // with the collation spelled out to stop the two halves disagreeing
            // about `C:\Users` versus `c:\users`. Every stored parent now ends
            // in a separator, so `dir + SEP + %` matches the folder's own files
            // (`%` matching nothing) as well as its subdirectories', and the
            // `=` half has nothing left to do.
            Ok(frag(
                "f.parent LIKE ? ESCAPE '\\'",
                vec![Value::Text(like_subtree_pattern(&base))],
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

/// Tidy a user-supplied folder value: trim it, and drop any trailing
/// separator, of either flavour — that is how people naturally write a
/// directory, and either may show up on Windows.
///
/// Empty out means "every folder", which is what a bare `/` or a blank value
/// comes to; [`build_filter`] turns that into `1=1`.
///
/// It used to special-case a bare drive (`C:` → `C:\`), because the filter's
/// `parent = ?` half had to match the stored spelling exactly. That half is
/// gone, and [`like_subtree_pattern`] puts the separator back itself, so both
/// spellings now produce the same pattern.
fn normalize_folder_value(value: &str) -> String {
    value.trim().trim_end_matches(['/', '\\']).to_string()
}

/// A `LIKE ... ESCAPE '\'` pattern matching `dir`'s own files and everything
/// beneath it.
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

/// Parse a date string. Accepts `YYYY-MM-DD`. Returns unix seconds at 00:00 UTC.
fn parse_date_to_unix(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1970..=9999).contains(&y) {
        return None;
    }
    // Against the real month length, not a flat 1..=31: `civil_to_unix` happily
    // rolls 2024-02-31 over into March, so a typo would silently filter on a
    // date the user never typed rather than being reported.
    if d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(civil_to_unix(y, m as i64, d as i64))
}

/// Days in a Gregorian month, February by the leap rule.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's civil-from-days algorithm, converting (year, month, day)
/// in the Gregorian calendar to days since 1970-01-01.
fn civil_to_unix(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(key: &str, op: Op, value: &str) -> FilterFragment {
        build_filter(key, op, value, false).expect("builds")
    }

    #[test]
    fn type_filter_masks_the_bits() {
        let f = frag("type", Op::Contains, "Audio");
        assert_eq!(f.sql, "(f.type & ?) != 0");
        assert_eq!(
            f.params,
            vec![rusqlite::types::Value::Integer(
                FileType::AUDIO.bits() as i64
            )]
        );
    }

    #[test]
    fn modified_comparisons_and_day_ranges() {
        let f = frag("modified", Op::Ge, "2024-01-01");
        assert!(f.sql.contains("f.mtime >= ?"));
        assert_eq!(f.params.len(), 1);

        // `=` means the whole day, not the second.
        let f = frag("modified", Op::Eq, "2024-05-20");
        assert!(f.sql.contains("f.mtime >= ?") && f.sql.contains("f.mtime < ?"));
        assert_eq!(f.params.len(), 2);
    }

    #[test]
    fn path_filter_covers_the_folder_and_its_subtree() {
        let f = frag("path", Op::Contains, "/home/me/docs");
        // One `LIKE`, and one bound value: since every stored parent ends in a
        // separator, `dir + SEP + %` reaches the folder's own files as well as
        // its subdirectories'. The `parent = ?` half this used to need — and
        // the explicit collation that went with it — is gone.
        assert!(f.sql.contains("f.parent LIKE ?"), "{}", f.sql);
        assert!(!f.sql.contains("f.parent = ?"), "{}", f.sql);
        assert_eq!(f.params.len(), 1);
        // The LIKE must declare its escape character; without the clause a
        // Windows separator would be eaten as an escape.
        assert!(f.sql.contains("ESCAPE '\\'"), "{}", f.sql);
    }

    #[test]
    fn unsupported_shapes_error() {
        assert!(matches!(
            build_filter("artist", Op::Contains, "beatles", false),
            Err(TranslateError::UnknownProperty(_))
        ));
        assert!(matches!(
            build_filter("modified", Op::Ge, "not-a-date", false),
            Err(TranslateError::BadDate(_))
        ));
        assert!(matches!(
            build_filter("type", Op::Contains, "NotAThing", false),
            Err(TranslateError::UnknownProperty(_))
        ));
        // Only `name:` accepts a bare `contains`; an ordering operator on it
        // is meaningless.
        assert!(matches!(
            build_filter("name", Op::Ge, "report", false),
            Err(TranslateError::UnsupportedOp { .. })
        ));
    }

    /// `civil_to_unix` rolls an impossible day over into the next month, so a
    /// typo would otherwise filter on a date the user never typed — silently,
    /// since the query still runs.
    #[test]
    fn impossible_dates_are_rejected_rather_than_rolled_over() {
        for bad in [
            "2024-02-30",
            "2023-02-29", // not a leap year
            "2024-04-31",
            "2024-01-00",
            "2024-13-01",
            "1969-12-31", // before the epoch
        ] {
            assert!(
                parse_date_to_unix(bad).is_none(),
                "{} should be rejected",
                bad
            );
        }
        for good in ["2024-02-29", "2000-02-29", "2024-01-31", "2024-04-30"] {
            assert!(parse_date_to_unix(good).is_some(), "{} should parse", good);
        }
        // Centurial leap rule: 1900 is not a leap year, 2000 is.
        assert!(parse_date_to_unix("1900-02-29").is_none());
    }

    #[test]
    fn is_filter_key_covers_exactly_the_supported_keys() {
        for key in [
            "type",
            "modified",
            "mtime",
            "path",
            "folder",
            "includefolder",
            "name",
            "filename",
            "mime",
            "TYPE",
            "Path",
        ] {
            assert!(is_filter_key(key), "{} should be a filter key", key);
        }
        for key in ["artist", "regex", "size", ""] {
            assert!(!is_filter_key(key), "{} should not be a filter key", key);
        }
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
        // Stored parents always end in a separator, so each row here is spelled
        // the way the indexer would spell it.
        let rows = [
            format!("{}{}sub{}", base, SEP, SEP),            // inside
            format!("{}{}sub{}deep{}", base, SEP, SEP, SEP), // deeper
            format!("{}{}", base, SEP),                      // the folder's own files
            format!("{}a{}bc{}", root_prefix(), SEP, SEP),   // prefix sibling: outside
            format!("{}a{}", root_prefix(), SEP),            // parent: outside
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
        // Three, not two: the trailing separator is what brings the folder's
        // *own* files in, which is why the filter needs no second predicate.
        // The prefix sibling and the parent stay out.
        assert_eq!(matched, 3, "the subtree of {}, and only that", base);
    }

    #[test]
    fn like_subtree_pattern_escapes_metacharacters() {
        use std::path::MAIN_SEPARATOR as SEP;

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (parent TEXT NOT NULL)", [])
            .unwrap();

        let base = format!("{}a_b", root_prefix());
        for r in [
            format!("{}{}inside{}", base, SEP, SEP), // real child
            format!("{}axb{}bait{}", root_prefix(), SEP, SEP), // `_` must not glob to `x`
            format!("{}100%_done{}x{}", root_prefix(), SEP, SEP),
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
        // Trailing separators of either flavour are stripped.
        assert_eq!(normalize_folder_value("/home/me/"), "/home/me");
        assert_eq!(normalize_folder_value(r"C:\Users\me\"), r"C:\Users\me");
        // A bare drive and a rooted one now normalize alike: the pattern
        // builder puts the separator back either way.
        assert_eq!(normalize_folder_value("C:"), "C:");
        assert_eq!(normalize_folder_value(r"C:\"), "C:");
        assert_eq!(
            like_subtree_pattern(&normalize_folder_value("C:")),
            like_subtree_pattern(&normalize_folder_value(r"C:\")),
        );
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
    fn civil_conversion() {
        // 2024-01-01 → 1704067200 unix
        assert_eq!(civil_to_unix(2024, 1, 1), 1_704_067_200);
        // 1970-01-01 → 0
        assert_eq!(civil_to_unix(1970, 1, 1), 0);
        // 2000-02-29 is valid (leap year)
        assert_eq!(civil_to_unix(2000, 2, 29), 951_782_400);
    }
}
