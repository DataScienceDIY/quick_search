use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::db::open_or_recreate;
use crate::testutil::zstd_of;

fn tmp_path() -> std::path::PathBuf {
    crate::testutil::scratch_dir("repo").join("index.sqlite")
}

#[test]
fn insert_update_delete_round_trip() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        let id = insert_file(
            &tx,
            &NewFile {
                name: "a.txt",
                path: "/tmp/a.txt",
                parent: "/tmp",
                size: 42,
                mtime: 1_700_000_000,
                inode: Some(7),
                device_id: Some(64768),
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: Some(&[1, 2, 3]),
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(
            &tx,
            id,
            "a.txt",
            "hello world",
            &[("title".to_string(), "hi".to_string())],
            zstd_of("hello world").as_deref(),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Text is findable via FTS.
    let hit: i64 = conn
        .query_row(
            "SELECT rowid FROM searchabletext WHERE searchabletext MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(hit > 0);

    // Delete cleans up.
    {
        let tx = conn.transaction().unwrap();
        assert!(delete_file_by_path(&tx, "/tmp/a.txt").unwrap());
        tx.commit().unwrap();
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_count, 0);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn insert_writes_content_state_from_needs_content() {
    // A row nothing will extract is born NA, so "pending" downstream means
    // real outstanding work.
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let tx = conn.transaction().unwrap();
    let mut row = NewFile {
        name: "claimed.txt",
        path: "/tmp/claimed.txt",
        parent: "/tmp",
        size: 1,
        mtime: 1,
        inode: None,
        device_id: None,
        mime: Some("text/plain"),
        ftype: FileType::TEXT,
        hash: None,
        needs_content: true,
    };
    let claimed = insert_file(&tx, &row).unwrap().expect("unique path");
    row.name = "unclaimed.mp4";
    row.path = "/tmp/unclaimed.mp4";
    row.mime = Some("video/mp4");
    row.needs_content = false;
    let unclaimed = insert_file(&tx, &row).unwrap().expect("unique path");

    let state = |id: i64| -> i64 {
        tx.query_row(
            "SELECT content_state FROM files WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(state(claimed), STATE_PENDING);
    assert_eq!(state(unclaimed), STATE_NA);

    drop(tx);
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn update_writes_content_state_from_needs_content() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let mut row = NewFile {
        name: "a.txt",
        path: "/tmp/a.txt",
        parent: "/tmp",
        size: 10,
        mtime: 1,
        inode: None,
        device_id: None,
        mime: None,
        ftype: FileType::EMPTY,
        hash: None,
        needs_content: false,
    };
    let id = {
        let tx = conn.transaction().unwrap();
        let id = insert_file(&tx, &row).unwrap().expect("unique path");
        set_content_done(
            &tx,
            id,
            "a.txt",
            "old text",
            &[],
            zstd_of("old text").as_deref(),
        )
        .unwrap();
        tx.commit().unwrap();
        id
    };

    let content_state = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT content_state FROM files WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Rewritten as something an extractor claims: back to pending, and the
    // stale FTS row goes with it.
    {
        let tx = conn.transaction().unwrap();
        row.size = 20;
        row.mtime = 2;
        row.mime = Some("text/plain");
        row.ftype = FileType::TEXT;
        row.needs_content = true;
        let got = update_file_basic(&tx, &row).unwrap();
        assert_eq!(got, Some(id));
        tx.commit().unwrap();
    }
    let basic: i64 = conn
        .query_row(
            "SELECT basic_state FROM files WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(basic, STATE_DONE);
    assert_eq!(content_state(&conn), STATE_PENDING);

    let fts_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_hits, 0);

    // And rewritten as something nothing claims: NA, not pending. Without
    // this the row would re-enter the content pass on every run.
    {
        let tx = conn.transaction().unwrap();
        row.mtime = 3;
        row.mime = Some("video/mp4");
        row.needs_content = false;
        update_file_basic(&tx, &row)
            .unwrap()
            .expect("row still there");
        tx.commit().unwrap();
    }
    assert_eq!(content_state(&conn), STATE_NA);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn insert_file_twice_on_same_path_is_idempotent() {
    // A second visit to the same canonical path (overlapping roots, symlink
    // resolution) must be a silent no-op, not a run-ending error.
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let tx = conn.transaction().unwrap();
    let row = NewFile {
        name: "dup.txt",
        path: "/tmp/dup.txt",
        parent: "/tmp",
        size: 1,
        mtime: 1,
        inode: None,
        device_id: None,
        mime: Some("text/plain"),
        ftype: FileType::TEXT,
        hash: None,
        needs_content: true,
    };
    let id1 = insert_file(&tx, &row).unwrap().expect("first insert");
    let id2 = insert_file(&tx, &row).unwrap();
    assert!(id2.is_none(), "second insert of same path must return None");
    let count: i64 = tx
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let (id_read,): (i64,) = tx
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            params!["/tmp/dup.txt"],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_eq!(id_read, id1);
    tx.commit().unwrap();
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn delete_subtree_clears_every_dependent_table() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let add = |tx: &Transaction<'_>, path: &str| -> i64 {
        let name = path.rsplit('/').next().unwrap();
        let parent = &path[..path.rfind('/').unwrap()];
        let id = insert_file(
            tx,
            &NewFile {
                name,
                path,
                parent,
                size: 1,
                mtime: 1,
                inode: None,
                device_id: None,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(
            tx,
            id,
            name,
            "body text",
            &[("k".into(), "v".into())],
            zstd_of("body text").as_deref(),
        )
        .unwrap();
        id
    };

    {
        let tx = conn.transaction().unwrap();
        add(&tx, "/tree/a.txt");
        add(&tx, "/tree/deep/b.txt");
        let failed = add(&tx, "/tree/deep/c.txt");
        set_content_failed(&tx, failed, "bad parse").unwrap();
        // Outside the range: a prefix sibling, and a LIKE-metacharacter
        // neighbour that a `LIKE 'tree_%'` sweep would have swallowed.
        add(&tx, "/tree2/keep.txt");
        add(&tx, "/treeX/keep.txt");
        tx.commit().unwrap();
    }

    let range = crate::file_handling::ExtractCursor::for_root("/tree");
    let removed = {
        let tx = conn.transaction().unwrap();
        let n = delete_subtree(&tx, &range.lo, &range.hi).unwrap();
        tx.commit().unwrap();
        n
    };
    assert_eq!(removed, 3, "three files under /tree");

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2, "siblings survive");
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM properties"), 2);
    assert_eq!(
        count("SELECT COUNT(*) FROM failed_files"),
        0,
        "the failed row went with its file"
    );

    let survivors: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap();
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    };
    assert_eq!(survivors, vec!["/tree2/keep.txt", "/treeX/keep.txt"]);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Seed a database with `paths` as fully-indexed rows, each carrying an
/// FTS entry, stored text and a property. Returns `path -> id`.
fn seeded(conn: &mut Connection, paths: &[&str]) -> std::collections::HashMap<String, i64> {
    let tx = conn.transaction().unwrap();
    let mut ids = std::collections::HashMap::new();
    for path in paths {
        let name = path.rsplit('/').next().unwrap();
        let parent = &path[..path.rfind('/').unwrap()];
        let id = insert_file(
            &tx,
            &NewFile {
                name,
                path,
                parent,
                size: 1,
                mtime: 1,
                inode: None,
                device_id: None,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(
            &tx,
            id,
            name,
            "body text",
            &[("k".into(), "v".into())],
            zstd_of("body text").as_deref(),
        )
        .unwrap();
        ids.insert((*path).to_string(), id);
    }
    tx.commit().unwrap();
    ids
}

/// The out-of-root sweep must keep every configured root's rows and take
/// everything else — including a path that merely *starts* with a root's
/// name, which is a different folder.
#[test]
fn delete_outside_ranges_keeps_exactly_the_configured_roots() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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
            let range = crate::file_handling::ExtractCursor::for_root(r);
            (range.lo, range.hi)
        })
        .collect();
    let removed = {
        let tx = conn.transaction().unwrap();
        let n = delete_outside_ranges(&tx, &ranges).unwrap();
        tx.commit().unwrap();
        n
    };
    assert_eq!(removed, 2, "the name-prefix sibling and the outsider");

    let survivors: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap();
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    };
    assert_eq!(
        survivors,
        vec![
            "/roots/one/a.txt",
            "/roots/one/deep/b.txt",
            "/roots/two/c.txt"
        ]
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM documents_text", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );

    // No roots configured is a half-written config, not an instruction to
    // delete the entire index.
    let tx = conn.transaction().unwrap();
    assert_eq!(delete_outside_ranges(&tx, &[]).unwrap(), 0);
    tx.commit().unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// A file is only really gone when its `files` row, FTS postings, stored
/// text, properties and any failure record all go; a survivor in any one of
/// them keeps the file findable.
#[test]
fn delete_ids_clears_every_dependent_table() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(
        &mut conn,
        &["/t/a.txt", "/t/b.log", "/t/deep/c.log", "/t/keep.txt"],
    );
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/t/deep/c.log"], "bad parse").unwrap();
        tx.commit().unwrap();
    }

    let doomed = vec![ids["/t/b.log"], ids["/t/deep/c.log"]];
    let removed = {
        let tx = conn.transaction().unwrap();
        let n = delete_ids(&tx, &doomed).unwrap();
        tx.commit().unwrap();
        n
    };
    assert_eq!(removed, 2);

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM properties"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM failed_files"), 0);

    // The FTS index really lost them, not just the `files` row: a
    // contentless table keeps serving deleted rowids without the tombstone.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'body'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 2);

    // Empty input is a no-op, not a statement with an empty `IN ()`.
    let tx = conn.transaction().unwrap();
    assert_eq!(delete_ids(&tx, &[]).unwrap(), 0);
    tx.commit().unwrap();

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// More ids than `DELETE_IDS_CHUNK`, so the short final chunk and the full
/// ones both run.
#[test]
fn delete_ids_spans_chunk_boundaries() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let paths: Vec<String> = (0..DELETE_IDS_CHUNK + 7)
        .map(|i| format!("/t/f{:05}.txt", i))
        .collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let ids = seeded(&mut conn, &refs);

    let mut all: Vec<i64> = ids.values().copied().collect();
    all.sort_unstable();
    let keep = all.pop().unwrap();
    let removed = {
        let tx = conn.transaction().unwrap();
        let n = delete_ids(&tx, &all).unwrap();
        tx.commit().unwrap();
        n
    };
    assert_eq!(removed, DELETE_IDS_CHUNK + 6);
    let left: i64 = conn
        .query_row("SELECT id FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, keep);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Dropping stored text must cost the file its snippets and nothing else.
#[test]
fn drop_stored_text_keeps_the_file_searchable() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(&mut conn, &["/t/a.txt", "/t/b.txt"]);

    {
        let tx = conn.transaction().unwrap();
        drop_stored_text(&tx, &[ids["/t/a.txt"]]).unwrap();
        tx.commit().unwrap();
    }

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 2);
    assert_eq!(
        count("SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'body'"),
        2,
        "both files still match on content"
    );
    assert_eq!(count("SELECT COUNT(*) FROM properties"), 2);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Re-queuing content leaves the row's metadata alone but clears what the
/// last extraction produced, so a second pass cannot double-insert into FTS.
#[test]
fn reset_content_pending_clears_the_last_extraction() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(&mut conn, &["/t/a.txt", "/t/b.txt"]);
    let id = ids["/t/a.txt"];
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/t/b.txt"], "bad parse").unwrap();
        tx.commit().unwrap();
    }

    {
        let tx = conn.transaction().unwrap();
        reset_content_pending(&tx, id).unwrap();
        reset_content_pending(&tx, ids["/t/b.txt"]).unwrap();
        tx.commit().unwrap();
    }

    let row: (i64, i64, Option<String>) = conn
        .query_row(
            "SELECT content_state, mtime, failure_msg FROM files WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, STATE_PENDING);
    assert_eq!(row.1, 1, "metadata untouched — the file did not change");
    assert_eq!(row.2, None);

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2, "rows stay");
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM properties"), 0);
    assert_eq!(
        count("SELECT COUNT(*) FROM failed_files"),
        0,
        "a stale failure must not outlive the retry it was queued for"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// The reconciliation scan pages by path, so it must serve every row in
/// the range exactly once, in order, and stop at the range bound rather
/// than at a name prefix.
#[test]
fn rows_in_range_page_walks_the_range_once() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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

    let range = crate::file_handling::ExtractCursor::for_root("/t");
    let mut seen = Vec::new();
    let mut after = range.lo.clone();
    loop {
        let page = rows_in_range_page(&conn, &after, &range.hi, 2).unwrap();
        let Some(last) = page.last() else { break };
        after = last.path.clone();
        seen.extend(page.into_iter().map(|r| r.path));
    }
    assert_eq!(
        seen,
        vec!["/t/a.txt", "/t/deep/b.txt", "/t/deep/deeper/c.txt"],
        "in path order, once each, and the prefix siblings are outside"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// `idx_files_parent` carries `name` and `mtime` so `dir_rows` never touches
/// the table heap; trimming it back to `(parent)` would silently reintroduce
/// a row fetch per entry.
#[test]
fn dir_rows_is_served_entirely_from_the_index() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT name, mtime FROM files WHERE parent = ?1",
            params!["/some/dir"],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("COVERING INDEX idx_files_parent"),
        "dir_rows must be index-only, got: {}",
        plan
    );
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// The range form is an index seek; `LIKE … ESCAPE` cannot be (SQLite
/// disables the LIKE optimisation whenever an ESCAPE clause is present).
#[test]
fn the_subtree_range_is_an_index_seek_not_a_scan() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN DELETE FROM files WHERE path >= ?1 AND path < ?2",
            params!["/tree/", "/tree0"],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("SEARCH") && !plan.contains("SCAN"),
        "range delete must seek, got: {}",
        plan
    );
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn last_full_index_round_trip() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    assert_eq!(get_last_full_index(&conn), None, "fresh DB has no marker");
    set_last_full_index(&conn, 1_700_000_123).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_123));
    // Overwrite, not accumulate.
    set_last_full_index(&conn, 1_700_000_999).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_999));
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn checkpoint_and_close_truncates_wal() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        insert_file(
            &tx,
            &NewFile {
                name: "w.txt",
                path: "/tmp/w.txt",
                parent: "/tmp",
                size: 1,
                mtime: 1,
                inode: None,
                device_id: None,
                mime: None,
                ftype: FileType::EMPTY,
                hash: None,
                needs_content: false,
            },
        )
        .unwrap();
        tx.commit().unwrap();
    }
    checkpoint_and_close(conn);
    // After a TRUNCATE checkpoint + close of the last connection the WAL
    // sidecar is gone or empty; the row lives in the main file.
    let wal = std::path::PathBuf::from(format!("{}-wal", p.display()));
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_len, 0, "WAL should be truncated on clean close");
    let conn = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Bulk rows, cheap to write, enough of them to make the file grow.
