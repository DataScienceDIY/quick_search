use super::*;
use crate::testutil::wait_for;

/// Check `holds` until `n` more ticks have run, and once after: a negative
/// ("nothing happened") is read over the ticks that would have done it, not
/// after a guessed sleep.
fn for_ticks(coord: &IndexCoordinator, n: u64, holds: impl Fn()) {
    let until = coord.ticks() + n;
    let what = format!("{} more ticks", n);
    wait_for(&what, Duration::from_secs(60), || {
        holds();
        coord.ticks() >= until
    });
    holds();
}

/// Shut `coord` down and wait for its thread to end: each command is handled
/// and published before the next is read, so all of them are in once it has.
/// `shutdown` alone stops waiting after `EXIT_GRACE`.
fn shut_down(coord: &IndexCoordinator) {
    coord.shutdown();
    wait_for(
        "the coordinator thread to end",
        Duration::from_secs(30),
        || coord.thread_ended(),
    );
}

/// Wait for `check`, polling every millisecond: for a moment inside a run
/// over a few hundred files, which lasts a fraction of a second and could
/// pass between two of `wait_for`'s polls.
fn catch(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Files the running walk has handed the writer, over every root; 0 while no
/// run is writing.
fn walked(coord: &IndexCoordinator) -> usize {
    match coord.state().activity {
        IndexingStatus::Running { roots, .. } => roots.iter().map(|r| r.walked).sum(),
        _ => 0,
    }
}

/// Whether the between-runs reconciliation has finished and published its
/// summary, with the mode and the queue beside it.
fn reconciled(coord: &IndexCoordinator) -> bool {
    matches!(coord.state().reconcile, Some(ReconcileState::Finished(_)))
}

/// Rows in the index at `db`; -1 when it does not open.
fn rows_in(db: &std::path::Path) -> i64 {
    match db::open_existing(&db.to_string_lossy(), false) {
        Ok(conn) => conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap_or(0),
        Err(_) => -1,
    }
}

fn start_coord(config: Config) -> IndexCoordinator {
    IndexCoordinator::start(config, Arc::new(|| {})).unwrap()
}

fn start_coord_watching(config: Config, watcher: WatcherConfig) -> IndexCoordinator {
    IndexCoordinator::start_with_watcher_config(config, Arc::new(|| {}), watcher).unwrap()
}

struct Fixture {
    _scratch: crate::testutil::Scratch,
    dir: PathBuf,
    db: PathBuf,
    config: Config,
}

impl Fixture {
    fn new(auto: bool) -> Fixture {
        let scratch = crate::testutil::Scratch::dir("coord");
        let dir = scratch.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        let db = scratch.join("index.sqlite");
        let mut config = Config::default();
        config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
        config.paths.database_path = db.to_string_lossy().into_owned();
        config.indexing.auto_index = auto;
        Fixture {
            _scratch: scratch,
            dir,
            db,
            config,
        }
    }

    /// The fixture's config with `*.log` files ignored. Appended, not
    /// replaced: replacing would also be a widening.
    fn narrowed(&self) -> Config {
        let mut narrowed = self.config.clone();
        narrowed.indexing.ignore_patterns.push("*.log".into());
        narrowed
    }

    /// A coordinator over `keep.txt` and `drop.log`, after its first run.
    fn indexed_pair(&self) -> IndexCoordinator {
        std::fs::write(self.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(self.dir.join("drop.log"), "dropped content").unwrap();
        let coord = start_coord(self.config.clone());
        coord.reindex_now();
        wait_for("initial run", Duration::from_secs(20), || {
            let s = coord.state();
            s.last_full_index.is_some()
                && s.mode == IndexMode::ManualStopped
                && self.file_count() == 2
        });
        coord
    }

    /// A manual fixture with one file indexed, switched to auto mode, its
    /// last-run stamp moved `offset` seconds from now. Returns the stamp.
    fn stamped_auto(offset: i64, interval_minutes: u64) -> (Fixture, u64) {
        let mut f = Fixture::new(false);
        std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
        f.seed_index();
        let stamp = now_unix().saturating_add_signed(offset);
        f.stamp_last_index(stamp);
        f.config.indexing.auto_index = true;
        f.config.indexing.reindex_interval_minutes = interval_minutes;
        (f, stamp)
    }

    fn file_count(&self) -> i64 {
        rows_in(&self.db)
    }

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

    fn stamp_last_index(&self, ts: u64) {
        let conn = db::open_existing(&self.db.to_string_lossy(), true).unwrap();
        db::repo::set_last_full_index(&conn, ts).unwrap();
    }

    /// Write `n` files at the top of the tree, `f0.txt` up.
    fn write_files(&self, n: usize) {
        for i in 0..n {
            std::fs::write(self.dir.join(format!("f{i}.txt")), format!("body {i}")).unwrap();
        }
    }

    /// One row per write batch and per writer turn: a run over a few hundred
    /// new files is then long enough to act inside.
    fn slow_writer(&mut self) {
        self.config.processing.batch_size = 1;
        self.config.processing.writer_turn_slice_ms = 0;
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
    assert!(!coord.is_indexing(), "nothing claimed the database yet");

    coord.reindex_now();
    wait_for("run to complete", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped
    });
    assert_eq!(f.file_count(), 1);
    // The cheap predicate and the full status must never disagree: a GUI
    // polling `is_indexing` decides cache lifetime by its falling edge.
    assert!(!coord.is_indexing());
    assert!(matches!(coord.state().activity, IndexingStatus::Idle));
    coord.shutdown();
}

/// A shutdown that finds a prune queued, or its first slice under way, ends
/// the thread within the grace and leaves the prune owed. It usually lands
/// before any slice; a slice cut short is `scope`'s
/// `cancelling_stops_the_scan_without_finishing_it`.
#[test]
fn shutdown_with_a_prune_queued_leaves_it_undone() {
    let f = Fixture::new(false);
    // Enough rows that a slice already under way cannot finish first.
    for i in 0..400 {
        std::fs::write(f.dir.join(format!("f{}.log", i)), "dropped content").unwrap();
    }
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    wait_for("initial run", Duration::from_secs(60), || {
        coord.state().last_full_index.is_some() && f.file_count() == 401
    });

    let mut narrowed = f.narrowed();
    // One row per statement: a huge index's shape at a test's size.
    narrowed.processing.batch_size = 1;
    coord.apply_config(narrowed.clone());

    // The wait is bounded either way; a thread still in the scan at the end
    // of the grace would be left running, not joined.
    coord.shutdown();
    assert!(
        coord.thread_ended(),
        "shutdown gave up on a thread still in the prune"
    );
    assert!(
        f.outstanding_work(&narrowed).touches_index(),
        "the prune ran to completion"
    );
}

#[test]
fn manual_mode_prunes_a_narrowed_filter_without_running() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    let stamped = coord.state().last_full_index;

    coord.apply_config(f.narrowed());

    wait_for("the prune to finish", Duration::from_secs(20), || {
        reconciled(&coord)
    });
    assert_eq!(f.file_count(), 1, "the log entry is pruned");
    // A run the prune started would read as one by now.
    assert_eq!(
        coord.state().mode,
        IndexMode::ManualStopped,
        "still stopped"
    );
    assert!(!coord.is_indexing(), "narrowing needs no walk");
    assert_eq!(
        coord.state().last_full_index,
        stamped,
        "no run happened: narrowing needs no walk"
    );
    coord.shutdown();
}

#[test]
fn a_completed_prune_records_what_it_reconciled() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();

    let narrowed = f.narrowed();
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

#[test]
fn a_finished_prune_keeps_reporting_itself_for_a_while() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();

    coord.apply_config(f.narrowed());
    wait_for("the summary to appear", Duration::from_secs(20), || {
        matches!(coord.state().reconcile, Some(ReconcileState::Finished(_)))
    });

    let Some(ReconcileState::Finished(progress)) = coord.state().reconcile else {
        panic!("the summary went away as soon as it arrived");
    };
    assert_eq!(progress.deleted, 1, "the log entry, and only it");
    coord.shutdown();
}

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

