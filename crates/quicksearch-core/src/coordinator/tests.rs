use super::*;

fn wait_for<F: Fn() -> bool>(what: &str, timeout: Duration, check: F) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {}", what);
}

/// A coordinator whose wake-up goes nowhere; wake-up tests pass their own
/// sink.
fn start_coord(config: Config) -> IndexCoordinator {
    IndexCoordinator::start(config, Arc::new(|| {})).unwrap()
}

fn start_coord_watching(config: Config, watcher: WatcherConfig) -> IndexCoordinator {
    IndexCoordinator::start_with_watcher_config(config, Arc::new(|| {}), watcher).unwrap()
}

struct Fixture {
    dir: PathBuf,
    db: PathBuf,
    config: Config,
}

impl Fixture {
    fn new(auto: bool) -> Fixture {
        let scratch = crate::testutil::scratch_dir("coord");
        let dir = scratch.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        let db = scratch.join("index.sqlite");
        let mut config = Config::default();
        config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
        config.paths.database_path = db.to_string_lossy().into_owned();
        config.indexing.auto_index = auto;
        Fixture { dir, db, config }
    }

    fn file_count(&self) -> i64 {
        match db::open_existing(&self.db.to_string_lossy(), false) {
            Ok(conn) => conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap_or(0),
            Err(_) => -1,
        }
    }

    /// What a run starting now would still find to reconcile.
    fn outstanding_work(&self, config: &Config) -> IndexWork {
        crate::scope::outstanding_work(&self.db.to_string_lossy(), config).unwrap()
    }

    fn stored_value(&self, key: &str) -> Option<String> {
        let conn = db::open_existing(&self.db.to_string_lossy(), false).unwrap();
        conn.query_row(
            "SELECT value FROM config_validation WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .ok()
    }

    /// Leave a real, complete index on disk and stop — the starting point
    /// for every test about what the *next* launch does.
    fn seed_index(&self) {
        let mut manual = self.config.clone();
        manual.indexing.auto_index = false;
        let coord = start_coord(manual);
        coord.reindex_now();
        wait_for("seed index", Duration::from_secs(30), || {
            coord.state().last_full_index.is_some()
        });
        coord.shutdown();
    }

    /// Rewrite the stamp the periodic scheduler measures against.
    fn stamp_last_index(&self, ts: u64) {
        let conn = db::open_existing(&self.db.to_string_lossy(), true).unwrap();
        db::repo::set_last_full_index(&conn, ts).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
        std::fs::remove_file(&self.db).ok();
    }
}

#[test]
fn manual_mode_starts_idle_and_reindex_now_round_trips() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("one.txt"), "manual mode content").unwrap();

    let coord = start_coord(f.config.clone());
    assert_eq!(coord.state().mode, IndexMode::ManualStopped);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(f.file_count(), -1, "no run without a command");

    coord.reindex_now();
    wait_for("run to complete", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped
    });
    assert_eq!(f.file_count(), 1);
    coord.shutdown();
}

/// Closing the window during a prune must not wait the prune out.
#[test]
fn shutdown_during_a_prune_does_not_wait_for_it() {
    let f = Fixture::new(false);
    // Enough rows that the scan cannot finish before the shutdown lands.
    for i in 0..400 {
        std::fs::write(f.dir.join(format!("f{}.log", i)), "dropped content").unwrap();
    }
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(60), || {
        coord.state().last_full_index.is_some() && f.file_count() == 401
    });

    let mut narrowed = f.config.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    // One row per statement: a huge index's shape at a test's size.
    narrowed.processing.batch_size = 1;
    coord.apply_config(narrowed.clone());

    let asked = Instant::now();
    coord.shutdown();
    let took = asked.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "shutdown waited {:?} for the prune",
        took
    );
    // The work was left unfinished, not raced through.
    assert!(
        f.outstanding_work(&narrowed).touches_index(),
        "the prune ran to completion, so this proves nothing about waiting"
    );
}

