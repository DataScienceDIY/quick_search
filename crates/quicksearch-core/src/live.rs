//! Watching the search results a frontend is actually showing.
//!
//! Rows refresh from the file, never the index; writes go through
//! [`crate::coordinator::IndexCoordinator::update_paths`]. Watches go on
//! parent directories, not files: editors save by renaming over the target,
//! orphaning the inode a file watch would hold.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{
    Config as NotifyConfig, Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode,
    Watcher as NotifyWatcher,
};

use crate::config::Config;
use crate::extract::sandbox::KillMark;
use crate::extract::Registry;
use crate::file_handling::mtime_secs;
use crate::platform::{presence, Presence};
use crate::query::split::split_for_cascade;
use crate::search::fuzzy::{matcher_for, Bitap};
use crate::search::ContentTier;
use crate::shutdown::{join_within, EXIT_GRACE};
use crate::snippet::Snippet;

const SETTLE: Duration = Duration::from_millis(150);

const MIN_INTERVAL: Duration = Duration::from_millis(750);

const TICK: Duration = Duration::from_millis(50);

/// Rows updated per settled window, each of which may re-read its file; the
/// rest wait one more [`SETTLE`] apiece, so a burst over many rows lands
/// over seconds rather than at once.
const MAX_PER_TICK: usize = 4;

/// Parent directories watched per arm. Rows in further directories are
/// neither watched nor swept: they are left as the index has them.
const MAX_DIRS: usize = 64;

/// Rows watched per arm, in the order given; rows beyond them are left as the
/// index has them.
const MAX_TARGETS: usize = 256;

/// One row the frontend is showing and wants kept current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The row's path, spelled exactly as the search returned it. Event paths
    /// are compared against this byte for byte.
    pub path: String,
    /// `Some` when this row displays body text and needs its snippet re-cut
    /// on change; the tier decides how. `None` never opens the file.
    pub text: Option<ContentTier>,
    /// The size the row is displaying; the arm-time sweep compares the file
    /// against this, doubling as a check of the index against the disk.
    pub size: u64,
    /// The modified time the row is displaying; see [`Target::size`].
    pub mtime: i64,
}

/// What a change did to a row's Content Match window: "did not look" and
/// "looked and it is not there any more" must reach the frontend as
/// different answers.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowUpdate {
    /// Not a body-text row, or its body could not be re-read; the cell keeps
    /// what it has.
    Unchanged,
    Cut(Snippet),
    NoMatch,
}

/// A ready-to-apply change to one row on screen.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveUpdate {
    /// The file moved. `path` is the row's old path, the frontend's key.
    Renamed {
        path: String,
        to: String,
        name: String,
    },
    Changed {
        path: String,
        size: u64,
        mtime: i64,
        window: WindowUpdate,
    },
    /// The file is no longer there. Reversible: the directory watch stays, so
    /// a file recreated at the same path reports [`LiveUpdate::Changed`].
    Gone { path: String },
}

