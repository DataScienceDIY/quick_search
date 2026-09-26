use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::db::open_or_recreate;
use crate::testutil::{zstd_of, Scratch};

/// A fresh index in its own scratch directory, and its path.
fn fresh() -> (Scratch, String, Connection) {
    let (dir, path) = Scratch::db("repo");
    let path = path.to_str().unwrap().to_string();
    let conn = open_or_recreate(&path, &Default::default()).unwrap();
    (dir, path, conn)
}

/// A `text/plain` row that wants content; tests vary it with struct update
/// syntax.
fn text_row<'a>(parent: &'a str, name: &'a str) -> NewFile<'a> {
    NewFile {
        name,
        parent,
        size: 1,
        mtime: 1,
        mime: Some("text/plain"),
        ftype: FileType::TEXT,
        hash: None,
        needs_content: true,
    }
}

/// A row no extractor claims: born `STATE_NA`.
fn na_row<'a>(parent: &'a str, name: &'a str) -> NewFile<'a> {
    NewFile {
        mime: None,
        ftype: FileType::EMPTY,
        needs_content: false,
        ..text_row(parent, name)
    }
}

fn insert_at(tx: &Transaction<'_>, path: &str, needs_content: bool) -> i64 {
    let (parent, name) = crate::file_handling::split_db_path(path).expect("a file's path");
    let row = NewFile {
        needs_content,
        ..text_row(parent, name)
    };
    insert_file(tx, &row).unwrap().expect("unique path")
}

/// A range spelled out, for rows seeded with `/` parents on every platform.
fn literal_range(lo: &str, hi: &str) -> RootRange {
    RootRange {
        lo: lo.into(),
        hi: hi.into(),
    }
}

