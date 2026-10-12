//! Graceful shutdown on a termination signal: SIGINT or SIGTERM on Unix, a
//! console control event (Ctrl-C, Ctrl-Break, closing the console) on
//! Windows. The handler stops the watcher, aborts any running index pass,
//! waits a bounded grace for the coordinator thread and exits. Call
//! [`install_signal_handler`] once from a binary's `main` with a cloned
//! [`IndexCoordinator`] handle.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::coordinator::IndexCoordinator;

/// How long a stop waits for one thread before abandoning it. A thread can be
/// inside a call that no flag reaches (one file's extraction, a `stat` on a
/// hung mount), and exit must not wait on it: abandoning is safe under WAL,
/// and what the thread had queued is lost at exit anyway.
pub const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Join `handle` if its thread ends by `deadline`. On `false` the handle is
/// dropped, which detaches the thread: it runs on until it returns or the
/// process exits. Checks at least once, so a thread that has already ended is
/// joined even when `deadline` has passed. The thread's result is discarded;
/// a panic counts as ended.
pub fn join_within<T>(handle: JoinHandle<T>, deadline: Instant) -> bool {
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

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

#[cfg(test)]
#[path = "shutdown_tests.rs"]
mod tests;
