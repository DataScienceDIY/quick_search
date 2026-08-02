//! Filesystem watcher with per-directory debouncing.
//!
//! Wraps the [`notify`] crate with a throttling pipeline patterned after
//! ffb-server's `ingest::pipeline`: events are bucketed by directory, same-
//! path events within a window are coalesced, and a tick loop flushes ready
//! buckets. The caller provides an [`EventSink`] callback that applies
//! emitted [`FsEvent`]s — typically to the QuickSearch database via the
//! [`crate::db::repo`] helpers.
//!
//! # Two registration strategies
//!
//! Which one applies is [`crate::platform::WATCH_ROOTS_RECURSIVELY`].
//!
//! **Per directory (inotify).** inotify has no recursive watch: one watch
//! descriptor covers exactly one directory's entries.
//! `RecursiveMode::Recursive` is emulated inside `notify` by walking the tree
//! and adding one watch per directory — with no way to skip subtrees. That
//! spent descriptors on `.git`, `node_modules`, and hidden directories whose
//! events the indexer then discarded, and exhausted
//! `fs.inotify.max_user_watches` on large roots. So this module walks the
//! roots itself through [`crate::file_handling::filtered_dirs`] — the same
//! pruning the indexer uses — and registers each surviving directory
//! `NonRecursive`. Taking over recursion means also registering directories
//! created later, which [`register_tree`] does from the event loop.
//!
//! **Per root (`ReadDirectoryChangesW`).** One handle covers the whole
//! subtree, including directories created later, so the walk and the
//! per-directory bookkeeping are skipped entirely. Registering per directory
//! here would be actively harmful rather than merely wasteful: `notify`
//! allocates a 16 KiB buffer inline per watch plus a directory handle, so a
//! large tree would ask for gigabytes of buffers and tens of thousands of
//! handles. The saving that pruning bought on inotify is unavailable — the
//! events arrive whether or not we want them — so the same filters run on the
//! event path instead, in [`is_event_interesting`].
//!
//! # The cap
//!
//! Registration stops at [`WatcherConfig::max_watched_dirs`] and reports
//! [`WatchError::TooManyDirectories`]; a kernel refusal reports
//! [`WatchError::KernelLimit`]. Both are all-or-nothing — the whole
//! watcher fails and no root gets live updates, leaving the coordinator's
//! periodic reindex as the only refresh path. Partial registration is
//! worse than none, because a half-watched root looks live while going
//! silently stale.
//!
//! Only those two are fatal. A single directory the kernel refuses for its
//! own reasons — an unreadable folder, one deleted mid-walk — is logged and
//! skipped: it costs its own events, not the tree's, and letting it abort
//! registration would report that incidental failure as the reason live
//! updates are off. See [`add_watch`].
//!
//! Neither limit is reachable under the per-root strategy, where the watch
//! count is the number of configured roots.
//!
//! This module deliberately stays sync (std::thread + crossbeam-style
//! channels via `std::sync::mpsc`) so it integrates cleanly with the
//! existing indexer which is not async.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{Config as NotifyConfig, ErrorKind as NotifyErrorKind, Event as NotifyEvent, EventKind,
             RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};

use crate::config::IgnoreSet;
use crate::file_handling::{filtered_dirs, UnreadableDirs};
use crate::platform::path_has_hidden_component_under;

/// Directory budget for live updates. Past this, watching costs more than
/// it returns: the kernel's per-user `max_user_watches` is a shared
/// resource, and a tree this size is cheaper to rescan on a timer than to
/// track. Roughly 1 KiB of unswappable kernel memory per watch.
///
/// Applies only where watches are taken per directory. Under a per-root
/// recursive watch the count is the number of configured roots, so the cap is
/// unreachable by design rather than by generosity.
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
    Rename { from: PathBuf, to: PathBuf },
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
    TooManyDirectories { dirs: usize, cap: usize },
    /// The kernel refused a watch before our own cap was reached —
    /// `fs.inotify.max_user_watches` is lower than the cap, or other
    /// processes have consumed the shared budget.
    KernelLimit { registered: usize },
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
                // Only inotify has a tunable the user can actually raise;
                // pointing a Windows user at a sysctl would be nonsense.
                if cfg!(target_os = "linux") {
                    " (raise fs.inotify.max_user_watches to watch more)"
                } else {
                    ""
                }
            ),
            WatchError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for WatchError {}

