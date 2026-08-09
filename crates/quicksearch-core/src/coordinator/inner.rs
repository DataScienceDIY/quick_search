//! The coordinator's event loop: one thread owning the watcher, the
//! debounce buffer, and the incremental reconcile cursor.

use super::*;

pub(super) struct Inner {
    pub(super) config: Config,
    pub(super) indexing: Arc<IndexingService>,
    pub(super) shared: Arc<Mutex<Shared>>,
    pub(super) notify: Notify,
    /// Last published value of the "something is moving" predicate;
    /// [`Inner::publish`] wakes the frontend only on the rising edge.
    pub(super) awake: bool,
    /// Read inside the reconciliation, set from the thread that shuts this
    /// one down; see [`ReconcileStop`].
    pub(super) reconcile_stop: Arc<ReconcileStop>,
    pub(super) event_tx: mpsc::Sender<FsEvent>,
    pub(super) event_rx: mpsc::Receiver<FsEvent>,
    pub(super) watcher: Option<Watcher>,
    pub(super) watcher_config: WatcherConfig,
    /// In-flight async watcher registration (see [`Inner::start_watcher`]).
    pub(super) watcher_rx: Option<mpsc::Receiver<(u64, Result<Watcher, WatchError>)>>,
    pub(super) watcher_gen: u64,
    pub(super) pending: HashMap<PathBuf, FsEvent>,
    /// When the most recent event arrived; the burst is over once this is
    /// `pending_settle` old.
    pub(super) last_event_at: Option<Instant>,
    /// When the oldest un-applied event arrived, so a steady trickle cannot
    /// defer application past `pending_max_defer`.
    pub(super) pending_since: Option<Instant>,
    pub(super) needs_full_run: bool,
    /// Reconciliation owed to a config change, part-applied across ticks.
    /// Unlike `needs_full_run`, this is acted on in manual mode too.
    pub(super) pending_work: Option<WorkCursor>,
    /// The last reconciliation to finish, and when. Published until it is
    /// [`RECONCILE_SUMMARY_LINGER`] old; see [`ReconcileState::Finished`].
    pub(super) reconcile_done: Option<(ReconcileProgress, Instant)>,
    /// A reconciliation was abandoned part-way; read by [`Inner::teardown`].
    pub(super) reconcile_cut_short: bool,
    /// A start was requested; set false once the service reports running,
    /// so idle-after-running transitions are detectable.
    pub(super) saw_running: bool,
    /// When the published file count was last read; `None` forces a re-read
    /// on the next tick.
    pub(super) files_at: Option<Instant>,
    /// Something has happened since the last time this coordinator settled.
    /// Drives [`Inner::go_idle`] once per busy→idle transition.
    pub(super) was_busy: bool,
    pub(super) write_conn: Option<Connection>,
    /// Shared with the watcher, which filters registrations by the same set.
    pub(super) ignore: Arc<IgnoreSet>,
    pub(super) registry: Registry,
    pub(super) mode: IndexMode,
}

