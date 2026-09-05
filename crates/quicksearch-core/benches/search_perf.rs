//! How big the search connection's page cache has to be, and what it is worth
//! — the two claims `db::schema::PRAGMAS_SEARCH` and
//! `search::IDLE_RELEASE` rest on.
//!
//! Queries run as a keystroke sequence (`quar`, `quart`, `quartz`,
//! `quartzi`), because that is what a real session is: the user types, and
//! every keystroke re-runs the search. **The warm column is the one that
//! decides the constant.** A cold query happens once, on the first keystroke
//! after `IDLE_RELEASE` drops the connection; a warm one happens on every
//! keystroke after it.
//!
//! What has to stay resident is the **`files` table**, not the FTS index.
//! `search/cascade/passes.rs` answers filename queries (ranks 1–4, 9–10) with
//! `SELECT … FROM files f WHERE f.name LIKE '%…%'` — a full table scan, no FTS
//! at all — and the fuzzy pass scans it again with `WHERE 1=1`. So the working
//! set scales with **file count**, not with document volume, and the corpus
//! dimension below is what makes that visible.
//!
//! The keyed rows are where an undersized cache hurts first: a page-cache miss
//! costs an AES-CBC decrypt, not a `memcpy`.
//!
//! **Every keyed figure recorded below was taken while
//! `db::schema::HMAC_MODE` was HMAC-SHA512**, so a miss then also cost a
//! per-page verify. It no longer does, and `benches/cipher_hmac.rs` measured
//! that as 1.78x on warm search — so the knees found here are deeper than the
//! ones a re-sweep would find. `schema::SEARCH_CACHE_BYTES_PER_FILE` says the
//! same thing from the other side: it is now conservative, and re-running this
//! bench is what would tighten it.
//!
//! Printed rather than asserted: shared-box timings are not stable enough for
//! a pass/fail gate, and a flaky perf gate gets muted rather than fixed.
//!
//! ```text
//! TMPDIR=/media/shared/qs-scratch QSB_SEARCH_PERF=1 \
//!   cargo bench -p quicksearch-core --bench search_perf
//! ```
//!
//! `TMPDIR` wants real storage: the 1M-file arms are ~580 MB each and a tmpfs
//! `/tmp` would turn every miss into a RAM copy. Budget ~20 minutes, nearly
//! all of it seeding.
//!
//! # What it found
//!
//! Warm search, best of the settled session, `schema::PAGE_SIZE` = 8192, rows
//! at a realistic width (139 B — see [`spec`]):
//!
//! | corpus | `files` table | keyed, under knee | keyed, at knee | knee | ratio |
//! |---|---|---|---|---|---|
//! | 200k | 26.5 MiB | 41.4 ms (≤24 MiB) | **10.7 ms** | 32 MiB | 1.21x |
//! | 600k | 79.5 MiB | 121.2 ms (≤64 MiB) | **33.7 ms** | 96 MiB | 1.21x |
//! | 1M | 132.4 MiB | 199.6 ms (≤128 MiB) | **58.3 ms** | 256 MiB | ≤1.93x |
//!
//! 1. **The knee is 1.21x the `files` table, at every corpus.** Not
//!    approximately: 26.5→32 and 79.5→96 both land on it, and 1M's true knee
//!    is somewhere in (128, 256] where 1.21x predicts 160. It tracks *file
//!    count*, not document volume, because what every keystroke rescans is
//!    `files` (see the pass-A note above), never the FTS index. That product —
//!    139 B/row × 1.21 — is `schema::SEARCH_CACHE_BYTES_PER_FILE`.
//! 2. **Below the knee an encrypted index is 3.4–3.9x slower**, and the step
//!    is a cliff, not a slope: 600k measured 121–130 ms at every ceiling from
//!    1 to 64 MiB and 33.7 ms at 96.
//! 3. **Plain has no knee.** Its widest spread was 1.35x and it is not even
//!    monotonic — 32 and 48 MiB measured slower than 1 MiB at 600k — which is
//!    run-to-run noise, not a curve. A miss it takes is a `memcpy` from the OS
//!    cache; a miss the keyed arm takes is an AES-CBC decrypt (plus, when
//!    these were measured, an HMAC-SHA512 verify). Hence
//!    `schema::SEARCH_CACHE_PLAIN_MIB`, flat.
//!
//! **Row width is half the answer and was nearly missed.** The narrow rows the
//! search harnesses used to seed — `hash` NULL, a 16-character parent — are
//! 69.5 B, exactly half of a realistic 139 B. Calibrating against those would
//! have under-sized every cache by two and put every user back under the knee.
//!
//! **The 128 MiB automatic cap binds at ~800k files.** At 1M the derived value
//! is capped at 128 while the index wants ~160: 205 ms per keystroke against
//! the 58 ms available. That is the memory-versus-speed trade
//! `schema::SEARCH_CACHE_MAX_MIB` documents, and
//! `[search] cache_size_mib` is how a user takes the other side of it.

