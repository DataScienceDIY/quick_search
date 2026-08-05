//! The indexing coordinator: the one object binaries construct.
//!
//! Owns the [`IndexingService`] (full runs), the filesystem [`Watcher`]
//! (change events), a periodic-reindex scheduler, and the mode state
//! machine:
//!
//! - **Auto** — watcher running; events apply incrementally between full
//!   runs; a full reindex triggers whenever `last_full_index` is older
//!   than the configured interval (or has never happened).
//! - **ManualStopped** — watcher off, pending events dropped, nothing runs
//!   until the user acts.
//! - **ManualRunning** — one user-forced full run; returns to
//!   `ManualStopped` when it finishes. (A forced run in Auto stays Auto.)
//!
//! `indexing.auto_index` is the persisted form of that mode: it picks the
//! starting mode here, and every mode change keeps the coordinator's copy
//! of the config in step with it. Writing the file back is the caller's
//! job — the coordinator's config is a copy, not the source of truth.
//!
//! Single-writer guarantee: incremental writes are deferred while a full
//! run is active — the coordinator's tick simply does nothing until the
//! `IndexingService` reports idle, then drains its queue. Overflowing the
//! queue (>100k pending paths) collapses into one full run instead.
//!
//! Settling is part of the state machine, not an implementation detail. The
//! first tick that finds nothing to do calls [`Inner::go_idle`], which drops
//! the write connection — reopening it is cheap, and holding it means holding
//! whatever page cache the last reconciliation filled for the life of the
//! process — and then returns the heap that this coordinator, the search
//! worker and the last indexing run have all freed into their allocator
//! arenas. It fires once per busy→idle transition, so a coordinator that has
//! nothing to do costs nothing to have around. This is also where the
//! published file count comes from: the status bar's figure is read off the
//! connection this thread already holds, on an interval, rather than by a
//! frontend opening its own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::{diff_actions, Config, IgnoreSet, IndexWork};
use crate::db;
use crate::extract::Registry;
use crate::incremental::apply_fs_event;
use crate::indexing::{ConfigChange, IndexingService, IndexingStatus, PrepStep, ReconcileProgress};
use crate::scope::WorkCursor;
use crate::watcher::{FsEvent, WatchError, WatchFilters, Watcher, WatcherConfig};

/// Pending-event ceiling; beyond this a full run is cheaper than replay.
const PENDING_OVERFLOW: usize = 100_000;

/// How often the published file count is re-read while idle.
///
/// It moves only when something writes to the index, and everything that does
/// so — a run finishing, a batch of watcher events — either invalidates it
/// directly or is followed by another tick within this window. Long enough
/// that a `COUNT(*)` over a multi-million-row index is not a recurring cost,
/// short enough that the status bar is not visibly wrong.
const FILE_COUNT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    Auto,
    ManualStopped,
    ManualRunning,
}

/// Whether live updates are running, and if not, why.
///
/// A failed watcher used to be a printed line the GUI never saw, so roots
/// silently fell back to the periodic reindex with no user-visible sign.
/// This is the state the UI acts on; the log line is the detail behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherStatus {
    /// Not running: manual mode, or no roots configured.
    Off,
    /// Registration in flight — it walks every root, so this can last
    /// minutes on large or networked trees.
    Starting,
    /// Live updates active over `dirs` watched directories.
    Active { dirs: usize },
    /// Live updates unavailable; the periodic reindex is the only refresh.
    Disabled { reason: WatchError },
}

/// A config reconciliation the coordinator applies between runs.
///
/// Separate from [`IndexingStatus`] on purpose. That enum is what every caller
/// reads to decide whether a full run owns the database, and this work is the
/// coordinator's own — putting it there would make the tick that performs it
/// believe it must keep off the file. On a large index it is minutes of
/// scanning that used to report nothing but `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileState {
    /// Scanning now; the counters move every slice.
    Running(ReconcileProgress),
    /// Finished within the last [`RECONCILE_SUMMARY_LINGER`].
    ///
    /// The tail is the whole point: narrowing a filter on a small index is
    /// over in a millisecond, and a display that only existed while the work
    /// did would leave the user with no sign it ever happened.
    Finished(ReconcileProgress),
}

/// How long a finished reconciliation keeps reporting itself.
pub const RECONCILE_SUMMARY_LINGER: Duration = Duration::from_secs(10);

/// One-stop poll surface for the GUI.
#[derive(Debug, Clone)]
pub struct IndexerState {
    pub mode: IndexMode,
    pub activity: IndexingStatus,
    /// Unix seconds of the last completed full run, if any.
    pub last_full_index: Option<u64>,
    /// Rows in the index, refreshed while idle. `None` before the first read.
    ///
    /// Deliberately approximate: it is a status-bar figure, not a fact
    /// anything decides on, and the alternative to letting it lag is running a
    /// `COUNT(*)` on a cadence rather than when the index is quiet.
    pub files: Option<i64>,
    /// Watcher events waiting to be applied.
    pub queued_events: usize,
    /// Live-update health; see [`WatcherStatus`].
    pub watcher: WatcherStatus,
    /// The between-runs reconciliation, while it runs and briefly after.
    ///
    /// A run's *own* reconciliation is not here — it is a step of the run, and
    /// reads as [`IndexingStatus::Preparing`] with a
    /// [`PrepStep::Reconciling`]. Both report the same
    /// [`ReconcileProgress`], so the display does not have to care which one
    /// it is looking at.
    pub reconcile: Option<ReconcileState>,
}

