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

/// Lock, ignoring poisoning. A panic on a worker thread must not cascade
/// into every thread that later takes the lock: `coordinator::state()` runs
/// on the GUI's per-frame path.
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

/// Run `f`, turning a panic into `Err` with the panic's message, for work
/// whose panic must not take its thread or skip what follows it.
pub(crate) fn contained<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        // The payloads `panic!` builds: a literal, or a formatted message.
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::contained;

    #[test]
    fn contained_passes_a_normal_result_through() {
        assert_eq!(contained(|| 7), Ok(7));
    }

    #[test]
    fn contained_carries_the_message_of_a_literal_panic() {
        assert_eq!(
            contained(|| panic!("boom")),
            Err::<(), _>("boom".to_string())
        );
    }

    #[test]
    fn contained_carries_the_message_of_a_formatted_panic() {
        let n = 3;
        let got: Result<(), _> = contained(|| panic!("boom {n}"));
        assert_eq!(got, Err("boom 3".to_string()));
    }

    #[test]
    fn contained_names_a_payload_that_is_not_a_message() {
        let got = contained(|| std::panic::panic_any(42u8));
        assert_eq!(got, Err::<(), _>("unknown panic".to_string()));
    }
}
