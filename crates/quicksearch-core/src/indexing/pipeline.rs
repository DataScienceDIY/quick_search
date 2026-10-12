//! One run of the indexer: the per-root [`RootPipeline`] and the
//! writer loop in [`IndexingService::run_indexing`] they funnel into.

use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db;
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    cleanup_stale_index_entries, count_tree_entries_fast, fts_begin_bulk_write,
    fts_finalize_after_text_indexing, mark_oversize_pending_na, normalize_root_string,
    process_batch_inserts, process_batch_updates, store_extracted, ExtractCursor, ExtractScope,
    FileIndexAction, OwnedNewFile, RootRange,
};
use crate::walk::{thread_count_for, walk_indexable_files, ParallelWalk, TryNext, WalkEvent};

use super::*;

/// Append to `out` the rows under stored parents the walk never reached (a
/// deleted or newly excluded directory is never read, so no per-directory
/// diff sees them), except under unreadable directories. Sound only after a
/// clean walk, one that read every queued directory in full or recorded it
/// unreadable: `seen_dirs` holds queued directories, not read ones. Uses a
/// read connection so extracting roots keep the writer; every walk batch is
/// already committed.
fn sweep_unvisited_parents(
    conn: &Connection,
    root: &str,
    seen_dirs: &HashSet<String>,
    unreadable: &crate::file_handling::UnreadableDirs,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let mut unvisited: Vec<String> = Vec::new();
    repo::for_each_parent_in_range(conn, &RootRange::for_root(root), |parent| {
        if !seen_dirs.contains(&parent) && !unreadable.covers(&parent) {
            unvisited.push(parent);
        }
    })?;

    for parent in unvisited {
        out.extend(repo::paths_in_dir(conn, &parent)?);
    }
    Ok(())
}

