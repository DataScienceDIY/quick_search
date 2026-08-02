//! End-to-end phase-1 tests over a real tree and a real database.
//!
//! These cover the failure mode that unit tests structurally cannot: a full
//! run deletes index rows for every path it did not see, so any walk that
//! quietly reports less than it should destroys data. That damage is
//! invisible on a first index — `existing_files` is empty, so nothing is
//! stale — and only appears on the second run.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quicksearch_core::config::Config;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "quicksearch-e2e-{}-{}-{}",
        tag,
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn touch(p: &Path, body: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Run one full index and wait for it to finish.
///
/// Completion is detected via the `last_full_index` marker, which
/// `run_indexing` writes only on a successful finish. Polling the status
/// enum instead would race: a small tree finishes between two polls, so
/// `Idle` is ambiguous between "not started yet" and "already done".
fn index_once(root: &Path, db: &Path, config: &Config) {
    if db.exists() {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
            .unwrap();
    }

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut done = false;
    while Instant::now() < deadline {
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if db.exists() {
            if let Ok(conn) = rusqlite::Connection::open(db) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(done, "indexing did not finish within the timeout");
    service.stop_indexing().unwrap();
}

/// (path, mtime, content_state) for every indexed row, ordered by path.
fn rows(db: &Path) -> Vec<(String, i64, i64)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT path, mtime, content_state FROM files ORDER BY path")
        .unwrap();
    let out = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
}

fn test_config() -> Config {
    let config = Config::default();
    // Keep the run to phase 1 semantics we're asserting on; extraction is
    // covered elsewhere.
    config
}

#[test]
fn reindexing_an_unchanged_tree_changes_nothing() {
    let root = tmp_dir("stable");
    let db_dir = tmp_dir("stable-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    touch(&root.join("a.txt"), b"alpha");
    touch(&root.join("sub/b.txt"), b"bravo");
    touch(&root.join("sub/deep/c.txt"), b"charlie");
    touch(&root.join("other/d.md"), b"delta");

    index_once(&root, &db, &config);
    let first = rows(&db);
    assert_eq!(first.len(), 4, "all four files indexed");

    index_once(&root, &db, &config);
    let second = rows(&db);

    // The whole point: a second run over an unchanged tree must not delete
    // and re-insert anything. A wiped-and-rebuilt row would come back with
    // content_state reset, throwing away extracted text for no reason.
    assert_eq!(first, second, "an unchanged tree must re-index to an identical set");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn deleted_files_are_removed_and_new_ones_added() {
    let root = tmp_dir("churn");
    let db_dir = tmp_dir("churn-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    touch(&root.join("keep.txt"), b"keep");
    touch(&root.join("remove.txt"), b"remove");
    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 2);

    std::fs::remove_file(root.join("remove.txt")).unwrap();
    touch(&root.join("added.txt"), b"added");
    index_once(&root, &db, &config);

    let names: Vec<String> = rows(&db)
        .into_iter()
        .map(|(p, _, _)| Path::new(&p).file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["added.txt", "keep.txt"], "stale cleanup still works");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn a_modified_file_is_updated_in_place() {
    let root = tmp_dir("modify");
    let db_dir = tmp_dir("modify-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    let target = root.join("doc.txt");
    touch(&target, b"first");
    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 1);

    // Filesystem mtime has one-second granularity in the stored value, so
    // move it decisively rather than racing it.
    touch(&target, b"second body, clearly different");
    let later = SystemTime::now() + Duration::from_secs(5);
    filetime_set(&target, later);

    index_once(&root, &db, &config);
    let after = rows(&db);
    assert_eq!(after.len(), 1, "still exactly one row");
    assert_ne!(before[0].1, after[0].1, "mtime was refreshed");
    assert_eq!(before[0].0, after[0].0, "same path");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Set a file's mtime without pulling in a dependency for it.
fn filetime_set(path: &Path, when: SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
    f.sync_all().unwrap();
}

#[test]
#[cfg(unix)]
fn an_unreadable_directory_does_not_delete_its_rows() {
    // The scenario this guards: a network share or removable drive that is
    // briefly unavailable. The walk sees nothing beneath it, which must not
    // be read as "every file under here was deleted".
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_dir("blip");
    let db_dir = tmp_dir("blip-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    touch(&root.join("visible.txt"), b"visible");
    let vault = root.join("vault");
    touch(&vault.join("secret.txt"), b"secret");
    touch(&vault.join("nested/deeper.txt"), b"deeper");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 3, "all three indexed while readable");

    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o000)).unwrap();
    index_once(&root, &db, &config);
    let during = rows(&db);
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        during.len(),
        3,
        "rows under an unreadable directory must survive, not be deleted"
    );

    // And once it is readable again, everything still lines up.
    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 3);

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn stopping_mid_run_deletes_nothing() {
    // Pins the end-to-end property: an interrupted run must never delete the
    // rows it did not reach.
    //
    // Two independent guards currently provide it — `run_indexing` skips
    // cleanup when the walk did not complete, and `cleanup_stale_index_entries`
    // re-checks the stop flag before its first delete. This test passes with
    // either one alone, so it does not prove the former is present; it is here
    // to catch the day someone removes the last of them.
    let root = tmp_dir("stop");
    let db_dir = tmp_dir("stop-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    for i in 0..1500 {
        touch(&root.join(format!("d{}/f{:04}.txt", i % 25, i)), b"body");
    }

    index_once(&root, &db, &config);
    let full = rows(&db);
    assert_eq!(full.len(), 1500);

    // Start again and stop almost immediately, so the walk is cut short.
    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(15));
    service.stop_indexing().unwrap();
    drop(service);
    std::thread::sleep(Duration::from_millis(250));

    let after = rows(&db);
    assert_eq!(
        after.len(),
        1500,
        "an interrupted run must not delete the rows it never got to"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn a_wide_tree_indexes_every_file_exactly_once() {
    // Exercises the parallel walk's chunking and termination against a real
    // database, where a duplicate path would be a UNIQUE violation and a
    // dropped path would be a missing row.
    let root = tmp_dir("wide");
    let db_dir = tmp_dir("wide-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    let count = 900;
    for i in 0..count {
        touch(&root.join(format!("d{}/f{:04}.txt", i % 13, i)), b"body");
    }

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), count, "every file indexed exactly once");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), count, "and the second run is stable");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Like `index_once`, but over several roots at once — the per-root
/// pipeline path.
fn index_roots_once(roots: &[&Path], db: &Path, config: &Config) {
    if db.exists() {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
            .unwrap();
    }
    let service = IndexingService::new();
    service
        .start_indexing(
            roots.iter().map(|r| r.to_string_lossy().into_owned()).collect(),
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut done = false;
    while Instant::now() < deadline {
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if db.exists() {
            if let Ok(conn) = rusqlite::Connection::open(db) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(done, "indexing did not finish within the timeout");
    service.stop_indexing().unwrap();
}

#[test]
fn two_roots_walk_extract_and_clean_independently() {
    let root_a = tmp_dir("multi-a");
    let root_b = tmp_dir("multi-b");
    let db_dir = tmp_dir("multi-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    // Imbalanced roots so the round-robin writer sees a firehose and a
    // trickle in the same run.
    for i in 0..60 {
        touch(&root_a.join(format!("a{:03}.txt", i)), b"alpha corpus xylophone");
    }
    for i in 0..5 {
        touch(&root_b.join(format!("b{:03}.txt", i)), b"bravo corpus quagmire");
    }

    index_roots_once(&[&root_a, &root_b], &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 65, "both roots fully walked");
    let pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE content_state = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending, 0, "per-root extraction drained both roots");
    // Content from EACH root is searchable.
    for term in ["\"xylophone\"", "\"quagmire\""] {
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
                [term],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hits > 0, "content from both roots must be indexed ({})", term);
    }
    drop(conn);

    // Stale cleanup is global: deleting a file from the trickle root must
    // remove exactly that row on the next multi-root run.
    std::fs::remove_file(root_b.join("b000.txt")).unwrap();
    index_roots_once(&[&root_a, &root_b], &db, &config);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 64, "stale row swept across roots");

    std::fs::remove_dir_all(&root_a).ok();
    std::fs::remove_dir_all(&root_b).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

// ---------------------------------------------------------------------------
// Inline extraction: the walk finishes files whose head is the whole file.
//
// `hash_length` is what decides how much of a file the walk reads, so setting
// it to 0 leaves an empty head, nothing can be extracted inline, and the run
// degrades to the pure two-pass behaviour. That makes it the control against
// which the optimised path must produce an identical index.
// ---------------------------------------------------------------------------

/// Everything about a file's indexed content that a user can observe: its
/// state, the stored snippet body, and its property rows.
fn content_rows(db: &Path) -> Vec<(String, i64, Option<String>, Option<i64>, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT f.path, f.content_state, f.failure_msg, d.text_len,
                    COALESCE(GROUP_CONCAT(p.key || '=' || p.value, ','), '')
               FROM files f
               LEFT JOIN documents_text d ON d.file_id = f.id
               LEFT JOIN properties p ON p.file_id = f.id
              GROUP BY f.id
              ORDER BY f.path",
        )
        .unwrap();
    let out = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
}

/// The decompressed body stored for a file, if any.
fn stored_text(db: &Path, suffix: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT d.text_zstd FROM documents_text d
               JOIN files f ON f.id = d.file_id
              WHERE f.path LIKE '%' || ?1",
            [suffix],
            |r| r.get(0),
        )
        .ok();
    blob.map(|b| String::from_utf8(zstd::decode_all(&b[..]).unwrap()).unwrap())
}

/// A tree that exercises every branch of the inline decision at once.
fn seed_mixed_tree(root: &Path) {
    let big = "lorem ipsum dolor sit amet ".repeat(600); // ~16 KiB, past any head
    touch(&root.join("small.txt"), b"a small plaintext body with xylophone in it");
    touch(&root.join("large.txt"), big.as_bytes());
    touch(&root.join("empty.txt"), b"");
    // Invalid UTF-8 with a .txt extension: claimed by the plaintext extractor,
    // but not decodable, so it must be reported as a failure either way.
    touch(&root.join("bad.txt"), &[0x68, 0x69, 0xff, 0xfe, 0x00, 0x41]);
    // No extension `infer` or `mime_guess` recognises: no extractor claims it.
    touch(&root.join("blob.bin"), &[0x00, 0x01, 0x02, 0xfd, 0xfe, 0xff]);
    touch(&root.join("nested/deep/note.md"), b"# heading\n\nquagmire body text\n");
}

#[test]
fn inline_extraction_produces_an_identical_index_to_the_two_pass_path() {
    let root = tmp_dir("inline-equiv");
    let db_dir = tmp_dir("inline-equiv-db");
    seed_mixed_tree(&root);

    // Control: hash_length 0 => empty head => nothing can be inlined.
    let mut control = Config::default();
    control.processing.hash_length = 0;
    let db_control = db_dir.join("control.sqlite");
    index_once(&root, &db_control, &control);

    // Optimised: the default head covers every small file in the tree.
    let optimised = Config::default();
    let db_opt = db_dir.join("optimised.sqlite");
    index_once(&root, &db_opt, &optimised);

    assert_eq!(
        content_rows(&db_control),
        content_rows(&db_opt),
        "inlining during the walk must not change a single indexed byte"
    );

    // And the bodies themselves round-trip identically, not just their lengths.
    for f in ["small.txt", "large.txt", "note.md"] {
        assert_eq!(
            stored_text(&db_control, f),
            stored_text(&db_opt, f),
            "stored body differs for {}",
            f
        );
    }

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn the_head_boundary_decides_inlining_without_changing_the_result() {
    let root = tmp_dir("inline-boundary");
    let db_dir = tmp_dir("inline-boundary-db");

    // Exactly at the limit, and one byte past it.
    let mut config = Config::default();
    config.processing.hash_length = 64;
    let at = "x".repeat(64);
    let past = "y".repeat(65);
    touch(&root.join("at.txt"), at.as_bytes());
    touch(&root.join("past.txt"), past.as_bytes());

    let db = db_dir.join("index.sqlite");
    index_once(&root, &db, &config);

    // Both are fully extracted; the boundary only decides *which pass* did it.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE content_state != 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending, 0, "both sides of the boundary end up extracted");
    drop(conn);

    assert_eq!(stored_text(&db, "at.txt").as_deref(), Some(at.as_str()));
    assert_eq!(stored_text(&db, "past.txt").as_deref(), Some(past.as_str()));

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn undecodable_small_files_are_reported_as_failures_not_silently_skipped() {
    let root = tmp_dir("inline-badutf8");
    let db_dir = tmp_dir("inline-badutf8-db");
    let db = db_dir.join("index.sqlite");

    touch(&root.join("bad.txt"), &[0x68, 0x69, 0xff, 0xfe]);
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, msg): (i64, Option<String>) = conn
        .query_row(
            "SELECT content_state, failure_msg FROM files WHERE path LIKE '%bad.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // Inlining must not swallow the error: the walk declines to record it, so
    // the content pass still opens the file and stores a reason.
    assert_eq!(state, 2, "undecodable content is FAILED, not DONE or NA");
    assert!(
        msg.unwrap_or_default().contains("bad.txt"),
        "the failure names the file"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn an_empty_file_is_done_with_no_snippet_sidecar() {
    let root = tmp_dir("inline-empty");
    let db_dir = tmp_dir("inline-empty-db");
    let db = db_dir.join("index.sqlite");

    touch(&root.join("empty.txt"), b"");
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, sidecars): (i64, i64) = conn
        .query_row(
            "SELECT f.content_state, (SELECT COUNT(*) FROM documents_text d WHERE d.file_id = f.id)
               FROM files f WHERE f.path LIKE '%empty.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, 1, "an empty file is extracted, not failed");
    assert_eq!(sidecars, 0, "no zstd frame for an empty body");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn the_content_extension_filter_still_excludes_small_text_files() {
    let root = tmp_dir("inline-filter");
    let db_dir = tmp_dir("inline-filter-db");
    let db = db_dir.join("index.sqlite");

    let mut config = Config::default();
    config.indexing.content_extensions = vec!["md".into()];
    touch(&root.join("kept.md"), b"kept quagmire body");
    touch(&root.join("skipped.txt"), b"skipped xylophone body");
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let states: Vec<(String, i64)> = conn
        .prepare("SELECT path, content_state FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (path, state) in &states {
        if path.ends_with("kept.md") {
            assert_eq!(*state, 1, "an allowed extension is extracted");
        } else {
            assert_eq!(*state, 3, "a filtered extension is NA, never inlined");
        }
    }
    drop(conn);
    assert_eq!(stored_text(&db, "skipped.txt"), None, "no body stored for a filtered file");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn contentless_mode_still_indexes_inlined_files_without_storing_bodies() {
    let root = tmp_dir("inline-contentless");
    let db_dir = tmp_dir("inline-contentless-db");
    let db = db_dir.join("index.sqlite");

    let mut config = Config::default();
    config.processing.store_text_for_snippets = false;
    touch(&root.join("small.txt"), b"searchable xylophone body");
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let sidecars: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents_text", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sidecars, 0, "contentless mode stores no bodies");
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH '\"xylophone\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "an inlined file is still searchable in contentless mode");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}