/// Run `f` in one committed transaction.
fn with_tx<T>(conn: &mut Connection, f: impl FnOnce(&Transaction<'_>) -> T) -> T {
    let tx = conn.transaction().unwrap();
    let out = f(&tx);
    tx.commit().unwrap();
    out
}

/// [`update_file_basic`] in its own transaction.
fn update(conn: &mut Connection, row: &NewFile<'_>) -> Option<i64> {
    with_tx(conn, |tx| update_file_basic(tx, row).unwrap())
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn fts_rows(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM searchabletext")
}

/// Rows whose body the FTS index matches for `term`.
fn matches(conn: &Connection, term: &str) -> i64 {
    crate::testutil::fts_count(conn, term)
}

fn state(conn: &Connection, id: i64) -> i64 {
    conn.query_row("SELECT content_state FROM files WHERE id = ?1", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Store `text` on a pending row, which must still be pending.
fn done(tx: &Transaction<'_>, id: i64, text: &str) {
    assert!(store_pending_text(tx, id, text, zstd_of(text).as_deref()).unwrap());
}

/// Fail a row as a later run would: a changed file clears it back to pending
/// (the state `update_file_basic` leaves), then its extraction fails.
fn fail(tx: &Transaction<'_>, id: i64, reason: &str) {
    if state(tx, id) == STATE_DONE {
        clear_content_for_ids(tx, &[id]).unwrap();
    }
    set_content_state(tx, &[id], STATE_PENDING).unwrap();
    let failed = ContentOutcome::Failed(reason.to_string());
    assert!(complete_pending(tx, id, &failed, None).unwrap());
}

/// Every stored path, in `(parent, name)` order.
fn survivors(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT parent || name FROM files ORDER BY parent, name")
        .unwrap();
    let paths = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    paths
}

/// Seed fully-indexed rows, each with an FTS entry and stored text; returns
/// `path -> id`.
fn seeded(conn: &mut Connection, paths: &[&str]) -> std::collections::HashMap<String, i64> {
    with_tx(conn, |tx| {
        paths
            .iter()
            .map(|path| {
                let id = insert_at(tx, path, true);
                done(tx, id, "body text");
                (path.to_string(), id)
            })
            .collect()
    })
}

/// [`complete_pending`] clears nothing first, so its safety is that only a
/// pending row is written. A duplicate `searchabletext` row shows up as a file
/// appearing twice in results, never as an error.
#[test]
fn a_pending_write_leaves_exactly_one_fts_row() {
    let row = |mtime| NewFile {
        size: 10,
        mtime,
        ..text_row("/d/", "a.txt")
    };
    let (_dir, _path, mut conn) = fresh();

    let id = with_tx(&mut conn, |tx| {
        let id = insert_file(tx, &row(1)).unwrap().unwrap();
        done(tx, id, "sphinx quartz");

        // The update path: the sequence `process_batch_updates` performs.
        let same = update_file_basic(tx, &row(2)).unwrap().unwrap();
        assert_eq!(same, id, "the same row");
        done(tx, id, "sphinx onyx");
        id
    });

    assert_eq!(fts_rows(&conn), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 1);
    assert_eq!(matches(&conn, "onyx"), 1, "the new body is searchable");
    assert_eq!(matches(&conn, "quartz"), 0, "the old body is gone");

    // A second write to a row that is no longer pending (the content pass
    // holding a row the walk's inline text already stored) writes nothing.
    let stored = with_tx(&mut conn, |tx| {
        store_pending_text(tx, id, "sphinx jasper", zstd_of("sphinx jasper").as_deref()).unwrap()
    });
    assert!(!stored);
    assert_eq!(fts_rows(&conn), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 1);
    assert_eq!(matches(&conn, "jasper"), 0);
    assert_eq!(matches(&conn, "onyx"), 1);
}

/// Stale cleanup can delete a pending row while its extraction is in flight.
/// Writing the outcome anyway would leave a posting with no `files` row,
/// matching searches forever and inherited by whichever file reuses the id.
#[test]
fn an_outcome_for_a_row_deleted_in_flight_writes_nothing() {
    let (_dir, _path, mut conn) = fresh();
    let (text_id, failed_id) = with_tx(&mut conn, |tx| {
        let a = insert_at(tx, "/gone/a.txt", true);
        let b = insert_at(tx, "/gone/b.txt", true);
        delete_ids(tx, &[a, b], &[]).unwrap();
        (a, b)
    });
    with_tx(&mut conn, |tx| {
        let text = ContentOutcome::Done {
            text: "sphinx quartz".into(),
        };
        assert!(
            !complete_pending(tx, text_id, &text, zstd_of("sphinx quartz").as_deref()).unwrap()
        );
        let failed = ContentOutcome::Failed("boom".into());
        assert!(!complete_pending(tx, failed_id, &failed, None).unwrap());
    });
    assert_eq!(fts_rows(&conn), 0, "no orphan posting");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 0);
}

/// The size report reads each body's length out of its zstd frame header,
/// which works only because [`DocEncoder`] is handed the whole input up
/// front. A streaming encoder would zero that figure.
#[test]
fn a_compressed_body_carries_its_uncompressed_length() {
    let mut enc = DocEncoder::new().unwrap();
    let long = "lorem ipsum dolor sit amet ".repeat(4096);
    for text in ["", "hello world", &long] {
        let blob = enc.encode(text).unwrap();
        assert_eq!(
            raw_text_len(&blob),
            Some(text.len() as u64),
            "frame header lost the content size for a {}-byte body",
            text.len()
        );
        // The size report projects only a prefix, never the whole body.
        let prefix = &blob[..blob.len().min(18)];
        assert_eq!(
            raw_text_len(prefix),
            Some(text.len() as u64),
            "the first 18 bytes must be enough to read the length"
        );
    }
}

/// Several rows into one arena, as the writer does. `zstd`'s `WriteBuf` for
/// `Vec` writes from offset zero and sets the length, so compressing straight
/// into a shared arena would overwrite the previous row. Each body must come
/// back byte-identical out of its own range.
#[test]
fn an_arena_keeps_every_row_it_is_given() {
    let mut enc = DocEncoder::new().unwrap();
    let bodies = [
        "the first document",
        "",
        "a considerably longer second document ".repeat(512).as_str(),
        "third",
    ]
    .map(str::to_string);

    let mut arena = Vec::new();
    let mut ranges = Vec::new();
    for text in &bodies {
        ranges.push(enc.encode_into(text, &mut arena).unwrap());
    }

    for (text, at) in bodies.iter().zip(&ranges) {
        let blob = &arena[at.clone()];
        assert_eq!(
            raw_text_len(blob),
            Some(text.len() as u64),
            "a row's frame does not describe its own body"
        );
        assert_eq!(
            DocDecoder::new().unwrap().decode(blob),
            Some(text.as_str()),
            "a row did not survive sharing the arena"
        );
    }

    // The ranges tile the arena in order: a gap or an overlap means one row
    // landed on another.
    let mut next = 0;
    for at in &ranges {
        assert_eq!(at.start, next, "rows must be contiguous");
        next = at.end;
    }
    assert_eq!(next, arena.len(), "the arena holds exactly the four bodies");

    // Reuse: a second batch must not read the first one's bytes.
    arena.clear();
    let at = enc.encode_into("a fresh batch", &mut arena).unwrap();
    assert_eq!(at.start, 0);
    assert_eq!(
        DocDecoder::new().unwrap().decode(&arena[at]),
        Some("a fresh batch")
    );
}

#[test]
fn insert_update_delete_round_trip() {
    let (_dir, _path, mut conn) = fresh();
    with_tx(&mut conn, |tx| {
        let row = NewFile {
            size: 42,
            mtime: 1_700_000_000,
            hash: Some(&[1, 2, 3]),
            ..text_row("/tmp/", "a.txt")
        };
        let id = insert_file(tx, &row).unwrap().expect("unique path");
        done(tx, id, "hello world");
    });

    assert_eq!(matches(&conn, "hello"), 1);

    with_tx(&mut conn, |tx| {
        assert!(delete_file_by_path(tx, "/tmp/a.txt").unwrap())
    });
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM files"), 0);
    assert_eq!(fts_rows(&conn), 0);
}

#[test]
fn insert_writes_content_state_from_needs_content() {
    let (_dir, _path, mut conn) = fresh();
    let tx = conn.transaction().unwrap();
    let claimed = insert_file(&tx, &text_row("/tmp/", "claimed.txt"))
        .unwrap()
        .expect("unique path");
    let unclaimed = NewFile {
        mime: Some("video/mp4"),
        needs_content: false,
        ..text_row("/tmp/", "unclaimed.mp4")
    };
    let unclaimed = insert_file(&tx, &unclaimed).unwrap().expect("unique path");

    assert_eq!(state(&tx, claimed), STATE_PENDING);
    assert_eq!(state(&tx, unclaimed), STATE_NA);
}

#[test]
fn update_writes_content_state_from_needs_content() {
    let (_dir, _path, mut conn) = fresh();
    let row = NewFile {
        size: 10,
        ..text_row("/tmp/", "a.txt")
    };
    let id = with_tx(&mut conn, |tx| {
        let id = insert_file(tx, &row).unwrap().expect("unique path");
        done(tx, id, "old text");
        id
    });

    // Rewritten as something an extractor claims: back to pending, and the
    // stale FTS row goes too.
    let claimed = NewFile {
        size: 20,
        mtime: 2,
        mime: Some("text/plain"),
        ftype: FileType::TEXT,
        needs_content: true,
        ..row
    };
    assert_eq!(update(&mut conn, &claimed), Some(id));
    assert_eq!(state(&conn, id), STATE_PENDING);
    assert_eq!(matches(&conn, "old"), 0);

    // Rewritten as something nothing claims: NA, not pending, or the row
    // re-enters the content pass on every run.
    let unclaimed = NewFile {
        mtime: 3,
        mime: Some("video/mp4"),
        needs_content: false,
        ..claimed
    };
    update(&mut conn, &unclaimed).expect("row still there");
    assert_eq!(state(&conn, id), STATE_NA);
}

#[test]
fn insert_file_twice_on_same_path_is_idempotent() {
    // Overlapping roots or symlink resolution revisit a canonical path.
    let (_dir, _path, mut conn) = fresh();
    let tx = conn.transaction().unwrap();
    let row = text_row("/tmp/", "dup.txt");
    let id1 = insert_file(&tx, &row).unwrap().expect("first insert");
    let id2 = insert_file(&tx, &row).unwrap();
    assert!(id2.is_none(), "second insert of same path must return None");
    assert_eq!(count(&tx, "SELECT COUNT(*) FROM files"), 1);
    let id_read: i64 = tx
        .query_row(
            "SELECT id FROM files WHERE parent = ?1 AND name = ?2",
            params!["/tmp/", "dup.txt"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(id_read, id1);
    tx.commit().unwrap();
}

#[test]
fn delete_subtree_clears_every_dependent_table() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(
        &mut conn,
        &[
            "/tree/a.txt",
            "/tree/deep/b.txt",
            "/tree/deep/c.txt",
            // Outside the range: a prefix sibling, and a LIKE-metacharacter
            // neighbour that a `LIKE 'tree_%'` sweep would have swallowed.
            "/tree2/keep.txt",
            "/treeX/keep.txt",
        ],
    );
    with_tx(&mut conn, |tx| {
        fail(tx, ids["/tree/deep/c.txt"], "bad parse")
    });

    let range = RootRange::for_root("/tree");
    let removed = with_tx(&mut conn, |tx| delete_subtree(tx, &range).unwrap());
    assert_eq!(removed, 3, "three files under /tree");

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM files"),
        2,
        "siblings survive"
    );
    assert_eq!(fts_rows(&conn), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM failed_files"),
        0,
        "the failed row went with its file"
    );
    assert_eq!(survivors(&conn), vec!["/tree2/keep.txt", "/treeX/keep.txt"]);
}

/// The out-of-root sweep must take a path that merely *starts* with a root's
/// name: a different folder.
#[test]
fn delete_outside_ranges_keeps_exactly_the_configured_roots() {
    let (_dir, _path, mut conn) = fresh();
    seeded(
        &mut conn,
        &[
            "/roots/one/a.txt",
            "/roots/one/deep/b.txt",
            "/roots/two/c.txt",
            "/roots/onefold/d.txt",
            "/elsewhere/target.txt",
        ],
    );

    let ranges: Vec<(String, String)> = ["/roots/one", "/roots/two"]
        .iter()
        .map(|r| {
            let range = RootRange::for_root(r);
            (range.lo, range.hi)
        })
        .collect();
    let removed = with_tx(&mut conn, |tx| delete_outside_ranges(tx, &ranges).unwrap());
    assert_eq!(removed, 2, "the name-prefix sibling and the outsider");

    assert_eq!(
        survivors(&conn),
        vec![
            "/roots/one/a.txt",
            "/roots/one/deep/b.txt",
            "/roots/two/c.txt"
        ]
    );
    assert_eq!(fts_rows(&conn), 3);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 3);

    // No roots configured is a half-written config, not an instruction to
    // delete the entire index.
    assert_eq!(
        with_tx(&mut conn, |tx| delete_outside_ranges(tx, &[]).unwrap()),
        0
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM files"), 3);
}

/// A survivor in any dependent table keeps the file findable.
#[test]
fn delete_ids_clears_every_dependent_table() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(
        &mut conn,
        &["/t/a.txt", "/t/b.log", "/t/deep/c.log", "/t/keep.txt"],
    );
    with_tx(&mut conn, |tx| fail(tx, ids["/t/deep/c.log"], "bad parse"));

    let doomed = vec![ids["/t/b.log"], ids["/t/deep/c.log"]];
    // `c.log` is FAILED and so holds no posting: the distinction the second
    // argument draws.
    let with_postings = vec![ids["/t/b.log"]];
    let removed = with_tx(&mut conn, |tx| {
        delete_ids(tx, &doomed, &with_postings).unwrap()
    });
    assert_eq!(removed, 2);

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM files"), 2);
    assert_eq!(fts_rows(&conn), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 0);

    // A contentless table keeps serving deleted rowids without the tombstone.
    assert_eq!(matches(&conn, "body"), 2);

    // Empty input is a no-op, not a statement with an empty `IN ()`.
    assert_eq!(
        with_tx(&mut conn, |tx| delete_ids(tx, &[], &[]).unwrap()),
        0
    );
}

#[test]
fn delete_ids_spans_chunk_boundaries() {
    let (_dir, _path, mut conn) = fresh();
    let paths: Vec<String> = (0..DELETE_IDS_CHUNK + 7)
        .map(|i| format!("/t/f{:05}.txt", i))
        .collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let ids = seeded(&mut conn, &refs);

    let mut all: Vec<i64> = ids.values().copied().collect();
    all.sort_unstable();
    let keep = all.pop().unwrap();
    // `seeded` leaves every row DONE, so both lists span the boundary.
    let removed = with_tx(&mut conn, |tx| delete_ids(tx, &all, &all).unwrap());
    assert_eq!(removed, DELETE_IDS_CHUNK + 6);
    let left: i64 = conn
        .query_row("SELECT id FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, keep);
}

/// `update_file_basic` clears content only for the states that hold it, one
/// row per transition. Under-clearing is the dangerous direction: a DONE row
/// keeping its posting surfaces a hit whose text no longer matches.
#[test]
fn updating_clears_content_exactly_for_the_states_that_hold_it() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(&mut conn, &["/t/done.txt", "/t/failed.txt", "/t/keep.txt"]);
    let (na_id, pending_id) = with_tx(&mut conn, |tx| {
        // Clears failed.txt's posting too; the narrowing rests on that.
        fail(tx, ids["/t/failed.txt"], "bad parse");
        let na = insert_file(tx, &na_row("/t/", "na.bin"))
            .unwrap()
            .expect("unique path");
        (na, insert_at(tx, "/t/pending.txt", true))
    });
    let changed = |name: &'static str, needs_content: bool| NewFile {
        size: 9,
        mtime: 9,
        needs_content,
        ..text_row("/t/", name)
    };

    // DONE → PENDING: the posting and the body go, in the same transaction.
    assert_eq!(
        update(&mut conn, &changed("done.txt", true)),
        Some(ids["/t/done.txt"])
    );
    assert_eq!(state(&conn, ids["/t/done.txt"]), STATE_PENDING);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM searchabletext WHERE rowid IN \
             (SELECT id FROM files WHERE name = 'done.txt')"
        ),
        0
    );

    // FAILED → PENDING: the failure record goes with the transition.
    assert_eq!(
        update(&mut conn, &changed("failed.txt", true)),
        Some(ids["/t/failed.txt"])
    );
    assert_eq!(state(&conn, ids["/t/failed.txt"]), STATE_PENDING);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 0);

    // NA → NA: nothing to clear, and the metadata still lands.
    assert_eq!(update(&mut conn, &changed("na.bin", false)), Some(na_id));
    assert_eq!(state(&conn, na_id), STATE_NA);
    let mtime: i64 = conn
        .query_row("SELECT mtime FROM files WHERE id = ?1", [na_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(mtime, 9);

    // PENDING → NA: the state transition alone.
    assert_eq!(
        update(&mut conn, &changed("pending.txt", false)),
        Some(pending_id)
    );
    assert_eq!(state(&conn, pending_id), STATE_NA);

    // keep.txt, never updated, keeps its posting, and it is the only one.
    assert_eq!(fts_rows(&conn), 1);
    assert_eq!(state(&conn, ids["/t/keep.txt"]), STATE_DONE);

    // No row: `None`, not an invented insert.
    assert_eq!(update(&mut conn, &changed("vanished.txt", true)), None);
}

/// The resolver behind stale cleanup's `with_postings` decision returns the
/// stored state, and `None` rather than an error for paths it cannot resolve:
/// another writer may already have removed them.
#[test]
fn id_and_state_for_path_reads_the_stored_state() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(&mut conn, &["/t/done.txt"]);
    let na_id = with_tx(&mut conn, |tx| {
        insert_file(tx, &na_row("/t/", "na.bin"))
            .unwrap()
            .expect("unique path")
    });

    assert_eq!(
        id_and_state_for_path(&conn, "/t/done.txt").unwrap(),
        Some((ids["/t/done.txt"], STATE_DONE))
    );
    assert_eq!(
        id_and_state_for_path(&conn, "/t/na.bin").unwrap(),
        Some((na_id, STATE_NA))
    );
    assert_eq!(id_and_state_for_path(&conn, "/t/vanished.txt").unwrap(), None);
    assert_eq!(
        id_and_state_for_path(&conn, "no-separator").unwrap(),
        None,
        "an unsplittable path resolves to nothing rather than erroring"
    );
}