impl Inner {
    pub(super) fn run(mut self, cmd_rx: mpsc::Receiver<CoordCmd>) {
        if self.mode == IndexMode::Auto {
            self.enter_auto();
        }
        loop {
            // While reconciliation slices are owed the idle wait shrinks: at
            // one slice per second a large index's prune stretches to minutes.
            let idle = if self.pending_work.is_some() {
                Duration::from_millis(1)
            } else {
                Duration::from_secs(1)
            };
            match cmd_rx.recv_timeout(idle) {
                Ok(CoordCmd::Shutdown) => break,
                Ok(cmd) => self.handle_cmd(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => self.tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.poll_watcher_startup();
            self.publish();
        }
        self.teardown();
    }

    fn handle_cmd(&mut self, cmd: CoordCmd) {
        match cmd {
            CoordCmd::SetMode(IndexMode::Auto) => self.enter_auto(),
            CoordCmd::SetMode(IndexMode::ManualStopped) => self.enter_manual_stopped(),
            // ManualRunning isn't directly settable; ReindexNow is the verb.
            CoordCmd::SetMode(IndexMode::ManualRunning) | CoordCmd::ReindexNow => {
                self.start_full_run();
                if self.mode != IndexMode::Auto {
                    self.mode = IndexMode::ManualRunning;
                }
            }
            CoordCmd::ConfigChanged(new) => {
                let want_auto = new.indexing.auto_index;
                let actions = diff_actions(&self.config, &new);
                self.config = new;
                if let Err(e) = self.reload_filters() {
                    crate::log_warn!("coordinator: {}", e);
                }
                // The write connection may point at an old database_path, and
                // so may the count read through it.
                self.write_conn = None;
                self.files_at = None;
                // A wipe stays the caller's decision (see `rebuild_index`);
                // everything short of one is reconciled here, in both modes.
                if !actions.requires_rebuild && !actions.work.is_empty() {
                    self.start_work(actions.work);
                }
                if want_auto && self.mode != IndexMode::Auto {
                    // The mode lives in `auto_index`, so a config that
                    // disagrees with the running mode *is* a mode change.
                    self.enter_auto();
                } else if !want_auto && self.mode == IndexMode::Auto {
                    self.enter_manual_stopped();
                } else if self.mode == IndexMode::Auto {
                    // Watched roots / symlink behavior may have changed; a
                    // restart is cheap and unconditional beats a diff here.
                    self.start_watcher();
                }
                // The root list, its spellings, or the database behind it may
                // all have moved; re-pair them with what is stored.
                self.refresh_last_full_index();
            }
            CoordCmd::RebuildIndex => {
                let db = self.db_path();
                self.write_conn = None;
                self.files_at = None;
                // Nothing to reconcile against once the file is gone.
                self.pending_work = None;
                self.reconcile_done = None;
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    crate::log_warn!("coordinator: rebuild: {}", e);
                }
                self.clear_root_counts();
                self.start_full_run();
                if self.mode != IndexMode::Auto {
                    self.mode = IndexMode::ManualRunning;
                }
            }
            CoordCmd::ClearIndex => {
                // Manual first: in Auto the periodic scheduler would see a
                // missing index and rebuild what was just deleted.
                self.enter_manual_stopped();
                self.write_conn = None;
                self.pending_work = None;
                self.reconcile_done = None;
                let db = self.db_path();
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    crate::log_warn!("coordinator: clear index: {}", e);
                }
                let mut shared = crate::lock_ok(&self.shared);
                shared.last_full_index = None;
                // Zero, not `None`: nothing will rebuild this index, so no
                // later read corrects a stale figure.
                shared.files = Some(0);
                // Per root the empty list reads as "not yet indexed", which is
                // what every folder now is.
                shared.root_counts = Arc::new(Vec::new());
                drop(shared);
                self.files_at = None;
            }
            CoordCmd::Shutdown => unreachable!("handled in run()"),
        }
    }

