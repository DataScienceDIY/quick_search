use super::pipeline::RootPipeline;
use super::*;
use crate::extract::Registry;
use crate::file_handling::{
    normalize_root_string, path_to_db_string, split_db_path, ExtractCursor,
};
use crate::testutil::scratch_dir_canonical as tmp_dir;
use crate::walk::walk_indexable_files;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

fn config_with(roots: Vec<String>, overrides: &[(&str, usize)]) -> Config {
    let mut cfg = Config::default();
    cfg.paths.indexing_paths = roots;
    for (root, workers) in overrides {
        cfg.indexing
            .root_workers
            .insert((*root).to_string(), *workers);
    }
    cfg
}

#[test]
fn an_override_survives_a_trailing_slash() {
    let dir = tmp_dir("slash");
    let spelled = format!("{}/", dir.display());
    let cfg = config_with(vec![spelled.clone()], &[(&spelled, 24)]);
    assert_eq!(
        resolved_root_workers(&cfg).get(&normalize_root_string(&dir.to_string_lossy())),
        Some(&24),
        "the walk canonicalizes the root; the override must follow"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn an_override_survives_a_symlinked_root() {
    let dir = tmp_dir("symlink");
    let target = dir.join("real");
    let link = dir.join("link");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let spelled = link.to_string_lossy().into_owned();
    let cfg = config_with(vec![spelled.clone()], &[(&spelled, 12)]);
    let resolved = resolved_root_workers(&cfg);
    assert_eq!(
        resolved.get(&normalize_root_string(&target.to_string_lossy())),
        Some(&12)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn overrides_for_folders_that_are_no_longer_indexed_are_dropped() {
    let dir = tmp_dir("stale");
    let kept = dir.to_string_lossy().into_owned();
    let cfg = config_with(vec![kept.clone()], &[(&kept, 8), ("/gone", 32)]);
    let resolved = resolved_root_workers(&cfg);
    assert_eq!(resolved.len(), 1, "{:?}", resolved);
    assert_eq!(resolved.values().next(), Some(&8));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_root_without_an_override_gets_no_entry() {
    let dir = tmp_dir("auto");
    let root = dir.to_string_lossy().into_owned();
    let cfg = config_with(vec![root], &[]);
    assert!(resolved_root_workers(&cfg).is_empty(), "absent = auto");
    std::fs::remove_dir_all(&dir).ok();
}

/// A walking pipeline for `dir` whose walk pool has `workers` threads and
/// reads its rows from `walk_db`, and whose content pass over `db_path` has
/// `content_workers`. The pass covers an empty range, so it ends at once, but
/// its pool size is fixed when it is built.
fn idle_pipeline(
    dir: &Path,
    walk_db: &str,
    db_path: &str,
    workers: usize,
    content_workers: usize,
    stop: &Arc<AtomicBool>,
) -> RootPipeline {
    let root = dir.to_string_lossy().into_owned();
    let walk = walk_indexable_files(
        std::slice::from_ref(&root),
        false,
        crate::config::IgnoreSet::compile(&[]).unwrap(),
        walk_db,
        Config::default(),
        Arc::new(Registry::default_set()),
        stop.clone(),
        workers,
    );
    let content = crate::content::extract_content(
        db_path,
        &ExtractCursor::for_root(&dir.join("nothing").to_string_lossy()),
        Arc::new(Registry::default_set()),
        Config::default(),
        stop.clone(),
        content_workers,
    );
    RootPipeline {
        root,
        walk,
        count_total: Arc::new(AtomicUsize::new(0)),
        workers,
        pending_updates: Vec::new(),
        pending_inserts: Vec::new(),
        walked: 0,
        phase: RootPhase::Walking,
        phase_started: Instant::now(),
        content: Some(content),
        ready: Vec::new(),
        written: 0,
        totals: None,
        current_file: None,
    }
}

#[test]
fn worker_counts_follow_the_phase() {
    let dir = tmp_dir("phase-workers");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    drop(db::open_or_recreate(&db_path, &Default::default()).unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let mut p = idle_pipeline(&dir, &db_path, &db_path, 3, 2, &stop);

    assert_eq!(p.worker_counts().1, 3, "walking: the walk's own pool");
    p.phase = RootPhase::Extracting;
    assert_eq!(p.worker_counts().1, 2, "extracting: the content pool");
    p.phase = RootPhase::Done;
    assert_eq!(p.worker_counts(), (0, 0), "a finished root runs nothing");

    drop(p);
    std::fs::remove_dir_all(&dir).ok();
}

/// Pinned with a zero slice, under which every turn writes exactly one row.
#[test]
fn an_extracting_turn_lands_its_leftovers_one_slice_at_a_time() {
    use super::pipeline::RunCx;
    use crate::content::ExtractedRow;
    use crate::db::repo::{insert_file, NewFile};
    use crate::file_handling::ContentOutcome;
    use crate::mime::FileType;

    let dir = tmp_dir("slice-leftovers");
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let mut conn = db::open_or_recreate(&db_path, &Default::default()).unwrap();
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let mut ready: Vec<ExtractedRow> = Vec::new();
    {
        let tx = conn.transaction().unwrap();
        for i in 0..5 {
            let path = tree.join(format!("f{}.txt", i));
            std::fs::write(&path, "sphinx of black quartz").unwrap();
            let file_id = insert_file(
                &tx,
                &NewFile {
                    name: &format!("f{}.txt", i),
                    // A stored parent always ends in a separator; without one
                    // the row sorts below every `RootRange`.
                    parent: &crate::file_handling::dir_to_db_parent(&tree),
                    size: 22,
                    mtime: 1,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            ready.push(ExtractedRow::new(
                file_id,
                crate::db::repo::RowPath::new(
                    &crate::file_handling::dir_to_db_parent(&tree),
                    &format!("f{}.txt", i),
                ),
                ContentOutcome::Done {
                    text: format!("sphinx of black quartz {}", i),
                },
            ));
        }
        tx.commit().unwrap();
    }
    let conn_mutex = Arc::new(Mutex::new(conn));

    let stop = Arc::new(AtomicBool::new(false));
    let config = Config::default();
    // The pass reports `Finished` on its own, and the turn has to keep going
    // past that until `ready` is empty.
    let mut p = idle_pipeline(&dir, &db_path, &db_path, 1, 1, &stop);
    p.phase = RootPhase::Extracting;
    p.ready = ready;
    let mut cx = RunCx::new(
        conn_mutex.clone(),
        &config,
        &db_path,
        &stop,
        Arc::new(Mutex::new(IndexingStatus::Idle)),
    )
    .unwrap();
    cx.slice = Duration::ZERO;

    let mut turns = 0;
    while p.phase == RootPhase::Extracting {
        turns += 1;
        assert!(
            turns < 200,
            "the root never finished: {} written",
            p.written
        );
        let before = p.written;
        let progressed = p.service_extracting(&mut cx).unwrap();
        assert!(
            p.written - before <= 1,
            "a zero slice wrote {} rows in one turn",
            p.written - before
        );
        assert!(
            p.phase != RootPhase::Done || p.ready.is_empty(),
            "Done with {} rows still to write",
            p.ready.len()
        );
        if !progressed {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(p.written, 5);
    assert!(p.ready.is_empty());
    assert!(
        turns >= 5,
        "five rows cannot land in {} zero-slice turns",
        turns
    );
    let done: i64 = conn_mutex
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(done, 5, "every row reached the index");
    assert_eq!(p.snapshot().extracted, 5);
    assert_eq!(p.snapshot().extract_total, Some(0));
    // A whole path, as the walk phase publishes, not the bare `files.name`.
    // The rows land in order, so the last one written is the last one seeded.
    assert_eq!(
        p.snapshot().current_file.as_deref(),
        Some(format!("{}f4.txt", crate::file_handling::dir_to_db_parent(&tree)).as_str()),
        "the extracting phase names the file by its full path"
    );

    drop(p);
    std::fs::remove_dir_all(&dir).ok();
}

/// One run, stopped from the start when `stop` is set.
fn run_with(config: &Config, db_path: &str, stop: bool) -> Result<(), String> {
    run_indexing_with(
        config,
        db_path,
        &Arc::new(AtomicBool::new(stop)),
        &Arc::new(Mutex::new(IndexingStatus::Idle)),
    )
}

/// [`run_with`] for the tests that read the status the run leaves behind.
fn run_indexing_with(
    config: &Config,
    db_path: &str,
    stop: &Arc<AtomicBool>,
    status: &Arc<Mutex<IndexingStatus>>,
) -> Result<(), String> {
    IndexingService::run_indexing(
        status,
        &config.paths.indexing_paths,
        db_path,
        stop,
        config,
        &Arc::new(Mutex::new(None)),
        &db::InterruptSlot::default(),
    )
}

/// A config indexing `root` alone into `db_path`.
fn run_config(root: &Path, db_path: &str) -> Config {
    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.to_string();
    config
}

fn outstanding_work(db_path: &str, config: &Config) -> crate::config::IndexWork {
    crate::scope::outstanding_work(db_path, config).unwrap()
}

fn stored_walk_count(db_path: &str, root: &str) -> Option<usize> {
    let conn = db::open_existing(db_path, false).unwrap();
    crate::db::repo::get_root_walk_count(&conn, root)
}

fn stored_root_counts(db_path: &str, root: &str) -> Option<crate::db::repo::RootCounts> {
    let conn = db::open_existing(db_path, false).unwrap();
    crate::db::repo::get_root_counts(&conn, root)
}

/// The recorded figure is the next run's progress denominator, and nothing
/// ever re-derives it.
/// Unix only: a Windows `icacls` deny ACE does not bind the owning process
/// reliably enough to test against.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_keeps_the_walk_count_unrecorded() {
    let dir = tmp_dir("unreadable-count");
    // A subdirectory, so the index and its sidecars are not under the root.
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("visible.txt"), "indexed").unwrap();
    let locked = tree.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("inside.txt"), "never seen").unwrap();

    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = run_config(&tree, &db_path);
    let root = normalize_root_string(&tree.to_string_lossy());

    crate::platform::deny_read(&locked).unwrap();
    let blocked = run_with(&config, &db_path, false);
    // Restore before asserting, so a failure still leaves a removable tree.
    crate::platform::restore_read(&locked).ok();
    blocked.unwrap();

    assert_eq!(
        stored_walk_count(&db_path, &root),
        None,
        "a walk that could not read a directory saw only part of the tree"
    );

    run_with(&config, &db_path, false).unwrap();
    assert_eq!(
        stored_walk_count(&db_path, &root),
        Some(2),
        "a clean walk records its file count"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_stopped_run_keeps_the_walk_count_unrecorded() {
    let dir = tmp_dir("stopped-count");
    for i in 0..20 {
        std::fs::write(dir.join(format!("f{}.txt", i)), "body").unwrap();
    }
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = run_config(&dir, &db_path);
    let root = normalize_root_string(&dir.to_string_lossy());

    run_with(&config, &db_path, true).unwrap();
    assert_eq!(stored_walk_count(&db_path, &root), None);

    std::fs::remove_dir_all(&dir).ok();
}

/// A walk whose row reader fails reads no directory, and the vanished sweep
/// would take everything below the root for deleted. Only the walk's reader
/// fails here, on a file that is not an index; the run's writer is fine.
#[test]
fn a_walk_reader_failure_deletes_nothing() {
    use super::pipeline::{cleanup_stale, RunCx};

    let dir = tmp_dir("walk-reader-failure");
    let tree = dir.join("tree");
    let files = [tree.join("a.txt"), tree.join("sub/b.txt")];
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let conn = db::open_or_recreate(&db_path, &Default::default()).unwrap();
    for path in &files {
        crate::testutil::touch(path, b"body");
        let stored = path_to_db_string(path);
        let (parent, name) = split_db_path(&stored).expect("a file's path");
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime, type, content_state)
             VALUES (?1, ?2, 4, 1, 0, 3)",
            rusqlite::params![name, parent],
        )
        .unwrap();
    }
    let not_an_index = dir.join("not-an-index.db");
    std::fs::write(&not_an_index, b"plain text, not SQLite\n".repeat(100)).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let config = run_config(&tree, &db_path);
    let walk_db = not_an_index.to_string_lossy();
    let mut p = idle_pipeline(&tree, &walk_db, &db_path, 2, 1, &stop);
    let conn = Arc::new(Mutex::new(conn));
    let status = Arc::new(Mutex::new(IndexingStatus::Idle));
    let mut cx = RunCx::new(conn, &config, &db_path, &stop, status).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while p.phase == RootPhase::Walking {
        assert!(Instant::now() < deadline, "the walk never ended");
        if !p.service_walking(&mut cx).unwrap() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(!cx.stale_cleanup_ok, "the walk ended early");
    cleanup_stale(std::slice::from_ref(&p), &mut cx).unwrap();
    drop(p);
    drop(cx);

    let conn = db::open_existing(&db_path, false).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "both files are on disk; neither row may go");
    drop(conn);
    assert_eq!(
        stored_walk_count(&db_path, &normalize_root_string(&tree.to_string_lossy())),
        None,
        "a walk that read nothing saw none of the tree"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_completed_run_records_what_each_root_holds() {
    let dir = tmp_dir("root-counts");
    // A subdirectory, so the index and its sidecars are not under the root.
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "alpha body").unwrap();
    std::fs::write(tree.join("b.txt"), "beta body").unwrap();
    std::fs::write(tree.join("c.log"), "outside the whitelist").unwrap();

    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let mut config = run_config(&tree, &db_path);
    config.indexing.content_extensions = vec!["txt".into()];
    let root = normalize_root_string(&tree.to_string_lossy());

    run_with(&config, &db_path, false).unwrap();

    let stored = stored_root_counts(&db_path, &root).expect("a completed run records them");
    assert_eq!(
        stored,
        crate::db::repo::RootCounts { files: 3, fts: 2 },
        "every file under the root, and the two the whitelist let through"
    );

    let conn = db::open_existing(&db_path, false).unwrap();
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    let fts: i64 = conn
        .query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r.get(0))
        .unwrap();
    assert_eq!((stored.files, stored.fts), (files, fts));
    drop(conn);

    std::fs::remove_dir_all(&dir).ok();
}

/// Pinned in both directions: a guard that simply never stored anything
/// would satisfy the negative half on its own.
#[test]
fn a_stopped_run_keeps_the_recorded_counts() {
    let dir = tmp_dir("stopped-root-counts");
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "alpha body").unwrap();

    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = run_config(&tree, &db_path);
    let root = normalize_root_string(&tree.to_string_lossy());

    run_with(&config, &db_path, false).unwrap();
    let after_run = stored_root_counts(&db_path, &root).expect("recorded");
    assert_eq!(after_run.files, 1);

    std::fs::write(tree.join("b.txt"), "beta body").unwrap();
    std::fs::write(tree.join("c.txt"), "gamma body").unwrap();
    run_with(&config, &db_path, true).unwrap();
    assert_eq!(
        stored_root_counts(&db_path, &root),
        Some(after_run),
        "a stopped run leaves the last completed run's figures alone"
    );

    run_with(&config, &db_path, false).unwrap();
    assert_eq!(stored_root_counts(&db_path, &root).unwrap().files, 3);

    std::fs::remove_dir_all(&dir).ok();
}

/// Stamping the fingerprint would tell every later run the index already
/// matches, and nothing would revisit the rows the scan had not reached.
#[test]
fn an_interrupted_reconcile_records_nothing() {
    let dir = tmp_dir("interrupted-reconcile");
    std::fs::write(dir.join("keep.txt"), "kept").unwrap();
    std::fs::write(dir.join("drop.log"), "dropped").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();

    let mut config = run_config(&dir, &db_path);
    config.indexing.ignore_patterns = vec![];
    run_with(&config, &db_path, false).unwrap();

    let mut narrowed = config.clone();
    narrowed.indexing.ignore_patterns = vec!["*.log".into()];
    let pending = outstanding_work(&db_path, &narrowed);
    assert!(pending.touches_index(), "the narrowing has rows to remove");

    run_with(&narrowed, &db_path, true).unwrap();
    assert_eq!(
        outstanding_work(&db_path, &narrowed),
        pending,
        "the same work is still owed"
    );

    run_with(&narrowed, &db_path, false).unwrap();
    assert!(
        outstanding_work(&db_path, &narrowed).is_empty(),
        "a completed run leaves nothing to reconcile"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The record holds the size cap, an input of every row's content decision.
/// A digest change bumps the schema version instead, and trigram is the only
/// tokenizer, so a `hash_algorithm` or `tokenize` row an older build left is
/// never a reason to offer a rebuild.
#[test]
fn the_record_holds_the_cap_and_retired_rows_are_ignored() {
    let dir = tmp_dir("config-record");
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let conn = db::open_or_recreate(&db_path, &Default::default()).unwrap();
    let config = Config::default();
    let roots = vec![dir.to_string_lossy().into_owned()];
    IndexingService::update_config(&conn, &config, &roots).unwrap();
    let recorded = |key: &str| -> Option<String> {
        conn.query_row(
            "SELECT value FROM config_validation WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .ok()
    };
    assert_eq!(
        recorded("maximum_text_file_size"),
        Some(config.processing.maximum_text_file_size.to_string())
    );
    assert_eq!(recorded("hash_algorithm"), None, "no longer recorded");
    assert_eq!(recorded("tokenize"), None, "no longer recorded");

    conn.execute(
        "INSERT INTO config_validation (key, value) VALUES ('hash_algorithm', 'size+tail'), \
         ('tokenize', 'unicode61')",
        [],
    )
    .unwrap();
    assert_eq!(
        IndexingService::validate_config(&conn, &config, &roots).unwrap(),
        None
    );
    let mut longer = config.clone();
    longer.processing.hash_length *= 2;
    let changes = IndexingService::validate_config(&conn, &longer, &roots)
        .unwrap()
        .expect("a hash_length change is reported");
    let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, ["hash_length"]);

    drop(conn);
    std::fs::remove_dir_all(&dir).ok();
}

/// An index over a small tree in a fresh directory, outside the tree so the
/// run does not index its own files. Returns the directory and the index.
fn built_index(tag: &str) -> (std::path::PathBuf, String) {
    let dir = tmp_dir(tag);
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "indexed").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    run_with(&run_config(&tree, &db_path), &db_path, false).unwrap();
    (dir, db_path)
}

/// A run still active at the deadline keeps the file: unlinking under it
/// would leave it writing into a deleted inode. `Optimizing` is set by hand,
/// the state a VACUUM that outlives the stop leaves; the command thread
/// ignores the stop it is sent, as it does for a real one.
#[test]
fn a_delete_while_still_active_is_refused_and_keeps_the_file() {
    let (dir, db_path) = built_index("delete-active");
    let before = std::fs::read(&db_path).unwrap();
    let service = IndexingService::new();
    *crate::lock_ok(&service.status) = IndexingStatus::Optimizing;
    let refused = service.delete_index_within(&db_path, Instant::now());
    // Before asserting: dropping the service waits on a status that settles.
    *crate::lock_ok(&service.status) = IndexingStatus::Idle;
    let err = refused.unwrap_err();
    assert!(err.contains("still stopping"), "{err}");
    assert_eq!(std::fs::read(&db_path).unwrap(), before);
    drop(service);
    std::fs::remove_dir_all(&dir).ok();
}

/// A stop sent while the service is idle (a delete sends one whatever runs)
/// can be handled only once the next start has claimed `Preparing`. Applied
/// to that run, it left it reading `Stopping` for good, beyond the reach of
/// any later stop; a stop names the run it was meant for.
#[test]
fn a_stop_sent_while_idle_does_not_stop_the_next_run() {
    let dir = tmp_dir("stale-stop");
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "indexed").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = run_config(&tree, &db_path);
    let service = IndexingService::new();
    let stale = service.stop_command();
    service
        .start_indexing(config.paths.indexing_paths.clone(), db_path.clone(), config)
        .unwrap();
    // Queued behind the start: handled once the run is under way.
    service.command_tx.send(stale).unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while service.is_active() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!service.is_active(), "{:?}", service.get_status());
    let conn = db::open_existing(&db_path, false).unwrap();
    let stamp = crate::db::repo::get_last_full_index(&conn).unwrap();
    assert!(stamp.is_some(), "the run was stopped");
    drop(conn);
    drop(service);
    std::fs::remove_dir_all(&dir).ok();
}

/// The status a claimed run starts from, as `start_indexing` leaves it.
fn claimed(service: &IndexingService) {
    service.force_status(IndexingStatus::Preparing {
        start_time: Instant::now(),
        step: PrepStep::Starting,
    });
}

/// A run that panics ends in `Error` and releases its connection, as any
/// failed run: unwound past, it left the status in flight for good, refusing
/// every later start. Here the panic lands after a stop took the status, and
/// a report from outside is refused until the run has ended.
#[test]
fn a_panicking_run_reports_an_error_not_a_wedge() {
    let service = IndexingService::new();
    claimed(&service);
    IndexingService::run_contained(
        &service.status,
        &service.db_connection,
        || {
            let conn = Connection::open_in_memory().unwrap();
            *crate::lock_ok(&service.db_connection) = Some(Arc::new(Mutex::new(conn)));
            service.request_stop();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !matches!(service.get_status(), IndexingStatus::Stopping) {
                assert!(Instant::now() < deadline, "the stop never landed");
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !service.report_error("outside".into()),
                "the run holds the status"
            );
            panic!("boom")
        },
        || unreachable!("a failed run is not optimized"),
    );
    match service.get_status() {
        IndexingStatus::Error(e) => assert_eq!(e, "indexing stopped unexpectedly: boom"),
        other => panic!("{other:?}"),
    }
    assert!(crate::lock_ok(&service.db_connection).is_none());
    assert!(!service.is_active());
    assert!(
        service.report_error("later".into()),
        "an ended run takes a report"
    );
}

/// A panic in maintenance is logged and the status still ends at `Idle`:
/// left at `Optimizing`, the run would count as in flight for good.
#[test]
fn a_panicking_optimize_still_ends_idle() {
    let service = IndexingService::new();
    claimed(&service);
    let mut seen = None;
    IndexingService::run_contained(
        &service.status,
        &service.db_connection,
        || Ok(()),
        || {
            seen = Some(service.get_status());
            panic!("optimize boom")
        },
    );
    assert!(matches!(seen, Some(IndexingStatus::Optimizing)), "{seen:?}");
    assert!(matches!(service.get_status(), IndexingStatus::Idle));
    let lines = crate::log::snapshot();
    let logged = "optimize stopped unexpectedly: optimize boom";
    assert!(lines.iter().any(|line| line.text.contains(logged)));
}

/// A run that fails ends in its own `Error`, unoptimized.
#[test]
fn a_failed_run_ends_in_its_error() {
    let service = IndexingService::new();
    claimed(&service);
    IndexingService::run_contained(
        &service.status,
        &service.db_connection,
        || Err("disk full".into()),
        || unreachable!("a failed run is not optimized"),
    );
    match service.get_status() {
        IndexingStatus::Error(e) => assert_eq!(e, "disk full"),
        other => panic!("{other:?}"),
    }
}

/// `Optimizing` is a run in flight: its thread still holds the database
/// and writes `Idle` once done, which a run started meanwhile would read as
/// its own end. A refused start keeps the run number too, so a stop for the
/// optimizing run still names it.
#[test]
fn a_start_during_optimizing_is_refused() {
    let service = IndexingService::new();
    service.force_status(IndexingStatus::Optimizing);
    let run = service.run.load(Ordering::SeqCst);
    let refused = service.start_indexing(
        vec!["/nonexistent/quicksearch-root".into()],
        "/nonexistent/quicksearch.db".into(),
        Config::default(),
    );
    let after = service.get_status();
    // Before asserting: dropping the service waits on a status that settles.
    service.force_status(IndexingStatus::Idle);
    assert!(refused.unwrap_err().contains("already running"));
    assert!(matches!(after, IndexingStatus::Optimizing), "{after:?}");
    assert_eq!(service.run.load(Ordering::SeqCst), run);
}

/// The delete waits for the stop, maintenance included, rather than a fixed
/// time, and nothing is left running against the file it removed.
#[test]
fn a_stopped_run_is_deleted_only_once_idle() {
    let dir = tmp_dir("delete-running");
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    for i in 0..200 {
        std::fs::write(tree.join(format!("f{i}.txt")), format!("body {i}")).unwrap();
    }
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = run_config(&tree, &db_path);
    let service = IndexingService::new();
    service
        .start_indexing(config.paths.indexing_paths.clone(), db_path.clone(), config)
        .unwrap();
    assert!(service.is_active(), "start_indexing claims the run at once");

    service.delete_index_for_rebuild(&db_path).unwrap();
    assert!(!service.is_active());
    assert!(!Path::new(&db_path).exists());
    for suffix in ["-wal", "-shm"] {
        assert!(
            !Path::new(&format!("{db_path}{suffix}")).exists(),
            "{suffix}"
        );
    }
    drop(service);
    std::fs::remove_dir_all(&dir).ok();
}

/// A reader nothing in this process can release (a duplicates scan's) fails
/// the delete on Windows for as long as it is open. Past the deadline that is
/// one attempt and an honest error, and the file is left whole.
#[cfg(windows)]
#[test]
fn a_reader_held_past_the_deadline_fails_the_delete() {
    let (dir, db_path) = built_index("delete-held");
    let reader = db::open_existing(&db_path, false).unwrap();
    let service = IndexingService::new();
    let failed = service.delete_index_within(&db_path, Instant::now());
    drop(reader);
    let err = failed.unwrap_err();
    assert!(err.contains("Failed to delete"), "{err}");
    assert!(Path::new(&db_path).exists());
    drop(service);
    std::fs::remove_dir_all(&dir).ok();
}

/// A reader that closes while the delete is under way does not fail it:
/// whichever thread gets past the barrier first, the delete retries until
/// the file goes.
#[cfg(windows)]
#[test]
fn a_reader_closed_during_the_delete_lets_it_through() {
    let (dir, db_path) = built_index("delete-released");
    let reader = db::open_existing(&db_path, false).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let closer = {
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            drop(reader);
        })
    };
    let service = IndexingService::new();
    barrier.wait();
    let deleted = service.delete_index_within(&db_path, Instant::now() + STOP_GRACE);
    closer.join().unwrap();
    deleted.unwrap();
    assert!(!Path::new(&db_path).exists());
    drop(service);
    std::fs::remove_dir_all(&dir).ok();
}

/// A context with no run behind it: `maintaining` only ever touches the
/// status, never the database.
fn cx_for<'a>(
    config: &'a Config,
    stop: &'a Arc<AtomicBool>,
    status: &Arc<Mutex<IndexingStatus>>,
) -> pipeline::RunCx<'a> {
    let conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
    pipeline::RunCx::new(
        Arc::new(Mutex::new(conn)),
        config,
        "/nowhere",
        stop,
        status.clone(),
    )
    .unwrap()
}

fn running_status(maintenance: Option<MaintenanceStep>) -> Arc<Mutex<IndexingStatus>> {
    Arc::new(Mutex::new(IndexingStatus::Running {
        start_time: Instant::now(),
        roots: vec![progress(RootPhase::Extracting, 100, None)],
        maintenance,
    }))
}

/// The counters freeze for the length of the step either way; the only
/// question is whether the status says so.
#[test]
fn an_upkeep_step_is_published_for_exactly_as_long_as_it_runs() {
    let config = Config::default();
    let stop = Arc::new(AtomicBool::new(false));
    let status = running_status(None);
    let cx = cx_for(&config, &stop, &status);

    let guard = cx.maintaining(MaintenanceStep::Checkpoint);
    match &*crate::lock_ok(&status) {
        IndexingStatus::Running {
            roots, maintenance, ..
        } => {
            assert_eq!(*maintenance, Some(MaintenanceStep::Checkpoint));
            assert_eq!(roots.len(), 1, "the published snapshot was replaced");
            assert_eq!(roots[0].walked, 100, "the counters were rewritten");
        }
        other => panic!("the run went missing: {:?}", other),
    }

    drop(guard);
    match &*crate::lock_ok(&status) {
        IndexingStatus::Running {
            roots, maintenance, ..
        } => {
            assert_eq!(*maintenance, None, "the step outlived its work");
            assert_eq!(
                roots[0].walked, 100,
                "the roots went missing on the way out"
            );
        }
        other => panic!("the run went missing: {:?}", other),
    };
}

/// The command thread owns the `Stopping` transition, as `publish_status`
/// keeps, and an upkeep step is no exception: neither end of it may
/// resurrect a run that has been told to stop.
#[test]
fn an_upkeep_step_never_clobbers_a_stop() {
    let config = Config::default();
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(IndexingStatus::Stopping));
    let cx = cx_for(&config, &stop, &status);

    let guard = cx.maintaining(MaintenanceStep::MergingText);
    assert!(matches!(*crate::lock_ok(&status), IndexingStatus::Stopping));
    drop(guard);
    assert!(matches!(*crate::lock_ok(&status), IndexingStatus::Stopping));
}

