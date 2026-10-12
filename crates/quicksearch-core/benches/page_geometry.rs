//! What the index's page geometry costs, on disk and in query time.
//!
//! Two levers, swept together because they are coupled: the database page
//! size (`db::schema::PAGE_SIZE`) and FTS5's record size
//! (`db::schema::fts_pgsz_for`). The second is derived from the first, so
//! neither can move alone: at a page size of 8192, an FTS5 record built for
//! 4096 leaves half of every page empty.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_PGSZ=1 \
//!   cargo bench -p quicksearch-core --bench page_geometry
//! ```
//!
//! Point `TMPDIR` at real storage; see `arms::warn_if_tmpdir_unset`. Each
//! arm is dropped once measured, so scratch peaks at one 1M-file arm, about
//! 2.4 GB: its ~300 MiB database plus a WAL that `testutil::seed_index`
//! checkpoints as a default-config run does, at `maximum_wal_size` (2 GiB;
//! less on a volume with under ~8 GiB free).
//!
//! # Settled: FTS5 record size
//!
//! SQLCipher reserves part of every page for its IV and any authenticator, so
//! a keyed page holds `page − reserve − 35` bytes inline. FTS5's default
//! record of 4050 fits a plain 4096-byte page; under an 80-byte HMAC-SHA512
//! reserve it missed by 71 bytes and sent every full leaf to an overflow page.
//! `fts_pgsz_for` derives the record from the page size and reserve, which
//! keeps the miss at zero at every page size. Measured at 120k files before
//! the derivation existed; keyed figures predate `HMAC_MODE = Off` (see
//! `db::schema::HMAC_MODE`):
//!
//! | | plain 4050 | plain shipped | keyed 4050 | keyed shipped |
//! |---|---|---|---|---|
//! | bulk write | 3.87 s | 4.04 s | 7.34 s | 7.13 s |
//! | size | 156.4 MiB | 156.4 MiB | **170.2 MiB** | **159.1 MiB** |
//! | fts overflow pages | 0 | 0 | **27381** | 0 |
//! | cold `chalcedony` | 11.35 ms | 11.37 ms | 29.91 ms | 29.57 ms |
//! | warm `chalcedony` | 9.29 ms | 9.24 ms | 9.41 ms | 9.27 ms |
//!
//! A disk-space fix (1.089x → 1.011x encrypted-over-plain), not a speed fix:
//! both indexing and search moved less than the harness's own noise floor.
//! `tests/encrypted_size.rs` gates the size half, so this bench does not
//! re-measure it.
//!
//! # Settled: database page size
//!
//! 8192; `db::schema::PAGE_SIZE` holds the sweep and the reasoning.
//!
//! # Reading it
//!
//! The plain arm is the noise floor. Cold is SQLite-cold: a fresh
//! connection, so its cache starts empty. Nothing evicts the OS cache, so no
//! tier is storage-cold. Warm runs at the cache ceiling each arm ships with.
//! Report the tiers separately; do not average them.

mod arms;

use quicksearch_core::db;
use quicksearch_core::testutil::{Arm, SeedSpec};

/// Page sizes to sweep, up to `SQLITE_MAX_PAGE_SIZE`. 512 is excluded: under
/// HMAC-SHA512's 80-byte reserve its usable size falls below SQLite's
/// 480-byte floor. FTS5 caps its own record size at 64 KiB and rejects
/// anything larger, so 65536 is the last size where `fts_pgsz_for` still has
/// room.
const SWEPT: [i64; 7] = [1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// `QSB_PGSZ_SIZES=8192,16384` narrows the sweep. A full run takes about 5
/// minutes, most of it seeding, so re-asking one question should not re-ask
/// all of them.
fn swept() -> Vec<i64> {
    match std::env::var("QSB_PGSZ_SIZES") {
        Ok(list) => list
            .split(',')
            .map(|s| s.trim().parse().expect("QSB_PGSZ_SIZES wants integers"))
            .collect(),
        Err(_) => SWEPT.to_vec(),
    }
}

/// `QSB_PGSZ_SHAPE_ONLY=1` skips the large corpora.
fn shape_only() -> bool {
    std::env::var("QSB_PGSZ_SHAPE_ONLY").is_ok()
}

/// The shape tier: every page size, cheap enough to run them all.
const SHAPE_FILES: usize = 200_000;
/// The confirmation tiers, run only for the baseline and the shape tier's
/// winner.
const SCALE_FILES: [usize; 2] = [600_000, 1_000_000];

fn enabled() -> bool {
    std::env::var("QSB_PGSZ").is_ok()
}

fn spec(files: usize, page_size: i64) -> SeedSpec {
    SeedSpec {
        page_size: Some(page_size),
        ..arms::spec(files)
    }
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_PGSZ=1 to run");
        return;
    }
    arms::warn_if_tmpdir_unset("the large tiers may not fit");

    let best = shape_tier();
    if shape_only() {
        println!("\n(QSB_PGSZ_SHAPE_ONLY set — skipping the large corpora)");
        return;
    }
    scale_tier(best);
}

/// Every page size at [`SHAPE_FILES`], plain and keyed. Returns the keyed page
/// size with the lowest total warm time, the metric that matters because a
/// session re-queries on every keystroke.
fn shape_tier() -> i64 {
    arms::banner("shape", SHAPE_FILES);
    let mut best = (db::schema::PAGE_SIZE, f64::MAX);
    for page_size in swept() {
        for keyed in [false, true] {
            let arm = Arm::seed(
                format!("{} {}", if keyed { "keyed" } else { "plain" }, page_size),
                &format!("pgsz-{}-{}", page_size, keyed),
                keyed,
                &spec(SHAPE_FILES, page_size),
            );
            let warm_total = report(&arm);
            if keyed && warm_total < best.1 {
                best = (page_size, warm_total);
            }
            arm.discard();
        }
    }
    println!(
        "\n>>> lowest keyed warm total at page_size {} ({:.1} ms across {} workloads)",
        best.0,
        best.1 * 1000.0,
        arms::WORKLOADS.len()
    );
    best.0
}

/// The baseline and the winner only, at the larger corpora.
fn scale_tier(best: i64) {
    let mut sizes = vec![db::schema::PAGE_SIZE];
    if best != db::schema::PAGE_SIZE {
        sizes.push(best);
    }
    for files in SCALE_FILES {
        arms::banner("scale", files);
        for page_size in &sizes {
            for keyed in [false, true] {
                let arm = Arm::seed(
                    format!("{} {}", if keyed { "keyed" } else { "plain" }, page_size),
                    &format!("pgsz-{}-{}-{}", files, page_size, keyed),
                    keyed,
                    &spec(files, *page_size),
                );
                report(&arm);
                arm.discard();
            }
        }
    }
}

/// Everything measured about one arm. Returns its total warm query time in
/// seconds, the metric [`shape_tier`] ranks on.
fn report(arm: &Arm) -> f64 {
    arms::header(arm);
    arms::attribution(arm);
    arms::workloads(arm)
}
