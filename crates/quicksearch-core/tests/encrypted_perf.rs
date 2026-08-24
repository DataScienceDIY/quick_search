//! Encryption must cost a constant factor, not a different algorithm.
//!
//! SQLCipher decrypts and HMAC-verifies every 4 KiB page it reads, so a keyed
//! index is intrinsically slower than a plain one — that part is not a bug and
//! this file does not try to gate it. What it gates is *amplification*: a query
//! whose cost is one page fetch per row is fine unencrypted (the page cache
//! makes it nearly free) and disastrous keyed. `find_duplicate_groups` was
//! exactly that until it was rewritten to stay inside `idx_files_hash`:
//!
//! | shape | plain | encrypted | ratio |
//! |---|---|---|---|
//! | row fetch per file (pre-`8a7810d`) | 0.59 s | 2.28 s | **3.9x** |
//! | covering index scan (current) | 0.34 s | 0.44 s | 1.3x |
//!
//! Measured on 400k rows, so the ceiling below sits between those two: the old
//! shape fails it, the current one passes with room. The ratio is what makes
//! this a *test* rather than a benchmark — both arms run the same workload on
//! the same machine in the same process, so host speed, CPU governor and CI
//! contention divide out. Absolute times are printed but never asserted.
//!
//! Its own integration binary because it installs a process-global key, the
//! same reason `tests/encrypted.rs` gives.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, find_duplicate_groups, SearchHit, SearchOptions};
use quicksearch_core::security::IndexKey;
use quicksearch_core::testutil::{scratch_db, seed_index, SeedSpec, BODY_TERM, NEEDLE};

/// A raw 32-byte key, not an Argon2id derivation: the KDF costs half a second
/// in release and minutes in debug, and proves nothing about page work. It
/// reaches SQLCipher as raw hex either way (see `db::open::key_and_probe`), so
/// what is measured below is identical to a real unlocked index.
const KEY_HEX: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// Ceiling on encrypted/plain for one workload. Between the 3.9x the old
/// duplicate query cost and the 1.3x the current one costs; see the table
/// above. Raising this without a measurement in the same table defeats it.
const MAX_RATIO: f64 = 3.0;

/// Enough rows that neither index fits in `PRAGMAS_SEARCH`'s 32 MiB page
/// cache — the only regime where a per-page decrypt is visible at all. Below
/// that both arms are served from cache, every ratio is 1.0, and the gate
/// silently stops testing anything. `index_is_larger_than_the_search_cache`
/// pins that this seed still clears it.
const FILES: usize = 60_000;
const CONTENT_EVERY: usize = 5;

/// The cache the search connection actually opens with, from
/// `db::schema::PRAGMAS_SEARCH`.
const SEARCH_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// Best-of-N. The minimum is the run least disturbed by everything else on
/// the box, which is the honest figure for a comparison — a mean would
/// measure the CI runner's other tenants.
const RUNS: u32 = 5;

/// Below this, a ratio is noise over noise: two sub-millisecond timings
/// divide into anything. Every workload here is far above it; the guard is
/// for the day someone shrinks the seed.
const MIN_MEASURABLE: Duration = Duration::from_millis(3);

fn spec() -> SeedSpec {
    SeedSpec {
        files: FILES,
        content_every: CONTENT_EVERY,
        // One row in five pairs up: enough groups that ranking them is real
        // work, not so many that the whole table is one giant group.
        dup_every: 5,
        ..SeedSpec::default()
    }
}

fn key() -> IndexKey {
    IndexKey::from_hex(KEY_HEX).expect("a 64-hex-digit key")
}

/// Seed the same corpus twice, once plain and once keyed. Identical content
/// and identical insertion order, so the two indexes differ *only* by
/// encryption — which is what lets a display-limited query be compared at all
/// (the cascade stops when the limit fills, so a different rowid order would
/// decide the answer rather than the encryption).
fn seed_both() -> (PathBuf, PathBuf) {
    let plain = scratch_db("encperf-plain");
    let keyed = scratch_db("encperf-keyed");

    db::set_process_key(None);
    seed_index(&plain, &spec());

    db::set_process_key(Some(key()));
    seed_index(&keyed, &spec());
    db::set_process_key(None);

    (plain, keyed)
}

fn mib(path: &PathBuf) -> f64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0)
}

