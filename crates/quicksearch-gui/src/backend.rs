//! Wiring between the egui thread and the core services. All communication
//! is non-blocking from the UI's point of view; every core thread wakes the
//! UI through `ctx.request_repaint()`, which is what makes polling enough.
//!
//! The duplicates scan and the verification run on throwaway threads fired
//! by user actions. The verification can hold a large group's worth of file
//! handles, so it carries a cancel flag and shutdown raises it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::IndexCoordinator;
use quicksearch_core::live::{LiveUpdate, LiveWatcher};
use quicksearch_core::search::{DuplicateGroup, SearchService, SearchUpdate};
use quicksearch_core::shutdown;
use quicksearch_core::verify::{verify_identical, VerifyUpdate};

/// A duplicate group being read through. The thread is detached; cancelling
/// is just raising the flag, noticed between chunks.
pub struct VerifyJob {
    pub rx: mpsc::Receiver<VerifyUpdate>,
    cancel: Arc<AtomicBool>,
}

impl VerifyJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct Backend {
    pub coordinator: Arc<IndexCoordinator>,
    pub search: Option<SearchService>,
    pub search_rx: mpsc::Receiver<SearchUpdate>,
    pub dup_job: Option<mpsc::Receiver<Result<Vec<DuplicateGroup>, String>>>,
    pub verify_job: Option<VerifyJob>,
    /// Watches the on-screen results; `None` only after [`Backend::shutdown`].
    pub live: Option<LiveWatcher>,
    pub live_rx: mpsc::Receiver<LiveUpdate>,
}

impl Backend {
    /// Rebuild, after letting go of everything holding the index open: the
    /// search worker keeps its connection warm for half a minute, and
    /// without the release the delete fails on Windows and the rebuild
    /// silently becomes an ordinary run against the old index.
    pub fn rebuild_index(&self) {
        if let Some(search) = &self.search {
            search.release_connection();
        }
        self.coordinator.rebuild_index();
    }

    /// [`Backend::rebuild_index`]'s reasoning, for the delete-only path.
    pub fn clear_index(&self) {
        if let Some(search) = &self.search {
            search.release_connection();
        }
        self.coordinator.clear_index();
    }

    pub fn start(config: &Config, ctx: egui::Context) -> Result<Backend, String> {
        // eframe is reactive: a run the coordinator schedules on its own
        // would sit unseen behind a settled window until the pointer moved.
        let coord_ctx = ctx.clone();
        let coordinator = Arc::new(IndexCoordinator::start(
            config.clone(),
            Arc::new(move || coord_ctx.request_repaint()),
        )?);
        if let Err(e) = shutdown::install_signal_handler(coordinator.clone()) {
            quicksearch_core::log_warn!("failed to install signal handler: {}", e);
        }

        let repaint_ctx = ctx.clone();
        let (search, search_rx) = SearchService::new(
            config.resolved_database_path(),
            Arc::new(move || repaint_ctx.request_repaint()),
        );

        let live_ctx = ctx.clone();
        let (live, live_rx) = LiveWatcher::start(Arc::new(move || live_ctx.request_repaint()));

        Ok(Backend {
            coordinator,
            search: Some(search),
            search_rx,
            dup_job: None,
            verify_job: None,
            live: Some(live),
            live_rx,
        })
    }

    /// Point the live watcher at the on-screen rows; empty `targets` clears.
    pub fn watch_live(
        &self,
        query: &str,
        mut targets: Vec<quicksearch_core::live::Target>,
        config: &Config,
    ) {
        let Some(live) = &self.live else { return };
        // The one file the watcher must never open is the index itself:
        // closing a descriptor on it cancels SQLite's locks process-wide. A
        // row for it from an older build can still be on screen.
        targets.retain(|t| !config.is_index_file(std::path::Path::new(&t.path)));
        if targets.is_empty() {
            live.clear();
        } else {
            live.watch(query, targets, config);
        }
    }

    /// Bring the index in line with paths the live watcher just read, so it
    /// does not drift from what is on screen.
    pub fn reindex_live_paths(&self, paths: Vec<PathBuf>) {
        self.coordinator.update_paths(paths);
    }

    pub fn clear_live(&self) {
        if let Some(live) = &self.live {
            live.clear();
        }
    }

    /// `None` only after [`Backend::shutdown`].
    pub fn search(&self) -> Option<&SearchService> {
        self.search.as_ref()
    }

    /// Ignored while a scan is already running: a second one would re-read the
    /// whole hash index for an answer the first is about to produce.
    pub fn start_duplicates(&mut self, config: &Config, ctx: egui::Context) {
        if self.dup_job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let db = config.resolved_database_path();
        std::thread::spawn(move || {
            let result = quicksearch_core::search::find_duplicate_groups(&db.to_string_lossy(), 500);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.dup_job = Some(rx);
    }

    /// Compare every member against the first, byte for byte, on a worker
    /// thread. Replaces any run already going.
    pub fn start_verify(&mut self, mut paths: Vec<PathBuf>, config: &Config, ctx: egui::Context) {
        if let Some(job) = &self.verify_job {
            job.cancel();
        }
        // See `watch_live` for why the index must not be opened.
        paths.retain(|p| !config.is_index_file(p));
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            verify_identical(&paths, &worker_cancel, &mut |update| {
                // A closed receiver means the app moved on; the cancel flag
                // is what stops the work.
                let _ = tx.send(update);
                ctx.request_repaint();
            });
        });
        self.verify_job = Some(VerifyJob { rx, cancel });
    }

    pub fn cancel_verify(&mut self) {
        if let Some(job) = self.verify_job.take() {
            job.cancel();
        }
    }

    pub fn shutdown(&mut self) {
        self.cancel_verify();
        if let Some(search) = self.search.take() {
            search.shutdown();
        }
        if let Some(mut live) = self.live.take() {
            live.stop();
        }
        self.coordinator.shutdown();
    }
}