/// The reconciliation scan must serve every row in the range exactly once,
/// in order, and stop at the range bound rather than at a name prefix.
#[test]
fn rows_in_range_page_walks_the_range_once() {
    let (_dir, _path, mut conn) = fresh();
    seeded(
        &mut conn,
        &[
            "/t/a.txt",
            "/t/deep/b.txt",
            "/t/deep/deeper/c.txt",
            "/t2/outside.txt",
            "/tX/outside.txt",
        ],
    );

    let range = RootRange::for_root("/t");
    let mut seen = Vec::new();
    let mut after = (range.lo.clone(), String::new());
    loop {
        let page = rows_in_range_page(&conn, &after.0, &after.1, &range.hi, 2).unwrap();
        let Some(last) = page.last() else { break };
        after = (last.parent.clone(), last.name.clone());
        seen.extend(page.into_iter().map(|r| r.path));
    }
    assert_eq!(
        seen,
        vec!["/t/a.txt", "/t/deep/b.txt", "/t/deep/deeper/c.txt"],
        "in (parent, name) order, once each, and the prefix siblings are outside"
    );
}

/// `dir_rows` must seek on `idx_files_parent`, never scan the table, so its
/// cost is per directory rather than per tree. It is not index-only; see
/// `idx_files_parent` in `schema.rs`.
#[test]
fn dir_rows_seeks_the_parent_index() {
    let (_dir, _path, conn) = fresh();
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT name, mtime FROM files WHERE parent = ?1",
            params!["/some/dir/"],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("SEARCH") && plan.contains("idx_files_parent"),
        "dir_rows must seek the parent index, got: {}",
        plan
    );
}

