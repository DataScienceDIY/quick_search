//! Steady-state memory accounting for an already-running process.
//!
//! [`memprobe`](memprobe.rs) answers "how much RAM does a run peak at"; this
//! answers "how much is the process still holding once it has nothing to do",
//! which is a different question with a different answer. Under glibc a peak
//! is not returned to the OS when it is freed, so the idle floor is set by the
//! largest thing that ever happened rather than by anything currently live.
//! Telling those apart is the whole point of this probe.
//!
//! It reads another process's `/proc` rather than its own, so it measures a
//! binary that was built without knowing it would be measured. That matters
//! for a before/after: an in-process diagnostic would mean the "before" and
//! "after" numbers come from different binaries, and the delta would include
//! the diagnostic itself.
//!
//! ```text
//! cargo build -p quicksearch-core --example rssprobe --release
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch)
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60 250
//! ```
//!
//! With no duration it takes one snapshot and prints the full breakdown. With
//! one it samples for that many seconds first, so a number can be quoted for
//! an *idle* process rather than for whatever the process happened to be doing
//! the instant the probe ran.
//!
//! **`VmRSS` is the wrong number to optimise and it is the one every system
//! monitor shows.** Most of it here is `Shared_Clean`: the binary's own text,
//! libc, and the Mesa/GL stack the window pulls in. Those pages are shared
//! with every other process using them and the kernel drops them under
//! pressure. The two numbers worth acting on are:
//!
//! - **`RssAnon`** — heap and thread stacks. Nothing else. This is the share
//!   this codebase allocates and can therefore give back.
//! - **`Private_Dirty`** — what the process actually costs the machine, from
//!   `smaps_rollup`. Nobody else is sharing it and it cannot be evicted, only
//!   freed.
//!
//! The `glibc arenas` line is the one that says whether an anonymous figure is
//! live data or retention. glibc allocates each non-main arena as a 64 MiB
//! aligned region and commits into it; it never unmaps one, and `free` returns
//! chunks to the arena rather than to the kernel. So a stack of 64 MiB regions
//! holding far less than 64 MiB each is freed memory the process is still
//! charged for — which `malloc_trim(3)` can return and dropping a buffer
//! cannot. Live data does not look like that.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use quicksearch_core::testutil::{mib, size_class};

/// Default gap between samples. One `status` plus one `smaps_rollup` read per
/// sample, both small; the interval is set by how fine the timeline should be
/// rather than by overhead. `smaps` is read once at the end, not per sample —
/// it is the expensive one, being proportional to the mapping count.
const DEFAULT_SAMPLE_MS: u64 = 500;

/// The size glibc reserves per non-main arena (`HEAP_MAX_SIZE` on 64-bit).
/// Regions of exactly this size, aligned to it, are arenas rather than
/// anything the program asked for.
const ARENA_SPAN: u64 = 64 * 1024 * 1024;

/// One sample: the cheap per-interval reads only.
struct Sample {
    at: Duration,
    rss: u64,
    anon: u64,
    file: u64,
    private_dirty: u64,
    pss: u64,
    /// Carried per sample rather than read once at the end, so it survives the
    /// process exiting mid-run — it comes from the same `status` read anyway.
    hwm: u64,
}

/// Resident bytes per mapping, plus the two groupings that answer the
/// retention question directly.
#[derive(Default)]
struct Breakdown {
    /// Mapping name (or anonymous size class) to resident bytes, descending.
    entries: Vec<(String, u64)>,
    /// Non-main glibc arenas: how many, and resident across all of them.
    arenas: (usize, u64),
    /// The main arena, which grows by `brk` and is the one the kernel labels.
    main_heap: u64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(pid) = args.next().and_then(|p| p.parse::<u32>().ok()) else {
        eprintln!("usage: rssprobe <pid> [duration_s] [interval_ms]");
        std::process::exit(2);
    };
    let duration = Duration::from_secs(
        args.next()
            .map(|s| s.parse().expect("duration_s must be a number"))
            .unwrap_or(0),
    );
    let interval = Duration::from_millis(
        args.next()
            .map(|s| s.parse().expect("interval_ms must be a number"))
            .unwrap_or(DEFAULT_SAMPLE_MS)
            .max(1),
    );

    if !PathBuf::from(format!("/proc/{}", pid)).exists() {
        eprintln!("rssprobe: no process {}", pid);
        std::process::exit(1);
    }
    eprintln!("rssprobe pid={} {}", pid, comm(pid));

    let (samples, at_end) = sample_for(pid, duration, interval);
    report(&samples, &at_end, interval);
}

