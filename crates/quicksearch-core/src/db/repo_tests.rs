use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::db::open_or_recreate;
use crate::testutil::{zstd_of, Scratch};

fn tmp_path() -> (Scratch, std::path::PathBuf) {
    Scratch::db("repo")
}

/// `set_content_done_fresh` skips the pre-delete, so the whole of its safety
/// is the caller's claim that the row is clean. A duplicate `searchabletext`
/// row is the failure this guards; it surfaces as a file appearing twice in
/// results, not as an error, so nothing else in the suite would notice.
#[test]
fn the_fresh_content_write_leaves_exactly_one_fts_row() {
    fn new_file(name: &str, mtime: u64) -> NewFile<'_> {
        NewFile {
            name,
            parent: "/d/",
            size: 10,
            mtime,
            mime: Some("text/plain"),
            ftype: crate::mime::FileType::TEXT,
            hash: None,
            needs_content: true,
        }
    }
    fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn matches(conn: &rusqlite::Connection, term: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
            [term],
            |r| r.get(0),
        )
        .unwrap()
    }

    let (_dir, path) = tmp_path();
    let mut conn = open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();

    let tx = conn.transaction().unwrap();
    let id = insert_file(&tx, &new_file("a.txt", 1)).unwrap().unwrap();
    set_content_done_fresh(
        &tx,
        id,
        "sphinx quartz",
        zstd_of("sphinx quartz").as_deref(),
    )
    .unwrap();

    // The update path — the exact sequence `process_batch_updates` performs.
    let same = update_file_basic(&tx, &new_file("a.txt", 2))
        .unwrap()
        .unwrap();
    assert_eq!(same, id, "the same row");
    set_content_done_fresh(&tx, id, "sphinx onyx", zstd_of("sphinx onyx").as_deref()).unwrap();
    tx.commit().unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM searchabletext"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 1);
    assert_eq!(matches(&conn, "onyx"), 1, "the new body is searchable");
    assert_eq!(matches(&conn, "quartz"), 0, "the old body is gone");

    // The idempotent entry point still repairs a row that really does hold
    // content — what the content pass and the watcher do.
    let tx = conn.transaction().unwrap();
    set_content_done(
        &tx,
        id,
        "sphinx jasper",
        zstd_of("sphinx jasper").as_deref(),
    )
    .unwrap();
    tx.commit().unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM searchabletext"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 1);
    assert_eq!(matches(&conn, "jasper"), 1);
    assert_eq!(matches(&conn, "onyx"), 0);
}

/// The size report reads each body's length out of its zstd frame header,
/// which works only because [`DocEncoder`] is handed the whole input up
/// front. A switch to a streaming encoder would silently zero that figure.
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

/// Several rows into one arena, which is the shape the writer uses.
///
/// The trap this pins: `zstd`'s `WriteBuf` for `Vec` writes from **offset
/// zero** and sets the length, so compressing straight into a shared arena
/// silently overwrites the previous row and leaves every returned range
/// pointing past the end. Each body must come back byte-identical to what
/// the one-shot encoder produces, and out of its own range.
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

    // The ranges tile the arena in order and account for all of it: a gap or
    // an overlap means one row landed on another.
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
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        let id = insert_file(
            &tx,
            &NewFile {
                name: "a.txt",
                parent: "/tmp/",
                size: 42,
                mtime: 1_700_000_000,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: Some(&[1, 2, 3]),
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(&tx, id, "hello world", zstd_of("hello world").as_deref()).unwrap();
        tx.commit().unwrap();
    }

    let hit: i64 = conn
        .query_row(
            "SELECT rowid FROM searchabletext WHERE searchabletext MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(hit > 0);

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
}

#[test]
fn insert_writes_content_state_from_needs_content() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let tx = conn.transaction().unwrap();
    let mut row = NewFile {
        name: "claimed.txt",
        parent: "/tmp/",
        size: 1,
        mtime: 1,
        mime: Some("text/plain"),
        ftype: FileType::TEXT,
        hash: None,
        needs_content: true,
    };
    let claimed = insert_file(&tx, &row).unwrap().expect("unique path");
    row.name = "unclaimed.mp4";
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
}

