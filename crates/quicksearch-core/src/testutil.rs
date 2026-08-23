//! Scratch directories for tests. Public and `#[doc(hidden)]` rather than
//! `#[cfg(test)]`: the `tests/` binaries and the GUI crate are separate
//! compilation units, so a test-gated item would be invisible to them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The compressed body [`crate::db::repo::set_content_done`] wants.
pub fn zstd_of(text: &str) -> Option<Vec<u8>> {
    crate::db::repo::encode_one(text, true).expect("zstd encode")
}

/// Old enough that a failure investigated the next morning has its tree.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Whether `name` is one of [`scratch_dir`]'s own directories. Matched on the
/// *shape* — `quicksearch-{tag}-{pid}-{seq}` — not the prefix alone:
/// `packaging/capture.sh` keeps its output in `quicksearch-capture` in the
/// same directory, and a prefix match would eat a capture run's screenshots.
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

/// Remove scratch directories left by runs that are long over. Sweeping on
/// the way *in* keeps this run's evidence and yesterday's while nothing
/// accumulates without bound.
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
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// A fresh, empty directory under the system temp dir, named for `tag`.
/// Never cleaned up: a failing test's tree is most of the evidence. Long-dead
/// runs' trees are swept once per process instead — see [`sweep_stale`].
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

/// [`scratch_dir`] canonicalized: on macOS `/tmp` is a symlink to
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

/// A filename that is legal on disk but cannot round-trip through the index:
/// `to_string_lossy` yields exactly `{stem}\u{FFFD}{suffix}` — itself a name
/// a *different* file can really have, the collision the walk and watcher
/// screens prevent; pair with [`lossy_twin`] to reproduce it. Some
/// filesystems (FAT, exFAT) refuse the name — tests must tolerate that.
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

/// The genuinely-representable name [`unrepresentable_name`] collapses to
/// under `to_string_lossy`.
#[doc(hidden)]
pub fn lossy_twin(stem: &str, suffix: &str) -> String {
    format!("{}\u{FFFD}{}", stem, suffix)
}

/// Power-of-two bucket, so memory-map sizes group by what allocated them.
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

/// A scratch directory that removes itself on drop — **unless the thread is
/// panicking**, keeping [`scratch_dir`]'s policy that a failing test's tree
/// is the evidence.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn dir(tag: &str) -> Scratch {
        Scratch(scratch_dir(tag))
    }

    /// A scratch database path; the guard owns the directory, so SQLite's
    /// sidecars are removed with it.
    pub fn db(tag: &str) -> (Scratch, PathBuf) {
        let dir = Scratch::dir(tag);
        let db = dir.join("index.sqlite");
        (dir, db)
    }
}

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for Scratch {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Seeded synthetic corpora, shared by tests/, benches/ and examples/.
// ---------------------------------------------------------------------------

/// Deterministic word picker — a fixed seed makes two runs comparable.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }

    pub fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.next() as usize % from.len()]
    }
}

/// A scratch database path under a fresh directory.
pub fn scratch_db(tag: &str) -> PathBuf {
    scratch_dir(tag).join("index.sqlite")
}

/// The rare term a seeded index is searched for. Nine bytes: clears the
/// trigram floor, and sits exactly on the boundary where a
/// `fuzzy_max_edits = 2` pigeonhole split into three-char chunks becomes legal.
pub const NEEDLE: &str = "quartzite";

/// A term planted only in document *bodies*, never in a file name — forces
/// the full-text pass to do real work: the filename `LIKE` finds nothing, and
/// every trigram candidate has to be decompressed and verified.
pub const BODY_TERM: &str = "chalcedony";

/// Filler vocabulary for seeded indexes. **Deliberately shares no trigram
/// with [`NEEDLE`]**: with a vocabulary that merely resembled the needle,
/// every query would fill the display limit in the first few hundred rows,
/// the cascade would break out of pass A, and passes B–D would never run —
/// while the harness looked perfectly healthy.
pub const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "brown", "fox", "jumps", "lazy", "index", "search", "cascade", "snippet", "document",
    "content", "extract", "summary", "meeting", "invoice", "contract", "budget", "revenue",
    "planning", "review", "draft", "final", "notes", "appendix", "figure",
];

