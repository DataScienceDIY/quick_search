//! Wiring between the egui thread and the core services. Every core thread
//! wakes the UI through `ctx.request_repaint()`, so polling is enough.
//!
//! The duplicates scan and the verification run on throwaway threads fired
//! by user actions. The verification can hold a large group's worth of file
//! handles and the scan reads a file per listed member, so each carries a
//! cancel flag and shutdown raises both.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::IndexCoordinator;
use quicksearch_core::live::{LiveUpdate, LiveWatcher};
use quicksearch_core::search::{
    find_duplicate_groups, split_by_tail, DuplicateGroup, SearchService, SearchUpdate,
};
use quicksearch_core::shutdown;
use quicksearch_core::verify::{verify_identical, VerifyUpdate};

/// A duplicate group being read through. The thread is detached; cancelling
/// is just raising the flag, noticed between chunks.
pub struct VerifyJob {
    pub rx: mpsc::Receiver<VerifyUpdate>,
    cancel: Arc<AtomicBool>,
}

impl VerifyJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A finished duplicates scan.
pub struct DupScan {
    pub groups: Vec<DuplicateGroup>,
    /// The fetch came back full at this many groups, so the index may hold
    /// more. Taken before the tail check, which can leave fewer listed.
    pub limited_to: Option<u32>,
}

/// A duplicates scan in flight. It sends one answer, once the tail check is
/// done, so no group reaches the tab on its head hash alone. Cancelling drops
/// the members the tail check has not reached, so an answer about to be
/// discarded lands sooner.
pub struct DupJob {
    pub rx: mpsc::Receiver<Result<DupScan, String>>,
    cancel: Arc<AtomicBool>,
}

impl DupJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Duplicate groups a scan is asked for. Every group listed costs one row
/// fetch and one member query to hydrate, and a file read per member for the
/// tail check, so this is a real cost, not a display cap.
pub const DUP_SCAN_LIMIT: u32 = 500;
/// …and the most the hidden set may add to it.
const DUP_HIDDEN_ALLOWANCE: u32 = 4_500;

/// The limit for a scan whose result `hidden` groups will be dropped from.
/// Hidden groups are groups the scan returns and the tab discards, so a flat
/// limit would quietly charge the user for every group they dismissed, the
/// opposite of what hiding one is for. Bounded, so a config with a million
/// hidden hashes cannot turn one scan into a full hydration of the index.
fn dup_scan_limit(hidden: usize) -> u32 {
    DUP_SCAN_LIMIT + (hidden.min(DUP_HIDDEN_ALLOWANCE as usize) as u32)
}

pub struct Backend {
    pub coordinator: Arc<IndexCoordinator>,
    pub search: Option<SearchService>,
    pub search_rx: mpsc::Receiver<SearchUpdate>,
    pub dup_job: Option<DupJob>,
    pub verify_job: Option<VerifyJob>,
    /// Watches the on-screen results; `None` only after [`Backend::shutdown`].
    pub live: Option<LiveWatcher>,
    /// The current watch's updates ([`LiveWatcher::watch`]); `None` while
    /// nothing is watched. Replaced or taken, it drops whatever the previous
    /// watch had not delivered, so no update lands on rows it was not for.
    pub live_rx: Option<mpsc::Receiver<LiveUpdate>>,
}

impl Backend {
    /// Delete the index and rebuild it. The search connection needs no
    /// release here: the delete has it let go first (`db::hold_index`). A
    /// running duplicates scan holds the file too, with nothing to release
    /// it; on Windows the delete waits for it to finish, within the
    /// coordinator's stop grace.
    pub fn rebuild_index(&self) {
        self.coordinator.rebuild_index();
    }

    /// Delete the index without rebuilding; see [`Backend::rebuild_index`].
    pub fn clear_index(&self) {
        self.coordinator.clear_index();
    }

    /// Delete the index and start nothing until [`Backend::rebuild_index`];
    /// the receiver gets the outcome. See [`IndexCoordinator::delete_index`].
    pub fn delete_index(&self) -> mpsc::Receiver<Result<(), String>> {
        self.coordinator.delete_index()
    }

    pub fn start(config: &Config, ctx: egui::Context) -> Result<Backend, String> {
        // eframe is reactive: a run the coordinator schedules on its own
        // would sit unseen behind a settled window until the pointer moved.
        let coord_ctx = ctx.clone();
        let coordinator = Arc::new(IndexCoordinator::start(
            config.clone(),
            Arc::new(move || coord_ctx.request_repaint()),
        )?);
        if let Err(e) = shutdown::install_signal_handler(coordinator.clone()) {
            quicksearch_core::log_warn!("failed to install signal handler: {}", e);
        }

        let repaint_ctx = ctx.clone();
        let (search, search_rx) = SearchService::new(
            config.resolved_database_path(),
            Arc::new(move || repaint_ctx.request_repaint()),
        );

        let live_ctx = ctx.clone();
        let live = LiveWatcher::start(Arc::new(move || live_ctx.request_repaint()));

        Ok(Backend {
            coordinator,
            search: Some(search),
            search_rx,
            dup_job: None,
            verify_job: None,
            live: Some(live),
            live_rx: None,
        })
    }