#[test]
fn a_prune_never_records_settings_only_a_rebuild_can_satisfy() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    let built_with = f.stored_value("hash_length").unwrap();

    let mut rebuilt = f.config.clone();
    rebuilt.processing.hash_length = f.config.processing.hash_length * 2;
    coord.apply_config(rebuilt.clone());

    let mut narrowed = rebuilt.clone();
    narrowed.indexing.ignore_patterns.push("*.log".into());
    coord.apply_config(narrowed);
    // The summary is published after the record.
    wait_for("the prune recorded", Duration::from_secs(20), || {
        reconciled(&coord)
    });
    assert_eq!(f.file_count(), 1, "the log entry is pruned");

    assert_eq!(
        f.stored_value("hash_length"),
        Some(built_with),
        "the recorded hash length still describes the stored hashes, so the \
         rebuild prompt survives an unrelated prune"
    );
    coord.shutdown();
}

#[test]
fn manual_mode_reindexes_a_widened_filter_and_returns_to_stopped() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("later.log"), "arrives later").unwrap();

    let narrowed = f.narrowed();
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
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
    std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();
    let coord = start_coord(f.config.clone());
    // Asked for, so it need not wait for the first tick.
    coord.reindex_now();
    wait_for("the first run", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && !coord.is_indexing() && f.file_count() == 2
    });
    let stamped = coord.state().last_full_index;

    // Narrow and widen at once; turning auto off in the same edit is what
    // enters manual-stopped with the plan already queued. Beside the tree,
    // not in it: a nested root would be refused, not cancelled.
    let extra = f.dir.parent().unwrap().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(extra.join("new.txt"), "in the new root").unwrap();
    let mut edited = f.narrowed();
    edited
        .paths
        .indexing_paths
        .push(extra.to_string_lossy().into_owned());
    edited.indexing.auto_index = false;
    coord.apply_config(edited);

    wait_for("the prune to finish", Duration::from_secs(20), || {
        reconciled(&coord)
    });
    assert_eq!(coord.state().mode, IndexMode::ManualStopped);
    assert!(
        !coord.is_indexing(),
        "the walk the widening asked for was cancelled by the stop"
    );
    assert_eq!(coord.state().last_full_index, stamped);
    assert_eq!(
        f.file_count(),
        1,
        "the prune landed, and the new root is still unindexed"
    );
    coord.shutdown();
}

/// Short debounce windows so trailing-edge events apply within test
/// timeouts (production defaults are 2 s / 30 s).
fn fast_watcher() -> WatcherConfig {
    WatcherConfig {
        pending_settle: Duration::from_millis(200),
        pending_max_defer: Duration::from_secs(3),
        ..WatcherConfig::default()
    }
}

#[test]
fn a_lapsed_stamp_starts_a_run_at_startup() {
    let (f, stamped) = Fixture::stamped_auto(-7200, 1);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for(
        "a run without waiting out the interval",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some_and(|t| t > stamped),
    );
    coord.shutdown();
}

#[test]
fn a_fresh_stamp_waits_out_the_interval() {
    let (f, stamped) = Fixture::stamped_auto(0, 600);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    // The first tick would start a due run, and the next see it running or
    // re-read its stamp.
    for_ticks(&coord, 2, || {
        assert!(
            !coord.is_indexing(),
            "a stamp inside the interval must not start a run"
        );
        assert_eq!(coord.state().last_full_index, Some(stamped));
    });
    coord.shutdown();
}

/// A stamp ahead of the clock happens: NTP correction, migrated index.
#[test]
fn a_stamp_from_the_future_is_treated_as_due() {
    let (f, ahead) = Fixture::stamped_auto(86_400, 600);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    // The run re-stamps from *this* machine's clock, ending the skew.
    wait_for(
        "a run despite the future stamp",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some_and(|t| t < ahead),
    );
    coord.shutdown();
}

#[test]
fn a_scheduled_run_wakes_the_frontend_once() {
    use std::sync::atomic::AtomicUsize;

    let (f, stamped) = Fixture::stamped_auto(-7200, 1);
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

    // The watcher's arrival wakes the frontend too, once.
    wait_for("the scheduled run", Duration::from_secs(20), || {
        let s = coord.state();
        s.last_full_index.is_some_and(|t| t > stamped)
            && matches!(s.watcher, WatcherStatus::Active { .. })
    });
    let during = wakes.load(Ordering::Relaxed);
    assert!(during > 0, "the run started without waking the frontend");

    for_ticks(&coord, 2, || {
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            during,
            "an idle coordinator must not keep waking the frontend"
        )
    });
    coord.shutdown();
}

// --- targeted updates (see `IndexCoordinator::update_paths`) --------------

impl Fixture {
    fn stored_mtime(&self, path: &std::path::Path) -> Option<i64> {
        let conn = db::open_existing(&self.db.to_string_lossy(), false).ok()?;
        conn.query_row(
            "SELECT mtime FROM files WHERE parent || name = ?1",
            [path.to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .ok()
    }

    /// Send `paths` in one batch with `marker`, a file it writes under a
    /// root, and wait until the batch is applied: the marker's row shows it
    /// reached the index, an empty queue that nothing is left over.
    fn update_and_settle(
        &self,
        coord: &IndexCoordinator,
        mut paths: Vec<PathBuf>,
        marker: PathBuf,
    ) {
        std::fs::write(&marker, "shows the batch applied").unwrap();
        paths.push(marker.clone());
        coord.update_paths(paths);
        wait_for("the batch applied", Duration::from_secs(20), || {
            self.stored_mtime(&marker).is_some() && coord.state().queued_events == 0
        });
    }
}

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

/// A path re-sent on every live refresh, as a growing file on screen is,
/// waits for the queue to settle instead of being re-indexed every tick.
#[test]
fn update_paths_waits_for_the_queue_to_settle() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    f.seed_index();
    let settle = WatcherConfig {
        pending_settle: Duration::from_secs(1),
        pending_max_defer: Duration::from_secs(20),
        ..WatcherConfig::default()
    };
    let coord = start_coord_watching(f.config.clone(), settle);
    let added = f.dir.join("growing.log");
    std::fs::write(&added, "first line").unwrap();

    let spam_until = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < spam_until {
        coord.update_paths(vec![added.clone()]);
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        f.stored_mtime(&added),
        None,
        "applied while the path was still being re-sent"
    );
    wait_for("the settled insert", Duration::from_secs(20), || {
        f.stored_mtime(&added).is_some()
    });
    coord.shutdown();
}

#[test]
fn update_paths_leaves_a_row_that_already_agrees_alone() {
    let f = Fixture::new(false);
    let file = f.dir.join("steady.txt");
    std::fs::write(&file, "unchanged").unwrap();
    f.seed_index();
    let before = f.stored_mtime(&file).expect("seeded");

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    f.update_and_settle(&coord, vec![file.clone()], f.dir.join("marker.txt"));

    assert_eq!(f.stored_mtime(&file), Some(before));
    assert_eq!(f.file_count(), 2, "the seeded row and the marker's");
    coord.shutdown();
}

#[test]
fn update_paths_ignores_a_path_outside_every_root() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    f.seed_index();
    assert_eq!(f.file_count(), 1);

    let outside = f.dir.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let stray = outside.join("moved-here.txt");
    std::fs::write(&stray, "renamed out of the index").unwrap();

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    f.update_and_settle(&coord, vec![stray.clone()], f.dir.join("marker.txt"));

    assert_eq!(
        f.stored_mtime(&stray),
        None,
        "an un-indexed folder gained a row"
    );
    assert_eq!(f.file_count(), 2, "the seeded row and the marker's");
    coord.shutdown();
    std::fs::remove_dir_all(&outside).ok();
}

/// The live view shows a vanished file as gone; its row stays until the
/// watcher or the next run removes it, since a display cannot tell a deletion
/// from an unplugged drive.
#[test]
fn update_paths_keeps_a_row_whose_file_is_gone() {
    let f = Fixture::new(false);
    let file = f.dir.join("doomed.txt");
    std::fs::write(&file, "not for long").unwrap();
    f.seed_index();
    let before = f.stored_mtime(&file).expect("seeded");

    let coord = start_coord(f.config.clone());
    std::fs::remove_file(&file).unwrap();
    f.update_and_settle(&coord, vec![file.clone()], f.dir.join("marker.txt"));

    assert_eq!(
        f.stored_mtime(&file),
        Some(before),
        "a targeted update removed a row"
    );
    coord.shutdown();
}

