//! What one content extraction *costs in memory*, and what a pool of them
//! costs together.
//!
//! [`memprobe`](memprobe.rs) measures a whole run, where extraction is mixed
//! in with the walk, the writer and SQLite. This isolates the extractors:
//! the same [`decide_content`] the content pass calls, over a real corpus,
//! with a pool the size of a real one.
//!
//! ```text
//! cargo build -p quicksearch-core --example extractprobe --release
//! ./target/release/examples/extractprobe ~/Documents            # 1 worker: per-file cost
//! ./target/release/examples/extractprobe ~/Documents 16         # a network root's pool
//! ./target/release/examples/extractprobe ~/Documents 16 4       # four such roots
//! ```
//!
//! The question it answers: **does peak memory track the number of files or
//! the number of workers?** A run's file count is the user's; its worker
//! count is ours (`walk::thread_count_for`, one pool per root), so if the
//! peak follows the pool, the fix is a bound and not a rewrite.
//!
//! `peak live` is from a counting allocator and is exact; `VmHWM` is what the
//! machine sees, and the gap between them is glibc holding freed chunks on
//! its arena free lists. With one worker the per-file table is exact — the
//! largest entries are the files that would spike a real run.

use std::alloc::{GlobalAlloc, Layout};

// What `Counting` wraps: the allocator the shipped binaries install, or the
// figures describe a build nobody runs. See `platform::Allocator`.
use quicksearch_core::platform::Allocator as Inner;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod common;

// ---------------------------------------------------------------------------
// Allocation accounting
// ---------------------------------------------------------------------------

/// [`Inner`], counting — per binary, so the shipped `quicksearch` is
/// untouched. `PEAK_LIVE` is the high-water of live bytes: unlike RSS it
/// cannot be inflated by the allocator declining to return pages.
struct Counting;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE: AtomicU64 = AtomicU64::new(0);
/// High-water since the last [`take_mark`]; the per-file column.
static MARK: AtomicU64 = AtomicU64::new(0);

