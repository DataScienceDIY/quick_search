//! Parallel filesystem walk for the full indexing run.
//!
//! One shared queue of directories, N worker threads. The important part is
//! *what* a worker keeps together: it reads a directory **and** does that
//! directory's per-file work — stat, classify, hash — before moving on.
//!
//! That grouping is the whole design. On SMB, one `QUERY_DIRECTORY` returns
//! size, mtime and attributes for every entry in a directory, and the cifs
//! client primes its inode cache from the reply — but only for `actimeo`,
//! one second by default, and end-user mount options are not ours to set. A
//! `stat` issued right after the directory read is therefore free, while the
//! same `stat` a few seconds later is a full network round trip. Parallelising
//! the directory reads alone — what a general-purpose parallel walker does —
//! hands entries to a consumer that stats them well outside that window, so it
//! throws the cache away and lands *slower* than a serial walk while burning
//! more CPU.
//!
//! The second property this buys: every path below a root is canonical by
//! construction. Roots are canonicalized once at seed time and directories are
//! only ever reached by joining names onto them, so per-file `canonicalize`
//! calls — roughly one `readlink` per path component, per file — disappear.
//! Symlinks are the one exception and are resolved where they are found.
//!
//! Deliberately std-only (`std::thread` + `std::sync::mpsc`), matching the
//! house style set out in [`crate::watcher`].

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
    classify_by_mtime, classify_for_indexing, path_to_db_string, prepare_file_record,
    warn_if_unrepresentable, DirRows, FileIndexAction, OwnedNewFile, UnreadableDirs,
};
use crate::indexing::should_abort;

/// Files one worker takes for itself before handing the rest to the pool.
///
/// Without this split a single very wide directory — a Photos or Downloads
/// folder, a scanned-document share — would be walked by exactly one thread.
const FILES_PER_JOB: usize = 128;

/// Bounded hand-off to the DB writer. Deep enough that workers don't stall
/// while a transaction commits, shallow enough to bound memory.
const CHANNEL_CAP: usize = 4096;

/// Worker threads for a root on local storage. The work is latency-bound
/// rather than CPU-bound, but a local disk needs little queueing and deep
/// parallelism just adds seeks.
const LOCAL_THREADS: usize = 4;

/// Worker threads for a root on a network filesystem. Every uncached
/// metadata operation is a round trip, so throughput is round-trips-in-flight
/// divided by latency; threads are how we raise the numerator.
const NETWORK_THREADS: usize = 16;

/// One file the walk found, with everything the DB writer needs.
#[derive(Debug)]
pub struct WalkedFile {
    /// Canonical path, and the `files.path` key.
    pub path: String,
    pub action: FileIndexAction,
    /// `None` when there is nothing to write: the file was unchanged, or its
    /// record could not be built. Never a reason to drop [`WalkedFile::path`].
    pub record: Option<OwnedNewFile>,
    /// 128-bit truncated SHA-256 of [`WalkedFile::path`], for the writer's
    /// duplicate-visit set. Computed here so the cost lands on the walk pool
    /// rather than on the single writer thread.
    pub digest: u128,
    /// True when this file was reached by resolving a symlink, so the
    /// directory it was found in is *not* the directory its row belongs to.
    ///
    /// Such a row is invisible to the reconciliation of its real parent —
    /// which may be a directory no walk ever visits — so the caller must
    /// exempt it from the vanished-directory sweep.
    pub aliased: bool,
}

impl WalkedFile {
    /// Seen, but with nothing to write. Distinct from not being emitted at
    /// all: the row stays.
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
// `File` is far larger than the deletion variant, and deliberately so: this
// is the walk's hot path, one event per file on a tree of millions. Boxing to
// even out the variants would add exactly the per-file allocation the
// owned-record design exists to avoid.
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
/// The metadata half is `Some` only on Windows, where `FindNextFileW` returned
/// size, mtime and attributes alongside the name. On Unix `getdents64` returns
/// only `d_type`, so it is always `None`, [`prepare`] does the single `statx`
/// the walk has always done, and `Option<CachedMetadata>` is zero-sized — a
/// queued file costs exactly the `PathBuf` it cost before.
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
    /// Process this slice of one directory's files, split off because the
    /// directory was too wide for one worker to be worth serialising on.
    /// Shares its directory's rows. On Windows each entry also carries the
    /// metadata its directory read returned, so classifying these files costs
    /// no syscall at all.
    Files(Vec<PendingFile>, Arc<DirRows>),
    /// A resolved symlink target, with the stored mtime for its own path.
    /// Classified against that rather than against any directory's rows.
    Alias(PathBuf, Option<u64>),
}

/// Directories fetched but not yet taken by a worker. Each holds its rows
/// live, so without a cap the prefetcher would run ahead of the pool and
/// materialise rows for thousands of directories at once — reintroducing in
/// one structure the memory this design exists to remove.
const PREFETCH_AHEAD: usize = 64;

#[derive(Default)]
struct Queue {
    /// LIFO. A directory's children are processed close in time to the read
    /// that discovered them, which is what the attribute cache rewards, and it
    /// keeps the live frontier depth-first-ish instead of holding an entire
    /// breadth-first level in memory.
    jobs: Vec<Job>,
    /// Directories discovered but not yet given their rows, and symlink
    /// targets awaiting an mtime lookup. The prefetcher drains both.
    needs_rows: Vec<PathBuf>,
    needs_alias: Vec<PathBuf>,
    /// Set while the prefetcher is mid-query, holding work that is in neither
    /// list. Part of the end-of-walk proof: see [`Shared::take`].
    prefetching: bool,
    /// Fetched directories sitting in `jobs`, i.e. the ones actually holding
    /// rows. What [`PREFETCH_AHEAD`] bounds.
    ///
    /// Counted rather than derived from `jobs.len()`: that also holds
    /// `Job::Files` chunks, which carry no rows of their own, and a wide
    /// directory pushes many of them. Bounding on the total would let file
    /// chunks starve directory prefetching and leave the pool waiting on
    /// rows that were never fetched.
    ///
    /// The consequence is that file chunks, not this counter, are what bounds
    /// the walker's memory: nothing throttles them, so one very wide directory
    /// can hold its whole listing in `jobs` at once — and on Windows each of
    /// those entries also carries its directory read's metadata
    /// ([`PendingFile`]), so that is the larger cost of the two.
    dirs_ready: usize,
    /// Workers currently holding a job — that is, workers that may still push
    /// more. The walk is over when this is zero and `jobs` is empty.
    active: usize,
    /// Canonical directories already queued. Collapses overlapping roots and
    /// makes symlink cycles impossible: a cycle must revisit a canonical path,
    /// and every directory pushed here is canonical.
    ///
    /// Also the record of which directories the walk reached, which the
    /// caller's vanished-directory sweep reads once the walk has finished.
    seen_dirs: HashSet<PathBuf>,
    done: bool,
}

