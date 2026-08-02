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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::UNIX_EPOCH;

use crate::config::{Config, IgnoreSet};
use crate::extract::Registry;
use crate::file_handling::{
    classify_for_indexing, path_to_db_string, prepare_file_record, warn_if_unrepresentable,
    ExistingFileEntry,
    FileIndexAction, OwnedNewFile, UnreadableDirs,
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
    ///
    /// Present for *every* file the walk saw, including unchanged ones and
    /// ones that could not be read. The caller's "seen" set drives stale-row
    /// deletion, so a path missing from this stream is a path whose index row
    /// gets deleted.
    pub path: String,
    pub action: FileIndexAction,
    /// `None` when there is nothing to write: the file was unchanged, or its
    /// record could not be built. Never a reason to drop [`WalkedFile::path`].
    pub record: Option<OwnedNewFile>,
}

/// Work waiting for a thread.
enum Job {
    /// Read this directory and process its files.
    Dir(PathBuf),
    /// Process this slice of one directory's files, split off because the
    /// directory was too wide for one worker to be worth serialising on.
    Files(Vec<PathBuf>),
}

#[derive(Default)]
struct Queue {
    /// LIFO. A directory's children are processed close in time to the read
    /// that discovered them, which is what the attribute cache rewards, and it
    /// keeps the live frontier depth-first-ish instead of holding an entire
    /// breadth-first level in memory.
    jobs: Vec<Job>,
    /// Workers currently holding a job — that is, workers that may still push
    /// more. The walk is over when this is zero and `jobs` is empty.
    active: usize,
    /// Canonical directories already queued. Collapses overlapping roots and
    /// makes symlink cycles impossible: a cycle must revisit a canonical path,
    /// and every directory pushed here is canonical.
    seen_dirs: HashSet<PathBuf>,
    done: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    idle: Condvar,
    /// Workers currently processing (not parked waiting for work). Purely
    /// observational, for progress display: two relaxed atomic ops per
    /// *job* (a directory read plus up to [`FILES_PER_JOB`] files), so it
    /// costs nothing the queue mutex didn't already.
    busy: AtomicUsize,
}

/// Decrements the busy count however the worker leaves its job — including
/// early returns on stop and panics.
struct BusyGuard<'a>(&'a AtomicUsize);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Lock-free view of walker activity for progress displays.
#[derive(Clone)]
pub struct WorkerStats {
    shared: Arc<Shared>,
    total: usize,
}

impl WorkerStats {
    /// Workers doing work right now (the rest are parked).
    pub fn active(&self) -> usize {
        self.shared.busy.load(Ordering::Relaxed).min(self.total)
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
                return Some((job, ActiveJob { shared: self, finished: false }));
            }
            if q.active == 0 {
                q.done = true;
                self.idle.notify_all();
                return None;
            }
            q = self.idle.wait(q).unwrap();
        }
    }

    /// Push discovered work and give the job slot back, under a single lock
    /// acquisition. Doing both together is what makes the `active == 0` test
    /// in [`Shared::take`] an end-of-walk proof rather than a race: a worker
    /// that has popped the last job but not yet published its children must
    /// never look idle.
    fn publish(&self, found: Vec<Job>) {
        let mut q = self.queue.lock().unwrap();
        for job in found {
            if let Job::Dir(ref dir) = job {
                if !q.seen_dirs.insert(dir.clone()) {
                    continue;
                }
            }
            q.jobs.push(job);
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
    fn finish(mut self, found: Vec<Job>) {
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

struct Ctx {
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: IgnoreSet,
    existing_files: Arc<HashMap<String, ExistingFileEntry>>,
    config: Config,
    /// Lets a worker finish small text files outright: the head it reads to
    /// hash them is already their entire contents, so an extractor that works
    /// from bytes saves the content pass an open/read/close per file.
    registry: Arc<Registry>,
    unreadable: UnreadableDirs,
    stop_flag: Arc<Mutex<bool>>,
    suspend_flag: Arc<AtomicBool>,
}

/// Read one directory, apply the hidden/ignore rules, and split the result:
/// subdirectories and overflow file chunks go to `found` for the pool, the
/// remaining files come back for this worker to handle immediately.
fn read_directory(dir: &Path, ctx: &Ctx, found: &mut Vec<Job>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            // Not the same as "this directory is empty": see UnreadableDirs.
            crate::log_warn!("cannot read {}: {}", dir.display(), e);
            ctx.unreadable.record(dir.to_path_buf());
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                crate::log_warn!("cannot read an entry of {}: {}", dir.display(), e);
                ctx.unreadable.record(dir.to_path_buf());
                continue;
            }
        };

        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `entry.metadata()` is only consulted on Windows, where it is free —
        // the attributes came back with the directory read. On Unix the
        // closure is never called, so this stays at zero extra syscalls.
        if !ctx.include_hidden
            && crate::platform::entry_is_hidden(&name, || entry.metadata().ok())
        {
            continue;
        }
        if ctx.ignore.matches_component(&name) {
            continue;
        }
        let path = entry.path();
        if ctx.ignore.matches_path_pattern(&path) {
            continue;
        }

        // `file_type` is the cached `d_type` from the directory read, so
        // splitting directories from files here is free.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => found.push(Job::Dir(path)),
            Ok(ft) if ft.is_symlink() => {
                // Resolve aliases where they are found. The target's canonical
                // path is what the index stores, and pushing only canonical
                // directories is what keeps `seen_dirs` able to break cycles.
                if let Ok(target) = path.canonicalize() {
                    match fs::metadata(&target) {
                        Ok(m) if m.is_dir() => {
                            if ctx.follow_symlinks {
                                found.push(Job::Dir(target));
                            }
                        }
                        Ok(_) => files.push(target),
                        Err(_) => {}
                    }
                }
            }
            Ok(_) => files.push(path),
            Err(_) => {}
        }
    }

    // Spread a wide directory across the pool, keeping the tail for
    // ourselves so the entries the read just warmed are handled now.
    while files.len() > FILES_PER_JOB {
        let chunk = files.split_off(files.len() - FILES_PER_JOB);
        found.push(Job::Files(chunk));
    }
    files
}

