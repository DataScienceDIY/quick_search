pub mod cli;
pub mod config;
pub mod content;
pub mod coordinator;
pub mod db;
pub mod extract;
pub mod file_handling;
pub mod incremental;
pub mod indexing;
pub mod log;
pub mod mime;
pub mod platform;
pub mod query;
pub mod scope;
pub mod search;
pub mod security;
pub mod shutdown;
pub mod snippet;
#[doc(hidden)]
pub mod testutil;
pub mod textenc;
pub mod walk;
pub mod watcher;

/// Lock, ignoring poisoning. Every Mutex in this crate guards whole-value
/// replacements, so a poisoned guard still holds the last fully-written
/// value. A panic on a worker thread must not cascade into every thread
/// that later takes the lock — `coordinator::state()` runs on the GUI's
/// per-frame path.
pub(crate) fn lock_ok<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