/// WAL file size in bytes; 0 if absent.
fn wal_len(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Free space at which a run gives up. Filling the volume is not a clean
/// failure: a WAL write through the `-shm` mmap that the filesystem cannot
/// back is delivered as SIGBUS, which no `Result` can catch (and on
/// copy-on-write filesystems even overwrites need new extents). Stopping
/// with an error while there is still room is the only safe end.
const DISK_FLOOR: u64 = 128 * 1024 * 1024;

/// Share of the free space above [`DISK_FLOOR`] the log may occupy; a
/// quarter leaves room for index growth and FTS merge segments.
const WAL_SHARE_OF_FREE: u64 = 4;

/// The configured checkpoint threshold, lowered to what the volume can
/// absorb. A configured `0` is bounded too: not a licence to fill the disk.
fn wal_cap_for_volume(configured: u64, db_path: &Path) -> u64 {
    let Some(free) = crate::platform::available_space(db_path) else {
        return configured;
    };
    let effective = wal_cap_for_free(configured, free);
    if effective != configured {
        crate::log_info!(
            "{} free where the index lives: forcing a WAL checkpoint every {} MiB \
             instead of {}",
            human_mib(free),
            effective / (1024 * 1024),
            match configured {
                0 => "never".to_string(),
                n => format!("{} MiB", n / (1024 * 1024)),
            }
        );
    }
    effective
}

/// The arithmetic of [`wal_cap_for_volume`], split from the syscall for tests.
pub(super) fn wal_cap_for_free(configured: u64, free: u64) -> u64 {
    let room = free.saturating_sub(DISK_FLOOR) / WAL_SHARE_OF_FREE;
    // Checkpoints more often than MINIMUM_WAL_SIZE cost more in locks than
    // the log costs in space; the in-run check stops a doomed run.
    let capped = room.max(crate::config::MINIMUM_WAL_SIZE);
    // `0` is "no cap", so it loses every `min`; hence the explicit arm.
    if configured == 0 {
        capped
    } else {
        configured.min(capped)
    }
}

fn human_mib(bytes: u64) -> String {
    format!("{} MiB", bytes / (1024 * 1024))
}

/// Held by `run_indexing` so the count threads die on every exit path.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Most extracted rows a root holds back between turns. Not `batch`: a row
/// carries up to `maximum_text_size` of text, so 500 would be 128 MiB per root.
const READY_TOPUP: usize = 64;

/// One root's in-flight indexing state, owned by the writer loop.
pub(super) struct RootPipeline {
    pub(super) root: String,
    pub(super) walk: ParallelWalk,
    /// This root's walk denominator; 0 = not yet known.
    pub(super) count_total: Arc<AtomicUsize>,
    pub(super) workers: usize,
    pub(super) pending_updates: Vec<OwnedNewFile>,
    pub(super) pending_inserts: Vec<OwnedNewFile>,
    pub(super) walked: usize,
    pub(super) phase: RootPhase,
    pub(super) content: Option<crate::content::ContentPass>,
    /// Rows pulled off the pass and not yet written; a turn may leave some.
    pub(super) ready: Vec<crate::content::ExtractedRow>,
    pub(super) written: usize,
    /// The pass's range counts, cached so a `Done` root still has them.
    pub(super) totals: Option<ExtractScope>,
    pub(super) current_file: Option<String>,
    /// When this root's current phase began.
    pub(super) phase_started: Instant,
}

/// Idle-wait ceiling, not a delay: the loop parks on a channel and any
/// producer wakes it at once.
const IDLE_BACKOFF: Duration = Duration::from_millis(2);

/// A batch writer: [`process_batch_updates`] or [`process_batch_inserts`].
type WriteBatch = fn(
    &Arc<Mutex<Connection>>,
    &[OwnedNewFile],
    &Arc<AtomicBool>,
    &mut crate::db::repo::DocEncoder,
) -> Result<(), String>;

/// Write `rows` through `write`, then clear them; on an error they stay.
fn flush(
    cx: &mut RunCx<'_>,
    rows: &mut Vec<OwnedNewFile>,
    write: WriteBatch,
) -> Result<(), String> {
    write(&cx.conn_mutex, rows, cx.stop_flag, &mut cx.encoder)?;
    rows.clear();
    Ok(())
}

/// What each run-scoped structure holds, for the `probe` builds only.
///
/// Peak RSS is one anonymous heap: `smaps` can say "the heap grew", never
/// "the stale-candidate list grew". This says which. Sizes are estimates of
/// the *heap* each structure owns: the point is which one dominates and
/// whether it tracks the tree, not a byte-exact total. See `examples/memprobe.rs`.
#[cfg(feature = "probe")]
mod census {
    use super::{RootPipeline, RunCx};
    use crate::testutil::mib;
    use std::time::{Duration, Instant};

    /// Rare next to a status publish: each line walks every string in the
    /// run-scoped sets, which is O(tree) work that must not shape what it
    /// measures.
    const INTERVAL: Duration = Duration::from_secs(2);

    /// Owned string bytes plus the `String` headers a collection holds.
    fn strings_bytes<'a>(it: impl Iterator<Item = &'a String>, len: usize) -> u64 {
        let body: usize = it.map(String::len).sum();
        body as u64 + (len * std::mem::size_of::<String>()) as u64
    }

    pub(super) fn due(last: &mut Instant) -> bool {
        if last.elapsed() < INTERVAL {
            return false;
        }
        *last = Instant::now();
        true
    }

    /// One tail step's cost: how long it held the writer and what it left in
    /// the log. Autocheckpoint is off across the tail, so a per-step reading
    /// is the only way to say which step is responsible for the log's peak.
    /// `db::repo::maintain` emits the same shape for the half that runs after
    /// this connection has gone.
    pub(super) fn tail(step: &str, db_path: &str, started: Instant) {
        crate::log_info!(
            "tail t={:.1}s  wal {}  after {}",
            started.elapsed().as_secs_f64(),
            mib(super::wal_len(&format!("{}-wal", db_path))),
            step
        );
    }

    /// One line per structure group: the log collapses embedded newlines, so
    /// a multi-line report would arrive as one unreadable line.
    pub(super) fn report(cx: &RunCx<'_>, pipelines: &[RootPipeline], started: Instant) {
        let at = started.elapsed().as_secs_f64();
        crate::log_info!(
            "census t={:.1}s  stale {} ({})",
            at,
            cx.stale_candidates.len(),
            mib(strings_bytes(
                cx.stale_candidates.iter(),
                cx.stale_candidates.len()
            )),
        );
        for p in pipelines {
            let (dirs, dir_bytes) = p.walk.seen_dirs_footprint();
            let inline = |rows: &[crate::file_handling::OwnedNewFile]| -> u64 {
                rows.iter()
                    .map(|r| {
                        (r.inline_text.as_ref().map_or(0, String::len) + r.path().len()) as u64
                    })
                    .sum()
            };
            let ready: u64 = p
                .ready
                .iter()
                .map(|r| {
                    (crate::file_handling::outcome_body(&r.outcome).map_or(0, str::len)
                        + r.path().len()) as u64
                })
                .sum();
            crate::log_info!(
                "census t={:.1}s  {} [{:?}] dirs {} ({})  pending {}+{} ({})  ready {} ({})",
                at,
                p.root,
                p.phase,
                dirs,
                mib(dir_bytes),
                p.pending_inserts.len(),
                p.pending_updates.len(),
                mib(inline(&p.pending_inserts) + inline(&p.pending_updates)),
                p.ready.len(),
                mib(ready),
            );
        }
    }
}

/// No-op stand-ins for the census outside `probe` builds.
#[cfg(not(feature = "probe"))]
mod census {
    use super::{RootPipeline, RunCx};
    use std::time::Instant;

    pub(super) fn due(_last: &mut Instant) -> bool {
        false
    }

    pub(super) fn tail(_step: &str, _db_path: &str, _started: Instant) {}