// `ConfigChanged(Config)` dwarfs the unit variants, but these are sent one
// at a time down an mpsc channel at human cadence — a mode flip, a settings
// save. Boxing would trade a rare oversized move for an allocation on a path
// whose whole job is to be simple.
#[allow(clippy::large_enum_variant)]
enum CoordCmd {
    SetMode(IndexMode),
    ReindexNow,
    ConfigChanged(Config),
    RebuildIndex,
    ClearIndex,
    Shutdown,
}

pub struct IndexCoordinator {
    cmd_tx: mpsc::Sender<CoordCmd>,
    indexing: Arc<IndexingService>,
    shared: Arc<Mutex<Shared>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    /// How [`Self::shutdown`] reaches a reconciliation in progress; see
    /// [`Inner::apply_work`].
    reconcile_stop: Arc<ReconcileStop>,
}

/// The two halves of cutting the coordinator's reconciliation short — the pair
/// [`db::InterruptSlot`] describes.
///
/// A command cannot do it: the thread that would read the command is the
/// thread inside the scan. Closing the window used to wait for both.
#[derive(Default)]
struct ReconcileStop {
    cancel: AtomicBool,
    interrupt: db::InterruptSlot,
}

impl ReconcileStop {
    /// Flag first, then interrupt: the flag is what stops the *next*
    /// statement, and setting it after the interrupt would leave a window in
    /// which the pass starts one more.
    fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        db::interrupt(&self.interrupt);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// State mirrored out of the coordinator thread for `state()`.
struct Shared {
    mode: IndexMode,
    last_full_index: Option<u64>,
    /// Rows in `files`, for the idle status bar's "N files indexed".
    ///
    /// Published from here rather than fetched by the frontend because the
    /// coordinator is already the thing that holds a connection and already
    /// knows when the index is quiet enough to ask. The GUI used to spawn a
    /// thread and open its own connection for this every five seconds, which
    /// cost a page cache and an arena per refresh for a decorative number.
    /// `None` until the first successful read.
    files: Option<i64>,
    queued_events: usize,
    watcher: WatcherStatus,
    reconcile: Option<ReconcileState>,
}

impl IndexCoordinator {
    pub fn start(config: Config) -> Result<IndexCoordinator, String> {
        Self::start_with_watcher_config(config, WatcherConfig::default())
    }

    /// [`Self::start`] with explicit watcher debounce tuning (tests use
    /// short windows; the default 30 s throttle is right for real use).
    pub fn start_with_watcher_config(
        config: Config,
        watcher_config: WatcherConfig,
    ) -> Result<IndexCoordinator, String> {
        let indexing = Arc::new(IndexingService::new());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        let initial_mode = if config.indexing.auto_index {
            IndexMode::Auto
        } else {
            IndexMode::ManualStopped
        };
        let shared = Arc::new(Mutex::new(Shared {
            mode: initial_mode,
            last_full_index: None,
            files: None,
            queued_events: 0,
            watcher: WatcherStatus::Off,
            reconcile: None,
        }));

        let reconcile_stop = Arc::new(ReconcileStop::default());
        let mut inner = Inner {
            config,
            indexing: indexing.clone(),
            shared: shared.clone(),
            reconcile_stop: reconcile_stop.clone(),
            event_tx,
            event_rx,
            watcher: None,
            watcher_config,
            watcher_rx: None,
            watcher_gen: 0,
            pending: HashMap::new(),
            last_event_at: None,
            pending_since: None,
            needs_full_run: false,
            pending_work: None,
            reconcile_done: None,
            reconcile_cut_short: false,
            saw_running: false,
            files_at: None,
            was_busy: false,
            write_conn: None,
            ignore: Arc::new(IgnoreSet::compile(&[]).expect("empty ignore set")),
            registry: Registry::default_set(),
            mode: initial_mode,
        };
        inner.reload_filters()?;
        inner.refresh_last_full_index();

        let handle = std::thread::Builder::new()
            .name("qs-coordinator".into())
            .spawn(move || inner.run(cmd_rx))
            .map_err(|e| format!("spawn coordinator: {}", e))?;

        Ok(IndexCoordinator {
            cmd_tx,
            indexing,
            shared,
            handle: Mutex::new(Some(handle)),
            stopped: AtomicBool::new(false),
            reconcile_stop,
        })
    }

    pub fn state(&self) -> IndexerState {
        let shared = self.shared.lock().unwrap();
        IndexerState {
            mode: shared.mode,
            activity: self.indexing.get_status(),
            last_full_index: shared.last_full_index,
            files: shared.files,
            queued_events: shared.queued_events,
            watcher: shared.watcher.clone(),
            reconcile: shared.reconcile,
        }
    }

    pub fn set_mode(&self, mode: IndexMode) {
        let _ = self.cmd_tx.send(CoordCmd::SetMode(mode));
    }

