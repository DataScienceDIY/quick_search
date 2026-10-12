//! How big the search connection's page cache has to be, and what it is
//! worth: the sweep behind `db::schema::recommended_search_cache_mib` and
//! `search::IDLE_RELEASE`.
//!
//! Queries run as a keystroke sequence (`quar`, `quart`, `quartz`,
//! `quartzi`), as in a real session, where every keystroke re-runs the
//! search. The warm column decides the constant: a cold query happens once,
//! on the first keystroke after `IDLE_RELEASE` drops the connection, and a
//! warm one on every keystroke after it.
//!
//! The `files` table has to stay resident, not the FTS index.
//! `search/cascade/passes.rs` answers filename queries (ranks 1–4, 9–10) with
//! `SELECT … FROM files f WHERE f.name LIKE '%…%'`, a full table scan with no
//! FTS, and the fuzzy pass scans it again (`LIKE`-narrowed for terms of 9+
//! characters, unpredicated below that). So the working set scales with file
//! count, not document volume, and the corpus dimension below shows it.
//!
//! Keyed figures predate `HMAC_MODE = Off`; see `db::schema::HMAC_MODE`.
//!
//! Printed rather than asserted: shared-box timings are not stable enough for
//! a pass/fail gate, and a flaky perf gate gets muted rather than fixed.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_SEARCH_PERF=1 \
//!   cargo bench -p quicksearch-core --bench search_perf
//! ```
//!
//! Point `TMPDIR` at real storage (see `arms::warn_if_tmpdir_unset`); the
//! 1M-file arms were ~580 MB each, before the seed's uncapped WAL
//! (`testutil::seed_index`), which needs room on top. Budget ~20 minutes,
//! nearly all of it seeding.
//!
//! # What it found
//!
//! Warm search, the mean keystroke of one settled pass,
//! `schema::PAGE_SIZE` = 8192, rows at a realistic width (139 B; see
//! [`spec`]), on the earlier fixed grid of 1–256 MiB (powers of two plus 24,
//! 48 and 96):
//!
//! | corpus | `files` table | keyed, under knee | keyed, at knee | knee | ratio |
//! |---|---|---|---|---|---|
//! | 200k | 26.5 MiB | 41.4 ms (≤24 MiB) | **10.7 ms** | 32 MiB | ≤1.21x |
//! | 600k | 79.5 MiB | 121.2 ms (≤64 MiB) | **33.7 ms** | 96 MiB | ≤1.21x |
//! | 1M | 132.4 MiB | 199.6 ms (≤128 MiB) | **58.3 ms** | 256 MiB | ≤1.93x |
//!
//! 1. The knee is at most 1.21x the `files` table: the grid brackets it in
//!    (24, 32] MiB at 200k and (64, 96] at 600k, and the two agree by
//!    construction, since corpus and grid both step by 3. 1M's lies in
//!    (128, 256], where 1.21x predicts 160. Hence 139 B/row × 1.21 = 168 B,
//!    `schema::SEARCH_CACHE_BYTES_PER_FILE`, which errs large.
//! 2. Below the knee an encrypted index is 3.4–3.9x slower, and the step is a
//!    cliff, not a slope: 600k keyed ran 121–130 ms at every ceiling up to
//!    64 MiB and 33.7 ms at 96.
//! 3. Plain has no knee. Its widest spread was 1.35x and it is not monotonic
//!    (32 and 48 MiB measured slower than 1 MiB at 600k): run-to-run noise,
//!    not a curve. A miss on a plain index is a `memcpy` from the OS cache; on
//!    a keyed one it is an AES-CBC decrypt. Hence
//!    `schema::SEARCH_CACHE_PLAIN_MIB`, flat.
//!
//! Row width is half the answer. The default seed's narrow rows (`hash` NULL,
//! a 16-character `/seed/NNN/` parent) measure 69.5 B, half of a realistic
//! 139 B; calibrating on them would under-size every cache by two and put
//! every user back under the knee.
//!
//! The 128 MiB automatic cap (`schema::SEARCH_CACHE_MAX_MIB`) binds at ~800k
//! files. At 1M the index wants ~160 MiB and gets 128: 205 ms per keystroke
//! against the 58 ms available at 256. `[search] cache_size_mib` is how a
//! user trades memory for that speed.

