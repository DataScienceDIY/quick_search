//! Graceful shutdown on a termination signal: SIGINT or SIGTERM on Unix, a
//! console control event (Ctrl-C, Ctrl-Break, closing the console) on
//! Windows. The handler stops the watcher, aborts any running index pass,
//! flushes the WAL and exits. Call [`install_signal_handler`] once from a
//! binary's `main` with a cloned [`IndexCoordinator`] handle.

use std::sync::Arc;

use crate::coordinator::IndexCoordinator;

/// Install a handler that calls [`IndexCoordinator::shutdown`] and then exits
/// with status 0.
///
/// ctrlc's `termination` feature also claims SIGHUP; it is handed back, so a
/// hangup behaves as it would without this handler (and `nohup` still works).
/// Fails if a handler is already installed in this process (ctrlc allows one)
/// or the OS refuses; the caller logs the error.
pub fn install_signal_handler(coordinator: Arc<IndexCoordinator>) -> Result<(), String> {
    crate::platform::preserving_sighup(|| {
        ctrlc::set_handler(move || {
            crate::log_info!("Received termination signal, shutting down gracefully...");
            coordinator.shutdown();
            std::process::exit(0);
        })
    })
    .map_err(|e| format!("install signal handler: {}", e))
}
