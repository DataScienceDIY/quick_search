//! Peak-memory accounting for a full indexing run.
//!
//! [`indexprobe`](indexprobe.rs) answers "how fast"; this answers "how much
//! RAM", which is the number that decides whether indexing a large root is
//! usable on a small machine. It drives the same [`IndexingService`] the GUI
//! drives, so what it measures is the indexer's own footprint with no window,
//! no renderer and no GL context in the total.
//!
//! ```text
//! cargo build -p quicksearch-core --example memprobe --release
//! ./target/release/examples/memprobe cold /media/shared /var/tmp/qs-mem/index.db
//! ./target/release/examples/memprobe warm /media/shared /var/tmp/qs-mem/index.db
//! ./target/release/examples/memprobe cold /media/shared /var/tmp/qs-mem/index.db 10
//! ```
//!
//! The optional trailing number is the sampling interval in milliseconds
//! (default 100). Drop it to single digits to name the file a spike happened
//! on: at 100 ms the extractor has moved on by the time RSS is read, so the
//! file the timeline shows beside a spike is only approximately the cause.
//!
//! `cold` deletes the database first: every file is new, so the walk hashes
//! and extracts all of them and `existing_files` starts empty. `warm` re-runs
//! against the finished database, which is the case that loads one
//! `existing_files` entry per indexed path up front — the allocation that
//! scales with tree size rather than with in-flight work.
//!
//! Two peaks are reported and they measure different things:
//!
//! - **VmHWM** is the kernel's own high-water mark for resident set size. It
//!   cannot miss a spike, so it is the number to quote.
//! - **sampled peak** comes from polling `/proc/self/statm`, and exists only
//!   to say *when* the peak happened. The timeline it prints attributes the
//!   peak to the walk or to extraction; a sampled peak far under VmHWM means
//!   the real spike was shorter than the sampling interval.
//!
//! RSS counts the page cache backing the mmap'd database, so the figure is a
//! ceiling on what the process needs, not a floor on what it must have: those
//! pages are evictable under pressure. `/usr/bin/time -v` on this binary
//! reports the same VmHWM, as a cross-check that nothing here is fooling
//! itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::indexing::{IndexingService, IndexingStatus, RootPhase};

/// Default RSS sampling interval. Cheap (one small `/proc` read), so this is
/// set by how fine-grained the timeline should be rather than by overhead.
const DEFAULT_SAMPLE_MS: u64 = 100;

/// How often the completion marker is checked, in milliseconds. Rarer than
/// sampling because each check opens a connection to the database being
/// written, and a finer sampling interval must not turn into more of them.
const MARKER_INTERVAL_MS: u64 = 500;

/// Wall-clock ceiling. A 100k-file tree indexes in minutes; anything past
/// this is a hang, and reporting a peak for a run that never finished would
/// be worse than failing.
const TIMEOUT: Duration = Duration::from_secs(3 * 3600);

/// Resident bytes at the peak, grouped by what the mapping is.
///
/// A peak figure alone cannot be acted on: 60 MiB of heap is a buffer to
/// size down, 60 MiB of file-backed pages is page cache the kernel will
/// drop under pressure, and 60 MiB of thread stacks is a pool that is too
/// wide. `smaps` is the only place that distinction is visible.
#[derive(Default, Clone)]
struct Breakdown {
    entries: Vec<(String, u64)>,
}

/// One RSS reading with the progress that produced it.
struct Sample {
    at: Duration,
    rss: u64,
    walked: usize,
    extracted: usize,
    phase: &'static str,
    /// What extraction was working on. A peak that a single file causes is
    /// a different problem from one that grows with the tree, and this is
    /// what tells the two apart.
    file: String,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let (Some(root), Some(db)) = (args.next(), args.next()) else {
        eprintln!("usage: memprobe <cold|warm> <root> <db> [sample_ms]");
        std::process::exit(2);
    };
    if mode != "cold" && mode != "warm" {
        eprintln!("usage: memprobe <cold|warm> <root> <db> [sample_ms]");
        std::process::exit(2);
    }
    let interval = Duration::from_millis(
        args.next()
            .map(|s| s.parse().expect("sample_ms must be a number"))
            .unwrap_or(DEFAULT_SAMPLE_MS)
            .max(1),
    );
    let db = PathBuf::from(db);

    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).expect("create database directory");
    }
    if mode == "cold" {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", db.display(), suffix));
        }
    }

    run(&mode, &root, &db, interval);
}

