//! The indexing coordinator: the one object binaries construct.
//!
//! Owns the [`IndexingService`] (full runs), the filesystem [`Watcher`]
//! (change events), a periodic-reindex scheduler, and the mode state
//! machine:
//!
//! - **Auto** — watcher running; events apply incrementally between full
//!   runs; a full reindex triggers whenever `last_full_index` is older
//!   than the configured interval (or has never happened).
//! - **ManualStopped** — watcher off, pending events dropped, nothing runs
//!   until the user acts.
//! - **ManualRunning** — one user-forced full run; returns to
//!   `ManualStopped` when it finishes. (A forced run in Auto stays Auto.)
//!
//! Single-writer guarantee: incremental writes are deferred while a full
//! run is active — the coordinator's tick simply does nothing until the
//! `IndexingService` reports idle, then drains its queue. Overflowing the
//! queue (>100k pending paths) collapses into one full run instead.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet};
use crate::db;
use crate::extract::Registry;
use crate::incremental::apply_fs_event;
use crate::indexing::{ConfigChange, IndexingService, IndexingStatus};
use crate::watcher::{FsEvent, WatchError, WatchFilters, Watcher, WatcherConfig};

/// Pending-event ceiling; beyond this a full run is cheaper than replay.
const PENDING_OVERFLOW: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    Auto,
    ManualStopped,
    ManualRunning,
}

/// Whether live updates are running, and if not, why.
///
/// A failed watcher used to be a printed line the GUI never saw, so roots
/// silently fell back to the periodic reindex with no user-visible sign.
/// This is the state the UI acts on; the log line is the detail behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherStatus {
    /// Not running: manual mode, or no roots configured.
    Off,
    /// Registration in flight — it walks every root, so this can last
    /// minutes on large or networked trees.
    Starting,
    /// Live updates active over `dirs` watched directories.
    Active { dirs: usize },
    /// Live updates unavailable; the periodic reindex is the only refresh.
    Disabled { reason: WatchError },
}

/// One-stop poll surface for the GUI.
#[derive(Debug, Clone)]
pub struct IndexerState {
    pub mode: IndexMode,
    pub activity: IndexingStatus,
    /// Unix seconds of the last completed full run, if any.
    pub last_full_index: Option<u64>,
    /// Watcher events waiting to be applied.
    pub queued_events: usize,
    /// Live-update health; see [`WatcherStatus`].
    pub watcher: WatcherStatus,
}

enum CoordCmd {
    SetMode(IndexMode),
    ReindexNow,
    ConfigChanged(Config),
    RebuildIndex,
    ClearIndex,
    Shutdown,
}

pub struct IndexCoordinator {
    cmd_tx: mpsc::Sender<CoordCmd>,
    indexing: Arc<IndexingService>,
    shared: Arc<Mutex<Shared>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
}

/// State mirrored out of the coordinator thread for `state()`.
struct Shared {
    mode: IndexMode,
    last_full_index: Option<u64>,
    queued_events: usize,
    watcher: WatcherStatus,
}

impl IndexCoordinator {
    pub fn start(config: Config) -> Result<IndexCoordinator, String> {
        Self::start_with_watcher_config(config, WatcherConfig::default())
    }

