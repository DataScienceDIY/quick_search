//! The walk's worker-pool engine: the bounded LIFO job queue, prefetch
//! accounting, and per-worker busy stats. The filesystem logic stays in
//! the parent module.

use super::*;

/// Cap on directories fetched but not yet taken by a worker; each holds its
/// rows live.
const PREFETCH_AHEAD: usize = 64;

#[derive(Default)]
pub(super) struct Queue {
    /// LIFO: a directory's children are processed close in time to the read
    /// that discovered them, which is what the attribute cache rewards.
    pub(super) jobs: Vec<Job>,
    /// Directories discovered but not yet given their rows, and symlink
    /// targets awaiting an mtime lookup. The prefetcher drains both.
    pub(super) needs_rows: Vec<PathBuf>,
    pub(super) needs_alias: Vec<PathBuf>,
    /// Set while the prefetcher is mid-query, holding work that is in neither
    /// list. Part of the end-of-walk proof: see [`Shared::take`].
    pub(super) prefetching: bool,
    /// Fetched directories sitting in `jobs` — what [`PREFETCH_AHEAD`] bounds.
    /// Not derived from `jobs.len()`, which also counts `Job::Files` chunks:
    /// they carry no rows and would starve directory prefetching.
    pub(super) dirs_ready: usize,
    /// Workers currently holding a job — that is, workers that may still push
    /// more. The walk is over when this is zero and `jobs` is empty.
    pub(super) active: usize,
    /// Canonical directories already queued. Collapses overlapping roots and
    /// makes symlink cycles impossible: a cycle must revisit a canonical path.
    ///
    /// Also the record of which directories the walk reached, read by the
    /// caller's vanished-directory sweep.
    pub(super) seen_dirs: HashSet<PathBuf>,
    pub(super) done: bool,
}

impl Queue {
    /// Whether any stage still holds work. The prefetch stage is invisible to
    /// a `jobs`/`active` test, so it has to be named here explicitly.
    pub(super) fn idle(&self) -> bool {
        self.jobs.is_empty()
            && self.needs_rows.is_empty()
            && self.needs_alias.is_empty()
            && !self.prefetching
            && self.active == 0
    }
}

pub(super) struct Shared {
    pub(super) queue: Mutex<Queue>,
    pub(super) idle: Condvar,
    /// Workers currently processing (not parked waiting for work), for
    /// progress display.
    pub(super) stats: WorkerStats,
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
    pub(super) fn take(&self) -> Option<(Job, ActiveJob<'_>)> {
        let mut q = crate::lock_ok(&self.queue);
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
            // Only "nobody anywhere holds work" proves the walk is over — a
            // directory in the prefetch stage still becomes a job.
            if q.idle() {
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

    /// Claim one unit of prefetch work, or `None` once the walk is over.
    ///
    /// Parks while the runnable queue is already `PREFETCH_AHEAD` deep, so
    /// fetched-but-unclaimed rows stay bounded.
    pub(super) fn take_prefetch(&self) -> Option<PrefetchWork> {
        let mut q = crate::lock_ok(&self.queue);
        loop {
            if q.done {
                return None;
            }
            // Aliases carry a single mtime, not rows, so they are never
            // throttled.
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
            q = self
                .idle
                .wait(q)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Publish a prefetched job and clear the in-flight flag under one lock,
    /// keeping the idle test in [`Shared::take`] race-free.
    pub(super) fn finish_prefetch(&self, job: Job) {
        let mut q = crate::lock_ok(&self.queue);
        if matches!(job, Job::Dir(..)) {
            q.dirs_ready += 1;
        }
        q.jobs.push(job);
        q.prefetching = false;
        self.idle.notify_all();
    }

    /// Give the prefetch slot back without producing a job (the query failed).
    pub(super) fn abandon_prefetch(&self) {
        let mut q = crate::lock_ok(&self.queue);
        q.prefetching = false;
        self.idle.notify_all();
    }
}

/// One unit of work for the prefetcher.
pub(super) enum PrefetchWork {
    Dir(PathBuf),
    Alias(PathBuf),
}

/// What a worker discovered while reading a directory.
pub(super) enum Found {
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
    pub(super) fn publish(&self, found: Vec<Found>) {
        let mut q = crate::lock_ok(&self.queue);
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

    pub(super) fn shutdown(&self) {
        let mut q = crate::lock_ok(&self.queue);
        q.done = true;
        self.idle.notify_all();
    }
}

/// Hands the job slot back even if the worker panics or returns early. A
/// stranded count would leave every other worker waiting on a number that
/// never reaches zero.
pub(super) struct ActiveJob<'a> {
    shared: &'a Shared,
    finished: bool,
}

impl ActiveJob<'_> {
    pub(super) fn finish(mut self, found: Vec<Found>) {
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
