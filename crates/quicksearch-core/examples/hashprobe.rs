//! The index identifies a file as `sha256(size ‖ first hash_length bytes)`,
//! so one number decides bytes read per file and how often two different
//! files are declared duplicates. Collisions are measured over `1..32 KiB`
//! against the 32 KiB baseline — right **by definition**; `--verify` is the
//! footnote that reads baseline groups byte for byte and reports how many
//! of *those* were wrong too.
//!
//! ```text
//! cargo build -p quicksearch-core --example hashprobe --release
//! ./target/release/examples/hashprobe collide /media/shared --workers 8 --verify
//! ./target/release/examples/hashprobe collide /media/shared \
//!     --ignore target --ignore build --ignore __pycache__
//! ```
//!
//! Read cost per sample belongs to [`indexprobe`](indexprobe.rs); the
//! traversal walks the shipped default [`IgnoreSet`], and `--ignore` adds.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sha2::{Digest, Sha256};

use quicksearch_core::config::{Config, IgnoreSet};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{content_extractable, filtered_walk, UnreadableDirs};
use quicksearch_core::mime::guess_mime_from_head;
use quicksearch_core::textenc::looks_like_text;
use quicksearch_core::verify::{verify_identical, VerifyUpdate};

use common::{mib, Io};

/// Sample sizes, smallest first; the last is the baseline. Widening the
/// window can only *split* a group, so the six partitions are successive
/// refinements — one traversal answers all of them, and the per-sample
/// false-positive counts are monotonic ([`Sweep::check_invariants`]).
const SWEEP: [usize; 6] = [1 << 10, 2 << 10, 4 << 10, 8 << 10, 16 << 10, 32 << 10];

const BASE: usize = SWEEP.len() - 1;

/// Eight rather than the core count: round-trip bound, not CPU bound — on
/// virtiofs the thread is parked in `read` almost the whole time.
const DEFAULT_WORKERS: usize = 8;

/// Ceiling on bytes `--verify` reads; whatever it stops short of is
/// reported, not silently dropped.
const DEFAULT_VERIFY_BUDGET: u64 = 8 * 1024 * 1024 * 1024;

type Sha = [u8; 32];

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

struct Scan {
    size: u64,
    /// `sha256(size ‖ head[..n])` for each `n` in [`SWEEP`].
    digests: [Sha; SWEEP.len()],
    /// Bit `i`: the MIME guessed from `SWEEP[i]` bytes differs from the
    /// baseline's — a short sample also classifies on less evidence.
    mime_differs: u8,
    /// Bit `i`: [`looks_like_text`] disagrees with the baseline's answer.
    text_differs: u8,
    /// Whether the content pass would have had anything to extract.
    extractable: bool,
}

/// Files the traversal saw but could not use, kept apart from the results so a
/// corpus that is half unreadable cannot masquerade as a clean measurement.
#[derive(Default, Debug)]
struct ScanErrors {
    unreadable: u64,
    /// Zero-length, excluded exactly as `duplicates.rs` excludes them.
    empty: u64,
    irregular: u64,
}

/// Read one file's head once (`min(size, 32 KiB)`) and derive every sample
/// size's answer from the same bytes — one traversal measures all six,
/// guaranteed mutually consistent as six separate runs never could be.
fn scan_one(path: &Path, size: u64, config: &Config, registry: &Registry) -> Option<Scan> {
    let mut head = vec![0u8; size.min(SWEEP[BASE] as u64) as usize];
    let mut file = File::open(path).ok()?;
    // `read_exact`, like the indexer: a file that shrank between the stat and
    // this read is not a file we know the size of, so it is not scanned.
    file.read_exact(&mut head).ok()?;

    let mut digests = [[0u8; 32]; SWEEP.len()];
    for (slot, n) in digests.iter_mut().zip(SWEEP) {
        let mut hasher = Sha256::new();
        hasher.update(size.to_le_bytes());
        hasher.update(&head[..head.len().min(n)]);
        slot.copy_from_slice(&hasher.finalize());
    }

    let base_mime = guess_mime_from_head(path, &head);
    let base_text = looks_like_text(&head);
    let (mut mime_differs, mut text_differs) = (0u8, 0u8);
    for (i, n) in SWEEP.iter().enumerate().take(BASE) {
        let window = &head[..head.len().min(*n)];
        if guess_mime_from_head(path, window) != base_mime {
            mime_differs |= 1 << i;
        }
        if looks_like_text(window) != base_text {
            text_differs |= 1 << i;
        }
    }

    let extractable = size <= config.processing.maximum_text_file_size
        && content_extractable(path, base_mime.as_deref(), config, registry);

    Some(Scan {
        size,
        digests,
        mime_differs,
        text_differs,
        extractable,
    })
}

