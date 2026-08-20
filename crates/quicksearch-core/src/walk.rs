//! Parallel filesystem walk for the full indexing run.
//!
//! One shared queue of directories, N worker threads. A worker reads a
//! directory **and** does that directory's per-file work — stat, classify,
//! hash — before moving on. On SMB, one `QUERY_DIRECTORY` returns size, mtime
//! and attributes for every entry, and the cifs client primes its inode cache
//! from the reply — but only for `actimeo`, one second by default. A `stat`
//! right after the directory read is therefore free; the same `stat` a few
//! seconds later is a full network round trip.
//!
//! Every path below a root is canonical by construction: roots are
//! canonicalized once at seed time and directories are only ever reached by
//! joining names onto them. Symlinks are the exception and are resolved where
//! they are found.

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

/// Bounded hand-off to the DB writer.
const CHANNEL_CAP: usize = 4096;

/// Worker threads for a root on local storage.
const LOCAL_THREADS: usize = 4;

/// Worker threads for a root on a network filesystem, where every uncached
/// metadata operation is a round trip.
const NETWORK_THREADS: usize = 16;

/// One file the walk found, with everything the DB writer needs.
#[derive(Debug)]
pub struct WalkedFile {
    /// Canonical path. The row it keys is `(parent, name)`; see
    /// [`crate::file_handling::split_db_path`].
    pub path: String,
    pub action: FileIndexAction,
    /// `None` when there is nothing to write: unchanged, or the record could
    /// not be built.
    pub record: Option<OwnedNewFile>,
    /// 128-bit truncated SHA-256 of [`WalkedFile::path`], for the writer's
    /// duplicate-visit set.
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

/// What the walk emits. Files as they are classified, plus the per-directory
/// verdict on which index rows no longer have a file behind them.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WalkEvent {
    File(WalkedFile),
    /// Paths whose row should be deleted: present in one directory's index
    /// rows, absent from that directory's listing. Emitted once per directory
    /// read, and only for directories that were read successfully.
    Stale(Vec<String>),
}

/// One file a directory read produced: its path, plus whatever that read
/// already told us about it.
///
/// `cached` is `Some` only on Windows, where `FindNextFileW` returns size,
/// mtime and attributes alongside the name; Unix `getdents64` returns only
/// `d_type`.
struct PendingFile {
    path: PathBuf,
    cached: Option<crate::platform::CachedMetadata>,
}

impl PendingFile {
    /// A path that did not come from a directory entry — a resolved symlink
    /// target — so there is nothing cached to carry.
    fn uncached(path: PathBuf) -> Self {
        PendingFile { path, cached: None }
    }
}

/// Work waiting for a thread.
enum Job {
    /// Read this directory and process its files. Carries the directory's
    /// index rows, fetched by the prefetcher before the job became runnable.
    Dir(PathBuf, Arc<DirRows>),
    /// A slice of one directory's files, sharing that directory's rows.
    Files(Vec<PendingFile>, Arc<DirRows>),
    /// A resolved symlink target, with the stored mtime for its own path.
    Alias(PathBuf, Option<u64>),
}

/// How many entries each filter rejected, for the one-line summary a run logs
/// when it finishes.
///
/// A pruned *directory* is one increment, not one per file beneath it: the
/// subtree is never enumerated.
#[derive(Debug, Default)]
pub struct PruneCounts {
    /// Names beginning with a dot.
    pub dot_named: AtomicU64,
    /// Windows entries carrying `FILE_ATTRIBUTE_HIDDEN`.
    pub attribute: AtomicU64,
    /// Rejected by a configured ignore pattern, of either kind.
    pub ignored: AtomicU64,
}

impl PruneCounts {
    pub fn total(&self) -> u64 {
        self.dot_named.load(Ordering::Relaxed)
            + self.attribute.load(Ordering::Relaxed)
            + self.ignored.load(Ordering::Relaxed)
    }

    /// The summary line, or `None` when nothing was pruned.
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
    /// The index's own files, which this walk must never so much as open.
    /// See [`crate::file_handling::index_file_set`] for why opening one is
    /// fatal rather than merely wasteful.
    ///
    /// Precomputed rather than derived per entry: it is one canonicalize, and
    /// the alternative is a syscall against every file in the tree.
    index_files: HashSet<PathBuf>,
    pruned: PruneCounts,
    config: Config,
    registry: Arc<Registry>,
    unreadable: UnreadableDirs,
    stop_flag: Arc<AtomicBool>,
}

