//! Parallel filesystem walk: one shared queue of directories, N workers. A
//! worker reads a directory **and** does its per-file work before moving on:
//! on SMB the directory read primes the client's attribute cache for only
//! about a second (`actimeo`), so an immediate `stat` is free and a late one
//! is a full network round trip. Every path below a root is canonical by
//! construction: roots are canonicalized once at seed time and directories
//! only ever reached by joining names onto them.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::config::{Config, IgnoreSet};
use crate::extract::Registry;
use crate::file_handling::{
    classify_by_mtime, classify_for_indexing, dir_to_db_parent, path_to_db_string,
    prepare_file_record, DirRows, FileIndexAction, OwnedNewFile, UnreadableDirs,
};

mod pool;
#[cfg(test)]
mod tests;

pub(crate) use pool::WorkerStats;
use pool::{Found, PrefetchWork, Queue, Shared};

/// Files one worker takes for itself before handing the rest to the pool.
const FILES_PER_JOB: usize = 128;

const CHANNEL_CAP: usize = 4096;

const LOCAL_THREADS: usize = 4;

const NETWORK_THREADS: usize = 16;

/// One file the walk found, with everything the DB writer needs.
#[derive(Debug)]
pub struct WalkedFile {
    /// Canonical path. The row it keys is `(parent, name)`; see
    /// [`crate::file_handling::split_db_path`].
    pub path: String,
    pub action: FileIndexAction,
    pub record: Option<OwnedNewFile>,
    /// 128-bit truncated SHA-256 of the path, for the duplicate-visit set.
    pub digest: u128,
    /// True when this file was reached by resolving a symlink. Its row is
    /// invisible to its real parent's reconciliation, so the caller must
    /// exempt it from the vanished-directory sweep.
    pub aliased: bool,
}

