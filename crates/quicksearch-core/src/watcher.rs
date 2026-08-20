//! Filesystem watcher with per-directory debouncing.
//!
//! Wraps [`notify`]: events bucket by directory, same-path events within a
//! window coalesce, and a tick loop flushes ready buckets into the caller's
//! [`EventSink`].
//!
//! # Two registration strategies ([`crate::platform::WATCH_ROOTS_RECURSIVELY`])
//!
//! **Per directory (inotify).** inotify has no recursive watch, and
//! `notify`'s emulation walks the tree adding one descriptor per directory
//! with no way to skip subtrees — exhausting `fs.inotify.max_user_watches`
//! on large roots. This module walks the roots itself through
//! [`crate::file_handling::filtered_dirs`] and registers each surviving
//! directory `NonRecursive`; owning recursion means also registering
//! directories created later, which [`register_tree`] does from the event
//! loop.
//!
//! **Per root (`ReadDirectoryChangesW`).** One handle covers the whole
//! subtree, later directories included. Per-directory registration here would
//! be actively harmful: `notify` allocates a 16 KiB buffer plus a directory
//! handle per watch. Pruning cannot save events on this path — they arrive
//! regardless — so the same filters run per event in
//! [`is_event_interesting`].
//!
//! # The cap
//!
//! Registration stops at [`WatcherConfig::max_watched_dirs`]
//! ([`WatchError::TooManyDirectories`]); a kernel refusal is
//! [`WatchError::KernelLimit`]. Both fail the whole watcher — a half-watched
//! root looks live while going silently stale. A single directory the kernel
//! refuses is logged and skipped instead (see [`add_watch`]).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{
    Config as NotifyConfig, ErrorKind as NotifyErrorKind, Event as NotifyEvent, EventKind,
    RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
};

use crate::config::IgnoreSet;
use crate::file_handling::{filtered_dirs, UnreadableDirs};
use crate::platform::path_has_hidden_component_under;

/// Directory budget for live updates; each inotify watch costs roughly 1 KiB
/// of unswappable kernel memory out of the shared per-user
/// `max_user_watches`.
///
/// Applies only where watches are taken per directory; under a per-root
/// recursive watch the count is the number of configured roots.
pub const DEFAULT_MAX_WATCHED_DIRS: usize = 128_000;

/// An event surfaced to the caller after debouncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Create(PathBuf),
    Modify(PathBuf),
    Remove(PathBuf),
    /// Rename where both endpoints arrived in the same notify event. For
    /// split rename halves (From or To only) the watcher emits Remove/Create
    /// instead.
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
}

/// Sink callback. Called on the watcher thread; implementors should keep
/// work short and push heavier operations to their own worker.
pub type EventSink = Arc<dyn Fn(FsEvent) + Send + Sync + 'static>;

/// Which directories are worth a watch descriptor. Mirrors the indexer's
/// walk filters so the watcher never spends a descriptor on a subtree the
/// indexer would discard.
#[derive(Debug, Clone)]
pub struct WatchFilters {
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub ignore: Arc<IgnoreSet>,
}

/// Why live updates are unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    /// The indexed roots hold more directories than the cap allows.
    TooManyDirectories {
        dirs: usize,
        cap: usize,
    },
    /// The kernel refused a watch before our own cap was reached —
    /// `fs.inotify.max_user_watches` is lower than the cap, or other
    /// processes have consumed the shared budget.
    KernelLimit {
        registered: usize,
    },
    /// The kernel's event queue overflowed and events were dropped. Unlike
    /// the two above this says nothing about the watcher's *capacity* — it
    /// keeps working — only that the index is now out of step with the disk
    /// by an unknown amount, so a full run is owed.
    Overflowed,
    Other(String),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchError::TooManyDirectories { dirs, cap } => write!(
                f,
                "the total number of directories to monitor exceeds {} \
                 (stopped counting at {}); live updates are disabled and \
                 changes are picked up by the periodic reindex instead",
                fmt_cap(*cap),
                dirs
            ),
            WatchError::KernelLimit { registered } => write!(
                f,
                "the system watch limit was reached after {} directories{}",
                registered,
                // Only inotify has a tunable the user can raise.
                if cfg!(target_os = "linux") {
                    " (raise fs.inotify.max_user_watches to watch more)"
                } else {
                    ""
                }
            ),
            WatchError::Overflowed => write!(
                f,
                "the system event queue overflowed and changes were missed; \
                 reindexing to catch up"
            ),
            WatchError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for WatchError {}

