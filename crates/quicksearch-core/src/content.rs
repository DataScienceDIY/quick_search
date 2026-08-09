//! Parallel content extraction for one indexing root.
//!
//! The second half of a root's pipeline, and the sibling of [`crate::walk`]:
//! a pool of worker threads produces finished work over a bounded channel, and
//! the single writer drains it round-robin against every other root.
//!
//! **One feeder thread owns the only database connection**, paging through
//! the root's pending rows, while N workers do nothing but filesystem work. A
//! connection per worker would multiply SQLite's page cache by the pool size
//! (see [`crate::db::schema::PRAGMAS_WALK_READER`]).

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::config::Config;
use crate::extract::Registry;
use crate::file_handling::{decide_content, ContentOutcome, ExtractCursor};
use crate::indexing::should_abort;
use crate::walk::{try_recv_next, TryNext, WorkerStats};

/// Finished rows waiting for the writer.
///
/// Far shallower than the walk's 4096: an [`ExtractedRow`] carries up to
/// `maximum_text_size` of extracted text (256 KiB by default), so the walk's
/// depth would put gigabytes in flight. At 32 the ceiling is ~8 MiB per root.
const READY_CAP: usize = 32;

/// Rows fetched but not yet claimed by a worker. Small so the feeder does not
/// run arbitrarily far ahead of an I/O-bound pool; only ids and paths.
const QUEUE_AHEAD: usize = 256;

/// How many rows the feeder fetches per query. Large enough that a slow root
/// is not paying a round trip per file, small enough to stay inside
/// [`QUEUE_AHEAD`].
const FEED_PAGE: usize = 128;

/// One file's extracted content, ready to be written.
#[derive(Debug)]
pub struct ExtractedRow {
    pub file_id: i64,
    /// The `files.name` the FTS row is indexed under.
    pub name: String,
    pub outcome: ContentOutcome,
}

/// A row the feeder handed to the pool: everything a worker needs, and nothing
/// that would make it touch the database.
#[derive(Debug)]
struct Pending {
    file_id: i64,
    name: String,
    path: String,
    mime: Option<String>,
}

#[derive(Default)]
struct Queue {
    rows: Vec<Pending>,
    /// Set while the feeder is mid-query, holding rows that are in neither the
    /// queue nor a worker. Without it a worker could see an empty queue
    /// between two pages and declare the pass finished early.
    feeding: bool,
    /// True once the feeder has read the last page.
    drained: bool,
    done: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    idle: Condvar,
}

impl Shared {
    /// Claim a row, blocking while the feeder might still produce more.
    ///
    /// `None` only when the queue is empty *and* the feeder is finished — at
    /// that instant nobody is left who could add another row.
    fn take(&self) -> Option<Pending> {
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

    /// Claim the right to fetch one page, or `None` once the pass is over.
    /// Parks while the queue is already [`QUEUE_AHEAD`] deep.
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
    fn finish_feed(&self, rows: Vec<Pending>, last_page: bool) {
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

/// A running content pass. Draining it yields finished rows; dropping it stops
/// the workers and joins them.
pub struct ContentPass {
    rx: Option<mpsc::Receiver<ExtractedRow>>,
    handles: Vec<JoinHandle<()>>,
    feeder: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    stats: WorkerStats,
}

impl ContentPass {
    /// Non-blocking pull, for the writer loop multiplexing several roots.
    pub fn try_next(&mut self) -> TryNext<ExtractedRow> {
        try_recv_next(self.rx.as_ref())
    }

    /// A cheap, cloneable handle for reading pool activity while the pass is
    /// mutably borrowed by the writer loop; the sibling of
    /// [`crate::walk::ParallelWalk::worker_stats`].
    pub fn worker_stats(&self) -> WorkerStats {
        self.stats.clone()
    }

    /// Join the workers and report whether every one finished cleanly.
    /// See [`crate::walk::ParallelWalk::finish`].
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
        // No-op if the caller already called `finish`.
        self.finish();
    }
}

/// Page the root's pending rows into the queue from one read-only connection.
///
/// A failed query ends the pass rather than retrying: the rows stay
/// `content_state = 0` and the next run picks them up, which is the same
/// outcome as being interrupted.
fn feeder(shared: &Shared, db_path: &str, mut cursor: ExtractCursor, max_size: i64) {
    let conn = match crate::db::open::open_walk_reader(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            crate::log_warn!("content reader: {}", e);
            shared.shutdown();
            return;
        }
    };

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
        if let Some((id, _, _, _)) = page.last() {
            cursor.last_id = *id;
        }
        let rows = page
            .into_iter()
            .map(|(file_id, name, path, mime)| Pending {
                file_id,
                name,
                path,
                mime,
            })
            .collect();
        shared.finish_feed(rows, last_page);
        if last_page {
            return;
        }
    }
}