use std::time::{Duration, Instant};

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::testutil::{Arm, SeedSpec};
use rusqlite::Connection;

/// Corpora chosen to bracket `PRAGMAS_SEARCH`: at 200k the `files` table is
/// ~19 MiB and fits inside 32 MiB, at 1M it is ~98 MiB and cannot. If the
/// working set really is `files`, the knee moves between these two.
const CORPORA: [usize; 3] = [200_000, 600_000, 1_000_000];

/// Cache ceilings in KiB (negative is KiB; positive would be a page count).
/// 1 MiB is deliberately far too small — the curve needs a visible floor for
/// any "enough" to be a measurement — and 256 MiB is past anything shippable,
/// so a knee inside the range is a knee and not the edge of the sweep.
///
/// 24, 48 and 96 MiB break the powers of two. Without them the knee can only
/// be located to the next power up, which pins the cache-over-`files` ratio no
/// tighter than (1.0, 1.93] — too loose to derive a constant from.
const CACHE_SIZES: [i64; 12] = [
    -1024, -2048, -4096, -8192, -16384, -24576, -32768, -49152, -65536, -98304, -131072, -262144,
];

/// What `PRAGMAS_SEARCH` ships with, marked in the output so the curve can be
/// read against it without counting columns.
const SHIPPED_CACHE: i64 = -32768;

const SEQUENCE: [&str; 4] = ["quar", "quart", "quartz", "quartzi"];

/// A cache is "enough" once warm is within this of the best warm on the same
/// arm — used to *locate* a knee, once there is one to locate.
const KNEE_TOLERANCE: f64 = 1.10;

/// How much worse the worst ceiling must be than the best before the curve is
/// called a knee at all.
///
/// A real one is unmistakable: keyed at 200k ran 41.4 ms flat below 32 MiB and
/// 10.7 ms at or above it, monotonically, a 4x step. A plain arm at the same
/// corpus wanders over about 1.3x and is not even monotonic — 32 and 48 MiB
/// measured *slower* than 1 MiB — which is run-to-run noise wearing the shape
/// of a curve. At 1.10 the locator happily reports a knee in that noise, so
/// the gate to being a knee is set well above it.
const KNEE_MIN_SPREAD: f64 = 1.5;

fn enabled() -> bool {
    std::env::var("QSB_SEARCH_PERF").is_ok()
}

