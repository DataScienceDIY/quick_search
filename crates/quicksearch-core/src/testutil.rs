//! Scratch directories for tests.
//!
//! Public and `#[doc(hidden)]` rather than `#[cfg(test)]`: the `tests/`
//! integration binaries and the GUI crate are separate compilation units, so
//! a test-gated item here would be invisible to them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Distinguishes directories requested within one process; a timestamp alone
/// lets two tests in the same millisecond collide.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The compressed body [`crate::db::repo::set_content_done`] wants, for tests
/// that only care that a sidecar row gets written.
///
/// Production callers compress a whole batch through one
/// [`crate::db::repo::DocEncoder`] before taking the connection lock; a test
/// writing one row has nothing to amortize and wants the one-liner.
pub fn zstd_of(text: &str) -> Option<Vec<u8>> {
    crate::db::repo::encode_one(text, true).expect("zstd encode")
}

/// A fresh, empty directory under the system temp dir, named for `tag`.
/// Not cleaned up on drop: when a test fails, the tree it built is most of
/// the evidence. Panics — a test that cannot create a directory has nothing
/// left to assert.
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

/// Power-of-two bucket, so memory-map sizes group by what allocated them
/// rather than by their exact size. Shared by the memory probes.
#[doc(hidden)]
pub fn size_class(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib < 1.0 {
        "< 1 MiB".to_string()
    } else {
        let bucket = 1u64 << (63 - (bytes / (1024 * 1024)).leading_zeros() as u64);
        format!("~{} MiB", bucket)
    }
}

#[doc(hidden)]
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
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