impl LiveUpdate {
    /// The row this update is keyed by: the path the frontend knows it as.
    pub fn path(&self) -> &str {
        match self {
            LiveUpdate::Renamed { path, .. }
            | LiveUpdate::Changed { path, .. }
            | LiveUpdate::Gone { path } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Changed,
    Renamed(PathBuf),
    /// The file left. From a rename's `From` half this is provisional: the
    /// very next event may name the destination ([`Loop::awaiting`]), and on
    /// Linux the `Both` that follows does, so it may still become
    /// [`Op::Renamed`]. A `Remove` never pairs.
    Gone,
}

enum Msg {
    Event(NotifyEvent),
    Watch {
        query: String,
        targets: Vec<Target>,
        config: Box<Config>,
        updates: mpsc::Sender<LiveUpdate>,
    },
    Clear,
    /// Answered, with the kill mark it was handled under, once every
    /// message sent before it is handled.
    #[cfg(test)]
    Sync(mpsc::Sender<KillMark>),
    Stop,
}

/// Handle on the watcher thread. Dropping it stops the thread.
pub struct LiveWatcher {
    tx: mpsc::Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl LiveWatcher {
    /// Spawn the watcher, watching nothing until [`LiveWatcher::watch`].
    /// `notify` is called after every update is queued, so an egui frontend
    /// can `request_repaint`; pass a no-op for headless use.
    pub fn start(notify: Arc<dyn Fn() + Send + Sync>) -> LiveWatcher {
        let (tx, rx) = mpsc::channel::<Msg>();
        let event_tx = tx.clone();
        let handle = thread::Builder::new()
            .name("qs-live".into())
            .spawn(move || Loop::new(rx, event_tx, notify).run())
            .expect("spawn live watcher");
        LiveWatcher {
            tx,
            handle: Some(handle),
        }
    }

    /// Replace the watched set wholesale, and return this arm's updates. The
    /// previous arm's receiver disconnects, with whatever it had not yet
    /// delivered: an update for the old rows never lands on the new ones.
    /// `config` supplies the extraction limits, so a snippet cut here is the
    /// text the indexer would have stored; registration is on the watcher's
    /// thread and never blocks the caller.
    pub fn watch(
        &self,
        query: &str,
        targets: Vec<Target>,
        config: &Config,
    ) -> mpsc::Receiver<LiveUpdate> {
        let (updates, rx) = mpsc::channel();
        // A failed send drops `updates`, so `rx` reads `Disconnected`.
        let _ = self.tx.send(Msg::Watch {
            query: query.to_string(),
            targets,
            config: Box::new(config.clone()),
            updates,
        });
        rx
    }

    /// Drop every watch and forget every pending update; the last arm's
    /// receiver disconnects.
    pub fn clear(&self) {
        let _ = self.tx.send(Msg::Clear);
    }

    /// Wait until the thread has handled every message sent before this: a
    /// [`Self::watch`] has registered its watches and sent its sweep. Gives
    /// the kill mark the thread handled this one under.
    #[cfg(test)]
    fn sync(&self) -> KillMark {
        let (done, rx) = mpsc::channel();
        let _ = self.tx.send(Msg::Sync(done));
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the live thread never answered")
    }

    /// Stop the thread, waiting at most [`EXIT_GRACE`] for it: a sweep reads
    /// no messages between its stats and extractions, and a `stat` on a hung
    /// mount never returns, so a thread still busy then is left running.
    /// Idempotent; [`Drop`] calls it.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(handle) = self.handle.take() {
            join_within(handle, Instant::now() + EXIT_GRACE);
        }
    }
}

impl Drop for LiveWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Loop {
    rx: mpsc::Receiver<Msg>,
    event_tx: mpsc::Sender<Msg>,
    /// The current arm's channel; `None` between arms.
    update_tx: Option<mpsc::Sender<LiveUpdate>>,
    notify: Arc<dyn Fn() + Send + Sync>,
    watcher: Option<RecommendedWatcher>,
    targets: HashMap<String, Target>,
    pending: HashMap<String, Op>,
    last_emit: HashMap<String, Instant>,
    /// The shown row whose rename `From` was the last event, with its cookie:
    /// only the next event may name where it went. Both halves of a rename
    /// are emitted back to back, and Windows gives them no cookie to pair by.
    /// A flush holds the row back until that event, or a wait that brings
    /// none, clears this.
    awaiting: Option<(String, Option<usize>)>,
    settle_at: Option<Instant>,
    query: Option<crate::query::split::CascadeQuery>,
    /// Fuzzy matcher built at arm time for re-cutting [`ContentTier::Fuzzy`]
    /// rows; `None` when the term does not fuzz (the pass would not run).
    fuzzy: Option<Bitap>,
    config: Option<Box<Config>>,
    registry: Registry,
    /// Read as each wait ends: a kill landing while the thread works on what
    /// the wait brought reaches every sandbox child it spawns for it
    /// ([`KillMark`]), and one that landed while it was parked does not.
    kills: KillMark,
}

impl Loop {
    fn new(
        rx: mpsc::Receiver<Msg>,
        event_tx: mpsc::Sender<Msg>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Loop {
        Loop {
            rx,
            event_tx,
            update_tx: None,
            notify,
            watcher: None,
            targets: HashMap::new(),
            pending: HashMap::new(),
            last_emit: HashMap::new(),
            awaiting: None,
            settle_at: None,
            query: None,
            fuzzy: None,
            config: None,
            registry: Registry::default_set(),
            kills: KillMark::now(),
        }
    }

    fn run(mut self) {
        loop {
            // With nothing to settle, park until a message arrives.
            let msg = match self.settle_at {
                None => self
                    .rx
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    self.rx.recv_timeout(wait.min(TICK))
                }
            };
            self.kills = KillMark::now();
            if !self.handle(msg) {
                return;
            }
        }
    }

