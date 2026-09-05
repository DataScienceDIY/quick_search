//! [`memprobe`](memprobe.rs) answers "how much RAM does a run peak at";
//! this answers "how much is the process still holding once idle". It
//! reads another process's `/proc`, so the measured binary was built
//! without knowing it would be measured.
//!
//! ```text
//! cargo build -p quicksearch-core --example rssprobe --release
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch)
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60 250
//! ```
//!
//! **`VmRSS` is the wrong number to optimise** — mostly `Shared_Clean`,
//! shared and droppable, yet it is what every monitor shows. Act on
//! **`RssAnon`** (heap and thread stacks — what this codebase allocates)
//! and **`Private_Dirty`** (what the process costs the machine). The
//! `glibc arenas` line says whether an anonymous figure is live data or
//! retention that `malloc_trim(3)` could return.

// The GUI's idle footprint is an allocator property too; see `memprobe`.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

use quicksearch_core::testutil::{mib, size_class};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Default gap between samples; `smaps` — proportional to the mapping
/// count — is read once at the end, not per sample.
const DEFAULT_SAMPLE_MS: u64 = 500;

/// The size glibc reserves per non-main arena (`HEAP_MAX_SIZE` on 64-bit).
const ARENA_SPAN: u64 = 64 * 1024 * 1024;

struct Sample {
    at: Duration,
    rss: u64,
    anon: u64,
    file: u64,
    private_dirty: u64,
    pss: u64,
    /// Carried per sample, so it survives the process exiting mid-run.
    hwm: u64,
}

#[derive(Default)]
struct Breakdown {
    entries: Vec<(String, u64)>,
    arenas: (usize, u64),
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

/// Sample until `duration` elapses; zero still takes one sample. The
/// per-mapping breakdown rides the last *live* sample: a process exiting
/// mid-run would otherwise report "nothing resident".
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
        // Read while the process might exit: every sample may be the last.
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
    // smaps_rollup is the kernel's own sum — one read, not one per mapping;
    // missing only on ancient kernels, treated as zero.
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

/// Parse `Key:  N kB` lines into bytes; non-size lines are skipped.
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

/// Resident bytes per mapping from smaps, summed by name, arenas
/// identified; anonymous mappings are bucketed by size class —
/// individually unnamed, and there can be hundreds.
fn breakdown(pid: u32) -> Breakdown {
    let Ok(smaps) = std::fs::read_to_string(format!("/proc/{}/smaps", pid)) else {
        return Breakdown::default();
    };

    // Two passes over one parse: by-name totals, then arenas — which need
    // the raw address ranges the names throw away.
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

struct Region {
    lo: u64,
    hi: u64,
    anonymous: bool,
    name: String,
    rss: u64,
}

/// Count glibc's non-main arenas: one aligned `mmap` of [`ARENA_SPAN`] that
/// glibc commits piecewise, so by `smaps` it is usually split — contiguous
/// anonymous runs are coalesced first, and a run spanning exactly one
/// aligned span is an arena. Could collide with a deliberate aligned 64 MiB
/// mmap; nothing here does.
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

/// Parse one `smaps` header line; anonymous mappings are named by size
/// class so a hundred 8 MiB regions read as one line.
fn parse_map_header(line: &str) -> Option<Region> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let (lo, hi) = range.split_once('-')?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
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

    // Both ends of each range: drifting and flat are different findings,
    // and only a flat figure can be called a floor.
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
