//! Bringing a stored index back in line with a changed configuration,
//! without deleting it.
//!
//! The index is a cache of what a walk under the configured roots would
//! produce. When the configuration changes, the two disagree — and almost
//! always in a way that can be *reconciled* rather than rebuilt:
//!
//! * A root was removed. Its rows are a contiguous `files.path` range, so
//!   they go in five statements ([`crate::db::repo::delete_subtree`]).
//! * An ignore pattern was added, hidden files were switched off, symlinks
//!   stopped being followed. The rows to drop are picked out by a predicate
//!   no SQL range can express, so [`Scope::covers`] re-runs the walker's own
//!   filtering rules against each stored path.
//! * The content filter moved. The rows stay; only their extracted text,
//!   properties and FTS entry are re-decided.
//!
//! Only settings that make stored data unreadable or incomparable — the FTS
//! tokenizer, the hash length, the encryption key — still force a wipe. See
//! [`crate::config::diff_actions`], which decides which of these applies, and
//! [`crate::config::IndexWork`], the plan it produces.
//!
//! ## Why the scan is per-root
//!
//! [`advance`] walks each configured root's `[lo, hi)` range rather than the
//! whole `files` table. That is not only a matter of using the index: with
//! `follow_symlinks` on, a symlink target is stored under its own canonical
//! path, which may lie outside every root. Such a row is legitimately
//! indexed, has no owning root, and so has no filtering rules that can be
//! applied to it — scanning by range means it is simply never visited, the
//! same exemption `aliased_paths` gives it during a full run's stale sweep.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet, IndexWork};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{content_extractable, fts_finalize_after_text_indexing, ExtractCursor};

/// How long [`advance`] may work before handing control back.
///
/// Not a throughput knob — the caller decides when to come back — but a bound
/// on how long a Stop, a search or a further config edit waits behind a scan
/// in progress. The same budget the coordinator gives its watcher queue.
pub const SLICE: Duration = Duration::from_millis(250);

/// One configured root, with the `files.path` range it owns precomputed.
struct Root {
    path: PathBuf,
    lo: String,
    hi: String,
}

/// The set of paths the current configuration would index.
///
/// The walker applies these rules on the way down, pruning a directory before
/// it descends. `Scope` applies the same rules to a path that is already
/// stored, which is what lets a narrowed filter delete the rows that fell out
/// of scope instead of rebuilding the whole index. The two must agree exactly,
/// or every run would re-add what the last prune removed; the tests below pin
/// that agreement against [`crate::walk`]'s own behaviour.
pub struct Scope {
    roots: Vec<Root>,
    ignore: IgnoreSet,
    include_hidden: bool,
}

impl Scope {
    pub fn from_config(config: &Config) -> Result<Scope, String> {
        let roots = config
            .normalized_indexing_paths()
            .into_iter()
            .map(|root| {
                let range = ExtractCursor::for_root(&root);
                Root {
                    path: PathBuf::from(root),
                    lo: range.lo,
                    hi: range.hi,
                }
            })
            .collect();
        Ok(Scope {
            roots,
            ignore: IgnoreSet::compile(&config.indexing.ignore_patterns)
                .map_err(|e| format!("ignore patterns: {}", e))?,
            include_hidden: config.indexing.include_hidden,
        })
    }

    /// The configured root `path` lives under, if any. `Path::starts_with`
    /// compares whole components, so `/a/bc` is never read as living under
    /// `/a/b`.
    pub fn owning_root(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .map(|r| r.path.as_path())
            .find(|root| path.starts_with(root) && path != *root)
    }

    /// Whether the walker would still emit `path` while walking `root`.
    ///
    /// Mirrors `read_directory`'s three `continue`s. The full-path ignore
    /// patterns are tested once against the whole path because
    /// [`IgnoreSet::matches_path_pattern`] already walks every ancestor,
    /// which is exactly the union of the per-level tests the walker performs
    /// on its way down. The hidden and component-pattern rules are tested per
    /// component *below* the root: a root is never filtered, because the user
    /// chose it (see `walk_parallel`).
    pub fn covers(&self, root: &Path, path: &Path) -> bool {
        if self.ignore.matches_path_pattern(path) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let mut current = root.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                // Stored paths are canonical, so stripping a canonical root
                // leaves plain names. Anything else did not come from a walk.
                return false;
            };
            current.push(name);
            let name = name.to_string_lossy();
            // The metadata closure is only consulted on Windows, where hidden
            // is an attribute rather than a leading dot; on Unix this stays at
            // zero syscalls, exactly as it does in the walker.
            if !self.include_hidden
                && crate::platform::entry_is_hidden(&name, || std::fs::metadata(&current).ok())
            {
                return false;
            }
            if self.ignore.matches_component(&name) {
                return false;
            }
        }
        true
    }
}

