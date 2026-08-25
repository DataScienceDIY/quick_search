use super::pipeline::RootPipeline;
use super::*;
use crate::extract::Registry;
use crate::file_handling::ExtractCursor;
use crate::walk::walk_indexable_files;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    // Canonical: the temp dir may sit behind a symlink (/tmp -> /private/tmp)
    // and these tests compare walked paths against the root they passed in.
    crate::testutil::scratch_dir_canonical(tag)
}

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

#[test]
fn worker_counts_follow_the_phase() {
    let dir = tmp_dir("phase-workers");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    drop(db::open_or_recreate(&db_path, "trigram").unwrap());

    let root = dir.to_string_lossy().into_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let walk = walk_indexable_files(
        std::slice::from_ref(&root),
        false,
        false,
        crate::config::IgnoreSet::compile(&[]).unwrap(),
        &db_path,
        Config::default(),
        Arc::new(Registry::default_set()),
        stop.clone(),
        3,
    );
    // An empty range, so the pass ends immediately — but its pool size is
    // fixed when it is built, which is what the display reports.
    let content = crate::content::extract_content(
        &db_path,
        &ExtractCursor::for_root(&dir.join("nothing").to_string_lossy()),
        Arc::new(Registry::default_set()),
        Config::default(),
        stop,
        2,
    );

    let mut p = RootPipeline {
        root,
        walk,
        count_total: Arc::new(AtomicUsize::new(0)),
        workers: 3,
        pending_updates: Vec::new(),
        pending_inserts: Vec::new(),
        walked: 0,
        walk_clean: true,
        phase: RootPhase::Walking,
        phase_started: Instant::now(),
        content: Some(content),
        ready: Vec::new(),
        written: 0,
        totals: None,
        current_file: None,
    };

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
    let mut conn = db::open_or_recreate(&db_path, "trigram").unwrap();
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
                    // the row sorts below every `ExtractCursor` range.
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

    let root = dir.to_string_lossy().into_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let config = Config::default();
    let walk = walk_indexable_files(
        std::slice::from_ref(&root),
        false,
        false,
        crate::config::IgnoreSet::compile(&[]).unwrap(),
        &db_path,
        config.clone(),
        Arc::new(Registry::default_set()),
        stop.clone(),
        1,
    );
    // An empty range: the pass reports `Finished` on its own, and the turn
    // has to keep going past that until `ready` is empty.
    let content = crate::content::extract_content(
        &db_path,
        &ExtractCursor::for_root(&dir.join("nothing").to_string_lossy()),
        Arc::new(Registry::default_set()),
        config.clone(),
        stop.clone(),
        1,
    );
    let mut p = RootPipeline {
        root,
        walk,
        count_total: Arc::new(AtomicUsize::new(0)),
        workers: 1,
        pending_updates: Vec::new(),
        pending_inserts: Vec::new(),
        walked: 0,
        walk_clean: true,
        phase: RootPhase::Extracting,
        phase_started: Instant::now(),
        content: Some(content),
        ready,
        written: 0,
        totals: None,
        current_file: None,
    };
    let mut cx = RunCx::new(
        conn_mutex.clone(),
        &config,
        &db_path,
        &stop,
        Arc::new(Mutex::new(IndexingStatus::Idle)),
    );
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

    drop(p);
    std::fs::remove_dir_all(&dir).ok();
}

fn run_with(config: &Config, db_path: &str, stop: &Arc<AtomicBool>) -> Result<(), String> {
    run_indexing_with(
        config,
        db_path,
        stop,
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
    let mut config = Config::default();
    config.paths.indexing_paths = vec![tree.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.clone();
    let root = normalize_root_string(&tree.to_string_lossy());

    crate::platform::deny_read(&locked).unwrap();
    let blocked = run_with(&config, &db_path, &Arc::new(AtomicBool::new(false)));
    // Restore before asserting, so a failure still leaves a removable tree.
    crate::platform::restore_read(&locked).ok();
    blocked.unwrap();

    assert_eq!(
        stored_walk_count(&db_path, &root),
        None,
        "a walk that could not read a directory saw only part of the tree"
    );

    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
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
    let mut config = Config::default();
    config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.clone();
    let root = normalize_root_string(&dir.to_string_lossy());

    run_with(&config, &db_path, &Arc::new(AtomicBool::new(true))).unwrap();
    assert_eq!(stored_walk_count(&db_path, &root), None);

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
    let mut config = Config::default();
    config.paths.indexing_paths = vec![tree.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.clone();
    config.indexing.content_extensions = vec!["txt".into()];
    let root = normalize_root_string(&tree.to_string_lossy());

    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();

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
    let mut config = Config::default();
    config.paths.indexing_paths = vec![tree.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.clone();
    let root = normalize_root_string(&tree.to_string_lossy());

    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
    let after_run = stored_root_counts(&db_path, &root).expect("recorded");
    assert_eq!(after_run.files, 1);

    std::fs::write(tree.join("b.txt"), "beta body").unwrap();
    std::fs::write(tree.join("c.txt"), "gamma body").unwrap();
    run_with(&config, &db_path, &Arc::new(AtomicBool::new(true))).unwrap();
    assert_eq!(
        stored_root_counts(&db_path, &root),
        Some(after_run),
        "a stopped run leaves the last completed run's figures alone"
    );

    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
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

    let mut config = Config::default();
    config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
    config.paths.database_path = db_path.clone();
    config.indexing.ignore_patterns = vec![];
    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();

    let mut narrowed = config.clone();
    narrowed.indexing.ignore_patterns = vec!["*.log".into()];
    let pending = outstanding_work(&db_path, &narrowed);
    assert!(pending.touches_index(), "the narrowing has rows to remove");

    run_with(&narrowed, &db_path, &Arc::new(AtomicBool::new(true))).unwrap();
    assert_eq!(
        outstanding_work(&db_path, &narrowed),
        pending,
        "the same work is still owed"
    );

    run_with(&narrowed, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
    assert!(
        outstanding_work(&db_path, &narrowed).is_empty(),
        "a completed run leaves nothing to reconcile"
    );

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

/// The command thread owns the `Stopping` transition — the rule
/// `publish_status` has always kept, and an upkeep step is no exception:
/// neither end of it may resurrect a run that has been told to stop.
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
/// the walk the exact count wins — `find` counts tree entries where `walked`
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
    let mut walking = progress(RootPhase::Walking, 100, Some(1000));
    let mut extracting = progress(RootPhase::Extracting, 500, Some(9999));
    extracting.extracted = 200;
    extracting.extract_total = Some(400);
    walking.extracted = 0;

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
    counting.extract_total = None;
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

/// Regression: with the `find` estimate held past the walk, this run finished
/// at 7,999,707 / 10,562,418 = 76% and the bar never filled; real figures.
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

/// `walked` can outrun a denominator that was exact when taken — a root
/// re-walked through symlink aliases — and must stop the bar at full.
#[test]
fn overall_progress_edge_cases() {
    let o = overall_progress(&[]);
    assert_eq!(o.processed, 0);
    assert_eq!(o.total, Some(0));

    let o = overall_progress(&[progress(RootPhase::Done, 0, None)]);
    assert_eq!(o.total, Some(0));
    assert_eq!(o.fraction(), None, "no division by zero");

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
