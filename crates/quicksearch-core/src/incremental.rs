//! Incremental single-path index updates from watcher events and live paths.
//! [`upsert_path`] applies the walk's scope: the canonical path must lie
//! under one configured root ([`Scope::root_of`]), pass [`Scope::covers`]
//! below it, and not be one of the index's own files. Removals are not
//! filtered; they can only delete rows, and only those the disk no longer
//! holds ([`remove_path`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::Transaction;

use crate::config::Config;
use crate::db::repo;
use crate::extract::sandbox::KillMark;
use crate::extract::{Registry, Scratch};
use crate::file_handling::{
    classify, db_key_for_missing_path, extract_and_store, filtered_walk, path_to_db_string,
    prepare_file_record, warn_if_unrepresentable, FileIndexAction, RootRange, Stamp,
    UnreadableDirs,
};
use crate::scope::Scope;

/// How much of one directory walk may be applied in this turn, and where the
/// last turn stopped. One event is not always one file: a moved-in directory
/// arrives as a single `Create` covering everything beneath it. This runs on
/// the coordinator's own thread, so an unbounded call would leave the command
/// loop reading no commands, including the shutdown a closing window waits on.
/// One file still blocks: the budget is checked between files, so a slow
/// extraction or a `stat` on a hung mount can outlast the shutdown's grace
/// ([`crate::shutdown::EXIT_GRACE`]), and exit leaves the thread behind.
pub struct Budget<'a> {
    /// Checked after each file, never before the first: every call writes at
    /// least one, so a turn handed a spent budget still makes progress.
    pub deadline: Instant,
    /// Checked before each file: once set, the call writes nothing more.
    pub cancel: &'a AtomicBool,
    /// Read before the caller's stop check: a sandbox child this call
    /// spawns dies at once if a kill ran since ([`KillMark`]). The
    /// coordinator reads one per turn; one-off callers read
    /// [`KillMark::now`].
    pub kills: KillMark,
    /// An earlier turn of this event was cut short. A first turn walks a
    /// directory in the order it lists, unsorted; a resumed one walks it in
    /// `Path` order, the order `after` can resume.
    pub resumed: bool,
    /// The last path an earlier turn wrote for this event; the walk resumes
    /// after it. Read only when `resumed`.
    pub after: Option<PathBuf>,
}

/// Whether an event was applied in full, or ran out of budget partway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Done,
    /// Budget spent. What was written stays written; the caller should
    /// re-queue the same path, resumed, with `after` as its next
    /// [`Budget::after`]. `None` after an unsorted first turn, which has no
    /// order to resume in: the next turn starts over, sorted, and finds what
    /// this one wrote unchanged.
    Unfinished {
        after: Option<PathBuf>,
    },
}

/// Bring the index in step with `path`, inside the caller's transaction,
/// which may hold many paths. A missing path is a no-op (its `Remove`
/// resolves it); a file whose stamp matches its row ([`classify`])
/// short-circuits without being opened.
///
/// A directory is walked only with `walk` (a `Create`: a new or moved-in
/// tree). Its own `Modify` (a `chmod`, a `touch`, an xattr, and on Windows
/// any change among its entries) is a no-op, since each of its files reports
/// its own changes. The cost: after a directory's own permission or
/// hidden-attribute change (`chmod +r`, `attrib -h`), its subtree waits for
/// the periodic run. See [`Budget`] for the one call that is not small.
pub fn upsert_path(
    tx: &Transaction<'_>,
    path: &Path,
    walk: bool,
    config: &Config,
    scope: &Scope,
    registry: &Registry,
    budget: &Budget<'_>,
) -> Result<Applied, String> {
    // A link is never followed, as the walk follows none: its target is
    // indexed where the walk meets it, if it meets it at all.
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(Applied::Done);
    };
    if meta.file_type().is_symlink() || (meta.is_dir() && !walk) {
        return Ok(Applied::Done);
    }
    // Judged where the walk meets the path: canonical, against canonical
    // roots. A root that is itself a link lies under its canonical spelling
    // with everything below it; a link *below* a root (an ancestor linked in
    // after the event) leads outside every root, as does an event still
    // queued under a root since removed.
    let Ok(canonical) = path.canonicalize() else {
        return Ok(Applied::Done);
    };
    // A path the index cannot spell is "nothing indexable here", not an
    // error: an error would set `needs_full_run`, and a full run screens the
    // same subtree out for the same reason, so the reindex would fix nothing.
    if canonical.to_str().is_none() {
        return Ok(Applied::Done);
    }
    let key = path_to_db_string(&canonical);
    let path = Path::new(&key);
    // Tested below the root only, as the walk tests it: `~/venv/proj` is a
    // root like any other.
    let Some(root) = scope.root_of(path) else {
        return Ok(Applied::Done);
    };
    if !scope.covers(root, path) {
        return Ok(Applied::Done);
    }
    // One for the whole call, as a content worker keeps one per pass.
    let mut scratch = Scratch::since(config, budget.kills);
    if meta.is_dir() {
        // Resumed by path: the walk prunes everything up to `after` rather
        // than re-walking it, and a file added or removed since the last turn
        // shifts nothing. Each resumed turn still lists the directories along
        // the resume path. A resume by count was cheaper and wrong. The
        // sibling sort that `Path` order needs measured 1.17 → 1.27 ms
        // (`benches/index.rs`, `directory_event::unchanged`, 500 files) when
        // every turn paid it. Only a resumed turn does now, and most events
        // fit in one: 1.26 → 1.14 ms fastest, 1.33 → 1.27 ms median there
        // (five alternating A/B rounds on a loaded box; `modified` stayed at
        // about 0.7 µs).
        let sorted = budget.resumed;
        let failures = UnreadableDirs::default();
        let mut entries = filtered_walk(
            &key,
            config.indexing.include_hidden,
            scope.ignore(),
            &failures,
            budget.after.as_deref().filter(|_| sorted),
            sorted,
        )
        .peekable();
        let mut written = None;
        while let Some(entry) = entries.next() {
            if budget.cancel.load(Ordering::Relaxed) {
                let after = written.or_else(|| budget.after.clone());
                return Ok(Applied::Unfinished {
                    after: after.filter(|_| sorted),
                });
            }
            upsert_file(tx, entry.path(), config, registry, &mut scratch)?;
            // Peeked so that the turn writing the last file reports Done,
            // rather than costing one more turn that finds nothing.
            if Instant::now() >= budget.deadline && entries.peek().is_some() {
                return Ok(Applied::Unfinished {
                    after: Some(entry.into_path()).filter(|_| sorted),
                });
            }
            written = Some(entry.into_path());
        }
        Ok(Applied::Done)
    } else {
        upsert_file(tx, path, config, registry, &mut scratch).map(|()| Applied::Done)
    }
}