/// The range form is an index seek; `LIKE … ESCAPE` cannot be (SQLite
/// disables the LIKE optimisation whenever ESCAPE is present).
#[test]
fn the_subtree_range_is_an_index_seek_not_a_scan() {
    let (_dir, _path, conn) = fresh();
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN DELETE FROM files WHERE parent >= ?1 AND parent < ?2",
            params!["/tree/", "/tree0"],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("SEARCH") && !plan.contains("SCAN"),
        "range delete must seek, got: {}",
        plan
    );
}

/// The keyset page must be one index walk, with no temp b-tree for the
/// `ORDER BY`.
#[test]
fn the_reconcile_page_seeks_and_does_not_sort() {
    let (_dir, _path, conn) = fresh();
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT id, parent, name, size, mime, content_state FROM files
              WHERE (parent, name) > (?1, ?2) AND parent < ?3
              ORDER BY parent, name
              LIMIT ?4",
        )
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map(params!["/tree/", "", "/tree0", 2], |r| r.get(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let plan = plan.join(" | ");
    assert!(
        plan.contains("SEARCH") && plan.contains("idx_files_parent"),
        "keyset page must seek the parent index, got: {}",
        plan
    );
    assert!(
        !plan.contains("TEMP B-TREE"),
        "the index order must satisfy the ORDER BY outright, got: {}",
        plan
    );
}

#[test]
fn last_full_index_round_trip() {
    let (_dir, _path, conn) = fresh();
    assert_eq!(get_last_full_index(&conn), None, "fresh DB has no marker");
    set_last_full_index(&conn, 1_700_000_123).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_123));
    // Overwrite, not accumulate.
    set_last_full_index(&conn, 1_700_000_999).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_999));
}