fn run(mode: &str, root: &str, db: &Path, interval: Duration) {
    let config = Config::default();

    // Cleared for the same reason indexprobe clears it: the marker is the
    // only unambiguous completion signal, and a stale one from the previous
    // run would end this one immediately.
    if db.exists() {
        let conn = rusqlite::Connection::open(db).expect("open db");
        conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
            .expect("clear marker");
    }

    let baseline = rss().expect("read /proc/self/statm");
    eprintln!(
        "memprobe {}: root={} db={}\n  baseline RSS {} (process before indexing starts)",
        mode,
        root,
        db.display(),
        mib(baseline)
    );

    let service = IndexingService::new();
    let start = Instant::now();
    service
        .start_indexing(
            vec![root.to_string()],
            db.to_string_lossy().into_owned(),
            config,
        )
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
        // Only on a new high: reading smaps costs far more than statm, and
        // the breakdown is only wanted for the sample that sets the peak.
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

        if ticks % marker_every == 0 && db.exists() {
            if let Ok(conn) = rusqlite::Connection::open(db) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
    }
    let elapsed = start.elapsed();

    // Read before stopping: the peak belongs to the run, and stopping frees
    // nothing that VmHWM would forget anyway.
    let hwm = vm_hwm();
    assert!(done, "indexing did not finish within {:?}", TIMEOUT);
    service.stop_indexing().expect("stop");

    report(mode, elapsed, baseline, hwm, &samples, db, interval, &at_peak);
}

/// Flatten per-root progress into one line's worth of numbers. Roots are
/// summed: the process has one address space, so a per-root split would not
/// explain a peak that several roots contribute to at once.
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

fn report(
    mode: &str,
    elapsed: Duration,
    baseline: u64,
    hwm: Option<u64>,
    samples: &[Sample],
    db: &Path,
    interval: Duration,
    at_peak: &Breakdown,
) {
    // One line per 5% of the run, so the shape is visible at any duration.
    let step = (samples.len() / 20).max(1);
    eprintln!("\n  {:>8}  {:>10}  {:>9}  {:>10}  {}", "t", "RSS", "walked", "extracted", "phase");
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
    // The maximum, not the last: progress reads zero again once the service
    // returns to Idle, and the final sample is usually that one.
    let files = samples.iter().map(|s| s.walked).max().unwrap_or(0);
    let db_bytes = db_size(db);

    eprintln!("\n{} run: {:.1}s, {} files walked", mode, elapsed.as_secs_f64(), files);
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
    if let Some(h) = hwm {
        if files > 0 {
            eprintln!(
                "  growth per file   {:.0} bytes ((peak - baseline) / files walked)",
                h.saturating_sub(baseline) as f64 / files as f64
            );
        }
    }
    eprintln!("  database on disk  {} (counts toward RSS as page cache)", mib(db_bytes));

    if !at_peak.entries.is_empty() {
        eprintln!("\n  resident bytes at the peak, by mapping:");
        for (name, bytes) in at_peak.entries.iter().take(8) {
            eprintln!("    {:>10}  {}", mib(*bytes), name);
        }
    }

    // A transient spike is the failure mode a peak figure hides: steady-state
    // use can be modest while one file briefly doubles it. Ranking the
    // sample-to-sample rises names the files that do it.
    let mut jumps: Vec<(u64, &Sample)> = samples
        .windows(2)
        .map(|w| (w[1].rss.saturating_sub(w[0].rss), &w[1]))
        .filter(|(delta, _)| *delta > 4 * 1024 * 1024)
        .collect();
    jumps.sort_by_key(|(delta, _)| std::cmp::Reverse(*delta));
    if !jumps.is_empty() {
        eprintln!("\n  largest RSS rises between samples ({:?} apart):", interval);
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

/// Resident bytes per mapping from `/proc/self/smaps`, summed by name.
///
/// The name is the mapping's path, or `[heap]`/`[stack]` for the ones the
/// kernel labels. Everything else is anonymous — thread stacks and any
/// large `malloc` that went to `mmap` rather than the main arena — and is
/// bucketed by size class, because individually they are unnamed and there
/// can be hundreds of them.
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

/// The name for a `smaps` header line, or `None` if the line is not one.
///
/// A header is `addr-addr perms offset dev inode [path]`. Anonymous mappings
/// have inode 0 and no path; they are bucketed by size so that a hundred
/// 8 MiB regions read as one line rather than a hundred.
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

/// Power-of-two bucket, so mappings group by what allocated them rather
/// than by their exact size.
fn size_class(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib < 1.0 {
        "< 1 MiB".to_string()
    } else {
        let bucket = 1u64 << (63 - (bytes / (1024 * 1024)).leading_zeros() as u64);
        format!("~{} MiB", bucket)
    }
}

/// The kernel's peak RSS for this process, from `/proc/self/status`.
fn vm_hwm() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// 4 KiB everywhere this runs. Reading it from `sysconf` would mean a libc
/// dependency for a constant that has never differed on the targets that
/// have `/proc`.
fn page_size() -> u64 {
    4096
}

/// The index plus its WAL: the WAL is where a run's writes sit until the
/// next checkpoint, so leaving it out understates a run in progress.
fn db_size(db: &Path) -> u64 {
    ["", "-wal"]
        .iter()
        .filter_map(|s| std::fs::metadata(format!("{}{}", db.display(), s)).ok())
        .map(|m| m.len())
        .sum()
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}
