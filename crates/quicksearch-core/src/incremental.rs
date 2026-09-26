//! Incremental single-path index updates, driven by watcher events, applying
//! the same filters as the full walk; the two must agree. The watcher only
//! reports paths under the configured roots, so containment is not rechecked.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet};
use crate::db::repo;
use crate::extract::{Registry, Scratch};
use crate::file_handling::{
    db_key_for_missing_path, extract_and_store, filtered_walk, mtime_secs, path_to_db_string,
    prepare_file_record, warn_if_unrepresentable, RootRange, UnreadableDirs,
};
use crate::platform::path_has_hidden_component_under;
use crate::watcher::FsEvent;

/// How much of one event may be applied in this turn, and where the last turn
/// stopped. One event is not always one file: a moved-in directory arrives as
/// a single `Create` covering everything beneath it. This runs on the
/// coordinator's own thread, so an unbounded call would leave the command loop
/// reading no commands, including the shutdown a closing window waits on.
pub struct Budget<'a> {
    pub deadline: Instant,
    pub cancel: &'a AtomicBool,
    /// Entries an earlier turn already applied for this event.
    pub resume_from: usize,
}

impl Budget<'_> {
    fn spent(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }
}

/// Whether an event was applied in full, or ran out of budget partway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Done,
    /// Budget spent. What was written is committed; the caller should re-queue
    /// the same event with `resume_from` set to `done`.
    Unfinished {
        done: usize,
    },
}

/// Apply one filesystem event to the index. Missing files are treated as
/// no-ops (the pending Remove event resolves them); unchanged mtimes
/// short-circuit without touching the DB. See [`Budget`] for the one event
/// that is not small.
pub fn apply_fs_event(
    conn: &mut Connection,
    event: &FsEvent,
    config: &Config,
    ignore: &IgnoreSet,
    registry: &Registry,
    budget: &Budget<'_>,
) -> Result<Applied, String> {
    match event {
        FsEvent::Create(p) | FsEvent::Modify(p) => {
            upsert_path(conn, p, config, ignore, registry, budget)
        }
        FsEvent::Remove(p) => remove_path(conn, p).map(|()| Applied::Done),
        FsEvent::Rename { from, to } => {
            remove_path(conn, from)?;
            upsert_path(conn, to, config, ignore, registry, budget)
        }
    }
}

fn upsert_path(
    conn: &mut Connection,
    path: &Path,
    config: &Config,
    ignore: &IgnoreSet,
    registry: &Registry,
    budget: &Budget<'_>,
) -> Result<Applied, String> {
    if ignore.matches_path(path) {
        return Ok(Applied::Done);
    }
    // Measured from the innermost configured root: the walk never filters the
    // root it was handed, and disagreeing here makes the index churn every cycle.
    if !config.indexing.include_hidden
        && path_has_hidden_component_under(path, &config.resolved_indexing_paths())
    {
        return Ok(Applied::Done);
    }
    // Checked before the followed `metadata` below: with following off the
    // walk records no symlinks, and `upsert_file` canonicalizes, so a followed link targeting outside every root would
    // write a row no sweep range covers and only a rebuild clears.
    if !config.indexing.follow_symlinks {
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.file_type().is_symlink() => return Ok(Applied::Done),
            Err(_) => return Ok(Applied::Done),
            Ok(_) => {}
        }
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(Applied::Done);
    };
    if meta.is_dir() {
        // A path the index cannot spell is "nothing indexable here", not an
        // error: an error would set `needs_full_run`, and a full run screens
        // the same subtree out for the same reason, so the reindex would fix
        // nothing.
        let Some(root) = path.to_str() else {
            return Ok(Applied::Done);
        };
        // Resumed by `skip`, with two limits: the traversal itself is not
        // skipped (walk cost stays quadratic in turns), and `skip(n)` is
        // positional, so a tree that changes underneath skips the wrong files
        // until the next full run. If either matters, resume by last path.
        let mut done = budget.resume_from;
        for entry in filtered_walk(
            root,
            config.indexing.follow_symlinks,
            config.indexing.include_hidden,
            ignore,
            &UnreadableDirs::default(),
        )
        .skip(budget.resume_from)
        {
            if budget.spent() {
                return Ok(Applied::Unfinished { done });
            }
            upsert_file(conn, entry.path(), config, registry)?;
            done += 1;
        }
        Ok(Applied::Done)
    } else {
        upsert_file(conn, path, config, registry).map(|()| Applied::Done)
    }
}

