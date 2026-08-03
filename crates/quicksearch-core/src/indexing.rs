use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashSet;
use rusqlite::{params, Connection, OptionalExtension};

use crate::extract::Registry;
use crate::file_handling::{
    cleanup_stale_index_entries,
    count_tree_entries_fast,
    extract_one_batch,
    extract_scope_prepare,
    fts_finalize_after_text_indexing,
    process_batch_inserts,
    process_batch_updates,
    path_to_db_string,
    ExtractCursor,
    FileIndexAction,
    OwnedNewFile,
};
use crate::config::Config;
use crate::walk::{
    thread_count_for, walk_indexable_files, ParallelWalk, TryNext, WalkEvent, WorkerStats,
};
use crate::db;
use crate::db::repo;

/// Where one root's pipeline is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPhase {
    /// The parallel walk is discovering and writing file metadata.
    Walking,
    /// The walk finished; content extraction is draining this root's
    /// pending rows.
    Extracting,
    Done,
}

/// Progress for one indexing root. Each root runs its own walker and its
/// own extraction cursor; the GUI shows one row per root.
#[derive(Debug, Clone)]
pub struct RootProgress {
    pub root: String,
    pub phase: RootPhase,
    /// Files the walk has seen so far.
    pub walked: usize,
    /// Concurrent `find`-based denominator; `None` until the count lands.
    pub walk_total: Option<usize>,
    /// Rows with searchable text: extracted in earlier runs plus this one.
    pub extracted: usize,
    /// The root's whole searchable set: pending + already-extracted rows
    /// at the moment the walk finished.
    pub extract_total: usize,
    pub current_file: Option<String>,
    /// Walker threads busy right now / pool size.
    pub active_workers: usize,
    pub total_workers: usize,
}

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    Running {
        start_time: Instant,
        roots: Vec<RootProgress>,
    },
    Stopping,
    Error(String),
}

/// One setting whose stored (index-build-time) value differs from the
/// current config. Values that hold lists (roots, patterns, extensions)
/// are newline-joined — display them as multi-line columns, not inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    pub key: String,
    /// What the index was built with.
    pub stored: String,
    /// What the config says now.
    pub current: String,
}

#[derive(Debug, Clone)]
pub enum IndexingCommand {
    Start {
        /// One or more directory roots to index. Order determines walk order;
        /// duplicates are silently dropped at run time.
        paths: Vec<String>,
        db_path: String,
        config: Config,
    },
    Stop,
}

/// Collect rows whose parent directory the walk never reached.
///
/// Per-directory reconciliation can only speak for directories it read, so a
/// directory deleted wholesale — or newly excluded by an ignore pattern —
/// leaves its rows unaccounted for. This finds them by scanning the distinct
/// parents stored under `root` and keeping the ones absent from `seen_dirs`.
///
/// Two kinds of absence are *not* deletions and are filtered out:
///
/// - A parent under a directory the walk could not read. Its children were
///   never discovered, so their absence proves nothing.
/// - A path reached by resolving a symlink. Its row's parent may lie outside
///   every root and so is never visited by construction; `aliased` is the
///   record that the file itself was seen.
///
/// The parent scan streams (see [`repo::for_each_parent_in_range`]) so this
/// costs memory proportional to the *unvisited* directories, not to the tree.
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
    let conn = conn_mutex.lock().unwrap();

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

#[derive(Debug)]
pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
    command_tx: mpsc::Sender<IndexingCommand>,
    db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    suspend_flag: Arc<AtomicBool>,
    _handle: thread::JoinHandle<()>,
}

/// Polling interval for `should_abort` while suspended.
const SUSPEND_POLL_MS: u64 = 100;

/// Combined stop/suspend check used by worker loops. Returns `true` iff the
/// caller should abort the operation. While the suspend flag is set and stop
/// is not, this parks the thread by sleeping in short increments so a later
/// `resume()` unblocks it. Cheap to call in tight loops.
pub(crate) fn should_abort(
    stop: &Arc<Mutex<bool>>,
    suspend: &Arc<AtomicBool>,
) -> bool {
    loop {
        if *stop.lock().unwrap() {
            return true;
        }
        if !suspend.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(SUSPEND_POLL_MS));
    }
}

/// Set process priority for background operation
// fn set_background_priority() {
//     #[cfg(windows)]
//     {
//         use std::os::windows::raw::HANDLE;
        
