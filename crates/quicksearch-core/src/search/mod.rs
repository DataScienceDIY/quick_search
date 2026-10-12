//! Interruptible, streaming search service.
//!
//! One worker thread owns the cascade; [`SearchService::search`] bumps a
//! generation and streams [`SearchUpdate`] events back. Cancellation is one
//! layer, the generation: the cascade checks it between rows and passes, and
//! a progress handler ([`arm_cancel`]) checks it inside SQLite.

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

/// Idle window before the connection is released.
///
/// Long, because a release drops the search cache
/// ([`db::schema::pragmas_search`]) and the next keystroke pays to refill it:
/// on a 200k-file index the first query after a release costs 129 ms
/// unencrypted and 192 ms encrypted, against ~10 ms warm, because a keyed
/// refill decrypts every page. (The keyed figure predates `HMAC_MODE = Off`;
/// see [`db::schema::HMAC_MODE`].)
///
/// Removals do not wait on it: the service registers with [`db::hold_index`],
/// so [`db::remove_index_files`] has it send [`WorkerMsg::ReleaseConnection`]
/// before every delete of its index, a run's own wipe included, because
/// Windows fails the delete while any handle is open. Nor does a held
/// connection stop the WAL truncating: a read mark lasts for a statement, not
/// a connection.
const IDLE_RELEASE: Duration = Duration::from_secs(30 * 60);

/// One search result. `rank` is the sort key (lower = better): integer part
/// = cascade stage (1–11), fraction = tiebreak. Passes stream strongest
/// first; within one, hits arrive as found, so a later batch can outrank an
/// earlier one.
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
    /// stage; absent only where no window could be cut.
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
            SearchUpdate::Hits { generation, .. }
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

type ReleaseAck = Arc<Mutex<Option<mpsc::Sender<()>>>>;

/// How long [`SearchService::release_connection`] waits for the worker's answer.
const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// The release is a message rather than a flag because the worker is parked
/// in `recv_timeout`, where a flag would not reach it.
enum WorkerMsg {
    Search(SearchRequest),
    /// Drop the held connection so the index file can be deleted or replaced.
    ReleaseConnection,
}

pub struct SearchService {
    control: Arc<Control>,
    handle: Option<JoinHandle<()>>,
    /// Has [`db::remove_index_files`] release the connection before deleting
    /// the index it is on.
    _holder: db::IndexHolder,
}

/// The service's side of what it shares with the worker. The
/// [`db::hold_index`] callback reaches it only through a `Weak`: a strong
/// `req_tx` there would keep the worker alive past
/// [`SearchService::shutdown`].
struct Control {
    req_tx: mpsc::Sender<WorkerMsg>,
    latest_gen: Arc<AtomicU64>,
    db_path: Arc<Mutex<PathBuf>>,
    release_ack: ReleaseAck,
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
        let db_path = Arc::new(Mutex::new(db_path));
        let release_ack: ReleaseAck = Arc::new(Mutex::new(None));

        let worker = Worker {
            req_rx,
            update_tx,
            notify,
            latest_gen: latest_gen.clone(),
            db_path: db_path.clone(),
            release_ack: release_ack.clone(),
            open: None,
            idle_release,
        };
        let handle = std::thread::Builder::new()
            .name("qs-search".into())
            .spawn(move || worker.run())
            .expect("spawn search worker");