    pub(super) fn report(_cx: &RunCx<'_>, _pipelines: &[RootPipeline], _started: Instant) {}
}

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

fn phase_summary(n: usize, noun: &str, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs >= 0.001 {
        let rate = n as f64 / secs;
        format!("{} {} in {:.1}s ({:.0}/s)", n, noun, secs, rate)
    } else {
        format!("{} {} in {:.3}s", n, noun, secs)
    }
}

impl RootPipeline {
    fn phase_elapsed(&mut self) -> Duration {
        let started = std::mem::replace(&mut self.phase_started, Instant::now());
        started.elapsed()
    }

    /// Busy threads / pool size; a root outlives its walk, so chosen by phase.
    pub(super) fn worker_counts(&self) -> (usize, usize) {
        let stats = match self.phase {
            RootPhase::Walking => Some(self.walk.worker_stats()),
            RootPhase::Extracting => self.content.as_ref().map(|p| p.worker_stats()),
            RootPhase::Done => None,
        };
        stats.map_or((0, 0), |s| (s.active(), s.total()))
    }

    fn extract_totals(&self) -> Option<ExtractScope> {
        self.totals
            .or_else(|| self.content.as_ref().and_then(|p| p.totals()))
    }

    pub(super) fn snapshot(&self) -> RootProgress {
        let (active_workers, total_workers) = self.worker_counts();
        let totals = self.extract_totals();
        RootProgress {
            root: self.root.clone(),
            phase: self.phase,
            walked: self.walked,
            walk_total: match self.count_total.load(Ordering::Relaxed) {
                0 => None,
                n => Some(n),
            },
            // Never goes backwards. Not clamped against `extract_total`: the
            // numerator can briefly overshoot, since the count runs behind the
            // pass's first page.
            extracted: totals.map_or(self.written, |t| t.already_done + self.written),
            extract_total: totals.map(|t| t.pending + t.already_done),
            current_file: self.current_file.clone(),
            active_workers,
            total_workers,
        }
    }

    /// Drain walk events into the pending batches for up to one slice. The
    /// deadline is read once per `batch` events, so each call handles at
    /// least a batch while the channel has one; a batch lands as soon as it
    /// fills. `true` if anything happened.
    pub(super) fn service_walking(&mut self, cx: &mut RunCx<'_>) -> Result<bool, String> {
        let deadline = Instant::now() + cx.slice;
        let mut took = 0usize;
        loop {
            match self.walk.try_next() {
                TryNext::Item(event) => {
                    took += 1;
                    self.accept(event, cx)?;
                    if took.is_multiple_of(cx.batch) && Instant::now() >= deadline {
                        return Ok(true);
                    }
                }
                TryNext::Empty => return Ok(took > 0),
                TryNext::Finished => {
                    self.finish_walk(cx)?;
                    return Ok(true);
                }
            }
        }
    }

    /// Take one walk event: stale paths wait for the end of the run, and a
    /// file's record joins its batch, written once it holds `batch` rows.
    fn accept(&mut self, event: WalkEvent, cx: &mut RunCx<'_>) -> Result<(), String> {
        let file = match event {
            // Applied at run end: deleting mid-walk would break "a stopped
            // run deletes nothing".
            WalkEvent::Stale(paths) => {
                cx.stale_candidates.extend(paths);
                return Ok(());
            }
            WalkEvent::File(file) => file,
        };
        self.walked += 1;
        if self.walked.is_multiple_of(64) {
            self.current_file = Some(file.path.clone());
        }
        let Some(rec) = file.record else {
            return Ok(());
        };
        let (rows, write): (&mut Vec<OwnedNewFile>, WriteBatch) =
            if file.action == FileIndexAction::Update {
                (&mut self.pending_updates, process_batch_updates)
            } else {
                (&mut self.pending_inserts, process_batch_inserts)
            };
        rows.push(rec);
        if rows.len() >= cx.batch {
            flush(cx, rows, write)?;
        }
        Ok(())
    }

    /// The walk ended: land the buffered batches, then either hand the root
    /// to the content pass or mark it done.
    fn finish_walk(&mut self, cx: &mut RunCx<'_>) -> Result<(), String> {
        // Join before deciding; see `ParallelWalk::finish`.
        let walk_clean = self.walk.finish();
        flush(cx, &mut self.pending_updates, process_batch_updates)?;
        flush(cx, &mut self.pending_inserts, process_batch_inserts)?;

        let walk_time = self.phase_elapsed();
        crate::log_info!(
            "{}: walk {} — {} ({} workers)",
            self.root,
            if walk_clean { "done" } else { "ended early" },
            phase_summary(self.walked, "files", walk_time),
            self.workers
        );

        // A stop ends every walk early; only an unasked-for end is news.
        if cx.stop_flag.load(Ordering::Relaxed) {
            self.phase = RootPhase::Done;
            return Ok(());
        }
        if !walk_clean {
            crate::log_warn!(
                "the walk for {} ended early; skipping stale cleanup",
                self.root
            );
            cx.stale_cleanup_ok = false;
            self.phase = RootPhase::Done;
            return Ok(());
        }

        // Only when the walk saw the whole tree: the figure is sticky, and an
        // unreadable subtree would teach a too-small denominator.
        if self.walk.unreadable().is_empty() {
            let conn = crate::lock_ok(&cx.conn_mutex);
            if let Err(e) = crate::db::repo::set_root_walk_count(&conn, &self.root, self.walked) {
                crate::log_warn!("{}", e);
            }
        }
        let cursor = ExtractCursor::for_root(&self.root);
        // Counting the range is the pass's job, on its own connection: on the
        // writer it is seconds of every other walk standing still.
        {
            let _maintaining = cx.maintaining(MaintenanceStep::SizeLimit);
            let conn = crate::lock_ok(&cx.conn_mutex);
            mark_oversize_pending_na(&conn, &cursor.range, cx.config)?;
        }
        self.totals = None;
        self.written = 0;
        self.ready.clear();
        // Starts only now: the rows must exist before the feeder can page
        // over them. An empty range finishes on its own next turn.
        self.content = Some(crate::content::extract_content(
            cx.db_path,
            &cursor,
            cx.registry.clone(),
            cx.config.clone(),
            cx.stop_flag.clone(),
            self.workers,
        ));
        self.phase = RootPhase::Extracting;
        Ok(())
    }