#[test]
fn update_paths_on_a_directory_or_missing_path_deletes_nothing() {
    let f = Fixture::new(false);
    let (kept, gone) = (f.dir.join("kept"), f.dir.join("gone"));
    for dir in [&kept, &gone] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("one.txt"), "in a folder").unwrap();
    }
    let swapped = f.dir.join("swapped.txt");
    std::fs::write(&swapped, "a file, then a folder").unwrap();
    f.seed_index();
    assert_eq!(f.file_count(), 3);

    let coord = start_coord(f.config.clone());
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::remove_file(&swapped).unwrap();
    std::fs::create_dir(&swapped).unwrap();
    f.update_and_settle(
        &coord,
        vec![kept.clone(), gone.clone(), swapped.clone()],
        f.dir.join("marker.txt"),
    );

    // The three seeded rows and the marker's.
    assert_eq!(f.file_count(), 4);
    assert!(
        f.stored_mtime(&gone.join("one.txt")).is_some(),
        "a missing folder lost its rows"
    );
    assert!(
        f.stored_mtime(&swapped).is_some(),
        "a file that became a folder lost its row"
    );
    coord.shutdown();
}

/// What the live view sends for a root whose drive was pulled: every row it
/// shows, each read as gone. None is removed; the next run records the root
/// unreadable rather than emptied.
#[test]
fn a_root_emptied_like_an_unplugged_drive_keeps_rows() {
    let (f, a, b) = Fixture::two_roots(false);
    std::fs::create_dir_all(a.join("sub")).unwrap();
    std::fs::write(a.join("sub").join("deep.txt"), "below the root").unwrap();
    std::fs::write(a.join("top.txt"), "at the root").unwrap();
    f.seed_index();
    let before = f.rows_under(&a);
    assert_eq!(before, 3);

    // Emptied in place, as a mount point reads once its drive is gone.
    let away = f.dir.join("away");
    std::fs::create_dir(&away).unwrap();
    for entry in std::fs::read_dir(&a).unwrap() {
        let entry = entry.unwrap();
        std::fs::rename(entry.path(), away.join(entry.file_name())).unwrap();
    }
    let stored: Vec<PathBuf> = {
        let conn = db::open_existing(&f.db.to_string_lossy(), false).unwrap();
        let mut stmt = conn.prepare("SELECT parent || name FROM files").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|p| PathBuf::from(p.unwrap())).collect()
    };
    let coord = start_coord(f.config.clone());
    f.update_and_settle(&coord, stored, b.join("marker.txt"));

    assert_eq!(f.rows_under(&a), before, "an emptied root lost rows");
    coord.shutdown();
}

/// Written once the walk is past the root's listing, so only the queue can
/// bring the file in, and queued while the run still holds the index.
#[test]
fn update_paths_waits_for_a_full_run_rather_than_racing_it() {
    let mut f = Fixture::new(false);
    f.slow_writer();
    f.write_files(400);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    coord.reindex_now();
    catch("the run under way", || walked(&coord) > 0);

    let added = f.dir.join("late.txt");
    std::fs::write(&added, "submitted mid-run").unwrap();
    coord.update_paths(vec![added.clone()]);
    wait_for("the target queued", Duration::from_secs(20), || {
        coord.state().queued_events >= 1
    });
    assert!(coord.is_indexing(), "the run ended before the target came");

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

// --- the tick keeps time (see `Inner::run`) -------------------------------

/// Commands arriving faster than the tick interval, as a growing file on
/// screen sends them, must not postpone the tick that drains the watcher.
#[test]
fn a_command_stream_does_not_starve_the_tick() {
    let f = Fixture::new(true);
    let steady = f.dir.join("steady.txt");
    std::fs::write(&steady, "on screen").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 1
    });
    wait_for("the watcher", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });

    let ticks_before = coord.ticks();
    let later = f.dir.join("later.txt");
    std::fs::write(&later, "arrived during the stream").unwrap();
    let mut sent = 0;
    while sent < 100 && f.stored_mtime(&later).is_none() {
        coord.update_paths(vec![steady.clone()]);
        sent += 1;
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        f.stored_mtime(&later).is_some(),
        "the watcher's event was not applied while {} commands flowed",
        sent
    );
    assert!(coord.ticks() > ticks_before, "no tick ran between commands");
    coord.shutdown();
}

/// A path re-sent faster than it can settle is applied once its first
/// sending has waited `pending_max_defer`.
#[test]
fn a_trickled_target_is_applied_by_max_defer() {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    f.seed_index();
    let trickle = WatcherConfig {
        pending_settle: Duration::from_secs(1),
        pending_max_defer: Duration::from_secs(2),
        ..WatcherConfig::default()
    };
    let coord = start_coord_watching(f.config.clone(), trickle);
    let target = f.dir.join("trickled.txt");
    std::fs::write(&target, "re-sent on every refresh").unwrap();

    let mut sent = 0;
    while sent < 50 && f.stored_mtime(&target).is_none() {
        coord.update_paths(vec![target.clone()]);
        sent += 1;
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        f.stored_mtime(&target).is_some(),
        "still unapplied after {} sendings",
        sent
    );
    coord.shutdown();
}

/// Reconciliation waits out a full run at the tick's pace. It used to poll
/// every millisecond for the whole run, about 1000 ticks a second.
#[test]
fn a_config_change_during_a_run_does_not_busy_poll() {
    let f = Fixture::new(false);
    for i in 0..400 {
        let p = f.dir.join(format!("d{}/f{:03}.txt", i % 8, i));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("body {}", i)).unwrap();
    }
    let coord = start_coord(f.config.clone());
    let ticks_before = coord.ticks();
    let started = Instant::now();
    coord.reindex_now();
    // Queued behind the start, so it is handled while the run it started
    // holds the database, with reconciliation work to wait on.
    coord.apply_config(f.narrowed());
    wait_for("the run to end", Duration::from_secs(60), || {
        coord.state().last_full_index.is_some()
    });
    let secs = started.elapsed().as_secs_f64();
    let ticks = coord.ticks() - ticks_before;
    assert!(
        ticks as f64 <= 2.0 * secs + 2.0,
        "{} ticks in {:.2} s",
        ticks,
        secs
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

    std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
    wait_for("incremental add", Duration::from_secs(20), || {
        f.file_count() == 2
    });

    std::fs::remove_file(f.dir.join("later.txt")).unwrap();
    wait_for("incremental remove", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    coord.shutdown();
}

/// A root configured as a link to a folder is walked and watched, and every
/// row, from the full run or from the watcher, is keyed by the canonical
/// path: the link's spelling would be a second key for the same file.
#[test]
#[cfg(unix)]
fn a_file_created_under_a_symlinked_root_is_indexed_under_its_canonical_path() {
    let mut f = Fixture::new(true);
    let link = f.dir.parent().unwrap().join("link");
    std::os::unix::fs::symlink(&f.dir, &link).unwrap();
    f.config.paths.indexing_paths = vec![link.to_string_lossy().into_owned()];
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 1
    });
    wait_for("the link root watched", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { dirs: 1 })
    });

    std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
    wait_for("incremental add", Duration::from_secs(20), || {
        f.file_count() == 2
    });

    let real = f.dir.canonicalize().unwrap();
    let want: Vec<String> = ["later.txt", "seed.txt"]
        .iter()
        .map(|name| crate::file_handling::path_to_db_string(&real.join(name)))
        .collect();
    let conn = db::open_existing(&f.db.to_string_lossy(), false).unwrap();
    let mut stmt = conn
        .prepare("SELECT parent || name FROM files ORDER BY 1")
        .unwrap();
    let stored: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(stored, want);
    coord.shutdown();
}

// --- incremental writes obey the walk's scope ------------------------------