fn seed_rows(conn: &mut Connection, range: std::ops::Range<usize>) {
    let tx = conn.transaction().unwrap();
    for i in range {
        let path = format!("/tmp/bulk/{}.txt", i);
        let name = format!("{}.txt", i);
        let id = insert_file(
            &tx,
            &NewFile {
                name: &name,
                path: &path,
                parent: "/tmp/bulk",
                size: 1,
                mtime: 1,
                inode: None,
                device_id: None,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .unwrap();
        set_content_done(
            &tx,
            id,
            &name,
            &"lorem ipsum dolor sit amet ".repeat(64),
            &[],
            zstd_of(&"lorem ipsum dolor sit amet ".repeat(64)).as_deref(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn wal_bytes(p: &std::path::Path) -> u64 {
    std::fs::metadata(format!("{}-wal", p.display()))
        .map(|m| m.len())
        .unwrap_or(0)
}

#[test]
fn checkpoint_truncate_empties_the_log() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut conn, 0..200);
    assert!(wal_bytes(&p) > 0, "the writes should be sitting in the log");

    checkpoint_truncate(&conn).expect("nothing is holding the log back");
    assert_eq!(wal_bytes(&p), 0, "a completed TRUNCATE leaves no log");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// A reader pins the log, SQLite declines to reset it, and the only word of
/// it is in the result row.
#[test]
fn checkpoint_truncate_reports_an_incomplete_checkpoint() {
    let p = tmp_path();
    let mut writer = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut writer, 0..200);
    writer
        .busy_timeout(std::time::Duration::from_millis(100))
        .unwrap();

    let reader = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
    let mut stmt = reader.prepare("SELECT id FROM files").unwrap();
    let mut rows = stmt.query([]).unwrap();
    rows.next().unwrap().expect("a row to hold the snapshot on");

    let err = checkpoint_truncate(&writer).expect_err("a reader holds the log open");
    assert!(err.contains("incomplete"), "unexpected message: {}", err);
    assert!(wal_bytes(&p) > 0, "and the log is still there");

    drop(rows);
    drop(stmt);
    drop(reader);
    drop(writer);
    std::fs::remove_file(&p).ok();
}

/// SQLite's autocheckpoint copies committed frames into the database
/// continuously, but the log is only *reset* when the writer opens a
/// transaction at an instant no reader holds a read mark — and it tries that
/// lock exactly once, with no retry. A reader querying back to back keeps
/// that instant from arriving, so the log appends for as long as the run
/// lasts. An explicit checkpoint retries the same lock under `busy_timeout`
/// and gets it.
#[test]
fn a_busy_reader_defeats_the_autocheckpoint_but_not_a_forced_one() {
    // Bare rows: no zstd or tokenising, so the test stays fast while the
    // log still grows.
    fn seed_bare(conn: &mut Connection, range: std::ops::Range<usize>) {
        let tx = conn.transaction().unwrap();
        for i in range {
            let path = format!("/tmp/bare/{}.txt", i);
            let name = format!("{}.txt", i);
            insert_file(
                &tx,
                &NewFile {
                    name: &name,
                    path: &path,
                    parent: "/tmp/bare",
                    size: i as u64,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: None,
                    ftype: FileType::TEXT,
                    hash: Some(&[0u8; 32]),
                    needs_content: false,
                },
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    fn run(p: &std::path::Path, force_every: usize) -> u64 {
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        seed_bare(&mut conn, 0..500);

        // Stands in for a walk prefetcher: short reads, no gaps.
        let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, stop) = (p.to_path_buf(), stop.clone());
            std::thread::spawn(move || {
                let conn = crate::db::open_existing(path.to_str().unwrap(), false).unwrap();
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

    let unbounded = tmp_path();
    let left_alone = run(&unbounded, 0);
    std::fs::remove_file(&unbounded).ok();

    let bounded = tmp_path();
    let forced = run(&bounded, 4);
    std::fs::remove_file(&bounded).ok();

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

#[test]
fn maintain_vacuums_when_slack_is_significant() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut conn, 0..2000);
    checkpoint_truncate(&conn).unwrap();
    let before = std::fs::metadata(&p).unwrap().len();

    {
        let tx = conn.transaction().unwrap();
        for i in 0..1900 {
            delete_file_by_path(&tx, &format!("/tmp/bulk/{}.txt", i)).unwrap();
        }
        tx.commit().unwrap();
    }
    drop(conn);

    let conn = crate::db::open::open_maintenance(p.to_str().unwrap()).unwrap();
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert!(freelist > 0, "the deletions should have freed pages");

    let dir = p.parent().unwrap().to_string_lossy().into_owned();
    assert!(
        maintain(&conn, &dir).unwrap(),
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
    // The surviving rows are still there and still searchable.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 100);
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'lorem'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 100, "the FTS index survived the rewrite");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn maintain_skips_vacuum_on_a_tight_file() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut conn, 0..200);
    drop(conn);

    let conn = crate::db::open::open_maintenance(p.to_str().unwrap()).unwrap();
    let dir = p.parent().unwrap().to_string_lossy().into_owned();
    assert!(
        !maintain(&conn, &dir).unwrap(),
        "a file with no slack is not worth rewriting"
    );
    // The checkpoint is not conditional on the vacuum, though.
    assert_eq!(wal_bytes(&p), 0);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn set_content_failed_writes_failed_table() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let id = {
        let tx = conn.transaction().unwrap();
        let id = insert_file(
            &tx,
            &NewFile {
                name: "oops.bin",
                path: "/tmp/oops.bin",
                parent: "/tmp",
                size: 0,
                mtime: 1,
                inode: None,
                device_id: None,
                mime: None,
                ftype: FileType::EMPTY,
                hash: None,
                needs_content: false,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_failed(&tx, id, "bad parse").unwrap();
        tx.commit().unwrap();
        id
    };

    let reason: String = conn
        .query_row(
            "SELECT reason FROM failed_files WHERE file_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "bad parse");
    let content_state: i64 = conn
        .query_row(
            "SELECT content_state FROM files WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content_state, STATE_FAILED);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Insert one row under `path`, born pending when `needs_content`.
fn insert_at(tx: &Transaction<'_>, path: &str, needs_content: bool) -> i64 {
    let name = path.rsplit('/').next().unwrap();
    let parent = &path[..path.rfind('/').unwrap()];
    insert_file(
        tx,
        &NewFile {
            name,
            path,
            parent,
            size: 1,
            mtime: 1,
            inode: None,
            device_id: None,
            mime: Some("text/plain"),
            ftype: FileType::TEXT,
            hash: None,
            needs_content,
        },
    )
    .unwrap()
    .expect("unique path")
}

fn fts_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r.get(0))
        .unwrap()
}

/// The whole premise of answering both figures from `files`: a row reads
/// `content_state = DONE` exactly when it has a `searchabletext` row, so the
/// conditional sum *is* a count of the FTS table restricted to a path range.
///
/// Pinned against the FTS table itself rather than against the states that
/// were written, because the equivalence is what would break if some future
/// transition wrote one without the other.
#[test]
fn count_root_counts_the_fts_rows_it_says_it_does() {
    let p = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        // Two searchable, and one of each way a row can fail to be.
        for path in ["/tree/a.txt", "/tree/b.txt"] {
            let id = insert_at(&tx, path, true);
            set_content_done(
                &tx,
                id,
                "n",
                "body text",
                &[],
                zstd_of("body text").as_deref(),
            )
            .unwrap();
        }
        let failed = insert_at(&tx, "/tree/c.bin", true);
        set_content_failed(&tx, failed, "bad parse").unwrap();
        let na = insert_at(&tx, "/tree/d.iso", true);
        set_content_na(&tx, na).unwrap();
        insert_at(&tx, "/tree/e.txt", true); // still pending
        tx.commit().unwrap();
    }

    let counts = count_root(&conn, "/tree/", "/tree0").unwrap();
    assert_eq!(counts.files, 5, "every row under the root");
    assert_eq!(
        counts.fts,
        fts_rows(&conn),
        "the root holds everything, so its FTS figure is the whole table"
    );
    assert_eq!(counts.fts, 2);

    // A searchable row outside the range moves the table's total and not the
    // root's figure — otherwise the assertion above would hold for a count
    // that ignored its bounds.
    {
        let tx = conn.transaction().unwrap();
        let id = insert_at(&tx, "/elsewhere/f.txt", true);
        set_content_done(
            &tx,
            id,
            "n",
            "body text",
            &[],
            zstd_of("body text").as_deref(),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(fts_rows(&conn), 3);
    assert_eq!(
        count_root(&conn, "/tree/", "/tree0").unwrap(),
        counts,
        "a row outside the range belongs to no root's figures"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// An empty range is 0/0, not an error: a configured root nothing has been
/// walked into yet is a normal state, and `SUM` over no rows is NULL.
#[test]
fn count_root_reports_zero_for_an_empty_range() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    assert_eq!(
        count_root(&conn, "/nothing/", "/nothing0").unwrap(),
        RootCounts { files: 0, fts: 0 }
    );
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn root_counts_round_trip() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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

    // A value this build cannot parse reads as absent, like a missing one:
    // the folder list says "not yet indexed" rather than showing a number
    // that is a guess.
    for bad in ["", "12", "12,", "a,b", "12,5,3"] {
        conn.execute(
            "INSERT OR REPLACE INTO schema_info(key, value) VALUES ('counts:/tree', ?1)",
            params![bad],
        )
        .unwrap();
        assert_eq!(get_root_counts(&conn, "/tree"), None, "parsed {:?}", bad);
    }

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Both kinds of per-root figure are swept together, so a root removed and
/// later re-added starts from neither a stale denominator nor a stale count.
#[test]
fn prune_root_stats_drops_every_figure_of_a_dropped_root() {
    let p = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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

    drop(conn);
    std::fs::remove_file(&p).ok();
}