impl Queue {
    /// Whether any stage still holds work. The prefetch stage is invisible to
    /// a `jobs`/`active` test, so it has to be named here explicitly.
    fn idle(&self) -> bool {
        self.jobs.is_empty()
            && self.needs_rows.is_empty()
            && self.needs_alias.is_empty()
            && !self.prefetching
            && self.active == 0
    }
}

struct Shared {
    queue: Mutex<Queue>,
    idle: Condvar,
    /// Workers currently processing (not parked waiting for work). Purely
    /// observational, for progress display: two relaxed atomic ops per
    /// *job* (a directory read plus up to [`FILES_PER_JOB`] files), so it
    /// costs nothing the queue mutex didn't already.
    stats: WorkerStats,
}

/// Decrements the busy count however the worker leaves its job — including
/// early returns on stop and panics.
pub(crate) struct BusyGuard<'a>(&'a AtomicUsize);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Lock-free view of a worker pool's activity for progress displays.
///
/// Shared by the walk and by [`crate::content`]: a root runs one pool and then
/// the other, and the progress line has to report whichever is live — a pool
/// whose threads have exited reads as busy 0, which is only the truth while
/// that pool is the one running.
#[derive(Clone)]
pub struct WorkerStats {
    busy: Arc<AtomicUsize>,
    total: usize,
}

impl WorkerStats {
    pub(crate) fn new(total: usize) -> Self {
        WorkerStats {
            busy: Arc::new(AtomicUsize::new(0)),
            total,
        }
    }

    /// Count the calling thread as busy until the returned guard drops.
    pub(crate) fn enter(&self) -> BusyGuard<'_> {
        self.busy.fetch_add(1, Ordering::Relaxed);
        BusyGuard(&self.busy)
    }

    /// Workers doing work right now (the rest are parked).
    pub fn active(&self) -> usize {
        self.busy.load(Ordering::Relaxed).min(self.total)
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

impl Shared {
    /// Claim a job, blocking while other workers are still running.
    ///
    /// Returns `None` only when the queue is empty *and* no worker holds a
    /// job — at that instant nobody is left who could push more, so the walk
    /// is provably finished.
    fn take(&self) -> Option<(Job, ActiveJob<'_>)> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if q.done {
                return None;
            }
            if let Some(job) = q.jobs.pop() {
                q.active += 1;
                if matches!(job, Job::Dir(..)) {
                    q.dirs_ready -= 1;
                }
                // The prefetcher may have been parked behind PREFETCH_AHEAD.
                self.idle.notify_all();
                return Some((
                    job,
                    ActiveJob {
                        shared: self,
                        finished: false,
                    },
                ));
            }
            // Nothing runnable. Only "nobody anywhere holds work" proves the
            // walk is over — a directory sitting in the prefetch stage still
            // becomes a job, and declaring the walk finished with one parked
            // there would hand reconciliation a partial file set.
            if q.idle() {
                q.done = true;
                self.idle.notify_all();
                return None;
            }
            q = self.idle.wait(q).unwrap();
        }
    }

    /// Claim one unit of prefetch work, or `None` once the walk is over.
    ///
    /// Parks while the runnable queue is already `PREFETCH_AHEAD` deep, so
    /// fetched-but-unclaimed rows stay bounded.
    fn take_prefetch(&self) -> Option<PrefetchWork> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if q.done {
                return None;
            }
            // Aliases are never throttled: they carry a single mtime, not a
            // directory's rows, so they cost nothing to hold.
            if let Some(path) = q.needs_alias.pop() {
                q.prefetching = true;
                return Some(PrefetchWork::Alias(path));
            }
            if q.dirs_ready < PREFETCH_AHEAD {
                if let Some(dir) = q.needs_rows.pop() {
                    q.prefetching = true;
                    return Some(PrefetchWork::Dir(dir));
                }
            }
            if q.idle() {
                q.done = true;
                self.idle.notify_all();
                return None;
            }
            q = self.idle.wait(q).unwrap();
        }
    }

    /// Publish a prefetched job and clear the in-flight flag together, under
    /// one lock — the same indivisibility `publish` relies on, for the same
    /// reason.
    fn finish_prefetch(&self, job: Job) {
        let mut q = self.queue.lock().unwrap();
        if matches!(job, Job::Dir(..)) {
            q.dirs_ready += 1;
        }
        q.jobs.push(job);
        q.prefetching = false;
        self.idle.notify_all();
    }

    /// Give the prefetch slot back without producing a job (the query failed).
    fn abandon_prefetch(&self) {
        let mut q = self.queue.lock().unwrap();
        q.prefetching = false;
        self.idle.notify_all();
    }
}

/// One unit of work for the prefetcher.
enum PrefetchWork {
    Dir(PathBuf),
    Alias(PathBuf),
}

/// What a worker discovered while reading a directory.
///
/// Distinct from [`Job`] because most of it is not yet runnable: a newly
/// discovered directory has no rows, and a symlink target has no stored
/// mtime, until the prefetcher supplies them.
enum Found {
    /// A subdirectory. Needs its rows before a worker can classify inside it.
    Dir(PathBuf),
    /// A resolved symlink target. Needs an exact-path mtime lookup.
    Alias(PathBuf),
    /// Overflow files from the directory just read, which already has rows.
    Files(Vec<PendingFile>, Arc<DirRows>),
}

impl Shared {
    /// Push discovered work and give the job slot back, under a single lock
    /// acquisition. Doing both together is what makes the idle test in
    /// [`Shared::take`] an end-of-walk proof rather than a race: a worker
    /// that has popped the last job but not yet published its children must
    /// never look idle.
    fn publish(&self, found: Vec<Found>) {
        let mut q = self.queue.lock().unwrap();
        for item in found {
            match item {
                Found::Dir(dir) => {
                    if q.seen_dirs.insert(dir.clone()) {
                        q.needs_rows.push(dir);
                    }
                }
                Found::Alias(path) => q.needs_alias.push(path),
                Found::Files(files, rows) => q.jobs.push(Job::Files(files, rows)),
            }
        }
        q.active -= 1;
        self.idle.notify_all();
    }

    fn shutdown(&self) {
        let mut q = self.queue.lock().unwrap();
        q.done = true;
        self.idle.notify_all();
    }
}

/// Hands the job slot back even if the worker panics or returns early. A
/// stranded count would leave every other worker waiting on a number that
/// never reaches zero.
struct ActiveJob<'a> {
    shared: &'a Shared,
    finished: bool,
}