mod arms;

use std::time::Duration;

use quicksearch_core::db::schema::recommended_search_cache_mib;
use quicksearch_core::search::SearchOptions;
use quicksearch_core::testutil::{run_cascade, Arm, SeedSpec};

/// Two corpora under the automatic cap and one past it
/// (`schema::SEARCH_CACHE_MAX_MIB` binds at ~800k files). If the working set
/// is `files`, the knee moves with the corpus.
const CORPORA: [usize; 3] = [200_000, 600_000, 1_000_000];

/// Cache ceilings to sweep, as multiples of the arm's own `files` table. The
/// knee sits at the first ceiling that holds the table, which the earlier
/// fixed grid could only place at ≤1.21x, too loose to tell how much of
/// `schema::SEARCH_CACHE_BYTES_PER_FILE`'s headroom is needed.
const TABLE_MULTIPLES: [f64; 5] = [1.0, 1.05, 1.1, 1.2, 1.5];

/// The sweep's ends. 1 MiB is far too small, because the curve needs a
/// visible floor for any "enough" to be a measurement, and 256 MiB is past
/// the automatic cap, so a knee inside the range is a knee and not the edge
/// of the sweep.
const FLOOR_MIB: i64 = 1;
const CEILING_MIB: i64 = 256;

/// The ceilings for an arm whose `files` table is `files_bytes`, as
/// `PRAGMA cache_size` values, smallest cache first: negative is KiB
/// (positive would be a page count). The ends, [`TABLE_MULTIPLES`], and
/// `shipped`, so the curve can be read against the ceiling a user gets.
fn cache_sizes(files_bytes: u64, shipped: i64) -> Vec<i64> {
    let mut kib: Vec<i64> = TABLE_MULTIPLES
        .iter()
        .map(|m| (files_bytes as f64 * m / 1024.0).ceil() as i64)
        .collect();
    kib.extend([FLOOR_MIB * 1024, CEILING_MIB * 1024, -shipped]);
    kib.sort_unstable();
    kib.dedup();
    kib.into_iter().map(|k| -k).collect()
}

/// The ceiling `open_search_reader` derives for this arm, as a
/// `PRAGMA cache_size` value. The output marks it, so the curve can be read
/// against it without counting rows.
fn shipped_cache(arm: &Arm, files: usize) -> i64 {
    -recommended_search_cache_mib(files as i64, arm.keyed) * 1024
}

const SEQUENCE: [&str; 4] = ["quar", "quart", "quartz", "quartzi"];

/// A cache is "enough" once warm is within this of the best warm on the same
/// arm. Locates a knee once [`KNEE_MIN_SPREAD`] says there is one.
const KNEE_TOLERANCE: f64 = 1.10;

/// How much worse the worst ceiling must be than the best before the curve
/// counts as a knee. A real one is a 4x step (keyed at 200k: 41.4 ms below
/// 32 MiB, 10.7 ms at or above). [`KNEE_TOLERANCE`] alone would find a knee
/// in a plain arm's ~1.3x run-to-run noise, so this gate sits well above it.
const KNEE_MIN_SPREAD: f64 = 1.5;

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_PERF").is_ok()
}

/// A realistically wide `files` row: a content hash and a parent several
/// directories deep, where the default seed has `hash` NULL and a
/// 16-character `/seed/NNN/` parent. The row width sets the calibration (see
/// the module doc), so bytes per row are printed per arm.
fn spec(files: usize) -> SeedSpec {
    SeedSpec {
        files,
        commit_every: 5_000,
        // A hash on every row, as a real index has once hashing has run.
        dup_every: 2,
        // `/seed/NNN/word/word/word/word/word/`: about 50 characters, an
        // ordinary depth for a document tree.
        dir_depth: 6,
        // ~2 KB documents, the default: a corpus of tiny ones would make the
        // full-text pass look free when it is the cascade's most expensive.
        ..SeedSpec::default()
    }
}

