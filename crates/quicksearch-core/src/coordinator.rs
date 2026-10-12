//! The indexing coordinator: the one object binaries construct. Owns the
//! [`IndexingService`], the filesystem [`Watcher`], a periodic-reindex
//! scheduler, and the mode state machine:
//!
//! - `Auto`: watcher running; events apply incrementally between full runs;
//!   a full reindex triggers whenever `last_full_index` is older than the
//!   configured interval (or has never happened).
//! - `ManualStopped`: watcher off, pending events dropped, nothing runs
//!   until the user acts.
//! - `ManualRunning`: one user-forced full run; returns to `ManualStopped`
//!   when it finishes. (A forced run in Auto stays Auto.)
//!
//! `indexing.auto_index` persists the mode; writing the file back is the
//! caller's job, since the coordinator's config is a copy, not the source of
//! truth. Single-writer guarantee: incremental writes are deferred while a
//! full run is active, then the queue drains; overflow collapses into one
//! full run.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};

use crate::config::{diff_actions, Config, IgnoreSet, IndexWork};
use crate::db;
use crate::extract::sandbox::KillMark;
use crate::extract::Registry;
use crate::incremental::{Applied, Budget};
use crate::indexing::{ConfigChange, IndexingService, IndexingStatus, PrepStep, ReconcileProgress};
use crate::log::now_unix;
use crate::scope::{Scope, WorkCursor};
use crate::shutdown::{join_within, EXIT_GRACE};
use crate::watcher::{FsEvent, WatchError, WatchFilters, Watcher, WatcherConfig};

mod inner;
#[cfg(test)]
mod tests;

use inner::Inner;

/// Pending-event ceiling; beyond this a full run is cheaper than replay.
const PENDING_OVERFLOW: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    Auto,
    ManualStopped,
    ManualRunning,
}

/// Whether live updates are running, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherStatus {
    /// Not running: manual mode, or no roots configured.
    Off,
    /// Registration in flight; it walks every root, so this can last minutes
    /// on large or networked trees.
    Starting,
    Active {
        dirs: usize,
    },
    /// Live updates unavailable; the periodic reindex is the only refresh.
    Disabled {
        reason: WatchError,
    },
}

/// A config reconciliation the coordinator applies between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileState {
    Running(ReconcileProgress),
    /// Finished within the last [`RECONCILE_SUMMARY_LINGER`].
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
    pub files: Option<i64>,
    pub queued_events: usize,
    pub watcher: WatcherStatus,
    /// The between-runs reconciliation, while it runs and briefly after. A
    /// run's *own* reconciliation is not here: it reads as
    /// [`IndexingStatus::Preparing`] with a [`PrepStep::Reconciling`].
    pub reconcile: Option<ReconcileState>,
    /// What each configured root held when indexing last completed. Roots
    /// never indexed to completion are absent rather than zero.
    pub root_counts: Arc<Vec<RootCount>>,
}

/// One configured root's stored figures, keyed the way the caller spells it.
#[derive(Debug, Clone)]
pub struct RootCount {
    /// The root exactly as `paths.indexing_paths` gives it; the `schema_info`
    /// key behind it is canonicalized, so re-spelling a root keeps its figures.
    pub root: String,
    pub counts: db::repo::RootCounts,
}

#[allow(clippy::large_enum_variant)]
enum CoordCmd {
    SetMode(IndexMode),
    ReindexNow,
    ConfigChanged(Config),
    RebuildIndex,
    ClearIndex,
    DeleteIndex(mpsc::Sender<Result<(), String>>),
    UpdatePaths(Vec<PathBuf>),
    /// Record a reason on the running watcher ([`Watcher::force_degraded`]).
    #[cfg(test)]
    DegradeWatcher(WatchError),
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

/// The two halves of cutting the coordinator's reconciliation short: the pair
/// [`db::InterruptSlot`] describes. A command cannot do it: the thread that
/// would read the command is the thread inside the scan.
#[derive(Default)]
struct ReconcileStop {
    cancel: AtomicBool,
    interrupt: db::InterruptSlot,
}

impl ReconcileStop {
    /// Flag first, then interrupt: setting the flag after the interrupt would
    /// leave a window in which the pass starts one more statement.
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
    files: Option<i64>,
    queued_events: usize,
    watcher: WatcherStatus,
    reconcile: Option<ReconcileState>,
    root_counts: Arc<Vec<RootCount>>,
    /// How many times [`Inner::tick`] has run: tests count ticks, not seconds.
    #[cfg(test)]
    ticks: u64,
    /// How many registrations [`Inner::start_watcher`] has begun.
    #[cfg(test)]
    watcher_starts: u64,
}

impl IndexCoordinator {
    /// `notify` is called when this coordinator starts doing something the
    /// frontend should be drawing; see [`Notify`].
    pub fn start(config: Config, notify: Notify) -> Result<IndexCoordinator, String> {
        Self::start_with_watcher_config(config, notify, WatcherConfig::default())
    }

