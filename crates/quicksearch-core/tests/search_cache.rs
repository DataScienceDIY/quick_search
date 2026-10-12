//! The search connection has to *open* at the ceiling it was sized for.
//!
//! `db::schema`'s unit tests cover the arithmetic; this covers the wiring,
//! which is where it can silently do nothing: the ceiling is resolved inside
//! `open_search_reader` from the per-root counts a completed run stores and a
//! process-global, and a break anywhere along that path leaves a connection
//! quietly running on the read-only profile's 4 MiB with every test still
//! green. That a completed run stores counts equal to its rows is asserted in
//! `tests/full_index.rs`.
//!
//! Its own integration binary, with one `#[test]`, because it sets the
//! process-global key and cache override; see `tests/encrypted.rs`.

use quicksearch_core::db;
use quicksearch_core::db::repo::RootCounts;
use quicksearch_core::db::schema::{
    recommended_search_cache_mib, SEARCH_CACHE_MAX_MIB, SEARCH_CACHE_PLAIN_MIB,
};
use quicksearch_core::testutil::{measurement_key, seed_index, Scratch, SeedSpec};

/// Enough files that the recommendation clears the floor and is derived rather
/// than clamped: at 150 B/file, 16 MiB is reached around 112k. `seed_index`
/// stores that count, as a run over the index would. Seeded without content:
/// the FTS write is the slow part and this measures nothing about it.
const FILES: usize = 150_000;

fn spec() -> SeedSpec {
    SeedSpec {
        files: FILES,
        // No document bodies at all; `content_every` past `files` never fires.
        content_every: FILES + 1,
        commit_every: 25_000,
        dup_every: 2,
        dir_depth: 6,
        ..SeedSpec::default()
    }
}

/// Store `files` as the seed root's count, as a completed run does.
fn set_files(db_path: &str, files: usize) {
    let conn = db::open_existing(db_path, true).unwrap();
    let counts = RootCounts {
        files: files as i64,
        fts: 0,
    };
    db::repo::set_root_counts(&conn, "/seed", counts).unwrap();
}

/// Forget every root's counts, as for an index no run has completed.
fn clear_counts(db_path: &str) {
    let conn = db::open_existing(db_path, true).unwrap();
    db::repo::prune_root_stats(&conn, &[]).unwrap();
}

/// The ceiling a fresh search connection on `db_path` opens at.
fn opened_mib(db_path: &str) -> i64 {
    cache_mib(&db::open::open_search_reader(db_path).unwrap())
}

/// `PRAGMA cache_size` reads back as the negative KiB it was set to.
fn cache_mib(conn: &rusqlite::Connection) -> i64 {
    let kib: i64 = conn
        .query_row("PRAGMA cache_size", [], |r| r.get(0))
        .unwrap();
    assert!(
        kib < 0,
        "cache_size came back as {} — a positive value is a *page* count, \
         which would mean the KiB form never reached SQLite",
        kib
    );
    -kib / 1024
}

#[test]
fn the_search_connection_opens_at_the_ceiling_it_was_sized_for() {
    let (_dir, path) = Scratch::db("searchcache");
    let db_path = path.to_string_lossy().into_owned();

    // --- keyed, automatic -------------------------------------------------
    db::set_process_key(Some(measurement_key()));
    db::set_search_cache_override(None);
    seed_index(&path, &spec());
    let floor = recommended_search_cache_mib(0, true);
    let expected = recommended_search_cache_mib(FILES as i64, true);
    assert!(
        expected > floor,
        "the corpus must be big enough to clear the floor, or this proves nothing"
    );
    // `seed_index` stored the counts a completed run would.
    assert_eq!(
        opened_mib(&db_path),
        expected,
        "the derived ceiling did not reach the connection"
    );

    // Without counts there is nothing to derive from, so the floor is correct
    // and is what must come out.
    clear_counts(&db_path);
    assert_eq!(
        opened_mib(&db_path),
        floor,
        "an index with no completed run must fall back, not guess"
    );

    // A small index stays on the floor.
    set_files(&db_path, 1_000);
    assert_eq!(opened_mib(&db_path), floor, "a small index needs no more");

    // The next run's count is the next connection's ceiling, however little
    // the index grew.
    let grown = recommended_search_cache_mib((2 * FILES) as i64, true);
    assert!(grown > expected && grown < SEARCH_CACHE_MAX_MIB);
    set_files(&db_path, 2 * FILES);
    assert_eq!(
        opened_mib(&db_path),
        grown,
        "a grown index must grow its ceiling"
    );

    // --- an explicit override wins, including past the automatic cap ------
    let big = SEARCH_CACHE_MAX_MIB * 2;
    db::set_search_cache_override(Some(big));
    assert_eq!(
        opened_mib(&db_path),
        big,
        "an explicit ceiling must be applied verbatim, above the cap"
    );
    db::set_search_cache_override(None);

    // --- plain, automatic -------------------------------------------------
    // Same corpus, no key: an unencrypted index has no cliff and is held at a
    // flat ceiling by choice, for memory, however large the index is.
    let (_plain_dir, plain) = Scratch::db("searchcache-plain");
    let plain_path = plain.to_string_lossy().into_owned();
    db::set_process_key(None);
    seed_index(&plain, &spec());
    assert_eq!(
        opened_mib(&plain_path),
        SEARCH_CACHE_PLAIN_MIB,
        "an unencrypted index must not grow its cache with the corpus"
    );
}
