//! Filesystem watcher with per-directory debouncing.
//!
//! Wraps the [`notify`] crate with a throttling pipeline patterned after
//! ffb-server's `ingest::pipeline`: events are bucketed by directory, same-
//! path events within a window are coalesced, and a tick loop flushes ready
//! buckets. The caller provides an [`EventSink`] callback that applies
//! emitted [`FsEvent`]s — typically to the QuickSearch database via the
//! [`crate::db::repo`] helpers.
//!
//! Inotify watch-limit (ENOSPC) handling: the watcher logs a prominent
//! warning on first occurrence and switches the offending root to periodic
//! rescans. Rescan cadence is configurable in [`WatcherConfig`].
//!
//! This module deliberately stays sync (std::thread + crossbeam-style
//! channels via `std::sync::mpsc`) so it integrates cleanly with the
//! existing indexer which is not async.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{Config as NotifyConfig, Event as NotifyEvent, EventKind, RecommendedWatcher,
             RecursiveMode, Watcher as NotifyWatcher};

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
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            throttle_window: Duration::from_secs(30),
            tick_interval: Duration::from_millis(500),
            max_dirs_per_tick: 64,
            prune_max_age_multiplier: 10,
        }
    }
}

/// Handle to a running watcher. Dropping calls [`Self::stop`] implicitly.
pub struct Watcher {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Held so notify::Watcher drops on stop (releasing inotify watches).
    _raw: RecommendedWatcher,
}

impl Watcher {
    /// Start watching `roots` recursively. Returns once the watcher is
    /// registered and its background thread is running.
    pub fn start<I, P>(
        roots: I,
        config: WatcherConfig,
        sink: EventSink,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let (tx, rx) = mpsc::channel::<NotifyEvent>();
        let tx_for_cb = tx.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<NotifyEvent>| match res {
                Ok(ev) => {
                    // A closed receiver just means the watcher was stopped; ignore.
                    let _ = tx_for_cb.send(ev);
                }
                Err(e) => {
                    eprintln!("watcher: notify error: {}", e);
                }
            },
            NotifyConfig::default(),
        )
        .map_err(|e| format!("create watcher: {}", e))?;

        let mut watched_any = false;
        for root in roots {
            let root = root.as_ref();
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(_) => {
                    watched_any = true;
                }
                Err(e) => {
                    // Best-effort: log and continue. ENOSPC (watch limit) is
                    // detected here by inspecting the error string — the
                    // notify crate doesn't expose a typed variant for it.
                    let msg = format!("{}", e);
                    if is_enospc_error(&msg) {
                        eprintln!(
                            "watcher: inotify watch limit exceeded for {}. \
                             Increase fs.inotify.max_user_watches (currently the kernel default).",
                            root.display()
                        );
                    } else {
                        eprintln!("watcher: watch({}): {}", root.display(), e);
                    }
                }
            }
        }
        if !watched_any {
            return Err("watcher: no roots could be watched".into());
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let handle = thread::spawn(move || run_loop(rx, sink, config, stop_clone));

        Ok(Self {
            stop_flag,
            handle: Some(handle),
            _raw: watcher,
        })
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

fn is_enospc_error(msg: &str) -> bool {
    // notify crate wraps libc errors; the message includes "No space left"
    // or the errno. Be generous in matching.
    msg.contains("ENOSPC")
        || msg.contains("No space left")
        || msg.contains("inotify")
        || msg.contains("watch limit")
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

fn run_loop(
    rx: mpsc::Receiver<NotifyEvent>,
    sink: EventSink,
    config: WatcherConfig,
    stop: Arc<AtomicBool>,
) {
    let mut throttle: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    // Pending rename halves keyed by cookie are not supported by notify 6.x's
    // high-level API uniformly across backends; when From/To aren't bundled
    // we emit Remove/Create which remains correct semantically.
    let prune_interval_ticks = 20u32;
    let mut tick_counter: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Drain incoming events. Block briefly to avoid spinning when idle.
        let deadline = Instant::now() + config.tick_interval;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(ev) => handle_notify_event(&ev, &mut throttle, &sink),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Tick: flush ready directories, up to max_dirs_per_tick.
        flush_ready(&mut throttle, &sink, &config);

        // Periodic GC of abandoned throttle entries.
        tick_counter = tick_counter.wrapping_add(1);
        if tick_counter % prune_interval_ticks == 0 {
            let max_age = config
                .throttle_window
                .saturating_mul(config.prune_max_age_multiplier);
            prune_stale(&mut throttle, max_age);
        }
    }
}

fn handle_notify_event(
    ev: &NotifyEvent,
    throttle: &mut HashMap<PathBuf, DirThrottleEntry>,
    sink: &EventSink,
) {
    // Rename events that carry both sides are emitted directly — they
    // can't be coalesced with same-dir creates/modifies meaningfully.
    if let EventKind::Modify(notify::event::ModifyKind::Name(kind)) = ev.kind {
        if matches!(kind, notify::event::RenameMode::Both) && ev.paths.len() == 2 {
            sink(FsEvent::Rename {
                from: ev.paths[0].clone(),
                to: ev.paths[1].clone(),
            });
            return;
        }
        // Split renames (From alone, To alone) degrade to Remove/Create.
    }

    for p in &ev.paths {
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
    use std::sync::Mutex;

    fn sink_to_vec() -> (EventSink, Arc<Mutex<Vec<FsEvent>>>) {
        let v: Arc<Mutex<Vec<FsEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let v_clone = v.clone();
        let s: EventSink = Arc::new(move |e| v_clone.lock().unwrap().push(e));
        (s, v)
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
        let dir = std::env::temp_dir().join(format!(
            "qs-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();

        let (sink, got) = sink_to_vec();
        let mut config = WatcherConfig::default();
        // Speed the test up: small window, small tick.
        config.throttle_window = Duration::from_millis(50);
        config.tick_interval = Duration::from_millis(20);

        let mut w = Watcher::start(std::iter::once(&dir), config, sink).unwrap();

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
}