    /// Force a full reindex now. In manual mode the coordinator enters
    /// `ManualRunning` and returns to `ManualStopped` when done.
    pub fn reindex_now(&self) {
        let _ = self.cmd_tx.send(CoordCmd::ReindexNow);
    }

    /// Hand the coordinator an edited config. Watcher and paths follow on
    /// the next tick; rebuild decisions stay with the caller (see
    /// [`crate::config::diff_actions`] and [`Self::rebuild_index`]).
    pub fn apply_config(&self, config: Config) {
        let _ = self.cmd_tx.send(CoordCmd::ConfigChanged(config));
    }

    /// Delete the index and rebuild from scratch (user confirmed).
    pub fn rebuild_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::RebuildIndex);
    }

    /// Delete the index WITHOUT rebuilding (user confirmed). Indexing
    /// drops to manual-stopped so automatic mode doesn't immediately
    /// resurrect what the user just deleted.
    pub fn clear_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::ClearIndex);
    }

    /// Compare `config` against what the index was built with. Read-only.
    pub fn check_config_validation(
        &self,
        config: &Config,
    ) -> Result<Option<Vec<ConfigChange>>, String> {
        let db = config.resolved_database_path();
        let roots: Vec<String> = config.normalized_indexing_paths().into_iter().collect();
        self.indexing
            .check_config_validation(&db.to_string_lossy(), config, &roots)
    }

    /// Stop the watcher, any running index pass, and the coordinator
    /// thread. Idempotent; usable from a signal handler through an Arc.
    ///
    /// Cancelling the reconciliation comes *before* the command, because the
    /// command is read by the thread the scan is running on: a pass part-way
    /// through a large index would otherwise hold this join — and with it the
    /// window close that called it — for as long as the scan had left.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_stop.stop();
        let _ = self.cmd_tx.send(CoordCmd::Shutdown);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    /// Whether a configuration change is being applied to the index right
    /// now, by either of the two places that can be doing it.
    ///
    /// For a caller deciding whether to warn before quitting: an abandoned
    /// pass leaves entries the user excluded still in the index until the next
    /// indexing run redoes it.
    pub fn reconciling(&self) -> bool {
        // The lock is released before the service is asked, so this never
        // holds two of them at once.
        let between_runs = self.shared.lock().unwrap().reconcile;
        matches!(between_runs, Some(ReconcileState::Running(_)))
            || matches!(
                self.indexing.get_status(),
                IndexingStatus::Preparing {
                    step: PrepStep::Reconciling(_),
                    ..
                }
            )
    }
}

impl Drop for IndexCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Fold `event` into the last-event-wins pending map. Renames split into
/// their halves so downstream application never needs pair handling.
fn enqueue(pending: &mut HashMap<PathBuf, FsEvent>, event: FsEvent) {
    match event {
        FsEvent::Rename { from, to } => {
            pending.insert(from.clone(), FsEvent::Remove(from));
            pending.insert(to.clone(), FsEvent::Create(to));
        }
        FsEvent::Create(ref p) | FsEvent::Modify(ref p) | FsEvent::Remove(ref p) => {
            let key = p.clone();
            pending.insert(key, event);
        }
    }
}

/// Whether a queued event is a removal. The queue only ever holds
/// Create/Modify/Remove — [`enqueue`] splits renames into their halves.
fn is_removal(event: &FsEvent) -> bool {
    matches!(event, FsEvent::Remove(_))
}

/// Drop queued removals that a queued removal of one of their ancestors
/// already covers, in place.
///
/// `rm -rf dir/` reports `dir` and every path beneath it; applying `dir` sweeps
/// the whole range, so the descendants are duplicate work. Collapsing here —
/// rather than at application time — is what keeps a mass deletion from
/// tripping [`PENDING_OVERFLOW`] and forcing a redundant full run.
///
/// Only removals collapse against removals. A `Create` under a removed
/// directory is a re-creation and must survive: removals are applied first, so
/// it lands afterwards and the row is correct either way.
fn collapse_pending_removals(pending: &mut HashMap<PathBuf, FsEvent>) {
    if pending.values().filter(|e| is_removal(e)).take(2).count() < 2 {
        return;
    }
    let removed: std::collections::HashSet<PathBuf> = pending
        .iter()
        .filter(|(_, ev)| is_removal(ev))
        .map(|(p, _)| p.clone())
        .collect();
    // Component-wise containment, per `UnreadableDirs::covers`.
    pending.retain(|path, ev| {
        !is_removal(ev) || !path.ancestors().skip(1).any(|a| removed.contains(a))
    });
}

/// How long [`Inner::apply_pending`] may hold the command loop before handing
/// the rest of the queue to the next tick.
///
/// A long run over a busy tree can accumulate tens of thousands of events, and
/// draining them all inline would block Stop and Apply behind the backlog. A
/// deadline bounds that latency without throttling throughput the way a fixed
/// event budget would; `queued_events` already reports the remainder, so a
/// multi-tick drain is visible rather than silent.
const APPLY_BUDGET: Duration = Duration::from_millis(250);

