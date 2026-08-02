//! Incremental single-path index updates, driven by watcher events.
//!
//! One [`FsEvent`] becomes one (or a few) small transactions: files row,
//! `documents_text`, and FTS are updated together, so the index is
//! consistent after every commit. The same filters as the full walk apply
//! ([`IgnoreSet`], hidden components, `content_extensions`, size caps) —
//! a watcher event for something the walker would have skipped is a no-op.
//!
//! Renames are handled as remove + re-add: they're rare, and rewriting
//! `path`/`parent` strings plus re-tokenizing the FTS `name` column in
//! place is more machinery than re-extracting one file.
//!
//! Scope note: the watcher only reports paths under the configured roots,
//! so no root containment check is repeated here.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::config::{content_allowed, Config, IgnoreSet};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    db_key_for_missing_path, extract_and_store, filtered_walk, UnreadableDirs,
    prepare_file_record_from_path, store_inline_text,
};
use crate::platform::path_has_hidden_component_under;
use crate::query::translator::like_subtree_pattern;
use crate::watcher::FsEvent;

/// Apply one filesystem event to the index. Missing files are treated as
/// no-ops (a Create followed by a quick delete resolves via the Remove
/// event); unchanged mtimes short-circuit without touching the DB.
pub fn apply_fs_event(
    conn: &mut Connection,
    event: &FsEvent,
    config: &Config,
    ignore: &IgnoreSet,
    registry: &Registry,
) -> Result<(), String> {
    match event {
        FsEvent::Create(p) | FsEvent::Modify(p) => upsert_path(conn, p, config, ignore, registry),
        FsEvent::Remove(p) => remove_path(conn, p),
        FsEvent::Rename { from, to } => {
            remove_path(conn, from)?;
            upsert_path(conn, to, config, ignore, registry)
        }
    }
}

fn upsert_path(
    conn: &mut Connection,
    path: &Path,
    config: &Config,
    ignore: &IgnoreSet,
    registry: &Registry,
) -> Result<(), String> {
    if ignore.matches_path(path) {
        return Ok(());
    }
    // Measured from the innermost configured root: the walk never filters the
    // root it was handed, so a root that is itself hidden (`~/.config/app`, or
    // anything under `%LOCALAPPDATA%` on Windows) must not be rejected here —
    // that disagreement is what makes the index churn every cycle.
    if !config.indexing.include_hidden
        && path_has_hidden_component_under(path, &config.resolved_indexing_paths())
    {
        return Ok(());
    }
    let Ok(meta) = std::fs::metadata(path) else {
        // Already gone again — the pending Remove event handles it.
        return Ok(());
    };
    if meta.is_dir() {
        // A moved-in tree surfaces as one directory event; walk it with
        // the same filters as a full run.
        let Some(root) = path.to_str() else {
            return Ok(());
        };
        let entries: Vec<_> = filtered_walk(
            root,
            config.indexing.follow_symlinks,
            config.indexing.include_hidden,
            ignore,
            &UnreadableDirs::default(),
        )
        .collect();
        for entry in entries {
            upsert_file(conn, entry.path(), config, registry)?;
        }
        Ok(())
    } else {
        upsert_file(conn, path, config, registry)
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
            repo::update_file_basic(
                &tx,
                &rec.path,
                rec.size,
                rec.mtime,
                Some(&rec.hash),
                rec.mime.as_deref(),
                rec.ftype,
            )?;
            id
        }
        None => match repo::insert_file(&tx, &rec.as_new_file())? {
            Some(id) => id,
            // Lost a race with another writer on the same path; the row
            // that won is current enough.
            None => return Ok(()),
        },
    };

    if rec.size > config.processing.maximum_text_file_size
        || !content_allowed(Path::new(&rec.path), config)
    {
        repo::set_content_na(&tx, file_id)?;
    } else if let Some(text) = rec.inline_text.as_deref() {
        // Small enough that `prepare_file_record_from_path` already read the
        // whole file; reopening it here would be the same bytes twice.
        store_inline_text(&tx, file_id, &rec, text, config)?;
    } else {
        extract_and_store(
            &tx,
            file_id,
            &rec.name,
            &rec.path,
            rec.mime.as_deref(),
            registry,
            config,
        )?;
    }

    tx.commit().map_err(|e| format!("commit incremental tx: {}", e))
}

fn remove_path(conn: &mut Connection, path: &Path) -> Result<(), String> {
    // The insert side stores a canonicalized path, so the raw event spelling
    // is not a usable key — but the file is already gone, so `canonicalize`
    // cannot be called on it directly either.
    let path_str = db_key_for_missing_path(path);
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin incremental tx: {}", e))?;

    repo::delete_file_by_path(&tx, &path_str)?;

    // Directory removals surface as one event for the directory itself —
    // sweep everything indexed beneath it.
    let subtree: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\'")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![like_subtree_pattern(&path_str)], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
    };
    for p in &subtree {
        repo::delete_file_by_path(&tx, p)?;
    }

    tx.commit().map_err(|e| format!("commit incremental tx: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_recreate;

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
            let stamp = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let dir = std::env::temp_dir().join(format!("qs-incr-{}", stamp));
            std::fs::create_dir_all(&dir).unwrap();
            let db = std::env::temp_dir().join(format!("qs-incr-{}.sqlite", stamp));
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

        fn apply(&mut self, event: &FsEvent) {
            apply_fs_event(&mut self.conn, event, &self.config, &self.ignore, &self.registry)
                .unwrap();
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