/// Sample until `duration` elapses. A zero duration still takes one sample,
/// so the no-argument form is a snapshot rather than an error.
///
/// The per-mapping breakdown is taken alongside the last *live* sample rather
/// than after the loop, because a process that exits during a long run would
/// otherwise report an empty one — which reads like "nothing was resident"
/// rather than "nobody was there to ask".
fn sample_for(pid: u32, duration: Duration, interval: Duration) -> (Vec<Sample>, Breakdown) {
    let start = Instant::now();
    let mut samples = Vec::new();
    let mut at_end = Breakdown::default();
    loop {
        let elapsed = start.elapsed();
        let Some(s) = sample(pid, elapsed) else {
            if samples.is_empty() {
                eprintln!("rssprobe: cannot read /proc/{}: process gone?", pid);
                std::process::exit(1);
            }
            eprintln!(
                "  process exited after {:.1}s; figures below are its last live sample",
                elapsed.as_secs_f64()
            );
            break;
        };
        samples.push(s);
        // smaps is proportional to the mapping count, so it is read once per
        // sample only when the sample might be the last one — which, until the
        // loop ends, is every one of them.
        at_end = breakdown(pid);
        if start.elapsed() >= duration {
            break;
        }
        std::thread::sleep(interval);
    }
    (samples, at_end)
}

fn sample(pid: u32, at: Duration) -> Option<Sample> {
    let status = proc_kv(pid, "status")?;
    // smaps_rollup is the kernel's own sum over smaps, so it costs one read
    // rather than one per mapping. Missing only on kernels this old code will
    // never meet; treat it as zero rather than failing the sample.
    let rollup = proc_kv(pid, "smaps_rollup").unwrap_or_default();
    Some(Sample {
        at,
        rss: *status.get("VmRSS").unwrap_or(&0),
        anon: *status.get("RssAnon").unwrap_or(&0),
        file: *status.get("RssFile").unwrap_or(&0),
        private_dirty: *rollup.get("Private_Dirty").unwrap_or(&0),
        pss: *rollup.get("Pss").unwrap_or(&0),
        hwm: *status.get("VmHWM").unwrap_or(&0),
    })
}

/// Parse a `/proc/<pid>/<file>` of `Key:  N kB` lines into bytes.
///
/// Both `status` and `smaps_rollup` are in this format, and both carry lines
/// that are not sizes at all (`Name:`, `State:`, the rollup's address range).
/// Anything that does not end in `kB` is skipped rather than guessed at.
fn proc_kv(pid: u32, file: &str) -> Option<HashMap<String, u64>> {
    let text = std::fs::read_to_string(format!("/proc/{}/{}", pid, file)).ok()?;
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (Some(value), Some("kB")) = (fields.next(), fields.next()) else {
            continue;
        };
        if let Ok(kib) = value.parse::<u64>() {
            out.insert(key.to_string(), kib * 1024);
        }
    }
    Some(out)
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Resident bytes per mapping from `/proc/<pid>/smaps`, summed by name, with
/// glibc's arenas identified.
///
/// The name is the mapping's path, or `[heap]`/`[stack]` for the ones the
/// kernel labels. Everything else is anonymous — thread stacks, arena heaps,
/// and any single `malloc` large enough to have gone to `mmap` — and is
/// bucketed by size class, because individually they are unnamed and there can
/// be hundreds of them.
fn breakdown(pid: u32) -> Breakdown {
    let Ok(smaps) = std::fs::read_to_string(format!("/proc/{}/smaps", pid)) else {
        return Breakdown::default();
    };

    // Two passes over the same parse: one for the by-name totals the report
    // prints, one to find arenas — which needs the raw address ranges the
    // names throw away.
    let mut by_name: HashMap<String, u64> = HashMap::new();
    let mut regions: Vec<Region> = Vec::new();
    let mut current = String::new();

    for line in smaps.lines() {
        if let Some(rss_kib) = line.strip_prefix("Rss:") {
            let kib: u64 = rss_kib
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            *by_name.entry(current.clone()).or_default() += kib * 1024;
            if let Some(r) = regions.last_mut() {
                r.rss = kib * 1024;
            }
        } else if let Some(region) = parse_map_header(line) {
            current = region.name.clone();
            regions.push(region);
        }
    }

    let mut entries: Vec<(String, u64)> = by_name.into_iter().filter(|(_, b)| *b > 0).collect();
    entries.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));

    Breakdown {
        entries,
        arenas: find_arenas(&regions),
        main_heap: regions
            .iter()
            .filter(|r| r.name == "[heap]")
            .map(|r| r.rss)
            .sum(),
    }
}

/// One `smaps` mapping, kept with its address range so arenas can be found.
struct Region {
    lo: u64,
    hi: u64,
    anonymous: bool,
    name: String,
    rss: u64,
}