/// A fresh snapshot means the writer is back on files; a step that ended
/// while the round was mid-flight must not linger on it.
#[test]
fn a_status_publish_clears_the_step() {
    let dir = tmp_dir("publish-clears-step");
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    let config = config_with(vec![dir.to_string_lossy().into_owned()], &[]);
    let stop = Arc::new(AtomicBool::new(false));
    let status = running_status(Some(MaintenanceStep::RootCounts));

    run_indexing_with(&config, &db_path, &stop, &status).expect("indexed");
    match &*crate::lock_ok(&status) {
        IndexingStatus::Running { maintenance, .. } => assert_eq!(*maintenance, None),
        other => panic!("the run went missing: {:?}", other),
    };

    std::fs::remove_dir_all(&dir).ok();
}

fn progress(phase: RootPhase, walked: usize, walk_total: Option<usize>) -> RootProgress {
    RootProgress {
        root: "/r".to_string(),
        phase,
        walked,
        walk_total,
        extracted: 0,
        extract_total: None,
        current_file: None,
        active_workers: 0,
        total_workers: 0,
    }
}

/// An overtaken estimate would pin the bar at 100% and read as a hang; past
/// the walk the exact count wins: `find` counts tree entries where `walked`
/// counts walkable files, so the estimate reads far high.
#[test]
fn the_walk_denominator_prefers_the_best_available_count() {
    for (label, phase, walked, estimate, want) in [
        ("estimate", RootPhase::Walking, 100, Some(1000), Some(1000)),
        ("no count yet", RootPhase::Walking, 100, None, None),
        (
            "overtaken",
            RootPhase::Walking,
            1500,
            Some(1000),
            Some(1500),
        ),
        (
            "exact",
            RootPhase::Extracting,
            261_088,
            Some(6_677_062),
            Some(261_088),
        ),
        (
            "exact, no estimate",
            RootPhase::Extracting,
            261_088,
            None,
            Some(261_088),
        ),
        (
            "done",
            RootPhase::Done,
            261_088,
            Some(6_677_062),
            Some(261_088),
        ),
        (
            "done, no estimate",
            RootPhase::Done,
            261_088,
            None,
            Some(261_088),
        ),
    ] {
        assert_eq!(
            progress(phase, walked, estimate).walk_denominator(),
            want,
            "{label}"
        );
    }
}