#[test]
fn update_writes_content_state_from_needs_content() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let mut row = NewFile {
        name: "a.txt",
        parent: "/tmp/",
        size: 10,
        mtime: 1,
        mime: None,
        ftype: FileType::EMPTY,
        hash: None,
        needs_content: false,
    };
    let id = {
        let tx = conn.transaction().unwrap();
        let id = insert_file(&tx, &row).unwrap().expect("unique path");
        set_content_done(&tx, id, "old text", zstd_of("old text").as_deref()).unwrap();
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
    // stale FTS row goes too.
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
    assert_eq!(content_state(&conn), STATE_PENDING);

    let fts_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_hits, 0);

    // Rewritten as something nothing claims: NA, not pending — else the row
    // re-enters the content pass on every run.
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
}

#[test]
fn insert_file_twice_on_same_path_is_idempotent() {
    // Overlapping roots or symlink resolution revisit a canonical path.
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let tx = conn.transaction().unwrap();
    let row = NewFile {
        name: "dup.txt",
        parent: "/tmp/",
        size: 1,
        mtime: 1,
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
            "SELECT id FROM files WHERE parent = ?1 AND name = ?2",
            params!["/tmp/", "dup.txt"],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_eq!(id_read, id1);
    tx.commit().unwrap();
}

#[test]
fn delete_subtree_clears_every_dependent_table() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/tree/deep/c.txt"], "bad parse").unwrap();
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
    assert_eq!(
        count("SELECT COUNT(*) FROM failed_files"),
        0,
        "the failed row went with its file"
    );

    let survivors: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT parent || name FROM files ORDER BY parent, name")
            .unwrap();
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    };
    assert_eq!(survivors, vec!["/tree2/keep.txt", "/treeX/keep.txt"]);
}

/// Seed fully-indexed rows, each with an FTS entry and stored text; returns
/// `path -> id`.
fn seeded(conn: &mut Connection, paths: &[&str]) -> std::collections::HashMap<String, i64> {
    let tx = conn.transaction().unwrap();
    let mut ids = std::collections::HashMap::new();
    for path in paths {
        let (parent, name) = crate::file_handling::split_db_path(path).expect("a file's path");
        let id = insert_file(
            &tx,
            &NewFile {
                name,
                parent,
                size: 1,
                mtime: 1,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(&tx, id, "body text", zstd_of("body text").as_deref()).unwrap();
        ids.insert((*path).to_string(), id);
    }
    tx.commit().unwrap();
    ids
}

/// The out-of-root sweep must take a path that merely *starts* with a root's
/// name — a different folder.
#[test]
fn delete_outside_ranges_keeps_exactly_the_configured_roots() {
    let (_dir, p) = tmp_path();
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
            .prepare("SELECT parent || name FROM files ORDER BY parent, name")
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
}

/// A survivor in any dependent table keeps the file findable.
#[test]
fn delete_ids_clears_every_dependent_table() {
    let (_dir, p) = tmp_path();
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
    assert_eq!(count("SELECT COUNT(*) FROM failed_files"), 0);

    // A contentless table keeps serving deleted rowids without the tombstone.
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
}

#[test]
fn delete_ids_spans_chunk_boundaries() {
    let (_dir, p) = tmp_path();
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
}

/// Dropping stored text must cost the file its snippets and nothing else.
#[test]
fn drop_stored_text_keeps_the_file_searchable() {
    let (_dir, p) = tmp_path();
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
}

/// Clearing the last extraction is what keeps a second pass from
/// double-inserting into FTS.
#[test]
fn reset_content_pending_clears_the_last_extraction() {
    let (_dir, p) = tmp_path();
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

    let row: (i64, i64) = conn
        .query_row(
            "SELECT content_state, mtime FROM files WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0, STATE_PENDING);
    assert_eq!(row.1, 1, "metadata untouched — the file did not change");

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2, "rows stay");
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 0);
    assert_eq!(
        count("SELECT COUNT(*) FROM failed_files"),
        0,
        "a stale failure must not outlive the retry it was queued for"
    );
}

/// The reconciliation scan must serve every row in the range exactly once,
/// in order, and stop at the range bound rather than at a name prefix.
#[test]
fn rows_in_range_page_walks_the_range_once() {
    let (_dir, p) = tmp_path();
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

/// `dir_rows` must *seek* on `idx_files_parent`, never scan the table — the
/// seek is what keeps the cost per *directory* rather than per *tree*.
/// (Deliberately not index-only; see the index's comment in `schema.rs`.)
#[test]
fn dir_rows_seeks_the_parent_index() {
    let (_dir, p) = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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
    let (_dir, p) = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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
    let (_dir, p) = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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
    let (_dir, p) = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    assert_eq!(get_last_full_index(&conn), None, "fresh DB has no marker");
    set_last_full_index(&conn, 1_700_000_123).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_123));
    // Overwrite, not accumulate.
    set_last_full_index(&conn, 1_700_000_999).unwrap();
    assert_eq!(get_last_full_index(&conn), Some(1_700_000_999));
}