    /// Write finished extraction work for up to one slice; extraction itself
    /// runs on this root's own pool. Rows the slice does not reach stay in
    /// `ready`, and the pass is not declared done until they have all landed.
    pub(super) fn service_extracting(&mut self, cx: &mut RunCx<'_>) -> Result<bool, String> {
        let deadline = Instant::now() + cx.slice;
        let mut finished = false;
        let mut consumed = 0usize;
        let Self {
            content,
            ready,
            written,
            totals,
            current_file,
            ..
        } = self;
        let pass = content.as_mut().expect("extracting root has a pass");
        *totals = totals.or_else(|| pass.totals());
        loop {
            while ready.len() < READY_TOPUP {
                match pass.try_next() {
                    TryNext::Item(row) => ready.push(row),
                    TryNext::Empty => break,
                    TryNext::Finished => {
                        finished = true;
                        break;
                    }
                }
            }
            if ready.is_empty() {
                break;
            }
            // Rowid order: FTS5 flushes its pending terms to a new segment
            // whenever a write's rowid falls below the previous one, and the
            // pool finishes rows in completion order, not id order. Measured
            // (`indexprobe cold`, 10k docs of 9–20 KiB): 5% less CPU, 9% less
            // WAL written; docs past FTS5's 1 MiB hash flush anyway, so a
            // corpus of 8–200 KiB docs gained only the WAL figure (7%).
            ready.sort_unstable_by_key(|row| row.file_id);
            let stored = store_extracted(
                &cx.conn_mutex,
                ready,
                cx.stop_flag,
                deadline,
                &mut cx.encoder,
            )?;
            if stored.consumed > 0 {
                // The last row *written*, not the last fetched. The whole path,
                // as the walk phase publishes: the hint must not change what it
                // means to a shorter name when a root crosses into extraction.
                *current_file = Some(ready[stored.consumed - 1].path().to_string());
            }
            ready.drain(..stored.consumed);
            *written += stored.written;
            consumed += stored.consumed;
            if stored.consumed == 0 || !ready.is_empty() || Instant::now() >= deadline {
                break;
            }
        }
        if finished && ready.is_empty() {
            // After the join: the feeder counts behind its last page, so the
            // workers can finish before the count lands.
            if !pass.finish() {
                crate::log_warn!("a content worker for {} terminated abnormally", self.root);
            }
            *totals = totals.or_else(|| pass.totals());
            self.content = None;
            self.phase = RootPhase::Done;
            let extract_time = self.phase_elapsed();
            if self.written > 0 {
                crate::log_info!(
                    "{}: content done — {}",
                    self.root,
                    phase_summary(self.written, "files with text", extract_time)
                );
            }
        }
        Ok(finished || consumed > 0)
    }
}

pub(super) struct RunCx<'a> {
    pub(super) conn_mutex: Arc<Mutex<Connection>>,
    pub(super) config: &'a Config,
    pub(super) db_path: &'a str,
    pub(super) stop_flag: &'a Arc<AtomicBool>,
    /// Where an upkeep step announces itself; see [`RunCx::maintaining`].
    pub(super) status: Arc<Mutex<IndexingStatus>>,
    /// Walk workers use it to finish small text files without the content pass.
    pub(super) registry: Arc<Registry>,
    /// Rows per write batch ([`crate::config::ProcessingConfig::batch`]).
    pub(super) batch: usize,
    /// Writer time one root's turn may take before the round moves on
    /// ([`crate::config::ProcessingConfig::writer_turn_slice_ms`]).
    pub(super) slice: Duration,
    /// The run's one compression context, shared by every write.
    pub(super) encoder: crate::db::repo::DocEncoder,
    /// Rows with no file behind them, per-directory plus the vanished sweep.
    pub(super) stale_candidates: Vec<String>,
    pub(super) stale_cleanup_ok: bool,
}

