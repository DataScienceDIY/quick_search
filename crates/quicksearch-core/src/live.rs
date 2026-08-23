//! Watching the search results a frontend is actually showing.
//!
//! Rows refresh from the *file*, never the index — writes go through
//! [`crate::coordinator::IndexCoordinator::update_paths`]. Watches go on
//! parent *directories*, not files: editors save by renaming over the
//! target, orphaning the inode a file watch would hold.

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
use crate::extract::Registry;
use crate::query::split::split_for_cascade;
use crate::search::fuzzy::{edit_budget, Bitap};
use crate::search::ContentTier;
use crate::snippet::Snippet;

const SETTLE: Duration = Duration::from_millis(150);

const MIN_INTERVAL: Duration = Duration::from_millis(750);

const TICK: Duration = Duration::from_millis(50);

const MAX_PER_TICK: usize = 4;

const MAX_DIRS: usize = 64;

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
    /// The file moved. `path` is the row's old path — the frontend's key.
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
    /// The row this update is keyed by — the path the frontend knows it as.
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
    /// Provisional: on Linux the `From` half arrives before the paired event
    /// naming the destination, so this may still upgrade to [`Op::Renamed`].
    Gone,
}

enum Msg {
    Event(NotifyEvent),
    Watch {
        query: String,
        targets: Vec<Target>,
        config: Box<Config>,
    },
    Clear,
    Stop,
}

/// Handle on the watcher thread. Dropping it stops the thread.
pub struct LiveWatcher {
    tx: mpsc::Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl LiveWatcher {
    /// Spawn the watcher. `notify` is called after every update is queued, so
    /// an egui frontend can `request_repaint`; pass a no-op for headless use.
    pub fn start(notify: Arc<dyn Fn() + Send + Sync>) -> (LiveWatcher, mpsc::Receiver<LiveUpdate>) {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (update_tx, update_rx) = mpsc::channel::<LiveUpdate>();
        let event_tx = tx.clone();
        let handle = thread::Builder::new()
            .name("qs-live".into())
            .spawn(move || {
                Loop {
                    rx,
                    event_tx,
                    update_tx,
                    notify,
                    watcher: None,
                    targets: HashMap::new(),
                    pending: HashMap::new(),
                    last_emit: HashMap::new(),
                    orphan_to: Vec::new(),
                    settle_at: None,
                    query: None,
                    fuzzy: None,
                    config: None,
                    registry: Registry::default_set(),
                }
                .run()
            })
            .expect("spawn live watcher");
        (
            LiveWatcher {
                tx,
                handle: Some(handle),
            },
            update_rx,
        )
    }

    /// Replace the watched set wholesale. `config` supplies the extraction
    /// limits, so a snippet cut here is the text the indexer would have
    /// stored; registration is on the watcher's thread and never blocks the
    /// caller.
    pub fn watch(&self, query: &str, targets: Vec<Target>, config: &Config) {
        let _ = self.tx.send(Msg::Watch {
            query: query.to_string(),
            targets,
            config: Box::new(config.clone()),
        });
    }

    /// Drop every watch and forget every pending update.
    pub fn clear(&self) {
        let _ = self.tx.send(Msg::Clear);
    }

    /// Stop the thread and join it. Idempotent; [`Drop`] calls it.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
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
    update_tx: mpsc::Sender<LiveUpdate>,
    notify: Arc<dyn Fn() + Send + Sync>,
    watcher: Option<RecommendedWatcher>,
    targets: HashMap<String, Target>,
    pending: HashMap<String, Op>,
    last_emit: HashMap<String, Instant>,
    /// Rename destinations seen in this window whose source is not a target —
    /// the other half of a Windows rename, which carries no pairing cookie.
    orphan_to: Vec<PathBuf>,
    settle_at: Option<Instant>,
    query: Option<crate::query::split::CascadeQuery>,
    /// Fuzzy matcher built at arm time for re-cutting [`ContentTier::Fuzzy`]
    /// rows; `None` when the term does not fuzz (the pass would not run).
    fuzzy: Option<Bitap>,
    config: Option<Box<Config>>,
    registry: Registry,
}

impl Loop {
    fn run(mut self) {
        loop {
            let msg = match self.settle_at {
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                },
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match self.rx.recv_timeout(wait.min(TICK)) {
                        Ok(msg) => Some(msg),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            };
            match msg {
                Some(Msg::Stop) => return,
                Some(Msg::Clear) => self.reset(),
                Some(Msg::Watch {
                    query,
                    targets,
                    config,
                }) => self.rearm(&query, targets, config),
                Some(Msg::Event(event)) => {
                    classify(
                        &event,
                        &self.targets,
                        &mut self.pending,
                        &mut self.orphan_to,
                    );
                    // Arm on either queue: `orphan_to` drains only in
                    // `flush_settled`, and on Windows the pairing needs
                    // `orphans.len() == 1` — one leaked stale entry turns
                    // every later rename into "gone".
                    if (!self.pending.is_empty() || !self.orphan_to.is_empty())
                        && self.settle_at.is_none()
                    {
                        self.settle_at = Some(Instant::now() + SETTLE);
                    }
                }
                None => {}
            }
            self.flush_settled();
        }
    }

