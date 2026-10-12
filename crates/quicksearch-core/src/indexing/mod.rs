//! The indexing service: one writer loop fed by per-root
//! walk/extract pipelines, plus the command thread that owns it.

use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db;

mod config_check;
mod pipeline;
mod progress;
#[cfg(test)]
mod tests;

pub use progress::{
    overall_progress, MaintenanceStep, OverallProgress, ReconcileProgress, RootPhase, RootProgress,
};

/// How long [`IndexingService::delete_index_for_rebuild`] gives a run to stop
/// and the delete to go through. A run in maintenance stops at its next
/// interrupted statement; one still active at the deadline keeps the file.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What a run is doing before its first file is walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepStep {
    /// Waiting on the previous run's thread to wind down.
    PreviousRun,
    /// [`db::open_or_recreate`]: schema migration and WAL recovery.
    OpeningIndex,
    /// Re-testing stored rows against a configuration that changed since the last run.
    Reconciling(ReconcileProgress),
    /// The prologue's remaining database work, once any reconcile has ended:
    /// stamping the config, retrying failed files, reading the stored counts.
    Starting,
}

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    /// A run claimed but not yet at its walk. Holds the database exactly as
    /// `Running` does: every caller that defers to a run must defer to this too.
    Preparing {
        start_time: Instant,
        step: PrepStep,
    },
    Running {
        start_time: Instant,
        roots: Vec<RootProgress>,
        /// The index upkeep the writer is inside, if any. Run-wide, not
        /// per-root: while it is set no file is moving and every counter in
        /// `roots` is the last one published before the step began.
        maintenance: Option<MaintenanceStep>,
    },
    Stopping,
    /// Compacting and re-analysing the index after a run. Holds the database:
    /// the single-writer rule applies until this clears.
    Optimizing,
    Error(String),
}

/// One setting whose stored (index-build-time) value differs from the current
/// config. List values are newline-joined; display them as multi-line columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub key: String,
    pub stored: String,
    pub current: String,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum IndexingCommand {
    Start {
        paths: Vec<String>,
        db_path: String,
        config: Config,
    },
    /// Stop the run of this number ([`IndexingService::start_indexing`]
    /// counts them) and no other. Untagged, one sent while idle and handled
    /// once the next start had claimed `Preparing` left that run reading
    /// `Stopping`, beyond the reach of any later stop.
    Stop(u64),
}

pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
    /// The latest run's number, bumped by each claim under the `status`
    /// lock, which the stop handler holds while it compares.
    run: Arc<AtomicU64>,
    command_tx: mpsc::Sender<IndexingCommand>,
    db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    /// The long statement a run is inside, if any: the prologue's reconcile
    /// scan or the epilogue's VACUUM, never both, so one slot serves both.
    interrupt: Arc<db::InterruptSlot>,
    _handle: thread::JoinHandle<()>,
}

/// `indexing.root_workers` rekeyed from the spellings the user typed to the
/// canonical roots the indexer walks; entries no longer indexed are dropped.
fn resolved_root_workers(config: &Config) -> HashMap<String, usize> {
    config
        .indexing_roots()
        .filter_map(|(raw, root)| Some((root, *config.indexing.root_workers.get(raw)?)))
        .collect()
}

