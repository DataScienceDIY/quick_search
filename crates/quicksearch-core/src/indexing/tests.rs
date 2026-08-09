use super::pipeline::RootPipeline;
use super::*;
use crate::extract::Registry;
use crate::file_handling::ExtractCursor;
use crate::walk::walk_indexable_files;
use std::sync::atomic::AtomicUsize;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    // Canonical: the temp dir itself may sit behind a symlink
    // (/tmp -> /private/tmp), and these tests compare walked paths
    // against the root they passed in.
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

/// The progress line reports whichever pool the root's current phase is
/// running; a walk pool that already exited must not be read.
#[test]
fn worker_counts_follow_the_phase() {
    let dir = tmp_dir("phase-workers");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    let db_path = dir.join("index.db").to_string_lossy().into_owned();
    drop(db::open_or_recreate(&db_path, "trigram").unwrap());

    let root = dir.to_string_lossy().into_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let suspend = Arc::new(AtomicBool::new(false));
    let walk = walk_indexable_files(
        std::slice::from_ref(&root),
        false,
        false,
        crate::config::IgnoreSet::compile(&[]).unwrap(),
        &db_path,
        Config::default(),
        Arc::new(Registry::default_set()),
        stop.clone(),
        suspend.clone(),
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
        suspend,
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
        extract_total: 0,
        extracted: 0,
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

/// One full run over `config`'s roots, driven directly so the caller owns
/// the stop flag. Returns when the run does.
fn run_with(config: &Config, db_path: &str, stop: &Arc<AtomicBool>) -> Result<(), String> {
    IndexingService::run_indexing(
        &Arc::new(Mutex::new(IndexingStatus::Idle)),
        &config.paths.indexing_paths,
        db_path,
        stop,
        &Arc::new(AtomicBool::new(false)),
        config,
        &Arc::new(Mutex::new(None)),
        &db::InterruptSlot::default(),
    )
}

fn outstanding_work(db_path: &str, config: &Config) -> crate::config::IndexWork {
    crate::scope::outstanding_work(db_path, config).unwrap()
}

/// The count `root`'s last clean walk recorded, if any.
fn stored_walk_count(db_path: &str, root: &str) -> Option<usize> {
    let conn = db::open_existing(db_path, false).unwrap();
    crate::db::repo::get_root_walk_count(&conn, root)
}

/// A walk that could not read part of its tree must not record its count:
/// the figure is the next run's progress denominator, and nothing ever
/// re-derives it.
///
/// Unix only: on Windows the `icacls` deny ACE does not bind the owning
/// process reliably enough to test against.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_keeps_the_walk_count_unrecorded() {
    let dir = tmp_dir("unreadable-count");
    // The tree is a subdirectory, so the index and its WAL sidecars do not
    // sit inside the root being walked and count as files.
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

    // And the same tree, readable, does record one — otherwise the
    // assertion above would hold for a guard that never records anything.
    run_with(&config, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
    assert_eq!(
        stored_walk_count(&db_path, &root),
        Some(2),
        "a clean walk records its file count"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A run the stop flag cut short is the other way a walk comes back with a
/// partial count.
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

    // Already set, so the run stops at its first check — the deterministic
    // stand-in for a shutdown part-way through a walk.
    run_with(&config, &db_path, &Arc::new(AtomicBool::new(true))).unwrap();
    assert_eq!(stored_walk_count(&db_path, &root), None);

    std::fs::remove_dir_all(&dir).ok();
}

/// A reconcile the stop flag cut short must leave the stored fingerprint
/// alone: stamping it would tell every later run the index already matches,
/// and nothing would ever revisit the rows the scan had not reached.
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

    // Already set, so the reconcile aborts on its first check — the
    // deterministic stand-in for a shutdown mid-scan.
    run_with(&narrowed, &db_path, &Arc::new(AtomicBool::new(true))).unwrap();
    assert_eq!(
        outstanding_work(&db_path, &narrowed),
        pending,
        "the same work is still owed"
    );

    // And the run that is allowed to finish both applies and records it.
    run_with(&narrowed, &db_path, &Arc::new(AtomicBool::new(false))).unwrap();
    assert!(
        outstanding_work(&db_path, &narrowed).is_empty(),
        "a completed run leaves nothing to reconcile"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A root's progress with only the fields the denominator rules read.
fn progress(phase: RootPhase, walked: usize, walk_total: Option<usize>) -> RootProgress {
    RootProgress {
        root: "/r".to_string(),
        phase,
        walked,
        walk_total,
        extracted: 0,
        extract_total: 0,
        current_file: None,
        active_workers: 0,
        total_workers: 0,
    }
}

#[test]
fn a_walking_root_falls_back_to_the_find_estimate() {
    let p = progress(RootPhase::Walking, 100, Some(1000));
    assert_eq!(p.walk_denominator(), Some(1000));
}

#[test]
fn a_walking_root_without_a_count_yet_has_no_denominator() {
    let p = progress(RootPhase::Walking, 100, None);
    assert_eq!(p.walk_denominator(), None);
}

/// An estimate the walk has already overtaken is provably wrong, and a bar
/// pinned at 100% while the walk is still running reads as a hang.
#[test]
fn an_overtaken_estimate_is_raised_to_the_walked_count() {
    let p = progress(RootPhase::Walking, 1500, Some(1000));
    assert_eq!(p.walk_denominator(), Some(1500));
}

/// `find` counts tree entries where `walked` counts only walkable files, so
/// the estimate reads far high; once the walk ends the estimate must go.
#[test]
fn a_root_past_its_walk_uses_the_exact_count() {
    for phase in [RootPhase::Extracting, RootPhase::Done] {
        assert_eq!(
            progress(phase, 261_088, Some(6_677_062)).walk_denominator(),
            Some(261_088),
            "{:?} must not keep the estimate",
            phase
        );
        assert_eq!(
            progress(phase, 261_088, None).walk_denominator(),
            Some(261_088),
            "{:?} needs no estimate to have landed",
            phase
        );
    }
}

#[test]
fn overall_progress_sums_both_halves_of_every_root() {
    let mut walking = progress(RootPhase::Walking, 100, Some(1000));
    let mut extracting = progress(RootPhase::Extracting, 500, Some(9999));
    extracting.extracted = 200;
    extracting.extract_total = 400;
    walking.extracted = 0;

    let o = overall_progress(&[walking, extracting]);
    assert_eq!(o.processed, 100 + 500 + 200);
    // 1000 (estimate) + 500 (exact) + 400 (extraction scope).
    assert_eq!(o.total, Some(1900));
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

/// Roots past their walk carry their own totals, so a run whose counts
/// never landed still gains a percentage once the walks end.
#[test]
fn a_run_past_its_walks_needs_no_estimate_at_all() {
    let roots = [
        progress(RootPhase::Done, 10, None),
        progress(RootPhase::Extracting, 5, None),
    ];
    assert_eq!(overall_progress(&roots).total, Some(15));
}

/// The regression: with the `find` estimate held past the walk, the run
/// below finished at 7,999,707 / 10,562,418 = 76% and the bar never
/// filled. These are the real figures from that run.
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
        p.extract_total = extracted;
        p
    })
    .collect();

    let o = overall_progress(&roots);
    assert_eq!(o.processed, 4_114_351 + 3_885_356);
    assert_eq!(o.total, Some(o.processed), "the estimate must be gone");
    assert_eq!(o.fraction(), Some(1.0));
}

#[test]
fn a_run_with_nothing_to_do_has_no_fraction_to_show() {
    let o = overall_progress(&[progress(RootPhase::Done, 0, None)]);
    assert_eq!(o.total, Some(0));
    assert_eq!(o.fraction(), None, "no division by zero");
}

/// `walked` can outrun a denominator that was exact when taken — a root
/// re-walked through symlink aliases, say. The bar must stop at full.
#[test]
fn the_fraction_never_exceeds_one() {
    let mut p = progress(RootPhase::Done, 10, None);
    p.extracted = 100;
    let o = overall_progress(&[p]);
    assert_eq!(o.processed, 110);
    assert_eq!(o.total, Some(10));
    assert_eq!(o.fraction(), Some(1.0));
}

#[test]
fn a_run_with_no_roots_is_complete_rather_than_unknown() {
    let o = overall_progress(&[]);
    assert_eq!(o.processed, 0);
    assert_eq!(o.total, Some(0));
}