impl WalkedFile {
    /// Seen, but with nothing to write: the row stays.
    fn skipped(path: String, digest: u128, aliased: bool) -> Self {
        WalkedFile {
            path,
            action: FileIndexAction::Skip,
            record: None,
            digest,
            aliased,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WalkEvent {
    File(WalkedFile),
    /// Paths whose row should be deleted: in one directory's index rows,
    /// absent from its listing. Emitted only for successfully read dirs.
    Stale(Vec<String>),
}

/// One file a directory read produced. `cached` is `Some` only on Windows,
/// where `FindNextFileW` returns size, mtime and attributes alongside the
/// name; Unix `getdents64` returns only `d_type`.
struct PendingFile {
    path: PathBuf,
    cached: Option<crate::platform::CachedMetadata>,
}

impl PendingFile {
    fn uncached(path: PathBuf) -> Self {
        PendingFile { path, cached: None }
    }
}

enum Job {
    Dir(PathBuf, Arc<DirRows>),
    Files(Vec<PendingFile>, Arc<DirRows>),
    /// A resolved symlink target, with the stored mtime for its own path.
    Alias(PathBuf, Option<u64>),
}

/// How many entries each filter rejected. A pruned *directory* is one
/// increment, not one per file beneath it: the subtree is never enumerated.
#[derive(Debug, Default)]
pub struct PruneCounts {
    pub dot_named: AtomicU64,
    /// Windows entries carrying `FILE_ATTRIBUTE_HIDDEN`.
    pub attribute: AtomicU64,
    pub ignored: AtomicU64,
}

impl PruneCounts {
    pub fn total(&self) -> u64 {
        self.dot_named.load(Ordering::Relaxed)
            + self.attribute.load(Ordering::Relaxed)
            + self.ignored.load(Ordering::Relaxed)
    }

    pub fn summary(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        Some(format!(
            "pruned {} entries: {} hidden by attribute, {} dot-named, {} by ignore pattern",
            self.total(),
            self.attribute.load(Ordering::Relaxed),
            self.dot_named.load(Ordering::Relaxed),
            self.ignored.load(Ordering::Relaxed),
        ))
    }
}

struct Ctx {
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: IgnoreSet,
    /// The index's own files, which this walk must never so much as open —
    /// see [`crate::file_handling::index_file_set`] for why opening one is fatal.
    index_files: HashSet<PathBuf>,
    pruned: PruneCounts,
    config: Config,
    registry: Arc<Registry>,
    unreadable: UnreadableDirs,
    stop_flag: Arc<AtomicBool>,
}

static UNREADABLE_WARNINGS: crate::log::Throttle = crate::log::Throttle::new(20);

/// The same, for unrepresentable names — a share can hold thousands.
static UNREPRESENTABLE_WARNINGS: crate::log::Throttle = crate::log::Throttle::new(20);

pub fn reset_run_warnings() {
    UNREADABLE_WARNINGS.reset();
    UNREPRESENTABLE_WARNINGS.reset();
}

/// Read one directory: subdirectories and overflow file chunks go to `found`
/// for the pool, the remaining files come back for this worker, `stale` gets
/// the paths whose row has no file behind it. A directory that cannot be read
/// returns before reconciling — it must not read as an empty one.
fn read_directory(
    dir: &Path,
    rows: &Arc<DirRows>,
    ctx: &Ctx,
    found: &mut Vec<Found>,
    stale: &mut Vec<String>,
) -> Vec<PendingFile> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            if UNREADABLE_WARNINGS.allow() {
                crate::log_warn!("cannot read {}: {}", dir.display(), e);
            }
            // "Gone" is not "could not look": a directory deleted mid-walk
            // *should* fall to the stale sweep. Only `NotFound` is unambiguous
            // — the same distinction `verb_for` draws.
            if e.kind() != std::io::ErrorKind::NotFound {
                ctx.unreadable.record(dir.to_path_buf());
            }
            return Vec::new();
        }
    };
    // Every `continue` in the loop must be a genuine "not indexable", or the
    // stale diff below deletes live rows.
    let mut present: HashSet<String> = HashSet::new();
    let mut unreadable_entry = false;

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                if UNREADABLE_WARNINGS.allow() {
                    crate::log_warn!("cannot read an entry of {}: {}", dir.display(), e);
                }
                ctx.unreadable.record(dir.to_path_buf());
                // An incomplete listing cannot decide what is missing: an
                // entry we failed to read looks identical to a deleted one.
                unreadable_entry = true;
                continue;
            }
        };

        let name = entry.file_name();
        // **The screen for names the index cannot spell, and the only one**
        // (invalid UTF-8 on Unix, unpaired UTF-16 surrogates on Windows).
        // Skipped by choice: every path in this walk is a database *key*, and
        // a lossy path must never become a DB key — the lossy spelling names a
        // different file. Leaving the entry out of `present` is safe: no
        // stored row can carry such a name, so there is no row to protect.
        let Some(name) = name.to_str() else {
            if UNREPRESENTABLE_WARNINGS.allow() {
                crate::log_warn!(
                    "Skipping {:?} (name is not valid UTF-8, so it cannot be stored, hashed \
                     or text-indexed)",
                    entry.path()
                );
            }
            continue;
        };
        // The closure runs only on Windows, where `entry.metadata()` is free
        // and reports the entry itself, not a link target.
        if !ctx.include_hidden {
            if let Some(reason) =
                crate::platform::entry_hidden_reason(name, || entry.metadata().ok())
            {
                match reason {
                    crate::platform::HiddenReason::DotPrefix => {
                        ctx.pruned.dot_named.fetch_add(1, Ordering::Relaxed);
                    }
                    crate::platform::HiddenReason::Attribute => {
                        ctx.pruned.attribute.fetch_add(1, Ordering::Relaxed);
                        // A folder skipped over an attribute Explorer does
                        // not show has no other way of being discovered.
                        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                            crate::log_info!(
                                "skipping {}: hidden attribute set (enable \"include hidden \
                                 files\" to index it)",
                                entry.path().display()
                            );
                        }
                    }
                }
                continue;
            }
        }
        if ctx.ignore.matches_component(name) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let path = entry.path();
        if ctx.ignore.matches_path_pattern(&path) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // The index's own database and sidecars — hashing one cancels
        // SQLite's locks process-wide (`file_handling::index_file_set`).
        // `continue`, not `skipped`: the name stays out of `present`, so old
        // rows for the index fall to the stale sweep; `skipped` keeps them.
        if ctx.index_files.contains(&path) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // `file_type` is the cached `d_type` from the directory read.
        match entry.file_type() {
            // Not marked present: a name that was a file last run and is a
            // directory now *should* lose its row.
            Ok(ft) if ft.is_dir() => found.push(Found::Dir(path)),
            Ok(ft) if ft.is_symlink() => {
                // Directory and file targets gate together, or the walkers
                // disagree: `filtered_walk` follows neither kind, so a file
                // target followed only here would never update between runs.
                if !ctx.follow_symlinks {
                    continue;
                }
                // Normalized like the roots, or on Windows the target keeps
                // `canonicalize`'s `\\?\` prefix, under which ignore patterns
                // never match and `seen_dirs` cannot dedup.
                if let Ok(target) = path.canonicalize() {
                    // The target is a different path than the screened link;
                    // its lossy spelling would name some other file entirely.
                    if target.to_str().is_none() {
                        if UNREPRESENTABLE_WARNINGS.allow() {
                            crate::log_warn!(
                                "Skipping {} (its target {:?} is not valid UTF-8, so it cannot \
                                 be stored, hashed or text-indexed)",
                                path.display(),
                                target
                            );
                        }
                        continue;
                    }
                    let target = PathBuf::from(path_to_db_string(&target));
                    // A symlink pointing at the index would otherwise walk
                    // straight into an `open`.
                    if ctx.index_files.contains(&target) {
                        ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    match fs::metadata(&target) {
                        Ok(m) if m.is_dir() => found.push(Found::Dir(target)),
                        // The target's row belongs to its own directory, so
                        // it is not marked present here.
                        Ok(_) => found.push(Found::Alias(target)),
                        Err(_) => {}
                    }
                }
            }
            Ok(_) => {
                present.insert(name.to_string());
                // `None` on Unix and on any reparse point.
                let cached = crate::platform::entry_cached_metadata(|| entry.metadata().ok());
                files.push(PendingFile { path, cached });
            }
            // Type unknown: mark it present so an existing row survives.
            Err(_) => {
                present.insert(name.to_string());
            }
        }
    }

    if !unreadable_entry {
        stale.extend(
            rows.keys()
                .filter(|name| !present.contains(name.as_str()))
                .map(|name| path_to_db_string(&dir.join(name))),
        );
    }

    // Spread a wide directory across the pool, keeping the tail for ourselves
    // so the entries the read just warmed are handled now.
    while files.len() > FILES_PER_JOB {
        let chunk = files.split_off(files.len() - FILES_PER_JOB);
        found.push(Found::Files(chunk, rows.clone()));
    }
    files
}