/// Render the directory cap compactly: the default 128_000 reads "128k".
fn fmt_cap(cap: usize) -> String {
    if cap >= 1000 && cap.is_multiple_of(1000) {
        format!("{}k", cap / 1000)
    } else {
        cap.to_string()
    }
}

/// Tuning for the whole filesystem-event pipeline: the watcher's own
/// per-directory debounce, and the coordinator's queue on the far side of it.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// How long the coordinator's queue must go quiet before it is applied.
    /// An `rm -rf` arrives as a burst; waiting for quiet lets one pass see
    /// the whole set to collapse against.
    pub pending_settle: Duration,
    /// Ceiling on how long [`WatcherConfig::pending_settle`] may hold the
    /// queue back. A steady trickle of changes never goes quiet, and must not
    /// starve.
    pub pending_max_defer: Duration,
    /// Per-directory debounce window. Bursts of events in the same directory
    /// collapse to one flush after this interval of quiet.
    pub throttle_window: Duration,
    /// How often the tick loop inspects the throttle map. Short ticks mean
    /// low latency for first-in-a-burst; long ticks lower CPU at idle.
    pub tick_interval: Duration,
    /// Maximum directories *selected* per tick. A ceiling on the work one
    /// pass takes on; [`FLUSH_BUDGET`] is what bounds how long it may spend
    /// on them, and is the real limit.
    pub max_dirs_per_tick: usize,
    /// When to garbage-collect stale throttle entries (idle > window * N).
    pub prune_max_age_multiplier: u32,
    /// Directory budget; see [`DEFAULT_MAX_WATCHED_DIRS`].
    pub max_watched_dirs: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            pending_settle: Duration::from_secs(2),
            pending_max_defer: Duration::from_secs(30),
            throttle_window: Duration::from_secs(30),
            tick_interval: Duration::from_millis(500),
            max_dirs_per_tick: 512,
            prune_max_age_multiplier: 10,
            max_watched_dirs: DEFAULT_MAX_WATCHED_DIRS,
        }
    }
}

/// The set of registered watches, plus the notify handle that owns them.
///
/// Held behind a mutex because both the registering walk and the event loop
/// add to it. The poll surface ([`Watcher::watched_dirs`],
/// [`Watcher::is_degraded`]) reads atomics instead, so a coordinator poll
/// never blocks behind a large subtree registration.
struct WatchRegistry {
    raw: RecommendedWatcher,
    /// Ordered, not hashed, so [`WatchRegistry::remove_tree`] can take the
    /// subtree as a range instead of scanning every watched directory. The
    /// set reaches `max_watched_dirs` (128k by default) and `rm -rf` deletes
    /// bottom-up, so every directory in a deleted tree hits that path.
    dirs: BTreeSet<PathBuf>,
    cap: usize,
}

/// How long one flush pass may spend handing events to the sink.
///
/// Paired with `max_dirs_per_tick`: the count decides how many directories a
/// pass takes on, this decides when it stops regardless. A backlog then
/// drains at whatever the machine can actually do rather than at a fixed
/// directories-per-second, while one enormous directory still cannot hold the
/// tick loop.
const FLUSH_BUDGET: Duration = Duration::from_millis(50);

/// The mode every `watch()` call uses on this platform. See
/// [`crate::platform::WATCH_ROOTS_RECURSIVELY`] for why it differs.
const WATCH_MODE: RecursiveMode = if crate::platform::WATCH_ROOTS_RECURSIVELY {
    RecursiveMode::Recursive
} else {
    RecursiveMode::NonRecursive
};

