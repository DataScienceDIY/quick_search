//! Brings a stored index back in line with a changed configuration without
//! rebuilding it. Nothing here stamps the stored configuration — the caller
//! does, and only once the cursor reports finished; see [`outstanding_work`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet, IndexWork};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{content_extractable, fts_finalize_after_text_indexing, ExtractCursor};
use crate::indexing::ReconcileProgress;

/// How long [`advance`] may work before handing control back. This bounds
/// only the wait *between* statements; one statement can outlast the budget,
/// which is why the pass also publishes its connection via [`crate::db::InterruptGuard`].
pub const SLICE: Duration = Duration::from_millis(250);

/// One configured root, with the `files.parent` range it owns precomputed.
struct Root {
    path: PathBuf,
    lo: String,
    hi: String,
}

/// The set of paths the current configuration would index: the walker's own
/// filtering rules, applied to a path that is already stored. The two must
/// agree exactly, or every run would re-add what the last prune removed.
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

    /// Whether the walker would still emit `path` while walking `root`.
    /// Mirrors `read_directory`'s filtering; a root itself is never filtered.
    pub fn covers(&self, root: &Path, path: &Path) -> bool {
        self.covers_cached(root, path, &mut CoverCache::default())
    }

    /// [`Scope::covers`], reusing verdicts for the directories on the way down —
    /// on Windows this collapses per-ancestor `symlink_metadata` opens to ~one per directory.
    pub fn covers_cached(&self, root: &Path, path: &Path, cache: &mut CoverCache) -> bool {
        if self.ignore.matches_path_pattern(path) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let mut current = root.to_path_buf();
        let depth = relative.components().count();
        for (i, component) in relative.components().enumerate() {
            let std::path::Component::Normal(name) = component else {
                // Stored paths are canonical; anything but a plain name did not come from a walk.
                return false;
            };
            current.push(name);
            // Leaves are asked once and never again; caching them grows the map for no hits.
            let is_leaf = i + 1 == depth;
            if !is_leaf {
                if let Some(allowed) = cache.get(&current) {
                    if !allowed {
                        return false;
                    }
                    continue;
                }
            }
            let allowed = self.component_allowed(&current, &name.to_string_lossy());
            if !is_leaf {
                cache.insert(current.clone(), allowed);
            }
            if !allowed {
                return false;
            }
        }
        true
    }

    fn component_allowed(&self, current: &Path, name: &str) -> bool {
        // `symlink_metadata`: judged as itself, never its target — the walker does the same.
        if !self.include_hidden
            && crate::platform::entry_is_hidden(name, || std::fs::symlink_metadata(current).ok())
        {
            return false;
        }
        !self.ignore.matches_component(name)
    }
}

/// Directory verdicts already reached by [`Scope::covers_cached`]. Past the
/// cap the map is cleared outright — rows arrive in roughly insertion order,
/// so the entries that matter are the ones just added.
#[derive(Default)]
pub struct CoverCache {
    dirs: std::collections::HashMap<PathBuf, bool>,
}

impl CoverCache {
    const CAP: usize = 20_000;

    fn get(&self, dir: &Path) -> Option<bool> {
        self.dirs.get(dir).copied()
    }

    fn insert(&mut self, dir: PathBuf, allowed: bool) {
        if self.dirs.len() >= Self::CAP {
            self.dirs.clear();
        }
        self.dirs.insert(dir, allowed);
    }
}

/// How far an in-progress [`advance`] has got; the caller hands back the
/// same cursor each tick with a fresh deadline. Resumable within one pass
/// only: abandonment (or a mid-pass config edit) restarts a freshly derived
/// plan — every part is idempotent — so counters can go backwards between snapshots.
pub struct WorkCursor {
    work: IndexWork,
    scope: Scope,
    drop_idx: usize,
    dropped_aliases: bool,
    root_idx: usize,
    /// Last `(parent, name)` served by the scan — the keyset cursor. An empty
    /// parent means "start this root's range from its `lo` bound".
    after: (String, String),
    finalized: bool,
    /// Rows deleted so far.
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
    examined: usize,
    /// Row count taken once when the scan first needs a page — a denominator
    /// that moved would walk the display backwards.
    total: Option<usize>,
}

impl WorkCursor {
    pub fn new(work: IndexWork, config: &Config) -> Result<WorkCursor, String> {
        Ok(WorkCursor {
            work,
            scope: Scope::from_config(config)?,
            drop_idx: 0,
            dropped_aliases: false,
            root_idx: 0,
            after: (String::new(), String::new()),
            finalized: false,
            deleted: 0,
            recontented: 0,
            examined: 0,
            total: None,
        })
    }

    pub fn done(&self) -> bool {
        self.finalized
    }

    pub fn progress(&self) -> ReconcileProgress {
        ReconcileProgress {
            examined: self.examined,
            total: self.total,
            deleted: self.deleted,
            recontented: self.recontented,
        }
    }