impl ActiveJob<'_> {
    fn finish(mut self, found: Vec<Found>) {
        self.shared.publish(found);
        self.finished = true;
    }
}

impl Drop for ActiveJob<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.publish(Vec::new());
        }
    }
}

/// How many entries each filter rejected, for the one-line summary a run logs
/// when it finishes.
///
/// Counted rather than logged per entry, deliberately. The process log is a
/// 5,000-line ring ([`crate::log::CAPACITY`]) that evicts oldest-first with no
/// level protection, so a line per ignored entry would push every warning —
/// including the unreadable-directory ones that distinguish a network blip from
/// a deletion — out of the buffer before the run ended. `log::record` also takes
/// a global mutex and writes to stderr, which would serialize the worker pool on
/// the one path the walker is built to keep syscall-free.
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

    /// The summary line, or `None` when nothing was pruned — a clean tree
    /// should not add noise to the log.
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
    pruned: PruneCounts,
    config: Config,
    /// Lets a worker finish small text files outright: the head it reads to
    /// hash them is already their entire contents, so an extractor that works
    /// from bytes saves the content pass an open/read/close per file.
    registry: Arc<Registry>,
    unreadable: UnreadableDirs,
    stop_flag: Arc<AtomicBool>,
    suspend_flag: Arc<AtomicBool>,
}

/// Individual unreadable-directory warnings allowed per run before only the
/// count is kept. Reset by [`reset_run_warnings`].
static UNREADABLE_WARNINGS: crate::log::Throttle = crate::log::Throttle::new(20);

/// Arm this module's per-run warning throttle. See
/// [`crate::file_handling::reset_run_warnings`], which the same caller invokes.
pub fn reset_run_warnings() {
    UNREADABLE_WARNINGS.reset();
}

