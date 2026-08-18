//! End-to-end phase-1 tests over a real tree and a real database.
//!
//! These cover the failure mode that unit tests structurally cannot: a full
//! run deletes index rows for every path it did not see, so any walk that
//! quietly reports less than it should destroys data. That damage is
//! invisible on a first index — `existing_files` is empty, so nothing is
//! stale — and only appears on the second run.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use quicksearch_core::config::Config;
use quicksearch_core::file_handling::{
    count_extract_scope, mark_oversize_pending_na, ExtractCursor, ExtractScope,
};
use quicksearch_core::indexing::{IndexingService, IndexingStatus, RootPhase};

mod common;
use common::{scratch_dir as tmp_dir, touch};

/// The removed `extract_scope_prepare`: the oversize sweep the writer still
/// does, then the count the content pass now does on its own connection.
fn extract_scope_prepare(
    conn_mutex: &Arc<Mutex<rusqlite::Connection>>,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<ExtractScope, String> {
    let conn = conn_mutex.lock().unwrap();
    mark_oversize_pending_na(&conn, cursor, config).unwrap();
    count_extract_scope(&conn, cursor, config)
}

/// Run one full index over `root` and wait for it to finish.
fn index_once(root: &Path, db: &Path, config: &Config) {
    common::IndexOnce {
        db,
        roots: vec![root.to_string_lossy().into_owned()],
        config,
        fresh_marker: true,
        encrypted: false,
    }
    .run()
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
    // Keep the run to phase 1 semantics we're asserting on; extraction is
    // covered elsewhere.
    Config::default()
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
    assert_eq!(
        first, second,
        "an unchanged tree must re-index to an identical set"
    );

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
        .map(|(p, _, _)| {
            Path::new(&p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        vec!["added.txt", "keep.txt"],
        "stale cleanup still works"
    );

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
fn a_stamped_run_has_finished_its_stale_cleanup() {
    // `last_full_index` is what the coordinator schedules the next periodic
    // reindex from. Stamping it for a run that was cut short suppresses
    // reindexing for the whole interval (24 h by default) — and the damage is
    // concrete: stale cleanup is skipped when the run is stopped, so rows for
    // files that no longer exist stay in the index and keep turning up in
    // search results until something else forces a rebuild.
    //
    // The hole this guards: the writer loop set `aborted` only at the *top* of
    // an iteration, while the "every root is Done" exit sits at the bottom and
    // breaks directly. A stop landing inside the pass — or inside stale cleanup
    // itself, which returns early and leaves rows behind — reached that bottom
    // break with `aborted` still false and stamped the run as complete.
    //
    // The assertion is one-sided on purpose, so timing can never make it fail
    // spuriously: a stamp *always* has to mean cleanup finished, whether the
    // stop landed inside the window or never landed at all.
    let root = tmp_dir("stop-stamp");
    let db_dir = tmp_dir("stop-stamp-db");
    let db = db_dir.join("index.sqlite");
    let mut config = test_config();
    // Nothing to extract, so a root goes Walking → Done in one pass and the
    // run's whole tail is the stale cleanup this test wants to interrupt.
    config.processing.maximum_text_file_size = 0;

    const FILES: usize = 8000;
    for i in 0..FILES {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 25, i)), b"body");
    }
    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), FILES);

    // Every file vanishes, so the next run has FILES stale rows to delete —
    // a tail long enough for a stop to land inside it.
    for i in 0..FILES {
        std::fs::remove_file(root.join(format!("d{}/f{:05}.txt", i % 25, i))).unwrap();
    }

    let marker = |db: &Path| -> Option<u64> {
        let conn = rusqlite::Connection::open(db).ok()?;
        quicksearch_core::db::repo::get_last_full_index(&conn)
    };

    for delay_ms in [2u64, 5, 10, 20, 35, 60, 100, 200] {
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
                .unwrap();
        }
        assert_eq!(marker(&db), None, "stamp cleared before the run");

        let service = IndexingService::new();
        service
            .start_indexing(
                vec![root.to_string_lossy().into_owned()],
                db.to_string_lossy().into_owned(),
                config.clone(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(delay_ms));
        service.stop_indexing().unwrap();
        drop(service);
        std::thread::sleep(Duration::from_millis(300));

        if marker(&db).is_some() {
            assert_eq!(
                rows(&db).len(),
                0,
                "delay {}ms: the run stamped itself complete but left stale rows behind",
                delay_ms
            );
            // Cleanup finished, so there is nothing left for later delays to
            // interrupt; the rest of the sweep would be vacuous.
            break;
        }
    }

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

#[test]
fn starting_a_run_claims_the_status_before_it_returns() {
    // The coordinator enforces the single-writer rule by polling
    // `get_status()`. That is only sound if the Running transition has already
    // happened when `start_indexing` returns — it used to be performed by the
    // service's command thread, *after* it joined the previous run's handle,
    // so a caller could see Idle and start writing to the database this run is
    // about to reopen (and possibly wipe).
    let root = tmp_dir("start-claims");
    let db_dir = tmp_dir("start-claims-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();
    touch(&root.join("a.txt"), b"body");

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    // No sleep, no poll: the very next observation must already show the run.
    // `Preparing` is what a claim looks like before the command thread has
    // even picked the start up — it is still joining the previous run — and
    // it holds the index exactly as `Running` does.
    assert!(
        matches!(service.get_status(), IndexingStatus::Preparing { .. }),
        "status must be claimed synchronously, got {:?}",
        service.get_status()
    );

    // And a second start is a reportable error rather than a silently
    // dropped command.
    let err = service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap_err();
    assert!(err.contains("already running"), "got: {}", err);

    service.stop_indexing().unwrap();
    drop(service);
    std::thread::sleep(Duration::from_millis(250));

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
            roots
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect(),
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
        touch(
            &root_a.join(format!("a{:03}.txt", i)),
            b"alpha corpus xylophone",
        );
    }
    for i in 0..5 {
        touch(
            &root_b.join(format!("b{:03}.txt", i)),
            b"bravo corpus quagmire",
        );
    }

    index_roots_once(&[&root_a, &root_b], &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 65, "both roots fully walked");
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = 0",
            [],
            |r| r.get(0),
        )
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
        assert!(
            hits > 0,
            "content from both roots must be indexed ({})",
            term
        );
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
// Reconciliation without a global path set.
//
// Classification and stale detection are per-directory: a worker diffs one
// directory's listing against that directory's index rows. These cover the
// cases that arrangement cannot see from inside a single directory read.
// ---------------------------------------------------------------------------

