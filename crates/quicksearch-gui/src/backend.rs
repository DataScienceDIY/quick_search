//! Wiring between the egui thread and the core services.
//!
//! All communication is non-blocking from the UI's point of view: searches
//! stream over an mpsc receiver drained each frame, indexing state is
//! polled, and the duplicates query runs on a throwaway worker thread.
//! Every core thread wakes the UI through `ctx.request_repaint()`, which is
//! what makes polling enough.
//!
//! The duplicates scan and the byte-for-byte verification of one of its
//! groups are the throwaway threads, and both fire on a user action rather
//! than a timer: a thread per refresh opens its own connection — a page cache
//! and an allocator arena glibc never gives back. The verification opens no
//! connection at all, but it can hold a large group's worth of file handles,
//! so it carries a cancel flag and shutdown raises it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::IndexCoordinator;
use quicksearch_core::live::{LiveUpdate, LiveWatcher};
use quicksearch_core::search::{DuplicateGroup, SearchService, SearchUpdate};
use quicksearch_core::shutdown;
use quicksearch_core::verify::{verify_identical, VerifyUpdate};

/// A duplicate group being read through. The thread is detached and owns
/// nothing the app needs back, so cancelling is just raising the flag: the
/// worker notices between chunks and drops the receiver's other end.
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
    /// Watches the results currently on screen; see [`quicksearch_core::live`].
    /// `None` only after [`Backend::shutdown`].
    pub live: Option<LiveWatcher>,
    pub live_rx: mpsc::Receiver<LiveUpdate>,
}

impl Backend {
    /// Ask the coordinator to rebuild, after letting go of everything that
    /// holds the index file open.
    ///
    /// The search worker keeps its connection for half a minute after the last
    /// keystroke so a typing session runs against a warm cache — which is
    /// exactly the wrong thing to be holding when the file is about to be
    /// deleted. Without this the delete fails on Windows and the rebuild
    /// silently becomes an ordinary run against the old index; after a
    /// password change that leaves the config claiming protection the file on
    /// disk does not have.
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
        // It calls this on the edge into work, not on a cadence.
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

    /// Point the live watcher at the rows currently on screen, or clear it
    /// with an empty `targets`.
    pub fn watch_live(
        &self,
        query: &str,
        mut targets: Vec<quicksearch_core::live::Target>,
        config: &Config,
    ) {
        let Some(live) = &self.live else { return };
        // The watcher re-reads a row's file to re-cut its snippet, and the one
        // file it must never open is the index it is reading the row from:
        // closing a descriptor on it cancels SQLite's locks process-wide. The
        // walk no longer writes such rows, but one from an older build lives
        // until the stale sweep reaches its directory, and it can be on screen
        // before then.
        targets.retain(|t| !config.is_index_file(std::path::Path::new(&t.path)));
        if targets.is_empty() {
            live.clear();
        } else {
            live.watch(query, targets, config);
        }
    }

    /// Ask the coordinator to bring the index in line with these paths — the
    /// files the live watcher has just read from disk on the frontend's
    /// behalf, so the index does not drift from what is on screen.
    pub fn reindex_live_paths(&self, paths: Vec<PathBuf>) {
        self.coordinator.update_paths(paths);
    }

    pub fn clear_live(&self) {
        if let Some(live) = &self.live {
            live.clear();
        }
    }

    /// `None` only after [`Backend::shutdown`], i.e. during teardown frames.
    pub fn search(&self) -> Option<&SearchService> {
        self.search.as_ref()
    }

    /// Kick off (or restart) the duplicates listing on a worker thread.
    pub fn start_duplicates(&mut self, config: &Config, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        let db = config.resolved_database_path();
        std::thread::spawn(move || {
            let result =
                quicksearch_core::search::find_duplicate_groups(&db.to_string_lossy(), 500, 0);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.dup_job = Some(rx);
    }

    /// Read a duplicate group through on a worker thread, comparing every
    /// member against the first byte for byte. Replaces any run already going.
    pub fn start_verify(&mut self, mut paths: Vec<PathBuf>, config: &Config, ctx: egui::Context) {
        if let Some(job) = &self.verify_job {
            job.cancel();
        }
        // Byte-for-byte comparison opens every member. See `watch_live` for
        // why the index must not be one of them.
        paths.retain(|p| !config.is_index_file(p));
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            verify_identical(&paths, &worker_cancel, &mut |update| {
                // A closed receiver means the app moved on; the cancel flag
                // is what stops the work, so there is nothing to do here.
                let _ = tx.send(update);
                ctx.request_repaint();
            });
        });
        self.verify_job = Some(VerifyJob { rx, cancel });
    }

    /// Stop a verification and forget it. The worker sees the flag between
    /// chunks and exits on its own.
    pub fn cancel_verify(&mut self) {
        if let Some(job) = self.verify_job.take() {
            job.cancel();
        }
    }

    /// Join the search worker and stop the coordinator. Called once from
    /// `on_exit`.
    pub fn shutdown(&mut self) {
        // Detached and holding open file handles: the flag is what makes a
        // verification of a slow, large group let go on the way out.
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