struct Inner {
    config: Config,
    indexing: Arc<IndexingService>,
    shared: Arc<Mutex<Shared>>,
    /// Read inside the reconciliation, set from the thread that shuts this
    /// one down; see [`ReconcileStop`].
    reconcile_stop: Arc<ReconcileStop>,
    event_tx: mpsc::Sender<FsEvent>,
    event_rx: mpsc::Receiver<FsEvent>,
    watcher: Option<Watcher>,
    watcher_config: WatcherConfig,
    /// In-flight async watcher registration (see [`Inner::start_watcher`]).
    watcher_rx: Option<mpsc::Receiver<(u64, Result<Watcher, WatchError>)>>,
    watcher_gen: u64,
    pending: HashMap<PathBuf, FsEvent>,
    /// When the most recent event arrived; the burst is over once this is
    /// `pending_settle` old.
    last_event_at: Option<Instant>,
    /// When the oldest un-applied event arrived, so a steady trickle cannot
    /// defer application past `pending_max_defer`.
    pending_since: Option<Instant>,
    needs_full_run: bool,
    /// Reconciliation owed to a config change, part-applied across ticks.
    ///
    /// Deliberately not folded into `needs_full_run`: that flag also carries
    /// watcher overflow and incremental failure, which must stay dormant in
    /// manual mode, whereas a config change the user just made is acted on in
    /// either mode.
    pending_work: Option<WorkCursor>,
    /// The last reconciliation to finish, and when. Published until it is
    /// [`RECONCILE_SUMMARY_LINGER`] old; see [`ReconcileState::Finished`].
    reconcile_done: Option<(ReconcileProgress, Instant)>,
    /// A reconciliation was abandoned part-way; read by [`Inner::teardown`].
    reconcile_cut_short: bool,
    /// A start was requested; set false once the service reports running,
    /// so idle-after-running transitions are detectable.
    saw_running: bool,
    /// When the published file count was last read, so it can be refreshed on
    /// an interval rather than every tick. `None` forces the next tick to
    /// re-read it.
    files_at: Option<Instant>,
    /// Something has happened since the last time this coordinator settled.
    ///
    /// Drives [`Inner::go_idle`], which must fire once per busy→idle
    /// transition rather than once per tick: releasing the page cache and
    /// trimming the heap are both worth doing when the work stops and both are
    /// pure overhead every second thereafter.
    was_busy: bool,
    write_conn: Option<Connection>,
    /// Shared with the watcher, which filters registrations by the same set.
    ignore: Arc<IgnoreSet>,
    registry: Registry,
    mode: IndexMode,
}

impl Inner {
    fn run(mut self, cmd_rx: mpsc::Receiver<CoordCmd>) {
        if self.mode == IndexMode::Auto {
            self.enter_auto();
        }
        loop {
            // Reconciliation is applied one slice per tick, so while any is
            // owed the idle wait has to shrink or the slices are a second
            // apart and a large index's prune stretches over minutes of wall
            // clock. Coming straight back keeps the duty cycle high while
            // still servicing every queued command between slices.
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
                // A wipe stays the caller's decision — it is destructive and
                // the GUI may have to ask first (see `rebuild_index`).
                // Everything short of one this thread reconciles itself, in
                // both modes and without asking: deleting rows the user just
                // put out of scope is not a change to confirm, it is the
                // change they made.
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
            }
            CoordCmd::RebuildIndex => {
                let db = self.db_path();
                self.write_conn = None;
                // The file is about to be deleted, so the published count is
                // about to be wrong by all of it. Re-read on the next tick
                // rather than letting the interval carry a stale figure across
                // a wipe the user just asked for.
                self.files_at = None;
                // Nothing to reconcile against once the file is gone — and
                // nothing to report about what was reconciled in the index
                // that is about to stop existing.
                self.pending_work = None;
                self.reconcile_done = None;
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    crate::log_warn!("coordinator: rebuild: {}", e);
                }
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
                let mut shared = self.shared.lock().unwrap();
                shared.last_full_index = None;
                // Zero, not `None`: nothing is going to rebuild this index, so
                // there is no later read to correct a stale figure, and "0
                // files indexed" is the truth about what was just deleted.
                shared.files = Some(0);
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
            // Eagerly, not on the usual interval: the number the run just
            // changed is the one the user is looking at when it finishes.
            self.files_at = None;
            if self.mode == IndexMode::ManualRunning {
                self.mode = IndexMode::ManualStopped;
            }
        }

        self.refresh_file_count();

        // Ahead of the mode gate: a config edit is reconciled in manual mode
        // too. It may end by starting a run, which is why this cannot wait for
        // the Auto-only scheduling below.
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