#[test]
fn checkpoint_and_close_truncates_wal() {
    let (_dir, p, mut conn) = fresh();
    with_tx(&mut conn, |tx| {
        insert_file(tx, &na_row("/tmp/", "w.txt")).unwrap()
    });
    checkpoint_and_close(conn);
    assert_eq!(wal_bytes(&p), 0, "WAL should be truncated on clean close");
    let conn = crate::db::open_existing(&p, false).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM files"), 1);
}

fn seed_rows(conn: &mut Connection, range: std::ops::Range<usize>) {
    let body = "lorem ipsum dolor sit amet ".repeat(64);
    with_tx(conn, |tx| {
        for i in range {
            let name = format!("{}.txt", i);
            let id = insert_file(tx, &text_row("/tmp/bulk/", &name))
                .unwrap()
                .unwrap();
            done(tx, id, &body);
        }
    });
}

#[test]
fn checkpoint_truncate_empties_the_log() {
    let (_dir, p, mut conn) = fresh();
    seed_rows(&mut conn, 0..200);
    assert!(wal_bytes(&p) > 0, "the writes should be sitting in the log");

    checkpoint_truncate(&conn).expect("nothing is holding the log back");
    assert_eq!(wal_bytes(&p), 0, "a completed TRUNCATE leaves no log");
}

/// A reader pins the log, SQLite declines to reset it, and only the result
/// row says so.
#[test]
fn checkpoint_truncate_reports_an_incomplete_checkpoint() {
    let (_dir, p, mut writer) = fresh();
    seed_rows(&mut writer, 0..200);
    writer
        .busy_timeout(std::time::Duration::from_millis(100))
        .unwrap();

    let reader = crate::db::open_existing(&p, false).unwrap();
    let mut stmt = reader.prepare("SELECT id FROM files").unwrap();
    let mut rows = stmt.query([]).unwrap();
    rows.next().unwrap().expect("a row to hold the snapshot on");

    let err = checkpoint_truncate(&writer).expect_err("a reader holds the log open");
    assert!(err.contains("incomplete"), "unexpected message: {}", err);
    assert!(wal_bytes(&p) > 0, "and the log is still there");
}

