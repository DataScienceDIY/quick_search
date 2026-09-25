//! What one search allocates, per cascade shape; `search_perf` measures how
//! long it takes. The counting allocator (`alloc`) sees Rust-side allocations
//! only, which leaves the cascade's own churn.
//!
//! Reading the numbers: `allocs` proportional to rows scanned is per-row
//! work, proportional to hits is fixed; `bytes` is churn; `peak` is
//! footprint. Timings are orientation only, because the counting hook is not
//! free; `search_perf` decides wall-clock.
//!
//! Gated by `QSB_SEARCH_ALLOC`:
//!
//! ```text
//! QSB_SEARCH_ALLOC=1 cargo bench -p quicksearch-core --bench search_alloc
//! ```

mod alloc;

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use alloc::{Counters, Usage};
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::testutil::{scratch_db, seed_index, SeedSpec};

/// Smaller than `search_perf`'s 200k: allocation counts are deterministic
/// and linear in rows, so they need no statistical settling.
const NUM_FILES: usize = 50_000;

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_ALLOC").is_ok()
}

struct Case {
    label: &'static str,
    query: &'static str,
    fuzzy: bool,
    passes: &'static str,
}

/// Every case but the last is rare: a query that fills the display limit
/// measures how quickly the cascade gives up, not what a scan costs. The
/// cases spread across the passes, so a change helping one and hurting
/// another cannot hide inside a single total.
const CASES: &[Case] = &[
    Case {
        label: "no match",
        query: "zzzznomatch",
        fuzzy: false,
        passes: "A, whole table, nothing accepted",
    },
    Case {
        label: "literal, rare",
        query: "quartzite",
        fuzzy: false,
        passes: "A whole table + B verifies FTS hits",
    },
    Case {
        label: "wildcard, rare",
        query: "quar*zite",
        fuzzy: false,
        passes: "A whole table, LIKE-prefiltered on one segment",
    },
    Case {
        label: "fuzzy, rare",
        // One edit from the needle: exact passes miss, fuzzy ones do the work.
        query: "quartzyte",
        fuzzy: true,
        passes: "A + B + C + D, C and D prefiltered (9+ chars)",
    },
    Case {
        // The full-text pass with real work: the name LIKE finds nothing, so
        // every hit is a trigram candidate pass B decompressed and verified.
        label: "content, many",
        query: "chalcedony",
        fuzzy: false,
        passes: "A finds nothing + B verifies ~500 docs",
    },
    Case {
        label: "regex, literal",
        query: r"regex:quartz\w+",
        fuzzy: false,
        passes: "regex name + content, both prefiltered on \"quartz\"",
    },
    Case {
        // No literal to extract, so both regex passes read everything: the
        // prefilter's limit, beside its win above.
        label: "regex, no literal",
        query: r"regex:[0-9]{6}[a-y]{6}",
        fuzzy: false,
        passes: "regex name + content, no prefilter possible",
    },
    Case {
        // The accept-predicate: a common term beside a regex whose path
        // check misses everything, so every pass-A candidate fetches and
        // decodes its stored body (or finds none) through
        // `Cx::regex_accepts`. That per-row decode is what `DocDecoder` is
        // for.
        label: "term + regex",
        query: r"content regex:zzznever\d",
        fuzzy: false,
        passes: "A hits many, regex accept-predicate decodes per candidate",
    },
    Case {
        label: "common (capped)",
        query: "content",
        fuzzy: false,
        passes: "A, stops at the display limit",
    },
];

/// Run one query on a held connection, reporting allocator movement. Hits
/// are counted, not kept, since keeping them would measure the harness's
/// `Vec`; the count is reported so a changed result set shows up here too.
fn measure(conn: &rusqlite::Connection, case: &Case) -> (Usage, usize, Duration) {
    let split = split_for_cascade(case.query).expect("the query set parses");
    let latest = AtomicU64::new(1);
    let options = SearchOptions {
        fuzzy: case.fuzzy,
        limit: 1000,
        ..SearchOptions::default()
    };

    let mut count = 0usize;
    let mut sink = |hits: Vec<SearchHit>| count += hits.len();

    // The snapshot brackets the cascade and nothing else.
    let start = Counters::start();
    let clock = Instant::now();
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("the cascade runs");
    let elapsed = clock.elapsed();
    (start.since(), count, elapsed)
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn main() {
    alloc::the_counters_track_real_allocations();
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_ALLOC=1 to run");
        return;
    }
    allocation_traffic_per_query();
}

/// Allocation traffic per [`CASES`] entry, printed rather than asserted: a
/// threshold on a shared machine gets muted. The counts are stable enough
/// for a ratchet once the numbers settle.
fn allocation_traffic_per_query() {
    let db = scratch_db("searchalloc");
    let seeded = Instant::now();
    seed_index(
        &db,
        &SeedSpec {
            files: NUM_FILES,
            ..SeedSpec::default()
        },
    );
    println!(
        "seeded {} rows in {:.1?} ({} MiB on disk)\n",
        NUM_FILES,
        seeded.elapsed(),
        std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0) / (1024 * 1024)
    );

    // One connection for the run, as the worker holds one, opened through
    // the production entry point for its pragma profile.
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the seeded index");

    println!(
        "{:<16} {:>12} {:>10} {:>12} {:>12} {:>7} {:>9}   passes",
        "case", "allocs", "reallocs", "bytes (MiB)", "peak (MiB)", "hits", "time"
    );
    for case in CASES {
        // Warm once, then measure: a cold first query would report SQLite's
        // one-off setup as the cascade's churn.
        let _ = measure(&conn, case);
        let (usage, hits, elapsed) = measure(&conn, case);
        println!(
            "{:<16} {:>12} {:>10} {:>12} {:>12} {:>7} {:>9.1?}   {}",
            case.label,
            usage.allocs,
            usage.reallocs,
            mib(usage.bytes),
            mib(usage.peak),
            hits,
            elapsed,
            case.passes,
        );
    }

    println!(
        "\n{} rows scanned per whole-table pass. An `allocs` figure at or above \
         that is per-row work;\na prefiltered pass should sit near its hit count.",
        NUM_FILES
    );
}