//         // Windows implementation
//         extern "system" {
//             fn GetCurrentProcess() -> HANDLE;
//             fn SetPriorityClass(hprocess: HANDLE, dwpriorityclass: u32) -> i32;
//         }
        
//         const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
//         unsafe {
//             SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
//         }
//     }
    
//     #[cfg(unix)]
//     {
//         // Unix implementation  
//         use std::os::unix::process::CommandExt;
//         unsafe {
//             libc::nice(10); // Lower priority
//         }
//     }
// }

/// Flips an [`AtomicBool`] when dropped. Held by `run_indexing` so the
/// per-root count subprocesses die on every exit path of a run.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// One root's in-flight indexing state, owned by the writer loop.
struct RootPipeline {
    root: String,
    walk: ParallelWalk,
    stats: WorkerStats,
    /// Concurrent `find` count; 0 = not yet known.
    count_total: Arc<AtomicUsize>,
    pending_updates: Vec<OwnedNewFile>,
    pending_inserts: Vec<OwnedNewFile>,
    walked: usize,
    walk_clean: bool,
    phase: RootPhase,
    extract: Option<ExtractCursor>,
    extract_total: usize,
    extracted: usize,
    current_file: Option<String>,
}

impl IndexingService {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(IndexingStatus::Idle));
        let (command_tx, command_rx) = mpsc::channel();
        let db_connection = Arc::new(Mutex::new(None));
        let suspend_flag = Arc::new(AtomicBool::new(false));

        let status_clone = status.clone();
        let db_connection_clone = db_connection.clone();
        let suspend_clone = suspend_flag.clone();
        let handle = thread::spawn(move || {
            Self::indexing_thread(status_clone, command_rx, db_connection_clone, suspend_clone);
        });

        IndexingService {
            status,
            command_tx,
            db_connection,
            suspend_flag,
            _handle: handle,
        }
    }

    /// Pause the indexer. All worker loops that call [`should_abort`] will
    /// block until [`resume`](Self::resume) is called. No-op if already
    /// suspended. Does not stop the worker — stop_indexing is still the way
    /// to abort.
    pub fn suspend(&self) {
        self.suspend_flag.store(true, Ordering::Relaxed);
    }

    /// Resume indexing after [`suspend`](Self::suspend). No-op if not
    /// suspended.
    pub fn resume(&self) {
        self.suspend_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_suspended(&self) -> bool {
        self.suspend_flag.load(Ordering::Relaxed)
    }

    /// Start indexing one or more roots. Paths are walked in order; duplicate
    /// or nested roots are de-duplicated by the indexer. At least one path is
    /// required.
    pub fn start_indexing(
        &self,
        paths: Vec<String>,
        db_path: String,
        config: Config,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("start_indexing requires at least one path".into());
        }
        self.command_tx
            .send(IndexingCommand::Start { paths, db_path, config })
            .map_err(|e| format!("Failed to send start command: {}", e))
    }

    /// Signal a running index pass to stop without waiting for it. Used
    /// on shutdown paths that must stay responsive — the worker notices
    /// the flag between batches and WAL makes an unflushed exit safe.
    pub fn request_stop(&self) {
        let _ = self.command_tx.send(IndexingCommand::Stop);
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        // First send the stop command
        self.command_tx
            .send(IndexingCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))?;

        // Wait for indexing to transition to stopping state
        let mut attempts = 0;
        while attempts < 50 { // Wait up to 5 seconds
            match self.get_status() {
                IndexingStatus::Stopping => break,
                IndexingStatus::Idle => return Ok(()), // Already stopped
                IndexingStatus::Error(_) => return Ok(()), // Consider error state as stopped
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
            }
        }

        // Flush the WAL and release the shared connection. WAL mode itself
        // stays on — it's the persistent journal mode for the index.
        if let Ok(mut db_opt) = self.db_connection.lock() {
            if let Some(db_conn_arc) = db_opt.take() {
                if let Ok(conn) = db_conn_arc.lock() {
                    let _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE);", ());
                }
            }
        }

        Ok(())
    }

    pub fn get_status(&self) -> IndexingStatus {
        self.status.lock().unwrap().clone()
    }

    /// Force graceful shutdown - used for signal handling
    pub fn graceful_shutdown(&self) -> Result<(), String> {
        self.stop_indexing()
    }

    /// Check if configuration changes require index recreation. A pure
    /// *read* check: opens the existing index without ever wiping it (the
    /// old `open_or_recreate` here could destroy the index before the user
    /// confirmed the rebuild dialog). A missing or incompatible DB means
    /// there is nothing to validate — the indexer will (re)build under its
    /// own policy anyway.
    pub fn check_config_validation(&self, db_path: &str, config: &Config, indexing_path: &str) -> Result<Option<Vec<ConfigChange>>, String> {
        match db::open_existing(db_path, false) {
            Ok(conn) => Self::validate_config(&conn, config, indexing_path),
            Err(_) => Ok(None),
        }
    }

    /// Stop indexing and delete the database file for a clean rebuild
    pub fn delete_index_for_rebuild(&self, db_path: &str) -> Result<(), String> {
        // Stop any running indexing first
        self.stop_indexing()
            .map_err(|e| format!("Failed to stop indexing: {}", e))?;

        // Wait for indexing to actually stop
        let mut attempts = 0;
        while attempts < 50 { // Wait up to 5 seconds
            match self.get_status() {
                IndexingStatus::Idle => break,
                IndexingStatus::Stopping | IndexingStatus::Running { .. } => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
                IndexingStatus::Error(_) => break, // Consider error state as stopped
            }
        }

        // Delete the database file and its WAL sidecars.
        if std::path::Path::new(db_path).exists() {
            std::fs::remove_file(db_path)
                .map_err(|e| format!("Failed to delete database file: {}", e))?;
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{}", db_path, suffix));
        }

        Ok(())
    }

    fn indexing_thread(
        status: Arc<Mutex<IndexingStatus>>,
        command_rx: mpsc::Receiver<IndexingCommand>,
        db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        suspend_flag: Arc<AtomicBool>,
    ) {
        let stop_flag = Arc::new(Mutex::new(false));
        let mut indexing_handle: Option<thread::JoinHandle<()>> = None;
        
        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start { paths, db_path, config } => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        continue; // Already running
                    }

                    // Join any previous indexing thread
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    *stop_flag.lock().unwrap() = false;
                    // One placeholder row per root so the GUI has structure
                    // to draw before the writer loop publishes real numbers.
                    *status.lock().unwrap() = IndexingStatus::Running {
                        start_time: Instant::now(),
                        roots: paths
                            .iter()
                            .map(|p| RootProgress {
                                root: p.clone(),
                                phase: RootPhase::Walking,
                                walked: 0,
                                walk_total: None,
                                extracted: 0,
                                extract_total: 0,
                                current_file: Some("Starting…".to_string()),
                                active_workers: 0,
                                total_workers: 0,
                            })
                            .collect(),
                    };

                    // Run indexing in a separate thread
                    let status_clone = status.clone();
                    let stop_flag_clone = stop_flag.clone();
                    let paths_owned = paths.clone();
                    let db_path_owned = db_path.clone();
                    let config_owned = config.clone();

                    let db_connection_clone = db_connection.clone();
                    let suspend_clone = suspend_flag.clone();
                    indexing_handle = Some(thread::spawn(move || {
                        if let Err(e) = Self::run_indexing(&status_clone, &paths_owned, &db_path_owned, &stop_flag_clone, &suspend_clone, &config_owned, &db_connection_clone) {
                            *status_clone.lock().unwrap() = IndexingStatus::Error(e);
                        } else {
                            // Only set to Idle if we weren't stopped
                            if !*stop_flag_clone.lock().unwrap() {
                                *status_clone.lock().unwrap() = IndexingStatus::Idle;
                            }
                        }

                        // Clear the database connection when indexing completes
                        if let Ok(mut db_opt) = db_connection_clone.lock() {
                            *db_opt = None;
                        }
                    }));
                }
                IndexingCommand::Stop => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        *status.lock().unwrap() = IndexingStatus::Stopping;
                        *stop_flag.lock().unwrap() = true;
                    }
                }
            }
        }
        
        // Clean up any remaining indexing thread
        if let Some(handle) = indexing_handle {
            let _ = handle.join();
        }
    }

    fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        paths: &[String],
        db_path: &str,
        stop_flag: &Arc<Mutex<bool>>,
        suspend_flag: &Arc<AtomicBool>,
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("run_indexing: no paths provided".into());
        }

        // De-duplicate while preserving order. Roots are canonicalized first
        // so `/home/jeremy` and `/home/jeremy/` (or a symlink to either)
        // collapse to one walk. Pure nested-root deduplication (skip a root
        // that is a prefix of an already-walked root) is handled by the
        // per-file `seen_paths` set below.
        let mut seen_roots = HashSet::new();
        let roots: Vec<String> = paths
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .canonicalize()
                    .ok()
                    // Same spelling rules as `files.path`: a hand-rolled
                    // four-character strip turns `\\?\UNC\server\share` into
                    // `UNC\server\share`, which is not a path — and no longer
                    // looks like a share, so the root would silently walk with
                    // the local thread count instead of the network one.
                    .map(|c| path_to_db_string(&c))
                    .unwrap_or_else(|| p.clone())
            })
            .filter(|p| seen_roots.insert(p.clone()))
            .collect();

        // Open and migrate the database to the current schema version.
        let conn = db::open_or_recreate(db_path, &config.processing.tokenize)?;

        // Update configuration (for new installations or when no validation issues).
        // `indexing_path` in the validation table stores the joined list so
        // adding/removing a root triggers the same rebuild prompt as changing
        // the legacy single path did.
        Self::update_config(&conn, config, &roots.join("\n"))?;

        // No up-front load of the whole `files` table: each walk's prefetcher
        // fetches one directory's rows at a time, so classification data is
        // never all resident at once and the walk starts immediately instead
        // of after a full table scan.

        let conn_mutex = Arc::new(Mutex::new(conn));
        
        // Store the database connection for proper cleanup on stop
        if let Ok(mut db_opt) = db_connection.lock() {
            *db_opt = Some(conn_mutex.clone());
        }

        let run_start = Instant::now();

        // Per-root concurrent counts, killed the moment this run exits by
        // any path — the guard flips the token on drop and the count
        // threads' subprocesses die within one poll interval.
        let count_cancel = Arc::new(AtomicBool::new(false));
        let _count_guard = CancelOnDrop(count_cancel.clone());

        // Shared with every root's walk workers, which use it to finish small
        // text files without handing them to the content pass below.
        let registry = Arc::new(Registry::default_set());
        let quantum = config.processing.batch_size.max(1);

        // One pipeline per root: its own walker (with per-root worker
        // count), its own count thread, its own buffers and extraction
        // cursor. They all funnel into this single writer thread.
        let mut pipelines: Vec<RootPipeline> = Vec::with_capacity(roots.len());
        for root in &roots {
            let ignore = crate::config::IgnoreSet::compile(&config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?;
            let workers = config
                .indexing
                .root_workers
                .get(root)
                .copied()
                .filter(|w| *w > 0)
                .unwrap_or_else(|| thread_count_for(std::slice::from_ref(root)))
                .clamp(1, 64);
            let walk = walk_indexable_files(
                std::slice::from_ref(root),
                config.indexing.follow_symlinks,
                config.indexing.include_hidden,
                ignore,
                db_path,
                config.clone(),
                registry.clone(),
                stop_flag.clone(),
                suspend_flag.clone(),
                workers,
            );
            let stats = walk.worker_stats();

            let count_total = Arc::new(AtomicUsize::new(0));
            {
                let root = root.clone();
                let cancel = count_cancel.clone();
                let total = count_total.clone();
                let _ = thread::Builder::new().name("qs-count".into()).spawn(move || {
                    match count_tree_entries_fast(&root, &cancel) {
                        // A genuinely empty root stores 1 so "known" stays
                        // distinguishable from the 0 = unknown sentinel; an
                        // empty root's walk finishes instantly anyway.
                        Ok(n) => total.store(n.max(1), Ordering::Relaxed),
                        Err(e) => {
                            if !e.contains("cancelled") {
                                crate::log_warn!("count for {}: {}", root, e);
                            }
                        }
                    }
                });
            }

            pipelines.push(RootPipeline {
                root: root.clone(),
                walk,
                stats,
                count_total,
                pending_updates: Vec::new(),
                pending_inserts: Vec::new(),
                walked: 0,
                walk_clean: true,
                phase: RootPhase::Walking,
                extract: None,
                extract_total: 0,
                extracted: 0,
                current_file: None,
            });
        }

        // Publish a status snapshot. Never clobbers Stopping — the command
        // thread owns that transition.
        let publish = |pipelines: &[RootPipeline]| {
            let roots: Vec<RootProgress> = pipelines
                .iter()
                .map(|p| RootProgress {
                    root: p.root.clone(),
                    phase: p.phase,
                    walked: p.walked,
                    walk_total: match p.count_total.load(Ordering::Relaxed) {
                        0 => None,
                        n => Some(n),
                    },
                    extracted: p.extracted,
                    extract_total: p.extract_total,
                    current_file: p.current_file.clone(),
                    active_workers: p.stats.active(),
                    total_workers: p.stats.total(),
                })
                .collect();
            if let Ok(mut g) = status.lock() {
                if !matches!(*g, IndexingStatus::Stopping) {
                    *g = IndexingStatus::Running { start_time: run_start, roots };
                }
            }
        };
        publish(&pipelines);

        // Round-robin with skipping: each round takes at most one quantum
        // from every root that has work ready. Write-bottlenecked, all
        // active roots get even quanta; read-bottlenecked, roots with
        // empty channels are skipped and the firehose roots get the
        // writer's full attention.
        // 128-bit path digests, not paths. Its only job is to drop a repeat
        // visit, and at millions of files owning every path string again —
        // on top of the rows SQLite already holds — was the single largest
        // allocation in a run. See `walk::path_digest`.
        let mut seen_paths: HashSet<u128> = HashSet::new();
        // Rows the per-directory reconciliation found no file behind, plus
        // whatever the vanished-directory sweep adds once the walks end.
        let mut stale_candidates: Vec<String> = Vec::new();
        // Paths reached by resolving a symlink, whose row lives under a
        // parent that may be outside every root.
        let mut aliased_paths: HashSet<String> = HashSet::new();
        let mut aborted = false;
        let mut stale_cleanup_ok = true;
        let mut cleanup_done = false;
        let mut stale_deleted = 0usize;
        let mut rr = 0usize;

        loop {
            if should_abort(stop_flag, suspend_flag) {
                aborted = true;
                break;
            }
            let mut progressed = false;
            let n = pipelines.len();
            for k in 0..n {
                let p = &mut pipelines[(rr + k) % n];
                match p.phase {
                    RootPhase::Walking => {
                        let mut took = 0usize;
                        while took < quantum {
                            match p.walk.try_next() {
                                TryNext::Item(WalkEvent::Stale(paths)) => {
                                    took += 1;
                                    // Applied at the end of the run, not here:
                                    // deleting mid-walk would break the "a
                                    // stopped run deletes nothing" guarantee,
                                    // and an aliased sighting that exempts a
                                    // path may still be ahead of us.
                                    stale_candidates.extend(paths);
                                }
                                TryNext::Item(WalkEvent::File(file)) => {
                                    took += 1;
                                    p.walked += 1;
                                    if p.walked % 64 == 0 {
                                        p.current_file = Some(file.path.clone());
                                    }
                                    if file.aliased {
                                        // Its row's parent is a directory this
                                        // walk may never visit, so the
                                        // vanished-directory sweep must not
                                        // treat that parent's absence as proof
                                        // the file is gone.
                                        aliased_paths.insert(file.path.clone());
                                    }
                                    // Dedupes a canonical file reachable
                                    // through several spellings, or from more
                                    // than one root.
                                    if !seen_paths.insert(file.digest) {
                                        continue;
                                    }
                                    let Some(rec) = file.record else { continue };
                                    if file.action == FileIndexAction::Update {
                                        p.pending_updates.push(rec);
                                        if p.pending_updates.len() >= quantum {
                                            process_batch_updates(
                                                &conn_mutex,
                                                &p.pending_updates,
                                                stop_flag,
                                                config,
                                            )?;
                                            p.pending_updates.clear();
                                        }
                                    } else {
                                        p.pending_inserts.push(rec);
                                        if p.pending_inserts.len() >= quantum {
                                            process_batch_inserts(
                                                &conn_mutex,
                                                &p.pending_inserts,
                                                stop_flag,
                                                config,
                                            )?;
                                            p.pending_inserts.clear();
                                        }
                                    }
                                }
                                TryNext::Empty => break,
                                TryNext::Finished => {
                                    // Join before deciding anything: workers
                                    // close the channel when they stop for
                                    // *any* reason, so a panic and a finished
                                    // walk look identical from here.
                                    p.walk_clean = p.walk.finish();
                                    process_batch_updates(
                                        &conn_mutex,
                                        &p.pending_updates,
                                        stop_flag,
                                        config,
                                    )?;
                                    p.pending_updates.clear();
                                    process_batch_inserts(
                                        &conn_mutex,
                                        &p.pending_inserts,
                                        stop_flag,
                                        config,
                                    )?;
                                    p.pending_inserts.clear();

                                    if !p.walk_clean {
                                        crate::log_warn!(
                                            "a walk worker for {} terminated abnormally; \
                                             skipping stale cleanup",
                                            p.root
                                        );
                                        stale_cleanup_ok = false;
                                        p.phase = RootPhase::Done;
                                    } else if *stop_flag.lock().unwrap() {
                                        p.phase = RootPhase::Done;
                                    } else {
                                        let cursor = ExtractCursor::for_root(&p.root);
                                        let scope =
                                            extract_scope_prepare(&conn_mutex, &cursor, config)?;
                                        // Progress counts the root's whole
                                        // searchable set: files extracted in
                                        // earlier runs start the counter, so
                                        // an unchanged root shows "X of X"
                                        // rather than "0 of 0".
                                        p.extract_total = scope.pending + scope.already_done;
                                        p.extracted = scope.already_done;
                                        if scope.pending == 0 {
                                            p.phase = RootPhase::Done;
                                        } else {
                                            p.extract = Some(cursor);
                                            p.phase = RootPhase::Extracting;
                                        }
                                    }
                                    progressed = true;
                                    break;
                                }
                            }
                        }
                        progressed |= took > 0;
                    }
                    RootPhase::Extracting => {
                        let cursor = p.extract.as_mut().expect("extracting root has a cursor");
                        let mut last_file: Option<String> = None;
                        let processed = extract_one_batch(
                            &conn_mutex,
                            cursor,
                            &registry,
                            config,
                            stop_flag,
                            suspend_flag,
                            &mut |name| last_file = Some(name.to_string()),
                        )?;
                        if last_file.is_some() {
                            p.current_file = last_file;
                        }
                        if processed == 0 {
                            p.extract = None;
                            p.phase = RootPhase::Done;
                        } else {
                            p.extracted += processed;
                        }
                        progressed = true;
                    }
                    RootPhase::Done => {}
                }
            }
            rr = rr.wrapping_add(1);

            // Once every walk has ended, reconcile deletions — globally,
            // because a file may be reachable through more than one root's
            // symlinks. Runs at most once per run, on this writer thread.
            if !cleanup_done && pipelines.iter().all(|p| p.phase != RootPhase::Walking) {
                cleanup_done = true;
                let stopped = *stop_flag.lock().unwrap();
                if stale_cleanup_ok && !stopped {
                    // Directories that vanished entirely are never read, so
                    // per-directory reconciliation never sees them; only a
                    // scan of stored parents against the ones the walk
                    // reached can find the rows beneath them.
                    for p in &pipelines {
                        sweep_unvisited_parents(
                            &conn_mutex,
                            &p.root,
                            &p.walk.seen_dirs(),
                            p.walk.unreadable(),
                            &aliased_paths,
                            &mut stale_candidates,
                        )?;
                    }
                    // No unreadable-directory filter here: neither source of
                    // candidates can produce one. `read_directory` returns
                    // before reconciling a directory it could not read, and
                    // the sweep skips parents beneath one. Re-checking here
                    // as well would put the same rule in two places, free to
                    // drift, and the tests could not tell which one held.
                    let stale_paths: Vec<String> = stale_candidates.drain(..).collect();
                    let unreadable_count: usize = pipelines
                        .iter()
                        .map(|p| p.walk.unreadable().paths().len())
                        .sum();
                    if unreadable_count > 0 {
                        crate::log_warn!(
                            "{} director{} could not be read; index entries beneath \
                             them were kept rather than deleted",
                            unreadable_count,
                            if unreadable_count == 1 { "y" } else { "ies" }
                        );
                    }
                    if !stale_paths.is_empty() {
                        if let Some(first) = pipelines.first_mut() {
                            first.current_file =
                                Some("Removing stale index entries…".to_string());
                        }
                        stale_deleted = cleanup_stale_index_entries(
                            &conn_mutex,
                            stale_paths.as_slice(),
                            stop_flag,
                            suspend_flag,
                        )?;
                    }
                }
                progressed = true;
            }

            publish(&pipelines);

            if pipelines.iter().all(|p| p.phase == RootPhase::Done) {
                break;
            }
            if !progressed {
                thread::sleep(Duration::from_millis(2));
            }
        }

        if aborted {
            // Buffered records are valid work — land them before leaving.
            for p in &mut pipelines {
                process_batch_updates(&conn_mutex, &p.pending_updates, stop_flag, config)?;
                p.pending_updates.clear();
                process_batch_inserts(&conn_mutex, &p.pending_inserts, stop_flag, config)?;
                p.pending_inserts.clear();
            }
            if let Ok(mut status_guard) = status.lock() {
                *status_guard = IndexingStatus::Idle;
            }
            // Deliberately no stale cleanup: a partial walk has a partial
            // seen set, and deleting everything it did not reach would
            // empty most of the index.
            return Ok(());
        }

        // FTS housekeeping once per completed run (cheap if nothing changed).
        let _ = stale_deleted;
        {
            let conn = conn_mutex.lock().unwrap();
            fts_finalize_after_text_indexing(&conn)?;
        }

        // Stamp the successful run so the coordinator can schedule the next
        // periodic reindex from it.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(conn) = conn_mutex.lock() {
            let _ = crate::db::repo::set_last_full_index(&conn, now);
        }

        Ok(())
    }

    /// The config keys whose change invalidates the stored index, paired
    /// with their current values. One list drives both [`validate_config`]
    /// and [`update_config`] so the two can never drift apart.
    fn config_validation_entries(config: &Config, indexing_path: &str) -> Vec<(&'static str, String)> {
        let sorted_joined = |v: &[String]| {
            let mut v: Vec<String> = v.to_vec();
            v.sort();
            v.join("\n")
        };
        vec![
            ("hash_length", config.processing.hash_length.to_string()),
            // Hashes are only comparable within one algorithm, and
            // duplicate detection groups purely by hash. Bump this string
            // whenever the digest input changes so existing indexes are
            // offered a rebuild instead of silently mixing schemes.
            ("hash_algorithm", "size+head".to_string()),
            ("indexing_path", normalize_root_string(indexing_path)),
            ("tokenize", config.processing.tokenize.clone()),
            ("include_hidden", config.indexing.include_hidden.to_string()),
            (
                "ignore_patterns",
                sorted_joined(&config.indexing.ignore_patterns),
            ),
            (
                "content_extensions",
                sorted_joined(&config.indexing.content_extensions),
            ),
        ]
    }

    /// Compare current config against the values stored in the index.
    /// Returns `Some(changes)` when the index was built under settings
    /// that no longer match — the caller offers a rebuild. A key absent
    /// from the DB (older index) only counts as changed when the DB has
    /// stored *any* validation state before.
    fn validate_config(
        conn: &Connection,
        config: &Config,
        indexing_path: &str,
    ) -> Result<Option<Vec<ConfigChange>>, String> {
        let mut changes = Vec::new();
        for (key, current) in Self::config_validation_entries(config, indexing_path) {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT value FROM config_validation WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("read config_validation.{}: {}", key, e))?;
            if let Some(stored) = stored {
                if stored != current {
                    changes.push(ConfigChange {
                        key: key.to_string(),
                        stored,
                        current,
                    });
                }
            }
        }
        Ok(if changes.is_empty() { None } else { Some(changes) })
    }

    /// Stamp the index with the settings it's being built under.
    fn update_config(conn: &Connection, config: &Config, indexing_path: &str) -> Result<(), String> {
        for (key, current) in Self::config_validation_entries(config, indexing_path) {
            conn.execute(
                "INSERT OR REPLACE INTO config_validation (key, value) VALUES (?1, ?2)",
                params![key, current],
            )
            .map_err(|e| format!("store config_validation.{}: {}", key, e))?;
        }
        Ok(())
    }
}

/// Canonicalize a root string for storage/comparison, stripping the Windows
/// UNC prefix. Multi-root strings (newline-joined) fail canonicalize and
/// pass through verbatim, which still compares consistently.
fn normalize_root_string(indexing_path: &str) -> String {
    let path = std::path::Path::new(indexing_path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(indexing_path));
    path_to_db_string(&path)
}

impl Drop for IndexingService {
    fn drop(&mut self) {
        // Ensure graceful shutdown when the service is dropped
        let _ = self.stop_indexing();
    }
}

impl Default for IndexingService {
    fn default() -> Self {
        Self::new()
    }
}

