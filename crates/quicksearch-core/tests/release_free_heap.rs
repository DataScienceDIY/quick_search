//! `release_free_heap` has to actually return memory to the kernel.
//!
//! Its own integration binary because it needs a `#[global_allocator]`, which
//! only takes effect in the crate that declares it: the lib test harness
//! declares none, so a unit test here would measure the system allocator and
//! pass no matter what `release_free_heap` did.
//!
//! The failure it guards is silent. `mi_collect` is reached through our own
//! `extern "C"` block (`libmimalloc-sys` binds it only behind its `extended`
//! feature), so deleting the call, or the declaration drifting from
//! `void mi_collect(bool)`, leaves a build that compiles, runs, and keeps
//! every run's peak resident for the life of the process (under glibc, a
//! multi-million-file run settled at 985 MB with 871 MB of unreturnable
//! slack).

#![cfg(target_os = "linux")]

#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

/// Blocks big enough to be worth returning and small enough to come from the
/// allocator's segments rather than a direct `mmap`: the mid-size churn that
/// fragments, not the large buffers, which are given back cleanly.
const BLOCK: usize = 100 * 1024;
const BLOCKS: usize = 4_000;

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .expect("VmRSS")
        * 1024
}

#[test]
fn releasing_the_free_heap_returns_it_to_the_kernel() {
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);

    let before = rss_bytes();
    // Written to, not just reserved: untouched pages are never resident, so
    // an unwritten allocation would prove nothing about reclaiming one.
    let mut held: Vec<Vec<u8>> = (0..BLOCKS)
        .map(|i| vec![(i % 251) as u8; BLOCK])
        .collect();
    let peak = rss_bytes();
    assert!(
        peak > before + (BLOCKS * BLOCK / 2) as u64,
        "the corpus never became resident: {:.0} MiB to {:.0} MiB",
        mib(before),
        mib(peak)
    );

    held.clear();
    held.shrink_to_fit();
    let freed = rss_bytes();

    quicksearch_core::platform::release_free_heap();
    let released = rss_bytes();

    // The call itself must return over half the corpus, so frees that already
    // gave it back would fail this rather than pass it. Loose: it checks
    // order-of-magnitude reclamation, not a figure that drifts with allocator
    // versions. Measured 454 MiB peak, still 454 MiB after the frees, 5 MiB
    // after the call.
    assert!(
        freed.saturating_sub(released) > (BLOCKS * BLOCK / 2) as u64,
        "release_free_heap returned only {:.0} MiB (started {:.0}, peaked {:.0}, \
         {:.0} after freeing, {:.0} after the call) — mi_collect is not reaching \
         the allocator, or the frees alone returned the corpus",
        mib(freed.saturating_sub(released)),
        mib(before),
        mib(peak),
        mib(freed),
        mib(released)
    );
}
