use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db;
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    cleanup_stale_index_entries, count_tree_entries_fast, extract_scope_prepare,
    fts_finalize_after_text_indexing, normalize_root_string, process_batch_inserts,
    process_batch_updates, store_extracted, ExtractCursor, FileIndexAction, OwnedNewFile,
};
use crate::walk::{thread_count_for, walk_indexable_files, ParallelWalk, TryNext, WalkEvent};

/// Where one root's pipeline is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPhase {
    /// The parallel walk is discovering and writing file metadata.
    Walking,
    /// The walk finished; content extraction is draining this root's
    /// pending rows.
    Extracting,
    Done,
}

/// Progress for one indexing root. Each root runs its own walker and its
/// own extraction cursor; the GUI shows one row per root.
#[derive(Debug, Clone)]
pub struct RootProgress {
    pub root: String,
    pub phase: RootPhase,
    /// Files the walk has seen so far. Final and exact once the root leaves
    /// [`RootPhase::Walking`] — see [`RootProgress::walk_denominator`].
    pub walked: usize,
    /// What to divide `walked` by, from whichever source this root has.
    /// `None` until one lands.
    ///
    /// Exact on a root walked before — last clean run's file count, read from
    /// the index and available immediately. Only a root with no stored count
    /// falls back to the concurrent `find` scan, which counts tree *entries*
    /// rather than walkable files and so reads high. Read it through
    /// [`RootProgress::walk_denominator`] rather than directly.
    pub walk_total: Option<usize>,
    /// Rows with searchable text: extracted in earlier runs plus this one.
    pub extracted: usize,
    /// The root's whole searchable set: pending + already-extracted rows at
    /// the moment the walk finished. Files no extractor claims are already
    /// `NA` by then, so this is the count of files that have or will have
    /// text — not the count of files under the root.
    pub extract_total: usize,
    pub current_file: Option<String>,
    /// Threads busy right now / pool size, for whichever pool this root's
    /// current phase is running — the walk's while walking, the content
    /// pass's while extracting. Both zero once the root is done: its threads
    /// are gone, and reporting a dead pool's size is how the status line came
    /// to read "0/44 workers".
    pub active_workers: usize,
    pub total_workers: usize,
}

impl RootProgress {
    /// This root's walk-phase contribution to a progress denominator.
    ///
    /// While the walk runs, `walk_total` is the best figure there is, and how
    /// good that is depends on where it came from — see the field. A root
    /// walked before contributes its exact stored count; a first-time root
    /// contributes the `find` scan's entry count, which on a home directory
    /// runs over 1.6x high. Once the walk ends `walked` is final and exact for
    /// *both*, so `walk_total` is dropped either way: keeping it is what
    /// stopped the bar reaching 100%.
    pub fn walk_denominator(&self) -> Option<usize> {
        match self.phase {
            // Never below what has already been walked: a denominator the walk
            // has overtaken is provably wrong, and a bar pinned at 100%
            // mid-walk reads as a hang.
            RootPhase::Walking => self.walk_total.map(|t| t.max(self.walked)),
            RootPhase::Extracting | RootPhase::Done => Some(self.walked),
        }
    }
}

/// Files processed and the run's total, across every root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverallProgress {
    /// Both halves of the work: every file the walks have seen, plus every
    /// row with searchable text. A file is counted once for each, so this is
    /// a work-units figure rather than a file count.
    pub processed: usize,
    /// `None` while a still-walking root has no count yet; no root past its
    /// walk can withhold one, so a run always gains a total in the end.
    pub total: Option<usize>,
}

impl OverallProgress {
    /// Completed share, clamped to 1. `None` when there is nothing to
    /// divide by — an unknown total, or a run with no work in it at all.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.processed as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}

/// Aggregate every root's progress into the one pair the status bar shows.
///
/// The extraction half needs no estimate: `extract_total` is queried exactly,
/// from the rows themselves, the moment a root's walk ends. Before that it is
/// zero and the root contributes only its walk — which is also the only part
/// of it the GUI reports.
pub fn overall_progress(roots: &[RootProgress]) -> OverallProgress {
    let processed = roots.iter().map(|r| r.walked + r.extracted).sum();
    let mut total = Some(0usize);
    for r in roots {
        match (total, r.walk_denominator()) {
            (Some(acc), Some(walk)) => total = Some(acc + walk + r.extract_total),
            _ => {
                total = None;
                break;
            }
        }
    }
    OverallProgress { processed, total }
}

/// How far a configuration reconciliation has got.
///
/// Reported by both places one can run: a full run's prologue
/// ([`IndexingService::reconcile_stored_config`]) and the coordinator's
/// between-runs pass ([`crate::scope::advance`] driven from its tick). On a
/// multi-million-row index either is minutes of work with nothing else to
/// show for it, so it gets its own counters rather than a spinner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileProgress {
    /// Stored rows re-tested against the current configuration so far.
    pub examined: usize,
    /// Rows in the index, counted once when the scan starts. `None` while
    /// the plan is doing whole-range work that reads no rows at all.
    pub total: Option<usize>,
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
}

impl ReconcileProgress {
    /// Completed share, clamped to 1. `None` when there is nothing to divide
    /// by, matching [`OverallProgress::fraction`].
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.examined as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }
}

/// What a run is doing before its first file is walked.
///
/// Each of these can run for minutes on a large index — a WAL recovery, a
/// previous run winding down, a full re-test of every stored row against a
/// changed configuration — and all of them used to be one static "Starting…"
/// with no elapsed clock, which is indistinguishable from a hang.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepStep {
    /// Waiting on the previous run's thread to wind down.
    PreviousRun,
    /// [`db::open_or_recreate`]: schema migration and WAL recovery.
    OpeningIndex,
    /// Re-testing stored rows against a configuration that changed since the
    /// last run.
    Reconciling(ReconcileProgress),
}

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    /// A run has been claimed but has not reached its walk yet. Holds the
    /// database exactly as `Running` does, so every caller that defers to a
    /// run must defer to this too.
    Preparing {
        start_time: Instant,
        step: PrepStep,
    },
    Running {
        start_time: Instant,
        roots: Vec<RootProgress>,
    },
    Stopping,
    /// Compacting and re-analysing the index after a run — see
    /// [`IndexingService::run_maintenance`]. Distinct from `Running` because
    /// it holds the database with no per-file progress to show, and distinct
    /// from `Idle` because the single-writer rule still applies: the
    /// coordinator must stay off the file until this clears.
    Optimizing,
    Error(String),
}

/// One setting whose stored (index-build-time) value differs from the
/// current config. Values that hold lists (roots, patterns, extensions)
/// are newline-joined — display them as multi-line columns, not inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub key: String,
    /// What the index was built with.
    pub stored: String,
    /// What the config says now.
    pub current: String,
}

// `Start` carries a whole `Config`. Like `CoordCmd`, these are one-per-user-
// action control messages, not a data path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum IndexingCommand {
    Start {
        /// One or more directory roots to index. Every root walks
        /// concurrently on its own pool, so order decides only which spelling
        /// of a duplicated root survives; duplicates are dropped at run time.
        paths: Vec<String>,
        db_path: String,
        config: Config,
    },
    Stop,
}