impl Fixture {
    /// Rows stored anywhere under `dir`.
    fn rows_under(&self, dir: &std::path::Path) -> i64 {
        let root = crate::file_handling::normalize_root_string(&dir.to_string_lossy());
        let range = crate::file_handling::RootRange::for_root(&root);
        let conn = db::open_existing(&self.db.to_string_lossy(), false).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM files WHERE parent >= ?1 AND parent < ?2",
            [&range.lo, &range.hi],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Roots `a/` and `b/` inside the tree, a file in each.
    fn two_roots(auto: bool) -> (Fixture, PathBuf, PathBuf) {
        let mut f = Fixture::new(auto);
        let (a, b) = (f.dir.join("a"), f.dir.join("b"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("seed.txt"), "indexed from the start").unwrap();
        }
        f.config.paths.indexing_paths = vec![
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        (f, a, b)
    }

    /// The fixture's config with `root` its only root.
    fn only(&self, root: &std::path::Path) -> Config {
        let mut config = self.config.clone();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config
    }
}

/// With the index inside a root, its own `-wal` and `-shm` events never
/// queue: applied, each would be the commit that reports the next, and
/// opening one drops SQLite's locks (see `index_file_set`).
#[test]
fn an_index_inside_a_root_goes_quiet() {
    let mut f = Fixture::new(true);
    f.db = f.dir.join("index.sqlite");
    f.config.paths.database_path = f.db.to_string_lossy().into_owned();
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 1
    });
    wait_for("the watcher", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    // A real file still arrives, and its commit is one more round of the
    // index's own events.
    std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
    wait_for("incremental add", Duration::from_secs(20), || {
        f.file_count() == 2
    });

    for_ticks(&coord, 5, || {
        assert_eq!(
            coord.state().queued_events,
            0,
            "the index's own files queued"
        )
    });
    let conn = db::open_existing(&f.db.to_string_lossy(), false).unwrap();
    let own: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE name LIKE 'index.sqlite%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(own, 0, "the index indexed itself");
    coord.shutdown();
}

/// An event still queued under a root just removed writes nothing: the
/// reconciliation drops that root's rows, and the event would put one back
/// under a folder no longer indexed.
#[test]
fn removing_a_root_drops_its_queued_events() {
    let (f, a, b) = Fixture::two_roots(true);
    // Slow enough to settle that the root goes while the event waits.
    let slow = WatcherConfig {
        pending_settle: Duration::from_secs(2),
        pending_max_defer: Duration::from_secs(30),
        ..WatcherConfig::default()
    };
    let coord = start_coord_watching(f.config.clone(), slow);
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 2
    });
    wait_for("the watcher", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });

    std::fs::write(b.join("new.txt"), "written as its root goes").unwrap();
    wait_for("the event queued", Duration::from_secs(20), || {
        coord.state().queued_events > 0
    });
    coord.apply_config(f.only(&a));
    // Not before the drop: the queue waits out the reconciliation.
    wait_for("the queue applied", Duration::from_secs(20), || {
        coord.state().queued_events == 0
    });
    assert_eq!(f.rows_under(&b), 0, "a row under the removed root");
    assert_eq!(f.file_count(), 1);
    coord.shutdown();
}

/// `removing_a_root_drops_its_queued_events` for a path the live view
/// sent while its root was still configured.
#[test]
fn removing_a_root_drops_its_queued_targets() {
    let (f, a, b) = Fixture::two_roots(false);
    f.seed_index();
    assert_eq!(f.file_count(), 2);

    let coord = start_coord(f.config.clone());
    let added = b.join("new.txt");
    std::fs::write(&added, "on screen as its root goes").unwrap();
    coord.update_paths(vec![added]);
    coord.apply_config(f.only(&a));
    wait_for("the target queued", Duration::from_secs(20), || {
        coord.state().queued_events > 0
    });
    wait_for("the queue applied", Duration::from_secs(20), || {
        coord.state().queued_events == 0
    });
    wait_for("the root's rows dropped", Duration::from_secs(20), || {
        f.file_count() == 1
    });
    assert_eq!(f.rows_under(&b), 0, "a row under the removed root");
    coord.shutdown();
}

/// A live edit removing an offline root configured through a link drops the
/// link's spelling, which owns no rows. The record keeps the target, so the
/// next run drops the target's rows by its recorded spelling; forgotten, they
/// would sit outside every root for good.
#[test]
#[cfg(unix)]
fn removing_an_offline_linked_root_keeps_its_target_recorded() {
    let (mut f, target, other) = Fixture::two_roots(false);
    let link = f.dir.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    f.config.paths.indexing_paths = vec![
        link.to_string_lossy().into_owned(),
        other.to_string_lossy().into_owned(),
    ];
    f.seed_index();
    assert_eq!(f.rows_under(&target), 1);
    let recorded = crate::file_handling::normalize_root_string(&target.to_string_lossy());

    let away = f.dir.join("away");
    std::fs::rename(&target, &away).unwrap();
    let coord = start_coord(f.config.clone());
    coord.apply_config(f.only(&other));
    wait_for("the reconciliation", Duration::from_secs(20), || {
        matches!(coord.state().reconcile, Some(ReconcileState::Finished(_)))
    });
    let stamped = f.stored_value("indexing_path").unwrap();
    assert!(
        stamped.split('\n').any(|root| root == recorded),
        "the record forgot {}: {:?}",
        recorded,
        stamped
    );

    std::fs::rename(&away, &target).unwrap();
    coord.reindex_now();
    wait_for("the target's rows dropped", Duration::from_secs(20), || {
        f.rows_under(&target) == 0
    });
    assert_eq!(f.rows_under(&other), 1);
    coord.shutdown();
}

/// A live edit whose glob does not compile leaves the last good patterns in
/// force, and the queue keeps applying.
#[test]
fn a_live_bad_glob_does_not_stall_the_queue() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && f.file_count() == 1
    });
    let mut bad = f.config.clone();
    bad.indexing.ignore_patterns.push("[abc".into());
    let starts = coord.watcher_starts();
    coord.apply_config(bad);

    // The edit restarts the watcher, and a write made before its watches are
    // back goes unreported.
    wait_for("the watcher back", Duration::from_secs(20), || {
        coord.watcher_starts() > starts
            && matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    let later = f.dir.join("later.txt");
    std::fs::write(&later, "written after the edit").unwrap();
    wait_for("the write indexed", Duration::from_secs(20), || {
        f.stored_mtime(&later).is_some()
    });
    coord.shutdown();
}

/// The published count follows the coordinator's own writes, and an idle
/// coordinator does not go back to recount: a row slipped in behind its back
/// stays unreported until something it does changes the index.
#[test]
fn the_file_count_is_refreshed_by_writes_not_by_time() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial auto index", Duration::from_secs(20), || {
        coord.state().files == Some(1)
    });

    std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
    wait_for(
        "the count to follow an incremental add",
        Duration::from_secs(20),
        || coord.state().files == Some(2),
    );

    let conn = db::open_existing(&f.db.to_string_lossy(), true).unwrap();
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime, type, content_state)
         VALUES ('ghost.txt', '/nowhere/', 0, 0, 0, 3)",
        [],
    )
    .unwrap();
    drop(conn);
    for_ticks(&coord, 2, || {
        assert_eq!(coord.state().files, Some(2), "an idle tick recounted")
    });
    coord.shutdown();
}

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

    assert_eq!(coord.state().mode, IndexMode::Auto);
    wait_for(
        "full run despite no watcher",
        Duration::from_secs(20),
        || coord.state().last_full_index.is_some() && f.file_count() == 1,
    );
    coord.shutdown();
}

/// The lost events may have been a directory's `Create` or `Delete`, which
/// leave the watch set wrong, so an overflow registers afresh; the run it
/// schedules finds whatever else was missed.
#[test]
fn an_overflow_re_registers_the_watcher_and_schedules_a_run() {
    // Not due for an hour: only the overflow can start a run.
    let (f, stamped) = Fixture::stamped_auto(-60, 60);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    let starts = coord.watcher_starts();

    coord.degrade_watcher(WatchError::Overflowed);
    wait_for(
        "a fresh registration and a run",
        Duration::from_secs(20),
        || {
            let s = coord.state();
            coord.watcher_starts() > starts
                && matches!(s.watcher, WatcherStatus::Active { .. })
                && s.last_full_index.is_some_and(|t| t > stamped)
        },
    );
    coord.shutdown();
}

/// A limit hit after an overflow was recorded used to be masked by it, and
/// the watcher stayed Active over a tree it no longer covered. Now the
/// overflow's fresh registration meets the limit and ends Disabled. Should
/// the old watcher's own limit win the race instead, it ends there too.
#[test]
fn an_overflow_with_a_standing_limit_ends_disabled() {
    let f = Fixture::new(true);
    std::fs::create_dir_all(f.dir.join("a")).unwrap();
    let watcher_config = WatcherConfig {
        max_watched_dirs: 3,
        ..fast_watcher()
    };
    let coord = start_coord_watching(f.config.clone(), watcher_config);
    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { dirs: 2 })
    });

    for sub in ["b", "c"] {
        std::fs::create_dir(f.dir.join(sub)).unwrap();
    }
    coord.degrade_watcher(WatchError::Overflowed);

    wait_for("watcher disabled", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Disabled { .. })
    });
    assert_eq!(
        coord.state().watcher,
        WatcherStatus::Disabled {
            reason: WatchError::TooManyDirectories { dirs: 3, cap: 3 }
        }
    );
    coord.shutdown();
}