/// A narrowed filter is applied to the stored index without a prompt and
/// without a run — including in manual mode.
#[test]
fn manual_mode_prunes_a_narrowed_filter_without_running() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped && f.file_count() == 2
    });
    let stamped = coord.state().last_full_index;

    // Appended, not replaced: replacing would also be a widening.
    let mut narrowed = f.config.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    coord.apply_config(narrowed);

    wait_for(
        "the log entry to be pruned",
        Duration::from_secs(20),
        || f.file_count() == 1,
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        coord.state().mode,
        IndexMode::ManualStopped,
        "still stopped"
    );
    assert_eq!(
        coord.state().last_full_index,
        stamped,
        "no run happened — narrowing needs no walk"
    );
    coord.shutdown();
}

/// A prune that finishes must record what it reconciled against, or every
/// later run rescans every row to redo work already done.
#[test]
fn a_completed_prune_records_what_it_reconciled() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && f.file_count() == 2
    });

    let mut narrowed = f.config.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    assert!(
        f.outstanding_work(&narrowed).touches_index(),
        "the edit must be one the index does not yet reflect"
    );

    coord.apply_config(narrowed.clone());
    wait_for(
        "the log entry to be pruned",
        Duration::from_secs(20),
        || f.file_count() == 1,
    );
    // The prune and the stamp are two steps of one tick; the count above
    // can be observed between them.
    wait_for("the prune to be recorded", Duration::from_secs(20), || {
        !f.outstanding_work(&narrowed).touches_index()
    });

    assert!(
        f.outstanding_work(&narrowed).is_empty(),
        "a run starting now has nothing left to reconcile"
    );
    coord.shutdown();
}

/// A finished prune keeps reporting itself, so a millisecond prune is
/// still visible.
#[test]
fn a_finished_prune_keeps_reporting_itself_for_a_while() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && f.file_count() == 2
    });

    let mut narrowed = f.config.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    coord.apply_config(narrowed);
    wait_for("the summary to appear", Duration::from_secs(20), || {
        matches!(coord.state().reconcile, Some(ReconcileState::Finished(_)))
    });

    let Some(ReconcileState::Finished(progress)) = coord.state().reconcile else {
        panic!("the summary went away as soon as it arrived");
    };
    assert_eq!(progress.deleted, 1, "the log entry, and only it");
    coord.shutdown();
}

/// A summary is a report of what just happened, not a state the app sits in.
#[test]
fn a_summary_stops_being_fresh_once_the_linger_is_up() {
    let now = Instant::now();
    assert!(summary_is_fresh(now, now));
    assert!(summary_is_fresh(
        now,
        now + RECONCILE_SUMMARY_LINGER - Duration::from_millis(1)
    ));
    assert!(!summary_is_fresh(now, now + RECONCILE_SUMMARY_LINGER));
    assert!(!summary_is_fresh(now, now + RECONCILE_SUMMARY_LINGER * 60));
}

/// Settings only a rebuild can satisfy stay at the values the index was
/// *built* with: stamping them would clear a rebuild the user declined.
#[test]
fn a_prune_never_records_settings_only_a_rebuild_can_satisfy() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && f.file_count() == 2
    });
    let built_with = f.stored_value("hash_length").unwrap();

    // Change the hash length and decline the rebuild it needs: the
    // coordinator's copy now disagrees with the stored hashes.
    let mut rebuilt = f.config.clone();
    rebuilt.processing.hash_length = f.config.processing.hash_length * 2;
    coord.apply_config(rebuilt.clone());
    std::thread::sleep(Duration::from_millis(500));

    // An unrelated reconcilable edit; its stamp must not carry the hash
    // length.
    let mut narrowed = rebuilt.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    coord.apply_config(narrowed);
    wait_for(
        "the log entry to be pruned",
        Duration::from_secs(20),
        || f.file_count() == 1,
    );
    std::thread::sleep(Duration::from_millis(500));

    assert_eq!(
        f.stored_value("hash_length"),
        Some(built_with),
        "the recorded hash length still describes the stored hashes, so the \
         rebuild prompt survives an unrelated prune"
    );
    coord.shutdown();
}

