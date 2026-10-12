//! Query latency on an existing index, the counterweight to `indexprobe`:
//! every FTS write-side knob buys indexing time by leaving more segments
//! behind, and each segment is a b-tree a query visits.
//!
//! ```text
//! cargo build -p quicksearch-core --example searchtime --release
//! ./target/release/examples/searchtime /path/to/index.db
//! ./target/release/examples/searchtime /path/to/index.db serialize "fn main" ~recieve
//! ```
//!
//! Queries after the path replace the built-in set, for a corpus whose
//! vocabulary the defaults do not hit; a leading `~` runs one fuzzy.
//!
//! Queries run against one held connection, as the worker holds one; each
//! is timed best-of-N so a scheduling hiccup is not the headline.

// Timed on the allocator the shipped binaries install; see
// `platform::Allocator`.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

mod common;

use std::path::PathBuf;
use std::time::Duration;

use quicksearch_core::search::SearchOptions;
use quicksearch_core::testutil;

const RUNS: u32 = 5;

/// The content queries go through however many segments the write side
/// left behind; the filename query is the control and must not move. The
/// filter rows price `modified:` and `mime:` against that control: neither
/// column has an index, so they must cost no more than the scan it already is.
const QUERIES: &[(&str, bool, &str)] = &[
    ("filename (control)", false, "doc42"),
    ("filename + modified", false, "doc42 modified:>=2025-06-01"),
    ("filename + one day", false, "doc42 modified:2025-06-01"),
    ("filename + mime", false, "doc42 mime:application/pdf"),
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
    // `QSB_CACHE_KIB` overrides the cache ceiling, to tell a working set too
    // big for the cache from an index too big.
    if let Ok(kib) = std::env::var("QSB_CACHE_KIB") {
        conn.execute_batch(&format!("PRAGMA cache_size = -{};", kib.trim()))
            .expect("set cache_size");
    }

    let segments = common::count(&conn, "SELECT COUNT(*) FROM searchabletext_idx");
    let rows = common::count(&conn, "SELECT COUNT(*) FROM files");
    println!(
        "{}  ({} rows, {} segment-index entries)",
        db.display(),
        rows,
        segments
    );
    println!("{:<22} {:>10} {:>8}", "query", "best", "hits");

    let custom: Vec<(String, bool, String)> = std::env::args()
        .skip(2)
        .map(|q| match q.strip_prefix('~') {
            Some(term) => (q.clone(), true, term.to_string()),
            None => (q.clone(), false, q),
        })
        .collect();
    let queries: Vec<(&str, bool, &str)> = if custom.is_empty() {
        QUERIES.to_vec()
    } else {
        custom
            .iter()
            .map(|(label, fuzzy, query)| (label.as_str(), *fuzzy, query.as_str()))
            .collect()
    };

    let mut total = Duration::ZERO;
    for &(label, fuzzy, query) in &queries {
        // The display limit makes this unfair: `scan_pass` stops when the
        // limit fills, and candidates stream in insertion order, so which
        // documents drew low ids changes bytes read 13x. A `QSB_LIMIT` past
        // the corpus makes every index do equal work.
        let limit = std::env::var("QSB_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let options = SearchOptions {
            fuzzy,
            limit,
            ..SearchOptions::default()
        };
        let mut best = Duration::MAX;
        let mut hits = 0usize;
        // Time varying while `rchar` does not means waiting on the disk.
        let io_before = common::Io::read();
        for _ in 0..RUNS {
            let (took, count) = testutil::run_cascade(&conn, query, &options);
            best = best.min(took);
            hits = count;
        }
        total += best;
        let io = common::Io::read().since(&io_before);
        println!(
            "{:<22} {:>10.1?} {:>8}   disk-read {:>8.1} MiB   rchar {:>8.1} MiB  (over {} runs)",
            label,
            best,
            hits,
            io.read_bytes as f64 / 1048576.0,
            io.rchar as f64 / 1048576.0,
            RUNS,
        );
    }
    println!("{:<22} {:>10.1?}", "TOTAL", total);
}