/// A storm overflows the fresh watcher as soon as it is up. Each
/// re-registration walks every root, so one inside the gap waits, recorded,
/// for the gap to end: no second registration, and no run scheduled every
/// tick in the meantime.
#[test]
fn a_second_overflow_soon_after_the_first_waits_out_the_gap() {
    // Not due for an hour: only an overflow can start a run.
    let (f, stamped) = Fixture::stamped_auto(-60, 60);
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("watcher active", Duration::from_secs(20), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    let starts = coord.watcher_starts();
    coord.degrade_watcher(WatchError::Overflowed);
    wait_for(
        "the first overflow's registration and run",
        Duration::from_secs(20),
        || {
            let s = coord.state();
            matches!(s.watcher, WatcherStatus::Active { .. })
                && s.last_full_index.is_some_and(|t| t > stamped)
                && !coord.is_indexing()
        },
    );
    assert_eq!(coord.watcher_starts(), starts + 1);
    let ran = coord.state().last_full_index;

    coord.degrade_watcher(WatchError::Overflowed);
    let ticks = coord.ticks();
    wait_for("three ticks", Duration::from_secs(10), || {
        coord.ticks() >= ticks + 3
    });
    assert_eq!(coord.watcher_starts(), starts + 1, "re-registered again");
    let s = coord.state();
    assert!(matches!(s.watcher, WatcherStatus::Active { .. }));
    assert_eq!(s.last_full_index, ran);
    assert!(!coord.is_indexing(), "{:?}", s.activity);
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
    for_ticks(&coord, 2, || {
        assert_eq!(f.file_count(), 1, "stopped mode must not index new files");
        assert_eq!(coord.state().queued_events, 0);
    });

    coord.shutdown();
}

/// A stop that lands mid-run ends the run and leaves the mode stopped; the
/// stopped run stamps nothing.
#[test]
fn stop_during_a_run_returns_to_manual_stopped() {
    let mut f = Fixture::new(false);
    std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
    f.seed_index();
    f.slow_writer();
    f.write_files(400);
    let coord = start_coord(f.config.clone());
    let stamped = coord.state().last_full_index;
    assert!(stamped.is_some(), "the seed run stamped the index");

    coord.reindex_now();
    catch("the run under way", || {
        walked(&coord) > 0 && coord.state().mode == IndexMode::ManualRunning
    });
    coord.set_mode(IndexMode::ManualStopped);
    wait_for("the run to end", Duration::from_secs(30), || {
        !coord.is_indexing()
    });
    // The tick after the end is the one that re-reads the stamp.
    for_ticks(&coord, 2, || {
        let s = coord.state();
        assert_eq!(s.mode, IndexMode::ManualStopped);
        assert_eq!(
            s.last_full_index, stamped,
            "the stopped run stamped the index"
        );
    });
    coord.shutdown();
}

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

/// A queue holding `events`, folded as the watcher's are.
fn queued(events: impl IntoIterator<Item = FsEvent>) -> inner::Debounced {
    let mut queue = inner::Debounced::default();
    for ev in events {
        enqueue(&mut queue, ev);
    }
    queue
}

fn spelled<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<String> {
    let mut spelled: Vec<String> = paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    spelled.sort();
    spelled
}

#[test]
fn collapsing_reduces_a_tree_deletion_to_its_root() {
    let dir = PathBuf::from("/x/tree");
    let mut events = vec![FsEvent::Remove(dir.clone())];
    for i in 0..500 {
        let sub = dir.join(format!("sub{}", i % 5));
        events.push(FsEvent::Remove(sub.join(format!("f{}.txt", i))));
        events.push(FsEvent::Remove(sub));
    }
    events.push(FsEvent::Remove(PathBuf::from("/x/treehouse")));
    events.push(FsEvent::Create(dir.join("reborn.txt")));
    let mut queue = queued(events);

    collapse_pending_removals(&mut queue.removals);

    assert_eq!(
        spelled(&queue.removals),
        ["/x/tree", "/x/treehouse"],
        "only the removal root and the prefix sibling remain"
    );
    assert_eq!(spelled(queue.upserts.keys()), ["/x/tree/reborn.txt"]);
}

#[test]
fn collapsing_a_queue_without_removals_is_a_no_op() {
    let mut queue = queued([
        FsEvent::Create(PathBuf::from("/x/a")),
        FsEvent::Modify(PathBuf::from("/x/a/b")),
    ]);
    collapse_pending_removals(&mut queue.removals);
    assert_eq!(queue.len(), 2);
}

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

/// Removed once the run has written some of its rows and still holds the
/// index: those rows can go only through the queue, which holds the removal
/// until the run is done.
#[test]
fn a_deletion_during_a_full_run_is_queued_then_applied() {
    // Not due for an hour: the run is the one asked for below.
    let (mut f, _) = Fixture::stamped_auto(-60, 60);
    f.slow_writer();
    let d0 = f.dir.join("d0");
    for i in 0..300 {
        let p = f.dir.join(format!("d{}/f{:03}.txt", i % 6, i));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("body {}", i)).unwrap();
    }

    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("watcher active", Duration::from_secs(30), || {
        matches!(coord.state().watcher, WatcherStatus::Active { .. })
    });
    coord.reindex_now();
    catch("rows under d0 written mid-run", || {
        walked(&coord) > 0 && f.rows_under(&d0) > 0 && coord.is_indexing()
    });
    std::fs::remove_dir_all(&d0).unwrap();
    // Drained by the next tick, whether or not the run has ended by then.
    wait_for("the removal queued", Duration::from_secs(20), || {
        coord.state().queued_events >= 1
    });

    // The seed and 300 files, 50 of them under d0.
    wait_for(
        "deletion applied after the run",
        Duration::from_secs(60),
        || f.file_count() == 251,
    );
    coord.shutdown();
}

/// Only what the watcher registers with restarts it; a theme or search
/// setting must leave a large tree's watches alone.
#[test]
fn only_watch_inputs_restart_the_watcher() {
    let old = Config::default();
    let differs = |edit: fn(&mut Config)| {
        let mut new = old.clone();
        edit(&mut new);
        inner::watch_inputs_differ(&old, &new)
    };
    assert!(!differs(|_| {}));
    assert!(!differs(|c| c.search.debounce_ms += 100));
    assert!(!differs(|c| c.processing.batch_size += 1));
    assert!(!differs(|c| c.indexing.reindex_interval_minutes += 1));
    assert!(differs(|c| c
        .paths
        .indexing_paths
        .push("/elsewhere".into())));
    assert!(differs(
        |c| c.indexing.include_hidden = !c.indexing.include_hidden
    ));
    assert!(differs(|c| c.indexing.ignore_patterns.push("*.tmp".into())));
}

/// With the same inputs an edit restarts nothing that a restart cannot
/// mend: not a registration in flight, which would begin again from the
/// top, nor a tree over the directory cap, which would fail the same way.
/// A start that failed for any other reason is retried.
#[test]
fn an_edit_restarts_the_watcher_only_when_it_must() {
    let old = Config::default();
    let mut changed = old.clone();
    changed.indexing.include_hidden = !changed.indexing.include_hidden;
    let active = WatcherStatus::Active { dirs: 1 };
    let capped = WatcherStatus::Disabled {
        reason: WatchError::TooManyDirectories { dirs: 2, cap: 2 },
    };
    let limited = WatcherStatus::Disabled {
        reason: WatchError::KernelLimit { registered: 2 },
    };

    assert!(!inner::rewatch_needed(&old, &old, &WatcherStatus::Starting));
    assert!(!inner::rewatch_needed(&old, &old, &active));
    assert!(!inner::rewatch_needed(&old, &old, &capped));
    assert!(inner::rewatch_needed(&old, &old, &limited));
    assert!(inner::rewatch_needed(&old, &old, &WatcherStatus::Off));
    for status in [
        WatcherStatus::Starting,
        active,
        capped,
        limited,
        WatcherStatus::Off,
    ] {
        assert!(
            inner::rewatch_needed(&old, &changed, &status),
            "changed inputs must restart from {:?}",
            status
        );
    }
}

