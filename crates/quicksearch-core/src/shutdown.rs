//! Process-level shutdown helpers.
//!
//! Wires Ctrl-C (and on Unix, SIGTERM) to a graceful shutdown that flushes
//! the indexing DB and exits. Replaces the ad-hoc `ctrlc::set_handler` +
//! `OnceLock` dance the GUI used to carry. Call
//! [`install_signal_handler`] once from a binary's `main` with a cloned
//! [`IndexingService`] handle.

use std::sync::Arc;

use crate::indexing::IndexingService;

/// Install a Ctrl-C (and, where supported, SIGTERM) handler that calls
/// [`IndexingService::graceful_shutdown`] and then exits with status 0.
///
/// Returns an error only if a handler was already installed elsewhere in
/// this process (ctrlc::set_handler is one-shot).
pub fn install_signal_handler(service: Arc<IndexingService>) -> Result<(), String> {
    ctrlc::set_handler(move || {
        eprintln!("Received Ctrl-C, shutting down gracefully...");
        if let Err(e) = service.graceful_shutdown() {
            eprintln!("Error during graceful shutdown: {}", e);
        }
        std::process::exit(0);
    })
    .map_err(|e| format!("install signal handler: {}", e))
}