/// `journal_size_limit` gives the space back after a TRUNCATE that could not
/// take the reset lock. Without it the log stays at its high-water mark.
///
/// The limit is not a checkpoint: `sqlite3WalFrames` applies it at the first
/// commit after the log restarts, which is the next write once a checkpoint
/// has copied every frame out. So the space comes back from whichever writer
/// touches the index next, with no successful TRUNCATE. `db::schema`'s
/// `every_writing_profile_bounds_the_log` keeps the profiles at
/// [`crate::config::MINIMUM_WAL_SIZE`]; this shows the limit works.
#[test]
fn journal_size_limit_gives_the_space_back_after_a_blocked_truncate() {
    // With positions: the test needs more than 16 MiB of log, and position
    // lists write it from the fewest rows.
    let (_dir, p) = Scratch::db("repo");
    let p = p.to_str().unwrap().to_string();
    let positional = crate::config::ProcessingConfig {
        compact_text_index: false,
        ..Default::default()
    };
    let mut writer = open_or_recreate(&p, &positional).unwrap();
    seed_rows(&mut writer, 0..10);
    writer
        .busy_timeout(std::time::Duration::from_millis(100))
        .unwrap();

    let peak = {
        // Held across the seeding, so the log grows past the limit: pinned to
        // an early frame, no checkpoint can reset it. A run has this shape,
        // with a reader per root live from start to finish.
        let reader = crate::db::open_existing(&p, false).unwrap();
        let mut stmt = reader.prepare("SELECT id FROM files").unwrap();
        let mut rows = stmt.query([]).unwrap();
        rows.next().unwrap().expect("a row to hold the snapshot on");

        // 6k rows measured 12.5 MiB of log; this clears 16 MiB with room.
        seed_rows(&mut writer, 10..10_000);
        let peak = wal_bytes(&p);
        assert!(
            peak > crate::config::MINIMUM_WAL_SIZE,
            "the fixture left {} bytes of log, under the limit it must exceed",
            peak
        );

        checkpoint_truncate(&writer).expect_err("a reader holds the log open");
        assert_eq!(wal_bytes(&p), peak, "a blocked TRUNCATE trims nothing");
        peak
    };

    // The reader is gone, so a PASSIVE checkpoint copies every frame out, but
    // trims nothing: the log has not restarted yet.
    writer
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .unwrap();
    assert_eq!(
        wal_bytes(&p),
        peak,
        "a checkpoint that does not restart the log cannot trim it"
    );

    // The next write restarts it, and that commit honours the limit. No
    // TRUNCATE was ever accepted.
    seed_rows(&mut writer, 10_000..10_001);
    let after = wal_bytes(&p);
    assert!(
        after <= crate::config::MINIMUM_WAL_SIZE,
        "the log went {} -> {} bytes against a {} byte limit",
        peak,
        after,
        crate::config::MINIMUM_WAL_SIZE
    );
}

/// Autocheckpoint tries the reset lock exactly once, with no retry, so a
/// reader querying back to back keeps the log growing for the whole run. An
/// explicit checkpoint retries the same lock under `busy_timeout` and gets it.
#[test]
fn a_busy_reader_defeats_the_autocheckpoint_but_not_a_forced_one() {
    // Bare rows: no zstd or tokenising, so the test stays fast.
    fn seed_bare(conn: &mut Connection, range: std::ops::Range<usize>) {
        with_tx(conn, |tx| {
            for i in range {
                let name = format!("{}.txt", i);
                let row = NewFile {
                    size: i as u64,
                    mime: None,
                    hash: Some(&[0u8; 32]),
                    needs_content: false,
                    ..text_row("/tmp/bare/", &name)
                };
                insert_file(tx, &row).unwrap();
            }
        });
    }

    fn run(p: &str, force_every: usize) -> u64 {
        let mut conn = open_or_recreate(p, &Default::default()).unwrap();
        seed_bare(&mut conn, 0..500);

        // Stands in for a walk prefetcher: short reads, no gaps.
        let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, stop) = (p.to_string(), stop.clone());
            std::thread::spawn(move || {
                let conn = crate::db::open_existing(&path, false).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    let _: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM files WHERE parent = '/tmp/bare'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                }
            })
        };

        let mut peak = 0u64;
        for batch in 0..60 {
            let lo = 1000 + batch * 300;
            seed_bare(&mut conn, lo..lo + 300);
            if force_every > 0 && batch % force_every == force_every - 1 {
                let _ = checkpoint_truncate(&conn);
            }
            peak = peak.max(wal_bytes(p));
        }

        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        drop(conn);
        peak
    }

    let (_dir_a, unbounded) = Scratch::db("repo");
    let left_alone = run(unbounded.to_str().unwrap(), 0);

    let (_dir_b, bounded) = Scratch::db("repo");
    let forced = run(bounded.to_str().unwrap(), 4);

    eprintln!(
        "peak WAL: autocheckpoint only {}, forced {}",
        left_alone, forced
    );
    assert!(
        forced * 2 < left_alone,
        "forcing checkpoints did not bound the log: {} vs {}",
        forced,
        left_alone
    );
}

/// Sizing the search cache reads the row count from `sqlite_stat1` rather than
/// counting, so it has to survive the two states that table is really in.
#[test]
fn the_analyzed_file_count_ignores_the_partial_index_and_missing_stats() {
    let (_dir, _path, mut conn) = fresh();
    assert_eq!(
        analyzed_file_count(&conn),
        None,
        "a never-analyzed index must say so, not report zero files"
    );

    seed_rows(&mut conn, 0..2000);
    // All 2000 rows land content_state = 0, so `idx_files_content_pending`
    // covers every one; mark most done so the partial index is smaller than
    // the table, which is the trap being tested.
    conn.execute("UPDATE files SET content_state = 1 WHERE id % 100 != 0", [])
        .unwrap();
    conn.execute_batch("ANALYZE;").unwrap();

    assert!(
        count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 0") < 2000,
        "the partial index must be smaller than the table for this to test anything"
    );
    assert_eq!(
        analyzed_file_count(&conn),
        Some(2000),
        "the partial index's smaller count must not win"
    );
}

