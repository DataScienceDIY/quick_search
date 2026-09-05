//! What indexing one file *allocates*. `index.rs` answers "how long does a
//! step take"; `extractprobe` answers "how much RAM does a pool need";
//! this answers "how many trips to the allocator does one file cost, and how
//! big is the transient peak behind it".
//!
//! Only Rust-side allocations are counted — SQLite mallocs directly and is
//! invisible — which leaves precisely the walk's and the extractors' own
//! churn. The fixtures are the shared extraction corpus (`tests/corpus/`),
//! so every format QuickSearch claims appears exactly once, written by a
//! library that is not the one reading it back.
//!
//! Reading the numbers: **allocs** is per-file allocator traffic and is what
//! a scratch buffer removes; **bytes** is churn; **peak** is the transient
//! high-water one file reaches, and is what multiplies by the worker count
//! (`walk::thread_count_for`, one pool per root). A `peak` far above the
//! file's own size is amplification inside a parser.
//!
//! Gated by `QSB_INDEX_ALLOC`:
//!
//! ```text
//! QSB_INDEX_ALLOC=1 cargo bench -p quicksearch-core --bench index_alloc
//! ```

use std::alloc::{GlobalAlloc, Layout};

// What `Counting` wraps: the allocator the shipped binaries install, or the
// figures describe a build nobody runs. See `platform::Allocator`.
use quicksearch_core::platform::Allocator as Inner;
use std::cell::Cell;
use std::path::Path;

use quicksearch_core::config::Config;
use quicksearch_core::db::repo::{self, DocEncoder};
use quicksearch_core::extract::{Registry, Scratch};
use quicksearch_core::file_handling::{decide_content, prepare_file_record};
use quicksearch_core::mime;

// The corpus lives with the tests that assert on its content; this reads the
// same fixtures rather than growing a second, drifting set.
#[path = "../tests/corpus/mod.rs"]
mod corpus;

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

// Counters are per thread — load-bearing: everything measured here is
// synchronous on the main thread, so "this thread" *is* "the region", and a
// background thread (a lazily-spawned pool inside a parser, say) cannot
// silently land in someone else's total.
//
// `const`-initialized `Cell`s: a lazy initializer would allocate from inside
// the allocator, and a destructor can panic during thread teardown — exactly
// when the last deallocations happen.
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

#[derive(Clone, Copy, Default)]
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
            // Above the region's starting live figure, so a no-op reads zero.
            peak: (get_live(&PEAK) - self.live).max(0) as u64,
        }
    }
}

