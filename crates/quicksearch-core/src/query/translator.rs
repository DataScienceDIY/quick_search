//! `key op value` filters → composable SQL fragments.
//!
//! One filter becomes one [`FilterFragment`] over table alias `f` with
//! anonymous `?` placeholders: the cascade ANDs fragments onto every stage,
//! so they must compose by plain appending, which numbered ones would not.

use chrono::{
    DateTime, Local, MappedLocalTime, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone,
};

use super::Op;
use crate::file_handling::trim_trailing_separators;
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

/// Escape a phrase for FTS5 MATCH: quotes doubled, wrapping quotes render
/// all other MATCH metacharacters inert. Injection-safe by construction.
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

/// SQL with anonymous `?` placeholders over alias `f`, plus bound values.
#[derive(Debug, Clone)]
pub struct FilterFragment {
    pub sql: String,
    pub params: Vec<rusqlite::types::Value>,
}

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

/// `regex:` is matched in Rust, not translated, and its value lexes raw.
pub fn is_regex_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("regex")
}

/// Translate one `key op value` filter; the single source of filter
/// semantics. `glob` marks a value whose unquoted `*` acts as a wildcard;
/// only `name:`/`filename:` honor it.
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
    let allow = |ok: &[Op]| -> Result<(), TranslateError> {
        if ok.contains(&op) {
            Ok(())
        } else {
            Err(TranslateError::UnsupportedOp {
                key: key.into(),
                op,
            })
        }
    };

    match key.to_ascii_lowercase().as_str() {
        "type" => {
            allow(&[Op::Contains, Op::Eq])?;
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
            let date = date_value(value)?;
            Ok(day_filter(op, day_bounds(date, local_instant)))
        }
        "path" | "folder" | "includefolder" => {
            allow(&[Op::Contains, Op::Eq])?;
            let base = normalize_folder_value(value);
            if base.is_empty() {
                // "everything": Windows has no single root, so say it directly.
                return Ok(frag("1=1", Vec::new()));
            }
            // Every stored parent ends in a separator, so `dir + SEP + %`
            // matches the folder's own files (`%` matching nothing) as well
            // as its subdirectories', with no `parent = ?` half.
            Ok(frag(
                "f.parent LIKE ? ESCAPE '\\'",
                vec![Value::Text(like_subtree_pattern(&base))],
            ))
        }
        "name" | "filename" => {
            allow(&[Op::Contains])?;
            // Each `*` becomes an unescaped `%`; the pieces around it still
            // get escaped so user metacharacters stay literal.
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
            allow(&[Op::Contains, Op::Eq])?;
            Ok(frag("f.mime = ?", vec![Value::Text(value.into())]))
        }
        _ => Err(TranslateError::UnknownProperty(key.to_string())),
    }
}

/// [`build_filter`]'s verdict without its SQL, for a caller asking on every
/// keystroke (the GUI's highlighter, on the UI thread). A date is parsed
/// and never placed in the local zone, which is a syscall at times (see
/// [`local_instant`]).
pub fn check_filter(key: &str, op: Op, value: &str, glob: bool) -> Result<(), TranslateError> {
    match key.to_ascii_lowercase().as_str() {
        "modified" | "mtime" => date_value(value).map(drop),
        _ => build_filter(key, op, value, glob).map(drop),
    }
}

/// The date a `modified:` value names, or why it names none.
fn date_value(value: &str) -> Result<NaiveDate, TranslateError> {
    parse_date(value).ok_or_else(|| TranslateError::BadDate(value.into()))
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

/// Trim whitespace and trailing separators (by the platform's rules, see
/// [`trim_trailing_separators`]). On Windows the separators are also unified
/// to the `\` every stored parent is spelled with, so `path:D:/data` and a
/// UNC `//srv/share` find their files. Empty out means "every folder";
/// [`build_filter`] turns that into `1=1`.
fn normalize_folder_value(value: &str) -> String {
    let base = trim_trailing_separators(value.trim());
    if cfg!(windows) {
        base.replace('/', "\\")
    } else {
        base.to_string()
    }
}

/// A `LIKE ... ESCAPE '\'` pattern matching `dir`'s own files and everything
/// beneath it. The separator is escaped along with the base, because on
/// Windows the separator is the escape character. SQLite takes the character
/// after an escape literally whatever it is, so a doubled `\` is well
/// defined.
pub fn like_subtree_pattern(dir: &str) -> String {
    format!(
        "{}{}%",
        escape_like(trim_trailing_separators(dir)),
        escape_like(std::path::MAIN_SEPARATOR_STR)
    )
}

/// A local calendar date typed `yyyy-mm-dd`, leading zeros optional
/// (`2024-5-2`). ASCII digits only, so `+2024` and fullwidth digits fail, and
/// a day the month lacks fails rather than rolling over: a typo must not
/// filter on a date the user never typed. Years run 1970..=9998, so the
/// day's end, the next midnight, still has a four-digit year.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let &[y, m, d] = &s.split('-').collect::<Vec<_>>()[..] else {
        return None;
    };
    let number = |t: &str| -> Option<u32> {
        let digits = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| t.parse().ok())?
    };
    let year = number(y).filter(|y| (1970..=9998).contains(y))?;
    NaiveDate::from_ymd_opt(year as i32, number(m)?, number(d)?)
}

