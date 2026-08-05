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