        // Only when this tick found nothing to do. A tick that applied a batch
        // is very likely to be followed by another that does the same, and
        // releasing the connection between them would reopen it a moment later
        // with a cold cache.
        if !worked {
            self.go_idle();
        }
    }

    /// Settle: hand back what the work needed and the process no longer does.
    ///
    /// Two things, and they have to happen in this order. Dropping
    /// [`Inner::write_conn`] closes a connection whose page cache a
    /// reconciliation pass can have filled to its ceiling, and which is
    /// otherwise held for the life of the process —
    /// [`Inner::ensure_write_conn`] reopens it lazily, and cheaply, because
    /// the key is applied in raw form and never re-derived. Then
    /// [`crate::platform::release_free_heap`] returns those pages, and
    /// everything the last indexing run and the search worker freed into
    /// their arenas, to the kernel.
    ///
    /// Gated on [`Inner::was_busy`] so it runs once when the work stops rather
    /// than every tick forever after.
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
            // Before the overflow test, not after: an `rm -rf` of half a
            // million files collapses to a handful of directory roots, and
            // measuring the queue by its raw event count would throw all of
            // them away and schedule a full run instead.
            collapse_pending_removals(&mut self.pending);
        }
        if self.pending.len() > PENDING_OVERFLOW {
            // Replaying a storm one file at a time is slower than one
            // incremental full run (unchanged files skip on mtime).
            self.clear_pending();
            self.needs_full_run = true;
        }
    }

    /// Queue reconciliation for a config change.
    ///
    /// A plan still in flight is folded in and restarted rather than dropped:
    /// it was computed against the previous configuration, so the new one —
    /// which diffs against that same previous config — cannot know what it
    /// had left undone. Restarting re-does the finished half, which every
    /// part of the pass is idempotent precisely so that it can.
    fn start_work(&mut self, mut work: IndexWork) {
        if let Some(outstanding) = self.pending_work.take() {
            work.merge_from(outstanding.work());
        }
        match WorkCursor::new(work, &self.config) {
            Ok(cursor) => self.pending_work = Some(cursor),
            // Only an uncompilable ignore pattern gets here, and the GUI
            // validates those before saving. Refusing to reconcile is the
            // safe half: nothing is deleted on a filter nobody could build.
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
                // No index to reconcile: a run builds it under the new config
                // anyway, which reaches the same place by a longer road.
                crate::log_warn!("coordinator: reconcile unavailable ({}); scheduling run", e);
                self.pending_work = None;
                self.needs_full_run = true;
                return;
            }
        };
        let mut cursor = self.pending_work.take().expect("caller checked");
        let outcome = {
            // Held only for the slice: the handle names whatever statement
            // this connection is running, and outside `advance` that is
            // nothing this cancellation has any business ending.
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
            // A cancelled statement fails like any other, and telling the two
            // apart from a stringified error is guesswork — so ask the flag we
            // set ourselves. Shutting down mid-scan is not a fault to report.
            if self.reconcile_stop.cancelled() {
                self.reconcile_cut_short = true;
                crate::log_info!(
                    "configuration change interrupted after {} index entries; \
                     the next indexing run starts it again",
                    cursor.progress().examined
                );
                return;
            }
            // Abandoned rather than retried: the cursor is already dropped,
            // and a database error that persists would otherwise spin this
            // loop for the life of the process. The next full run reconciles
            // from the stored fingerprint, which is the backstop for exactly
            // this.
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
        // Held for the linger so the display outlives the work: on a small
        // index this whole pass is over between two frames.
        self.reconcile_done = Some((cursor.progress(), Instant::now()));
        // Record what the pass just brought the index into line with. Only
        // here, on the path where the work finished and nothing errored: the
        // stale record is what makes the next full run redo an abandoned
        // reconcile, and it is the documented backstop for the error path
        // above. Without this stamp a completed prune left the index still
        // describing itself with the old configuration, so every later run
        // rescanned every row to re-apply work already done — the silent wait
        // a large index spends before its walk starts.
        if let Some(conn) = self.write_conn.as_ref() {
            // The same spelling a run records: canonicalized, and sorted into
            // one string by `config_validation_entries`.
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
        // Widening the configuration adds files that exist only on disk, so
        // only a walk can produce their rows. `ReindexNow`'s exact behaviour,
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
        // `clear` empties the map but keeps the table it grew into, and this
        // one grows to [`PENDING_OVERFLOW`] — an `rm -rf` of a large watched
        // tree leaves a 100k-slot allocation behind for the life of the
        // process. Releasing it is the point of clearing here at all.
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
    /// Removals lead deliberately. The queue is an unordered map, so the old
    /// arbitrary application order could delete a row a `Create` in the same
    /// batch had just written. With removals first both interleavings converge
    /// on the truth: a delete-then-recreate ends with the file present, and a
    /// create-then-delete ends with it absent, because the upsert half consults
    /// the filesystem and finds nothing there.
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
                // The batch leaves `pending` either way — replaying a write
                // that just failed, once a tick forever, is the worse
                // failure. A full run is what recovers the rows instead.
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
                    // Same reasoning as the removal half above: the event is
                    // already out of `pending`, so a full run is the only
                    // thing that still picks the file up.
                    crate::log_warn!("coordinator: apply {:?}: {}; scheduling full run", ev, e);
                    self.needs_full_run = true;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }

        self.write_conn = Some(conn);
        // Whatever is left goes to the next tick, and goes immediately: the
        // pause was ours, not the filesystem's, so it must not re-arm the
        // settle window.
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
        // profile, and this connection outlives every run. See
        // [`db::schema::PRAGMAS_INCREMENTAL`]. Reopening is cheap — the key is
        // applied in raw form and never re-derived — which is what makes
        // dropping it in `go_idle` reasonable.
        db::open::open_incremental_writer(&self.db_path())
    }

    fn periodic_due(&self) -> bool {
        let interval_secs = self
            .config
            .indexing
            .reindex_interval_minutes
            .saturating_mul(60);
        let last = self.shared.lock().unwrap().last_full_index;
        match last {
            None => true,
            Some(last) => now_unix().saturating_sub(last) >= interval_secs,
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
        // Backstop for hand-edited configs; the GUI rejects nested roots at
        // add/apply/startup with proper messaging.
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
        // Queued creates and modifies are dropped — the walk about to start
        // rediscovers every one of them. Removals are kept, because it cannot:
        // a deletion under a directory the walk fails to read is deliberately
        // left alone by `unreadable.covers`, and one of a symlink target whose
        // real parent lies outside every root is exempted by `aliased_paths`.
        // Those rows would otherwise leak until a rebuild. Re-applying a
        // removal the walk did happen to catch is a harmless no-op.
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
        // `start_indexing` claims the Running status before it returns, so
        // `get_status()` is already authoritative — no poll, and no window in
        // which this thread could believe the service idle and start writing
        // to a database the run is about to reopen.
        self.saw_running = true;
    }

    fn enter_auto(&mut self) {
        self.mode = IndexMode::Auto;
        // Keep the config copy honest: `auto_index` is this mode written
        // down, and the caller persists it from its own copy.
        self.config.indexing.auto_index = true;
        self.start_watcher();
        if self.shared.lock().unwrap().last_full_index.is_none() {
            self.needs_full_run = true;
        }
    }

    fn enter_manual_stopped(&mut self) {
        self.mode = IndexMode::ManualStopped;
        self.config.indexing.auto_index = false;
        self.stop_watcher();
        self.clear_pending();
        // Stopping means "no runs now", so a config change that also widened
        // the scope loses its walk — but keeps its pruning. Deleting rows the
        // user put out of scope is the edit they made, not indexing work.
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

    /// Begin watcher startup WITHOUT blocking the command loop.
    /// Registering inotify watches walks every indexable directory of
    /// every root — minutes on large or networked trees — and it used to
    /// run inline here, wedging every queued command (Start/Stop/Apply)
    /// behind it. The finished watcher is handed back through a channel
    /// polled each loop turn; a generation counter discards superseded
    /// registrations.
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
        // Same filters the indexer walks with, so no descriptor is spent on
        // a directory whose events would be discarded on arrival.
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
                        // Not just a log line any more: the GUI needs this to
                        // tell the user live updates are off and only the
                        // periodic reindex is refreshing the index.
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
    /// and schedule a full run so nothing missed while it degraded is left
    /// stale. A partially watched tree looks live while going silently out
    /// of date, so we prefer none at all plus periodic rescans.
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
        self.shared.lock().unwrap().watcher = status;
    }

    fn db_path(&self) -> String {
        self.config
            .resolved_database_path()
            .to_string_lossy()
            .into_owned()
    }

    fn reload_filters(&mut self) -> Result<(), String> {
        self.ignore = Arc::new(
            IgnoreSet::compile(&self.config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?,
        );
        Ok(())
    }

    /// Re-read the stamp the last completed full run left behind.
    ///
    /// A failure to open is deliberately *not* published as `None`. Only a
    /// successful read means "never indexed", and `periodic_due` answers that
    /// by starting a full run immediately — so treating a locked or
    /// contended database as "never" would schedule a fresh run every tick
    /// for as long as the condition lasts. Keeping the previous value leaves
    /// the schedule where it was until a read succeeds.
    /// Re-read the published row count, at most every [`FILE_COUNT_INTERVAL`].
    ///
    /// Called only from the idle half of [`Inner::tick`], so it cannot run
    /// while a full run holds the database. `COUNT(*)` is answered from the
    /// narrowest index rather than the table (see [`db::repo::row_count`]),
    /// but it is still a key scan of every row, so it runs behind an interval
    /// and behind the interrupt guard that lets shutdown cut it short rather
    /// than waiting out a scan of a several-million-row index.
    fn refresh_file_count(&mut self) {
        if let Some(at) = self.files_at {
            if at.elapsed() < FILE_COUNT_INTERVAL {
                return;
            }
        }
        // Stamped before the read, not after: a count that keeps failing must
        // back off exactly as a successful one does, or a missing index turns
        // into an open attempt every tick.
        self.files_at = Some(Instant::now());

        let Ok(conn) = db::open_existing(&self.db_path(), false) else {
            // No readable index yet. The status bar says "0 files indexed",
            // which is both true and what the empty case should look like.
            return;
        };
        // The same slot `apply_work` arms: it is how the thread tearing this
        // one down cuts short whatever statement the coordinator is inside,
        // which a command cannot do because this thread is the one that would
        // read the command.
        let _guard = db::InterruptGuard::arm(&self.reconcile_stop.interrupt, &conn);
        match db::repo::row_count(&conn) {
            Ok(n) => self.shared.lock().unwrap().files = Some(n as i64),
            // Interrupted by a shutdown, or a torn index a run will rebuild.
            // Either way the last figure is better than none.
            Err(e) => crate::log_warn!("coordinator: file count unavailable: {}", e),
        }
    }

    fn refresh_last_full_index(&self) {
        match db::open_existing(&self.db_path(), false) {
            Ok(conn) => {
                let last = db::repo::get_last_full_index(&conn);
                self.shared.lock().unwrap().last_full_index = last;
            }
            Err(e) => crate::log_warn!("coordinator: last-full-index unreadable: {}", e),
        }
    }

    fn publish(&mut self) {
        let reconcile = match &self.pending_work {
            Some(cursor) => Some(ReconcileState::Running(cursor.progress())),
            None => {
                // The tail ages out here rather than in `tick`, which returns
                // early for the whole length of a run; this runs every turn of
                // the loop, so it expires within a second of its deadline
                // whatever else is going on.
                let now = Instant::now();
                self.reconcile_done = self
                    .reconcile_done
                    .filter(|(_, at)| summary_is_fresh(*at, now));
                self.reconcile_done
                    .map(|(progress, _)| ReconcileState::Finished(progress))
            }
        };
        let mut shared = self.shared.lock().unwrap();
        shared.mode = self.mode;
        shared.queued_events = self.pending.len();
        shared.reconcile = reconcile;
    }

    /// Must stay fast: it runs (transitively) on the GUI thread during
    /// window close, and desktops show a "terminate this application?"
    /// dialog after a few unresponsive seconds. Signal, don't wait — an
    /// abandoned run is safe under WAL, and so is an abandoned reconcile:
    /// nothing recorded it, so the next run does it again.
    fn teardown(mut self) {
        self.stop_watcher();
        let status = self.indexing.get_status();
        let idle = matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_));
        if !idle {
            self.indexing.request_stop();
            // Dropping the service joins its worker, and a VACUUM answers to
            // nothing but `sqlite3_interrupt` — without this, closing the
            // window during an optimize pass would wait out a rewrite of the
            // whole index. The interrupted VACUUM rolls back, and the next
            // run's checkpoints land the log.
            self.indexing.cancel_db_work();
        }
        // Unfinished either way it can end: still holding its cursor, or
        // abandoned mid-statement by the cancellation.
        let cut_short = self.reconcile_cut_short || self.pending_work.is_some();
        if let Some(conn) = self.write_conn.take() {
            // A reconciliation cut short is the one idle case that must not
            // checkpoint. It can have written a great deal of WAL — deleting
            // a root's rows is all log — and a TRUNCATE checkpoint of it is
            // more of exactly the wait the cancellation just spared the user.
            // Dropping is safe: WAL keeps the log and the next run lands it.
            if idle && !cut_short {
                db::repo::checkpoint_and_close(conn);
            }
            // Otherwise just drop: a TRUNCATE checkpoint would block
            // behind the running writer.
        }
    }
}

