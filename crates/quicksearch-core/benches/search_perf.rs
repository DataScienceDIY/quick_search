//! What a warm page cache is worth to search, and how big it has to be:
//! warm beats cold, and 8 MiB ([`PRAGMAS_SEARCH`]) is enough — the two
//! claims holding one connection across requests rests on.
//!
//! Queries run as a keystroke sequence (`q`, `qu`, `qui`, `quic`); the
//! second and later queries are the number that matters.
//!
//! The encrypted column is where a smaller cache hurts first: a page-cache
//! miss costs an AES decrypt plus an HMAC verify, not a `memcpy`.
//!
//! Printed rather than asserted: shared-box timings are not stable enough
//! for a pass/fail gate, and a flaky perf gate gets muted rather than
//! fixed. Gated by `QSB_SEARCH_PERF`:
//!
//! ```text
//! QSB_SEARCH_PERF=1 cargo bench -p quicksearch-core --bench search_perf
//! ```

use std::time::{Duration, Instant};

use quicksearch_core::db::set_process_key;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::security::IndexKey;
use quicksearch_core::testutil::{scratch_db, seed_index, SeedSpec};
use rusqlite::Connection;

/// Large enough that the b-tree has interior levels and FTS several
/// segments — below that everything fits in any cache and says nothing.
const NUM_FILES: usize = 200_000;

/// Cache ceilings: `-40960` is what every read connection used to take,
/// `-8192` is `PRAGMAS_SEARCH`, and `-1024` is deliberately too small — the
/// curve needs a visible floor for "8 MiB is enough" to be a measurement.
const CACHE_SIZES: [i64; 6] = [-40960, -32768, -16384, -8192, -4096, -1024];

const SEQUENCE: [&str; 4] = ["quar", "quart", "quartz", "quartzi"];

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_PERF").is_ok()
}

fn seed(path: &std::path::Path) {
    seed_index(
        path,
        &SeedSpec {
            files: NUM_FILES,
            body_words: 60,
            ..SeedSpec::default()
        },
    );
}

/// Run one query; hits are counted, not kept — holding 200k `SearchHit`s
/// would measure the allocator instead of the scan.
fn time_query(conn: &Connection, query: &str) -> (Duration, usize) {
    let split = split_for_cascade(query).unwrap();
    let latest = std::sync::atomic::AtomicU64::new(1);
    let mut count = 0usize;
    let mut sink = |hits: Vec<SearchHit>| count += hits.len();
    let options = SearchOptions {
        limit: 1000,
        ..SearchOptions::default()
    };
    let start = Instant::now();
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).unwrap();
    (start.elapsed(), count)
}

/// Open at an explicit cache ceiling — `open_search_reader` deliberately
/// does not expose one, and comparing ceilings is the whole point.
fn open_at(path: &std::path::Path, cache_size: i64) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = {};
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
        cache_size
    ))
    .unwrap();
    conn
}

fn run_matrix(label: &str, path: &std::path::Path) {
    println!("\n=== {} ===", label);
    println!(
        "{:>12}  {:>10}  {:>10}  {:>10}  {:>8}",
        "cache_size", "cold", "warm avg", "warm best", "hits"
    );
    for cache_size in CACHE_SIZES {
        let conn = open_at(path, cache_size);
        let (cold, hits) = time_query(&conn, SEQUENCE[0]);
        let mut warm = Vec::new();
        for query in &SEQUENCE[1..] {
            warm.push(time_query(&conn, query).0);
        }
        let avg = warm.iter().sum::<Duration>() / warm.len() as u32;
        let best = warm.iter().min().copied().unwrap_or_default();
        println!(
            "{:>12}  {:>9.1?}  {:>9.1?}  {:>9.1?}  {:>8}",
            cache_size, cold, avg, best, hits
        );
    }
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_PERF=1 to run");
        return;
    }
    unencrypted();
    encrypted();
}

/// The headline comparison, to be *read* when [`PRAGMAS_SEARCH`] is questioned.
fn unencrypted() {
    let plain = scratch_db("searchperf-plain");
    let seeded = Instant::now();
    seed(&plain);
    println!(
        "seeded {} rows in {:.1?} ({} MiB on disk)",
        NUM_FILES,
        seeded.elapsed(),
        std::fs::metadata(&plain).map(|m| m.len()).unwrap_or(0) / (1024 * 1024)
    );
    run_matrix("unencrypted", &plain);
}

/// Runs after [`unencrypted`]: [`set_process_key`] is process-global, so the
/// plain index must be opened before any key is set.
fn encrypted() {
    set_process_key(Some(
        IndexKey::from_hex(&"42".repeat(32)).expect("valid 32-byte key"),
    ));
    let enc = scratch_db("searchperf-enc");
    seed(&enc);

    println!("\n(encrypted: every cache miss costs an AES-CBC + HMAC-SHA512 per page)");
    // Both orders: a difference that survives reversing them is a property
    // of the ceiling, not of when it was measured.
    let mut order: Vec<i64> = CACHE_SIZES.to_vec();
    order.extend(CACHE_SIZES.iter().rev());
    for cache_size in order {
        let conn = quicksearch_core::db::open_existing(&enc.to_string_lossy(), false).unwrap();
        conn.execute_batch(&format!("PRAGMA cache_size = {};", cache_size))
            .unwrap();
        let (cold, hits) = time_query(&conn, SEQUENCE[0]);
        let mut warm = Vec::new();
        for query in &SEQUENCE[1..] {
            warm.push(time_query(&conn, query).0);
        }
        let avg = warm.iter().sum::<Duration>() / warm.len() as u32;
        println!(
            "{:>12}  cold {:>9.1?}  warm avg {:>9.1?}  hits {}",
            cache_size, cold, avg, hits
        );
    }
    set_process_key(None);
}