    /// [`Self::start`] with explicit watcher debounce tuning (tests use
    /// short windows; the default 30 s throttle is right for real use).
    pub fn start_with_watcher_config(
        config: Config,
        watcher_config: WatcherConfig,
    ) -> Result<IndexCoordinator, String> {
        let indexing = Arc::new(IndexingService::new());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        let initial_mode = if config.indexing.auto_index {
            IndexMode::Auto
        } else {
            IndexMode::ManualStopped
        };
        let shared = Arc::new(Mutex::new(Shared {
            mode: initial_mode,
            last_full_index: None,
            queued_events: 0,
            watcher: WatcherStatus::Off,
        }));

        let mut inner = Inner {
            config,
            indexing: indexing.clone(),
            shared: shared.clone(),
            event_tx,
            event_rx,
            watcher: None,
            watcher_config,
            watcher_rx: None,
            watcher_gen: 0,
            pending: HashMap::new(),
            needs_full_run: false,
            saw_running: false,
            write_conn: None,
            ignore: Arc::new(IgnoreSet::compile(&[]).expect("empty ignore set")),
            registry: Registry::default_set(),
            mode: initial_mode,
        };
        inner.reload_filters()?;
        inner.refresh_last_full_index();

        let handle = std::thread::Builder::new()
            .name("qs-coordinator".into())
            .spawn(move || inner.run(cmd_rx))
            .map_err(|e| format!("spawn coordinator: {}", e))?;

        Ok(IndexCoordinator {
            cmd_tx,
            indexing,
            shared,
            handle: Mutex::new(Some(handle)),
            stopped: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> IndexerState {
        let shared = self.shared.lock().unwrap();
        IndexerState {
            mode: shared.mode,
            activity: self.indexing.get_status(),
            last_full_index: shared.last_full_index,
            queued_events: shared.queued_events,
            watcher: shared.watcher.clone(),
        }
    }

    pub fn set_mode(&self, mode: IndexMode) {
        let _ = self.cmd_tx.send(CoordCmd::SetMode(mode));
    }

    /// Force a full reindex now. In manual mode the coordinator enters
    /// `ManualRunning` and returns to `ManualStopped` when done.
    pub fn reindex_now(&self) {
        let _ = self.cmd_tx.send(CoordCmd::ReindexNow);
    }

    /// Hand the coordinator an edited config. Watcher and paths follow on
    /// the next tick; rebuild decisions stay with the caller (see
    /// [`crate::config::diff_actions`] and [`Self::rebuild_index`]).
    pub fn apply_config(&self, config: Config) {
        let _ = self.cmd_tx.send(CoordCmd::ConfigChanged(config));
    }

    /// Delete the index and rebuild from scratch (user confirmed).
    pub fn rebuild_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::RebuildIndex);
    }

    /// Delete the index WITHOUT rebuilding (user confirmed). Indexing
    /// drops to manual-stopped so automatic mode doesn't immediately
    /// resurrect what the user just deleted.
    pub fn clear_index(&self) {
        let _ = self.cmd_tx.send(CoordCmd::ClearIndex);
    }

    /// Compare `config` against what the index was built with. Read-only.
    pub fn check_config_validation(&self, config: &Config) -> Result<Option<Vec<ConfigChange>>, String> {
        let db = config.resolved_database_path();
        let roots = joined_roots(config);
        self.indexing
            .check_config_validation(&db.to_string_lossy(), config, &roots)
    }

    /// Stop the watcher, any running index pass, and the coordinator
    /// thread. Idempotent; usable from a signal handler through an Arc.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.cmd_tx.send(CoordCmd::Shutdown);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for IndexCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Newline-joined resolved roots — the shape `start_indexing` /
/// `config_validation` store.
fn joined_roots(config: &Config) -> String {
    config
        .resolved_indexing_paths()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fold `event` into the last-event-wins pending map. Renames split into
/// their halves so downstream application never needs pair handling.
fn enqueue(pending: &mut HashMap<PathBuf, FsEvent>, event: FsEvent) {
    match event {
        FsEvent::Rename { from, to } => {
            pending.insert(from.clone(), FsEvent::Remove(from));
            pending.insert(to.clone(), FsEvent::Create(to));
        }
        FsEvent::Create(ref p) | FsEvent::Modify(ref p) | FsEvent::Remove(ref p) => {
            let key = p.clone();
            pending.insert(key, event);
        }
    }
}

struct Inner {
    config: Config,
    indexing: Arc<IndexingService>,
    shared: Arc<Mutex<Shared>>,
    event_tx: mpsc::Sender<FsEvent>,
    event_rx: mpsc::Receiver<FsEvent>,
    watcher: Option<Watcher>,
    watcher_config: WatcherConfig,
    /// In-flight async watcher registration (see [`Inner::start_watcher`]).
    watcher_rx: Option<mpsc::Receiver<(u64, Result<Watcher, WatchError>)>>,
    watcher_gen: u64,
    pending: HashMap<PathBuf, FsEvent>,
    needs_full_run: bool,
    /// A start was requested; set false once the service reports running,
    /// so idle-after-running transitions are detectable.
    saw_running: bool,
    write_conn: Option<Connection>,
    /// Shared with the watcher, which filters registrations by the same set.
    ignore: Arc<IgnoreSet>,
    registry: Registry,
    mode: IndexMode,
}

impl Inner {
    fn run(mut self, cmd_rx: mpsc::Receiver<CoordCmd>) {
        if self.mode == IndexMode::Auto {
            self.enter_auto();
        }
        loop {
            match cmd_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(CoordCmd::Shutdown) => break,
                Ok(cmd) => self.handle_cmd(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => self.tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.poll_watcher_startup();
            self.publish();
        }
        self.teardown();
    }

    fn handle_cmd(&mut self, cmd: CoordCmd) {
        match cmd {
            CoordCmd::SetMode(IndexMode::Auto) => self.enter_auto(),
            CoordCmd::SetMode(IndexMode::ManualStopped) => self.enter_manual_stopped(),
            // ManualRunning isn't directly settable; ReindexNow is the verb.
            CoordCmd::SetMode(IndexMode::ManualRunning) | CoordCmd::ReindexNow => {
                self.start_full_run();
                if self.mode != IndexMode::Auto {
                    self.mode = IndexMode::ManualRunning;
                }
            }
            CoordCmd::ConfigChanged(new) => {
                self.config = new;
                if let Err(e) = self.reload_filters() {
                    crate::log_warn!("coordinator: {}", e);
                }
                // The write connection may point at an old database_path.
                self.write_conn = None;
                // Watched roots / symlink behavior may have changed; a
                // restart is cheap and unconditional beats a diff here.
                if self.mode == IndexMode::Auto {
                    self.start_watcher();
                }
            }
            CoordCmd::RebuildIndex => {
                let db = self.db_path();
                self.write_conn = None;
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    crate::log_warn!("coordinator: rebuild: {}", e);
                }
                self.start_full_run();
                if self.mode != IndexMode::Auto {
                    self.mode = IndexMode::ManualRunning;
                }
            }
            CoordCmd::ClearIndex => {
                // Manual first: in Auto the periodic scheduler would see a
                // missing index and rebuild what was just deleted.
                self.enter_manual_stopped();
                self.write_conn = None;
                let db = self.db_path();
                if let Err(e) = self.indexing.delete_index_for_rebuild(&db) {
                    crate::log_warn!("coordinator: clear index: {}", e);
                }
                self.shared.lock().unwrap().last_full_index = None;
            }
            CoordCmd::Shutdown => unreachable!("handled in run()"),
        }
    }

    fn tick(&mut self) {
        self.check_watcher_degraded();
        self.drain_events();

        let status = self.indexing.get_status();
        match status {
            IndexingStatus::Running { .. } | IndexingStatus::Stopping => {
                // Single-writer rule: never touch the DB while a full run
                // is active; the queue drains on a later tick.
                self.saw_running = true;
                return;
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => {}
        }

        // A run just finished — pick up its last_full_index stamp and
        // resolve the manual-run mode.
        if self.saw_running {
            self.saw_running = false;
            self.refresh_last_full_index();
            if self.mode == IndexMode::ManualRunning {
                self.mode = IndexMode::ManualStopped;
            }
        }

        if self.mode != IndexMode::Auto {
            if self.mode == IndexMode::ManualStopped {
                self.pending.clear();
            }
            return;
        }

        if !self.pending.is_empty() && !self.needs_full_run {
            self.apply_pending();
        }

        if self.needs_full_run || self.periodic_due() {
            self.start_full_run();
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            enqueue(&mut self.pending, ev);
        }
        if self.pending.len() > PENDING_OVERFLOW {
            // Replaying a storm one file at a time is slower than one
            // incremental full run (unchanged files skip on mtime).
            self.pending.clear();
            self.needs_full_run = true;
        }
    }

    fn apply_pending(&mut self) {
        let events: Vec<FsEvent> = self.pending.drain().map(|(_, ev)| ev).collect();
        let conn = match self.ensure_write_conn() {
            Ok(conn) => conn,
            Err(e) => {
                // Missing or stale DB: incremental can't help, rebuild.
                crate::log_warn!("coordinator: incremental unavailable ({}); scheduling full run", e);
                self.needs_full_run = true;
                return;
            }
        };
        // Borrow dance: pull the connection out while applying.
        let mut conn = conn;
        for ev in &events {
            if let Err(e) = apply_fs_event(&mut conn, ev, &self.config, &self.ignore, &self.registry)
            {
                crate::log_warn!("coordinator: apply {:?}: {}", ev, e);
            }
        }
        self.write_conn = Some(conn);
    }

    fn ensure_write_conn(&mut self) -> Result<Connection, String> {
        if let Some(conn) = self.write_conn.take() {
            return Ok(conn);
        }
        db::open_existing(&self.db_path(), true)
    }

    fn periodic_due(&self) -> bool {
        let interval_secs = self
            .config
            .indexing
            .reindex_interval_minutes
            .saturating_mul(60);
        let last = self.shared.lock().unwrap().last_full_index;
        match last {
            None => true,
            Some(last) => now_unix().saturating_sub(last) >= interval_secs,
        }
    }

    fn start_full_run(&mut self) {
        let roots: Vec<String> = self
            .config
            .resolved_indexing_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if roots.is_empty() {
            crate::log_warn!("coordinator: no indexing roots configured");
            return;
        }
        // Backstop for hand-edited configs; the GUI rejects nested roots at
        // add/apply/startup with proper messaging.
        let nested = crate::config::nested_roots(&roots);
        if !nested.is_empty() {
            for (child, parent) in &nested {
                crate::log_warn!(
                    "coordinator: refusing to index: root {} is nested under {}",
                    child, parent
                );
            }
            return;
        }
        // The full run owns the DB (and may wipe/rebuild the file).
        self.write_conn = None;
        self.needs_full_run = false;
        self.pending.clear();
        if let Err(e) = self
            .indexing
            .start_indexing(roots, self.db_path(), self.config.clone())
        {
            crate::log_warn!("coordinator: start indexing: {}", e);
            return;
        }
        // Give the service's command thread a moment to flip the status;
        // small trees can finish between two coordinator ticks, and the
        // finished-run bookkeeping keys off `saw_running`.
        for _ in 0..200 {
            if !matches!(
                self.indexing.get_status(),
                IndexingStatus::Idle | IndexingStatus::Error(_)
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.saw_running = true;
    }

    fn enter_auto(&mut self) {
        self.mode = IndexMode::Auto;
        self.start_watcher();
        if self.shared.lock().unwrap().last_full_index.is_none() {
            self.needs_full_run = true;
        }
    }

    fn enter_manual_stopped(&mut self) {
        self.mode = IndexMode::ManualStopped;
        self.stop_watcher();
        self.pending.clear();
        let status = self.indexing.get_status();
        if !matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_)) {
            // Signal only — waiting up to 5 s here would stall every
            // queued command behind the Stop click.
            self.indexing.request_stop();
        }
    }

    /// Begin watcher startup WITHOUT blocking the command loop.
    /// Registering inotify watches walks every indexable directory of
    /// every root — minutes on large or networked trees — and it used to
    /// run inline here, wedging every queued command (Start/Stop/Apply)
    /// behind it. The finished watcher is handed back through a channel
    /// polled each loop turn; a generation counter discards superseded
    /// registrations.
    fn start_watcher(&mut self) {
        self.stop_watcher();
        let roots = self.config.resolved_indexing_paths();
        if roots.is_empty() {
            return;
        }
        let generation = self.watcher_gen;
        let sink_tx = self.event_tx.clone();
        let sink = Arc::new(move |ev: FsEvent| {
            let _ = sink_tx.send(ev);
        });
        let config = self.watcher_config.clone();
        // Same filters the indexer walks with, so no descriptor is spent on
        // a directory whose events would be discarded on arrival.
        let filters = WatchFilters {
            include_hidden: self.config.indexing.include_hidden,
            follow_symlinks: self.config.indexing.follow_symlinks,
            ignore: self.ignore.clone(),
        };
        let (tx, rx) = mpsc::channel();
        self.watcher_rx = Some(rx);
        self.set_watcher_status(WatcherStatus::Starting);
        let spawned = std::thread::Builder::new()
            .name("qs-watcher-start".into())
            .spawn(move || {
                let result = Watcher::start(roots, filters, config, sink);
                // A failed send means the coordinator moved on; dropping
                // the watcher here unregisters it.
                let _ = tx.send((generation, result));
            });
        if spawned.is_err() {
            self.watcher_rx = None;
            self.set_watcher_status(WatcherStatus::Off);
        }
    }

    /// Collect a finished watcher registration, if any. Called every
    /// command-loop turn so it lands regardless of tick timing.
    fn poll_watcher_startup(&mut self) {
        let Some(rx) = &self.watcher_rx else {
            return;
        };
        match rx.try_recv() {
            Ok((generation, result)) => {
                self.watcher_rx = None;
                if generation != self.watcher_gen {
                    return; // superseded; the watcher drops and unregisters
                }
                match result {
                    Ok(w) => {
                        let status = WatcherStatus::Active {
                            dirs: w.watched_dirs(),
                        };
                        self.watcher = Some(w);
                        self.set_watcher_status(status);
                    }
                    Err(e) => {
                        // Not just a log line any more: the GUI needs this to
                        // tell the user live updates are off and only the
                        // periodic reindex is refreshing the index.
                        crate::log_warn!("coordinator: watcher: {}", e);
                        self.set_watcher_status(WatcherStatus::Disabled { reason: e });
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.watcher_rx = None;
                self.set_watcher_status(WatcherStatus::Off);
            }
        }
    }

    /// Tear the watcher down if it ran out of watch budget after starting,
    /// and schedule a full run so nothing missed while it degraded is left
    /// stale. A partially watched tree looks live while going silently out
    /// of date, so we prefer none at all plus periodic rescans.
    fn check_watcher_degraded(&mut self) {
        let Some(w) = &self.watcher else {
            return;
        };
        let Some(mut reason) = w.degraded_reason() else {
            return;
        };
        // The async notify callback can't know the count; fill it in here.
        if let WatchError::KernelLimit { registered } = &mut reason {
            if *registered == 0 {
                *registered = w.watched_dirs();
            }
        }
        self.stop_watcher();
        self.needs_full_run = true;
        self.set_watcher_status(WatcherStatus::Disabled { reason });
    }

    fn stop_watcher(&mut self) {
        // Invalidate any in-flight registration and drop its channel.
        self.watcher_gen = self.watcher_gen.wrapping_add(1);
        self.watcher_rx = None;
        if let Some(mut w) = self.watcher.take() {
            w.stop();
        }
        self.set_watcher_status(WatcherStatus::Off);
    }

    fn set_watcher_status(&self, status: WatcherStatus) {
        self.shared.lock().unwrap().watcher = status;
    }

    fn db_path(&self) -> String {
        self.config
            .resolved_database_path()
            .to_string_lossy()
            .into_owned()
    }

    fn reload_filters(&mut self) -> Result<(), String> {
        self.ignore = Arc::new(
            IgnoreSet::compile(&self.config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?,
        );
        Ok(())
    }

    fn refresh_last_full_index(&self) {
        let last = db::open_existing(&self.db_path(), false)
            .ok()
            .and_then(|conn| db::repo::get_last_full_index(&conn));
        self.shared.lock().unwrap().last_full_index = last;
    }

    fn publish(&self) {
        let mut shared = self.shared.lock().unwrap();
        shared.mode = self.mode;
        shared.queued_events = self.pending.len();
    }

    /// Must stay fast: it runs (transitively) on the GUI thread during
    /// window close, and desktops show a "terminate this application?"
    /// dialog after a few unresponsive seconds. Signal, don't wait — an
    /// abandoned run is safe under WAL.
    fn teardown(mut self) {
        self.stop_watcher();
        let status = self.indexing.get_status();
        let idle = matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_));
        if !idle {
            self.indexing.request_stop();
        }
        if let Some(conn) = self.write_conn.take() {
            if idle {
                db::repo::checkpoint_and_close(conn);
            }
            // Otherwise just drop: a TRUNCATE checkpoint would block
            // behind the running writer.
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for<F: Fn() -> bool>(what: &str, timeout: Duration, check: F) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {}", what);
    }

    struct Fixture {
        dir: PathBuf,
        db: PathBuf,
        config: Config,
    }

    impl Fixture {
        fn new(auto: bool) -> Fixture {
            let stamp = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let dir = std::env::temp_dir().join(format!("qs-coord-{}", stamp));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::env::temp_dir().join(format!("qs-coord-{}.sqlite", stamp));
            let mut config = Config::default();
            config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
            config.paths.database_path = db.to_string_lossy().into_owned();
            config.indexing.auto_index = auto;
            Fixture { dir, db, config }
        }

        fn file_count(&self) -> i64 {
            match db::open_existing(&self.db.to_string_lossy(), false) {
                Ok(conn) => conn
                    .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                    .unwrap_or(0),
                Err(_) => -1,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
            std::fs::remove_file(&self.db).ok();
        }
    }

    #[test]
    fn manual_mode_starts_idle_and_reindex_now_round_trips() {
        let f = Fixture::new(false);
        std::fs::write(f.dir.join("one.txt"), "manual mode content").unwrap();

        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        assert_eq!(coord.state().mode, IndexMode::ManualStopped);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(f.file_count(), -1, "no run without a command");

        coord.reindex_now();
        wait_for("run to complete", Duration::from_secs(20), || {
            let s = coord.state();
            s.last_full_index.is_some() && s.mode == IndexMode::ManualStopped
        });
        assert_eq!(f.file_count(), 1);
        coord.shutdown();
    }

    /// Short debounce windows so trailing-edge events flush within test
    /// timeouts (production default is a 30 s window).
    fn fast_watcher() -> WatcherConfig {
        WatcherConfig {
            throttle_window: Duration::from_millis(300),
            tick_interval: Duration::from_millis(100),
            ..WatcherConfig::default()
        }
    }

    #[test]
    fn auto_mode_runs_initial_index_and_applies_watcher_events() {
        let f = Fixture::new(true);
        std::fs::write(f.dir.join("seed.txt"), "initial content").unwrap();

        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
        wait_for("initial auto index", Duration::from_secs(20), || {
            coord.state().last_full_index.is_some() && f.file_count() == 1
        });

        // New file → watcher event → incremental application.
        std::fs::write(f.dir.join("later.txt"), "arrived later").unwrap();
        wait_for("incremental add", Duration::from_secs(20), || {
            f.file_count() == 2
        });

        // Deletion sweeps the row.
        std::fs::remove_file(f.dir.join("later.txt")).unwrap();
        wait_for("incremental remove", Duration::from_secs(20), || {
            f.file_count() == 1
        });

        coord.shutdown();
    }

    /// A healthy watcher reports its own size, so the GUI can say
    /// "watching N folders" instead of guessing.
    #[test]
    fn auto_mode_reports_an_active_watcher() {
        let f = Fixture::new(true);
        std::fs::create_dir_all(f.dir.join("sub")).unwrap();
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();

        wait_for("watcher active", Duration::from_secs(20), || {
            matches!(coord.state().watcher, WatcherStatus::Active { .. })
        });
        match coord.state().watcher {
            WatcherStatus::Active { dirs } => assert_eq!(dirs, 2, "root + sub"),
            other => panic!("expected Active, got {:?}", other),
        }
        coord.shutdown();
    }

    /// The regression this whole change exists for: exceeding the watch
    /// budget must surface as `Disabled` rather than a stderr line the GUI
    /// never sees — and the periodic reindex must keep the index fresh.
    #[test]
    fn exceeding_the_watch_cap_disables_updates_but_keeps_indexing() {
        let f = Fixture::new(true);
        for sub in ["a", "b", "c"] {
            std::fs::create_dir_all(f.dir.join(sub)).unwrap();
        }
        std::fs::write(f.dir.join("seed.txt"), "content").unwrap();

        let watcher_config = WatcherConfig {
            max_watched_dirs: 2,
            ..fast_watcher()
        };
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), watcher_config).unwrap();

        wait_for("watcher disabled", Duration::from_secs(20), || {
            matches!(coord.state().watcher, WatcherStatus::Disabled { .. })
        });
        match coord.state().watcher {
            WatcherStatus::Disabled { reason } => assert_eq!(
                reason,
                WatchError::TooManyDirectories { dirs: 2, cap: 2 },
                "the cap, not some other failure"
            ),
            other => panic!("expected Disabled, got {:?}", other),
        }

        // Periodic reindex is the fallback and must still run: mode stays
        // Auto, only the watcher is off.
        assert_eq!(coord.state().mode, IndexMode::Auto);
        wait_for("full run despite no watcher", Duration::from_secs(20), || {
            coord.state().last_full_index.is_some() && f.file_count() == 1
        });
        coord.shutdown();
    }

    #[test]
    fn manual_mode_reports_the_watcher_off() {
        let f = Fixture::new(false);
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(coord.state().watcher, WatcherStatus::Off);
        coord.shutdown();
    }

    #[test]
    fn stopping_turns_the_watcher_status_off() {
        let f = Fixture::new(true);
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
        wait_for("watcher active", Duration::from_secs(20), || {
            matches!(coord.state().watcher, WatcherStatus::Active { .. })
        });

        coord.set_mode(IndexMode::ManualStopped);
        wait_for("watcher off", Duration::from_secs(10), || {
            coord.state().watcher == WatcherStatus::Off
        });
        coord.shutdown();
    }

    #[test]
    fn manual_stop_drops_watcher_and_events() {
        let f = Fixture::new(true);
        std::fs::write(f.dir.join("seed.txt"), "content").unwrap();
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        wait_for("initial index", Duration::from_secs(20), || {
            f.file_count() == 1
        });

        coord.set_mode(IndexMode::ManualStopped);
        wait_for("mode switch", Duration::from_secs(5), || {
            coord.state().mode == IndexMode::ManualStopped
        });

        std::fs::write(f.dir.join("unseen.txt"), "never indexed").unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(f.file_count(), 1, "stopped mode must not index new files");
        assert_eq!(coord.state().queued_events, 0);

        coord.shutdown();
    }

    #[test]
    fn apply_config_with_new_root_then_reindex_indexes_it() {
        // The reported failure: add directories, apply, click "Start
        // indexing now" — the run must pick up the new roots promptly
        // (watcher re-registration happens concurrently, never blocking
        // the command loop).
        let f = Fixture::new(true);
        std::fs::write(f.dir.join("first.txt"), "one").unwrap();
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
        wait_for("initial index", Duration::from_secs(20), || {
            f.file_count() == 1
        });

        let extra_root = f.dir.parent().unwrap().join(format!(
            "qs-coord-extra-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&extra_root).unwrap();
        std::fs::write(extra_root.join("second.txt"), "two").unwrap();

        let mut new_cfg = f.config.clone();
        new_cfg
            .paths
            .indexing_paths
            .push(extra_root.to_string_lossy().into_owned());
        coord.apply_config(new_cfg);
        coord.reindex_now();

        wait_for("new root indexed", Duration::from_secs(20), || {
            f.file_count() == 2
        });
        coord.shutdown();
        std::fs::remove_dir_all(&extra_root).ok();
    }

    #[test]
    fn enqueue_last_wins_and_rename_splits() {
        let mut pending = HashMap::new();
        let a = PathBuf::from("/x/a");
        enqueue(&mut pending, FsEvent::Create(a.clone()));
        enqueue(&mut pending, FsEvent::Modify(a.clone()));
        assert_eq!(pending.len(), 1);
        assert!(matches!(pending.get(&a), Some(FsEvent::Modify(_))));

        enqueue(
            &mut pending,
            FsEvent::Rename {
                from: a.clone(),
                to: PathBuf::from("/x/b"),
            },
        );
        assert_eq!(pending.len(), 2);
        assert!(matches!(pending.get(&a), Some(FsEvent::Remove(_))));
        assert!(matches!(
            pending.get(&PathBuf::from("/x/b")),
            Some(FsEvent::Create(_))
        ));
    }

    #[test]
    fn clear_index_deletes_db_and_stays_manual() {
        let f = Fixture::new(true); // auto mode — clear must not auto-resurrect
        std::fs::write(f.dir.join("a.txt"), "content").unwrap();
        let coord =
            IndexCoordinator::start_with_watcher_config(f.config.clone(), fast_watcher()).unwrap();
        wait_for("initial index", Duration::from_secs(20), || {
            f.file_count() == 1
        });

        coord.clear_index();
        wait_for("index deleted", Duration::from_secs(10), || {
            f.file_count() == -1 // open_existing fails: file gone
        });
        wait_for("manual mode", Duration::from_secs(5), || {
            coord.state().mode == IndexMode::ManualStopped
        });
        assert_eq!(coord.state().last_full_index, None);

        // Give the (now manual) coordinator a few ticks: the index must
        // stay deleted rather than being rebuilt by the scheduler.
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(f.file_count(), -1, "cleared index must stay cleared");

        coord.shutdown();
    }

    #[test]
    fn nested_roots_refuse_to_run() {
        let f = Fixture::new(false);
        let child = f.dir.join("nested");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("x.txt"), "content").unwrap();

        let mut config = f.config.clone();
        config
            .paths
            .indexing_paths
            .push(child.to_string_lossy().into_owned());
        let coord = IndexCoordinator::start(config).unwrap();
        coord.reindex_now();
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(
            f.file_count(),
            -1,
            "a run over nested roots must be refused (no DB created)"
        );
        coord.shutdown();
    }

    #[test]
    fn shutdown_is_idempotent_and_joins() {
        let f = Fixture::new(false);
        let coord = IndexCoordinator::start(f.config.clone()).unwrap();
        coord.shutdown();
        coord.shutdown(); // second call is a no-op
    }
}