/// A **realistically wide** `files` row, which is the whole calibration.
///
/// The default seed stores `hash` NULL and a 16-character `/seed/NNN/` parent;
/// a real row carries a 32-byte content hash and a parent nested several
/// directories deep, and `parent` is stored per row. Since the working set
/// *is* the `files` table, calibrating a cache constant against the narrow
/// shape would under-size it by roughly the ratio between them — the bytes per
/// row are printed per arm so that ratio stays visible rather than assumed.
fn spec(files: usize) -> SeedSpec {
    SeedSpec {
        files,
        // ~2 KB documents, the default: a corpus of tiny ones would make the
        // full-text pass look free when it is the cascade's most expensive.
        commit_every: 5_000,
        // A hash on every row, as a real index has once hashing has run.
        dup_every: 2,
        // `/seed/NNN/word/word/word/word/word/` — about 50 characters, which
        // is an ordinary depth for a document tree.
        dir_depth: 6,
        ..SeedSpec::default()
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Run one query; hits are counted, not kept — holding 200k `SearchHit`s
/// would measure the allocator instead of the scan.
fn time_query(conn: &Connection, query: &str) -> (Duration, usize) {
    let split = split_for_cascade(query).unwrap();
    let latest = std::sync::atomic::AtomicU64::new(1);
    let mut count = 0usize;
    let mut sink = |hits: Vec<SearchHit>| count += hits.len();
    let options = SearchOptions {
        limit: 1000,
        ..SearchOptions::default()
    };
    let start = Instant::now();
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).unwrap();
    (start.elapsed(), count)
}

/// One arm at one cache ceiling: the first keystroke on a fresh connection,
/// then the steady state after a priming pass.
///
/// The connection comes from the production `open_search_reader` with only
/// `cache_size` overridden, so the two key states differ by the key and
/// nothing else — the old version opened the plain arm with a raw
/// `Connection::open`, which skipped the key path entirely and made the two
/// columns incomparable.
fn measure(arm: &Arm, cache_size: i64) -> (Duration, Duration, usize) {
    let conn = arm.open_search();
    conn.execute_batch(&format!("PRAGMA cache_size = {};", cache_size))
        .unwrap();

    let (cold, hits) = time_query(&conn, SEQUENCE[0]);
    // A priming pass, so "warm" is a settled session rather than the three
    // keystrokes after the first.
    for query in SEQUENCE {
        time_query(&conn, query);
    }
    let total: Duration = SEQUENCE.iter().map(|q| time_query(&conn, q).0).sum();
    (cold, total / SEQUENCE.len() as u32, hits)
}

fn run_matrix(arm: &Arm, files: usize) {
    let files_bytes = arm.table_bytes("files");
    // Bytes per row is the constant `schema::recommended_search_cache_mib` is
    // built on, so it is printed rather than left to be inferred from the
    // table size and the corpus.
    println!(
        "\n=== {} ===  {:.1} MiB on disk, files table {:.1} MiB ({:.0} B/row), \
         seeded in {:.1?}",
        arm.what,
        mib(arm.size_bytes()),
        mib(files_bytes),
        files_bytes as f64 / files as f64,
        arm.seeded_in
    );
    println!(
        "{:>12}  {:>10}  {:>10}  {:>10}  {:>8}",
        "cache", "cold", "warm", "vs best", "hits"
    );

    let rows: Vec<(i64, Duration, Duration, usize)> = CACHE_SIZES
        .iter()
        .map(|&cache_size| {
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
            "{:>9} MiB{}  {:>9.1?}  {:>9.1?}  {:>9.2}x  {:>8}",
            -cache_size / 1024,
            if *cache_size == SHIPPED_CACHE {
                " *"
            } else {
                "  "
            },
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
            mib(files_bytes)
        );
        return;
    }

    // Otherwise the knee is the smallest ceiling still within tolerance of the
    // best. Reported against the `files` table because that is the quantity it
    // should track — if the two move together across corpora, the constant
    // should be written in terms of file count. `max` on a negative KiB
    // ceiling is the *smallest* cache.
    let knee = rows
        .iter()
        .filter(|(_, _, warm, _)| warm.as_secs_f64() <= best.as_secs_f64() * KNEE_TOLERANCE)
        .map(|(cache_size, _, _, _)| *cache_size)
        .max()
        .unwrap_or(SHIPPED_CACHE);
    // The ratio is the number the constant is derived from: how much cache one
    // byte of `files` needs. Reported per arm so the derivation can be checked
    // against every corpus rather than fitted to one.
    println!(
        "knee at {} MiB (within {:.0}% of best warm); files table is {:.1} MiB; \
         ratio {:.2}x; shipped ceiling is {} MiB{}",
        -knee / 1024,
        (KNEE_TOLERANCE - 1.0) * 100.0,
        mib(files_bytes),
        (-knee * 1024) as f64 / files_bytes as f64,
        -SHIPPED_CACHE / 1024,
        if knee < SHIPPED_CACHE {
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
    if std::env::var_os("TMPDIR").is_none() {
        eprintln!(
            "warning: TMPDIR unset — scratch goes to {}. If that is tmpfs the \
             large arms will not fit and every miss is a RAM copy.",
            std::env::temp_dir().display()
        );
    }
    for files in CORPORA {
        for keyed in [false, true] {
            let arm = Arm::seed(
                format!(
                    "{} {}k files",
                    if keyed { "keyed" } else { "plain" },
                    files / 1000
                ),
                &format!("searchperf-{}-{}", files, keyed),
                keyed,
                &spec(files),
            );
            run_matrix(&arm, files);
            arm.discard();
        }
    }
}