/// Whether a reconciliation that finished at `at` is still worth reporting.
///
/// Its own function so the rule can be tested without waiting the linger out.
fn summary_is_fresh(at: Instant, now: Instant) -> bool {
    now.duration_since(at) < RECONCILE_SUMMARY_LINGER
}

use crate::log::now_unix;

#[cfg(test)]
mod tests {
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

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
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

    /// Closing the window during a prune must not wait the prune out. The
    /// thread that reads the shutdown command is the thread inside the scan,
    /// so before this the join sat behind however many rows were left — on a
    /// large index, minutes of it, with the window still on screen and the
    /// desktop offering to kill the app.
    #[test]
    fn shutdown_during_a_prune_does_not_wait_for_it() {
        let f = Fixture::new(false);
        // Enough rows that the scan cannot plausibly finish between the
        // config landing and the shutdown two lines later.
        for i in 0..400 {
            std::fs::write(f.dir.join(format!("f{}.log", i)), "dropped content").unwrap();
        }
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.reindex_now();
        wait_for("initial run", Duration::from_secs(60), || {
            coord.state().last_full_index.is_some() && f.file_count() == 401
        });

        let mut narrowed = f.config.clone();
        narrowed.indexing.ignore_patterns.push("*.log".into());
        // One row per page, so the scan is as many statements as there are
        // rows: the shape a huge index has, at a size a test can afford.
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
        // And it really did leave the work unfinished rather than racing
        // through it: the stored record still describes the old settings, so
        // the next run derives the same plan and applies it.
        assert!(
            f.outstanding_work(&narrowed).touches_index(),
            "the prune ran to completion, so this proves nothing about waiting"
        );
    }