/// `mv build build.old; mkdir build`: the `Create` joins the removal rather
/// than replacing it, so the old tree's rows still go.
#[test]
fn a_create_keeps_a_queued_removal_of_the_same_path() {
    let d = PathBuf::from("/x/d");
    let queue = queued([FsEvent::Remove(d.clone()), FsEvent::Create(d.clone())]);
    assert_eq!(spelled(&queue.removals), ["/x/d"]);
    assert_eq!(queue.upserts.get(&d), Some(&true));
}

/// Whatever was queued for a path before its removal is dropped with it.
#[test]
fn a_removal_drops_the_upsert_before_it() {
    let d = PathBuf::from("/x/d");
    for events in [
        vec![FsEvent::Create(d.clone()), FsEvent::Remove(d.clone())],
        vec![
            FsEvent::Remove(d.clone()),
            FsEvent::Create(d.clone()),
            FsEvent::Remove(d.clone()),
        ],
    ] {
        let queue = queued(events);
        assert_eq!(spelled(&queue.removals), ["/x/d"]);
        assert!(queue.upserts.is_empty());
    }
}

/// A `Create` walks a directory and a `Modify` does not; one never turns
/// the other into a no-walk, in either order.
#[test]
fn a_modify_never_downgrades_a_create() {
    let d = PathBuf::from("/x/d");
    for events in [
        [FsEvent::Create(d.clone()), FsEvent::Modify(d.clone())],
        [FsEvent::Modify(d.clone()), FsEvent::Create(d.clone())],
    ] {
        assert_eq!(queued(events).upserts.get(&d), Some(&true));
    }
    assert_eq!(
        queued([FsEvent::Modify(d.clone())]).upserts.get(&d),
        Some(&false)
    );
}

#[test]
fn a_rename_splits_into_a_removal_and_a_walk() {
    let (a, b) = (PathBuf::from("/x/a"), PathBuf::from("/x/b"));
    let queue = queued([FsEvent::Rename {
        from: a.clone(),
        to: b.clone(),
    }]);
    assert_eq!(spelled(&queue.removals), ["/x/a"]);
    assert_eq!(spelled(queue.upserts.keys()), ["/x/b"]);
    assert_eq!(queue.upserts.get(&b), Some(&true));
}

/// A fixture's index, written by [`inner::apply_slice`] directly, as one
/// turn of [`Inner::apply_queue`] writes it.
struct Slicer {
    f: Fixture,
    conn: Connection,
    ignore: Arc<IgnoreSet>,
    registry: Registry,
    cancel: AtomicBool,
}

impl Slicer {
    fn new() -> Slicer {
        let f = Fixture::new(false);
        let conn = db::open_or_recreate(&f.db.to_string_lossy(), &Default::default()).unwrap();
        Slicer {
            ignore: Arc::new(IgnoreSet::compile(&f.config.indexing.ignore_patterns).unwrap()),
            f,
            conn,
            registry: Registry::default_set(),
            cancel: AtomicBool::new(false),
        }
    }

    /// One slice; `true` if it owes a full run.
    fn slice(
        &mut self,
        queue: &mut inner::Debounced,
        resume: &mut HashMap<PathBuf, Option<PathBuf>>,
        deadline: Instant,
    ) -> bool {
        let cx = inner::SliceCx {
            config: &self.f.config,
            ignore: &self.ignore,
            registry: &self.registry,
            cancel: &self.cancel,
            kills: crate::extract::sandbox::KillMark::now(),
            targeted: false,
        };
        let tx = self.conn.transaction().unwrap();
        let failed = inner::apply_slice(&tx, queue, resume, &cx, deadline);
        tx.commit().unwrap();
        failed
    }

    /// `events` applied in full by one unhurried slice.
    fn apply(&mut self, events: impl IntoIterator<Item = FsEvent>) {
        let mut queue = queued(events);
        let far = Instant::now() + Duration::from_secs(3600);
        assert!(!self.slice(&mut queue, &mut HashMap::new(), far));
        assert!(queue.is_empty());
    }

    fn write(&self, name: &str) -> PathBuf {
        let p = self.f.dir.join(name);
        crate::testutil::touch(&p, name.as_bytes());
        p
    }

    /// Every stored path, sorted.
    fn keys(&self) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT parent || name FROM files")
            .unwrap();
        let mut keys: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        keys.sort();
        keys
    }
}

/// The key the index stores for `p`, which must exist.
fn key_of(p: &std::path::Path) -> String {
    crate::file_handling::path_to_db_string(&p.canonicalize().unwrap())
}

/// `mv d d.old; mkdir d` in one window. The removal of `d` finds a
/// directory there again, which alone would keep its subtree; its queued
/// `Create` makes it a rewalk, so the old tree's rows go.
#[test]
fn a_directory_moved_aside_and_recreated_keeps_no_ghost_rows() {
    let mut s = Slicer::new();
    s.write("d/x.txt");
    s.write("d/sub/y.txt");
    let (d, old) = (s.f.dir.join("d"), s.f.dir.join("d.old"));
    s.apply([FsEvent::Create(d.clone())]);
    assert_eq!(s.keys().len(), 2);

    std::fs::rename(&d, &old).unwrap();
    std::fs::create_dir(&d).unwrap();
    s.apply([
        FsEvent::Rename {
            from: d.clone(),
            to: old.clone(),
        },
        FsEvent::Create(d),
    ]);
    let mut want = vec![key_of(&old.join("x.txt")), key_of(&old.join("sub/y.txt"))];
    want.sort();
    assert_eq!(s.keys(), want);
}

/// A spent deadline still buys one entry per slice: one removal, and the
/// upserts only in a slice that found no removal left, never dropped.
#[test]
fn a_spent_deadline_applies_one_removal_per_slice_and_upserts_last() {
    let mut s = Slicer::new();
    let (a, b) = (s.write("a.txt"), s.write("b.txt"));
    s.apply([FsEvent::Create(a.clone()), FsEvent::Create(b.clone())]);
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    let c = s.write("c.txt");

    let mut queue = queued([
        FsEvent::Remove(a),
        FsEvent::Remove(b),
        FsEvent::Create(c.clone()),
    ]);
    let mut resume = HashMap::new();
    let mut slice = |s: &mut Slicer, queue: &mut inner::Debounced| {
        assert!(!s.slice(queue, &mut resume, Instant::now()));
        (queue.removals.len(), queue.upserts.len(), s.keys().len())
    };
    assert_eq!(slice(&mut s, &mut queue), (1, 1, 1));
    assert_eq!(slice(&mut s, &mut queue), (0, 1, 0));
    assert_eq!(slice(&mut s, &mut queue), (0, 0, 1));
    assert_eq!(s.keys(), [key_of(&c)]);
}

/// A walk's resume point belongs to the tree it was walking. Once a removal
/// at or above it is applied, the walk starts over, and a sibling whose name
/// merely shares the prefix keeps its own.
#[test]
fn a_resume_point_under_a_removed_path_is_dropped() {
    let mut s = Slicer::new();
    let (d, dd) = (s.f.dir.join("d"), s.f.dir.join("dd"));
    s.write("d/a.txt");
    let mut resume = HashMap::from([
        (d.clone(), Some(d.join("m.txt"))),
        (d.join("deep"), Some(d.join("deep/m.txt"))),
        (dd.clone(), Some(dd.join("m.txt"))),
    ]);
    let mut queue = queued([FsEvent::Remove(d.clone()), FsEvent::Create(d.clone())]);
    let far = Instant::now() + Duration::from_secs(3600);
    assert!(!s.slice(&mut queue, &mut resume, far));

    assert_eq!(spelled(resume.keys()), spelled([&dd]));
    // Resumed after `m.txt`, the walk would have pruned `a.txt`.
    assert_eq!(s.keys(), [key_of(&d.join("a.txt"))]);
}