/// How far an in-progress [`advance`] has got.
///
/// The work is resumable because a multi-million-row index must not hold the
/// coordinator's command loop for the length of a full scan; the caller hands
/// back the same cursor each tick with a fresh deadline, the way
/// `apply_pending` drains the watcher queue.
pub struct WorkCursor {
    work: IndexWork,
    scope: Scope,
    /// Index into `work.drop_roots` of the next range to delete outright.
    drop_idx: usize,
    /// Set once the out-of-root sweep has run; it is a single statement set,
    /// so it either happened or it did not.
    dropped_aliases: bool,
    /// Index into `scope.roots` of the range being scanned.
    root_idx: usize,
    /// Last path served by the scan — the keyset cursor. Empty means "start
    /// this root's range from its `lo` bound".
    after: String,
    /// Set once the FTS automerge that follows a batch of deletions has run.
    finalized: bool,
    /// Rows deleted so far, for the log line when the work completes.
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
}

impl WorkCursor {
    pub fn new(work: IndexWork, config: &Config) -> Result<WorkCursor, String> {
        Ok(WorkCursor {
            work,
            scope: Scope::from_config(config)?,
            drop_idx: 0,
            dropped_aliases: false,
            root_idx: 0,
            after: String::new(),
            finalized: false,
            deleted: 0,
            recontented: 0,
        })
    }

    pub fn done(&self) -> bool {
        self.finalized
    }

    /// Whether a full walk must follow this reconciliation.
    pub fn reindex(&self) -> bool {
        self.work.reindex
    }

    /// The plan being applied, for a caller that has to restart against a
    /// newer configuration and must not lose what this one had left to do.
    pub fn work(&self) -> &IndexWork {
        &self.work
    }

    /// Drop the walk this reconciliation asked for, keeping the rest. For a
    /// caller that has since been told not to run anything.
    pub fn cancel_reindex(&mut self) {
        self.work.reindex = false;
    }
}