/// Widening deletes nothing; the walk starts on its own and returns manual
/// mode to stopped, the way `reindex_now` does.
#[test]
fn manual_mode_reindexes_a_widened_filter_and_returns_to_stopped() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("later.log"), "arrives later").unwrap();

    let mut narrowed = f.config.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    let coord = start_coord(narrowed.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped && f.file_count() == 1
    });

    coord.apply_config(f.config.clone());
    wait_for(
        "the widened walk to find it",
        Duration::from_secs(20),
        || f.file_count() == 2 && coord.state().mode == IndexMode::ManualStopped,
    );
    coord.shutdown();
}

#[test]
fn stopping_cancels_a_queued_walk_but_not_a_queued_prune() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped && f.file_count() == 2
    });
    let stamped = coord.state().last_full_index;

    // Narrow and widen at once; turning auto off in the same edit is what
    // enters manual-stopped with the plan already queued.
    let extra = f.dir.join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(extra.join("new.txt"), "in the new root").unwrap();
    let mut edited = f.config.clone();
    edited.indexing.ignore_patterns.push("*.log".into());
    edited
        .paths
        .indexing_paths
        .push(extra.to_string_lossy().into_owned());
    edited.indexing.auto_index = false;
    coord.set_mode(IndexMode::ManualStopped);
    coord.apply_config(edited);

    wait_for("the prune to land", Duration::from_secs(20), || {
        f.file_count() == 1
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        coord.state().last_full_index,
        stamped,
        "the walk the widening asked for was cancelled by the stop"
    );
    assert_eq!(f.file_count(), 1, "and the new root is still unindexed");
    coord.shutdown();
}

/// Short debounce windows so trailing-edge events flush within test
/// timeouts (production defaults are 30 s / 2 s).
fn fast_watcher() -> WatcherConfig {
    WatcherConfig {
        throttle_window: Duration::from_millis(300),
        tick_interval: Duration::from_millis(100),
        pending_settle: Duration::from_millis(200),
        pending_max_defer: Duration::from_secs(3),
        ..WatcherConfig::default()
    }
}

/// The interval is measured against the on-disk stamp, so time spent
/// closed counts: come back late and the reindex is owed on the first tick.
#[test]
fn a_lapsed_stamp_starts_a_run_at_startup() {
    let mut f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    f.seed_index();

    let stamped = now_unix() - 7200;
    f.stamp_last_index(stamped);
    f.config.indexing.auto_index = true;
    f.config.indexing.reindex_interval_minutes = 1;

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for(
        "a run without waiting out the interval",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some_and(|t| t > stamped),
    );
    coord.shutdown();
}

/// A stamp still inside the interval is not due; a launch must leave it
/// alone.
#[test]
fn a_fresh_stamp_waits_out_the_interval() {
    let mut f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    f.seed_index();

    let stamped = now_unix();
    f.stamp_last_index(stamped);
    f.config.indexing.auto_index = true;
    f.config.indexing.reindex_interval_minutes = 600;

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        coord.state().last_full_index,
        Some(stamped),
        "a stamp inside the interval must not start a run"
    );
    coord.shutdown();
}

/// A stamp ahead of the clock (NTP correction, migrated index) must read
/// as due, not "just indexed".
#[test]
fn a_stamp_from_the_future_is_treated_as_due() {
    let mut f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    f.seed_index();

    let ahead = now_unix() + 86_400;
    f.stamp_last_index(ahead);
    f.config.indexing.auto_index = true;
    f.config.indexing.reindex_interval_minutes = 600;

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    // The run re-stamps from *this* machine's clock, ending the skew.
    wait_for(
        "a run despite the future stamp",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some_and(|t| t < ahead),
    );
    coord.shutdown();
}