fn upsert_file(
    conn: &mut Connection,
    path: &Path,
    config: &Config,
    registry: &Registry,
) -> Result<(), String> {
    let Ok(canonical) = path.canonicalize() else {
        return Ok(());
    };
    if warn_if_unrepresentable(&canonical) {
        return Ok(());
    }
    let db_path = path_to_db_string(&canonical);
    let Ok(meta) = std::fs::metadata(&canonical) else {
        return Ok(());
    };

    let tx = conn
        .transaction()
        .map_err(|e| format!("begin incremental tx: {}", e))?;

    // Settled before the file is opened, as the full walk settles it: an
    // unchanged file (a re-sent event, a `chmod`, the live view re-checking a
    // row it shows) costs one lookup, never a read and a hash. Measured
    // (`benches/index.rs`, `unchanged_event`, 500 small text files): 3.3 ms
    // hashing and sniffing each against 1.4 ms.
    let stored = repo::mtime_for_path(&tx, &db_path)?;
    if stored.is_some() && stored == mtime_secs(&meta) {
        return Ok(());
    }
    // One file at a time, so it owns the scratch rather than being handed one.
    let mut scratch = Scratch::new(config);
    let Some(rec) = prepare_file_record(&db_path, &meta, config, registry, &mut scratch) else {
        return Ok(());
    };
    let written = match stored {
        Some(_) => repo::update_file_basic(&tx, &rec.as_new_file())?,
        None => repo::insert_file(&tx, &rec.as_new_file())?,
    };
    // `None`: lost a race with another writer; the row that won is current
    // enough.
    let Some(file_id) = written else {
        return Ok(());
    };

    // A row that needs no content is already NA: both writers above set its
    // state from `rec`. That gate is also the only size gate on this path:
    // `decide_content` has none, and falling through would read a
    // multi-gigabyte `.txt` whole into memory.
    if rec.needs_content {
        match rec.inline_text.as_deref() {
            // `prepare_file_record` already read the whole file.
            Some(text) => {
                let zstd = repo::encode_one(text)?;
                repo::store_pending_text(&tx, file_id, text, zstd.as_deref())?;
            }
            None => extract_and_store(&tx, file_id, rec.path(), rec.mime, registry, config)?,
        }
    }

    tx.commit()
        .map_err(|e| format!("commit incremental tx: {}", e))
}

fn remove_path(conn: &mut Connection, path: &Path) -> Result<(), String> {
    remove_paths(conn, std::slice::from_ref(&path.to_path_buf()), 1)
}

