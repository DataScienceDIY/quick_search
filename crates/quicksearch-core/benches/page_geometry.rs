//! What the index's page geometry costs, on disk and in query time.
//!
//! Two levers, swept together because they are coupled: the **database page
//! size** (`db::schema::PAGE_SIZE`) and FTS5's **record size**
//! (`db::schema::fts_pgsz_for`). The second is derived from the first, so
//! neither can be moved alone — at a page size of 8192 an FTS5 record built
//! for 4096 leaves half of every page empty.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_PGSZ=1 \
//!   cargo bench -p quicksearch-core --bench page_geometry
//! ```
//!
//! `TMPDIR` is not optional in spirit. `testutil::scratch_dir` builds on
//! `std::env::temp_dir()`, and a `/tmp` that is tmpfs cannot produce a page
//! fetch that was not already in RAM — it would price the one regime this is
//! not trying to characterise, and a 1M-file arm would not fit besides. Point
//! it at real storage. The matrix wants ~2 GB at a time (each arm is dropped
//! once measured) and around ten minutes, most of it seeding.
//!
//! # Settled: FTS5 record size
//!
//! SQLCipher reserves part of every page for its IV and any authenticator, so
//! a keyed page holds `page − reserve − 35` bytes inline. FTS5's default
//! record of 4050 was chosen for a *plain* 4096 page and missed that by 71
//! bytes under the 80-byte reserve of the day, which sent every full leaf to
//! an overflow page. Measured at 120k files before `fts_pgsz_for` existed, and
//! while `db::schema::HMAC_MODE` was still HMAC-SHA512 — at today's 16-byte
//! reserve the miss is smaller, but the derivation is what makes it zero at
//! *every* page size:
//!
//! | | plain 4050 | plain shipped | keyed 4050 | keyed shipped |
//! |---|---|---|---|---|
//! | bulk write | 3.87 s | 4.04 s | 7.34 s | 7.13 s |
//! | size | 156.4 MiB | 156.4 MiB | **170.2 MiB** | **159.1 MiB** |
//! | fts overflow pages | 0 | 0 | **27381** | 0 |
//! | cold `chalcedony` | 11.35 ms | 11.37 ms | 29.91 ms | 29.57 ms |
//! | warm `chalcedony` | 9.29 ms | 9.24 ms | 9.41 ms | 9.27 ms |
//!
//! A disk-space fix (1.089x → 1.011x encrypted-over-plain), not a speed fix:
//! both indexing and search moved less than the harness's own noise floor.
//! `tests/encrypted_perf.rs` gates the size half of that and is the reason
//! this bench does not re-measure it.
//!
//! # Open: database page size
//!
//! A keyed index decrypts a whole page to read one row out of it. If the
//! expensive fetches are *scattered* single rows, a smaller page cuts that
//! work in proportion, and — `cache_size` being a byte ceiling — lets the same
//! 32 MiB hold four times as many distinct rows. Pulling the other way, the
//! `files` scan behind every filename query is sequential and wants large
//! pages, and the reserve costs proportionally more of a small page: at the
//! 80 bytes of the HMAC-SHA512 era that was 2% of a 4096-byte page against
//! 7.8% of a 1024-byte one, and at today's 16 it is 0.4% against 1.6%.
//!
//! [`attribution`] settles which of those a query actually does, by counting
//! page-cache misses per query shape rather than inferring them from timings.
//!
//! # Reading it
//!
//! **The plain arm is the noise floor**, and at the 200k tier a just-seeded
//! index is small enough that the OS page cache serves nearly all of it — so
//! those figures price decrypt work with little I/O in them. The 1M tier
//! exceeds what stays cached, and is where real reads enter: storage reads in
//! ≥4 KiB blocks whatever the page size, so a sub-4K page cuts decryption but
//! not I/O. The two tiers are reported separately for that reason; do not
//! average them.

use std::time::{Duration, Instant};

use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::testutil::{cache_stats, Arm, SeedSpec, BODY_TERM, NEEDLE};
use rusqlite::Connection;