/// A *scheduled* run must wake the frontend: nothing the user did starts
/// it, so no repaint would otherwise observe it.
#[test]
fn a_run_it_schedules_itself_wakes_the_frontend() {
    use std::sync::atomic::AtomicUsize;

    let mut f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    f.seed_index();

    let stamped = now_unix() - 7200;
    f.stamp_last_index(stamped);
    f.config.indexing.auto_index = true;
    f.config.indexing.reindex_interval_minutes = 1;

    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let coord = IndexCoordinator::start_with_watcher_config(
        f.config.clone(),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
        fast_watcher(),
    )
    .unwrap();

    wait_for("the scheduled run", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some_and(|t| t > stamped)
    });
    let during = wakes.load(Ordering::Relaxed);
    assert!(during > 0, "the run started without waking the frontend");

    // The wake is an edge into work, not a heartbeat: an idle coordinator
    // must not wake the frontend once per tick.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        wakes.load(Ordering::Relaxed),
        during,
        "an idle coordinator must not keep waking the frontend"
    );
    coord.shutdown();
}

// --- targeted updates (see `IndexCoordinator::update_paths`) --------------

impl Fixture {
    /// The `mtime` the index holds for one path, or `None` if it has no row.
    fn stored_mtime(&self, path: &std::path::Path) -> Option<i64> {
        let conn = db::open_existing(&self.db.to_string_lossy(), false).ok()?;
        conn.query_row(
            "SELECT mtime FROM files WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .ok()
    }
}

/// The point of the whole thing: the frontend has just read a file the user is
/// looking at, and the index catches up even though indexing is stopped — with
/// no watcher running and no full run scheduled.
#[test]
fn update_paths_indexes_one_file_with_indexing_stopped() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    f.seed_index();
    assert_eq!(f.file_count(), 1);

    let coord = start_coord(f.config.clone());
    let added = f.dir.join("added.txt");
    std::fs::write(&added, "written while indexing was stopped").unwrap();
    coord.update_paths(vec![added.clone()]);

    wait_for("targeted insert", Duration::from_secs(20), || {
        f.stored_mtime(&added).is_some()
    });
    assert_eq!(
        coord.state().mode,
        IndexMode::ManualStopped,
        "a targeted update started a run"
    );
    assert!(
        coord.state().last_full_index.is_some(),
        "the seed stamp was disturbed"
    );
    coord.shutdown();
}

/// The same call is how a row is *validated*: submitting a path the index
/// already agrees with must not rewrite it, which is what makes it cheap
/// enough for the frontend to submit whatever it just looked at.
#[test]
fn update_paths_leaves_a_row_that_already_agrees_alone() {
    let f = Fixture::new(false);
    let file = f.dir.join("steady.txt");
    std::fs::write(&file, "unchanged").unwrap();
    f.seed_index();
    let before = f.stored_mtime(&file).expect("seeded");

    let coord = start_coord(f.config.clone());
    coord.update_paths(vec![file.clone()]);
    // No state change to wait on, so wait out a few ticks instead.
    std::thread::sleep(Duration::from_secs(3));

    assert_eq!(f.stored_mtime(&file), Some(before));
    assert_eq!(f.file_count(), 1);
    coord.shutdown();
}

/// A path outside every indexed root is not the index's to hold, however it
/// was submitted: a result renamed into an un-indexed folder must not follow
/// the row into the index at its new home.
#[test]
fn update_paths_ignores_a_path_outside_every_root() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    f.seed_index();
    assert_eq!(f.file_count(), 1);

    // A sibling of the indexed tree, under the same scratch parent.
    let outside = f.dir.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let stray = outside.join("moved-here.txt");
    std::fs::write(&stray, "renamed out of the index").unwrap();

    let coord = start_coord(f.config.clone());
    coord.update_paths(vec![stray.clone()]);
    std::thread::sleep(Duration::from_secs(3));

    assert_eq!(
        f.stored_mtime(&stray),
        None,
        "an un-indexed folder gained a row"
    );
    assert_eq!(f.file_count(), 1);
    coord.shutdown();
    std::fs::remove_dir_all(&outside).ok();
}

