//! Brings a stored index back in line with a changed configuration without
//! rebuilding it. Nothing here stamps the stored configuration; the caller
//! does, once the cursor reports finished (see [`outstanding_work`]).
//!
//! # Where the time goes
//!
//! Almost entirely in FTS5: withdrawing content from a row tombstones its
//! posting, 100x the cost of deciding the row. `examples/contentprobe.rs` and
//! `examples/pruneprobe.rs` attribute a pass to its phases, and three changes
//! came out of them: [`SLICE`]-long transactions instead of one per page,
//! [`PagePlan`]'s chunked writes narrowed to the rows that can hold what is
//! being cleared, and [`crate::file_handling::fts_begin_tombstone_burst`], by
//! far the largest (read it for why).
//!
//! Measured and not adopted:
//!
//! - A bigger page cache for the pass. Raising `PRAGMAS_INCREMENTAL`'s 4 MiB
//!   to 64 MiB cuts misses from 79,266 to 6,499 but not the clock (927 ms
//!   against 976): the tombstone writes behind the misses cost the time.
//! - Range-deleting a wholly excluded directory instead of paging through it
//!   (`pruneprobe`'s `+subtree`): 247 ms against `+fts(done)`'s 243.
//! - Scanning by rowid rather than `(parent, name)` when `prune_scope` is off
//!   (never built): reading the pages is 24–30 ms of a ~950 ms pass.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};

use crate::config::{Config, IgnoreSet, IndexWork};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    content_extractable, fts_begin_tombstone_burst, fts_end_tombstone_burst, RootRange,
};
use crate::indexing::ReconcileProgress;

/// How long [`advance`] may work before handing control back. It bounds only
/// the wait between statements; one statement can outlast it, so the pass
/// also publishes its connection through [`crate::db::InterruptGuard`].
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
    ignore: Arc<IgnoreSet>,
    include_hidden: bool,
}

impl Scope {
    /// `config`'s scope, filtering by `ignore` rather than by compiling
    /// `config`'s patterns: the coordinator hands in its last good set, so a
    /// live edit whose glob does not compile cannot stall its queue.
    pub fn new(config: &Config, ignore: Arc<IgnoreSet>) -> Scope {
        let roots = config
            .normalized_indexing_paths()
            .into_iter()
            .map(|root| {
                let range = RootRange::for_root(&root);
                Root {
                    path: PathBuf::from(root),
                    lo: range.lo,
                    hi: range.hi,
                }
            })
            .collect();
        Scope {
            roots,
            ignore,
            include_hidden: config.indexing.include_hidden,
        }
    }

    pub fn from_config(config: &Config) -> Result<Scope, String> {
        let ignore = IgnoreSet::compile(&config.indexing.ignore_patterns)
            .map_err(|e| format!("ignore patterns: {}", e))?;
        Ok(Scope::new(config, Arc::new(ignore)))
    }

    pub fn ignore(&self) -> &IgnoreSet {
        &self.ignore
    }

    /// The innermost root containing `path`, by whole components, the root
    /// itself included ([`crate::platform::split_at_root`]). Roots are held
    /// canonical, so `path` must be too, in the spelling the index stores: a
    /// root that is itself a link is reached through its canonical path, and
    /// so is every file under it.
    pub fn root_of(&self, path: &Path) -> Option<&Path> {
        let roots = self.roots.iter().map(|root| root.path.as_path());
        crate::platform::split_at_root(path, roots).map(|(root, _)| root)
    }

    /// Whether the walker would still emit `path` while walking `root`.
    /// Mirrors `read_directory`'s filtering; a root itself is never filtered.
    pub fn covers(&self, root: &Path, path: &Path) -> bool {
        self.covers_cached(root, path, &mut CoverCache::default())
    }

    /// [`Scope::covers`], reusing verdicts for the directories on the way
    /// down; on Windows that collapses per-ancestor `symlink_metadata` opens to
    /// about one per directory.
    pub fn covers_cached(&self, root: &Path, path: &Path, cache: &mut CoverCache) -> bool {
        if self.ignore.matches_path_pattern(path) {
            return false;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let mut current = root.to_path_buf();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let std::path::Component::Normal(name) = component else {
                // Stored paths are canonical; anything but a plain name did not come from a walk.
                return false;
            };
            current.push(name);
            // Leaves are asked once and never again; caching them grows the map for no hits.
            let is_leaf = components.peek().is_none();
            let allowed = (!is_leaf)
                .then(|| cache.get(&current))
                .flatten()
                .unwrap_or_else(|| {
                    let allowed = self.component_allowed(&current, &name.to_string_lossy());
                    if !is_leaf {
                        cache.insert(current.clone(), allowed);
                    }
                    allowed
                });
            if !allowed {
                return false;
            }
        }
        true
    }