#[test]
fn maintain_vacuums_when_slack_is_significant() {
    let (_dir, p, mut conn) = fresh();
    seed_rows(&mut conn, 0..2000);
    checkpoint_truncate(&conn).unwrap();
    let before = std::fs::metadata(&p).unwrap().len();

    with_tx(&mut conn, |tx| {
        for i in 0..1900 {
            delete_file_by_path(tx, &format!("/tmp/bulk/{}.txt", i)).unwrap();
        }
    });
    drop(conn);

    let conn = crate::db::open::open_maintenance(&p).unwrap();
    assert!(
        count(&conn, "PRAGMA freelist_count") > 0,
        "the deletions should have freed pages"
    );

    assert!(
        maintain(&conn, &p).unwrap(),
        "that much slack is worth a vacuum"
    );
    assert_eq!(
        wal_bytes(&p),
        0,
        "the vacuum's own writes are checkpointed too"
    );
    assert!(
        std::fs::metadata(&p).unwrap().len() < before,
        "the file should have shrunk"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM files"), 100);
    assert_eq!(
        matches(&conn, "lorem"),
        100,
        "the FTS index survived the rewrite"
    );
}

#[test]
fn maintain_skips_vacuum_on_a_tight_file() {
    let (_dir, p, mut conn) = fresh();
    seed_rows(&mut conn, 0..200);
    drop(conn);

    let conn = crate::db::open::open_maintenance(&p).unwrap();
    assert!(
        !maintain(&conn, &p).unwrap(),
        "a file with no slack is not worth rewriting"
    );
    // The checkpoint is not conditional on the vacuum, though.
    assert_eq!(wal_bytes(&p), 0);
}

#[test]
fn a_failed_extraction_records_its_reason() {
    let (_dir, _path, mut conn) = fresh();
    let id = with_tx(&mut conn, |tx| {
        let id = insert_at(tx, "/tmp/oops.txt", true);
        fail(tx, id, "bad parse");
        id
    });

    let reason: String = conn
        .query_row(
            "SELECT reason FROM failed_files WHERE file_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "bad parse");
    assert_eq!(state(&conn, id), STATE_FAILED);
}

/// Every way a changed file leaves [`STATE_DONE`] takes the posting and the
/// stored body with it, so its invariant holds however a row got where it is.
///
/// `count_root_counts_the_fts_rows_it_says_it_does` checks rows that were
/// never DONE, which passes even for a transition that leaves a stale posting
/// behind; this checks the rows that were. [`delete_ids`] narrows its
/// tombstones on it: a row that kept a posting past DONE would keep it past
/// deletion and answer searches for a file that is gone.
#[test]
fn leaving_done_always_takes_the_posting_with_it() {
    let (_dir, _path, mut conn) = fresh();

    // Each row starts DONE, with a posting and a stored body, and is then
    // rewritten in place: still text (back to pending), or no longer
    // anything an extractor claims (NA).
    for (name, needs_content) in [("rewritten.txt", true), ("now-binary.txt", false)] {
        with_tx(&mut conn, |tx| {
            let id = insert_at(tx, &format!("/t/{}", name), true);
            done(tx, id, "body text");
            let row = NewFile {
                size: 99,
                mtime: 99,
                needs_content,
                ..text_row("/t/", name)
            };
            update_file_basic(tx, &row)
                .unwrap()
                .expect("the row is there");
        });
    }

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 1"),
        0,
        "every row left DONE"
    );
    assert_eq!(fts_rows(&conn), 0, "and none of them kept its posting");
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM documents_text"),
        0,
        "nor its stored body, which would outlive the file it was read from"
    );
}

/// The premise of `count_root`: `content_state = DONE` exactly when a
/// `searchabletext` row exists, pinned against the FTS table itself.
#[test]
fn count_root_counts_the_fts_rows_it_says_it_does() {
    let (_dir, _path, mut conn) = fresh();
    with_tx(&mut conn, |tx| {
        // Two searchable, and one of each way a row can fail to be.
        for path in ["/tree/a.txt", "/tree/b.txt"] {
            let id = insert_at(tx, path, true);
            done(tx, id, "body text");
        }
        let failed = insert_at(tx, "/tree/c.bin", true);
        fail(tx, failed, "bad parse");
        let na = insert_at(tx, "/tree/d.iso", true);
        assert!(complete_pending(tx, na, &ContentOutcome::NotApplicable, None).unwrap());
        insert_at(tx, "/tree/e.txt", true); // still pending
    });

    let counts = count_root(&conn, &literal_range("/tree/", "/tree0")).unwrap();
    assert_eq!(counts.files, 5, "every row under the root");
    assert_eq!(
        counts.fts,
        fts_rows(&conn),
        "the root holds everything, so its FTS figure is the whole table"
    );
    assert_eq!(counts.fts, 2);

    // A searchable row outside the range moves the table's total and not the
    // root's figure.
    with_tx(&mut conn, |tx| {
        let id = insert_at(tx, "/elsewhere/f.txt", true);
        done(tx, id, "body text");
    });
    assert_eq!(fts_rows(&conn), 3);
    assert_eq!(
        count_root(&conn, &literal_range("/tree/", "/tree0")).unwrap(),
        counts,
        "a row outside the range belongs to no root's figures"
    );
}

/// An empty range is 0/0, not an error, though `SUM` over no rows is NULL.
#[test]
fn count_root_reports_zero_for_an_empty_range() {
    let (_dir, _path, conn) = fresh();
    assert_eq!(
        count_root(&conn, &literal_range("/nothing/", "/nothing0")).unwrap(),
        RootCounts { files: 0, fts: 0 }
    );
}

