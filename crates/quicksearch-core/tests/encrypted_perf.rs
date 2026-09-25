//! Encryption must cost a constant factor, not a different algorithm.
//!
//! SQLCipher AES-decrypts every page it reads, so a keyed index is
//! intrinsically slower than a plain one; this file does not gate that. It
//! gates *amplification*: a query that fetches one page per row is nearly free
//! unencrypted, because the page cache serves it, and disastrous keyed.
//! `find_duplicate_groups` had that shape until `8a7810d` kept it inside
//! `idx_files_hash`:
//!
//! | shape | plain | encrypted | ratio |
//! |---|---|---|---|
//! | row fetch per file (pre-`8a7810d`) | 0.59 s | 2.28 s | 3.9x |
//! | covering index scan (current) | 0.34 s | 0.44 s | 1.3x |
//!
//! Measured on 400k rows under HMAC-SHA512 (see `db::schema::HMAC_MODE`); with
//! it off, every shape here runs 1.03–1.20x. [`MAX_RATIO`] sits between the
//! two rows. Asserting a ratio makes this a test rather than a benchmark: every
//! arm runs the same workload on the same machine in the same process, so host
//! speed, CPU governor and CI contention divide out. Absolute times are printed,
//! never asserted.
//!
//! # Size
//!
//! Four arms, because the second variable is FTS5's *record* size. A table
//! leaf holds `page − reserve − 35` bytes inline, where a plain file's reserve
//! is 0 and a keyed one's is `HMAC_MODE.reserve()`. FTS5's own default record
//! of 4050 was chosen to fit a plain 4096 page; `db::schema::fts_pgsz_for`
//! derives it from the profile instead. Measured at 120k files,
//! `schema::PAGE_SIZE` = 8192:
//!
//! | arm | size | fts leaves | overflow |
//! |---|---|---|---|
//! | plain, pgsz 4050 | 131.3 MiB | 10986 | 0 |
//! | plain, derived | 130.8 MiB | 10922 | 0 |
//! | keyed, pgsz 4050 | 131.2 MiB | 10986 | 0 |
//! | keyed, derived | 130.9 MiB | 10942 | 0 |
//!
//! Encrypted over plain on disk: 1.001x.
//!
//! The `_4050` arms check that the derived record size is never larger on disk
//! than FTS5's fixed default, in both key states. At a 16-byte keyed reserve a
//! leaf holds 8141 bytes, so two 4052-byte records fit either way and the
//! margin is small. The default is wrong at other geometries: under
//! HMAC-SHA512's 80-byte reserve a keyed 8192 page held one record, and the
//! index came out at 221.0 MiB, 1.688x plain.
//!
//! Leaf geometry does not move the query times here beyond this seed's noise:
//! the search cache serves the working set either way. `benches/page_geometry.rs`
//! times geometry on corpora that do not fit, and `benches/cipher_hmac.rs`
//! prices the authenticator.
//!
//! Its own integration binary because it installs a process-global key; see
//! `tests/encrypted.rs`.

use std::time::{Duration, Instant};

use quicksearch_core::db::schema::recommended_search_cache_mib;
use quicksearch_core::search::{find_duplicate_groups, SearchOptions};
use quicksearch_core::testutil::{
    mib, run_cascade, seed_arms, Arm, SeedSpec, ARM_KEYED, ARM_KEYED_4050, ARM_PLAIN,
    ARM_PLAIN_4050, BODY_TERM, NEEDLE,
};

/// Ceiling on encrypted/plain for one workload. The worst shape measures
/// 1.20x, so 2.0 leaves 66% headroom and still fails the 3.9x row-fetch shape
/// outright. Raising it without a measurement in the table above defeats the
/// gate.
const MAX_RATIO: f64 = 2.0;

/// Ceiling on the encrypted index's *size* over the plain one, both as
/// shipped. Measured at 1.001x: `fts_pgsz_for` hands the reserve back to the
/// leaves. The ceiling leaves room for a corpus whose table mix differs.
const MAX_SIZE_RATIO: f64 = 1.03;

/// Enough rows that neither shipped index fits in its search cache (see
/// [`search_cache_bytes`]), the only regime where a per-page decrypt is
/// visible. Below that the cache serves both arms, every ratio is 1.0, and the
/// gate tests nothing; the test asserts this seed clears it. At
/// `schema::PAGE_SIZE` = 8192 it also keeps every workload above
/// [`MIN_MEASURABLE`], which 60k did not. Grow it when the code outruns it.
const FILES: usize = 120_000;
const CONTENT_EVERY: usize = 5;

/// Best-of-N: the minimum is the run least disturbed by the rest of the box;
/// a mean would measure the CI runner's other tenants.
const RUNS: u32 = 5;

/// Below this, a ratio is noise over noise: two sub-millisecond timings divide
/// into anything.
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

/// The search cache ceiling for an index of [`FILES`] rows in `arm`'s key
/// state, from `db::schema::recommended_search_cache_mib`.
fn search_cache_bytes(arm: &Arm) -> u64 {
    recommended_search_cache_mib(FILES as i64, arm.keyed) as u64 * 1024 * 1024
}

/// The fastest of [`RUNS`] timings.
fn best_of(mut run: impl FnMut() -> Duration) -> Duration {
    (0..RUNS).map(|_| run()).min().expect("RUNS > 0")
}

