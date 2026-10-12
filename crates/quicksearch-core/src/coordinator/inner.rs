//! The coordinator's event loop: one thread owning the watcher, the
//! debounce buffer, and the incremental reconcile cursor.

use super::*;

/// Time between ticks, unless a tick asks to continue at once.
const TICK: Duration = Duration::from_secs(1);

/// First wait after a full run is refused or fails to start.
const RUN_RETRY_BASE: Duration = Duration::from_secs(30);

const RUN_RETRY_MAX: Duration = Duration::from_secs(300);

/// The least time between two re-registrations an overflow forces. Each one
/// walks every root, and a storm that overflows the fresh watcher as soon as
/// it is up would otherwise spend itself walking, and scheduling full runs;
/// near `max_user_watches` a registration that follows the last too soon
/// can also meet the descriptors the old watcher is still releasing.
const OVERFLOW_REWATCH_GAP: Duration = Duration::from_secs(300);

/// An event queue and the two timers that decide when to apply it: once no
/// event has arrived for `pending_settle`, or once the oldest has waited
/// `pending_max_defer`, so a trickle cannot defer it forever. That holds under
/// a stream of commands too: the tick that checks the timers keeps time (see
/// [`Inner::run`]). One for the watcher's events, one for the paths a
/// frontend is displaying. Events fold into two sets ([`enqueue`]), applied
/// removals first ([`apply_slice`]).
#[derive(Default)]
pub(super) struct Debounced {
    pub(super) removals: HashSet<PathBuf>,
    /// Paths to bring in step with the disk; `true` walks a directory (a
    /// `Create`), `false` does not (a `Modify`).
    pub(super) upserts: HashMap<PathBuf, bool>,
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
            enqueue(self, ev);
            received = true;
        }
        if received {
            let now = Instant::now();
            self.last_at = Some(now);
            self.since.get_or_insert(now);
        }
        received
    }

    pub(super) fn len(&self) -> usize {
        self.removals.len() + self.upserts.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.removals.is_empty() && self.upserts.is_empty()
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
        // Replaced, not cleared: `clear` keeps capacity, up to 100k slots
        // after a storm.
        self.removals = HashSet::new();
        self.upserts = HashMap::new();
        self.last_at = None;
        self.since = None;
    }

    /// Keep only the removals, dropping the timers if nothing is left.
    fn keep_removals(&mut self) {
        self.upserts.clear();
        if self.is_empty() {
            self.last_at = None;
            self.since = None;
        }
    }

    /// After an apply that ran out of budget: the remainder goes to the next
    /// tick (1 s later) without re-arming the settle window, since the pause
    /// was ours, not the filesystem's.
    fn after_partial_apply(&mut self) {
        self.last_at = None;
        if self.is_empty() {
            self.since = None;
        }
    }
}

/// When another full run may be attempted after one is refused or fails:
/// the wait doubles from [`RUN_RETRY_BASE`] up to [`RUN_RETRY_MAX`], and only
/// a run that ends cleanly resets it.
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

/// Whether `new` changes anything [`Inner::start_watcher`] registers with.
/// Every other edit (theme, shortcut, search limits) keeps the running
/// watcher: restarting it re-walks every directory under every root to
/// re-register its watch, and misses whatever changes in the gap.
pub(super) fn watch_inputs_differ(old: &Config, new: &Config) -> bool {
    old.resolved_indexing_paths() != new.resolved_indexing_paths()
        || old.indexing.include_hidden != new.indexing.include_hidden
        || old.indexing.ignore_patterns != new.indexing.ignore_patterns
}

/// Whether a config change restarts the watcher in automatic mode, given what
/// it is doing now (`status`): when the watch inputs changed, or when nothing
/// is watching or registering, so an edit retries a start that failed. With
/// the same inputs two states are left alone: a registration in flight, which
/// a restart would only begin again, and a tree over the directory cap, which
/// a retry would walk up to the cap only to fail the same way.
pub(super) fn rewatch_needed(old: &Config, new: &Config, status: &WatcherStatus) -> bool {
    watch_inputs_differ(old, new)
        || match status {
            WatcherStatus::Starting | WatcherStatus::Active { .. } => false,
            WatcherStatus::Disabled {
                reason: WatchError::TooManyDirectories { .. },
            } => false,
            WatcherStatus::Off | WatcherStatus::Disabled { .. } => true,
        }
}