    fn reset(&mut self) {
        self.watcher = None;
        self.targets.clear();
        self.pending.clear();
        self.last_emit.clear();
        self.orphan_to.clear();
        self.settle_at = None;
        self.query = None;
        self.fuzzy = None;
        self.config = None;
    }

    /// Point the watcher at a new set of rows, then check each against the
    /// disk. Registration failures are per-directory and silent beyond the
    /// log: this is a cosmetic feature.
    fn rearm(&mut self, query: &str, targets: Vec<Target>, config: Box<Config>) {
        self.reset();
        if targets.is_empty() {
            return;
        }
        self.query = split_for_cascade(query).ok();
        // The same construction the fuzzy pass makes, so a fuzzy row is
        // re-cut against exactly what matched it; case folds in the mask table.
        self.fuzzy = self.query.as_ref().and_then(|q| {
            if q.pattern.is_wildcard() {
                return None;
            }
            let k = edit_budget(q.term.len(), config.search.fuzzy_max_edits)?;
            Bitap::new(q.term.as_bytes(), k)
        });
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
                    // Deliberately not the indexer's all-or-nothing: rows
                    // under this directory are dropped, not swept — better
                    // left alone than corrected once and silently frozen.
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

    /// Compare every target against the disk once and emit what disagrees: a
    /// row whose file changed off screen — or whose index row was out of date
    /// when the search returned it — is corrected the moment it is watched.
    fn sweep(&mut self) {
        let mut updates: Vec<LiveUpdate> = Vec::new();
        for target in self.targets.values() {
            match std::fs::metadata(&target.path) {
                Ok(meta) if meta.is_file() => {
                    let (size, mtime) = (meta.len(), mtime_of(&meta));
                    if size == target.size && mtime == target.mtime {
                        continue;
                    }
                    updates.push(self.changed_update(&target.path, target.text, size, mtime));
                }
                _ => updates.push(LiveUpdate::Gone {
                    path: target.path.clone(),
                }),
            }
        }
        let now = Instant::now();
        for update in updates {
            // So an event arriving right behind the sweep — a save still in
            // flight — does not repeat the same answer a moment later.
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

        // Windows reports rename halves separately with no pairing cookie.
        // Exactly one target gone plus one unclaimed destination is the same
        // file; anything more ambiguous resolves to the truthful "gone".
        let orphans = std::mem::take(&mut self.orphan_to);
        let gone: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, op)| **op == Op::Gone)
            .map(|(path, _)| path.clone())
            .collect();
        if gone.len() == 1 && orphans.len() == 1 {
            self.pending
                .insert(gone[0].clone(), Op::Renamed(orphans[0].clone()));
        }

        let now = Instant::now();
        let ready: Vec<(String, Op)> = self
            .pending
            .iter()
            .filter(|(path, _)| {
                self.last_emit
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
                match std::fs::metadata(&path) {
                    Ok(meta) if meta.is_file() => {
                        let text = self.targets.get(&path).and_then(|t| t.text);
                        self.changed_update(&path, text, meta.len(), mtime_of(&meta))
                    }
                    _ => LiveUpdate::Gone { path },
                }
            }
        };
        self.note_emitted(&update);
        self.send(update);
    }