/// One `stat`, then classify; only files that are actually going to be
/// written get opened, and small text files are finished outright.
fn prepare(path: PathBuf, ctx: &Ctx) -> WalkedFile {
    let db_path = path_to_db_string(&path);

    // A name that is not valid UTF-8 cannot be stored in `files.path` and read
    // back as the same file, so there is nothing to hash or text-index. `Skip`
    // rather than an early return with no entry: the caller reads a missing
    // path as "deleted", and this file was seen, not removed.
    if warn_if_unrepresentable(&path) {
        return WalkedFile { path: db_path, action: FileIndexAction::Skip, record: None };
    }

    let Ok(meta) = fs::metadata(&path) else {
        // Seen but unreadable. Emitting it anyway keeps its index row alive:
        // a transient stat failure must not read as "deleted".
        return WalkedFile { path: db_path, action: FileIndexAction::Skip, record: None };
    };
    let Some(mtime) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
    else {
        return WalkedFile { path: db_path, action: FileIndexAction::Skip, record: None };
    };

    let action = classify_for_indexing(&db_path, mtime, &ctx.existing_files);
    let record = match action {
        // Unchanged: never opened, never hashed. This is nearly every file on
        // a re-index, and it is the case that has to stay at one syscall.
        FileIndexAction::Skip => None,
        // `prepare_file_record` gates on `is_file()`, which is what keeps us
        // from opening a FIFO — that would block forever, uninterruptibly.
        _ => prepare_file_record(&db_path, &meta, &ctx.config, &ctx.registry),
    };

    WalkedFile { path: db_path, action, record }
}

fn worker(shared: &Shared, ctx: &Ctx, tx: &mpsc::SyncSender<WalkedFile>) {
    while let Some((job, slot)) = shared.take() {
        shared.busy.fetch_add(1, Ordering::Relaxed);
        let _busy = BusyGuard(&shared.busy);
        if should_abort(&ctx.stop_flag, &ctx.suspend_flag) {
            shared.shutdown();
            return;
        }

        let mut found = Vec::new();
        let files = match job {
            Job::Dir(dir) => read_directory(&dir, ctx, &mut found),
            Job::Files(files) => files,
        };

        // Hand the subdirectories over before doing our own per-file work, so
        // the rest of the pool never idles waiting behind one worker. This
        // also confines the job slot to `read_directory`.
        slot.finish(found);

        for path in files {
            if should_abort(&ctx.stop_flag, &ctx.suspend_flag) {
                shared.shutdown();
                return;
            }
            if tx.send(prepare(path, ctx)).is_err() {
                // Receiver gone: the run was stopped or failed. Not an error.
                shared.shutdown();
                return;
            }
        }
    }
}