        let control = Arc::new(Control {
            req_tx,
            latest_gen,
            db_path,
            release_ack,
        });
        let weak = Arc::downgrade(&control);
        let holder = db::hold_index(move |path| {
            let Some(control) = weak.upgrade() else {
                return;
            };
            if crate::lock_ok(&control.db_path).as_path() == path {
                control.release();
            }
        });
        (
            SearchService {
                control,
                handle: Some(handle),
                _holder: holder,
            },
            update_rx,
        )
    }

    /// Start a new search, cancelling any in-flight one; returns the
    /// generation whose events to keep.
    pub fn search(&self, input: &str, options: SearchOptions) -> u64 {
        let control = &self.control;
        let generation = control.latest_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = control.req_tx.send(WorkerMsg::Search(SearchRequest {
            generation,
            input: input.to_string(),
            options,
        }));
        generation
    }

    /// Drop the held connection and wait briefly; best-effort.
    pub fn release_connection(&self) {
        self.control.release();
    }

    pub fn cancel(&self) {
        self.control.cancel();
    }

    pub fn set_db_path(&self, path: PathBuf) {
        *crate::lock_ok(&self.control.db_path) = path;
        self.cancel();
    }

    pub fn shutdown(self) {
        self.cancel();
        let SearchService {
            control, handle, ..
        } = self;
        drop(control);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Control {
    /// Drop the held connection and wait briefly; best-effort. Cancels first: a
    /// search enqueued just before this would otherwise run after the release
    /// and reopen the connection.
    fn release(&self) {
        self.cancel();
        let (ack_tx, ack_rx) = mpsc::channel();
        *crate::lock_ok(&self.release_ack) = Some(ack_tx);
        if self.req_tx.send(WorkerMsg::ReleaseConnection).is_err() {
            return;
        }
        let _ = ack_rx.recv_timeout(RELEASE_WAIT);
    }

    fn cancel(&self) {
        self.latest_gen.fetch_add(1, Ordering::SeqCst);
    }
}

/// SQLite's own wording for a damaged file: the `SQLITE_CORRUPT` error string
/// and a schema that will not parse. Whole phrases, because the message also
/// carries user text, the index path or a glob, that may say "corrupt".
const DAMAGE_PHRASES: [&str; 2] = [
    "database disk image is malformed",
    "malformed database schema",
];

/// Map a SQLite-level error to the sentence a frontend shows as it is. These
/// are user-facing sentences, not tags: nothing keys off their wording.
pub fn classify_sql_err(error_msg: &str) -> String {
    if error_msg.starts_with(db::KEY_MISMATCH_PREFIX) {
        // First, so nothing in the detail (a path) ever reads as damage: the
        // advice to clear would delete an intact index the right password
        // opens. The tag is for code; the user gets the detail.
        db::key_mismatch_parts(error_msg)
            .map_or(error_msg, |(_, detail)| detail)
            .to_string()
    } else if DAMAGE_PHRASES.iter().any(|p| error_msg.contains(p)) {
        format!(
            "The index is damaged ({}). Use Clear index on the Manage Index tab, then \
             index again.",
            error_msg
        )
    } else if error_msg.contains("fts5: syntax error") {
        "Search syntax error: the search term contains characters that cannot be processed."
            .to_string()
    } else {
        format!("Search failed: {}", error_msg)
    }
}

/// VDBE instructions between two looks at the generation from inside a
/// statement. A look is one relaxed load, and a superseded statement dies
/// within this many instructions, microseconds.
///
/// Measured neutral against the `interrupt()` it replaced (200k files,
/// plain and keyed; a keystroke makes ~1,400 calls, a fuzzy one up to
/// ~5,800). `benches/search_perf.rs`, medians of 6 runs before and 4 after:
/// warm `quar`..`quartzi` at the shipped cache 10.9 → 10.6 ms keyed and
/// 13.3 → 13.9 plain; the fuzzy pairing 46–189 → 48–197 ms keyed and
/// 52–186 → 52–193 plain, every row inside the ±5% run-to-run spread.
/// Interleaved on one connection, armed against unarmed over 20 rounds:
/// best times within 1% on 34 of 40 rows, 3.6% at worst, +0.2% on average.
const PROGRESS_OPS: std::ffi::c_int = 1000;

/// Have SQLite kill `conn`'s running statement once `latest` moves past
/// `generation`, until [`disarm_cancel`]. The handler runs inside every
/// statement, so a cancel that lands between the cascade's last check and a
/// pass's first step is not lost, as an
/// [`rusqlite::InterruptHandle::interrupt`] there would be: SQLite clears
/// that flag when a statement starts with no other active. The kill fails the
/// statement as `SQLITE_INTERRUPT`, which [`cascade::run`] reads as
/// cancellation. The newest generation is never killed.
pub(crate) fn arm_cancel(
    conn: &Connection,
    generation: u64,
    latest: Arc<AtomicU64>,
) -> rusqlite::Result<()> {
    conn.progress_handler(
        PROGRESS_OPS,
        Some(move || latest.load(Ordering::Relaxed) != generation),
    )
}

/// Clear [`arm_cancel`]'s handler: it answers for one generation only.
pub(crate) fn disarm_cancel(conn: &Connection) -> rusqlite::Result<()> {
    conn.progress_handler(0, None::<fn() -> bool>)
}

struct Worker {
    req_rx: mpsc::Receiver<WorkerMsg>,
    update_tx: mpsc::Sender<SearchUpdate>,
    notify: Arc<dyn Fn() + Send + Sync>,
    latest_gen: Arc<AtomicU64>,
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
            let mut req = match self.req_rx.recv_timeout(self.idle_release) {
                Ok(WorkerMsg::ReleaseConnection) => {
                    self.release();
                    continue;
                }
                Ok(WorkerMsg::Search(req)) => req,
                // Once per idle transition, never a repeating tick.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.drop_connection();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            // Only the newest search matters, but a release must not be
            // coalesced away; anything enqueued ahead of it is already
            // cancelled and drops out rather than reopening the connection.
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

    /// Drop the held connection, if any, and return freed heap.
    fn drop_connection(&mut self) {
        if self.open.take().is_some() {
            crate::platform::release_free_heap();
        }
    }

    /// Drop the held connection and tell whoever asked.
    fn release(&mut self) {
        self.drop_connection();
        if let Some(ack) = crate::lock_ok(&self.release_ack).take() {
            let _ = ack.send(());
        }
    }

    /// Take the connection, reopening when the path changed or the epoch
    /// moved. A moved epoch means the file at the same path was replaced, which
    /// no path comparison catches; the worker would be left on a deleted inode.
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
        match self.search(&req) {
            Ok(Some(Outcome { total, limited })) => self.send(SearchUpdate::Completed {
                generation,
                total,
                limited,
            }),
            // Cancelled: the newer generation owns the UI now.
            Ok(None) => {}
            Err(message) => self.send(SearchUpdate::Error {
                generation,
                message,
            }),
        }
    }

    /// Run one request's cascade on the held connection, streaming its hits.
    /// `Ok(None)` means cancelled; `Err` is the message to show.
    fn search(&mut self, req: &SearchRequest) -> Result<Option<Outcome>, String> {
        let generation = req.generation;
        let split = split_for_cascade(&req.input).map_err(|e| e.to_string())?;
        let db_path = crate::lock_ok(&self.db_path).clone();
        let open = self
            .take_connection(&db_path)
            .map_err(|e| classify_sql_err(&e))?;
        arm_cancel(&open.conn, generation, self.latest_gen.clone())
            .map_err(|e| classify_sql_err(&e.to_string()))?;

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
        let disarmed = disarm_cancel(&open.conn);

        // A failed cascade may have failed because of this connection; putting
        // it back would wedge every later search behind it. Nor does one go
        // back still armed for this generation.
        if outcome.is_ok() && disarmed.is_ok() {
            self.open = Some(open);
        }
        outcome.map_err(|e| classify_sql_err(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A million steps and zero rows: the shape that gave the cascade no row
    /// to check the generation on.
    fn zero_row_scan(conn: &Connection) -> rusqlite::Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000) \
             SELECT x FROM c WHERE x < 0",
            [],
            |row| row.get(0),
        )
        .optional()
    }

    /// The cancel lands before the statement starts, where an interrupt was
    /// cleared and lost.
    #[test]
    fn a_cancel_before_the_first_step_still_kills_a_zero_row_scan() {
        let conn = Connection::open_in_memory().unwrap();
        arm_cancel(&conn, 7, Arc::new(AtomicU64::new(8))).unwrap();
        let err = zero_row_scan(&conn).expect_err("a superseded generation must be killed");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted),
            "unexpected error: {}",
            err
        );
    }

    /// Killing the newest generation would fail it as `Search failed:
    /// interrupted` rather than cancel it.
    #[test]
    fn the_newest_generation_is_never_killed() {
        let conn = Connection::open_in_memory().unwrap();
        arm_cancel(&conn, 7, Arc::new(AtomicU64::new(7))).unwrap();
        assert_eq!(zero_row_scan(&conn).unwrap(), None);
    }

    #[test]
    fn a_disarmed_connection_runs_after_a_cancel() {
        let conn = Connection::open_in_memory().unwrap();
        let latest = Arc::new(AtomicU64::new(7));
        arm_cancel(&conn, 7, latest.clone()).unwrap();
        latest.store(8, Ordering::SeqCst);
        disarm_cancel(&conn).unwrap();
        assert_eq!(zero_row_scan(&conn).unwrap(), None);
    }

    /// A kill by the handler ends the search as cancelled, never as `Search
    /// failed: interrupted`. The generation moves once the first hit is out,
    /// and the rest of the scan is rows the `LIKE` rejects inside SQLite.
    /// That the handler, not the scan's own end, stops it is shown a level
    /// down, in `cascade::tests::the_armed_handler_kills_a_pass_mid_scan`.
    #[test]
    fn a_kill_by_the_handler_reads_as_cancellation() {
        let (_dir, path) = crate::testutil::Scratch::db("handler-kill");
        crate::testutil::seed_index(
            &path,
            &crate::testutil::SeedSpec {
                files: 5_000,
                needle_names: 1,
                content_every: usize::MAX,
                ..Default::default()
            },
        );
        let conn = db::open::open_search_reader(&path.to_string_lossy()).unwrap();
        let latest = Arc::new(AtomicU64::new(7));
        arm_cancel(&conn, 7, latest.clone()).unwrap();

        let split = split_for_cascade(crate::testutil::NEEDLE).unwrap();
        let mut batches = 0usize;
        let outcome = cascade::run(
            &conn,
            &split,
            &SearchOptions::default(),
            7,
            &latest,
            &mut |_batch| {
                batches += 1;
                latest.store(8, Ordering::SeqCst);
            },
        );
        assert_eq!(outcome, Ok(None));
        assert_eq!(batches, 1, "no batch after the generation moved");
    }

    #[test]
    fn key_mismatch_is_never_classified_as_corruption() {
        // A path that says "corrupt" and "malformed" must not make an intact
        // index read as damaged, whatever the cause.
        let detail = "index at /tmp/corrupt/malformed.sqlite: wrong password (or the file is \
                      not a QuickSearch index)";
        for cause in ["wrong-password", "not-encrypted", "password-required"] {
            let msg = format!("{}{}: {}", db::KEY_MISMATCH_PREFIX, cause, detail);
            let classified = classify_sql_err(&msg);
            assert_eq!(classified, detail, "{cause}: the detail without the tag");
            assert!(!classified.contains("KEY_MISMATCH:"), "{classified}");
            assert!(!classified.contains("damaged"), "{classified}");
        }

        // A tag with no known cause is passed on whole, still not as damage.
        let odd = format!("{}{}", db::KEY_MISMATCH_PREFIX, detail);
        assert_eq!(classify_sql_err(&odd), odd);

        // The raw wording for an undecryptable page stays out of that bucket too.
        let raw = "Failed to read database at /tmp/x.sqlite: file is not a database";
        assert!(!classify_sql_err(raw).contains("damaged"));
    }

    #[test]
    fn corruption_and_syntax_classification_still_work() {
        let raw = "database disk image is malformed";
        let damaged = classify_sql_err(raw);
        assert!(damaged.starts_with("The index is damaged ("), "{damaged}");
        assert!(damaged.contains(raw), "{damaged}");
        assert!(damaged.contains("Clear index"), "{damaged}");
        assert!(classify_sql_err("fts5: syntax error near \"NEAR\"").starts_with("Search syntax"));
        assert!(classify_sql_err("no such table: files").starts_with("Search failed:"));
        let schema = "Failed to read database at /x.sqlite: malformed database schema (files)";
        assert!(classify_sql_err(schema).starts_with("The index is damaged ("));
    }

    /// The words alone, from the user's own path, are no damage.
    #[test]
    fn a_path_that_says_corrupt_is_not_damage() {
        let path = "/home/u/corrupt-disk-backup/index.sqlite";
        for msg in [
            format!("Failed to open database at {path}: unable to open database file"),
            format!("Failed to read database at {path}: disk I/O error"),
            format!("session ignore filter: malformed glob in {path}"),
        ] {
            let classified = classify_sql_err(&msg);
            assert!(!classified.contains("damaged"), "{classified}");
            assert_eq!(classified, format!("Search failed: {msg}"));
        }
        let real = format!("Failed to read database at {path}: database disk image is malformed");
        assert!(classify_sql_err(&real).starts_with("The index is damaged ("));
    }
}