impl WatchRegistry {
    /// Register `dir` unless already watched. `Ok(false)` means "nothing to
    /// do" — already watched, or the path vanished mid-walk.
    fn add(&mut self, dir: &Path) -> Result<bool, WatchError> {
        if self.dirs.contains(dir) {
            return Ok(false);
        }
        if self.dirs.len() >= self.cap {
            return Err(WatchError::TooManyDirectories {
                dirs: self.dirs.len(),
                cap: self.cap,
            });
        }
        match self.raw.watch(dir, WATCH_MODE) {
            Ok(()) => {
                self.dirs.insert(dir.to_path_buf());
                Ok(true)
            }
            // notify maps ENOSPC from inotify_add_watch to this.
            Err(e) if matches!(e.kind, NotifyErrorKind::MaxFilesWatch) => {
                Err(WatchError::KernelLimit {
                    registered: self.dirs.len(),
                })
            }
            // Deleted between the walk and the watch call; not an error.
            Err(e) if matches!(e.kind, NotifyErrorKind::PathNotFound) => Ok(false),
            Err(e) => Err(WatchError::Other(format!("watch {}: {}", dir.display(), e))),
        }
    }

    /// Forget `dir` and every watched directory beneath it, returning how
    /// many were dropped. Containment is component-wise, per
    /// [`crate::file_handling::UnreadableDirs::covers`].
    ///
    /// The kernel drops watches for deleted directories on its own; unwatching
    /// anyway keeps notify's internal descriptor map from growing across a long
    /// session of directory churn.
    fn remove_tree(&mut self, dir: &Path) -> usize {
        // Exact, not a heuristic: registration walks top-down, so a watched
        // directory beneath `dir` implies `dir` itself is watched. This early
        // return keeps the O(watched dirs) scan off the per-file Remove path.
        if !self.dirs.contains(dir) {
            return 0;
        }
        // A range from `dir`, stopping at the first entry that is no longer
        // beneath it: descendants sort immediately after their ancestor, so
        // this visits the subtree and one entry more, rather than the whole
        // set. `starts_with` is still the test — it compares whole components,
        // where a raw string prefix would take `/a/bc` for a child of `/a/b`.
        let doomed: Vec<PathBuf> = self
            .dirs
            .range(dir.to_path_buf()..)
            .take_while(|d| d.starts_with(dir))
            .cloned()
            .collect();
        for d in &doomed {
            self.dirs.remove(d);
            let _ = self.raw.unwatch(d);
        }
        doomed.len()
    }
}

/// Register `dir` during startup, propagating only the budget limits.
///
/// A directory the kernel refuses on its own terms — most often one the user
/// cannot read — costs live updates that one directory, not the tree. The
/// budget errors stay fatal: those really do mean the tree can't be covered.
fn add_watch(reg: &mut WatchRegistry, dir: &Path) -> Result<(), WatchError> {
    match reg.add(dir) {
        Ok(_) => Ok(()),
        Err(limit @ (WatchError::TooManyDirectories { .. } | WatchError::KernelLimit { .. })) => {
            Err(limit)
        }
        Err(e) => {
            crate::log_warn!("watcher: {}", e);
            Ok(())
        }
    }
}

/// Handle to a running watcher. Dropping calls [`Self::stop`] implicitly.
pub struct Watcher {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    dir_count: Arc<AtomicUsize>,
    degraded: Degraded,
    /// Held so the notify handle drops on stop, releasing every watch.
    _registry: Arc<Mutex<WatchRegistry>>,
}

/// Set when the watcher runs out of budget *after* starting, carrying which
/// limit was hit so the UI can say the right thing.
type Degraded = Arc<Mutex<Option<WatchError>>>;