/// Collect rows whose parent directory the walk never reached.
///
/// Per-directory reconciliation can only speak for directories it read, so a
/// directory deleted wholesale — or newly excluded by an ignore pattern —
/// leaves its rows unaccounted for. This finds them by scanning the distinct
/// parents stored under `root` and keeping the ones absent from `seen_dirs`.
///
/// Two kinds of absence are *not* deletions and are filtered out:
///
/// - A parent under a directory the walk could not read. Its children were
///   never discovered, so their absence proves nothing.
/// - A path reached by resolving a symlink. Its row's parent may lie outside
///   every root and so is never visited by construction; `aliased` is the
///   record that the file itself was seen.
///
/// The parent scan streams (see [`repo::for_each_parent_in_range`]) so this
/// costs memory proportional to the *unvisited* directories, not to the tree.
fn sweep_unvisited_parents(
    conn_mutex: &Arc<Mutex<Connection>>,
    root: &str,
    seen_dirs: &HashSet<String>,
    unreadable: &crate::file_handling::UnreadableDirs,
    aliased: &HashSet<String>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    // Same keyset range the extraction cursor uses: `[root + "/", root + "0")`.
    let range = ExtractCursor::for_root(root);
    let conn = conn_mutex.lock().unwrap();

    // Collected rather than streamed into the second query: both borrow the
    // same connection, and the outer statement is still live while iterating.
    let mut unvisited: Vec<String> = Vec::new();
    repo::for_each_parent_in_range(&conn, &range.lo, &range.hi, |parent| {
        if !seen_dirs.contains(&parent) && !unreadable.covers(&parent) {
            unvisited.push(parent);
        }
    })?;

    for parent in unvisited {
        for path in repo::paths_in_dir(&conn, &parent)? {
            if !aliased.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(())
}

// No `Debug`: rusqlite's `InterruptHandle` has none, and nothing formats the
// service anyway.
pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
    command_tx: mpsc::Sender<IndexingCommand>,
    db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    suspend_flag: Arc<AtomicBool>,
    /// The long single statement a run is inside, if any: the reconcile scan
    /// in its prologue, or the VACUUM in its epilogue. Never both — one is
    /// what the run does before it walks and the other what it does after —
    /// so one slot is enough, and one [`IndexingService::cancel_db_work`]
    /// reaches whichever is there.
    interrupt: Arc<db::InterruptSlot>,
    _handle: thread::JoinHandle<()>,
}

/// Polling interval for `should_abort` while suspended.
const SUSPEND_POLL_MS: u64 = 100;

/// `indexing.root_workers` rekeyed from the spellings the user typed to the
/// canonical roots the indexer walks, so an override survives a `~`, a
/// trailing slash, a relative path or a symlinked root. Entries naming a
/// folder that is no longer indexed are dropped.
fn resolved_root_workers(config: &Config) -> HashMap<String, usize> {
    config
        .paths
        .indexing_paths
        .iter()
        .zip(config.resolved_indexing_paths())
        .filter_map(|(raw, resolved)| {
            let workers = config.indexing.root_workers.get(raw).copied()?;
            Some((normalize_root_string(&resolved.to_string_lossy()), workers))
        })
        .collect()
}

/// Size of the write-ahead log on disk, or 0 if it is absent.
///
/// A run watches this because SQLite will not bound it. The autocheckpoint
/// copies committed frames into the index continuously, but the log only
/// *shrinks* when the writer opens a transaction at an instant no reader holds
/// a read mark — a lock SQLite tries once, without retrying. A run keeps a
/// reader per root querying from start to finish (one read per directory while
/// walking, one per page of rows while extracting), so that instant does not
/// come, and the log appends for the length of the run: on a large tree it
/// ends up larger than the index it journals. An explicit checkpoint retries
/// the same lock under `busy_timeout` and wins, which is why `run_indexing`
/// forces one every `maximum_wal_size` bytes.
fn wal_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Combined stop/suspend check used by worker loops. Returns `true` iff the
/// caller should abort the operation. While the suspend flag is set and stop
/// is not, this parks the thread by sleeping in short increments so a later
/// `resume()` unblocks it. Cheap to call in tight loops.
pub(crate) fn should_abort(stop: &Arc<AtomicBool>, suspend: &Arc<AtomicBool>) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        if !suspend.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(SUSPEND_POLL_MS));
    }
}

/// Flips an [`AtomicBool`] when dropped. Held by `run_indexing` so the
/// per-root count subprocesses die on every exit path of a run.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// One root's in-flight indexing state, owned by the writer loop.
struct RootPipeline {
    root: String,
    walk: ParallelWalk,
    /// This root's walk denominator; 0 = not yet known. Stored at once from
    /// last run's count, or filled in later by the `find` scan for a root that
    /// has none. See [`RootProgress::walk_total`].
    count_total: Arc<AtomicUsize>,
    /// Threads this root gets, for the walk and then for extraction: both are
    /// round-trip bound on a share for the same reason, so both use the value
    /// `thread_count_for` / `root_workers` produced.
    workers: usize,
    pending_updates: Vec<OwnedNewFile>,
    pending_inserts: Vec<OwnedNewFile>,
    walked: usize,
    walk_clean: bool,
    phase: RootPhase,
    /// The running content pass, once this root's walk has finished.
    content: Option<crate::content::ContentPass>,
    extract_total: usize,
    extracted: usize,
    current_file: Option<String>,
    /// When this root's current phase began, for the one line each phase logs
    /// when it ends.
    ///
    /// Timing lives here rather than in a probe binary because the gap this
    /// exists to measure is between *platforms*, and nobody reproducing a slow
    /// index on their own machine is going to build an example first. One line
    /// per phase per root is cheap enough to leave on always — `log::record`
    /// takes a global mutex and writes stderr unbuffered, which is only a
    /// problem at per-file rates.
    phase_started: Instant,
}

impl RootPipeline {
    /// End the current phase and return how long it took, restarting the clock
    /// for the next one.
    fn phase_elapsed(&mut self) -> Duration {
        let started = std::mem::replace(&mut self.phase_started, Instant::now());
        started.elapsed()
    }
}

/// `n` per second, or `None` when the interval is too short to divide by.
///
/// Guards the display, not the arithmetic: a walk that finishes in under a
/// millisecond would otherwise report a rate in the millions, which reads as a
/// bug rather than as "there was nothing to do".
fn per_second(n: usize, elapsed: Duration) -> Option<f64> {
    let secs = elapsed.as_secs_f64();
    (secs >= 0.001).then(|| n as f64 / secs)
}

/// How long the writer loop waits when every root has momentarily run dry.
///
/// A ceiling, not a delay: the loop parks on a channel, so a walker that
/// produces anything wakes it at once. It only matters when nothing at all is
/// happening, and then only to bound how long a newly-arrived event can sit.
const IDLE_BACKOFF: Duration = Duration::from_millis(2);

/// Report what the per-run warning throttles counted but did not print.
///
/// The count is the informative part: three unhashable files and thirty
/// thousand call for very different responses, and neither reads as such from a
/// wall of identical lines. See [`crate::log::Throttle`].
fn report_run_warnings() {
    let (failed, suppressed) = crate::file_handling::hash_failure_counts();
    if failed > 0 {
        crate::log_warn!(
            "{} file{} could not be read to hash{}",
            failed,
            if failed == 1 { "" } else { "s" },
            if suppressed > 0 {
                format!(" ({} similar warnings not shown)", suppressed)
            } else {
                String::new()
            }
        );
    }
}

/// One phase's timing line: "12,345 files in 41.2s (300/s)".
fn phase_summary(n: usize, noun: &str, elapsed: Duration) -> String {
    match per_second(n, elapsed) {
        Some(rate) => format!(
            "{} {} in {:.1}s ({:.0}/s)",
            n,
            noun,
            elapsed.as_secs_f64(),
            rate
        ),
        None => format!("{} {} in {:.3}s", n, noun, elapsed.as_secs_f64()),
    }
}

impl RootPipeline {
    /// Busy threads / pool size for the pool this root is currently running.
    ///
    /// A root outlives its walk — the `ParallelWalk` stays in the struct after
    /// its workers exit — so the pool has to be chosen by phase, not read from
    /// whichever handle happens to be at hand.
    fn worker_counts(&self) -> (usize, usize) {
        let stats = match self.phase {
            RootPhase::Walking => Some(self.walk.worker_stats()),
            RootPhase::Extracting => self.content.as_ref().map(|p| p.worker_stats()),
            RootPhase::Done => None,
        };
        stats.map_or((0, 0), |s| (s.active(), s.total()))
    }
}

