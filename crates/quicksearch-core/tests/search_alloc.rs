//! What a search moves through the allocator.
//!
//! [`search_perf`](search_perf.rs) answers "how long does a keystroke take";
//! this answers "how much does it allocate to get there", which is a different
//! question with a different answer and, until this existed, no way to measure
//! it from the repository at all. `db/repo.rs` already carries one finding of
//! exactly this kind — that `zstd::decode_all` was "27 of the 30 GiB a fuzzy
//! search moved through the allocator" — and that number came from tooling
//! nobody could re-run.
//!
//! # What is counted, and what is not
//!
//! This installs a `#[global_allocator]` that wraps [`System`] and counts every
//! Rust-side allocation. A global allocator is **per binary**, so this affects
//! only this test executable — the shipped `quicksearch` binary is untouched,
//! which is what makes an always-available accounting harness safe to keep.
//!
//! It deliberately does **not** see SQLite. The bundled SQLCipher amalgamation
//! calls libc `malloc` directly rather than going through Rust's `GlobalAlloc`,
//! so its page cache (32 MiB under [`PRAGMAS_SEARCH`]) and its record buffers
//! are invisible here. That is a feature: what is left is precisely the
//! cascade's own churn — the per-row `String`s, the document folds, the snippet
//! buffers — which is the part the code can do something about.
//!
//! # Reading the numbers
//!
//! - **allocs** is the count. It is the number to watch for per-row work: a
//!   pass that scans the whole table and allocates once per row shows up here
//!   as a figure proportional to the row count, and a fix shows up as a figure
//!   proportional to the *hit* count.
//! - **bytes** is total traffic — allocation churn. High traffic with a low
//!   peak means buffers being built and dropped in a loop.
//! - **peak live** is the high-water mark of outstanding bytes during the
//!   query, and the only one of the three that speaks to footprint rather than
//!   to churn.
//!
//! Timings are printed for orientation only. The counting allocator adds a
//! thread-local read-modify-write to every allocation, so this binary is
//! **not** where wall-clock is decided; `search_perf` is.
//!
//! # What it has measured so far
//!
//! 50,000 rows, 5,000 stored documents of ~2 KB, 32 MiB index. Baseline is the
//! cascade as it stood before this round of work; each column after it is
//! cumulative.
//!
//! | case | allocs | → after | bytes | → after | time | → after |
//! |---|---:|---:|---:|---:|---:|---:|
//! | no match | 14 | 14 | 0.0 | 0.0 | 2.5 ms | 2.5 ms |
//! | literal, rare | 226 | 226 | 0.0 | 0.0 | 2.5 ms | 2.6 ms |
//! | wildcard, rare | 50,324 | **348** | 1.2 MiB | 0.0 | 11.5 ms | **2.6 ms** |
//! | fuzzy, rare | 150,134 | **242** | 4.0 MiB | 0.0 | 62.1 ms | **17.4 ms** |
//! | regex, literal | 50,221 | **283** | 1.3 MiB | 0.1 MiB | 22.4 ms | **2.7 ms** |
//! | regex, no literal | — | 46 | — | 0.0 | — | 25.0 ms |
//! | common (capped) | 4,056 | 4,056 | 0.6 MiB | 0.6 MiB | 1.4 ms | 1.5 ms |
//!
//! (`content, many` and `regex, no literal` were added after the baseline was
//! taken — the first to give the full-text pass real verification work, the
//! second to keep the regex prefilter's *limit* as visible as its win. Neither
//! has a "before" column. The 25 ms in the last row is not a regression: it is
//! what a `regex:` query cost before this work and still costs when the pattern
//! offers no literal to filter on, which is the honest shape of the feature.)
//!
//! What the table is the reason for keeping:
//!
//! * borrowing the row's name instead of allocating one per scanned row — the
//!   50,324 and 150,134 figures were exactly one and three allocations per row;
//! * folding inside the bitap mask table rather than folding each haystack,
//!   which took the fuzzy pass's remaining two-per-row to nothing;
//! * a `LIKE` prefilter for multi-segment wildcards, which had been scanning
//!   with no SQL filter at all — the whole of the wildcard row's time saving;
//! * the pigeonhole trigram prefilter on the fuzzy full-text pass;
//! * a non-cryptographic hasher for the emitted-id set, worth ~5% of a fuzzy
//!   search (see `cascade::IdHasher`);
//! * required-literal prefilters on both `regex:` passes, which had been
//!   running the user's pattern over every name, every path and every stored
//!   document — the same `Required` machinery the fuzzy pass uses, fed by the
//!   literal analysis the regex engine already does for its own prefilter.
//!
//! And what it argued *against*, each recorded as a losing arm in
//! `benches/search.rs`: a `memchr2` candidate scan for the case-insensitive
//! filename probe, and hoisting `memmem::Finder` construction out of the
//! full-text pass. Both are obvious-looking and neither is measurable.
//!
//! Two things this table is worth reading carefully for. **Allocation churn and
//! wall-clock are not the same axis**: the first two changes removed 99.8% of
//! the allocations and moved the clock barely at all, because the fuzzy pass's
//! cost was zstd and bitap rather than malloc. And **the two big wins are both
//! the same idea**: work out what must be present for a row to match, and let
//! the database reject the rest. That took wildcards from 11.5 ms to 2.6,
//! fuzzy from 62 to 17, and `regex:` from 22 to 2.7 — where the per-row
//! micro-optimisations, measured honestly, were worth nothing at all.
//!
//! Take timings on a quiet machine. The allocation columns are deterministic
//! and reproduce bit-for-bit under any load; the time column does not, and a
//! busy box inflates it by 2-3x across the board.
//!
//! Gated by `QSB_SEARCH_ALLOC` so the harness doesn't pay the seed cost on
//! every `cargo test`. To run it:
//!
//! ```text
//! QSB_SEARCH_ALLOC=1 cargo test --release -p quicksearch-core \
//!     --test search_alloc -- --nocapture
//! ```
//!
//! [`PRAGMAS_SEARCH`]: quicksearch_core::db::schema::PRAGMAS_SEARCH
//! [`System`]: std::alloc::System

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

