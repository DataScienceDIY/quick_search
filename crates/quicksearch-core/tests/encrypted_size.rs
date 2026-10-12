//! An encrypted index takes no more disk than a plain one, and the derived
//! FTS5 record size never takes more than FTS5's default.
//!
//! Four arms, because the second variable is FTS5's *record* size. A table
//! leaf holds `page − reserve − 35` bytes inline, where a plain file's reserve
//! is 0 and a keyed one's is `HMAC_MODE.reserve()`. FTS5's own default record
//! of 4050 was chosen to fit a plain 4096 page; `db::schema::fts_pgsz_for`
//! derives it from the profile instead. Measured at 120k files,
//! `schema::PAGE_SIZE` = 8192, under a run's FTS5 settings and finalize merge
//! (`testutil::seed_index`):
//!
//! | arm | size | in use | fts leaves | overflow |
//! |---|---|---|---|---|
//! | plain, pgsz 4050 | 43.7 MiB | 43.7 MiB | 886 | 0 |
//! | plain, derived | 45.8 MiB | 43.6 MiB | 875 | 0 |
//! | keyed, pgsz 4050 | 43.7 MiB | 43.7 MiB | 886 | 0 |
//! | keyed, derived | 45.8 MiB | 43.6 MiB | 876 | 0 |
//!
//! Encrypted over plain in use: 0.999x.
//!
//! Both gates compare bytes in use, the file less its freelist. The finalize
//! merge leaves a derived arm in one segment, with its inputs' 2.1 MiB of
//! pages on the freelist, and a `_4050` arm in eight with none free: that
//! records whose merge finished, not what a record size costs, and the next
//! write reuses those pages.
//!
//! The `_4050` arms check that the derived record size never takes more space
//! than FTS5's fixed default, in both key states. At a 16-byte keyed reserve a
//! leaf holds 8141 bytes, so two 4052-byte records fit either way and the
//! margin is small. The default is wrong at other geometries: under
//! HMAC-SHA512's 80-byte reserve a keyed 8192 page held one record, and on the
//! seed as it was before a run's FTS5 settings the index came out at
//! 221.0 MiB, 1.688x plain.
//!
//! Timing lives in the benches: `benches/page_geometry.rs` times geometry,
//! and `benches/cipher_hmac.rs` prices the authenticator.
//!
//! Its own integration binary because it installs a process-global key; see
//! `tests/encrypted.rs`.

use quicksearch_core::testutil::{
    mib, seed_arms, SeedSpec, ARM_KEYED, ARM_KEYED_4050, ARM_PLAIN, ARM_PLAIN_4050,
};

/// Ceiling on the encrypted index's bytes in use over the plain one's, both
/// as shipped. Measured at 0.999x: `fts_pgsz_for` hands the reserve back to
/// the leaves. The ceiling leaves room for a corpus whose table mix differs.
const MAX_SIZE_RATIO: f64 = 1.03;

/// The corpus the table above was measured on.
const FILES: usize = 120_000;
const CONTENT_EVERY: usize = 5;

fn spec() -> SeedSpec {
    SeedSpec {
        files: FILES,
        content_every: CONTENT_EVERY,
        // A hash on every row, as the table above was measured with.
        dup_every: 5,
        ..SeedSpec::default()
    }
}

#[test]
fn an_encrypted_index_takes_no_more_disk_than_a_plain_one() {
    let arms = seed_arms("encsize", &spec());

    println!(
        "seeded {} files ({} with content) per arm\n",
        FILES,
        FILES / CONTENT_EVERY,
    );
    println!(
        "{:<28}{:>10}{:>11}{:>12}{:>12}",
        "arm", "size", "in use", "fts leaves", "overflow"
    );
    for arm in &arms {
        let (leaf, overflow) = arm.fts_pages();
        println!(
            "{:<28}{:>11}{:>11}{:>12}{:>12}",
            arm.what,
            mib(arm.size_bytes()),
            mib(arm.used_bytes()),
            leaf,
            overflow
        );
    }

    // The derived record size against FTS5's fixed 4050; see the module doc.
    for (pinned, derived, what) in [
        (ARM_PLAIN_4050, ARM_PLAIN, "plain"),
        (ARM_KEYED_4050, ARM_KEYED, "keyed"),
    ] {
        let (before, after) = (arms[pinned].used_bytes(), arms[derived].used_bytes());
        assert!(
            after <= before,
            "the derived pgsz costs the {} index space: {} against {} on \
             FTS5's fixed 4050",
            what,
            mib(after),
            mib(before)
        );
    }
    let size_ratio = arms[ARM_KEYED].used_bytes() as f64 / arms[ARM_PLAIN].used_bytes() as f64;
    println!("\nencrypted/plain in use: {:.3}x", size_ratio);
    assert!(
        size_ratio <= MAX_SIZE_RATIO,
        "an encrypted index uses {:.3}x the plain one's space, over the {:.2}x \
         ceiling — the usual cause is FTS5 leaves that no longer fit inside \
         SQLCipher's reduced usable page",
        size_ratio,
        MAX_SIZE_RATIO
    );

    for arm in arms {
        arm.discard();
    }
}