fn worker(
    shared: &Shared,
    tx: &mpsc::SyncSender<ExtractedRow>,
    registry: &Registry,
    config: &Config,
    stop_flag: &Arc<AtomicBool>,
    suspend_flag: &Arc<AtomicBool>,
    stats: &WorkerStats,
) {
    while let Some(row) = shared.take() {
        // Held for the whole of `decide_content`; that is the work the
        // progress line reports.
        let _busy = stats.enter();
        if should_abort(stop_flag, suspend_flag) {
            shared.shutdown();
            return;
        }
        let outcome = decide_content(&row.path, row.mime.as_deref(), registry, config);
        let sent = tx.send(ExtractedRow {
            file_id: row.file_id,
            name: row.name,
            outcome,
        });
        if sent.is_err() {
            // Receiver gone: the run was stopped or failed. Not an error.
            shared.shutdown();
            return;
        }
    }
}

/// Extract every pending row under `cursor`'s range, in parallel.
/// `workers` is the root's own count — the same value its walk uses —
/// clamped to 1..=64.
#[allow(clippy::too_many_arguments)]
pub fn extract_content(
    db_path: &str,
    cursor: &ExtractCursor,
    registry: Arc<Registry>,
    config: Config,
    stop_flag: Arc<AtomicBool>,
    suspend_flag: Arc<AtomicBool>,
    workers: usize,
) -> ContentPass {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        idle: Condvar::new(),
    });
    let max_size = i64::try_from(config.processing.maximum_text_file_size).unwrap_or(i64::MAX);

    let (tx, rx) = mpsc::sync_channel(READY_CAP);
    let stats = WorkerStats::new(workers.clamp(1, 64));
    let handles = (0..stats.total())
        .map(|_| {
            let (shared, tx) = (shared.clone(), tx.clone());
            let (registry, config) = (registry.clone(), config.clone());
            let (stop_flag, suspend_flag) = (stop_flag.clone(), suspend_flag.clone());
            let stats = stats.clone();
            crate::platform::spawn_worker("qs-extract", move || {
                crate::platform::set_background_priority();
                worker(
                    &shared,
                    &tx,
                    &registry,
                    &config,
                    &stop_flag,
                    &suspend_flag,
                    &stats,
                )
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
            feeder(&shared, &db_path, cursor, max_size)
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
    use crate::file_handling::{extract_scope_prepare, store_extracted};
    use crate::mime::FileType;
    use std::path::{Path, PathBuf};
    /// A path that does not exist yet — the caller builds the tree under it.
    fn tmp(tag: &str) -> PathBuf {
        crate::testutil::scratch_dir(tag).join("tree")
    }

    /// A tree of `n` text files under `root/sub`, plus an index holding a
    /// pending row for each.
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
                        path: f.to_str().unwrap(),
                        parent: d.to_str().unwrap(),
                        size: std::fs::metadata(&f).unwrap().len(),
                        mtime: 1,
                        inode: None,
                        device_id: None,
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
            Arc::new(AtomicBool::new(false)),
            workers,
        )
    }

    /// Drain a pass to exhaustion, blocking between polls the way the writer
    /// loop's outer sleep does.
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

    /// The pass is scoped by the cursor's path range, so a sibling root's
    /// rows are never touched.
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
        assert_eq!(
            store_extracted(&conn_mutex, &rows, &stop, &config).unwrap(),
            3
        );

        let state = |p: &Path| -> i64 {
            conn_mutex
                .lock()
                .unwrap()
                .query_row(
                    "SELECT content_state FROM files WHERE path = ?1",
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

        // A second run over the unchanged root reports it already extracted,
        // so progress reads "3 of 3" rather than "0 of 0".
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
        // The "nothing to do at t=0" corner: every worker must observe the pass
        // as finished rather than waiting for rows that will never arrive.
        let (tree, db) = seed("empty", &[("r1", 2)]);
        let mut pass = pass_for(&tree, &db, "nonexistent", 4);
        assert!(drain(&mut pass).is_empty());
        assert!(pass.finish());
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
            Arc::new(AtomicBool::new(false)),
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
        // Pull one, leave the rest queued and the channel full.
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
        // The termination protocol is racy by nature; run it enough times under
        // real contention that a premature exit would show up.
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

    /// The pass counts its own busy threads, which is what the progress line
    /// shows once a root leaves the walk behind.
    #[test]
    fn the_pool_reports_its_own_activity() {
        let (tree, db) = seed("stats", &[("r1", 300)]);
        let mut pass = pass_for(&tree, &db, "r1", 4);
        let stats = pass.worker_stats();
        assert_eq!(stats.total(), 4);

        // Nothing is drained, so the channel fills and every worker parks
        // mid-row inside `send` — busy by the display's definition.
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

    /// A file that vanished between the walk and extraction is a failure with
    /// a reason, not a silent skip: the row records why so it is not retried
    /// forever.
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
