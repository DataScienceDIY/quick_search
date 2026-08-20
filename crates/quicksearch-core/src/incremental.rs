//! Incremental single-path index updates, driven by watcher events.
//!
//! One [`FsEvent`] becomes one (or a few) small transactions: files row,
//! `documents_text`, and FTS are updated together, so the index is
//! consistent after every commit. The same filters as the full walk apply
//! ([`IgnoreSet`], hidden components, `content_extensions`, size caps) —
//! a watcher event for something the walker would have skipped is a no-op.
//! Renames are handled as remove + re-add.
//!
//! The watcher only reports paths under the configured roots, so no root
//! containment check is repeated here.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension};

use crate::config::{Config, IgnoreSet};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    db_key_for_missing_path, extract_and_store, filtered_walk, prepare_file_record_from_path,
    ExtractCursor, UnreadableDirs,
};
use crate::platform::path_has_hidden_component_under;
use crate::watcher::FsEvent;

/// How much of one event may be applied in this turn, and where the last turn
/// stopped.
///
/// One event is not always one file: a directory moved into a watched tree
/// arrives as a single `Create` covering everything beneath it. Shaped like
/// [`crate::scope::advance`]'s arguments and there for the same reason — this
/// runs on the coordinator's own thread, so an unbounded call is a command
/// loop that reads no commands, including the shutdown a closing window is
/// waiting on.
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
    /// Everything the event implied is in the index.
    Done,
    /// Budget spent. What was written is committed; the caller should re-queue
    /// the same event with `resume_from` set to `done`.
    Unfinished { done: usize },
}

