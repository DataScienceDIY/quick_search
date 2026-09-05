//! Parallel content extraction for one indexing root: a worker pool produces
//! finished work over a bounded channel; the single writer drains it in
//! time-bounded turns, serving walks first so a fast pass waits rather than
//! holding up anyone's walk (see `indexing::pipeline`).
//!
//! **One feeder thread owns the only database connection** — a connection per
//! worker would multiply SQLite's page cache by the pool size
//! ([`crate::db::schema::PRAGMAS_WALK_READER`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::config::Config;
use crate::extract::Registry;
use crate::file_handling::{decide_content, ContentOutcome, ExtractCursor, ExtractScope};
use crate::walk::{try_recv_next, TryNext, WorkerStats};

/// Finished rows waiting for the writer. Far shallower than the walk's 4096:
/// a row carries up to `maximum_text_size` of text, so the walk's depth would
/// put gigabytes in flight; at 32 the ceiling is ~8 MiB per root.
const READY_CAP: usize = 32;

/// Rows fetched but not yet claimed; bounds how far the feeder runs ahead.
const QUEUE_AHEAD: usize = 256;

const FEED_PAGE: usize = 128;

/// One file's extracted content, ready to be written.
#[derive(Debug)]
pub struct ExtractedRow {
    pub file_id: i64,
    /// The path buffer the feeder built, carried through rather than split:
    /// see [`crate::db::repo::RowPath`].
    path: crate::db::repo::RowPath,
    pub outcome: ContentOutcome,
}

impl ExtractedRow {
    pub fn new(
        file_id: i64,
        path: crate::db::repo::RowPath,
        outcome: ContentOutcome,
    ) -> ExtractedRow {
        ExtractedRow {
            file_id,
            path,
            outcome,
        }
    }

    /// The whole path, parent and all — what progress and logs name the file by.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }
}

#[derive(Default)]
struct Queue {
    rows: Vec<crate::db::repo::PendingRow>,
    /// Feeder mid-query, holding rows in neither the queue nor a worker;
    /// without it a worker could see an empty queue between two pages and
    /// declare the pass finished early.
    feeding: bool,
    drained: bool,
    done: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    idle: Condvar,
    /// What the range held when the pass began. Set by the feeder just
    /// *behind* its first page, so the pool is never blocked on the scan;
    /// never set if the feeder could not count.
    totals: std::sync::OnceLock<ExtractScope>,
}

