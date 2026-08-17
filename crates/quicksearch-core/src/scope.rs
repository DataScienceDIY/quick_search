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
//! The scan is per-root: [`advance`] walks each root's `[lo, hi)` range
//! rather than the whole `files` table. With `follow_symlinks` on, a symlink
//! target is stored under its own canonical path, possibly outside every
//! root — such a row has no owning root and no filtering rules to apply, and
//! scanning by range never visits it.
//!
//! The pass can be abandoned through the flag-plus-interrupt pair
//! [`crate::db::InterruptSlot`] describes. That is safe because nothing here
//! records anything: the *caller* stamps the stored configuration, only once
//! the cursor reports finished, so an abandoned pass leaves the next run to
//! derive the same plan again. See [`outstanding_work`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet, IndexWork};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{content_extractable, fts_finalize_after_text_indexing, ExtractCursor};
use crate::indexing::ReconcileProgress;

/// How long [`advance`] may work before handing control back — a bound on
/// how long a Stop, a search or a further config edit waits behind a scan.
///
/// It bounds the wait *between* statements only; one statement can outlast
/// the whole budget, which is why the pass also publishes its connection
/// through [`crate::db::InterruptGuard`].
pub const SLICE: Duration = Duration::from_millis(250);

/// One configured root, with the `files.path` range it owns precomputed.
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
    ///
    /// Mirrors `read_directory`'s three `continue`s. Full-path ignore
    /// patterns are tested once against the whole path
    /// ([`IgnoreSet::matches_path_pattern`] walks every ancestor); the hidden
    /// and component-pattern rules per component *below* the root — a root
    /// itself is never filtered.
    pub fn covers(&self, root: &Path, path: &Path) -> bool {
        self.covers_cached(root, path, &mut CoverCache::default())
    }

    /// [`Scope::covers`], reusing the verdicts already reached for the
    /// directories on the way down.
    ///
    /// On Unix the repeated ancestor tests are free string comparisons. On
    /// Windows each component is a `symlink_metadata` — a `CreateFileW`
    /// through the full filter-driver stack; a 3M-row index at depth 8 was
    /// ~24M file opens. The cache collapses that to roughly one per
    /// directory.
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
                // Stored paths are canonical, so stripping a canonical root
                // leaves plain names. Anything else did not come from a walk.
                return false;
            };
            current.push(name);
            // Leaves are asked once and never again; caching them grows the
            // map one entry per row for no hits.
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

    /// Whether one path component passes the hidden and component-pattern
    /// rules. `current` is its full path, which the attribute test needs.
    fn component_allowed(&self, current: &Path, name: &str) -> bool {
        // The metadata closure is only consulted on Windows, where hidden is
        // an attribute rather than a leading dot; on Unix this is zero
        // syscalls. `symlink_metadata` because the component is judged as
        // itself, never as what it points at — the walker does the same.
        if !self.include_hidden
            && crate::platform::entry_is_hidden(name, || std::fs::symlink_metadata(current).ok())
        {
            return false;
        }
        !self.ignore.matches_component(name)
    }
}

/// Directory verdicts already reached by [`Scope::covers_cached`].
///
/// Bounded: a multi-million-row scan would otherwise hold every directory
/// under every root at once. Past the cap the map is cleared outright — rows
/// arrive in roughly insertion order, so the entries that matter are the
/// ones just added.
#[derive(Default)]
pub struct CoverCache {
    dirs: std::collections::HashMap<PathBuf, bool>,
}

impl CoverCache {
    /// Directories remembered before the map is cleared. Roughly 100 bytes per
    /// entry, so this is a few megabytes at most.
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
/// same cursor each tick with a fresh deadline.
///
/// Resumable within one pass only: an abandoned cursor takes its position
/// with it, and the next attempt restarts a freshly derived plan — every
/// part of the pass is idempotent so that costs time and nothing else. A
/// config edit arriving mid-pass has the same effect, which is why the
/// counters can go backwards between two published snapshots.
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
    /// Rows the scan has re-tested against the current configuration.
    examined: usize,
    /// Rows in the index, counted once when the scan first needs a page — a
    /// denominator that moved would walk the display backwards. `None` until
    /// then, and for a plan that reads no rows at all.
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
            after: String::new(),
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

    /// A snapshot for the status the caller publishes.
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
///
/// `cancel` means "do not start another statement" — the statement already
/// running answers to [`crate::db::interrupt`] and nothing else. A cancelled
/// pass leaves the work owed: the stored configuration is stamped only by a
/// caller that saw the cursor finish.
pub fn advance(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), String> {
    // Whole ranges first: deleting by range spares the scan the work.
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
        // One count, the first time a page is actually needed; without it
        // the display has no denominator at all.
        if cursor.total.is_none() {
            if cancelled(cancel) {
                return Ok(());
            }
            cursor.total = Some(repo::row_count(conn)?);
        }
        let page = config.processing.batch_size.max(1) as i64;
        // Lives across pages: consecutive pages walk the same directories.
        let mut covered = CoverCache::default();
        while cursor.root_idx < cursor.scope.roots.len() {
            if cancelled(cancel) {
                return Ok(());
            }
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

    // Deletions leave FTS tombstones; the automerge collapses them. Skipping
    // it costs only tidiness — the next run's automerge does the same.
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
            // The walker's own decision, recomputed. Both directions run
            // whenever either flag is set: a row that disagrees with the
            // current config is wrong however it got that way.
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
/// substituted back in. Fields the table does not record keep `config`'s own
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
                // Keeping the caller's value makes the diff describe a config
                // the index was not built under; don't let it pass in silence.
                Err(e) => crate::log_warn!("stored hash_length {:?} unreadable: {}", value, e),
            },
            "tokenize" => stored.processing.tokenize = value,
            // An unrecognized key is a record from a newer build; ignore it.
            _ => {}
        }
    }
    Ok(stored)
}

/// The reconciliation the index still owes `config`, derived from its own
/// record of what it was last brought into line with.
///
/// Empty is the normal answer; non-empty means a pass was abandoned or the
/// config was edited while the app was closed. Roots are canonicalized first
/// — that is the spelling the record holds.
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
