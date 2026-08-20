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

/// How old a leftover scratch directory must be before [`sweep_stale`] takes
/// it. Far longer than any test run, so a failure investigated the same day —
/// or the next morning — still has its tree.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Whether `name` is one of [`scratch_dir`]'s own directories.
///
/// Matched on the *shape* — `quicksearch-{tag}-{pid}-{seq}`, so the last two
/// dash-separated components must be numbers — rather than on the
/// `quicksearch-` prefix alone. `packaging/capture.sh` keeps its output in
/// `quicksearch-capture` in the same directory, and a prefix match would eat a
/// capture run's screenshots along with the litter.
fn is_scratch_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("quicksearch-") else {
        return false;
    };
    let numeric = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut tail = rest.rsplitn(3, '-');
    // seq, then pid, and a tag must remain in front of them.
    numeric(tail.next()) && numeric(tail.next()) && tail.next().is_some_and(|tag| !tag.is_empty())
}

/// Remove scratch directories left by runs that are long over.
///
/// Nothing here cleans up on the way *out*: a failed test's tree is most of
/// the evidence, which is why [`scratch_dir`] deliberately leaves it. But
/// passing tests leave theirs too, and most never remove it — so the temp
/// directory grew by roughly three hundred directories per full run and had
/// accumulated some nine thousand of them. Where `/tmp` is a tmpfs that is
/// gigabytes of RAM, which slows the whole suite and pushes the
/// timing-sensitive tests toward their budgets.
///
/// Sweeping on the way *in* keeps both halves: this run's evidence survives,
/// and so does yesterday's, while nothing accumulates without bound. Only
/// [`scratch_dir`]'s own naming is touched.
fn sweep_stale() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_scratch_name) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if stale {
            // Best effort throughout: two test binaries starting together race
            // on the same directory and one of them loses, which is fine.
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// A fresh, empty directory under the system temp dir, named for `tag`.
///
/// Not cleaned up on drop: when a test fails, the tree it built is most of
/// the evidence. Long-dead runs' trees are swept once per process instead —
/// see [`sweep_stale`]. Panics — a test that cannot create a directory has
/// nothing left to assert.
#[doc(hidden)]
pub fn scratch_dir(tag: &str) -> PathBuf {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(sweep_stale);

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

/// A filename that is legal on disk but cannot round-trip through the index,
/// spelled so that `to_string_lossy` yields exactly `{stem}\u{FFFD}{suffix}` —
/// which is itself a perfectly ordinary filename, and so a name a *different*
/// file can really have. That collision is what the screens in
/// `crate::walk::read_directory` and `crate::watcher` exist to prevent, and
/// pairing this with [`lossy_twin`] is how the tests reproduce it.
///
/// The two platforms fail in different ways and both are real:
///
/// * On Unix an `OsStr` is arbitrary bytes, so any invalid UTF-8 byte does it.
///   `0xFF` can never appear in well-formed UTF-8.
/// * On Windows a path is UTF-16 code units and NTFS does not check that they
///   are well-*formed*, so an unpaired surrogate is storable. Rust models this
///   with WTF-8, and `to_str()` returns `None` for precisely that case. Far
///   from theoretical: WSL's DrvFs encodes non-UTF-8 Linux names this way by
///   design, and Samba shares of Linux servers produce them from legacy
///   encodings.
///
/// Some filesystems (FAT, exFAT, some network redirectors) refuse the name —
/// tests that put one on disk must tolerate the creation failing rather than
/// asserting on it.
#[doc(hidden)]
pub fn unrepresentable_name(stem: &str, suffix: &str) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = stem.as_bytes().to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(suffix.as_bytes());
        std::ffi::OsString::from_vec(bytes)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut units: Vec<u16> = stem.encode_utf16().collect();
        // A high surrogate with nothing after it to pair with.
        units.push(0xD800);
        units.extend(suffix.encode_utf16());
        std::ffi::OsString::from_wide(&units)
    }
}

/// The name [`unrepresentable_name`] collapses to under `to_string_lossy`, as
/// a name that is genuinely representable — so a test can put both on disk and
/// assert the real one survives what happens to the other.
#[doc(hidden)]
pub fn lossy_twin(stem: &str, suffix: &str) -> String {
    format!("{}\u{FFFD}{}", stem, suffix)
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

    /// The premise every collision test rests on, pinned per platform: the
    /// name really is unrepresentable, and its lossy image really is a name
    /// another file could have. If this ever stops holding, those tests would
    /// silently start asserting nothing.
    #[test]
    fn the_unrepresentable_name_collapses_onto_its_twin() {
        let bad = unrepresentable_name("x", ".txt");
        assert!(
            bad.to_str().is_none(),
            "the name must not be representable: {:?}",
            bad
        );
        assert_eq!(
            bad.to_string_lossy(),
            lossy_twin("x", ".txt"),
            "the two names must collide under to_string_lossy"
        );
        assert!(
            std::path::Path::new(&lossy_twin("x", ".txt"))
                .as_os_str()
                .to_str()
                .is_some(),
            "the twin must itself be a perfectly ordinary name"
        );
    }

    /// The sweep runs against a shared temp directory, so what it matches is
    /// the whole safety argument. `quicksearch-capture` is the one that would
    /// hurt: `packaging/capture.sh` puts a run's screenshots and screencasts
    /// there, and a prefix match would delete them mid-capture.
    #[test]
    fn only_scratch_directories_are_swept() {
        for ours in [
            "quicksearch-coord-1234-0",
            "quicksearch-stall-heavy-1001402-7",
            "quicksearch-a-0-0",
            // Tags contain dashes of their own; only the last two components
            // are read as numbers.
            "quicksearch-sniff-binary-db-2621744-1",
        ] {
            assert!(is_scratch_name(ours), "{ours} should be swept");
        }

        for theirs in [
            // The capture output directory, the reason this is a shape match.
            "quicksearch-capture",
            "quicksearch",
            "quicksearch-",
            // A tag but no pid/seq pair.
            "quicksearch-coord",
            "quicksearch-coord-1234",
            // Numbers, but nothing in front of them to be a tag.
            "quicksearch-1234-0",
            // Not ours at all.
            "cargo-install-abc-1-2",
            "tmp-quicksearch-coord-1-2",
        ] {
            assert!(!is_scratch_name(theirs), "{theirs} must not be swept");
        }
    }

    /// Fresh directories survive; only long-dead runs are collected. Uses a
    /// hand-built name rather than `scratch_dir` so the assertion is about the
    /// age gate and not about whatever else the suite has left lying around.
    #[test]
    fn the_sweep_keeps_recent_trees_and_takes_old_ones() {
        let fresh = scratch_dir("sweep-fresh");
        touch(&fresh.join("evidence.txt"), b"kept");

        // Same shape, but back-dated past the threshold. `set_times` is the
        // only way to age a directory without waiting twelve hours for it.
        let old = std::env::temp_dir().join(format!(
            "quicksearch-sweep-old-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&old).expect("create the aged directory");
        let long_ago =
            std::time::SystemTime::now() - STALE_AFTER - std::time::Duration::from_secs(60);
        std::fs::File::open(&old)
            .and_then(|d| {
                d.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(long_ago)
                        .set_modified(long_ago),
                )
            })
            .expect("back-date the aged directory");

        sweep_stale();

        assert!(fresh.exists(), "a fresh scratch tree was swept away");
        assert!(!old.exists(), "a long-dead scratch tree survived the sweep");

        std::fs::remove_dir_all(&fresh).ok();
    }
}
