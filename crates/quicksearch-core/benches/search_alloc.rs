//! `search_perf` answers "how long does a keystroke take"; this answers
//! "how much does it allocate to get there". Only Rust-side allocations are
//! counted — SQLite mallocs directly and is invisible, which leaves
//! precisely the cascade's own churn.
//!
//! Reading the numbers: **allocs** proportional to rows scanned is per-row
//! work, proportional to hits is fixed; **bytes** is churn; **peak live** is
//! footprint. Timings are orientation only — the counting hook is not free;
//! `search_perf` decides wall-clock.
//!
//! Gated by `QSB_SEARCH_ALLOC`:
//!
//! ```text
//! QSB_SEARCH_ALLOC=1 cargo bench -p quicksearch-core --bench search_alloc
//! ```

use std::alloc::{GlobalAlloc, Layout};

// What `Counting` wraps: the allocator the shipped binaries install, or the
// figures describe a build nobody runs. See `platform::Allocator`.
use quicksearch_core::platform::Allocator as Inner;
use std::cell::Cell;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

use quicksearch_core::testutil::{scratch_db, seed_index, SeedSpec};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

// Counters are per thread — load-bearing: libtest runs tests concurrently
// in one process, and with global counters another test's allocations land
// in whatever region is open here (the self-test caught exactly that).
// Since `cascade::run` is synchronous, "this thread" *is* "the cascade".
//
// `const`-initialized `Cell`s: a lazy initializer would allocate from
// inside the allocator, and a destructor can panic during thread teardown —
// exactly when the last deallocations happen.
// `LIVE` and `PEAK` are **signed**: cross-thread frees drive a balance
// legitimately negative; held unsigned it reads ~1.8e19 and `PEAK.max`
// latches there forever. Counts are `u64` (they only rise), balances `i64`.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static REALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

#[inline]
fn get(counter: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
    counter.try_with(Cell::get).unwrap_or(0)
}

#[inline]
fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>, by: u64) -> u64 {
    counter
        .try_with(|c| {
            let v = c.get().wrapping_add(by);
            c.set(v);
            v
        })
        .unwrap_or(0)
}

#[inline]
fn get_live(counter: &'static std::thread::LocalKey<Cell<i64>>) -> i64 {
    counter.try_with(Cell::get).unwrap_or(0)
}

#[inline]
fn bump_live(by: i64) -> i64 {
    LIVE.try_with(|c| {
        let v = c.get().wrapping_add(by);
        c.set(v);
        v
    })
    .unwrap_or(0)
}

#[inline]
fn note_peak(live: i64) {
    PEAK.try_with(|p| p.set(p.get().max(live))).ok();
}

/// [`Inner`], with counters; a failed allocation is not counted, so the
/// totals describe memory that really existed.
struct Counting;

