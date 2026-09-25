//! What `cipher_hmac` and `page_geometry` share: the seed shape, the
//! workloads and the per-arm report. `search_perf` uses [`mib`] and
//! [`warn_if_tmpdir_unset`].

// Each bench binary uses only part of it.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use quicksearch_core::db;
use quicksearch_core::search::SearchOptions;
use quicksearch_core::testutil::{cache_stats, run_cascade, Arm, SeedSpec, BODY_TERM, NEEDLE};
use rusqlite::Connection;

/// One file in this many carries a document body.
pub const CONTENT_EVERY: usize = 8;

/// Best-of-N. The minimum is the run least disturbed by whatever else is on
/// the box, which is the honest figure for a comparison.
pub const RUNS: u32 = 5;

/// The workloads, in report order. A common word from `testutil::WORDS`
/// leads: its posting lists are long, where the rare terms stop at the
/// display limit having touched very little.
pub const WORKLOADS: [(&str, &str, bool); 6] = [
    ("body (common)", "planning", false),
    ("body (rare)", BODY_TERM, false),
    ("name", NEEDLE, false),
    ("fuzzy", "quartzlte", true),
    ("wildcard", "quart*", false),
    ("regex", "regex:quart[sz]ite", false),
];

/// The corpus every arm is seeded with; the caller sets the variable under
/// test. It commits in slices, as a production run does: each commit
/// flushes an FTS5 segment, so one enormous transaction would not resemble a
/// run, and the write path is half of what these benches measure.
pub fn spec(files: usize) -> SeedSpec {
    SeedSpec {
        files,
        content_every: CONTENT_EVERY,
        dup_every: 5,
        commit_every: 5_000,
        ..SeedSpec::default()
    }
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Warn when `TMPDIR` is unset, finishing the sentence with `consequence`.
/// Scratch then goes to `std::env::temp_dir()`, and a tmpfs there cannot
/// produce a page fetch that was not already in RAM: every cold figure would
/// price RAM, and a 1M-file arm would not fit besides.
pub fn warn_if_tmpdir_unset(consequence: &str) {
    if std::env::var_os("TMPDIR").is_none() {
        eprintln!(
            "warning: TMPDIR unset — scratch goes to {}. If that is tmpfs, {}.",
            std::env::temp_dir().display(),
            consequence
        );
    }
}

/// Print the banner that opens a corpus tier.
pub fn banner(tier: &str, files: usize) {
    println!(
        "\n######## {} tier: {} files, {} with content ########",
        tier,
        files,
        files / CONTENT_EVERY
    );
}

/// Print an arm's size, FTS page counts and write rate; returns its FTS
/// overflow page count.
pub fn header(arm: &Arm) -> i64 {
    let (leaf, overflow) = arm.fts_pages();
    println!(
        "\n=== {} ===  {:.1} MiB on disk, files table {:.1} MiB, \
         fts {} leaf / {} overflow, written in {:.1?} ({:.0} rows/s)",
        arm.what,
        mib(arm.size_bytes()),
        mib(arm.table_bytes("files")),
        leaf,
        overflow,
        arm.seeded_in,
        seeded_rows(arm) as f64 / arm.seeded_in.as_secs_f64(),
    );
    overflow
}

fn seeded_rows(arm: &Arm) -> i64 {
    let conn = arm.open_search();
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Print where the page fetches go. Each shape runs on its own fresh
/// connection and reports the misses it caused, so the cost lands on the
/// table that caused it rather than on whichever query happened to be slow.
///
/// The shapes are the ones `search/cascade/passes.rs` issues: pass A is a
/// `files` scan with no FTS in it, and pass B's FTS MATCH is joined back to
/// `files` by rowid and to `documents_text` for the body, so each posting
/// costs a random row seek and a blob read on top of the posting list that
/// produced it. The middle two rows separate those.
pub fn attribution(arm: &Arm) {
    let like = format!("%{}%", BODY_TERM);
    let match_expr = format!("text: \"{}\"", BODY_TERM);

    let shapes: [(&str, &str, &str); 4] = [
        (
            "pass A: files scan",
            "SELECT COUNT(*) FROM files f WHERE f.name LIKE ?1 ESCAPE '\\'",
            "like",
        ),
        (
            "  FTS postings only",
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
            "match",
        ),
        (
            "  + files rowid join",
            "SELECT COUNT(*) FROM searchabletext \
             JOIN files f ON f.id = searchabletext.rowid \
             WHERE searchabletext MATCH ?1",
            "match",
        ),
        (
            "pass B: + the bodies",
            "SELECT SUM(LENGTH(dt.text_zstd)) FROM searchabletext \
             JOIN files f ON f.id = searchabletext.rowid \
             LEFT JOIN documents_text dt ON dt.file_id = f.id \
             WHERE searchabletext MATCH ?1",
            "match",
        ),
    ];

    println!(
        "{:<24}{:>12}{:>12}{:>14}",
        "cold page misses", "misses", "time", "MiB decrypted"
    );
    for (what, sql, param) in shapes {
        // A fresh connection per shape: the miss count is only meaningful
        // from an empty cache.
        let conn = arm.open_search();
        let bound: &str = if param == "like" { &like } else { &match_expr };
        let before = cache_stats(&conn).1;
        let start = Instant::now();
        conn.query_row(sql, [bound], |r| r.get::<_, Option<i64>>(0))
            .expect("attribution shape runs");
        let elapsed = start.elapsed();
        let misses = cache_stats(&conn).1 - before;
        let page = arm.page_size.unwrap_or(db::schema::PAGE_SIZE);
        println!(
            "{:<24}{:>12}{:>12}{:>14.1}",
            what,
            misses,
            format!("{:.2?}", elapsed),
            (misses * page) as f64 / (1024.0 * 1024.0)
        );
    }
}

/// Print the cold and warm time of every [`WORKLOADS`] entry; returns the
/// total warm time in seconds.
pub fn workloads(arm: &Arm) -> f64 {
    println!(
        "{:<16}{:>12}{:>12}{:>12}{:>10}",
        "workload", "cold", "warm", "cold miss", "hits"
    );
    let conn = arm.open_search();
    let mut warm_total = 0.0;
    for (what, query, fuzzy) in WORKLOADS {
        let options = SearchOptions {
            fuzzy,
            ..SearchOptions::default()
        };
        let (cold_time, misses, hits) = cold(arm, query, &options);
        let warm_time = warm(&conn, query, &options);
        warm_total += warm_time.as_secs_f64();
        println!(
            "{:<16}{:>12}{:>12}{:>12}{:>10}",
            what,
            format!("{:.2?}", cold_time),
            format!("{:.2?}", warm_time),
            misses,
            hits
        );
    }
    warm_total
}

/// Best of [`RUNS`], each on a fresh connection, so SQLite's page cache
/// starts empty and every page the query wants is a miss: the regime where
/// per-page work is paid rather than skipped. Returns the best time with
/// that run's miss count, and the hit count.
fn cold(arm: &Arm, query: &str, options: &SearchOptions) -> (Duration, i64, usize) {
    let mut best = Duration::MAX;
    let mut misses = 0;
    let mut hits = 0;
    for _ in 0..RUNS {
        let conn = arm.open_search();
        let before = cache_stats(&conn).1;
        let (elapsed, n) = run_cascade(&conn, query, options);
        if elapsed < best {
            best = elapsed;
            misses = cache_stats(&conn).1 - before;
        }
        hits = n;
    }
    (best, misses, hits)
}

/// Best of [`RUNS`] on one connection after a priming run: the steady state
/// of a typing session, which almost every real search is.
fn warm(conn: &Connection, query: &str, options: &SearchOptions) -> Duration {
    run_cascade(conn, query, options);
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        best = best.min(run_cascade(conn, query, options).0);
    }
    best
}