#[test]
fn overall_progress_sums_both_halves_of_every_root() {
    let walking = progress(RootPhase::Walking, 100, Some(1000));
    let mut extracting = progress(RootPhase::Extracting, 500, Some(9999));
    extracting.extracted = 200;
    extracting.extract_total = Some(400);

    let o = overall_progress(&[walking, extracting]);
    assert_eq!(o.processed, 100 + 500 + 200);
    // 1000 (estimate) + 500 (exact) + 400 (extraction scope).
    assert_eq!(o.total, Some(1900));
}

/// Counted into both halves, so the bar cannot jump when the count lands.
#[test]
fn an_uncounted_extraction_contributes_only_its_walk() {
    let mut counting = progress(RootPhase::Extracting, 500, None);
    counting.extracted = 7;
    let o = overall_progress(&[counting]);
    assert_eq!(o.processed, 500);
    assert_eq!(o.total, Some(500));
}

#[test]
fn one_uncounted_walking_root_leaves_the_whole_total_unknown() {
    let known = progress(RootPhase::Done, 10, Some(10));
    let unknown = progress(RootPhase::Walking, 5, None);
    let o = overall_progress(&[known, unknown]);
    assert_eq!(o.processed, 15);
    assert_eq!(o.total, None);
    assert_eq!(o.fraction(), None);
}

