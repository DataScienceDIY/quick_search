//! Encryption must cost a constant factor, not a different algorithm.
//!
//! SQLCipher AES-decrypts every page it reads, so a keyed index is
//! intrinsically slower than a plain one — that part is not a bug and this
//! file does not try to gate it. What it gates is *amplification*: a query
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
//! this a *test* rather than a benchmark — every arm runs the same workload on
//! the same machine in the same process, so host speed, CPU governor and CI
//! contention divide out. Absolute times are printed but never asserted.
//!
//! Since `db::schema::HMAC_MODE` became `Off` the constant factor is much
//! smaller — every shape here now runs 1.03–1.20x, where the same shapes were
//! up to 1.3x with a per-page HMAC-SHA512 to pay as well.
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
//! Encrypted over plain on disk: **1.001x**.
//!
//! **The `_4050` arms no longer demonstrate much, and that is the change
//! rather than a defect in them.** They existed because a keyed page used to
//! give up 80 bytes, which left a keyed 8192 page holding only *one*
//! 4052-byte record — two would not fit under the 8077-byte limit — so half of
//! every page went empty and the index came out at 221.0 MiB, 1.688x plain.
//! At a 16-byte reserve the limit is 8141 and two fit with room, so FTS5's
//! fixed default happens to be fine here. It is still wrong at other page
//! sizes, which is why the derivation stays and why these arms still assert
//! `derived <= pinned` — just with a much smaller margin than they used to.
//!
//! The query times are unmoved by leaf geometry, within this seed's noise: the
//! working set is served from the search cache either way, so it shows up on
//! disk long before it shows up here. `benches/page_geometry.rs` is where it
//! is timed, on corpora that do not fit, and `benches/cipher_hmac.rs` is where
//! the authenticator itself was priced.
//!
//! Its own integration binary because it installs a process-global key, the
//! same reason `tests/encrypted.rs` gives.

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, find_duplicate_groups, SearchHit, SearchOptions};
use quicksearch_core::testutil::{
    measurement_key, seed_arms, Arm, SeedSpec, ARM_KEYED, ARM_KEYED_4050, ARM_PLAIN,
    ARM_PLAIN_4050, BODY_TERM, NEEDLE,
};

/// Ceiling on encrypted/plain for one workload. It still has to sit under the
/// 3.9x the old duplicate query cost — that is the regression this gate is
/// for — but it no longer has to leave room for a per-page HMAC: with
/// `HMAC_MODE` off the worst shape measures 1.20x, so 2.0 is 66% of headroom
/// over the worst observed and still fails the amplified shape outright.
/// Raising this without a measurement in the table above defeats it.
const MAX_RATIO: f64 = 2.0;

/// Ceiling on the encrypted index's *size* relative to the plain one, both as
/// shipped. Measured at 1.001x: `fts_pgsz_for` hands the reserve back to the
/// leaves, so a protected index is now the same size as an unprotected one.
/// The ceiling keeps room for a corpus whose table mix differs.
const MAX_SIZE_RATIO: f64 = 1.03;

/// Enough rows that neither index fits in `PRAGMAS_SEARCH`'s 32 MiB page
/// cache — the only regime where a per-page decrypt is visible at all. Below
/// that both arms are served from cache, every ratio is 1.0, and the gate
/// silently stops testing anything. The assertion below pins that this seed
/// still clears it.
///
/// Raised from 60k when `schema::PAGE_SIZE` became 8192: the same queries got
/// fast enough that `cascade literal name` and `cascade wildcard` fell under
/// [`MIN_MEASURABLE`], which is that guard working, not failing. The seed has
/// to grow when the code outruns it.
const FILES: usize = 120_000;
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

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
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

/// One workload timed on every arm, in `seed_arms` order. Collected rather
/// than asserted inline so a run reports *every* ratio, not just the first one
/// that failed.
struct Measured {
    what: &'static str,
    per_arm: Vec<Duration>,
}

impl Measured {
    /// Encrypted over plain, both as shipped — the ratio this file exists to
    /// gate.
    fn ratio(&self) -> f64 {
        self.per_arm[SHIPPED_KEYED].as_secs_f64() / self.per_arm[SHIPPED_PLAIN].as_secs_f64()
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

/// Time `find_duplicate_groups`, which opens its own connection — so the
/// process key has to be right at call time, not at open time.
fn time_duplicates(arm: &Arm) -> Duration {
    let db_path = arm.path.to_string_lossy().into_owned();
    let keyed = arm.keyed;
    let out = best_of(|| {
        db::set_process_key(keyed.then(measurement_key));
        let groups = find_duplicate_groups(&db_path, 200).expect("duplicate scan");
        assert!(!groups.is_empty(), "the seed must contain duplicate groups");
    });
    db::set_process_key(None);
    out
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

/// Aliases for `testutil`'s arm order, naming the pair that is the shipped
/// product; the other two exist only to price the change against.
const SHIPPED_PLAIN: usize = ARM_PLAIN;
const SHIPPED_KEYED: usize = ARM_KEYED;

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
            "{:<28}{:>7.1} MiB{:>12}{:>12}",
            arm.what,
            mib(arm.size_bytes()),
            leaf,
            overflow
        );
    }
    println!();

    assert!(
        arms[SHIPPED_PLAIN].size_bytes() > SEARCH_CACHE_BYTES,
        "seed is smaller than the {} MiB search cache, so every arm would be \
         served entirely from memory and every ratio below would be a \
         meaningless 1.0 — raise FILES",
        SEARCH_CACHE_BYTES / (1024 * 1024)
    );

    // Duplicate finding first: it is the shape this gate exists for.
    let mut measured = vec![Measured {
        what: "find_duplicate_groups",
        per_arm: arms.iter().map(time_duplicates).collect(),
    }];

    // The cascade's four shapes. Arms alternate per workload so a machine that
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

    // Deriving the record size from the page size has to beat pinning FTS5's
    // own 4050 — for *both* key states. It used to be a keyed-only concern,
    // when the page size was the 4096 that 4050 was chosen for; at
    // `schema::PAGE_SIZE` neither key state gets a fitting leaf by accident.
    for (pinned, derived, what) in [
        (ARM_PLAIN_4050, SHIPPED_PLAIN, "plain"),
        (ARM_KEYED_4050, SHIPPED_KEYED, "keyed"),
    ] {
        let (before, after) = (arms[pinned].size_bytes(), arms[derived].size_bytes());
        assert!(
            after <= before,
            "the derived pgsz costs the {} index space: {:.1} MiB against \
             {:.1} MiB on FTS5's fixed 4050",
            what,
            mib(after),
            mib(before)
        );
    }
    let keyed_after = arms[SHIPPED_KEYED].size_bytes();
    let size_ratio = keyed_after as f64 / arms[SHIPPED_PLAIN].size_bytes() as f64;
    println!("\nencrypted/plain on disk: {:.3}x", size_ratio);
    assert!(
        size_ratio <= MAX_SIZE_RATIO,
        "an encrypted index is {:.3}x the plain one on disk, over the {:.2}x \
         ceiling — the usual cause is FTS5 leaves that no longer fit inside \
         SQLCipher's reduced usable page",
        size_ratio,
        MAX_SIZE_RATIO
    );

    // Only the shipped pair: nothing is asserted about the two `pgsz 4050`
    // arms, so their timings being at the noise floor costs a reader nothing.
    let too_short: Vec<&Measured> = measured
        .iter()
        .filter(|m| {
            m.per_arm[SHIPPED_PLAIN] < MIN_MEASURABLE || m.per_arm[SHIPPED_KEYED] < MIN_MEASURABLE
        })
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