/// Whether `ev` names one of the index's own files, at either end of a rename.
fn names_an_index_file(config: &Config, ev: &FsEvent) -> bool {
    match ev {
        FsEvent::Create(p) | FsEvent::Modify(p) | FsEvent::Remove(p) => config.is_index_file(p),
        FsEvent::Rename { from, to } => config.is_index_file(from) || config.is_index_file(to),
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

/// What one [`apply_slice`] reads besides its queue.
pub(super) struct SliceCx<'a> {
    pub(super) config: &'a Config,
    /// The last set that compiled, so a bad live glob cannot stall the queue.
    pub(super) ignore: &'a Arc<IgnoreSet>,
    pub(super) registry: &'a Registry,
    pub(super) cancel: &'a AtomicBool,
    /// Read before the turn's stop check ([`crate::incremental::Budget::kills`]).
    pub(super) kills: KillMark,
    /// Which failure policy applies ([`apply_failed`]).
    pub(super) targeted: bool,
}

/// Apply `queue` until `deadline`, inside `tx`: removals first, then, once
/// none is left, upserts. Each call applies at least one entry, so a turn
/// handed a spent deadline still drains the queue; after a removal a spent
/// deadline ends the call before any other upsert. Removals go first because
/// the sets are unordered, and a removal after an upsert could delete the
/// rows it just wrote. A removal whose path also has a `Create` queued
/// deletes the row and the whole subtree unexamined, and the first turn of
/// the walk that re-derives whatever is real runs with it, in `tx`
/// ([`crate::incremental::remove_path`]): a queue dropped between slices
/// drops the two together, never the walk alone. A directory walk the
/// deadline cuts short goes back into the queue with its entry in
/// `resume_from`, so one `mv` of a large tree cannot hold the command loop,
/// or the shutdown behind it, indefinitely. `true` if an apply failed in a
/// way that owes a full run.
pub(super) fn apply_slice(
    tx: &Transaction<'_>,
    queue: &mut Debounced,
    resume_from: &mut HashMap<PathBuf, Option<PathBuf>>,
    cx: &SliceCx<'_>,
    deadline: Instant,
) -> bool {
    let mut failed = false;
    let scope = Scope::new(cx.config, cx.ignore.clone());
    // Moved out, not cloned: what the deadline leaves goes back, so a storm's
    // queue is not copied whole on every slice. Measured (200k targeted
    // removals): drained in 10.0 → 9.1 s, peak RSS 85 → 81 MB.
    let mut removals = std::mem::take(&mut queue.removals).into_iter();
    while let Some(path) = removals.next() {
        // A walk under a removed path starts over: its tree is gone, or new.
        resume_from.retain(|walked, _| !walked.starts_with(&path));
        let rewalk = queue.upserts.get(&path) == Some(&true);
        if let Err(e) = crate::incremental::remove_path(tx, &path, rewalk) {
            // The path leaves the queue either way: retrying a failed write
            // every tick is worse, and the full run recovers the rows.
            failed |= apply_failed(cx.targeted, "remove", e);
        }
        if rewalk {
            let finished = upsert_turn(tx, &path, true, resume_from, cx, &scope, deadline)
                .unwrap_or_else(|e| {
                    failed |= apply_failed(cx.targeted, &format!("apply {}", path.display()), e);
                    true
                });
            if finished {
                queue.upserts.remove(&path);
            }
        }
        if Instant::now() >= deadline {
            queue.removals.extend(removals);
            return failed;
        }
    }

    let mut upserts = std::mem::take(&mut queue.upserts).into_iter();
    for (path, walk) in upserts.by_ref() {
        let finished = upsert_turn(tx, &path, walk, resume_from, cx, &scope, deadline)
            .unwrap_or_else(|e| {
                failed |= apply_failed(cx.targeted, &format!("apply {}", path.display()), e);
                true
            });
        if !finished {
            queue.upserts.insert(path, walk);
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    queue.upserts.extend(upserts);
    failed
}

/// One turn of the upsert of `path`, resumed if `resume_from` has an entry
/// for it. `Ok(false)` if the deadline cut a walk short: the entry is
/// recorded again, and the caller keeps the path queued.
fn upsert_turn(
    tx: &Transaction<'_>,
    path: &std::path::Path,
    walk: bool,
    resume_from: &mut HashMap<PathBuf, Option<PathBuf>>,
    cx: &SliceCx<'_>,
    scope: &Scope,
    deadline: Instant,
) -> Result<bool, String> {
    let resume = resume_from.remove(path);
    let budget = Budget {
        deadline,
        cancel: cx.cancel,
        kills: cx.kills,
        resumed: resume.is_some(),
        after: resume.flatten(),
    };
    let applied =
        crate::incremental::upsert_path(tx, path, walk, cx.config, scope, cx.registry, &budget)?;
    match applied {
        Applied::Done => Ok(true),
        Applied::Unfinished { after } => {
            resume_from.insert(path.to_path_buf(), after);
            Ok(false)
        }
    }
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
    pub(super) watcher_rx: Option<mpsc::Receiver<Result<Watcher, WatchError>>>,
    /// The latest start's cancel flag ([`Inner::start_watcher`]).
    pub(super) watcher_cancel: Arc<AtomicBool>,
    /// When an overflow last re-registered the watcher
    /// ([`OVERFLOW_REWATCH_GAP`]).
    pub(super) overflow_rewatched: Option<Instant>,
    pub(super) pending: Debounced,
    /// Paths a frontend asked for by name ([`IndexCoordinator::update_paths`]).
    /// Survives [`Inner::clear_pending`] and applies in manual mode: it keeps
    /// the rows a user is *reading* in step with the disk.
    pub(super) targeted: Debounced,
    /// The directory events a turn's budget cut short, keyed by the event's
    /// path: each is walked sorted from then on, after the last path it
    /// wrote, or from the start for `None` (the first turn walks unsorted,
    /// which leaves no resume point).
    pub(super) resume_from: HashMap<PathBuf, Option<PathBuf>>,
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
    /// The published row count may be behind the database: set by anything
    /// that could have written, cleared by [`Inner::refresh_file_count`].
    pub(super) files_stale: bool,
    /// Drives [`Inner::go_idle`] once per busy→idle transition.
    pub(super) was_busy: bool,
    pub(super) write_conn: Option<Connection>,
    /// The index was deleted for [`IndexCoordinator::delete_index`]: no full
    /// run starts until a rebuild or a clear ends the hold.
    pub(super) held: bool,
    /// Why a delete failed, refused while a run still held the activity
    /// ([`IndexingService::report_error`]); published by the tick that sees
    /// the run end, dropped by a run that starts first.
    pub(super) unreported: Option<String>,
    /// The last set that compiled. Shared with the watcher, which filters
    /// registrations by it, and with each queue turn's [`Scope`].
    pub(super) ignore: Arc<IgnoreSet>,
    pub(super) registry: Registry,
    pub(super) mode: IndexMode,
}

impl Inner {
    pub(super) fn run(mut self, cmd_rx: mpsc::Receiver<CoordCmd>) {
        if self.mode == IndexMode::Auto {
            self.enter_auto();
        }
        // Time drives the tick; a stream of commands must not postpone it.
        let mut next_tick = Instant::now() + TICK;
        loop {
            match cmd_rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(CoordCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    let had_work = self.pending_work.is_some();
                    self.handle_cmd(cmd);
                    // A config change's reconciliation starts at once.
                    if !had_work && self.pending_work.is_some() {
                        next_tick = Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= next_tick {
                let again = self.tick();
                next_tick = Instant::now() + if again { Duration::ZERO } else { TICK };
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
                let rewatch =
                    rewatch_needed(&self.config, &new, &crate::lock_ok(&self.shared).watcher);
                self.config = new;
                if let Err(e) = self.reload_filters() {
                    crate::log_warn!("coordinator: {}", e);
                }
                // The write connection may point at an old database_path.
                self.write_conn = None;
                self.files_stale = true;
                if !actions.requires_rebuild && !actions.work.is_empty() {
                    self.start_work(actions.work);
                }
                match (want_auto, self.mode == IndexMode::Auto) {
                    (true, false) => self.enter_auto(),
                    (false, true) => self.enter_manual_stopped(),
                    // Still automatic: the watcher follows the new config.
                    (true, true) if rewatch => self.start_watcher(),
                    (true, true) => {}
                    (false, false) => {}
                }
                // The figures read so far are the old file's. The new path
                // may hold no index yet; one that does republishes below.
                if actions.search_db_changed {
                    self.forget_published_index(None);
                }
                self.refresh_last_full_index();
            }
            CoordCmd::RebuildIndex => {
                self.held = false;
                // After a failed delete a run would reopen the *old* index
                // and present itself as the rebuild the user asked for.
                if self.delete_index("rebuild the index").is_ok() {
                    self.start_requested_run();
                }
            }
            CoordCmd::ClearIndex => {
                self.held = false;
                // Manual first: in Auto the periodic scheduler would see a
                // missing index and rebuild what was just deleted.
                self.enter_manual_stopped();
                // The figures are reset only once the file is gone.
                let _ = self.delete_index("clear the index");
            }
            CoordCmd::DeleteIndex(reply) => {
                let deleted = self.delete_index("delete the index");
                self.held = deleted.is_ok();
                // A caller that gave up waiting is no reason to undo the hold.
                let _ = reply.send(deleted);
            }
            CoordCmd::UpdatePaths(paths) => {
                // Refreshes only: `upsert_path` applies the walk's scope, and
                // leaves the index alone for a path it cannot stat or that
                // is a directory.
                self.targeted.push(paths.into_iter().map(FsEvent::Modify));
                self.was_busy = true;
            }
            #[cfg(test)]
            CoordCmd::DegradeWatcher(reason) => {
                if let Some(w) = &self.watcher {
                    w.force_degraded(reason);
                }
            }
            CoordCmd::Shutdown => unreachable!("handled in run()"),
        }
    }

    /// One round of upkeep. `true` asks for the next round at once: a
    /// reconciliation slice made progress and has more to do.
    fn tick(&mut self) -> bool {
        #[cfg(test)]
        {
            crate::lock_ok(&self.shared).ticks += 1;
        }
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
                return false;
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => {}
        }

        // Over the run's own outcome: the delete is what the user asked for.
        if let Some(message) = self.unreported.take() {
            self.indexing.report_error(message);
        }

        if self.run_in_flight {
            self.run_in_flight = false;
            self.was_busy = true;
            // An errored run never stamps `last_full_index`. With no fresh
            // stamp `periodic_due` stays true, and without the backoff a walk
            // would start every second; with one, the run it owed waits for
            // the interval.
            if matches!(status, IndexingStatus::Error(_)) {
                self.defer_runs();
            } else {
                self.run_retry.reset();
            }
            self.refresh_last_full_index();
            self.files_stale = true;
            if self.mode == IndexMode::ManualRunning {
                self.mode = IndexMode::ManualStopped;
            }
        }

        self.refresh_file_count();

        // Only when this tick found nothing to do does it go idle: releasing
        // the connection between batches would reopen it moments later with
        // a cold cache. A targeted remainder keeps it until a quiet tick.
        let mut worked = false;

        // Ahead of every gate: rows a user is looking at right now. Settled
        // like the watcher's queue, or a growing file on screen, re-sent on
        // every live refresh, is re-read, hashed and re-indexed every tick.
        if !self.targeted.is_empty() && self.targeted.settled(&self.watcher_config) {
            self.apply_targeted();
            worked = true;
        }

        // Ahead of the mode gate: reconciled in manual mode too.
        if self.pending_work.is_some() {
            self.apply_work();
            // A cancelled slice returns without progress; going again at once
            // would spin until the shutdown that set the flag arrives.
            return self.pending_work.is_some() && !self.reconcile_stop.cancelled();
        }

        if self.mode != IndexMode::Auto {
            if self.mode == IndexMode::ManualStopped {
                self.clear_pending();
            }
            if !worked {
                self.go_idle();
            }
            return false;
        }

        if !self.pending.is_empty()
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

        if !worked {
            self.go_idle();
        }
        false
    }

    /// Settle: drop [`Inner::write_conn`], then release freed heap, in that
    /// order. The release returns what mimalloc holds; SQLite's freed caches
    /// belong to the system malloc.
    fn go_idle(&mut self) {
        if !self.was_busy {
            return;
        }
        self.was_busy = false;
        // Whatever made it busy may have added or removed rows.
        self.files_stale = true;
        self.write_conn = None;
        crate::platform::release_free_heap();
    }

    fn drain_events(&mut self) {
        // The index's own files never queue (see `index_file_set`): with the
        // database under a watched root, every commit and every close
        // reports its `-wal` and `-shm`, and applying that is the next
        // commit.
        let events = self
            .event_rx
            .try_iter()
            .filter(|ev| !names_an_index_file(&self.config, ev));
        if self.pending.push(events) {
            self.was_busy = true;
            // Before the overflow test: collapsed, an `rm -rf` storm stays
            // under [`PENDING_OVERFLOW`] instead of forcing a full run.
            collapse_pending_removals(&mut self.pending.removals);
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
            // A recorded root stays recorded unless this pass dropped it by
            // that spelling. An offline root configured through a link was
            // dropped by the link's, which owns no rows; forgetting its target
            // would orphan them. The next run's reconcile drops the target,
            // by its recorded spelling, once every configured root resolves.
            let stamped = crate::scope::stored_config(conn, &self.config).and_then(|stored| {
                let mut roots = self.config.normalized_indexing_paths();
                roots.extend(
                    stored
                        .normalized_indexing_paths()
                        .into_iter()
                        .filter(|root| !cursor.work().drop_roots.contains(root)),
                );
                let roots: Vec<String> = roots.into_iter().collect();
                IndexingService::stamp_reconciled(conn, &self.config, &roots)
            });
            if let Err(e) = stamped {
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
            .retain(|p, _| self.targeted.upserts.contains_key(p));
    }

    /// Apply as much of the queue as fits in [`APPLY_BUDGET`] (see
    /// [`apply_slice`]).
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

    /// One [`apply_slice`] of a queue within [`APPLY_BUDGET`], in one
    /// transaction. `targeted` picks the queue and the failure policy:
    /// pending failures escalate to [`Inner::needs_full_run`], targeted ones
    /// only log. Measured against a transaction per file (`benches/index.rs`,
    /// `directory_event`, one event over 500 small text files): 250 →
    /// 88 ms with every file changed, 1.55 → 1.09 ms with none.
    fn apply_queue(&mut self, conn: &mut Connection, targeted: bool) {
        // Read before the check: a shutdown that lands after it still kills
        // every sandbox child this turn spawns, and one before it is seen
        // here, since the shutdown cancels before it kills. Single files
        // check nothing between them.
        let kills = KillMark::now();
        if self.reconcile_stop.cancelled() {
            return;
        }
        let deadline = Instant::now() + APPLY_BUDGET;
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                self.needs_full_run |= apply_failed(targeted, "begin", e);
                return;
            }
        };
        let cx = SliceCx {
            config: &self.config,
            ignore: &self.ignore,
            registry: &self.registry,
            cancel: &self.reconcile_stop.cancel,
            kills,
            targeted,
        };
        let queue = if targeted {
            &mut self.targeted
        } else {
            &mut self.pending
        };
        let mut failed = apply_slice(&tx, queue, &mut self.resume_from, &cx, deadline);
        if let Err(e) = tx.commit() {
            failed |= apply_failed(targeted, "commit", e);
        }
        self.needs_full_run |= failed;
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
                    .retain(|p, _| self.pending.upserts.contains_key(p));
                return;
            }
        };
        self.apply_queue(&mut conn, true);
        self.write_conn = Some(conn);
        self.targeted.after_partial_apply();
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

    /// A full run somebody asked for. In manual mode a run under way reads as
    /// `ManualRunning` until it ends; a refused one leaves `ManualStopped`.
    fn start_requested_run(&mut self) {
        self.start_full_run(RunTrigger::Requested);
        if self.mode != IndexMode::Auto && self.run_in_flight {
            self.mode = IndexMode::ManualRunning;
        }
    }

    fn start_full_run(&mut self, trigger: RunTrigger) {
        // Every trigger at once: a request, the periodic timer, a repair, a
        // reconcile's walk. Only a rebuild or a clear lifts the hold.
        if self.held {
            return;
        }
        // A run already in progress is not a refusal: refusing would back off
        // future runs and replace visible progress with an error.
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
        let nested = crate::config::nested_roots(&self.config);
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
        // unreadable directory (`unreadable.covers`), and those rows would
        // leak until a rebuild.
        self.pending.keep_removals();
        // A resume point without its upsert would prune the start of the
        // next walk at that path.
        self.resume_from
            .retain(|p, _| self.targeted.upserts.contains_key(p));
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
        NO_ROOTS.reset();
        NESTED_ROOTS.reset();
        START_FAILURES.reset();
        // Published after this run, it would describe a delete long past.
        self.unreported = None;
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

    /// Hold off further full runs, doubling the wait each time. The backoff,
    /// reset only by a run that ends cleanly, is what keeps the retries from
    /// coming every second. `needs_full_run` is cleared too: it gates
    /// [`Inner::apply_pending`], which must not wait on a run held off.
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
    /// Each start gets a cancel flag of its own, which [`Inner::stop_watcher`]
    /// sets: a superseded registration stops at its next directory, releasing
    /// the watches it took, and its result finds the channel closed. What
    /// changes between one watcher's stop and the next one's registration
    /// reaching that directory is missed: an accepted gap, which the periodic
    /// run closes (an overflow's restart schedules a run of its own).
    fn start_watcher(&mut self) {
        self.stop_watcher();
        let roots = self.config.resolved_indexing_paths();
        if roots.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.watcher_cancel = cancel.clone();
        let sink_tx = self.event_tx.clone();
        let sink = Arc::new(move |ev: FsEvent| {
            let _ = sink_tx.send(ev);
        });
        let config = self.watcher_config.clone();
        // Same filters the indexer walks with; no descriptor is spent on
        // directories whose events would be discarded on arrival.
        let filters = WatchFilters {
            include_hidden: self.config.indexing.include_hidden,
            ignore: self.ignore.clone(),
        };
        let (tx, rx) = mpsc::channel();
        self.watcher_rx = Some(rx);
        self.set_watcher_status(WatcherStatus::Starting);
        #[cfg(test)]
        {
            crate::lock_ok(&self.shared).watcher_starts += 1;
        }
        let spawned = std::thread::Builder::new()
            .name("qs-watcher-start".into())
            .spawn(move || {
                let result = Watcher::start(roots, filters, config, sink, cancel);
                // A failed send means the coordinator moved on; the dropped
                // watcher unregisters itself.
                let _ = tx.send(result);
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
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.watcher_rx = None;
                self.set_watcher_status(WatcherStatus::Off);
                return;
            }
        };
        self.watcher_rx = None;
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

    /// Act on what the running watcher hit after starting, and schedule the
    /// full run that finds out what was missed. An overflow re-registers from
    /// scratch: a lost directory `Create` or `Delete` leaves the watch set
    /// wrong, and only a fresh walk mends it; a limit that still stands then
    /// surfaces from the new registration as Disabled. Within
    /// [`OVERFLOW_REWATCH_GAP`] of the last such registration, an overflow
    /// waits on the watcher, still recorded, for the first tick after the
    /// gap, and is acted on then, run and all: a run any sooner would leave
    /// the watch set as wrong as before. Any other reason tears the watcher
    /// down: a partially watched tree looks live while going silently out of
    /// date.
    fn check_watcher_degraded(&mut self) {
        let Some(w) = &self.watcher else {
            return;
        };
        let Some(mut reason) = w.degraded_reason() else {
            return;
        };
        if matches!(reason, WatchError::Overflowed) {
            if self
                .overflow_rewatched
                .is_some_and(|at| at.elapsed() < OVERFLOW_REWATCH_GAP)
            {
                return;
            }
            crate::log_warn!("watcher: {}", reason);
            self.needs_full_run = true;
            self.overflow_rewatched = Some(Instant::now());
            self.start_watcher();
            return;
        }
        self.needs_full_run = true;
        // The async notify callback can't know the count; fill it in here.
        if let WatchError::KernelLimit { registered } = &mut reason {
            if *registered == 0 {
                *registered = w.watched_dirs();
            }
        }
        self.stop_watcher();
        self.set_watcher_status(WatcherStatus::Disabled { reason });
    }

    fn stop_watcher(&mut self) {
        // A registration still walking stops at its next directory.
        self.watcher_cancel.store(true, Ordering::Relaxed);
        self.watcher_rx = None;
        if let Some(mut w) = self.watcher.take() {
            w.stop();
        }
        self.set_watcher_status(WatcherStatus::Off);
    }

    /// Publish `status`, waking the frontend when it changed: registration
    /// can finish minutes after startup with no input to repaint on, and a
    /// frontend polling for it would repaint the whole time.
    fn set_watcher_status(&self, status: WatcherStatus) {
        let mut shared = crate::lock_ok(&self.shared);
        if shared.watcher != status {
            shared.watcher = status;
            drop(shared);
            (self.notify)();
        }
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

    /// Re-read the published row count once something may have changed it.
    /// Called only from the idle half of [`Inner::tick`], so it cannot run
    /// while a full run holds the database; `COUNT(*)` is still a key scan,
    /// hence the interrupt guard. It used to run every 30 s whatever the
    /// mode, opening a connection each time to recount an unchanged index.
    fn refresh_file_count(&mut self) {
        if !self.files_stale {
            return;
        }
        // Cleared before the read: a failing count must not retry every
        // tick, or a missing index means an open attempt every second.
        self.files_stale = false;

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
    /// open or read is *not* published as `None`: `periodic_due` reads `None`
    /// as "never indexed" and would start a fresh run every tick. A changed
    /// `database_path` is the exception, and is reset first
    /// ([`Inner::forget_published_index`]): the old file's stamp would put the
    /// new one's first run off by a whole interval.
    pub(super) fn refresh_last_full_index(&self) {
        let read = db::open_existing(&self.db_path(), false).and_then(|conn| {
            let last = db::repo::get_last_full_index(&conn)?;
            Ok((last, self.read_root_counts(&conn)))
        });
        match read {
            Ok((last, counts)) => {
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

    /// Publish no completed run and no per-root figures, with `files` as the
    /// row count: what a missing index reads as.
    fn forget_published_index(&self, files: Option<i64>) {
        let mut shared = crate::lock_ok(&self.shared);
        shared.last_full_index = None;
        shared.files = files;
        shared.root_counts = Arc::new(Vec::new());
    }

    /// Stop any run and delete the index, for a user's `what` ("clear the
    /// index"). Only a delete that went through resets the published
    /// figures and drops the reconciliation owed to the old file; one that
    /// failed keeps both, logs, and reports why in the activity the frontend
    /// shows. While a run still holds that activity the report waits for the
    /// tick that sees the run end ([`Inner::unreported`]).
    fn delete_index(&mut self, what: &str) -> Result<(), String> {
        self.write_conn = None;
        let owed = self.pending_work.take();
        self.files_stale = true;
        let deleted = self.indexing.delete_index_for_rebuild(&self.db_path());
        match &deleted {
            Ok(()) => {
                self.reconcile_done = None;
                // Zero, not `None`: no later read corrects a stale figure.
                self.forget_published_index(Some(0));
            }
            Err(e) => {
                self.pending_work = owed;
                crate::log_warn!("coordinator: {}: {}", what, e);
                let message = format!("could not {}: {}", what, e);
                if !self.indexing.report_error(message.clone()) {
                    self.unreported = Some(message);
                }
            }
        }
        deleted
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

    /// Kept short: [`IndexCoordinator::shutdown`] waits only
    /// [`crate::shutdown::EXIT_GRACE`] for this thread before leaving it
    /// running, and a closing window waits on that. Signal, don't wait: an
    /// abandoned run is safe under WAL, and an abandoned reconcile is redone by
    /// the next run.
    fn teardown(mut self) {
        self.stop_watcher();
        // Read once: `idle` gates two decisions that must agree.
        let idle = !self.indexing.is_active();
        if !idle {
            self.indexing.request_stop();
            // The service's drop polls up to 5 s for the run to end
            // (`stop_indexing`), and a VACUUM answers only to
            // `sqlite3_interrupt`; the interrupted VACUUM rolls back.
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
