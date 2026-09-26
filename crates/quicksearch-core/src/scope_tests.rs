use super::*;
use crate::testutil::{touch, Scratch};
use crate::walk::{walk_indexable_files, WalkEvent};
use rusqlite::OptionalExtension;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn empty_db(dir: &Path) -> PathBuf {
    let db = dir.join("index.sqlite");
    crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
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

/// A tree of `.log` files plus `keep.txt`, indexed one row per file, and a
/// config narrowed to exclude the logs that scans one row per page, so a
/// counter that only moved at the end would show.
struct LogPrune {
    conn: Connection,
    narrowed: Config,
    work: IndexWork,
    registry: Registry,
    _db_dir: Scratch,
    _root: Scratch,
}

impl LogPrune {
    fn new(tag: &str, logs: usize) -> LogPrune {
        let root = Scratch::canonical(tag);
        for i in 0..logs {
            touch(&root.join(format!("f{}.log", i)), b"x");
        }
        touch(&root.join("keep.txt"), b"x");

        let db_dir = Scratch::dir(&format!("{}-db", tag));
        let db = empty_db(&db_dir);
        let mut conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config.processing.batch_size = 1;
        seed(&mut conn, &on_disk(&root));

        let mut narrowed = config.clone();
        narrowed.indexing.ignore_patterns = vec!["*.log".into()];
        let work = crate::config::diff_actions(&config, &narrowed).work;
        LogPrune {
            conn,
            narrowed,
            work,
            registry: Registry::default_set(),
            _db_dir: db_dir,
            _root: root,
        }
    }

    fn cursor(&self) -> WorkCursor {
        WorkCursor::new(self.work.clone(), &self.narrowed).unwrap()
    }

    fn advance(&mut self, cursor: &mut WorkCursor, deadline: Instant, cancel: &AtomicBool) {
        advance(
            &mut self.conn,
            &self.narrowed,
            &self.registry,
            cursor,
            deadline,
            cancel,
        )
        .unwrap();
    }
}

/// Stricter than the walker and every prune deletes rows the next run puts
/// straight back; laxer and the rows the user excluded survive.
#[test]
fn scope_agrees_with_the_walker() {
    let root = Scratch::canonical("agree");
    for file in [
        "keep.txt",
        "sub/keep2.txt",
        "sub/skip.tmp",
        "sub/node_modules/dep/index.js",
        ".hidden/inside.txt",
        ".dotfile",
        "build/out/artifact.o",
        "build/keep3.txt",
        "nested/build/also.o",
    ] {
        touch(&root.join(file), b"x");
    }

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
        let db_dir = Scratch::dir("agree-db");
        let emitted = walked(&config, &empty_db(&db_dir));
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
}

#[test]
fn a_root_is_never_filtered_but_its_children_still_are() {
    let base = Scratch::canonical("root-name");
    let root = base.join("node_modules");
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("node_modules/nested.txt"), b"x");

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.indexing.ignore_patterns = vec!["node_modules".into()];
    let scope = Scope::from_config(&config).unwrap();
    assert!(scope.covers(&root, &root.join("keep.txt")));
    assert!(!scope.covers(&root, &root.join("node_modules/nested.txt")));

    let db_dir = Scratch::dir("root-name-db");
    let emitted = walked(&config, &empty_db(&db_dir));
    for path in on_disk(&root) {
        assert_eq!(scope.covers(&root, &path), emitted.contains(&path));
    }

    // A full-path pattern reaching the root itself takes the whole tree.
    config.indexing.ignore_patterns = vec![root.to_string_lossy().into_owned()];
    let scope = Scope::from_config(&config).unwrap();
    assert!(!scope.covers(&root, &root.join("keep.txt")));
}

/// A scan that reports nothing is indistinguishable from a hang.
#[test]
fn the_scan_reports_its_way_through_every_row() {
    let mut fx = LogPrune::new("progress", 7);
    let mut cursor = fx.cursor();
    assert_eq!(
        cursor.progress(),
        ReconcileProgress::default(),
        "nothing counted before the first slice"
    );

    let run = AtomicBool::new(false);
    let mut seen: Vec<usize> = Vec::new();
    while !cursor.done() {
        // A deadline already past: each call does the least it can, so the
        // counters are sampled at their finest granularity.
        fx.advance(&mut cursor, Instant::now(), &run);
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
}

/// FTS5's delete-merge threshold is off for the length of a pass and comes
/// back on at the end of it.
///
/// Without the first half the pass is several times slower
/// (`file_handling::fts_begin_tombstone_burst` has the table). Without the
/// second, no later `'merge'` ever reclaims a tombstone again
/// (`fts5IndexFindDeleteMerge` returns early on a zero threshold), and FTS5
/// persists the setting in its `%_config` shadow table, so the damage is
/// silent and outlives the process.
#[test]
fn a_pass_turns_delete_merging_off_and_puts_it_back() {
    let mut fx = LogPrune::new("burst", 6);

    // Read back out of FTS5's own `%_config`: what outlives the process is
    // what is written there.
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
        threshold(&fx.conn),
        None,
        "a fresh index leaves FTS5 on its own default and records nothing"
    );

    // One slice, with a deadline already past: the pass is under way and has
    // committed at least one page, so the threshold is off and durably so.
    let mut cursor = fx.cursor();
    let run = AtomicBool::new(false);
    fx.advance(&mut cursor, Instant::now(), &run);
    assert!(!cursor.done(), "one page cannot have finished seven rows");
    assert_eq!(
        threshold(&fx.conn),
        Some(0),
        "delete-merging is off while the pass is creating tombstones"
    );

    while !cursor.done() {
        fx.advance(&mut cursor, Instant::now(), &run);
    }
    assert_eq!(
        threshold(&fx.conn),
        Some(i64::from(crate::file_handling::FTS_DELETEMERGE)),
        "a finished pass leaves tombstone reclamation working again"
    );
}

/// Cancelled from the outset, a pass runs not one statement and stays
/// unfinished, so nothing downstream records the config as reconciled.
#[test]
fn cancelling_before_the_first_slice_touches_nothing() {
    let mut fx = LogPrune::new("cancel-early", 6);
    let mut cursor = fx.cursor();
    fx.advance(&mut cursor, Instant::now() + SLICE, &AtomicBool::new(true));
    assert!(!cursor.done(), "a cancelled pass is never finished");
    assert_eq!(
        cursor.progress(),
        ReconcileProgress::default(),
        "a cancelled pass touched the index"
    );
}

/// Cancelled part-way, the cursor stays unfinished and the counters keep what
/// the first slice earned; rows already reached stay gone, since the pass is
/// idempotent.
#[test]
fn cancelling_stops_the_scan_without_finishing_it() {
    let mut fx = LogPrune::new("cancel", 6);
    let stop = AtomicBool::new(false);
    let mut cursor = fx.cursor();
    fx.advance(&mut cursor, Instant::now(), &stop);
    let part_way = cursor.progress();
    assert!(part_way.examined > 0 && !cursor.done(), "nothing to cancel");

    stop.store(true, Ordering::Relaxed);
    fx.advance(&mut cursor, Instant::now() + SLICE, &stop);
    assert!(!cursor.done(), "the pass finished despite the cancellation");
    assert_eq!(
        cursor.progress(),
        part_way,
        "the cancelled slice did more work"
    );
}
