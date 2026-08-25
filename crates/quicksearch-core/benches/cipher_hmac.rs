//! What SQLCipher's per-page authenticator costs, so the build can decide
//! whether to keep one.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_HMAC=1 \
//!   cargo bench -p quicksearch-core --bench cipher_hmac
//! ```
//!
//! `TMPDIR` is not optional in spirit, for the reason
//! `benches/page_geometry.rs` gives at length: a tmpfs `/tmp` cannot produce a
//! page fetch that was not already in RAM, and the scale tier will not fit
//! besides. Point it at real storage.
//!
//! # The question
//!
//! The cipher is not a choice. SQLCipher 4 removed `PRAGMA cipher` and the
//! provider hard-codes AES-256-CBC, so the only lever the build has is the
//! HMAC — and that lever is worth pulling on because the index holds text read
//! out of files the same user can already read. Anything positioned to *tamper*
//! with the index could read the originals instead, so per-page authentication
//! defends very little while being paid on every page read and every page
//! write. An unprotected index has never had any, either.
//!
//! Three modes, and the reason the middle one is not obviously pointless:
//!
//! | mode | reserve | per page |
//! |---|---|---|
//! | `Sha512` | 80 | SQLCipher's default |
//! | `Sha256` | 48 | SHA-NI on Zen and Ice Lake+, and 32 bytes of page back |
//! | `Off` | 16 | no authenticator at all |
//!
//! The reserve matters twice: it is page space the rows do not get, and
//! `db::schema::fts_pgsz_for` derives FTS5's record size from it, so each arm
//! also gets a differently-shaped leaf.
//!
//! **The write path is the one to watch.** `sqlcipher_openssl_hmac` calls
//! `EVP_MAC_fetch(NULL, "HMAC", NULL)`, `EVP_MAC_CTX_new` and an
//! `EVP_MAC_init` that fetches the digest *by name* — two OpenSSL 3 provider
//! lookups per page, on top of the hash itself. That fixed cost is paid
//! whichever digest is selected, which is why `Sha256` may buy far less than
//! its digest speed suggests, and why `Off` may buy far more.
//!
//! # Reading it
//!
//! The plain arm is the noise floor, not a candidate: it is what the product
//! does with no password set. Rank the three keyed arms against each other and
//! against it.

use std::time::{Duration, Instant};

use quicksearch_core::db;
use quicksearch_core::db::schema::HmacMode;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, find_duplicate_groups, SearchHit, SearchOptions};
use quicksearch_core::testutil::{cache_stats, Arm, SeedSpec, BODY_TERM, NEEDLE};
use rusqlite::Connection;

/// Newest-to-oldest is deliberate: `Off` is the candidate, `Sha512` the
/// incumbent, and reporting the candidate first makes the table read as a
/// comparison against what ships rather than a sweep with no thesis.
const MODES: [HmacMode; 3] = [HmacMode::Off, HmacMode::Sha256, HmacMode::Sha512];

/// The shape tier — all four arms, cheap enough to run every time.
const SHAPE_FILES: usize = 200_000;
/// The confirmation tier, where the working set stops fitting the OS cache and
/// real reads enter. `QSB_HMAC_SHAPE_ONLY=1` skips it.
const SCALE_FILES: usize = 1_000_000;

const CONTENT_EVERY: usize = 8;

/// Commit in slices, as a production run does: each commit flushes an FTS5
/// segment, so a single enormous transaction would not resemble one — and the
/// write path is half of what this bench is for.
const COMMIT_EVERY: usize = 5_000;

/// Best-of-N. The minimum is the run least disturbed by whatever else is on
/// the box, which is the honest figure for a comparison.
const RUNS: u32 = 5;

/// The workloads, in the order they are reported. One common word leads: its
/// posting lists are long, where the rare terms stop at the display limit
/// having touched very little.
const WORKLOADS: [(&str, &str, bool); 6] = [
    ("body (common)", "planning", false),
    ("body (rare)", BODY_TERM, false),
    ("name", NEEDLE, false),
    ("fuzzy", "quartzlte", true),
    ("wildcard", "quart*", false),
    ("regex", "regex:quart[sz]ite", false),
];

fn enabled() -> bool {
    std::env::var("QSB_HMAC").is_ok()
}

fn shape_only() -> bool {
    std::env::var("QSB_HMAC_SHAPE_ONLY").is_ok()
}