/// The process's time zone (the user's, for the CLI and GUI), as
/// [`day_bounds`] takes it. Where clocks go back from 24:00 to 23:00, chrono
/// also offers the old offset's reading of midnight, an instant the clocks
/// showed as 23:00, so an instant counts only if the zone reads it back as
/// `local`.
///
/// On Unix chrono keeps the zone per thread: the first call on a thread
/// parses the tzfile, and a call more than a second after the last check
/// reads `TZ` (under the environment lock) and `lstat`s `/etc/localtime`.
/// That is a syscall per `modified:` query at most, paid on the search
/// worker; [`check_filter`] keeps it off the UI thread.
fn local_instant(local: NaiveDateTime) -> MappedLocalTime<i64> {
    let shows =
        |t: &DateTime<Local>| Local.from_utc_datetime(&t.naive_utc()).naive_local() == local;
    match Local.from_local_datetime(&local) {
        MappedLocalTime::Ambiguous(a, b) if !shows(&a) => MappedLocalTime::Single(b.timestamp()),
        MappedLocalTime::Ambiguous(a, b) if !shows(&b) => MappedLocalTime::Single(a.timestamp()),
        other => other.map(|t| t.timestamp()),
    }
}

/// Unix seconds `[start, end)` of local day `date`, computed once per query,
/// never per row. `to_utc` gives the instants at which the zone's clocks
/// show a wall-clock time. A day starts at its first instant: 00:00, or
/// inside a DST gap the first whole hour that exists, and in a fold the
/// earlier of the two.
fn day_bounds(
    date: NaiveDate,
    to_utc: impl Fn(NaiveDateTime) -> MappedLocalTime<i64>,
) -> (i64, i64) {
    // Not `earliest()`: chrono orders a fold's pair by offset on Unix, which
    // puts the later instant first.
    let first = |t: MappedLocalTime<i64>| match t {
        MappedLocalTime::Single(t) => Some(t),
        MappedLocalTime::Ambiguous(a, b) => Some(a.min(b)),
        MappedLocalTime::None => None,
    };
    // No gap spans more than a day (Samoa skipped all of 2011-12-30), so 48
    // hours always reach a time that exists. A zone that places nothing at
    // all (chrono reports an OS error that way too) is read as UTC.
    let first_instant = |midnight: NaiveDateTime| {
        (0..48)
            .find_map(|h| first(to_utc(midnight + TimeDelta::hours(h))))
            .unwrap_or_else(|| midnight.and_utc().timestamp())
    };
    let midnight = date.and_time(NaiveTime::MIN);
    (
        first_instant(midnight),
        first_instant(midnight + TimeDelta::days(1)),
    )
}

