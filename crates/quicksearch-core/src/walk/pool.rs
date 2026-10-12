//! The walk's worker-pool engine: bounded LIFO job queue, prefetch
//! accounting, per-worker busy stats. Filesystem logic stays in the parent.

use super::*;

/// Cap on directories fetched but not yet taken; each holds its rows live.
const PREFETCH_AHEAD: usize = 64;

#[derive(Default)]
pub(super) struct Queue {
    /// LIFO: a directory's children are processed close in time to the read
    /// that discovered them, which the attribute cache rewards.
    pub(super) jobs: Vec<Job>,
    pub(super) needs_rows: Vec<PathBuf>,
    /// Prefetcher mid-query, holding work in neither list; part of the
    /// end-of-walk proof in [`Shared::take`]. A prefetcher that exits early
    /// leaves it set, and its [`EndWalk`] ends the walk instead.
    pub(super) prefetching: bool,
    /// What [`PREFETCH_AHEAD`] bounds. Not `jobs.len()`: that also counts
    /// `Job::Files` chunks, which hold no directory's rows and would starve
    /// prefetching.
    pub(super) dirs_ready: usize,
    pub(super) active: usize,
    /// Canonical directories already queued: collapses overlapping roots and
    /// tells the vanished sweep which directories the walk reached. Queued,
    /// not read: only an [`exhausted`](Queue::exhausted) walk read them all.
    pub(super) seen_dirs: HashSet<PathBuf>,
    pub(super) done: bool,
    /// Set only by the idle proof, never by a stop or a shutdown: the walk ran
    /// out of work, so every queued directory was read or recorded unreadable.
    pub(super) exhausted: bool,
}

impl Queue {
    pub(super) fn idle(&self) -> bool {
        self.jobs.is_empty() && self.needs_rows.is_empty() && !self.prefetching && self.active == 0
    }
}

pub(super) struct Shared {
    pub(super) queue: Mutex<Queue>,
    pub(super) idle: Condvar,
    pub(super) stats: WorkerStats,
}

/// Decrements the busy count however the worker leaves, stop or panic.
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

    pub(crate) fn enter(&self) -> BusyGuard<'_> {
        self.busy.fetch_add(1, Ordering::Relaxed);
        BusyGuard(&self.busy)
    }

    pub fn active(&self) -> usize {
        self.busy.load(Ordering::Relaxed).min(self.total)
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

impl Shared {
    /// Claim a job. Returns `None` only when the queue is empty and no worker
    /// holds a job: at that instant nobody is left who could push more, so the
    /// walk is provably finished.
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
            // A directory in the prefetch stage still becomes a job, hence
            // the full idle() test.
            if q.idle() {
                q.done = true;
                q.exhausted = true;
                self.idle.notify_all();
                return None;
            }
            q = crate::wait_ok(&self.idle, q);
        }
    }

    /// Claim a directory whose rows to fetch, or `None` once the walk is
    /// over. Parks while the runnable queue is already `PREFETCH_AHEAD` deep.
    pub(super) fn take_prefetch(&self) -> Option<PathBuf> {
        let mut q = crate::lock_ok(&self.queue);
        loop {
            if q.done {
                return None;
            }
            if q.dirs_ready < PREFETCH_AHEAD {
                if let Some(dir) = q.needs_rows.pop() {
                    q.prefetching = true;
                    return Some(dir);
                }
            }
            if q.idle() {
                q.done = true;
                q.exhausted = true;
                self.idle.notify_all();
                return None;
            }
            q = crate::wait_ok(&self.idle, q);
        }
    }

    /// Publish a prefetched directory and clear the in-flight flag under one
    /// lock, keeping the idle test in [`Shared::take`] race-free.
    pub(super) fn finish_prefetch(&self, dir: PathBuf, rows: DirRows) {
        let mut q = crate::lock_ok(&self.queue);
        q.dirs_ready += 1;
        q.jobs.push(Job::Dir(dir, rows));
        q.prefetching = false;
        self.idle.notify_all();
    }
}

/// What a worker discovered while reading a directory.
pub(super) enum Found {
    Dir(PathBuf),
    /// Overflow files from the directory just read, each carrying its row.
    Files(Vec<PendingFile>),
}

impl Shared {
    /// Push discovered work and give the job slot back under a single lock,
    /// which makes the idle test in [`Shared::take`] a proof, not a race: a
    /// worker that popped the last job but has not yet published its
    /// children must never look idle.
    pub(super) fn publish(&self, found: Vec<Found>) {
        let mut q = crate::lock_ok(&self.queue);
        for item in found {
            match item {
                Found::Dir(dir) => {
                    if q.seen_dirs.insert(dir.clone()) {
                        q.needs_rows.push(dir);
                    }
                }
                Found::Files(files) => q.jobs.push(Job::Files(files)),
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

/// Held by the prefetcher for its whole life: however it leaves, a failure,
/// an early return or a panic, the walk ends there and the parked workers are
/// released. A no-op at a normal end, where the idle proof has already set
/// `done`.
pub(super) struct EndWalk<'a>(pub(super) &'a Shared);

impl Drop for EndWalk<'_> {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// Hands the job slot back even if the worker panics or returns early; a
/// stranded count would strand every other worker too.
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