#[test]
fn a_run_past_its_walks_needs_no_estimate_at_all() {
    let roots = [
        progress(RootPhase::Done, 10, None),
        progress(RootPhase::Extracting, 5, None),
    ];
    assert_eq!(overall_progress(&roots).total, Some(15));
}

/// Real figures from a run that, with the `find` estimate held past the walk,
/// would finish at 7,999,707 / 10,562,418 = 76% and never fill the bar.
#[test]
fn a_finished_run_reaches_exactly_one_hundred_percent() {
    let roots: Vec<RootProgress> = [
        (261_088usize, 238_929usize),
        (45_202, 10_339),
        (2_000_000, 2_574_506),
        (300_000, 221_641),
        (1_508_061, 839_941),
    ]
    .iter()
    .map(|&(walked, extracted)| {
        let mut p = progress(RootPhase::Done, walked, Some(walked * 2));
        p.extracted = extracted;
        p.extract_total = Some(extracted);
        p
    })
    .collect();

    let o = overall_progress(&roots);
    assert_eq!(o.processed, 4_114_351 + 3_885_356);
    assert_eq!(o.total, Some(o.processed), "the estimate must be gone");
    assert_eq!(o.fraction(), Some(1.0));
}

#[test]
fn nothing_to_do_is_a_known_zero_with_no_fraction() {
    let o = overall_progress(&[]);
    assert_eq!(o.processed, 0);
    assert_eq!(o.total, Some(0));

    let o = overall_progress(&[progress(RootPhase::Done, 0, None)]);
    assert_eq!(o.total, Some(0));
    assert_eq!(o.fraction(), None, "no division by zero");
}

