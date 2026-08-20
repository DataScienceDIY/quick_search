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
//! starting mode and tracks every mode change; writing the file back is the
//! caller's job — the coordinator's config is a copy, not the source of truth.
//!
//! Single-writer guarantee: incremental writes are deferred while a full
//! run is active, then the queue drains. Overflowing the queue (>100k
//! pending paths) collapses into one full run instead.
//!
//! Once per busy→idle transition, [`Inner::go_idle`] drops the write
//! connection (and its page cache), returns freed heap to the OS, and
//! refreshes the published file count.

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
use crate::incremental::{apply_fs_event, Applied, Budget};
use crate::indexing::{ConfigChange, IndexingService, IndexingStatus, PrepStep, ReconcileProgress};
use crate::scope::WorkCursor;
use crate::watcher::{FsEvent, WatchError, WatchFilters, Watcher, WatcherConfig};

mod inner;
#[cfg(test)]
mod tests;

use inner::Inner;

/// Pending-event ceiling; beyond this a full run is cheaper than replay.
const PENDING_OVERFLOW: usize = 100_000;

/// How often the published file count is re-read while idle.
const FILE_COUNT_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Registration in flight — it walks every root, so this can last
    /// minutes on large or networked trees.
    Starting,
    /// Live updates active over `dirs` watched directories.
    Active { dirs: usize },
    /// Live updates unavailable; the periodic reindex is the only refresh.
    Disabled { reason: WatchError },
}

/// A config reconciliation the coordinator applies between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileState {
    /// Scanning now; the counters move every slice.
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
    /// Watcher events waiting to be applied.
    pub queued_events: usize,
    /// Live-update health; see [`WatcherStatus`].
    pub watcher: WatcherStatus,
    /// The between-runs reconciliation, while it runs and briefly after.
    ///
    /// A run's *own* reconciliation is not here — it reads as
    /// [`IndexingStatus::Preparing`] with a [`PrepStep::Reconciling`].
    pub reconcile: Option<ReconcileState>,
    /// What each configured root held when indexing last completed. Roots
    /// never indexed to completion are absent rather than zero.
    ///
    /// `Arc` because `state()` is called more than once per frame and this
    /// changes only when a run ends.
    pub root_counts: Arc<Vec<RootCount>>,
}

/// One configured root's stored figures, keyed the way the caller spells it.
#[derive(Debug, Clone)]
pub struct RootCount {
    /// The root exactly as `paths.indexing_paths` gives it, so a frontend can
    /// match it against the string it already draws. The `schema_info` key
    /// behind it is the canonicalized spelling, so re-spelling a root in the
    /// config keeps its figures.
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
    UpdatePaths(Vec<PathBuf>),
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
/// thread inside the scan.
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
    /// Rows in `files`, for the idle status bar's "N files indexed".
    /// `None` until the first successful read.
    files: Option<i64>,
    queued_events: usize,
    watcher: WatcherStatus,
    reconcile: Option<ReconcileState>,
    root_counts: Arc<Vec<RootCount>>,
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
            watcher_gen: 0,
            pending: HashMap::new(),
            targeted: HashMap::new(),
            resume_from: HashMap::new(),
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

    /// Bring the index up to date for these paths and nothing else.
    ///
    /// For [`crate::live`]: a frontend that has just read a displayed file
    /// from disk hands the path here so the index agrees with what the user
    /// is looking at. Deliberately **not** gated on [`IndexMode`] — the whole
    /// point is that the rows on screen stay honest with indexing stopped —
    /// but still applied on the coordinator's own thread, so the
    /// single-writer rule holds and a full run is never raced.
    ///
    /// Each path is re-read and rewritten only if its modified time has moved
    /// (see [`crate::incremental::apply_fs_event`]), so submitting a path that
    /// is already current costs a `stat` and a row lookup. A path that no
    /// longer exists is removed from the index.
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

    /// Stop the watcher, any running index pass, and the coordinator
    /// thread. Idempotent; usable from a signal handler through an Arc.
    ///
    /// The reconciliation is cancelled *before* the command is sent: the
    /// command is read by the thread inside the scan, so the join — and the
    /// window close behind it — would otherwise wait out the scan.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_stop.stop();
        let _ = self.cmd_tx.send(CoordCmd::Shutdown);
        if let Some(handle) = crate::lock_ok(&self.handle).take() {
            let _ = handle.join();
        }
    }

    /// Whether a configuration change is being applied to the index right
    /// now, by either of the two places that can be doing it.
    ///
    /// An abandoned pass leaves entries the user excluded still in the index
    /// until the next indexing run redoes it.
    pub fn reconciling(&self) -> bool {
        // Lock released before asking the service; never hold both at once.
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

/// The verb a path submitted through [`IndexCoordinator::update_paths`]
/// deserves, or `None` to leave the index alone.
///
/// The caller knows a file changed, not what it changed into. `is_file()` is
/// the fast answer and almost always the right one — one `stat`, and this runs
/// while results are on screen — but it folds every stat error into `false`,
/// and a `Remove` costs the row *and everything beneath it*. So the negative
/// answer, and only it, is confirmed with a second `stat` that can tell "gone"
/// from "cannot see it just now": a share that dropped, a drive pulled while
/// its rows were displayed, a parent another process chmod'd.
///
/// In doubt the index wins. A stale row is a wrong line on screen until the
/// next full run; a deleted live one is data no run brings back until the file
/// is walked again — and if the reason it could not be read was that its whole
/// tree went away, that walk will not reach it either.
fn verb_for(path: PathBuf) -> Option<FsEvent> {
    if path.is_file() {
        return Some(FsEvent::Modify(path));
    }
    // `metadata`, not `symlink_metadata`: it has to agree with `is_file()`
    // above about following links, or the two can disagree about the verb.
    match std::fs::metadata(&path) {
        // There, but no longer something the walk would index.
        Ok(_) => Some(FsEvent::Remove(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(FsEvent::Remove(path)),
        Err(_) => None,
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

fn is_removal(event: &FsEvent) -> bool {
    matches!(event, FsEvent::Remove(_))
}

/// Drop queued removals that a queued removal of one of their ancestors
/// already covers, in place. Keeps `rm -rf` on a large tree from tripping
/// [`PENDING_OVERFLOW`] into a redundant full run.
///
/// Only removals collapse: a `Create` under a removed directory is a
/// re-creation and must survive (removals are applied first).
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
const APPLY_BUDGET: Duration = Duration::from_millis(250);

/// Wakes the frontend when this thread changes something worth drawing.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Whether a reconciliation that finished at `at` is still worth reporting.
fn summary_is_fresh(at: Instant, now: Instant) -> bool {
    now.duration_since(at) < RECONCILE_SUMMARY_LINGER
}

use crate::log::now_unix;
