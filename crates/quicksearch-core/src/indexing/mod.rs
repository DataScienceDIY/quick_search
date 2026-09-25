//! The indexing service: one writer loop fed by per-root
//! walk/extract pipelines, plus the command thread that owns it.

use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Instant;

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
    Stop,
}

pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
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
        let (command_tx, command_rx) = mpsc::channel();
        let db_connection = Arc::new(Mutex::new(None));
        let interrupt: Arc<db::InterruptSlot> = Arc::new(db::InterruptSlot::default());

        let handle = {
            let (status, db_connection, interrupt) =
                (status.clone(), db_connection.clone(), interrupt.clone());
            thread::spawn(move || {
                Self::indexing_thread(status, command_rx, db_connection, interrupt);
            })
        };

        IndexingService {
            status,
            command_tx,
            db_connection,
            interrupt,
            _handle: handle,
        }
    }

    /// Cut short the long statement a run is inside: an
    /// [`IndexingStatus::Optimizing`] VACUUM or the prologue's reconcile scan.
    /// Stop does *not* reach the VACUUM; an interrupted statement rolls back.
    pub fn cancel_db_work(&self) {
        db::interrupt(&self.interrupt)
    }

    /// Start indexing one or more roots; duplicates collapse to one walk.
    /// Returns `Err` if a run is already in flight.
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
                // The service is gone; don't leave the status stuck on a phantom run.
                *crate::lock_ok(&self.status) = IndexingStatus::Idle;
                format!("Failed to send start command: {}", e)
            })
    }

    /// Signal a running index pass to stop without waiting. The worker notices
    /// the flag between batches and WAL makes an unflushed exit safe.
    pub fn request_stop(&self) {
        let _ = self.command_tx.send(IndexingCommand::Stop);
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        self.stop_indexing_inner(true)
    }

    fn stop_indexing_inner(&self, checkpoint: bool) -> Result<(), String> {
        self.command_tx
            .send(IndexingCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))?;

        for _ in 0..50 {
            match self.get_status() {
                IndexingStatus::Stopping => break,
                // Already stopped; an error state counts as stopped.
                IndexingStatus::Idle | IndexingStatus::Error(_) => return Ok(()),
                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }

        if let Some(db_conn_arc) = crate::lock_ok(&self.db_connection).take() {
            if checkpoint {
                let conn = crate::lock_ok(&db_conn_arc);
                if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                    crate::log_warn!("{}", e);
                }
            }
        }

        Ok(())
    }

    /// Publish a failure that happened *outside* a run, so the status bar shows it.
    pub fn report_error(&self, message: String) {
        *crate::lock_ok(&self.status) = IndexingStatus::Error(message);
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

    /// Stop indexing and delete the database file for a clean rebuild.
    pub fn delete_index_for_rebuild(&self, db_path: &str) -> Result<(), String> {
        // No checkpoint: copying the WAL into a file about to be deleted is
        // pure cost.
        self.stop_indexing_inner(false)
            .map_err(|e| format!("Failed to stop indexing: {}", e))?;
        // The optimize pass holds open the file about to be deleted.
        self.cancel_db_work();

        // An error state counts as stopped.
        for _ in 0..50 {
            if !self.is_active() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Unconditionally, before the removal: the half-deleted case is where
        // a stale reader handle does damage. See [`db::bump_index_epoch`].
        db::bump_index_epoch();

        // Readers may still hold handles (the search worker keeps its for
        // `IDLE_RELEASE`), which the retrying delete waits out.
        db::remove_index_files(std::path::Path::new(db_path))
            .map_err(|e| format!("Failed to delete database file: {}", e))
    }

    fn indexing_thread(
        status: Arc<Mutex<IndexingStatus>>,
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
                        // The writer thread: every DB write and text extraction happens here.
                        crate::platform::set_background_priority();
                        let result = Self::run_indexing(
                            &status,
                            &paths,
                            &db_path,
                            &stop_flag,
                            &config,
                            &db_connection,
                            &interrupt,
                        );

                        // Released before maintenance: VACUUM needs its own connection.
                        *crate::lock_ok(&db_connection) = None;

                        match result {
                            Err(e) => *crate::lock_ok(&status) = IndexingStatus::Error(e),
                            // Stopped runs included: a cut-short run still leaves a log to land.
                            Ok(()) => {
                                *crate::lock_ok(&status) = IndexingStatus::Optimizing;
                                Self::run_maintenance(&db_path, &interrupt);
                                *crate::lock_ok(&status) = IndexingStatus::Idle;
                            }
                        }

                        // Under glibc a run's freed working set (hundreds of
                        // MB at peak) never returns to the OS on its own.
                        crate::platform::release_free_heap();
                    }));
                }
                IndexingCommand::Stop => {
                    let mut guard = crate::lock_ok(&status);
                    if matches!(
                        *guard,
                        IndexingStatus::Preparing { .. } | IndexingStatus::Running { .. }
                    ) {
                        *guard = IndexingStatus::Stopping;
                        stop_flag.store(true, Ordering::Relaxed);
                        // The flag alone only stops the *next* statement; the
                        // prologue's reconcile can be inside one for minutes.
                        db::interrupt(&interrupt);
                    }
                }
            }
        }

        if let Some(handle) = indexing_handle {
            let _ = handle.join();
        }
    }

    /// Optimize the index once a run ends, completed or stopped; best-effort.
    /// Runs on its own connection, since VACUUM on the indexer's would build
    /// the replacement index in RAM. Not cancelled by the stop flag:
    /// `interrupt` is the one way out (see [`Self::cancel_db_work`]).
    fn run_maintenance(db_path: &str, interrupt: &db::InterruptSlot) {
        let conn = match crate::db::open::open_maintenance(db_path) {
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
