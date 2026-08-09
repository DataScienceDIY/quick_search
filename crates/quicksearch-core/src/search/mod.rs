//! Interruptible, streaming search service.
//!
//! One dedicated worker thread owns the cascade. Callers send queries via
//! [`SearchService::search`]; results stream back over an mpsc receiver as
//! [`SearchUpdate`] events tagged with a generation number. Starting a new
//! search bumps the generation and interrupts the in-flight SQLite
//! statement, so a keystroke never waits on the previous query.
//!
//! Cancellation is two-layer:
//! - **cooperative** — the cascade compares its generation against the
//!   latest every few hundred rows and stops silently when stale;
//! - **interrupt** — [`rusqlite::InterruptHandle::interrupt`] kills the
//!   statement currently executing. An interrupted stale search is normal
//!   cancellation, not an error.
//!
//! The interrupt handle is stored tagged with the generation that owns it,
//! and only ever fired at a generation the counter has already moved past:
//! interrupting the *current* generation would surface as "Search failed:
//! interrupted" instead of results, and the worker really can dequeue and
//! start a request before the caller that queued it gets back onto the CPU.
//!
//! Consumers that want a plain blocking search (the CLI mode) skip the
//! service entirely and call [`cascade::run`] with a collecting sink.
//!
//! The worker holds one connection across requests and releases it after
//! [`IDLE_RELEASE`] of quiet; [`Worker::take_connection`] covers when it
//! must be reopened instead of reused.

pub mod cascade;
pub mod duplicates;
pub mod fuzzy;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::Connection;

use crate::db;
use crate::query::split::split_for_cascade;
use crate::snippet::Snippet;

pub use cascade::Outcome;
pub use duplicates::{find_duplicate_groups, DuplicateGroup};

/// How long the worker keeps its connection after the last request. Not held
/// forever: an open handle on a deleted index keeps its blocks allocated on
/// disk, and an open reader stops SQLite from resetting the WAL.
const IDLE_RELEASE: Duration = Duration::from_secs(30);

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
    Started {
        generation: u64,
    },
    Hits {
        generation: u64,
        hits: Vec<SearchHit>,
    },
    Completed {
        generation: u64,
        total: usize,
        limited: bool,
    },
    Error {
        generation: u64,
        message: String,
    },
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

/// The search the worker is executing right now, and the handle that kills
/// its statement, tagged with the generation that owns it.
type InFlight = Arc<Mutex<Option<(u64, rusqlite::InterruptHandle)>>>;

