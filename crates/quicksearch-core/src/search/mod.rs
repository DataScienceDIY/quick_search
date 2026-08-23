//! Interruptible, streaming search service.
//!
//! One worker thread owns the cascade; [`SearchService::search`] bumps a
//! generation and streams [`SearchUpdate`] events back. Cancellation is
//! two-layer: cooperative generation checks plus
//! [`rusqlite::InterruptHandle::interrupt`] on superseded statements.

pub mod cascade;
pub mod duplicates;
pub mod fuzzy;
pub mod prefilter;

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

/// Idle window before the connection is released: an open reader stops
/// SQLite resetting the WAL and pins a deleted index's blocks.
const IDLE_RELEASE: Duration = Duration::from_secs(30);

/// One search result. `rank` is the sort key (lower = better): integer part
/// = cascade stage (1–11), fraction = tiebreak. Batches arrive rank-ordered
/// and later batches only append, so a rank-sorted view never reshuffles.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub file_id: i64,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub rank: f64,
    pub stage: u8,
    /// The matched span in context: name, path, or a window of the body per
    /// stage (absent for full-text stages when no text is stored).
    pub snippet: Option<Snippet>,
}

/// Which field a hit's [`SearchHit::snippet`] excerpts, derived from the
/// cascade stage. See the rank table at the top of [`crate::search::cascade`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchField {
    Name,
    Contents,
    Path,
}

/// How a [`MatchField::Contents`] hit matched its body. Not interchangeable:
/// re-cutting a fuzzy hit as if it were exact finds nothing and reads as
/// "the file no longer matches".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentTier {
    /// Stages 5 and 6: the body contains the term as written.
    Exact,
    /// Stage 8: within the fuzzy edit budget of the term.
    Fuzzy,
}

impl SearchHit {
    pub fn match_field(&self) -> MatchField {
        match self.stage {
            1..=4 | 7 => MatchField::Name,
            5 | 6 | 8 => MatchField::Contents,
            // 9..=11, and whatever a later tier adds: the path is the safe reading.
            _ => MatchField::Path,
        }
    }

    pub fn content_tier(&self) -> Option<ContentTier> {
        match self.stage {
            5 | 6 => Some(ContentTier::Exact),
            8 => Some(ContentTier::Fuzzy),
            _ => None,
        }
    }
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
    /// Ceiling on the fuzzy edit budget (`[search].fuzzy_max_edits`); 0 disables.
    pub fuzzy_max_edits: usize,
    /// Hard cap on total hits per search (`[search].display_limit`).
    pub limit: usize,
    /// Streaming batch size (`[search].results_per_page`).
    pub batch: usize,
    /// Session-scoped ignore patterns (GUI chips); applied before the display cap.
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

/// The statement-kill handle, tagged with the generation that owns it.
type InFlight = Arc<Mutex<Option<(u64, rusqlite::InterruptHandle)>>>;

type ReleaseAck = Arc<Mutex<Option<mpsc::Sender<()>>>>;

/// How long [`SearchService::release_connection`] waits for the worker's answer.
const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// The release is a message rather than a flag because the worker is parked
/// in a 30-second `recv_timeout` where a flag would not reach it.
enum WorkerMsg {
    Search(SearchRequest),
    /// Drop the held connection so the index file can be deleted or replaced.
    ReleaseConnection,
}

pub struct SearchService {
    req_tx: mpsc::Sender<WorkerMsg>,
    latest_gen: Arc<AtomicU64>,
    in_flight: InFlight,
    db_path: Arc<Mutex<PathBuf>>,
    release_ack: ReleaseAck,
    handle: Option<JoinHandle<()>>,
}

impl SearchService {
    /// Spawn the worker. `notify` is invoked after every update event so an
    /// egui frontend can `request_repaint` (pass a no-op for headless use).
    pub fn new(
        db_path: PathBuf,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (SearchService, mpsc::Receiver<SearchUpdate>) {
        Self::new_with_idle_release(db_path, notify, IDLE_RELEASE)
    }

    /// [`Self::new`] with an explicit connection-release window (for tests).
    pub fn new_with_idle_release(
        db_path: PathBuf,
        notify: Arc<dyn Fn() + Send + Sync>,
        idle_release: Duration,
    ) -> (SearchService, mpsc::Receiver<SearchUpdate>) {
        let (req_tx, req_rx) = mpsc::channel::<WorkerMsg>();
        let (update_tx, update_rx) = mpsc::channel::<SearchUpdate>();
        let latest_gen = Arc::new(AtomicU64::new(0));
        let in_flight: InFlight = Arc::new(Mutex::new(None));
        let db_path = Arc::new(Mutex::new(db_path));
        let release_ack: ReleaseAck = Arc::new(Mutex::new(None));

        let worker = Worker {
            req_rx,
            update_tx,
            notify,
            latest_gen: latest_gen.clone(),
            in_flight: in_flight.clone(),
            db_path: db_path.clone(),
            release_ack: release_ack.clone(),
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
                release_ack,
                handle: Some(handle),
            },
            update_rx,
        )
    }

