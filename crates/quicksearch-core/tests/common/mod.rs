//! Shared scaffolding for the integration-test binaries.

#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

#[allow(unused_imports)]
pub use quicksearch_core::testutil::{touch, Lcg, BODY_TERM};

const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Delete `db`'s `last_full_index` marker, so only the next run to finish
/// can put it back.
pub fn clear_marker(db: &Path) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
        .unwrap();
}

/// One full indexing run, awaited until `Idle`, then checked for the
/// `last_full_index` marker, which is written only on a successful finish.
///
/// `Idle` and not the marker ends the wait: the marker lands before the
/// run's tail and the post-run VACUUM, which would otherwise still be
/// writing when the caller's next statement runs. `Idle` is unambiguous
/// because `start_indexing` claims the status before it returns.
pub struct IndexOnce<'a> {
    pub db: &'a Path,
    pub roots: Vec<String>,
    pub config: &'a Config,
    /// Delete any existing completion marker first. Off for suites indexing a
    /// database whose lifecycle they are themselves testing.
    pub fresh_marker: bool,
    /// Read the marker through the keyed open: a plain open cannot read an
    /// encrypted index.
    pub encrypted: bool,
}

impl IndexOnce<'_> {
    pub fn run(mut self) {
        if self.fresh_marker && self.db.exists() {
            clear_marker(self.db);
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
        loop {
            match service.get_status() {
                IndexingStatus::Idle => break,
                IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "indexing did not finish within {:?}",
                INDEX_TIMEOUT
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(self.completed(), "the run ended without finishing");
    }

    /// Whether the marker is present.
    fn completed(&self) -> bool {
        let conn = if self.encrypted {
            db::open_existing(&self.db.to_string_lossy(), false).unwrap()
        } else {
            rusqlite::Connection::open(self.db).unwrap()
        };
        matches!(db::repo::get_last_full_index(&conn), Ok(Some(_)))
    }
}