/// Traverse `roots` with the shipped defaults, `workers` at a time: a
/// bounded channel, so a million files never materialise as a vector, and
/// per-worker accumulation merged once at the end.
fn scan(
    roots: &[String],
    workers: usize,
    config: &Config,
) -> (Vec<PathBuf>, Vec<Scan>, ScanErrors) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, u64)>(4096);
    let rx = Arc::new(Mutex::new(rx));
    let done = AtomicUsize::new(0);

    let mut paths = Vec::new();
    let mut scans = Vec::new();
    let mut errors = ScanErrors::default();

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let done = &done;
            handles.push(scope.spawn(move || {
                // Building a registry per worker is cheaper than sharing one.
                let registry = Registry::default_set();
                let (mut paths, mut scans) = (Vec::new(), Vec::new());
                let mut unreadable = 0u64;
                loop {
                    // The lock is held for the handoff and never across a read.
                    let Ok((path, size)) = ({
                        let rx = rx.lock().expect("scan channel");
                        rx.recv()
                    }) else {
                        break;
                    };
                    match scan_one(&path, size, config, &registry) {
                        Some(scan) => {
                            paths.push(path);
                            scans.push(scan);
                        }
                        None => unreadable += 1,
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(25_000) {
                        eprint!("\r  scanned {} files...", n);
                    }
                }
                (paths, scans, unreadable)
            }));
        }

        let ignore = IgnoreSet::compile(&config.indexing.ignore_patterns).expect("ignore patterns");
        let failures = UnreadableDirs::default();
        for root in roots {
            for entry in filtered_walk(
                root,
                config.indexing.follow_symlinks,
                config.indexing.include_hidden,
                &ignore,
                &failures,
            ) {
                if !entry.file_type().is_file() {
                    errors.irregular += 1;
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    errors.unreadable += 1;
                    continue;
                };
                if meta.len() == 0 {
                    errors.empty += 1;
                    continue;
                }
                if tx.send((entry.into_path(), meta.len())).is_err() {
                    break;
                }
            }
        }
        drop(tx);

        for handle in handles {
            let (p, s, unreadable) = handle.join().expect("scan worker");
            paths.extend(p);
            scans.extend(s);
            errors.unreadable += unreadable;
        }
    });
    eprint!("\r");

    (paths, scans, errors)
}

// ---------------------------------------------------------------------------
// Collision accounting
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct GroupStat {
    members: u64,
    /// Distinct baseline groups merged; `1` is honest, more is false positive.
    baseline_groups: u64,
    /// `C(members, 2)`.
    reported_pairs: u64,
    /// Of those, the pairs the baseline says are different files.
    fp_pairs: u64,
    /// The Duplicates tab's reclaimable-bytes figure minus what the baseline
    /// subgroups really hold — the number that could talk someone into
    /// deleting something.
    overstated_bytes: u64,
}

