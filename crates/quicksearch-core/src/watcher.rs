//! Filesystem watcher with per-directory debouncing over [`notify`].
//!
//! inotify has no recursive watch, so each surviving directory is registered
//! `NonRecursive`; `ReadDirectoryChangesW` takes one recursive handle per
//! root, filtering per event. If the watch budget cannot cover everything,
//! the whole watcher fails: a half-watched root looks live while going
//! silently stale.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{
    Config as NotifyConfig, ErrorKind as NotifyErrorKind, Event as NotifyEvent, EventKind,
    RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
};

use crate::config::IgnoreSet;
use crate::file_handling::{filtered_dirs, UnreadableDirs};
use crate::platform::path_has_hidden_component_under;

/// Directory budget for live updates; each inotify watch costs roughly 1 KiB
/// of unswappable kernel memory out of the shared per-user `max_user_watches`.
pub const DEFAULT_MAX_WATCHED_DIRS: usize = 128_000;

/// An event surfaced to the caller after debouncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Create(PathBuf),
    Modify(PathBuf),
    Remove(PathBuf),
    /// Rename with both endpoints in one notify event; split halves emit
    /// Remove/Create instead.
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
}

/// Sink callback. Called on the watcher thread; implementors should keep
/// work short and push heavier operations to their own worker.
pub type EventSink = Arc<dyn Fn(FsEvent) + Send + Sync + 'static>;

/// Directory filters; mirror the indexer's walk filters.
#[derive(Debug, Clone)]
pub struct WatchFilters {
    pub include_hidden: bool,
    pub ignore: Arc<IgnoreSet>,
}

/// Why live updates are unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    TooManyDirectories {
        dirs: usize,
        cap: usize,
    },
    /// The kernel refused a watch before our own cap: `max_user_watches` is
    /// lower, or other processes have consumed the shared budget.
    KernelLimit {
        registered: usize,
    },
    /// The kernel's event queue (on Windows, the root's change buffer)
    /// overflowed and events were dropped. A one-shot, not a capacity problem
    /// like the other reasons, but the index is out of step by an unknown
    /// amount, and so is the watch set: a lost directory `Create` leaves the
    /// new directory unwatched, a lost `Delete` keeps the old one registered.
    /// The coordinator answers with a full run and a fresh registration, from
    /// which a limit that still stands surfaces as itself; during a storm, at
    /// most once in a few minutes.
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

fn fmt_cap(cap: usize) -> String {
    if cap >= 1000 && cap.is_multiple_of(1000) {
        format!("{}k", cap / 1000)
    } else {
        cap.to_string()
    }
}

/// Tuning for the whole filesystem-event pipeline. The watcher forwards
/// events as they arrive; the coordinator's queue coalesces them by path and
/// decides when to apply them.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// How long the coordinator's queue must go quiet before it is applied.
    pub pending_settle: Duration,
    /// Ceiling on how long `pending_settle` may hold the queue back.
    pub pending_max_defer: Duration,
    /// Directory budget; see [`DEFAULT_MAX_WATCHED_DIRS`].
    pub max_watched_dirs: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            pending_settle: Duration::from_secs(2),
            pending_max_defer: Duration::from_secs(30),
            max_watched_dirs: DEFAULT_MAX_WATCHED_DIRS,
        }
    }
}

/// The set of registered watches, plus the notify handle that owns them.
struct WatchRegistry {
    raw: RecommendedWatcher,
    /// Ordered so [`WatchRegistry::remove_tree`] can range-scan a subtree.
    dirs: BTreeSet<PathBuf>,
    cap: usize,
}


const WATCH_MODE: RecursiveMode = if crate::platform::WATCH_ROOTS_RECURSIVELY {
    RecursiveMode::Recursive
} else {
    RecursiveMode::NonRecursive
};

