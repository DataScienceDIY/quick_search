//! Shared scaffolding for the integration-test binaries.
//!
//! Cargo compiles this into each `tests/*.rs` that declares `mod common;`, so
//! every binary gets its own copy and each one uses a different subset.

#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

#[allow(unused_imports)]
pub use quicksearch_core::testutil::{scratch_dir, scratch_dir_canonical, touch};

/// A scratch database path under a fresh directory. The sidecars SQLite
/// creates alongside it (`-wal`, `-shm`) land in the same directory.
pub fn scratch_db(tag: &str) -> std::path::PathBuf {
    scratch_dir(tag).join("index.sqlite")
}

/// Deterministic pseudo-random word picker — the same LCG and constants
/// `benches/corpus`, `examples/indexprobe` and `tests/search_perf` use, for the
/// same reason: a fixed seed is what makes two runs comparable, so a number
/// that moved is a real change rather than a different corpus.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }
}

/// The rare term a seeded index is searched for.
///
/// Nine bytes — long enough to clear the trigram floor, and exactly
/// `3 × (2 + 1)`, so it sits on the boundary where a `fuzzy_max_edits = 2`
/// pigeonhole split into three-character chunks becomes legal.
pub const NEEDLE: &str = "quartzite";

/// A term planted only in document *bodies*, never in a file name.
///
/// The needle above reaches the index through both, so a query for it is
/// answered mostly by the filename pass. This one forces the full-text pass to
/// do real work: the filename `LIKE` finds nothing, and every candidate the
/// trigram index returns has to be decompressed and verified.
pub const BODY_TERM: &str = "chalcedony";

/// Filler vocabulary for seeded indexes.
///
/// **Deliberately shares no trigram with [`NEEDLE`]** — no filler word contains
/// so much as `qua`. That is what makes a needle query genuinely rare, and it
/// matters more than it looks: with a vocabulary that merely *resembled* the
/// needle, every query would fill the display limit within the first few
/// hundred rows, the cascade would break out of pass A, and passes B, C and D
/// would never run at all. A harness built that way reports the same figure for
/// a literal and a fuzzy search and looks perfectly healthy doing it.
pub const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "eta",
    "theta",
    "iota",
    "kappa",
    "lambda",
    "brown",
    "fox",
    "jumps",
    "lazy",
    "index",
    "search",
    "cascade",
    "snippet",
    "document",
    "content",
    "extract",
    "summary",
    "meeting",
    "invoice",
    "contract",
    "budget",
    "revenue",
    "planning",
    "review",
    "draft",
    "final",
    "notes",
    "appendix",
    "figure",
];

/// What [`seed_index`] should build.
pub struct SeedSpec {
    pub files: usize,
    /// One file in every `content_every` gets extracted text. A tenth is the
    /// real shape — most files in a tree are not text — and it keeps the FTS
    /// index smaller than the table, as it is in practice.
    pub content_every: usize,
    /// Words in each stored document body.
    pub body_words: usize,
    /// Directories to spread the rows across, so `files.parent` has real
    /// variety and `idx_files_parent` has interior levels.
    pub dirs: usize,
    /// File names carrying [`NEEDLE`]. Kept far below any sane display limit so
    /// a needle query never fills it — an early-exiting query measures how fast
    /// the cascade gives up, not how fast it scans.
    pub needle_names: usize,
    /// Document bodies carrying [`NEEDLE`], on top of the names.
    pub needle_docs: usize,
    /// Document bodies carrying [`BODY_TERM`]. Sized by the caller to stay
    /// under the display limit, or the pass stops early and measures the
    /// give-up rather than the verification.
    pub body_term_docs: usize,
}

impl Default for SeedSpec {
    fn default() -> SeedSpec {
        SeedSpec {
            files: 50_000,
            content_every: 10,
            // ~2 KB of text per document. Not arbitrary: the fuzzy full-text
            // pass decompresses and scans every stored body, so a corpus of
            // 400-byte documents makes that pass look free when in production
            // it is the most expensive thing the cascade does. Still far under
            // `maximum_text_size` (256 KiB), which is the real worst case.
            body_words: 300,
            dirs: 500,
            needle_names: 50,
            needle_docs: 50,
            body_term_docs: 500,
        }
    }
}