    fn tick(&mut self) {
        self.check_watcher_degraded();
        self.drain_events();

        let status = self.indexing.get_status();
        match status {
            IndexingStatus::Preparing { .. }
            | IndexingStatus::Running { .. }
            | IndexingStatus::Stopping
            | IndexingStatus::Optimizing => {
                // Single-writer rule: never touch the DB while a full run
                // is active; the queue drains on a later tick. Optimizing
                // counts — it holds a write transaction over the whole file
                // for as long as the rewrite takes.
                self.saw_running = true;
                return;
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => {}
        }

        // A run just finished — pick up its last_full_index stamp and
        // resolve the manual-run mode.
        if self.saw_running {
            self.saw_running = false;
            self.was_busy = true;
            self.refresh_last_full_index();
            // Eager re-read: the run just changed the number on screen.
            self.files_at = None;
            if self.mode == IndexMode::ManualRunning {
                self.mode = IndexMode::ManualStopped;
            }
        }

        self.refresh_file_count();

        // Ahead of the mode gate: a config edit is reconciled in manual mode
        // too.
        if self.pending_work.is_some() {
            self.apply_work();
            return;
        }

        if self.mode != IndexMode::Auto {
            if self.mode == IndexMode::ManualStopped {
                self.clear_pending();
            }
            self.go_idle();
            return;
        }

        let mut worked = false;
        if !self.pending.is_empty() && !self.needs_full_run && self.pending_settled() {
            self.apply_pending();
            worked = true;
        }

        if self.needs_full_run || self.periodic_due() {
            self.start_full_run();
            worked = true;
        }

        // Only when this tick found nothing to do: releasing the connection
        // between batches would reopen it a moment later with a cold cache.
        if !worked {
            self.go_idle();
        }
    }

    /// Settle: drop [`Inner::write_conn`] (and its page cache), then return
    /// freed heap to the kernel — in that order. Gated on [`Inner::was_busy`]
    /// so it runs once per busy→idle transition, not every tick.
    fn go_idle(&mut self) {
        if !self.was_busy {
            return;
        }
        self.was_busy = false;
        self.write_conn = None;
        crate::platform::release_free_heap();
    }

    fn drain_events(&mut self) {
        let mut received = false;
        while let Ok(ev) = self.event_rx.try_recv() {
            enqueue(&mut self.pending, ev);
            received = true;
        }
        if received {
            let now = Instant::now();
            self.was_busy = true;
            self.last_event_at = Some(now);
            self.pending_since.get_or_insert(now);
            // Before the overflow test: collapsed, an `rm -rf` storm stays
            // under [`PENDING_OVERFLOW`] instead of forcing a full run.
            collapse_pending_removals(&mut self.pending);
        }
        if self.pending.len() > PENDING_OVERFLOW {
            self.clear_pending();
            self.needs_full_run = true;
        }
    }

    /// Queue reconciliation for a config change.
    ///
    /// An in-flight plan is folded in and restarted rather than dropped: the
    /// new diff is against the same previous config, so it cannot know what
    /// the old plan left undone.
    fn start_work(&mut self, mut work: IndexWork) {
        if let Some(outstanding) = self.pending_work.take() {
            work.merge_from(outstanding.work());
        }
        match WorkCursor::new(work, &self.config) {
            Ok(cursor) => self.pending_work = Some(cursor),
            // Only an uncompilable ignore pattern gets here; refusing to
            // reconcile deletes nothing.
            Err(e) => crate::log_warn!("coordinator: cannot reconcile config change: {}", e),
        }
    }

    /// Advance the queued reconciliation by one slice, and start the full run
    /// it asked for once it is finished.
    fn apply_work(&mut self) {
        self.was_busy = true;
        let mut conn = match self.ensure_write_conn() {
            Ok(conn) => conn,
            Err(e) => {
                // No index to reconcile; a run builds it under the new config.
                crate::log_warn!("coordinator: reconcile unavailable ({}); scheduling run", e);
                self.pending_work = None;
                self.needs_full_run = true;
                return;
            }
        };
        let Some(mut cursor) = self.pending_work.take() else {
            return;
        };
        let outcome = {
            // Armed only for the slice; outside `advance` the cancellation
            // must not end whatever else this connection runs.
            let _armed = db::InterruptGuard::arm(&self.reconcile_stop.interrupt, &conn);
            crate::scope::advance(
                &mut conn,
                &self.config,
                &self.registry,
                &mut cursor,
                Instant::now() + crate::scope::SLICE,
                &self.reconcile_stop.cancel,
            )
        };
        self.write_conn = Some(conn);
        if let Err(e) = outcome {
            // A cancelled statement fails like any other; ask our own flag
            // rather than parsing the error.
            if self.reconcile_stop.cancelled() {
                self.reconcile_cut_short = true;
                crate::log_info!(
                    "configuration change interrupted after {} index entries; \
                     the next indexing run starts it again",
                    cursor.progress().examined
                );
                return;
            }
            // Not retried — a persistent error would spin this loop forever;
            // the next full run reconciles from the stored fingerprint.
            crate::log_warn!(
                "coordinator: reconcile: {}; leaving it to the next indexing run",
                e
            );
            return;
        }
        if !cursor.done() {
            // Nothing is recorded for a pass that stopped early, cancelled or
            // not: the stale record is what makes the next run redo it.
            self.pending_work = Some(cursor);
            return;
        }
        self.reconcile_done = Some((cursor.progress(), Instant::now()));
        // Stamp only on the path where the pass finished and nothing errored:
        // the stale record is what makes the next full run redo an abandoned
        // reconcile.
        if let Some(conn) = self.write_conn.as_ref() {
            // Same spelling a run records (see `config_validation_entries`).
            let roots: Vec<String> = self
                .config
                .normalized_indexing_paths()
                .into_iter()
                .collect();
            if let Err(e) = IndexingService::stamp_reconciled(conn, &self.config, &roots) {
                crate::log_warn!("coordinator: record reconciled configuration: {}", e);
            }
        }
        if cursor.deleted > 0 || cursor.recontented > 0 {
            crate::log_info!(
                "configuration change: {} index entries removed, {} re-examined \
                 for text extraction",
                cursor.deleted,
                cursor.recontented
            );
        }
        // Widening adds files only a walk can produce; mirrors `ReindexNow`,
        // including the manual-mode round trip back to stopped.
        if cursor.reindex() {
            self.start_full_run();
            if self.mode != IndexMode::Auto {
                self.mode = IndexMode::ManualRunning;
            }
        }
    }

    /// Drop the queue and the timers that describe it, so a stale
    /// `pending_since` cannot force an immediate apply of the next event.
    fn clear_pending(&mut self) {
        self.pending.clear();
        // `clear` keeps the map's capacity — up to 100k slots after a storm;
        // shrinking is the point.
        self.pending.shrink_to_fit();
        self.last_event_at = None;
        self.pending_since = None;
    }

    /// Whether the queue has gone quiet long enough to be worth applying, or
    /// has waited long enough that it must be applied regardless.
    fn pending_settled(&self) -> bool {
        let quiet = self
            .last_event_at
            .is_none_or(|t| t.elapsed() >= self.watcher_config.pending_settle);
        let overdue = self
            .pending_since
            .is_some_and(|t| t.elapsed() >= self.watcher_config.pending_max_defer);
        quiet || overdue
    }

    /// Apply as much of the queue as fits in [`APPLY_BUDGET`], removals first.
    ///
    /// Removals lead because the queue is an unordered map: an arbitrary order
    /// could delete a row a `Create` in the same batch had just written.
    fn apply_pending(&mut self) {
        self.was_busy = true;
        let mut conn = match self.ensure_write_conn() {
            Ok(conn) => conn,
            Err(e) => {
                // Missing or stale DB: incremental can't help, rebuild.
                crate::log_warn!(
                    "coordinator: incremental unavailable ({}); scheduling full run",
                    e
                );
                self.needs_full_run = true;
                return;
            }
        };
        let deadline = Instant::now() + APPLY_BUDGET;
        let chunk = self.config.processing.batch_size.max(1);

        let removals: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, ev)| is_removal(ev))
            .map(|(p, _)| p.clone())
            .collect();
        for batch in removals.chunks(chunk) {
            if let Err(e) = crate::incremental::remove_paths(&mut conn, batch, chunk) {
                // The batch leaves `pending` either way — retrying a failed
                // write every tick is worse; the full run recovers the rows.
                crate::log_warn!("coordinator: remove: {}; scheduling full run", e);
                self.needs_full_run = true;
            }
            for path in batch {
                self.pending.remove(path);
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        if Instant::now() < deadline {
            let upserts: Vec<PathBuf> = self
                .pending
                .iter()
                .filter(|(_, ev)| !is_removal(ev))
                .map(|(p, _)| p.clone())
                .collect();
            for path in upserts {
                let Some(ev) = self.pending.remove(&path) else {
                    continue;
                };
                if let Err(e) =
                    apply_fs_event(&mut conn, &ev, &self.config, &self.ignore, &self.registry)
                {
                    // As above: the event is out of `pending`, so only a full
                    // run still picks the file up.
                    crate::log_warn!("coordinator: apply {:?}: {}; scheduling full run", ev, e);
                    self.needs_full_run = true;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }

        self.write_conn = Some(conn);
        // The remainder goes to the next tick immediately: the pause was
        // ours, not the filesystem's, so it must not re-arm the settle window.
        self.last_event_at = None;
        if self.pending.is_empty() {
            self.pending_since = None;
        }
    }

    fn ensure_write_conn(&mut self) -> Result<Connection, String> {
        if let Some(conn) = self.write_conn.take() {
            return Ok(conn);
        }
        // Not `open_existing(_, true)`: that hands out the bulk indexer's
        // profile (see [`db::schema::PRAGMAS_INCREMENTAL`]).
        db::open::open_incremental_writer(&self.db_path())
    }

    fn periodic_due(&self) -> bool {
        let interval_secs = self
            .config
            .indexing
            .reindex_interval_minutes
            .saturating_mul(60);
        let last = crate::lock_ok(&self.shared).last_full_index;
        match last {
            None => true,
            Some(last) => {
                let now = now_unix();
                // A stamp ahead of the clock (NTP correction, index moved
                // between machines) would read as "just indexed" and suppress
                // the periodic reindex for the life of the skew; treat as due.
                now < last || now - last >= interval_secs
            }
        }
    }

    fn start_full_run(&mut self) {
        let roots: Vec<String> = self
            .config
            .resolved_indexing_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if roots.is_empty() {
            crate::log_warn!("coordinator: no indexing roots configured");
            return;
        }
        // Backstop for hand-edited configs; the GUI rejects nested roots
        // itself.
        let nested = crate::config::nested_roots(&roots);
        if !nested.is_empty() {
            for (child, parent) in &nested {
                crate::log_warn!(
                    "coordinator: refusing to index: root {} is nested under {}",
                    child,
                    parent
                );
            }
            return;
        }
        // The full run owns the DB (and may wipe/rebuild the file).
        self.write_conn = None;
        self.needs_full_run = false;
        // Creates and modifies are dropped — the walk rediscovers them.
        // Removals are kept: the walk cannot see a deletion under an
        // unreadable directory (`unreadable.covers`) or of an aliased symlink
        // target (`aliased_paths`), and those rows would leak until a rebuild.
        self.pending.retain(|_, ev| is_removal(ev));
        if self.pending.is_empty() {
            self.last_event_at = None;
            self.pending_since = None;
        }
        if let Err(e) = self
            .indexing
            .start_indexing(roots, self.db_path(), self.config.clone())
        {
            crate::log_warn!("coordinator: start indexing: {}", e);
            return;
        }
        // `start_indexing` claims Running before returning, so there is no
        // window in which this thread believes the service idle and writes to
        // a database the run is about to reopen.
        self.saw_running = true;
    }

    fn enter_auto(&mut self) {
        self.mode = IndexMode::Auto;
        self.config.indexing.auto_index = true;
        self.start_watcher();
        if crate::lock_ok(&self.shared).last_full_index.is_none() {
            self.needs_full_run = true;
        }
    }

    fn enter_manual_stopped(&mut self) {
        self.mode = IndexMode::ManualStopped;
        self.config.indexing.auto_index = false;
        self.stop_watcher();
        self.clear_pending();
        // Stopping cancels a widened scope's walk but keeps its pruning:
        // deleting rows the user put out of scope is their edit, not indexing.
        if let Some(cursor) = self.pending_work.as_mut() {
            cursor.cancel_reindex();
        }
        let status = self.indexing.get_status();
        if !matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_)) {
            // Signal only — waiting up to 5 s here would stall every
            // queued command behind the Stop click.
            self.indexing.request_stop();
        }
    }

    /// Begin watcher startup WITHOUT blocking the command loop — registering
    /// inotify watches walks every root, minutes on large or networked trees.
    /// The finished watcher comes back through a channel polled each loop
    /// turn; a generation counter discards superseded registrations.
    fn start_watcher(&mut self) {
        self.stop_watcher();
        let roots = self.config.resolved_indexing_paths();
        if roots.is_empty() {
            return;
        }
        let generation = self.watcher_gen;
        let sink_tx = self.event_tx.clone();
        let sink = Arc::new(move |ev: FsEvent| {
            let _ = sink_tx.send(ev);
        });
        let config = self.watcher_config.clone();
        // Same filters the indexer walks with; no descriptor is spent on a
        // directory whose events would be discarded on arrival.
        let filters = WatchFilters {
            include_hidden: self.config.indexing.include_hidden,
            follow_symlinks: self.config.indexing.follow_symlinks,
            ignore: self.ignore.clone(),
        };
        let (tx, rx) = mpsc::channel();
        self.watcher_rx = Some(rx);
        self.set_watcher_status(WatcherStatus::Starting);
        let spawned = std::thread::Builder::new()
            .name("qs-watcher-start".into())
            .spawn(move || {
                let result = Watcher::start(roots, filters, config, sink);
                // A failed send means the coordinator moved on; dropping
                // the watcher here unregisters it.
                let _ = tx.send((generation, result));
            });
        if spawned.is_err() {
            self.watcher_rx = None;
            self.set_watcher_status(WatcherStatus::Off);
        }
    }

    /// Collect a finished watcher registration, if any. Called every
    /// command-loop turn so it lands regardless of tick timing.
    fn poll_watcher_startup(&mut self) {
        let Some(rx) = &self.watcher_rx else {
            return;
        };
        match rx.try_recv() {
            Ok((generation, result)) => {
                self.watcher_rx = None;
                if generation != self.watcher_gen {
                    return; // superseded; the watcher drops and unregisters
                }
                match result {
                    Ok(w) => {
                        let status = WatcherStatus::Active {
                            dirs: w.watched_dirs(),
                        };
                        self.watcher = Some(w);
                        self.set_watcher_status(status);
                    }
                    Err(e) => {
                        crate::log_warn!("coordinator: watcher: {}", e);
                        self.set_watcher_status(WatcherStatus::Disabled { reason: e });
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.watcher_rx = None;
                self.set_watcher_status(WatcherStatus::Off);
            }
        }
    }

    /// Tear the watcher down if it ran out of watch budget after starting,
    /// and schedule a full run: a partially watched tree looks live while
    /// going silently out of date.
    fn check_watcher_degraded(&mut self) {
        let Some(w) = &self.watcher else {
            return;
        };
        let Some(mut reason) = w.degraded_reason() else {
            return;
        };
        // The async notify callback can't know the count; fill it in here.
        if let WatchError::KernelLimit { registered } = &mut reason {
            if *registered == 0 {
                *registered = w.watched_dirs();
            }
        }
        self.stop_watcher();
        self.needs_full_run = true;
        self.set_watcher_status(WatcherStatus::Disabled { reason });
    }

    fn stop_watcher(&mut self) {
        // Invalidate any in-flight registration and drop its channel.
        self.watcher_gen = self.watcher_gen.wrapping_add(1);
        self.watcher_rx = None;
        if let Some(mut w) = self.watcher.take() {
            w.stop();
        }
        self.set_watcher_status(WatcherStatus::Off);
    }

    fn set_watcher_status(&self, status: WatcherStatus) {
        crate::lock_ok(&self.shared).watcher = status;
    }

    fn db_path(&self) -> String {
        self.config
            .resolved_database_path()
            .to_string_lossy()
            .into_owned()
    }

    pub(super) fn reload_filters(&mut self) -> Result<(), String> {
        self.ignore = Arc::new(
            IgnoreSet::compile(&self.config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?,
        );
        Ok(())
    }

    /// Re-read the published row count, at most every [`FILE_COUNT_INTERVAL`].
    ///
    /// Called only from the idle half of [`Inner::tick`], so it cannot run
    /// while a full run holds the database. `COUNT(*)` is still a key scan of
    /// every row — hence the interval and the interrupt guard.
    fn refresh_file_count(&mut self) {
        if let Some(at) = self.files_at {
            if at.elapsed() < FILE_COUNT_INTERVAL {
                return;
            }
        }
        // Stamped before the read: a failing count must back off like a
        // successful one, or a missing index means an open attempt every tick.
        self.files_at = Some(Instant::now());

        let Ok(conn) = db::open_existing(&self.db_path(), false) else {
            return;
        };
        // Same slot `apply_work` arms; shutdown cuts the scan short with it.
        let _guard = db::InterruptGuard::arm(&self.reconcile_stop.interrupt, &conn);
        match db::repo::row_count(&conn) {
            Ok(n) => crate::lock_ok(&self.shared).files = Some(n as i64),
            // Interrupted shutdown or torn index; the last figure beats none.
            Err(e) => crate::log_warn!("coordinator: file count unavailable: {}", e),
        }
    }

    /// Re-read what the last completed full run left behind: its stamp, and
    /// the per-root figures the folder list shows.
    ///
    /// Both off one connection because they are wanted at the same moments —
    /// startup, a run finishing, a config change. Neither is a scan: the stamp
    /// and each root's counts are single `schema_info` key lookups, the work of
    /// counting having been done by the run that stored them.
    ///
    /// A failed open is *not* published as `None`: `periodic_due` reads `None`
    /// as "never indexed" and would start a fresh run every tick for as long
    /// as the failure lasts.
    pub(super) fn refresh_last_full_index(&self) {
        match db::open_existing(&self.db_path(), false) {
            Ok(conn) => {
                let last = db::repo::get_last_full_index(&conn);
                let counts = self.read_root_counts(&conn);
                let mut shared = crate::lock_ok(&self.shared);
                shared.last_full_index = last;
                shared.root_counts = Arc::new(counts);
            }
            Err(e) => crate::log_warn!("coordinator: last-full-index unreadable: {}", e),
        }
    }

    /// Pair every configured root with its stored figures, keyed by the
    /// spelling the config uses so a frontend can match what it draws.
    ///
    /// The `schema_info` keys are canonicalized, which is what makes writing
    /// `~/docs` where the config said `/home/me/docs` keep the figures — the
    /// same re-keying `indexing::resolved_root_workers` does in the other
    /// direction.
    fn read_root_counts(&self, conn: &Connection) -> Vec<RootCount> {
        self.config
            .paths
            .indexing_paths
            .iter()
            .zip(self.config.resolved_indexing_paths())
            .filter_map(|(raw, resolved)| {
                let root = crate::file_handling::normalize_root_string(&resolved.to_string_lossy());
                let counts = db::repo::get_root_counts(conn, &root)?;
                Some(RootCount {
                    root: raw.clone(),
                    counts,
                })
            })
            .collect()
    }

    /// Forget the published figures: the index behind them is gone, and
    /// nothing will correct them until a run rebuilds it.
    fn clear_root_counts(&self) {
        crate::lock_ok(&self.shared).root_counts = Arc::new(Vec::new());
    }

    fn publish(&mut self) {
        let reconcile = match &self.pending_work {
            Some(cursor) => Some(ReconcileState::Running(cursor.progress())),
            None => {
                // The tail ages out here, not in `tick` — tick returns early
                // for the whole length of a run.
                let now = Instant::now();
                self.reconcile_done = self
                    .reconcile_done
                    .filter(|(_, at)| summary_is_fresh(*at, now));
                self.reconcile_done
                    .map(|(progress, _)| ReconcileState::Finished(progress))
            }
        };
        let busy = reconcile.is_some()
            || !matches!(
                self.indexing.get_status(),
                IndexingStatus::Idle | IndexingStatus::Error(_)
            );
        let mut shared = crate::lock_ok(&self.shared);
        shared.mode = self.mode;
        shared.queued_events = self.pending.len();
        shared.reconcile = reconcile;
        drop(shared);

        // Edge-triggered wake: without it a settled window never observes the
        // first movement (e.g. a fresh launch's due reindex), and a
        // level-triggered call would cost a wake-up per second forever.
        if busy && !self.awake {
            (self.notify)();
        }
        self.awake = busy;
    }

    /// Must stay fast: it runs (transitively) on the GUI thread during window
    /// close. Signal, don't wait — an abandoned run is safe under WAL, and an
    /// abandoned reconcile is redone by the next run.
    fn teardown(mut self) {
        self.stop_watcher();
        let status = self.indexing.get_status();
        let idle = matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_));
        if !idle {
            self.indexing.request_stop();
            // Dropping the service joins its worker, and a VACUUM answers
            // only to `sqlite3_interrupt`; the interrupted VACUUM rolls back.
            self.indexing.cancel_db_work();
        }
        let cut_short = self.reconcile_cut_short || self.pending_work.is_some();
        if let Some(conn) = self.write_conn.take() {
            // A cut-short reconcile must not checkpoint: it can have written a
            // great deal of WAL, and a TRUNCATE checkpoint of it is the wait
            // the cancellation just spared. Dropping is safe — WAL keeps the
            // log and the next run lands it.
            if idle && !cut_short {
                db::repo::checkpoint_and_close(conn);
            }
            // Otherwise just drop: a TRUNCATE checkpoint would block behind
            // the running writer.
        }
    }
}
