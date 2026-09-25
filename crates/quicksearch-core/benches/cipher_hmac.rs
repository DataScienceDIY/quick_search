//! Prices SQLCipher's per-page authenticator (HMAC off, SHA-256, SHA-512)
//! against a plain index, on the write path, warm and cold search, and the
//! duplicate scan. The trade-off and the results live on
//! `db::schema::HMAC_MODE`.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_HMAC=1 \
//!   cargo bench -p quicksearch-core --bench cipher_hmac
//! ```
//!
//! Point `TMPDIR` at real storage; see `arms::warn_if_tmpdir_unset`. The
//! plain arm is the noise floor, not a candidate: rank the three keyed arms
//! against each other and against it.

mod arms;

use std::time::{Duration, Instant};

use quicksearch_core::db::schema::HmacMode;
use quicksearch_core::search::find_duplicate_groups;
use quicksearch_core::testutil::{Arm, SeedSpec};

/// The shipped mode first, so the table reads as what ships against the
/// alternatives.
const MODES: [HmacMode; 3] = [HmacMode::Off, HmacMode::Sha256, HmacMode::Sha512];

/// The shape tier: all four arms, cheap enough to run every time.
const SHAPE_FILES: usize = 200_000;
/// The confirmation tier, where the working set stops fitting the OS cache and
/// real reads enter. `QSB_HMAC_SHAPE_ONLY=1` skips it.
const SCALE_FILES: usize = 1_000_000;

fn enabled() -> bool {
    std::env::var("QSB_HMAC").is_ok()
}

fn shape_only() -> bool {
    std::env::var("QSB_HMAC_SHAPE_ONLY").is_ok()
}

/// Each arm's label, scratch tag, key state and mode: `None` is the plain
/// floor, `Some` a keyed mode.
fn identities(tag: &str) -> Vec<(String, String, bool, Option<HmacMode>)> {
    let mut out = vec![(
        "plain (no password)".to_string(),
        format!("{}-plain", tag),
        false,
        None,
    )];
    for mode in MODES {
        out.push((
            format!("keyed, HMAC {}", mode.label()),
            format!("{}-{}", tag, mode.label()),
            true,
            Some(mode),
        ));
    }
    out
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_HMAC=1 to run");
        return;
    }
    arms::warn_if_tmpdir_unset("every 'cold' figure below is RAM and the scale tier may not fit");

    tier(SHAPE_FILES, "shape");
    if shape_only() {
        println!("\n(QSB_HMAC_SHAPE_ONLY set — skipping the scale tier)");
        return;
    }
    tier(SCALE_FILES, "scale");
}

/// Every arm at one corpus size, seeded and dropped one at a time so only one
/// index is resident.
fn tier(files: usize, tag: &str) {
    arms::banner(tag, files);
    let mut summary: Vec<(String, f64, f64, f64, u64)> = Vec::new();
    for (what, suffix, keyed, hmac) in identities(tag) {
        let spec = SeedSpec {
            hmac,
            ..arms::spec(files)
        };
        let arm = Arm::seed(&what, &suffix, keyed, &spec);
        let (warm_total, dup) = report(&arm);
        summary.push((
            what,
            arm.seeded_in.as_secs_f64(),
            warm_total,
            dup.as_secs_f64(),
            arm.size_bytes(),
        ));
        arm.discard();
    }

    // The whole bench in one table, because the per-arm blocks above are too
    // far apart on a terminal to compare by eye.
    println!("\n---- {} tier summary ----", tag);
    println!(
        "{:<24}{:>12}{:>12}{:>12}{:>12}",
        "arm", "seed", "warm total", "duplicates", "size"
    );
    let baseline = summary.first().map(|s| (s.1, s.2, s.3)).unwrap_or_default();
    for (what, seeded, warm_total, dup, size) in &summary {
        println!(
            "{:<24}{:>12}{:>12}{:>12}{:>12}",
            what,
            format!("{:.1} s", seeded),
            format!("{:.1} ms", warm_total * 1000.0),
            format!("{:.0} ms", dup * 1000.0),
            format!("{:.1} MiB", arms::mib(*size)),
        );
    }
    println!(
        "\n{:<24}{:>12}{:>12}{:>12}",
        "over plain", "seed", "warm total", "duplicates"
    );
    for (what, seeded, warm_total, dup, _) in &summary {
        println!(
            "{:<24}{:>12}{:>12}{:>12}",
            what,
            format!("{:.2}x", seeded / baseline.0),
            format!("{:.2}x", warm_total / baseline.1),
            format!("{:.2}x", dup / baseline.2),
        );
    }
}

/// Everything measured about one arm. Returns the warm query total and the
/// duplicate scan, the two numbers the summary ranks on.
fn report(arm: &Arm) -> (f64, Duration) {
    let overflow = arms::header(arm);
    assert_eq!(
        overflow, 0,
        "{}: FTS5 leaves overflowed, so this arm is measuring a broken \
         derivation rather than its authenticator",
        arm.what
    );
    arms::attribution(arm);
    (arms::workloads(arm), duplicates(arm))
}

/// Best time of the duplicate scan. `find_duplicate_groups` reads the most
/// pages per unit of answer (a full `idx_files_hash` scan), so a per-page
/// cost shows up there most plainly. It opens its own connection, so the
/// process key and profile have to be installed at call time.
fn duplicates(arm: &Arm) -> Duration {
    let db_path = arm.path.to_string_lossy().into_owned();
    arm.with_key(|| {
        let mut best = Duration::MAX;
        for _ in 0..arms::RUNS {
            let start = Instant::now();
            let groups = find_duplicate_groups(&db_path, 200).expect("duplicate scan");
            assert!(!groups.is_empty(), "the seed must contain duplicate groups");
            best = best.min(start.elapsed());
        }
        best
    })
}