/// A row whose file has gone leaves the index too — the frontend hands over
/// the path, not a verb, so the coordinator decides from what is on disk.
#[test]
fn update_paths_removes_a_row_whose_file_is_gone() {
    let f = Fixture::new(false);
    let file = f.dir.join("doomed.txt");
    std::fs::write(&file, "not for long").unwrap();
    f.seed_index();
    assert!(f.stored_mtime(&file).is_some());

    let coord = start_coord(f.config.clone());
    std::fs::remove_file(&file).unwrap();
    coord.update_paths(vec![file.clone()]);

    wait_for("targeted remove", Duration::from_secs(20), || {
        f.stored_mtime(&file).is_none()
    });
    coord.shutdown();
}

/// The single-writer rule still holds: a targeted update submitted while a
/// full run owns the database waits for it rather than opening a second
/// writer beside it.
#[test]
fn update_paths_waits_for_a_full_run_rather_than_racing_it() {
    let f = Fixture::new(false);
    for i in 0..400 {
        std::fs::write(f.dir.join(format!("f{i}.txt")), "body").unwrap();
    }
    let coord = start_coord(f.config.clone());
    coord.reindex_now();

    let added = f.dir.join("late.txt");
    std::fs::write(&added, "submitted mid-run").unwrap();
    coord.update_paths(vec![added.clone()]);

    wait_for("run finished", Duration::from_secs(60), || {
        coord.state().last_full_index.is_some()
    });
    wait_for(
        "targeted insert after the run",
        Duration::from_secs(20),
        || f.stored_mtime(&added).is_some(),
    );
    coord.shutdown();
}

#[test]
fn auto_mode_runs_initial_index_and_applies_watcher_events() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 1
    });

    // New file → watcher event → incremental application.
    std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
    wait_for("incremental add", Duration::from_secs(20), || {
        f.file_count() == 2
    });

    // Deletion sweeps the row.
    std::fs::remove_file(f.dir.join("later.txt")).unwrap();
    wait_for("incremental remove", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    coord.shutdown();
}

/// A healthy watcher reports its own size for the GUI's "watching N
/// folders".
#[test]
fn auto_mode_reports_an_active_watcher() {
    let f = Fixture::new(true);
    std::fs::create_dir_all(f.dir.join("sub")).unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());

    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    match coord.state().watcher {
        WatcherStatus::Active { dirs } => assert_eq!(dirs, 2, "root + sub"),
        other => panic!("expected Active, got {:?}", other),
    }
    coord.shutdown();
}

/// Exceeding the watch budget must surface as `Disabled`, and the periodic
/// reindex must keep the index fresh.
#[test]
fn exceeding_the_watch_cap_disables_updates_but_keeps_indexing() {
    let f = Fixture::new(true);
    for sub in ["a", "b", "c"] {
        std::fs::create_dir_all(f.dir.join(sub)).unwrap();
    }
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();

    let watcher_config = WatcherConfig {
        max_watched_dirs: 2,
        ..fast_watcher()
    };
    let coord = start_coord_watching(f.config.clone(), watcher_config);

    wait_for("watcher disabled", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Disabled { .. })
    });
    match coord.state().watcher {
        WatcherStatus::Disabled { reason } => assert_eq!(
            reason,
            WatchError::TooManyDirectories { dirs: 2, cap: 2 },
            "the cap, not some other failure"
        ),
        other => panic!("expected Disabled, got {:?}", other),
    }

    // Periodic reindex is the fallback and must still run: mode stays
    // Auto, only the watcher is off.
    assert_eq!(coord.state().mode, IndexMode::Auto);
    wait_for(
        "full run despite no watcher",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some() && f.file_count() == 1,
    );
    coord.shutdown();
}

#[test]
fn manual_mode_reports_the_watcher_off() {
    let f = Fixture::new(false);
    let coord = start_coord(f.config.clone());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(coord.state().watcher, WatcherStatus::Off);
    coord.shutdown();
}

#[test]
fn stopping_turns_the_watcher_status_off() {
    let f = Fixture::new(true);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });

    coord.set_mode(IndexMode::ManualStopped);
    wait_for("watcher off", Duration::from_secs(10), || {
        coord.state().watcher == WatcherStatus::Off
    });
    coord.shutdown();
}

