//! Every layout the benches sweep has to survive a round trip through the real
//! open path, keyed and plain.
//!
//! Its own integration binary, and one `#[test]`, for the reason
//! `tests/encrypted.rs` gives: it mutates process-global state — the key,
//! `db::set_page_size_override` and `db::set_hmac_mode_override` — which unit
//! tests must never do, because the lib test binary runs them in parallel
//! against the same globals.
//!
//! The keyed half is the one that matters. A keyed file's header is
//! ciphertext, so SQLCipher cannot discover the layout by reading it: told the
//! wrong page size *or* the wrong HMAC mode — which sets the page reserve —
//! the header decrypts to noise and `key_and_probe` reports
//! `KEY_MISMATCH: wrong-password`. This pins that what we write with is what
//! we read back with, so that failure mode stays reachable only by actually
//! changing `schema::PROFILE` — which its doc comments spell out.

use quicksearch_core::db;
use quicksearch_core::db::schema::{HmacMode, Profile};
use quicksearch_core::testutil::{measurement_key, scratch_db, seed_index, SeedSpec};

/// The sweep, plus 16384 to keep one size above anything considered.
const SWEPT: [i64; 5] = [1024, 2048, 4096, 8192, 16384];

/// Every authenticator `benches/cipher_hmac.rs` prices. A page size is only
/// half the layout; the reserve is the other half and it is just as fatal to
/// get wrong.
const MODES: [HmacMode; 3] = [HmacMode::Off, HmacMode::Sha256, HmacMode::Sha512];

/// Enough documents to fill real FTS5 leaves at the largest page size here;
/// below that, nothing would ever reach the inline limit and the overflow
/// assertion would pass on a broken derivation.
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

/// Put the process globals back where a fresh run would have them.
fn restore_shipped_profile() {
    db::set_process_key(None);
    db::set_page_size_override(db::schema::PAGE_SIZE);
    db::set_hmac_mode_override(db::schema::HMAC_MODE);
}

/// `PRAGMA page_size` answers as TEXT on a keyed connection and INTEGER
/// otherwise — the same quirk `db::repo::pragma_number` exists for, which is
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

/// One `#[test]`, two phases, for the reason the header gives: both phases
/// drive the same process globals, so running them concurrently would have
/// each one moving the other's page size out from under it.
#[test]
fn page_geometry_round_trips_and_older_files_rebuild() {
    every_swept_page_size_round_trips_keyed_and_plain();
    every_hmac_mode_round_trips();
    an_index_under_a_previous_profile_is_rebuilt_not_called_a_wrong_password();
}

fn every_swept_page_size_round_trips_keyed_and_plain() {
    for page_size in SWEPT {
        for keyed in [false, true] {
            let profile = Profile {
                page_size,
                hmac: db::schema::HMAC_MODE,
            };
            let path = scratch_db(&format!("pagesize-{}-{}", page_size, keyed));
            db::set_process_key(keyed.then(measurement_key));
            seed_index(&path, &spec(profile));

            // A *fresh* open, which is where a keyed file at an unexpected
            // page size would fail outright.
            let conn = db::open_existing(&path.to_string_lossy(), false).unwrap_or_else(|e| {
                panic!("reopen page_size={} keyed={}: {}", page_size, keyed, e)
            });

            assert_eq!(
                page_size_of(&conn),
                page_size,
                "page_size={} keyed={}: the file came back at another size",
                page_size,
                keyed
            );

            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                rows, FILES as i64,
                "reopen must find the corpus, not wipe it"
            );

            // The derived pgsz has to keep FTS5 leaves inline at every size,
            // which is the whole reason it is derived rather than pinned.
            let overflow: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM dbstat \
                     WHERE name = 'searchabletext_data' AND pagetype = 'overflow'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let leaves: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM dbstat \
                     WHERE name = 'searchabletext_data' AND pagetype = 'leaf'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                leaves > 10,
                "page_size={} keyed={}: {} leaves is too few to have filled any",
                page_size,
                keyed,
                leaves
            );
            assert_eq!(
                overflow, 0,
                "page_size={} keyed={}: {} of {} leaves overflowed — the \
                 derived pgsz missed the inline limit",
                page_size, keyed, overflow, leaves
            );

            drop(conn);
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }
    }

    restore_shipped_profile();
}