    /// Point the live watcher at the on-screen rows; empty `targets` clears.
    pub fn watch_live(
        &mut self,
        query: &str,
        mut targets: Vec<quicksearch_core::live::Target>,
        config: &Config,
    ) {
        let Some(live) = &self.live else { return };
        // The one file the watcher must never open is the index itself:
        // closing a descriptor on it cancels SQLite's locks process-wide. A
        // row for it from an older build can still be on screen.
        targets.retain(|t| !config.is_index_file(std::path::Path::new(&t.path)));
        self.live_rx = if targets.is_empty() {
            live.clear();
            None
        } else {
            Some(live.watch(query, targets, config))
        };
    }

    /// Bring the index in line with paths the live watcher just read, so it
    /// does not drift from what is on screen.
    pub fn reindex_live_paths(&self, paths: Vec<PathBuf>) {
        self.coordinator.update_paths(paths);
    }

    /// Drop every watch, and with them every undelivered update.
    pub fn clear_live(&mut self) {
        self.live_rx = None;
        if let Some(live) = &self.live {
            live.clear();
        }
    }

    /// `None` only after [`Backend::shutdown`].
    pub fn search(&self) -> Option<&SearchService> {
        self.search.as_ref()
    }

    /// Ignored while a scan is already running: a second one would re-read the
    /// whole hash index for an answer the first is about to produce.
    pub fn start_duplicates(&mut self, config: &Config, ctx: egui::Context) {
        if self.dup_job.is_some() {
            return;
        }
        let limit = dup_scan_limit(config.duplicates.hidden_groups.len());
        self.dup_job = Some(spawn_dup_scan(config.clone(), limit, ctx));
    }

    /// Hurry the running duplicates scan, if any, whose answer is about to be
    /// discarded. It still lands, and `dup_job` clears then.
    pub fn cancel_duplicates(&self) {
        if let Some(job) = &self.dup_job {
            job.cancel();
        }
    }

    /// Compare every member against the first, byte for byte, on a worker
    /// thread. Replaces any run already going.
    pub fn start_verify(&mut self, mut paths: Vec<PathBuf>, config: &Config, ctx: egui::Context) {
        if let Some(job) = &self.verify_job {
            job.cancel();
        }
        // See `watch_live` for why the index must not be opened.
        paths.retain(|p| !config.is_index_file(p));
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            verify_identical(&paths, &worker_cancel, &mut |update| {
                // A closed receiver means the app moved on; the cancel flag
                // stops the work.
                let _ = tx.send(update);
                ctx.request_repaint();
            });
        });
        self.verify_job = Some(VerifyJob { rx, cancel });
    }

    pub fn cancel_verify(&mut self) {
        if let Some(job) = self.verify_job.take() {
            job.cancel();
        }
    }

    /// Stop every core thread. The live watcher and the coordinator each get
    /// [`shutdown::EXIT_GRACE`] and are then left running, so a stuck thread
    /// holds the closing window about a second, never indefinitely.
    pub fn shutdown(&mut self) {
        self.cancel_verify();
        self.cancel_duplicates();
        if let Some(search) = self.search.take() {
            search.shutdown();
        }
        if let Some(mut live) = self.live.take() {
            live.stop();
        }
        self.coordinator.shutdown();
    }
}

/// Run [`scan_duplicates`] on a thread of its own, which sends its answer and
/// nothing else.
fn spawn_dup_scan(config: Config, limit: u32, ctx: egui::Context) -> DupJob {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    std::thread::spawn(move || {
        let _ = tx.send(scan_duplicates(&config, limit, &worker_cancel));
        ctx.request_repaint();
    });
    DupJob { rx, cancel }
}