/// What [`seed_index`] should build.
pub struct SeedSpec {
    pub files: usize,
    /// One file in every `content_every` gets extracted text.
    pub content_every: usize,
    /// Words in each stored document body.
    pub body_words: usize,
    /// Directories to spread the rows across.
    pub dirs: usize,
    /// File names carrying [`NEEDLE`]. Kept far below any sane display limit
    /// — an early-exiting query measures how fast the cascade gives up.
    pub needle_names: usize,
    /// Document bodies carrying [`NEEDLE`], on top of the names.
    pub needle_docs: usize,
    /// Document bodies carrying [`BODY_TERM`]; sized by the caller to stay
    /// under the display limit.
    pub body_term_docs: usize,
}

impl Default for SeedSpec {
    fn default() -> SeedSpec {
        SeedSpec {
            files: 50_000,
            content_every: 10,
            // ~2 KB per document: a corpus of tiny documents makes the fuzzy
            // full-text pass look free when it is the cascade's most expensive.
            body_words: 300,
            dirs: 500,
            needle_names: 50,
            needle_docs: 50,
            body_term_docs: 500,
        }
    }
}

/// Seed an index with synthetic rows, in one transaction. Shared by the
/// measurement harnesses so they all describe the same corpus.
pub fn seed_index(path: &std::path::Path, spec: &SeedSpec) {
    use crate::db::repo::{insert_file, set_content_done, NewFile};
    use crate::mime::FileType;

    let mut conn = crate::db::open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let mut rng = Lcg::new(0x5eed);
    // Spacing, not a random draw: a cluster at the front would let a pass
    // stop early and report a fraction of the work a real rare query costs.
    let name_stride = spec.files / spec.needle_names.max(1);
    let doc_stride = spec.files / spec.needle_docs.max(1);
    let body_stride = spec.files / spec.body_term_docs.max(1);
    let tx = conn.transaction().unwrap();
    for i in 0..spec.files {
        let w1 = rng.pick(WORDS);
        let w2 = rng.pick(WORDS);
        let name = if spec.needle_names > 0 && i % name_stride.max(1) == 0 {
            format!("{}-{}-{:07}.txt", w1, NEEDLE, i)
        } else {
            format!("{}-{}-{:07}.txt", w1, w2, i)
        };
        // Stored parents always end in a separator; see `dir_to_db_parent`.
        let dir = format!("/seed/{:03}/", i % spec.dirs.max(1));
        let id = insert_file(
            &tx,
            &NewFile {
                name: &name,
                parent: &dir,
                size: 4096,
                mtime: 1_700_000_000 + i as u64,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: i % spec.content_every.max(1) == 0,
            },
        )
        .unwrap()
        .expect("unique path");
        if i % spec.content_every.max(1) == 0 {
            let mut body: Vec<&str> = (0..spec.body_words).map(|_| *rng.pick(WORDS)).collect();
            if spec.needle_docs > 0 && i % doc_stride.max(1) == 0 {
                // Mid-body, so a snippet window has to be cut around it.
                body[spec.body_words / 2] = NEEDLE;
            }
            if spec.body_term_docs > 0 && i % body_stride.max(1) == 0 {
                // Two thirds in, so verifying it scans most of the document.
                body[spec.body_words * 2 / 3] = BODY_TERM;
            }
            let body = body.join(" ");
            set_content_done(&tx, id, &body, zstd_of(&body).as_deref()).unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
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

    /// The premise every collision test rests on, pinned per platform: if
    /// this ever stops holding, those tests silently assert nothing.
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
    /// the whole safety argument.
    #[test]
    fn only_scratch_directories_are_swept() {
        for ours in [
            "quicksearch-coord-1234-0",
            "quicksearch-stall-heavy-1001402-7",
            "quicksearch-a-0-0",
            // Tags contain dashes of their own.
            "quicksearch-sniff-binary-db-2621744-1",
        ] {
            assert!(is_scratch_name(ours), "{ours} should be swept");
        }

        for theirs in [
            // The capture output directory, the reason this is a shape match.
            "quicksearch-capture",
            "quicksearch",
            "quicksearch-",
            "quicksearch-coord",
            "quicksearch-coord-1234",
            // Numbers, but nothing in front of them to be a tag.
            "quicksearch-1234-0",
            "cargo-install-abc-1-2",
            "tmp-quicksearch-coord-1-2",
        ] {
            assert!(!is_scratch_name(theirs), "{theirs} must not be swept");
        }
    }

    /// Fresh directories survive; only long-dead runs are collected.
    #[test]
    fn the_sweep_keeps_recent_trees_and_takes_old_ones() {
        let fresh = scratch_dir("sweep-fresh");
        touch(&fresh.join("evidence.txt"), b"kept");

        // Same shape, but back-dated past the threshold.
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