/// Individual unreadable-directory warnings allowed per run before only the
/// count is kept. Reset by [`reset_run_warnings`].
static UNREADABLE_WARNINGS: crate::log::Throttle = crate::log::Throttle::new(20);

/// The same, for names that cannot round-trip through the index. Throttled
/// because a share can hold thousands of them: one legacy-encoded directory on
/// a Samba mount, or a `\\wsl.localhost\` tree, and every entry under it is a
/// separate occurrence.
static UNREPRESENTABLE_WARNINGS: crate::log::Throttle = crate::log::Throttle::new(20);

/// Arm this module's per-run warning throttles.
pub fn reset_run_warnings() {
    UNREADABLE_WARNINGS.reset();
    UNREPRESENTABLE_WARNINGS.reset();
}

/// Read one directory, apply the hidden/ignore rules, and split the result:
/// subdirectories and overflow file chunks go to `found` for the pool, the
/// remaining files come back for this worker to handle immediately.
///
/// Also reconciles the directory against its index rows: `stale` receives the
/// paths whose row has no file behind it any more.
///
/// A directory that cannot be read returns before reconciling, so nothing
/// under it is ever deleted — an unreadable directory must not read as an
/// empty one.
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
            // Not the same as "this directory is empty": see UnreadableDirs.
            if UNREADABLE_WARNINGS.allow() {
                crate::log_warn!("cannot read {}: {}", dir.display(), e);
            }
            // "Gone" is not "could not look", and only the second one is a
            // reason to distrust the walk. A directory deleted while the walk
            // was in flight — a build tree, a browser cache — is a fact about
            // the filesystem: its rows *should* fall to the stale sweep, and
            // recording it here would both spare them and cost this root its
            // stored walk count, which is what makes every later run pay for
            // a second `find | wc` traversal of the whole tree.
            //
            // The same distinction the coordinator draws in `verb_for`:
            // `NotFound` is unambiguous, every other errno is not.
            if e.kind() != std::io::ErrorKind::NotFound {
                ctx.unreadable.record(dir.to_path_buf());
            }
            return Vec::new();
        }
    };
    // Names surviving the filters, for the stale diff below. Every `continue`
    // in the loop must be a genuine "not indexable", or the diff deletes live
    // rows.
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
        // **The screen for names the index cannot spell, and the only one.**
        //
        // `files.name` and `files.parent` are TEXT, so a name that is not valid
        // UTF-8 has no representation there; `to_string_lossy` would give one,
        // but it is many-to-one, and every use of a path in this walk is a
        // database *key*. A lossy name collides with the real name of a
        // different file — U+FFFD is an ordinary filename character — and the
        // collision is not a cosmetic one: the lossy parent makes the
        // prefetcher hand this directory another directory's rows, and the diff
        // below then reports all of them stale. Screening here, before any
        // string is built, is what keeps that from being possible at all.
        //
        // On Unix this is any invalid byte sequence; on Windows it is an
        // unpaired UTF-16 surrogate, which NTFS stores happily and WSL's DrvFs
        // emits by design for non-UTF-8 Linux names.
        //
        // A directory is pruned whole and at its root: the join below would
        // carry the bad component into every path beneath it, so nothing under
        // it could be indexed either way, and stopping here costs one warning
        // instead of one per descendant.
        //
        // Leaving the entry out of `present` is safe, despite the rule above
        // that every `continue` must be a genuine "not indexable". No stored
        // row can carry a name that is not valid UTF-8, so this entry has no
        // row to protect; and a row whose name happens to *equal* the lossy
        // spelling belongs to some other, representable entry, which this same
        // listing yields separately and which marks itself present.
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
        // The closure runs only on Windows, where `entry.metadata()` is free —
        // the attributes came back with the directory read, and it reports the
        // entry itself rather than a link target.
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
                        // Announced because a plainly visible folder skipped
                        // over an attribute Explorer does not show has no
                        // other way of being discovered.
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
        // The index's own database and sidecars. Not a user preference and
        // not overridable, because hashing one is not a slow row, it is a
        // process-wide cancellation of SQLite's locks on a file we are in the
        // middle of writing — see `file_handling::index_file_set`.
        //
        // `continue` rather than a `WalkedFile::skipped`, deliberately: this
        // leaves the name out of `present`, so any row an earlier run wrote
        // for the index — before this pruning existed, or from a spell when
        // `database_path` pointed elsewhere — falls to the stale sweep and is
        // deleted. `skipped` would keep it forever.
        if ctx.index_files.contains(&path) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // `file_type` is the cached `d_type` from the directory read.
        match entry.file_type() {
            // Directories hold no `files` row and are not marked present: a
            // name that was a file last run and is a directory now *should*
            // lose its row.
            Ok(ft) if ft.is_dir() => found.push(Found::Dir(path)),
            Ok(ft) if ft.is_symlink() => {
                // Directory and file targets must be gated together, or the
                // two walkers disagree: `filtered_walk` follows neither kind,
                // so a file target followed only here would be indexed by
                // every full run and never updated between them.
                if !ctx.follow_symlinks {
                    continue;
                }
                // The index stores the target's canonical path, and pushing
                // only canonical directories is what keeps `seen_dirs` able
                // to break cycles. Normalized like the roots, or on Windows
                // the target keeps `canonicalize`'s `\\?\` prefix, under
                // which full-path ignore patterns would never match and
                // `seen_dirs` could not dedup against an overlapping root.
                if let Ok(target) = path.canonicalize() {
                    // The link's own name passed the screen above; the target
                    // is a different path and gets its own. Without this, a
                    // link to an unrepresentable path would be resolved to its
                    // *lossy* spelling — a path naming some other file
                    // entirely, which would then be walked or indexed in its
                    // place.
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
                    // Again on the resolved target: the check above tested the
                    // link's own name, and a symlink pointing at the index
                    // would otherwise walk straight past it into an `open`.
                    // Harmless for a directory target — the set holds only
                    // files — which is why one check covers both arms.
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
                // `None` on Unix and on any reparse point: see
                // `entry_cached_metadata`.
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
        // Rebuild each stored path the way `prepare` does, by joining onto
        // the canonical directory, so separators and roots match the
        // stored spelling exactly.
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
    /// By name within the directory being walked — the ordinary case.
    InDir(&'a DirRows),
    /// Already resolved by exact path, for a symlink target whose row lives
    /// under a different parent.
    Exact(Option<u64>),
}

/// 128-bit truncated SHA-256 of a path, for the writer's duplicate-visit set.
///
/// Truncated rather than full: 16 bytes is ~4e-26 collision probability at
/// 7M paths, where 8 bytes would be ~1e-6 — and a collision here silently
/// drops a real file from the index. Cryptographic rather than fast because
/// filenames on a shared volume are attacker-supplied, so a cheap hash would
/// let a chosen pair hide one of the two files.
pub fn path_digest(path: &str) -> u128 {
    let digest = Sha256::digest(path.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(bytes)
}

/// At most one `stat`, then classify; only files that are actually going to be
/// written get opened, and small text files are finished outright.
///
/// "At most": on Windows [`PendingFile::cached`] may already hold the answer —
/// see [`crate::platform::metadata_or_stat`].
fn prepare(file: PendingFile, known: Known<'_>, ctx: &Ctx) -> WalkedFile {
    let PendingFile { path, cached } = file;
    // Every route here has already screened the path: a root is screened after
    // canonicalizing (which can resolve onto a name the config string was
    // not), `Job::Files` comes from a listing `read_directory` filtered, and
    // `Job::Alias` from a target the symlink arm checked. That
    // matters because `path_to_db_string` is lossy, and the string below is
    // used as a database key *and* hashed into the run's duplicate-visit set —
    // a lossy one would key another file's row and could consume its digest,
    // silently dropping it from the index.
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
            // `to_str`, not `to_string_lossy`: this name is looked up in the
            // directory's stored rows, and the lossy spelling of one file is a
            // valid name for another. The screen in `read_directory` is what
            // makes it always `Some`.
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            classify_for_indexing(name, mtime, rows)
        }
        Known::Exact(stored) => classify_by_mtime(stored, mtime),
    };
    let record = match action {
        // Unchanged: never opened, never hashed — the case that must stay at
        // one syscall.
        FileIndexAction::Skip => None,
        // `prepare_file_record` gates on `is_file()`, which keeps us from
        // opening a FIFO — that would block forever, uninterruptibly.
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
/// read-only connection.
///
/// A failed query is not fatal: the job is abandoned rather than retried, and
/// the directory it was for simply goes unwalked, which reconciliation reads
/// as "not seen" and therefore deletes nothing.
fn prefetcher(shared: &Shared, db_path: &str) {
    let conn = match crate::db::open::open_walk_reader(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            // Without rows, continuing would treat every file as new and
            // every row as stale.
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
    /// One event pulled off the channel by [`ParallelWalk::wait_ready`] and not
    /// yet handed to [`ParallelWalk::try_next`].
    pending: Option<WalkEvent>,
    handles: Vec<JoinHandle<()>>,
    prefetch: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    ctx: Arc<Ctx>,
}

impl ParallelWalk {
    /// Directories that could not be read. Only final once the iterator has
    /// ended.
    pub fn unreadable(&self) -> &UnreadableDirs {
        &self.ctx.unreadable
    }

    /// How many entries each filter rejected. Final on the same terms as
    /// [`ParallelWalk::unreadable`].
    pub fn pruned(&self) -> &PruneCounts {
        &self.ctx.pruned
    }

    /// Every canonical directory the walk queued, in `files.parent` spelling.
    ///
    /// The vanished-directory sweep needs this: a directory deleted wholesale
    /// is never read, so nothing reconciles the rows beneath it.
    ///
    /// Only meaningful once the walk has finished.
    pub fn seen_dirs(&self) -> HashSet<String> {
        crate::lock_ok(&self.shared.queue)
            .seen_dirs
            .iter()
            .map(|d| dir_to_db_parent(d))
            .collect()
    }

    /// A cheap, cloneable handle for reading worker activity.
    ///
    /// Meaningful only while the walk is running: once the workers exit, the
    /// busy count is permanently zero.
    pub fn worker_stats(&self) -> WorkerStats {
        self.shared.stats.clone()
    }

    /// Join the workers and report whether every one of them finished
    /// cleanly.
    ///
    /// A dead worker and a finished worker look identical from the receiving
    /// end — both close the channel — and treating a panicked walk as complete
    /// would hand stale cleanup a partial file set. Join before deciding
    /// anything about what the walk saw.
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
    /// Nothing ready right now; the pass is still running.
    Empty,
    /// The pass has ended (all workers exited, for any reason).
    Finished,
}

/// Translate a non-blocking channel pull into [`TryNext`]. `None` is a
/// receiver the owner already dropped, which reads as finished.
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

/// [`try_recv_next`], but willing to wait up to `timeout` for something to
/// arrive.
///
/// Used instead of a sleep backoff: on Windows the default timer resolution
/// is 15.6 ms, so a 2 ms sleep actually stalls for 15.6. `recv_timeout` parks
/// on the channel's own condition variable, so a sender wakes it immediately.
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
    /// Non-blocking variant of `next`, for callers multiplexing several
    /// walks (the per-root writer loop).
    pub fn try_next(&mut self) -> TryNext<WalkEvent> {
        if let Some(event) = self.pending.take() {
            return TryNext::Item(event);
        }
        try_recv_next(self.rx.as_ref())
    }

    /// Wait up to `timeout` for this walk to produce something, holding
    /// whatever arrives for the next [`ParallelWalk::try_next`].
    ///
    /// Returns whether anything is now ready.
    pub fn wait_ready(&mut self, timeout: std::time::Duration) -> bool {
        if self.pending.is_some() {
            return true;
        }
        match recv_next_timeout(self.rx.as_ref(), timeout) {
            TryNext::Item(event) => {
                self.pending = Some(event);
                true
            }
            // A finished walk is "ready": there is something to do (notice
            // it ended).
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
        // No-op if the caller already called `finish`.
        self.finish();
    }
}

/// Walk `roots` in parallel, yielding every indexable file exactly once per
/// canonical path.
///
/// `workers` is explicit so callers can honour per-root overrides; use
/// [`thread_count_for`] for the storage-appropriate default. Clamped to
/// 1..=64.
/// `db_path` is opened read-only by this walk's row prefetcher; the walk
/// itself never writes.
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
        // Canonicalize here so "everything below a root is already canonical"
        // holds however this is called: a non-canonical root would make every
        // file look new and every stored row look stale.
        //
        // Roots themselves are never filtered — the user chose them.
        match fs::canonicalize(root) {
            // A root string is UTF-8 by construction — it came from the config
            // — but `canonicalize` resolves symlinks, so what it resolves *to*
            // need not be: `~/docs` can be a link to a directory whose real
            // name the index cannot spell. Stored lossily, the root would be
            // walked under a parent string that names some other directory
            // entirely. Treated exactly like a root that would not resolve at
            // all, which is what it amounts to: yields nothing, and is recorded
            // so stale cleanup does not read that as "everything was deleted".
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
                // An unmounted root yields nothing, indistinguishable from
                // "all its files were deleted"; recorded so stale cleanup
                // leaves it alone.
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

/// Pick a worker count for these roots.
///
/// A network share wants far more threads than cores — each worker spends its
/// time blocked on a round trip — and with a mix of roots the higher count
/// wins.
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
