//! The coordinator's event loop: one thread owning the watcher, the
//! debounce buffer, and the incremental reconcile cursor.

use super::*;

/// First wait after a full run is refused or fails to start.
const RUN_RETRY_BASE: Duration = Duration::from_secs(30);

const RUN_RETRY_MAX: Duration = Duration::from_secs(300);

/// The watcher's event queue and the two timers that decide when to apply
/// it: once no event has arrived for `pending_settle`, or once the oldest
/// has waited `pending_max_defer`, so a trickle cannot defer it forever.
#[derive(Default)]
pub(super) struct Debounced {
    pub(super) events: HashMap<PathBuf, FsEvent>,
    /// Most recent arrival.
    last_at: Option<Instant>,
    /// Oldest un-applied arrival.
    since: Option<Instant>,
}

impl Debounced {
    /// Queue `events`, stamping the arrival if there were any; `true` if so.
    fn push(&mut self, events: impl Iterator<Item = FsEvent>) -> bool {
        let mut received = false;
        for ev in events {
            enqueue(&mut self.events, ev);
            received = true;
        }
        if received {
            let now = Instant::now();
            self.last_at = Some(now);
            self.since.get_or_insert(now);
        }
        received
    }

    fn settled(&self, config: &WatcherConfig) -> bool {
        let quiet = self
            .last_at
            .is_none_or(|t| t.elapsed() >= config.pending_settle);
        let overdue = self
            .since
            .is_some_and(|t| t.elapsed() >= config.pending_max_defer);
        quiet || overdue
    }

    /// Drop the queue and its timers, so a stale `since` cannot force an
    /// immediate apply of the next event.
    fn clear(&mut self) {
        self.events.clear();
        // `clear` keeps capacity: up to 100k slots after a storm.
        self.events.shrink_to_fit();
        self.last_at = None;
        self.since = None;
    }

    /// Keep only the removals, dropping the timers if nothing is left.
    fn keep_removals(&mut self) {
        self.events.retain(|_, ev| is_removal(ev));
        if self.events.is_empty() {
            self.last_at = None;
            self.since = None;
        }
    }

    /// After an apply that ran out of budget: the remainder goes to the next
    /// tick at once, since the pause was ours, not the filesystem's, and must
    /// not re-arm the settle window.
    fn after_partial_apply(&mut self) {
        self.last_at = None;
        if self.events.is_empty() {
            self.since = None;
        }
    }
}

/// When another full run may be attempted after one is refused or fails:
/// the wait doubles from [`RUN_RETRY_BASE`] up to [`RUN_RETRY_MAX`].
pub(super) struct Backoff {
    until: Option<Instant>,
    delay: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            until: None,
            delay: RUN_RETRY_BASE,
        }
    }
}

impl Backoff {
    fn defer(&mut self) {
        self.until = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(RUN_RETRY_MAX);
    }

    fn reset(&mut self) {
        *self = Backoff::default();
    }

    fn active(&self) -> bool {
        self.until.is_some_and(|at| Instant::now() < at)
    }
}