    /// Act on what one wait brought, then flush what has settled; `false`
    /// once the thread should exit.
    fn handle(&mut self, msg: Result<Msg, mpsc::RecvTimeoutError>) -> bool {
        match msg {
            Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Ok(Msg::Clear) => self.reset(),
            Ok(Msg::Watch {
                query,
                targets,
                config,
                updates,
            }) => self.rearm(&query, targets, config, updates),
            #[cfg(test)]
            Ok(Msg::Sync(done)) => {
                let _ = done.send(self.kills);
            }
            Ok(Msg::Event(event)) => {
                classify(&event, &self.targets, &mut self.pending, &mut self.awaiting);
                // A row awaiting its destination is pending too.
                if !self.pending.is_empty() && self.settle_at.is_none() {
                    self.settle_at = Some(Instant::now() + SETTLE);
                }
            }
            // No event came right behind a `From`, so none can say where its
            // row went.
            Err(mpsc::RecvTimeoutError::Timeout) => self.awaiting = None,
        }
        self.flush_settled();
        true
    }

    fn reset(&mut self) {
        self.update_tx = None;
        self.watcher = None;
        self.targets.clear();
        self.pending.clear();
        self.last_emit.clear();
        self.awaiting = None;
        self.settle_at = None;
        self.query = None;
        self.fuzzy = None;
        self.config = None;
    }

    /// Point the watcher at a new set of rows, reporting on `updates`, then
    /// check each against the disk. Registration failures are per-directory
    /// and silent beyond the log: this is a cosmetic feature.
    fn rearm(
        &mut self,
        query: &str,
        targets: Vec<Target>,
        config: Box<Config>,
        updates: mpsc::Sender<LiveUpdate>,
    ) {
        self.reset();
        self.update_tx = Some(updates);
        if targets.is_empty() {
            return;
        }
        self.query = split_for_cascade(query).ok();
        // The fuzzy pass's own matcher, so a fuzzy row is re-cut against
        // exactly what matched it.
        self.fuzzy = self
            .query
            .as_ref()
            .and_then(|q| matcher_for(q, config.search.fuzzy_max_edits))
            .map(|(_, bitap)| bitap);
        self.config = Some(config);

        let mut watcher = {
            let tx = self.event_tx.clone();
            let sink = move |res: notify::Result<NotifyEvent>| {
                if let Ok(event) = res {
                    let _ = tx.send(Msg::Event(event));
                }
            };
            match RecommendedWatcher::new(sink, NotifyConfig::default()) {
                Ok(w) => w,
                Err(e) => {
                    crate::log_warn!("live results: no watcher available: {}", e);
                    return;
                }
            }
        };

        let mut dirs: Vec<PathBuf> = Vec::new();
        for target in targets.into_iter().take(MAX_TARGETS) {
            // Never canonicalized: notify builds each event path as
            // `watched_dir.join(name)`, so keeping the index's spelling lets
            // event paths be compared to `Target::path` as plain strings.
            let Some(dir) = Path::new(&target.path).parent().map(Path::to_path_buf) else {
                continue;
            };
            if !dirs.contains(&dir) {
                if dirs.len() >= MAX_DIRS {
                    continue;
                }
                if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    // Unlike the indexer, not all-or-nothing: rows under this
                    // directory are dropped rather than swept, because a row
                    // corrected once and then frozen misleads more than one
                    // left alone.
                    crate::log_warn!("live results: not watching {}: {}", dir.display(), e);
                    continue;
                }
                dirs.push(dir);
            }
            self.targets.insert(target.path.clone(), target);
        }
        if self.targets.is_empty() {
            return;
        }
        self.watcher = Some(watcher);
        self.sweep();
    }

    /// Compare every target against the disk once and emit what disagrees, so
    /// a row whose file changed off screen (or whose index row was stale when
    /// the search returned it) is corrected the moment it is watched. A row
    /// whose file cannot be looked at is left as it is.
    fn sweep(&mut self) {
        let updates: Vec<LiveUpdate> = self
            .targets
            .values()
            .filter_map(|t| self.disk_update(&t.path, t.text, Some((t.size, t.mtime))))
            .collect();
        let now = Instant::now();
        for update in updates {
            // So an event arriving right behind the sweep (a save still in
            // flight) does not repeat the same answer a moment later.
            self.last_emit.insert(update.path().to_string(), now);
            self.note_emitted(&update);
            self.send(update);
        }
    }

