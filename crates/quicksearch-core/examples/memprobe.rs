//!
//! [`indexprobe`](indexprobe.rs) answers "how fast"; this answers "how
//! much RAM", driving the same [`IndexingService`] the GUI drives with no
//! window or GL context in the total. For the idle footprint of a running
//! GUI, use [`rssprobe`](rssprobe.rs).
//!
//! ```text
//! cargo build -p quicksearch-core --example memprobe --release
//! ./target/release/examples/memprobe cold /media/shared /var/tmp/qs-mem/index.db
//! ./target/release/examples/memprobe warm /media/shared /var/tmp/qs-mem/index.db
//! ./target/release/examples/memprobe cold ~ /var/tmp/qs-mem/index.db 250 probe.toml
//! ```
//!
//! **Roots are comma-separated**, because per-root state is what multiplies:
//! a one-root run cannot show the buffers that exist once per pipeline.
//!
//! ```text
//! ./target/release/examples/memprobe cold /media/shared,/home/me,/usr /var/tmp/qs-mem/index.db
//! ```
//!
//! Trailing arguments: sampling interval in ms (default 100) and a config
//! file. `cold` deletes the database first; `warm` re-runs against the
//! finished one. Reading the report: **VmHWM** cannot miss a spike — quote
//! it; the sampled peak only says *when*. `growth per file` is a ratio, not
//! a per-file cost. Nothing reported is evictable page cache. `settled RSS`
//! is what a long-lived process keeps — glibc frees to its arena, so
//! without `release_free_heap` the peak becomes the floor.
//!
//! Built with `--features probe` the indexer also prints a `census` line
//! naming what its run-scoped structures hold, which is the half `smaps`
//! cannot answer: a mapping is "heap", never "the stale-candidate list".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::indexing::{IndexingService, IndexingStatus, RootPhase};
use quicksearch_core::testutil::{mib, size_class};

mod common;

const DEFAULT_SAMPLE_MS: u64 = 100;

/// Rarer than sampling: each check opens a connection to the live database.
const MARKER_INTERVAL_MS: u64 = 500;

/// Past this is a hang; a peak for a run that never finished would be worse
/// than failing.
const TIMEOUT: Duration = Duration::from_secs(3 * 3600);

/// Generous: the wait is for `VACUUM` on a multi-gigabyte index, not for
/// anything that scales with the sampling interval.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(600);

/// The writer releases its connection and returns pages *after* setting
/// `Idle`; sampling the instant it flips would miss the thing measured.
const SETTLE_QUIET: Duration = Duration::from_secs(2);

/// Resident bytes at the peak, grouped: heap, droppable file-backed cache
/// and thread stacks each want different fixes — only `smaps` shows which.
#[derive(Default, Clone)]
struct Breakdown {
    entries: Vec<(String, u64)>,
}

struct Sample {
    at: Duration,
    rss: u64,
    walked: usize,
    extracted: usize,
    phase: &'static str,
    /// What extraction was working on: a single-file peak is a different
    /// problem from one growing with the tree.
    file: String,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let (Some(roots), Some(db)) = (args.next(), args.next()) else {
        eprintln!("usage: memprobe <cold|warm> <root[,root...]> <db> [sample_ms] [config.toml]");
        std::process::exit(2);
    };
    if mode != "cold" && mode != "warm" {
        eprintln!("usage: memprobe <cold|warm> <root[,root...]> <db> [sample_ms] [config.toml]");
        std::process::exit(2);
    }
    let roots: Vec<String> = roots
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect();
    if roots.is_empty() {
        eprintln!("memprobe: no roots given");
        std::process::exit(2);
    }
    let interval = Duration::from_millis(
        args.next()
            .map(|s| s.parse().expect("sample_ms must be a number"))
            .unwrap_or(DEFAULT_SAMPLE_MS)
            .max(1),
    );
    let config_path = args.next().map(PathBuf::from);
    let db = PathBuf::from(db);

    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).expect("create database directory");
    }
    if mode == "cold" {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", db.display(), suffix));
        }
    }

    run(&mode, &roots, &db, interval, config_path.as_deref());
}