/// Log a failed apply of `what` by the queue's policy: a targeted failure
/// only logs, a pending one also schedules a full run. `true` for the latter.
fn apply_failed(targeted: bool, what: &str, e: impl std::fmt::Display) -> bool {
    if targeted {
        crate::log_warn!("coordinator: targeted {}: {}", what, e);
    } else {
        crate::log_warn!("coordinator: {}: {}; scheduling full run", what, e);
    }
    !targeted
}

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
    pub(super) pending: Debounced,
    /// Paths a frontend asked for by name ([`IndexCoordinator::update_paths`]).
    /// Survives [`Inner::clear_pending`] and applies in manual mode: it keeps
    /// the rows a user is *reading* in step with the disk.
    pub(super) targeted: HashMap<PathBuf, FsEvent>,
    /// How far a directory event got before its turn's budget ran out.
    pub(super) resume_from: HashMap<PathBuf, usize>,
    pub(super) needs_full_run: bool,
    pub(super) run_retry: Backoff,
    /// Reconciliation owed to a config change, part-applied across ticks;
    /// unlike `needs_full_run` it is acted on in manual mode too.
    pub(super) pending_work: Option<WorkCursor>,
    pub(super) reconcile_done: Option<(ReconcileProgress, Instant)>,
    /// A reconciliation was abandoned part-way; read by [`Inner::teardown`].
    pub(super) reconcile_cut_short: bool,
    /// Set while a full run holds the database; cleared by the tick that sees
    /// it end.
    pub(super) run_in_flight: bool,
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
                self.start_requested_run()
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
                match (want_auto, self.mode == IndexMode::Auto) {
                    (true, false) => self.enter_auto(),
                    (false, true) => self.enter_manual_stopped(),
                    // Still automatic: the watcher follows the new config.
                    (true, true) => self.start_watcher(),
                    (false, false) => {}
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
                self.start_requested_run();
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
                    .map(|root| crate::file_handling::RootRange::for_root(root).lo)
                    .collect();
                let under_root = |path: &PathBuf| {
                    let spelled = path.to_string_lossy();
                    prefixes.iter().any(|lo| spelled.starts_with(lo.as_str()))
                };
                paths
                    .into_iter()
                    .filter(under_root)
                    .filter_map(verb_for)
                    .for_each(|event| enqueue(&mut self.targeted, event));
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
                self.run_in_flight = true;
                return;
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => {}
        }

        if self.run_in_flight {
            self.run_in_flight = false;
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
        if !self.pending.events.is_empty()
            && !self.needs_full_run
            && self.pending.settled(&self.watcher_config)
        {
            self.apply_pending();
            worked = true;
        }

        let deferred = self.run_retry.active();
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
    /// freed heap to the kernel, in that order.
    fn go_idle(&mut self) {
        if !self.was_busy {
            return;
        }
        self.was_busy = false;
        self.write_conn = None;
        crate::platform::release_free_heap();
    }

    fn drain_events(&mut self) {
        if self.pending.push(self.event_rx.try_iter()) {
            self.was_busy = true;
            // Before the overflow test: collapsed, an `rm -rf` storm stays
            // under [`PENDING_OVERFLOW`] instead of forcing a full run.
            collapse_pending_removals(&mut self.pending.events);
        }
        if self.pending.events.len() > PENDING_OVERFLOW {
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
        // Restored before anything below: `start_full_run` takes it back.
        self.write_conn = Some(conn);
        match outcome {
            // A cancelled statement fails like any other; ask our own flag.
            Err(_) if self.reconcile_stop.cancelled() => {
                self.reconcile_cut_short = true;
                crate::log_info!(
                    "configuration change interrupted after {} index entries; \
                     the next indexing run starts it again",
                    cursor.progress().examined
                );
                return;
            }
            // Not retried: a persistent error would spin this loop forever.
            Err(e) => {
                crate::log_warn!(
                    "coordinator: reconcile: {}; leaving it to the next indexing run",
                    e
                );
                return;
            }
            Ok(_) => {}
        }
        if !cursor.done() {
            // Nothing is recorded for a pass that stopped early: the stale
            // record makes the next run redo it.
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
            self.start_requested_run();
        }
    }

    /// Drop the event queue and its timers (see [`Debounced::clear`]).
    fn clear_pending(&mut self) {
        self.pending.clear();
        // Resume points for `targeted` survive: that queue outlives this call.
        self.resume_from
            .retain(|p, _| self.targeted.contains_key(p));
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
        self.pending.after_partial_apply();
    }

    /// Drain one queue's removals, then its upserts, within [`APPLY_BUDGET`].
    /// `targeted` picks the queue and the failure policy: pending failures
    /// escalate to [`Inner::needs_full_run`], targeted ones only log.
    fn apply_queue(&mut self, conn: &mut Connection, targeted: bool) {
        let deadline = Instant::now() + APPLY_BUDGET;
        let chunk = self.config.processing.batch_size.max(1);
        let queue = if targeted {
            &mut self.targeted
        } else {
            &mut self.pending.events
        };

        let removals: Vec<PathBuf> = queue
            .iter()
            .filter(|(_, ev)| is_removal(ev))
            .map(|(p, _)| p.clone())
            .collect();
        for batch in removals.chunks(chunk) {
            if let Err(e) = crate::incremental::remove_paths(conn, batch, chunk) {
                // The batch leaves the queue either way: retrying a failed
                // write every tick is worse, and the full run recovers the rows.
                self.needs_full_run |= apply_failed(targeted, "remove", e);
            }
            for path in batch {
                queue.remove(path);
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        if Instant::now() < deadline {
            let upserts: Vec<PathBuf> = queue
                .iter()
                .filter(|(_, ev)| !is_removal(ev))
                .map(|(p, _)| p.clone())
                .collect();
            for path in upserts {
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
                    // this loop, or the shutdown behind it, indefinitely.
                    Ok(Applied::Unfinished { done }) => {
                        self.resume_from.insert(path.clone(), done);
                        queue.insert(path, ev);
                        break;
                    }
                    Ok(Applied::Done) => {}
                    Err(e) => {
                        let what = format!("apply {:?}", ev);
                        self.needs_full_run |= apply_failed(targeted, &what, e);
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
                self.resume_from
                    .retain(|p, _| self.pending.events.contains_key(p));
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
        // A stamp ahead of the clock (NTP correction, moved index) would
        // suppress the reindex for the skew's life; `checked_sub` reads it as
        // due.
        last.is_none_or(|last| {
            now_unix()
                .checked_sub(last)
                .is_none_or(|age| age >= interval_secs)
        })
    }

    /// A full run somebody asked for; in manual mode it reads as
    /// `ManualRunning` until it ends.
    fn start_requested_run(&mut self) {
        self.start_full_run(RunTrigger::Requested);
        if self.mode != IndexMode::Auto {
            self.mode = IndexMode::ManualRunning;
        }
    }

    fn start_full_run(&mut self, trigger: RunTrigger) {
        // A run already in progress is not a refusal: refusing would back off
        // future runs and replace visible progress with an error. Also covers
        // `Optimizing`, which `start_indexing` does not reject.
        if self.indexing.is_active() {
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
        // Creates and modifies are dropped: the walk rediscovers them.
        // Removals are kept: the walk cannot see a deletion under an
        // unreadable directory (`unreadable.covers`) or of an aliased symlink
        // target (`aliased_paths`), and those rows would leak until a rebuild.
        self.pending.keep_removals();
        // A stale resume point would `skip` entries of an unrelated walk.
        self.resume_from
            .retain(|p, _| self.pending.events.contains_key(p) || self.targeted.contains_key(p));
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
        self.run_retry.reset();
        NO_ROOTS.reset();
        NESTED_ROOTS.reset();
        START_FAILURES.reset();
        // `start_indexing` claims Running before returning: no window in
        // which this thread writes to a database the run is about to reopen.
        self.run_in_flight = true;
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
        self.run_retry.defer();
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
        if self.indexing.is_active() {
            // Signal only: waiting here would stall every queued command.
            self.indexing.request_stop();
        }
    }

    /// Begin watcher startup without blocking the command loop: registering
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
        let (generation, result) = match rx.try_recv() {
            Ok(message) => message,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.watcher_rx = None;
                self.set_watcher_status(WatcherStatus::Off);
                return;
            }
        };
        self.watcher_rx = None;
        if generation != self.watcher_gen {
            return; // superseded; the watcher drops and unregisters
        }
        let status = match result {
            Ok(w) => {
                let status = WatcherStatus::Active {
                    dirs: w.watched_dirs(),
                };
                self.watcher = Some(w);
                status
            }
            Err(e) => {
                crate::log_warn!("coordinator: watcher: {}", e);
                WatcherStatus::Disabled { reason: e }
            }
        };
        self.set_watcher_status(status);
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
        // Schedule the run that finds out what was missed, without tearing
        // down, and consume the reason; see [`WatchError::Overflowed`].
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
    /// while a full run holds the database; `COUNT(*)` is still a key scan,
    /// hence the interval and the interrupt guard.
    fn refresh_file_count(&mut self) {
        if self
            .files_at
            .is_some_and(|at| at.elapsed() < FILE_COUNT_INTERVAL)
        {
            return;
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
            .indexing_roots()
            .filter_map(|(raw, root)| {
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
                // The tail ages out here, not in `tick`: tick returns early
                // for the whole length of a run.
                let now = Instant::now();
                self.reconcile_done = self
                    .reconcile_done
                    .filter(|(_, at)| summary_is_fresh(*at, now));
                self.reconcile_done
                    .map(|(progress, _)| ReconcileState::Finished(progress))
            }
        };
        let busy = reconcile.is_some() || self.indexing.is_active();
        let mut shared = crate::lock_ok(&self.shared);
        shared.mode = self.mode;
        shared.queued_events = self.pending.events.len() + self.targeted.len();
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
    /// close. Signal, don't wait: an abandoned run is safe under WAL, and an
    /// abandoned reconcile is redone by the next run.
    fn teardown(mut self) {
        self.stop_watcher();
        // Read once: `idle` gates two decisions that must agree.
        let idle = !self.indexing.is_active();
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
            // just spared. Dropping is safe: the next run lands the log.
            if idle && !cut_short {
                db::repo::checkpoint_and_close(conn);
            }
        }
    }
}