impl Watcher {
    /// Register watches for every indexable directory under `roots` and
    /// start the debouncing loop.
    ///
    /// Registration walks each root, so this takes proportional time on
    /// large trees — callers run it off their main loop.
    pub fn start<I, P>(
        roots: I,
        filters: WatchFilters,
        config: WatcherConfig,
        sink: EventSink,
    ) -> Result<Self, WatchError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let (tx, rx) = mpsc::channel::<NotifyEvent>();
        let degraded: Degraded = Arc::new(Mutex::new(None));
        let degraded_cb = degraded.clone();
        let raw = RecommendedWatcher::new(
            move |res: notify::Result<NotifyEvent>| match res {
                Ok(ev) => {
                    // An overflow of the kernel's own event queue arrives here
                    // — on the *Ok* arm, as `EventKind::Other` with the rescan
                    // flag and no paths at all — so the error arm below never
                    // sees it and the per-path loop downstream iterates zero
                    // times. Left alone it is silent data loss: an arbitrary
                    // set of creates, modifies and removes never reaches the
                    // index while the watcher goes on reporting itself
                    // healthy. It is reported rather than repaired here
                    // because the events are simply gone; only a full run can
                    // find out what happened.
                    if ev.need_rescan() {
                        let mut slot = crate::lock_ok(&degraded_cb);
                        slot.get_or_insert(WatchError::Overflowed);
                        return;
                    }
                    // A closed receiver just means the watcher was stopped; ignore.
                    let _ = tx.send(ev);
                }
                Err(e) => {
                    // The limit can also be hit asynchronously, when notify
                    // reacts to a directory appearing.
                    if matches!(e.kind, NotifyErrorKind::MaxFilesWatch) {
                        let mut slot = crate::lock_ok(&degraded_cb);
                        slot.get_or_insert(WatchError::KernelLimit { registered: 0 });
                    }
                    crate::log_warn!("watcher: notify error: {}", e);
                }
            },
            NotifyConfig::default(),
        )
        .map_err(|e| WatchError::Other(format!("create watcher: {}", e)))?;

        let registry = Arc::new(Mutex::new(WatchRegistry {
            raw,
            dirs: BTreeSet::new(),
            cap: config.max_watched_dirs,
        }));

        // Any error here drops `registry`, which drops the notify handle and
        // releases every watch already taken — the all-or-nothing guarantee.
        // Release is asynchronous: notify's Drop signals its event-loop
        // thread, which then closes the inotify fd. Measured at ~50 ms for a
        // few hundred watches, so a caller that immediately retries may
        // briefly see the old descriptors still charged to the user's quota.
        let roots: Vec<PathBuf> = roots
            .into_iter()
            .map(|r| r.as_ref().to_path_buf())
            .collect();

        {
            let failures = UnreadableDirs::default();
            let mut reg = crate::lock_ok(&registry);
            for root in &roots {
                if crate::platform::WATCH_ROOTS_RECURSIVELY {
                    // One watch covers the subtree. Ignored and hidden
                    // subtrees can't be skipped here — nothing is registered
                    // for them to skip — so their events are dropped on
                    // arrival instead, in `is_event_interesting`.
                    add_watch(&mut reg, root)?;
                    continue;
                }
                let Some(root_str) = root.to_str() else {
                    crate::log_warn!("watcher: skipping non-UTF-8 root {}", root.display());
                    continue;
                };
                for entry in filtered_dirs(
                    root_str,
                    filters.follow_symlinks,
                    filters.include_hidden,
                    &filters.ignore,
                    &failures,
                ) {
                    add_watch(&mut reg, entry.path())?;
                }
            }
            if reg.dirs.is_empty() {
                return Err(WatchError::Other("no roots could be watched".into()));
            }
        }