impl<'a> RunCx<'a> {
    pub(super) fn new(
        conn_mutex: Arc<Mutex<Connection>>,
        config: &'a Config,
        db_path: &'a str,
        stop_flag: &'a Arc<AtomicBool>,
        status: Arc<Mutex<IndexingStatus>>,
    ) -> Result<RunCx<'a>, String> {
        Ok(RunCx {
            conn_mutex,
            config,
            db_path,
            stop_flag,
            status,
            registry: Arc::new(Registry::default_set()),
            batch: config.processing.batch(),
            slice: Duration::from_millis(config.processing.writer_turn_slice_ms),
            encoder: crate::db::repo::DocEncoder::new()?,
            stale_candidates: Vec::new(),
            stale_cleanup_ok: true,
        })
    }

    /// Publish `step` on the running status until the returned guard drops, so
    /// frozen per-file counters don't read as a hang. Touches nothing else:
    /// counters keep their last values, `Stopping` is left alone, and the
    /// guard owns its `Arc`, leaving `cx` free to borrow mutably.
    pub(super) fn maintaining(&self, step: MaintenanceStep) -> MaintenanceGuard {
        set_maintenance(&self.status, Some(step));
        MaintenanceGuard {
            status: self.status.clone(),
        }
    }
}

/// Clears the step its [`RunCx::maintaining`] set.
pub(super) struct MaintenanceGuard {
    status: Arc<Mutex<IndexingStatus>>,
}

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        set_maintenance(&self.status, None);
    }
}

/// Annotate the published run, if there still is one: a status that has moved
/// on to `Stopping` is the command thread's, and a step is not news worth
/// resurrecting a run for.
fn set_maintenance(status: &Arc<Mutex<IndexingStatus>>, step: Option<MaintenanceStep>) {
    if let IndexingStatus::Running { maintenance, .. } = &mut *crate::lock_ok(status) {
        *maintenance = step;
    }
}

/// Publish a status snapshot. Never clobbers Stopping: the command thread
/// owns that transition. Always clears any upkeep step, since fresh per-file
/// figures mean the writer is back on files.
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
            maintenance: None,
        };
    }
}

/// One pipeline for one root; a never-walked root also gets a count thread.
fn build_pipeline(
    cx: &RunCx<'_>,
    root: &str,
    stored_count: Option<usize>,
    worker_override: Option<usize>,
    count_cancel: &Arc<AtomicBool>,
) -> Result<RootPipeline, String> {
    let ignore = crate::config::IgnoreSet::compile(&cx.config.indexing.ignore_patterns)
        .map_err(|e| format!("ignore patterns: {}", e))?;
    let roots = [root.to_string()];
    let workers = worker_override
        .filter(|w| *w > 0)
        .unwrap_or_else(|| thread_count_for(&roots))
        .clamp(1, 64);
    let walk = walk_indexable_files(
        &roots,
        cx.config.indexing.include_hidden,
        ignore,
        cx.db_path,
        cx.config.clone(),
        cx.registry.clone(),
        cx.stop_flag.clone(),
        workers,
    );

    // A walked root needs no scan: its stored count is exact. A never-walked
    // one gets a second, count-only traversal beside the walk, kept for the
    // first run's progress figure. Measured (`indexprobe cold`, 50k files in
    // 5k directories, alternating runs): on a virtiofs share it cost ~4% of
    // wall time (2.29 s against 2.20 s without it) and ~7% of CPU, and no
    // prefetch benefit to the walk showed; on warm local disk the difference
    // was within noise. Its figure counts directories and ignored entries
    // too, so it runs high.
    let count_total = Arc::new(AtomicUsize::new(0));
    match stored_count {
        // `max(1)`: 0 is the "unknown" sentinel; an empty root must not store it.
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
                        Ok(n) => total.store(n.max(1), Ordering::Relaxed),
                        Err(e) if !e.contains("cancelled") => {
                            crate::log_warn!("count for {}: {}", root, e)
                        }
                        Err(_) => {}
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
        phase: RootPhase::Walking,
        workers,
        content: None,
        ready: Vec::new(),
        written: 0,
        totals: None,
        current_file: None,
        phase_started: Instant::now(),
    })
}

/// Reconcile deletions once every walk has ended, every root's in one pass.
/// A no-op for a stopped run, or one with a walk that ended early.
pub(super) fn cleanup_stale(pipelines: &[RootPipeline], cx: &mut RunCx<'_>) -> Result<(), String> {
    let stopped = cx.stop_flag.load(Ordering::Relaxed);
    if !cx.stale_cleanup_ok || stopped {
        return Ok(());
    }
    // The whole pass, not just the deleting: the sweep below reads every
    // stored parent under every root, and the merge that ends the deletion is
    // minutes of writer time on a big index.
    let _maintaining = cx.maintaining(MaintenanceStep::RemovingStale);
    // One read connection for the whole sweep, so the writer stays free for
    // any root still extracting; see `sweep_unvisited_parents`.
    let sweep_conn = crate::db::open::open_walk_reader(cx.db_path)?;
    for p in pipelines.iter() {
        sweep_unvisited_parents(
            &sweep_conn,
            &p.root,
            &p.walk.seen_dirs(),
            p.walk.unreadable(),
            &mut cx.stale_candidates,
        )?;
    }
    drop(sweep_conn);
    let stale_paths = std::mem::take(&mut cx.stale_candidates);
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
    for p in pipelines.iter() {
        if let Some(summary) = p.walk.pruned().summary() {
            crate::log_info!("{}: {}", p.root, summary);
        }
    }
    if !stale_paths.is_empty() {
        let started = Instant::now();
        let stale_deleted = cleanup_stale_index_entries(
            &cx.conn_mutex,
            stale_paths.as_slice(),
            cx.stop_flag,
            cx.config,
        )?;
        crate::log_info!(
            "stale cleanup — {}",
            phase_summary(stale_deleted, "index entries removed", started.elapsed())
        );
    }
    Ok(())
}