    fn changed_update(
        &self,
        path: &str,
        text: Option<ContentTier>,
        size: u64,
        mtime: i64,
    ) -> LiveUpdate {
        LiveUpdate::Changed {
            path: path.to_string(),
            size,
            mtime,
            window: text.map_or(WindowUpdate::Unchanged, |tier| {
                self.window_from_disk(path, size, tier)
            }),
        }
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
        let outcome =
            crate::file_handling::decide_content(path, mime.as_deref(), &self.registry, config);
        let Some(text) = crate::file_handling::outcome_body(&outcome) else {
            return WindowUpdate::Unchanged;
        };
        let cut = match tier {
            ContentTier::Exact => {
                // Unlike in the passes, the body here may genuinely have
                // stopped matching; an unmarked window is how that reads.
                let folded = text.to_ascii_lowercase();
                crate::search::cascade::text_snippet(&query.pattern, text, &folded)
                    .filter(|snip| !snip.ranges.is_empty())
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
        if self.update_tx.send(update).is_ok() {
            (self.notify)();
        }
    }
}

/// The first `limit` bytes of a file, for MIME sniffing. A short read is the
/// whole file and is not an error; an unreadable file simply has no MIME.
fn read_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    // The earlier `stat` said file; by now it may be a FIFO, and a blocking
    // open here costs every live row for the rest of the session — and hangs
    // exit, since `LiveWatcher::stop` joins this thread.
    let file = crate::platform::open_regular_file(path).ok()?;
    let mut head = Vec::new();
    file.take(limit as u64).read_to_end(&mut head).ok()?;
    Some(head)
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Fold one notify event into the pending decisions for this window; every
/// shape below is a real emission from `notify` 6.1 on some platform.
fn classify(
    event: &NotifyEvent,
    targets: &HashMap<String, Target>,
    pending: &mut HashMap<String, Op>,
    orphan_to: &mut Vec<PathBuf>,
) {
    use notify::event::{ModifyKind, RenameMode};

    // `to_str`, not `to_string_lossy`: an unrepresentable path still has a
    // lossy spelling that is a valid path for some *other* file, so keying
    // lossily would fire a Gone or a Changed at a row we are displaying.
    let target_key = |p: &PathBuf| {
        p.to_str()
            .filter(|k| targets.contains_key(*k))
            .map(str::to_owned)
    };

    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // Linux emits this *after* the From/To pair, so it lands in the
            // same window and overwrites the provisional Gone recorded below.
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
                } else if path.to_str().is_some() {
                    // Screened too: an unrepresentable orphan would pair with
                    // a lone `Gone` into the same bad `Renamed`.
                    orphan_to.push(path.clone());
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for path in &event.paths {
                if let Some(key) = target_key(path) {
                    pending.entry(key).or_insert(Op::Gone);
                }
            }
        }
        EventKind::Remove(_) => {
            for path in &event.paths {
                if let Some(key) = target_key(path) {
                    pending.insert(key, Op::Gone);
                }
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in &event.paths {
                if let Some(key) = target_key(path) {
                    // A Create at a watched path un-deletes the row.
                    pending.insert(key, Op::Changed);
                }
            }
        }
        // Access events, and paths we are not showing. Live results never
        // *add* rows: nothing here can know a new file matches the query.
        _ => {}
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