/// `mv d tmp; mv tmp d` in one window: a removal and a `Create` for a
/// directory whose files are all still there. The unexamined delete lands in
/// the same slice as its walk's first turn, so a queue dropped after that
/// slice, as a switch to manual drops it, costs no row of a live file.
#[test]
fn a_rewalk_deletes_only_together_with_its_walk() {
    let mut s = Slicer::new();
    let a = s.write("d/a.txt");
    let d = s.f.dir.join("d");
    s.apply([FsEvent::Create(d.clone())]);
    assert_eq!(s.keys(), [key_of(&a)]);

    let mut queue = queued([FsEvent::Remove(d.clone()), FsEvent::Create(d)]);
    assert!(!s.slice(&mut queue, &mut HashMap::new(), Instant::now()));
    assert_eq!(s.keys(), [key_of(&a)], "the delete landed without its walk");
    assert!(
        queue.is_empty(),
        "one file: the first turn was the whole walk"
    );
}

/// A first turn walks unsorted, so cut short it leaves no resume point; the
/// event goes on sorted from the start, one file per spent slice, and none
/// is lost or written twice.
#[test]
fn a_walk_cut_in_its_first_turn_goes_on_sorted() {
    let mut s = Slicer::new();
    for i in 0..4 {
        s.write(&format!("d/f{i}.txt"));
    }
    let d = s.f.dir.join("d");
    let mut queue = queued([FsEvent::Create(d.clone())]);
    let mut resume = HashMap::new();
    assert!(!s.slice(&mut queue, &mut resume, Instant::now()));
    assert_eq!(s.keys().len(), 1);
    assert_eq!(resume, HashMap::from([(d, None)]));

    // The first sorted turn passes over the file the first turn wrote.
    let mut slices = 1;
    while !queue.is_empty() {
        assert!(slices < 5, "a slice made no progress");
        assert!(!s.slice(&mut queue, &mut resume, Instant::now()));
        slices += 1;
    }
    assert_eq!(slices, 5);
    assert_eq!(s.keys().len(), 4);
    assert!(resume.is_empty());
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

    for_ticks(&coord, 2, || {
        assert_eq!(f.file_count(), -1, "cleared index must stay cleared");
        assert!(!coord.is_indexing(), "a run started after the clear");
    });

    coord.shutdown();
}

/// A clear that lands mid-run stops the run and deletes the index, and the
/// stopped run's end puts nothing back: no file, no count, no stamp.
#[test]
fn clear_during_a_run_leaves_no_index() {
    let mut f = Fixture::new(false);
    f.slow_writer();
    f.write_files(400);
    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    catch("the run under way", || walked(&coord) > 0);

    coord.clear_index();
    wait_for("the index deleted", Duration::from_secs(30), || {
        coord.state().files == Some(0)
    });
    let index_files = crate::file_handling::index_file_set(&f.db);
    for_ticks(&coord, 5, || {
        let s = coord.state();
        assert!(
            index_files.iter().all(|p| !p.exists()),
            "an index file is back: {:?}",
            index_files
        );
        assert_eq!(s.files, Some(0));
        assert_eq!(s.last_full_index, None);
    });
    assert_eq!(coord.state().mode, IndexMode::ManualStopped);
    coord.shutdown();
}

/// A manual fixture whose `database_path` names another application's
/// database, as a typo in Settings leaves it. Returns the file's bytes.
fn foreign_db_fixture() -> (Fixture, Vec<u8>) {
    let f = Fixture::new(false);
    std::fs::write(f.dir.join("a.txt"), "content").unwrap();
    Connection::open(&f.db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
             INSERT INTO moz_places(url) VALUES('https://example.invalid/');",
        )
        .unwrap();
    let bytes = std::fs::read(&f.db).unwrap();
    (f, bytes)
}

#[test]
fn rebuild_leaves_a_foreign_file_and_reports() {
    let (f, before) = foreign_db_fixture();
    let coord = start_coord(f.config.clone());
    coord.rebuild_index();
    wait_for("the refusal", Duration::from_secs(10), || {
        let activity = coord.state().activity;
        matches!(activity, IndexingStatus::Error(why) if why.contains(db::FOREIGN_DB_PREFIX))
    });
    coord.shutdown();
    assert_eq!(std::fs::read(&f.db).unwrap(), before);
}

#[test]
fn clear_index_leaves_a_foreign_file() {
    let (f, before) = foreign_db_fixture();
    let coord = start_coord(f.config.clone());
    coord.clear_index();
    shut_down(&coord);
    assert_eq!(std::fs::read(&f.db).unwrap(), before);
    let state = coord.state();
    assert!(
        matches!(&state.activity, IndexingStatus::Error(why)
            if why.starts_with("could not clear the index: ")
                && why.contains(db::FOREIGN_DB_PREFIX)),
        "{:?}",
        state.activity
    );
    // Nothing was deleted, so nothing may read as emptied.
    assert_eq!(state.files, None);
}

/// A fixture whose directory, the one holding the index, can be made
/// read-only, so a delete of a real index fails.
#[cfg(unix)]
fn set_dir_mode(f: &Fixture, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let dir = f.db.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Root unlinks through any directory mode, so there is no failure to see.
#[cfg(unix)]
fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// A clear that fails deleted nothing: the published figures still describe
/// the file that is there, and the reason is where the user is looking.
#[cfg(unix)]
#[test]
fn a_failed_clear_keeps_its_figures_and_reports() {
    if running_as_root() {
        return;
    }
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    wait_for("the published count", Duration::from_secs(10), || {
        coord.state().files == Some(2)
    });
    let before = coord.state();

    set_dir_mode(&f, 0o555);
    coord.clear_index();
    // Restored too early, the directory would let the clear through.
    shut_down(&coord);
    set_dir_mode(&f, 0o755);

    let after = coord.state();
    assert!(
        matches!(&after.activity, IndexingStatus::Error(why)
            if why.starts_with("could not clear the index: ")),
        "{:?}",
        after.activity
    );
    assert_eq!(after.files, before.files);
    assert_eq!(after.last_full_index, before.last_full_index);
    let roots = |s: &IndexerState| -> Vec<String> {
        s.root_counts.iter().map(|r| r.root.clone()).collect()
    };
    assert_eq!(roots(&after), roots(&before));
    assert!(!roots(&after).is_empty());
    assert_eq!(f.file_count(), 2);
}

/// What the security change relies on: once the old index is deleted, no
/// trigger at all builds a new one until the rebuild that carries the new
/// key.
#[test]
fn a_held_delete_starts_nothing_until_rebuild() {
    let f = Fixture::new(true);
    std::fs::write(f.dir.join("a.txt"), "content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("initial index", Duration::from_secs(20), || {
        f.file_count() == 1
    });

    let deleted = coord.delete_index().recv_timeout(Duration::from_secs(20));
    assert_eq!(deleted, Ok(Ok(())));
    assert!(!f.db.exists());
    coord.reindex_now();
    coord.set_mode(IndexMode::Auto);
    // The first tick would start the run that an index-less Auto owes.
    for_ticks(&coord, 2, || {
        assert!(!f.db.exists(), "a run started while the delete was held");
        assert!(!coord.is_indexing());
    });

    coord.rebuild_index();
    wait_for("the rebuild", Duration::from_secs(20), || {
        f.file_count() == 1
    });
    coord.shutdown();
}

/// A delete that failed holds nothing back: the old index is still there and
/// still usable, so the next run may go ahead.
#[cfg(unix)]
#[test]
fn a_failed_delete_reports_and_releases_the_hold() {
    if running_as_root() {
        return;
    }
    let f = Fixture::new(false);
    let coord = f.indexed_pair();

    set_dir_mode(&f, 0o555);
    let reply = coord.delete_index().recv_timeout(Duration::from_secs(20));
    set_dir_mode(&f, 0o755);
    let err = reply.expect("a reply").unwrap_err();
    assert!(err.contains("Failed to delete"), "{err}");
    let activity = coord.state().activity;
    assert!(
        matches!(&activity, IndexingStatus::Error(why)
            if why.starts_with("could not delete the index: ")),
        "{:?}",
        activity
    );

    std::fs::write(f.dir.join("new.txt"), "more content").unwrap();
    coord.reindex_now();
    wait_for(
        "a run after the failed delete",
        Duration::from_secs(20),
        || f.file_count() == 3,
    );
    coord.shutdown();
}

