//! Process-level shutdown helpers.
//!
//! Wires Ctrl-C (and on Unix, SIGTERM) to a graceful shutdown that stops
//! the watcher, aborts any running index pass, flushes the WAL, and
//! exits. Call [`install_signal_handler`] once from a binary's `main`
//! with a cloned [`IndexCoordinator`] handle.

use std::sync::Arc;

use crate::coordinator::IndexCoordinator;

/// Install a Ctrl-C (and, where supported, SIGTERM) handler that calls
/// [`IndexCoordinator::shutdown`] and then exits with status 0.
///
/// Returns an error only if a handler was already installed elsewhere in
/// this process (ctrlc::set_handler is one-shot).
pub fn install_signal_handler(coordinator: Arc<IndexCoordinator>) -> Result<(), String> {
    ctrlc::set_handler(move || {
        crate::log_info!("Received Ctrl-C, shutting down gracefully...");
        coordinator.shutdown();
        std::process::exit(0);
    })
    .map_err(|e| format!("install signal handler: {}", e))
}