#[test]
fn manual_stop_drops_watcher_and_events() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    let coord = start_coord(f.config.clone());
    wait_for("initial index", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    coord.set_mode(IndexMode::ManualStopped);
    wait_for("mode switch", Duration::from_secs(5), || {
        coord.state().mode == IndexMode::ManualStopped
    });

    std::fs::write(f.dir.join("unseen.txt"), "never indexed").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(f.file_count(), 1, "stopped mode must not index new files");
    assert_eq!(coord.state().queued_events, 0);

    coord.shutdown();
}

/// A config whose `auto_index` disagrees with the running mode switches it.
#[test]
fn applying_a_config_switches_the_mode_to_match_auto_index() {
    let f = Fixture::new(true);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });

    let mut manual = f.config.clone();
    manual.indexing.auto_index = false;
    coord.apply_config(manual.clone());
    wait_for("manual mode", Duration::from_secs(10), || {
        let s = coord.state();
        s.mode == IndexMode::ManualStopped && s.watcher == WatcherStatus::Off
    });

    let mut auto = manual.clone();
    auto.indexing.auto_index = true;
    coord.apply_config(auto);
    wait_for("automatic mode", Duration::from_secs(20), || {
        let s = coord.state();
        s.mode == IndexMode::Auto && matches!(s.watcher, WatcherStatus::Active { .. })
    });

    coord.shutdown();
}

#[test]
fn apply_config_with_new_root_then_reindex_indexes_it() {
    // Add directories, apply, "Start indexing now": the run must pick up
    // the new roots promptly.
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("first.txt"), "one").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial index", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    let extra_root = f
        .dir
        .parent()
        .unwrap()
        .join(format!("qs-coord-extra-{}", std::process::id()));
    std::fs::create_dir_all(&extra_root).unwrap();
    std::fs::write(extra_root.join("second.txt"), "two").unwrap();

    let mut new_cfg = f.config.clone();
    new_cfg
        .paths
        .indexing_paths
        .push(extra_root.to_string_lossy().into_owned());
    coord.apply_config(new_cfg);
    coord.reindex_now();

    wait_for("new root indexed", Duration::from_secs(20), || {
        f.file_count() == 2
    });
    coord.shutdown();
    std::fs::remove_dir_all(&extra_root).ok();
}

/// Only the removal root needs applying — its range sweep covers the rest.
#[test]
fn collapsing_reduces_a_tree_deletion_to_its_root() {
    let mut pending = HashMap::new();
    let dir = PathBuf::from("/x/tree");
    enqueue(&mut pending, FsEvent::Remove(dir.clone()));
    for i in 0..500 {
        enqueue(
            &mut pending,
            FsEvent::Remove(dir.join(format!("sub{}/f{}.txt", i % 5, i))),
        );
        enqueue(
            &mut pending,
            FsEvent::Remove(dir.join(format!("sub{}", i % 5))),
        );
    }
    // Not under the removed tree, and not a removal: both must survive.
    enqueue(&mut pending, FsEvent::Remove(PathBuf::from("/x/treehouse")));
    enqueue(&mut pending, FsEvent::Create(dir.join("reborn.txt")));

    collapse_pending_removals(&mut pending);

    let mut left: Vec<String> = pending
        .keys()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![
            "/x/tree".to_string(),
            "/x/tree/reborn.txt".to_string(),
            "/x/treehouse".to_string(),
        ],
        "only the removal root, the re-creation, and the prefix sibling remain"
    );
}

#[test]
fn collapsing_a_queue_without_removals_is_a_no_op() {
    let mut pending = HashMap::new();
    enqueue(&mut pending, FsEvent::Create(PathBuf::from("/x/a")));
    enqueue(&mut pending, FsEvent::Modify(PathBuf::from("/x/a/b")));
    collapse_pending_removals(&mut pending);
    assert_eq!(pending.len(), 2);
}