/// Apply one filesystem event to the index. Missing files are treated as
/// no-ops (a Create followed by a quick delete resolves via the Remove
/// event); unchanged mtimes short-circuit without touching the DB.
///
/// See [`Budget`] for the one event that is not small.
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
    // Measured from the innermost configured root: the walk never filters
    // the root it was handed, so a root that is itself hidden must not be
    // rejected here — that disagreement makes the index churn every cycle.
    if !config.indexing.include_hidden
        && path_has_hidden_component_under(path, &config.resolved_indexing_paths())
    {
        return Ok(Applied::Done);
    }
    let Ok(meta) = std::fs::metadata(path) else {
        // Already gone again — the pending Remove event handles it.
        return Ok(Applied::Done);
    };
    if meta.is_dir() {
        // A moved-in tree surfaces as one directory event; walk it with
        // the same filters as a full run.
        //
        // A non-UTF-8 path is a whole subtree missing from the index, so it
        // is an error (the caller schedules a full run) rather than a quiet
        // `Ok`.
        let Some(root) = path.to_str() else {
            return Err(format!("directory path is not valid UTF-8: {:?}", path));
        };
        // Streamed, not collected: `mv` of a large tree is one event, and
        // materialising its entries first is a `DirEntry` per file resident
        // before a single row is written. Each file is its own transaction,
        // so stopping between two of them leaves the index consistent and the
        // remainder for the next turn.
        //
        // `skip` rather than re-testing every entry: `upsert_file` on an
        // unchanged file is cheap but not free, and paying it again for
        // everything already done would make a large tree quadratic in the
        // number of turns it takes. The walk order is deterministic for an
        // unchanged tree; if the tree does change under us the count is only
        // an optimisation, and the next full run is what makes it exact.
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
    let Some(rec) = prepare_file_record_from_path(path, config, registry) else {
        return Ok(());
    };

    let tx = conn
        .transaction()
        .map_err(|e| format!("begin incremental tx: {}", e))?;

    let existing: Option<(i64, i64)> = tx
        .query_row(
            "SELECT id, mtime FROM files WHERE path = ?1",
            rusqlite::params![rec.path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("lookup {}: {}", rec.path, e))?;

    let file_id = match existing {
        Some((_, mtime)) if mtime.max(0) as u64 == rec.mtime => return Ok(()),
        Some((id, _)) => {
            repo::update_file_basic(&tx, &rec.as_new_file())?;
            id
        }
        None => match repo::insert_file(&tx, &rec.as_new_file())? {
            Some(id) => id,
            // Lost a race with another writer on the same path; the row
            // that won is current enough.
            None => return Ok(()),
        },
    };

    // The only size gate on this path — `decide_content` has none, and
    // falling through would hand a multi-gigabyte `.txt` to the plaintext
    // extractor, which reads the whole file into memory.
    if !rec.needs_content {
        repo::set_content_na(&tx, file_id)?;
    } else if let Some(text) = rec.inline_text.as_deref() {
        // `prepare_file_record_from_path` already read the whole file.
        let zstd = repo::encode_one(text, config.processing.store_text_for_snippets)?;
        repo::set_content_done(&tx, file_id, text, zstd.as_deref())?;
    } else {
        extract_and_store(
            &tx,
            file_id,
            &rec.path,
            rec.mime.as_deref(),
            registry,
            config,
        )?;
    }

    tx.commit()
        .map_err(|e| format!("commit incremental tx: {}", e))
}

fn remove_path(conn: &mut Connection, path: &Path) -> Result<(), String> {
    remove_paths(conn, std::slice::from_ref(&path.to_path_buf()), 1)
}

/// Delete `paths` and everything indexed beneath them, in transactions of at
/// most `chunk` paths.
///
/// Pays off only when the caller has reduced the set to its *roots*: then
/// `rm -rf dir/` is a fixed handful of range-driven statements
/// ([`repo::delete_subtree`]) rather than five per file. Chunking bounds how
/// long any single transaction holds the connection.
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
            // The insert side stores a canonicalized path, so the raw event
            // spelling is not a usable key — but the file is already gone, so
            // `canonicalize` cannot be called on it directly either.
            let path_str = db_key_for_missing_path(path);
            // The path itself, whether it was a file or a directory...
            repo::delete_file_by_path(&tx, &path_str)?;
            // ...then everything beneath it, for a directory removal.
            let range = ExtractCursor::for_root(&path_str);
            repo::delete_subtree(&tx, &range.lo, &range.hi)?;
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
            let conn = open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
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

        /// Applies with an effectively unlimited budget: these tests are about
        /// what lands in the index, not about the slicing. See
        /// [`Fixture::apply_within`] for the budget itself.
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

        /// The key the index actually stores. Must go through
        /// `path_to_db_string`, or every lookup here misses the
        /// `\\?\`-stripped spelling on Windows.
        fn canonical(&self, p: &Path) -> String {
            crate::file_handling::path_to_db_string(&p.canonicalize().unwrap())
        }

        fn row(&self, path: &str) -> Option<(i64, i64, i64)> {
            self.conn
                .query_row(
                    "SELECT id, mtime, content_state FROM files WHERE path = ?1",
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
            self.conn
                .query_row(
                    "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
                    rusqlite::params![format!("\"{}\"", term)],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
            std::fs::remove_file(&self.db).ok();
        }
    }

    /// The collapse must not change what ends up deleted — only how much work
    /// it takes to get there.
    /// A directory event is applied in slices, and a slice resumes where the
    /// last one stopped instead of re-walking what it already did.
    ///
    /// `mv` of a large tree is one `Create`. Applying it in one go held the
    /// coordinator's command loop — and the shutdown queued behind it — for as
    /// long as the whole tree took; applying it in slices that each restarted
    /// from the top would be quadratic instead. Asserted by resume point
    /// rather than by clock, because a timing-based assertion says nothing
    /// reliable on a loaded CI runner.
    #[test]
    fn a_directory_event_resumes_where_its_budget_ran_out() {
        let mut f = Fixture::new();
        for i in 0..4 {
            f.write(&format!("sub/f{i}.txt"), "body");
        }
        let sub = f.dir.join("sub");

        // Nothing may be spent, so nothing is applied and the resume point is
        // where it started.
        let outcome = f.apply_within(&FsEvent::Create(sub.clone()), Duration::ZERO);
        assert_eq!(outcome, Applied::Unfinished { done: 0 });
        assert_eq!(f.counts().0, 0, "a spent budget must write nothing");

        // Resuming past the first two entries applies only what is left, which
        // is what makes slicing linear rather than quadratic.
        let outcome = f.apply_resuming(&FsEvent::Create(sub.clone()), Duration::from_secs(3600), 2);
        assert_eq!(outcome, Applied::Done);
        assert_eq!(
            f.counts().0,
            2,
            "entries before the resume point must be skipped, not re-applied"
        );

        // And from the start, the rest arrive.
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

        // What a real `rm -rf` produces: the directory plus every path under it.
        let reported: Vec<std::path::PathBuf> = vec![
            canonical_tree.clone().into(),
            format!("{}/deep", canonical_tree).into(),
            format!("{}/a.txt", canonical_tree).into(),
            format!("{}/deep/b.txt", canonical_tree).into(),
        ];
        // Collapsed to its root the way the coordinator collapses an
        // arriving queue (`collapse_pending_removals`): one range covers the
        // whole tree, which is what makes `remove_paths` cheap.
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

        // Same mtime → no-op (id unchanged, no re-extraction).
        f.apply(&FsEvent::Modify(p.clone()));
        let (id2, mtime2, _) = f.row(&canonical).unwrap();
        assert_eq!((id1, mtime1), (id2, mtime2));

        // Bump mtime and content → re-extracted, FTS follows.
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
        // Missing file too.
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

    /// A Remove event whose path is spelled differently from the stored key
    /// must still delete the row. `dir/./f.txt` and `dir/f.txt` are the same
    /// file; only the canonicalized spelling is in the index.
    #[test]
    fn remove_with_a_non_canonical_spelling_still_deletes() {
        let mut f = Fixture::new();
        let p = f.write("sub/gone.txt", "vanishing text");
        f.apply(&FsEvent::Create(p.clone()));
        assert_eq!(f.counts(), (1, 1, 1));

        std::fs::remove_file(&p).unwrap();
        // Same file, spelled with a redundant `.` component.
        let odd = f.dir.join("sub").join(".").join("gone.txt");
        f.apply(&FsEvent::Remove(odd));
        assert_eq!(f.counts(), (0, 0, 0), "row removed despite the spelling");
    }

    /// The subtree sweep must not take siblings whose names merely share a
    /// string prefix — `tree2` is not inside `tree`.
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

    /// A directory whose name contains a LIKE metacharacter must be swept
    /// literally, not as a wildcard.
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
}