/// A directory deleted wholesale is never read, so per-directory
/// reconciliation never runs for it. Only the sweep over stored parents finds
/// the rows underneath.
#[test]
fn a_deleted_directory_takes_its_whole_subtree_out_of_the_index() {
    let root = tmp_dir("gone-dir");
    let db_dir = tmp_dir("gone-dir-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    touch(&root.join("keep.txt"), b"stays");
    touch(&root.join("doomed/a.txt"), b"goes");
    touch(&root.join("doomed/b.txt"), b"goes");
    // Nested, so the sweep has to reach a parent two levels below the root.
    touch(&root.join("doomed/deeper/c.txt"), b"goes too");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 4, "all four indexed");

    std::fs::remove_dir_all(root.join("doomed")).unwrap();
    index_once(&root, &db, &config);

    let names: Vec<String> = rows(&db)
        .into_iter()
        .map(|(p, _, _)| {
            Path::new(&p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["keep.txt"], "the whole subtree is swept");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// A symlink target whose own directory the walk never enters.
///
/// Two flavours, and only one of them exercises the alias exemption:
///
/// - A target *outside* every root is already safe, because the sweep only
///   scans parents within a root's path range.
/// - A target inside the root but under a *pruned* directory — hidden here —
///   has a parent that is in range and legitimately absent from `seen_dirs`.
///   Nothing but the record that the file itself was seen distinguishes it
///   from a row whose directory was deleted.
#[test]
#[cfg(unix)]
fn a_symlink_target_in_an_unwalked_directory_survives_reindexing() {
    let root = tmp_dir("alias-root");
    let outside = tmp_dir("alias-outside");
    let db_dir = tmp_dir("alias-db");
    let db = db_dir.join("index.sqlite");
    // Aliases only exist when links are followed; with the default (off) a
    // symlink is not resolved at all, which the tail of this test checks.
    let mut config = test_config();
    config.indexing.follow_symlinks = true;

    touch(&root.join("normal.txt"), b"inside the root");

    // In range, but under a hidden directory the walk prunes.
    let hidden_target = root.join(".pruned/inner.txt");
    touch(&hidden_target, b"only reachable through the link");
    std::os::unix::fs::symlink(&hidden_target, root.join("hidden_link.txt")).unwrap();

    // Out of range entirely.
    let outer_target = outside.join("target.txt");
    touch(&outer_target, b"outside the root entirely");
    std::os::unix::fs::symlink(&outer_target, root.join("outside_link.txt")).unwrap();

    index_once(&root, &db, &config);
    let first = rows(&db);
    assert_eq!(first.len(), 3, "both targets indexed under their own paths");
    assert!(
        first
            .iter()
            .any(|(p, _, _)| p.ends_with(".pruned/inner.txt")),
        "the pruned-directory target is stored under its canonical path"
    );

    // The second run is where a sweep keyed only on "was this parent
    // visited?" deletes the pruned-directory row.
    index_once(&root, &db, &config);
    assert_eq!(rows(&db), first, "an aliased row must survive a re-index");

    // And the other half of the setting: with links off, neither target is
    // indexed — including the one outside the root, which the user never asked
    // us to look at. This is also what keeps the full run in agreement with
    // `filtered_walk`, which the watcher uses and which follows neither kind.
    let db2 = db_dir.join("links-off.sqlite");
    index_once(&root, &db2, &test_config());
    let off: Vec<String> = rows(&db2).into_iter().map(|(p, _, _)| p).collect();
    assert_eq!(off.len(), 1, "only the ordinary file: {:?}", off);
    assert!(off[0].ends_with("normal.txt"));

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// A file reached only through a symlink must still be *updated* when it
/// changes. Classifying it against the linking directory's rows would miss,
/// read as Insert, and `INSERT OR IGNORE` would then silently do nothing.
#[test]
#[cfg(unix)]
fn a_modified_symlink_target_is_updated_not_silently_ignored() {
    let root = tmp_dir("alias-mod-root");
    let outside = tmp_dir("alias-mod-outside");
    let db_dir = tmp_dir("alias-mod-db");
    let db = db_dir.join("index.sqlite");
    let mut config = test_config();
    config.indexing.follow_symlinks = true;

    let target = outside.join("target.txt");
    touch(&target, b"first body");
    std::os::unix::fs::symlink(&target, root.join("link.txt")).unwrap();

    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 1);

    std::fs::write(&target, b"second body, quite different").unwrap();
    filetime_set(&target, SystemTime::now() + Duration::from_secs(120));

    index_once(&root, &db, &config);
    let after = rows(&db);
    assert_eq!(after.len(), 1, "still exactly one row");
    assert_eq!(after[0].0, before[0].0, "same path");
    assert_ne!(
        after[0].1, before[0].1,
        "mtime was refreshed, so it was re-read"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Overlapping roots reach the same files twice. The writer's digest set is
/// the only thing left that collapses those visits.
#[test]
fn overlapping_roots_index_each_file_exactly_once() {
    let outer = tmp_dir("overlap-outer");
    let db_dir = tmp_dir("overlap-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    let inner = outer.join("inner");
    touch(&outer.join("top.txt"), b"in the outer root only");
    touch(&inner.join("shared.txt"), b"reachable from both roots");
    touch(&inner.join("also.txt"), b"likewise");

    index_roots_once(&[&outer, &inner], &db, &config);

    let all = rows(&db);
    assert_eq!(all.len(), 3, "three files, however many roots reach them");
    let shared: Vec<&(String, i64, i64)> = all
        .iter()
        .filter(|(p, _, _)| p.ends_with("shared.txt"))
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "the doubly-reachable file has exactly one row"
    );

    // And the overlap must not make anything look stale on a second pass.
    index_roots_once(&[&outer, &inner], &db, &config);
    assert_eq!(rows(&db), all, "a second overlapping run changes nothing");

    std::fs::remove_dir_all(&outer).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// A directory that becomes unreadable between runs must not read as empty.
/// Per-directory reconciliation returns before diffing when the read fails,
/// and the sweep skips parents beneath it.
#[test]
#[cfg(unix)]
fn a_directory_that_becomes_unreadable_deletes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_dir("locked-later");
    let db_dir = tmp_dir("locked-later-db");
    let db = db_dir.join("index.sqlite");
    let config = test_config();

    touch(&root.join("open.txt"), b"always readable");
    let vault = root.join("vault");
    touch(&vault.join("secret.txt"), b"readable for now");
    touch(&vault.join("deeper/also.txt"), b"and this one");

    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 3, "all three indexed while readable");

    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o000)).unwrap();
    index_once(&root, &db, &config);
    let after = rows(&db);
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o755)).ok();

    assert_eq!(after, before, "an unreadable directory is not an empty one");

    std::fs::remove_dir_all(&root).ok();
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
/// state, its failure reason, and the compressed size of its stored body.
type ContentRow = (String, i64, Option<String>, Option<i64>);

fn content_rows(db: &Path) -> Vec<ContentRow> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT f.path, f.content_state, ff.reason, LENGTH(d.text_zstd)
               FROM files f
               LEFT JOIN documents_text d ON d.file_id = f.id
               LEFT JOIN failed_files ff ON ff.file_id = f.id
              ORDER BY f.path",
        )
        .unwrap();
    let out = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
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
    touch(
        &root.join("small.txt"),
        b"a small plaintext body with xylophone in it",
    );
    touch(&root.join("large.txt"), big.as_bytes());
    touch(&root.join("empty.txt"), b"");
    // Binary bytes with a .txt extension: claimed by the plaintext
    // extractor, but the NUL fails the binary guard (and the FF FE pair is
    // not at offset 0, so it is no BOM), so it must be reported as a
    // failure either way.
    touch(&root.join("bad.txt"), &[0x68, 0x69, 0xff, 0xfe, 0x00, 0x41]);
    // No extension table, magic, or text sniff has an answer for NUL soup:
    // no MIME, no extractor.
    touch(
        &root.join("blob.bin"),
        &[0x00, 0x01, 0x02, 0xfd, 0xfe, 0xff],
    );
    touch(
        &root.join("nested/deep/note.md"),
        b"# heading\n\nquagmire body text\n",
    );
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
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state != 1",
            [],
            |r| r.get(0),
        )
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

    // The NUL keeps this undecodable: without it these bytes would now
    // decode as windows-1252 and the test would assert nothing.
    touch(&root.join("bad.txt"), &[0x68, 0x00, 0x69, 0xff]);
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, msg): (i64, Option<String>) = conn
        .query_row(
            "SELECT f.content_state, ff.reason FROM files f \
               LEFT JOIN failed_files ff ON ff.file_id = f.id \
              WHERE f.path LIKE '%bad.txt'",
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

/// A `.doc` that is not a readable OLE2 compound file — a truncated download,
/// or something misnamed — records a failure with a reason.
///
/// This is the end-to-end shape of the legacy-Office support: the walk types
/// the file from its extension, the office extractor claims `application/
/// msword`, and the OLE2 reader either produces text or says why it could not.
/// Until that reader existed, every `.doc` took the third path instead —
/// `DONE` with empty text — which reads as "indexed, contains nothing" and is
/// indistinguishable from a genuinely empty document.
#[test]
fn an_unreadable_legacy_office_file_fails_with_a_reason() {
    let root = tmp_dir("legacy-doc");
    let db_dir = tmp_dir("legacy-doc-db");
    let db = db_dir.join("index.sqlite");

    touch(
        &root.join("broken.doc"),
        b"D0CF11E0 this is not really a compound file",
    );
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, msg): (i64, Option<String>) = conn
        .query_row(
            "SELECT f.content_state, ff.reason FROM files f \
               LEFT JOIN failed_files ff ON ff.file_id = f.id \
              WHERE f.path LIKE '%broken.doc'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        state, 2,
        "an unreadable .doc is FAILED, not DONE-with-no-text"
    );
    let msg = msg.unwrap_or_default();
    assert!(msg.contains("broken.doc"), "names the file: {msg}");
    assert!(msg.contains("compound file"), "says what went wrong: {msg}");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// The text sniff end-to-end: extensionless text files (README, Makefile,
/// go.sum) are content-indexed off their head bytes, while an extensionless
/// binary blob stays NA.
#[test]
fn extensionless_text_files_are_indexed() {
    let root = tmp_dir("extless");
    let db_dir = tmp_dir("extless-db");
    let db = db_dir.join("index.sqlite");

    touch(
        &root.join("README"),
        b"QuickSearch indexes zanzibar contents.\n",
    );
    touch(&root.join("Makefile"), b"all:\n\tcargo build --release\n");
    touch(&root.join("go.sum"), b"example.com/x v1.0.0 h1:abcdef=\n");
    touch(&root.join("blob"), &[0x00, 0x01, 0xfe, 0xff]);
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let state_of = |name: &str| -> i64 {
        conn.query_row(
            "SELECT content_state FROM files WHERE path LIKE '%' || ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    for name in ["README", "Makefile", "go.sum"] {
        assert_eq!(state_of(name), 1, "{} should be content-indexed", name);
    }
    assert_eq!(state_of("blob"), 3, "binary blob stays not-applicable");
    drop(conn);

    assert_eq!(
        stored_text(&db, "README").as_deref(),
        Some("QuickSearch indexes zanzibar contents.\n"),
        "the stored body round-trips"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Charset decoding end-to-end: UTF-16LE files (the shape of a Windows
/// registry export) and legacy single-byte text are stored as UTF-8 —
/// `stored_text` decodes the zstd sidecar with `String::from_utf8`, so a
/// `Some` result *is* the storage-is-UTF-8 assertion.
#[test]
fn utf16_files_are_stored_as_utf8() {
    let root = tmp_dir("charset");
    let db_dir = tmp_dir("charset-db");
    let db = db_dir.join("index.sqlite");

    let reg_src =
        "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Xylograph]\r\n";
    let mut reg_body = vec![0xFF, 0xFE];
    reg_body.extend(reg_src.encode_utf16().flat_map(|u| u.to_le_bytes()));
    touch(&root.join("export.reg"), &reg_body);

    // The same encoding behind no extension at all: BOM first, sniff after.
    let mut extless = vec![0xFF, 0xFE];
    extless.extend(
        "utf16 notes about quokkas"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes()),
    );
    touch(&root.join("NOTES16"), &extless);

    touch(
        &root.join("legacy.txt"),
        b"un caf\xe9 tr\xe8s agr\xe9able pr\xe8s du mus\xe9e",
    );
    index_once(&root, &db, &Config::default());

    assert_eq!(stored_text(&db, "export.reg").as_deref(), Some(reg_src));
    assert_eq!(
        stored_text(&db, "NOTES16").as_deref(),
        Some("utf16 notes about quokkas")
    );
    assert_eq!(
        stored_text(&db, "legacy.txt").as_deref(),
        Some("un café très agréable près du musée")
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// RTF end-to-end through both extraction paths: a small file the walk
/// finishes inline, and one past `hash_length` that the content pass opens.
/// Stored text is the parsed prose, not RTF control words.
#[test]
fn rtf_files_are_extracted() {
    let root = tmp_dir("rtf");
    let db_dir = tmp_dir("rtf-db");
    let db = db_dir.join("index.sqlite");

    touch(
        &root.join("small.rtf"),
        br"{\rtf1\ansi Meeting notes about the pangolin budget.}",
    );
    let big_body = format!(
        r"{{\rtf1\ansi {}}}",
        r"paragraphs about the pangolin budget \par ".repeat(400)
    );
    assert!(big_body.len() > 8192, "must exceed the default head");
    touch(&root.join("big.rtf"), big_body.as_bytes());
    index_once(&root, &db, &Config::default());

    for name in ["small.rtf", "big.rtf"] {
        let text = stored_text(&db, name).unwrap_or_else(|| panic!("{} has no stored text", name));
        assert!(
            text.contains("pangolin budget"),
            "{}: {:?}",
            name,
            &text[..text.len().min(80)]
        );
        assert!(!text.contains(r"\rtf"), "{} stored control words", name);
    }

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// End-to-end version of the fix: the extraction denominator the manage-index
/// tab renders is `extract_total`, and it must count files that need text —
/// not every indexed file. Asserted through a real `IndexingService` run so it
/// covers the walk, the batch writers and `extract_scope_prepare` together.
#[test]
fn the_extraction_denominator_counts_only_files_that_need_text() {
    let root = tmp_dir("denominator");
    let db_dir = tmp_dir("denominator-db");
    let db = db_dir.join("index.sqlite");

    // Three files an extractor claims, seven it never will. `big.txt` is the
    // interesting one: larger than `hash_length`, so the walk cannot finish it
    // inline and it is the only row the content pass actually opens. The
    // unclaimed seven get NUL-bearing bodies so neither the extension tables
    // nor the text sniff have anything to say about them.
    for name in ["a.txt", "b.json"] {
        touch(&root.join(name), b"body bytes with no magic");
    }
    touch(&root.join("big.txt"), &vec![b'z'; 32 * 1024]);
    for name in ["d.mp4", "e.zip", "f.bin", "g.exe", "h.iso", "i.so", "j"] {
        touch(&root.join(name), b"\x00\x01body bytes\x00");
    }

    let config = Config::default();
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let count = |state: i64| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = ?1",
            [state],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(0), 0, "a finished run leaves nothing pending");
    assert_eq!(count(1), 3, "the claimed files have text");
    assert_eq!(count(3), 7, "the rest are NA, and were NA from the walk on");
    drop(conn);

    // The exact call `indexing.rs` makes to fill `RootProgress::extract_total`,
    // run against the index the full pass just produced. Asserted here rather
    // than by sampling the live status, which cannot be observed reliably: a
    // ten-file tree finishes between two polls.
    let conn = Arc::new(Mutex::new(
        // Writable: the scope call's first act is the idempotent oversize sweep.
        quicksearch_core::db::open_existing(db.to_str().unwrap(), true).unwrap(),
    ));
    let cursor = ExtractCursor::for_root(root.to_str().unwrap());
    let scope = extract_scope_prepare(&conn, &cursor, &config).unwrap();
    assert_eq!(
        (scope.pending, scope.already_done),
        (0, 3),
        "extract_total is the searchable set, not the file count"
    );
    // Which is what the row renders: "3 / 3" on an unchanged re-run. Before
    // this was decided at walk time it read "10 / 10", seven of them files
    // with nothing to extract.
    assert_eq!(scope.pending + scope.already_done, 3);
    drop(conn);

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
    assert_eq!(
        stored_text(&db, "skipped.txt"),
        None,
        "no body stored for a filtered file"
    );

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
    assert_eq!(
        hits, 1,
        "an inlined file is still searchable in contentless mode"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// What one watch of a heavy/light overlap saw; see [`observe_overlap`].
struct Overlap {
    /// Light files drained and heavy rows stored, across the window in which
    /// the heavy root extracted while the light root walked.
    light_drained: usize,
    heavy_stored: usize,
    /// The heavy root's `extract_total` and pool size, for the fixture guards.
    heavy_pending: usize,
    heavy_pool: usize,
    /// Distinct published states seen inside the window, counted by the
    /// counters changing rather than by polls — the watcher polls far faster
    /// than the writer publishes, so poll count would say nothing. One is a
    /// window that was raced past, not a window that was measured, and the
    /// deltas either side of it are meaningless.
    samples: usize,
}

/// Watch a two-root run until the heavy root has finished extracting and
/// report how the two counters moved while both were in flight, then stop
/// the run. Removing the fixture is the caller's.
///
/// Deltas across the overlap, never durations. Sparse samples cost only the
/// window's edges, and they trim both counters together. Panics if the window
/// never opened — a fixture that does not exercise the case proves nothing.
///
/// What can only be seen here is what the writer *published*, once a round
/// (`publish_status` in `indexing/pipeline.rs`). A caller whose heavy root
/// finishes its content pass inside one round leaves no snapshot holding both
/// phases at once and lands on the panic below however healthy the run was —
/// which is why the callers set `writer_turn_slice_ms` small enough that a
/// round is far shorter than the pass, and why the panic prints what the
/// phases actually did.
fn observe_overlap(service: &IndexingService, heavy_tag: &str, light_tag: &str) -> Overlap {
    let mut opened: Option<(usize, usize)> = None; // (light.walked, heavy.extracted)
    let mut last = (0usize, 0usize);
    let mut heavy_pending = 0usize;
    let mut heavy_pool = 0usize;
    let mut samples = 0usize;
    let mut counted: Option<(usize, usize)> = None;
    // Every (heavy, light) phase pair published, in order and without repeats.
    // Only the diagnosis uses it: both phases are monotone, so this is at most
    // a handful of entries and it says exactly which phase went missing.
    let mut phases: Vec<(RootPhase, RootPhase)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let mut in_window = false;
        match service.get_status() {
            IndexingStatus::Running { roots, .. } => {
                let heavy_p = roots.iter().find(|r| r.root.contains(heavy_tag));
                let light_p = roots.iter().find(|r| r.root.contains(light_tag));
                if let (Some(h), Some(l)) = (heavy_p, light_p) {
                    if phases.last() != Some(&(h.phase, l.phase)) {
                        phases.push((h.phase, l.phase));
                    }
                    // The light root's *walk* is what used to be starved, so the
                    // window closes with it — past that there is no drain left
                    // to observe, and `walked` is the only counter in play.
                    in_window = h.phase == RootPhase::Extracting && l.phase == RootPhase::Walking;
                    if in_window {
                        last = (l.walked, h.extracted);
                        opened.get_or_insert(last);
                        // A new publication, not a new poll: the counters only
                        // move when the writer has published a fresh round.
                        if counted != Some(last) {
                            counted = Some(last);
                            samples += 1;
                        }
                        if let Some(total) = h.extract_total {
                            heavy_pending = total;
                        }
                        heavy_pool = h.total_workers;
                    }
                }
            }
            // The run is claimed but has not reached its walk yet; there is
            // nothing to sample, and breaking here would end the watch before
            // the run it is watching had started.
            IndexingStatus::Preparing { .. } => {}
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => break,
        }
        // Both phases are monotone, so a closed window will not reopen.
        if opened.is_some() && !in_window {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    service.stop_indexing().unwrap();

    let Some((light_open, heavy_open)) = opened else {
        panic!(
            "never observed the heavy root extracting while the light root walked. \
             Published (heavy, light) phases: {:?}. An empty list means neither \
             root matched the tags {:?}/{:?}; a list with no heavy Extracting in \
             it means the heavy root's content pass began and ended between two \
             status publications, so lower `writer_turn_slice_ms` until a writer \
             round is shorter than that pass.",
            phases, heavy_tag, light_tag
        );
    };
    Overlap {
        light_drained: last.0 - light_open,
        heavy_stored: last.1 - heavy_open,
        heavy_pending,
        heavy_pool,
        samples,
    }
}

/// A slow root must not stall the others.
///
/// This is the complaint stated directly: one root doing heavy extraction used
/// to occupy the single writer thread — and the database connection — for a
/// whole batch of files at a time, during which no other root's walk was
/// drained at all. Their walker threads filled their channels and blocked.
///
/// So the assertion is about *stalls*, not throughput. Throughput would be the
/// wrong measure: writing is serial by construction (one SQLite connection),
/// so on a local disk the writer, not extraction, is the bottleneck and a
/// wall-clock comparison would mostly measure the machine.
///
/// A stall is therefore counted in *work*, not in milliseconds: while the heavy
/// root extracts, how many files the light root's walk was drained of, against
/// how many rows the heavy root's extraction landed. Both counters are advanced
/// by the same writer loop, each root's turn bounded by one slice
/// (`service_walking`, `service_extracting` in `indexing/pipeline.rs`), so their
/// ratio *is* the interleaving.
///
/// - Serialised — the regression — the writer reads the heavy batch itself and
///   drains nobody meanwhile. Whatever shape that takes it obeys
///   `light < heavy + quantum`: one quantum of each per round is the most a
///   single thread taking turns can manage. Its own time budget says the same
///   from the other side, since time spent reading is time not spent inserting.
/// - As built, extraction is off on the root's own pool and the writer's turn
///   for the heavy root is a store and nothing more, so the light root is
///   drained at the writer's full rate throughout — on this fixture several
///   times the bound.
///
/// Counting rather than timing is what makes the verdict the same on a loaded
/// CI runner and an idle workstation. Every way a host can be slow — a
/// preempted writer, a checkpoint, a long round — freezes *both* counters, and
/// cancels. The wall-clock figure this replaced did not cancel: the same
/// correct behaviour measured ~20 ms here and 188 ms on the CI runner, which is
/// *more* than the 130 ms the broken design measured here. At that point CI was
/// overriding the budget six-fold and the check had stopped telling the two
/// designs apart. A bound that has to be calibrated per host is not an
/// assertion.
#[test]
fn a_heavy_root_does_not_stall_a_light_one() {
    // The writer's round-robin quantum. Set here rather than inherited from the
    // default 500 because the bound below is arithmetic in it, and because a
    // 500-file round is a coarse enough publish interval to look like a stall
    // on a slow host all by itself.
    const QUANTUM: usize = 16;
    // HEAVY: few files, each big enough that reading one is real work, with a
    // small `maximum_text_size` so the cost lands in extraction rather than in
    // the writer's tokenising. Few and large rather than many and small, and
    // more so than it first looks. What the light root manages to drain
    // depends only on the heavy root's *total* bytes — that is what sets how
    // long the pass runs — while the bound it must beat, `3 × (rows +
    // quantum)`, grows with the row count. So for a fixture of a given size on
    // disk, every file it is split into costs margin. These are as large as
    // `maximum_text_file_size` below allows and there are as few of them as
    // the half-the-rows guard tolerates.
    const HEAVY_FILES: usize = 12;
    // LIGHT: a wide tree of tiny files, so its walk outlasts the heavy root's
    // extraction and its counter moves finely. Each is inlined by its walk
    // worker, so this root has no extraction phase of its own to confuse the
    // window with. Sized for a walk the writer never holds up: at 6000 it was
    // over before half the heavy rows had landed.
    const LIGHT_FILES: usize = 16_000;
    // Light files drained per (heavy row + quantum). Three times a bound the
    // serialised design provably cannot reach, and roughly a ninth of what the
    // built one reaches here.
    const MIN_INTERLEAVE: usize = 3;

    let heavy = tmp_dir("stall-heavy");
    // 36 bytes a repeat, so just under the 8 MiB that `maximum_text_file_size`
    // has to stay above; twelve of them is the same ~96 MB of fixture this
    // always built, redistributed into files that discriminate better.
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(233_000)
        .into_bytes();
    for i in 0..HEAVY_FILES {
        touch(&heavy.join(format!("d{}/big{:04}.txt", i % 4, i)), &body);
    }
    let light = tmp_dir("stall-light");
    for i in 0..LIGHT_FILES {
        touch(&light.join(format!("d{}/f{:05}.txt", i % 60, i)), b"x");
    }

    let db_dir = tmp_dir("stall-db");
    let db = db_dir.join("index.sqlite");
    let roots = vec![
        heavy.to_string_lossy().into_owned(),
        light.to_string_lossy().into_owned(),
    ];

    let mut config = test_config();
    config.processing.maximum_text_size = 1024;
    // Above the heavy files, or `mark_oversize_pending_na` writes them off as
    // N/A before the pass starts and there is no extraction phase at all.
    config.processing.maximum_text_file_size = 16 * 1024 * 1024;
    config.processing.batch_size = QUANTUM;
    // The window is only as long as one worker takes to read the heavy root —
    // a tenth of a second or so, and on a host whose /tmp is a tmpfs rather
    // less. Published once a round, the default 100 ms slice makes a round of
    // the same order as the whole phase, and the watcher then sees the heavy
    // root go Walking → Done having never once been published as Extracting.
    // At 2 ms the round is bounded by the work in it, so the pass spans tens
    // of publications on any host. This is the fix for a CI failure that was
    // pure sampling: nothing about the run was unhealthy.
    config.processing.writer_turn_slice_ms = 2;
    // One extraction thread for the heavy root, so its pass costs about what
    // the broken design's inline read would and the two differ only in *which*
    // thread pays for it. `root_workers` is keyed by the `indexing_paths`
    // spelling; both sides canonicalize before matching.
    config.paths.indexing_paths = roots.clone();
    config.indexing.root_workers.insert(roots[0].clone(), 1);
    // The default WAL cap is far above anything this run writes, so no forced
    // checkpoint lands inside the window. That stops being true if the fixture
    // ever grows by an order of magnitude.

    let service = IndexingService::new();
    service
        .start_indexing(roots, db.to_string_lossy().into_owned(), config.clone())
        .unwrap();
    let seen = observe_overlap(&service, "stall-heavy", "stall-light");
    drop(service);

    // Before the assertions, unlike the rest of this file: those tests keep
    // their trees because a failing test's tree is the evidence, but this
    // fixture is generated and identical every run, and its evidence is the two
    // counters printed below. Leaving 92 MB of it in a RAM-backed /tmp behind a
    // failure is itself a reason for the next run to fail.
    std::fs::remove_dir_all(&heavy).ok();
    std::fs::remove_dir_all(&light).ok();
    std::fs::remove_dir_all(&db_dir).ok();

    // The fixture is as configured. Each of these silently costs a factor of
    // the margin below if it stops holding, so they are checked before the
    // ratio is read as a verdict on the design.
    assert!(
        seen.samples >= 8,
        "the overlap was published {} time(s); the deltas either side of a window \
         that thin are noise, not a measurement. A writer round has to be much \
         shorter than the heavy root's content pass — lower writer_turn_slice_ms \
         (currently {} ms) or give the heavy root more bytes to read",
        seen.samples,
        config.processing.writer_turn_slice_ms
    );
    assert_eq!(
        seen.heavy_pool, 1,
        "the heavy root must extract on the single worker root_workers asked for; \
         with the default four its pass is four times shorter and so is the margin"
    );
    assert_eq!(
        seen.heavy_pending, HEAVY_FILES,
        "every heavy file must reach the content pass; one inlined by its walk \
         worker never produces an extraction phase to overlap with"
    );
    assert!(
        seen.heavy_stored * 2 >= HEAVY_FILES,
        "only {} of {} heavy rows landed inside the observed window; the sample \
         did not cover the pass",
        seen.heavy_stored,
        HEAVY_FILES
    );

    eprintln!(
        "light files drained while the heavy root extracted: {} against {} heavy \
         rows (quantum {}, {} publications) — {}x the {}x required; the \
         serialised design cannot exceed 1x",
        seen.light_drained,
        seen.heavy_stored,
        QUANTUM,
        seen.samples,
        seen.light_drained / (seen.heavy_stored + QUANTUM),
        MIN_INTERLEAVE
    );
    assert!(
        seen.light_drained >= MIN_INTERLEAVE * (seen.heavy_stored + QUANTUM),
        "the light root was drained of only {} files while the heavy root landed \
         {} rows; one quantum of each per round is all a writer that extracts \
         inline can manage, so anything near {} means the extraction is back on \
         the writer thread",
        seen.light_drained,
        seen.heavy_stored,
        seen.heavy_stored + QUANTUM
    );
}

/// The sibling of [`a_heavy_root_does_not_stall_a_light_one`] for the cost that
/// test deliberately keeps small: the writer's own tokenising.
///
/// There the heavy files are expensive to *read* and cheap to *write*
/// (`maximum_text_size = 1024`), so it never exercised the writer. Here each
/// heavy row carries the default 256 KiB of text and its FTS5 trigram insert is
/// the expensive step — and it runs on the writer thread, inside the
/// transaction, where nothing can take it off. Four workers keep the ready
/// channel full, so what one turn finds waiting is a whole channel of them.
///
/// Before turns had a slice, an extraction turn wrote everything it found —
/// half a second to two seconds of tokenising — and the light root's walk got
/// one quantum in between: the ratio below came in under one. With turns
/// bounded by `writer_turn_slice_ms` and walks served first, the light root
/// drains at
/// its own rate while the heavy root lands a row or two per round.
#[test]
fn a_heavy_root_does_not_stall_a_light_one_at_the_writer() {
    const QUANTUM: usize = 16;
    // Over the walk's inline threshold, and enough that the stored text is the
    // full `maximum_text_size` (256 KiB) — the tokenising is what is measured.
    const HEAVY_FILES: usize = 32;
    // Wider than the sibling's: with the walk no longer waiting on the writer
    // it drains so fast that 6000 files were gone before half the heavy rows
    // had landed, and the window closed on a sample too short to trust.
    const LIGHT_FILES: usize = 16_000;
    // As in the sibling: three times a bound the unsliced writer cannot reach.
    const MIN_INTERLEAVE: usize = 3;

    let heavy = tmp_dir("stall-writer-heavy");
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(9_000)
        .into_bytes();
    for i in 0..HEAVY_FILES {
        touch(&heavy.join(format!("d{}/big{:04}.txt", i % 8, i)), &body);
    }
    let light = tmp_dir("stall-writer-light");
    for i in 0..LIGHT_FILES {
        touch(&light.join(format!("d{}/f{:05}.txt", i % 60, i)), b"x");
    }

    let db_dir = tmp_dir("stall-writer-db");
    let db = db_dir.join("index.sqlite");
    let roots = vec![
        heavy.to_string_lossy().into_owned(),
        light.to_string_lossy().into_owned(),
    ];

    let mut config = test_config();
    config.processing.batch_size = QUANTUM;
    // As in the sibling, and for the same reason: the status is published once
    // a writer round, so a round has to be short against the phase being
    // watched or the window is a race rather than a measurement. This one's
    // pass is long — the writer tokenises 256 KiB a row — but nothing about
    // the fixture guarantees that on a host whose FTS5 is faster than this
    // one's, and it costs nothing to not depend on it.
    config.processing.writer_turn_slice_ms = 2;
    config.paths.indexing_paths = roots.clone();
    // Four readers, so the heavy rows reach the writer faster than it can
    // tokenise them and the ready channel is full when its turn comes.
    config.indexing.root_workers.insert(roots[0].clone(), 4);

    let service = IndexingService::new();
    service
        .start_indexing(roots, db.to_string_lossy().into_owned(), config.clone())
        .unwrap();
    let seen = observe_overlap(&service, "stall-writer-heavy", "stall-writer-light");
    drop(service);

    std::fs::remove_dir_all(&heavy).ok();
    std::fs::remove_dir_all(&light).ok();
    std::fs::remove_dir_all(&db_dir).ok();

    assert!(
        seen.samples >= 8,
        "the overlap was published {} time(s); the deltas either side of a window \
         that thin are noise, not a measurement (writer_turn_slice_ms {})",
        seen.samples,
        config.processing.writer_turn_slice_ms
    );
    assert_eq!(
        seen.heavy_pool, 4,
        "the heavy root must extract on four workers"
    );
    assert_eq!(
        seen.heavy_pending, HEAVY_FILES,
        "every heavy file must reach the content pass"
    );
    // A quarter, not the sibling's half: the light walk now outruns the heavy
    // pass by design, and a window over eight 256 KiB rows is evidence enough
    // that the writer yielded between them.
    assert!(
        seen.heavy_stored * 4 >= HEAVY_FILES,
        "only {} of {} heavy rows landed inside the observed window; the sample \
         did not cover the pass",
        seen.heavy_stored,
        HEAVY_FILES
    );

    eprintln!(
        "light files drained while the heavy root tokenised: {} against {} heavy \
         rows (quantum {}, {} publications) — {}x the {}x required",
        seen.light_drained,
        seen.heavy_stored,
        QUANTUM,
        seen.samples,
        seen.light_drained / (seen.heavy_stored + QUANTUM),
        MIN_INTERLEAVE
    );
    assert!(
        seen.light_drained >= MIN_INTERLEAVE * (seen.heavy_stored + QUANTUM),
        "the light root was drained of only {} files while the heavy root landed \
         {} rows; an extraction turn is writing to the end of its batch again \
         instead of yielding at its slice",
        seen.light_drained,
        seen.heavy_stored
    );
}

/// The write-ahead log must not grow for the length of a run.
///
/// SQLite's autocheckpoint copies committed frames into the index but can only
/// *reset* the log at an instant no reader holds a read mark — a lock it tries
/// once, without retrying. A run keeps a reader per root querying continuously,
/// so that instant does not come and the log appends until the run ends: the
/// case that prompted this was a 12.5 GiB index carrying a 21.6 GiB log.
///
/// So the assertion is about the *peak while running*. It has to be sampled
/// in flight — `stop_indexing` and the post-run maintenance both truncate the
/// log on the way out, so a reading taken afterwards proves nothing about what
/// happened during.
#[test]
fn the_wal_stays_bounded_during_a_run() {
    let root = tmp_dir("wal-bound");
    // Wide and text-heavy: every file lands in the FTS index, which is what
    // actually fills the log.
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(200)
        .into_bytes();
    for i in 0..4000 {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 40, i)), &body);
    }

    let db_dir = tmp_dir("wal-bound-db");
    let db = db_dir.join("index.sqlite");
    let wal = db_dir.join("index.sqlite-wal");
    let mut config = test_config();
    // The floor `MINIMUM_WAL_SIZE` clamps to, so the cap is exercised many
    // times over a fixture this size rather than once at the very end.
    config.processing.maximum_wal_size = 16 * 1024 * 1024;

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    let mut peak = 0u64;
    let mut checkpointed = false;
    let mut last = 0u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        peak = peak.max(len);
        // A drop in length is a checkpoint that ran mid-run; without one the
        // bound below could be met simply by the fixture being too small.
        if len + 1024 * 1024 < last {
            checkpointed = true;
        }
        last = len;
        match service.get_status() {
            // Preparing included: the run is claimed but has not opened the
            // database yet, so there is no log to watch and nothing to stop
            // watching for either.
            IndexingStatus::Running { .. } | IndexingStatus::Preparing { .. } => {}
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => break,
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Let the maintenance pass finish before tearing the service down.
    let idle_by = Instant::now() + Duration::from_secs(120);
    while Instant::now() < idle_by && !matches!(service.get_status(), IndexingStatus::Idle) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    drop(service);

    eprintln!("peak WAL during the run: {} bytes", peak);
    assert!(
        checkpointed,
        "the log never shrank mid-run; the fixture is not exercising the cap"
    );
    // Generously above the 16 MiB cap: the check runs between round-robin
    // rounds, so a round's worth of commits can land on top of it, and a
    // checkpoint that loses a lock race defers to the next cap of growth.
    assert!(
        peak < 96 * 1024 * 1024,
        "the log peaked at {} bytes against a 16 MiB cap",
        peak
    );
    assert_eq!(after, 0, "the optimize pass leaves an empty log behind");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Stopping a run does not skip the optimize pass.
///
/// A run cut short is exactly when the log is at its largest and nothing else
/// will come along to land it: the writer connection closes, and the next run
/// may be hours away. So Stop ends the *indexing*, and the pass that follows
/// runs either way — visible as `Optimizing` until it is done.
#[test]
fn a_stopped_run_is_still_optimized() {
    let root = tmp_dir("stop-optimize");
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(200)
        .into_bytes();
    for i in 0..4000 {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 40, i)), &body);
    }

    let db_dir = tmp_dir("stop-optimize-db");
    let db = db_dir.join("index.sqlite");
    let wal = db_dir.join("index.sqlite-wal");

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            test_config(),
        )
        .unwrap();

    // Let it get far enough in to have written something worth landing.
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 512 * 1024 {
            break;
        }
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    service.request_stop();

    let mut saw_optimizing = false;
    let idle_by = Instant::now() + Duration::from_secs(120);
    loop {
        match service.get_status() {
            IndexingStatus::Optimizing => saw_optimizing = true,
            IndexingStatus::Idle => break,
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => {}
        }
        assert!(
            Instant::now() < idle_by,
            "the stopped run never reached Idle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(
        saw_optimizing,
        "a stopped run must still publish Optimizing"
    );
    assert_eq!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
        0,
        "the optimize pass must land the stopped run's log"
    );
    drop(service);

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// High-byte binaries are listed but never full-text extracted.
///
/// The whole reason the text sniff demands valid UTF-8. Protobuf and friends
/// carry no NUL and no control bytes, so the binary guard passes them; before
/// the guard was tightened they were adopted as `text/plain`, read in full,
/// run through chardetng's never-failing windows-1252 floor and stored as
/// mojibake. On a real 99k-file tree that was 93% of every byte of extracted
/// text.
///
/// End-to-end because the interesting part is the *combination*: the row must
/// survive in `files` (the file is still findable by name) while acquiring no
/// `documents_text` sidecar and no `failed_files` entry — it is not a failure,
/// it is a file with no text in it. The `.txt` alongside it holds the same
/// bytes and must still extract, which is what proves the fix cost nothing for
/// files an extension already identified.
#[test]
fn high_byte_binaries_are_listed_but_not_text_extracted() {
    let root = tmp_dir("sniff-binary");
    let db_dir = tmp_dir("sniff-binary-db");
    let db = db_dir.join("index.sqlite");

    // Head of a real protobuf-framed GPS log: varint framing around ASCII
    // NMEA sentences. No NUL, no control-byte density — it clears the binary
    // guard on its own.
    let mut pb = b"\x10\n\x02v1\x10\x01\x18\xe2\xe3\xfc\xd3\x9d\xca\x97\xe4\x189\x08".to_vec();
    pb.extend_from_slice(b"\x12*$GNGGA,181558.00,,,,,0,00,99.99,,,,,,*78\r\n");
    assert!(!pb.contains(&0u8), "fixture must not trip the NUL guard");

    let legacy = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.";

    touch(&root.join("rtk.pb"), &pb);
    touch(&root.join("legacy.txt"), legacy);
    touch(&root.join("notes.md"), b"ordinary utf-8 prose");
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let probe = |suffix: &str| -> (i64, i64, i64) {
        conn.query_row(
            "SELECT f.content_state,
                    (SELECT COUNT(*) FROM documents_text d WHERE d.file_id = f.id),
                    (SELECT COUNT(*) FROM failed_files x WHERE x.file_id = f.id)
               FROM files f WHERE f.path LIKE '%' || ?1",
            [suffix],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_else(|e| panic!("{suffix} must be indexed: {e}"))
    };

    // 3 = not applicable. Present in `files`, so filename search still finds
    // it; no sidecar, so none of its bytes reached the index.
    assert_eq!(
        probe("rtk.pb"),
        (3, 0, 0),
        "a high-byte binary must be listed, not extracted, and not a failure"
    );

    // Same bytes, known extension: typed by mime_guess, never sniffed, still
    // decoded through chardetng and stored.
    let (state, sidecars, failures) = probe("legacy.txt");
    assert_eq!(
        (state, failures),
        (1, 0),
        "a legacy-charset .txt must still extract"
    );
    assert_eq!(sidecars, 1, "and must still store its text");

    assert_eq!(probe("notes.md"), (1, 1, 0), "ordinary UTF-8 is unaffected");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}
