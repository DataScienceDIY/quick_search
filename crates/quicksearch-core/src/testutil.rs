//! Scratch directories for tests.
//!
//! Public and `#[doc(hidden)]` rather than `#[cfg(test)]`: the `tests/`
//! integration binaries and the GUI crate are separate compilation units, so a
//! test-gated item here would be invisible to them. This is the only reason it
//! is not private.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Distinguishes directories requested within one process. Two tests running
/// on different threads in the same millisecond would otherwise collide — the
/// hand-rolled helpers this replaces all keyed off a timestamp, which made
/// that rare rather than impossible.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A fresh, empty directory under the system temp dir, named for `tag`.
///
/// Not cleaned up on drop, deliberately: when a test fails, the tree it built
/// is most of the evidence. The OS clears the temp dir eventually.
///
/// Panics rather than returning a `Result` — a test that cannot create a
/// directory has nothing left to assert.
#[doc(hidden)]
pub fn scratch_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "quicksearch-{}-{}-{}",
        tag,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).expect("create scratch dir");
    p
}

/// [`scratch_dir`] canonicalized, for the tests that compare walked paths
/// against the root they were given. On macOS `/tmp` is a symlink to
/// `/private/tmp`, so an uncanonicalized root and a walked path disagree.
#[doc(hidden)]
pub fn scratch_dir_canonical(tag: &str) -> PathBuf {
    std::fs::canonicalize(scratch_dir(tag)).expect("canonicalize scratch dir")
}

/// Write `body` to `path`, creating parent directories as needed.
#[doc(hidden)]
pub fn touch(path: &std::path::Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, body).expect("write file");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_call_gets_its_own_empty_directory() {
        let a = scratch_dir("selftest");
        let b = scratch_dir("selftest");
        assert_ne!(a, b, "two calls must not collide");
        for d in [&a, &b] {
            assert!(d.is_dir());
            assert_eq!(std::fs::read_dir(d).unwrap().count(), 0, "starts empty");
        }
    }

    #[test]
    fn touch_creates_missing_parents() {
        let dir = scratch_dir("selftest-touch");
        let deep = dir.join("a/b/c.txt");
        touch(&deep, b"hi");
        assert_eq!(std::fs::read(&deep).unwrap(), b"hi");
    }
}