fn spec(files: usize, hmac: Option<HmacMode>) -> SeedSpec {
    SeedSpec {
        files,
        content_every: CONTENT_EVERY,
        dup_every: 5,
        commit_every: COMMIT_EVERY,
        hmac,
        ..SeedSpec::default()
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// One arm's identity: `None` is the plain floor, `Some` a keyed mode.
fn arms(tag: &str) -> Vec<(String, String, bool, Option<HmacMode>)> {
    let mut out = vec![(
        "plain (no password)".to_string(),
        format!("{}-plain", tag),
        false,
        None,
    )];
    for mode in MODES {
        out.push((
            format!("keyed, HMAC {}", mode.label()),
            format!("{}-{}", tag, mode.label()),
            true,
            Some(mode),
        ));
    }
    out
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_HMAC=1 to run");
        return;
    }
    if std::env::var_os("TMPDIR").is_none() {
        eprintln!(
            "warning: TMPDIR unset — scratch goes to {}. If that is tmpfs, \
             every 'cold' figure below is RAM and the scale tier may not fit.",
            std::env::temp_dir().display()
        );
    }

    tier(SHAPE_FILES, "shape");
    if shape_only() {
        println!("\n(QSB_HMAC_SHAPE_ONLY set — skipping the scale tier)");
        return;
    }
    tier(SCALE_FILES, "scale");
}

/// Every arm at one corpus size, seeded and dropped one at a time so only one
/// index is resident.
fn tier(files: usize, tag: &str) {
    println!(
        "\n######## {} tier: {} files, {} with content ########",
        tag,
        files,
        files / CONTENT_EVERY
    );
    let mut summary: Vec<(String, f64, f64, f64, u64)> = Vec::new();
    for (what, suffix, keyed, hmac) in arms(tag) {
        let arm = Arm::seed(&what, &suffix, keyed, &spec(files, hmac));
        let (warm_total, dup) = report(&arm);
        summary.push((
            what,
            arm.seeded_in.as_secs_f64(),
            warm_total,
            dup.as_secs_f64(),
            arm.size_bytes(),
        ));
        arm.discard();
    }

    // The whole bench in one table, because the per-arm blocks above are too
    // far apart on a terminal to compare by eye.
    println!("\n---- {} tier summary ----", tag);
    println!(
        "{:<24}{:>12}{:>12}{:>12}{:>12}",
        "arm", "seed", "warm total", "duplicates", "size"
    );
    let baseline = summary.first().map(|s| (s.1, s.2, s.3)).unwrap_or_default();
    for (what, seeded, warm_total, dup, size) in &summary {
        println!(
            "{:<24}{:>12}{:>12}{:>12}{:>12}",
            what,
            format!("{:.1} s", seeded),
            format!("{:.1} ms", warm_total * 1000.0),
            format!("{:.0} ms", dup * 1000.0),
            format!("{:.1} MiB", mib(*size)),
        );
    }
    println!(
        "\n{:<24}{:>12}{:>12}{:>12}",
        "over plain", "seed", "warm total", "duplicates"
    );
    for (what, seeded, warm_total, dup, _) in &summary {
        println!(
            "{:<24}{:>12}{:>12}{:>12}",
            what,
            format!("{:.2}x", seeded / baseline.0),
            format!("{:.2}x", warm_total / baseline.1),
            format!("{:.2}x", dup / baseline.2),
        );
    }
}

/// Everything measured about one arm. Returns `(warm query total, duplicate
/// scan)` — the two numbers the summary ranks on.
fn report(arm: &Arm) -> (f64, Duration) {
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
    assert_eq!(
        overflow, 0,
        "{}: FTS5 leaves overflowed, so this arm is measuring a broken \
         derivation rather than its authenticator",
        arm.what
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
    drop(conn);

    (warm_total, duplicates(arm))
}

/// `find_duplicate_groups` is the read shape with the most pages per unit of
/// answer — a full `idx_files_hash` scan — so it is where a per-page cost
/// shows up most plainly. It opens its own connection, so the process key and
/// profile have to be installed at *call* time.
fn duplicates(arm: &Arm) -> Duration {
    let db_path = arm.path.to_string_lossy().into_owned();
    arm.with_key(|| {
        let mut best = Duration::MAX;
        for _ in 0..RUNS {
            let start = Instant::now();
            let groups = find_duplicate_groups(&db_path, 200).expect("duplicate scan");
            assert!(!groups.is_empty(), "the seed must contain duplicate groups");
            best = best.min(start.elapsed());
        }
        best
    })
}

fn seeded_rows(arm: &Arm) -> i64 {
    let conn = arm.open_search();
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0)
}

/// **Where the page fetches go**, so a difference between arms lands on the
/// table that caused it. The shapes are the ones `search/cascade/passes.rs`
/// issues; see `benches/page_geometry.rs`, which uses the same four.
fn attribution(arm: &Arm) {
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
        // A fresh connection per shape: the miss count is only meaningful from
        // an empty cache.
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
/// starts empty and every page the query wants is a miss — the regime where a
/// per-page authenticator is paid rather than skipped.
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