impl Shared {
    /// Claim a row. `None` only when the queue is empty *and* the feeder is
    /// finished — at that instant nobody is left who could add another row.
    fn take(&self) -> Option<crate::db::repo::PendingRow> {
        let mut q = crate::lock_ok(&self.queue);
        loop {
            if q.done {
                return None;
            }
            if let Some(row) = q.rows.pop() {
                // The feeder may be parked behind QUEUE_AHEAD.
                self.idle.notify_all();
                return Some(row);
            }
            if q.drained && !q.feeding {
                q.done = true;
                self.idle.notify_all();
                return None;
            }
            q = self
                .idle
                .wait(q)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Claim the right to fetch one page; parks at [`QUEUE_AHEAD`] deep.
    fn take_feed_slot(&self) -> Option<()> {
        let mut q = crate::lock_ok(&self.queue);
        loop {
            if q.done || q.drained {
                return None;
            }
            if q.rows.len() < QUEUE_AHEAD {
                q.feeding = true;
                return Some(());
            }
            q = self
                .idle
                .wait(q)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Publish a page and clear the in-flight flag together, under one lock —
    /// the indivisibility [`Shared::take`]'s end-of-pass test relies on.
    fn finish_feed(&self, rows: Vec<crate::db::repo::PendingRow>, last_page: bool) {
        let mut q = crate::lock_ok(&self.queue);
        // Reversed: `take` pops from the back, and rows should reach workers
        // in id order so a partial run leaves a contiguous prefix done.
        q.rows.extend(rows.into_iter().rev());
        q.feeding = false;
        if last_page {
            q.drained = true;
        }
        self.idle.notify_all();
    }

    fn shutdown(&self) {
        let mut q = crate::lock_ok(&self.queue);
        q.done = true;
        q.drained = true;
        self.idle.notify_all();
    }
}

/// A running content pass; dropping it stops the workers and joins them.
pub struct ContentPass {
    rx: Option<mpsc::Receiver<ExtractedRow>>,
    handles: Vec<JoinHandle<()>>,
    feeder: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    stats: WorkerStats,
}

impl ContentPass {
    pub fn try_next(&mut self) -> TryNext<ExtractedRow> {
        try_recv_next(self.rx.as_ref())
    }

    /// Cloneable handle for reading pool activity.
    pub fn worker_stats(&self) -> WorkerStats {
        self.stats.clone()
    }

    /// The range's counts as they stood when the pass began. `None` until
    /// the feeder has counted, and forever if it could not.
    pub fn totals(&self) -> Option<ExtractScope> {
        self.shared.totals.get().copied()
    }

    /// Join the workers; see [`crate::walk::ParallelWalk::finish`].
    pub fn finish(&mut self) -> bool {
        // Dropping the receiver first releases any worker parked in `send`.
        self.rx = None;
        self.shared.shutdown();
        let mut clean = true;
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                clean = false;
            }
        }
        if let Some(handle) = self.feeder.take() {
            if handle.join().is_err() {
                clean = false;
            }
        }
        clean
    }
}

impl Drop for ContentPass {
    fn drop(&mut self) {
        self.shared.shutdown();
        self.finish();
    }
}

/// Page the root's pending rows into the queue from one read-only connection.
/// A failed query ends the pass: the rows stay `content_state = 0` and the
/// next run picks them up — the same outcome as being interrupted.
fn feeder(shared: &Shared, db_path: &str, mut cursor: ExtractCursor, config: &Config) {
    let conn = match crate::db::open::open_walk_reader(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            crate::log_warn!("content reader: {}", e);
            shared.shutdown();
            return;
        }
    };

    // The count happens *behind* the first page: counting first idled the
    // whole pool for a scan that only feeds a progress bar. Deferring costs a
    // numerator that briefly runs ahead — a shape `RootProgress` reports
    // and deliberately does not clamp.
    let mut counted = false;
    let mut count_now = |conn: &rusqlite::Connection, cursor: &ExtractCursor| {
        if counted {
            return;
        }
        counted = true;
        match crate::file_handling::count_extract_scope(conn, cursor, config) {
            Ok(totals) => {
                let _ = shared.totals.set(totals);
            }
            Err(e) => crate::log_warn!("content reader: {}", e),
        }
    };

    let max_size = crate::file_handling::max_text_file_size(config);
    while shared.take_feed_slot().is_some() {
        let page =
            match crate::db::repo::pending_content_page(&conn, &cursor, max_size, FEED_PAGE as i64)
            {
                Ok(page) => page,
                Err(e) => {
                    crate::log_warn!("{}", e);
                    shared.shutdown();
                    return;
                }
            };
        let last_page = page.len() < FEED_PAGE;
        if let Some(row) = page.last() {
            cursor.last_id = row.file_id;
        }
        shared.finish_feed(page, last_page);
        count_now(&conn, &cursor);
        if last_page {
            return;
        }
    }
    // An empty range still counts — `an_empty_range_terminates_immediately`
    // pins that it reports a known zero rather than an unknown.
    count_now(&conn, &cursor);
}

fn worker(
    shared: &Shared,
    tx: &mpsc::SyncSender<ExtractedRow>,
    registry: &Registry,
    config: &Config,
    stop_flag: &Arc<AtomicBool>,
    stats: &WorkerStats,
) {
    // One per worker, for the whole pass: the container and stream buffers
    // inside it are what every extraction stages through.
    let mut scratch = crate::extract::Scratch::new(config);
    while let Some(row) = shared.take() {
        let _busy = stats.enter();
        if stop_flag.load(Ordering::Relaxed) {
            shared.shutdown();
            return;
        }
        let outcome = decide_content(
            row.path.as_str(),
            row.mime.as_deref(),
            registry,
            config,
            &mut scratch,
        );
        let sent = tx.send(ExtractedRow {
            file_id: row.file_id,
            path: row.path,
            outcome,
        });
        if sent.is_err() {
            // Receiver gone: the run was stopped or failed. Not an error.
            shared.shutdown();
            return;
        }
    }
}

/// Extract every pending row under `cursor`'s range, in parallel. `workers`
/// is the root's own count, clamped to 1..=64.
pub fn extract_content(
    db_path: &str,
    cursor: &ExtractCursor,
    registry: Arc<Registry>,
    config: Config,
    stop_flag: Arc<AtomicBool>,
    workers: usize,
) -> ContentPass {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        idle: Condvar::new(),
        totals: std::sync::OnceLock::new(),
    });

    let (tx, rx) = mpsc::sync_channel(READY_CAP);
    let stats = WorkerStats::new(workers.clamp(1, 64));
    let handles = (0..stats.total())
        .map(|_| {
            let (shared, tx) = (shared.clone(), tx.clone());
            let (registry, config) = (registry.clone(), config.clone());
            let stop_flag = stop_flag.clone();
            let stats = stats.clone();
            crate::platform::spawn_worker("qs-extract", move || {
                crate::platform::set_background_priority();
                worker(&shared, &tx, &registry, &config, &stop_flag, &stats)
            })
        })
        .collect();
    // The workers must hold the only senders, or `try_recv` never reports
    // the end of the pass.
    drop(tx);

    let feeder_handle = {
        let (shared, db_path, cursor) = (shared.clone(), db_path.to_string(), cursor.clone());
        crate::platform::spawn_worker("qs-feeder", move || {
            crate::platform::set_background_priority();
            feeder(&shared, &db_path, cursor, &config)
        })
    };

    ContentPass {
        rx: Some(rx),
        handles,
        feeder: Some(feeder_handle),
        shared,
        stats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use crate::db::open_or_recreate;
    use crate::db::repo::{self, insert_file, NewFile};
    use crate::file_handling::{store_extracted, ExtractScope, Stored};
    use crate::mime::FileType;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The writer's oversize sweep, then the pass's own count.
    fn extract_scope_prepare(
        conn_mutex: &Arc<Mutex<rusqlite::Connection>>,
        cursor: &ExtractCursor,
        config: &Config,
    ) -> Result<ExtractScope, String> {
        let conn = crate::lock_ok(conn_mutex);
        crate::file_handling::mark_oversize_pending_na(&conn, cursor, config)?;
        crate::file_handling::count_extract_scope(&conn, cursor, config)
    }
    fn tmp(tag: &str) -> PathBuf {
        crate::testutil::scratch_dir(tag).join("tree")
    }

    /// A tree of text files, plus an index holding a pending row for each.
    fn seed(tag: &str, dirs: &[(&str, usize)]) -> (PathBuf, PathBuf) {
        let tree = tmp(&format!("{}-tree", tag));
        let db = tmp(&format!("{}-db", tag));
        let mut conn = open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
        let tx = conn.transaction().unwrap();
        for (dir, n) in dirs {
            let d = tree.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            for i in 0..*n {
                let f = d.join(format!("f{:04}.txt", i));
                std::fs::write(&f, format!("sphinx of black quartz {} {}", dir, i)).unwrap();
                insert_file(
                    &tx,
                    &NewFile {
                        name: f.file_name().unwrap().to_str().unwrap(),
                        parent: &crate::file_handling::dir_to_db_parent(&d),
                        size: std::fs::metadata(&f).unwrap().len(),
                        mtime: 1,
                        mime: Some("text/plain"),
                        ftype: FileType::TEXT,
                        hash: None,
                        needs_content: true,
                    },
                )
                .unwrap()
                .expect("unique path");
            }
        }
        tx.commit().unwrap();
        drop(conn);
        (tree, db)
    }

    fn pass_for(tree: &Path, db: &Path, sub: &str, workers: usize) -> ContentPass {
        extract_content(
            db.to_str().unwrap(),
            &ExtractCursor::for_root(tree.join(sub).to_str().unwrap()),
            Arc::new(Registry::default_set()),
            Config::default(),
            Arc::new(AtomicBool::new(false)),
            workers,
        )
    }

    fn drain(pass: &mut ContentPass) -> Vec<ExtractedRow> {
        let mut out = Vec::new();
        loop {
            match pass.try_next() {
                TryNext::Item(row) => out.push(row),
                TryNext::Empty => thread::sleep(std::time::Duration::from_millis(1)),
                TryNext::Finished => return out,
            }
        }
    }

    #[test]
    fn every_pending_row_is_yielded_exactly_once() {
        let (tree, db) = seed("once", &[("r1", 250)]);
        let mut pass = pass_for(&tree, &db, "r1", 4);
        let rows = drain(&mut pass);
        assert!(pass.finish(), "no worker panicked");

        assert_eq!(rows.len(), 250);
        let ids: std::collections::HashSet<i64> = rows.iter().map(|r| r.file_id).collect();
        assert_eq!(ids.len(), 250, "no id may be yielded twice");
        assert!(
            rows.iter()
                .all(|r| matches!(r.outcome, ContentOutcome::Done { .. })),
            "every plaintext file extracts"
        );

        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn the_pass_is_scoped_to_its_root_range() {
        let (tree, db) = seed("scope", &[("r1", 3), ("r2", 3)]);
        let conn_mutex = Arc::new(Mutex::new(
            open_or_recreate(db.to_str().unwrap(), "trigram").unwrap(),
        ));
        let config = Config::default();
        let cursor = ExtractCursor::for_root(tree.join("r1").to_str().unwrap());

        let scope = extract_scope_prepare(&conn_mutex, &cursor, &config).unwrap();
        assert_eq!(scope.pending, 3, "only r1's files are in range");
        assert_eq!(scope.already_done, 0, "nothing extracted yet");

        let mut pass = pass_for(&tree, &db, "r1", 2);
        let rows = drain(&mut pass);
        assert!(pass.finish());
        assert_eq!(rows.len(), 3);

        let stop = Arc::new(AtomicBool::new(false));
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            store_extracted(&conn_mutex, &rows, &stop, &config, far).unwrap(),
            Stored {
                consumed: 3,
                written: 3
            }
        );

        let state = |p: &Path| -> i64 {
            conn_mutex
                .lock()
                .unwrap()
                .query_row(
                    "SELECT content_state FROM files WHERE parent || name = ?1",
                    rusqlite::params![p.to_str().unwrap()],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(state(&tree.join("r1/f0000.txt")), repo::STATE_DONE);
        assert_eq!(
            state(&tree.join("r2/f0000.txt")),
            repo::STATE_PENDING,
            "out-of-range row untouched"
        );

        // A second run over the unchanged root reads "3 of 3", not "0 of 0".
        let scope2 = extract_scope_prepare(&conn_mutex, &cursor, &config).unwrap();
        assert_eq!((scope2.pending, scope2.already_done), (0, 3));

        let hits: i64 = conn_mutex
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH '\"sphinx\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 3);

        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn an_empty_range_terminates_immediately() {
        let (tree, db) = seed("empty", &[("r1", 2)]);
        let mut pass = pass_for(&tree, &db, "nonexistent", 4);
        assert!(drain(&mut pass).is_empty());
        assert!(pass.finish());
        // An empty range is a known zero, not an unknown.
        assert_eq!(
            pass.totals(),
            Some(ExtractScope {
                pending: 0,
                already_done: 0
            })
        );
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    /// The count is what stood at the start: rows this pass writes are not
    /// inside it.
    #[test]
    fn the_pass_counts_its_range_before_it_starts() {
        let (tree, db) = seed("totals", &[("r1", 3), ("r2", 2)]);
        let mut pass = pass_for(&tree, &db, "r1", 2);
        let rows = drain(&mut pass);
        assert!(pass.finish());
        assert_eq!(rows.len(), 3);
        assert_eq!(
            pass.totals(),
            Some(ExtractScope {
                pending: 3,
                already_done: 0
            }),
            "only r1's rows, all of them pending when the pass began"
        );
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    /// `store_extracted` stops at its deadline but always gets at least one
    /// row down, so a caller looping on it cannot spin.
    #[test]
    fn store_extracted_honours_its_deadline_but_always_makes_progress() {
        let (tree, db) = seed("deadline", &[("r1", 5)]);
        let conn_mutex = Arc::new(Mutex::new(
            open_or_recreate(db.to_str().unwrap(), "trigram").unwrap(),
        ));
        let config = Config::default();
        let mut pass = pass_for(&tree, &db, "r1", 2);
        let rows = drain(&mut pass);
        assert!(pass.finish());
        assert_eq!(rows.len(), 5);
        let stop = Arc::new(AtomicBool::new(false));

        // A deadline already gone by: one row, then out.
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            store_extracted(&conn_mutex, &rows, &stop, &config, past).unwrap(),
            Stored {
                consumed: 1,
                written: 1
            }
        );
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            store_extracted(&conn_mutex, &rows[1..], &stop, &config, far).unwrap(),
            Stored {
                consumed: 4,
                written: 4
            }
        );
        let done: i64 = conn_mutex
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM files WHERE content_state = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(done, 5, "every row landed across the two calls");

        // Stopped before it starts: nothing consumed.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            store_extracted(&conn_mutex, &rows, &stop, &config, far).unwrap(),
            Stored::default()
        );

        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn an_already_stopped_pass_does_not_run_to_completion() {
        let (tree, db) = seed("stop", &[("r1", 400)]);
        let mut pass = extract_content(
            db.to_str().unwrap(),
            &ExtractCursor::for_root(tree.join("r1").to_str().unwrap()),
            Arc::new(Registry::default_set()),
            Config::default(),
            Arc::new(AtomicBool::new(true)),
            4,
        );
        assert!(drain(&mut pass).len() < 400);
        assert!(pass.finish());
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn dropping_the_pass_early_does_not_hang() {
        // Workers blocked in `send` must be released by the receiver going
        // away, or `Drop` would join threads that never wake.
        let (tree, db) = seed("early-drop", &[("r1", 500)]);
        let mut pass = pass_for(&tree, &db, "r1", 4);
        loop {
            match pass.try_next() {
                TryNext::Item(_) => break,
                TryNext::Empty => thread::sleep(std::time::Duration::from_millis(1)),
                TryNext::Finished => break,
            }
        }
        drop(pass); // must return, not deadlock
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn repeated_passes_agree_on_the_result_set() {
        // The termination protocol is racy by nature; run it under real
        // contention until a premature exit would show up.
        let (tree, db) = seed("repeat", &[("r1", 120)]);
        for run in 0..20 {
            let mut pass = pass_for(&tree, &db, "r1", 4);
            let rows = drain(&mut pass);
            assert!(pass.finish(), "run {}", run);
            assert_eq!(rows.len(), 120, "run {}", run);
        }
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn the_pool_reports_its_own_activity() {
        let (tree, db) = seed("stats", &[("r1", 300)]);
        let mut pass = pass_for(&tree, &db, "r1", 4);
        let stats = pass.worker_stats();
        assert_eq!(stats.total(), 4);

        // Nothing is drained, so the channel fills and every worker parks
        // mid-row inside `send`.
        let mut peak = 0;
        for _ in 0..500 {
            peak = peak.max(stats.active());
            if peak == 4 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(peak, 4, "every worker busy while the channel is full");

        assert_eq!(drain(&mut pass).len(), 300);
        assert!(pass.finish());
        assert_eq!(stats.active(), 0, "a finished pool is idle");

        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }

    /// Vanished files fail with a reason so they are not retried forever.
    #[test]
    fn a_missing_file_is_reported_as_failed() {
        let (tree, db) = seed("missing", &[("r1", 2)]);
        std::fs::remove_file(tree.join("r1/f0000.txt")).unwrap();
        let mut pass = pass_for(&tree, &db, "r1", 2);
        let rows = drain(&mut pass);
        assert!(pass.finish());
        assert_eq!(rows.len(), 2, "both rows are still reported");
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r.outcome, ContentOutcome::Failed(_)))
                .count(),
            1
        );
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_file(&db).ok();
    }
}
