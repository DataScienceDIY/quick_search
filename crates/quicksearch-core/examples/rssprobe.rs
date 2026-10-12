//! Samples another process's memory from its `/proc`, so the measured binary
//! needs no instrumentation. [`memprobe`](memprobe.rs) answers "how much RAM
//! does a run peak at"; this answers "how much does the process still hold
//! once idle".
//!
//! ```text
//! cargo build -p quicksearch-core --example rssprobe --release
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch)
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60
//! ./target/release/examples/rssprobe $(pgrep -x quicksearch) 60 250
//! ```
//!
//! Do not optimise `VmRSS`, the figure every monitor shows: it is mostly
//! `Shared_Clean`, shared and droppable. Act on `RssAnon` (heap and thread
//! stacks, what this codebase allocates) and `Private_Dirty` (what the
//! process costs the machine). The `glibc heap` line says whether an
//! anonymous figure is live data or retention that `malloc_trim(3)` could
//! return.

mod common;

use quicksearch_core::testutil::mib;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::Region;

/// Default gap between samples. Each sample also reads `smaps`, whose cost
/// grows with the mapping count.
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
/// per-mapping breakdown comes from the last live sample, so a process
/// exiting mid-run does not report "nothing resident".
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
    // `smaps_rollup` is the kernel's own sum, one read instead of one per
    // mapping; kernels without it read as zero.
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

/// Resident bytes per mapping from `smaps`, summed by name, with glibc's
/// arenas identified.
fn breakdown(pid: u32) -> Breakdown {
    let Ok(smaps) = std::fs::read_to_string(format!("/proc/{}/smaps", pid)) else {
        return Breakdown::default();
    };
    // Totals by name, and arenas, which need the address ranges the names
    // drop.
    let regions: Vec<Region> = common::smaps_regions(&smaps).collect();
    Breakdown {
        entries: common::resident_by_name(regions.iter().map(|r| (r.name.clone(), r.rss))),
        arenas: find_arenas(&regions),
        main_heap: regions
            .iter()
            .filter(|r| r.name == "[heap]")
            .map(|r| r.rss)
            .sum(),
    }
}

/// Count glibc's non-main arenas and their resident bytes. An arena is one
/// aligned `mmap` of [`ARENA_SPAN`] that glibc commits piecewise, so `smaps`
/// usually shows it split: contiguous anonymous runs are coalesced first, and
/// a run spanning exactly one aligned span counts. A deliberate aligned
/// 64 MiB `mmap` would count too.
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