    /// [`Self::start`] with explicit watcher debounce tuning.
    pub fn start_with_watcher_config(
        config: Config,
        notify: Notify,
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
            root_counts: Arc::new(Vec::new()),
            #[cfg(test)]
            ticks: 0,
            #[cfg(test)]
            watcher_starts: 0,
        }));

        let reconcile_stop = Arc::new(ReconcileStop::default());
        let mut inner = Inner {
            config,
            indexing: indexing.clone(),
            shared: shared.clone(),
            notify,
            awake: false,
            reconcile_stop: reconcile_stop.clone(),
            event_tx,
            event_rx,
            watcher: None,
            watcher_config,
            watcher_rx: None,
            watcher_cancel: Arc::new(AtomicBool::new(false)),
            overflow_rewatched: None,
            pending: inner::Debounced::default(),
            targeted: inner::Debounced::default(),
            resume_from: HashMap::new(),
            needs_full_run: false,
            run_retry: inner::Backoff::default(),
            pending_work: None,
            reconcile_done: None,
            reconcile_cut_short: false,
            run_in_flight: false,
            files_stale: true,
            was_busy: false,
            write_conn: None,
            held: false,
            unreported: None,
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
        let shared = crate::lock_ok(&self.shared);
        IndexerState {
            mode: shared.mode,
            activity: self.indexing.get_status(),
            last_full_index: shared.last_full_index,
            files: shared.files,
            queued_events: shared.queued_events,
            watcher: shared.watcher.clone(),
            reconcile: shared.reconcile,
            root_counts: shared.root_counts.clone(),
        }
    }

    /// Whether a run currently holds the database. [`Self::state`] answers this
    /// too, but clones the per-root progress with it; this is the form for a
    /// caller polling once a frame.
    pub fn is_indexing(&self) -> bool {
        self.indexing.is_active()
    }

    #[cfg(test)]
    fn ticks(&self) -> u64 {
        crate::lock_ok(&self.shared).ticks
    }

    #[cfg(test)]
    fn watcher_starts(&self) -> u64 {
        crate::lock_ok(&self.shared).watcher_starts
    }

    /// Have the running watcher report `reason`, as if it had hit it; a no-op
    /// while none runs.
    #[cfg(test)]
    fn degrade_watcher(&self, reason: WatchError) {
        let _ = self.cmd_tx.send(CoordCmd::DegradeWatcher(reason));
    }

    /// Whether the coordinator thread has ended. Its command receiver lives
    /// until `Inner::run` returns, so a send fails only after that; the probe
    /// is a no-op to a running thread.
    #[cfg(test)]
    fn thread_ended(&self) -> bool {
        self.cmd_tx.send(CoordCmd::UpdatePaths(Vec::new())).is_err()
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

    /// Delete the index and rebuild from scratch (user confirmed). A delete
    /// that fails is reported in [`IndexerState::activity`] and starts
    /// nothing.
    pub fn rebuild_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::RebuildIndex);
    }

    /// Delete the index and start nothing: no full run starts until
    /// [`Self::rebuild_index`] or [`Self::clear_index`] follows, so the caller
    /// can change what the next index is built under (the key) in between.
    /// The receiver gets the outcome; it reads `Disconnected` if the
    /// coordinator is gone. A failed delete holds nothing.
    pub fn delete_index(&self) -> mpsc::Receiver<Result<(), String>> {
        let (tx, rx) = mpsc::channel();
        // A failed send drops `tx` with the command.
        let _ = self.cmd_tx.send(CoordCmd::DeleteIndex(tx));
        rx
    }

    /// Delete the index WITHOUT rebuilding (user confirmed). Indexing
    /// drops to manual-stopped so automatic mode doesn't immediately
    /// resurrect what the user just deleted. A delete that fails is reported
    /// and keeps the published figures.
    pub fn clear_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::ClearIndex);
    }

    /// Refresh the index's rows for these files from the disk, and nothing
    /// else.
    ///
    /// For [`crate::live`]. Deliberately not gated on [`IndexMode`], since the
    /// point is that the rows on screen stay honest with indexing stopped, but
    /// still applied on the coordinator's own thread, so the single-writer
    /// rule holds. It never removes a row: a path that is missing, unreadable
    /// or a directory changes nothing, since a display cannot tell a deletion
    /// from an unplugged drive. The watcher and the next run remove rows.
    pub fn update_paths(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let _ = self.cmd_tx.send(CoordCmd::UpdatePaths(paths));
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

    /// Stop the watcher, any running index pass, and the coordinator thread.
    /// Idempotent; usable from a signal handler through an Arc. Waits at most
    /// [`EXIT_GRACE`] for the thread, then leaves it running: one file's
    /// extraction or a `stat` on a hung mount reaches no flag, and a closing
    /// window must not wait on either. The reconciliation is cancelled
    /// *before* the command is sent: the command is read by the thread inside
    /// the scan, which would otherwise spend the grace there. A file parsing
    /// in a sandbox child is killed, so that file at least is no wait. That
    /// cancel also precedes the kill, which a turn's kill mark relies on
    /// (`Inner::apply_queue`).
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_stop.stop();
        let _ = self.cmd_tx.send(CoordCmd::Shutdown);
        crate::extract::sandbox::kill_sandbox_children();
        if let Some(handle) = crate::lock_ok(&self.handle).take() {
            if !join_within(handle, Instant::now() + EXIT_GRACE) {
                crate::log_warn!(
                    "the coordinator thread did not stop within {:?}; not waiting for it",
                    EXIT_GRACE
                );
            }
        }
    }

    /// Whether a configuration change is being applied to the index right
    /// now, by either of the two places that can be doing it.
    pub fn reconciling(&self) -> bool {
        let between_runs = crate::lock_ok(&self.shared).reconcile;
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

/// Fold `event` into `queue`'s two sets. A `Create` never replaces a queued
/// removal of the same path, so `mv build build.old; mkdir build` still drops
/// the old tree's rows, even when a full run holds the queue for minutes
/// between the two. A `Remove` drops the path's upsert, a `Modify` never
/// downgrades a `Create` to no walk, and a rename is a `Remove` plus a
/// `Create`.
fn enqueue(queue: &mut inner::Debounced, event: FsEvent) {
    match event {
        FsEvent::Remove(p) => {
            queue.upserts.remove(&p);
            queue.removals.insert(p);
        }
        FsEvent::Create(p) => {
            queue.upserts.insert(p, true);
        }
        FsEvent::Modify(p) => {
            queue.upserts.entry(p).or_insert(false);
        }
        FsEvent::Rename { from, to } => {
            enqueue(queue, FsEvent::Remove(from));
            enqueue(queue, FsEvent::Create(to));
        }
    }
}

/// Drop queued removals already covered by a queued removal of an ancestor,
/// keeping `rm -rf` on a large tree under [`PENDING_OVERFLOW`]. Upserts are a
/// set of their own, so a re-creation under a removed directory survives
/// (removals are applied first).
fn collapse_pending_removals(removals: &mut HashSet<PathBuf>) {
    if removals.len() < 2 {
        return;
    }
    let covered: Vec<PathBuf> = removals
        .iter()
        .filter(|path| path.ancestors().skip(1).any(|a| removals.contains(a)))
        .cloned()
        .collect();
    for path in &covered {
        removals.remove(path);
    }
}

/// How long one [`Inner::apply_queue`] call, for either queue, may hold the
/// command loop; the rest of the queue waits for the next tick, 1 s later.
const APPLY_BUDGET: Duration = Duration::from_millis(250);

/// Wakes the frontend when this thread changes something worth drawing.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

fn summary_is_fresh(at: Instant, now: Instant) -> bool {
    now.duration_since(at) < RECONCILE_SUMMARY_LINGER
}
