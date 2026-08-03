//! Interruptible, streaming search service.
//!
//! One dedicated worker thread owns the cascade. The GUI (or any caller)
//! sends queries via [`SearchService::search`]; results stream back over
//! an mpsc receiver as [`SearchUpdate`] events tagged with a generation
//! number. Starting a new search bumps the generation and interrupts the
//! in-flight SQLite statement, so a keystroke never waits on the previous
//! query.
//!
//! Cancellation is two-layer:
//! - **cooperative** — the cascade compares its generation against the
//!   latest every few hundred rows and stops silently when stale;
//! - **interrupt** — [`rusqlite::InterruptHandle::interrupt`] kills the
//!   statement currently executing (covering the "no rows produced yet"
//!   phases like FTS candidate gathering). An interrupted stale search is
//!   normal cancellation, not an error.
//!
//! Consumers that want a plain blocking search (the CLI mode) skip the
//! service entirely and call [`cascade::run`] with a collecting sink.

pub mod cascade;
pub mod duplicates;
pub mod fuzzy;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use crate::db;
use crate::query::split::split_for_cascade;
use crate::snippet::Snippet;

pub use cascade::Outcome;
pub use duplicates::{find_duplicate_groups, DuplicateGroup};

/// One search result. `rank` is the sort key (lower = better): integer
/// part = cascade stage (1–11), fraction = occurrence-count or
/// edit-distance tiebreak. Batches arrive already rank-ordered and later
/// batches only append, so a rank-sorted view never reshuffles.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub file_id: i64,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub rank: f64,
    pub stage: u8,
    /// The matched span in context: the filename for name stages, the full
    /// path for path stages, a window of the body for full-text stages
    /// (absent there when document text isn't stored).
    pub snippet: Option<Snippet>,
}

#[derive(Debug, Clone)]
pub enum SearchUpdate {
    Started { generation: u64 },
    Hits { generation: u64, hits: Vec<SearchHit> },
    Completed { generation: u64, total: usize, limited: bool },
    Error { generation: u64, message: String },
}

impl SearchUpdate {
    pub fn generation(&self) -> u64 {
        match self {
            SearchUpdate::Started { generation }
            | SearchUpdate::Hits { generation, .. }
            | SearchUpdate::Completed { generation, .. }
            | SearchUpdate::Error { generation, .. } => *generation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Enable the fuzzy stages (ranks 7, 8 and 11).
    pub fuzzy: bool,
    /// Ceiling on the fuzzy edit budget (`[search].fuzzy_max_edits`); see
    /// [`fuzzy::edit_budget`]. 0 disables the fuzzy stages.
    pub fuzzy_max_edits: usize,
    /// Hard cap on total hits per search (`[search].display_limit`).
    pub limit: usize,
    /// Streaming batch size (`[search].results_per_page`).
    pub batch: usize,
    /// Session-scoped ignore patterns (GUI chips), same glob semantics as
    /// the config's `ignore_patterns`. Applied before the display cap.
    pub session_ignores: Vec<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            fuzzy: false,
            fuzzy_max_edits: 2,
            limit: 1000,
            batch: 100,
            session_ignores: Vec::new(),
        }
    }
}

struct SearchRequest {
    generation: u64,
    input: String,
    options: SearchOptions,
}

pub struct SearchService {
    req_tx: mpsc::Sender<SearchRequest>,
    latest_gen: Arc<AtomicU64>,
    interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
    db_path: Arc<Mutex<PathBuf>>,
    handle: Option<JoinHandle<()>>,
}