/// `v` in service order for round `round`: from index `round % len` to the
/// end, then from the start, so no root is always served first.
fn rotated<T>(v: &mut [T], round: usize) -> impl Iterator<Item = &mut T> {
    let (head, tail) = v.split_at_mut(round.checked_rem(v.len()).unwrap_or(0));
    tail.iter_mut().chain(head)
}

/// The in-run bound on the WAL. SQLite shrinks the log only when no reader
/// holds a read mark, and a run keeps a reader per root from start to
/// finish; an explicit checkpoint retries that lock under `busy_timeout` and
/// wins. So the writer forces one each time the log grows by `cap`, and
/// stops the run when the volume nears [`DISK_FLOOR`].
struct WalGuard {
    path: String,
    /// 0 when the log is unbounded.
    cap: u64,
    /// The log size that triggers the next checkpoint.
    next_at: u64,
}

impl WalGuard {
    fn new(config: &Config, db_path: &str) -> WalGuard {
        let configured = match config.processing.maximum_wal_size {
            0 => 0,
            n => n.max(crate::config::MINIMUM_WAL_SIZE),
        };
        let cap = wal_cap_for_volume(configured, Path::new(db_path));
        WalGuard {
            path: format!("{}-wal", db_path),
            cap,
            next_at: cap,
        }
    }

    /// Checkpoint if a round that made progress left the log past `next_at`,
    /// then stop the run if the disk is nearly full.
    fn after_round(&mut self, cx: &RunCx<'_>, progressed: bool) -> Result<(), String> {
        // Called after `publish_status`: the checkpoint may block for
        // `busy_timeout`, and it must not sit in front of `stop_indexing`,
        // hence the stop-flag check.
        if self.cap == 0
            || !progressed
            || cx.stop_flag.load(Ordering::Relaxed)
            || wal_len(&self.path) < self.next_at
        {
            return Ok(());
        }
        {
            let _maintaining = cx.maintaining(MaintenanceStep::Checkpoint);
            let conn = crate::lock_ok(&cx.conn_mutex);
            if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
                crate::log_warn!("{}", e);
            }
        }
        // Only here, not every round: the log is at its largest, and the
        // checkpoint above has just returned whatever it could, so what is
        // left is the honest figure.
        if let Some(free) = crate::platform::available_space(Path::new(cx.db_path)) {
            if free < DISK_FLOOR {
                return Err(format!(
                    "Stopped: only {} free where the index lives ({}). \
                     Indexing needs room for its write-ahead log, and \
                     filling the disk can kill the process outright \
                     rather than fail cleanly. Free some space and run \
                     again — what is already indexed is kept.",
                    human_mib(free),
                    cx.db_path
                ));
            }
        }
        // Re-armed from what is on disk: a lost race costs one attempt per
        // further `cap` of growth, not one per round.
        self.next_at = wal_len(&self.path) + self.cap;
        Ok(())
    }
}