    /// A narrowed filter is applied to the stored index without a prompt and
    /// without a run — including in manual mode, where the user has said not
    /// to index anything. Deleting entries they just excluded is not indexing
    /// work; it is the edit they made.
    #[test]
    fn manual_mode_prunes_a_narrowed_filter_without_running() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.reindex_now();
        wait_for("initial run", Duration::from_secs(20), || {
            let s = coord.state();
            s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped && f.file_count() == 2
        });
        let stamped = coord.state().last_full_index;

        // Appended, not replaced: dropping the default patterns at the same
        // time would be a widening too, and this is about narrowing alone.
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
    /// later run re-derives the same plan and rescans every row under every
    /// root to redo work already done. On a multi-million-file index that
    /// rescan is minutes of silence before the walk starts — the whole reason
    /// indexing looked hung after a prune.
    #[test]
    fn a_completed_prune_records_what_it_reconciled() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
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

    /// A prune of a two-file index is one transaction, over well inside a
    /// frame. Reporting it only while it runs would mean the user changes a
    /// setting and sees nothing at all — so the result outlives the work.
    #[test]
    fn a_finished_prune_keeps_reporting_itself_for_a_while() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
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

    /// And it does go away: a summary is a report of what just happened, not
    /// a state the app sits in. The rule is tested directly rather than by
    /// sleeping out the linger.
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

