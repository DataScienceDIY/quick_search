//! Brings a stored index back in line with a changed configuration without
//! rebuilding it. Nothing here stamps the stored configuration — the caller
//! does, and only once the cursor reports finished; see [`outstanding_work`].
//!
//! # Where the time goes
//!
//! Almost entirely in FTS5: withdrawing content from a row tombstones its
//! posting, and that is 100x what deciding the row costs. `examples/contentprobe.rs`
//! and `examples/pruneprobe.rs` attribute a pass to a phase; the three
//! decisions that came out of them are [`SLICE`]-long transactions rather than
//! one per page, [`PagePlan`]'s chunked writes narrowed to the rows that can
//! actually hold what is being cleared, and
//! [`crate::file_handling::fts_begin_tombstone_burst`] — much the largest of
//! the three, and the place to read for why.
//!
//! Measured and **not** adopted, so they are not re-derived. Both looked
//! obvious; neither paid.
//!
//! - **A bigger page cache for the pass.** `PRAGMAS_INCREMENTAL`'s 4 MiB looks
//!   far too small for a scan that rewrites across the whole index, and raising
//!   it does exactly what it should to the miss count — 79,266 down to 6,499 at
//!   64 MiB — while the clock stays put (927 ms against 976). The misses were
//!   never the expensive part; the tombstone writes behind them were.
//! - **Range-deleting a wholly-excluded directory** rather than paging through
//!   it (`pruneprobe`'s `+subtree`). It does skip the page loop for every
//!   doomed row, and costs 247 ms against `+fts(done)`'s 243.
//!
//! A third was never built, for the same reason: scanning by rowid instead of
//! by `(parent, name)` when `prune_scope` is off, which the root loop would
//! allow. Reading the pages is 24–30 ms of a ~950 ms pass — there is nothing
//! there to win.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::{Config, IgnoreSet, IndexWork};
use crate::db::repo;
use crate::extract::Registry;
use crate::file_handling::{
    content_extractable, fts_begin_tombstone_burst, fts_end_tombstone_burst, ExtractCursor,
};
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
        // A de-configured root can take every posting under it with it, which
        // is the same burst the row scan makes; `advance` restores and merges.
        fts_begin_tombstone_burst(&tx);
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
        fts_begin_tombstone_burst(&tx);
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
        scan_rows(conn, config, registry, cursor, deadline, cancel)?;
        if !cursor.scan_done() {
            return Ok(());
        }
    }

    // Deletions leave FTS tombstones; restoring the threshold and merging
    // collapses them. Skipping the merge costs only tidiness — the next run's
    // does the same — but the threshold must go back whether anything was
    // deleted or not, since `scan_rows` lowers it before it knows.
    if cancelled(cancel) {
        return Ok(());
    }
    fts_end_tombstone_burst(conn);
    cursor.finalized = true;
    Ok(())
}

/// Rows one transaction may cover before it commits, whatever the clock says.
///
/// A backstop, not the primary limit: [`SLICE`] normally ends a transaction
/// first. It exists so that a fast disk and a wide slice cannot build an
/// unbounded set of dirty pages before anything is durable.
const COMMIT_ROWS: usize = 20_000;

/// Page every configured root, deciding and writing rows until the deadline.
///
/// One transaction spans as much of the slice as it can rather than one per
/// page. The read runs inside it, so each page sees the previous one's writes;
/// the cursor advances with the reads, so a rollback leaves it ahead of the
/// database — which is safe only because every caller of a failed [`advance`]
/// discards the cursor and nothing is stamped until one finishes. Cancellation
/// commits what it has: the work is idempotent, so a resumed cursor redoing it
/// would be correct too, but there is no reason to throw it away.
///
/// **The deadline is tested only after a page has been applied**, so a call
/// always makes progress. Testing it on entry instead is a spin: the caller
/// loops until the cursor finishes, and one that hands over an already-expired
/// deadline — which `advance`'s own `row_count` can produce on a large index,
/// and which `scope_tests` does deliberately — would never advance it.
fn scan_rows(
    conn: &mut Connection,
    config: &Config,
    registry: &Registry,
    cursor: &mut WorkCursor,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let page = config.processing.batch_size.max(1) as i64;
    let mut covered = CoverCache::default();
    let mut plan = PagePlan::default();
    while !cursor.scan_done() {
        if cancelled(cancel) {
            return Ok(());
        }
        let tx = conn
            .transaction()
            .map_err(|e| format!("begin reconcile transaction: {}", e))?;
        // Inside the transaction, so a rollback puts the threshold back with
        // everything else; `advance` restores it for good when the pass ends.
        // First thing in it, so the flush this implies has nothing to flush.
        fts_begin_tombstone_burst(&tx);
        let mut buffered = 0usize;
        let mut spent = false;
        while !cursor.scan_done() {
            let root = &cursor.scope.roots[cursor.root_idx];
            if cursor.after.0.is_empty() {
                // `(lo, "")` sorts below every row in the range — no stored name is empty.
                cursor.after = (root.lo.clone(), String::new());
            }
            let rows =
                repo::rows_in_range_page(&tx, &cursor.after.0, &cursor.after.1, &root.hi, page)?;
            let Some(last) = rows.last() else {
                // An exhausted root is not a page of work — moving to the next
                // one must not be able to spend the slice on its own.
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
                spent = true;
                break;
            }
        }
        tx.commit()
            .map_err(|e| format!("commit reconcile transaction: {}", e))?;
        if spent {
            return Ok(());
        }
    }
    Ok(())
}

fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// One page's decisions, bucketed so every write goes out as a chunked
/// `IN (...)` list rather than a bound statement per row.
///
/// The buckets are by *stored* state as well as by destination, because that is
/// what says which of the three dependent writes a row actually needs: a
/// posting and a stored body exist only for `STATE_DONE`, a failure record only
/// for `STATE_FAILED` (`repo::leaving_done_always_takes_the_posting_with_it`
/// pins both). A `STATE_PENDING` row leaving for `STATE_NA` needs one `UPDATE`
/// and nothing else, where the per-row form spent four statements discovering
/// that twice over.
///
/// Reused across pages — `clear` keeps the capacity — so a whole pass allocates
/// these once.
#[derive(Default)]
struct PagePlan {
    /// Rows leaving the index entirely, and the subset of them that can hold a
    /// posting (see [`repo::delete_ids`]).
    doomed: Vec<i64>,
    doomed_content: Vec<i64>,
    /// Rows keeping their posting but losing the snippet source.
    stale_text: Vec<i64>,
    /// Surviving rows changing `content_state`, and — across both — those whose
    /// stored state says they have content or a failure record to clear first.
    to_pending: Vec<i64>,
    to_na: Vec<i64>,
    restated_content: Vec<i64>,
    restated_failure: Vec<i64>,
}

impl PagePlan {
    fn clear(&mut self) {
        for list in [
            &mut self.doomed,
            &mut self.doomed_content,
            &mut self.stale_text,
            &mut self.to_pending,
            &mut self.to_na,
            &mut self.restated_content,
            &mut self.restated_failure,
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
            repo::STATE_DONE => self.restated_content.push(row.id),
            repo::STATE_FAILED => self.restated_failure.push(row.id),
            _ => {}
        }
    }

    /// Returns `(deleted, recontented)`.
    fn write(&self, tx: &rusqlite::Transaction<'_>) -> Result<(usize, usize), String> {
        let deleted = if self.doomed.is_empty() {
            0
        } else {
            repo::delete_ids(tx, &self.doomed, &self.doomed_content)?
        };
        repo::drop_stored_text(tx, &self.stale_text)?;
        repo::clear_content_for_ids(tx, &self.restated_content)?;
        repo::clear_failed_for_ids(tx, &self.restated_failure)?;
        repo::set_content_state(tx, &self.to_pending, repo::STATE_PENDING)?;
        repo::set_content_state(tx, &self.to_na, repo::STATE_NA)?;
        Ok((deleted, self.to_pending.len() + self.to_na.len()))
    }
}

/// Decide one page of rows into `plan`, then write it. Returns
/// `(deleted, recontented)`.
#[allow(clippy::too_many_arguments)]
fn apply_page(
    tx: &rusqlite::Transaction<'_>,
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
                plan.doomed_content.push(row.id);
            }
            continue;
        }
        // Only a DONE row can have a `documents_text` body to drop.
        if work.drop_text && row.content_state == repo::STATE_DONE {
            plan.stale_text.push(row.id);
        }
        if work.reconcile_content || work.restore_text {
            // The walker's decision, recomputed. Both directions run whenever
            // either flag is set: a disagreeing row is wrong however it got that way.
            let wants = row.size <= config.processing.maximum_text_file_size
                && content_extractable(path, row.mime.as_deref(), config, registry);
            if !wants && row.content_state != repo::STATE_NA {
                plan.restate(row, false);
            } else if wants
                && (row.content_state == repo::STATE_NA
                    || (work.restore_text && row.content_state == repo::STATE_DONE))
            {
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