/// Page sizes to sweep, up to `SQLITE_MAX_PAGE_SIZE`. 512 is excluded: with
/// SQLCipher's 80-byte reserve its usable size falls under SQLite's 480-byte
/// floor. FTS5 caps its own record size at 64 KiB and rejects anything larger,
/// so 65536 is the last size where `fts_pgsz_for` still has room.
const SWEPT: [i64; 7] = [1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// `QSB_PGSZ_SIZES=8192,16384` narrows the sweep; a full run is ~45 minutes,
/// nearly all of it seeding, so re-asking one question should not re-ask all
/// of them.
fn swept() -> Vec<i64> {
    match std::env::var("QSB_PGSZ_SIZES") {
        Ok(list) => list
            .split(',')
            .map(|s| s.trim().parse().expect("QSB_PGSZ_SIZES wants integers"))
            .collect(),
        Err(_) => SWEPT.to_vec(),
    }
}

/// `QSB_PGSZ_SHAPE_ONLY=1` skips the large corpora.
fn shape_only() -> bool {
    std::env::var("QSB_PGSZ_SHAPE_ONLY").is_ok()
}

/// The shape tier — every page size, cheap enough to run them all.
const SHAPE_FILES: usize = 200_000;
/// The confirmation tiers, run only for the baseline and the shape tier's
/// winner. 1M is where the working set stops fitting in the OS cache.
const SCALE_FILES: [usize; 2] = [600_000, 1_000_000];

const CONTENT_EVERY: usize = 8;

/// Commit in slices, as a production run does: each commit flushes an FTS5
/// segment, so a single enormous transaction would not resemble one.
const COMMIT_EVERY: usize = 5_000;

/// Best-of-N. The minimum is the run least disturbed by whatever else is on
/// the box, which is the honest figure for a comparison.
const RUNS: u32 = 5;

/// The workloads, in the order they are reported. One word from
/// `testutil::WORDS` leads: its posting lists are long, where the rare terms
/// stop at the display limit having touched very little.
const WORKLOADS: [(&str, &str, bool); 6] = [
    ("body (common)", "planning", false),
    ("body (rare)", BODY_TERM, false),
    ("name", NEEDLE, false),
    ("fuzzy", "quartzlte", true),
    ("wildcard", "quart*", false),
    ("regex", "regex:quart[sz]ite", false),
];

fn enabled() -> bool {
    std::env::var("QSB_PGSZ").is_ok()
}

fn spec(files: usize, page_size: i64) -> SeedSpec {
    SeedSpec {
        files,
        content_every: CONTENT_EVERY,
        dup_every: 5,
        commit_every: COMMIT_EVERY,
        page_size: Some(page_size),
        ..SeedSpec::default()
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_PGSZ=1 to run");
        return;
    }
    if std::env::var_os("TMPDIR").is_none() {
        eprintln!(
            "warning: TMPDIR unset — scratch goes to {}. If that is tmpfs, \
             every 'cold' figure below is RAM and the large tiers may not fit.",
            std::env::temp_dir().display()
        );
    }

    let best = shape_tier();
    if shape_only() {
        println!("\n(QSB_PGSZ_SHAPE_ONLY set — skipping the large corpora)");
        return;
    }
    scale_tier(best);
}

/// Every page size at [`SHAPE_FILES`], plain and keyed. Returns the keyed page
/// size with the lowest total warm time — the metric that matters, because a
/// session re-queries on every keystroke.
fn shape_tier() -> i64 {
    println!(
        "\n######## shape tier: {} files, {} with content ########",
        SHAPE_FILES,
        SHAPE_FILES / CONTENT_EVERY
    );
    let mut best = (db::schema::PAGE_SIZE, f64::MAX);
    for page_size in swept() {
        for keyed in [false, true] {
            let arm = Arm::seed(
                format!("{} {}", if keyed { "keyed" } else { "plain" }, page_size),
                &format!("pgsz-{}-{}", page_size, keyed),
                keyed,
                &spec(SHAPE_FILES, page_size),
            );
            let warm_total = report(&arm);
            if keyed && warm_total < best.1 {
                best = (page_size, warm_total);
            }
            arm.discard();
        }
    }
    println!(
        "\n>>> lowest keyed warm total at page_size {} ({:.1} ms across {} workloads)",
        best.0,
        best.1 * 1000.0,
        WORKLOADS.len()
    );
    best.0
}

/// The baseline and the winner only, at the larger corpora.
fn scale_tier(best: i64) {
    let mut sizes = vec![db::schema::PAGE_SIZE];
    if best != db::schema::PAGE_SIZE {
        sizes.push(best);
    }
    for files in SCALE_FILES {
        println!(
            "\n######## scale tier: {} files, {} with content ########",
            files,
            files / CONTENT_EVERY
        );
        for page_size in &sizes {
            for keyed in [false, true] {
                let arm = Arm::seed(
                    format!("{} {}", if keyed { "keyed" } else { "plain" }, page_size),
                    &format!("pgsz-{}-{}-{}", files, page_size, keyed),
                    keyed,
                    &spec(files, *page_size),
                );
                report(&arm);
                arm.discard();
            }
        }
    }
}

/// Everything measured about one arm. Returns its total warm query time in
/// seconds, the metric [`shape_tier`] ranks on.
fn report(arm: &Arm) -> f64 {
    let (leaf, overflow) = arm.fts_pages();
    let files_bytes = arm.table_bytes("files");
    println!(
        "\n=== {} ===  {:.1} MiB on disk, files table {:.1} MiB, \
         fts {} leaf / {} overflow, written in {:.1?} ({:.0} rows/s)",
        arm.what,
        mib(arm.size_bytes()),
        mib(files_bytes),
        leaf,
        overflow,
        arm.seeded_in,
        seeded_rows(arm) as f64 / arm.seeded_in.as_secs_f64(),
    );

    attribution(arm);

    println!(
        "{:<16}{:>12}{:>12}{:>12}{:>10}",
        "workload", "cold", "warm", "cold miss", "hits"
    );
    let conn = arm.open_search();
    let mut warm_total = 0.0;
    for (what, query, fuzzy) in WORKLOADS {
        let (cold_time, misses, hits) = cold(arm, query, fuzzy);
        let warm_time = warm(&conn, query, fuzzy);
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

fn seeded_rows(arm: &Arm) -> i64 {
    let conn = arm.open_search();
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0)
}

/// **Where the page fetches go.** Each shape runs on its own fresh connection
/// and reports the misses it caused, so the cost lands on the table that
/// caused it rather than on whichever query happened to be slow.
///
/// The shapes are the ones `search/cascade/passes.rs` actually issues: pass A
/// is a `files` scan with no FTS in it at all, and pass B's FTS MATCH is
/// joined straight back to `files` by rowid and to `documents_text` for the
/// body — so each posting costs a random row seek and a blob read on top of
/// the posting list that produced it. The middle two rows separate those.
fn attribution(arm: &Arm) {
    let term = BODY_TERM;
    let like = format!("%{}%", term);
    let match_expr = format!("text: \"{}\"", term);

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

/// Run one query, counting hits rather than keeping them — holding the
/// `SearchHit`s would measure the allocator instead of the scan.
fn run_query(conn: &Connection, query: &str, fuzzy: bool) -> (Duration, usize) {
    let split = split_for_cascade(query).expect("query parses");
    let options = SearchOptions {
        fuzzy,
        ..SearchOptions::default()
    };
    let latest = std::sync::atomic::AtomicU64::new(1);
    let mut hits = 0usize;
    let mut sink = |batch: Vec<SearchHit>| hits += batch.len();
    let start = Instant::now();
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("cascade runs");
    (start.elapsed(), hits)
}

/// Best of `RUNS`, each on a **fresh** connection, so SQLite's page cache
/// starts empty and every page the query wants is a miss. Returns the miss
/// count alongside, which is what makes the timing interpretable.
fn cold(arm: &Arm, query: &str, fuzzy: bool) -> (Duration, i64, usize) {
    let mut best = Duration::MAX;
    let mut misses = 0;
    let mut hits = 0;
    for _ in 0..RUNS {
        let conn = arm.open_search();
        let before = cache_stats(&conn).1;
        let (elapsed, n) = run_query(&conn, query, fuzzy);
        if elapsed < best {
            best = elapsed;
            misses = cache_stats(&conn).1 - before;
        }
        hits = n;
    }
    (best, misses, hits)
}

/// Best of `RUNS` on one connection after a priming run — the steady state of
/// a typing session, which is what almost every real search is.
fn warm(conn: &Connection, query: &str, fuzzy: bool) -> Duration {
    run_query(conn, query, fuzzy);
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        best = best.min(run_query(conn, query, fuzzy).0);
    }
    best
}