/// Run `write` all or nothing inside the caller's transaction: a file whose
/// writes fail part-way must not keep a row that claims text its posting
/// never got.
fn atomically(
    tx: &Transaction<'_>,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    tx.execute_batch("SAVEPOINT one_path")
        .map_err(|e| format!("begin incremental savepoint: {}", e))?;
    match write() {
        Ok(()) => tx
            .execute_batch("RELEASE one_path")
            .map_err(|e| format!("release incremental savepoint: {}", e)),
        Err(e) => {
            let _ = tx.execute_batch("ROLLBACK TO one_path; RELEASE one_path");
            Err(e)
        }
    }
}

fn upsert_file(
    tx: &Transaction<'_>,
    path: &Path,
    config: &Config,
    registry: &Registry,
    scratch: &mut Scratch,
) -> Result<(), String> {
    let Ok(canonical) = path.canonicalize() else {
        return Ok(());
    };
    // Never opened, let alone indexed (see `index_file_set`): with the
    // database under a root, each commit's `-wal` write is an event, and
    // indexing it would be the next commit. Checked here, before any open,
    // so a directory event's walk is screened too.
    if config.is_index_file(&canonical) {
        return Ok(());
    }
    if warn_if_unrepresentable(&canonical) {
        return Ok(());
    }
    let db_path = path_to_db_string(&canonical);
    let Ok(meta) = std::fs::metadata(&canonical) else {
        return Ok(());
    };

    // Settled before the file is opened, by the stamp the full walk compares:
    // an unchanged file (a re-sent event, a `chmod`, the live view re-checking
    // a row it shows) costs one lookup, never a read and a hash. Measured
    // (`benches/index.rs`, `directory_event::unchanged`, 500 small text
    // files): 3.3 ms hashing and sniffing each against 1.4 ms. A size change
    // within the stored second is a change, as the live view reports it;
    // reading the whole stamp rather than the mtime alone measured 1.12 →
    // 1.16 ms there.
    let stored = repo::stamp_for_path(tx, &db_path)?;
    if classify(stored, Stamp::of(&meta)) == FileIndexAction::Skip {
        return Ok(());
    }
    let Some(rec) = prepare_file_record(&db_path, &meta, config, registry, scratch) else {
        return Ok(());
    };
    atomically(tx, || {
        let written = match stored {
            Some(_) => repo::update_file_basic(tx, &rec.as_new_file())?,
            None => repo::insert_file(tx, &rec.as_new_file())?,
        };
        // `None`: lost a race with another writer; the row that won is
        // current enough.
        let Some(file_id) = written else {
            return Ok(());
        };

        // A row that needs no content is already NA: both writers above set
        // its state from `rec`. That gate is also the only size gate on this
        // path: `decide_content` has none, and falling through would read a
        // multi-gigabyte `.txt` whole into memory.
        if rec.needs_content {
            match rec.inline_text.as_deref() {
                // `prepare_file_record` already read the whole file.
                Some(text) => {
                    let zstd = repo::encode_one(text)?;
                    repo::store_pending_text(tx, file_id, text, zstd.as_deref())?;
                }
                None => {
                    extract_and_store(tx, file_id, rec.path(), rec.mime, registry, config, scratch)?
                }
            }
        }
        Ok(())
    })
}