#[test]
fn checkpoint_and_close_truncates_wal() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        insert_file(
            &tx,
            &NewFile {
                name: "w.txt",
                parent: "/tmp/",
                size: 1,
                mtime: 1,
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
    let wal = std::path::PathBuf::from(format!("{}-wal", p.display()));
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_len, 0, "WAL should be truncated on clean close");
    let conn = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

fn seed_rows(conn: &mut Connection, range: std::ops::Range<usize>) {
    let tx = conn.transaction().unwrap();
    for i in range {
        let name = format!("{}.txt", i);
        let id = insert_file(
            &tx,
            &NewFile {
                name: &name,
                parent: "/tmp/bulk/",
                size: 1,
                mtime: 1,
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
            &"lorem ipsum dolor sit amet ".repeat(64),
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
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut conn, 0..200);
    assert!(wal_bytes(&p) > 0, "the writes should be sitting in the log");

    checkpoint_truncate(&conn).expect("nothing is holding the log back");
    assert_eq!(wal_bytes(&p), 0, "a completed TRUNCATE leaves no log");
}

/// A reader pins the log, SQLite declines to reset it, and the only word of
/// it is in the result row.
#[test]
fn checkpoint_truncate_reports_an_incomplete_checkpoint() {
    let (_dir, p) = tmp_path();
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
}

/// The floor under the case above. A TRUNCATE that cannot take the reset lock
/// leaves the file at its high-water mark, and without `journal_size_limit`
/// that mark is where it stays — one bad run leaves a multi-gigabyte log
/// behind for every later reader to page around.
///
/// The limit is not a checkpoint: `sqlite3WalFrames` applies it at the **first
/// commit after the log restarts**, which is the next write once a checkpoint
/// has copied every frame out. So the space comes back on its own, from
/// whichever writer touches the index next, with no successful TRUNCATE
/// anywhere in the story. That is the property worth having — the run that
/// bloated the log is exactly the one whose checkpoint is most likely to lose
/// its lock race.
///
/// The writing pragma profiles carry it; `db::schema`'s
/// `every_writing_profile_bounds_the_log` is what keeps them in step with
/// [`crate::config::MINIMUM_WAL_SIZE`], and this is what shows it works.
#[test]
fn journal_size_limit_gives_the_space_back_after_a_blocked_truncate() {
    let (_dir, p) = tmp_path();
    let mut writer = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut writer, 0..10);
    writer
        .busy_timeout(std::time::Duration::from_millis(100))
        .unwrap();

    let peak = {
        // Held across the seeding, which is what lets the log grow past the
        // limit at all: pinned to an early frame, no checkpoint of any kind
        // can reset it. This is the run's own shape — a reader per root, live
        // from start to finish.
        let reader = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
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

    // The reader is gone, so an ordinary PASSIVE checkpoint copies every
    // frame out — but on its own it trims nothing, because the log has not
    // restarted yet.
    writer
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .unwrap();
    assert_eq!(
        wal_bytes(&p),
        peak,
        "a checkpoint that does not restart the log cannot trim it"
    );

    // The next write restarts it, and *that* commit honours the limit. No
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
        let tx = conn.transaction().unwrap();
        for i in range {
            let name = format!("{}.txt", i);
            insert_file(
                &tx,
                &NewFile {
                    name: &name,
                    parent: "/tmp/bare/",
                    size: i as u64,
                    mtime: 1,
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

    let (_dir_a, unbounded) = tmp_path();
    let left_alone = run(&unbounded, 0);

    let (_dir_b, bounded) = tmp_path();
    let forced = run(&bounded, 4);

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
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    assert_eq!(
        analyzed_file_count(&conn),
        None,
        "a never-analyzed index must say so, not report zero files"
    );

    seed_rows(&mut conn, 0..2000);
    // All 2000 rows land content_state = 0, so `idx_files_content_pending`
    // covers every one of them; mark most done to make the partial index
    // genuinely smaller than the table, which is the trap being tested.
    conn.execute("UPDATE files SET content_state = 1 WHERE id % 100 != 0", [])
        .unwrap();
    conn.execute_batch("ANALYZE;").unwrap();

    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        pending < 2000,
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
    let (_dir, p) = tmp_path();
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

    assert!(
        maintain(&conn, p.to_str().unwrap()).unwrap(),
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
}

#[test]
fn maintain_skips_vacuum_on_a_tight_file() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    seed_rows(&mut conn, 0..200);
    drop(conn);

    let conn = crate::db::open::open_maintenance(p.to_str().unwrap()).unwrap();
    assert!(
        !maintain(&conn, p.to_str().unwrap()).unwrap(),
        "a file with no slack is not worth rewriting"
    );
    // The checkpoint is not conditional on the vacuum, though.
    assert_eq!(wal_bytes(&p), 0);
}

#[test]
fn set_content_failed_writes_failed_table() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let id = {
        let tx = conn.transaction().unwrap();
        let id = insert_file(
            &tx,
            &NewFile {
                name: "oops.bin",
                parent: "/tmp/",
                size: 0,
                mtime: 1,
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
}

fn insert_at(tx: &Transaction<'_>, path: &str, needs_content: bool) -> i64 {
    let (parent, name) = crate::file_handling::split_db_path(path).expect("a file's path");
    insert_file(
        tx,
        &NewFile {
            name,
            parent,
            size: 1,
            mtime: 1,
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

/// The premise of `count_root`: `content_state = DONE` exactly when a
/// `searchabletext` row exists. Pinned against the FTS table itself, because
/// the equivalence is what breaks if a transition writes one without the
/// other.
#[test]
fn count_root_counts_the_fts_rows_it_says_it_does() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    {
        let tx = conn.transaction().unwrap();
        // Two searchable, and one of each way a row can fail to be.
        for path in ["/tree/a.txt", "/tree/b.txt"] {
            let id = insert_at(&tx, path, true);
            set_content_done(&tx, id, "body text", zstd_of("body text").as_deref()).unwrap();
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
    // root's figure.
    {
        let tx = conn.transaction().unwrap();
        let id = insert_at(&tx, "/elsewhere/f.txt", true);
        set_content_done(&tx, id, "body text", zstd_of("body text").as_deref()).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(fts_rows(&conn), 3);
    assert_eq!(
        count_root(&conn, "/tree/", "/tree0").unwrap(),
        counts,
        "a row outside the range belongs to no root's figures"
    );
}

/// An empty range is 0/0, not an error — `SUM` over no rows is NULL.
#[test]
fn count_root_reports_zero_for_an_empty_range() {
    let (_dir, p) = tmp_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    assert_eq!(
        count_root(&conn, "/nothing/", "/nothing0").unwrap(),
        RootCounts { files: 0, fts: 0 }
    );
}

#[test]
fn root_counts_round_trip() {
    let (_dir, p) = tmp_path();
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
    let (_dir, p) = tmp_path();
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
}

/// The parent-scan range must include the root's *own* directory or files
/// sitting directly in a root are never reconciled — true only because every
/// stored parent ends in a separator, making the root's parent exactly `lo`.
#[test]
fn the_parent_scan_reaches_the_roots_own_directory() {
    let (_dir, p) = tmp_path();
    let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
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

    let range = crate::file_handling::ExtractCursor::for_root("/tree");
    let mut seen = Vec::new();
    for_each_parent_in_range(&conn, &range.lo, &range.hi, |parent| seen.push(parent)).unwrap();
    seen.sort();
    assert_eq!(
        seen,
        vec!["/tree/", "/tree/deep/", "/tree/deep/deeper/"],
        "the root's own directory is in range, and the siblings are not"
    );
}

/// Pins the two facts the delete helpers rest on: the cascade fires on a
/// production connection, and `searchabletext` (FTS5, no foreign key) does
/// NOT cascade — why its delete stays explicit.
#[test]
fn deleting_a_file_row_cascades_the_fk_tables() {
    let (_dir, path) = tmp_path();
    let mut conn = open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(&mut conn, &["/casc/a.txt", "/casc/b.txt"]);
    let count =
        |conn: &Connection, sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/casc/b.txt"], "boom").unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents_text"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM failed_files"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM searchabletext"), 2);

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
        count(&conn, "SELECT COUNT(*) FROM searchabletext"),
        2,
        "FTS5 cannot cascade — the explicit delete in the helpers is load-bearing"
    );
}

#[test]
fn retry_failed_files_resets_state_and_clears_records() {
    let (_dir, path) = tmp_path();
    let mut conn = open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(&mut conn, &["/retry/bad.txt", "/retry/good.txt"]);
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/retry/bad.txt"], "parser choked").unwrap();
        tx.commit().unwrap();
    }

    let tx = conn.transaction().unwrap();
    assert_eq!(retry_failed_files(&tx).unwrap(), 1);
    tx.commit().unwrap();

    let state = |id: i64| -> i64 {
        conn.query_row("SELECT content_state FROM files WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(state(ids["/retry/bad.txt"]), STATE_PENDING);
    assert_eq!(
        state(ids["/retry/good.txt"]),
        STATE_DONE,
        "done rows untouched"
    );
    let failures: i64 = conn
        .query_row("SELECT COUNT(*) FROM failed_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(failures, 0);
}

/// `update_file_basic` must clear a stale failure record on both content
/// paths, or `list-failed` keeps reporting a fixed file as broken.
#[test]
fn a_changed_file_clears_its_failure_record() {
    let (_dir, path) = tmp_path();
    let mut conn = open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let ids = seeded(&mut conn, &["/chg/a.txt", "/chg/b.txt"]);
    {
        let tx = conn.transaction().unwrap();
        set_content_failed(&tx, ids["/chg/a.txt"], "boom").unwrap();
        set_content_failed(&tx, ids["/chg/b.txt"], "boom").unwrap();
        tx.commit().unwrap();
    }

    let update = |conn: &mut Connection, name: &str, needs_content: bool| {
        let tx = conn.transaction().unwrap();
        let id = update_file_basic(
            &tx,
            &NewFile {
                name,
                parent: "/chg/",
                size: 2,
                mtime: 9,
                mime: if needs_content {
                    Some("text/plain")
                } else {
                    None
                },
                ftype: FileType::TEXT,
                hash: None,
                needs_content,
            },
        )
        .unwrap()
        .expect("row exists");
        tx.commit().unwrap();
        id
    };
    // The bug this pins: needs_content = false lands in NA with the stale
    // failure record intact.
    let a = update(&mut conn, "a.txt", false);
    let b = update(&mut conn, "b.txt", true);

    let state = |id: i64| -> i64 {
        conn.query_row("SELECT content_state FROM files WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(state(a), STATE_NA);
    assert_eq!(state(b), STATE_PENDING);
    let failures: i64 = conn
        .query_row("SELECT COUNT(*) FROM failed_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(failures, 0, "both records cleared");
}