impl SearchService {
    /// Spawn the worker. `notify` is invoked after every update event so
    /// an egui frontend can `request_repaint` (pass a no-op for headless
    /// use). Returns the service handle plus the update receiver, which
    /// the caller drains non-blockingly.
    pub fn new(
        db_path: PathBuf,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (SearchService, mpsc::Receiver<SearchUpdate>) {
        let (req_tx, req_rx) = mpsc::channel::<SearchRequest>();
        let (update_tx, update_rx) = mpsc::channel::<SearchUpdate>();
        let latest_gen = Arc::new(AtomicU64::new(0));
        let interrupt = Arc::new(Mutex::new(None));
        let db_path = Arc::new(Mutex::new(db_path));

        let worker = Worker {
            req_rx,
            update_tx,
            notify,
            latest_gen: latest_gen.clone(),
            interrupt: interrupt.clone(),
            db_path: db_path.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("qs-search".into())
            .spawn(move || worker.run())
            .expect("spawn search worker");

        (
            SearchService {
                req_tx,
                latest_gen,
                interrupt,
                db_path,
                handle: Some(handle),
            },
            update_rx,
        )
    }

    /// Start a new search, cancelling any in-flight one. Returns the
    /// generation whose events to keep.
    pub fn search(&self, input: &str, options: SearchOptions) -> u64 {
        let generation = self.latest_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.req_tx.send(SearchRequest {
            generation,
            input: input.to_string(),
            options,
        });
        // The new request can't be running yet (the worker hasn't dequeued
        // it), so this only ever kills a stale generation's statement.
        self.interrupt_current();
        generation
    }

    /// Cancel without starting anything new.
    pub fn cancel(&self) {
        self.latest_gen.fetch_add(1, Ordering::SeqCst);
        self.interrupt_current();
    }

    /// Point subsequent searches at a different index file.
    pub fn set_db_path(&self, path: PathBuf) {
        *self.db_path.lock().unwrap() = path;
        self.cancel();
    }

    fn interrupt_current(&self) {
        if let Ok(guard) = self.interrupt.lock() {
            if let Some(handle) = guard.as_ref() {
                handle.interrupt();
            }
        }
    }

    /// Cancel, close the request channel, and join the worker.
    pub fn shutdown(self) {
        self.cancel();
        let SearchService { req_tx, handle, .. } = self;
        drop(req_tx);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// Map SQLite-level errors to the tagged strings frontends key off.
/// `DATABASE_CORRUPTED:` drives the GUI's recovery dialog.
pub fn classify_sql_err(error_msg: &str) -> String {
    if error_msg.starts_with(db::KEY_MISMATCH_PREFIX) {
        // Wrong or missing encryption key. Already user-legible, and it
        // must never fall into the corruption bucket — the recovery dialog
        // would offer to delete an index that is perfectly intact.
        error_msg.to_string()
    } else if error_msg.contains("malformed")
        || error_msg.contains("corrupt")
        || error_msg.contains("database disk image is malformed")
    {
        format!("DATABASE_CORRUPTED: {}", error_msg)
    } else if error_msg.contains("fts5: syntax error") {
        "Search syntax error: the search term contains characters that cannot be processed."
            .to_string()
    } else {
        format!("Search failed: {}", error_msg)
    }
}

struct Worker {
    req_rx: mpsc::Receiver<SearchRequest>,
    update_tx: mpsc::Sender<SearchUpdate>,
    notify: Arc<dyn Fn() + Send + Sync>,
    latest_gen: Arc<AtomicU64>,
    interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
    db_path: Arc<Mutex<PathBuf>>,
}

impl Worker {
    fn run(self) {
        while let Ok(first) = self.req_rx.recv() {
            // A fast typist queues several requests; only the newest one
            // matters.
            let mut req = first;
            while let Ok(newer) = self.req_rx.try_recv() {
                req = newer;
            }
            if req.generation != self.latest_gen.load(Ordering::SeqCst) {
                continue;
            }
            self.handle(req);
        }
    }

    fn send(&self, update: SearchUpdate) {
        let _ = self.update_tx.send(update);
        (self.notify)();
    }

    fn handle(&self, req: SearchRequest) {
        let generation = req.generation;
        self.send(SearchUpdate::Started { generation });

        let split = match split_for_cascade(&req.input) {
            Ok(s) => s,
            Err(e) => {
                self.send(SearchUpdate::Error {
                    generation,
                    message: e.to_string(),
                });
                return;
            }
        };

        let db_path = self.db_path.lock().unwrap().clone();
        // Per-request open: microseconds, and always sees a freshly
        // rebuilt index file rather than pinning a deleted inode.
        let conn = match db::open_existing(&db_path.to_string_lossy(), false) {
            Ok(c) => c,
            Err(e) => {
                self.send(SearchUpdate::Error {
                    generation,
                    message: classify_sql_err(&e),
                });
                return;
            }
        };
        *self.interrupt.lock().unwrap() = Some(conn.get_interrupt_handle());

        let mut sink = |hits: Vec<SearchHit>| {
            self.send(SearchUpdate::Hits { generation, hits });
        };
        let outcome = cascade::run(
            &conn,
            &split,
            &req.options,
            generation,
            &self.latest_gen,
            &mut sink,
        );

        *self.interrupt.lock().unwrap() = None;

        match outcome {
            Ok(Some(Outcome { total, limited })) => self.send(SearchUpdate::Completed {
                generation,
                total,
                limited,
            }),
            // Cancelled — the newer generation owns the UI now.
            Ok(None) => {}
            Err(e) => self.send(SearchUpdate::Error {
                generation,
                message: classify_sql_err(&e),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mismatch_is_never_classified_as_corruption() {
        let msg = format!(
            "{}index at /tmp/x.sqlite: wrong password (or the file is not a QuickSearch index)",
            db::KEY_MISMATCH_PREFIX
        );
        let classified = classify_sql_err(&msg);
        assert_eq!(classified, msg, "must pass through verbatim");
        assert!(!classified.starts_with("DATABASE_CORRUPTED:"));

        // The raw SQLite wording for an undecryptable page must not land in
        // the corruption bucket either — the GUI recovery dialog offers to
        // delete the file, which is exactly wrong for an intact encrypted
        // index. (It doesn't match the corruption needles; pin that.)
        let raw = "Failed to read database at /tmp/x.sqlite: file is not a database";
        assert!(!classify_sql_err(raw).starts_with("DATABASE_CORRUPTED:"));
    }

    #[test]
    fn corruption_and_syntax_classification_still_work() {
        assert!(classify_sql_err("database disk image is malformed")
            .starts_with("DATABASE_CORRUPTED:"));
        assert!(classify_sql_err("fts5: syntax error near \"NEAR\"").starts_with("Search syntax"));
        assert!(classify_sql_err("no such table: files").starts_with("Search failed:"));
    }
}