/// Count glibc's non-main arenas and their resident bytes.
///
/// An arena is one `mmap` of [`ARENA_SPAN`], aligned to it, which glibc then
/// commits into piecewise — so by the time it reaches `smaps` it has usually
/// been split into a readable part and a `---p` remainder. Matching a single
/// mapping of the full span therefore misses most of them. Contiguous runs of
/// anonymous mappings are coalesced first, and a run that spans exactly one
/// aligned [`ARENA_SPAN`] is an arena.
///
/// This can in principle collide with a program that mmaps 64 MiB at 64 MiB
/// alignment on purpose. Nothing here does, and the alternative — parsing
/// glibc's internal `heap_info` out of the process — is not worth it for a
/// figure whose job is to point at a cause rather than to be exact.
fn find_arenas(regions: &[Region]) -> (usize, u64) {
    let mut count = 0;
    let mut resident = 0;
    let mut i = 0;
    while i < regions.len() {
        if !regions[i].anonymous {
            i += 1;
            continue;
        }
        let lo = regions[i].lo;
        let mut j = i;
        let mut rss = 0;
        while j < regions.len()
            && regions[j].anonymous
            && regions[j].lo == if j == i { lo } else { regions[j - 1].hi }
            && regions[j].hi <= lo + ARENA_SPAN
        {
            rss += regions[j].rss;
            j += 1;
        }
        if j > i && regions[j - 1].hi == lo + ARENA_SPAN && lo.is_multiple_of(ARENA_SPAN) {
            count += 1;
            resident += rss;
            i = j;
        } else {
            i += 1;
        }
    }
    (count, resident)
}

/// Parse one `smaps` header line: `addr-addr perms offset dev inode [path]`.
///
/// Anonymous mappings have no path; they are named by size class so that a
/// hundred 8 MiB regions read as one line rather than a hundred.
fn parse_map_header(line: &str) -> Option<Region> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let (lo, hi) = range.split_once('-')?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    // Fields 2-5 are perms, offset, dev, inode; anything after is the path.
    let path = fields.nth(4).unwrap_or("");
    Some(Region {
        lo,
        hi,
        anonymous: path.is_empty(),
        name: if path.is_empty() {
            format!("anon {}", size_class(hi.saturating_sub(lo)))
        } else {
            path.to_string()
        },
        rss: 0,
    })
}

fn report(samples: &[Sample], at_end: &Breakdown, interval: Duration) {
    if samples.len() > 1 {
        // One line per 5% of the run, so the shape is visible at any duration.
        let step = (samples.len() / 20).max(1);
        eprintln!(
            "\n  {:>8}  {:>10}  {:>10}  {:>10}  {:>12}  {:>10}",
            "t", "VmRSS", "RssAnon", "RssFile", "PrivDirty", "Pss"
        );
        for s in samples.iter().step_by(step) {
            eprintln!(
                "  {:>7.1}s  {:>10}  {:>10}  {:>10}  {:>12}  {:>10}",
                s.at.as_secs_f64(),
                mib(s.rss),
                mib(s.anon),
                mib(s.file),
                mib(s.private_dirty),
                mib(s.pss)
            );
        }
    }

    let last = samples.last().expect("at least one sample");
    let elapsed = last.at;
    eprintln!(
        "\nsteady state over {:.1}s ({} samples, {:?} apart)",
        elapsed.as_secs_f64(),
        samples.len(),
        interval
    );

    // Both ends of each range, because a figure that is drifting is a
    // different finding from one that is flat, and only the flat one can be
    // called a floor.
    range_line("VmRSS", samples, |s| s.rss, "");
    range_line(
        "RssAnon",
        samples,
        |s| s.anon,
        "heap + thread stacks: the share this code owns",
    );
    range_line(
        "RssFile",
        samples,
        |s| s.file,
        "binary text, libc, Mesa/GL: shared and evictable",
    );
    range_line(
        "Private_Dirty",
        samples,
        |s| s.private_dirty,
        "what this process costs the machine",
    );
    range_line("Pss", samples, |s| s.pss, "");

    eprintln!(
        "  {:<16} {:>10}   kernel high-water: the peak this floor was left by",
        "VmHWM",
        mib(last.hwm)
    );

    let b = at_end;
    let (arenas, arena_rss) = b.arenas;
    eprintln!(
        "\n  glibc heap: {} non-main arena(s) holding {}, plus [heap] {}",
        arenas,
        mib(arena_rss),
        mib(b.main_heap)
    );
    if arenas > 0 {
        eprintln!(
            "    reserved {} across them, so {:.0}% of the arena space is resident",
            mib(arenas as u64 * ARENA_SPAN),
            100.0 * arena_rss as f64 / (arenas as u64 * ARENA_SPAN) as f64
        );
    }

    if !b.entries.is_empty() {
        eprintln!("\n  resident bytes by mapping:");
        for (name, bytes) in b.entries.iter().take(12) {
            eprintln!("    {:>10}  {}", mib(*bytes), name);
        }
    }
}

fn range_line(label: &str, samples: &[Sample], get: fn(&Sample) -> u64, note: &str) {
    let last = get(samples.last().expect("at least one sample"));
    let min = samples.iter().map(get).min().unwrap_or(0);
    let max = samples.iter().map(get).max().unwrap_or(0);
    let spread = if samples.len() > 1 && max > min {
        format!("  (min {}, max {})", mib(min), mib(max))
    } else {
        String::new()
    };
    if note.is_empty() {
        eprintln!("  {:<16} {:>10}{}", label, mib(last), spread);
    } else {
        eprintln!("  {:<16} {:>10}   {}{}", label, mib(last), note, spread);
    }
}
