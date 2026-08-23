//! The coordinator's event loop: one thread owning the watcher, the
//! debounce buffer, and the incremental reconcile cursor.

use super::*;

/// First wait after a full run is refused or fails to start.
pub(super) const RUN_RETRY_BASE: Duration = Duration::from_secs(30);

const RUN_RETRY_MAX: Duration = Duration::from_secs(300);

/// Refusal log throttles; reset the moment a run starts.
static NO_ROOTS: crate::log::Throttle = crate::log::Throttle::new(3);
static NESTED_ROOTS: crate::log::Throttle = crate::log::Throttle::new(3);
static START_FAILURES: crate::log::Throttle = crate::log::Throttle::new(3);

/// How a refusal to start a full run is reported; see [`Inner::refuse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunTrigger {
    /// User action; a refusal owes them an answer.
    Requested,
    /// Periodic timer or self-scheduled repair; one log line, then quiet.
    Scheduled,
}

pub(super) struct Inner {
    pub(super) config: Config,
    pub(super) indexing: Arc<IndexingService>,
    pub(super) shared: Arc<Mutex<Shared>>,
    pub(super) notify: Notify,
    pub(super) awake: bool,
    pub(super) reconcile_stop: Arc<ReconcileStop>,
    pub(super) event_tx: mpsc::Sender<FsEvent>,
    pub(super) event_rx: mpsc::Receiver<FsEvent>,
    pub(super) watcher: Option<Watcher>,
    pub(super) watcher_config: WatcherConfig,
    pub(super) watcher_rx: Option<mpsc::Receiver<(u64, Result<Watcher, WatchError>)>>,
    pub(super) watcher_gen: u64,
    pub(super) pending: HashMap<PathBuf, FsEvent>,
    /// Paths a frontend asked for by name ([`IndexCoordinator::update_paths`]).
    /// Survives [`Inner::clear_pending`] and applies in manual mode: it keeps
    /// the rows a user is *reading* in step with the disk.
    pub(super) targeted: HashMap<PathBuf, FsEvent>,
    /// How far a directory event got before its turn's budget ran out.
    pub(super) resume_from: HashMap<PathBuf, usize>,
    /// Most recent event arrival; the burst is over once `pending_settle` old.
    pub(super) last_event_at: Option<Instant>,
    /// Oldest un-applied event; a trickle cannot defer past `pending_max_defer`.
    pub(super) pending_since: Option<Instant>,
    pub(super) needs_full_run: bool,
    /// Earliest time another full run may be *attempted* after a refusal.
    pub(super) run_retry_at: Option<Instant>,
    pub(super) run_retry_delay: Duration,
    /// Reconciliation owed to a config change, part-applied across ticks;
    /// unlike `needs_full_run` it is acted on in manual mode too.
    pub(super) pending_work: Option<WorkCursor>,
    pub(super) reconcile_done: Option<(ReconcileProgress, Instant)>,
    /// A reconciliation was abandoned part-way; read by [`Inner::teardown`].
    pub(super) reconcile_cut_short: bool,
    /// Cleared once the service reports running; detects idle-after-running.
    pub(super) saw_running: bool,
    pub(super) files_at: Option<Instant>,
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
                self.start_full_run(RunTrigger::Requested);
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
                // The write connection may point at an old database_path.
                self.write_conn = None;
                self.files_at = None;
                if !actions.requires_rebuild && !actions.work.is_empty() {
                    self.start_work(actions.work);
                }
                if want_auto && self.mode != IndexMode::Auto {
                    self.enter_auto();
                } else if !want_auto && self.mode == IndexMode::Auto {
                    self.enter_manual_stopped();
                } else if self.mode == IndexMode::Auto {
                    self.start_watcher();
                }
                self.refresh_last_full_index();
            }
            CoordCmd::RebuildIndex => {
                let db = self.db_path();
                self.write_conn = None;
                self.files_at = None;
                self.pending_work = None;
                self.reconcile_done = None;
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    // A run started now would reopen the *old* index and
                    // present itself as the rebuild the user asked for.
                    crate::log_warn!("coordinator: rebuild: {}", e);
                    self.indexing
                        .report_error(format!("could not rebuild the index: {}", e));
                    return;
                }
                self.clear_root_counts();
                self.start_full_run(RunTrigger::Requested);
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
                // Zero, not `None`: no later read corrects a stale figure.
                shared.files = Some(0);
                shared.root_counts = Arc::new(Vec::new());
                drop(shared);
                self.files_at = None;
            }
            CoordCmd::UpdatePaths(paths) => {
                // Only paths under an indexed root: nothing downstream checks,
                // and a file renamed *out* would be indexed at its new home.
                let prefixes: Vec<String> = self
                    .config
                    .normalized_indexing_paths()
                    .iter()
                    .map(|root| crate::file_handling::ExtractCursor::for_root(root).lo)
                    .collect();
                for path in paths {
                    let spelled = path.to_string_lossy();
                    if !prefixes.iter().any(|lo| spelled.starts_with(lo.as_str())) {
                        continue;
                    }
                    let Some(event) = verb_for(path) else {
                        continue;
                    };
                    enqueue(&mut self.targeted, event);
                }
                self.was_busy = true;
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
                // Single-writer rule: never touch the DB while a full run is
                // active; Optimizing holds a write transaction over the file.
                self.saw_running = true;
                return;
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => {}
        }

        if self.saw_running {
            self.saw_running = false;
            self.was_busy = true;
            // An errored run never stamps `last_full_index`, so `periodic_due`
            // stays true; without the backoff a walk would start every second.
            if matches!(status, IndexingStatus::Error(_)) {
                self.defer_runs();
            }
            self.refresh_last_full_index();
            self.files_at = None;
            if self.mode == IndexMode::ManualRunning {
                self.mode = IndexMode::ManualStopped;
            }
        }

        self.refresh_file_count();

        // Ahead of every gate: rows a user is looking at right now.
        if !self.targeted.is_empty() {
            self.apply_targeted();
        }

        // Ahead of the mode gate: reconciled in manual mode too.
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

        let deferred = self.run_retry_at.is_some_and(|at| Instant::now() < at);
        if (self.needs_full_run || self.periodic_due()) && !deferred {
            self.start_full_run(RunTrigger::Scheduled);
            worked = true;
        }

        // Only when this tick found nothing to do: releasing the connection
        // between batches would reopen it moments later with a cold cache.
        if !worked {
            self.go_idle();
        }
    }

    /// Settle: drop [`Inner::write_conn`] (and its page cache), then return
    /// freed heap to the kernel — in that order.
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

    /// Queue reconciliation for a config change. An in-flight plan is folded
    /// in and restarted: the new diff is against the same previous config, so
    /// it cannot know what the old plan left undone.
    fn start_work(&mut self, mut work: IndexWork) {
        if let Some(outstanding) = self.pending_work.take() {
            work.merge_from(outstanding.work());
        }
        match WorkCursor::new(work, &self.config) {
            Ok(cursor) => self.pending_work = Some(cursor),
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
            // Armed only for the slice; the cancellation must not end whatever
            // else this connection runs.
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
            // A cancelled statement fails like any other; ask our own flag.
            if self.reconcile_stop.cancelled() {
                self.reconcile_cut_short = true;
                crate::log_info!(
                    "configuration change interrupted after {} index entries; \
                     the next indexing run starts it again",
                    cursor.progress().examined
                );
                return;
            }
            // Not retried — a persistent error would spin this loop forever.
            crate::log_warn!(
                "coordinator: reconcile: {}; leaving it to the next indexing run",
                e
            );
            return;
        }
        if !cursor.done() {
            // Nothing is recorded for a pass that stopped early: the stale
            // record is what makes the next run redo it.
            self.pending_work = Some(cursor);
            return;
        }
        self.reconcile_done = Some((cursor.progress(), Instant::now()));
        if let Some(conn) = self.write_conn.as_ref() {
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
        if cursor.reindex() {
            self.start_full_run(RunTrigger::Requested);
            if self.mode != IndexMode::Auto {
                self.mode = IndexMode::ManualRunning;
            }
        }
    }

    /// Drop the queue and the timers that describe it, so a stale
    /// `pending_since` cannot force an immediate apply of the next event.
    fn clear_pending(&mut self) {
        self.pending.clear();
        // `clear` keeps capacity — up to 100k slots after a storm.
        self.pending.shrink_to_fit();
        // Resume points for `targeted` survive: that queue outlives this call.
        self.resume_from
            .retain(|p, _| self.targeted.contains_key(p));
        self.last_event_at = None;
        self.pending_since = None;
    }

    fn pending_settled(&self) -> bool {
        let quiet = self
            .last_event_at
            .is_none_or(|t| t.elapsed() >= self.watcher_config.pending_settle);
        let overdue = self
            .pending_since
            .is_some_and(|t| t.elapsed() >= self.watcher_config.pending_max_defer);
        quiet || overdue
    }

    /// Apply as much of the queue as fits in [`APPLY_BUDGET`], removals first:
    /// the queue is an unordered map, and an arbitrary order could delete a
    /// row a `Create` in the same batch had just written.
    fn apply_pending(&mut self) {
        self.was_busy = true;
        let mut conn = match self.ensure_write_conn() {
            Ok(conn) => conn,
            Err(e) => {
                crate::log_warn!(
                    "coordinator: incremental unavailable ({}); scheduling full run",
                    e
                );
                self.needs_full_run = true;
                return;
            }
        };
        self.apply_queue(&mut conn, false);
        self.write_conn = Some(conn);
        // The remainder goes to the next tick immediately: the pause was
        // ours, not the filesystem's, so it must not re-arm the settle window.
        self.last_event_at = None;
        if self.pending.is_empty() {
            self.pending_since = None;
        }
    }

    /// Drain one queue's removals, then its upserts, within [`APPLY_BUDGET`].
    /// `targeted` picks the queue and the failure policy: pending failures
    /// escalate to [`Inner::needs_full_run`], targeted ones only log.
    fn apply_queue(&mut self, conn: &mut Connection, targeted: bool) {
        let deadline = Instant::now() + APPLY_BUDGET;
        let chunk = self.config.processing.batch_size.max(1);
        let queue = if targeted { &self.targeted } else { &self.pending };

        let removals: Vec<PathBuf> = queue
            .iter()
            .filter(|(_, ev)| is_removal(ev))
            .map(|(p, _)| p.clone())
            .collect();
        for batch in removals.chunks(chunk) {
            if let Err(e) = crate::incremental::remove_paths(conn, batch, chunk) {
                // The batch leaves the queue either way — retrying a failed
                // write every tick is worse; the full run recovers the rows.
                if targeted {
                    crate::log_warn!("coordinator: targeted remove: {}", e);
                } else {
                    crate::log_warn!("coordinator: remove: {}; scheduling full run", e);
                    self.needs_full_run = true;
                }
            }
            let queue = if targeted {
                &mut self.targeted
            } else {
                &mut self.pending
            };
            for path in batch {
                queue.remove(path);
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        if Instant::now() < deadline {
            let queue = if targeted { &self.targeted } else { &self.pending };
            let upserts: Vec<PathBuf> = queue
                .iter()
                .filter(|(_, ev)| !is_removal(ev))
                .map(|(p, _)| p.clone())
                .collect();
            for path in upserts {
                let queue = if targeted {
                    &mut self.targeted
                } else {
                    &mut self.pending
                };
                let Some(ev) = queue.remove(&path) else {
                    continue;
                };
                match apply_fs_event(
                    conn,
                    &ev,
                    &self.config,
                    &self.ignore,
                    &self.registry,
                    &Budget {
                        deadline,
                        cancel: &self.reconcile_stop.cancel,
                        resume_from: self.resume_from.remove(&path).unwrap_or(0),
                    },
                ) {
                    // A directory event can cover a whole moved-in tree. Put
                    // it back with how far it got, so one `mv` cannot hold
                    // this loop — or the shutdown behind it — indefinitely.
                    Ok(Applied::Unfinished { done }) => {
                        self.resume_from.insert(path.clone(), done);
                        let queue = if targeted {
                            &mut self.targeted
                        } else {
                            &mut self.pending
                        };
                        queue.insert(path, ev);
                        break;
                    }
                    Ok(Applied::Done) => {}
                    Err(e) if targeted => {
                        crate::log_warn!("coordinator: targeted apply {:?}: {}", ev, e);
                    }
                    Err(e) => {
                        crate::log_warn!("coordinator: apply {:?}: {}; scheduling full run", ev, e);
                        self.needs_full_run = true;
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }

    /// Apply the by-name queue: the paths a frontend is displaying. Never
    /// escalates to [`Inner::needs_full_run`]: a frontend reads what it shows
    /// from the file itself, so a failure here leaves the screen correct and
    /// only the index behind.
    fn apply_targeted(&mut self) {
        self.was_busy = true;
        let mut conn = match self.ensure_write_conn() {
            Ok(conn) => conn,
            Err(e) => {
                crate::log_warn!("coordinator: targeted update unavailable: {}", e);
                self.targeted.clear();
                // A stale resume point would misapply to the path's next event.
                self.resume_from.retain(|p, _| self.pending.contains_key(p));
                return;
            }
        };
        self.apply_queue(&mut conn, true);
        self.write_conn = Some(conn);
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
        // `.max(1)`: at zero every tick is "due" and runs would be
        // back-to-back forever; manual mode is how you say "never".
        let interval_secs = self
            .config
            .indexing
            .reindex_interval_minutes
            .max(1)
            .saturating_mul(60);
        let last = crate::lock_ok(&self.shared).last_full_index;
        match last {
            None => true,
            Some(last) => {
                let now = now_unix();
                // A stamp ahead of the clock (NTP correction, moved index)
                // would suppress the reindex for the skew's life; treat as due.
                now < last || now - last >= interval_secs
            }
        }
    }

    fn start_full_run(&mut self, trigger: RunTrigger) {
        // A run already in progress is not a refusal: refusing would back off
        // future runs and replace visible progress with an error. Also covers
        // `Optimizing`, which `start_indexing` does not reject.
        if !matches!(
            self.indexing.get_status(),
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) {
            return;
        }
        let roots: Vec<String> = self
            .config
            .resolved_indexing_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if roots.is_empty() {
            self.refuse(trigger, &NO_ROOTS, "no indexing roots are configured");
            return;
        }
        // Backstop for hand-edited configs; the GUI rejects nested roots.
        let nested = crate::config::nested_roots(&roots);
        if !nested.is_empty() {
            let detail = nested
                .iter()
                .map(|(child, parent)| format!("{} is nested under {}", child, parent))
                .collect::<Vec<_>>()
                .join("; ");
            self.refuse(
                trigger,
                &NESTED_ROOTS,
                &format!("refusing to index nested roots: {}", detail),
            );
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
        // A stale resume point would `skip` entries of an unrelated walk.
        self.resume_from
            .retain(|p, _| self.pending.contains_key(p) || self.targeted.contains_key(p));
        if self.pending.is_empty() {
            self.last_event_at = None;
            self.pending_since = None;
        }
        if let Err(e) = self
            .indexing
            .start_indexing(roots, self.db_path(), self.config.clone())
        {
            self.refuse(
                trigger,
                &START_FAILURES,
                &format!("could not start indexing: {}", e),
            );
            return;
        }
        self.run_retry_at = None;
        self.run_retry_delay = RUN_RETRY_BASE;
        NO_ROOTS.reset();
        NESTED_ROOTS.reset();
        START_FAILURES.reset();
        // `start_indexing` claims Running before returning: no window in
        // which this thread writes to a database the run is about to reopen.
        self.saw_running = true;
    }

    /// Refuse a full run: back off, and say so at a volume the trigger earns.
    /// The throttles keep the scheduled retry from evicting the log ring but
    /// must never silence a run somebody asked for, so a requested run resets
    /// the throttle and publishes the reason where the user is looking.
    fn refuse(&mut self, trigger: RunTrigger, throttle: &crate::log::Throttle, reason: &str) {
        self.defer_runs();
        if trigger == RunTrigger::Requested {
            throttle.reset();
            self.indexing.report_error(reason.to_string());
        }
        if throttle.allow() {
            crate::log_warn!("coordinator: {}", reason);
        }
    }

    /// Hold off further full runs, doubling the wait each time.
    /// `needs_full_run` is cleared too, or `tick` would retry every second.
    fn defer_runs(&mut self) {
        self.needs_full_run = false;
        self.run_retry_at = Some(Instant::now() + self.run_retry_delay);
        self.run_retry_delay = (self.run_retry_delay * 2).min(RUN_RETRY_MAX);
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
            // Signal only — waiting here would stall every queued command.
            self.indexing.request_stop();
        }
    }

    /// Begin watcher startup WITHOUT blocking the command loop — registering
    /// inotify watches walks every root, minutes on large or networked trees.
    /// A generation counter discards superseded registrations.
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
        // Same filters the indexer walks with; no descriptor is spent on
        // directories whose events would be discarded on arrival.
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
                // A failed send means the coordinator moved on; the dropped
                // watcher unregisters itself.
                let _ = tx.send((generation, result));
            });
        if spawned.is_err() {
            self.watcher_rx = None;
            self.set_watcher_status(WatcherStatus::Off);
        }
    }

    /// Collect a finished watcher registration, if any.
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

    /// Tear the watcher down if it ran out of watch budget after starting, and
    /// schedule a full run: a partially watched tree looks live while going
    /// silently out of date.
    fn check_watcher_degraded(&mut self) {
        let Some(w) = &self.watcher else {
            return;
        };
        let Some(mut reason) = w.degraded_reason() else {
            return;
        };
        // An overflow is not a capacity problem: `inotify` and
        // `ReadDirectoryChangesW` both keep delivering after their queue
        // overflows — the rescan flag means "you missed some", not "this
        // watch is broken" — so schedule the run that finds out; do not tear
        // down and re-register. Consuming the reason makes this a one-shot: a
        // standing `Overflowed` would re-arm `needs_full_run` every tick and
        // hide a later `KernelLimit`.
        if matches!(reason, WatchError::Overflowed) {
            w.clear_degraded();
            crate::log_warn!("watcher: {}", reason);
            self.needs_full_run = true;
            return;
        }
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
    /// Called only from the idle half of [`Inner::tick`], so it cannot run
    /// while a full run holds the database; `COUNT(*)` is still a key scan —
    /// hence the interval and the interrupt guard.
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
            Err(e) => crate::log_warn!("coordinator: file count unavailable: {}", e),
        }
    }

    /// Re-read the last completed run's stamp and per-root figures. A failed
    /// open is *not* published as `None`: `periodic_due` reads `None` as
    /// "never indexed" and would start a fresh run every tick.
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
    /// spelling the config uses. The `schema_info` keys are canonicalized, so
    /// re-spelling a root (`~/docs` vs `/home/me/docs`) keeps the figures.
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
        shared.queued_events = self.pending.len() + self.targeted.len();
        shared.reconcile = reconcile;
        drop(shared);

        // Edge-triggered: a settled window would never observe the first
        // movement, and a level-triggered call costs a wake-up per second.
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
            // A cut-short reconcile must not checkpoint: it can have written
            // much WAL, and a TRUNCATE checkpoint is the wait the cancellation
            // just spared. Dropping is safe — the next run lands the log.
            if idle && !cut_short {
                db::repo::checkpoint_and_close(conn);
            }
        }
    }
}