mod common;
use common::{scratch_db, seed_index, SeedSpec};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

// Counters are **per thread**, not global, and that is load-bearing rather
// than an optimization.
//
// libtest runs `#[test]` functions on concurrent threads in one process, and
// there is only one allocator. With global counters, another test's harness
// setup — libtest allocates an output-capture buffer per test, before the test
// body can take any lock of ours — lands inside whatever region is open here
// and is charged to it. That is not hypothetical: it is what the self-test
// below caught, twice, and no mutex in this file can fix it because the
// offending allocation happens before the other test body runs at all.
//
// Per-thread counting is immune to that, and is also the more precise
// question: `cascade::run` is synchronous and does its work on the calling
// thread, so "what did this thread allocate" *is* "what did the cascade
// allocate".
//
// `const`-initialized `Cell`s, deliberately. A `thread_local!` with a lazy
// initializer would allocate on first touch — from inside the allocator — and
// one with a destructor can panic when touched during thread teardown, which
// is exactly when the last deallocations happen. `Cell<u64>` has no `Drop`, so
// neither hazard exists.
// `LIVE` and `PEAK` are **signed**, and that is not fussiness.
//
// Per-thread accounting is inherently asymmetric: a buffer allocated on one
// thread and freed on another decrements a counter that never incremented, so
// a thread's live total legitimately goes negative. libtest does exactly this
// — a test thread starts life having freed more than it allocated. Held as
// `u64` that reads as ~1.8e19, and `PEAK.max(live)` then latches onto it and
// never moves again, so every peak in the table would be reported as 8 bytes.
// That was not a hypothetical either; it is what the self-test caught on the
// third attempt, and it is why the counts are `u64` (they only ever rise) and
// the balances are `i64`.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static REALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