fn run(mode: &str, roots: &[String], db: &Path, interval: Duration, config_path: Option<&Path>) {
    // A config file only supplies the knobs that change *what* indexing
    // does; the defaults keep runs comparable.
    let config = match config_path {
        Some(p) => Config::load_from(p).expect("load probe config"),
        None => Config::default(),
    };

    // A stale marker from the previous run would end this one immediately.
    if db.exists() {
        let conn = rusqlite::Connection::open(db).expect("open db");
        conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
            .expect("clear marker");
    }

    let baseline = rss().expect("read /proc/self/statm");
    eprintln!(
        "memprobe {}: {} root(s)={} db={}\n  baseline RSS {} (process before indexing starts)",
        mode,
        roots.len(),
        roots.join(" "),
        db.display(),
        mib(baseline)
    );

    let service = IndexingService::new();
    let start = Instant::now();
    service
        .start_indexing(roots.to_vec(), db.to_string_lossy().into_owned(), config)
        .expect("start indexing");

    let mut samples: Vec<Sample> = Vec::new();
    let deadline = start + TIMEOUT;
    let marker_every = (MARKER_INTERVAL_MS / interval.as_millis().max(1) as u64).max(1) as u32;
    let mut ticks: u32 = 0;
    let mut done = false;
    let mut high = 0u64;
    let mut at_peak = Breakdown::default();

    while Instant::now() < deadline {
        std::thread::sleep(interval);
        ticks += 1;

        let status = service.get_status();
        if let IndexingStatus::Error(e) = &status {
            panic!("indexing failed: {}", e);
        }
        let (walked, extracted, phase, file) = progress(&status);
        let now = rss().unwrap_or(0);
        // Only on a new high: smaps costs far more than statm.
        if now > high {
            high = now;
            at_peak = breakdown();
        }
        samples.push(Sample {
            at: start.elapsed(),
            rss: now,
            walked,
            extracted,
            phase,
            file,
        });

        if ticks.is_multiple_of(marker_every) && db.exists() {
            if let Ok(conn) = rusqlite::Connection::open(db) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
    }
    let elapsed = start.elapsed();

    let hwm = common::vm_hwm();
    assert!(done, "indexing did not finish within {:?}", TIMEOUT);

    let settled = settle(&service);
    service.stop_indexing().expect("stop");

    report(
        mode, elapsed, baseline, hwm, settled, &samples, db, interval, &at_peak,
    );
}

/// The peak says what indexing needs; this says what it *keeps* — `free`
/// under glibc returns to the arena, not the kernel, so this is the
/// process's floor. The final release lands even after `Idle` is published,
/// hence the wait, and then a little longer.
fn settle(service: &IndexingService) -> u64 {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(service.get_status(), IndexingStatus::Idle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(SETTLE_QUIET);
    rss().unwrap_or(0)
}

/// Flatten per-root progress into one line: the process has one address
/// space, so a per-root split would not explain a shared peak.
fn progress(status: &IndexingStatus) -> (usize, usize, &'static str, String) {
    let IndexingStatus::Running { roots, .. } = status else {
        return (0, 0, "-", String::new());
    };
    let walked = roots.iter().map(|r| r.walked).sum();
    let extracted = roots.iter().map(|r| r.extracted).sum();
    // The whole run's phase is the least-advanced root's: while any root is
    // still walking, walk-sized allocations are still live.
    let phase = if roots.iter().any(|r| r.phase == RootPhase::Walking) {
        "walk"
    } else if roots.iter().any(|r| r.phase == RootPhase::Extracting) {
        "extract"
    } else {
        "done"
    };
    let file = roots
        .iter()
        .find_map(|r| r.current_file.clone())
        .unwrap_or_default();
    (walked, extracted, phase, file)
}

#[allow(clippy::too_many_arguments)]
fn report(
    mode: &str,
    elapsed: Duration,
    baseline: u64,
    hwm: Option<u64>,
    settled: u64,
    samples: &[Sample],
    db: &Path,
    interval: Duration,
    at_peak: &Breakdown,
) {
    // One line per 5% of the run, so the shape is visible at any duration.
    let step = (samples.len() / 20).max(1);
    eprintln!(
        "\n  {:>8}  {:>10}  {:>9}  {:>10}  phase",
        "t", "RSS", "walked", "extracted"
    );
    for s in samples.iter().step_by(step) {
        eprintln!(
            "  {:>7.1}s  {:>10}  {:>9}  {:>10}  {}",
            s.at.as_secs_f64(),
            mib(s.rss),
            s.walked,
            s.extracted,
            s.phase
        );
    }

    let peak = samples.iter().max_by_key(|s| s.rss);
    // The maximum, not the last: progress reads zero again at Idle.
    let files = samples.iter().map(|s| s.walked).max().unwrap_or(0);
    let db_bytes = db_size(db);

    eprintln!(
        "\n{} run: {:.1}s, {} files walked",
        mode,
        elapsed.as_secs_f64(),
        files
    );
    match hwm {
        Some(h) => eprintln!("  peak RSS (VmHWM)  {}", mib(h)),
        None => eprintln!("  peak RSS (VmHWM)  unavailable"),
    }
    if let Some(p) = peak {
        eprintln!(
            "  sampled peak      {} at t={:.1}s during {} ({} walked, {} extracted){}",
            mib(p.rss),
            p.at.as_secs_f64(),
            p.phase,
            p.walked,
            p.extracted,
            if p.file.is_empty() {
                String::new()
            } else {
                format!("\n                    on {}", p.file)
            }
        );
    }
    eprintln!("  baseline RSS      {}", mib(baseline));
    eprintln!(
        "  settled RSS       {} (once idle: what the run kept, not what it needed)",
        mib(settled)
    );
    if let Some(h) = hwm {
        eprintln!(
            "  returned          {} of the {} the run took above baseline",
            mib(h.saturating_sub(settled)),
            mib(h.saturating_sub(baseline))
        );
    }
    if let Some(h) = hwm {
        if files > 0 {
            eprintln!(
                "  growth per file   {:.0} bytes ((peak - baseline) / files walked)",
                h.saturating_sub(baseline) as f64 / files as f64
            );
        }
    }
    eprintln!(
        "  database on disk  {} (counts toward RSS as page cache)",
        mib(db_bytes)
    );

    if !at_peak.entries.is_empty() {
        eprintln!("\n  resident bytes at the peak, by mapping:");
        for (name, bytes) in at_peak.entries.iter().take(8) {
            eprintln!("    {:>10}  {}", mib(*bytes), name);
        }
    }

    // A transient spike is what a peak figure hides; ranking
    // sample-to-sample rises names the files that cause it.
    let mut jumps: Vec<(u64, &Sample)> = samples
        .windows(2)
        .map(|w| (w[1].rss.saturating_sub(w[0].rss), &w[1]))
        .filter(|(delta, _)| *delta > 4 * 1024 * 1024)
        .collect();
    jumps.sort_by_key(|(delta, _)| std::cmp::Reverse(*delta));
    if !jumps.is_empty() {
        eprintln!(
            "\n  largest RSS rises between samples ({:?} apart):",
            interval
        );
        for (delta, s) in jumps.iter().take(8) {
            eprintln!(
                "    +{:>9} to {:>10} at t={:>6.1}s  {}  {}",
                mib(*delta),
                mib(s.rss),
                s.at.as_secs_f64(),
                s.phase,
                s.file
            );
        }
    }
}

/// Resident set size now, from `/proc/self/statm` field 2 (resident pages).
fn rss() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size())
}