/// `modified op D` over D's instants `[start, end)`. Every operator compares
/// whole days, so `>D` starts the day after D and `<=D` takes all of D. The
/// bounds are plain integers: SQLite does no date conversion.
fn day_filter(op: Op, (start, end): (i64, i64)) -> FilterFragment {
    let (sql, bounds) = match op {
        Op::Eq | Op::Contains => ("(f.mtime >= ? AND f.mtime < ?)", vec![start, end]),
        Op::Lt => ("f.mtime < ?", vec![start]),
        Op::Le => ("f.mtime < ?", vec![end]),
        Op::Gt => ("f.mtime >= ?", vec![end]),
        Op::Ge => ("f.mtime >= ?", vec![start]),
    };
    FilterFragment {
        sql: sql.to_string(),
        params: bounds
            .into_iter()
            .map(rusqlite::types::Value::Integer)
            .collect(),
    }
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

    const ALL_OPS: [Op; 6] = [Op::Contains, Op::Eq, Op::Lt, Op::Le, Op::Gt, Op::Ge];

    /// A zone `hours` east of UTC, as [`day_bounds`] takes it.
    fn fixed_zone(hours: i32) -> impl Fn(NaiveDateTime) -> MappedLocalTime<i64> {
        let offset = chrono::FixedOffset::east_opt(hours * 3600).unwrap();
        move |t| offset.from_local_datetime(&t).map(|t| t.timestamp())
    }

    fn date(s: &str) -> NaiveDate {
        parse_date(s).unwrap_or_else(|| panic!("{} should parse", s))
    }

    fn utc_midnight(day: NaiveDate) -> i64 {
        day.and_time(NaiveTime::MIN).and_utc().timestamp()
    }

    /// Each operator's rows, checked against real SQLite, at the edges of
    /// the local day: `>D` starts after all of D, `<=D` takes all of it.
    #[test]
    fn day_operators_compare_whole_days() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (mtime INTEGER NOT NULL)", [])
            .unwrap();
        let day = date("2024-05-20");
        for hours in [-8, 14] {
            let (start, end) = day_bounds(day, fixed_zone(hours));
            assert_eq!(start, utc_midnight(day) - i64::from(hours) * 3600);
            assert_eq!(end - start, 86_400, "UTC{:+}", hours);

            conn.execute("DELETE FROM files", []).unwrap();
            let rows = [start - 1, start, start + 43_200, end - 1, end];
            for r in rows {
                conn.execute("INSERT INTO files (mtime) VALUES (?1)", [r])
                    .unwrap();
            }
            let matching = |op: Op| -> Vec<i64> {
                let f = day_filter(op, (start, end));
                let sql = format!("SELECT mtime FROM files f WHERE {} ORDER BY 1", f.sql);
                let mut stmt = conn.prepare(&sql).unwrap();
                let found = stmt
                    .query_map(rusqlite::params_from_iter(&f.params), |r| r.get(0))
                    .unwrap();
                found.map(Result::unwrap).collect()
            };
            let before = vec![start - 1];
            let within = vec![start, start + 43_200, end - 1];
            let after = vec![end];
            assert_eq!(matching(Op::Lt), before, "UTC{:+}", hours);
            assert_eq!(matching(Op::Eq), within, "UTC{:+}", hours);
            assert_eq!(matching(Op::Contains), within, "UTC{:+}", hours);
            assert_eq!(matching(Op::Gt), after, "UTC{:+}", hours);
            assert_eq!(matching(Op::Le), [&before[..], &within[..]].concat());
            assert_eq!(matching(Op::Ge), [&within[..], &after[..]].concat());
            // Sorted, so equal to the sorted rows only if <, = and > are
            // disjoint and cover every row.
            let parts = [matching(Op::Lt), matching(Op::Eq), matching(Op::Gt)];
            assert_eq!(parts.concat(), rows, "UTC{:+}", hours);
        }
    }

    /// `build_filter` takes the bounds from the process's zone, whatever it
    /// is: local midnight lies within UTC-12..UTC+14 of UTC midnight.
    #[test]
    fn modified_filters_use_the_local_day() {
        let day = date("2024-05-20");
        let (start, end) = day_bounds(day, local_instant);
        for key in ["modified", "mtime"] {
            for op in ALL_OPS {
                let f = frag(key, op, "2024-05-20");
                let expected = day_filter(op, (start, end));
                assert_eq!((f.sql, f.params), (expected.sql, expected.params));
            }
        }
        assert!((-14 * 3600..=12 * 3600).contains(&(start - utc_midnight(day))));
        assert!((23 * 3600..=25 * 3600).contains(&(end - start)));
    }

    /// A zone at UTC-3 that springs forward to UTC-2 at local midnight:
    /// that day's 00:00-01:00 never happens, so the day starts at 01:00.
    #[test]
    fn a_dst_gap_midnight_starts_at_the_first_valid_hour() {
        let day = date("2024-09-08");
        let midnight = day.and_time(NaiveTime::MIN);
        let gap_end = midnight + TimeDelta::hours(1);
        let zone = |t: NaiveDateTime| {
            let wall = t.and_utc().timestamp();
            if t < midnight {
                MappedLocalTime::Single(wall + 3 * 3600)
            } else if t < gap_end {
                MappedLocalTime::None
            } else {
                MappedLocalTime::Single(wall + 2 * 3600)
            }
        };
        let (start, end) = day_bounds(day, zone);
        assert_eq!(start, utc_midnight(day) + 3 * 3600, "01:00 UTC-2");
        assert_eq!(end - start, 23 * 3600);
        // The day before ends where this one starts: no instant is lost.
        let (before_start, before_end) = day_bounds(day.pred_opt().unwrap(), zone);
        assert_eq!(before_end, start);
        assert_eq!(before_end - before_start, 86_400);
    }

    /// Samoa skipped 2011-12-30 by moving from UTC-10 to UTC+14 at its
    /// midnight: the missing day is empty, and its neighbours meet.
    #[test]
    fn a_skipped_day_matches_nothing() {
        let skipped = date("2011-12-30");
        let midnight = skipped.and_time(NaiveTime::MIN);
        let next = midnight + TimeDelta::days(1);
        let zone = |t: NaiveDateTime| {
            let wall = t.and_utc().timestamp();
            if t < midnight {
                MappedLocalTime::Single(wall + 10 * 3600)
            } else if t < next {
                MappedLocalTime::None
            } else {
                MappedLocalTime::Single(wall - 14 * 3600)
            }
        };
        let (start, end) = day_bounds(skipped, zone);
        assert_eq!(start, end, "a day that never happened");
        assert_eq!(day_bounds(skipped.pred_opt().unwrap(), zone).1, start);
        assert_eq!(day_bounds(skipped.succ_opt().unwrap(), zone).0, start);
    }

    /// A zone at UTC-2 that falls back to UTC-3 at 01:00: 00:00-01:00
    /// happens twice, and the day starts at the first of them, whichever
    /// order the pair comes in (chrono's differs between platforms).
    #[test]
    fn a_dst_overlap_uses_the_earliest_instant() {
        let day = date("2024-04-07");
        let midnight = day.and_time(NaiveTime::MIN);
        let fold_end = midnight + TimeDelta::hours(1);
        for later_first in [false, true] {
            let zone = |t: NaiveDateTime| {
                let wall = t.and_utc().timestamp();
                let (first, second) = (wall + 2 * 3600, wall + 3 * 3600);
                if t < midnight {
                    MappedLocalTime::Single(first)
                } else if t >= fold_end {
                    MappedLocalTime::Single(second)
                } else if later_first {
                    MappedLocalTime::Ambiguous(second, first)
                } else {
                    MappedLocalTime::Ambiguous(first, second)
                }
            };
            let (start, end) = day_bounds(day, zone);
            assert_eq!(start, utc_midnight(day) + 2 * 3600, "00:00 UTC-2");
            assert_eq!(end - start, 25 * 3600);
        }
    }

    /// The production zone against real tzdata, in a child process per case
    /// so this process's TZ is never touched. Every case moves the clocks at
    /// midnight; at the two folds, `earliest()` or the plain earlier of
    /// chrono's pair would be an hour off. Without tzdata it fails rather
    /// than passing on no cases, unless `QSB_SKIP_TZ` is set.
    #[test]
    #[cfg(unix)]
    fn local_days_follow_real_zone_rules() {
        let mut ran = 0;
        for (zone, day, ..) in LOCAL_DAY_CASES {
            let tzdata = std::path::Path::new("/usr/share/zoneinfo").join(zone);
            if !tzdata.exists() {
                eprintln!("skipping {}: no tzdata", zone);
                continue;
            }
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "query::translator::tests::local_days_child"])
                .env("TZ", zone)
                .env(LOCAL_DAYS_CHILD, day)
                .output()
                .expect("spawn the test binary");
            let report = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{} {}: {}", zone, day, report);
            // A renamed child matches no test and passes on nothing.
            assert!(report.contains("1 passed"), "{} {}: {}", zone, day, report);
            ran += 1;
        }
        assert!(
            ran > 0 || std::env::var_os("QSB_SKIP_TZ").is_some(),
            "no tzdata under /usr/share/zoneinfo: install it, or set QSB_SKIP_TZ"
        );
    }

    #[cfg(unix)]
    const LOCAL_DAYS_CHILD: &str = "QS_LOCAL_DAYS_CHILD";

    /// `(zone, day, start, hours)`: the day's first instant and its length.
    #[cfg(unix)]
    const LOCAL_DAY_CASES: [(&str, &str, i64, i64); 4] = [
        // 24:00 jumps to 01:00: the day starts at 01:00 UTC-3.
        ("America/Santiago", "2024-09-08", 1_725_768_000, 23),
        // 24:00 goes back to 23:00: midnight comes once, at UTC-4.
        ("America/Santiago", "2024-04-07", 1_712_462_400, 24),
        // 01:00 goes back to 00:00: the first midnight, at UTC-4.
        ("America/Havana", "2024-11-03", 1_730_606_400, 25),
        // UTC-10 to UTC+14 at midnight: the day is empty.
        ("Pacific/Apia", "2011-12-30", 1_325_239_200, 0),
    ];

    /// The child half of [`local_days_follow_real_zone_rules`]. A no-op in
    /// an ordinary run.
    #[test]
    #[cfg(unix)]
    fn local_days_child() {
        let Ok(day) = std::env::var(LOCAL_DAYS_CHILD) else {
            return;
        };
        let (zone, _, start, hours) = LOCAL_DAY_CASES
            .into_iter()
            .find(|c| c.1 == day)
            .expect("a listed day");
        let bounds = day_bounds(date(&day), local_instant);
        assert_eq!(bounds, (start, start + hours * 3600), "{} {}", zone, day);
    }

    /// The highlighter's verdict is the engine's, dates included.
    #[test]
    fn check_filter_agrees_with_build_filter() {
        let cases = [
            ("modified", Op::Ge, "2024-05-02"),
            ("MTime", Op::Lt, "2024-5-2"),
            ("modified", Op::Eq, "2024-13-01"),
            ("modified", Op::Contains, "tomorrow"),
            ("mtime", Op::Gt, "1969-12-31"),
            ("type", Op::Contains, "Audio"),
            ("type", Op::Lt, "Audio"),
            ("type", Op::Eq, "NotAThing"),
            ("path", Op::Gt, "/x"),
            ("name", Op::Contains, "a*b"),
            ("bogus", Op::Contains, "x"),
        ];
        for (key, op, value) in cases {
            assert_eq!(
                check_filter(key, op, value, true).is_ok(),
                build_filter(key, op, value, true).is_ok(),
                "{} {:?} {}",
                key,
                op,
                value
            );
        }
        assert!(check_filter("modified", Op::Ge, "2024-05-02", true).is_ok());
        assert!(check_filter("modified", Op::Ge, "2024-02-30", true).is_err());
    }

    #[test]
    fn a_zone_that_places_nothing_is_read_as_utc() {
        let day = date("2024-05-20");
        let midnight = utc_midnight(day);
        let bounds = day_bounds(day, |_| MappedLocalTime::None);
        assert_eq!(bounds, (midnight, midnight + 86_400));
    }

    #[test]
    fn date_values_parse_canonically() {
        assert_eq!(parse_date("2024-5-2"), parse_date("2024-05-02"));
        assert_eq!(parse_date("2024-5-2"), NaiveDate::from_ymd_opt(2024, 5, 2));
        for good in ["1970-01-01", "9998-12-31"] {
            assert!(parse_date(good).is_some(), "{} should parse", good);
        }
        for bad in [
            "+2024-01-01",
            "2024-+1-01",
            "-2024-01-01",
            "2024-01-01T00:00",
            "2024-01-01'--",
            "2024-01-01-",
            "2024--01",
            "2024-01-",
            " 2024-01-01",
            "2024-01-01 ",
            "２０２４-01-01", // fullwidth digits
            "2024-01-99999999999",
            "10000-01-01",
            "9999-12-31",
            "",
        ] {
            assert!(parse_date(bad).is_none(), "{:?} should be rejected", bad);
            assert!(matches!(
                build_filter("modified", Op::Le, bad, false),
                Err(TranslateError::BadDate(_))
            ));
        }
    }

    #[test]
    fn path_filter_covers_the_folder_and_its_subtree() {
        let f = frag("path", Op::Contains, "/home/me/docs");
        assert!(f.sql.contains("f.parent LIKE ?"), "{}", f.sql);
        assert!(!f.sql.contains("f.parent = ?"), "{}", f.sql);
        assert_eq!(f.params.len(), 1);
        // Without ESCAPE a Windows separator would be eaten as an escape.
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
        assert!(matches!(
            build_filter("name", Op::Ge, "report", false),
            Err(TranslateError::UnsupportedOp { .. })
        ));
    }

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
            assert!(parse_date(bad).is_none(), "{} should be rejected", bad);
        }
        for good in ["2024-02-29", "2000-02-29", "2024-01-31", "2024-04-30"] {
            assert!(parse_date(good).is_some(), "{} should parse", good);
        }
        // Centurial leap rule: 2100 is not a leap year, 2000 is.
        assert!(parse_date("2100-02-29").is_none());
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

    #[test]
    fn is_regex_key_ignores_case_and_nothing_else() {
        for key in ["regex", "REGEX", "Regex"] {
            assert!(is_regex_key(key), "{}", key);
        }
        for key in ["regexp", "rege", "re gex", "type", ""] {
            assert!(!is_regex_key(key), "{}", key);
        }
    }

    /// The one place the separator and the LIKE escape collide (on Windows
    /// they are the same byte), so it is checked against real SQLite.
    #[test]
    fn like_subtree_pattern_matches_only_the_subtree() {
        use std::path::MAIN_SEPARATOR as SEP;

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (parent TEXT NOT NULL)", [])
            .unwrap();

        let base = format!("{}a{}b", root_prefix(), SEP);
        // Each row is spelled the way the indexer would: trailing separator.
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
        // Three, not two: the trailing separator brings the folder's *own*
        // files in; the prefix sibling and the parent stay out.
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
        assert_eq!(normalize_folder_value("/home/me/"), "/home/me");
        assert_eq!(normalize_folder_value(" /home/me// "), "/home/me");
        // Empty means "everywhere".
        assert_eq!(normalize_folder_value("/"), "");
        assert_eq!(normalize_folder_value("  "), "");
    }

    /// Stored parents use `\` alone, so every value is respelled with it.
    #[test]
    #[cfg(windows)]
    fn windows_folder_values_take_the_stored_spelling() {
        assert_eq!(normalize_folder_value(r"C:\Users\me\"), r"C:\Users\me");
        // A bare drive and a rooted one normalize alike.
        assert_eq!(normalize_folder_value("C:"), "C:");
        assert_eq!(normalize_folder_value(r"C:\"), "C:");
        assert_eq!(normalize_folder_value("C:/"), "C:");
        assert_eq!(
            like_subtree_pattern(&normalize_folder_value("C:")),
            like_subtree_pattern(&normalize_folder_value(r"C:\")),
        );
        assert_eq!(normalize_folder_value("D:/data/sub/"), r"D:\data\sub");
        assert_eq!(normalize_folder_value("//srv/share"), r"\\srv\share");
        assert_eq!(normalize_folder_value(r"D:/a\b/"), r"D:\a\b");
        assert_eq!(normalize_folder_value(r"\"), "");
    }

    /// `path:D:/data` must find the parents the indexer spelled `D:\data\`,
    /// checked against real SQLite: `\` is also the LIKE escape.
    #[test]
    #[cfg(windows)]
    fn forward_slash_value_matches_backslash_parents() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE files (parent TEXT NOT NULL)", [])
            .unwrap();
        for r in [
            r"D:\data\",          // the folder's own files
            r"D:\data\sub\",      // beneath it
            r"D:\database\",      // prefix sibling: outside
            r"D:\",               // parent: outside
            r"\\srv\share\docs\", // UNC
        ] {
            conn.execute("INSERT INTO files (parent) VALUES (?1)", [r])
                .unwrap();
        }
        let count = |value: &str| -> i64 {
            let f = frag("path", Op::Contains, value);
            conn.query_row(
                &format!("SELECT COUNT(*) FROM files f WHERE {}", f.sql),
                rusqlite::params_from_iter(&f.params),
                |r| r.get(0),
            )
            .unwrap()
        };
        for value in ["D:/data", "D:/data/", r"D:/data\", r"D:\data"] {
            assert_eq!(count(value), 2, "path:{}", value);
        }
        assert_eq!(count("D:/data/sub"), 1);
        assert_eq!(count("//srv/share"), 1);
    }

    /// On Unix `\` is a name byte: kept inside a value and at its end.
    #[test]
    #[cfg(unix)]
    fn a_backslash_in_a_unix_folder_value_is_a_name_byte() {
        assert_eq!(normalize_folder_value(r"/a\b"), r"/a\b");
        assert_eq!(normalize_folder_value(r"/a\"), r"/a\");
        assert_eq!(normalize_folder_value(r"/a\/"), r"/a\");
        assert_eq!(like_subtree_pattern(r"/a\"), r"/a\\/%");
    }

    #[test]
    fn empty_folder_value_matches_everything() {
        let frag = build_filter("path", Op::Contains, "/", false).unwrap();
        assert_eq!(frag.sql, "1=1");
        assert!(frag.params.is_empty(), "no placeholders to renumber");
    }

    fn root_prefix() -> String {
        if cfg!(windows) {
            r"C:\".to_string()
        } else {
            "/".to_string()
        }
    }
}