    /// Start a new search, cancelling any in-flight one; returns the
    /// generation whose events to keep.
    pub fn search(&self, input: &str, options: SearchOptions) -> u64 {
        let generation = self.latest_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // Interrupt before enqueueing: an idle worker can dequeue the new
        // request and be mid-statement within microseconds.
        self.interrupt_stale();
        let _ = self.req_tx.send(WorkerMsg::Search(SearchRequest {
            generation,
            input: input.to_string(),
            options,
        }));
        generation
    }

    /// Drop the held connection and wait briefly. Cancels first, necessarily:
    /// a search enqueued just before this would otherwise run after the
    /// release, **reopening** the connection. Best-effort.
    pub fn release_connection(&self) {
        self.cancel();
        let (ack_tx, ack_rx) = mpsc::channel();
        *crate::lock_ok(&self.release_ack) = Some(ack_tx);
        if self.req_tx.send(WorkerMsg::ReleaseConnection).is_err() {
            return;
        }
        let _ = ack_rx.recv_timeout(RELEASE_WAIT);
    }

    pub fn cancel(&self) {
        self.latest_gen.fetch_add(1, Ordering::SeqCst);
        self.interrupt_stale();
    }

    pub fn set_db_path(&self, path: PathBuf) {
        *crate::lock_ok(&self.db_path) = path;
        self.cancel();
    }

    /// Kill the running statement — but only if the generation counter has
    /// already moved past the search that owns it. Interrupting the *newest*
    /// search does not cancel it, it fails it as `Search failed: interrupted`.
    fn interrupt_stale(&self) {
        let latest = self.latest_gen.load(Ordering::SeqCst);
        if let Some((generation, handle)) = crate::lock_ok(&self.in_flight).as_ref() {
            if *generation != latest {
                handle.interrupt();
            }
        }
    }

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
    } else if error_msg.contains("malformed") || error_msg.contains("corrupt") {
        format!("DATABASE_CORRUPTED: {}", error_msg)
    } else if error_msg.contains("fts5: syntax error") {
        "Search syntax error: the search term contains characters that cannot be processed."
            .to_string()
    } else {
        format!("Search failed: {}", error_msg)
    }
}

struct Worker {
    req_rx: mpsc::Receiver<WorkerMsg>,
    update_tx: mpsc::Sender<SearchUpdate>,
    notify: Arc<dyn Fn() + Send + Sync>,
    latest_gen: Arc<AtomicU64>,
    in_flight: InFlight,
    db_path: Arc<Mutex<PathBuf>>,
    release_ack: ReleaseAck,
    open: Option<OpenIndex>,
    /// How long `open` survives with no requests; [`IDLE_RELEASE`] outside tests.
    idle_release: Duration,
}

struct OpenIndex {
    conn: Connection,
    epoch: u64,
    path: PathBuf,
}

impl Worker {
    fn run(mut self) {
        loop {
            let first = match self.req_rx.recv_timeout(self.idle_release) {
                Ok(WorkerMsg::ReleaseConnection) => {
                    self.release();
                    continue;
                }
                Ok(WorkerMsg::Search(req)) => req,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // glibc parks the dropped connection's page cache in an
                    // arena; without the trim the process floor stays ~42 MiB
                    // higher. Once per idle transition, never a repeating tick.
                    if self.open.take().is_some() {
                        crate::platform::release_free_heap();
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            // Only the newest search matters, but a release must not be
            // coalesced away; anything enqueued ahead of it is already
            // cancelled and drops out rather than reopening the connection.
            let mut req = first;
            while let Ok(newer) = self.req_rx.try_recv() {
                match newer {
                    WorkerMsg::Search(newer) => req = newer,
                    WorkerMsg::ReleaseConnection => self.release(),
                }
            }
            if req.generation != self.latest_gen.load(Ordering::SeqCst) {
                continue;
            }
            self.handle(req);
        }
    }

    /// Drop the held connection and tell whoever asked; same trim as idle.
    fn release(&mut self) {
        if self.open.take().is_some() {
            crate::platform::release_free_heap();
        }
        if let Some(ack) = crate::lock_ok(&self.release_ack).take() {
            let _ = ack.send(());
        }
    }

    /// Take the connection, reopening when the path changed or the epoch
    /// moved — the file at the *same* path was replaced, which no path
    /// comparison can catch and would leave this worker on a deleted inode.
    fn take_connection(&mut self, db_path: &Path) -> Result<OpenIndex, String> {
        let epoch = db::index_epoch();
        if let Some(open) = self.open.take() {
            if open.epoch == epoch && open.path == db_path {
                return Ok(open);
            }
            // The handle on the old index must be gone before one on the new exists.
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
        // Publish the kill handle before the first statement runs.
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

        // A failed cascade may have failed *because* of this connection;
        // putting it back would wedge every later search behind it.
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

    /// A query long enough to be killed while executing, on its own thread.
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

    /// Cancel repeatedly, so every window an interrupt could land in is exercised.
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

    /// Killing the newest generation surfaces as an error instead of results.
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

    /// A superseded generation still dies promptly — a keystroke must not
    /// wait on the previous query.
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

        // The raw wording for an undecryptable page stays out of that bucket too.
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