/// Measure one reported group against the baseline. Pairs are counted
/// arithmetically: a hardlink farm has `C(m, 2)` pairs, a number to compute
/// and not a loop to run.
fn group_stat(members: &[u32], base: &[u32], size: &[u64]) -> GroupStat {
    #[derive(Default, Clone, Copy)]
    struct Sub {
        count: u64,
        sum: u64,
        max: u64,
    }

    let mut subs: HashMap<u32, Sub> = HashMap::new();
    let (mut sum, mut max) = (0u64, 0u64);
    for &i in members {
        let (s, b) = (size[i as usize], base[i as usize]);
        sum += s;
        max = max.max(s);
        let sub = subs.entry(b).or_default();
        sub.count += 1;
        sub.sum += s;
        sub.max = sub.max.max(s);
    }

    let m = members.len() as u64;
    let reported_pairs = m * m.saturating_sub(1) / 2;
    let true_pairs: u64 = subs.values().map(|s| s.count * (s.count - 1) / 2).sum();
    let truthful_bytes: u64 = subs.values().map(|s| s.sum - s.max).sum();

    GroupStat {
        members: m,
        baseline_groups: subs.len() as u64,
        reported_pairs,
        fp_pairs: reported_pairs - true_pairs,
        overstated_bytes: (sum - max) - truthful_bytes,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Collisions {
    groups: u64,
    files: u64,
    reported_pairs: u64,
    fp_groups: u64,
    fp_files: u64,
    fp_pairs: u64,
    overstated_bytes: u64,
}

/// Sum over a partition; groups of one are skipped, exactly as the
/// `HAVING cnt > 1` in `search::duplicates` skips them.
fn tally(partition: &[Vec<u32>], base: &[u32], size: &[u64]) -> Collisions {
    let mut total = Collisions::default();
    for members in partition {
        if members.len() < 2 {
            continue;
        }
        let stat = group_stat(members, base, size);
        total.groups += 1;
        total.files += stat.members;
        total.reported_pairs += stat.reported_pairs;
        if stat.baseline_groups > 1 {
            total.fp_groups += 1;
            total.fp_files += stat.members;
            total.fp_pairs += stat.fp_pairs;
            total.overstated_bytes += stat.overstated_bytes;
        }
    }
    total
}

fn partition(scans: &[Scan], which: usize) -> Vec<Vec<u32>> {
    let mut by_digest: HashMap<Sha, Vec<u32>> = HashMap::new();
    for (i, scan) in scans.iter().enumerate() {
        by_digest
            .entry(scan.digests[which])
            .or_default()
            .push(i as u32);
    }
    by_digest.into_values().collect()
}

fn baseline_ids(scans: &[Scan]) -> Vec<u32> {
    let mut ids = vec![0u32; scans.len()];
    let mut next: HashMap<Sha, u32> = HashMap::new();
    for (i, scan) in scans.iter().enumerate() {
        let n = next.len() as u32;
        ids[i] = *next.entry(scan.digests[BASE]).or_insert(n);
    }
    ids
}

struct Sweep {
    rows: Vec<Collisions>,
}

impl Sweep {
    /// Invariants that hold by construction, checked because a probe that
    /// silently reports a plausible wrong number is worse than no probe:
    /// the baseline's false-positive columns are exactly zero, and every
    /// "wrong" column is non-increasing as the sample refines.
    fn check_invariants(&self) {
        let base = &self.rows[BASE];
        assert_eq!(base.fp_groups, 0, "the baseline defines truth");
        assert_eq!(base.fp_files, 0, "the baseline defines truth");
        assert_eq!(base.fp_pairs, 0, "the baseline defines truth");
        assert_eq!(base.overstated_bytes, 0, "the baseline defines truth");

        for pair in self.rows.windows(2) {
            let (small, large) = (&pair[0], &pair[1]);
            assert!(
                large.files <= small.files,
                "a wider window cannot merge more files"
            );
            assert!(
                large.reported_pairs <= small.reported_pairs,
                "a wider window cannot report more pairs"
            );
            assert!(
                large.fp_files <= small.fp_files,
                "false positives must shrink"
            );
            assert!(
                large.fp_pairs <= small.fp_pairs,
                "false positives must shrink"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// collide
// ---------------------------------------------------------------------------

fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p > 0.0 && p < 0.01 {
        return "<0.01%".to_string();
    }
    format!("{:.2}%", p)
}

fn sample_name(n: usize) -> String {
    if n.is_multiple_of(1024) {
        format!("{} KiB", n / 1024)
    } else {
        format!("{} B", n)
    }
}

fn collide(roots: &[String], workers: usize, ignore: &[String], verify_budget: Option<u64>) {
    let mut config = Config::default();
    // `--ignore` is not a convenience: build output is where a disk keeps
    // its near-identical files, and a corpus including it answers a
    // different question. Both answers are wanted.
    config
        .indexing
        .ignore_patterns
        .extend(ignore.iter().cloned());
    eprintln!(
        "scanning {} root(s) with {} workers, {} per file, ignoring {:?}",
        roots.len(),
        workers,
        sample_name(SWEEP[BASE]),
        config.indexing.ignore_patterns,
    );

    let io_start = Io::read();
    let start = Instant::now();
    let (paths, scans, errors) = scan(roots, workers, &config);
    let elapsed = start.elapsed();
    let io = Io::read().since(&io_start);

    let sizes: Vec<u64> = scans.iter().map(|s| s.size).collect();
    let base = baseline_ids(&scans);

    eprintln!(
        "\nscanned {} files in {:.1?} ({:.0} files/sec), read {} in {} calls",
        scans.len(),
        elapsed,
        scans.len() as f64 / elapsed.as_secs_f64(),
        mib(io.rchar),
        io.syscr,
    );
    eprintln!(
        "skipped {} empty, {} irregular (symlink/FIFO/device), {} unreadable",
        errors.empty, errors.irregular, errors.unreadable,
    );
    eprintln!(
        "total bytes indexed-as-content: {} across {} distinct baseline groups\n",
        mib(sizes.iter().sum::<u64>()),
        base.iter().max().map(|m| m + 1).unwrap_or(0),
    );

    let sweep = Sweep {
        rows: (0..SWEEP.len())
            .map(|i| tally(&partition(&scans, i), &base, &sizes))
            .collect(),
    };
    sweep.check_invariants();

    let files = scans.len() as u64;
    println!(
        "### Duplicate grouping vs. the {} baseline\n",
        sample_name(SWEEP[BASE])
    );
    println!(
        "| sample | dup groups | dup files | reported pairs | FP groups | FP files | FP pairs | FP rate (pairs) | FP rate (files) | overstated |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (i, row) in sweep.rows.iter().enumerate() {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            sample_name(SWEEP[i]),
            row.groups,
            row.files,
            row.reported_pairs,
            row.fp_groups,
            row.fp_files,
            row.fp_pairs,
            pct(row.fp_pairs, row.reported_pairs),
            pct(row.fp_files, files),
            mib(row.overstated_bytes),
        );
    }

    println!("\n### Where the false positives go when the window grows\n");
    println!("| step | FP pairs resolved | FP files resolved | overstated bytes resolved |");
    println!("|---|---:|---:|---:|");
    for i in 0..BASE {
        let (small, large) = (&sweep.rows[i], &sweep.rows[i + 1]);
        println!(
            "| {} -> {} | {} | {} | {} |",
            sample_name(SWEEP[i]),
            sample_name(SWEEP[i + 1]),
            small.fp_pairs - large.fp_pairs,
            small.fp_files - large.fp_files,
            mib(small.overstated_bytes - large.overstated_bytes),
        );
    }

    // The head's other two jobs: how much evidence the MIME and text sniffs
    // get, and how many files the walk can finish inline.
    println!("\n### What else the window decides\n");
    println!("| sample | inlinable files | share | MIME differs | text sniff differs |");
    println!("|---:|---:|---:|---:|---:|");
    for (i, n) in SWEEP.iter().enumerate() {
        let inlinable = scans
            .iter()
            .filter(|s| s.extractable && s.size <= *n as u64)
            .count() as u64;
        let (mime, text) = if i == BASE {
            (0, 0)
        } else {
            (
                scans
                    .iter()
                    .filter(|s| s.mime_differs & (1 << i) != 0)
                    .count() as u64,
                scans
                    .iter()
                    .filter(|s| s.text_differs & (1 << i) != 0)
                    .count() as u64,
            )
        };
        println!(
            "| {} | {} | {} | {} | {} |",
            sample_name(*n),
            inlinable,
            pct(inlinable, files),
            mime,
            text,
        );
    }

    top_offenders(&scans, &paths, &base, &sizes);

    if let Some(budget) = verify_budget {
        verify_baseline(&scans, &paths, &sizes, budget);
    }
}

/// The five worst groups at the default 8 KiB: *what* collides.
fn top_offenders(scans: &[Scan], paths: &[PathBuf], base: &[u32], sizes: &[u64]) {
    let default_index = SWEEP
        .iter()
        .position(|n| *n == Config::default().processing.hash_length)
        .unwrap_or(BASE);

    let mut worst: Vec<(GroupStat, Vec<u32>)> = partition(scans, default_index)
        .into_iter()
        .filter(|members| members.len() >= 2)
        .map(|members| (group_stat(&members, base, sizes), members))
        .filter(|(stat, _)| stat.baseline_groups > 1)
        .collect();
    worst.sort_by_key(|(stat, _)| std::cmp::Reverse((stat.overstated_bytes, stat.members)));

    println!(
        "\n### Worst false-positive groups at the {} default\n",
        sample_name(SWEEP[default_index])
    );
    if worst.is_empty() {
        println!("None: every group the default reports is real.");
        return;
    }
    for (stat, members) in worst.iter().take(5) {
        println!(
            "- {} members from {} distinct files, {} each, {} overstated",
            stat.members,
            stat.baseline_groups,
            mib(sizes[members[0] as usize]),
            mib(stat.overstated_bytes),
        );
        // Two members from *different* baseline groups — the pair actually
        // claimed wrongly, what a reader would run `cmp` on.
        let first = members[0];
        if let Some(other) = members
            .iter()
            .find(|m| base[**m as usize] != base[first as usize])
        {
            println!("  - {}", paths[first as usize].display());
            println!("  - {}", paths[*other as usize].display());
        }
    }
}

/// Read the baseline's own duplicate groups byte for byte and report how
/// many were wrong — only groups holding a file larger than the sample can
/// be. Opt-in and budgeted: the one part of this probe that reads whole files.
fn verify_baseline(scans: &[Scan], paths: &[PathBuf], sizes: &[u64], budget: u64) {
    let baseline = partition(scans, BASE);
    let baseline_dup_groups = baseline.iter().filter(|m| m.len() >= 2).count();
    let mut groups: Vec<Vec<u32>> = baseline
        .into_iter()
        .filter(|m| m.len() >= 2 && sizes[m[0] as usize] > SWEEP[BASE] as u64)
        .collect();
    // Digest order: uniformly random and stable between runs.
    // Largest-claim-first would spend the budget on static libraries and
    // disk images — long identical headers — and inflate the rate severalfold.
    groups.sort_by_key(|m| scans[m[0] as usize].digests[BASE]);

    let cancel = AtomicBool::new(false);
    let (mut checked, mut wrong, mut differing, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let (mut skipped, mut skipped_bytes) = (0u64, 0u64);
    let mut examples: Vec<String> = Vec::new();

    let mut exhausted = false;
    for members in &groups {
        let cost: u64 = members.iter().map(|m| sizes[*m as usize]).sum();
        // Stop dead at the budget: carrying on admits whatever still fits —
        // *small* groups — and the sample quietly becomes weighted toward
        // files the baseline is least likely to get wrong.
        exhausted |= bytes + cost > budget;
        if exhausted {
            skipped += 1;
            skipped_bytes += cost;
            continue;
        }
        let group: Vec<PathBuf> = members.iter().map(|m| paths[*m as usize].clone()).collect();
        let mut report = None;
        verify_identical(&group, &cancel, &mut |update| {
            if let VerifyUpdate::Done(r) = update {
                report = Some(r);
            }
        });
        let Some(report) = report else { continue };
        checked += 1;
        bytes += report.bytes_read;
        if !report.all_identical() {
            wrong += 1;
            differing += report.differing() as u64;
            if examples.len() < 5 {
                examples.push(format!(
                    "{} ({} members, {} differ)",
                    group[0].display(),
                    group.len(),
                    report.differing()
                ));
            }
        }
        if checked % 100 == 0 {
            eprint!("\r  verified {} groups, {}...", checked, mib(bytes));
        }
    }
    eprint!("\r");

    println!("\n### Is the baseline itself right?\n");
    println!(
        "{} of {} baseline groups hold a file larger than {}, so only those can be wrong.",
        groups.len(),
        baseline_dup_groups,
        sample_name(SWEEP[BASE]),
    );
    println!(
        "Compared {} of them byte for byte, sampled in digest order ({} read): \
         **{} were not identical** ({} of the sample), {} members differing.",
        checked,
        mib(bytes),
        wrong,
        pct(wrong, checked),
        differing,
    );
    if skipped > 0 {
        println!(
            "{} groups ({}) went unchecked against the {} budget — not counted either way.",
            skipped,
            mib(skipped_bytes),
            mib(budget),
        );
    }
    for example in &examples {
        println!("- {}", example);
    }
}

// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or_default();

    let mut positional = Vec::new();
    let mut workers = DEFAULT_WORKERS;
    let mut verify_budget = None;
    let mut ignore = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--workers" => workers = rest.next().and_then(|v| v.parse().ok()).unwrap_or(workers),
            "--ignore" => ignore.extend(rest.next().cloned()),
            "--verify" => verify_budget = Some(verify_budget.unwrap_or(DEFAULT_VERIFY_BUDGET)),
            "--verify-max-bytes" => {
                verify_budget = rest.next().and_then(|v| v.parse().ok()).or(verify_budget)
            }
            other => positional.push(other.to_string()),
        }
    }

    match mode {
        "collide" if !positional.is_empty() => {
            collide(&positional, workers.max(1), &ignore, verify_budget);
        }
        _ => {
            eprintln!(
                "usage:\n  \
                 hashprobe collide <root>... [--workers N] [--ignore PATTERN]... \
                 [--verify] [--verify-max-bytes B]"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force the pair counts [`group_stat`] shortcuts, so the shortcut
    /// is checked against the definition rather than itself.
    fn brute_force_pairs(members: &[u32], base: &[u32]) -> (u64, u64) {
        let (mut reported, mut fp) = (0u64, 0u64);
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                reported += 1;
                if base[*a as usize] != base[*b as usize] {
                    fp += 1;
                }
            }
        }
        (reported, fp)
    }

    /// [`group_stat`] across the group shapes that matter, cross-checked
    /// against [`brute_force_pairs`].
    #[test]
    fn group_stat_matches_enumeration_across_group_shapes() {
        struct Case {
            label: &'static str,
            members: Vec<u32>,
            base: Vec<u32>,
            sizes: Vec<u64>,
            want: GroupStat,
        }
        let cases = [
            Case {
                label: "honest",
                members: vec![0, 1, 2],
                base: vec![7, 7, 7],
                sizes: vec![100, 100, 100],
                want: GroupStat {
                    members: 3,
                    baseline_groups: 1,
                    reported_pairs: 3,
                    fp_pairs: 0,
                    overstated_bytes: 0,
                },
            },
            Case {
                label: "mixed",
                members: vec![0, 1, 2, 3],
                base: vec![1, 1, 2, 2],
                sizes: vec![100; 4],
                want: GroupStat {
                    members: 4,
                    baseline_groups: 2,
                    reported_pairs: 6,
                    fp_pairs: 4,
                    overstated_bytes: 100,
                },
            },
            Case {
                label: "all singletons",
                members: vec![0, 1, 2, 3],
                base: vec![1, 2, 3, 4],
                sizes: vec![500; 4],
                want: GroupStat {
                    members: 4,
                    baseline_groups: 4,
                    reported_pairs: 6,
                    fp_pairs: 6,
                    overstated_bytes: 1500,
                },
            },
            Case {
                label: "lumpy",
                members: (0..10).collect(),
                base: vec![0, 0, 0, 1, 1, 2, 3, 3, 3, 3],
                sizes: vec![64; 10],
                want: GroupStat {
                    members: 10,
                    baseline_groups: 4,
                    reported_pairs: 45,
                    fp_pairs: 45 - (3 + 1 + 0 + 6),
                    overstated_bytes: 3 * 64,
                },
            },
            Case {
                label: "empty",
                members: vec![],
                base: vec![],
                sizes: vec![],
                want: GroupStat::default(),
            },
            Case {
                label: "lone",
                members: vec![0],
                base: vec![5],
                sizes: vec![900],
                want: GroupStat {
                    members: 1,
                    baseline_groups: 1,
                    reported_pairs: 0,
                    fp_pairs: 0,
                    overstated_bytes: 0,
                },
            },
        ];
        for c in &cases {
            let stat = group_stat(&c.members, &c.base, &c.sizes);
            assert_eq!(stat, c.want, "{}", c.label);
            let (reported, fp) = brute_force_pairs(&c.members, &c.base);
            assert_eq!(
                (stat.reported_pairs, stat.fp_pairs),
                (reported, fp),
                "{} vs enumeration",
                c.label
            );
        }
    }

    #[test]
    fn tally_skips_singletons_and_sums_the_rest() {
        let partition = vec![vec![0, 1], vec![2], vec![3, 4, 5]];
        let base = [1, 1, 9, 2, 3, 3];
        let sizes = [10u64, 10, 10, 20, 20, 20];
        let total = tally(&partition, &base, &sizes);
        assert_eq!(total.groups, 2, "the singleton is not a duplicate group");
        assert_eq!(total.files, 5);
        assert_eq!(total.reported_pairs, 1 + 3);
        assert_eq!(total.fp_groups, 1);
        assert_eq!(
            total.fp_files, 3,
            "every member of a mixed group is misinformed"
        );
        assert_eq!(total.fp_pairs, 2);
        assert_eq!(total.overstated_bytes, 20);
    }

    /// The hardlink-farm case: the pair count is quadratic, so it has to be
    /// arithmetic — no brute force here; that is the point.
    #[test]
    fn a_group_of_thousands_does_not_need_enumerating() {
        let members: Vec<u32> = (0..10_000).collect();
        let base = vec![0u32; 10_000];
        let sizes = vec![1u64; 10_000];
        let stat = group_stat(&members, &base, &sizes);
        assert_eq!(stat.reported_pairs, 10_000 * 9_999 / 2);
        assert_eq!(stat.fp_pairs, 0);
    }

}