impl IndexingService {
    /// Open the index and ready it for `roots`: reconcile it against the
    /// settings it was last written under, stamp the new ones, queue failed
    /// files for a retry and apply the write-side FTS settings. Returns the
    /// connection and the roots stamped, which hold recorded ones while a
    /// root is offline (see [`Self::reconcile_stored_config`]). `None` when a
    /// stop cut the reconcile short.
    fn prologue(
        status: &Arc<Mutex<IndexingStatus>>,
        interrupt: &db::InterruptSlot,
        db_path: &str,
        config: &Config,
        roots: &[String],
        stop_flag: &Arc<AtomicBool>,
    ) -> Result<Option<(Connection, Vec<String>)>, String> {
        // A large WAL recovery happens here; announced before attempted.
        Self::set_prep_step(status, PrepStep::OpeningIndex);
        let mut conn = db::open_or_recreate(db_path, &config.processing)?;

        // Reconcile against the settings the index was last written under,
        // *before* stamping the new ones: the old record is the only thing
        // that knows a root was dropped. Against `roots`, not the config's
        // paths, which could name a different tree. Stamping is conditional
        // on the reconcile *finishing*: a cut-short stamp orphans the rows.
        let Some(effective) =
            Self::reconcile_stored_config(status, interrupt, &mut conn, config, roots, stop_flag)?
        else {
            return Ok(None);
        };
        // Everything from here to the first walk is database work of its own;
        // leaving the step on `Reconciling` reads as a reconcile that hung.
        Self::set_prep_step(status, PrepStep::Starting);
        Self::update_config(&conn, config, &effective)?;

        // Failed files are retried once per run; only a retry can tell.
        {
            let tx = conn
                .transaction()
                .map_err(|e| format!("begin failed-file retry: {}", e))?;
            let retried = db::repo::retry_failed_files(&tx)?;
            tx.commit()
                .map_err(|e| format!("commit failed-file retry: {}", e))?;
            if retried > 0 {
                crate::log_info!("retrying {} previously failed files", retried);
            }
        }

        // Before a single row is written, or a fresh index's first run would
        // write under FTS5's defaults.
        fts_begin_bulk_write(&conn);

        // Autocheckpoint off for the run: it can never reset the log while a
        // reader per root is live, so it copies pages back perpetually at full
        // price. Safe only because this writer bounds its own log, through
        // `wal_cap_for_volume`, [`WalGuard`] and [`checkpoint_tail`]; a writer
        // without all of them must keep the automatic one.
        if let Err(e) = conn.execute_batch("PRAGMA wal_autocheckpoint = 0;") {
            crate::log_warn!("could not disable autocheckpoint (non-fatal): {}", e);
        }
        Ok(Some((conn, effective)))
    }