/// Render the directory cap compactly: the default 128_000 reads "128k".
fn fmt_cap(cap: usize) -> String {
    if cap >= 1000 && cap % 1000 == 0 {
        format!("{}k", cap / 1000)
    } else {
        cap.to_string()
    }
}

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Per-directory debounce window. Bursts of events in the same directory
    /// collapse to one flush after this interval of quiet.
    pub throttle_window: Duration,
    /// How often the tick loop inspects the throttle map. Short ticks mean
    /// low latency for first-in-a-burst; long ticks lower CPU at idle.
    pub tick_interval: Duration,
    /// Maximum directories processed per tick. Caps the time spent in a
    /// single flush pass so long backlogs don't monopolize the thread.
    pub max_dirs_per_tick: usize,
    /// When to garbage-collect stale throttle entries (idle > window * N).
    pub prune_max_age_multiplier: u32,
    /// Directory budget; see [`DEFAULT_MAX_WATCHED_DIRS`].
    pub max_watched_dirs: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            throttle_window: Duration::from_secs(30),
            tick_interval: Duration::from_millis(500),
            max_dirs_per_tick: 64,
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
    dirs: HashSet<PathBuf>,
    cap: usize,
}

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
    /// many were dropped.
    ///
    /// `Path::starts_with` compares whole components, so `/a/bc` is not
    /// treated as living under `/a/b`. The kernel drops watches for deleted
    /// directories on its own; unwatching anyway keeps notify's internal
    /// descriptor map from growing across a long session of directory churn.
    fn remove_tree(&mut self, dir: &Path) -> usize {
        let doomed: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|d| d.starts_with(dir))
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
/// cannot read, which the walk still hands us because its *parent* was
/// readable — costs live updates that one directory, not the tree. Treating
/// it as fatal used to abort the whole registration and report "permission
/// denied" as the reason live updates were off, hiding the real, actionable
/// limit from anyone whose roots also exceeded the directory budget. The
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
/// limit was hit so the UI can say the right thing. A plain flag would force
/// the coordinator to guess.
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
                    // A closed receiver just means the watcher was stopped; ignore.
                    let _ = tx.send(ev);
                }
                Err(e) => {
                    // The limit can also be hit asynchronously, when notify
                    // reacts to a directory appearing.
                    if matches!(e.kind, NotifyErrorKind::MaxFilesWatch) {
                        let mut slot = degraded_cb.lock().unwrap();
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
            dirs: HashSet::new(),
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
            let mut reg = registry.lock().unwrap();
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

        let dir_count = Arc::new(AtomicUsize::new(registry.lock().unwrap().dirs.len()));
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
        self.degraded.lock().unwrap().clone()
    }

    /// Whether the watcher ran out of budget after starting.
    pub fn is_degraded(&self) -> bool {
        self.degraded.lock().unwrap().is_some()
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

/// Hand-written because `RecommendedWatcher` is not `Debug`, and the watch
/// set behind a mutex is not worth locking to print.
impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("watched_dirs", &self.watched_dirs())
            .field("degraded", &self.is_degraded())
            .finish()
    }
}

/// Everything the event loop needs, bundled to keep signatures readable.
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

/// A queued event, deduplicated per path within a window.
#[derive(Debug, Clone)]
struct QueuedEvent {
    op: QueuedOp,
}