/// Resident bytes per mapping from `/proc/self/smaps`, summed by name;
/// anonymous mappings are bucketed by size class — individually unnamed,
/// and there can be hundreds.
fn breakdown() -> Breakdown {
    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") else {
        return Breakdown::default();
    };
    let mut by_name: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut current = String::new();

    for line in smaps.lines() {
        if let Some(rss_kib) = line.strip_prefix("Rss:") {
            let kib: u64 = rss_kib
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            *by_name.entry(current.clone()).or_default() += kib * 1024;
        } else if let Some(header) = parse_map_header(line) {
            current = header;
        }
    }

    let mut entries: Vec<(String, u64)> = by_name.into_iter().filter(|(_, b)| *b > 0).collect();
    entries.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    Breakdown { entries }
}

/// The name for a `smaps` header line, or `None`. Anonymous mappings are
/// bucketed by size so a hundred 8 MiB regions read as one line.
fn parse_map_header(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let (lo, hi) = range.split_once('-')?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    // Fields 2-5 are perms, offset, dev, inode; anything after is the path.
    let path = fields.nth(4).unwrap_or("");
    if !path.is_empty() {
        return Some(path.to_string());
    }
    Some(format!("anon {}", size_class(hi.saturating_sub(lo))))
}

/// 4 KiB everywhere this runs; `sysconf` would mean a libc dependency for a
/// constant that never differs where `/proc` exists.
fn page_size() -> u64 {
    4096
}

/// The index plus its WAL — leaving the WAL out understates a run in progress.
fn db_size(db: &Path) -> u64 {
    ["", "-wal"]
        .iter()
        .filter_map(|s| std::fs::metadata(format!("{}{}", db.display(), s)).ok())
        .map(|m| m.len())
        .sum()
}