    /// Turn the settled window's decisions into updates.
    fn flush_settled(&mut self) {
        let Some(at) = self.settle_at else { return };
        if Instant::now() < at {
            return;
        }
        self.settle_at = None;

        let now = Instant::now();
        // A row whose `From` was the last event waits for the next one, which
        // may name where it went: reported now, it would read as gone.
        let awaited = self.awaiting.as_ref().map(|(path, _)| path);
        let ready: Vec<(String, Op)> = self
            .pending
            .iter()
            .filter(|(path, _)| {
                Some(*path) != awaited
                    && self
                        .last_emit
                        .get(*path)
                        .is_none_or(|t| now.duration_since(*t) >= MIN_INTERVAL)
            })
            .take(MAX_PER_TICK)
            .map(|(path, op)| (path.clone(), op.clone()))
            .collect();

        for (path, op) in ready {
            self.pending.remove(&path);
            self.last_emit.insert(path.clone(), now);
            self.apply(path, op);
        }
        if !self.pending.is_empty() {
            self.settle_at = Some(now + SETTLE);
        }
    }

    fn apply(&mut self, path: String, op: Op) {
        let update = match op {
            Op::Gone => LiveUpdate::Gone { path },
            Op::Renamed(to) => {
                let name = to
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                LiveUpdate::Renamed {
                    path,
                    to: to.to_string_lossy().into_owned(),
                    name,
                }
            }
            Op::Changed => {
                // Read from the file, not from the index: this has to land
                // whatever the indexer is doing, or not doing.
                let text = self.targets.get(&path).and_then(|t| t.text);
                let Some(update) = self.disk_update(&path, text, None) else {
                    return;
                };
                update
            }
        };
        self.note_emitted(&update);
        self.send(update);
    }

    /// What one `stat` of a target's file says about its row: `Changed`,
    /// `Gone` once the file is not there, and nothing when the `stat` could
    /// not look ([`Presence::Unknown`]) or the file still has the `stored`
    /// size and modified time. A dehydrated cloud file keeps the window its
    /// row has: re-cutting it would read the file, and so recall it.
    fn disk_update(
        &self,
        path: &str,
        text: Option<ContentTier>,
        stored: Option<(u64, i64)>,
    ) -> Option<LiveUpdate> {
        let meta = match presence(Path::new(path)) {
            Presence::File(meta) => meta,
            Presence::Gone => {
                return Some(LiveUpdate::Gone {
                    path: path.to_string(),
                })
            }
            Presence::Unknown => return None,
        };
        let (size, mtime) = (meta.len(), mtime_secs(&meta) as i64);
        if stored == Some((size, mtime)) {
            return None;
        }
        let text = text.filter(|_| !crate::platform::is_cloud_placeholder(&meta));
        Some(LiveUpdate::Changed {
            path: path.to_string(),
            size,
            mtime,
            window: text.map_or(WindowUpdate::Unchanged, |tier| {
                self.window_from_disk(path, size, tier)
            }),
        })
    }

    /// Re-cut this row's Content Match window from the file on disk, via the
    /// indexer's own extraction path and the tier's own matcher, so a
    /// refreshed row cannot disagree with a re-run search except on timing.
    fn window_from_disk(&self, path: &str, size: u64, tier: ContentTier) -> WindowUpdate {
        let (Some(config), Some(query)) = (self.config.as_deref(), self.query.as_ref()) else {
            return WindowUpdate::Unchanged;
        };
        // The indexer would not have stored text for a file this large.
        if size > config.processing.maximum_text_file_size {
            return WindowUpdate::Unchanged;
        }
        let file = Path::new(path);
        let Some(head) = read_head(file, config.processing.hash_length) else {
            return WindowUpdate::Unchanged;
        };
        let mime = crate::mime::guess_mime_from_head(file, &head);
        // One file per call, on the UI's refresh path: no loop to amortize a
        // longer-lived scratch over.
        let mut scratch = crate::extract::Scratch::since(config, self.kills);
        let outcome = crate::file_handling::decide_content(
            path,
            mime,
            &self.registry,
            config,
            &mut scratch,
        );
        let Some(text) = crate::file_handling::outcome_body(&outcome) else {
            return WindowUpdate::Unchanged;
        };
        let cut = match tier {
            // Unlike in the passes, the body may have stopped matching here.
            ContentTier::Exact => {
                crate::search::cascade::body_match(&query.pattern, text, &mut String::new())
                    .map(|m| m.snippet)
            }
            ContentTier::Fuzzy => match &self.fuzzy {
                Some(bitap) => crate::search::cascade::fuzzy_snippet(bitap, text).map(|(_, s)| s),
                // The term does not fuzz; the row cannot be re-judged.
                None => return WindowUpdate::Unchanged,
            },
        };
        match cut {
            Some(snip) => WindowUpdate::Cut(snip),
            None => WindowUpdate::NoMatch,
        }
    }