#[inline]
fn note(live: u64) {
    PEAK_LIVE.fetch_max(live, Ordering::Relaxed);
    MARK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { Inner.alloc(l) };
        if !p.is_null() {
            note(LIVE.fetch_add(l.size() as u64, Ordering::Relaxed) + l.size() as u64);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { Inner.alloc_zeroed(l) };
        if !p.is_null() {
            note(LIVE.fetch_add(l.size() as u64, Ordering::Relaxed) + l.size() as u64);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
        unsafe { Inner.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { Inner.realloc(p, l, new) };
        if !q.is_null() {
            let (old, new) = (l.size() as u64, new as u64);
            let live = if new >= old {
                LIVE.fetch_add(new - old, Ordering::Relaxed) + (new - old)
            } else {
                LIVE.fetch_sub(old - new, Ordering::Relaxed) - (old - new)
            };
            note(live);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The high-water since the last call, rebased to live-now. Meaningful only
/// while one thread is allocating — hence the per-file table's `workers == 1`
/// guard.
fn take_mark() -> u64 {
    let live = LIVE.load(Ordering::Relaxed);
    MARK.swap(live, Ordering::Relaxed).saturating_sub(live)
}

use quicksearch_core::config::Config;
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::decide_content;
use quicksearch_core::testutil::mib;

/// Head bytes read for the MIME sniff — the same window the walk uses, so
/// this probe classifies files exactly as a run would.
fn sniff(path: &Path, hash_length: usize) -> Option<&'static str> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; hash_length];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    quicksearch_core::mime::guess_mime_from_head(path, &head)
}

struct Candidate {
    path: String,
    mime: &'static str,
    size: u64,
}

/// Every file under `dir` an extractor claims and a run would not reject as
/// oversize: exactly the set the content pass would be handed.
fn candidates(dir: &Path, config: &Config, registry: &Registry) -> Vec<Candidate> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > config.processing.maximum_text_file_size {
            continue;
        }
        let Some(path) = entry.path().to_str().map(str::to_string) else {
            continue;
        };
        let Some(mime) = sniff(entry.path(), config.processing.hash_length) else {
            continue;
        };
        if !registry.supports(mime) {
            continue;
        }
        out.push(Candidate {
            path,
            mime,
            size: meta.len(),
        });
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next().map(PathBuf::from) else {
        eprintln!("usage: extractprobe <dir> [workers] [replicas]");
        std::process::exit(2);
    };
    let workers: usize = args
        .next()
        .map(|v| v.parse().expect("workers must be a number"))
        .unwrap_or(1)
        .max(1);
    let replicas: usize = args
        .next()
        .map(|v| v.parse().expect("replicas must be a number"))
        .unwrap_or(1)
        .max(1);

    let config = Config::default();
    let registry = Arc::new(Registry::default_set());

    let found = candidates(&dir, &config, &registry);
    if found.is_empty() {
        eprintln!(
            "extractprobe: nothing under {} is extractable",
            dir.display()
        );
        std::process::exit(1);
    }
    eprintln!(
        "extractprobe {}: {} extractable file(s), {} workers, {} replica(s)",
        dir.display(),
        found.len(),
        workers,
        replicas
    );

    // Baseline *after* the scan: the candidate list is the probe's own cost,
    // not extraction's.
    let baseline_rss = rss();
    let baseline_live = LIVE.load(Ordering::Relaxed);
    PEAK_LIVE.store(baseline_live, Ordering::Relaxed);

    // Replicas share one queue, so every worker stays busy to the end
    // instead of the pool draining down to one straggler.
    let queue: Vec<&Candidate> = (0..replicas).flat_map(|_| found.iter()).collect();
    let next = AtomicUsize::new(0);
    let worst: Mutex<Vec<(u64, String, u64, &'static str)>> = Mutex::new(Vec::new());
    let per_file = workers == 1;

    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..workers {
            let (queue, next, worst) = (&queue, &next, &worst);
            let (registry, config) = (registry.clone(), config.clone());
            // One per worker, as the content pass does: the per-file figures
            // below are a worker's steady state, not its first file.
            let mut scratch = quicksearch_core::extract::Scratch::new(&config);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(c) = queue.get(i) else { return };
                if per_file {
                    take_mark();
                }
                let outcome = decide_content(&c.path, Some(c.mime), &registry, &config, &mut scratch);
                if per_file {
                    let cost = take_mark();
                    let text =
                        quicksearch_core::file_handling::outcome_body(&outcome).map_or(0, str::len);
                    let mut w = worst
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    w.push((cost, c.path.clone(), c.size, c.mime));
                    // Kept small so the table itself is not the peak.
                    w.sort_by_key(|(cost, ..)| std::cmp::Reverse(*cost));
                    w.truncate(12);
                    let _ = text;
                }
            });
        }
    });
    let elapsed = start.elapsed();

    let peak_live = PEAK_LIVE
        .load(Ordering::Relaxed)
        .saturating_sub(baseline_live);
    let hwm = common::vm_hwm().unwrap_or(0);
    eprintln!(
        "\n  {} file(s) in {:.1}s ({:.0}/s)",
        queue.len(),
        elapsed.as_secs_f64(),
        queue.len() as f64 / elapsed.as_secs_f64().max(0.001),
    );
    eprintln!(
        "  peak live   {}  above a {} baseline — what extraction really holds",
        mib(peak_live),
        mib(baseline_live),
    );
    eprintln!(
        "  peak RSS    {} (VmHWM), now {} — the gap is glibc holding freed chunks",
        mib(hwm),
        mib(rss()),
    );
    eprintln!(
        "  baseline    {} RSS before the first extraction",
        mib(baseline_rss)
    );
    eprintln!(
        "  per worker  {} of peak live, at {} workers",
        mib(peak_live / workers as u64),
        workers,
    );

    let w = worst
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !w.is_empty() {
        eprintln!("\n  most expensive files (live bytes held while extracting):");
        for (cost, path, size, mime) in w.iter() {
            eprintln!(
                "    {:>10}  from a {:>9} {}  {}",
                mib(*cost),
                mib(*size),
                mime,
                path
            );
        }
        eprintln!(
            "\n  A pool of N runs N of these at once. `maximum_text_file_size` bounds\n  \
             the input ({}), not the working set above.",
            mib(config.processing.maximum_text_file_size),
        );
    }
}

/// Resident set size now, from `/proc/self/statm` field 2 (resident pages).
fn rss() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}
