//! Shared scaffolding for the integration-test binaries.

#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

#[allow(unused_imports)]
pub use quicksearch_core::testutil::{scratch_dir, scratch_dir_canonical, touch};

#[allow(unused_imports)]
pub use quicksearch_core::testutil::{
    scratch_db, seed_index, Lcg, SeedSpec, BODY_TERM, NEEDLE, WORDS,
};

const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// One full indexing run, awaited via the `last_full_index` marker, which is
/// written only on a successful finish — polling for `Idle` would race: a
/// small tree finishes between two polls, leaving `Idle` ambiguous between
/// "not started yet" and "already done".
pub struct IndexOnce<'a> {
    pub db: &'a Path,
    pub roots: Vec<String>,
    pub config: &'a Config,
    /// Delete any existing completion marker first. Off for suites indexing a
    /// database whose lifecycle they are themselves testing.
    pub fresh_marker: bool,
    /// Poll the marker through the keyed open: a plain open cannot read an
    /// encrypted index and would never observe its own completion.
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

    /// Whether the marker is present; mid-creation is simply "not yet".
    fn completed(&self) -> bool {
        let conn = if self.encrypted {
            db::open_existing(&self.db.to_string_lossy(), false).ok()
        } else {
            rusqlite::Connection::open(self.db).ok()
        };
        conn.is_some_and(|c| db::repo::get_last_full_index(&c).is_some())
    }
}
