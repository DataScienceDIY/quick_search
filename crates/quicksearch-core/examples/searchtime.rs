//! Search latency against an index that already exists on disk.
//!
//! The counterweight to `indexprobe`. Every FTS write-side knob — `automerge`,
//! a final `'optimize'`, `pgsz` — buys indexing time by leaving more segments
//! behind, and a segment is a b-tree a query has to visit. Halving a cold index
//! while doubling a keystroke is a regression wearing an improvement's clothes,
//! and this is what says which one happened.
//!
//! ```text
//! cargo build -p quicksearch-core --example searchtime --release
//! ./target/release/examples/searchtime /path/to/index.db
//! ```
//!
//! Queries are run against one held connection, as the search worker holds one
//! across a typing session, and each is timed best-of-N so a single scheduling
//! hiccup does not become the headline.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

/// `(read_bytes, rchar)` from `/proc/self/io`: what reached the block layer,
/// and what passed through the read syscalls. A query whose time varies while
/// `rchar` does not is not doing more work — it is waiting on the disk.
fn proc_io() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let field = |key: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.trim().trim_start_matches(':').trim().parse().ok())
            .unwrap_or(0)
    };
    (field("read_bytes"), field("rchar"))
}

/// Runs per query; the best is reported.
const RUNS: u32 = 5;

/// The query set, chosen to reach the passes an FTS setting can affect.
///
/// The content queries are the point — they are the ones that go through
/// `searchabletext` and therefore through however many segments the write side
/// left behind. The filename query is the control: it never touches FTS, so it
/// must not move.
const QUERIES: &[(&str, bool, &str)] = &[
    ("filename (control)", false, "doc42"),
    ("content, common", false, "mountain"),
    ("content, rare", false, "quartzite"),
    ("content, two words", false, "ocean forest"),
    ("fuzzy content", true, "mountian"),
];

fn main() {
    let db = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: searchtime <index.db>"),
    );
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the index");
    // Override the profile's ceiling, to test whether a slow index is slow
    // because its working set does not fit rather than because it is bigger.
    if let Ok(kib) = std::env::var("QSB_CACHE_KIB") {
        conn.execute_batch(&format!("PRAGMA cache_size = -{};", kib.trim()))
            .expect("set cache_size");
    }

    let segments: i64 = conn
        .query_row("SELECT COUNT(*) FROM searchabletext_idx", [], |r| r.get(0))
        .unwrap_or(-1);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(-1);
    println!(
        "{}  ({} rows, {} segment-index entries)",
        db.display(),
        rows,
        segments
    );
    println!("{:<22} {:>10} {:>8}", "query", "best", "hits");

    let mut total = Duration::ZERO;
    for (label, fuzzy, query) in QUERIES {
        let split = split_for_cascade(query).expect("query parses");
        // The display limit makes this benchmark unfair between indexes.
        // `scan_pass` stops as soon as the limit is full, and it streams FTS
        // candidates in rowid order — which is `file_id` order, which is the
        // order the *walk* happened to insert rows. So an index where the large
        // documents drew low ids decompresses megabytes to fill 1000 hits while
        // one where the small documents did reads a few hundred kilobytes, and
        // the two differ by 13x for reasons that have nothing to do with what is
        // being compared. Measured: 2.4 MiB against 110.9 MiB of `rchar` for the
        // same query and the same hit count.
        //
        // Raising the limit past the corpus makes every index examine every
        // candidate, which is the only way two of them are doing equal work.
        let limit = std::env::var("QSB_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let options = SearchOptions {
            fuzzy: *fuzzy,
            limit,
            ..SearchOptions::default()
        };
        let mut best = Duration::MAX;
        let mut hits = 0usize;
        let io_before = proc_io();
        for _ in 0..RUNS {
            let latest = AtomicU64::new(1);
            let mut count = 0usize;
            let mut sink = |h: Vec<SearchHit>| count += h.len();
            let start = Instant::now();
            cascade::run(&conn, &split, &options, 1, &latest, &mut sink).expect("cascade runs");
            best = best.min(start.elapsed());
            hits = count;
        }
        total += best;
        let (rd, rc) = proc_io();
        let (rd0, rc0) = io_before;
        println!(
            "{:<22} {:>10.1?} {:>8}   disk-read {:>8.1} MiB   rchar {:>8.1} MiB  (over {} runs)",
            label,
            best,
            hits,
            (rd - rd0) as f64 / 1048576.0,
            (rc - rc0) as f64 / 1048576.0,
            RUNS,
        );
    }
    println!("{:<22} {:>10.1?}", "TOTAL", total);
}