/// The options every measurement here runs under, with `fuzzy` the variable.
/// Fuzzy is off in both `SearchOptions::default()` and the GUI default
/// (`[search] fuzzy_default`), so the fuzzy rows are what a user opts into,
/// not what a default install pays per keystroke.
fn options(fuzzy: bool) -> SearchOptions {
    SearchOptions {
        limit: 1000,
        fuzzy,
        ..SearchOptions::default()
    }
}

/// The fuzzy pairing's keystroke sequence, reported per term rather than
/// averaged. Under the default `fuzzy_max_edits` of 2, `pigeonhole_chunks`
/// needs `3 × (k + 1)` characters (`edit_budget` gives k=1 at 3–5 and k=2
/// from 6), so only terms of 9+ characters are prefiltered. The short terms
/// are the control a pass-C narrowing must not move; the long ones are what
/// it narrows. `quartzite` is `testutil::NEEDLE`, present in seeded names, so
/// the prefiltered rows return real hits.
const FUZZY_SEQUENCE: [&str; 4] = ["quar", "quartz", "quartzite", "quartzites"];

/// Warm per-term timings, fuzzy off against on, at the shipped cache, which
/// the counts `seed_index` records give the connection: the gate for the
/// fuzzy filename pass (`passes::pass_fuzzy_filename`). The off column is the
/// control, which a pass-C change must not move.
fn run_fuzzy_pairing(arm: &Arm) {
    let conn = arm.open_search();
    // One settling pass per mode so every printed number is warm.
    for fuzzy in [false, true] {
        let opts = options(fuzzy);
        for query in FUZZY_SEQUENCE {
            run_cascade(&conn, query, &opts);
        }
    }
    println!(
        "{:>12}  {:>10}  {:>10}  {:>8}   (fuzzy pairing, shipped cache, warm)",
        "term", "off", "on", "hits"
    );
    for query in FUZZY_SEQUENCE {
        let (off, _) = run_cascade(&conn, query, &options(false));
        let (on, hits) = run_cascade(&conn, query, &options(true));
        println!("{:>12}  {:>9.1?}  {:>9.1?}  {:>8}", query, off, on, hits);
    }
}

/// One arm at one cache ceiling: the first keystroke on a fresh connection,
/// then the steady state after a priming pass. The connection comes from the
/// production `open_search_reader` with only `cache_size` overridden, so the
/// two key states differ by the key alone.
fn measure(arm: &Arm, cache_size: i64) -> (Duration, Duration, usize) {
    let conn = arm.open_search();
    conn.execute_batch(&format!("PRAGMA cache_size = {};", cache_size))
        .unwrap();

    let opts = options(false);
    let (cold, hits) = run_cascade(&conn, SEQUENCE[0], &opts);
    // A priming pass, so "warm" is a settled session rather than the three
    // keystrokes after the first.
    for query in SEQUENCE {
        run_cascade(&conn, query, &opts);
    }
    let total: Duration = SEQUENCE
        .iter()
        .map(|q| run_cascade(&conn, q, &opts).0)
        .sum();
    (cold, total / SEQUENCE.len() as u32, hits)
}