/// The same round trip across the other half of the layout. A keyed file
/// written under one authenticator and read under another does not decrypt at
/// all, so `benches/cipher_hmac.rs` can only compare modes if each one
/// survives its own open — and the FTS5 derivation has to follow the reserve
/// or the arm being measured is one full of overflow pages.
fn every_hmac_mode_round_trips() {
    for hmac in MODES {
        for keyed in [false, true] {
            let profile = Profile {
                page_size: db::schema::PAGE_SIZE,
                hmac,
            };
            let path = scratch_db(&format!("hmac-{}-{}", hmac.label(), keyed));
            db::set_process_key(keyed.then(measurement_key));
            db::set_hmac_mode_override(hmac);
            seed_index(&path, &spec(profile));

            let conn = db::open_existing(&path.to_string_lossy(), false)
                .unwrap_or_else(|e| panic!("reopen hmac={:?} keyed={}: {}", hmac, keyed, e));

            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                rows, FILES as i64,
                "hmac={:?} keyed={}: reopen must find the corpus, not wipe it",
                hmac, keyed
            );

            let overflow: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM dbstat \
                     WHERE name = 'searchabletext_data' AND pagetype = 'overflow'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let leaves: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM dbstat \
                     WHERE name = 'searchabletext_data' AND pagetype = 'leaf'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                leaves > 10,
                "hmac={:?} keyed={}: {} leaves is too few to have filled any",
                hmac,
                keyed,
                leaves
            );
            assert_eq!(
                overflow, 0,
                "hmac={:?} keyed={}: {} of {} leaves overflowed — the derived \
                 pgsz did not follow the reserve this mode sets",
                hmac, keyed, overflow, leaves
            );

            // A protected arm has to actually be encrypted whatever the
            // authenticator: `cipher_use_hmac = OFF` weakens the file, it does
            // not turn the cipher off.
            drop(conn);
            if keyed {
                let head = std::fs::read(&path).unwrap();
                assert_ne!(
                    &head[..16],
                    b"SQLite format 3\0",
                    "hmac={:?}: the file is plaintext, not merely unauthenticated",
                    hmac
                );
            }
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }
    }

    restore_shipped_profile();
}

/// The migration this build has to survive: indexes already on disk were built
/// under a `PROFILES_PREVIOUS` layout, and a keyed one read under the wrong
/// page size *or* the wrong page reserve decrypts to noise. Without the
/// reopen-under-the-old-profile retry every such index would come back as
/// `KEY_MISMATCH: wrong-password` — an accusation the user cannot act on,
/// against a password that is perfectly correct.
///
/// Each entry is checked twice over: once with the schema version rolled back,
/// as a page-size change always came with, and once left at the current
/// version. The second case is the one an HMAC change introduces — the file
/// reads back perfectly under its old profile, so nothing but the profile
/// itself says it is stale.
fn an_index_under_a_previous_profile_is_rebuilt_not_called_a_wrong_password() {
    // This test iterates the list, so an empty one would make it vacuous
    // rather than failing — and an empty one is exactly the regression it
    // exists to catch. Entries may only be dropped when it is acceptable for
    // indexes under that layout to read as a wrong password.
    assert!(
        !db::schema::PROFILES_PREVIOUS.is_empty(),
        "PROFILES_PREVIOUS is empty: every index built under an earlier \
         layout now reports KEY_MISMATCH instead of rebuilding, and this test \
         would have said nothing about it"
    );
    let mut checked = 0;
    for keyed in [false, true] {
        for previous in db::schema::PROFILES_PREVIOUS {
            for stale_version in [true, false] {
                checked += 1;
                let tag = format!(
                    "prevprofile-{}-{}-{}-{}",
                    previous.page_size,
                    previous.hmac.label(),
                    keyed,
                    stale_version
                );
                let path = scratch_db(&tag);
                let db_path = path.to_string_lossy().into_owned();
                let what = format!(
                    "profile=({}) keyed={} stale_version={}",
                    previous, keyed, stale_version
                );

                // Build one the way the old version would have.
                db::set_process_key(keyed.then(measurement_key));
                seed_index(&path, &spec(*previous));
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

                // An unencrypted file is the one case where the profile is not
                // a staleness signal, and that is by design: it has no
                // reserve, so the HMAC half never applied to it, and SQLite
                // ignores `PRAGMA page_size` on a file that exists, so it just
                // opens at whatever it was built with. Only the schema version
                // can condemn it — and the version is what a page-size change
                // has always moved alongside. Pin the *positive* behaviour so
                // this stays a decision rather than a gap.
                if !keyed && !stale_version {
                    db::open_existing(&db_path, false).unwrap_or_else(|e| {
                        panic!(
                            "{}: a plain index at the current schema version \
                             must stay usable whatever its page size: {}",
                            what, e
                        )
                    });
                    assert!(
                        !db::index_needs_rebuild(&db_path),
                        "{}: and it must not be condemned to a rebuild either",
                        what
                    );
                    db::set_process_key(None);
                    std::fs::remove_dir_all(path.parent().unwrap()).ok();
                    continue;
                }

                // The key still verifies: it is the right key, on a file whose
                // layout this build no longer writes.
                db::verify_process_key(&db_path)
                    .unwrap_or_else(|e| panic!("{}: a correct password was refused: {}", what, e));

                // A consumer is told to re-index rather than that the file is
                // unreadable or the password wrong.
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

                // And the indexer announces the rebuild before doing it.
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
                let rows: i64 = conn
                    .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(rows, 0, "a rebuild starts empty; the walk refills it");
                drop(conn);

                if keyed {
                    let head = std::fs::read(&path).unwrap();
                    assert_ne!(
                        &head[..16],
                        b"SQLite format 3\0",
                        "a rebuilt protected index must come back encrypted"
                    );
                }

                db::set_process_key(None);
                std::fs::remove_dir_all(path.parent().unwrap()).ok();
            }
        }
    }
    assert_eq!(
        checked,
        4 * db::schema::PROFILES_PREVIOUS.len(),
        "every previous profile has to be checked keyed and plain, at a stale \
         schema version and at the current one"
    );
    restore_shipped_profile();
}