/// Seed an index with synthetic rows, in one transaction.
///
/// Shared by the measurement harnesses so they all describe the same corpus;
/// what varies between them is the size, not the shape.
pub fn seed_index(path: &Path, spec: &SeedSpec) {
    use quicksearch_core::db::repo::{insert_file, set_content_done, NewFile};
    use quicksearch_core::mime::FileType;
    use quicksearch_core::testutil::zstd_of;

    let mut conn = db::open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let mut rng = Lcg::new(0x5eed);
    // Spacing rather than a random draw, so the planted rows are spread across
    // the table instead of clustering in whatever prefix the scan reaches
    // first — a cluster at the front would let a pass stop early and report a
    // fraction of the work a real rare query costs.
    let name_stride = spec.files / spec.needle_names.max(1);
    let doc_stride = spec.files / spec.needle_docs.max(1);
    let body_stride = spec.files / spec.body_term_docs.max(1);
    let tx = conn.transaction().unwrap();
    for i in 0..spec.files {
        let w1 = WORDS[(rng.next() as usize) % WORDS.len()];
        let w2 = WORDS[(rng.next() as usize) % WORDS.len()];
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
            let mut body: Vec<&str> = (0..spec.body_words)
                .map(|_| WORDS[(rng.next() as usize) % WORDS.len()])
                .collect();
            if spec.needle_docs > 0 && i % doc_stride.max(1) == 0 {
                // Mid-body, so a snippet window has to be cut around it rather
                // than falling out of a head-of-file window for free.
                body[spec.body_words / 2] = NEEDLE;
            }
            if spec.body_term_docs > 0 && i % body_stride.max(1) == 0 {
                // Two thirds in, so verifying it means scanning most of the
                // document rather than stopping at the first few bytes.
                body[spec.body_words * 2 / 3] = BODY_TERM;
            }
            let body = body.join(" ");
            set_content_done(&tx, id, &body, zstd_of(&body).as_deref()).unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
}

/// How long a single indexing run may take before the test gives up. Generous:
/// CI runs these in a container against a cold page cache.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// One full indexing run, awaited to completion.
///
/// Completion is read from the `last_full_index` marker rather than the status
/// enum, because `run_indexing` writes that marker only on a successful finish.
/// Polling for `IndexingStatus::Idle` instead would race: a small tree finishes
/// between two polls, leaving `Idle` ambiguous between "not started yet" and
/// "already done".
pub struct IndexOnce<'a> {
    pub db: &'a Path,
    pub roots: Vec<String>,
    pub config: &'a Config,
    /// Delete any existing completion marker first, so a second run over the
    /// same index is distinguishable from the first. Off for suites that index
    /// into a database whose lifecycle they are themselves testing.
    pub fresh_marker: bool,
    /// Poll the marker through the keyed open. An encrypted index cannot be
    /// read by a plain `rusqlite::Connection::open`, so a run against one would
    /// otherwise never observe its own completion and time out.
    pub encrypted: bool,
}

impl IndexOnce<'_> {
    pub fn run(mut self) {
        if self.fresh_marker && self.db.exists() {
            let conn = rusqlite::Connection::open(self.db).unwrap();
            conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
                .unwrap();
        }

        let service = IndexingService::new();
        service
            .start_indexing(
                std::mem::take(&mut self.roots),
                self.db.to_string_lossy().into_owned(),
                self.config.clone(),
            )
            .unwrap();

        let deadline = Instant::now() + INDEX_TIMEOUT;
        let mut done = false;
        while Instant::now() < deadline {
            if let IndexingStatus::Error(e) = service.get_status() {
                panic!("indexing failed: {}", e);
            }
            if self.db.exists() && self.completed() {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done, "indexing did not finish within {:?}", INDEX_TIMEOUT);
        service.stop_indexing().unwrap();
    }

    /// Whether the completion marker is present. A database mid-creation is
    /// simply "not yet", not a failure — the poll comes round again.
    fn completed(&self) -> bool {
        let conn = if self.encrypted {
            db::open_existing(&self.db.to_string_lossy(), false).ok()
        } else {
            rusqlite::Connection::open(self.db).ok()
        };
        conn.is_some_and(|c| db::repo::get_last_full_index(&c).is_some())
    }
}