/// Apply as much of `cursor` as fits before `deadline`, one page of rows per
/// transaction. Returns with the cursor advanced; call again until
/// [`WorkCursor::done`].
pub fn advance(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
) -> Result<(), String> {
    // Whole ranges first: a removed root's rows can never satisfy the scan's
    // filters anyway, and deleting them by range spares the scan the work.
    while cursor.drop_idx < cursor.work.drop_roots.len() {
        let range = ExtractCursor::for_root(&cursor.work.drop_roots[cursor.drop_idx]);
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin drop-root transaction: {}", e))?;
        let removed = repo::delete_subtree(&tx, &range.lo, &range.hi)?;
        tx.commit()
            .map_err(|e| format!("commit drop-root transaction: {}", e))?;
        cursor.deleted += removed;
        cursor.drop_idx += 1;
        if Instant::now() >= deadline {
            return Ok(());
        }
    }

    // Before the per-root scan and after the root deletions: the ranges it
    // spares must already be the final set of roots.
    if !cursor.dropped_aliases && cursor.work.drop_aliases {
        let ranges: Vec<(String, String)> = cursor
            .scope
            .roots
            .iter()
            .map(|r| (r.lo.clone(), r.hi.clone()))
            .collect();
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin drop-alias transaction: {}", e))?;
        let removed = repo::delete_outside_ranges(&tx, &ranges)?;
        tx.commit()
            .map_err(|e| format!("commit drop-alias transaction: {}", e))?;
        cursor.deleted += removed;
        cursor.dropped_aliases = true;
        if Instant::now() >= deadline {
            return Ok(());
        }
    }

    if cursor.work.scans_rows() {
        let page = config.processing.batch_size.max(1) as i64;
        while cursor.root_idx < cursor.scope.roots.len() {
            let root = &cursor.scope.roots[cursor.root_idx];
            if cursor.after.is_empty() {
                cursor.after = root.lo.clone();
            }
            let rows = repo::rows_in_range_page(conn, &cursor.after, &root.hi, page)?;
            let Some(last) = rows.last() else {
                cursor.root_idx += 1;
                cursor.after.clear();
                continue;
            };
            cursor.after = last.path.clone();
            let root = cursor.scope.roots[cursor.root_idx].path.clone();
            let (deleted, recontented) = apply_page(
                conn,
                config,
                registry,
                &cursor.scope,
                &cursor.work,
                &root,
                &rows,
            )?;
            cursor.deleted += deleted;
            cursor.recontented += recontented;
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    // Deletions leave the FTS index with tombstones and a long segment list;
    // the same automerge that follows a run's stale cleanup collapses them.
    if cursor.deleted > 0 || cursor.recontented > 0 {
        fts_finalize_after_text_indexing(conn);
    }
    cursor.finalized = true;
    Ok(())
}

/// Decide and write one page of rows. Returns `(deleted, recontented)`.
fn apply_page(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    scope: &Scope,
    work: &IndexWork,
    root: &Path,
    rows: &[repo::ScopeRow],
) -> Result<(usize, usize), String> {
    let mut doomed: Vec<i64> = Vec::new();
    let mut stale_text: Vec<i64> = Vec::new();
    let mut to_pending: Vec<i64> = Vec::new();
    let mut to_na: Vec<i64> = Vec::new();

    for row in rows {
        let path = Path::new(&row.path);
        if work.prune_scope && !scope.covers(root, path) {
            doomed.push(row.id);
            continue;
        }
        if work.drop_text {
            stale_text.push(row.id);
        }
        if work.reconcile_content || work.restore_text {
            // The walker's own decision, recomputed from the columns it wrote
            // it into. Both directions run whenever either flag is set: the
            // answer comes from the *current* config, so a row that disagrees
            // with it is wrong however it got that way.
            let wants = row.size <= config.processing.maximum_text_file_size
                && content_extractable(path, row.mime.as_deref(), config, registry);
            if !wants && row.content_state != repo::STATE_NA {
                to_na.push(row.id);
            } else if wants
                && (row.content_state == repo::STATE_NA
                    || (work.restore_text && row.content_state == repo::STATE_DONE))
            {
                to_pending.push(row.id);
            }
        }
    }

    let tx = conn
        .transaction()
        .map_err(|e| format!("begin reconcile transaction: {}", e))?;
    let deleted = if doomed.is_empty() {
        0
    } else {
        repo::delete_ids(&tx, &doomed)?
    };
    if !stale_text.is_empty() {
        repo::drop_stored_text(&tx, &stale_text)?;
    }
    for id in &to_pending {
        repo::reset_content_pending(&tx, *id)?;
    }
    for id in &to_na {
        repo::remove_content_for_id(&tx, *id)?;
        repo::set_content_na(&tx, *id)?;
    }
    tx.commit()
        .map_err(|e| format!("commit reconcile transaction: {}", e))?;
    Ok((deleted, to_pending.len() + to_na.len()))
}

/// The configuration the index was last built with, as far as
/// `config_validation` records it: `config` with the recorded fields
/// substituted back in.
///
/// Fields the table does not record keep `config`'s own values, so they never
/// read as changed — the table is a record of what the walk used, not a second
/// copy of the config. Feeding this to [`crate::config::diff_actions`] is what
/// lets a config edited while the app was closed produce exactly the same plan
/// as one edited live, from one decision table rather than two.
pub fn stored_config(conn: &Connection, config: &Config) -> Result<Config, String> {
    let mut stored = config.clone();
    let recorded = crate::indexing::IndexingService::stored_validation(conn)?;
    let lines = |value: &str| -> Vec<String> {
        value
            .split('\n')
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect()
    };
    for (key, value) in recorded {
        match key.as_str() {
            "indexing_path" => stored.paths.indexing_paths = lines(&value),
            "ignore_patterns" => stored.indexing.ignore_patterns = lines(&value),
            "content_extensions" => stored.indexing.content_extensions = lines(&value),
            "include_hidden" => stored.indexing.include_hidden = value == "true",
            "follow_symlinks" => stored.indexing.follow_symlinks = value == "true",
            "store_text_for_snippets" => {
                stored.processing.store_text_for_snippets = value == "true"
            }
            "hash_length" => {
                if let Ok(n) = value.parse() {
                    stored.processing.hash_length = n;
                }
            }
            "tokenize" => stored.processing.tokenize = value,
            _ => {}
        }
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{walk_indexable_files, WalkEvent};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn tmp_tree(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-scope-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn empty_db(dir: &Path) -> PathBuf {
        let db = dir.join("index.sqlite");
        crate::db::open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
        db
    }

    /// Every file the walker actually emits under `config`'s single root.
    fn walked(config: &Config, db: &Path) -> HashSet<PathBuf> {
        let root = config.paths.indexing_paths[0].clone();
        walk_indexable_files(
            &[root],
            config.indexing.follow_symlinks,
            config.indexing.include_hidden,
            IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap(),
            db.to_str().unwrap(),
            config.clone(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            2,
        )
        .filter_map(|e| match e {
            WalkEvent::File(f) => Some(PathBuf::from(f.path)),
            WalkEvent::Stale(_) => None,
        })
        .collect()
    }

    /// Every file that physically exists under `root`, walker or no walker.
    fn on_disk(root: &Path) -> Vec<PathBuf> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .collect()
    }

    /// The whole point of `Scope`: it must reach the same verdict the walker
    /// does for every file on disk. If it is stricter, every prune deletes
    /// rows the next run puts straight back; if it is laxer, the rows the
    /// user excluded survive. Either way the index never settles.
    #[test]
    fn scope_agrees_with_the_walker() {
        let root = tmp_tree("agree");
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/keep2.txt"));
        touch(&root.join("sub/skip.tmp"));
        touch(&root.join("sub/node_modules/dep/index.js"));
        touch(&root.join(".hidden/inside.txt"));
        touch(&root.join(".dotfile"));
        touch(&root.join("build/out/artifact.o"));
        touch(&root.join("build/keep3.txt"));
        touch(&root.join("nested/build/also.o"));

        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config.indexing.ignore_patterns = vec![
            "*.tmp".into(),
            "node_modules".into(),
            // A full-path pattern: prunes this one directory, not every
            // directory called `out`.
            root.join("build/out").to_string_lossy().into_owned(),
        ];

        for include_hidden in [false, true] {
            config.indexing.include_hidden = include_hidden;
            let db = empty_db(&tmp_tree("agree-db"));
            let emitted = walked(&config, &db);
            let scope = Scope::from_config(&config).unwrap();

            for path in on_disk(&root) {
                assert_eq!(
                    scope.covers(&root, &path),
                    emitted.contains(&path),
                    "disagreement on {} (include_hidden = {})",
                    path.display(),
                    include_hidden
                );
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// A root is never filtered — the user chose it. A component pattern
    /// naming the root must not empty it out, but a full-path pattern that
    /// matches the root still prunes everything below it, because that is
    /// what the walker's ancestor check does when it reads the children.
    #[test]
    fn a_root_is_never_filtered_but_its_children_still_are() {
        let base = tmp_tree("root-name");
        let root = base.join("node_modules");
        touch(&root.join("keep.txt"));
        touch(&root.join("node_modules/nested.txt"));

        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config.indexing.ignore_patterns = vec!["node_modules".into()];
        let scope = Scope::from_config(&config).unwrap();
        assert!(scope.covers(&root, &root.join("keep.txt")));
        assert!(!scope.covers(&root, &root.join("node_modules/nested.txt")));

        let db = empty_db(&tmp_tree("root-name-db"));
        let emitted = walked(&config, &db);
        for path in on_disk(&root) {
            assert_eq!(scope.covers(&root, &path), emitted.contains(&path));
        }

        // A full-path pattern reaching the root itself takes the whole tree.
        config.indexing.ignore_patterns = vec![root.to_string_lossy().into_owned()];
        let scope = Scope::from_config(&config).unwrap();
        assert!(!scope.covers(&root, &root.join("keep.txt")));

        std::fs::remove_dir_all(&base).ok();
    }

    /// Root ownership compares whole components, so a sibling whose name
    /// merely starts with a root's is not inside it — a prune that got this
    /// wrong would delete a neighbouring folder's entire index.
    #[test]
    fn owning_root_does_not_match_name_prefixes() {
        let base = tmp_tree("prefix");
        let root = base.join("data");
        let sibling = base.join("database");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        let scope = Scope::from_config(&config).unwrap();

        assert_eq!(scope.owning_root(&root.join("f.txt")), Some(root.as_path()));
        assert_eq!(scope.owning_root(&sibling.join("f.txt")), None);
        // The root itself is a directory, never a row, and owns nothing.
        assert_eq!(scope.owning_root(&root), None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A path under no configured root has no rules that could be applied to
    /// it — a followed symlink's target is the real case. The scan reaches it
    /// by never visiting it, so `owning_root` returning `None` is what keeps
    /// it alive.
    #[test]
    fn a_path_outside_every_root_has_no_owner() {
        let base = tmp_tree("outside");
        let root = base.join("indexed");
        std::fs::create_dir_all(&root).unwrap();

        let mut config = Config::default();
        config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
        config.indexing.ignore_patterns = vec!["*".into()];
        let scope = Scope::from_config(&config).unwrap();

        assert_eq!(scope.owning_root(Path::new("/elsewhere/target.txt")), None);
        std::fs::remove_dir_all(&base).ok();
    }
}
