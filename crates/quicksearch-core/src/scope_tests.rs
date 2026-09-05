use super::*;
use crate::walk::{walk_indexable_files, WalkEvent};
use rusqlite::OptionalExtension;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn tmp_tree(tag: &str) -> PathBuf {
    crate::testutil::scratch_dir_canonical(tag)
}

fn touch(p: &Path) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

fn empty_db(dir: &Path) -> PathBuf {
    let db = dir.join("index.sqlite");
    crate::db::open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
    db
}

/// Every file the walker actually emits under `config`'s single root.
fn walked(config: &Config, db: &Path) -> HashSet<PathBuf> {
    let root = config.paths.indexing_paths[0].clone();
    walk_indexable_files(
        &[root],
        config.indexing.follow_symlinks,
        config.indexing.include_hidden,
        IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap(),
        db.to_str().unwrap(),
        config.clone(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        2,
    )
    .filter_map(|e| match e {
        WalkEvent::File(f) => Some(PathBuf::from(f.path)),
        WalkEvent::Stale(_) => None,
    })
    .collect()
}

/// One `files` row per path, which is all the scan reads.
fn seed(conn: &mut Connection, paths: &[PathBuf]) {
    let tx = conn.transaction().unwrap();
    for path in paths {
        let path = path.to_string_lossy();
        // The indexer's own split, so the separator stays with the parent.
        let (parent, name) = crate::file_handling::split_db_path(&path).expect("a file's path");
        repo::insert_file(
            &tx,
            &repo::NewFile {
                name,
                parent,
                size: 1,
                mtime: 1,
                mime: Some("text/plain"),
                ftype: crate::mime::FileType::TEXT,
                hash: None,
                needs_content: false,
            },
        )
        .unwrap()
        .expect("unique path");
    }
    tx.commit().unwrap();
}

fn on_disk(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

/// Stricter than the walker and every prune deletes rows the next run puts
/// straight back; laxer and the rows the user excluded survive.
#[test]
fn scope_agrees_with_the_walker() {
    let root = tmp_tree("agree");
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/keep2.txt"));
    touch(&root.join("sub/skip.tmp"));
    touch(&root.join("sub/node_modules/dep/index.js"));
    touch(&root.join(".hidden/inside.txt"));
    touch(&root.join(".dotfile"));
    touch(&root.join("build/out/artifact.o"));
    touch(&root.join("build/keep3.txt"));
    touch(&root.join("nested/build/also.o"));

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.indexing.ignore_patterns = vec![
        "*.tmp".into(),
        "node_modules".into(),
        // A full-path pattern: this directory, not every one called `out`.
        root.join("build/out").to_string_lossy().into_owned(),
    ];

    for include_hidden in [false, true] {
        config.indexing.include_hidden = include_hidden;
        let db = empty_db(&tmp_tree("agree-db"));
        let emitted = walked(&config, &db);
        let scope = Scope::from_config(&config).unwrap();

        for path in on_disk(&root) {
            assert_eq!(
                scope.covers(&root, &path),
                emitted.contains(&path),
                "disagreement on {} (include_hidden = {})",
                path.display(),
                include_hidden
            );
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_root_is_never_filtered_but_its_children_still_are() {
    let base = tmp_tree("root-name");
    let root = base.join("node_modules");
    touch(&root.join("keep.txt"));
    touch(&root.join("node_modules/nested.txt"));

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.indexing.ignore_patterns = vec!["node_modules".into()];
    let scope = Scope::from_config(&config).unwrap();
    assert!(scope.covers(&root, &root.join("keep.txt")));
    assert!(!scope.covers(&root, &root.join("node_modules/nested.txt")));

    let db = empty_db(&tmp_tree("root-name-db"));
    let emitted = walked(&config, &db);
    for path in on_disk(&root) {
        assert_eq!(scope.covers(&root, &path), emitted.contains(&path));
    }

    // A full-path pattern reaching the root itself takes the whole tree.
    config.indexing.ignore_patterns = vec![root.to_string_lossy().into_owned()];
    let scope = Scope::from_config(&config).unwrap();
    assert!(!scope.covers(&root, &root.join("keep.txt")));

    std::fs::remove_dir_all(&base).ok();
}

/// A scan that reports nothing is indistinguishable from a hang.
#[test]
fn the_scan_reports_its_way_through_every_row() {
    let root = tmp_tree("progress");
    for i in 0..7 {
        touch(&root.join(format!("f{}.log", i)));
    }
    touch(&root.join("keep.txt"));

    let db_dir = tmp_tree("progress-db");
    let db = empty_db(&db_dir);
    let mut conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    // One row per page, so a counter that only moved at the end is visible.
    config.processing.batch_size = 1;
    seed(&mut conn, &on_disk(&root));

    let mut narrowed = config.clone();
    narrowed.indexing.ignore_patterns = vec!["*.log".into()];
    let work = crate::config::diff_actions(&config, &narrowed).work;
    let mut cursor = WorkCursor::new(work, &narrowed).unwrap();
    assert_eq!(
        cursor.progress(),
        ReconcileProgress::default(),
        "nothing counted before the first slice"
    );

    let registry = Registry::default_set();
    let run = AtomicBool::new(false);
    let mut seen: Vec<usize> = Vec::new();
    while !cursor.done() {
        // A deadline already past: each call does the least it can, so the
        // counters are sampled at their finest granularity.
        advance(
            &mut conn,
            &narrowed,
            &registry,
            &mut cursor,
            Instant::now(),
            &run,
        )
        .unwrap();
        seen.push(cursor.progress().examined);
    }

    let end = cursor.progress();
    assert_eq!(end.total, Some(8), "counted once, before the first page");
    assert_eq!(end.examined, 8, "every row was re-tested");
    assert_eq!(end.deleted, 7, "the logs, and only the logs");
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "the count never goes backwards: {:?}",
        seen
    );
    assert!(
        seen.len() > 2 && seen[0] < end.examined,
        "progress was reported during the scan, not only at its end: {:?}",
        seen
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// FTS5's delete-merge threshold is turned off for the length of a pass and
/// must come back on at the end of it.
///
/// Both halves are asserted, and both matter. Without the first the pass is
/// several times slower than it needs to be
/// (`file_handling::fts_begin_tombstone_burst` carries the table). Without the
/// second the index is left in a state where **no** later `'merge'` ever
/// reclaims a tombstone again — `fts5IndexFindDeleteMerge` returns early on a
/// zero threshold — and because FTS5 persists the setting in its `%_config`
/// shadow table, that outlives the process. It is a silent, permanent
/// degradation, which is exactly the kind of thing that needs a test rather
/// than a comment.
#[test]
fn a_pass_turns_delete_merging_off_and_puts_it_back() {
    let root = tmp_tree("burst");
    for i in 0..6 {
        touch(&root.join(format!("f{}.log", i)));
    }
    touch(&root.join("keep.txt"));

    let db_dir = tmp_tree("burst-db");
    let db = empty_db(&db_dir);
    let mut conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.processing.batch_size = 1;
    seed(&mut conn, &on_disk(&root));

    // Read back out of FTS5's own `%_config`, not out of a value we remember:
    // what outlives the process is what is written there.
    let threshold = |conn: &Connection| -> Option<i64> {
        conn.query_row(
            "SELECT v FROM searchabletext_config WHERE k = 'deletemerge'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    };
    assert_eq!(
        threshold(&conn),
        None,
        "a fresh index leaves FTS5 on its own default and records nothing"
    );

    let mut narrowed = config.clone();
    narrowed.indexing.ignore_patterns = vec!["*.log".into()];
    let work = crate::config::diff_actions(&config, &narrowed).work;
    let mut cursor = WorkCursor::new(work, &narrowed).unwrap();
    let registry = Registry::default_set();
    let run = AtomicBool::new(false);

    // One slice, with a deadline already past: the pass is under way and has
    // committed at least one page, so the threshold is off and durably so.
    advance(
        &mut conn,
        &narrowed,
        &registry,
        &mut cursor,
        Instant::now(),
        &run,
    )
    .unwrap();
    assert!(!cursor.done(), "one page cannot have finished seven rows");
    assert_eq!(
        threshold(&conn),
        Some(0),
        "delete-merging is off while the pass is creating tombstones"
    );

    while !cursor.done() {
        advance(
            &mut conn,
            &narrowed,
            &registry,
            &mut cursor,
            Instant::now(),
            &run,
        )
        .unwrap();
    }
    assert_eq!(
        threshold(&conn),
        Some(i64::from(crate::file_handling::FTS_DELETEMERGE)),
        "a finished pass leaves tombstone reclamation working again"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// The cursor is left un-finished, so nothing downstream records the config
/// as reconciled. Rows already reached stay gone: the pass is idempotent.
#[test]
fn cancelling_stops_the_scan_without_finishing_it() {
    let root = tmp_tree("cancel");
    for i in 0..6 {
        touch(&root.join(format!("f{}.log", i)));
    }
    touch(&root.join("keep.txt"));

    let db_dir = tmp_tree("cancel-db");
    let db = empty_db(&db_dir);
    let mut conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.processing.batch_size = 1;
    seed(&mut conn, &on_disk(&root));

    let mut narrowed = config.clone();
    narrowed.indexing.ignore_patterns = vec!["*.log".into()];
    let work = crate::config::diff_actions(&config, &narrowed).work;
    let registry = Registry::default_set();

    // Cancelled from the outset: not one statement runs.
    let stop = AtomicBool::new(true);
    let mut cursor = WorkCursor::new(work.clone(), &narrowed).unwrap();
    advance(
        &mut conn,
        &narrowed,
        &registry,
        &mut cursor,
        Instant::now() + SLICE,
        &stop,
    )
    .unwrap();
    assert!(!cursor.done(), "a cancelled pass is never finished");
    assert_eq!(
        cursor.progress(),
        ReconcileProgress::default(),
        "a cancelled pass touched the index"
    );

    // And part-way through: the counters keep what the first slice earned.
    let stop = AtomicBool::new(false);
    let mut cursor = WorkCursor::new(work, &narrowed).unwrap();
    advance(
        &mut conn,
        &narrowed,
        &registry,
        &mut cursor,
        Instant::now(),
        &stop,
    )
    .unwrap();
    let part_way = cursor.progress();
    assert!(part_way.examined > 0 && !cursor.done(), "nothing to cancel");

    stop.store(true, Ordering::Relaxed);
    advance(
        &mut conn,
        &narrowed,
        &registry,
        &mut cursor,
        Instant::now() + SLICE,
        &stop,
    )
    .unwrap();
    assert!(!cursor.done(), "the pass finished despite the cancellation");
    assert_eq!(
        cursor.progress(),
        part_way,
        "the cancelled slice did more work"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}