    /// Keep the target's baseline in step with what the frontend was just
    /// told, so a later sweep over the same arm does not repeat itself.
    fn note_emitted(&mut self, update: &LiveUpdate) {
        let LiveUpdate::Changed {
            path, size, mtime, ..
        } = update
        else {
            return;
        };
        if let Some(target) = self.targets.get_mut(path) {
            target.size = *size;
            target.mtime = *mtime;
        }
    }

    fn send(&self, update: LiveUpdate) {
        if let Some(tx) = &self.update_tx {
            if tx.send(update).is_ok() {
                (self.notify)();
            }
        }
    }
}

/// The first `limit` bytes of a file, for MIME sniffing. A short read is the
/// whole file and is not an error; an unreadable file simply has no MIME.
fn read_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    // The earlier `stat` said file, but by now it may be a FIFO: a blocking
    // open here would stall every live row for the rest of the session, and
    // `LiveWatcher::stop` would give up on this thread.
    let (file, _) = crate::platform::open_regular_file(path).ok()?;
    let mut head = Vec::new();
    file.take(limit as u64).read_to_end(&mut head).ok()?;
    Some(head)
}

/// Fold one notify event into the pending decisions for this window; every
/// shape below is a real emission from `notify` 6.1 on some platform. A
/// rename's two halves pair only back to back (`awaiting`): a destination
/// pairs with nothing else in the window, and a deletion pairs with nothing.
fn classify(
    event: &NotifyEvent,
    targets: &HashMap<String, Target>,
    pending: &mut HashMap<String, Op>,
    awaiting: &mut Option<(String, Option<usize>)>,
) {
    use notify::event::{ModifyKind, RenameMode};

    let awaited = awaiting.take();

    // `to_str`, not `to_string_lossy`: a lossy key names another file (see
    // [`crate::file_handling::path_to_db_string`]) and would fire a Gone or a
    // Changed at a row we are displaying.
    let target_key = |p: &PathBuf| {
        p.to_str()
            .filter(|k| targets.contains_key(*k))
            .map(str::to_owned)
    };
    // Every displayed row the event names takes `op`.
    let mark = |pending: &mut HashMap<String, Op>, op: Op| {
        for key in event.paths.iter().filter_map(target_key) {
            pending.insert(key, op.clone());
        }
    };

    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // Linux emits this *after* the From/To pair, so it lands in the
            // same window and overrides what they decided.
            let (Some(from), Some(to)) = (event.paths.first(), event.paths.get(1)) else {
                return;
            };
            if let Some(from_key) = target_key(from) {
                // A destination the index cannot spell would put a wrong path
                // on the row; the file left, so `Gone` is the honest update.
                let op = match to.to_str() {
                    Some(_) => Op::Renamed(to.clone()),
                    None => Op::Gone,
                };
                pending.insert(from_key, op);
            } else if let Some(to_key) = target_key(to) {
                // The atomic-save shape: a temporary file renamed over a row
                // we are watching. The row did not move; its contents changed.
                pending.insert(to_key, Op::Changed);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for path in &event.paths {
                if let Some(key) = target_key(path) {
                    pending.insert(key, Op::Changed);
                } else if let Some((source, cookie)) = &awaited {
                    // The other half of `source`'s rename if the cookies
                    // agree: Linux's differ for a move-out followed by an
                    // unrelated move-in, Windows has none. Screened as `Both`
                    // is: an unrepresentable destination leaves it `Gone`.
                    if *cookie == event.tracker() && path.to_str().is_some() {
                        pending.insert(source.clone(), Op::Renamed(path.clone()));
                    }
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for path in &event.paths {
                if let Some(key) = target_key(path) {
                    pending.entry(key.clone()).or_insert(Op::Gone);
                    *awaiting = Some((key, event.tracker()));
                }
            }
        }
        EventKind::Remove(_) => mark(pending, Op::Gone),
        // A Create at a watched path un-deletes the row.
        EventKind::Create(_) | EventKind::Modify(_) => mark(pending, Op::Changed),
        // Access events, and paths we are not showing. Live results never add
        // rows: nothing here can know a new file matches the query.
        _ => {}
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