impl IndexingService {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(IndexingStatus::Idle));
        let run = Arc::new(AtomicU64::new(0));
        let (command_tx, command_rx) = mpsc::channel();
        let db_connection = Arc::new(Mutex::new(None));
        let interrupt: Arc<db::InterruptSlot> = Arc::new(db::InterruptSlot::default());

        let handle = {
            let (status, run, db_connection, interrupt) = (
                status.clone(),
                run.clone(),
                db_connection.clone(),
                interrupt.clone(),
            );
            thread::spawn(move || {
                Self::indexing_thread(status, run, command_rx, db_connection, interrupt);
            })
        };

        IndexingService {
            status,
            run,
            command_tx,
            db_connection,
            interrupt,
            _handle: handle,
        }
    }

    /// Cut short the long statement a run is inside: an
    /// [`IndexingStatus::Optimizing`] VACUUM or the prologue's reconcile scan.
    /// Stop does *not* reach the VACUUM; an interrupted statement rolls back.
    /// One call reaches only the statement running now:
    /// [`Self::delete_index_for_rebuild`] repeats it until the run is idle,
    /// for at most [`STOP_GRACE`].
    pub fn cancel_db_work(&self) {
        db::interrupt(&self.interrupt)
    }

    /// Start indexing one or more roots; duplicates collapse to one walk.
    /// Returns `Err` if a run is already in flight, `Optimizing` included:
    /// that run's thread still holds the database and writes `Idle` when done.
    ///
    /// The `Idle → Preparing` transition happens here, synchronously: the
    /// command thread cannot flip the status until it has joined the previous
    /// run's thread, and a caller polling for the flip could start writing to
    /// a database this run is about to reopen.
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
            let mut status = crate::lock_ok(&self.status);
            if !matches!(*status, IndexingStatus::Idle | IndexingStatus::Error(_)) {
                return Err("indexing is already running".into());
            }
            *status = IndexingStatus::Preparing {
                start_time: Instant::now(),
                step: PrepStep::PreviousRun,
            };
            self.run.fetch_add(1, Ordering::SeqCst);
        }
        self.command_tx
            .send(IndexingCommand::Start {
                paths,
                db_path,
                config,
            })
            .map_err(|e| {
                // The service is gone; don't leave the status stuck on a phantom run.
                *crate::lock_ok(&self.status) = IndexingStatus::Idle;
                format!("Failed to send start command: {}", e)
            })
    }

    /// Signal a running index pass to stop without waiting. The worker notices
    /// the flag between batches and WAL makes an unflushed exit safe. Reaches
    /// only the run under way now, if any: never one started after.
    pub fn request_stop(&self) {
        let _ = self.command_tx.send(self.stop_command());
    }

    /// A [`IndexingCommand::Stop`] for the latest run.
    fn stop_command(&self) -> IndexingCommand {
        IndexingCommand::Stop(self.run.load(Ordering::SeqCst))
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        self.command_tx
            .send(self.stop_command())
            .map_err(|e| format!("Failed to send stop command: {}", e))?;

        for _ in 0..50 {
            match self.get_status() {
                IndexingStatus::Stopping => break,
                // Already stopped; an error state counts as stopped.
                IndexingStatus::Idle | IndexingStatus::Error(_) => return Ok(()),
                _ => thread::sleep(Duration::from_millis(100)),
            }
        }

        if let Some(db_conn_arc) = crate::lock_ok(&self.db_connection).take() {
            let conn = crate::lock_ok(&db_conn_arc);
            if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                crate::log_warn!("{}", e);
            }
        }

        Ok(())
    }

    /// Publish a failure that happened *outside* a run, so the status bar
    /// shows it. Only over `Idle` or an earlier `Error`: over a live run's
    /// status it would read as idle to every caller that defers to the run,
    /// while the run still writes, and the run would overwrite it anyway.
    /// `false` if a run held the status and nothing was published.
    pub fn report_error(&self, message: String) -> bool {
        let mut status = crate::lock_ok(&self.status);
        if !matches!(*status, IndexingStatus::Idle | IndexingStatus::Error(_)) {
            return false;
        }
        *status = IndexingStatus::Error(message);
        true
    }

    /// Set the status by hand, for tests elsewhere in the crate: the state a
    /// run would leave, without the run.
    #[cfg(test)]
    pub(crate) fn force_status(&self, status: IndexingStatus) {
        *crate::lock_ok(&self.status) = status;
    }

    pub fn get_status(&self) -> IndexingStatus {
        crate::lock_ok(&self.status).clone()
    }

    /// Whether a run is under way, without cloning the per-root progress
    /// [`Self::get_status`] carries. For callers that poll every frame and only
    /// want the yes-or-no. `Error` is a run that *ended*, so it reads as idle.
    pub fn is_active(&self) -> bool {
        !matches!(
            *crate::lock_ok(&self.status),
            IndexingStatus::Idle | IndexingStatus::Error(_)
        )
    }

    /// A no-op once the status has left `Preparing`: a Stop that arrives
    /// mid-prologue owns the status and must not be clobbered.
    fn set_prep_step(status: &Arc<Mutex<IndexingStatus>>, step: PrepStep) {
        let mut g = crate::lock_ok(status);
        if let IndexingStatus::Preparing { start_time, .. } = *g {
            *g = IndexingStatus::Preparing { start_time, step };
        }
    }

    /// The instant this run was claimed; now for callers that bypass
    /// [`Self::start_indexing`].
    fn run_start(status: &Arc<Mutex<IndexingStatus>>) -> Instant {
        match *crate::lock_ok(status) {
            IndexingStatus::Preparing { start_time, .. }
            | IndexingStatus::Running { start_time, .. } => start_time,
            _ => Instant::now(),
        }
    }

    /// Check if configuration changes require index recreation. A pure *read*
    /// that never wipes; a missing or incompatible DB means nothing to validate.
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

    /// Stop indexing and delete the database file for a clean rebuild, within
    /// [`STOP_GRACE`]. Refused, with the reason and nothing deleted, while a
    /// run is still stopping at the deadline, or for a file that is not our
    /// index (see [`db::remove_index_files`]).
    pub fn delete_index_for_rebuild(&self, db_path: &str) -> Result<(), String> {
        self.delete_index_within(db_path, Instant::now() + STOP_GRACE)
    }

    /// [`Self::delete_index_for_rebuild`] against `deadline`, which covers the
    /// stop and the delete together.
    fn delete_index_within(&self, db_path: &str, deadline: Instant) -> Result<(), String> {
        self.request_stop();
        // An error state counts as stopped. Once idle the run has released
        // `db_connection` too: it empties the slot before it publishes
        // `Error`, `Optimizing` or `Idle`.
        while self.is_active() {
            // Every round, not once: maintenance can arm a checkpoint, CREATE
            // INDEX or VACUUM after the last cancel, the stop flag reaches
            // none of them, and each holds open the file about to be deleted.
            self.cancel_db_work();
            if Instant::now() >= deadline {
                // Unlinking now would leave the run writing into a deleted
                // inode.
                return Err("indexing is still stopping; try again".into());
            }
            thread::sleep(Duration::from_millis(100));
        }

        // The search worker's held connection is released by the removal
        // itself (`db::hold_index`). Another reader's, a running duplicates
        // scan's, fails the delete on Windows until it closes, so the delete
        // retries until the same deadline.
        db::open::remove_index_files_until(std::path::Path::new(db_path), deadline)
    }

    fn indexing_thread(
        status: Arc<Mutex<IndexingStatus>>,
        latest_run: Arc<AtomicU64>,
        command_rx: mpsc::Receiver<IndexingCommand>,
        db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
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
                    // `start_indexing` already claimed the status; nothing to re-check.
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    stop_flag.store(false, Ordering::Relaxed);

                    let (status, stop_flag, db_connection, interrupt) = (
                        status.clone(),
                        stop_flag.clone(),
                        db_connection.clone(),
                        interrupt.clone(),
                    );
                    indexing_handle = Some(thread::spawn(move || {
                        // The run's thread: it holds the run's only write
                        // connection; a panic in the run becomes `Error`.
                        crate::platform::set_background_priority();
                        Self::run_contained(
                            &status,
                            &db_connection,
                            || {
                                Self::run_indexing(
                                    &status,
                                    &paths,
                                    &db_path,
                                    &stop_flag,
                                    &config,
                                    &db_connection,
                                    &interrupt,
                                )
                            },
                            || Self::run_maintenance(&db_path, &interrupt),
                        );

                        // An idle point: returns what mimalloc holds of the
                        // run's freed peak; SQLite's freed caches belong to
                        // the system malloc.
                        crate::platform::release_free_heap();
                    }));
                }
                IndexingCommand::Stop(run) => {
                    let mut guard = crate::lock_ok(&status);
                    if run == latest_run.load(Ordering::SeqCst)
                        && matches!(
                            *guard,
                            IndexingStatus::Preparing { .. } | IndexingStatus::Running { .. }
                        )
                    {
                        *guard = IndexingStatus::Stopping;
                        stop_flag.store(true, Ordering::Relaxed);
                        // The flag alone only stops the *next* statement; the
                        // prologue's reconcile can be inside one for minutes.
                        db::interrupt(&interrupt);
                        drop(guard);
                        // Nor does it reach a sandbox child mid-parse. After
                        // the flag, so a worker whose kill mark is newer than
                        // this kill sees the flag instead
                        // (`extract::sandbox::KillMark`).
                        crate::extract::sandbox::kill_sandbox_children();
                    }
                }
            }
        }

        if let Some(handle) = indexing_handle {
            let _ = handle.join();
        }
    }

    /// A claimed run to its end: `run`, then, once it returns `Ok`,
    /// `maintain` under `Optimizing`, then the status the run leaves. A panic
    /// in `run` becomes `Error`, as any failed run does; one in `maintain` is
    /// logged and the status still ends at `Idle`. Unwinding past either
    /// would leave the status in flight for good, refusing every later start.
    fn run_contained(
        status: &Mutex<IndexingStatus>,
        db_connection: &Mutex<Option<Arc<Mutex<Connection>>>>,
        run: impl FnOnce() -> Result<(), String>,
        maintain: impl FnOnce(),
    ) {
        let result = crate::contained(run)
            .unwrap_or_else(|m| Err(format!("indexing stopped unexpectedly: {}", m)));

        // Released before maintenance, which opens its own connection.
        *crate::lock_ok(db_connection) = None;

        match result {
            Err(e) => *crate::lock_ok(status) = IndexingStatus::Error(e),
            // Stopped runs included: a cut-short run still leaves a log to land.
            Ok(()) => {
                *crate::lock_ok(status) = IndexingStatus::Optimizing;
                if let Err(m) = crate::contained(maintain) {
                    crate::log_warn!("optimize stopped unexpectedly: {}", m);
                }
                *crate::lock_ok(status) = IndexingStatus::Idle;
            }
        }
    }

    /// Optimize the index once a run ends, completed or stopped; best-effort.
    /// Opens its own writer on the indexer's profile, whose temporaries stay
    /// in memory; [`db::repo::maintain`] moves them to disk for VACUUM alone.
    /// Not cancelled by the stop flag: `interrupt` is the one way out (see
    /// [`Self::cancel_db_work`]).
    fn run_maintenance(db_path: &str, interrupt: &db::InterruptSlot) {
        let conn = match db::open_existing(db_path, true) {
            Ok(conn) => conn,
            Err(e) => {
                crate::log_warn!("optimize: {}", e);
                return;
            }
        };
        let armed = db::InterruptGuard::arm(interrupt, &conn);
        let outcome = crate::db::repo::maintain(&conn, db_path);
        drop(armed);
        match outcome {
            Ok(true) => crate::log_info!("optimized the index and reclaimed unused space"),
            Ok(false) => {}
            Err(e) => crate::log_warn!("optimize failed (non-fatal): {}", e),
        }
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