impl WatchRegistry {
    /// Register `dir` unless already watched. `Ok(false)` means "nothing to
    /// do": already watched, or the path vanished mid-walk.
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
    /// many were dropped. The kernel drops watches for deleted directories on
    /// its own; unwatching anyway keeps notify's internal descriptor map from
    /// growing across a long session of directory churn.
    fn remove_tree(&mut self, dir: &Path) -> usize {
        // Registration walks top-down, so a watched directory beneath `dir`
        // implies `dir` itself is watched.
        if !self.dirs.contains(dir) {
            return 0;
        }
        // Descendants sort immediately after their ancestor. `starts_with`
        // compares whole components; a raw string prefix would take `/a/bc`
        // for a child of `/a/b`.
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

/// Register `dir` during startup: the budget limits stay fatal (the tree
/// can't be covered), while a directory the kernel refuses on its own terms
/// costs live updates that one directory, not the tree.
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
    /// The loop's own channel: `None` ends it. The notify handle holds the
    /// other sender, so the channel never closes while the loop runs.
    stop_tx: mpsc::Sender<Option<NotifyEvent>>,
    handle: Option<JoinHandle<()>>,
    /// The flag [`Self::start`] was handed; [`Self::stop`] sets it.
    cancel: Arc<AtomicBool>,
    dir_count: Arc<AtomicUsize>,
    degraded: Degraded,
    /// Held so the notify handle drops on stop, releasing every watch.
    _registry: Arc<Mutex<WatchRegistry>>,
}

/// Set when the watcher runs out of budget *after* starting.
type Degraded = Arc<Mutex<Option<WatchError>>>;

/// `Err` once `cancel` is set. Registration checks it per directory, so a
/// superseded start stops walking, and the dropped registry releases every
/// watch it took instead of holding them against the user's quota.
fn not_cancelled(cancel: &AtomicBool) -> Result<(), WatchError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(WatchError::Other("watch registration cancelled".into()));
    }
    Ok(())
}