/// One workload timed on every arm, in `seed_arms` order. Collected rather
/// than asserted inline so a run reports every ratio, not just the first
/// failure.
struct Measured {
    what: &'static str,
    per_arm: Vec<Duration>,
}

impl Measured {
    /// Encrypted over plain, both as shipped: the ratio this file gates.
    fn ratio(&self) -> f64 {
        self.per_arm[ARM_KEYED].as_secs_f64() / self.per_arm[ARM_PLAIN].as_secs_f64()
    }

    fn line(&self) -> String {
        let times: String = self
            .per_arm
            .iter()
            .map(|d| format!("{:>18.2?}", d))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{:<28}{}   ratio {:>5.2}x", self.what, times, self.ratio())
    }
}

/// Time `find_duplicate_groups`, which opens its own connection, so the arm's
/// key has to be installed at call time.
fn time_duplicates(arm: &Arm) -> Duration {
    let db_path = arm.path.to_string_lossy().into_owned();
    arm.with_key(|| {
        best_of(|| {
            let start = Instant::now();
            let groups = find_duplicate_groups(&db_path, 200).expect("duplicate scan");
            let took = start.elapsed();
            assert!(!groups.is_empty(), "the seed must contain duplicate groups");
            took
        })
    })
}

/// Time one cascade query on a connection opened under its arm's key state.
/// An open connection keeps its own codec, so no key needs installing here.
fn time_query(conn: &rusqlite::Connection, query: &str, fuzzy: bool) -> Duration {
    let options = SearchOptions {
        fuzzy,
        ..SearchOptions::default()
    };
    best_of(|| {
        let (took, hits) = run_cascade(conn, query, &options);
        assert!(hits > 0, "'{}' must match something to be timed", query);
        took
    })
}

#[test]
fn encryption_costs_a_constant_factor_not_a_different_algorithm() {
    let arms = seed_arms("encperf", &spec());

    // Every connection is opened up front, each under its own key state.
    let conns: Vec<rusqlite::Connection> = arms.iter().map(Arm::open_search).collect();

    println!(
        "seeded {} files ({} with content) per arm\n",
        FILES,
        FILES / CONTENT_EVERY,
    );
    println!(
        "{:<28}{:>10}{:>12}{:>12}",
        "arm", "size", "fts leaves", "overflow"
    );
    for arm in &arms {
        let (leaf, overflow) = arm.fts_pages();
        println!(
            "{:<28}{:>11}{:>12}{:>12}",
            arm.what,
            mib(arm.size_bytes()),
            leaf,
            overflow
        );
    }
    println!();

    for arm in [&arms[ARM_PLAIN], &arms[ARM_KEYED]] {
        assert!(
            arm.size_bytes() > search_cache_bytes(arm),
            "the {} seed is smaller than its {} search cache, so it would be \
             served entirely from memory and every ratio below would be a \
             meaningless 1.0 — raise FILES",
            arm.what,
            mib(search_cache_bytes(arm))
        );
    }

    // Duplicate finding first: it is the shape this gate exists for.
    let mut measured = vec![Measured {
        what: "find_duplicate_groups",
        per_arm: arms.iter().map(time_duplicates).collect(),
    }];

    // The cascade's shapes. Arms alternate per workload so a machine that
    // slows down partway through moves all of them, not one.
    for (what, query, fuzzy) in [
        ("cascade literal name", NEEDLE, false),
        ("cascade literal body", BODY_TERM, false),
        ("cascade fuzzy", "quartzlte", true),
        ("cascade wildcard", "quart*", false),
        ("cascade regex", "regex:quart[sz]ite", false),
    ] {
        measured.push(Measured {
            what,
            per_arm: conns
                .iter()
                .map(|conn| time_query(conn, query, fuzzy))
                .collect(),
        });
    }

    println!(
        "\n{:<28}{}",
        "workload",
        arms.iter()
            .map(|a| format!("{:>18}", a.what))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for m in &measured {
        println!("{}", m.line());
    }

    // The derived record size against FTS5's fixed 4050; see the module doc.
    for (pinned, derived, what) in [
        (ARM_PLAIN_4050, ARM_PLAIN, "plain"),
        (ARM_KEYED_4050, ARM_KEYED, "keyed"),
    ] {
        let (before, after) = (arms[pinned].size_bytes(), arms[derived].size_bytes());
        assert!(
            after <= before,
            "the derived pgsz costs the {} index space: {} against {} on \
             FTS5's fixed 4050",
            what,
            mib(after),
            mib(before)
        );
    }
    let size_ratio = arms[ARM_KEYED].size_bytes() as f64 / arms[ARM_PLAIN].size_bytes() as f64;
    println!("\nencrypted/plain on disk: {:.3}x", size_ratio);
    assert!(
        size_ratio <= MAX_SIZE_RATIO,
        "an encrypted index is {:.3}x the plain one on disk, over the {:.2}x \
         ceiling — the usual cause is FTS5 leaves that no longer fit inside \
         SQLCipher's reduced usable page",
        size_ratio,
        MAX_SIZE_RATIO
    );

    // Only the shipped pair: nothing is asserted about the `pgsz 4050` arms'
    // timings.
    let too_short: Vec<&Measured> = measured
        .iter()
        .filter(|m| m.per_arm[ARM_PLAIN] < MIN_MEASURABLE || m.per_arm[ARM_KEYED] < MIN_MEASURABLE)
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