        let dir_count = Arc::new(AtomicUsize::new(crate::lock_ok(&registry).dirs.len()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let ctx = LoopCtx {
            sink,
            config,
            stop: stop_flag.clone(),
            registry: registry.clone(),
            filters,
            roots,
            dir_count: dir_count.clone(),
            degraded: degraded.clone(),
        };
        let handle = thread::spawn(move || run_loop(rx, ctx));

        Ok(Self {
            stop_flag,
            handle: Some(handle),
            dir_count,
            degraded,
            _registry: registry,
        })
    }

    /// How many directories currently hold a watch descriptor.
    pub fn watched_dirs(&self) -> usize {
        self.dir_count.load(Ordering::Relaxed)
    }

    /// Which limit the watcher hit after starting, if any. `Some` means it
    /// can no longer see the whole tree; the coordinator polls this and
    /// falls back to periodic rescans.
    pub fn degraded_reason(&self) -> Option<WatchError> {
        crate::lock_ok(&self.degraded).clone()
    }

    /// Whether the watcher ran out of budget after starting.
    pub fn is_degraded(&self) -> bool {
        crate::lock_ok(&self.degraded).is_some()
    }

    /// Forget the recorded reason, so a later one can take its place.
    ///
    /// For [`WatchError::Overflowed`] only, and the distinction is the whole
    /// point of the method: the other two reasons are *standing* — the watch
    /// budget does not come back — while an overflow is a one-shot "you missed
    /// some" from a watcher that is still delivering. Left in place it would
    /// re-trigger on every coordinator tick and, worse, mask a real
    /// [`WatchError::KernelLimit`] arriving afterwards, because the callback
    /// records with `get_or_insert`.
    pub fn clear_degraded(&self) {
        *crate::lock_ok(&self.degraded) = None;
    }

    /// Signal the background thread to stop and wait for it to join. Safe to
    /// call multiple times.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hand-written because `RecommendedWatcher` is not `Debug`.
impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("watched_dirs", &self.watched_dirs())
            .field("degraded", &self.is_degraded())
            .finish()
    }
}

/// Everything the event loop needs.
struct LoopCtx {
    sink: EventSink,
    config: WatcherConfig,
    stop: Arc<AtomicBool>,
    registry: Arc<Mutex<WatchRegistry>>,
    filters: WatchFilters,
    /// The configured roots, needed to judge "hidden" relative to them: a
    /// root may itself sit under a hidden directory (`~/.config/app`, or
    /// anything below `%LOCALAPPDATA%`), and the walk keeps such a root.
    roots: Vec<PathBuf>,
    dir_count: Arc<AtomicUsize>,
    degraded: Degraded,
}

/// A queued operation, deduplicated per path within a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedOp {
    Create,
    Modify,
    Remove,
}

#[derive(Debug)]
struct DirThrottleEntry {
    /// Last time this entry's queue was flushed (or when the entry was
    /// created as leading-edge).
    record_time: Instant,
    /// Per-path pending op. Same path seen twice in a window keeps only the
    /// latest op — coalescing a rename-as-create+modify spam into one event.
    queue: HashMap<PathBuf, QueuedOp>,
    /// If true, the next tick flushes regardless of window age. Set for the
    /// first event in a previously-idle directory so it reacts fast.
    immediate: bool,
}

fn run_loop(rx: mpsc::Receiver<NotifyEvent>, ctx: LoopCtx) {
    let mut throttle: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    // Pending rename halves keyed by cookie are not supported by notify 6.x's
    // high-level API uniformly across backends; when From/To aren't bundled
    // we emit Remove/Create which remains correct semantically.
    let prune_interval_ticks = 20u32;
    let mut tick_counter: u32 = 0;

    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        // Drain incoming events. Block briefly to avoid spinning when idle.
        let deadline = Instant::now() + ctx.config.tick_interval;
        loop {
            if ctx.stop.load(Ordering::Relaxed) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(ev) => handle_notify_event(&ev, &mut throttle, &ctx),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        flush_ready(&mut throttle, &ctx.sink, &ctx.config);

        // Periodic GC of abandoned throttle entries.
        tick_counter = tick_counter.wrapping_add(1);
        if tick_counter.is_multiple_of(prune_interval_ticks) {
            let max_age = ctx
                .config
                .throttle_window
                .saturating_mul(ctx.config.prune_max_age_multiplier);
            prune_stale(&mut throttle, max_age);
        }
    }
}