/// `walked` can outrun a denominator that was exact when taken (files added
/// since the last walk stored it) and must stop the bar at full.
#[test]
fn work_past_an_exact_denominator_stops_the_bar_at_full() {
    let mut p = progress(RootPhase::Done, 10, None);
    p.extracted = 100;
    // A counted scope the writes then overran; an uncounted one would be
    // left out of both halves and prove nothing here.
    p.extract_total = Some(0);
    let o = overall_progress(&[p]);
    assert_eq!(o.processed, 110);
    assert_eq!(o.total, Some(10));
    assert_eq!(o.fraction(), Some(1.0), "the bar stops at full");
}

/// A safety valve, not a second tuning knob: the wal-index is reached through
/// an mmap, so a write the filesystem cannot back is a SIGBUS, not a clean
/// failure. `0` means "never force a checkpoint"; bounded by the volume like
/// any other value, it keeps that meaning without a special case.
#[test]
fn the_wal_cap_is_bounded_by_the_volume() {
    use super::pipeline::wal_cap_for_free;
    let configured: u64 = 512 * 1024 * 1024;
    for (label, cfg, free, want) in [
        ("roomy", configured, 500 * 1024 * 1024 * 1024, configured),
        // Exactly enough: floor plus four times the log.
        (
            "just enough",
            configured,
            128 * 1024 * 1024 + configured * 4,
            configured,
        ),
        // 1 GiB free: 896 MiB above the floor, a quarter of which is 224 MiB.
        ("tight", configured, 1024 * 1024 * 1024, 224 * 1024 * 1024),
        ("disabled, tight", 0, 1024 * 1024 * 1024, 224 * 1024 * 1024),
        ("full", configured, 0, crate::config::MINIMUM_WAL_SIZE),
        (
            "nearly full",
            configured,
            1024,
            crate::config::MINIMUM_WAL_SIZE,
        ),
        (
            "at the floor",
            configured,
            127 * 1024 * 1024,
            crate::config::MINIMUM_WAL_SIZE,
        ),
    ] {
        assert_eq!(wal_cap_for_free(cfg, free), want, "{label}");
    }
    assert!(
        wal_cap_for_free(0, 500 * 1024 * 1024 * 1024) > 100 * 1024 * 1024 * 1024,
        "disabled checkpoints on a roomy disk are effectively never"
    );
}

