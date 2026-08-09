use super::*;
use crate::walk::{walk_indexable_files, WalkEvent};
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
        let (parent, name) = path.rsplit_once('/').unwrap();
        repo::insert_file(
            &tx,
            &repo::NewFile {
                name,
                path: &path,
                parent,
                size: 1,
                mtime: 1,
                inode: None,
                device_id: None,
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

/// Every file that physically exists under `root`, walker or no walker.
fn on_disk(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

/// `Scope` must reach the same verdict the walker does for every file on
/// disk: stricter, and every prune deletes rows the next run puts straight
/// back; laxer, and the rows the user excluded survive.
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
        // A full-path pattern: prunes this one directory, not every
        // directory called `out`.
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

/// A root is never filtered — the user chose it. A component pattern naming
/// the root must not empty it out, but a full-path pattern matching the
/// root still prunes everything below it.
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

/// Root ownership compares whole components, so a sibling whose name
/// merely starts with a root's is not inside it — a prune that got this
/// wrong would delete a neighbouring folder's entire index.
#[test]
fn owning_root_does_not_match_name_prefixes() {
    let base = tmp_tree("prefix");
    let root = base.join("data");
    let sibling = base.join("database");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    let scope = Scope::from_config(&config).unwrap();

    assert_eq!(scope.owning_root(&root.join("f.txt")), Some(root.as_path()));
    assert_eq!(scope.owning_root(&sibling.join("f.txt")), None);
    // The root itself is a directory, never a row, and owns nothing.
    assert_eq!(scope.owning_root(&root), None);
    std::fs::remove_dir_all(&base).ok();
}

/// The counters the status display reads: a scan that reports nothing is
/// indistinguishable from a hang.
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
        // A deadline already past, so each call does the least it can and
        // the counters are sampled at their finest granularity.
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

/// Cancelling stops the pass at the next statement boundary and leaves the
/// cursor un-finished, so nothing downstream can record the configuration
/// as reconciled. Rows already reached stay gone — the pass is idempotent
/// and the next run finishes it.
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

    // And part-way through: one slice with the flag clear, the rest with
    // it set. The counters keep what the first slice earned.
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

/// A path under no configured root has no rules to apply — a followed
/// symlink's target is the real case; `owning_root` returning `None` is
/// what keeps it alive.
#[test]
fn a_path_outside_every_root_has_no_owner() {
    let base = tmp_tree("outside");
    let root = base.join("indexed");
    std::fs::create_dir_all(&root).unwrap();

    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.indexing.ignore_patterns = vec!["*".into()];
    let scope = Scope::from_config(&config).unwrap();

    assert_eq!(scope.owning_root(Path::new("/elsewhere/target.txt")), None);
    std::fs::remove_dir_all(&base).ok();
}
