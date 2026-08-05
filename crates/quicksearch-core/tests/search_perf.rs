//! What a warm page cache is worth to search, and how big it has to be.
//!
//! The search worker holds one connection across requests (see
//! [`quicksearch_core::search`]) precisely so that a typing session runs
//! against a cache that is already warm. This measures the two things that
//! claim rests on:
//!
//! 1. **Warm beats cold**, by enough to justify holding the connection at all.
//! 2. **8 MiB is enough** — the size [`PRAGMAS_SEARCH`] settles on. The hot set
//!    across queries is the `files` b-tree interior nodes and the tips of the
//!    FTS5 segments, not the table, so past some point a larger ceiling buys
//!    nothing and only raises what an idle process is holding.
//!
//! Queries are run as a *sequence* — `q`, `qu`, `qui`, `quic` — because that is
//! what a search-per-keystroke frontend actually does. The first is the
//! outlier; the second and later ones are the number that matters.
//!
//! The encrypted column is the one to watch. Under SQLCipher a page cache miss
//! costs an AES-CBC decrypt plus an HMAC-SHA512 verify per 4 KiB page rather
//! than a `memcpy`, so if a smaller cache is going to hurt anywhere it is here.
//!
//! Gated by `QSB_SEARCH_PERF` so the harness doesn't pay the seed cost on every
//! `cargo test`. To run it:
//!
//! ```text
//! QSB_SEARCH_PERF=1 cargo test --release -p quicksearch-core \
//!     --test search_perf -- --nocapture
//! ```

use std::time::{Duration, Instant};

use quicksearch_core::db::repo::{insert_file, set_content_done, NewFile};
use quicksearch_core::db::{open_or_recreate, set_process_key};
use quicksearch_core::mime::FileType;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::security::IndexKey;
use rusqlite::Connection;

mod common;
use common::scratch_db;

/// Rows to seed. Large enough that the `files` b-tree has real interior levels
/// and the FTS index has more than one segment — below that everything fits in
/// any cache and the comparison says nothing.
const NUM_FILES: usize = 200_000;

/// Cache ceilings to compare, as `PRAGMA cache_size` values in KiB.
///
/// `-40960` is what every read connection used to take, `-8192` is
/// `PRAGMAS_SEARCH`, and `-1024` is deliberately too small — it is there to
/// show the curve has a floor worth being above, so that "8 MiB is enough" is
/// a measurement rather than an assumption.
const CACHE_SIZES: [i64; 6] = [-40960, -32768, -16384, -8192, -4096, -1024];

/// The prefixes of one word, typed one character at a time.
const SEQUENCE: [&str; 4] = ["quar", "quart", "quartz", "quartzi"];

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_PERF").is_ok()
}

/// Deterministic pseudo-random word picker. Same LCG as `indexprobe`, for the
/// same reason: a fixed seed makes two runs comparable.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }
}

const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "quartz", "quartzite", "quarry", "quarter", "quantum", "brown", "fox", "jumps",
    "lazy", "index", "search", "cascade", "snippet", "document", "content", "extract",
];

/// Seed an index with `NUM_FILES` rows, a tenth of them content-indexed.
///
/// Only a tenth so the FTS index stays smaller than the table, which is the
/// real shape — most files in a tree are not text.
fn seed(path: &std::path::Path) {
    let mut conn = open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let mut rng = Lcg(0x5eed);
    let tx = conn.transaction().unwrap();
    for i in 0..NUM_FILES {
        let w1 = WORDS[(rng.next() as usize) % WORDS.len()];
        let w2 = WORDS[(rng.next() as usize) % WORDS.len()];
        let name = format!("{}-{}-{:07}.txt", w1, w2, i);
        let dir = format!("/seed/{:03}", i % 500);
        let full = format!("{}/{}", dir, name);
        let id = insert_file(
            &tx,
            &NewFile {
                name: &name,
                path: &full,
                parent: &dir,
                size: 4096,
                mtime: 1_700_000_000 + i as u64,
                inode: None,
                device_id: None,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: i % 10 == 0,
            },
        )
        .unwrap()
        .expect("unique path");
        if i % 10 == 0 {
            let body: Vec<&str> = (0..60)
                .map(|_| WORDS[(rng.next() as usize) % WORDS.len()])
                .collect();
            set_content_done(&tx, id, &name, &body.join(" "), &[], true).unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
}

/// Run one query to completion, returning how long it took and how many hits
/// it produced. Hits are counted, not kept — the cost being measured is the
/// scan, and holding 200k `SearchHit`s would measure the allocator instead.
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

/// Open a connection at an explicit cache ceiling.
///
/// Spelled out rather than going through `db::open::open_search_reader`
/// because the whole point is to compare ceilings, which that function
/// deliberately does not expose.
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
        // A connection per ceiling, held for the whole sequence — the same
        // lifetime the search worker gives it.
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

/// The headline comparison, printed rather than asserted.
///
/// Deliberately not a pass/fail threshold: timings on a shared CI box are not
/// stable enough for one, and a flaky perf gate gets muted rather than fixed.
/// This exists to be *read* when the number in [`PRAGMAS_SEARCH`] is being
/// chosen or questioned.
#[test]
fn cache_size_against_search_latency() {
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_PERF=1 to run");
        return;
    }

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

/// The same matrix against an encrypted index.
///
/// Separate test, and separate process-wide key, because
/// [`set_process_key`] is global: running both in one test would have the
/// plain index opened with a key set.
#[test]
fn cache_size_against_search_latency_encrypted() {
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_PERF=1 to run");
        return;
    }

    set_process_key(Some(
        IndexKey::from_hex(&"42".repeat(32)).expect("valid 32-byte key"),
    ));
    let enc = scratch_db("searchperf-enc");
    seed(&enc);

    println!("\n(encrypted: every cache miss costs an AES-CBC + HMAC-SHA512 per page)");
    // Both orders. The ceilings are tried largest-first and then
    // smallest-first because the OS page cache warms as the run proceeds, and
    // a difference that survives reversing the order is a property of the
    // ceiling rather than of when it was measured.
    let mut order: Vec<i64> = CACHE_SIZES.to_vec();
    order.extend(CACHE_SIZES.iter().rev());
    // Opened through the keyed path — a raw `Connection::open` cannot read it.
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