pub struct SearchService {
    req_tx: mpsc::Sender<SearchRequest>,
    latest_gen: Arc<AtomicU64>,
    in_flight: InFlight,
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
        Self::new_with_idle_release(db_path, notify, IDLE_RELEASE)
    }

    /// [`Self::new`] with an explicit connection-release window (tests use
    /// short ones).
    pub fn new_with_idle_release(
        db_path: PathBuf,
        notify: Arc<dyn Fn() + Send + Sync>,
        idle_release: Duration,
    ) -> (SearchService, mpsc::Receiver<SearchUpdate>) {
        let (req_tx, req_rx) = mpsc::channel::<SearchRequest>();
        let (update_tx, update_rx) = mpsc::channel::<SearchUpdate>();
        let latest_gen = Arc::new(AtomicU64::new(0));
        let in_flight: InFlight = Arc::new(Mutex::new(None));
        let db_path = Arc::new(Mutex::new(db_path));

        let worker = Worker {
            req_rx,
            update_tx,
            notify,
            latest_gen: latest_gen.clone(),
            in_flight: in_flight.clone(),
            db_path: db_path.clone(),
            open: None,
            idle_release,
        };
        let handle = std::thread::Builder::new()
            .name("qs-search".into())
            .spawn(move || worker.run())
            .expect("spawn search worker");

        (
            SearchService {
                req_tx,
                latest_gen,
                in_flight,
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
        // Interrupt before enqueueing: an idle worker can dequeue the new
        // request and be mid-statement within microseconds.
        self.interrupt_stale();
        let _ = self.req_tx.send(SearchRequest {
            generation,
            input: input.to_string(),
            options,
        });
        generation
    }

    /// Cancel without starting anything new.
    pub fn cancel(&self) {
        self.latest_gen.fetch_add(1, Ordering::SeqCst);
        self.interrupt_stale();
    }

    /// Point subsequent searches at a different index file.
    pub fn set_db_path(&self, path: PathBuf) {
        *crate::lock_ok(&self.db_path) = path;
        self.cancel();
    }

    /// Kill the running statement — but only if the generation counter has
    /// already moved past the search that owns it. Interrupting the *newest*
    /// search does not cancel it, it fails it as
    /// `Search failed: interrupted`; and the worker can pick up a request
    /// before the thread that queued it runs again.
    fn interrupt_stale(&self) {
        let latest = self.latest_gen.load(Ordering::SeqCst);
        if let Some((generation, handle)) = crate::lock_ok(&self.in_flight).as_ref() {
            if *generation != latest {
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
        // Must never fall into the corruption bucket — the recovery dialog
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
    in_flight: InFlight,
    db_path: Arc<Mutex<PathBuf>>,
    /// The connection, and the index generation and path it was opened
    /// against. See [`Worker::take_connection`].
    open: Option<OpenIndex>,
    /// How long `open` survives with no requests; [`IDLE_RELEASE`] outside
    /// tests.
    idle_release: Duration,
}

/// A connection held across requests, tagged with what it was opened on.
struct OpenIndex {
    conn: Connection,
    epoch: u64,
    path: PathBuf,
}

impl Worker {
    fn run(mut self) {
        loop {
            let first = match self.req_rx.recv_timeout(self.idle_release) {
                Ok(req) => req,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.open = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
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

    /// Take the connection to run this request on, reopening if the one held
    /// cannot be reused. Reuse is what keeps
    /// [`crate::db::schema::PRAGMAS_SEARCH`]'s page cache warm across
    /// keystrokes.
    ///
    /// Reopened when either half of what it was opened against has changed:
    /// the path (config points at a different index) or the epoch (the file
    /// at the *same* path was replaced — rebuild, clear, schema-drift wipe —
    /// which no path comparison can catch, and which would leave this worker
    /// querying a deleted inode and pinning its blocks on disk).
    fn take_connection(&mut self, db_path: &Path) -> Result<OpenIndex, String> {
        let epoch = db::index_epoch();
        if let Some(open) = self.open.take() {
            if open.epoch == epoch && open.path == db_path {
                return Ok(open);
            }
            // Dropped here, before the open below, so the handle on the old
            // index is gone before a handle on the new one exists.
            drop(open);
        }
        Ok(OpenIndex {
            conn: db::open::open_search_reader(&db_path.to_string_lossy())?,
            epoch,
            path: db_path.to_path_buf(),
        })
    }

    fn send(&self, update: SearchUpdate) {
        let _ = self.update_tx.send(update);
        (self.notify)();
    }

    fn handle(&mut self, req: SearchRequest) {
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

        let db_path = crate::lock_ok(&self.db_path).clone();
        let open = match self.take_connection(&db_path) {
            Ok(c) => c,
            Err(e) => {
                self.send(SearchUpdate::Error {
                    generation,
                    message: classify_sql_err(&e),
                });
                return;
            }
        };
        // Publish the handle tagged with the generation it kills, before the
        // first statement runs.
        *crate::lock_ok(&self.in_flight) = Some((generation, open.conn.get_interrupt_handle()));

        let mut sink = |hits: Vec<SearchHit>| {
            self.send(SearchUpdate::Hits { generation, hits });
        };
        let outcome = cascade::run(
            &open.conn,
            &split,
            &req.options,
            generation,
            &self.latest_gen,
            &mut sink,
        );

        *crate::lock_ok(&self.in_flight) = None;

        // Kept only if it still works: a failed cascade may have failed
        // *because* of this connection, and putting it back would wedge every
        // later search behind the same bad handle.
        if outcome.is_ok() {
            self.open = Some(open);
        }

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

    /// Start a query long enough to be killed while it is executing, on its
    /// own thread. Returns the handle that kills it and the result channel.
    fn spawn_slow_query() -> (
        rusqlite::InterruptHandle,
        mpsc::Receiver<rusqlite::Result<i64>>,
    ) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let handle = conn.get_interrupt_handle();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let counted = conn.query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000) \
                 SELECT count(*) FROM c",
                [],
                |row| row.get::<_, i64>(0),
            );
            let _ = tx.send(counted);
        });
        (handle, rx)
    }

    /// Cancel repeatedly for as long as the query runs, so every window in
    /// which an interrupt could land is exercised rather than hoped past.
    fn cancel_until_done(
        service: &SearchService,
        rx: &mpsc::Receiver<rusqlite::Result<i64>>,
    ) -> rusqlite::Result<i64> {
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(1)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => service.interrupt_stale(),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("query thread died"),
            }
        }
    }

    fn idle_service() -> SearchService {
        // Nothing is ever enqueued, so the path is never opened.
        SearchService::new(PathBuf::from("/nonexistent"), Arc::new(|| {})).0
    }

    /// Typing the next character must not kill the search that character
    /// started: killing the newest generation surfaces as
    /// "Search failed: interrupted" instead of results.
    #[test]
    fn cancelling_spares_the_newest_generation() {
        let service = idle_service();
        let (handle, rx) = spawn_slow_query();
        service.latest_gen.store(7, Ordering::SeqCst);
        *service.in_flight.lock().unwrap() = Some((7, handle));

        let result = cancel_until_done(&service, &rx);
        *service.in_flight.lock().unwrap() = None;
        assert_eq!(
            result.ok(),
            Some(1_000_000),
            "the newest generation was interrupted"
        );
        service.shutdown();
    }

    /// The other half: a generation the counter has moved past still dies
    /// promptly, which is what keeps a keystroke from waiting on the previous
    /// query.
    #[test]
    fn cancelling_kills_a_superseded_generation() {
        let service = idle_service();
        let (handle, rx) = spawn_slow_query();
        service.latest_gen.store(8, Ordering::SeqCst);
        *service.in_flight.lock().unwrap() = Some((7, handle));

        let err = cancel_until_done(&service, &rx)
            .expect_err("a superseded generation must be interrupted");
        *service.in_flight.lock().unwrap() = None;
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted),
            "unexpected error: {}",
            err
        );
        service.shutdown();
    }

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
        // the corruption bucket either; pin that.
        let raw = "Failed to read database at /tmp/x.sqlite: file is not a database";
        assert!(!classify_sql_err(raw).starts_with("DATABASE_CORRUPTED:"));
    }

    #[test]
    fn corruption_and_syntax_classification_still_work() {
        assert!(
            classify_sql_err("database disk image is malformed").starts_with("DATABASE_CORRUPTED:")
        );
        assert!(classify_sql_err("fts5: syntax error near \"NEAR\"").starts_with("Search syntax"));
        assert!(classify_sql_err("no such table: files").starts_with("Search failed:"));
    }
}