/// How a file's stored mtime is to be found.
enum Known<'a> {
    InDir(&'a DirRows),
    /// Resolved by exact path: a symlink target's row lives under a
    /// different parent.
    Exact(Option<u64>),
}

/// 128-bit truncated SHA-256 of a path, for the writer's duplicate-visit set.
/// 16 bytes is ~4e-26 collision probability at 7M paths (8 would be ~1e-6),
/// and a collision silently drops a real file. Cryptographic because shared
/// filenames are attacker-supplied: a chosen pair could hide one file.
pub fn path_digest(path: &str) -> u128 {
    let digest = Sha256::digest(path.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(bytes)
}

/// At most one `stat`, then classify; only files that will be written get
/// opened, and small text files are finished outright. "At most": on Windows
/// [`PendingFile::cached`] may already hold the answer.
fn prepare(file: PendingFile, known: Known<'_>, ctx: &Ctx) -> WalkedFile {
    let PendingFile { path, cached } = file;
    // Every route here has already screened the path for UTF-8:
    // `path_to_db_string` is lossy, and a lossy string would key another
    // file's row and could consume its digest.
    debug_assert!(
        path.to_str().is_some(),
        "an unrepresentable path reached prepare(): {:?}",
        path
    );
    let db_path = path_to_db_string(&path);
    let digest = path_digest(&db_path);
    let aliased = matches!(known, Known::Exact(_));

    let Ok(meta) = crate::platform::metadata_or_stat(&path, cached) else {
        // Seen but unreadable: a transient stat failure must not read as
        // "deleted".
        return WalkedFile::skipped(db_path, digest, aliased);
    };
    let Some(mtime) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
    else {
        return WalkedFile::skipped(db_path, digest, aliased);
    };

    let action = match known {
        Known::InDir(rows) => {
            // `to_str`, not lossy: the lossy spelling of one file is a valid
            // name for another. The screen makes it always `Some`.
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            classify_for_indexing(name, mtime, rows)
        }
        Known::Exact(stored) => classify_by_mtime(stored, mtime),
    };
    let record = match action {
        // Unchanged: never opened, never hashed — must stay at one syscall.
        FileIndexAction::Skip => None,
        // `prepare_file_record` gates on `is_file()`, which keeps us from
        // opening a FIFO — an uninterruptible forever-block.
        _ => prepare_file_record(&db_path, &meta, &ctx.config, &ctx.registry),
    };

    WalkedFile {
        path: db_path,
        action,
        record,
        digest,
        aliased,
    }
}

fn worker(shared: &Shared, ctx: &Ctx, tx: &mpsc::SyncSender<WalkEvent>) {
    while let Some((job, slot)) = shared.take() {
        let _busy = shared.stats.enter();
        if ctx.stop_flag.load(Ordering::Relaxed) {
            shared.shutdown();
            return;
        }

        let mut found = Vec::new();
        let mut stale = Vec::new();
        let (files, rows) = match job {
            Job::Dir(dir, rows) => {
                let files = read_directory(&dir, &rows, ctx, &mut found, &mut stale);
                (files, rows)
            }
            Job::Files(files, rows) => (files, rows),
            Job::Alias(path, stored) => {
                slot.finish(found);
                let file = PendingFile::uncached(path);
                if tx
                    .send(WalkEvent::File(prepare(file, Known::Exact(stored), ctx)))
                    .is_err()
                {
                    shared.shutdown();
                    return;
                }
                continue;
            }
        };

        // Hand the subdirectories over before doing our own per-file work, so
        // the rest of the pool never idles waiting behind one worker.
        slot.finish(found);

        if !stale.is_empty() && tx.send(WalkEvent::Stale(stale)).is_err() {
            shared.shutdown();
            return;
        }

        for file in files {
            if ctx.stop_flag.load(Ordering::Relaxed) {
                shared.shutdown();
                return;
            }
            if tx
                .send(WalkEvent::File(prepare(file, Known::InDir(&rows), ctx)))
                .is_err()
            {
                // Receiver gone: the run was stopped or failed. Not an error.
                shared.shutdown();
                return;
            }
        }
    }
}

/// Serves the pool's directory-row and symlink-mtime lookups from one
/// read-only connection. A failed query abandons the job: the directory goes
/// unwalked, which reconciliation reads as "not seen" and deletes nothing.
fn prefetcher(shared: &Shared, db_path: &str) {
    let conn = match crate::db::open::open_walk_reader(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            // Without rows, every file looks new and every row stale.
            crate::log_warn!("walk reader: {}", e);
            shared.shutdown();
            return;
        }
    };

    while let Some(work) = shared.take_prefetch() {
        match work {
            PrefetchWork::Dir(dir) => {
                match crate::db::repo::dir_rows(&conn, &dir_to_db_parent(&dir)) {
                    Ok(rows) => shared.finish_prefetch(Job::Dir(dir, Arc::new(rows))),
                    Err(e) => {
                        crate::log_warn!("{}", e);
                        shared.abandon_prefetch();
                    }
                }
            }
            PrefetchWork::Alias(path) => {
                match crate::db::repo::mtime_for_path(&conn, &path_to_db_string(&path)) {
                    Ok(stored) => shared.finish_prefetch(Job::Alias(path, stored)),
                    Err(e) => {
                        crate::log_warn!("{}", e);
                        shared.abandon_prefetch();
                    }
                }
            }
        }
    }
}