/// A running parallel walk. Iterating it drains finished files; dropping it
/// stops the workers and joins them.
pub struct ParallelWalk {
    rx: Option<mpsc::Receiver<WalkedFile>>,
    handles: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    ctx: Arc<Ctx>,
}

impl ParallelWalk {
    /// Directories that could not be read. Only final once the iterator has
    /// ended, because the channel closes when the last worker exits.
    pub fn unreadable(&self) -> &UnreadableDirs {
        &self.ctx.unreadable
    }

    /// A cheap, cloneable handle for reading worker activity while the
    /// walk's iterator is mutably borrowed by a `for` loop.
    pub fn worker_stats(&self) -> WorkerStats {
        WorkerStats {
            shared: self.shared.clone(),
            total: self.handles.len(),
        }
    }

    /// Join the workers and report whether every one of them finished
    /// cleanly.
    ///
    /// The caller needs this because a dead worker and a finished worker look
    /// identical from the receiving end: both close the channel, so iteration
    /// simply ends. Treating a panicked walk as a completed one would hand
    /// stale cleanup a partial file set and delete everything the dead workers
    /// never reached.
    pub fn finish(&mut self) -> bool {
        // Dropping the receiver first releases any worker parked in `send`.
        self.rx = None;
        let mut clean = true;
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                clean = false;
            }
        }
        clean
    }
}

/// Result of a non-blocking pull from a walk.
pub enum TryNext {
    Item(WalkedFile),
    /// Nothing ready right now; the walk is still running.
    Empty,
    /// The walk has ended (all workers exited, for any reason).
    Finished,
}

impl ParallelWalk {
    /// Non-blocking variant of `next`, for callers multiplexing several
    /// walks (the per-root writer loop).
    pub fn try_next(&mut self) -> TryNext {
        match &self.rx {
            None => TryNext::Finished,
            Some(rx) => match rx.try_recv() {
                Ok(file) => TryNext::Item(file),
                Err(mpsc::TryRecvError::Empty) => TryNext::Empty,
                Err(mpsc::TryRecvError::Disconnected) => TryNext::Finished,
            },
        }
    }
}

impl Iterator for ParallelWalk {
    type Item = WalkedFile;

    fn next(&mut self) -> Option<WalkedFile> {
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
#[allow(clippy::too_many_arguments)]
pub fn walk_indexable_files(
    roots: &[String],
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: IgnoreSet,
    existing_files: Arc<HashMap<String, ExistingFileEntry>>,
    config: Config,
    registry: Arc<Registry>,
    stop_flag: Arc<Mutex<bool>>,
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
                    queue.jobs.push(Job::Dir(dir));
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
        busy: AtomicUsize::new(0),
    });
    let ctx = Arc::new(Ctx {
        follow_symlinks,
        include_hidden,
        ignore,
        existing_files,
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
            thread::spawn(move || worker(&shared, &ctx, &tx))
        })
        .collect();
    // The workers must hold the only senders, or `recv` never reports the end
    // of the walk and phase 1 hangs forever.
    drop(tx);

    ParallelWalk { rx: Some(rx), handles, shared, ctx }
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
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-pwalk-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    fn walk(root: &Path, existing: HashMap<String, ExistingFileEntry>) -> Vec<WalkedFile> {
        walk_with(root, existing, false, false)
    }

    fn walk_with(
        root: &Path,
        existing: HashMap<String, ExistingFileEntry>,
        follow_symlinks: bool,
        include_hidden: bool,
    ) -> Vec<WalkedFile> {
        walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            follow_symlinks,
            include_hidden,
            IgnoreSet::compile(&[]).unwrap(),
            Arc::new(existing),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(Mutex::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        )
        .collect()
    }

