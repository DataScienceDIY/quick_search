//! Watching the search results a frontend is actually showing.
//!
//! What a row shows is read from the *file*, never from the index. That is
//! what lets this work with indexing stopped, with a file outside every
//! indexed root, or against a row the indexer has not caught up with yet —
//! and it is the whole point of the feature: the list on screen describes the
//! disk, not a snapshot of it.
//!
//! Nothing here writes to the index. Keeping it so is what lets this run
//! alongside the indexer without a second writer; a frontend that wants the
//! index brought back in line with what it just displayed hands the paths to
//! [`crate::coordinator::IndexCoordinator::update_paths`], which does the
//! write on its own thread.
//!
//! # Why directories, not files
//!
//! The obvious design is a watch per result file. It does not work. Editors
//! save by writing a temporary file and renaming it over the target, so the
//! event lands on the *directory* and the old inode — the one a file watch is
//! attached to — is simply orphaned. A file watch also cannot report the new
//! name of a rename. Watching the deduplicated set of parent directories
//! `NonRecursive` sees both, on inotify and on `ReadDirectoryChangesW` alike.
//!
//! # Why this is not [`crate::watcher`]
//!
//! That module exists to cover *subtrees*: it walks each root registering
//! every directory beneath it, adds directories that appear later, and backs a
//! 128k budget with an all-or-nothing guarantee. Pointing it at a result's
//! parent would register that parent's whole tree. This is a flat, fixed, tiny
//! set with no growth and no budget, and its timings are a tenth of that one's
//! — the indexer can afford to coalesce for thirty seconds, a cursor blinking
//! next to a stale filename cannot.
//!
//! # What an event turns into
//!
//! A rename is applied from the event itself. A content change is answered by
//! reading the file: `metadata` for size and modified time, and — for a row
//! whose cell shows body text — the same MIME sniffing and extractors the
//! indexer uses, re-cut through the same [`crate::search::cascade::text_snippet`]
//! (or, for a fuzzy hit, [`crate::search::cascade::fuzzy_snippet`]) the
//! search itself uses.
//!
//! Arming also sweeps every target once, comparing the file on disk against
//! the size and modified time the row is *currently displaying*. Since a fresh
//! result carries what the index said, that sweep is exactly a check of the
//! index against the disk, and it is what makes a row corrected while it was
//! scrolled out of view right itself the moment it comes back. It is also the
//! only thing that works on a filesystem the platform reports no events for.

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

/// How long events for one path are pooled before being acted on, so the
/// several notify emits behind a single save collapse into one update.
const SETTLE: Duration = Duration::from_millis(150);

/// Floor on how often any one path may produce an update. A file being written
/// in a loop — a log, a build artifact — cannot spin the UI.
const MIN_INTERVAL: Duration = Duration::from_millis(750);

/// How often the loop wakes while it has work pending. It blocks outright when
/// it has none, so an idle QuickSearch does not tick at all.
const TICK: Duration = Duration::from_millis(50);

/// Ceiling on updates emitted per tick, so a directory-wide change (an
/// unpack, a `chmod -R`) drains over several frames instead of one.
const MAX_PER_TICK: usize = 4;

/// Most watches to register. Results cluster hard — a query's hits usually
/// share a handful of directories — so this is generous for the visible rows
/// while staying negligible against the indexer's 128k budget.
const MAX_DIRS: usize = 64;

/// Most rows to track, whatever the frontend asks for.
const MAX_TARGETS: usize = 256;

/// One row the frontend is showing and wants kept current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The row's path, spelled exactly as the search returned it. Event paths
    /// are compared against this byte for byte — see the note in [`watch`].
    ///
    /// [`watch`]: LiveWatcher::watch
    pub path: String,
    /// `Some` when this row displays text from the file's *body*, and so
    /// needs its snippet re-cut when the file changes — and how, since an
    /// exact-tier window is cut around the literal term and a fuzzy one
    /// around a bitap match the literal is usually absent from. `None` for a
    /// filename or path match, which costs one `metadata` call per change and
    /// never opens the file.
    pub text: Option<ContentTier>,
    /// The size the row is displaying. The arm-time sweep compares the file
    /// against this, so on a fresh result — where it is whatever the index
    /// said — the sweep doubles as a check of the index against the disk.
    pub size: u64,
    /// The modified time the row is displaying; see [`Target::size`].
    pub mtime: i64,
}

/// What a change did to a row's Content Match window.
///
/// Three states, not an `Option`, because "I did not look" and "I looked and
/// it is not there any more" have to reach the frontend as different answers.
/// Blanking a cell because the file was too large to re-read would lose a
/// window the search legitimately found.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowUpdate {
    /// Nothing to say: not a body-text row, or its body could not be re-read
    /// (too large, no extractor, unreadable). The cell keeps what it has.
    Unchanged,
    /// Re-cut from the file as it is now.
    Cut(Snippet),
    /// The body was read and the query is no longer in it. The cell has
    /// nothing to show and falls back to its dash.
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
    /// The file's contents changed, as read from the file itself.
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

