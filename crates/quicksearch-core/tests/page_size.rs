//! Every layout the benches sweep survives a round trip through the real open
//! path, keyed and plain, and an index under a previous layout is rebuilt.
//!
//! Its own integration binary, with one `#[test]`, because it sets the
//! process-global key, page size and HMAC mode; see `tests/encrypted.rs`.
//!
//! The keyed half matters most. A keyed file's header is ciphertext, so
//! SQLCipher cannot read the layout off it: under the wrong page size or the
//! wrong HMAC mode, which sets the page reserve, the header decrypts to noise
//! and `key_and_probe` reports `KEY_MISMATCH: wrong-password`. This pins that a
//! file reads back under the layout it was written with, so only a change to
//! `schema::PROFILE` can reach that failure.

use quicksearch_core::db;
use quicksearch_core::db::schema::{HmacMode, Profile};
use quicksearch_core::testutil::{
    fts_page_counts, measurement_key, seed_index, Arm, Scratch, SeedSpec,
};

/// The sweep, plus 16384 to keep one size above anything considered.
const SWEPT: [i64; 5] = [1024, 2048, 4096, 8192, 16384];

/// Every authenticator `benches/cipher_hmac.rs` prices. The reserve is the
/// other half of the layout, and just as fatal to get wrong.
const MODES: [HmacMode; 3] = [HmacMode::Off, HmacMode::Sha256, HmacMode::Sha512];

/// Enough documents to fill real FTS5 leaves at the largest page size here;
/// below that, nothing would reach the inline limit and the overflow assertion
/// would pass on a broken derivation.
const FILES: usize = 4_000;

fn spec(profile: Profile) -> SeedSpec {
    SeedSpec {
        files: FILES,
        content_every: 2,
        page_size: Some(profile.page_size),
        hmac: Some(profile.hmac),
        ..SeedSpec::default()
    }
}

/// `PRAGMA page_size`, which answers as TEXT on a keyed connection and INTEGER
/// otherwise; `db::repo::pragma_number` handles the same quirk but is
/// crate-private.
fn page_size_of(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("PRAGMA page_size", [], |r| {
        Ok(match r.get_ref(0)? {
            rusqlite::types::ValueRef::Integer(n) => n,
            rusqlite::types::ValueRef::Text(t) => {
                std::str::from_utf8(t).unwrap().trim().parse().unwrap()
            }
            other => panic!("page_size answered {:?}", other.data_type()),
        })
    })
    .unwrap()
}

fn file_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap()
}

/// Whether the file at `path` starts with SQLite's plaintext header.
fn is_plaintext(path: &std::path::Path) -> bool {
    std::fs::read(path).unwrap()[..16] == *b"SQLite format 3\0"
}

/// One `#[test]` for the reason the header gives: every phase drives the same
/// process globals, so concurrent phases would move each other's layout out
/// from under them.
#[test]
fn page_geometry_round_trips_and_older_files_rebuild() {
    every_swept_page_size_round_trips_keyed_and_plain();
    every_hmac_mode_round_trips();
    an_index_under_a_previous_profile_is_rebuilt_not_called_a_wrong_password();
}

fn every_swept_page_size_round_trips_keyed_and_plain() {
    round_trips(SWEPT.map(|page_size| Profile {
        page_size,
        hmac: db::schema::HMAC_MODE,
    }));
}

/// A keyed file written under one authenticator and read under another does
/// not decrypt at all, so `benches/cipher_hmac.rs` can compare modes only if
/// each survives its own open, and the FTS5 derivation has to follow the
/// reserve or the measured arm is full of overflow pages.
fn every_hmac_mode_round_trips() {
    round_trips(MODES.map(|hmac| Profile {
        page_size: db::schema::PAGE_SIZE,
        hmac,
    }));
}

/// Seed each profile keyed and plain, reopen it fresh, and check it came back
/// whole, at its page size, with every FTS5 leaf inline, and encrypted if keyed.
fn round_trips(profiles: impl IntoIterator<Item = Profile>) {
    for profile in profiles {
        for keyed in [false, true] {
            let what = format!("profile=({}) keyed={}", profile, keyed);
            let tag = format!(
                "roundtrip-{}-{}-{}",
                profile.page_size,
                profile.hmac.label(),
                keyed
            );
            let arm = Arm::seed(&what, &tag, keyed, &spec(profile));

            // A *fresh* open, where a keyed file under an unexpected layout
            // fails outright. The connection keeps its codec after `with_key`
            // restores the shipped globals.
            let conn = arm
                .with_key(|| db::open_existing(&arm.path.to_string_lossy(), false))
                .unwrap_or_else(|e| panic!("{}: reopen: {}", what, e));

            assert_eq!(
                page_size_of(&conn),
                profile.page_size,
                "{}: the file came back at another size",
                what
            );
            assert_eq!(
                file_count(&conn),
                FILES as i64,
                "{}: reopen must find the corpus, not wipe it",
                what
            );

            // The derived pgsz keeps FTS5 leaves inline at every layout.
            let (leaves, overflow) = fts_page_counts(&conn);
            assert!(
                leaves > 10,
                "{}: {} leaves is too few to have filled any",
                what,
                leaves
            );
            assert_eq!(
                overflow, 0,
                "{}: {} of {} leaves overflowed — the derived pgsz missed the \
                 inline limit this layout sets",
                what, overflow, leaves
            );
            drop(conn);

            // `cipher_use_hmac = OFF` drops the authenticator, not the cipher.
            if keyed {
                assert!(
                    !is_plaintext(&arm.path),
                    "{}: the file is plaintext, not merely unauthenticated",
                    what
                );
            }
            arm.discard();
        }
    }
}

