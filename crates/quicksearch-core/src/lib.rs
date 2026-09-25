pub mod cli;
pub mod config;
pub mod content;
pub mod coordinator;
pub mod db;
pub mod extract;
pub mod file_handling;
pub mod incremental;
pub mod indexing;
pub mod live;
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
pub mod verify;
pub mod walk;
pub mod watcher;

/// Lock, ignoring poisoning. Every Mutex in this crate guards whole-value
/// replacements, so a poisoned guard still holds the last fully-written
/// value. A panic on a worker thread must not cascade into every thread
/// that later takes the lock: `coordinator::state()` runs on the GUI's
/// per-frame path.
pub(crate) fn lock_ok<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`std::sync::Condvar::wait`], ignoring poisoning for the reason
/// [`lock_ok`] gives.
pub(crate) fn wait_ok<'a, T>(
    cv: &std::sync::Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    cv.wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Join every handle, including those after one that panicked; `true` if
/// none did.
pub(crate) fn join_all<T>(handles: impl IntoIterator<Item = std::thread::JoinHandle<T>>) -> bool {
    handles
        .into_iter()
        .fold(true, |ok, h| h.join().is_ok() && ok)
}