/// A running parallel walk. Iterating it drains finished files; dropping it
/// stops the workers and joins them.
pub struct ParallelWalk {
    rx: Option<mpsc::Receiver<WalkEvent>>,
    pending: Option<WalkEvent>,
    handles: Vec<JoinHandle<()>>,
    prefetch: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    ctx: Arc<Ctx>,
}

impl ParallelWalk {
    /// Directories that could not be read. Final once the iterator has ended.
    pub fn unreadable(&self) -> &UnreadableDirs {
        &self.ctx.unreadable
    }

    pub fn pruned(&self) -> &PruneCounts {
        &self.ctx.pruned
    }

    /// Every canonical directory the walk queued, in `files.parent` spelling.
    /// The vanished-directory sweep needs this: a directory deleted wholesale
    /// is never read, so nothing reconciles the rows beneath it.
    pub fn seen_dirs(&self) -> HashSet<String> {
        crate::lock_ok(&self.shared.queue)
            .seen_dirs
            .iter()
            .map(|d| dir_to_db_parent(d))
            .collect()
    }

    /// Cloneable worker-activity handle; permanently zero once workers exit.
    pub fn worker_stats(&self) -> WorkerStats {
        self.shared.stats.clone()
    }

    /// Join the workers and report whether every one finished cleanly. A dead
    /// worker and a finished one look identical from the receiving end, and
    /// treating a panicked walk as complete would hand stale cleanup a
    /// partial file set.
    pub fn finish(&mut self) -> bool {
        // Dropping the receiver first releases any worker parked in `send`.
        self.rx = None;
        let mut clean = true;
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                clean = false;
            }
        }
        if let Some(handle) = self.prefetch.take() {
            // `shutdown` releases a prefetcher parked behind PREFETCH_AHEAD;
            // without it this join would block until the queue emptied.
            self.shared.shutdown();
            if handle.join().is_err() {
                clean = false;
            }
        }
        clean
    }
}