/// Indexes already on disk were built under a `PROFILES_PREVIOUS` layout, and
/// a keyed one read under the wrong page size or page reserve decrypts to
/// noise. Without the reopen-under-the-old-profile retry every such index would
/// come back as `KEY_MISMATCH: wrong-password`, against a correct password.
///
/// Each entry is checked with the schema version rolled back, as a page-size
/// change always came with, and at the current version, as an HMAC change
/// leaves it: that file reads back under its old profile, so only the profile
/// says it is stale.
fn an_index_under_a_previous_profile_is_rebuilt_not_called_a_wrong_password() {
    // An empty list would make this test vacuous, and an empty list is the
    // regression it exists to catch.
    assert!(
        !db::schema::PROFILES_PREVIOUS.is_empty(),
        "PROFILES_PREVIOUS is empty: every index built under an earlier \
         layout now reports KEY_MISMATCH instead of rebuilding, and this test \
         would have said nothing about it"
    );
    for keyed in [false, true] {
        for &previous in db::schema::PROFILES_PREVIOUS {
            for stale_version in [true, false] {
                check_previous_profile(previous, keyed, stale_version);
            }
        }
    }
}

/// Build an index the way an older version would have under `previous`, then
/// open it as this build does. Leaves the process globals at the shipped
/// profile with no key.
fn check_previous_profile(previous: Profile, keyed: bool, stale_version: bool) {
    let what = format!(
        "profile=({}) keyed={} stale_version={}",
        previous, keyed, stale_version
    );
    let (_dir, path) = Scratch::db(&format!(
        "prevprofile-{}-{}-{}-{}",
        previous.page_size,
        previous.hmac.label(),
        keyed,
        stale_version
    ));
    let db_path = path.to_string_lossy().into_owned();

    db::set_process_key(keyed.then(measurement_key));
    seed_index(&path, &spec(previous));
    {
        let conn = db::open_existing(&db_path, true).unwrap();
        assert_eq!(
            page_size_of(&conn),
            previous.page_size,
            "{}: the fixture must actually be at {} bytes",
            what,
            previous.page_size
        );
        if stale_version {
            conn.execute(
                "UPDATE schema_info SET value = ?1 WHERE key = 'version'",
                [(db::CURRENT_SCHEMA_VERSION - 1).to_string()],
            )
            .unwrap();
        }
    }

    // Back to the shipped layout, as a new build would be.
    db::set_page_size_override(db::schema::PAGE_SIZE);
    db::set_hmac_mode_override(db::schema::HMAC_MODE);

    // A plain file at the current version stays usable: it has no reserve, so
    // the HMAC half never applied to it, and SQLite ignores `PRAGMA page_size`
    // on an existing file. Only the schema version can condemn it. Pinned so
    // this stays a decision rather than a gap.
    if !keyed && !stale_version {
        db::open_existing(&db_path, false).unwrap_or_else(|e| {
            panic!(
                "{}: a plain index at the current schema version must stay \
                 usable whatever its page size: {}",
                what, e
            )
        });
        assert!(
            !db::index_needs_rebuild(&db_path),
            "{}: and it must not be condemned to a rebuild either",
            what
        );
        return;
    }

    // The key still verifies: it is the right key, on a file whose layout this
    // build no longer writes.
    db::verify_process_key(&db_path)
        .unwrap_or_else(|e| panic!("{}: a correct password was refused: {}", what, e));

    // A consumer is told to re-index, not that the password is wrong.
    let refusal = db::open_existing(&db_path, false).unwrap_err();
    assert!(
        !refusal.starts_with(db::KEY_MISMATCH_PREFIX),
        "{}: consumers must not see a key error: {}",
        what,
        refusal
    );
    assert!(
        refusal.contains("Re-index"),
        "{}: the refusal must say what to do: {}",
        what,
        refusal
    );

    // The indexer announces the rebuild before doing it.
    assert!(
        db::index_needs_rebuild(&db_path),
        "{}: the rebuild must be announced",
        what
    );

    // The rebuild lands at the current layout, encrypted if it was.
    let conn = db::open_or_recreate(&db_path, "trigram")
        .unwrap_or_else(|e| panic!("{}: rebuild failed: {}", what, e));
    assert_eq!(
        page_size_of(&conn),
        db::schema::PAGE_SIZE,
        "{}: the rebuilt file must adopt the current page size",
        what
    );
    assert_eq!(
        file_count(&conn),
        0,
        "a rebuild starts empty; the walk refills it"
    );
    drop(conn);
    if keyed {
        assert!(
            !is_plaintext(&path),
            "a rebuilt protected index must come back encrypted"
        );
    }
    db::set_process_key(None);
}