/// Deleting a populated directory must land as one queued removal, not one
/// per file, and the rows must actually go.
#[test]
fn a_deleted_directory_is_applied_as_a_single_collapsed_removal() {
    let f = Fixture::new(true);
    let tree = f.dir.join("tree");
    for i in 0..40 {
        let p = tree.join(format!("sub{}/f{:03}.txt", i % 4, i));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("body {}", i)).unwrap();
    }
    std::fs::write(f.dir.join("keep.txt"), "survivor").unwrap();

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial index", Duration::from_secs(30), || {
        f.file_count() == 41
    });

    std::fs::remove_dir_all(&tree).unwrap();
    wait_for(
        "subtree removed from the index",
        Duration::from_secs(30),
        || f.file_count() == 1,
    );

    coord.shutdown();
}

/// The queue must not be applied while a full run owns the database, and
/// the events must survive to be applied once it finishes.
#[test]
fn a_deletion_during_a_full_run_is_queued_then_applied() {
    let f = Fixture::new(false); // manual: runs happen only when asked
    for i in 0..300 {
        let p = f.dir.join(format!("d{}/f{:03}.txt", i % 6, i));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("body {}", i)).unwrap();
    }

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    coord.reindex_now();
    wait_for("first run", Duration::from_secs(30), || {
        coord.state().last_full_index.is_some() && f.file_count() == 300
    });

    // Auto mode so the watcher is live, then delete while a run is going.
    coord.set_mode(IndexMode::Auto);
    wait_for("watcher active", Duration::from_secs(30), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    coord.reindex_now();
    std::fs::remove_dir_all(f.dir.join("d0")).unwrap();

    // 300 files, 50 of them under d0.
    wait_for(
        "deletion applied after the run",
        Duration::from_secs(60),
        || f.file_count() == 250,
    );

    coord.shutdown();
}

#[test]
fn enqueue_last_wins_and_rename_splits() {
    let mut pending = HashMap::new();
    let a = PathBuf::from("/x/a");
    enqueue(&mut pending, FsEvent::Create(a.clone()));
    enqueue(&mut pending, FsEvent::Modify(a.clone()));
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending.get(&a), Some(FsEvent::Modify(_))));

    enqueue(
        &mut pending,
        FsEvent::Rename {
            from: a.clone(),
            to: PathBuf::from("/x/b"),
        },
    );
    assert_eq!(pending.len(), 2);
    assert!(matches!(pending.get(&a), Some(FsEvent::Remove(_))));
    assert!(matches!(
        pending.get(&PathBuf::from("/x/b")),
        Some(FsEvent::Create(_))
    ));
}

#[test]
fn clear_index_deletes_db_and_stays_manual() {
    let f = Fixture::new(true); // auto mode — clear must not auto-resurrect
    std::fs::write(f.dir.join("a.txt"), "content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial index", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    coord.clear_index();
    wait_for("index deleted", Duration::from_secs(10), || {
        f.file_count() == -1 // open_existing fails: file gone
    });
    wait_for("manual mode", Duration::from_secs(5), || {
        coord.state().mode == IndexMode::ManualStopped
    });
    assert_eq!(coord.state().last_full_index, None);

    // Give the (now manual) coordinator a few ticks: the index must
    // stay deleted rather than being rebuilt by the scheduler.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(f.file_count(), -1, "cleared index must stay cleared");

    coord.shutdown();
}

#[test]
fn nested_roots_refuse_to_run() {
    let f = Fixture::new(false);
    let child = f.dir.join("nested");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("x.txt"), "content").unwrap();

    let mut config = f.config.clone();
    config
        .paths
        .indexing_paths
        .push(child.to_string_lossy().into_owned());
    let coord = start_coord(config);
    coord.reindex_now();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        f.file_count(),
        -1,
        "a run over nested roots must be refused (no DB created)"
    );
    coord.shutdown();
}

#[test]
fn shutdown_is_idempotent_and_joins() {
    let f = Fixture::new(false);
    let coord = start_coord(f.config.clone());
    coord.shutdown();
    coord.shutdown(); // second call is a no-op
}