/// A delete refused because a run still holds the index (a VACUUM that
/// outlives the stop) leaves the run's status alone. Published as an error,
/// it read as idle while the run still wrote: the next tick opened the index
/// under it, a retry unlinked the file, and a new run could start. The
/// refusal is reported once the run ends. `Optimizing` is set by hand, the
/// state such a VACUUM leaves; each refusal waits out the stop grace.
#[test]
fn a_refused_delete_leaves_a_live_run_in_charge() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    coord.indexing.force_status(IndexingStatus::Optimizing);
    let ticks = coord.ticks();
    coord.rebuild_index();
    coord.rebuild_index();
    // A tick follows each command, and at most one comes before the first.
    wait_for("both refusals", Duration::from_secs(60), || {
        coord.ticks() >= ticks + 3
    });
    let activity = coord.state().activity;
    assert!(
        matches!(activity, IndexingStatus::Optimizing),
        "{activity:?}"
    );
    assert!(coord.is_indexing());
    assert_eq!(f.file_count(), 2, "the index was deleted under the run");

    coord.indexing.force_status(IndexingStatus::Idle);
    wait_for("the refusal, reported", Duration::from_secs(10), || {
        matches!(&coord.state().activity, IndexingStatus::Error(why)
            if why.starts_with("could not rebuild the index: ")
                && why.contains("still stopping"))
    });
    coord.shutdown();
}

/// A delete that fails deleted nothing, so the reconciliation it owed the
/// file that is still there is owed still, and applies once the run holding
/// the index ends.
#[test]
fn a_refused_delete_keeps_the_reconciliation_owed() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    coord.indexing.force_status(IndexingStatus::Optimizing);
    // Queued, not applied: the run holds the index.
    coord.apply_config(f.narrowed());
    let reply = coord.delete_index().recv_timeout(Duration::from_secs(30));
    let err = reply.expect("a reply").unwrap_err();
    assert!(err.contains("still stopping"), "{err}");
    assert_eq!(f.file_count(), 2);

    coord.indexing.force_status(IndexingStatus::Idle);
    wait_for("the owed prune", Duration::from_secs(20), || {
        f.file_count() == 1
    });
    coord.shutdown();
}

/// The fixture's config pointed at `name` beside its index, and that path.
fn moved_to(f: &Fixture, name: &str) -> (Config, PathBuf) {
    let moved = f.db.with_file_name(name);
    let mut config = f.config.clone();
    config.paths.database_path = moved.to_string_lossy().into_owned();
    (config, moved)
}

/// The old index's stamp must not stand in for the new path's: with a long
/// interval it would put the first run off for the whole of it. Nothing on
/// disk changes here, so no watcher event can be what starts the run.
#[test]
fn a_new_database_path_is_built_in_auto() {
    let mut f = Fixture::new(true);
    f.config.indexing.reindex_interval_minutes = 1000;
    std::fs::write(f.dir.join("a.txt"), "content").unwrap();
    let coord = start_coord_watching(f.config.clone(), fast_watcher());
    wait_for("the first index", Duration::from_secs(20), || {
        coord.state().last_full_index.is_some() && !coord.is_indexing()
    });

    let (config, moved) = moved_to(&f, "moved.sqlite");
    coord.apply_config(config);
    wait_for("an index at the new path", Duration::from_secs(20), || {
        rows_in(&moved) == 1
    });
    coord.shutdown();
}

#[test]
fn a_new_database_path_reads_as_never_indexed_in_manual() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    wait_for("the published count", Duration::from_secs(10), || {
        coord.state().files == Some(2)
    });

    let (config, moved) = moved_to(&f, "moved.sqlite");
    let ticks = coord.ticks();
    coord.apply_config(config);
    // Ticks are where a stale count would be re-read or a run started.
    wait_for("two ticks", Duration::from_secs(10), || {
        coord.ticks() >= ticks + 2
    });
    let state = coord.state();
    assert_eq!(state.last_full_index, None);
    assert_eq!(state.files, None);
    assert!(state.root_counts.is_empty());
    coord.shutdown();
    assert!(!moved.exists(), "manual mode builds nothing on its own");
}

#[test]
fn switching_back_to_an_existing_index_republishes_its_stamp() {
    let f = Fixture::new(false);
    let coord = f.indexed_pair();
    wait_for("the published count", Duration::from_secs(10), || {
        coord.state().files == Some(2)
    });
    let stamp = coord.state().last_full_index;

    let (config, _) = moved_to(&f, "moved.sqlite");
    coord.apply_config(config);
    wait_for("the new path's figures", Duration::from_secs(10), || {
        let s = coord.state();
        s.last_full_index.is_none() && s.files.is_none()
    });

    coord.apply_config(f.config.clone());
    wait_for("the old index's figures", Duration::from_secs(10), || {
        let s = coord.state();
        s.last_full_index == stamp && s.files == Some(2) && !s.root_counts.is_empty()
    });
    coord.shutdown();
}

/// A rebuild asked for mid-run waits out the stop, maintenance included,
/// then deletes and runs. It used to unlink under the running pass and, in
/// manual mode, start nothing at all.
#[test]
fn rebuild_during_a_run_starts_a_new_run() {
    use rusqlite::OptionalExtension;
    let f = Fixture::new(false);
    f.write_files(200);
    f.seed_index();
    let stale = now_unix() - 7200;
    f.stamp_last_index(stale);
    // Only a new file loses this; a reindex of the old one keeps it.
    db::open_existing(&f.db.to_string_lossy(), true)
        .unwrap()
        .execute(
            "INSERT INTO schema_info(key, value) VALUES ('test_marker', '1')",
            [],
        )
        .unwrap();

    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    coord.rebuild_index();
    wait_for("the rebuild's run", Duration::from_secs(30), || {
        let s = coord.state();
        s.last_full_index.is_some_and(|t| t > stale)
            && s.mode == IndexMode::ManualStopped
            && !coord.is_indexing()
    });
    assert_eq!(f.file_count(), 200);
    let marker: Option<String> = db::open_existing(&f.db.to_string_lossy(), false)
        .unwrap()
        .query_row(
            "SELECT value FROM schema_info WHERE key = 'test_marker'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(marker, None, "the old file was not deleted");
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
    shut_down(&coord);
    let state = coord.state();
    assert!(
        matches!(&state.activity, IndexingStatus::Error(why) if why.contains("nested")),
        "{:?}",
        state.activity
    );
    assert_eq!(
        f.file_count(),
        -1,
        "a run over nested roots must be refused (no DB created)"
    );
    assert_eq!(
        state.mode,
        IndexMode::ManualStopped,
        "a refused run must not read as running"
    );
}

/// A refused run never starts, so no run's end would ever return the mode
/// from `ManualRunning`.
#[test]
fn a_refused_manual_run_with_no_roots_stays_stopped() {
    let mut f = Fixture::new(false);
    f.config.paths.indexing_paths.clear();
    let coord = start_coord(f.config.clone());
    coord.reindex_now();
    shut_down(&coord);
    let state = coord.state();
    assert!(
        matches!(&state.activity, IndexingStatus::Error(why) if why.contains("no indexing roots")),
        "{:?}",
        state.activity
    );
    assert_eq!(state.mode, IndexMode::ManualStopped);
}

/// What the GUI runs on when its config file fails to load: no roots, so
/// even a requested run is refused and the index is never touched.
#[test]
fn a_fallback_config_never_runs() {
    let (_scratch, db) = crate::testutil::Scratch::db("coord-fallback");
    let mut config = Config::fallback();
    config.paths.database_path = db.to_string_lossy().into_owned();
    let coord = start_coord(config);
    assert_eq!(coord.state().mode, IndexMode::ManualStopped);

    coord.reindex_now();
    wait_for("the no-roots refusal", Duration::from_secs(10), || {
        let activity = coord.state().activity;
        matches!(activity, IndexingStatus::Error(why) if why.contains("no indexing roots"))
    });
    assert_eq!(coord.state().mode, IndexMode::ManualStopped);
    assert!(!coord.is_indexing());
    assert!(!db.exists(), "a refused run must not create the index");
    coord.shutdown();
}

#[test]
fn shutdown_is_idempotent_and_joins() {
    let f = Fixture::new(false);
    let coord = start_coord(f.config.clone());
    assert!(!coord.thread_ended());
    coord.shutdown();
    assert!(coord.thread_ended(), "shutdown left the thread running");
    coord.shutdown(); // second call is a no-op
}