#[derive(Debug, Clone, Copy)]
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
    queue: HashMap<PathBuf, QueuedEvent>,
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

        // Tick: flush ready directories, up to max_dirs_per_tick.
        flush_ready(&mut throttle, &ctx.sink, &ctx.config);

        // Periodic GC of abandoned throttle entries.
        tick_counter = tick_counter.wrapping_add(1);
        if tick_counter % prune_interval_ticks == 0 {
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
    let mut reg = ctx.registry.lock().unwrap();
    for entry in filtered_dirs(
        root_str,
        ctx.filters.follow_symlinks,
        ctx.filters.include_hidden,
        &ctx.filters.ignore,
        &failures,
    ) {
        // Same policy as startup: only the budget limits are fatal.
        if let Err(limit) = add_watch(&mut reg, entry.path()) {
            // Live updates can no longer cover the tree. Stop here and let
            // the coordinator tear us down; a partially watched tree would
            // look live while going silently stale. Keep the first reason —
            // later ones are consequences of the same exhaustion.
            let mut slot = ctx.degraded.lock().unwrap();
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
    // need one of their own.
    if !path.is_dir() {
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
    let mut reg = ctx.registry.lock().unwrap();
    if reg.remove_tree(path) > 0 {
        ctx.dir_count.store(reg.dirs.len(), Ordering::Relaxed);
    }
}

/// Whether an event for `path` is worth queueing at all.
///
/// The same predicate the walk applies, so the watcher and the indexer agree
/// on which subtrees exist. Where a platform registers watches per directory
/// this is mostly redundant — those subtrees were never watched — but under a
/// recursive root watch it is the *only* thing keeping `node_modules` churn
/// out of the throttle map. Applying it on both keeps one code path and drops
/// ignored events a debounce window earlier than
/// [`crate::incremental::apply_fs_event`] would.
fn is_event_interesting(ctx: &LoopCtx, path: &Path) -> bool {
    if ctx.filters.ignore.matches_path(path) {
        return false;
    }
    if !ctx.filters.include_hidden
        && path_has_hidden_component_under(path, &ctx.roots)
    {
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
            if !is_event_interesting(ctx, &ev.paths[0])
                && !is_event_interesting(ctx, &ev.paths[1])
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

fn enqueue(
    throttle: &mut HashMap<PathBuf, DirThrottleEntry>,
    path: PathBuf,
    op: QueuedOp,
) {
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
    let entry = throttle
        .entry(dir)
        .or_insert_with(|| DirThrottleEntry {
            record_time: Instant::now(),
            queue: HashMap::new(),
            immediate: true,
        });
    // Coalesce: Remove after Create → drop both. Modify after Modify → one Modify.
    match (op, entry.queue.get(&path).map(|q| q.op)) {
        (QueuedOp::Remove, Some(QueuedOp::Create)) => {
            entry.queue.remove(&path);
        }
        _ => {
            entry
                .queue
                .insert(path, QueuedEvent { op });
        }
    }
}

fn flush_ready(
    throttle: &mut HashMap<PathBuf, DirThrottleEntry>,
    sink: &EventSink,
    config: &WatcherConfig,
) {
    let now = Instant::now();
    // Collect ready dir keys first, up to max_dirs_per_tick. Copying keys
    // avoids borrow conflicts when we mutate entries below.
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
    for dir in ready {
        if let Some(entry) = throttle.get_mut(&dir) {
            let drained: Vec<(PathBuf, QueuedOp)> = entry
                .queue
                .drain()
                .map(|(p, q)| (p, q.op))
                .collect();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn sink_to_vec() -> (EventSink, Arc<StdMutex<Vec<FsEvent>>>) {
        let v: Arc<StdMutex<Vec<FsEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let v_clone = v.clone();
        let s: EventSink = Arc::new(move |e| v_clone.lock().unwrap().push(e));
        (s, v)
    }

    /// Filters matching the shipped defaults: hidden excluded, `.git` and
    /// `node_modules` ignored.
    fn default_filters() -> WatchFilters {
        WatchFilters {
            include_hidden: false,
            follow_symlinks: false,
            ignore: Arc::new(
                IgnoreSet::compile(&[".git".to_string(), "node_modules".to_string()]).unwrap(),
            ),
        }
    }

    fn fast_config() -> WatcherConfig {
        WatcherConfig {
            throttle_window: Duration::from_millis(50),
            tick_interval: Duration::from_millis(20),
            ..WatcherConfig::default()
        }
    }

    /// Unique temp directory; the repo has no `tempfile` dev-dependency.
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qs-watch-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enqueue_create_then_remove_cancels() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        let p = PathBuf::from("/tmp/a.txt");
        enqueue(&mut map, p.clone(), QueuedOp::Create);
        enqueue(&mut map, p.clone(), QueuedOp::Remove);
        let entry = map.get(p.parent().unwrap()).unwrap();
        assert!(entry.queue.is_empty(), "Create then Remove should cancel");
    }

    #[test]
    fn modify_after_modify_is_one() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        let p = PathBuf::from("/tmp/a.txt");
        enqueue(&mut map, p.clone(), QueuedOp::Modify);
        enqueue(&mut map, p.clone(), QueuedOp::Modify);
        let entry = map.get(p.parent().unwrap()).unwrap();
        assert_eq!(entry.queue.len(), 1);
    }

    #[test]
    fn flush_ready_leading_edge_fires_immediately() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        enqueue(&mut map, PathBuf::from("/tmp/a.txt"), QueuedOp::Create);
        let (sink, got) = sink_to_vec();
        let config = WatcherConfig::default();
        flush_ready(&mut map, &sink, &config);
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], FsEvent::Create(_)));
    }

    #[test]
    fn flush_ready_respects_max_dirs_per_tick() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        for i in 0..10 {
            enqueue(&mut map, PathBuf::from(format!("/dir{}/a", i)), QueuedOp::Create);
        }
        let (sink, got) = sink_to_vec();
        let mut config = WatcherConfig::default();
        config.max_dirs_per_tick = 3;
        flush_ready(&mut map, &sink, &config);
        // Each dir contributes one event because each entry has one path.
        assert_eq!(got.lock().unwrap().len(), 3);
    }

    #[test]
    fn prune_stale_drops_empty_old_entries() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        map.insert(
            PathBuf::from("/tmp"),
            DirThrottleEntry {
                record_time: Instant::now() - Duration::from_secs(3600),
                queue: HashMap::new(),
                immediate: false,
            },
        );
        prune_stale(&mut map, Duration::from_secs(1));
        assert!(map.is_empty());
    }

    #[test]
    fn prune_stale_keeps_active_entries() {
        let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
        let mut queue = HashMap::new();
        queue.insert(
            PathBuf::from("/tmp/a"),
            QueuedEvent { op: QueuedOp::Modify },
        );
        map.insert(
            PathBuf::from("/tmp"),
            DirThrottleEntry {
                record_time: Instant::now() - Duration::from_secs(3600),
                queue,
                immediate: false,
            },
        );
        prune_stale(&mut map, Duration::from_secs(1));
        assert_eq!(map.len(), 1);
    }

    /// End-to-end: create files in a tempdir, verify the watcher surfaces
    /// events via the sink. Short timeouts keep the test fast; if it becomes
    /// flaky on slow CI, increase the sleeps.
    #[test]
    fn e2e_create_modify_remove_surfaces() {
        let dir = tmp_dir("e2e");
        let (sink, got) = sink_to_vec();

        let mut w =
            Watcher::start(std::iter::once(&dir), default_filters(), fast_config(), sink).unwrap();

        let f = dir.join("hello.txt");
        std::fs::write(&f, "hi").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&f, "hi again").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::remove_file(&f).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        w.stop();

        let events = got.lock().unwrap().clone();
        // Expect at least one Create (or Modify, depending on backend) and one Remove.
        // Some platforms emit Create+Modify for `write`.
        let has_create_or_modify = events
            .iter()
            .any(|e| matches!(e, FsEvent::Create(_) | FsEvent::Modify(_)));
        let has_remove = events.iter().any(|e| matches!(e, FsEvent::Remove(_)));
        assert!(has_create_or_modify, "no create/modify in {:?}", events);
        assert!(has_remove, "no remove in {:?}", events);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the rewrite: descriptors are not spent on
    /// directories the indexer would discard.
    #[test]
    fn ignored_and_hidden_dirs_are_not_registered() {
        let dir = tmp_dir("filter");
        for sub in ["keep", "keep/nested", ".git", ".git/objects", "node_modules",
                    "node_modules/pkg", ".hidden"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }

        let w =
            Watcher::start(std::iter::once(&dir), default_filters(), fast_config(), sink_to_vec().0)
                .unwrap();

        // root + keep + keep/nested. The 4 ignored/hidden dirs cost nothing.
        assert_eq!(
            w.watched_dirs(),
            3,
            "expected root, keep, keep/nested only"
        );
        drop(w);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_hidden_registers_dotted_dirs() {
        let dir = tmp_dir("hidden");
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();

        let filters = WatchFilters {
            include_hidden: true,
            ..default_filters()
        };
        let w = Watcher::start(std::iter::once(&dir), filters, fast_config(), sink_to_vec().0)
            .unwrap();

        assert_eq!(w.watched_dirs(), 2, "root + .hidden");
        drop(w);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Exceeding the cap fails the whole registration — no root gets
    /// partial live updates.
    #[test]
    fn exceeding_the_cap_fails_all_or_nothing() {
        let dir = tmp_dir("cap");
        for sub in ["a", "b", "c", "d"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }

        let config = WatcherConfig {
            max_watched_dirs: 2,
            ..fast_config()
        };
        let err = Watcher::start(std::iter::once(&dir), default_filters(), config, sink_to_vec().0)
            .unwrap_err();

        assert_eq!(
            err,
            WatchError::TooManyDirectories { dirs: 2, cap: 2 },
            "5 directories under a cap of 2 must fail"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One folder the user cannot read is not a reason to switch live
    /// updates off for every root — it costs its own events only.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_directory_is_skipped_not_fatal() {
        let dir = tmp_dir("denied");
        std::fs::create_dir_all(dir.join("open")).unwrap();
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        crate::platform::deny_read(&locked).unwrap();

        let started =
            Watcher::start(std::iter::once(&dir), default_filters(), fast_config(), sink_to_vec().0);
        crate::platform::restore_read(&locked).ok();
        let w = started.expect("an unreadable directory must not fail the watcher");

        assert_eq!(w.watched_dirs(), 2, "root + open; locked is skipped");
        assert!(!w.is_degraded());
        drop(w);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bug this guards: an unreadable directory aborted registration
    /// with its own error, so a tree that was *also* over the cap reported
    /// "permission denied" as the reason live updates were off — hiding the
    /// one limit the user can actually act on.
    #[test]
    #[cfg(unix)]
    fn the_cap_outranks_an_unreadable_directory() {
        let dir = tmp_dir("denied-cap");
        for sub in ["a", "b", "c", "d"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        crate::platform::deny_read(&locked).unwrap();

        let config = WatcherConfig {
            max_watched_dirs: 2,
            ..fast_config()
        };
        let started =
            Watcher::start(std::iter::once(&dir), default_filters(), config, sink_to_vec().0);
        crate::platform::restore_read(&locked).ok();

        // Whichever order the walk visits them in, the cap is what stops us.
        assert_eq!(
            started.unwrap_err(),
            WatchError::TooManyDirectories { dirs: 2, cap: 2 },
            "the reported reason must be the cap, not the unreadable folder"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tree_inside_the_cap_registers() {
        let dir = tmp_dir("undercap");
        std::fs::create_dir_all(dir.join("a")).unwrap();

        let config = WatcherConfig {
            max_watched_dirs: 2,
            ..fast_config()
        };
        let w = Watcher::start(std::iter::once(&dir), default_filters(), config, sink_to_vec().0)
            .unwrap();
        assert_eq!(w.watched_dirs(), 2);
        assert!(!w.is_degraded());
        drop(w);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression guard for taking over recursion from notify: a directory
    /// created after startup must get its own watch, or its contents are
    /// invisible to live updates.
    #[test]
    fn a_directory_created_after_start_is_watched() {
        let dir = tmp_dir("newdir");
        let (sink, got) = sink_to_vec();
        let mut w =
            Watcher::start(std::iter::once(&dir), default_filters(), fast_config(), sink).unwrap();
        assert_eq!(w.watched_dirs(), 1, "only the root to begin with");

        let sub = dir.join("later");
        std::fs::create_dir(&sub).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(w.watched_dirs(), 2, "the new directory must be watched");

        // A file inside it is only visible if that watch really landed.
        let f = sub.join("inside.txt");
        std::fs::write(&f, "hi").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        w.stop();

        let events = got.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| matches!(
                e,
                FsEvent::Create(p) | FsEvent::Modify(p) if p == &f
            )),
            "no event for the file in the new directory: {:?}",
            events
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A whole tree can arrive in one event; every directory in it needs a
    /// watch, not just the top.
    #[test]
    fn a_moved_in_tree_registers_every_directory() {
        let staging = tmp_dir("staging");
        let dir = tmp_dir("movein");
        std::fs::create_dir_all(staging.join("tree/one/two")).unwrap();

        let mut w = Watcher::start(
            std::iter::once(&dir),
            default_filters(),
            fast_config(),
            sink_to_vec().0,
        )
        .unwrap();
        assert_eq!(w.watched_dirs(), 1);

        std::fs::rename(staging.join("tree"), dir.join("tree")).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(w.watched_dirs(), 4, "root + tree + tree/one + tree/one/two");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn a_removed_directory_releases_its_watches() {
        let dir = tmp_dir("rmdir");
        std::fs::create_dir_all(dir.join("gone/deep")).unwrap();

        let mut w = Watcher::start(
            std::iter::once(&dir),
            default_filters(),
            fast_config(),
            sink_to_vec().0,
        )
        .unwrap();
        assert_eq!(w.watched_dirs(), 3, "root + gone + gone/deep");

        std::fs::remove_dir_all(dir.join("gone")).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(w.watched_dirs(), 1, "descendants released with the parent");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `Path::starts_with` compares components, so a sibling sharing a name
    /// prefix must survive its neighbour's removal.
    #[test]
    fn remove_tree_does_not_match_name_prefixes() {
        let dir = tmp_dir("prefix");
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::create_dir_all(dir.join("bc")).unwrap();

        let mut w = Watcher::start(
            std::iter::once(&dir),
            default_filters(),
            fast_config(),
            sink_to_vec().0,
        )
        .unwrap();
        assert_eq!(w.watched_dirs(), 3);

        std::fs::remove_dir_all(dir.join("b")).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(w.watched_dirs(), 2, "root + bc; /a/bc is not under /a/b");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Crossing the cap at runtime (rather than at startup) must record
    /// *which* limit was hit, so the coordinator doesn't have to guess.
    #[test]
    fn running_out_of_budget_later_records_the_reason() {
        let dir = tmp_dir("degrade");
        let config = WatcherConfig {
            max_watched_dirs: 2,
            ..fast_config()
        };
        let mut w =
            Watcher::start(std::iter::once(&dir), default_filters(), config, sink_to_vec().0)
                .unwrap();
        assert!(!w.is_degraded(), "one directory is under the cap of 2");

        // Two more directories: the first fits, the second cannot.
        std::fs::create_dir(dir.join("fits")).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::create_dir(dir.join("overflows")).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(
            w.degraded_reason(),
            Some(WatchError::TooManyDirectories { dirs: 2, cap: 2 }),
            "the cap, not a kernel limit"
        );
        assert_eq!(w.watched_dirs(), 2, "never registers past the cap");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_created_ignored_directory_is_not_watched() {
        let dir = tmp_dir("newignored");
        let mut w = Watcher::start(
            std::iter::once(&dir),
            default_filters(),
            fast_config(),
            sink_to_vec().0,
        )
        .unwrap();

        std::fs::create_dir(dir.join("node_modules")).unwrap();
        std::fs::create_dir(dir.join(".cache")).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(w.watched_dirs(), 1, "neither ignored nor hidden dirs count");
        w.stop();
        std::fs::remove_dir_all(&dir).ok();
    }
}