/// Read one directory, apply the hidden/ignore rules, and split the result:
/// subdirectories and overflow file chunks go to `found` for the pool, the
/// remaining files come back for this worker to handle immediately.
///
/// Also reconciles the directory against its index rows. This is the right
/// place for it and the only cheap one: the *complete* filtered listing
/// exists here, before the directory is split across workers, so the diff
/// needs no per-directory completion count. `stale` receives the paths whose
/// row has no file behind it any more.
///
/// A directory that cannot be read returns before reconciling, so nothing
/// under it is ever deleted — an unreadable directory must not read as an
/// empty one.
///
/// Each returned file carries whatever this read already told us about it, so
/// that [`prepare`] does not have to ask the filesystem twice. See
/// [`PendingFile`].
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
            // Throttled because a permission-denied subtree produces one of
            // these per directory; `UnreadableDirs` records every one of them
            // regardless, and the run reports the total.
            if UNREADABLE_WARNINGS.allow() {
                crate::log_warn!("cannot read {}: {}", dir.display(), e);
            }
            ctx.unreadable.record(dir.to_path_buf());
            return Vec::new();
        }
    };
    // Names surviving the filters, for the diff below. Only meaningful
    // because every `continue` in the loop is a genuine "not indexable",
    // matching what would have been absent from the old global seen set.
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
                // The listing is now incomplete, so it cannot be used to
                // decide what is missing: an entry we failed to read would
                // look identical to one that was deleted.
                unreadable_entry = true;
                continue;
            }
        };

        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `entry.metadata()` is only consulted on Windows, where it is free —
        // the attributes came back with the directory read, and it reports the
        // entry itself rather than a link target, which is what
        // `entry_hidden_reason` requires. On Unix the closure is never called,
        // so this stays at zero extra syscalls.
        if !ctx.include_hidden {
            if let Some(reason) =
                crate::platform::entry_hidden_reason(&name, || entry.metadata().ok())
            {
                match reason {
                    crate::platform::HiddenReason::DotPrefix => {
                        ctx.pruned.dot_named.fetch_add(1, Ordering::Relaxed);
                    }
                    crate::platform::HiddenReason::Attribute => {
                        ctx.pruned.attribute.fetch_add(1, Ordering::Relaxed);
                        // Only the attribute case is announced, and only for a
                        // directory. A dot prefix explains itself and an ignore
                        // pattern is something the user typed, but a plainly
                        // visible folder skipped over an attribute Explorer does
                        // not show has no other way of being discovered — which
                        // is how a whole cloud-sync tree went missing in silence.
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
        if ctx.ignore.matches_component(&name) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let path = entry.path();
        if ctx.ignore.matches_path_pattern(&path) {
            ctx.pruned.ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // `file_type` is the cached `d_type` from the directory read, so
        // splitting directories from files here is free.
        match entry.file_type() {
            // Directories hold no `files` row, so they are deliberately not
            // marked present: a name that was a file last run and is a
            // directory now *should* lose its row.
            Ok(ft) if ft.is_dir() => found.push(Found::Dir(path)),
            Ok(ft) if ft.is_symlink() => {
                // With links off there is nothing here to index — targets
                // included. Returning before `canonicalize` also drops a
                // readlink chain and a stat per symlink, so honouring the
                // setting costs fewer syscalls than ignoring it, not more.
                //
                // Both kinds have to be gated together, or the two walkers
                // disagree: `filtered_walk` (which the watcher and the
                // incremental path use) passes `follow_links` straight to
                // walkdir and follows neither. A file target followed here but
                // not there is indexed by every full run and never updated
                // between them — and, if it resolves outside every configured
                // root, indexed despite living somewhere the user never asked
                // us to look.
                if !ctx.follow_symlinks {
                    continue;
                }
                // Resolve aliases where they are found. The target's canonical
                // path is what the index stores, and pushing only canonical
                // directories is what keeps `seen_dirs` able to break cycles.
                //
                // Normalized like the roots (`walk_indexable_files`), or on
                // Windows the target keeps `canonicalize`'s `\\?\` prefix:
                // every path below it would be spelled differently from the
                // plainly-spelled roots, so full-path ignore patterns would
                // never match under a followed junction and `seen_dirs` could
                // not dedup against an overlapping root.
                if let Ok(target) = path.canonicalize() {
                    let target = PathBuf::from(path_to_db_string(&target));
                    match fs::metadata(&target) {
                        Ok(m) if m.is_dir() => found.push(Found::Dir(target)),
                        // The row for a resolved target belongs to the
                        // target's own directory, not this one, so it is not
                        // marked present here and cannot be classified
                        // against these rows.
                        Ok(_) => found.push(Found::Alias(target)),
                        Err(_) => {}
                    }
                }
            }
            Ok(_) => {
                present.insert(name.into_owned());
                // Windows already sent this file's size and mtime back with its
                // name, and `DirEntry::metadata` there is a copy of that buffer
                // rather than a syscall — so carrying it means `prepare` never
                // opens the file just to ask the same question again. `None` on
                // Unix, and on any reparse point: see `entry_cached_metadata`.
                let cached = crate::platform::entry_cached_metadata(|| entry.metadata().ok());
                files.push(PendingFile { path, cached });
            }
            // Type unknown: the entry exists but we could not classify it.
            // Mark it present so an existing row survives — seen, not deleted.
            Err(_) => {
                present.insert(name.into_owned());
            }
        }
    }

    if !unreadable_entry {
        for name in rows.keys() {
            if !present.contains(name.as_str()) {
                // Rebuild the stored path the way `prepare` does, by joining
                // onto the canonical directory, so separators and roots match
                // the `files.path` spelling exactly.
                stale.push(path_to_db_string(&dir.join(name)));
            }
        }
    }

    // Spread a wide directory across the pool, keeping the tail for
    // ourselves so the entries the read just warmed are handled now. Each
    // chunk shares this directory's rows: they are the same directory, and
    // classifying them against anything else would read every file as new.
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
/// "At most", because on Windows the directory read already answered the
/// question and [`PendingFile::cached`] carries the answer — see
/// [`crate::platform::metadata_or_stat`].
fn prepare(file: PendingFile, known: Known<'_>, ctx: &Ctx) -> WalkedFile {
    let PendingFile { path, cached } = file;
    let db_path = path_to_db_string(&path);
    let digest = path_digest(&db_path);
    let aliased = matches!(known, Known::Exact(_));

    // A name that is not valid UTF-8 cannot be stored in `files.path` and read
    // back as the same file, so there is nothing to hash or text-index. `Skip`
    // rather than an early return with no entry: the caller reads a missing
    // path as "deleted", and this file was seen, not removed.
    if warn_if_unrepresentable(&path) {
        return WalkedFile::skipped(db_path, digest, aliased);
    }

    // On Windows this is the directory read's own copy of the entry: an
    // unchanged file now costs no syscall at all, and a changed one costs only
    // the open the hasher was going to do anyway. On Unix, and for every
    // reparse point, it is the same single `stat` as before.
    let Ok(meta) = crate::platform::metadata_or_stat(&path, cached) else {
        // Seen but unreadable. Emitting it anyway keeps its index row alive:
        // a transient stat failure must not read as "deleted".
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
            // The name is what these rows are keyed by; it is the last
            // component of the same path `db_path` was built from.
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            classify_for_indexing(&name, mtime, rows)
        }
        Known::Exact(stored) => classify_by_mtime(stored, mtime),
    };
    let record = match action {
        // Unchanged: never opened, never hashed. This is nearly every file on
        // a re-index, and it is the case that has to stay at one syscall.
        FileIndexAction::Skip => None,
        // `prepare_file_record` gates on `is_file()`, which is what keeps us
        // from opening a FIFO — that would block forever, uninterruptibly.
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
        if should_abort(&ctx.stop_flag, &ctx.suspend_flag) {
            shared.shutdown();
            return;
        }

        let mut found = Vec::new();
        let mut stale = Vec::new();
        // An alias is a single file with its mtime already resolved; the
        // other two variants are a directory's worth classified against
        // that directory's rows.
        let (files, rows) = match job {
            Job::Dir(dir, rows) => {
                let files = read_directory(&dir, &rows, ctx, &mut found, &mut stale);
                (files, rows)
            }
            Job::Files(files, rows) => (files, rows),
            Job::Alias(path, stored) => {
                slot.finish(found);
                // Reached through `canonicalize`, not through a directory
                // entry, so there is nothing cached to carry.
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
        // the rest of the pool never idles waiting behind one worker. This
        // also confines the job slot to `read_directory`.
        slot.finish(found);

        if !stale.is_empty() && tx.send(WalkEvent::Stale(stale)).is_err() {
            shared.shutdown();
            return;
        }

        for file in files {
            if should_abort(&ctx.stop_flag, &ctx.suspend_flag) {
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
/// One per walk. The alternative — a connection per worker — would multiply
/// SQLite's page cache by the pool size; see
/// [`crate::db::schema::PRAGMAS_WALK_READER`]. Every
/// query here is a single index lookup, so one thread stays far ahead of a
/// pool bound by `stat` latency.
///
/// A failed query is not fatal: the job is abandoned rather than retried, and
/// the directory it was for simply goes unwalked, which reconciliation reads
/// as "not seen" and therefore deletes nothing.
fn prefetcher(shared: &Shared, db_path: &str) {
    let conn = match crate::db::open::open_walk_reader(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            // Without rows nothing can be classified, so stopping the walk is
            // the honest outcome: continuing would treat every file as new
            // and every row as stale.
            crate::log_warn!("walk reader: {}", e);
            shared.shutdown();
            return;
        }
    };

    while let Some(work) = shared.take_prefetch() {
        match work {
            PrefetchWork::Dir(dir) => {
                match crate::db::repo::dir_rows(&conn, &path_to_db_string(&dir)) {
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
    /// yet handed to [`ParallelWalk::try_next`]. See `wait_ready`.
    pending: Option<WalkEvent>,
    handles: Vec<JoinHandle<()>>,
    /// Joined by [`ParallelWalk::finish`] alongside the workers. Held
    /// separately only so a failure to open its connection is attributable.
    prefetch: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    ctx: Arc<Ctx>,
}

impl ParallelWalk {
    /// Directories that could not be read. Only final once the iterator has
    /// ended, because the channel closes when the last worker exits.
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
    /// The caller's vanished-directory sweep needs this: a directory deleted
    /// wholesale is never read, so nothing reconciles the rows beneath it,
    /// and "was this parent reached at all" is the only way to find them.
    ///
    /// Only meaningful once the walk has finished.
    pub fn seen_dirs(&self) -> HashSet<String> {
        self.shared
            .queue
            .lock()
            .unwrap()
            .seen_dirs
            .iter()
            .map(|d| path_to_db_string(d))
            .collect()
    }

    /// A cheap, cloneable handle for reading worker activity while the
    /// walk's iterator is mutably borrowed by a `for` loop.
    ///
    /// Meaningful only while the walk is running: once the workers exit, the
    /// pool size stays but the busy count is permanently zero.
    pub fn worker_stats(&self) -> WorkerStats {
        self.shared.stats.clone()
    }

    /// Join the workers and report whether every one of them finished
    /// cleanly.
    ///
    /// The caller needs this because a dead worker and a finished worker look
    /// identical from the receiving end: both close the channel, so iteration
    /// simply ends. Treating a panicked walk as a completed one would hand
    /// stale cleanup a partial file set and delete everything the dead workers
    /// never reached. So: join before deciding anything about what the walk
    /// saw. This is the reference statement of that rule; the content pass
    /// ([`crate::content::ContentPass::finish`]) and the writer loop's
    /// `TryNext::Finished` arms follow it.
    pub fn finish(&mut self) -> bool {
        // Dropping the receiver first releases any worker parked in `send`.
        self.rx = None;
        let mut clean = true;
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                clean = false;
            }
        }
        // After the workers, so a prefetcher parked waiting for the pool to
        // drain below PREFETCH_AHEAD is already free to observe `done`.
        if let Some(handle) = self.prefetch.take() {
            // `shutdown` is what releases it; without that this would block
            // until the queue emptied on its own.
            self.shared.shutdown();
            if handle.join().is_err() {
                clean = false;
            }
        }
        clean
    }
}

/// Result of a non-blocking pull from a producer pool.
///
/// Shared by every pass the writer loop multiplexes — the walk here and the
/// content pass in [`crate::content`] — so that draining one root's work reads
/// identically whichever pass it came from.
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
/// What the writer loop backs off with instead of `thread::sleep`. A sleep is
/// the wrong instrument twice over: it ignores work that lands a microsecond
/// later, and on Windows the default timer resolution is 15.6 ms, so a 2 ms
/// backoff actually stalls for 15.6 — nearly eight times the intended pause,
/// every time the channels run momentarily dry. `recv_timeout` parks on the
/// channel's own condition variable, so a sender wakes it immediately and the
/// timeout is only the ceiling.
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
    /// The writer loop's idle backoff. It multiplexes several walks, so it
    /// cannot simply block on one of them and consume the result — hence the
    /// one-slot pushback: the event is taken off the channel, but the loop still
    /// sees it in its normal round-robin order.
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
            // A finished walk is "ready" in the sense the caller cares about:
            // there is something to do (notice it ended), so do not keep waiting.
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
    suspend_flag: Arc<AtomicBool>,
    workers: usize,
) -> ParallelWalk {
    let mut queue = Queue::default();
    let mut unresolvable: Vec<PathBuf> = Vec::new();
    for root in roots {
        // Canonicalize here, not just at the caller, so "everything below a
        // root is already canonical" holds however this is called. Without it
        // a non-canonical root would spell every path below it differently
        // from the stored rows: every file would look new *and* every stored
        // row would look stale.
        //
        // Roots themselves are never filtered — the user chose them, so a
        // hidden or ignore-matching root still gets walked.
        match fs::canonicalize(root) {
            Ok(dir) => {
                let dir = PathBuf::from(path_to_db_string(&dir));
                if queue.seen_dirs.insert(dir.clone()) {
                    // Through the prefetcher like any other directory: a root
                    // needs its rows before anything inside it can be
                    // classified.
                    queue.needs_rows.push(dir);
                }
            }
            Err(e) => {
                crate::log_warn!("cannot resolve indexing root {}: {}", root, e);
                // An unmounted or renamed root yields nothing, which is
                // indistinguishable from "all its files were deleted" unless
                // we say so. Recorded here so stale cleanup leaves it alone.
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
        pruned: PruneCounts::default(),
        config,
        registry,
        unreadable: UnreadableDirs::default(),
        stop_flag,
        suspend_flag,
    });

    for root in unresolvable {
        ctx.unreadable.record(root);
    }

    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
    let handles = (0..threads)
        .map(|_| {
            let (shared, ctx, tx) = (shared.clone(), ctx.clone(), tx.clone());
            crate::platform::spawn_worker("qs-walk", move || {
                // Walker threads are the bulk of a run's CPU and I/O; the
                // foreground must stay ahead of them.
                crate::platform::set_background_priority();
                worker(&shared, &ctx, &tx)
            })
        })
        .collect();
    // The workers must hold the only senders, or `recv` never reports the end
    // of the walk and phase 1 hangs forever. The prefetcher deliberately holds
    // none: it produces jobs, not files.
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
/// A network share wants far more threads than cores, because each worker
/// spends its time blocked on a round trip rather than on the CPU; a local
/// disk wants few. Users cannot be asked to tune this — the indexer runs on
/// machines we do not configure — so it is detected rather than configured.
/// With a mix of roots the higher count wins: over-threading a local disk
/// costs a little, under-threading a share costs everything.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(tag: &str) -> PathBuf {
        crate::testutil::scratch_dir(tag)
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    /// A database seeded with `rows` as already-indexed files.
    ///
    /// Classification data now comes from SQLite rather than from a map the
    /// caller passes in, so these tests build the state they are testing
    /// against the same way the indexer does.
    fn db_with(tag: &str, rows: &[(String, u64)]) -> PathBuf {
        let p = crate::testutil::scratch_dir(tag).join("index.sqlite");
        let conn = crate::db::open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        for (path, mtime) in rows {
            let as_path = Path::new(path);
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime, type, \
                                    basic_state, content_state)
                 VALUES (?1, ?2, ?3, 0, ?4, 0, 1, 3)",
                rusqlite::params![
                    as_path.file_name().unwrap().to_string_lossy(),
                    path,
                    as_path.parent().unwrap().to_string_lossy(),
                    *mtime as i64,
                ],
            )
            .unwrap();
        }
        p
    }

    fn empty_db(tag: &str) -> PathBuf {
        db_with(tag, &[])
    }

    fn walk(root: &Path, db: &Path) -> Vec<WalkedFile> {
        walk_with(root, db, false, false)
    }

    fn walk_with(
        root: &Path,
        db: &Path,
        follow_symlinks: bool,
        include_hidden: bool,
    ) -> Vec<WalkedFile> {
        files_only(walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            follow_symlinks,
            include_hidden,
            IgnoreSet::compile(&[]).unwrap(),
            db.to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        ))
    }

    /// Drop the reconciliation events; most tests are about which files the
    /// walk reports.
    fn files_only(walk: ParallelWalk) -> Vec<WalkedFile> {
        walk.filter_map(|e| match e {
            WalkEvent::File(f) => Some(f),
            WalkEvent::Stale(_) => None,
        })
        .collect()
    }

    /// Paths the walk decided no longer have a file behind them.
    fn stale_only(walk: ParallelWalk) -> Vec<String> {
        walk.filter_map(|e| match e {
            WalkEvent::Stale(paths) => Some(paths),
            WalkEvent::File(_) => None,
        })
        .flatten()
        .collect()
    }

    fn names(files: &[WalkedFile]) -> Vec<String> {
        let mut n: Vec<String> = files
            .iter()
            .map(|f| {
                Path::new(&f.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        n.sort();
        n
    }

    #[test]
    fn walks_a_nested_tree_exactly_once() {
        let root = tmp_tree("nested");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));
        touch(&root.join("sub/deep/c.txt"));
        touch(&root.join("other/d.txt"));

        let files = walk(&root, &empty_db("nested"));
        assert_eq!(names(&files), vec!["a.txt", "b.txt", "c.txt", "d.txt"]);

        let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
        assert_eq!(unique.len(), files.len(), "no path may be yielded twice");
        fs::remove_dir_all(&root).ok();
    }

    /// A name that is not valid UTF-8 survives `stat` but not the round trip
    /// through `files.path`, so it must be skipped before anything tries to
    /// open it by that string. Unix only: on Windows `OsString` comes from
    /// UTF-16 and there is no way to build the case.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_is_skipped_and_never_prepared() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = tmp_tree("nonutf8");
        touch(&root.join("plain.txt"));
        // 0xFF is not valid UTF-8 anywhere in a sequence, so the name only
        // survives `to_string_lossy` as U+FFFD.
        let bad = root.join(OsStr::from_bytes(b"DRH257\xff~X.MP4"));
        touch(&bad);
        assert!(bad.symlink_metadata().is_ok(), "the file really is on disk");

        let files = walk(&root, &empty_db("nonutf8"));

        // Both are yielded, so neither reads as deleted...
        assert_eq!(files.len(), 2, "the bad name is still reported as seen");
        // ...but only the representable one is prepared for insertion, which
        // is what keeps it out of the hasher and out of FTS.
        let prepared: Vec<&WalkedFile> = files.iter().filter(|f| f.record.is_some()).collect();
        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].path.ends_with("plain.txt"));

        let skipped = files.iter().find(|f| f.record.is_none()).unwrap();
        assert!(matches!(skipped.action, FileIndexAction::Skip));
        assert!(
            skipped.path.contains('\u{FFFD}'),
            "stored spelling is the lossy one"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wide_directory_is_split_across_workers_without_loss() {
        // More than FILES_PER_JOB in one flat directory, so the chunking path
        // and the termination protocol both run under real contention.
        let root = tmp_tree("wide");
        let count = FILES_PER_JOB * 4 + 7;
        for i in 0..count {
            touch(&root.join(format!("f{:05}.txt", i)));
        }

        let files = walk(&root, &empty_db("wide"));
        assert_eq!(files.len(), count, "every file is yielded exactly once");
        let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
        assert_eq!(unique.len(), count, "and none is yielded twice");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn terminates_on_an_empty_root() {
        // The "queue empty at t=0" corner: every worker must observe the walk
        // as finished rather than waiting for work that will never arrive.
        let root = tmp_tree("empty");
        assert!(walk(&root, &empty_db("empty")).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unchanged_files_are_never_opened() {
        // The property the whole SMB story rests on: a re-index of an
        // unchanged tree must cost one stat per file and no file opens.
        let root = tmp_tree("skip");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));

        let first = walk(&root, &empty_db("skip-first"));
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|f| f.action == FileIndexAction::Insert));

        let indexed: Vec<(String, u64)> = first
            .iter()
            .map(|f| (f.path.clone(), f.record.as_ref().unwrap().mtime))
            .collect();

        let second = walk(&root, &db_with("skip-second", &indexed));
        assert_eq!(
            second.len(),
            2,
            "unchanged files are still reported as seen"
        );
        for f in &second {
            assert_eq!(f.action, FileIndexAction::Skip);
            assert!(f.record.is_none(), "an unchanged file is never hashed");
        }
        fs::remove_dir_all(&root).ok();
    }

    /// The walk's mtime and a `stat`'s mtime must be the same number.
    ///
    /// On Windows the walk now reads mtime out of the directory entry while the
    /// *watcher* writes its rows from `fs::metadata`; if the two ever disagreed,
    /// every run would reclassify files nothing had touched and the index would
    /// churn forever. Seeding the index the watcher's way and demanding the walk
    /// call every file `Skip` is what pins them together.
    ///
    /// `unchanged_files_are_never_opened` cannot catch this on its own: both of
    /// its walks read from the same source, so they agree with each other even
    /// when both disagree with a stat. Vacuous on Unix, where there is only ever
    /// one source; on Windows it is the whole guarantee.
    #[test]
    fn a_walk_agrees_with_a_stat_seeded_index() {
        let root = tmp_tree("stat-seeded");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));

        let seeded: Vec<(String, u64)> = [root.join("a.txt"), root.join("sub/b.txt")]
            .iter()
            .map(|p| {
                let mtime = fs::metadata(p)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                (path_to_db_string(p), mtime)
            })
            .collect();

        let files = walk(&root, &db_with("stat-seeded", &seeded));
        assert_eq!(files.len(), 2);
        for f in &files {
            assert_eq!(
                f.action,
                FileIndexAction::Skip,
                "the directory read disagreed with a stat about {}",
                f.path
            );
            assert!(f.record.is_none());
        }
        fs::remove_dir_all(&root).ok();
    }

    /// Windows: the three fields `prepare` and `prepare_file_record` read out of
    /// the cached buffer, pinned against what a `stat` would have said — for the
    /// case that now skips the `stat` entirely.
    #[test]
    #[cfg(windows)]
    fn cached_directory_metadata_matches_a_stat_field_for_field() {
        let root = tmp_tree("cached-meta");
        touch(&root.join("a.txt"));
        fs::write(root.join("b.bin"), vec![0u8; 5000]).unwrap();

        for entry in fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            let cached = crate::platform::entry_cached_metadata(|| entry.metadata().ok())
                .expect("a plain file is served from the directory read");
            let fresh = fs::metadata(entry.path()).unwrap();
            assert_eq!(cached.is_file(), fresh.is_file());
            assert_eq!(cached.len(), fresh.len());
            assert_eq!(cached.modified().unwrap(), fresh.modified().unwrap());
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_seen_file_is_reported_even_when_it_cannot_be_read() {
        // A path missing from the stream gets its index row deleted, so
        // "couldn't process it" must still be reported as seen.
        let root = tmp_tree("unreadable-file");
        touch(&root.join("fine.txt"));
        let bad = root.join("bad.txt");
        touch(&bad);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();

            let files = walk(&root, &empty_db("unreadable-file"));
            fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).ok();

            assert_eq!(names(&files), vec!["bad.txt", "fine.txt"]);
            let bad_entry = files.iter().find(|f| f.path.ends_with("bad.txt")).unwrap();
            assert!(bad_entry.record.is_none(), "unopenable, so no record");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_directory_is_recorded_not_silently_empty() {
        let root = tmp_tree("unreadable-dir");
        touch(&root.join("visible.txt"));
        let locked = root.join("locked");
        touch(&locked.join("inside.txt"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

            let mut w = walk_indexable_files(
                &[root.to_string_lossy().into_owned()],
                false,
                false,
                IgnoreSet::compile(&[]).unwrap(),
                empty_db("unreadable-dir").to_str().unwrap(),
                Config::default(),
                Arc::new(Registry::default_set()),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                4,
            );
            let files: Vec<WalkedFile> = w
                .by_ref()
                .filter_map(|e| match e {
                    WalkEvent::File(f) => Some(f),
                    WalkEvent::Stale(_) => None,
                })
                .collect();
            let recorded = !w.unreadable().is_empty();
            let covers = w
                .unreadable()
                .covers(locked.join("inside.txt").to_str().unwrap());

            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

            assert_eq!(names(&files), vec!["visible.txt"]);
            assert!(recorded, "the failure must be recorded");
            assert!(covers, "so rows beneath it survive stale cleanup");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn symlink_loop_terminates() {
        // A hand-rolled walker has none of walkdir's cycle detection; the
        // canonical-directory set is what stands in for it.
        let root = tmp_tree("loop");
        touch(&root.join("real.txt"));
        std::os::unix::fs::symlink(&root, root.join("self_link")).unwrap();

        let files = walk_with(&root, &empty_db("loop"), true, false);
        assert_eq!(names(&files), vec!["real.txt"], "the cycle is visited once");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_file_resolves_to_its_target_path() {
        // Preserves the stored spelling: resolving links where they are found
        // is what lets the per-file `canonicalize` go away without re-spelling
        // rows on the next run.
        //
        // The walk reaches this file twice — directly, and through the alias —
        // and reports it twice. That is deliberate: the walker dedupes
        // *directories*, while the caller's `seen_paths` dedupes files, which
        // is where a UNIQUE(path) violation would otherwise come from. What
        // matters here is that both routes agree on the canonical path, so
        // that dedup can work at all.
        let root = tmp_tree("symlink-file");
        touch(&root.join("real/target.txt"));
        fs::create_dir_all(root.join("links")).unwrap();
        std::os::unix::fs::symlink(root.join("real/target.txt"), root.join("links/alias.txt"))
            .unwrap();

        let files = walk_with(&root, &empty_db("symlink-file"), true, false);
        let paths: HashSet<&String> = files.iter().map(|f| &f.path).collect();
        assert_eq!(paths.len(), 1, "both routes report one canonical path");

        let canonical = path_to_db_string(&root.join("real/target.txt").canonicalize().unwrap());
        assert_eq!(
            *paths.into_iter().next().unwrap(),
            canonical,
            "the target, not the alias"
        );

        // The alias itself is still reported, so its row is never mistaken for
        // deleted — it is reported under the *target's* path.
        assert_eq!(files.len(), 2, "seen twice, spelled once");
        assert!(files.iter().any(|f| f.aliased), "the link route is marked");
        fs::remove_dir_all(&root).ok();
    }

    /// The counterpart, and the reason both symlink kinds are gated together:
    /// `filtered_walk` — which the watcher and the incremental path use —
    /// passes `follow_links` to walkdir and follows neither kind. A file link
    /// followed only here would be re-indexed by every full run and never
    /// updated between them, and its target may sit outside every root.
    #[test]
    #[cfg(unix)]
    fn a_file_symlink_is_not_followed_when_links_are_off() {
        let root = tmp_tree("symlink-off");
        touch(&root.join("real/target.txt"));
        fs::create_dir_all(root.join("links")).unwrap();
        std::os::unix::fs::symlink(root.join("real/target.txt"), root.join("links/alias.txt"))
            .unwrap();
        // A target outside the walked tree: with links off it must not be
        // reachable at all.
        let outside = tmp_tree("symlink-off-outside");
        touch(&outside.join("elsewhere.txt"));
        std::os::unix::fs::symlink(outside.join("elsewhere.txt"), root.join("links/out.txt"))
            .unwrap();

        let files = walk_with(&root, &empty_db("symlink-off"), false, false);
        assert_eq!(
            names(&files),
            vec!["target.txt"],
            "only the real file, reached directly"
        );
        assert!(!files.iter().any(|f| f.aliased), "nothing was resolved");

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    /// Windows counterpart of the symlink tests: `canonicalize` spells a
    /// junction's target `\\?\C:\…`, and the walker must strip that before
    /// storing — otherwise everything beneath the junction is spelled
    /// differently from the plainly-spelled roots, full-path ignore patterns
    /// never match there, and the canonical-directory dedup fails.
    #[test]
    #[cfg(windows)]
    fn a_followed_junction_stores_plain_paths() {
        let root = tmp_tree("junction");
        touch(&root.join("real").join("target.txt"));
        // Junctions need no privileges, unlike symlinks; still, skip cleanly
        // on filesystems where mklink refuses.
        let made = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                root.join("jlink").to_str().unwrap(),
                root.join("real").to_str().unwrap(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            fs::remove_dir_all(&root).ok();
            return;
        }

        let files = walk_with(&root, &empty_db("junction"), true, false);
        for f in &files {
            assert!(
                !f.path.starts_with(r"\\?\"),
                "stored path leaked a verbatim prefix: {}",
                f.path
            );
        }
        // The junction resolves to the same canonical directory the walk
        // reaches directly, so the dedup visits it exactly once. A leaked
        // prefix would spell it twice and report the file twice.
        assert_eq!(names(&files), vec!["target.txt"], "visited once");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hidden_and_ignored_entries_are_pruned() {
        let root = tmp_tree("prune");
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/keep2.txt"));
        touch(&root.join("sub/skip.tmp"));
        touch(&root.join(".hidden/inside.txt"));
        touch(&root.join(".dotfile"));
        touch(&root.join("node_modules/dep/index.js"));

        let ignore =
            IgnoreSet::compile(&["*.tmp".to_string(), "node_modules".to_string()]).unwrap();
        let files: Vec<WalkedFile> = files_only(walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            ignore,
            empty_db("prune").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        ));
        assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

        let files = walk_with(&root, &empty_db("prune-hidden"), false, true);
        assert_eq!(
            names(&files),
            vec![
                ".dotfile",
                "index.js",
                "inside.txt",
                "keep.txt",
                "keep2.txt",
                "skip.tmp"
            ],
            "include_hidden with no ignore patterns keeps everything"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The counters behind the one-line summary a run logs.
    ///
    /// The property worth pinning is that a pruned *directory* costs one
    /// increment rather than one per file beneath it — the subtree is never
    /// enumerated, which is exactly why logging per entry was rejected and
    /// counting was not.
    #[test]
    fn pruned_entries_are_counted_by_reason() {
        let root = tmp_tree("prune-counts");
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/keep2.txt"));
        touch(&root.join("sub/skip.tmp"));
        // Two files below, one prune.
        touch(&root.join(".hidden/inside.txt"));
        touch(&root.join(".hidden/also-inside.txt"));
        touch(&root.join(".dotfile"));
        // Three levels below, still one prune.
        touch(&root.join("node_modules/dep/lib/index.js"));

        let ignore =
            IgnoreSet::compile(&["*.tmp".to_string(), "node_modules".to_string()]).unwrap();
        let mut walk = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            ignore,
            empty_db("prune-counts").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        let files: Vec<WalkedFile> = (&mut walk)
            .filter_map(|e| match e {
                WalkEvent::File(f) => Some(f),
                WalkEvent::Stale(_) => None,
            })
            .collect();
        assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

        let pruned = walk.pruned();
        assert_eq!(
            pruned.dot_named.load(Ordering::Relaxed),
            2,
            "`.hidden` and `.dotfile` — not the two files inside `.hidden`"
        );
        assert_eq!(
            pruned.ignored.load(Ordering::Relaxed),
            2,
            "`skip.tmp` and `node_modules` — not `index.js` three levels down"
        );
        // Attributes are a Windows concept; on Linux nothing can reach this
        // counter, and on Windows a temp tree carries no Hidden bit.
        assert_eq!(pruned.attribute.load(Ordering::Relaxed), 0);
        assert_eq!(pruned.total(), 4);

        let summary = pruned.summary().expect("something was pruned");
        assert!(summary.contains("4 entries"), "{}", summary);

        fs::remove_dir_all(&root).ok();
    }

    /// A clean tree must add no line to a log whose whole budget is 5,000
    /// entries.
    #[test]
    fn a_tree_with_nothing_pruned_reports_no_summary() {
        let root = tmp_tree("prune-none");
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/keep2.txt"));

        let mut walk = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("prune-none").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        let files: Vec<WalkedFile> = (&mut walk)
            .filter_map(|e| match e {
                WalkEvent::File(f) => Some(f),
                WalkEvent::Stale(_) => None,
            })
            .collect();
        assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

        assert_eq!(walk.pruned().total(), 0);
        assert!(walk.pruned().summary().is_none());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_directory_reports_rows_with_no_file_behind_them() {
        // The per-directory diff, at the level it is computed: one listing
        // against one directory's rows, before any splitting.
        let root = tmp_tree("reconcile");
        touch(&root.join("kept.txt"));
        touch(&root.join("sub/nested.txt"));

        let gone = path_to_db_string(&root.join("removed.txt"));
        let gone_nested = path_to_db_string(&root.join("sub/vanished.txt"));
        let kept = path_to_db_string(&root.join("kept.txt"));
        let db = db_with(
            "reconcile",
            &[
                (gone.clone(), 1),
                (gone_nested.clone(), 1),
                (kept.clone(), 1),
            ],
        );

        let mut stale = stale_only(walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            db.to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        ));
        stale.sort();

        let mut want = vec![gone, gone_nested];
        want.sort();
        assert_eq!(
            stale, want,
            "exactly the rows with no file, from both directories"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn an_unreadable_directory_reports_nothing_stale() {
        use std::os::unix::fs::PermissionsExt;

        // A failed read leaves an empty listing, which must never be diffed:
        // every row under it would look deleted.
        let root = tmp_tree("reconcile-locked");
        let locked = root.join("locked");
        touch(&locked.join("inside.txt"));

        let db = db_with(
            "reconcile-locked",
            &[(path_to_db_string(&locked.join("inside.txt")), 1)],
        );

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let stale = stale_only(walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            db.to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        ));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

        assert!(
            stale.is_empty(),
            "an unreadable directory is not an empty one"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hidden_root_is_still_walked() {
        // Roots are chosen explicitly, so the hidden rule must not silence one.
        let base = tmp_tree("hidden-root");
        let root = base.join(".config");
        touch(&root.join("app.conf"));

        let files = walk(&root, &empty_db("hidden-root"));
        assert_eq!(names(&files), vec!["app.conf"]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn stop_flag_ends_the_walk_without_hanging() {
        let root = tmp_tree("stop");
        for i in 0..500 {
            touch(&root.join(format!("f{:04}.txt", i)));
        }

        let stop = Arc::new(AtomicBool::new(true));
        let files: Vec<WalkedFile> = files_only(walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("stop").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            stop,
            Arc::new(AtomicBool::new(false)),
            4,
        ));

        assert!(
            files.len() < 500,
            "an already-stopped walk does not run to completion"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropping_the_walk_early_does_not_hang() {
        // Workers blocked in `send` must be released by the receiver going
        // away, or `Drop` would join threads that never wake.
        let root = tmp_tree("early-drop");
        for i in 0..2000 {
            touch(&root.join(format!("sub{}/f{}.txt", i % 10, i)));
        }

        let mut w = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("early-drop").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        assert!(w.next().is_some());
        drop(w); // must return, not deadlock

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn overlapping_roots_yield_each_file_once() {
        let root = tmp_tree("overlap");
        touch(&root.join("sub/a.txt"));

        let files: Vec<WalkedFile> = files_only(walk_indexable_files(
            &[
                root.to_string_lossy().into_owned(),
                root.join("sub").to_string_lossy().into_owned(),
            ],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("overlap").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        ));

        assert_eq!(files.len(), 1, "the nested root must not double-index");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repeated_walks_agree_on_the_result_set() {
        // The termination protocol is racy by nature; run it enough times
        // under contention that a premature exit would show up.
        let root = tmp_tree("repeat");
        for i in 0..40 {
            touch(&root.join(format!("d{}/f{}.txt", i % 7, i)));
        }
        let expected = names(&walk(&root, &empty_db("determinism-base")));
        assert_eq!(expected.len(), 40);

        for run in 0..30 {
            assert_eq!(
                names(&walk(&root, &empty_db(&format!("determinism-{}", run)))),
                expected,
                "run {}",
                run
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finish_reports_a_clean_walk_and_is_idempotent() {
        // The caller gates stale-row deletion on this: a walk whose workers
        // died yields a partial file set, and "not seen" would otherwise be
        // read as "deleted".
        let root = tmp_tree("finish");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));

        let mut w = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("finish").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        let files: Vec<WalkedFile> = w
            .by_ref()
            .filter_map(|e| match e {
                WalkEvent::File(f) => Some(f),
                WalkEvent::Stale(_) => None,
            })
            .collect();
        assert_eq!(files.len(), 2);
        assert!(w.finish(), "no worker panicked");
        // Drop calls it again; joining an already-drained handle list must be
        // a no-op rather than a panic.
        assert!(w.finish());
        drop(w);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn local_temp_dir_is_not_detected_as_network() {
        let root = tmp_tree("fstype");
        assert_eq!(
            thread_count_for(&[root.to_string_lossy().into_owned()]),
            LOCAL_THREADS
        );
        fs::remove_dir_all(&root).ok();
    }
}