/// What one settled event window decided about a path, before the filesystem
/// is consulted.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Changed,
    Renamed(PathBuf),
    /// Provisional: on Linux the `From` half of a rename arrives before the
    /// paired event that names the destination, so this may still be upgraded
    /// to [`Op::Renamed`] inside the same window.
    Gone,
}

/// Commands and events share one channel so the loop can block on `recv()`
/// whenever nothing is pending.
enum Msg {
    Event(NotifyEvent),
    Watch {
        query: String,
        targets: Vec<Target>,
        /// Boxed: this is by far the largest variant, and a `Watch` is rare
        /// next to the events sharing the channel with it.
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

    /// Replace the watched set wholesale.
    ///
    /// `query` is the search these rows came from; it is what a re-cut snippet
    /// is marked against. `config` supplies the extraction limits and filters,
    /// so a snippet cut here is the text the indexer would have stored.
    /// Registration happens on the watcher's own thread, so this never blocks
    /// the caller on a spun-down disk or a stale mount.
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
    /// The fuzzy matcher for `query`, built at arm time — building it is the
    /// cost, running it is cheap — for re-cutting [`ContentTier::Fuzzy`]
    /// rows. `None` when the term does not fuzz (too short, wildcarded, or a
    /// zero edit budget), which is when the fuzzy pass would not have run.
    fuzzy: Option<Bitap>,
    config: Option<Box<Config>>,
    /// Built once and reused: the extractors are stateless, and the frontend
    /// re-arms often enough that rebuilding the table per arm would be waste.
    registry: Registry,
}

impl Loop {
    fn run(mut self) {
        loop {
            // Block outright when there is nothing to time out on: an idle
            // window costs no wakeups at all.
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
                    if !self.pending.is_empty() && self.settle_at.is_none() {
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

    /// Point the watcher at a new set of rows, dropping everything about the
    /// old one, then check each one against the disk. Registration failures
    /// are per-directory and silent beyond the log: this is a cosmetic
    /// feature, and a modal about a missing highlight would be worse than the
    /// missing highlight.
    fn rearm(&mut self, query: &str, targets: Vec<Target>, config: Box<Config>) {
        self.reset();
        if targets.is_empty() {
            return;
        }
        self.query = split_for_cascade(query).ok();
        // The same construction the fuzzy pass makes for a scan, so a fuzzy
        // row is re-cut against exactly what matched it.
        self.fuzzy = self.query.as_ref().and_then(|q| {
            if q.pattern.is_wildcard() {
                return None;
            }
            let folded = q.term.to_ascii_lowercase();
            let k = edit_budget(folded.len(), config.search.fuzzy_max_edits)?;
            Bitap::new(folded.as_bytes(), k)
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
            // Derived from the row's own path, never canonicalized: notify
            // builds each event path as `watched_dir.join(name)`, so leaving
            // this spelled as the index spells it is what lets event paths be
            // compared to `Target::path` as plain strings.
            let Some(dir) = Path::new(&target.path).parent().map(Path::to_path_buf) else {
                continue;
            };
            if !dirs.contains(&dir) {
                if dirs.len() >= MAX_DIRS {
                    continue;
                }
                if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    // Deliberately not the indexer's all-or-nothing: partial
                    // coverage of a display nicety is fine. Rows under this
                    // directory are dropped rather than swept — a row nothing
                    // can follow is better left alone than corrected once and
                    // then silently frozen.
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

    /// Compare every target against the disk once and emit what disagrees.
    ///
    /// Nothing here waits for an event, which is the point: a row whose file
    /// changed while it was scrolled out of view — or one whose index row was
    /// simply out of date when the search returned it — is corrected the
    /// moment it is watched.
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
                // Unreadable counts as gone: the row cannot be shown as
                // current when we cannot see the file at all.
                _ => updates.push(LiveUpdate::Gone {
                    path: target.path.clone(),
                }),
            }
        }
        let now = Instant::now();
        for update in updates {
            // Recorded so an event arriving right behind the sweep — a save
            // still in flight when the row came on screen — does not repeat
            // the same answer a moment later.
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

        // Windows never reports the two halves of a rename as one event and
        // gives no cookie to pair them by. When exactly one target went away
        // and exactly one unclaimed destination appeared in the same window,
        // they are the same file; anything more ambiguous resolves to the
        // truthful "gone".
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

    /// What the file at `path` implies for the row showing it.
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

    /// Re-cut this row's Content Match window from the file on disk.
    ///
    /// Deliberately the indexer's own path — [`crate::mime::guess_mime_from_head`]
    /// then [`crate::file_handling::decide_content`], which already applies
    /// `content_extensions` and the `maximum_text_size` truncation — so the
    /// text a window is cut from is the text the index would have stored, and
    /// a refreshed row cannot disagree with a re-run search about anything but
    /// timing. Then the tier's own matcher, for the same reason.
    fn window_from_disk(&self, path: &str, size: u64, tier: ContentTier) -> WindowUpdate {
        let (Some(config), Some(query)) = (self.config.as_deref(), self.query.as_ref()) else {
            return WindowUpdate::Unchanged;
        };
        // The indexer would not have stored text for a file this large, so
        // neither does the row — reading it would stall this thread over a
        // window nobody can see the whole of anyway.
        if size > config.processing.maximum_text_file_size {
            return WindowUpdate::Unchanged;
        }
        let file = Path::new(path);
        let Some(head) = read_head(file, config.processing.hash_length) else {
            return WindowUpdate::Unchanged;
        };
        let mime = crate::mime::guess_mime_from_head(file, &head);
        // Extractors run third-party parsers over whatever the file now
        // holds. One that panics must not take this thread — and with it every
        // live row for the rest of the session — down with it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::file_handling::decide_content(path, mime.as_deref(), &self.registry, config)
        }));
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(_) => {
                crate::log_warn!("live results: extracting {} panicked", path);
                return WindowUpdate::Unchanged;
            }
        };
        let Some(text) = crate::file_handling::outcome_body(&outcome) else {
            return WindowUpdate::Unchanged;
        };
        let folded = text.to_ascii_lowercase();
        let cut = match tier {
            ContentTier::Exact => {
                // A literal term always yields a window, marked or not,
                // because the passes only ever call this for a body FTS
                // already matched. Here the body may genuinely have stopped
                // matching, and an unmarked window is how that reads.
                crate::search::cascade::text_snippet(&query.pattern, text, &folded)
                    .filter(|snip| !snip.ranges.is_empty())
            }
            ContentTier::Fuzzy => match &self.fuzzy {
                Some(bitap) => {
                    crate::search::cascade::fuzzy_snippet(bitap, text, &folded).map(|(_, s)| s)
                }
                // The term does not fuzz, so a fuzzy row cannot be re-judged;
                // leaving it is the honest reading.
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
    // The row's own `stat` decided this was a file; by now it may be a FIFO,
    // and a blocking open on this thread costs every live row for the rest of
    // the session — and hangs exit, since `LiveWatcher::stop` joins here.
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

/// Fold one notify event into the pending decisions for this window.
///
/// Pure and filesystem-free, which is what makes the platform differences
/// testable: every shape below is a real emission from `notify` 6.1 on one
/// platform or the other, and the coalescing window is what reconciles them.
fn classify(
    event: &NotifyEvent,
    targets: &HashMap<String, Target>,
    pending: &mut HashMap<String, Op>,
    orphan_to: &mut Vec<PathBuf>,
) {
    use notify::event::{ModifyKind, RenameMode};

    let key = |p: &PathBuf| p.to_string_lossy().into_owned();
    let is_target = |p: &PathBuf| targets.contains_key(&key(p));

    match event.kind {
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // Linux pairs the halves and emits this *after* the From/To pair,
            // so it lands in the same window and overwrites the provisional
            // Gone recorded below.
            let (Some(from), Some(to)) = (event.paths.first(), event.paths.get(1)) else {
                return;
            };
            if is_target(from) {
                pending.insert(key(from), Op::Renamed(to.clone()));
            } else if is_target(to) {
                // The atomic-save shape: a temporary file renamed over a row
                // we are watching. The row did not move; its contents changed.
                pending.insert(key(to), Op::Changed);
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for path in &event.paths {
                if is_target(path) {
                    pending.insert(key(path), Op::Changed);
                } else {
                    orphan_to.push(path.clone());
                }
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            for path in &event.paths {
                if is_target(path) {
                    // Provisional; a Both in this same window upgrades it.
                    pending.entry(key(path)).or_insert(Op::Gone);
                }
            }
        }
        EventKind::Remove(_) => {
            for path in &event.paths {
                if is_target(path) {
                    pending.insert(key(path), Op::Gone);
                }
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in &event.paths {
                if is_target(path) {
                    // A Create at a watched path un-deletes the row.
                    pending.insert(key(path), Op::Changed);
                }
            }
        }
        // Access events, and anything for a path we are not showing. Live
        // results never *add* rows: we have no way to know an unrelated new
        // file matches the query, and guessing would be a second, unranked
        // search wearing the first one's clothes.
        _ => {}
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
