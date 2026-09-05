//! The search connection has to *open* at the ceiling it was sized for.
//!
//! `db::schema`'s unit tests cover the arithmetic; this covers the wiring,
//! which is where it can silently do nothing: the ceiling is resolved inside
//! `open_search_reader` from a `sqlite_stat1` read and a process-global, and a
//! break anywhere along that path leaves a connection quietly running on the
//! read-only profile's 4 MiB with every test still green.
//!
//! Its own integration binary, and one `#[test]`, for the reason
//! `tests/encrypted.rs` gives: it drives process-global state — the key and
//! the cache override — which unit tests must never do, because the lib test
//! binary runs them in parallel against the same globals.

use quicksearch_core::db;
use quicksearch_core::db::schema::{
    recommended_search_cache_mib, SEARCH_CACHE_MAX_MIB, SEARCH_CACHE_PLAIN_MIB,
};
use quicksearch_core::testutil::{measurement_key, scratch_db, seed_index, SeedSpec};

/// Enough rows that the recommendation clears the floor and is therefore
/// actually derived rather than clamped — at 168 B/file, 16 MiB is reached
/// around 100k. Seeded without content: the FTS write is the slow part and
/// this measures nothing about it.
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
    let path = scratch_db("searchcache");
    let db_path = path.to_string_lossy().into_owned();

    // --- keyed, automatic -------------------------------------------------
    db::set_process_key(Some(measurement_key()));
    db::set_search_cache_override(None);
    seed_index(&path, &spec());

    // Without stats there is nothing to derive from, so the floor is correct
    // and is what must come out.
    {
        let conn = db::open::open_search_reader(&db_path).unwrap();
        assert_eq!(
            cache_mib(&conn),
            recommended_search_cache_mib(0, true),
            "a never-analyzed index must fall back, not guess"
        );
    }

    // `PRAGMA optimize` is what a real indexing run leaves behind; from here
    // the row count is readable and the ceiling should track it.
    {
        let conn = db::open_existing(&db_path, true).unwrap();
        conn.execute_batch("ANALYZE;").unwrap();
    }
    let expected = recommended_search_cache_mib(FILES as i64, true);
    assert!(
        expected > recommended_search_cache_mib(0, true),
        "the corpus must be big enough to clear the floor, or this proves nothing"
    );
    {
        let conn = db::open::open_search_reader(&db_path).unwrap();
        assert_eq!(
            cache_mib(&conn),
            expected,
            "the derived ceiling did not reach the connection"
        );
    }

    // --- an explicit override wins, including past the automatic cap ------
    let big = SEARCH_CACHE_MAX_MIB * 2;
    db::set_search_cache_override(Some(big));
    {
        let conn = db::open::open_search_reader(&db_path).unwrap();
        assert_eq!(
            cache_mib(&conn),
            big,
            "an explicit ceiling must be applied verbatim, above the cap"
        );
    }
    db::set_search_cache_override(None);

    // --- plain, automatic -------------------------------------------------
    // Same corpus, no key: the sweep found no knee unencrypted, so this must
    // be flat regardless of how large the index is.
    let plain = scratch_db("searchcache-plain");
    db::set_process_key(None);
    seed_index(&plain, &spec());
    {
        let conn = db::open_existing(&plain.to_string_lossy(), true).unwrap();
        conn.execute_batch("ANALYZE;").unwrap();
    }
    {
        let conn = db::open::open_search_reader(&plain.to_string_lossy()).unwrap();
        assert_eq!(
            cache_mib(&conn),
            SEARCH_CACHE_PLAIN_MIB,
            "an unencrypted index must not grow its cache with the corpus"
        );
    }

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
    std::fs::remove_dir_all(plain.parent().unwrap()).ok();
}
