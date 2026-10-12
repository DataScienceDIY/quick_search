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

/// Containment as `upsert_path` asks it: by whole components, innermost root
/// first, a root containing itself.
#[test]
fn root_of_finds_the_innermost_root_by_whole_components() {
    let scratch = Scratch::canonical("root-of");
    // The spelling roots are held in: on Windows, without the `\\?\`.
    let base = PathBuf::from(crate::file_handling::path_to_db_string(&scratch));
    let (outer, inner) = (base.join("b"), base.join("b").join("deep"));
    std::fs::create_dir_all(&inner).unwrap();
    let mut config = Config::default();
    config.paths.indexing_paths = vec![
        outer.to_string_lossy().into_owned(),
        inner.to_string_lossy().into_owned(),
    ];
    let scope = Scope::from_config(&config).unwrap();

    assert_eq!(scope.root_of(&outer.join("x.txt")), Some(outer.as_path()));
    assert_eq!(
        scope.root_of(&inner.join("y.txt")),
        Some(inner.as_path()),
        "the innermost root wins"
    );
    assert_eq!(
        scope.root_of(&outer),
        Some(outer.as_path()),
        "a root itself"
    );
    assert_eq!(
        scope.root_of(&base.join("bc").join("z.txt")),
        None,
        "a sibling with the root's name as a prefix"
    );
    assert_eq!(scope.root_of(&base), None, "an ancestor of every root");
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

/// An index whose record names `roots`. The connection comes first, so it
/// closes before the directory goes.
fn stamped(tag: &str, roots: &[String]) -> (Connection, Scratch) {
    let dir = Scratch::dir(tag);
    let db = empty_db(&dir);
    let conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
    crate::indexing::IndexingService::stamp_reconciled(&conn, &Config::default(), roots).unwrap();
    (conn, dir)
}

fn with_roots(roots: &[&String]) -> Config {
    let mut config = Config::default();
    config.paths.indexing_paths = roots.iter().map(|r| r.to_string()).collect();
    config
}

/// While one root is offline, every recorded root is kept, and each only once.
#[test]
fn an_offline_root_holds_every_recorded_root_once() {
    let base = Scratch::canonical("hold-offline");
    let online = base.join("online").to_string_lossy().into_owned();
    std::fs::create_dir_all(&online).unwrap();
    let offline = base.join("unmounted").to_string_lossy().into_owned();
    let removed = base.join("removed").to_string_lossy().into_owned();
    let (conn, _db) = stamped(
        "hold-offline-db",
        &[online.clone(), offline.clone(), removed.clone()],
    );

    let mut current = with_roots(&[&offline, &online]);
    hold_offline_roots(&conn, &mut current).unwrap();
    let want = vec![offline, online, removed];
    assert_eq!(current.paths.indexing_paths, want);

    hold_offline_roots(&conn, &mut current).unwrap();
    assert_eq!(
        current.paths.indexing_paths, want,
        "a second hold adds nothing"
    );
}

#[test]
fn hold_offline_roots_changes_nothing_while_every_root_resolves() {
    let root = Scratch::canonical("hold-online");
    let online = root.to_string_lossy().into_owned();
    let removed = root.join("removed").to_string_lossy().into_owned();
    let (conn, _db) = stamped("hold-online-db", &[online.clone(), removed]);

    let mut current = with_roots(&[&online]);
    hold_offline_roots(&conn, &mut current).unwrap();
    assert_eq!(current.paths.indexing_paths, vec![online]);
}

/// Neither an index with no record nor one recording no roots has anything
/// to hold.
#[test]
fn hold_offline_roots_changes_nothing_without_recorded_roots() {
    let dir = Scratch::dir("hold-unrecorded");
    let db = empty_db(&dir);
    let conn = crate::db::open_existing(db.to_str().unwrap(), true).unwrap();
    let offline = dir.join("unmounted").to_string_lossy().into_owned();

    let mut current = with_roots(&[&offline]);
    hold_offline_roots(&conn, &mut current).unwrap();
    assert_eq!(current.paths.indexing_paths, vec![offline.clone()]);
    drop(conn);

    let (conn, _db) = stamped("hold-empty-db", &[]);
    hold_offline_roots(&conn, &mut current).unwrap();
    assert_eq!(current.paths.indexing_paths, vec![offline]);
}

/// What [`outstanding_work`] owes the default config over one root, against
/// a record of that same config except `maximum_text_file_size`, recorded as
/// `cap` (no row for `None`).
fn owed_with_recorded_cap(tag: &str, cap: Option<&str>) -> IndexWork {
    let dir = Scratch::canonical(tag);
    let db_path = empty_db(&dir);
    let db = db_path.to_str().unwrap();
    let root = dir.to_string_lossy().into_owned();
    let config = with_roots(&[&root]);
    let roots: Vec<String> = config.normalized_indexing_paths().into_iter().collect();
    let conn = crate::db::open_existing(db, true).unwrap();
    crate::indexing::IndexingService::stamp_reconciled(&conn, &config, &roots).unwrap();
    let changed = match cap {
        Some(cap) => conn.execute(
            "UPDATE config_validation SET value = ?1 WHERE key = 'maximum_text_file_size'",
            [cap],
        ),
        None => conn.execute(
            "DELETE FROM config_validation WHERE key = 'maximum_text_file_size'",
            [],
        ),
    };
    assert_eq!(changed.unwrap(), 1, "the stamp records the cap");
    drop(conn);
    outstanding_work(db, &config).unwrap()
}

/// The cap decides whose text is read, and a walk skips a row whose file is
/// unchanged: a cap raised while the app was closed is owed a content pass
/// from the record alone.
#[test]
fn a_recorded_cap_below_the_config_is_owed_a_content_pass() {
    let owed = owed_with_recorded_cap("cap-raised", Some("1024"));
    assert!(owed.reconcile_content, "{:?}", owed);
    assert!(owed.touches_index());
}

/// An index from before the cap was recorded has nothing to differ from.
#[test]
fn an_unrecorded_cap_reads_as_unchanged() {
    let owed = owed_with_recorded_cap("cap-unrecorded", None);
    assert!(owed.is_empty(), "{:?}", owed);
}

/// Garbage in the record keeps the configured cap rather than reading as
/// some other one and restating rows.
#[test]
fn a_non_numeric_recorded_cap_is_ignored() {
    for garbage in ["x", "-1", "", "1e6"] {
        let owed = owed_with_recorded_cap("cap-garbage", Some(garbage));
        assert!(owed.is_empty(), "{:?} read as {:?}", garbage, owed);
    }
}

/// A cloud placeholder's NA row records nothing that tells it from a file
/// the old filter excluded, so a content edit, even a narrowing one, makes it
/// pending; the content pass is what keeps it unread. Windows-only, and not
/// run in CI.
#[cfg(windows)]
#[test]
fn a_reconciled_offline_file_goes_pending_and_the_content_pass_leaves_it_unread() {
    use crate::file_handling::{store_extracted, ContentOutcome, ExtractCursor};
    use crate::walk::TryNext;

    let root = Scratch::canonical("offline-reconcile");
    let file = root.join("notes.txt");
    touch(&file, b"sphinx of black quartz");
    crate::testutil::mark_offline(&file).unwrap();
    let db_dir = Scratch::dir("offline-reconcile-db");
    let db = empty_db(&db_dir);
    let db = db.to_str().unwrap();
    let mut conn = crate::db::open_existing(db, true).unwrap();
    // NA and unhashed, as the walk writes a placeholder.
    seed(&mut conn, std::slice::from_ref(&file));
    let state = |conn: &Connection| -> i64 {
        conn.query_row("SELECT content_state FROM files", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(state(&conn), repo::STATE_NA);

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    let mut narrowed = config.clone();
    narrowed.indexing.content_extensions = vec!["txt".into()];
    let work = crate::config::diff_actions(&config, &narrowed).work;
    assert!(work.reconcile_content, "{:?}", work);
    let registry = Arc::new(Registry::default_set());
    let run = AtomicBool::new(false);
    let far = Instant::now() + Duration::from_secs(60);
    let mut cursor = WorkCursor::new(work, &narrowed).unwrap();
    while !cursor.done() {
        advance(&mut conn, &narrowed, &registry, &mut cursor, far, &run).unwrap();
    }
    assert_eq!(state(&conn), repo::STATE_PENDING, "the row wants content");
    drop(conn);

    let stop = Arc::new(AtomicBool::new(false));
    let mut pass = crate::content::extract_content(
        db,
        &ExtractCursor::for_root(&root.to_string_lossy()),
        registry,
        narrowed,
        stop.clone(),
        1,
    );
    let mut rows = Vec::new();
    loop {
        match pass.try_next() {
            TryNext::Item(row) => rows.push(row),
            TryNext::Empty => std::thread::sleep(Duration::from_millis(1)),
            TryNext::Finished => break,
        }
    }
    assert!(pass.finish());
    assert_eq!(rows.len(), 1, "the pass took the pending row");
    // The bytes are local and readable: anything but NotApplicable means the
    // file was read.
    assert_eq!(rows[0].outcome, ContentOutcome::NotApplicable);

    let writer = std::sync::Mutex::new(crate::db::open_existing(db, true).unwrap());
    let writer = Arc::new(writer);
    let mut encoder = repo::DocEncoder::new().unwrap();
    store_extracted(&writer, &rows, &stop, far, &mut encoder).unwrap();
    assert_eq!(state(&crate::lock_ok(&writer)), repo::STATE_NA);
}