/// Register a directory that appeared after startup, and everything under
/// it — a whole tree can be moved in with a single event.
fn register_tree(ctx: &LoopCtx, root: &Path) {
    let Some(root_str) = root.to_str() else {
        return;
    };
    let failures = UnreadableDirs::default();
    let mut reg = crate::lock_ok(&ctx.registry);
    for entry in filtered_dirs(
        root_str,
        ctx.filters.follow_symlinks,
        ctx.filters.include_hidden,
        &ctx.filters.ignore,
        &failures,
    ) {
        // Same policy as startup: only the budget limits are fatal.
        if let Err(limit) = add_watch(&mut reg, entry.path()) {
            // A partially watched tree would look live while going silently
            // stale; stop and let the coordinator tear us down. Keep the
            // first reason.
            let mut slot = crate::lock_ok(&ctx.degraded);
            slot.get_or_insert(limit);
            break;
        }
    }
    ctx.dir_count.store(reg.dirs.len(), Ordering::Relaxed);
}

/// Register `path` if it is a directory the indexer would keep.
///
/// Uses the same path-based filters as [`crate::incremental::apply_fs_event`],
/// so we never hold a descriptor for a directory whose events would be
/// discarded on arrival.
fn watch_if_new_dir(ctx: &LoopCtx, path: &Path) {
    // A recursive root watch already covers anything created beneath it.
    if crate::platform::WATCH_ROOTS_RECURSIVELY {
        return;
    }
    // Files are reported through their parent's watch; only directories
    // need one of their own. `symlink_metadata` rather than `is_dir` so a
    // symlink to a directory is judged as the link it is: when following is
    // off the walk will not descend it, and watching it would spend
    // descriptors reporting events for a subtree that is never indexed. When
    // following is on it is a directory as far as everything else is
    // concerned, so fall back to the followed answer.
    let is_dir = match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => ctx.filters.follow_symlinks && path.is_dir(),
        Ok(md) => md.is_dir(),
        // Raced with a delete, or unreadable: nothing to register.
        Err(_) => return,
    };
    if !is_dir {
        return;
    }
    if !is_event_interesting(ctx, path) {
        return;
    }
    register_tree(ctx, path);
}

/// Drop watches for a directory that went away. Cheap no-op for files.
fn unwatch_tree(ctx: &LoopCtx, path: &Path) {
    // Nothing per-directory was ever registered, and the root's own watch
    // must outlive a deleted subdirectory.
    if crate::platform::WATCH_ROOTS_RECURSIVELY {
        return;
    }
    let mut reg = crate::lock_ok(&ctx.registry);
    if reg.remove_tree(path) > 0 {
        ctx.dir_count.store(reg.dirs.len(), Ordering::Relaxed);
    }
}

/// Whether an event for `path` is worth queueing at all.
///
/// The same predicate the walk applies, so the watcher and the indexer agree
/// on which subtrees exist. Under a recursive root watch it is the *only*
/// thing keeping `node_modules` churn out of the throttle map.
fn is_event_interesting(ctx: &LoopCtx, path: &Path) -> bool {
    // A path the index cannot spell, screened here because this is the one
    // gate every `FsEvent` passes through. Such a file is never indexed, so
    // there is no row for a Create to update and none for a Remove to delete —
    // but the incremental side keys on `path_to_db_string`, which is lossy, so
    // letting the event through means acting on whichever *different* file
    // happens to own the lossy spelling. The event carries no information and
    // every use of it is a mistake.
    if path.to_str().is_none() {
        return false;
    }
    if ctx.filters.ignore.matches_path(path) {
        return false;
    }
    if !ctx.filters.include_hidden && path_has_hidden_component_under(path, &ctx.roots) {
        return false;
    }
    true
}