    pub(super) fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        paths: &[String],
        db_path: &str,
        stop_flag: &Arc<AtomicBool>,
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        interrupt: &db::InterruptSlot,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("run_indexing: no paths provided".into());
        }

        let run_started = Instant::now();
        crate::walk::reset_run_warnings();
        crate::file_handling::reset_run_warnings();

        // Canonicalized first so spelling variants collapse to one walk.
        // Nested roots need no handling here: the coordinator refuses to
        // start a run with any (`config::nested_roots`).
        let mut seen_roots = HashSet::new();
        let roots: Vec<String> = paths
            .iter()
            .map(|p| normalize_root_string(p))
            .filter(|p| seen_roots.insert(p.clone()))
            .collect();

        let worker_overrides = resolved_root_workers(config);

        let run_start = Self::run_start(status);

        // The walks take `roots`; the record keeps `effective`, which holds
        // an offline root's recorded spelling and figures.
        let Some((conn, effective)) =
            Self::prologue(status, interrupt, db_path, config, &roots, stop_flag)?
        else {
            return Ok(());
        };
        let conn_mutex = Arc::new(Mutex::new(conn));

        // Published so `stop_indexing` can checkpoint through it without
        // waiting for this run's thread to unwind.
        *crate::lock_ok(db_connection) = Some(conn_mutex.clone());

        let count_cancel = Arc::new(AtomicBool::new(false));
        let _count_guard = CancelOnDrop(count_cancel.clone());

        let mut cx = RunCx::new(conn_mutex, config, db_path, stop_flag, status.clone())?;

        let stored_counts: Vec<Option<usize>> = {
            let conn = crate::lock_ok(&cx.conn_mutex);
            let _ = crate::db::repo::prune_root_stats(&conn, &effective);
            roots
                .iter()
                .map(|r| crate::db::repo::get_root_walk_count(&conn, r))
                .collect()
        };

        let mut pipelines = roots
            .iter()
            .zip(stored_counts)
            .map(|(root, stored_count)| {
                let workers = worker_overrides.get(root).copied();
                build_pipeline(&cx, root, stored_count, workers, &count_cancel)
            })
            .collect::<Result<Vec<_>, _>>()?;
        publish_status(status, run_start, &pipelines);

        let mut cleanup_done = false;
        let mut round = 0usize;
        let mut last_census = Instant::now();
        let mut wal = WalGuard::new(config, db_path);

        // Each round serves every walking root one slice, then one extracting
        // root one slice. Walk workers stall once their channel fills, so
        // walks go first; one extraction slice per round keeps a walk's wait
        // at one slice whatever the root count. A stop must never read as
        // completion, so every exit says whether it was one.
        let aborted = loop {
            if stop_flag.load(Ordering::Relaxed) {
                break true;
            }
            let mut progressed = false;
            let walking = rotated(&mut pipelines, round).filter(|p| p.phase == RootPhase::Walking);
            for p in walking {
                progressed |= p.service_walking(&mut cx)?;
            }
            // Between the stages: published only once a round, a short
            // content pass falls between two snapshots and a small root reads
            // as `Walking → Done`.
            publish_status(status, run_start, &pipelines);

            if let Some(p) =
                rotated(&mut pipelines, round).find(|p| p.phase == RootPhase::Extracting)
            {
                progressed |= p.service_extracting(&mut cx)?;
            }
            round = round.wrapping_add(1);

            if !cleanup_done && pipelines.iter().all(|p| p.phase != RootPhase::Walking) {
                cleanup_done = true;
                cleanup_stale(&pipelines, &mut cx)?;
                progressed = true;
            }

            publish_status(status, run_start, &pipelines);

            if census::due(&mut last_census) {
                census::report(&cx, &pipelines, run_started);
            }

            wal.after_round(&cx, progressed)?;

            if pipelines.iter().all(|p| p.phase == RootPhase::Done) {
                // A stop can land with every root reaching Done before the
                // top-of-loop check sees the flag; re-read it, or a cut-short
                // run is stamped as a completed full index.
                break stop_flag.load(Ordering::Relaxed);
            }
            if !progressed {
                // Park on a walking root's channel rather than sleeping; see
                // `ParallelWalk::wait_ready`.
                let waited = pipelines
                    .iter_mut()
                    .find(|p| p.phase == RootPhase::Walking)
                    .map(|p| p.walk.wait_ready(IDLE_BACKOFF))
                    .is_some();
                // Measured: parking on an extracting root's channel gained
                // nothing; don't retry without an extraction-bound corpus.
                if !waited {
                    thread::sleep(IDLE_BACKOFF);
                }
            }
        };

        // The tail's readers, released before any of its writing. Every
        // per-root walk prefetcher and content feeder lives in `pipelines`, and
        // a TRUNCATE waits out `busy_timeout` for any of them still holding a
        // snapshot, then fails (see [`repo::checkpoint_truncate`]). Nothing
        // below reads `pipelines`: the stale cleanup and every status publish
        // are inside the loop, and the counts iterate `roots`.
        let tail_started = Instant::now();
        drop(pipelines);
        census::tail("dropping the readers", db_path, tail_started);

        checkpoint_tail(&cx, interrupt);
        census::tail("tail checkpoint", db_path, tail_started);

        report_run_warnings();
        let ended = if aborted {
            "stopped after"
        } else {
            "complete in"
        };
        crate::log_info!(
            "indexing {} {:.1}s",
            ended,
            run_started.elapsed().as_secs_f64()
        );
        if aborted {
            // Nothing is landed on the way out: "a stopped run promises
            // nothing", and the next run finds it all again. No stale cleanup
            // either: a partial walk's seen set would delete most of the
            // index. Not yet Idle: a stopped run is still followed by an
            // optimize pass, and the final status is the caller's to publish.
            return Ok(());
        }

        {
            let _maintaining = cx.maintaining(MaintenanceStep::MergingText);
            let conn = crate::lock_ok(&cx.conn_mutex);
            fts_finalize_after_text_indexing(&conn);
        }
        census::tail("the FTS merge", db_path, tail_started);

        {
            // An absent stamp reads as "never indexed" and `periodic_due`
            // starts another full run on the very next tick.
            let now = crate::log::now_unix();
            let conn = crate::lock_ok(&cx.conn_mutex);
            if let Err(e) = crate::db::repo::set_last_full_index(&conn, now) {
                crate::log_warn!("{}", e);
            }

            // Per-root figures, while the pages are warm; under the interrupt
            // guard because quitting should not wait out a per-root scan.
            let _maintaining = cx.maintaining(MaintenanceStep::RootCounts);
            let _guard = db::InterruptGuard::arm(interrupt, &conn);
            for root in &roots {
                match repo::count_root(&conn, &RootRange::for_root(root)) {
                    Ok(counts) => {
                        if let Err(e) = repo::set_root_counts(&conn, root, counts) {
                            crate::log_warn!("{}", e);
                        }
                    }
                    Err(e) => crate::log_warn!("counts for {} unavailable: {}", root, e),
                }
            }
        }
        census::tail("the per-root counts", db_path, tail_started);

        checkpoint_tail(&cx, interrupt);
        census::tail("tail checkpoint", db_path, tail_started);

        Ok(())
    }
}

/// Land the log during the run's tail: once right after the writer loop and
/// once before `repo::maintain`. Autocheckpoint is off for this connection
/// (see [`IndexingService::prologue`]), so nothing else lands it; without
/// these, the tail's writing (the FTS merge above all) piles onto the log
/// unbounded and `repo::maintain` runs its VACUUM on a full one.
///
/// The first lands the run's own writing, so what the log holds afterwards
/// is the tail's alone; it runs on a stopped run too, when the log is
/// largest. The second empties the log for the VACUUM, whose copy-back
/// pushes the whole database through it; `maintain`'s own leading
/// checkpoint is best-effort and swallows failure. Under the interrupt
/// guard: a quit must not wait on a checkpoint's lock, and an abandoned log
/// is safe, since the next run lands it.
fn checkpoint_tail(cx: &RunCx<'_>, interrupt: &db::InterruptSlot) {
    let _maintaining = cx.maintaining(MaintenanceStep::Checkpoint);
    let conn = crate::lock_ok(&cx.conn_mutex);
    let _guard = db::InterruptGuard::arm(interrupt, &conn);
    if let Err(e) = crate::db::repo::checkpoint_truncate(&conn) {
        crate::log_warn!("{}", e);
    }
}
