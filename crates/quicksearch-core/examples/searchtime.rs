//! The counterweight to `indexprobe`: every FTS write-side knob buys
//! indexing time by leaving more segments behind, and a segment is a
//! b-tree a query has to visit.
//!
//! ```text
//! cargo build -p quicksearch-core --example searchtime --release
//! ./target/release/examples/searchtime /path/to/index.db
//! ```
//!
//! Queries run against one held connection, as the worker holds one; each
//! is timed best-of-N so a scheduling hiccup is not the headline.

mod common;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

const RUNS: u32 = 5;

/// The content queries go through however many segments the write side
/// left behind; the filename query is the control and must not move.
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
    // Override the cache ceiling: working set too big, or index too big?
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
        // The display limit makes this unfair: `scan_pass` stops when the
        // limit fills, and candidates stream in insertion order, so which
        // documents drew low ids changes bytes read 13x. A limit past the
        // corpus makes every index do equal work.
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
        // Time varying while `rchar` does not means waiting on the disk.
        let io_before = common::Io::read();
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