/// Delete `paths` and everything indexed beneath them, in transactions of at
/// most `chunk` paths. Pays off only when the caller has reduced the set to
/// its *roots*: then `rm -rf dir/` is a fixed handful of range-driven
/// statements ([`repo::delete_subtree`]) rather than five per file.
pub fn remove_paths(
    conn: &mut Connection,
    paths: &[std::path::PathBuf],
    chunk: usize,
) -> Result<(), String> {
    for batch in paths.chunks(chunk.max(1)) {
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin incremental tx: {}", e))?;
        for path in batch {
            // A path the index cannot spell was never indexed, so there is
            // nothing to delete; its lossy spelling names another file (see
            // `path_to_db_string`), whose row and subtree this would delete.
            if path.to_str().is_none() {
                continue;
            }
            // The stored key is canonicalized, but the file is already gone,
            // so `canonicalize` cannot run; `db_key_for_missing_path` approximates it.
            let path_str = db_key_for_missing_path(path);
            repo::delete_file_by_path(&tx, &path_str)?;
            repo::delete_subtree(&tx, &RootRange::for_root(&path_str))?;
        }
        tx.commit()
            .map_err(|e| format!("commit incremental tx: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_recreate;
    use rusqlite::OptionalExtension;
    use std::time::Duration;

    struct Fixture {
        conn: Connection,
        dir: std::path::PathBuf,
        db: std::path::PathBuf,
        config: Config,
        ignore: IgnoreSet,
        registry: Registry,
    }

    impl Fixture {
        fn new() -> Fixture {
            let scratch = crate::testutil::scratch_dir("incr");
            let dir = scratch.join("tree");
            std::fs::create_dir_all(&dir).unwrap();
            let db = scratch.join("index.sqlite");
            let conn = open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
            let config = Config::default();
            let ignore = IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap();
            Fixture {
                conn,
                dir,
                db,
                config,
                ignore,
                registry: Registry::default_set(),
            }
        }

        fn apply(&mut self, event: &FsEvent) {
            let done = self.apply_within(event, Duration::from_secs(3600));
            assert_eq!(done, Applied::Done, "unexpectedly ran out of budget");
        }

        fn apply_within(&mut self, event: &FsEvent, budget: Duration) -> Applied {
            self.apply_resuming(event, budget, 0)
        }

        fn apply_resuming(&mut self, event: &FsEvent, budget: Duration, from: usize) -> Applied {
            apply_fs_event(
                &mut self.conn,
                event,
                &self.config,
                &self.ignore,
                &self.registry,
                &Budget {
                    deadline: Instant::now() + budget,
                    cancel: &AtomicBool::new(false),
                    resume_from: from,
                },
            )
            .unwrap()
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
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
            std::fs::remove_file(&self.db).ok();
        }
    }

    /// The coordinator prunes `resume_from` alongside the queues: resuming
    /// from a count that belonged to an earlier walk skips real files, and
    /// they get no row until the next full run.
    #[test]
    fn resuming_past_the_end_indexes_nothing_rather_than_the_wrong_files() {
        let mut f = Fixture::new();
        for i in 0..3 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        let outcome =
            f.apply_resuming(&FsEvent::Create(sub.clone()), Duration::from_secs(3600), 99);
        assert_eq!(outcome, Applied::Done);
        assert_eq!(f.counts().0, 0);

        f.apply(&FsEvent::Create(sub));
        assert_eq!(f.counts().0, 3);
    }

    /// `chunks(0)` panics on the indexing thread above the arm that publishes
    /// `IndexingStatus::Error`, so a hand-edited zero would wedge indexing for
    /// the session while the UI reads "Running".
    #[test]
    fn a_zero_batch_size_does_not_panic_the_writer() {
        let mut f = Fixture::new();
        f.config.processing.batch_size = 0;
        f.write("a.txt", "body");
        f.apply(&FsEvent::Create(f.dir.join("a.txt")));
        assert_eq!(f.counts().0, 1, "the file should still be indexed");
    }

    /// A sliced directory event resumes where the last slice stopped: linear,
    /// not quadratic. Asserted by resume point rather than by clock, which says
    /// nothing reliable on a loaded CI runner.
    #[test]
    fn a_directory_event_resumes_where_its_budget_ran_out() {
        let mut f = Fixture::new();
        for i in 0..4 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        let outcome = f.apply_within(&FsEvent::Create(sub.clone()), Duration::ZERO);
        assert_eq!(outcome, Applied::Unfinished { done: 0 });
        assert_eq!(f.counts().0, 0, "a spent budget must write nothing");

        let outcome = f.apply_resuming(&FsEvent::Create(sub.clone()), Duration::from_secs(3600), 2);
        assert_eq!(outcome, Applied::Done);
        assert_eq!(
            f.counts().0,
            2,
            "entries before the resume point must be skipped, not re-applied"
        );

        f.apply(&FsEvent::Create(sub));
        assert_eq!(f.counts().0, 4);
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
        remove_paths(&mut f.conn, &roots, 200).unwrap();
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

    /// An unchanged file is settled from its stored mtime, as the full walk
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

        let outcome = apply_fs_event(
            &mut f.conn,
            &FsEvent::Create(bad),
            &f.config,
            &f.ignore,
            &f.registry,
            &Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &AtomicBool::new(false),
                resume_from: 0,
            },
        );
        assert_eq!(
            outcome,
            Ok(Applied::Done),
            "an unindexable subtree is not a failure"
        );
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

    /// With following off, a symlinked directory is not walked: `metadata`
    /// follows, so the link would read as a directory, and canonicalization
    /// would store rows under the target's real path that no sweep range
    /// covers and only a rebuild clears.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_directory_is_not_followed_when_following_is_off() {
        let mut f = Fixture::new();
        assert!(
            !f.config.indexing.follow_symlinks,
            "the default this test is about"
        );

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
    fn a_symlinked_file_is_not_indexed_when_following_is_off() {
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

    /// The other half of the guard: it must gate on the setting, not refuse
    /// symlinks outright.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_file_is_indexed_when_following_is_on() {
        let mut f = Fixture::new();
        f.config.indexing.follow_symlinks = true;
        let target = f.write("target.txt", "greetings earthling");
        let link = f.dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        f.apply(&FsEvent::Create(link.clone()));
        let canonical = f.canonical(&target);
        assert!(
            f.row(&canonical).is_some(),
            "the link should have indexed its target"
        );
    }
}
