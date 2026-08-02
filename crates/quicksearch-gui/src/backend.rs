//! Wiring between the egui thread and the core services.
//!
//! All communication is non-blocking from the UI's point of view:
//! searches stream over an mpsc receiver drained each frame, indexing
//! state is polled, and the duplicates query runs on a throwaway worker
//! thread. Core threads wake the UI through `ctx.request_repaint()`.

use std::sync::{mpsc, Arc};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::IndexCoordinator;
use quicksearch_core::search::{DuplicateGroup, SearchService, SearchUpdate};
use quicksearch_core::shutdown;

pub struct Backend {
    pub coordinator: Arc<IndexCoordinator>,
    pub search: Option<SearchService>,
    pub search_rx: mpsc::Receiver<SearchUpdate>,
    pub dup_job: Option<mpsc::Receiver<Result<Vec<DuplicateGroup>, String>>>,
}

impl Backend {
    pub fn start(config: &Config, ctx: egui::Context) -> Result<Backend, String> {
        let coordinator = Arc::new(IndexCoordinator::start(config.clone())?);
        if let Err(e) = shutdown::install_signal_handler(coordinator.clone()) {
            quicksearch_core::log_warn!("failed to install signal handler: {}", e);
        }

        let repaint_ctx = ctx.clone();
        let (search, search_rx) = SearchService::new(
            config.resolved_database_path(),
            Arc::new(move || repaint_ctx.request_repaint()),
        );

        Ok(Backend {
            coordinator,
            search: Some(search),
            search_rx,
            dup_job: None,
        })
    }

    pub fn search(&self) -> &SearchService {
        self.search.as_ref().expect("search service alive")
    }

    /// Kick off (or restart) the duplicates listing on a worker thread.
    pub fn start_duplicates(&mut self, config: &Config, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        let db = config.resolved_database_path();
        std::thread::spawn(move || {
            let result = quicksearch_core::search::find_duplicate_groups(
                &db.to_string_lossy(),
                500,
                0,
            );
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.dup_job = Some(rx);
    }

    /// Join the search worker and stop the coordinator. Called once from
    /// `on_exit`.
    pub fn shutdown(&mut self) {
        if let Some(search) = self.search.take() {
            search.shutdown();
        }
        self.coordinator.shutdown();
    }
}