/// Result of a non-blocking pull from a producer pool.
pub enum TryNext<T> {
    Item(T),
    Empty,
    /// All workers exited, for any reason.
    Finished,
}

/// `None` is a receiver the owner already dropped: reads as finished.
pub(crate) fn try_recv_next<T>(rx: Option<&mpsc::Receiver<T>>) -> TryNext<T> {
    match rx {
        None => TryNext::Finished,
        Some(rx) => match rx.try_recv() {
            Ok(item) => TryNext::Item(item),
            Err(mpsc::TryRecvError::Empty) => TryNext::Empty,
            Err(mpsc::TryRecvError::Disconnected) => TryNext::Finished,
        },
    }
}

/// [`try_recv_next`] with a wait. Not a sleep backoff: on Windows the default
/// timer resolution is 15.6 ms, so a 2 ms sleep stalls for 15.6;
/// `recv_timeout` parks on the channel's condvar and wakes immediately.
pub(crate) fn recv_next_timeout<T>(
    rx: Option<&mpsc::Receiver<T>>,
    timeout: std::time::Duration,
) -> TryNext<T> {
    match rx {
        None => TryNext::Finished,
        Some(rx) => match rx.recv_timeout(timeout) {
            Ok(item) => TryNext::Item(item),
            Err(mpsc::RecvTimeoutError::Timeout) => TryNext::Empty,
            Err(mpsc::RecvTimeoutError::Disconnected) => TryNext::Finished,
        },
    }
}

impl ParallelWalk {
    /// Non-blocking variant of `next`, for callers multiplexing several walks.
    pub fn try_next(&mut self) -> TryNext<WalkEvent> {
        if let Some(event) = self.pending.take() {
            return TryNext::Item(event);
        }
        try_recv_next(self.rx.as_ref())
    }

    /// Wait up to `timeout` for output, holding it for the next `try_next`.
    pub fn wait_ready(&mut self, timeout: std::time::Duration) -> bool {
        if self.pending.is_some() {
            return true;
        }
        match recv_next_timeout(self.rx.as_ref(), timeout) {
            TryNext::Item(event) => {
                self.pending = Some(event);
                true
            }
            TryNext::Finished => true,
            TryNext::Empty => false,
        }
    }
}