impl Watcher {
    /// Register watches for every indexable directory under `roots` and start
    /// the debouncing loop. Registration walks each root, so callers run it
    /// off their main loop, and can abandon it by setting `cancel`: the walk
    /// stops at its next directory (its next root on a recursive backend)
    /// and returns `Err`. The running watcher keeps the flag, so setting it
    /// later also ends the loop at its next event.
    pub fn start(
        roots: Vec<PathBuf>,
        filters: WatchFilters,
        config: WatcherConfig,
        sink: EventSink,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, WatchError> {
        let (tx, rx) = mpsc::channel::<Option<NotifyEvent>>();
        let stop_tx = tx.clone();
        let degraded: Degraded = Arc::new(Mutex::new(None));
        let degraded_cb = degraded.clone();
        let raw = RecommendedWatcher::new(
            move |res: notify::Result<NotifyEvent>| match res {
                Ok(ev) => {
                    // inotify, FSEvents and the patched Windows backend (a
                    // zero-byte completion; see vendor/notify) report overflow
                    // on the *Ok* arm, as `EventKind::Other` with the rescan
                    // flag and no paths, so the error arm never sees it.
                    if ev.need_rescan() {
                        crate::lock_ok(&degraded_cb).get_or_insert(WatchError::Overflowed);
                        return;
                    }
                    let _ = tx.send(Some(ev));
                }
                Err(e) => {
                    // The limit can also be hit asynchronously, when notify
                    // reacts to a directory appearing.
                    if matches!(e.kind, NotifyErrorKind::MaxFilesWatch) {
                        crate::lock_ok(&degraded_cb)
                            .get_or_insert(WatchError::KernelLimit { registered: 0 });
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
        // releases every watch already taken: the all-or-nothing guarantee.
        // Release is asynchronous, so an immediate retry may briefly see the
        // old descriptors still charged to the user's quota.
        {
            let failures = UnreadableDirs::default();
            let mut reg = crate::lock_ok(&registry);
            for root in &roots {
                let before = reg.dirs.len();
                if crate::platform::WATCH_ROOTS_RECURSIVELY {
                    // Ignored and hidden subtrees have nothing registered to
                    // skip; their events are dropped in `is_event_interesting`.
                    not_cancelled(&cancel)?;
                    add_watch(&mut reg, root)?;
                } else {
                    let Some(root_str) = root.to_str() else {
                        crate::log_warn!("watcher: skipping non-UTF-8 root {}", root.display());
                        continue;
                    };
                    for entry in
                        filtered_dirs(root_str, filters.include_hidden, &filters.ignore, &failures)
                    {
                        not_cancelled(&cancel)?;
                        add_watch(&mut reg, entry.path())?;
                    }
                }
                // Missing, or refused: this root stays silent while the
                // status reads Active on the other roots' strength.
                if reg.dirs.len() == before {
                    crate::log_warn!("watcher: nothing under {} could be watched", root.display());
                }
            }
            if reg.dirs.is_empty() {
                return Err(WatchError::Other("no roots could be watched".into()));
            }
        }

        let dir_count = Arc::new(AtomicUsize::new(crate::lock_ok(&registry).dirs.len()));
        let ctx = LoopCtx {
            sink,
            registry: registry.clone(),
            filters,
            roots,
            dir_count: dir_count.clone(),
            degraded: degraded.clone(),
            cancel: cancel.clone(),
        };
        let handle = thread::spawn(move || run_loop(rx, ctx));

        Ok(Self {
            stop_tx,
            handle: Some(handle),
            cancel,
            dir_count,
            degraded,
            _registry: registry,
        })
    }

    /// How many directories currently hold a watch descriptor.
    pub fn watched_dirs(&self) -> usize {
        self.dir_count.load(Ordering::Relaxed)
    }

    /// What went wrong after starting, if anything. The coordinator polls
    /// this: an overflow re-registers, a limit falls back to periodic rescans.
    pub fn degraded_reason(&self) -> Option<WatchError> {
        crate::lock_ok(&self.degraded).clone()
    }

    /// Record `reason` as if the watcher had hit it after starting.
    #[cfg(test)]
    pub(crate) fn force_degraded(&self, reason: WatchError) {
        *crate::lock_ok(&self.degraded) = Some(reason);
    }

    /// Stop the background thread and join it; safe to call multiple times.
    /// The cancel flag is set first: the `None` queues behind every event
    /// already sent, and the loop, seeing the flag, ends at the next one
    /// instead of working through the backlog (or a `register_tree` walk).
    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.stop_tx.send(None);
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

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("watched_dirs", &self.watched_dirs())
            .field("degraded", &self.degraded_reason())
            .finish()
    }
}

struct LoopCtx {
    sink: EventSink,
    registry: Arc<Mutex<WatchRegistry>>,
    filters: WatchFilters,
    /// Configured roots, in the spelling events arrive in (through the link,
    /// for a root that is one). Hidden and ignored names are judged below
    /// them, since a root may itself sit under one (`~/.config/app`).
    roots: Vec<PathBuf>,
    dir_count: Arc<AtomicUsize>,
    degraded: Degraded,
    /// The flag [`Watcher::start`] was handed: once set, the loop and any
    /// [`register_tree`] walk end.
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Create,
    Modify,
    Remove,
}

/// Forward each interesting event to the sink as it arrives, blocking in
/// between; `None` or the cancel flag is [`Watcher::stop`]. Nothing is held
/// back here: the coordinator's queue coalesces by path and waits for
/// `pending_settle` (at most `pending_max_defer`). A per-directory throttle
/// here used to hold a directory's second event for 30 s and woke this
/// thread twice a second while idle.
fn run_loop(rx: mpsc::Receiver<Option<NotifyEvent>>, ctx: LoopCtx) {
    while let Ok(Some(ev)) = rx.recv() {
        if ctx.cancel.load(Ordering::Relaxed) {
            break;
        }
        handle_notify_event(&ev, &ctx);
    }
}

/// Register a directory that appeared after startup, and everything under
/// it: a whole tree can be moved in with a single event. A stop ends the
/// walk at the next directory.
fn register_tree(ctx: &LoopCtx, root: &Path) {
    let Some(root_str) = root.to_str() else {
        return;
    };
    let failures = UnreadableDirs::default();
    let mut reg = crate::lock_ok(&ctx.registry);
    for entry in filtered_dirs(
        root_str,
        ctx.filters.include_hidden,
        &ctx.filters.ignore,
        &failures,
    ) {
        if ctx.cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Err(limit) = add_watch(&mut reg, entry.path()) {
            // A partially watched tree would look live while going silently
            // stale; stop and let the coordinator tear us down.
            crate::lock_ok(&ctx.degraded).get_or_insert(limit);
            break;
        }
    }
    ctx.dir_count.store(reg.dirs.len(), Ordering::Relaxed);
}

/// Register `path` if it is a directory the indexer would keep.
fn watch_if_new_dir(ctx: &LoopCtx, path: &Path) {
    // A recursive root watch already covers anything created beneath it.
    if crate::platform::WATCH_ROOTS_RECURSIVELY {
        return;
    }
    // Files are reported through their parent's watch; only directories need
    // one of their own. A symlink is judged as the link it is, never a
    // directory: the walk does not descend it, so watching it would spend
    // descriptors on a subtree that is never indexed.
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if md.is_dir() && is_event_interesting(ctx, path) {
        register_tree(ctx, path);
    }
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

/// Whether an event for `path` is worth queueing at all: the same predicate
/// the walk applies, names tested only below the root containing `path`, so
/// a root inside `node_modules` keeps its events. Under a recursive root
/// watch it alone keeps `node_modules` churn out of the coordinator's queue.
/// A path the index cannot spell is dropped (see `path_to_db_string`).
fn is_event_interesting(ctx: &LoopCtx, path: &Path) -> bool {
    path.to_str().is_some()
        && !ctx.filters.ignore.matches_below(path, &ctx.roots)
        && (ctx.filters.include_hidden || !path_has_hidden_component_under(path, &ctx.roots))
}

/// What `kind` means for `path`, one of its event's paths, or `None` for
/// nothing to apply. Split renames degrade to Create/Remove; notify 6.x does
/// not bundle halves by cookie uniformly across backends. A rename that names
/// no end (FSEvents reports every rename as `Name(Any)`) is judged by the
/// disk: a path that is there is where the name went, a `Create`, so a
/// directory moved in is walked; one that is not is where it left.
fn op_for(kind: &EventKind, path: &Path) -> Option<Op> {
    match kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            Some(Op::Create)
        }
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            Some(Op::Remove)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::Other)) => {
            Some(match std::fs::symlink_metadata(path) {
                Ok(_) => Op::Create,
                Err(_) => Op::Remove,
            })
        }
        EventKind::Modify(_) => Some(Op::Modify),
        _ => None,
    }
}

fn handle_notify_event(ev: &NotifyEvent, ctx: &LoopCtx) {
    let both = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
    if ev.kind == both && ev.paths.len() == 2 {
        // A rename is only uninteresting when *both* ends are: out of an
        // ignored directory into a watched one is a real Create.
        if !is_event_interesting(ctx, &ev.paths[0]) && !is_event_interesting(ctx, &ev.paths[1]) {
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

    for p in &ev.paths {
        // First: for most kinds a match, and for those that mean nothing (a
        // `Close(Write)` after every write) the end of the event.
        let Some(op) = op_for(&ev.kind, p) else {
            return;
        };
        if !is_event_interesting(ctx, p) {
            continue;
        }
        // Keep the watch set in step before the event is applied: a
        // directory created now may be populated at once and needs its watch.
        let event = match op {
            Op::Create => {
                watch_if_new_dir(ctx, p);
                FsEvent::Create(p.clone())
            }
            Op::Remove => {
                unwatch_tree(ctx, p);
                FsEvent::Remove(p.clone())
            }
            Op::Modify => FsEvent::Modify(p.clone()),
        };
        (ctx.sink)(event);
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