fn handle_notify_event(
    ev: &NotifyEvent,
    throttle: &mut HashMap<PathBuf, DirThrottleEntry>,
    ctx: &LoopCtx,
) {
    // Rename events that carry both sides are emitted directly — they
    // can't be coalesced with same-dir creates/modifies meaningfully.
    if let EventKind::Modify(notify::event::ModifyKind::Name(kind)) = ev.kind {
        if matches!(kind, notify::event::RenameMode::Both) && ev.paths.len() == 2 {
            // A rename is only uninteresting when *both* ends are: moving a
            // file out of an ignored directory into a watched one is a real
            // Create, and the reverse is a real Remove. `apply_fs_event`
            // re-checks each end, so passing the pair through is safe.
            if !is_event_interesting(ctx, &ev.paths[0]) && !is_event_interesting(ctx, &ev.paths[1])
            {
                return;
            }
            unwatch_tree(ctx, &ev.paths[0]);
            watch_if_new_dir(ctx, &ev.paths[1]);
            (ctx.sink)(FsEvent::Rename {
                from: ev.paths[0].clone(),
                to: ev.paths[1].clone(),
            });
            return;
        }
        // Split renames (From alone, To alone) degrade to Remove/Create.
    }

    for p in &ev.paths {
        if !is_event_interesting(ctx, p) {
            continue;
        }
        let op = match ev.kind {
            EventKind::Create(_) => QueuedOp::Create,
            EventKind::Remove(_) => QueuedOp::Remove,
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
                QueuedOp::Remove
            }
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To)) => {
                QueuedOp::Create
            }
            EventKind::Modify(_) => QueuedOp::Modify,
            _ => continue,
        };
        // Keep the watch set in step with the tree before debouncing: a
        // directory created now may be populated before the throttle
        // window expires, and those child events need its watch in place.
        match op {
            QueuedOp::Create => watch_if_new_dir(ctx, p),
            QueuedOp::Remove => unwatch_tree(ctx, p),
            QueuedOp::Modify => {}
        }
        enqueue(throttle, p.clone(), op);
    }
}

fn enqueue(throttle: &mut HashMap<PathBuf, DirThrottleEntry>, path: PathBuf, op: QueuedOp) {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| path.clone());
    let entry = throttle.entry(dir).or_insert_with(|| DirThrottleEntry {
        record_time: Instant::now(),
        queue: HashMap::new(),
        immediate: true,
    });
    // Coalesce: Remove after Create → drop both. Modify after Modify → one Modify.
    match (op, entry.queue.get(&path).copied()) {
        (QueuedOp::Remove, Some(QueuedOp::Create)) => {
            entry.queue.remove(&path);
        }
        _ => {
            entry.queue.insert(path, op);
        }
    }
}

fn flush_ready(
    throttle: &mut HashMap<PathBuf, DirThrottleEntry>,
    sink: &EventSink,
    config: &WatcherConfig,
) {
    let now = Instant::now();
    let mut ready: Vec<PathBuf> = Vec::new();
    for (dir, entry) in throttle.iter() {
        let age = now.saturating_duration_since(entry.record_time);
        if entry.immediate || (!entry.queue.is_empty() && age >= config.throttle_window) {
            ready.push(dir.clone());
            if ready.len() >= config.max_dirs_per_tick {
                break;
            }
        }
    }
    // The count above bounds how many directories are *selected*; this bounds
    // how long draining them may take, which is the thing that actually
    // matters. Directory queues differ by orders of magnitude, so a fixed
    // count is either too small after a large delete — at 64 per 500 ms tick
    // the drain rate is 128 directories a second regardless of backlog, and a
    // 20k-directory unpack takes minutes to reach the index — or too large for
    // one deep directory.
    let deadline = now + FLUSH_BUDGET;
    for dir in ready {
        if Instant::now() >= deadline {
            break;
        }
        if let Some(entry) = throttle.get_mut(&dir) {
            let drained: Vec<(PathBuf, QueuedOp)> = entry.queue.drain().collect();
            entry.immediate = false;
            entry.record_time = now;
            for (path, op) in drained {
                let ev = match op {
                    QueuedOp::Create => FsEvent::Create(path),
                    QueuedOp::Modify => FsEvent::Modify(path),
                    QueuedOp::Remove => FsEvent::Remove(path),
                };
                sink(ev);
            }
        }
    }
}

fn prune_stale(throttle: &mut HashMap<PathBuf, DirThrottleEntry>, max_age: Duration) {
    let now = Instant::now();
    throttle.retain(|_, entry| {
        !entry.queue.is_empty()
            || entry.immediate
            || now.saturating_duration_since(entry.record_time) < max_age
    });
    // `retain` keeps the table sized for the busiest moment this map has ever
    // seen; a single unpack burst would otherwise hold that memory for days.
    if throttle.is_empty() {
        throttle.shrink_to_fit();
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