    fn component_allowed(&self, current: &Path, name: &str) -> bool {
        // `symlink_metadata`: judged as itself, never its target, as the walker does.
        if !self.include_hidden
            && crate::platform::entry_is_hidden(name, || std::fs::symlink_metadata(current).ok())
        {
            return false;
        }
        !self.ignore.matches_component(name)
    }
}

/// Directory verdicts already reached by [`Scope::covers_cached`]. Past the
/// cap the map is cleared outright: rows arrive in roughly insertion order, so
/// the entries that matter are the ones just added.
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

/// How far an in-progress [`advance`] has got; the caller hands back the same
/// cursor each tick with a fresh deadline. Resumable within one pass only:
/// abandoning it, or a config edit mid-pass, restarts from a freshly derived
/// plan (every part is idempotent), so counters can go backwards between
/// snapshots.
pub struct WorkCursor {
    work: IndexWork,
    scope: Scope,
    drop_idx: usize,
    root_idx: usize,
    /// Last `(parent, name)` served by the scan: the keyset cursor. An empty
    /// parent means "start this root's range from its `lo` bound".
    after: (String, String),
    finalized: bool,
    /// Rows deleted so far.
    pub deleted: usize,
    /// Rows whose content state or stored text was re-decided.
    pub recontented: usize,
    examined: usize,
    /// Row count, taken once when the scan first needs a page: a moving
    /// denominator would walk the display backwards.
    total: Option<usize>,
}