impl Iterator for ParallelWalk {
    type Item = WalkEvent;

    fn next(&mut self) -> Option<WalkEvent> {
        if let Some(event) = self.pending.take() {
            return Some(event);
        }
        self.rx.as_ref()?.recv().ok()
    }
}

impl Drop for ParallelWalk {
    fn drop(&mut self) {
        self.shared.shutdown();
        self.finish();
    }
}

/// Walk `roots` in parallel, yielding every indexable file exactly once per
/// canonical path. `workers` (clamped to 1..=64) is explicit for per-root
/// overrides; `db_path` is opened read-only by the row prefetcher.
#[allow(clippy::too_many_arguments)]
pub fn walk_indexable_files(
    roots: &[String],
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: IgnoreSet,
    db_path: &str,
    config: Config,
    registry: Arc<Registry>,
    stop_flag: Arc<AtomicBool>,
    workers: usize,
) -> ParallelWalk {
    let mut queue = Queue::default();
    let mut unresolvable: Vec<PathBuf> = Vec::new();
    for root in roots {
        // A non-canonical root makes every file look new and every stored
        // row look stale. Roots themselves are never filtered — the user
        // chose them.
        match fs::canonicalize(root) {
            // A root string is UTF-8 (from the config), but what it resolves
            // to need not be; stored lossily it would be walked under a
            // parent naming some other directory. Treated as unresolvable.
            Ok(dir) if dir.to_str().is_none() => {
                crate::log_warn!(
                    "cannot index root {}: it resolves to {:?}, whose name is not valid UTF-8",
                    root,
                    dir
                );
                unresolvable.push(PathBuf::from(root));
            }
            Ok(dir) => {
                let dir = PathBuf::from(path_to_db_string(&dir));
                if queue.seen_dirs.insert(dir.clone()) {
                    queue.needs_rows.push(dir);
                }
            }
            Err(e) => {
                crate::log_warn!("cannot resolve indexing root {}: {}", root, e);
                // An unmounted root is indistinguishable from "everything
                // was deleted"; recorded so stale cleanup leaves it alone.
                unresolvable.push(PathBuf::from(root));
            }
        }
    }

    let threads = workers.clamp(1, 64);
    let shared = Arc::new(Shared {
        queue: Mutex::new(queue),
        idle: Condvar::new(),
        stats: WorkerStats::new(threads),
    });
    let ctx = Arc::new(Ctx {
        follow_symlinks,
        include_hidden,
        ignore,
        index_files: crate::file_handling::index_file_set(Path::new(db_path)),
        pruned: PruneCounts::default(),
        config,
        registry,
        unreadable: UnreadableDirs::default(),
        stop_flag,
    });

    for root in unresolvable {
        ctx.unreadable.record(root);
    }

    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
    let handles = (0..threads)
        .map(|_| {
            let (shared, ctx, tx) = (shared.clone(), ctx.clone(), tx.clone());
            crate::platform::spawn_worker("qs-walk", move || {
                crate::platform::set_background_priority();
                worker(&shared, &ctx, &tx)
            })
        })
        .collect();
    // The workers must hold the only senders, or `recv` never reports the end
    // of the walk.
    drop(tx);

    let prefetch = {
        let (shared, db_path) = (shared.clone(), db_path.to_string());
        crate::platform::spawn_worker("qs-prefetch", move || {
            crate::platform::set_background_priority();
            prefetcher(&shared, &db_path)
        })
    };

    ParallelWalk {
        rx: Some(rx),
        pending: None,
        handles,
        prefetch: Some(prefetch),
        shared,
        ctx,
    }
}

/// A network share wants far more threads than cores — each worker is mostly
/// blocked on a round trip — and with mixed roots the higher count wins.
pub fn thread_count_for(roots: &[String]) -> usize {
    let network = roots
        .iter()
        .any(|r| crate::platform::is_network_path(Path::new(r)));
    if network {
        NETWORK_THREADS
    } else {
        LOCAL_THREADS
    }
}
