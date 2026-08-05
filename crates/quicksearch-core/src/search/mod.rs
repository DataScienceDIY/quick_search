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
//! The interrupt handle is stored tagged with the generation that owns it,
//! and only ever fired at a generation the counter has already moved past.
//! Interrupting the *current* generation would surface to the user as
//! "Search failed: interrupted" instead of results, and the window for it
//! is real: the worker can dequeue and start a request before the caller
//! that queued it gets back onto the CPU.
//!
//! Consumers that want a plain blocking search (the CLI mode) skip the
//! service entirely and call [`cascade::run`] with a collecting sink.
//!
//! # The worker's connection
//!
//! The worker holds one connection across requests and releases it after
//! [`IDLE_RELEASE`] of quiet. This is not an optimisation of open cost — an
//! open is microseconds — but of the page cache behind it: a search runs on
//! every character typed, so reopening per request meant paying to warm a
//! cache and then discarding it, once per keystroke, forever.
//!
//! What the old per-request open bought, and what now has to be arranged
//! deliberately, is never operating on an index that has been replaced
//! underneath it. Two things can do that, and only one is visible in the path:
//! the config can point at a different file, and a rebuild, clear or
//! schema-drift wipe can put a *new* file at the *same* path. The second is
//! why [`crate::db::index_epoch`] exists — see [`Worker::take_connection`].
//! Holding a handle on a replaced index would mean stale results and, on
//! Linux, its blocks staying allocated until the handle closed.

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

/// How long the worker keeps its connection after the last request.
///
/// The connection is held across requests so a typing session runs against a
/// warm page cache (see [`Worker::take_connection`]). It is not held *forever*, for
/// two reasons that have nothing to do with the cache: an open handle on a
/// deleted index keeps its blocks allocated on disk, and an open reader stops
/// SQLite from resetting the WAL, which would then grow toward
/// `maximum_wal_size` and never come back down.
///
/// So: long enough to span the pauses inside a search session, short enough
/// that an abandoned one is not still holding the index minutes later.
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
/// its statement. Tagged with the generation so a caller can tell whether
/// the thing it is about to interrupt is the search it means to cancel or
/// one that has since replaced it.
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

    /// [`Self::new`] with an explicit connection-release window.
    ///
    /// Tests use short windows; the default [`IDLE_RELEASE`] is right for real
    /// use, where the window has to span the pauses inside a typing session.
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
        // request and be mid-statement within microseconds, and there is no
        // point handing it a kill the worker has to survive.
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
        *self.db_path.lock().unwrap() = path;
        self.cancel();
    }

    /// Kill the running statement — but only if the generation counter has
    /// already moved past the search that owns it.
    ///
    /// Callers bump `latest_gen` first, so anything still tagged with an
    /// older generation is stale by definition. The tag is what makes this
    /// safe rather than merely well-timed: interrupting the *newest* search
    /// does not cancel anything, it fails it, and the cascade reports that
    /// as `Search failed: interrupted` because its own generation is still
    /// current. On a loaded machine the worker really can pick up and start
    /// a request before the thread that queued it runs again, so "the new
    /// search cannot have started yet" is not an assumption to build on.
    fn interrupt_stale(&self) {
        let latest = self.latest_gen.load(Ordering::SeqCst);
        if let Ok(guard) = self.in_flight.lock() {
            if let Some((generation, handle)) = guard.as_ref() {
                if *generation != latest {
                    handle.interrupt();
                }
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
            // `recv_timeout`, not `recv`: a search session is a burst of
            // requests one keystroke apart, and the connection is worth
            // keeping for the length of one. Past that it is worth strictly
            // less than nothing — see [`IDLE_RELEASE`].
            let first = match self.req_rx.recv_timeout(self.idle_release) {
                Ok(req) => req,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.open = None;
                    continue;
                }
                // The service was dropped; nothing more is coming.
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
    /// cannot be reused.
    ///
    /// Taken out and handed back by the caller, rather than borrowed from
    /// `self`, for the same reason [`crate::coordinator`]'s writer is: the
    /// cascade needs the connection for its whole run, and everything else on
    /// `self` — the update channel, the generation counter, the interrupt slot
    /// — has to stay reachable while it does.
    ///
    /// Reuse is what makes [`crate::db::schema::PRAGMAS_SEARCH`]'s page cache
    /// worth having: searches run on every character typed, so the second
    /// query of a session and every one after it finds the b-tree interior
    /// pages and FTS5 segment tips already resident. Opening per request threw
    /// that away each time.
    ///
    /// It is reopened when either half of what it was opened against has
    /// changed. The path moves when the user points the config at a different
    /// index. The epoch moves when the file at the *same* path is replaced —
    /// a rebuild, a clear, or a schema-drift wipe — which no comparison of
    /// paths could ever catch, and which would otherwise leave this worker
    /// querying a deleted inode and pinning its blocks on disk.
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

        let db_path = self.db_path.lock().unwrap().clone();
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
        // first statement runs. Anyone cancelling from here on can tell this
        // search apart from the one that supersedes it.
        *self.in_flight.lock().unwrap() = Some((generation, open.conn.get_interrupt_handle()));

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

        *self.in_flight.lock().unwrap() = None;

        // Kept only if it still works. A failed cascade may have failed
        // *because* of this connection — a torn file, or an interrupt that
        // left it unusable — and putting it back would wedge every search
        // after this one behind the same bad handle. Dropping it costs one
        // reopen.
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
    /// started. The worker can be mid-statement on the newest generation by
    /// the time the caller gets around to cancelling — on a slow machine that
    /// is common, not exotic — and killing it there surfaces as
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
        // the corruption bucket either — the GUI recovery dialog offers to
        // delete the file, which is exactly wrong for an intact encrypted
        // index. (It doesn't match the corruption needles; pin that.)
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