fn run_matrix(arm: &Arm, files: usize) {
    let files_bytes = arm.table_bytes("files");
    let shipped = shipped_cache(arm, files);
    // `schema::SEARCH_CACHE_BYTES_PER_FILE` is built on bytes per row, so it
    // is printed rather than left to be inferred.
    println!(
        "\n=== {} ===  {:.1} MiB on disk, files table {:.1} MiB ({:.0} B/row), \
         seeded in {:.1?}",
        arm.what,
        arms::mib(arm.size_bytes()),
        arms::mib(files_bytes),
        files_bytes as f64 / files as f64,
        arm.seeded_in
    );
    println!(
        "{:>13}  {:>7}  {:>10}  {:>10}  {:>10}  {:>8}",
        "cache", "/files", "cold", "warm", "vs best", "hits"
    );

    let rows: Vec<(i64, Duration, Duration, usize)> = cache_sizes(files_bytes, shipped)
        .into_iter()
        .map(|cache_size| {
            let (cold, warm, hits) = measure(arm, cache_size);
            (cache_size, cold, warm, hits)
        })
        .collect();

    let best = rows
        .iter()
        .map(|(_, _, warm, _)| *warm)
        .min()
        .unwrap_or_default();
    for (cache_size, cold, warm, hits) in &rows {
        let ratio = warm.as_secs_f64() / best.as_secs_f64();
        println!(
            "{:>7.1} MiB{}  {:>6.2}x  {:>9.1?}  {:>9.1?}  {:>9.2}x  {:>8}",
            -*cache_size as f64 / 1024.0,
            if *cache_size == shipped { " *" } else { "  " },
            (-cache_size * 1024) as f64 / files_bytes as f64,
            cold,
            warm,
            ratio,
            hits
        );
    }

    // A curve without a real step is the normal shape for a *plain* arm, and
    // naming the low point of its noise a knee would invent a result.
    let worst = rows
        .iter()
        .map(|(_, _, warm, _)| *warm)
        .max()
        .unwrap_or_default();
    let spread = worst.as_secs_f64() / best.as_secs_f64();
    if spread < KNEE_MIN_SPREAD {
        println!(
            "no knee — warm spans only {:.2}x across the whole sweep, under \
             the {:.1}x a step has to clear; files table is {:.1} MiB",
            spread,
            KNEE_MIN_SPREAD,
            arms::mib(files_bytes)
        );
        return;
    }

    // Otherwise the knee is the smallest ceiling within tolerance of the best
    // (`max` on a negative KiB ceiling is the smallest cache), reported
    // against the `files` table, the quantity it should track across corpora.
    let knee = rows
        .iter()
        .filter(|(_, _, warm, _)| warm.as_secs_f64() <= best.as_secs_f64() * KNEE_TOLERANCE)
        .map(|(cache_size, _, _, _)| *cache_size)
        .max()
        .unwrap_or(shipped);
    // The constant is derived from the ratio, cache per byte of `files`;
    // printing it per arm checks the derivation against every corpus rather
    // than fitting it to one.
    println!(
        "knee at {:.1} MiB (within {:.0}% of best warm); files table is {:.1} MiB; \
         ratio {:.2}x; shipped ceiling is {} MiB{}",
        -knee as f64 / 1024.0,
        (KNEE_TOLERANCE - 1.0) * 100.0,
        arms::mib(files_bytes),
        (-knee * 1024) as f64 / files_bytes as f64,
        -shipped / 1024,
        if knee < shipped {
            " — TOO SMALL for this corpus"
        } else {
            ""
        }
    );
}

fn main() {
    if !enabled() {
        eprintln!("skipping: set QSB_SEARCH_PERF=1 to run");
        return;
    }
    arms::warn_if_tmpdir_unset("the large arms may not fit");
    // `QSB_SEARCH_PERF=fuzzy` runs only the fuzzy pairing: the cache sweep
    // takes minutes per arm and a pass-C gate does not need it. `QSB_CORPORA`
    // trims the corpus list (comma-separated file counts).
    let fuzzy_only = std::env::var("QSB_SEARCH_PERF").as_deref() == Ok("fuzzy");
    let corpora: Vec<usize> = std::env::var("QSB_CORPORA")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| CORPORA.to_vec());

    for files in corpora {
        for keyed in [false, true] {
            let arm = Arm::seed(
                format!(
                    "{} {}k files",
                    if keyed { "keyed" } else { "plain" },
                    files / 1000
                ),
                &format!("searchperf-{}-{}", files, keyed),
                keyed,
                &if fuzzy_only {
                    light_spec(files)
                } else {
                    spec(files)
                },
            );
            if fuzzy_only {
                println!("\n=== {} ===  seeded in {:.1?}", arm.what, arm.seeded_in);
            } else {
                run_matrix(&arm, files);
            }
            run_fuzzy_pairing(&arm);
            arm.discard();
        }
    }
}

/// The fuzzy-only corpus: `files` rows and nothing else. Pass C never reads a
/// document (its whole cost is the `files` scan plus the bitap), and 2 KB
/// bodies turn a minutes-long seed into hours on slow storage while adding
/// only pass-D noise. One row in `usize::MAX` still gets content, which keeps
/// pass D exercised at no cost.
fn light_spec(files: usize) -> SeedSpec {
    SeedSpec {
        content_every: usize::MAX,
        ..spec(files)
    }
}