/// Delete what `path` no longer holds, inside the caller's transaction: its
/// row unless a regular file is there now, and everything indexed beneath it
/// unless a directory is. What the disk still holds is left to its own
/// `Create` or `Modify`, the live view or the next full run. A path that
/// cannot be stat'ed keeps everything, as the walk keeps an unreadable
/// entry's rows; so does the old name after a case-only rename on a
/// case-insensitive volume, which still stats as the file.
///
/// With `rewalk` (a `Create` for the same path is queued behind this), the
/// stat is skipped and the row and the subtree both go: the walk that
/// follows re-derives whatever is real, and a directory that is back
/// (`mv build build.old; mkdir build`) would otherwise keep the old tree's
/// rows. The caller runs that walk's first turn in the same transaction, so
/// a queue dropped before the walk drops this delete too.
///
/// Pays off only when the caller has reduced its removals to their *roots*:
/// then `rm -rf dir/` is a fixed handful of range-driven statements
/// ([`repo::delete_subtree`]) rather than five per file.
pub fn remove_path(tx: &Transaction<'_>, path: &Path, rewalk: bool) -> Result<(), String> {
    // A path the index cannot spell was never indexed, so there is nothing
    // to delete; its lossy spelling names another file (see
    // `path_to_db_string`), whose row and subtree this would delete.
    if path.to_str().is_none() {
        return Ok(());
    }
    let (row, subtree) = if rewalk {
        (true, true)
    } else {
        // Resolved as the key is: ancestors followed, the path itself not. A
        // row at the key can be live only if this sees a regular file, rows
        // beneath it only if it sees a directory. Respelled from components
        // first: the OS reads `f/` and `f/.` as the directory `f`, which a
        // regular file is not, where the key reads them as `f`.
        let spelled: PathBuf = path.components().collect();
        match std::fs::symlink_metadata(&spelled).map_err(|e| e.kind()) {
            Ok(m) => (!m.is_file(), !m.is_dir()),
            Err(std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => (true, true),
            // Cannot look, so the delete cannot be confirmed.
            Err(_) => return Ok(()),
        }
    };
    // The stored key is canonicalized, but the path may be gone or now a
    // link, so `canonicalize` cannot be trusted with it;
    // `db_key_for_missing_path` resolves only its ancestors.
    let key = db_key_for_missing_path(path);
    atomically(tx, || {
        if row {
            repo::delete_file_by_path(tx, &key)?;
        }
        if subtree {
            repo::delete_subtree(tx, &RootRange::for_root(&key))?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IgnoreSet;
    use crate::db::open_or_recreate;
    use crate::watcher::FsEvent;
    use rusqlite::{Connection, OptionalExtension};
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        conn: Connection,
        dir: std::path::PathBuf,
        db: std::path::PathBuf,
        config: Config,
        ignore: Arc<IgnoreSet>,
        registry: Registry,
    }

    impl Fixture {
        /// `tree/` as the only root; the database sits beside it.
        fn new() -> Fixture {
            let scratch = crate::testutil::scratch_dir("incr");
            let dir = scratch.join("tree");
            std::fs::create_dir_all(&dir).unwrap();
            let db = scratch.join("index.sqlite");
            let conn = open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
            let mut config = Config::default();
            config.paths.indexing_paths = vec![dir.to_string_lossy().into_owned()];
            let ignore = Arc::new(IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap());
            Fixture {
                conn,
                dir,
                db,
                config,
                ignore,
                registry: Registry::default_set(),
            }
        }

        /// Built per apply, as the coordinator builds one per turn, so a
        /// test's edits to `config` take effect.
        fn scope(&self) -> Scope {
            Scope::new(&self.config, self.ignore.clone())
        }

        fn apply(&mut self, event: &FsEvent) {
            let done = self.apply_within(event, Duration::from_secs(3600), None);
            assert_eq!(done, Applied::Done, "unexpectedly ran out of budget");
        }

        /// Resumed after `after` if there is one, else a first turn.
        fn apply_within(
            &mut self,
            event: &FsEvent,
            budget: Duration,
            after: Option<PathBuf>,
        ) -> Applied {
            self.apply_with(
                event,
                &Budget {
                    deadline: Instant::now() + budget,
                    cancel: &AtomicBool::new(false),
                    kills: KillMark::now(),
                    resumed: after.is_some(),
                    after,
                },
            )
        }

        /// `event` as the coordinator's queue applies it: a `Create` walks a
        /// directory and a `Modify` does not; a `Rename` is a removal and a
        /// `Create`.
        fn apply_with(&mut self, event: &FsEvent, budget: &Budget<'_>) -> Applied {
            let scope = self.scope();
            let tx = self.conn.transaction().unwrap();
            let upsert = |path: &Path, walk| {
                upsert_path(
                    &tx,
                    path,
                    walk,
                    &self.config,
                    &scope,
                    &self.registry,
                    budget,
                )
            };
            let applied = match event {
                FsEvent::Create(p) => upsert(p, true),
                FsEvent::Modify(p) => upsert(p, false),
                FsEvent::Remove(p) => remove_path(&tx, p, false).map(|()| Applied::Done),
                FsEvent::Rename { from, to } => {
                    remove_path(&tx, from, false).and_then(|()| upsert(to, true))
                }
            }
            .unwrap();
            tx.commit().unwrap();
            applied
        }

        /// One spent-budget turn of `Create(dir)`, resumed, so sorted, after
        /// `after` or from the start: the resume point it reports, or `None`
        /// once the walk is done.
        fn spent_turn(&mut self, dir: &Path, after: Option<PathBuf>) -> Option<PathBuf> {
            match self.spent(dir, Some(after)) {
                Applied::Done => None,
                Applied::Unfinished { after } => {
                    Some(after.expect("a resumed turn says where it stopped"))
                }
            }
        }

        /// One spent-budget turn of `Create(dir)`: a first turn for `None`,
        /// else resumed after the path inside, if any, as the coordinator's
        /// `resume_from` records it.
        fn spent(&mut self, dir: &Path, resume: Option<Option<PathBuf>>) -> Applied {
            let budget = Budget {
                deadline: Instant::now(),
                cancel: &AtomicBool::new(false),
                kills: KillMark::now(),
                resumed: resume.is_some(),
                after: resume.flatten(),
            };
            self.apply_with(&FsEvent::Create(dir.to_path_buf()), &budget)
        }

        /// Spent-budget turns of `Create(dir)` until it is done, the first
        /// unsorted and the rest resumed, asserting that none writes more
        /// than one new row; the number of turns. A spent turn walks one
        /// entry, so the first resumed turn to meet the file the first turn
        /// wrote, unchanged, is the one that writes none.
        fn turns_to_finish(&mut self, dir: &Path) -> usize {
            let (mut turns, mut resume) = (0, None);
            loop {
                let (before, resumed) = (self.counts().0, resume.is_some());
                let applied = self.spent(dir, resume.take());
                turns += 1;
                let wrote = self.counts().0 - before;
                assert!(wrote <= 1, "turn {turns} wrote {wrote} rows");
                match applied {
                    Applied::Done => return turns,
                    Applied::Unfinished { after } => {
                        assert!(!resumed || after.is_some(), "turn {turns} lost its place");
                        resume = Some(after);
                    }
                }
            }
        }

        fn write(&self, name: &str, content: &str) -> std::path::PathBuf {
            let p = self.dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }

        /// The key the index actually stores: through `path_to_db_string`, or
        /// every lookup here misses the `\\?\`-stripped spelling on Windows.
        fn canonical(&self, p: &Path) -> String {
            crate::file_handling::path_to_db_string(&p.canonicalize().unwrap())
        }

        fn row(&self, path: &str) -> Option<(i64, i64, i64)> {
            self.conn
                .query_row(
                    "SELECT id, mtime, content_state FROM files WHERE parent || name = ?1",
                    rusqlite::params![path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .unwrap()
        }

        fn counts(&self) -> (i64, i64, i64) {
            let files = self
                .conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap();
            let fts = self
                .conn
                .query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r.get(0))
                .unwrap();
            let texts = self
                .conn
                .query_row("SELECT COUNT(*) FROM documents_text", [], |r| r.get(0))
                .unwrap();
            (files, fts, texts)
        }

        fn fts_hits(&self, term: &str) -> i64 {
            crate::testutil::fts_count(&self.conn, term)
        }

        /// Every stored path, sorted.
        fn keys(&self) -> Vec<String> {
            let mut stmt = self
                .conn
                .prepare("SELECT parent || name FROM files")
                .unwrap();
            let mut keys: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            keys.sort();
            keys
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
            std::fs::remove_file(&self.db).ok();
        }
    }

    /// A resume point past every entry (the files it followed were removed
    /// since) indexes nothing and finishes, rather than starting over.
    #[test]
    fn resuming_past_the_end_indexes_nothing() {
        let mut f = Fixture::new();
        for i in 0..3 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        let past = Some(PathBuf::from(f.canonical(&sub)).join("zzz"));
        let outcome = f.apply_within(
            &FsEvent::Create(sub.clone()),
            Duration::from_secs(3600),
            past,
        );
        assert_eq!(outcome, Applied::Done);
        assert_eq!(f.counts().0, 0);

        f.apply(&FsEvent::Create(sub));
        assert_eq!(f.counts().0, 3);
    }

    /// A sliced directory event resumes after the last path it wrote; earlier
    /// entries are pruned, not re-walked. Asserted by resume point rather than
    /// by clock, which says nothing reliable on a loaded CI runner.
    #[test]
    fn a_spent_budget_writes_one_file_and_the_next_turn_resumes_after_it() {
        let mut f = Fixture::new();
        for i in 0..4 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        let after = f.spent_turn(&sub, None).expect("three files left");
        assert_eq!(f.keys(), vec![after.to_string_lossy().into_owned()]);
        // Changed at the resume point: walked again, it would be re-extracted.
        std::fs::write(&after, "a longer body naming a zebra").unwrap();

        let outcome = f.apply_within(
            &FsEvent::Create(sub),
            Duration::from_secs(3600),
            Some(after),
        );
        assert_eq!(outcome, Applied::Done);
        assert_eq!(f.counts().0, 4);
        assert_eq!(f.fts_hits("zebra"), 0, "the resume point was walked again");
    }

    /// Every turn walks at least one file, so a queue handed spent budgets
    /// still drains, and no file is written twice. The unsorted first turn
    /// has no resume point, so the resumed turns walk every file once more
    /// in order: one turn more than there are files.
    #[test]
    fn spent_turns_write_each_file_exactly_once() {
        let mut f = Fixture::new();
        for i in 0..5 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        assert_eq!(f.turns_to_finish(&f.dir.join("sub")), 6);
        assert_eq!(f.counts(), (5, 5, 5));
    }

    /// The walk and the resume test agree on order: `Path` order, component
    /// by component. As strings, `a-b/y` sorts before `a/x` (`-` is below
    /// `/`), and a string comparison would prune it as already written.
    #[test]
    fn names_that_sort_differently_as_strings_are_each_written_once() {
        let mut f = Fixture::new();
        for name in ["a.b", "a/x", "a-b/y"] {
            f.write(name, "body");
        }
        assert_eq!(f.turns_to_finish(&f.dir.clone()), 4);
        assert_eq!(f.counts().0, 3);
    }

    /// A first turn walks unsorted and, cut short, reports no resume point;
    /// the next turn walks sorted from the start and passes over what the
    /// first wrote, unchanged, without touching its row.
    #[test]
    fn a_first_turn_cut_short_resumes_sorted_from_the_start() {
        let mut f = Fixture::new();
        for i in 0..3 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        assert_eq!(f.spent(&sub, None), Applied::Unfinished { after: None });
        let first = f.keys();
        assert_eq!(first.len(), 1);
        let row = f.row(&first[0]);
        let outcome = f.apply_with(
            &FsEvent::Create(sub.clone()),
            &Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &AtomicBool::new(false),
                kills: KillMark::now(),
                resumed: true,
                after: None,
            },
        );
        assert_eq!(outcome, Applied::Done);
        assert_eq!(f.counts(), (3, 3, 3));
        assert_eq!(f.row(&first[0]), row);
    }

    #[test]
    fn a_resume_point_deleted_between_turns_still_resumes_after_it() {
        let mut f = Fixture::new();
        for i in 0..4 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");
        let after = f.spent_turn(&sub, None).expect("three files left");
        std::fs::remove_file(&after).unwrap();

        let outcome = f.apply_within(
            &FsEvent::Create(sub),
            Duration::from_secs(3600),
            Some(after),
        );
        assert_eq!(outcome, Applied::Done);
        assert_eq!(f.counts().0, 4, "the three after it, and its own row");
    }

    /// A count would now skip one file too few and write the resume point
    /// again; a path resumes where it was. The new file waits for its own
    /// event or the periodic run.
    #[test]
    fn a_file_added_before_the_resume_point_shifts_nothing() {
        let mut f = Fixture::new();
        let names = ["f1.txt", "f2.txt", "f3.txt"].map(|n| f.write(&format!("sub/{n}"), "body"));
        let sub = f.dir.join("sub");

        let mut after = f.spent_turn(&sub, None);
        f.write("sub/f0.txt", "added before the resume point");
        let mut written = vec![f.keys()];
        while after.is_some() {
            after = f.spent_turn(&sub, after);
            written.push(f.keys());
        }
        let want: Vec<Vec<String>> = (1..=3)
            .map(|n| names[..n].iter().map(|p| f.canonical(p)).collect())
            .collect();
        assert_eq!(written, want, "one file per turn, in order, none twice");
    }

    /// Cancelled before its first file, a turn writes nothing and hands back
    /// the resume point it was given.
    #[test]
    fn a_cancelled_budget_writes_nothing() {
        let mut f = Fixture::new();
        for i in 0..3 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = PathBuf::from(f.canonical(&f.dir.join("sub")));
        let cancel = AtomicBool::new(true);
        for after in [None, Some(sub.join("f0.txt"))] {
            let budget = Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &cancel,
                kills: KillMark::now(),
                resumed: after.is_some(),
                after: after.clone(),
            };
            let outcome = f.apply_with(&FsEvent::Create(sub.clone()), &budget);
            assert_eq!(outcome, Applied::Unfinished { after });
            assert_eq!(f.counts().0, 0);
        }
    }

    /// A directory's own `Modify` (a `chmod`, a `touch`) walks nothing; its
    /// files report their own changes, and a `Create` still walks it.
    #[test]
    fn a_directory_modify_does_not_walk_its_tree() {
        let mut f = Fixture::new();
        f.write("sub/a.txt", "alpha");
        f.write("sub/deep/b.txt", "beta");
        let sub = f.dir.join("sub");

        f.apply(&FsEvent::Modify(sub.clone()));
        f.apply(&FsEvent::Modify(f.dir.clone()));
        assert_eq!(f.counts(), (0, 0, 0), "a directory Modify walked its tree");

        f.apply(&FsEvent::Create(sub));
        assert_eq!(f.counts().0, 2);
    }

    /// `mv d d.old; mkdir d`: `d` is a directory again, so the stat alone
    /// keeps the old tree's rows. With its `Create` queued behind the
    /// removal, `rewalk` drops them, and the walk writes what is there now.
    #[test]
    fn a_rewalk_removal_drops_the_old_tree_of_a_directory_that_is_back() {
        let mut f = Fixture::new();
        f.write("d/x.txt", "x");
        f.write("d/sub/y.txt", "y");
        let d = f.dir.join("d");
        f.apply(&FsEvent::Create(d.clone()));
        let old = f.keys();
        assert_eq!(old.len(), 2);

        std::fs::remove_dir_all(&d).unwrap();
        let fresh = f.write("d/new.txt", "the directory that is back");
        let remove = |f: &mut Fixture, rewalk| {
            let tx = f.conn.transaction().unwrap();
            remove_path(&tx, &d, rewalk).unwrap();
            tx.commit().unwrap();
        };
        remove(&mut f, false);
        assert_eq!(
            f.keys(),
            old,
            "without rewalk the stat keeps a directory's subtree"
        );
        remove(&mut f, true);
        assert_eq!(f.counts(), (0, 0, 0));

        f.apply(&FsEvent::Create(d.clone()));
        assert_eq!(f.keys(), vec![f.canonical(&fresh)]);
    }

    #[test]
    fn collapsed_removal_deletes_the_same_rows_as_the_full_set() {
        let mut f = Fixture::new();
        f.write("tree/a.txt", "alpha");
        f.write("tree/deep/b.txt", "beta");
        f.write("tree2/keep.txt", "survivor");
        let tree = f.dir.join("tree");
        f.apply(&FsEvent::Create(tree.clone()));
        f.apply(&FsEvent::Create(f.dir.join("tree2")));
        assert_eq!(f.counts().0, 3);

        let canonical_tree = f.canonical(&tree);
        std::fs::remove_dir_all(&tree).unwrap();

        let reported: Vec<std::path::PathBuf> = vec![
            canonical_tree.clone().into(),
            format!("{}/deep", canonical_tree).into(),
            format!("{}/a.txt", canonical_tree).into(),
            format!("{}/deep/b.txt", canonical_tree).into(),
        ];
        // Collapsed to its root, as the coordinator's `collapse_pending_removals` does.
        let roots = vec![reported[0].clone()];
        let tx = f.conn.transaction().unwrap();
        for root in &roots {
            remove_path(&tx, root, false).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(f.counts(), (1, 1, 1), "only tree2 survives");
        let survivor = f.canonical(&f.dir.join("tree2").join("keep.txt"));
        assert!(f.row(&survivor).is_some());
    }

    #[test]
    fn create_indexes_file_and_content() {
        let mut f = Fixture::new();
        let p = f.write("hello.txt", "greetings earthling");
        f.apply(&FsEvent::Create(p.clone()));

        let canonical = f.canonical(&p);
        let (_, _, content_state) = f.row(&canonical).expect("row exists");
        assert_eq!(content_state, repo::STATE_DONE);
        assert_eq!(f.counts(), (1, 1, 1), "files + FTS + text all written");
        assert_eq!(f.fts_hits("earthling"), 1);
    }

    #[test]
    fn modify_with_same_mtime_is_noop_and_changed_mtime_reextracts() {
        let mut f = Fixture::new();
        let p = f.write("doc.txt", "first version");
        f.apply(&FsEvent::Create(p.clone()));
        let canonical = f.canonical(&p);
        let (id1, mtime1, _) = f.row(&canonical).unwrap();

        f.apply(&FsEvent::Modify(p.clone()));
        let (id2, mtime2, _) = f.row(&canonical).unwrap();
        assert_eq!((id1, mtime1), (id2, mtime2));

        std::fs::write(&p, "second edition entirely").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let file = std::fs::File::options().write(true).open(&p).unwrap();
        file.set_modified(newer).unwrap();
        drop(file);
        f.apply(&FsEvent::Modify(p.clone()));
        assert_eq!(f.fts_hits("edition"), 1);
        assert_eq!(f.fts_hits("version"), 0, "stale tokens removed");
        assert_eq!(f.counts(), (1, 1, 1), "still exactly one of everything");
    }

    /// A replacement that keeps the old mtime (`cp -p`, an unpacked archive)
    /// but not the old size is a change.
    #[test]
    fn a_replacement_carrying_the_old_mtime_is_reextracted() {
        let mut f = Fixture::new();
        let p = f.write("doc.txt", "first version");
        let old = std::fs::metadata(&p).unwrap().modified().unwrap();
        f.apply(&FsEvent::Create(p.clone()));

        std::fs::write(&p, "second edition entirely").unwrap();
        crate::testutil::set_mtime(&p, old).unwrap();
        f.apply(&FsEvent::Modify(p.clone()));
        assert_eq!(f.fts_hits("edition"), 1);
        assert_eq!(f.fts_hits("version"), 0, "stale tokens removed");
        assert_eq!(f.counts(), (1, 1, 1), "still exactly one of everything");
    }

    /// A time before 1970 clamps to 0 rather than costing the file its row.
    #[test]
    fn a_pre_1970_file_is_indexed_by_the_watcher_path() {
        let mut f = Fixture::new();
        let p = f.write("old.txt", "punched cards");
        if !crate::testutil::backdate_before_1970(&p) {
            return; // The filesystem keeps no pre-1970 time; nothing to test.
        }
        f.apply(&FsEvent::Create(p.clone()));
        let (_, mtime, state) = f.row(&f.canonical(&p)).expect("indexed");
        assert_eq!((mtime, state), (0, repo::STATE_DONE));
        assert_eq!(f.fts_hits("punched"), 1);
    }

    /// An unchanged file is settled from its stored stamp, as the full walk
    /// settles it, without being opened. Here it cannot be opened, so an
    /// attempt would log a hash failure naming it.
    #[cfg(unix)]
    #[test]
    fn an_unchanged_file_is_not_opened() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new();
        let p = f.write("sealed.txt", "sphinx of black quartz");
        f.apply(&FsEvent::Create(p.clone()));
        let key = f.canonical(&p);
        let before = f.row(&key).expect("indexed");

        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads it anyway, and then there is nothing to prove.
        let readable = std::fs::File::open(&p).is_ok();
        f.apply(&FsEvent::Modify(p.clone()));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(f.row(&key), Some(before), "the row is untouched");
        if !readable {
            assert!(
                !crate::log::snapshot()
                    .iter()
                    .any(|line| line.text.contains("cannot hash") && line.text.contains(&key)),
                "the unchanged file was opened"
            );
        }
    }

    #[test]
    fn remove_file_cleans_all_tables() {
        let mut f = Fixture::new();
        let p = f.write("bye.txt", "ephemeral text");
        f.apply(&FsEvent::Create(p.clone()));
        let canonical = f.canonical(&p);
        std::fs::remove_file(&p).unwrap();
        f.apply(&FsEvent::Remove(canonical.clone().into()));
        assert!(f.row(&canonical).is_none());
        assert_eq!(f.counts(), (0, 0, 0));
    }

    #[test]
    fn directory_create_and_remove_walks_subtree() {
        let mut f = Fixture::new();
        f.write("tree/a.txt", "alpha content");
        f.write("tree/nested/b.txt", "beta content");
        f.write("tree/.hidden.txt", "should not index");
        f.write("tree/junk.tmp", "ignored pattern");
        let tree = f.dir.join("tree");
        f.apply(&FsEvent::Create(tree.clone()));
        assert_eq!(f.counts().0, 2, "hidden + ignored excluded");

        let canonical_tree = f.canonical(&tree);
        std::fs::remove_dir_all(&tree).unwrap();
        f.apply(&FsEvent::Remove(canonical_tree.into()));
        assert_eq!(f.counts(), (0, 0, 0), "subtree swept");
    }

    #[test]
    fn rename_moves_the_row() {
        let mut f = Fixture::new();
        let from = f.write("old-name.txt", "movable feast");
        f.apply(&FsEvent::Create(from.clone()));
        let canonical_from = f.canonical(&from);

        let to = f.dir.join("new-name.txt");
        std::fs::rename(&from, &to).unwrap();
        f.apply(&FsEvent::Rename {
            from: canonical_from.clone().into(),
            to: to.clone(),
        });

        assert!(f.row(&canonical_from).is_none());
        let canonical_to = f.canonical(&to);
        assert!(f.row(&canonical_to).is_some());
        assert_eq!(f.counts(), (1, 1, 1));
        assert_eq!(f.fts_hits("feast"), 1);
    }

    #[test]
    fn ignored_and_hidden_events_are_noops() {
        let mut f = Fixture::new();
        let ignored = f.write("junk.tmp", "x");
        let hidden = f.write(".secret", "x");
        f.apply(&FsEvent::Create(ignored));
        f.apply(&FsEvent::Create(hidden));
        f.apply(&FsEvent::Create(f.dir.join("never-existed.txt")));
        assert_eq!(f.counts(), (0, 0, 0));
    }

    #[test]
    fn content_extension_filter_gates_extraction() {
        let mut f = Fixture::new();
        f.config.indexing.content_extensions = vec!["md".into()];
        let txt = f.write("listed-only.txt", "text body here");
        f.apply(&FsEvent::Create(txt.clone()));

        let canonical = f.canonical(&txt);
        let (_, _, content_state) = f.row(&canonical).expect("row listed");
        assert_eq!(
            content_state,
            repo::STATE_NA,
            "filename indexed, content skipped"
        );
        assert_eq!(f.counts(), (1, 0, 0));
    }

    /// The third way a file ends up NA: no extractor claims its MIME. A row
    /// left pending would be re-fed to the content pass on every run.
    #[test]
    fn an_unclaimed_mime_is_na_in_the_watcher_path() {
        let mut f = Fixture::new();
        // `.mp4` sniffs as video/mp4 by extension; nothing extracts video.
        let vid = f.write("clip.mp4", "not really an mp4, and it needn't be");
        f.apply(&FsEvent::Create(vid.clone()));

        let canonical = f.canonical(&vid);
        let (_, _, content_state) = f.row(&canonical).expect("row listed");
        assert_eq!(
            content_state,
            repo::STATE_NA,
            "filename indexed, nothing to extract"
        );
        assert_eq!(f.counts(), (1, 0, 0));
    }

    /// `dir/./f.txt` and `dir/f.txt` are the same file; only the
    /// canonicalized spelling is in the index, yet the row must still go.
    #[test]
    fn remove_with_a_non_canonical_spelling_still_deletes() {
        let mut f = Fixture::new();
        let p = f.write("sub/gone.txt", "vanishing text");
        f.apply(&FsEvent::Create(p.clone()));
        assert_eq!(f.counts(), (1, 1, 1));

        std::fs::remove_file(&p).unwrap();
        let odd = f.dir.join("sub").join(".").join("gone.txt");
        f.apply(&FsEvent::Remove(odd));
        assert_eq!(f.counts(), (0, 0, 0), "row removed despite the spelling");
    }

    #[test]
    fn subtree_sweep_spares_prefix_siblings() {
        let mut f = Fixture::new();
        f.write("tree/a.txt", "alpha content");
        f.write("tree2/b.txt", "beta content");
        let tree = f.dir.join("tree");
        f.apply(&FsEvent::Create(tree.clone()));
        f.apply(&FsEvent::Create(f.dir.join("tree2")));
        assert_eq!(f.counts().0, 2);

        let canonical_tree = f.canonical(&tree);
        std::fs::remove_dir_all(&tree).unwrap();
        f.apply(&FsEvent::Remove(canonical_tree.into()));

        assert_eq!(f.counts().0, 1, "only tree/ was swept");
        let survivor = f.canonical(&f.dir.join("tree2").join("b.txt"));
        assert!(f.row(&survivor).is_some(), "tree2 untouched");
    }

    #[test]
    fn subtree_sweep_treats_like_metacharacters_literally() {
        let mut f = Fixture::new();
        f.write("a_b/inside.txt", "underscore dir");
        f.write("axb/other.txt", "wildcard bait");
        f.apply(&FsEvent::Create(f.dir.join("a_b")));
        f.apply(&FsEvent::Create(f.dir.join("axb")));
        assert_eq!(f.counts().0, 2);

        let target = f.dir.join("a_b");
        let canonical = f.canonical(&target);
        std::fs::remove_dir_all(&target).unwrap();
        f.apply(&FsEvent::Remove(canonical.into()));

        assert_eq!(f.counts().0, 1, "`_` must not match `x`");
        let survivor = f.canonical(&f.dir.join("axb").join("other.txt"));
        assert!(f.row(&survivor).is_some());
    }

    /// On Unix `notes\` is a file's own name, not `notes/` respelled: its
    /// Remove must not take the sibling directory's rows, whether the file
    /// is gone or still there (a Rename-from whose name was reused).
    #[test]
    #[cfg(unix)]
    fn a_trailing_backslash_file_spares_its_sibling_directory() {
        for still_present in [false, true] {
            let mut f = Fixture::new();
            let file = f.write("notes\\", "a name ending in a backslash");
            let child = f.write("notes/a.txt", "inside the sibling directory");
            f.apply(&FsEvent::Create(f.dir.clone()));
            let (file_key, child_key) = (f.canonical(&file), f.canonical(&child));
            assert_eq!(f.counts().0, 2);

            if !still_present {
                std::fs::remove_file(&file).unwrap();
            }
            f.apply(&FsEvent::Remove(file));
            assert!(
                f.row(&child_key).is_some(),
                "notes/a.txt deleted (file still present: {still_present})"
            );
            assert_eq!(f.row(&file_key).is_some(), still_present);
        }
    }

    /// A removed directory named `weird\` takes its own subtree, stored
    /// under `weird\/`, and spares the sibling directory `weird/`.
    #[test]
    #[cfg(unix)]
    fn a_removed_trailing_backslash_directory_takes_its_own_subtree() {
        let mut f = Fixture::new();
        f.write("weird\\/x.txt", "under the backslash directory");
        let kept = f.write("weird/y.txt", "under its sibling");
        f.apply(&FsEvent::Create(f.dir.clone()));
        assert_eq!(f.counts().0, 2);
        let kept = f.canonical(&kept);

        let gone = f.dir.join("weird\\");
        std::fs::remove_dir_all(&gone).unwrap();
        f.apply(&FsEvent::Remove(gone));
        assert_eq!(f.keys(), vec![kept]);
    }

    /// A lossy path must never become a DB key: for an unrepresentable path
    /// the key names a *different*, real file, and the event would take that
    /// file's row and its whole subtree for a file that was never indexed.
    #[test]
    fn removing_an_unrepresentable_path_spares_its_lossy_twin() {
        let mut f = Fixture::new();
        let twin = crate::testutil::lossy_twin("report", ".txt");
        let kept = f.write(&twin, "the file that must survive");
        f.apply(&FsEvent::Create(kept.clone()));
        let canonical = f.canonical(&kept);
        assert!(f.row(&canonical).is_some(), "seeded");

        // Never written to disk: a Remove is for a path already gone anyway.
        let bad = f
            .dir
            .join(crate::testutil::unrepresentable_name("report", ".txt"));
        f.apply(&FsEvent::Remove(bad));

        assert!(
            f.row(&canonical).is_some(),
            "the real file's row was deleted by an event for a different file"
        );
        assert_eq!(f.counts().0, 1);
    }

    /// `rm -rf real && ln -s other real`, then the Remove for `real`. Keyed
    /// through the link, it would take `other`'s rows and leave `real`'s.
    #[test]
    #[cfg(unix)]
    fn a_directory_replaced_by_a_link_spares_the_target() {
        let mut f = Fixture::new();
        f.write("real/a.txt", "under the directory the link replaces");
        let kept = f.write("other/b.txt", "under the link's target");
        f.apply(&FsEvent::Create(f.dir.clone()));
        let kept = f.canonical(&kept);
        assert_eq!(f.counts().0, 2);

        let real = f.dir.join("real");
        std::fs::remove_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(f.dir.join("other"), &real).unwrap();
        f.apply(&FsEvent::Remove(real));

        assert_eq!(f.keys(), vec![kept]);
    }

    /// `rm -rf d && ln -s t d`, then the Remove for `d/x.txt`: its parent now
    /// resolves to `t` for the key and the lstat alike, and `t/x.txt` is a
    /// live file. `d`'s own Remove, keyed by the link's name, takes the ghost.
    #[test]
    #[cfg(unix)]
    fn a_remove_through_a_new_ancestor_link_spares_the_target() {
        let mut f = Fixture::new();
        f.write("d/x.txt", "under the directory the link replaces");
        let kept = f.write("t/x.txt", "the same name under the target");
        f.apply(&FsEvent::Create(f.dir.clone()));
        let kept = f.canonical(&kept);
        assert_eq!(f.counts().0, 2);

        let d = f.dir.join("d");
        std::fs::remove_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(f.dir.join("t"), &d).unwrap();
        f.apply(&FsEvent::Remove(d.join("x.txt")));
        assert!(f.row(&kept).is_some(), "the target's live row was deleted");

        f.apply(&FsEvent::Remove(d));
        assert_eq!(f.keys(), vec![kept]);
    }

    /// The hostile forms of the two above: a link to the fixture root or to
    /// `/` where a directory was, then Removes for the link and, through it,
    /// for a live directory, a live file and a live nested file. Before the
    /// lstat and the unresolved key, the first alone deleted every row.
    #[test]
    #[cfg(unix)]
    fn a_link_to_the_root_or_to_slash_costs_no_live_row() {
        for to_slash in [false, true] {
            let mut f = Fixture::new();
            let a = f.write("keep/a.txt", "alpha");
            let b = f.write("keep/sub/b.txt", "beta");
            f.write("victim/c.txt", "the only row that may go");
            f.apply(&FsEvent::Create(f.dir.clone()));
            let kept = vec![f.canonical(&a), f.canonical(&b)];
            assert_eq!(f.counts().0, 3);

            let victim = f.dir.join("victim");
            let (target, keep) = if to_slash {
                let below = f.dir.canonicalize().unwrap();
                let below = below.strip_prefix("/").unwrap().join("keep");
                (std::path::PathBuf::from("/"), victim.join(below))
            } else {
                (f.dir.clone(), victim.join("keep"))
            };
            std::fs::remove_dir_all(&victim).unwrap();
            std::os::unix::fs::symlink(&target, &victim).unwrap();

            for p in [victim, keep.join("a.txt"), keep.join("sub/b.txt"), keep] {
                f.apply(&FsEvent::Remove(p.clone()));
                assert!(
                    kept.iter().all(|k| f.row(k).is_some()),
                    "Remove({}) with the link to {} cost a live row",
                    p.display(),
                    target.display()
                );
            }
            assert_eq!(f.keys(), kept, "c.txt's row outlived its directory");
        }
    }

    /// The OS reads `f/` and `f/.` as the directory `f`, which a regular file
    /// is not, while the key reads them as `f`: judged by the OS's reading, a
    /// live file would look gone and lose its row.
    #[test]
    fn a_spelling_that_demands_a_directory_spares_a_file() {
        let mut f = Fixture::new();
        let p = f.write("live.txt", "still here");
        f.apply(&FsEvent::Create(p.clone()));
        let key = f.canonical(&p);

        for odd in [format!("{}/", p.display()), format!("{}/.", p.display())] {
            f.apply(&FsEvent::Remove(odd.clone().into()));
            assert!(f.row(&key).is_some(), "Remove({odd}) deleted a live file");
        }
    }

    /// A file replaced by a directory: the file's row goes, and the
    /// directory's contents, already written by its Create, stay.
    #[test]
    fn a_remove_for_a_path_now_a_directory_keeps_its_contents() {
        let mut f = Fixture::new();
        let p = f.write("p", "a file for now");
        f.apply(&FsEvent::Create(p.clone()));

        std::fs::remove_file(&p).unwrap();
        let child = f.write("p/c.txt", "inside the directory that replaced it");
        f.apply(&FsEvent::Create(p.clone()));
        assert_eq!(f.counts().0, 2, "the file's row and the child's");

        f.apply(&FsEvent::Remove(p));
        assert_eq!(f.keys(), vec![f.canonical(&child)]);
    }

    /// A directory replaced by a file: nothing lives beneath a file, so the
    /// old subtree goes, while the file keeps the row its Create wrote. A
    /// Remove beneath it fails the lstat with "not a directory", which is
    /// gone as well.
    #[test]
    fn a_directory_replaced_by_a_file_drops_the_old_subtree() {
        let mut f = Fixture::new();
        f.write("d/x.txt", "x");
        f.write("d/sub/y.txt", "y");
        let d = f.dir.join("d");
        f.apply(&FsEvent::Create(d.clone()));
        assert_eq!(f.counts().0, 2);

        std::fs::remove_dir_all(&d).unwrap();
        std::fs::write(&d, "a file now").unwrap();
        f.apply(&FsEvent::Create(d.clone()));
        f.apply(&FsEvent::Remove(d.join("sub").join("y.txt")));
        assert_eq!(f.counts().0, 2, "y.txt's row outlived its directory");

        f.apply(&FsEvent::Remove(d.clone()));
        assert_eq!(f.keys(), vec![f.canonical(&d)]);
    }

    /// A path that cannot be stat'ed (its parent unsearchable) cannot be
    /// confirmed gone, so its rows stay, as the walk keeps an unreadable
    /// directory's.
    #[cfg(unix)]
    #[test]
    fn an_unstatable_path_keeps_its_rows() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = Fixture::new();
        f.write("locked/p/a.txt", "a");
        f.write("locked/p/sub/b.txt", "b");
        let locked = f.dir.join("locked");
        f.apply(&FsEvent::Create(locked.clone()));
        let before = f.keys();
        assert_eq!(before.len(), 2);

        let p = locked.join("p");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root sees through the mode, and then there is nothing to prove.
        let unstatable = std::fs::symlink_metadata(&p).is_err();
        if unstatable {
            f.apply(&FsEvent::Remove(p.join("a.txt")));
            f.apply(&FsEvent::Remove(p));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(f.keys(), before);
    }

    /// `coordinator::inner::apply_pending` turns any `Err` here into
    /// `needs_full_run`, and a full run screens the same subtree out, so the
    /// reindex would loop forever. Live on Windows, where a `\\wsl.localhost\`
    /// or Samba tree can hold such a directory and the root watch is recursive.
    #[test]
    fn a_directory_event_for_an_unrepresentable_path_is_not_an_error() {
        let mut f = Fixture::new();
        let bad = f.dir.join(crate::testutil::unrepresentable_name("dir", ""));
        if std::fs::create_dir_all(&bad).is_err() {
            eprintln!("skipped: this filesystem will not store an unrepresentable name");
            return;
        }
        std::fs::write(bad.join("inside.txt"), "body").unwrap();

        let scope = f.scope();
        let tx = f.conn.transaction().unwrap();
        let outcome = upsert_path(
            &tx,
            &bad,
            true,
            &f.config,
            &scope,
            &f.registry,
            &Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &AtomicBool::new(false),
                kills: KillMark::now(),
                resumed: false,
                after: None,
            },
        );
        assert_eq!(
            outcome,
            Ok(Applied::Done),
            "an unindexable subtree is not a failure"
        );
        tx.commit().unwrap();
        assert_eq!(f.counts().0, 0, "and nothing under it is indexed");
    }

    #[test]
    fn oversize_files_get_content_na() {
        let mut f = Fixture::new();
        f.config.processing.maximum_text_file_size = 4;
        let p = f.write("big.txt", "way more than four bytes");
        f.apply(&FsEvent::Create(p.clone()));
        let canonical = f.canonical(&p);
        let (_, _, content_state) = f.row(&canonical).unwrap();
        assert_eq!(content_state, repo::STATE_NA);
    }

    /// A symlinked directory is not walked: `metadata` follows, so the link
    /// would read as a directory, and canonicalization would store rows under
    /// the target's real path that no sweep range covers and only a rebuild
    /// clears.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_directory_is_never_followed() {
        let mut f = Fixture::new();
        let outside = crate::testutil::scratch_dir("incr-outside");
        std::fs::write(outside.join("secret.txt"), "content out of scope").unwrap();
        let link = f.dir.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        f.apply(&FsEvent::Create(link.clone()));
        assert_eq!(
            f.counts(),
            (0, 0, 0),
            "nothing under the link may be indexed"
        );

        std::fs::remove_dir_all(&outside).ok();
    }

    /// Canonicalization would file it under the target's path, outside the
    /// root that produced the event.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_file_is_never_indexed() {
        let mut f = Fixture::new();
        let outside = crate::testutil::scratch_dir("incr-outside-file");
        let target = outside.join("target.txt");
        std::fs::write(&target, "content out of scope").unwrap();
        let link = f.dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        f.apply(&FsEvent::Create(link.clone()));
        assert_eq!(f.counts(), (0, 0, 0), "the link must not be indexed");

        std::fs::remove_dir_all(&outside).ok();
    }

    /// With the database under a root, every commit reports its `-wal`:
    /// indexed, that write would be the next commit, and opening the file at
    /// all drops SQLite's locks (see `index_file_set`). Near-misses are
    /// ordinary files.
    #[test]
    fn the_index_own_files_are_never_indexed() {
        let mut f = Fixture::new();
        let data = f.dir.join("data");
        f.config.paths.database_path = data.join("qs.sqlite").to_string_lossy().into_owned();
        let own = [
            "qs.sqlite",
            "qs.sqlite-wal",
            "qs.sqlite-shm",
            "qs.sqlite-journal",
            "qs.sqlite.lock",
        ];
        let near_misses = ["qs.sqlite-walx", "qs.sqlite.bak"];
        for name in own.iter().chain(&near_misses) {
            f.write(&format!("data/{name}"), "not a database");
        }

        f.apply(&FsEvent::Create(data.clone()));
        for name in own.iter().chain(&near_misses) {
            f.apply(&FsEvent::Modify(data.join(name)));
        }
        let want: Vec<String> = near_misses
            .iter()
            .map(|name| f.canonical(&data.join(name)))
            .collect();
        assert_eq!(f.keys(), want);

        // Where the filesystem folds names, so does the guard.
        if crate::platform::PATHS_ARE_CASE_INSENSITIVE {
            f.config.paths.database_path = data.join("QS.SQLITE").to_string_lossy().into_owned();
            for name in own {
                f.apply(&FsEvent::Modify(data.join(name.to_uppercase())));
            }
            assert_eq!(f.keys(), want);
        }
    }

    /// The walk tests names below its root only, so a root whose own path
    /// holds an ignored name (`~/venv/proj`) keeps its events, spelled as
    /// configured or canonically. Below the root the name is still ignored.
    #[test]
    fn a_create_under_a_root_nested_in_an_ignored_name_is_written() {
        let mut f = Fixture::new();
        // With a detour, so the configured spelling is not the canonical one.
        let spelled = f.dir.join("node_modules/proj/../proj");
        f.config.paths.indexing_paths = vec![spelled.to_string_lossy().into_owned()];
        let a = f.write("node_modules/proj/a.txt", "reported as configured");
        let b = f.write("node_modules/proj/b.txt", "reported canonically");
        let below = f.write("node_modules/proj/node_modules/dep.txt", "still ignored");

        f.apply(&FsEvent::Create(spelled.join("a.txt")));
        f.apply(&FsEvent::Create(f.canonical(&b).into()));
        f.apply(&FsEvent::Create(below));
        f.apply(&FsEvent::Create(spelled.join("node_modules")));

        assert_eq!(f.keys(), vec![f.canonical(&a), f.canonical(&b)]);
    }

    /// Containment is by whole components, and a root contains itself.
    #[test]
    fn a_path_outside_every_root_writes_nothing() {
        let mut f = Fixture::new();
        let root = f.dir.join("b");
        f.config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        let sibling = f.write("bc/x.txt", "beside the root, its name a prefix");
        f.write("above.txt", "in the root's parent");
        let inside = f.write("b/y.txt", "inside the root");

        f.apply(&FsEvent::Create(sibling));
        f.apply(&FsEvent::Create(f.dir.join("bc")));
        f.apply(&FsEvent::Modify(f.dir.join("above.txt")));
        f.apply(&FsEvent::Create(f.dir.clone()));
        assert_eq!(f.counts(), (0, 0, 0), "written outside the root");

        f.apply(&FsEvent::Create(root));
        assert_eq!(f.keys(), vec![f.canonical(&inside)]);
    }

    /// A link made below a root after its event queued: the walk never
    /// follows it, and what it leads to lies outside every root.
    #[test]
    #[cfg(unix)]
    fn a_create_through_a_newly_linked_ancestor_writes_nothing() {
        let mut f = Fixture::new();
        let outside = crate::testutil::scratch_dir("incr-linked-ancestor");
        std::fs::write(outside.join("x.txt"), "outside every root").unwrap();
        let link = f.dir.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        f.apply(&FsEvent::Create(link.join("x.txt")));
        f.apply(&FsEvent::Modify(link.join("x.txt")));
        assert_eq!(f.counts(), (0, 0, 0), "a row outside every root");

        std::fs::remove_dir_all(&outside).ok();
    }

    /// A root that is itself a link reports through the link; its files are
    /// judged and stored by their canonical paths, as the walk stores them.
    #[test]
    #[cfg(unix)]
    fn an_event_under_a_linked_root_is_written_under_its_canonical_path() {
        let mut f = Fixture::new();
        std::fs::create_dir_all(f.dir.join("real")).unwrap();
        let link = f.dir.join("link");
        std::os::unix::fs::symlink(f.dir.join("real"), &link).unwrap();
        f.config.paths.indexing_paths = vec![link.to_string_lossy().into_owned()];
        let file = f.write("real/inside.txt", "reached through the root's link");
        let deep = f.write("real/sub/deep.txt", "under a directory event");

        f.apply(&FsEvent::Create(link.join("inside.txt")));
        f.apply(&FsEvent::Create(link.join("sub")));

        assert_eq!(f.keys(), vec![f.canonical(&file), f.canonical(&deep)]);
    }
}