impl WorkCursor {
    pub fn new(work: IndexWork, config: &Config) -> Result<WorkCursor, String> {
        Ok(WorkCursor {
            work,
            scope: Scope::from_config(config)?,
            drop_idx: 0,
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

    /// Whether the row scan has reached the end of the last root. Distinct from
    /// [`WorkCursor::done`], which also waits on the tombstone merge.
    fn scan_done(&self) -> bool {
        self.root_idx >= self.scope.roots.len()
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

    /// The plan being applied, so a restart against a newer config keeps what
    /// this one left to do.
    pub fn work(&self) -> &IndexWork {
        &self.work
    }

    /// Drop the walk this reconciliation asked for, keeping the rest.
    pub fn cancel_reindex(&mut self) {
        self.work.reindex = false;
    }
}

/// Apply as much of `cursor` as fits before `deadline`; call again until
/// [`WorkCursor::done`]. `cancel` means "do not start another statement": the
/// statement already running answers to [`crate::db::interrupt`] and nothing
/// else. A cancelled pass leaves the work owed until a caller sees the cursor
/// finish.
pub fn advance(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), String> {
    while let Some(root) = cursor.work.drop_roots.get(cursor.drop_idx) {
        if cancelled(cancel) {
            return Ok(());
        }
        let range = RootRange::for_root(root);
        cursor.deleted +=
            delete_in_burst(conn, "drop-root", |tx| repo::delete_subtree(tx, &range))?;
        cursor.drop_idx += 1;
        if Instant::now() >= deadline {
            return Ok(());
        }
    }

    if cancelled(cancel) {
        return Ok(());
    }

    if cursor.work.scans_rows() {
        if cursor.total.is_none() {
            if cancelled(cancel) {
                return Ok(());
            }
            cursor.total = Some(repo::row_count(conn)?);
        }
        scan_rows(conn, config, registry, cursor, deadline, cancel)?;
        if !cursor.scan_done() {
            return Ok(());
        }
    }

    // Deletions leave FTS tombstones; restoring the threshold and merging
    // collapses them. Skipping the merge costs only tidiness (the next run
    // merges too), but the threshold must go back whether or not anything was
    // deleted: `scan_rows` lowers it before it knows.
    if cancelled(cancel) {
        return Ok(());
    }
    fts_end_tombstone_burst(conn);
    cursor.finalized = true;
    Ok(())
}

/// Run one deletion in its own transaction with the tombstone burst on.
/// `what` names the transaction in errors.
fn delete_in_burst(
    conn: &mut Connection,
    what: &str,
    delete: impl FnOnce(&Transaction<'_>) -> Result<usize, String>,
) -> Result<usize, String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin {} transaction: {}", what, e))?;
    // Removing a root can take every posting under it, the same burst the row
    // scan makes; `advance` restores the threshold and merges.
    fts_begin_tombstone_burst(&tx);
    let removed = delete(&tx)?;
    tx.commit()
        .map_err(|e| format!("commit {} transaction: {}", what, e))?;
    Ok(removed)
}

/// Rows one [`advance`] call may scan before it commits and returns,
/// whatever the clock says. A backstop: [`SLICE`] normally ends the call
/// first. It keeps a fast disk and a wide slice from building an unbounded set
/// of dirty pages before anything is durable.
const COMMIT_ROWS: usize = 20_000;

/// Page through every configured root in one transaction, deciding and
/// writing rows until the deadline, [`COMMIT_ROWS`] or cancellation ends the
/// call.
///
/// Reads run inside the transaction, so each page sees the previous page's
/// writes. The cursor advances with the reads, so a rollback leaves it ahead
/// of the database; that is safe because callers discard the cursor after a
/// failed [`advance`]. Cancellation commits what is done; the work is
/// idempotent. The deadline is checked only after a page is applied, so every
/// call makes progress even with an expired deadline.
fn scan_rows(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cursor.scan_done() || cancelled(cancel) {
        return Ok(());
    }
    let page = config.processing.batch() as i64;
    let mut covered = CoverCache::default();
    let mut plan = PagePlan::default();
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin reconcile transaction: {}", e))?;
    // Inside the transaction, so a rollback puts the threshold back with
    // everything else; `advance` restores it for good when the pass ends.
    // First thing in it, so the flush this implies has nothing to flush.
    fts_begin_tombstone_burst(&tx);
    let mut buffered = 0usize;
    while !cursor.scan_done() {
        let root = &cursor.scope.roots[cursor.root_idx];
        if cursor.after.0.is_empty() {
            // `(lo, "")` sorts below every row in the range: no stored name is empty.
            cursor.after = (root.lo.clone(), String::new());
        }
        let rows = repo::rows_in_range_page(&tx, &cursor.after.0, &cursor.after.1, &root.hi, page)?;
        let Some(last) = rows.last() else {
            // An exhausted root is not a page of work, so moving to the next
            // one cannot spend the slice on its own.
            cursor.root_idx += 1;
            cursor.after = (String::new(), String::new());
            continue;
        };
        cursor.after = (last.parent.clone(), last.name.clone());
        cursor.examined += rows.len();
        buffered += rows.len();
        let root = cursor.scope.roots[cursor.root_idx].path.clone();
        let (deleted, recontented) = apply_page(
            &tx,
            config,
            registry,
            &cursor.scope,
            &cursor.work,
            &root,
            &rows,
            &mut covered,
            &mut plan,
        )?;
        cursor.deleted += deleted;
        cursor.recontented += recontented;
        if buffered >= COMMIT_ROWS || cancelled(cancel) || Instant::now() >= deadline {
            break;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit reconcile transaction: {}", e))
}

fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// One page's decisions, bucketed by destination and by stored state so
/// every write goes out as one chunked `IN (...)` list. Stored state says
/// which dependent writes a row needs: only [`repo::STATE_DONE`] holds a
/// posting and body, only [`repo::STATE_FAILED`] a failure record. Reused
/// across pages; `clear` keeps the capacity.
#[derive(Default)]
struct PagePlan {
    /// Rows leaving the index entirely, and the subset of them holding a
    /// posting (see [`repo::delete_ids`]).
    doomed: Vec<i64>,
    doomed_with_postings: Vec<i64>,
    /// Surviving rows changing `content_state`, and across both, those whose
    /// stored state holds a posting or a failure record to clear first.
    to_pending: Vec<i64>,
    to_na: Vec<i64>,
    restated_with_postings: Vec<i64>,
    restated_with_failures: Vec<i64>,
}

impl PagePlan {
    fn clear(&mut self) {
        for list in [
            &mut self.doomed,
            &mut self.doomed_with_postings,
            &mut self.to_pending,
            &mut self.to_na,
            &mut self.restated_with_postings,
            &mut self.restated_with_failures,
        ] {
            list.clear();
        }
    }

    /// Note that a surviving row is changing state, and what it must shed first.
    fn restate(&mut self, row: &repo::ScopeRow, to_pending: bool) {
        if to_pending {
            self.to_pending.push(row.id);
        } else {
            self.to_na.push(row.id);
        }
        match row.content_state {
            repo::STATE_DONE => self.restated_with_postings.push(row.id),
            repo::STATE_FAILED => self.restated_with_failures.push(row.id),
            _ => {}
        }
    }

    /// Returns `(deleted, recontented)`.
    fn write(&self, tx: &Transaction<'_>) -> Result<(usize, usize), String> {
        let deleted = repo::delete_ids(tx, &self.doomed, &self.doomed_with_postings)?;
        repo::clear_content_for_ids(tx, &self.restated_with_postings)?;
        repo::clear_failed_for_ids(tx, &self.restated_with_failures)?;
        repo::set_content_state(tx, &self.to_pending, repo::STATE_PENDING)?;
        repo::set_content_state(tx, &self.to_na, repo::STATE_NA)?;
        Ok((deleted, self.to_pending.len() + self.to_na.len()))
    }
}

/// Decide one page of rows into `plan`, then write it. Returns
/// `(deleted, recontented)`.
#[allow(clippy::too_many_arguments)]
fn apply_page(
    tx: &Transaction<'_>,
    config: &Config,
    registry: &Registry,
    scope: &Scope,
    work: &IndexWork,
    root: &Path,
    rows: &[repo::ScopeRow],
    covered: &mut CoverCache,
    plan: &mut PagePlan,
) -> Result<(usize, usize), String> {
    plan.clear();
    for row in rows {
        let path = Path::new(&row.path);
        if work.prune_scope && !scope.covers_cached(root, path, covered) {
            plan.doomed.push(row.id);
            if row.content_state == repo::STATE_DONE {
                plan.doomed_with_postings.push(row.id);
            }
            continue;
        }
        if work.reconcile_content {
            // The walker's decision, recomputed. Both directions: a
            // disagreeing row is wrong however it got that way. All but
            // dehydration, which the row does not record: a cloud
            // placeholder's NA row goes pending here, and `decide_content`
            // puts it back unread.
            let wants = row.size <= config.processing.maximum_text_file_size
                && content_extractable(path, row.mime.as_deref(), config, registry);
            if !wants && row.content_state != repo::STATE_NA {
                plan.restate(row, false);
            } else if wants && row.content_state == repo::STATE_NA {
                plan.restate(row, true);
            }
        }
    }
    plan.write(tx)
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
            // Recorded as the layout: the flag's opposite.
            "fts_positions" => stored.processing.compact_text_index = value == "false",
            "hash_length" => match value.parse() {
                Ok(n) => stored.processing.hash_length = n,
                Err(e) => crate::log_warn!("stored {} {:?} unreadable: {}", key, value, e),
            },
            "maximum_text_file_size" => match value.parse() {
                Ok(n) => stored.processing.maximum_text_file_size = n,
                Err(e) => crate::log_warn!("stored {} {:?} unreadable: {}", key, value, e),
            },
            // A key this build does not record (from a newer build, or the
            // retired `hash_algorithm` and `tokenize`); ignore it.
            _ => {}
        }
    }
    Ok(stored)
}

/// While any root of `current` fails to resolve, add every recorded root it
/// lacks, so diffing against [`stored_config`] drops nothing. An offline root
/// comes back as spelled: through a link or with a trailing separator, that
/// differs from the canonical spelling it was recorded under and would read as
/// a removed root, its rows deleted. Which recorded root it was cannot be
/// told, so every drop waits; a root removed meanwhile goes, by its recorded
/// spelling, on the first run where every configured root resolves. Never keyed
/// on emptiness: a new empty root would hold every drop.
pub(crate) fn hold_offline_roots(conn: &Connection, current: &mut Config) -> Result<(), String> {
    let Some(offline) = current
        .resolved_indexing_paths()
        .into_iter()
        .find(|root| root.canonicalize().is_err())
    else {
        return Ok(());
    };
    let mut held = 0;
    for root in stored_config(conn, current)?.paths.indexing_paths {
        if !current.paths.indexing_paths.contains(&root) {
            current.paths.indexing_paths.push(root);
            held += 1;
        }
    }
    if held > 0 {
        crate::log_info!(
            "{} cannot be reached; keeping {} recorded root(s) until every root resolves",
            offline.display(),
            held
        );
    }
    Ok(())
}

/// The reconciliation the index still owes `config`. Empty is the normal
/// answer; non-empty means a pass was abandoned or the config was edited
/// while the app was closed. Roots are canonicalized first, the spelling the
/// record holds. Nothing is owed a drop while a configured root is offline
/// (see `hold_offline_roots`).
pub fn outstanding_work(db_path: &str, config: &Config) -> Result<IndexWork, String> {
    let conn = crate::db::open_existing(db_path, false)?;
    let mut current = config.clone();
    current.paths.indexing_paths = config.normalized_indexing_paths().into_iter().collect();
    hold_offline_roots(&conn, &mut current)?;
    let stored = stored_config(&conn, &current)?;
    Ok(crate::config::diff_actions(&stored, &current).work)
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