    fn names(files: &[WalkedFile]) -> Vec<String> {
        let mut n: Vec<String> = files
            .iter()
            .map(|f| Path::new(&f.path).file_name().unwrap().to_string_lossy().into_owned())
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

        let files = walk(&root, HashMap::new());
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

        let files = walk(&root, HashMap::new());

        // Both are yielded, so neither reads as deleted...
        assert_eq!(files.len(), 2, "the bad name is still reported as seen");
        // ...but only the representable one is prepared for insertion, which
        // is what keeps it out of the hasher and out of FTS.
        let prepared: Vec<&WalkedFile> = files.iter().filter(|f| f.record.is_some()).collect();
        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].path.ends_with("plain.txt"));

        let skipped = files.iter().find(|f| f.record.is_none()).unwrap();
        assert!(matches!(skipped.action, FileIndexAction::Skip));
        assert!(skipped.path.contains('\u{FFFD}'), "stored spelling is the lossy one");

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

        let files = walk(&root, HashMap::new());
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
        assert!(walk(&root, HashMap::new()).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unchanged_files_are_never_opened() {
        // The property the whole SMB story rests on: a re-index of an
        // unchanged tree must cost one stat per file and no file opens.
        let root = tmp_tree("skip");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));

        let first = walk(&root, HashMap::new());
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|f| f.action == FileIndexAction::Insert));

        let existing: HashMap<String, ExistingFileEntry> = first
            .iter()
            .map(|f| {
                (f.path.clone(), ExistingFileEntry { mtime: f.record.as_ref().unwrap().mtime })
            })
            .collect();

        let second = walk(&root, existing);
        assert_eq!(second.len(), 2, "unchanged files are still reported as seen");
        for f in &second {
            assert_eq!(f.action, FileIndexAction::Skip);
            assert!(f.record.is_none(), "an unchanged file is never hashed");
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

            let files = walk(&root, HashMap::new());
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
                Arc::new(HashMap::new()),
                Config::default(),
                Arc::new(Registry::default_set()),
                Arc::new(Mutex::new(false)),
                Arc::new(AtomicBool::new(false)),
                4,
            );
            let files: Vec<WalkedFile> = w.by_ref().collect();
            let recorded = !w.unreadable().is_empty();
            let covers = w.unreadable().covers(locked.join("inside.txt").to_str().unwrap());

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

        let files = walk_with(&root, HashMap::new(), true, false);
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
        std::os::unix::fs::symlink(
            root.join("real/target.txt"),
            root.join("links/alias.txt"),
        )
        .unwrap();

        let files = walk(&root, HashMap::new());
        let paths: HashSet<&String> = files.iter().map(|f| &f.path).collect();
        assert_eq!(paths.len(), 1, "both routes report one canonical path");

        let canonical = path_to_db_string(&root.join("real/target.txt").canonicalize().unwrap());
        assert_eq!(*paths.into_iter().next().unwrap(), canonical, "the target, not the alias");
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

        let ignore = IgnoreSet::compile(&[
            "*.tmp".to_string(),
            "node_modules".to_string(),
        ])
        .unwrap();
        let files: Vec<WalkedFile> = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            ignore,
            Arc::new(HashMap::new()),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(Mutex::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        )
        .collect();
        assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

        let files = walk_with(&root, HashMap::new(), false, true);
        assert_eq!(
            names(&files),
            vec![".dotfile", "index.js", "inside.txt", "keep.txt", "keep2.txt", "skip.tmp"],
            "include_hidden with no ignore patterns keeps everything"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hidden_root_is_still_walked() {
        // Roots are chosen explicitly, so the hidden rule must not silence one.
        let base = tmp_tree("hidden-root");
        let root = base.join(".config");
        touch(&root.join("app.conf"));

        let files = walk(&root, HashMap::new());
        assert_eq!(names(&files), vec!["app.conf"]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn stop_flag_ends_the_walk_without_hanging() {
        let root = tmp_tree("stop");
        for i in 0..500 {
            touch(&root.join(format!("f{:04}.txt", i)));
        }

        let stop = Arc::new(Mutex::new(true));
        let files: Vec<WalkedFile> = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            Arc::new(HashMap::new()),
            Config::default(),
            Arc::new(Registry::default_set()),
            stop,
            Arc::new(AtomicBool::new(false)),
            4,
        )
        .collect();

        assert!(files.len() < 500, "an already-stopped walk does not run to completion");
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
            Arc::new(HashMap::new()),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(Mutex::new(false)),
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

        let files: Vec<WalkedFile> = walk_indexable_files(
            &[
                root.to_string_lossy().into_owned(),
                root.join("sub").to_string_lossy().into_owned(),
            ],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            Arc::new(HashMap::new()),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(Mutex::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        )
        .collect();

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
        let expected = names(&walk(&root, HashMap::new()));
        assert_eq!(expected.len(), 40);

        for run in 0..30 {
            assert_eq!(names(&walk(&root, HashMap::new())), expected, "run {}", run);
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
            Arc::new(HashMap::new()),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(Mutex::new(false)),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        let files: Vec<WalkedFile> = w.by_ref().collect();
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
        assert_eq!(thread_count_for(&[root.to_string_lossy().into_owned()]), LOCAL_THREADS);
        fs::remove_dir_all(&root).ok();
    }
}