/// One duplicates scan, as its thread runs it: the `limit` groups wasting the
/// most, then the tail check on just those. Groups it dissolves are not
/// replaced, so fewer than `limit` may be listed.
fn scan_duplicates(config: &Config, limit: u32, cancel: &AtomicBool) -> Result<DupScan, String> {
    let db = config.resolved_database_path();
    let groups = find_duplicate_groups(&db.to_string_lossy(), limit)?;
    let limited_to = (groups.len() as u32 >= limit).then_some(limit);
    // See `watch_live` for why the index must not be opened.
    let groups = split_by_tail(
        groups,
        config.processing.hash_length,
        |path| config.is_index_file(path),
        cancel,
    );
    Ok(DupScan { groups, limited_to })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use quicksearch_core::indexing::{IndexingService, IndexingStatus};
    use quicksearch_core::search::duplicates::Member;
    use quicksearch_core::testutil::{touch, wait_for, Scratch};

    /// Each group's member names, in listed order.
    fn names<'a>(groups: impl IntoIterator<Item = &'a Vec<Member>>) -> Vec<Vec<&'a str>> {
        groups
            .into_iter()
            .map(|members| members.iter().map(|m| m.1.as_str()).collect())
            .collect()
    }

    fn listed(groups: &[DuplicateGroup]) -> Vec<Vec<&str>> {
        names(groups.iter().map(|g| &g.members))
    }

    /// An indexed root holding two pairs whose members share a head: `fake_*`,
    /// whose tails differ, and `real_*`, real copies. The scratch guards come
    /// back with the config.
    fn indexed_pairs(tag: &str) -> (Scratch, Scratch, Config) {
        let root = Scratch::canonical(&format!("{tag}-root"));
        let (db_dir, db) = Scratch::db(&format!("{tag}-db"));
        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config.paths.database_path = db.to_string_lossy().into_owned();
        config.processing.hash_length = 1024;
        // The biggest waste by head alone, but the tails differ.
        let mut fake = vec![b'f'; 4000];
        touch(&root.join("fake_a.bin"), &fake);
        fake[3999] = b'g';
        touch(&root.join("fake_b.bin"), &fake);
        touch(&root.join("real_a.bin"), &[b'r'; 3000]);
        touch(&root.join("real_b.bin"), &[b'r'; 3000]);

        let service = IndexingService::new();
        service
            .start_indexing(
                config.paths.indexing_paths.clone(),
                config.paths.database_path.clone(),
                config.clone(),
            )
            .unwrap();
        wait_for("the run to end", Duration::from_secs(120), || {
            matches!(
                service.get_status(),
                IndexingStatus::Idle | IndexingStatus::Error(_)
            )
        });
        assert!(matches!(service.get_status(), IndexingStatus::Idle));
        let head_only = find_duplicate_groups(&config.paths.database_path, 10).unwrap();
        assert_eq!(
            names(head_only.iter().map(|g| &g.members)),
            [["fake_a.bin", "fake_b.bin"], ["real_a.bin", "real_b.bin"]],
            "the head alone groups both pairs"
        );
        (root, db_dir, config)
    }

    /// The tail check reads only the groups the fetch returned: one it
    /// dissolves leaves a gap rather than pulling in the next group.
    #[test]
    fn the_scan_tail_checks_only_the_groups_it_fetched() {
        let (_root, _db_dir, config) = indexed_pairs("dup-scan");
        let cancel = AtomicBool::new(false);
        let top = scan_duplicates(&config, 1, &cancel).unwrap();
        assert!(
            top.groups.is_empty(),
            "the one group fetched was dissolved and not replaced: {:?}",
            listed(&top.groups)
        );
        assert_eq!(top.limited_to, Some(1), "the fetch itself was full");

        let all = scan_duplicates(&config, 10, &cancel).unwrap();
        assert_eq!(listed(&all.groups), [["real_a.bin", "real_b.bin"]]);
        assert_eq!(all.limited_to, None);
    }

    /// The scan thread sends the tab one answer, the tail-checked one, and
    /// then hangs up: no head-only group is ever on the channel, even for a
    /// moment, and a cancelled scan lists nothing it did not check.
    #[test]
    fn nothing_reaches_the_tab_before_the_tail_check() {
        let (_root, _db_dir, config) = indexed_pairs("dup-send");
        let head_only = find_duplicate_groups(&config.paths.database_path, 10).unwrap();
        let checked = split_by_tail(
            head_only,
            config.processing.hash_length,
            |_| false,
            &AtomicBool::new(false),
        );

        let job = spawn_dup_scan(config.clone(), 10, egui::Context::default());
        let first = job.rx.recv_timeout(Duration::from_secs(60));
        let sent = first.expect("the scan sent nothing").unwrap();
        assert_eq!(sent.groups, checked, "what was sent was not checked");
        assert_eq!(listed(&sent.groups), [["real_a.bin", "real_b.bin"]]);
        assert!(
            matches!(
                job.rx.recv_timeout(Duration::from_secs(60)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "the scan sent a second message"
        );

        let cancelled = scan_duplicates(&config, 10, &AtomicBool::new(true)).unwrap();
        assert!(
            cancelled.groups.is_empty(),
            "a cancelled scan listed what it never read: {:?}",
            listed(&cancelled.groups)
        );
    }
}