    /// Whether a full walk must follow this reconciliation.
    pub fn reindex(&self) -> bool {
        self.work.reindex
    }

    /// The plan being applied — a restart against a newer config must not
    /// lose what this one left to do.
    pub fn work(&self) -> &IndexWork {
        &self.work
    }

    /// Drop the walk this reconciliation asked for, keeping the rest.
    pub fn cancel_reindex(&mut self) {
        self.work.reindex = false;
    }
}

/// Apply as much of `cursor` as fits before `deadline`, one page of rows per
/// transaction; call again until [`WorkCursor::done`]. `cancel` means "do
/// not start another statement" — the statement already running answers to
/// [`crate::db::interrupt`] and nothing else. A cancelled pass leaves the
/// work owed until a caller sees the cursor finish.
pub fn advance(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), String> {
    while cursor.drop_idx < cursor.work.drop_roots.len() {
        if cancelled(cancel) {
            return Ok(());
        }
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

    if cancelled(cancel) {
        return Ok(());
    }

    // After the root deletions and before the scan: the ranges spared here
    // must already be the final set of roots.
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
        if cursor.total.is_none() {
            if cancelled(cancel) {
                return Ok(());
            }
            cursor.total = Some(repo::row_count(conn)?);
        }
        let page = config.processing.batch_size.max(1) as i64;
        let mut covered = CoverCache::default();
        while cursor.root_idx < cursor.scope.roots.len() {
            if cancelled(cancel) {
                return Ok(());
            }
            let root = &cursor.scope.roots[cursor.root_idx];
            if cursor.after.0.is_empty() {
                // `(lo, "")` sorts below every row in the range — no stored name is empty.
                cursor.after = (root.lo.clone(), String::new());
            }
            let rows =
                repo::rows_in_range_page(conn, &cursor.after.0, &cursor.after.1, &root.hi, page)?;
            let Some(last) = rows.last() else {
                cursor.root_idx += 1;
                cursor.after = (String::new(), String::new());
                continue;
            };
            cursor.after = (last.parent.clone(), last.name.clone());
            cursor.examined += rows.len();
            let root = cursor.scope.roots[cursor.root_idx].path.clone();
            let (deleted, recontented) = apply_page(
                conn,
                config,
                registry,
                &cursor.scope,
                &cursor.work,
                &root,
                &rows,
                &mut covered,
            )?;
            cursor.deleted += deleted;
            cursor.recontented += recontented;
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    // Deletions leave FTS tombstones; a merge collapses them. Skipping it
    // costs only tidiness — the next run's merge does the same.
    if cancelled(cancel) {
        return Ok(());
    }
    if cursor.deleted > 0 || cursor.recontented > 0 {
        fts_finalize_after_text_indexing(conn);
    }
    cursor.finalized = true;
    Ok(())
}

fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Decide and write one page of rows. Returns `(deleted, recontented)`.
#[allow(clippy::too_many_arguments)]
fn apply_page(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    scope: &Scope,
    work: &IndexWork,
    root: &Path,
    rows: &[repo::ScopeRow],
    covered: &mut CoverCache,
) -> Result<(usize, usize), String> {
    let mut doomed: Vec<i64> = Vec::new();
    let mut stale_text: Vec<i64> = Vec::new();
    let mut to_pending: Vec<i64> = Vec::new();
    let mut to_na: Vec<i64> = Vec::new();

    for row in rows {
        let path = Path::new(&row.path);
        if work.prune_scope && !scope.covers_cached(root, path, covered) {
            doomed.push(row.id);
            continue;
        }
        if work.drop_text {
            stale_text.push(row.id);
        }
        if work.reconcile_content || work.restore_text {
            // The walker's decision, recomputed. Both directions run whenever
            // either flag is set: a disagreeing row is wrong however it got that way.
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
/// `config_validation` records it. Unrecorded fields keep `config`'s own
/// values, so they never read as changed.
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
            "hash_length" => match value.parse() {
                Ok(n) => stored.processing.hash_length = n,
                Err(e) => crate::log_warn!("stored hash_length {:?} unreadable: {}", value, e),
            },
            "tokenize" => stored.processing.tokenize = value,
            // A record from a newer build; ignore it.
            _ => {}
        }
    }
    Ok(stored)
}

/// The reconciliation the index still owes `config`. Empty is the normal
/// answer; non-empty means a pass was abandoned or the config was edited
/// while the app was closed. Roots are canonicalized first — that is the
/// spelling the record holds.
pub fn outstanding_work(db_path: &str, config: &Config) -> Result<IndexWork, String> {
    let conn = crate::db::open_existing(db_path, false)?;
    let mut current = config.clone();
    current.paths.indexing_paths = config.normalized_indexing_paths().into_iter().collect();
    let stored = stored_config(&conn, &current)?;
    Ok(crate::config::diff_actions(&stored, &current).work)
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