impl IndexingService {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(IndexingStatus::Idle));
        let (command_tx, command_rx) = mpsc::channel();
        let db_connection = Arc::new(Mutex::new(None));
        let suspend_flag = Arc::new(AtomicBool::new(false));
        let interrupt: Arc<db::InterruptSlot> = Arc::new(db::InterruptSlot::default());

        let status_clone = status.clone();
        let db_connection_clone = db_connection.clone();
        let suspend_clone = suspend_flag.clone();
        let interrupt_clone = interrupt.clone();
        let handle = thread::spawn(move || {
            Self::indexing_thread(
                status_clone,
                command_rx,
                db_connection_clone,
                suspend_clone,
                interrupt_clone,
            );
        });

        IndexingService {
            status,
            command_tx,
            db_connection,
            suspend_flag,
            interrupt,
            _handle: handle,
        }
    }

    /// Cut short the long statement a run is inside — an
    /// [`IndexingStatus::Optimizing`] VACUUM, or the reconcile scan of its
    /// prologue. No-op when there is none.
    ///
    /// The interrupt half of the pair [`db::InterruptSlot`] describes. Stop
    /// reaches the reconcile through it as well as through the stop flag; it
    /// does *not* reach the VACUUM, because optimizing is what runs after a run
    /// stops and waiting it out is the normal case. The exception is the caller
    /// that cannot wait — deleting the index for a rebuild, where a VACUUM
    /// still holding the file would fail the delete outright on Windows. An
    /// interrupted statement rolls back.
    pub fn cancel_db_work(&self) {
        db::interrupt(&self.interrupt)
    }

    /// Pause the indexer. All worker loops that call [`should_abort`] will
    /// block until [`resume`](Self::resume) is called. No-op if already
    /// suspended. Does not stop the worker — stop_indexing is still the way
    /// to abort.
    pub fn suspend(&self) {
        self.suspend_flag.store(true, Ordering::Relaxed);
    }

    /// Resume indexing after [`suspend`](Self::suspend). No-op if not
    /// suspended.
    pub fn resume(&self) {
        self.suspend_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_suspended(&self) -> bool {
        self.suspend_flag.load(Ordering::Relaxed)
    }

    /// Start indexing one or more roots. Each root gets its own walker and
    /// they all run concurrently, funnelling into one writer thread; duplicate
    /// roots collapse to one walk, and a file reachable from more than one of
    /// them is written once. At least one path is required. Returns `Err` if a
    /// run is already in flight.
    ///
    /// Nested roots are not de-duplicated here — they are refused outright,
    /// before this is ever called (see [`crate::config::nested_roots`]).
    ///
    /// The `Idle → Preparing` transition happens **here**, synchronously, not on
    /// the service's command thread. Callers use [`get_status`](Self::get_status)
    /// to enforce the single-writer rule, and the command thread cannot flip the
    /// status until it has finished joining the *previous* run's thread — which
    /// can take arbitrarily long. A caller that polled for the flip would give
    /// up and start writing to a database this run is about to reopen (and
    /// possibly wipe). Claiming the status under one lock before sending closes
    /// that window entirely, and makes "already running" a reportable error
    /// rather than a silently dropped command.
    ///
    /// That join is also the first thing the new run reports, which is why the
    /// claim is [`PrepStep::PreviousRun`] rather than an empty `Running`: the
    /// wait is real work with a real duration, and the status says so.
    pub fn start_indexing(
        &self,
        paths: Vec<String>,
        db_path: String,
        config: Config,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("start_indexing requires at least one path".into());
        }
        {
            let mut status = self.status.lock().unwrap();
            if matches!(
                *status,
                IndexingStatus::Preparing { .. }
                    | IndexingStatus::Running { .. }
                    | IndexingStatus::Stopping
            ) {
                return Err("indexing is already running".into());
            }
            *status = IndexingStatus::Preparing {
                start_time: Instant::now(),
                step: PrepStep::PreviousRun,
            };
        }
        self.command_tx
            .send(IndexingCommand::Start {
                paths,
                db_path,
                config,
            })
            .map_err(|e| {
                // The service is gone; leave the status honest rather than
                // stuck on a run that will never happen.
                *self.status.lock().unwrap() = IndexingStatus::Idle;
                format!("Failed to send start command: {}", e)
            })
    }

    /// Signal a running index pass to stop without waiting for it. Used
    /// on shutdown paths that must stay responsive — the worker notices
    /// the flag between batches and WAL makes an unflushed exit safe.
    pub fn request_stop(&self) {
        let _ = self.command_tx.send(IndexingCommand::Stop);
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        self.command_tx
            .send(IndexingCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))?;

        let mut attempts = 0;
        while attempts < 50 {
            // 50 × 100 ms: five seconds for the command thread to pick the
            // Stop up. Past that, checkpoint anyway rather than block a caller
            // that is usually shutting the process down.
            match self.get_status() {
                IndexingStatus::Stopping => break,
                IndexingStatus::Idle => return Ok(()), // Already stopped
                IndexingStatus::Error(_) => return Ok(()), // Consider error state as stopped
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
            }
        }

        // Flush the WAL and release the shared connection. WAL mode itself
        // stays on — it's the persistent journal mode for the index.
        if let Ok(mut db_opt) = self.db_connection.lock() {
            if let Some(db_conn_arc) = db_opt.take() {
                if let Ok(conn) = db_conn_arc.lock() {
                    if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                        crate::log_warn!("{}", e);
                    }
                }
            }
        }

        Ok(())
    }

    pub fn get_status(&self) -> IndexingStatus {
        self.status.lock().unwrap().clone()
    }

    /// Move the prologue on to `step`, keeping the run's start time so one
    /// elapsed clock spans the whole run.
    ///
    /// A no-op once the status has left `Preparing`: a Stop that arrives
    /// mid-prologue owns the status from that point on, the same deference
    /// `run_indexing`'s `publish` shows it.
    fn set_prep_step(status: &Arc<Mutex<IndexingStatus>>, step: PrepStep) {
        if let Ok(mut g) = status.lock() {
            if let IndexingStatus::Preparing { start_time, .. } = *g {
                *g = IndexingStatus::Preparing { start_time, step };
            }
        }
    }

    /// The instant this run was claimed, so the prologue and the walk share
    /// one clock. Falls back to now for a caller that drives `run_indexing`
    /// without going through [`Self::start_indexing`] — the tests and probes.
    fn run_start(status: &Arc<Mutex<IndexingStatus>>) -> Instant {
        match status.lock().map(|g| g.clone()) {
            Ok(IndexingStatus::Preparing { start_time, .. })
            | Ok(IndexingStatus::Running { start_time, .. }) => start_time,
            _ => Instant::now(),
        }
    }

    /// Force graceful shutdown - used for signal handling
    pub fn graceful_shutdown(&self) -> Result<(), String> {
        self.stop_indexing()
    }

    /// Check if configuration changes require index recreation. A pure
    /// *read* check: opens the existing index without ever wiping it (the
    /// old `open_or_recreate` here could destroy the index before the user
    /// confirmed the rebuild dialog). A missing or incompatible DB means
    /// there is nothing to validate — the indexer will (re)build under its
    /// own policy anyway.
    pub fn check_config_validation(
        &self,
        db_path: &str,
        config: &Config,
        roots: &[String],
    ) -> Result<Option<Vec<ConfigChange>>, String> {
        match db::open_existing(db_path, false) {
            Ok(conn) => Self::validate_config(&conn, config, roots),
            Err(_) => Ok(None),
        }
    }

    /// Stop indexing and delete the database file for a clean rebuild
    pub fn delete_index_for_rebuild(&self, db_path: &str) -> Result<(), String> {
        self.stop_indexing()
            .map_err(|e| format!("Failed to stop indexing: {}", e))?;
        // And cut short the optimize pass that follows it — this is the one
        // caller that cannot wait it out, since the file it is about to delete
        // is the file that pass has open.
        self.cancel_db_work();

        let mut attempts = 0;
        while attempts < 50 {
            // Five seconds, as in `stop_indexing`. Past that the delete below
            // is attempted anyway — on Windows it will fail while a handle is
            // still open, and the error says so.
            match self.get_status() {
                IndexingStatus::Idle => break,
                // Optimizing holds the file about to be deleted, so it is
                // waited on like a run; `cancel_db_work` above is what
                // keeps that wait short.
                IndexingStatus::Stopping
                | IndexingStatus::Preparing { .. }
                | IndexingStatus::Running { .. }
                | IndexingStatus::Optimizing => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
                IndexingStatus::Error(_) => break, // Consider error state as stopped
            }
        }

        // Before the removal, and unconditionally: readers holding this file
        // open have to be told even if the delete below fails, because the
        // half-deleted case is exactly the one where a stale handle does
        // damage. See [`db::bump_index_epoch`].
        db::bump_index_epoch();

        if std::path::Path::new(db_path).exists() {
            std::fs::remove_file(db_path)
                .map_err(|e| format!("Failed to delete database file: {}", e))?;
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{}", db_path, suffix));
        }

        Ok(())
    }

    fn indexing_thread(
        status: Arc<Mutex<IndexingStatus>>,
        command_rx: mpsc::Receiver<IndexingCommand>,
        db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        suspend_flag: Arc<AtomicBool>,
        interrupt: Arc<db::InterruptSlot>,
    ) {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut indexing_handle: Option<thread::JoinHandle<()>> = None;

        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start {
                    paths,
                    db_path,
                    config,
                } => {
                    // `start_indexing` already claimed the status and rejected
                    // a concurrent start, so there is nothing to re-check here.

                    // Join any previous indexing thread. This can block for as
                    // long as that run takes to wind down, which is exactly why
                    // the status flip does not live here.
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    stop_flag.store(false, Ordering::Relaxed);

                    let status_clone = status.clone();
                    let stop_flag_clone = stop_flag.clone();
                    let paths_owned = paths.clone();
                    let db_path_owned = db_path.clone();
                    let config_owned = config.clone();

                    let db_connection_clone = db_connection.clone();
                    let suspend_clone = suspend_flag.clone();
                    let interrupt_clone = interrupt.clone();
                    indexing_handle = Some(thread::spawn(move || {
                        // The writer thread: every DB write and every text
                        // extraction a run performs happens here.
                        crate::platform::set_background_priority();
                        let result = Self::run_indexing(
                            &status_clone,
                            &paths_owned,
                            &db_path_owned,
                            &stop_flag_clone,
                            &suspend_clone,
                            &config_owned,
                            &db_connection_clone,
                            &interrupt_clone,
                        );

                        // Released before maintenance, not after: VACUUM needs
                        // its own connection (see `db::open::open_maintenance`)
                        // and two writable connections on one file would only
                        // contend.
                        if let Ok(mut db_opt) = db_connection_clone.lock() {
                            *db_opt = None;
                        }

                        match result {
                            Err(e) => *status_clone.lock().unwrap() = IndexingStatus::Error(e),
                            // Stopped runs included: a run cut short still
                            // leaves a log to land and, if it got as far as
                            // deleting rows, slack to reclaim.
                            Ok(()) => {
                                *status_clone.lock().unwrap() = IndexingStatus::Optimizing;
                                Self::run_maintenance(&db_path_owned, &interrupt_clone);
                                *status_clone.lock().unwrap() = IndexingStatus::Idle;
                            }
                        }

                        // A run's whole working set becomes garbage at once
                        // here: the walk's in-flight buffers, every extractor's
                        // scratch, `seen_paths`, and the writer's page cache.
                        // Under glibc none of it goes back to the OS on its
                        // own, so a peak measured in hundreds of megabytes
                        // would otherwise be the process's floor for as long as
                        // it stays open. After the match, not inside it,
                        // because a failed or stopped run leaves exactly as
                        // much behind as a successful one.
                        crate::platform::release_free_heap();
                    }));
                }
                IndexingCommand::Stop => {
                    // Only a run is stoppable. Optimizing is what happens
                    // *after* a run stops, so a Stop landing during it has
                    // nothing left to ask for.
                    let mut guard = status.lock().unwrap();
                    if matches!(
                        *guard,
                        IndexingStatus::Preparing { .. } | IndexingStatus::Running { .. }
                    ) {
                        *guard = IndexingStatus::Stopping;
                        stop_flag.store(true, Ordering::Relaxed);
                        // The flag alone only stops the *next* statement, and
                        // the reconcile in a run's prologue can be inside one
                        // for minutes. Nothing else a run does holds the slot,
                        // so this is a no-op for a walk.
                        db::interrupt(&interrupt);
                    }
                }
            }
        }

        // The command channel closed, so the service is going away; a run
        // still in flight has to be joined before its thread outlives us.
        if let Some(handle) = indexing_handle {
            let _ = handle.join();
        }
    }

    /// Optimize the index once a run ends, completed or stopped: land the log,
    /// reclaim the file's slack, refresh the planner's statistics.
    /// Best-effort — nothing here can fail a run that is already over, so every
    /// outcome is a log line.
    ///
    /// Runs on its own connection, after the run's writer is closed. VACUUM on
    /// the indexer's connection would build the replacement index in RAM; see
    /// [`crate::db::schema::PRAGMAS_MAINTENANCE`].
    ///
    /// Deliberately not cancelled by the stop flag: Stop ends *indexing*, and
    /// this is what runs afterwards. `interrupt` is the one way out, for the
    /// caller that cannot wait — see [`Self::cancel_db_work`].
    fn run_maintenance(db_path: &str, interrupt: &db::InterruptSlot) {
        let conn = match crate::db::open::open_maintenance(db_path) {
            Ok(conn) => conn,
            Err(e) => {
                crate::log_warn!("optimize: {}", e);
                return;
            }
        };
        let armed = db::InterruptGuard::arm(interrupt, &conn);

        let dir = std::path::Path::new(db_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let outcome = crate::db::repo::maintain(&conn, &dir);

        drop(armed);
        match outcome {
            Ok(true) => crate::log_info!("optimized the index and reclaimed unused space"),
            Ok(false) => {}
            Err(e) => crate::log_warn!("optimize failed (non-fatal): {}", e),
        }
    }

    // Eight parameters, each a distinct piece of per-run state that the
    // thread genuinely needs. A struct to hold them would exist only to be
    // destructured on the first line.
    #[allow(clippy::too_many_arguments)]
    fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        paths: &[String],
        db_path: &str,
        stop_flag: &Arc<AtomicBool>,
        suspend_flag: &Arc<AtomicBool>,
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        interrupt: &db::InterruptSlot,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("run_indexing: no paths provided".into());
        }

        let run_started = Instant::now();
        // Per-run, so the counts these report describe this run rather than
        // everything since the process started.
        crate::walk::reset_run_warnings();
        crate::file_handling::reset_run_warnings();

        // De-duplicate while preserving order. Roots are canonicalized first
        // so `/home/jeremy` and `/home/jeremy/` (or a symlink to either)
        // collapse to one walk. Pure nested-root deduplication (skip a root
        // that is a prefix of an already-walked root) is handled by the
        // per-file `seen_paths` set below.
        let mut seen_roots = HashSet::new();
        let roots: Vec<String> = paths
            .iter()
            .map(|p| normalize_root_string(p))
            .filter(|p| seen_roots.insert(p.clone()))
            .collect();

        // Per-root worker overrides, rekeyed to match `roots`. The config
        // stores them under the root exactly as the user typed it, which is
        // not what a canonicalized root looks like once a `~`, a trailing
        // slash or a symlink is involved — and a lookup that misses is
        // invisible, it just quietly walks with the auto-detected count.
        let worker_overrides = resolved_root_workers(config);

        // One clock for the whole run: the prologue below can outlast the walk
        // on a large index, and an elapsed time that restarted at the walk
        // would hide exactly the wait the user is trying to understand.
        let run_start = Self::run_start(status);

        // Open and migrate the database to the current schema version. A
        // recovery of a large WAL happens here, before anything else can be
        // reported, so it is announced before it is attempted.
        Self::set_prep_step(status, PrepStep::OpeningIndex);
        let mut conn = db::open_or_recreate(db_path, &config.processing.tokenize)?;

        // Reconcile against the settings the index was last written under,
        // *before* stamping the new ones over them — the old record is the
        // only thing that knows a root was dropped. This is what makes a
        // config hand-edited while the app was closed behave like one edited
        // live: the walk below handles narrowed ignore rules on its own (a
        // filtered-out entry is simply absent from the directory listing it
        // reconciles against), but nothing in it ever visits a root that is no
        // longer configured, or revisits the content of a file whose mtime has
        // not moved.
        //
        // Against `roots`, not `config.paths.indexing_paths`: the roots to
        // walk are a parameter of this call, and nothing makes the caller
        // pass the ones its config names. Reconciling against the config
        // would then delete every row of the tree actually being indexed.
        //
        // Stamping is conditional on the reconcile *finishing*. A scan the stop
        // flag cut short has left rows the new configuration disowns still in
        // the index, and a walk cannot find them — it never descends into a
        // pruned directory. Stamping anyway would tell every future run the
        // index already matches, orphaning those rows for good; leaving the old
        // record in place is what makes the next run pick the work back up.
        if !Self::reconcile_stored_config(status, interrupt, &mut conn, config, &roots, stop_flag)?
        {
            return Ok(());
        }
        Self::update_config(&conn, config, &roots)?;

        // No up-front load of the whole `files` table: each walk's prefetcher
        // fetches one directory's rows at a time, so classification data is
        // never all resident at once and the walk starts immediately instead
        // of after a full table scan.

        let conn_mutex = Arc::new(Mutex::new(conn));

        // Published so `stop_indexing` can checkpoint through it without
        // waiting for this run's thread to unwind.
        if let Ok(mut db_opt) = db_connection.lock() {
            *db_opt = Some(conn_mutex.clone());
        }

        // Per-root concurrent counts, killed the moment this run exits by
        // any path — the guard flips the token on drop and the count
        // threads' subprocesses die within one poll interval.
        let count_cancel = Arc::new(AtomicBool::new(false));
        let _count_guard = CancelOnDrop(count_cancel.clone());

        // Shared with every root's walk workers, which use it to finish small
        // text files without handing them to the content pass below.
        let registry = Arc::new(Registry::default_set());
        let quantum = config.processing.batch_size.max(1);

        // Roots that have been walked before need no counting at all — last
        // run's file count is both cheaper and more accurate than a concurrent
        // scan. Read them all up front, under one lock, before the walks start
        // competing for the connection.
        let stored_counts: Vec<Option<usize>> = {
            let conn = conn_mutex.lock().unwrap();
            let _ = crate::db::repo::prune_root_walk_counts(&conn, &roots);
            roots
                .iter()
                .map(|r| crate::db::repo::get_root_walk_count(&conn, r))
                .collect()
        };

        // One pipeline per root: its own walker (with per-root worker
        // count), its own buffers and extraction cursor, and — only on a root
        // never walked before — its own count thread. They all funnel into this
        // single writer thread.
        let mut pipelines: Vec<RootPipeline> = Vec::with_capacity(roots.len());
        for (root, stored_count) in roots.iter().zip(stored_counts) {
            let ignore = crate::config::IgnoreSet::compile(&config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?;
            let workers = worker_overrides
                .get(root)
                .copied()
                .filter(|w| *w > 0)
                .unwrap_or_else(|| thread_count_for(std::slice::from_ref(root)))
                .clamp(1, 64);
            let walk = walk_indexable_files(
                std::slice::from_ref(root),
                config.indexing.follow_symlinks,
                config.indexing.include_hidden,
                ignore,
                db_path,
                config.clone(),
                registry.clone(),
                stop_flag.clone(),
                suspend_flag.clone(),
                workers,
            );

            // A root walked before needs no scan: its stored file count is the
            // denominator, available instantly and exact rather than 1.6x high.
            // Only a root with no stored count pays for one — and that scan is a
            // second full traversal of the tree, concurrent with the real walk,
            // which is precisely what this avoids repeating on every run.
            let count_total = Arc::new(AtomicUsize::new(0));
            match stored_count {
                // `max(1)` for the same reason the counting arm uses it: 0 is
                // the "unknown" sentinel, so an empty root must not store it.
                Some(n) => count_total.store(n.max(1), Ordering::Relaxed),
                None => {
                    let root = root.clone();
                    let cancel = count_cancel.clone();
                    let total = count_total.clone();
                    let _ = thread::Builder::new()
                        .name("qs-count".into())
                        .spawn(move || {
                            crate::platform::set_background_priority();
                            match count_tree_entries_fast(&root, &cancel) {
                                // A genuinely empty root stores 1 so "known"
                                // stays distinguishable from the 0 = unknown
                                // sentinel; an empty root's walk finishes
                                // instantly anyway.
                                Ok(n) => total.store(n.max(1), Ordering::Relaxed),
                                Err(e) => {
                                    if !e.contains("cancelled") {
                                        crate::log_warn!("count for {}: {}", root, e);
                                    }
                                }
                            }
                        });
                }
            }

            pipelines.push(RootPipeline {
                root: root.clone(),
                walk,
                count_total,
                pending_updates: Vec::new(),
                pending_inserts: Vec::new(),
                walked: 0,
                walk_clean: true,
                phase: RootPhase::Walking,
                workers,
                content: None,
                extract_total: 0,
                extracted: 0,
                current_file: None,
                phase_started: Instant::now(),
            });
        }

        // Publish a status snapshot. Never clobbers Stopping — the command
        // thread owns that transition.
        let publish = |pipelines: &[RootPipeline]| {
            let roots: Vec<RootProgress> = pipelines
                .iter()
                .map(|p| {
                    let (active_workers, total_workers) = p.worker_counts();
                    RootProgress {
                        root: p.root.clone(),
                        phase: p.phase,
                        walked: p.walked,
                        walk_total: match p.count_total.load(Ordering::Relaxed) {
                            0 => None,
                            n => Some(n),
                        },
                        extracted: p.extracted,
                        extract_total: p.extract_total,
                        current_file: p.current_file.clone(),
                        active_workers,
                        total_workers,
                    }
                })
                .collect();
            if let Ok(mut g) = status.lock() {
                if !matches!(*g, IndexingStatus::Stopping) {
                    *g = IndexingStatus::Running {
                        start_time: run_start,
                        roots,
                    };
                }
            }
        };
        publish(&pipelines);

        // 128-bit path digests, not paths. Its only job is to drop a repeat
        // visit, and at millions of files owning every path string again —
        // on top of the rows SQLite already holds — was the single largest
        // allocation in a run. See `walk::path_digest`.
        let mut seen_paths: HashSet<u128> = HashSet::new();
        // Rows the per-directory reconciliation found no file behind, plus
        // whatever the vanished-directory sweep adds once the walks end.
        let mut stale_candidates: Vec<String> = Vec::new();
        // Paths reached by resolving a symlink, whose row lives under a
        // parent that may be outside every root.
        let mut aliased_paths: HashSet<String> = HashSet::new();
        // Set by whichever of the two `break`s the loop leaves through, so a
        // run that was stopped is never mistaken for a completed one.
        let aborted;
        let mut stale_cleanup_ok = true;
        let mut cleanup_done = false;
        let mut rr = 0usize;
        // Log size at which to force a checkpoint, re-armed after every
        // attempt. See [`wal_len`] for why the run has to do this itself
        // rather than leave it to SQLite's autocheckpoint.
        let wal_path = format!("{}-wal", db_path);
        let wal_cap = match config.processing.maximum_wal_size {
            0 => 0,
            n => n.max(crate::config::MINIMUM_WAL_SIZE),
        };
        let mut checkpoint_at = wal_cap;

        // Round-robin with skipping: each round takes at most one quantum from
        // every root that has work ready. Write-bottlenecked, all active roots
        // get even quanta; read-bottlenecked, roots with empty channels are
        // skipped and the firehose roots get the writer's full attention.
        loop {
            if should_abort(stop_flag, suspend_flag) {
                aborted = true;
                break;
            }
            let mut progressed = false;
            let n = pipelines.len();
            for k in 0..n {
                let p = &mut pipelines[(rr + k) % n];
                match p.phase {
                    RootPhase::Walking => {
                        let mut took = 0usize;
                        while took < quantum {
                            match p.walk.try_next() {
                                TryNext::Item(WalkEvent::Stale(paths)) => {
                                    took += 1;
                                    // Applied at the end of the run, not here:
                                    // deleting mid-walk would break the "a
                                    // stopped run deletes nothing" guarantee,
                                    // and an aliased sighting that exempts a
                                    // path may still be ahead of us.
                                    stale_candidates.extend(paths);
                                }
                                TryNext::Item(WalkEvent::File(file)) => {
                                    took += 1;
                                    p.walked += 1;
                                    if p.walked.is_multiple_of(64) {
                                        p.current_file = Some(file.path.clone());
                                    }
                                    if file.aliased {
                                        // Its row's parent is a directory this
                                        // walk may never visit, so the
                                        // vanished-directory sweep must not
                                        // treat that parent's absence as proof
                                        // the file is gone.
                                        aliased_paths.insert(file.path.clone());
                                    }
                                    // Dedupes a canonical file reachable
                                    // through several spellings, or from more
                                    // than one root.
                                    if !seen_paths.insert(file.digest) {
                                        continue;
                                    }
                                    let Some(rec) = file.record else { continue };
                                    if file.action == FileIndexAction::Update {
                                        p.pending_updates.push(rec);
                                        if p.pending_updates.len() >= quantum {
                                            process_batch_updates(
                                                &conn_mutex,
                                                &p.pending_updates,
                                                stop_flag,
                                                config,
                                            )?;
                                            p.pending_updates.clear();
                                        }
                                    } else {
                                        p.pending_inserts.push(rec);
                                        if p.pending_inserts.len() >= quantum {
                                            process_batch_inserts(
                                                &conn_mutex,
                                                &p.pending_inserts,
                                                stop_flag,
                                                config,
                                            )?;
                                            p.pending_inserts.clear();
                                        }
                                    }
                                }
                                TryNext::Empty => break,
                                TryNext::Finished => {
                                    // Join before deciding anything about what
                                    // this walk saw; see `ParallelWalk::finish`.
                                    p.walk_clean = p.walk.finish();
                                    process_batch_updates(
                                        &conn_mutex,
                                        &p.pending_updates,
                                        stop_flag,
                                        config,
                                    )?;
                                    p.pending_updates.clear();
                                    process_batch_inserts(
                                        &conn_mutex,
                                        &p.pending_inserts,
                                        stop_flag,
                                        config,
                                    )?;
                                    p.pending_inserts.clear();

                                    let walk_time = p.phase_elapsed();
                                    crate::log_info!(
                                        "{}: walk {} — {} ({} workers)",
                                        p.root,
                                        if p.walk_clean { "done" } else { "ended early" },
                                        phase_summary(p.walked, "files", walk_time),
                                        p.workers
                                    );

                                    if !p.walk_clean {
                                        crate::log_warn!(
                                            "a walk worker for {} terminated abnormally; \
                                             skipping stale cleanup",
                                            p.root
                                        );
                                        stale_cleanup_ok = false;
                                        p.phase = RootPhase::Done;
                                    } else if stop_flag.load(Ordering::Relaxed) {
                                        p.phase = RootPhase::Done;
                                    } else {
                                        // A walk that saw the whole tree has the
                                        // next run's denominator in hand — and
                                        // is the reason that run spawns no
                                        // counting scan at all. Recorded only
                                        // when it really did see all of it: a
                                        // directory it could not read is a
                                        // subtree missing from `walked`, and
                                        // the figure is sticky, so one blip
                                        // would teach every later run to divide
                                        // by a number that is too small. The
                                        // stop and panic cases are already
                                        // excluded by the branches above; this
                                        // is the third way a walk comes back
                                        // short. `unreadable()` is final here —
                                        // the workers have exited and
                                        // `finish()` joined them.
                                        if p.walk.unreadable().is_empty() {
                                            let conn = conn_mutex.lock().unwrap();
                                            if let Err(e) = crate::db::repo::set_root_walk_count(
                                                &conn, &p.root, p.walked,
                                            ) {
                                                crate::log_warn!("{}", e);
                                            }
                                        }
                                        let cursor = ExtractCursor::for_root(&p.root);
                                        let scope =
                                            extract_scope_prepare(&conn_mutex, &cursor, config)?;
                                        // Progress counts the root's whole
                                        // searchable set: files extracted in
                                        // earlier runs start the counter, so
                                        // an unchanged root shows "X of X"
                                        // rather than "0 of 0".
                                        p.extract_total = scope.pending + scope.already_done;
                                        p.extracted = scope.already_done;
                                        if scope.pending == 0 {
                                            p.phase = RootPhase::Done;
                                        } else {
                                            // Starts only now: the rows have to
                                            // exist before the feeder can page
                                            // over them.
                                            p.content = Some(crate::content::extract_content(
                                                db_path,
                                                &cursor,
                                                registry.clone(),
                                                config.clone(),
                                                stop_flag.clone(),
                                                suspend_flag.clone(),
                                                p.workers,
                                            ));
                                            p.phase = RootPhase::Extracting;
                                        }
                                    }
                                    progressed = true;
                                    break;
                                }
                            }
                        }
                        progressed |= took > 0;
                    }
                    RootPhase::Extracting => {
                        // The same shape as the walking arm: drain up to a
                        // quantum of finished work, then write it. Reading and
                        // extracting happen on this root's own pool, so a slow
                        // root occupies the writer only for as long as its
                        // commits take.
                        let pass = p.content.as_mut().expect("extracting root has a pass");
                        let mut batch: Vec<crate::content::ExtractedRow> = Vec::new();
                        let mut finished = false;
                        while batch.len() < quantum {
                            match pass.try_next() {
                                TryNext::Item(row) => batch.push(row),
                                TryNext::Empty => break,
                                TryNext::Finished => {
                                    finished = true;
                                    break;
                                }
                            }
                        }
                        if let Some(row) = batch.last() {
                            p.current_file = Some(row.name.clone());
                        }
                        let took = batch.len();
                        p.extracted += store_extracted(&conn_mutex, &batch, stop_flag, config)?;
                        if finished {
                            // Join before deciding; see `ParallelWalk::finish`.
                            if !pass.finish() {
                                crate::log_warn!(
                                    "a content worker for {} terminated abnormally",
                                    p.root
                                );
                            }
                            p.content = None;
                            p.phase = RootPhase::Done;
                            let extract_time = p.phase_elapsed();
                            crate::log_info!(
                                "{}: content done — {}",
                                p.root,
                                phase_summary(p.extracted, "files with text", extract_time)
                            );
                        }
                        progressed |= finished || took > 0;
                    }
                    RootPhase::Done => {}
                }
            }
            rr = rr.wrapping_add(1);

            // Once every walk has ended, reconcile deletions — globally,
            // because a file may be reachable through more than one root's
            // symlinks. Runs at most once per run, on this writer thread.
            if !cleanup_done && pipelines.iter().all(|p| p.phase != RootPhase::Walking) {
                cleanup_done = true;
                let stopped = stop_flag.load(Ordering::Relaxed);
                if stale_cleanup_ok && !stopped {
                    // Directories that vanished entirely are never read, so
                    // per-directory reconciliation never sees them; only a
                    // scan of stored parents against the ones the walk
                    // reached can find the rows beneath them.
                    for p in &pipelines {
                        sweep_unvisited_parents(
                            &conn_mutex,
                            &p.root,
                            &p.walk.seen_dirs(),
                            p.walk.unreadable(),
                            &aliased_paths,
                            &mut stale_candidates,
                        )?;
                    }
                    // No unreadable-directory filter here: neither source of
                    // candidates can produce one. `read_directory` returns
                    // before reconciling a directory it could not read, and
                    // the sweep skips parents beneath one. Re-checking here
                    // as well would put the same rule in two places, free to
                    // drift, and the tests could not tell which one held.
                    //
                    // The aliased filter *is* applied to both, though. The
                    // sweep does its own, but per-directory reconciliation can
                    // produce an aliased path too: a symlink target that lives
                    // in a walked directory but is itself hidden or
                    // ignore-matched is skipped before `present.insert`, so it
                    // reads as stale — while the alias route inserted it moments
                    // earlier. Without this the row is written and deleted on
                    // every single run. One filter, in the one place the
                    // deletions are assembled.
                    let stale_paths: Vec<String> = stale_candidates
                        .drain(..)
                        .filter(|p| !aliased_paths.contains(p))
                        .collect();
                    let unreadable_count: usize = pipelines
                        .iter()
                        .map(|p| p.walk.unreadable().paths().len())
                        .sum();
                    if unreadable_count > 0 {
                        crate::log_warn!(
                            "{} director{} could not be read; index entries beneath \
                             them were kept rather than deleted",
                            unreadable_count,
                            if unreadable_count == 1 { "y" } else { "ies" }
                        );
                    }
                    // One line for the whole run, not one per entry — see
                    // `walk::PruneCounts`. Reported only on a complete run,
                    // because a stopped one pruned an arbitrary partial set and
                    // the number would mean nothing.
                    for p in &pipelines {
                        if let Some(summary) = p.walk.pruned().summary() {
                            crate::log_info!("{}: {}", p.root, summary);
                        }
                    }
                    if !stale_paths.is_empty() {
                        if let Some(first) = pipelines.first_mut() {
                            first.current_file = Some("Removing stale index entries…".to_string());
                        }
                        let started = Instant::now();
                        let stale_deleted = cleanup_stale_index_entries(
                            &conn_mutex,
                            stale_paths.as_slice(),
                            stop_flag,
                            suspend_flag,
                            config,
                        )?;
                        crate::log_info!(
                            "stale cleanup — {}",
                            phase_summary(
                                stale_deleted,
                                "index entries removed",
                                started.elapsed()
                            )
                        );
                    }
                }
                progressed = true;
            }

            publish(&pipelines);

            // After `publish`, so the GUI's last snapshot is fresh going into
            // a checkpoint that may block for `busy_timeout`.
            //
            // `progressed` is the cheap half of the test: this thread is the
            // only writer during a run, so a round that wrote nothing cannot
            // have grown the log, and without the gate this would stat the
            // file at 500 Hz through the idle backoff below. The stop flag is
            // the other half — `stop_indexing` wants this same connection, and
            // a checkpoint starting as the user hits Stop would sit in front
            // of it for five seconds.
            if wal_cap > 0
                && progressed
                && !stop_flag.load(Ordering::Relaxed)
                && wal_len(&wal_path) >= checkpoint_at
            {
                {
                    let conn = conn_mutex.lock().unwrap();
                    if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                        crate::log_warn!("{}", e);
                    }
                }
                // Re-armed from what is actually on disk, not from zero: a
                // checkpoint that lost the race to a running search then costs
                // one attempt per further `wal_cap` of growth instead of
                // retrying into every round for the rest of the run.
                checkpoint_at = wal_len(&wal_path) + wal_cap;
            }

            if pipelines.iter().all(|p| p.phase == RootPhase::Done) {
                // A stop can land *inside* the pass above — `TryNext::Finished`
                // and a drained content pass both mark a root Done — so
                // every root can reach Done without the top-of-loop check ever
                // seeing the flag. Re-read it here or a cut-short run would be
                // stamped as a completed full index and suppress the next
                // periodic reindex for the whole interval.
                aborted = stop_flag.load(Ordering::Relaxed);
                break;
            }
            if !progressed {
                // Park on a walking root's channel rather than sleeping a fixed
                // interval: a sender wakes this immediately, and on Windows a
                // 2 ms `thread::sleep` really stalls for the 15.6 ms timer tick.
                // `wait_ready` holds whatever it pulls, so the round-robin below
                // still sees it in order.
                let waited = pipelines
                    .iter_mut()
                    .find(|p| p.phase == RootPhase::Walking)
                    .map(|p| p.walk.wait_ready(IDLE_BACKOFF))
                    .is_some();
                // Nothing is walking — the roots left are extracting, whose
                // passes have no equivalent handle here. Fall back to the sleep.
                if !waited {
                    thread::sleep(IDLE_BACKOFF);
                }
            }
        }

        if aborted {
            // Buffered records are valid work — land them before leaving.
            for p in &mut pipelines {
                process_batch_updates(&conn_mutex, &p.pending_updates, stop_flag, config)?;
                p.pending_updates.clear();
                process_batch_inserts(&conn_mutex, &p.pending_inserts, stop_flag, config)?;
                p.pending_inserts.clear();
            }
            // Deliberately no stale cleanup: a partial walk has a partial
            // seen set, and deleting everything it did not reach would
            // empty most of the index.
            report_run_warnings();
            crate::log_info!(
                "indexing stopped after {:.1}s",
                run_started.elapsed().as_secs_f64()
            );
            // The final status is the caller's to publish: a stopped run is
            // still followed by an optimize pass, so this is not yet Idle.
            return Ok(());
        }

        report_run_warnings();
        crate::log_info!(
            "indexing complete in {:.1}s",
            run_started.elapsed().as_secs_f64()
        );

        // FTS housekeeping once per completed run (cheap if nothing changed).
        {
            let conn = conn_mutex.lock().unwrap();
            fts_finalize_after_text_indexing(&conn);
        }

        // Stamp the successful run so the coordinator can schedule the next
        // periodic reindex from it. Losing this stamp is not cosmetic: an
        // absent one reads as "never indexed", which `periodic_due` answers
        // by starting another full run on the very next tick, and then again
        // on the one after that.
        let now = crate::log::now_unix();
        match conn_mutex.lock() {
            Ok(conn) => {
                if let Err(e) = crate::db::repo::set_last_full_index(&conn, now) {
                    crate::log_warn!("{}", e);
                }
            }
            Err(e) => crate::log_warn!("last-full-index stamp skipped: {}", e),
        }

        Ok(())
    }

    /// The settings the index was built under, paired with their current
    /// values. One list drives [`validate_config`], [`update_config`] and
    /// [`crate::scope::stored_config`], so the record, the comparison and the
    /// reconstruction can never drift apart.
    ///
    /// Every list value is sorted before joining, and the roots arrive already
    /// canonicalized: the record describes what the walk *did*, and reordering
    /// or re-spelling a list does not change that. `stored_config` parses these
    /// back, so a key added here becomes a key a hand-edited config can be
    /// reconciled against.
    fn config_validation_entries(config: &Config, roots: &[String]) -> Vec<(&'static str, String)> {
        let sorted_joined = |v: &[String]| {
            let mut v: Vec<String> = v.to_vec();
            v.sort();
            v.join("\n")
        };
        vec![
            ("hash_length", config.processing.hash_length.to_string()),
            // Hashes are only comparable within one algorithm, and
            // duplicate detection groups purely by hash. Bump this string
            // whenever the digest input changes so existing indexes are
            // offered a rebuild instead of silently mixing schemes.
            ("hash_algorithm", "size+head".to_string()),
            ("indexing_path", sorted_joined(roots)),
            ("tokenize", config.processing.tokenize.clone()),
            ("include_hidden", config.indexing.include_hidden.to_string()),
            // Decides whether symlink targets are in the index at all, so a
            // change leaves rows that no longer belong.
            (
                "follow_symlinks",
                config.indexing.follow_symlinks.to_string(),
            ),
            (
                "ignore_patterns",
                sorted_joined(&config.indexing.ignore_patterns),
            ),
            (
                "content_extensions",
                sorted_joined(&config.indexing.content_extensions),
            ),
            (
                "store_text_for_snippets",
                config.processing.store_text_for_snippets.to_string(),
            ),
        ]
    }

    /// Every recorded `config_validation` key, for
    /// [`crate::scope::stored_config`] to rebuild the configuration the index
    /// was last written under.
    pub(crate) fn stored_validation(conn: &Connection) -> Result<Vec<(String, String)>, String> {
        let mut stmt = conn
            .prepare("SELECT key, value FROM config_validation")
            .map_err(|e| format!("prepare config_validation read: {}", e))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| format!("read config_validation: {}", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read config_validation row: {}", e))
    }

    /// The recorded settings a difference in which cannot be reconciled: the
    /// FTS tokenizer is part of the table definition, and a hash written under
    /// a different length or algorithm cannot be compared with a new one.
    /// Everything else `config_validation_entries` records is recoverable —
    /// see [`crate::scope`] — and naming it in a rebuild prompt would be
    /// asking the user to pay for a wipe that is not needed.
    const REBUILD_KEYS: [&'static str; 3] = ["hash_length", "hash_algorithm", "tokenize"];

    /// The settings the index was built under that no longer match and cannot
    /// be reconciled — the list a rebuild prompt shows. `None` when there are
    /// none. A key absent from the DB (older index) never counts as changed.
    fn validate_config(
        conn: &Connection,
        config: &Config,
        roots: &[String],
    ) -> Result<Option<Vec<ConfigChange>>, String> {
        let mut changes = Vec::new();
        for (key, current) in Self::config_validation_entries(config, roots)
            .into_iter()
            .filter(|(key, _)| Self::REBUILD_KEYS.contains(key))
        {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT value FROM config_validation WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("read config_validation.{}: {}", key, e))?;
            if let Some(stored) = stored {
                if stored != current {
                    changes.push(ConfigChange {
                        key: key.to_string(),
                        stored,
                        current,
                    });
                }
            }
        }
        Ok(if changes.is_empty() {
            None
        } else {
            Some(changes)
        })
    }

    /// Bring the index into line with `config` before a run walks anything.
    ///
    /// A no-op in the normal case: the coordinator already reconciled when the
    /// config was edited, so the stored record matches and the plan is empty.
    /// It earns its place for configs changed while the app was not running,
    /// where nothing else ever compares the two.
    ///
    /// Runs to completion rather than under a deadline — the run owns the
    /// database until it finishes anyway, and a caller waiting on the walk is
    /// not a caller that would rather have a half-reconciled index. It still
    /// slices, so the status moves and the stop flag is read between
    /// statements; `interrupt` is what reaches the statement already running.
    ///
    /// Returns whether it finished. On a multi-million-row index this is the
    /// longest thing that can happen before a single file is walked, so it
    /// publishes its cursor after every slice and says up front that it has
    /// started; `false` means it was cut short and nothing may be recorded as
    /// reconciled.
    fn reconcile_stored_config(
        status: &Arc<Mutex<IndexingStatus>>,
        interrupt: &db::InterruptSlot,
        conn: &mut Connection,
        config: &Config,
        roots: &[String],
        stop_flag: &Arc<AtomicBool>,
    ) -> Result<bool, String> {
        // What this run is about to index, which is `config` everywhere except
        // its roots — see the caller.
        let mut current = config.clone();
        current.paths.indexing_paths = roots.to_vec();

        let stored = crate::scope::stored_config(conn, &current)?;
        let work = crate::config::diff_actions(&stored, &current).work;
        if !work.touches_index() {
            return Ok(true);
        }
        // Ahead of the scan, not after it: the line that used to report this
        // only appeared once the work was already done, which is no help at
        // all to someone watching a run that has not moved for ten minutes.
        crate::log_info!(
            "configuration changed since the last run: reconciling the index ({})",
            work.summary()
        );
        let registry = Registry::default_set();
        let mut cursor = crate::scope::WorkCursor::new(work, &current)?;
        while !cursor.done() {
            if stop_flag.load(Ordering::Relaxed) {
                crate::log_info!(
                    "configuration reconcile interrupted after {} index entries; \
                     the next run starts it again",
                    cursor.progress().examined
                );
                return Ok(false);
            }
            let outcome = {
                // Armed per slice, so the handle names a statement this scan
                // is actually running and nothing else. `advance` reads the
                // same stop flag before each statement; this reaches the one
                // already in flight.
                let _armed = db::InterruptGuard::arm(interrupt, conn);
                crate::scope::advance(
                    conn,
                    &current,
                    &registry,
                    &mut cursor,
                    Instant::now() + crate::scope::SLICE,
                    stop_flag,
                )
            };
            if let Err(e) = outcome {
                // A statement cut short by the interrupt fails like any other
                // and cannot be told apart from a real failure by its message.
                // The flag we set ourselves is the answer: a stopped run is
                // not an error, it just records nothing.
                if stop_flag.load(Ordering::Relaxed) {
                    crate::log_info!(
                        "configuration reconcile interrupted after {} index entries; \
                         the next run starts it again",
                        cursor.progress().examined
                    );
                    return Ok(false);
                }
                return Err(e);
            }
            // The slice budget is already the GUI's repaint cadence, so this
            // needs no pacing of its own.
            Self::set_prep_step(status, PrepStep::Reconciling(cursor.progress()));
        }
        if cursor.deleted > 0 || cursor.recontented > 0 {
            crate::log_info!(
                "configuration changed since the last run: {} index entries removed, \
                 {} re-examined for text extraction",
                cursor.deleted,
                cursor.recontented
            );
        }
        Ok(true)
    }

    /// Stamp the index with the settings it's being built under.
    fn update_config(conn: &Connection, config: &Config, roots: &[String]) -> Result<(), String> {
        Self::stamp(conn, Self::config_validation_entries(config, roots))
    }

    /// Record the settings a reconciliation has just brought the index into
    /// line with.
    ///
    /// Everything except [`Self::REBUILD_KEYS`], which a scan cannot satisfy:
    /// a hash written under a different length, or an FTS table built with a
    /// different tokenizer, needs the wipe the user was prompted for. Stamping
    /// those here would clear a rebuild prompt the user declined, and it would
    /// be cleared by an unrelated later edit at that.
    ///
    /// Without this the record only ever moved at the end of a full run, so a
    /// prune applied between runs left the index describing itself with the
    /// *old* configuration — and every subsequent run re-derived the same plan
    /// and rescanned every row under every root to re-apply work that was
    /// already done.
    pub(crate) fn stamp_reconciled(
        conn: &Connection,
        config: &Config,
        roots: &[String],
    ) -> Result<(), String> {
        Self::stamp(
            conn,
            Self::config_validation_entries(config, roots)
                .into_iter()
                .filter(|(key, _)| !Self::REBUILD_KEYS.contains(key)),
        )
    }

    fn stamp(
        conn: &Connection,
        entries: impl IntoIterator<Item = (&'static str, String)>,
    ) -> Result<(), String> {
        for (key, current) in entries {
            conn.execute(
                "INSERT OR REPLACE INTO config_validation (key, value) VALUES (?1, ?2)",
                params![key, current],
            )
            .map_err(|e| format!("store config_validation.{}: {}", key, e))?;
        }
        Ok(())
    }
}

impl Drop for IndexingService {
    fn drop(&mut self) {
        let _ = self.stop_indexing();
    }
}

impl Default for IndexingService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The progress line reports whichever pool the root is currently
    /// running. Reading the walk's stats in every phase made an extracting
    /// root report "0 busy" out of a walk pool that had already exited, and
    /// let finished roots keep inflating the total.
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

    /// A walk that could not read part of its tree must not record its count.
    ///
    /// The figure is the next run's progress denominator, and nothing ever
    /// re-derives it: a run that missed a subtree because of a permissions
    /// blip or an unplugged share would leave every later run dividing by a
    /// number that is too small, with the bar saturating early and staying
    /// there. Both halves are pinned, because a guard that simply never
    /// recorded anything would also pass the negative case.
    ///
    /// Unix only: `deny_read` needs the whole run to actually be refused, and
    /// on Windows the `icacls` deny ACE does not bind the process that owns
    /// the directory reliably enough to test against.
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
    /// alone.
    ///
    /// Stamping it would tell every later run the index already matches the
    /// new configuration, and nothing would ever revisit the rows the scan had
    /// not reached — a walk cannot find them, because it never descends into a
    /// directory the new rules prune. The stale record is the only thing that
    /// makes the work resumable at all.
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
        // deterministic stand-in for a shutdown part-way through a scan of
        // millions of rows.
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

    /// The bug this whole rule exists for: `find` counts tree entries —
    /// directories, hidden files, ignore-pruned subtrees — where `walked`
    /// counts only walkable files, so the estimate reads far high. Once the
    /// walk ends the exact number is in hand and the estimate must go.
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
}