/// Read one monotonic counter, tolerating a thread whose locals are gone.
#[inline]
fn get(counter: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
    counter.try_with(Cell::get).unwrap_or(0)
}

/// Add to one monotonic counter, skipping a thread whose locals are gone.
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

/// Read one signed balance.
#[inline]
fn get_live(counter: &'static std::thread::LocalKey<Cell<i64>>) -> i64 {
    counter.try_with(Cell::get).unwrap_or(0)
}

/// Move the live balance and return the new value.
#[inline]
fn bump_live(by: i64) -> i64 {
    LIVE.try_with(|c| {
        let v = c.get().wrapping_add(by);
        c.set(v);
        v
    })
    .unwrap_or(0)
}

/// Raise the high-water mark to `live` if it is higher.
#[inline]
fn note_peak(live: i64) {
    PEAK.try_with(|p| p.set(p.get().max(live))).ok();
}

/// `System`, with counters. Every hook delegates and then accounts; a failed
/// allocation is not counted, so the totals describe memory that really
/// existed.
struct Counting;

#[inline]
fn note_alloc(size: usize) {
    bump(&ALLOCS, 1);
    bump(&BYTES, size as u64);
    note_peak(bump_live(size as i64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    /// Freeing on a thread that did not allocate drives this thread's balance
    /// negative; see the note on the `thread_local!` block for why that is
    /// ordinary and why the balance is signed.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump_live(-(layout.size() as i64));
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Counted as a resize rather than as a fresh allocation: a `Vec` doubling
    /// its way up to a document's length is one buffer, not twelve, and calling
    /// it twelve allocations would hide the difference between a reused buffer
    /// and a per-row one — which is exactly what this harness exists to show.
    /// Only the *growth* is added to traffic.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
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

/// Counter values at one instant.
#[derive(Clone, Copy)]
struct Counters {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
    /// Signed: a thread's balance is legitimately negative. See the
    /// `thread_local!` block.
    live: i64,
}

/// What happened between two instants.
#[derive(Clone, Copy)]
struct Usage {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
    peak: u64,
}

impl Counters {
    /// Snapshot, and re-arm the peak tracker at the current live figure so the
    /// high-water mark that follows belongs to the region being measured
    /// rather than to whatever the process did before it.
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
            // Above the live figure the region started from, so a query that
            // holds nothing reads as zero rather than as the process floor.
            peak: (get_live(&PEAK) - self.live).max(0) as u64,
        }
    }
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// Rows to seed. Smaller than `search_perf`'s 200k on purpose: allocation
/// counts are deterministic and scale linearly with rows scanned, so they need
/// no statistical settling — only enough rows that a per-row allocation is
/// unmistakable next to a per-hit one.
const NUM_FILES: usize = 50_000;

// No serializing mutex here, deliberately: the counters are per thread, so
// concurrent tests in this binary cannot reach each other's regions. An
// earlier revision did lock, and it did not work — see the comment on the
// `thread_local!` block.

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_ALLOC").is_ok()
}

/// One query, described so the table says which cascade passes it exercises.
struct Case {
    label: &'static str,
    query: &'static str,
    fuzzy: bool,
    /// The passes this case is here to measure, for the table's last column.
    passes: &'static str,
}

/// The query set.
///
/// Every case but the last is **rare on purpose**. A query that fills the
/// display limit makes `cascade::run` break out between passes, so a common
/// term measures how quickly the cascade gives up rather than what a scan
/// costs — and the passes this work targets never run at all. The rare cases
/// are also the ones a user actually waits on.
///
/// Spread across the passes deliberately: a change that helps one pass and
/// hurts another must not be able to hide inside a single total.
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
        // One edit from the planted needle, so the exact passes miss and the
        // fuzzy ones have to do the work.
        query: "quartzyte",
        fuzzy: true,
        passes: "A + B + C + D, C and D whole-table (S1/S2/S4)",
    },
    Case {
        // The full-text pass with real work to do: the name `LIKE` finds
        // nothing, so every hit is a document the trigram index returned and
        // pass B had to decompress, fold and verify.
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
        // No literal to extract, so both regex passes still read everything.
        // Kept beside the case above so the prefilter's *limit* is as visible
        // as its win.
        label: "regex, no literal",
        query: r"regex:[0-9]{6}[a-y]{6}",
        fuzzy: false,
        passes: "regex name + content, no prefilter possible",
    },
    Case {
        label: "common (capped)",
        query: "content",
        fuzzy: false,
        passes: "A, stops at the display limit",
    },
];

