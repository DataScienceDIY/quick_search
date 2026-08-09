//! One run of the indexer: the per-root [`RootPipeline`] and the
//! writer loop in [`IndexingService::run_indexing`] they funnel into.

use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db;
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    cleanup_stale_index_entries, count_tree_entries_fast, extract_scope_prepare,
    fts_finalize_after_text_indexing, normalize_root_string, process_batch_inserts,
    process_batch_updates, store_extracted, ExtractCursor, FileIndexAction, OwnedNewFile,
};
use crate::walk::{thread_count_for, walk_indexable_files, ParallelWalk, TryNext, WalkEvent};

use super::*;

/// Collect rows whose parent directory the walk never reached: a directory
/// deleted wholesale — or newly excluded — is never read, so per-directory
/// reconciliation cannot see its rows.
///
/// Two kinds of absence are *not* deletions and are filtered out:
///
/// - A parent under a directory the walk could not read. Its children were
///   never discovered, so their absence proves nothing.
/// - A path reached by resolving a symlink, whose row's parent may lie
///   outside every root; `aliased` records that the file itself was seen.
fn sweep_unvisited_parents(
    conn_mutex: &Arc<Mutex<Connection>>,
    root: &str,
    seen_dirs: &HashSet<String>,
    unreadable: &crate::file_handling::UnreadableDirs,
    aliased: &HashSet<String>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    // Same keyset range the extraction cursor uses: `[root + "/", root + "0")`.
    let range = ExtractCursor::for_root(root);
    let conn = crate::lock_ok(conn_mutex);

    // Collected rather than streamed into the second query: both borrow the
    // same connection, and the outer statement is still live while iterating.
    let mut unvisited: Vec<String> = Vec::new();
    repo::for_each_parent_in_range(&conn, &range.lo, &range.hi, |parent| {
        if !seen_dirs.contains(&parent) && !unreadable.covers(&parent) {
            unvisited.push(parent);
        }
    })?;

    for parent in unvisited {
        for path in repo::paths_in_dir(&conn, &parent)? {
            if !aliased.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// Size of the write-ahead log on disk, or 0 if it is absent.
///
/// SQLite will not bound the WAL: the log only *shrinks* when the writer
/// opens a transaction at an instant no reader holds a read mark — a lock
/// SQLite tries once, without retrying. A run keeps a reader per root
/// querying from start to finish, so the log appends for the whole run. An
/// explicit checkpoint retries the same lock under `busy_timeout` and wins,
/// which is why `run_indexing` forces one every `maximum_wal_size` bytes.
fn wal_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Flips an [`AtomicBool`] when dropped. Held by `run_indexing` so the
/// per-root count subprocesses die on every exit path of a run.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// One root's in-flight indexing state, owned by the writer loop.
pub(super) struct RootPipeline {
    pub(super) root: String,
    pub(super) walk: ParallelWalk,
    /// This root's walk denominator; 0 = not yet known. From last run's
    /// stored count, or the count scan for a root that has none.
    pub(super) count_total: Arc<AtomicUsize>,
    /// Threads this root gets, for both the walk and extraction.
    pub(super) workers: usize,
    pub(super) pending_updates: Vec<OwnedNewFile>,
    pub(super) pending_inserts: Vec<OwnedNewFile>,
    pub(super) walked: usize,
    pub(super) walk_clean: bool,
    pub(super) phase: RootPhase,
    /// The running content pass, once this root's walk has finished.
    pub(super) content: Option<crate::content::ContentPass>,
    pub(super) extract_total: usize,
    pub(super) extracted: usize,
    pub(super) current_file: Option<String>,
    /// When this root's current phase began, for the one line each phase logs
    /// when it ends.
    pub(super) phase_started: Instant,
}

impl RootPipeline {
    /// End the current phase and return how long it took, restarting the clock
    /// for the next one.
    fn phase_elapsed(&mut self) -> Duration {
        let started = std::mem::replace(&mut self.phase_started, Instant::now());
        started.elapsed()
    }
}

/// `n` per second, or `None` when the interval is too short to divide by —
/// a sub-millisecond walk would otherwise report a rate in the millions.
fn per_second(n: usize, elapsed: Duration) -> Option<f64> {
    let secs = elapsed.as_secs_f64();
    (secs >= 0.001).then(|| n as f64 / secs)
}

/// How long the writer loop waits when every root has momentarily run dry.
/// A ceiling, not a delay: the loop parks on a channel and any producer
/// wakes it at once.
const IDLE_BACKOFF: Duration = Duration::from_millis(2);

/// Report what the per-run warning throttles counted but did not print.
/// See [`crate::log::Throttle`].
fn report_run_warnings() {
    let (failed, suppressed) = crate::file_handling::hash_failure_counts();
    if failed > 0 {
        crate::log_warn!(
            "{} file{} could not be read to hash{}",
            failed,
            if failed == 1 { "" } else { "s" },
            if suppressed > 0 {
                format!(" ({} similar warnings not shown)", suppressed)
            } else {
                String::new()
            }
        );
    }
}

/// One phase's timing line: "12,345 files in 41.2s (300/s)".
fn phase_summary(n: usize, noun: &str, elapsed: Duration) -> String {
    match per_second(n, elapsed) {
        Some(rate) => format!(
            "{} {} in {:.1}s ({:.0}/s)",
            n,
            noun,
            elapsed.as_secs_f64(),
            rate
        ),
        None => format!("{} {} in {:.3}s", n, noun, elapsed.as_secs_f64()),
    }
}

impl RootPipeline {
    /// Busy threads / pool size for the pool this root is currently running.
    /// A root outlives its walk, so the pool is chosen by phase.
    pub(super) fn worker_counts(&self) -> (usize, usize) {
        let stats = match self.phase {
            RootPhase::Walking => Some(self.walk.worker_stats()),
            RootPhase::Extracting => self.content.as_ref().map(|p| p.worker_stats()),
            RootPhase::Done => None,
        };
        stats.map_or((0, 0), |s| (s.active(), s.total()))
    }

    fn snapshot(&self) -> RootProgress {
        let (active_workers, total_workers) = self.worker_counts();
        RootProgress {
            root: self.root.clone(),
            phase: self.phase,
            walked: self.walked,
            walk_total: match self.count_total.load(Ordering::Relaxed) {
                0 => None,
                n => Some(n),
            },
            extracted: self.extracted,
            extract_total: self.extract_total,
            current_file: self.current_file.clone(),
            active_workers,
            total_workers,
        }
    }

    /// Drain up to one quantum of walk events into the pending batches,
    /// finishing the walk if it ends. Returns whether anything happened.
    fn service_walking(&mut self, cx: &mut RunCx<'_>) -> Result<bool, String> {
        let mut took = 0usize;
        let mut finished = false;
        while took < cx.quantum {
            match self.walk.try_next() {
                TryNext::Item(WalkEvent::Stale(paths)) => {
                    took += 1;
                    // Applied at the end of the run: deleting mid-walk would
                    // break "a stopped run deletes nothing", and an aliased
                    // sighting that exempts a path may still be ahead.
                    cx.stale_candidates.extend(paths);
                }
                TryNext::Item(WalkEvent::File(file)) => {
                    took += 1;
                    self.walked += 1;
                    if self.walked.is_multiple_of(64) {
                        self.current_file = Some(file.path.clone());
                    }
                    if file.aliased {
                        // Its row's parent may never be visited, so the
                        // vanished-directory sweep must not read that parent's
                        // absence as proof the file is gone.
                        cx.aliased_paths.insert(file.path.clone());
                    }
                    // Dedupes a canonical file reachable through several
                    // spellings, or from more than one root.
                    if !cx.seen_paths.insert(file.digest) {
                        continue;
                    }
                    let Some(rec) = file.record else { continue };
                    if file.action == FileIndexAction::Update {
                        self.pending_updates.push(rec);
                        if self.pending_updates.len() >= cx.quantum {
                            process_batch_updates(
                                &cx.conn_mutex,
                                &self.pending_updates,
                                cx.stop_flag,
                                cx.config,
                            )?;
                            self.pending_updates.clear();
                        }
                    } else {
                        self.pending_inserts.push(rec);
                        if self.pending_inserts.len() >= cx.quantum {
                            process_batch_inserts(
                                &cx.conn_mutex,
                                &self.pending_inserts,
                                cx.stop_flag,
                                cx.config,
                            )?;
                            self.pending_inserts.clear();
                        }
                    }
                }
                TryNext::Empty => break,
                TryNext::Finished => {
                    self.finish_walk(cx)?;
                    finished = true;
                    break;
                }
            }
        }
        Ok(finished || took > 0)
    }

    /// The walk ended: land the buffered batches, then either hand the root
    /// to the content pass or mark it done.
    fn finish_walk(&mut self, cx: &mut RunCx<'_>) -> Result<(), String> {
        // Join before deciding anything about what this walk saw; see
        // `ParallelWalk::finish`.
        self.walk_clean = self.walk.finish();
        process_batch_updates(
            &cx.conn_mutex,
            &self.pending_updates,
            cx.stop_flag,
            cx.config,
        )?;
        self.pending_updates.clear();
        process_batch_inserts(
            &cx.conn_mutex,
            &self.pending_inserts,
            cx.stop_flag,
            cx.config,
        )?;
        self.pending_inserts.clear();

        let walk_time = self.phase_elapsed();
        crate::log_info!(
            "{}: walk {} — {} ({} workers)",
            self.root,
            if self.walk_clean {
                "done"
            } else {
                "ended early"
            },
            phase_summary(self.walked, "files", walk_time),
            self.workers
        );

        if !self.walk_clean {
            crate::log_warn!(
                "a walk worker for {} terminated abnormally; skipping stale cleanup",
                self.root
            );
            cx.stale_cleanup_ok = false;
            self.phase = RootPhase::Done;
        } else if cx.stop_flag.load(Ordering::Relaxed) {
            self.phase = RootPhase::Done;
        } else {
            // Recorded only when the walk saw the whole tree: the figure is
            // sticky, and an unreadable subtree would teach every later run a
            // too-small denominator. `unreadable()` is final here — `finish()`
            // joined the workers.
            if self.walk.unreadable().is_empty() {
                let conn = crate::lock_ok(&cx.conn_mutex);
                if let Err(e) = crate::db::repo::set_root_walk_count(&conn, &self.root, self.walked)
                {
                    crate::log_warn!("{}", e);
                }
            }
            let cursor = ExtractCursor::for_root(&self.root);
            let scope = extract_scope_prepare(&cx.conn_mutex, &cursor, cx.config)?;
            // Progress counts the root's whole searchable set: files extracted
            // in earlier runs start the counter, so an unchanged root shows
            // "X of X" rather than "0 of 0".
            self.extract_total = scope.pending + scope.already_done;
            self.extracted = scope.already_done;
            if scope.pending == 0 {
                self.phase = RootPhase::Done;
            } else {
                // Starts only now: the rows have to exist before the feeder
                // can page over them.
                self.content = Some(crate::content::extract_content(
                    cx.db_path,
                    &cursor,
                    cx.registry.clone(),
                    cx.config.clone(),
                    cx.stop_flag.clone(),
                    cx.suspend_flag.clone(),
                    self.workers,
                ));
                self.phase = RootPhase::Extracting;
            }
        }
        Ok(())
    }

    /// Drain up to a quantum of finished extraction work, then write it;
    /// extraction runs on this root's own pool. Returns whether anything
    /// happened.
    fn service_extracting(&mut self, cx: &mut RunCx<'_>) -> Result<bool, String> {
        let pass = self.content.as_mut().expect("extracting root has a pass");
        let mut batch: Vec<crate::content::ExtractedRow> = Vec::new();
        let mut finished = false;
        while batch.len() < cx.quantum {
            match pass.try_next() {
                TryNext::Item(row) => batch.push(row),
                TryNext::Empty => break,
                TryNext::Finished => {
                    finished = true;
                    break;
                }
            }
        }
        if let Some(row) = batch.last() {
            self.current_file = Some(row.name.clone());
        }
        let took = batch.len();
        self.extracted += store_extracted(&cx.conn_mutex, &batch, cx.stop_flag, cx.config)?;
        if finished {
            // Join before deciding; see `ParallelWalk::finish`.
            if !pass.finish() {
                crate::log_warn!("a content worker for {} terminated abnormally", self.root);
            }
            self.content = None;
            self.phase = RootPhase::Done;
            let extract_time = self.phase_elapsed();
            crate::log_info!(
                "{}: content done — {}",
                self.root,
                phase_summary(self.extracted, "files with text", extract_time)
            );
        }
        Ok(finished || took > 0)
    }
}

/// One run's shared environment and cross-root state, threaded through the
/// per-phase [`RootPipeline`] service methods.
struct RunCx<'a> {
    conn_mutex: Arc<Mutex<Connection>>,
    config: &'a Config,
    db_path: &'a str,
    stop_flag: &'a Arc<AtomicBool>,
    suspend_flag: &'a Arc<AtomicBool>,
    /// Shared with every root's walk workers, which use it to finish small
    /// text files without handing them to the content pass.
    registry: Arc<Registry>,
    quantum: usize,
    /// 128-bit path digests, not paths: at millions of files, owning every
    /// path string again was the single largest allocation in a run. See
    /// `walk::path_digest`.
    seen_paths: HashSet<u128>,
    /// Rows the per-directory reconciliation found no file behind, plus
    /// whatever the vanished-directory sweep adds once the walks end.
    stale_candidates: Vec<String>,
    /// Paths reached by resolving a symlink, whose row lives under a parent
    /// that may be outside every root.
    aliased_paths: HashSet<String>,
    stale_cleanup_ok: bool,
}

/// Publish a status snapshot. Never clobbers Stopping — the command thread
/// owns that transition.
fn publish_status(
    status: &Arc<Mutex<IndexingStatus>>,
    run_start: Instant,
    pipelines: &[RootPipeline],
) {
    let roots: Vec<RootProgress> = pipelines.iter().map(RootPipeline::snapshot).collect();
    let mut g = crate::lock_ok(status);
    if !matches!(*g, IndexingStatus::Stopping) {
        *g = IndexingStatus::Running {
            start_time: run_start,
            roots,
        };
    }
}

/// One pipeline for one root: its own walker (with per-root worker count),
/// its own buffers and extraction cursor, and — only on a root never walked
/// before — its own count thread.
fn build_pipeline(
    cx: &RunCx<'_>,
    root: &str,
    stored_count: Option<usize>,
    worker_override: Option<usize>,
    count_cancel: &Arc<AtomicBool>,
) -> Result<RootPipeline, String> {
    let ignore = crate::config::IgnoreSet::compile(&cx.config.indexing.ignore_patterns)
        .map_err(|e| format!("ignore patterns: {}", e))?;
    let workers = worker_override
        .filter(|w| *w > 0)
        .unwrap_or_else(|| thread_count_for(std::slice::from_ref(&root.to_string())))
        .clamp(1, 64);
    let walk = walk_indexable_files(
        std::slice::from_ref(&root.to_string()),
        cx.config.indexing.follow_symlinks,
        cx.config.indexing.include_hidden,
        ignore,
        cx.db_path,
        cx.config.clone(),
        cx.registry.clone(),
        cx.stop_flag.clone(),
        cx.suspend_flag.clone(),
        workers,
    );

    // A root walked before needs no scan: its stored count is exact rather
    // than 1.6x high, and the counting scan is a second full traversal of
    // the tree.
    let count_total = Arc::new(AtomicUsize::new(0));
    match stored_count {
        // `max(1)` for the same reason the counting arm uses it: 0 is the
        // "unknown" sentinel, so an empty root must not store it.
        Some(n) => count_total.store(n.max(1), Ordering::Relaxed),
        None => {
            let root = root.to_string();
            let cancel = count_cancel.clone();
            let total = count_total.clone();
            let _ = thread::Builder::new()
                .name("qs-count".into())
                .spawn(move || {
                    crate::platform::set_background_priority();
                    match count_tree_entries_fast(&root, &cancel) {
                        // An empty root stores 1: 0 is the "unknown" sentinel.
                        Ok(n) => total.store(n.max(1), Ordering::Relaxed),
                        Err(e) => {
                            if !e.contains("cancelled") {
                                crate::log_warn!("count for {}: {}", root, e);
                            }
                        }
                    }
                });
        }
    }

    Ok(RootPipeline {
        root: root.to_string(),
        walk,
        count_total,
        pending_updates: Vec::new(),
        pending_inserts: Vec::new(),
        walked: 0,
        walk_clean: true,
        phase: RootPhase::Walking,
        workers,
        content: None,
        extract_total: 0,
        extracted: 0,
        current_file: None,
        phase_started: Instant::now(),
    })
}

/// Reconcile deletions once every walk has ended — globally, because a file
/// may be reachable through more than one root's symlinks. Runs at most once
/// per run, on the writer thread. A no-op for a stopped run or one whose walk
/// terminated abnormally.
fn cleanup_stale(pipelines: &mut [RootPipeline], cx: &mut RunCx<'_>) -> Result<(), String> {
    let stopped = cx.stop_flag.load(Ordering::Relaxed);
    if !cx.stale_cleanup_ok || stopped {
        return Ok(());
    }
    for p in pipelines.iter() {
        sweep_unvisited_parents(
            &cx.conn_mutex,
            &p.root,
            &p.walk.seen_dirs(),
            p.walk.unreadable(),
            &cx.aliased_paths,
            &mut cx.stale_candidates,
        )?;
    }
    // No unreadable-directory filter here: neither source of candidates can
    // produce one — `read_directory` returns before reconciling an unreadable
    // directory, and the sweep skips parents beneath one.
    //
    // The aliased filter *is* applied to both: per-directory reconciliation
    // can flag a hidden or ignore-matched symlink target as stale while the
    // alias route inserted it — without this the row would be written and
    // deleted on every run.
    let stale_paths: Vec<String> = cx
        .stale_candidates
        .drain(..)
        .filter(|p| !cx.aliased_paths.contains(p))
        .collect();
    let unreadable_count: usize = pipelines
        .iter()
        .map(|p| p.walk.unreadable().paths().len())
        .sum();
    if unreadable_count > 0 {
        crate::log_warn!(
            "{} director{} could not be read; index entries beneath them were \
             kept rather than deleted",
            unreadable_count,
            if unreadable_count == 1 { "y" } else { "ies" }
        );
    }
    // One line for the whole run — see `walk::PruneCounts`.
    for p in pipelines.iter() {
        if let Some(summary) = p.walk.pruned().summary() {
            crate::log_info!("{}: {}", p.root, summary);
        }
    }
    if !stale_paths.is_empty() {
        if let Some(first) = pipelines.first_mut() {
            first.current_file = Some("Removing stale index entries…".to_string());
        }
        let started = Instant::now();
        let stale_deleted = cleanup_stale_index_entries(
            &cx.conn_mutex,
            stale_paths.as_slice(),
            cx.stop_flag,
            cx.suspend_flag,
            cx.config,
        )?;
        crate::log_info!(
            "stale cleanup — {}",
            phase_summary(stale_deleted, "index entries removed", started.elapsed())
        );
    }
    Ok(())
}

impl IndexingService {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        paths: &[String],
        db_path: &str,
        stop_flag: &Arc<AtomicBool>,
        suspend_flag: &Arc<AtomicBool>,
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        interrupt: &db::InterruptSlot,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("run_indexing: no paths provided".into());
        }

        let run_started = Instant::now();
        // Per-run: the counts must describe this run only.
        crate::walk::reset_run_warnings();
        crate::file_handling::reset_run_warnings();

        // De-duplicate while preserving order; canonicalized first so
        // spelling variants collapse to one walk. Nested-root dedup is
        // handled by the per-file `seen_paths` set.
        let mut seen_roots = HashSet::new();
        let roots: Vec<String> = paths
            .iter()
            .map(|p| normalize_root_string(p))
            .filter(|p| seen_roots.insert(p.clone()))
            .collect();

        // Rekeyed to match the canonicalized `roots`; a lookup that misses is
        // invisible. See `resolved_root_workers`.
        let worker_overrides = resolved_root_workers(config);

        // One clock for the whole run: the prologue can outlast the walk.
        let run_start = Self::run_start(status);

        // Open and migrate; a large WAL recovery happens here, so it is
        // announced before it is attempted.
        Self::set_prep_step(status, PrepStep::OpeningIndex);
        let mut conn = db::open_or_recreate(db_path, &config.processing.tokenize)?;

        // Reconcile against the settings the index was last written under,
        // *before* stamping the new ones — the old record is the only thing
        // that knows a root was dropped. Against `roots`, not
        // `config.paths.indexing_paths`: nothing makes the caller pass the
        // roots its config names, and reconciling against the config could
        // delete every row of the tree actually being indexed. Stamping is
        // conditional on the reconcile *finishing*: stamping a cut-short scan
        // would orphan the disowned rows for good.
        if !Self::reconcile_stored_config(status, interrupt, &mut conn, config, &roots, stop_flag)?
        {
            return Ok(());
        }
        Self::update_config(&conn, config, &roots)?;

        // No up-front load of the whole `files` table: each walk's prefetcher
        // fetches one directory's rows at a time.
        let conn_mutex = Arc::new(Mutex::new(conn));

        // Published so `stop_indexing` can checkpoint through it without
        // waiting for this run's thread to unwind.
        *crate::lock_ok(db_connection) = Some(conn_mutex.clone());

        // The guard kills the per-root count subprocesses on every exit path.
        let count_cancel = Arc::new(AtomicBool::new(false));
        let _count_guard = CancelOnDrop(count_cancel.clone());

        let mut cx = RunCx {
            conn_mutex,
            config,
            db_path,
            stop_flag,
            suspend_flag,
            registry: Arc::new(Registry::default_set()),
            quantum: config.processing.batch_size.max(1),
            seen_paths: HashSet::new(),
            stale_candidates: Vec::new(),
            aliased_paths: HashSet::new(),
            stale_cleanup_ok: true,
        };

        // Read stored counts up front, under one lock, before the walks
        // compete for the connection.
        let stored_counts: Vec<Option<usize>> = {
            let conn = crate::lock_ok(&cx.conn_mutex);
            let _ = crate::db::repo::prune_root_stats(&conn, &roots);
            roots
                .iter()
                .map(|r| crate::db::repo::get_root_walk_count(&conn, r))
                .collect()
        };

        // One pipeline per root, all funnelling into this one writer thread.
        let mut pipelines: Vec<RootPipeline> = Vec::with_capacity(roots.len());
        for (root, stored_count) in roots.iter().zip(stored_counts) {
            pipelines.push(build_pipeline(
                &cx,
                root,
                stored_count,
                worker_overrides.get(root).copied(),
                &count_cancel,
            )?);
        }
        publish_status(status, run_start, &pipelines);

        // Set by whichever `break` exits the loop, so a stopped run is never
        // mistaken for a completed one.
        let aborted;
        let mut cleanup_done = false;
        let mut rr = 0usize;
        // Log size at which to force a checkpoint; see [`wal_len`] for why
        // SQLite's autocheckpoint cannot be left to do this.
        let wal_path = format!("{}-wal", db_path);
        let wal_cap = match config.processing.maximum_wal_size {
            0 => 0,
            n => n.max(crate::config::MINIMUM_WAL_SIZE),
        };
        let mut checkpoint_at = wal_cap;

        // Round-robin with skipping: each round takes at most one quantum
        // from every root that has work ready.
        loop {
            if should_abort(stop_flag, suspend_flag) {
                aborted = true;
                break;
            }
            let mut progressed = false;
            let n = pipelines.len();
            for k in 0..n {
                let p = &mut pipelines[(rr + k) % n];
                progressed |= match p.phase {
                    RootPhase::Walking => p.service_walking(&mut cx)?,
                    RootPhase::Extracting => p.service_extracting(&mut cx)?,
                    RootPhase::Done => false,
                };
            }
            rr = rr.wrapping_add(1);

            if !cleanup_done && pipelines.iter().all(|p| p.phase != RootPhase::Walking) {
                cleanup_done = true;
                cleanup_stale(&mut pipelines, &mut cx)?;
                progressed = true;
            }

            publish_status(status, run_start, &pipelines);

            // After `publish_status`, so the GUI's last snapshot is fresh
            // going into a checkpoint that may block for `busy_timeout`.
            // `progressed` gates the stat (a round that wrote nothing cannot
            // have grown the log); the stop-flag check keeps a checkpoint from
            // sitting in front of `stop_indexing` for five seconds.
            if wal_cap > 0
                && progressed
                && !stop_flag.load(Ordering::Relaxed)
                && wal_len(&wal_path) >= checkpoint_at
            {
                {
                    let conn = crate::lock_ok(&cx.conn_mutex);
                    if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                        crate::log_warn!("{}", e);
                    }
                }
                // Re-armed from what is on disk: a checkpoint that lost the
                // race then costs one attempt per further `wal_cap` of
                // growth, not a retry every round.
                checkpoint_at = wal_len(&wal_path) + wal_cap;
            }

            if pipelines.iter().all(|p| p.phase == RootPhase::Done) {
                // A stop can land inside the pass above, with every root
                // reaching Done before the top-of-loop check sees the flag.
                // Re-read it, or a cut-short run is stamped as a completed
                // full index.
                aborted = stop_flag.load(Ordering::Relaxed);
                break;
            }
            if !progressed {
                // Park on a walking root's channel rather than sleeping: a
                // sender wakes this immediately, and on Windows a 2 ms
                // `thread::sleep` really stalls for the 15.6 ms timer tick.
                // `wait_ready` holds whatever it pulls, so the round-robin
                // still sees it in order.
                let waited = pipelines
                    .iter_mut()
                    .find(|p| p.phase == RootPhase::Walking)
                    .map(|p| p.walk.wait_ready(IDLE_BACKOFF))
                    .is_some();
                // Nothing walking (extracting passes have no such handle);
                // fall back to the sleep.
                if !waited {
                    thread::sleep(IDLE_BACKOFF);
                }
            }
        }

        if aborted {
            // Buffered records are valid work — land them before leaving.
            for p in &mut pipelines {
                process_batch_updates(&cx.conn_mutex, &p.pending_updates, stop_flag, config)?;
                p.pending_updates.clear();
                process_batch_inserts(&cx.conn_mutex, &p.pending_inserts, stop_flag, config)?;
                p.pending_inserts.clear();
            }
            // No stale cleanup: a partial walk's seen set would delete most
            // of the index.
            report_run_warnings();
            crate::log_info!(
                "indexing stopped after {:.1}s",
                run_started.elapsed().as_secs_f64()
            );
            // The final status is the caller's to publish: a stopped run is
            // still followed by an optimize pass, so this is not yet Idle.
            return Ok(());
        }

        report_run_warnings();
        crate::log_info!(
            "indexing complete in {:.1}s",
            run_started.elapsed().as_secs_f64()
        );

        // FTS housekeeping once per completed run (cheap if nothing changed).
        {
            let conn = crate::lock_ok(&cx.conn_mutex);
            fts_finalize_after_text_indexing(&conn);
        }

        // Stamp the successful run: an absent stamp reads as "never indexed"
        // and `periodic_due` starts another full run on the very next tick.
        let now = crate::log::now_unix();
        let conn = crate::lock_ok(&cx.conn_mutex);
        if let Err(e) = crate::db::repo::set_last_full_index(&conn, now) {
            crate::log_warn!("{}", e);
        }

        // What each root holds, for the folder list to show once these
        // pipelines and their `RootProgress` rows are gone. Here rather than on
        // a cadence: `count_root` reads every row in the range, and this run
        // has just written them, so the pages are as warm as they will ever be.
        // Under the interrupt guard because it is still a scan per root, and
        // quitting should not wait out one of them; a root whose count fails
        // keeps the figure it had.
        let _guard = db::InterruptGuard::arm(interrupt, &conn);
        for root in &roots {
            let range = ExtractCursor::for_root(root);
            match repo::count_root(&conn, &range.lo, &range.hi) {
                Ok(counts) => {
                    if let Err(e) = repo::set_root_counts(&conn, root, counts) {
                        crate::log_warn!("{}", e);
                    }
                }
                Err(e) => crate::log_warn!("counts for {} unavailable: {}", root, e),
            }
        }

        Ok(())
    }
}