/// `testutil::seed_index` bounds its log through [`WalGuard`]'s two halves
/// rather than through a run's `after_round`, so they are checked on their
/// own: not due under the cap, due past it, and re-armed a cap of growth
/// later once the checkpoint has emptied the log.
#[test]
fn the_wal_guard_checkpoints_once_the_log_passes_its_cap() {
    let (_dir, db) = crate::testutil::Scratch::db("walguard");
    let db = db.to_str().unwrap();
    let processing = crate::config::ProcessingConfig {
        // Raised to the floor, which no amount of free space lowers.
        maximum_wal_size: 1,
        ..Default::default()
    };
    let conn = db::open_or_recreate(db, &processing).unwrap();
    conn.execute_batch("PRAGMA wal_autocheckpoint = 0; CREATE TABLE ballast(b BLOB);")
        .unwrap();
    let wal_bytes = || std::fs::metadata(format!("{}-wal", db)).map_or(0, |m| m.len());
    // One commit of about a MiB each.
    let grow = |mib: usize| {
        for _ in 0..mib {
            conn.execute("INSERT INTO ballast VALUES (zeroblob(1048576))", [])
                .unwrap();
        }
    };

    let mut wal = WalGuard::new(&processing, db);
    assert!(!wal.due(), "a fresh log is not due");
    grow(8);
    assert!(!wal.due(), "half the cap is not due");
    grow(12);
    assert!(
        wal_bytes() > crate::config::MINIMUM_WAL_SIZE,
        "the fixture must pass the cap"
    );
    assert!(wal.due(), "a log past the cap is due");

    wal.checkpoint(&conn);
    assert_eq!(
        wal_bytes(),
        0,
        "with no reader, the checkpoint empties the log"
    );
    assert!(!wal.due(), "re-armed from the emptied log");
    grow(8);
    assert!(!wal.due(), "the next cap is counted from the checkpoint");
}