/// Run `f` `RUNS` times, keeping the fastest.
fn best_of(mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..RUNS {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

/// One workload's verdict. Collected rather than asserted inline so a run
/// reports *every* ratio, not just the first one that failed.
struct Measured {
    what: &'static str,
    plain: Duration,
    keyed: Duration,
}

impl Measured {
    fn ratio(&self) -> f64 {
        self.keyed.as_secs_f64() / self.plain.as_secs_f64()
    }

    fn line(&self) -> String {
        format!(
            "{:<28} plain {:>9.2?}   encrypted {:>9.2?}   ratio {:>5.2}x",
            self.what,
            self.plain,
            self.keyed,
            self.ratio()
        )
    }
}

/// Time `find_duplicate_groups`, which opens its own connection — so the
/// process key has to be right at call time, not at open time.
fn time_duplicates(path: &PathBuf, keyed: bool) -> Duration {
    let db_path = path.to_string_lossy().into_owned();
    best_of(|| {
        db::set_process_key(keyed.then(key));
        let groups = find_duplicate_groups(&db_path, 200).expect("duplicate scan");
        assert!(!groups.is_empty(), "the seed must contain duplicate groups");
    })
}

/// Time one cascade query on a connection opened while its key state was
/// installed. Connections keep their own codec, so no toggling is needed once
/// they are open.
fn time_query(conn: &rusqlite::Connection, query: &str, fuzzy: bool) -> Duration {
    let split = split_for_cascade(query).expect("query parses");
    let options = SearchOptions {
        fuzzy,
        ..SearchOptions::default()
    };
    best_of(|| {
        let latest = AtomicU64::new(1);
        let mut hits = 0usize;
        let mut sink = |batch: Vec<SearchHit>| hits += batch.len();
        cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("cascade runs");
        assert!(hits > 0, "'{}' must match something to be timed", query);
    })
}

#[test]
fn encryption_costs_a_constant_factor_not_a_different_algorithm() {
    let (plain, keyed) = seed_both();

    // Both connections are opened up front, each under its own key state.
    db::set_process_key(None);
    let plain_conn = db::open::open_search_reader(&plain.to_string_lossy()).expect("open plain");
    db::set_process_key(Some(key()));
    let keyed_conn = db::open::open_search_reader(&keyed.to_string_lossy()).expect("open keyed");
    db::set_process_key(None);

    println!(
        "seeded {} files ({} with content): plain {:.1} MiB, encrypted {:.1} MiB",
        FILES,
        FILES / CONTENT_EVERY,
        mib(&plain),
        mib(&keyed),
    );
    assert!(
        (mib(&plain) * 1024.0 * 1024.0) as u64 > SEARCH_CACHE_BYTES,
        "seed is smaller than the {} MiB search cache, so both arms would be \
         served entirely from memory and every ratio below would be a \
         meaningless 1.0 — raise FILES",
        SEARCH_CACHE_BYTES / (1024 * 1024)
    );

    // Duplicate finding first: it is the shape this gate exists for.
    let mut measured = vec![Measured {
        what: "find_duplicate_groups",
        plain: time_duplicates(&plain, false),
        keyed: time_duplicates(&keyed, true),
    }];

    // The cascade's four shapes. Arms alternate per workload so a machine that
    // slows down partway through moves both sides, not one.
    for (what, query, fuzzy) in [
        ("cascade literal name", NEEDLE, false),
        ("cascade literal body", BODY_TERM, false),
        ("cascade fuzzy", "quartzlte", true),
        ("cascade wildcard", "quart*", false),
        ("cascade regex", "regex:quart[sz]ite", false),
    ] {
        measured.push(Measured {
            what,
            plain: time_query(&plain_conn, query, fuzzy),
            keyed: time_query(&keyed_conn, query, fuzzy),
        });
    }

    for m in &measured {
        println!("{}", m.line());
    }

    let too_short: Vec<&Measured> = measured
        .iter()
        .filter(|m| m.plain < MIN_MEASURABLE || m.keyed < MIN_MEASURABLE)
        .collect();
    assert!(
        too_short.is_empty(),
        "these workloads finished under {:?}, so their ratios are noise over \
         noise rather than a measurement:\n{}",
        MIN_MEASURABLE,
        too_short
            .iter()
            .map(|m| m.line())
            .collect::<Vec<_>>()
            .join("\n")
    );

    let amplified: Vec<&Measured> = measured.iter().filter(|m| m.ratio() > MAX_RATIO).collect();
    assert!(
        amplified.is_empty(),
        "encryption amplified these beyond {:.1}x, which means per-page work \
         scaling with rows rather than a constant factor:\n{}",
        MAX_RATIO,
        amplified
            .iter()
            .map(|m| m.line())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