/// Measure one closure, discarding whatever it produced *inside* the region
/// so the drop is charged to it too — a result kept alive would report the
/// peak of the next case instead.
fn measure<T>(f: impl FnOnce() -> T) -> Usage {
    let start = Counters::start();
    drop(std::hint::black_box(f()));
    start.since()
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

fn enabled() -> bool {
    std::env::var("QSB_INDEX_ALLOC").is_ok()
}

fn kib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

fn main() {
    // The accounting is verified before anything is printed: a harness that
    // silently stopped counting would read as a spectacular optimization.
    the_counters_track_real_allocations();
    if !enabled() {
        eprintln!("skipping: set QSB_INDEX_ALLOC=1 to run");
        return;
    }
    let (dir, samples) = corpus::build("index-alloc");
    let config = Config::default();
    let registry = Registry::default_set();

    extraction_traffic_per_file(&samples, &config, &registry);
    walk_traffic_per_file(&samples, &config, &registry);
    compression_traffic_per_chunk(&config);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The content pass's half: `decide_content` is what a `qs-extract` worker
/// runs, and it is the whole of the per-file cost outside the connection.
fn extraction_traffic_per_file(samples: &[corpus::Sample], config: &Config, registry: &Registry) {
    println!(
        "extraction — decide_content, the content worker's per-file work\n\n\
         {:<10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>8}",
        "format", "file (KiB)", "allocs", "reallocs", "bytes (KiB)", "peak (KiB)", "amp"
    );

    // One scratch for the whole table, as a `qs-extract` worker holds one
    // for a whole pass: the figures below are a worker's *steady state*, not
    // its first file.
    let mut scratch = Scratch::new(config);
    let mut rows: Vec<(&str, u64, Usage)> = Vec::new();
    for sample in samples {
        let path = sample.path.to_string_lossy().into_owned();
        let size = std::fs::metadata(&sample.path).map(|m| m.len()).unwrap_or(0);
        let Some(mime) = sniff(&sample.path, config.processing.hash_length) else {
            continue;
        };
        // Warm once: a format's first call may build a lazy static the file
        // after it does not pay for.
        let _ = decide_content(&path, Some(mime), registry, config, &mut scratch);
        let usage = measure(|| decide_content(&path, Some(mime), registry, config, &mut scratch));
        rows.push((sample.label, size, usage));
    }

    for (label, size, usage) in &rows {
        println!(
            "{:<10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>8}",
            label,
            kib(*size),
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
            // How far above the file's own size the transient peak reached.
            // This is the figure that multiplies by the worker count.
            match size {
                0 => "-".to_string(),
                n => format!("{:.1}x", usage.peak as f64 / *n as f64),
            }
        );
    }
    println!(
        "\n  maximum_text_file_size {} KiB, maximum_text_size {} KiB — what every\n  \
         extractor's ceilings are now derived from.\n",
        config.processing.maximum_text_file_size / 1024,
        config.processing.maximum_text_size / 1024,
    );
}

/// The walk's half: one `stat`'s worth of metadata in, one finished record
/// out — hashing, MIME sniffing and the inline-text shortcut included.
fn walk_traffic_per_file(samples: &[corpus::Sample], config: &Config, registry: &Registry) {
    println!(
        "walk — prepare_file_record, the walk worker's per-file work\n\n\
         {:<10} {:>10} {:>10} {:>10} {:>12} {:>12}",
        "format", "file (KiB)", "allocs", "reallocs", "bytes (KiB)", "peak (KiB)"
    );

    // One scratch for the whole table; see `extraction_traffic_per_file`.
    let mut scratch = Scratch::new(config);
    for sample in samples {
        let path = sample.path.to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(&sample.path) else {
            continue;
        };
        let _ = prepare_file_record(&path, &meta, config, registry, &mut scratch);
        let usage = measure(|| prepare_file_record(&path, &meta, config, registry, &mut scratch));
        println!(
            "{:<10} {:>10} {:>10} {:>10} {:>12} {:>12}",
            sample.label,
            kib(meta.len()),
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
        );
    }
    println!(
        "\n  A file under hash_length ({} KiB) is extracted here rather than by the\n  \
         content pass, so its row carries inline text.\n",
        config.processing.hash_length / 1024
    );
}

/// Rows per writer chunk — `batch::STORE_CHUNK`, the unit `compress_bodies`
/// is handed.
const CHUNK: usize = 32;

/// The writer's half that runs *outside* the connection lock.
///
/// **The zstd context does not appear here**: `zstd::bulk::Compressor::new`
/// allocates through zstd's own C allocator, not Rust's, so it is invisible
/// to this harness and its cost is CPU only (`benches/index.rs`, group
/// `zstd_encode`). What this measures is the part that *is* Rust-side — one
/// output `Vec<u8>` per row against one arena per chunk.
fn compression_traffic_per_chunk(config: &Config) {
    // `maximum_text_size` is the worst case a row can carry.
    let doc = lipsum(config.processing.maximum_text_size);
    println!(
        "compression — one {}-row chunk of {} KiB documents\n\n{:<24} {:>10} {:>10} {:>12} {:>12}",
        CHUNK,
        kib(doc.len() as u64),
        "shape",
        "allocs",
        "reallocs",
        "bytes (KiB)",
        "peak (KiB)"
    );

    let per_row = measure(|| {
        (0..CHUNK)
            .map(|_| repo::encode_one(&doc, true).expect("encode"))
            .collect::<Vec<_>>()
    });
    // What the writer does now: one encoder and one arena for the chunk.
    let arena = measure(|| {
        let mut enc = DocEncoder::new().expect("encoder");
        let mut arena = Vec::new();
        (0..CHUNK)
            .map(|_| enc.encode_into(&doc, &mut arena).expect("encode"))
            .collect::<Vec<_>>()
            .len()
    });

    for (shape, usage) in [("a Vec per row", per_row), ("one arena, chunk", arena)] {
        println!(
            "{:<24} {:>10} {:>10} {:>12} {:>12}",
            shape,
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
        );
    }
    println!(
        "\n  The arena is reused across every chunk a writer call handles, so after the\n  \
         first its growth is zero too. The zstd context is invisible here — see above.\n"
    );
}

/// Head bytes read for the MIME sniff — the same window the walk uses, so
/// this classifies files exactly as a run would.
fn sniff(path: &Path, hash_length: usize) -> Option<&'static str> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; hash_length];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    mime::guess_mime_from_head(path, &head)
}

/// Deterministic filler; the compression figures must not move between runs.
fn lipsum(size: usize) -> String {
    const WORDS: &[&str] = &[
        "lorem", "ipsum", "dolor", "consectetur", "adipiscing", "tempor", "incididunt", "labore",
    ];
    let mut lcg = quicksearch_core::testutil::Lcg::new(0x5eed);
    let mut out = String::with_capacity(size + 16);
    while out.len() < size {
        out.push_str(WORDS[lcg.next_u64() as usize % WORDS.len()]);
        out.push(' ');
    }
    out.truncate(size);
    out
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

    // `measure` must charge the value's *drop* to its own region, or every
    // per-file peak here would belong to the case after it.
    let held = measure(|| Vec::<u8>::with_capacity(1 << 20));
    assert!(held.peak >= 1 << 20, "the allocation is inside the region");
    let after = Counters::start();
    std::hint::black_box(1u64 + 1);
    assert_eq!(after.since().peak, 0, "and it was freed before the next one");

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
}