#[test]
fn root_counts_round_trip() {
    let (_dir, _path, conn) = fresh();
    assert_eq!(get_root_counts(&conn, "/tree"), None, "never counted");

    set_root_counts(&conn, "/tree", RootCounts { files: 12, fts: 5 }).unwrap();
    assert_eq!(
        get_root_counts(&conn, "/tree"),
        Some(RootCounts { files: 12, fts: 5 })
    );
    // Overwrite, not accumulate.
    set_root_counts(&conn, "/tree", RootCounts { files: 20, fts: 9 }).unwrap();
    assert_eq!(
        get_root_counts(&conn, "/tree"),
        Some(RootCounts { files: 20, fts: 9 })
    );
    // Roots do not read each other's figures.
    assert_eq!(get_root_counts(&conn, "/other"), None);

    // A value this build cannot parse reads as absent, like a missing one.
    for bad in ["", "12", "12,", "a,b", "12,5,3"] {
        conn.execute(
            "INSERT OR REPLACE INTO schema_info(key, value) VALUES ('counts:/tree', ?1)",
            params![bad],
        )
        .unwrap();
        assert_eq!(get_root_counts(&conn, "/tree"), None, "parsed {:?}", bad);
    }
}

#[test]
fn prune_root_stats_drops_every_figure_of_a_dropped_root() {
    let (_dir, _path, conn) = fresh();
    for root in ["/kept", "/dropped"] {
        set_root_walk_count(&conn, root, 100).unwrap();
        set_root_counts(&conn, root, RootCounts { files: 90, fts: 40 }).unwrap();
    }
    set_last_full_index(&conn, 1_700_000_000).unwrap();

    prune_root_stats(&conn, &["/kept".to_string()]).unwrap();

    assert_eq!(get_root_walk_count(&conn, "/kept"), Some(100));
    assert_eq!(
        get_root_counts(&conn, "/kept"),
        Some(RootCounts { files: 90, fts: 40 })
    );
    assert_eq!(get_root_walk_count(&conn, "/dropped"), None);
    assert_eq!(get_root_counts(&conn, "/dropped"), None);
    // The sweep reads every `schema_info` key; unrelated ones must survive it.
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_000));
}

/// The parent-scan range must include the root's own directory, or files
/// directly in a root are never reconciled. It does because every stored
/// parent ends in a separator, making the root's parent exactly `lo`.
#[test]
fn the_parent_scan_reaches_the_roots_own_directory() {
    let (_dir, _path, mut conn) = fresh();
    seeded(
        &mut conn,
        &[
            "/tree/top.txt",           // directly in the root
            "/tree/deep/b.txt",        // a subdirectory
            "/tree/deep/deeper/c.txt", // deeper still
            "/tree2/outside.txt",      // prefix sibling: outside
            "/treeX/outside.txt",      // and the LIKE-metacharacter neighbour
        ],
    );

    let mut seen = Vec::new();
    for_each_parent_in_range(&conn, &RootRange::for_root("/tree"), |parent| {
        seen.push(parent)
    })
    .unwrap();
    seen.sort();
    assert_eq!(
        seen,
        vec!["/tree/", "/tree/deep/", "/tree/deep/deeper/"],
        "the root's own directory is in range, and the siblings are not"
    );
}

/// Pins the two facts the delete helpers rest on: the cascade fires on a
/// production connection, and `searchabletext` (FTS5, no foreign key) does not
/// cascade, so its delete stays explicit.
#[test]
fn deleting_a_file_row_cascades_the_fk_tables() {
    let (_dir, _path, mut conn) = fresh();
    seeded(&mut conn, &["/casc/a.txt", "/casc/b.txt"]);
    // The failure goes on a row that never extracted, the only way a run
    // reaches a failure, so the FTS figures below stay about the cascade.
    with_tx(&mut conn, |tx| {
        let never = insert_at(tx, "/casc/c.bin", true);
        fail(tx, never, "boom");
    });
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 1);
    assert_eq!(fts_rows(&conn), 2);

    conn.execute("DELETE FROM files", []).unwrap();

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM documents_text"),
        0,
        "documents_text must cascade"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM failed_files"),
        0,
        "failed_files must cascade"
    );
    assert_eq!(
        fts_rows(&conn),
        2,
        "FTS5 cannot cascade, so the helpers must delete from it explicitly"
    );
}

#[test]
fn retry_failed_files_resets_state_and_clears_records() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(&mut conn, &["/retry/bad.txt", "/retry/good.txt"]);
    with_tx(&mut conn, |tx| {
        fail(tx, ids["/retry/bad.txt"], "parser choked")
    });

    assert_eq!(with_tx(&mut conn, |tx| retry_failed_files(tx).unwrap()), 1);

    assert_eq!(state(&conn, ids["/retry/bad.txt"]), STATE_PENDING);
    assert_eq!(
        state(&conn, ids["/retry/good.txt"]),
        STATE_DONE,
        "done rows untouched"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 0);
}

/// `update_file_basic` must clear a stale failure record on both content
/// paths, or `list-failed` keeps reporting a fixed file as broken.
#[test]
fn a_changed_file_clears_its_failure_record() {
    let (_dir, _path, mut conn) = fresh();
    let ids = seeded(&mut conn, &["/chg/a.txt", "/chg/b.txt"]);
    with_tx(&mut conn, |tx| {
        fail(tx, ids["/chg/a.txt"], "boom");
        fail(tx, ids["/chg/b.txt"], "boom");
    });

    let changed = |name: &'static str, needs_content: bool| NewFile {
        size: 2,
        mtime: 9,
        mime: needs_content.then_some("text/plain"),
        needs_content,
        ..text_row("/chg/", name)
    };
    // The bug this pins: needs_content = false lands in NA with the stale
    // failure record intact.
    let a = update(&mut conn, &changed("a.txt", false)).expect("row exists");
    let b = update(&mut conn, &changed("b.txt", true)).expect("row exists");

    assert_eq!(state(&conn, a), STATE_NA);
    assert_eq!(state(&conn, b), STATE_PENDING);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM failed_files"),
        0,
        "both records cleared"
    );
}