    /// The three settings no scan can satisfy stay at the values the index was
    /// *built* with. A prune that stamped them would clear a rebuild the user
    /// was prompted for and declined — and would clear it from an unrelated
    /// later edit at that.
    #[test]
    fn a_prune_never_records_settings_only_a_rebuild_can_satisfy() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.reindex_now();
        wait_for("initial run", Duration::from_secs(20), || {
            let s = coord.state();
            s.last_full_index.is_some() && f.file_count() == 2
        });
        let built_with = f.stored_value("hash_length").unwrap();

        // The user changes the hash length and declines the rebuild it needs.
        // The coordinator takes the config either way — a wipe is the caller's
        // decision — so from here its copy disagrees with the stored hashes,
        // and nothing short of a rebuild can make them agree.
        let mut rebuilt = f.config.clone();
        rebuilt.processing.hash_length = f.config.processing.hash_length * 2;
        coord.apply_config(rebuilt.clone());
        std::thread::sleep(Duration::from_millis(500));

        // Then they edit a filter. This one *is* reconcilable, and the pass
        // that applies it stamps — with the coordinator's config, whose hash
        // length is the one the index does not have.
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

    /// Widening it does the opposite: nothing is deleted, and the walk that
    /// finds the newly-eligible files starts on its own, returning manual mode
    /// to stopped afterwards the way `reindex_now` does.
    #[test]
    fn manual_mode_reindexes_a_widened_filter_and_returns_to_stopped() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("later.log"), "arrives later").unwrap();

        let mut narrowed = f.config.clone();
        narrowed.indexing.ignore_patterns.push("*.log".into());
        let coord = IndexCoordinator::start(narrowed.clone()).unwrap();
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

    /// Stopping is the user saying "no runs now", so a widening edit already
    /// in flight loses its walk — but keeps the pruning half, which is not
    /// indexing work.
    #[test]
    fn stopping_cancels_a_queued_walk_but_not_a_queued_prune() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("keep.txt"), "kept content").unwrap();
        std::fs::write(f.dir.join("drop.log"), "dropped content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.reindex_now();
        wait_for("initial run", Duration::from_secs(20), || {
            let s = coord.state();
            s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped && f.file_count() == 2
        });
        let stamped = coord.state().last_full_index;

        // Narrow and widen at once: one new pattern to prune by, one root
        // added to walk for. Turning auto off in the same edit is what makes
        // the coordinator enter manual-stopped with the plan already queued.
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

    #[test]
    fn auto_mode_runs_initial_index_and_applies_watcher_events() {
        let f = Fixture::new(true);
        std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();

        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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

    /// A healthy watcher reports its own size, so the GUI can say
    /// "watching N folders" instead of guessing.
    #[test]
    fn auto_mode_reports_an_active_watcher() {
        let f = Fixture::new(true);
        std::fs::create_dir_all(f.dir.join("sub")).unwrap();
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();

        wait_for("watcher active", Duration::from_secs(20), || {
            matches!(coord.state().watcher, WatcherStatus::Active { .. })
        });
        match coord.state().watcher {
            WatcherStatus::Active { dirs } => assert_eq!(dirs, 2, "root + sub"),
            other => panic!("expected Active, got {:?}", other),
        }
        coord.shutdown();
    }

    /// The regression this whole change exists for: exceeding the watch
    /// budget must surface as `Disabled` rather than a stderr line the GUI
    /// never sees — and the periodic reindex must keep the index fresh.
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
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), watcher_config).unwrap();

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
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(coord.state().watcher, WatcherStatus::Off);
        coord.shutdown();
    }

    #[test]
    fn stopping_turns_the_watcher_status_off() {
        let f = Fixture::new(true);
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
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

    /// `auto_index` is the mode written down, so a config whose value
    /// disagrees with the running mode switches it. That is what lets the
    /// GUI persist a Stop click, and what makes a hand-edited config take
    /// effect without a restart.
    #[test]
    fn applying_a_config_switches_the_mode_to_match_auto_index() {
        let f = Fixture::new(true);
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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
        // The reported failure: add directories, apply, click "Start
        // indexing now" — the run must pick up the new roots promptly
        // (watcher re-registration happens concurrently, never blocking
        // the command loop).
        let f = Fixture::new(true);
        std::fs::write(f.dir.join("first.txt"), "one").unwrap();
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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

    /// A `rm -rf` reports the directory *and* everything under it. Only the
    /// directory needs applying — its range sweep covers the rest — and
    /// collapsing before the overflow test is what stops a large deletion from
    /// discarding the queue and forcing a full run.
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

        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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

        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
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
        let coord = IndexCoordinator::start(config).unwrap();
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
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.shutdown();
        coord.shutdown(); // second call is a no-op
    }
}
