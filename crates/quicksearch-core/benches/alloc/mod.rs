//! The counting global allocator the `*_alloc` benches install: it wraps the
//! shipped allocator and counts allocations, resizes, bytes and peak live
//! bytes. Only Rust-side allocations are counted; SQLite and zstd allocate
//! through their own C allocators and are invisible here.
//!
//! Counters are per thread, so another thread's allocations never land in
//! the open region. Every measured region runs synchronously on the calling
//! thread, so "this thread" is "the region".

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;

// The allocator the shipped binaries install, so the figures describe a
// build someone runs. See `platform::Allocator`.
use quicksearch_core::platform::Allocator as Inner;

// `const`-initialized `Cell`s: a lazy initializer would allocate inside the
// allocator, and a destructor can panic during thread teardown, when the last
// deallocations happen. `LIVE` and `PEAK` are signed because cross-thread
// frees drive a balance negative; unsigned, it would read ~1.8e19 and
// `PEAK`'s `max` would latch there forever.
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

/// [`Inner`], with counters. A failed allocation is not counted, so the
/// totals describe memory that existed.
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

    /// A cross-thread free drives the balance negative.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump_live(-(layout.size() as i64));
        unsafe { Inner.dealloc(ptr, layout) }
    }

    /// Counted as a resize, not an allocation: a doubling `Vec` is one
    /// buffer, not twelve, and that difference is what the benches show.
    /// Only growth adds to the byte traffic.
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

/// A snapshot of this thread's counters, opening a measured region.
#[derive(Clone, Copy)]
pub struct Counters {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
    /// Signed; see the `thread_local!` note.
    live: i64,
}

/// What a region allocated, from [`Counters::since`].
#[derive(Clone, Copy)]
pub struct Usage {
    pub allocs: u64,
    pub reallocs: u64,
    pub bytes: u64,
    pub peak: u64,
}

impl Counters {
    /// Snapshot and re-arm the peak tracker at the current live figure, so
    /// the following high-water mark belongs to the measured region.
    pub fn start() -> Counters {
        let live = get_live(&LIVE);
        PEAK.with(|p| p.set(live));
        Counters {
            allocs: get(&ALLOCS),
            reallocs: get(&REALLOCS),
            bytes: get(&BYTES),
            live,
        }
    }

    pub fn since(&self) -> Usage {
        Usage {
            allocs: get(&ALLOCS).wrapping_sub(self.allocs),
            reallocs: get(&REALLOCS).wrapping_sub(self.reallocs),
            bytes: get(&BYTES).wrapping_sub(self.bytes),
            // Above the region's starting live figure, so an empty region
            // reads zero.
            peak: (get_live(&PEAK) - self.live).max(0) as u64,
        }
    }
}

/// Verify the accounting itself. Each bench runs this before printing
/// anything: a harness that silently stopped counting would read as a
/// spectacular optimization.
pub fn the_counters_track_real_allocations() {
    let start = Counters::start();
    let mut v: Vec<u8> = Vec::new();
    // `push` in a loop because the growth is what is measured: the
    // `resize`/`vec![0; n]` clippy asks for would allocate once with no
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

    // Sink the balance below zero, as a cross-thread free does: a peak must
    // still be reported.
    bump_live(-(1 << 20));
    let negative = Counters::start();
    assert!(negative.live < 0, "the balance is negative");
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
    // Spawn and join sit outside the region: `spawn` boxes its closure on
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