/// Run one query to completion on a held connection, reporting what it moved
/// through the allocator.
///
/// Hits are counted, not kept: holding thousands of `SearchHit`s would measure
/// the harness's own `Vec` rather than the cascade's. The count is reported so
/// that a change which quietly alters the result set shows up here as well as
/// in `tests/cascade.rs`.
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

    // Everything the query needs is built above; the snapshot brackets the
    // cascade and nothing else.
    let start = Counters::start();
    let clock = Instant::now();
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("the cascade runs");
    let elapsed = clock.elapsed();
    (start.since(), count, elapsed)
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// Printed rather than asserted, for the reason `search_perf` gives: a
/// threshold on a shared machine gets muted rather than fixed. Allocation
/// counts are far more stable than timings, so a *ratchet* becomes reasonable
/// once the search work has landed and the numbers have settled — until then
/// this exists to be read when a change claims to have reduced churn.
#[test]
fn allocation_traffic_per_query() {
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_ALLOC=1 to run");
        return;
    }

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

    // One connection for the whole run, as the search worker holds one across
    // a typing session. Opened through the real entry point so the pragma
    // profile is the production one.
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the seeded index");

    println!(
        "{:<16} {:>12} {:>10} {:>12} {:>12} {:>7} {:>9}   {}",
        "case", "allocs", "reallocs", "bytes (MiB)", "peak (MiB)", "hits", "time", "passes"
    );
    for case in CASES {
        // Once to warm the page cache and the statement cache, then measured:
        // a cold first query would report SQLite's one-off setup as the
        // cascade's churn.
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

/// The accounting itself, so a number printed above is a number that means
/// something. A harness that silently stopped counting would read as a
/// spectacular optimization.
#[test]
fn the_counters_track_real_allocations() {
    let start = Counters::start();
    // A `Vec` that grows by doubling is one buffer: one alloc, then reallocs.
    let mut v: Vec<u8> = Vec::new();
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

    // Dropping it returns the bytes: live falls back, so a later region's peak
    // is not inflated by this one.
    let before_drop = get_live(&LIVE);
    drop(v);
    assert!(
        get_live(&LIVE) < before_drop,
        "dealloc must decrement live bytes"
    );

    // The signedness the whole scheme turns on. Sink the balance below zero, as
    // a thread that frees what another thread allocated really does, and check
    // that a peak is still reported: held unsigned, that negative balance reads
    // as ~1.8e19, `max` latches onto it, and every peak in the table is
    // reported as a handful of bytes forever.
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

    // The property that makes the whole harness trustworthy under libtest's
    // thread-per-test: another thread allocating hard does not touch this
    // thread's counters.
    //
    // The thread is spawned and joined *outside* the region on purpose.
    // `spawn` boxes the closure and allocates the `JoinHandle` on the calling
    // thread, and `join` frees them there — so bracketing the spawn would
    // measure this thread's own bookkeeping and report it as leakage. A
    // barrier hands control across without allocating.
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

    // A region that allocates nothing reports nothing, which is what makes
    // "this pass no longer allocates per row" a statement the table can make.
    let quiet = Counters::start();
    std::hint::black_box(1u64 + 1);
    let idle = quiet.since();
    assert_eq!((idle.allocs, idle.bytes, idle.peak), (0, 0, 0));
}