#[inline]
fn note_alloc(size: usize) {
    bump(&ALLOCS, 1);
    bump(&BYTES, size as u64);
    note_peak(bump_live(size as i64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { Inner.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { Inner.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    /// Cross-thread frees drive this negative; see the `thread_local!` note.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump_live(-(layout.size() as i64));
        unsafe { Inner.dealloc(ptr, layout) }
    }

    /// Counted as a resize: a doubling `Vec` is one buffer, not twelve — the
    /// difference this harness exists to show. Only growth adds to traffic.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { Inner.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            bump(&REALLOCS, 1);
            let (old, new) = (layout.size() as u64, new_size as u64);
            bump(&BYTES, new.saturating_sub(old));
            note_peak(bump_live(new as i64 - old as i64));
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Clone, Copy)]
struct Counters {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
    /// Signed; see the `thread_local!` note.
    live: i64,
}

#[derive(Clone, Copy)]
struct Usage {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
    peak: u64,
}

impl Counters {
    /// Snapshot and re-arm the peak tracker at the current live figure, so
    /// the following high-water mark belongs to the measured region.
    fn start() -> Counters {
        let live = get_live(&LIVE);
        PEAK.with(|p| p.set(live));
        Counters {
            allocs: get(&ALLOCS),
            reallocs: get(&REALLOCS),
            bytes: get(&BYTES),
            live,
        }
    }

    fn since(&self) -> Usage {
        Usage {
            allocs: get(&ALLOCS).wrapping_sub(self.allocs),
            reallocs: get(&REALLOCS).wrapping_sub(self.reallocs),
            bytes: get(&BYTES).wrapping_sub(self.bytes),
            // Above the region's starting live figure, so an empty query reads zero.
            peak: (get_live(&PEAK) - self.live).max(0) as u64,
        }
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// Smaller than `search_perf`'s 200k: allocation counts are deterministic
/// and linear in rows, so they need no statistical settling.
const NUM_FILES: usize = 50_000;

// No serializing mutex, deliberately: counters are per thread, so tests
// cannot reach each other's regions (an earlier locking revision failed).

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_ALLOC").is_ok()
}

struct Case {
    label: &'static str,
    query: &'static str,
    fuzzy: bool,
    passes: &'static str,
}

/// Every case but the last is **rare on purpose**: a query that fills the
/// display limit measures how quickly the cascade gives up, not what a scan
/// costs. Spread across the passes so a change helping one and hurting
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
        passes: "A whole table, no prefilter today (S3)",
    },
    Case {
        label: "fuzzy, rare",
        // One edit from the needle: exact passes miss, fuzzy ones do the work.
        query: "quartzyte",
        fuzzy: true,
        passes: "A + B + C + D, C and D whole-table (S1/S2/S4)",
    },
    Case {
        // The full-text pass with real work: the name LIKE finds nothing, so
        // every hit is a trigram candidate pass B decompressed and verified.
        label: "content, many",
        query: "chalcedony",
        fuzzy: false,
        passes: "A finds nothing + B verifies ~500 docs (S5/S7)",
    },
    Case {
        label: "regex, literal",
        query: r"regex:quartz\w+",
        fuzzy: false,
        passes: "regex name + content, both prefiltered on \"quartz\"",
    },
    Case {
        // No literal to extract, so both regex passes still read everything —
        // the prefilter's *limit*, beside its win above.
        label: "regex, no literal",
        query: r"regex:[0-9]{6}[a-y]{6}",
        fuzzy: false,
        passes: "regex name + content, no prefilter possible",
    },
    Case {
        // The accept-predicate: a common term beside a regex whose path
        // check misses everything, so every pass-A candidate fetches and
        // decodes its stored body (or discovers its absence) through
        // `Cx::regex_accepts` — per row, the shape `DocDecoder` exists for.
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
/// are counted, not kept — keeping them would measure the harness's `Vec` —
/// and reported so a changed result set shows up here too.
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
    // The accounting is verified before anything is printed: a harness that
    // silently stopped counting would read as a spectacular optimization.
    the_counters_track_real_allocations();
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_ALLOC=1 to run");
        return;
    }
    allocation_traffic_per_query();
}

/// Printed rather than asserted: a threshold on a shared machine gets muted.
/// Allocation counts are stable enough that a ratchet becomes reasonable
/// once the numbers settle; until then this exists to be read.
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

    // One connection for the run, as the worker holds one; opened through
    // the real entry point so the pragma profile is production's.
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
         that is per-row work;\nafter S1/S2 the scan passes should sit near their \
         hit counts instead.",
        NUM_FILES
    );
}

/// The accounting itself, verified unconditionally at startup.
fn the_counters_track_real_allocations() {
    let start = Counters::start();
    let mut v: Vec<u8> = Vec::new();
    // `push` in a loop on purpose: the growth is what is being measured, and
    // the `resize`/`vec![0; n]` clippy asks for would allocate once with no
    // reallocs at all, so the assertion below could never fail.
    #[allow(clippy::same_item_push)]
    for _ in 0..64 * 1024 {
        v.push(0);
    }
    let grown = start.since();
    assert_eq!(grown.allocs, 1, "a doubling Vec is one allocation");
    assert!(grown.reallocs > 0, "and several resizes");
    assert!(
        grown.peak >= 64 * 1024,
        "peak {} should cover the grown buffer",
        grown.peak
    );

    // Dropping returns the bytes, so a later region's peak is not inflated.
    let before_drop = get_live(&LIVE);
    drop(v);
    assert!(
        get_live(&LIVE) < before_drop,
        "dealloc must decrement live bytes"
    );

    // The signedness the scheme turns on: sink the balance below zero, as a
    // cross-thread free really does, and a peak must still be reported —
    // unsigned, `max` latches on ~1.8e19 forever.
    bump_live(-(1 << 20));
    let negative = Counters::start();
    assert!(negative.live < 0, "the balance is genuinely negative");
    let mut grow: Vec<u8> = Vec::with_capacity(32 * 1024);
    grow.push(1);
    let seen = negative.since().peak;
    drop(grow);
    assert!(
        seen >= 32 * 1024,
        "a negative live balance swallowed the peak: {}",
        seen
    );
    bump_live(1 << 20); // put back what was sunk, so later regions start clean

    // Another thread allocating hard must not touch this thread's counters.
    // Spawn and join sit *outside* the region: `spawn` boxes its closure on
    // the calling thread and `join` frees it there; a barrier hands control
    // across without allocating.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let child = {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait(); // the region is open
            let noisy: Vec<String> = (0..10_000).map(|i| format!("allocation {}", i)).collect();
            std::hint::black_box(noisy.len());
            barrier.wait(); // the noise is done
        })
    };
    let quiet_across_threads = Counters::start();
    barrier.wait();
    barrier.wait();
    let leaked = quiet_across_threads.since();
    child.join().expect("the noisy thread finishes");
    assert_eq!(
        (leaked.allocs, leaked.bytes),
        (0, 0),
        "another thread's allocations must not be charged to this region"
    );

    // A region that allocates nothing reports nothing.
    let quiet = Counters::start();
    std::hint::black_box(1u64 + 1);
    let idle = quiet.since();
    assert_eq!((idle.allocs, idle.bytes, idle.peak), (0, 0, 0));
}
