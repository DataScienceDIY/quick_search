//! Where the time goes when an added ignore pattern scrubs the index.
//!
//! Adding one pattern sets `IndexWork { prune_scope: true, reindex: false }`
//! (`config::diff_actions`). `scope::advance` then reads every stored row
//! under every root, decides each one, and deletes the losers by id. This
//! probe attributes that time to a phase.
//!
//! ```text
//! cargo build -p quicksearch-core --example pruneprobe --release
//! ./target/release/examples/pruneprobe
//! ```
//!
//! `QSB_FILES`, `QSB_DIRS`, `QSB_DIR_DEPTH` and `QSB_CONTENT_EVERY` size the
//! corpus; `QSB_ARMS=plain|keyed|both` picks the key states (see
//! `common::arms`).
//!
//! The stages are cumulative, so consecutive rows subtract to a phase cost:
//!
//! ```text
//! page                 read the rows, decide nothing
//! +cover               ...and run the scope test per row
//! +files               ...and delete the doomed `files` rows
//! +fts(all)            ...and tombstone every doomed id
//! +fts(done)           ...tombstoning only ids that can have an FTS row
//! +subtree             ...range-deleting a doomed directory instead of paging it
//! ```
//!
//! The stages decompose the cost; none is the shipped shape. `scope::advance`
//! tombstones only the ids that can hold a posting (`+fts(done)`), commits per
//! slice, and turns FTS5's delete-merging off for the pass
//! (`file_handling::fts_begin_tombstone_burst`), so the `live` row lands under
//! every stage.
//!
//! Measured and not adopted: `+subtree` bought nothing over `+fts(done)`
//! (247 ms against 243 on the 40k corpus, with all 8,080 doomed rows skipping
//! the page loop), and raising the page cache cut misses twelvefold while
//! barely moving the clock.
//!
//! Read the `commit` column, not `fts`. FTS5 buffers a contentless delete in
//! memory and writes the tombstone pages when the transaction flushes, so the
//! `DELETE` times as nearly free and the cost lands in the commit. The
//! transaction-size table sweeps pages per transaction for the same reason:
//! if the flush costs per commit rather than per tombstone, transaction size
//! matters and row count does not.
//!
//! The final table prices the other bulk withdrawal, a completed run's stale
//! cleanup (`file_handling::cleanup_stale_index_entries`): the same doomed
//! rows, deleted by path. Its stages are probe code, fixed like the stages
//! above; its `live` row is the shipped function.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};

use quicksearch_core::config::Config;
use quicksearch_core::db::{self, repo};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{
    cleanup_stale_index_entries, fts_begin_tombstone_burst, fts_end_tombstone_burst,
    fts_finalize_after_text_indexing, split_db_path, RootRange,
};
use quicksearch_core::scope::{self, Scope, WorkCursor};
use quicksearch_core::testutil::{self, Arm, SeedSpec};

use common::{
    chunked, clone_arm, count, env_usize, merge_to_quiescence, open, print_merge, set_cache,
    Commit, Io, MERGE_PAGES, PAGE, SHIPPED_MIB,
};

/// The root every `seed_index` row hangs under. It need not exist on disk:
/// nothing in the prune path stats a stored path on Unix
/// (`platform::entry_hidden_reason` short-circuits on the dot prefix).
const ROOT: &str = "/seed";

/// The ignore patterns the probe can add, each excluding a fifth of the index.
///
/// `seed_index` names its second path segment `WORDS[(d * 7 + 13) % 35]`, and
/// because 7 and 35 share a factor only these five of the thirty-five words
/// are reachable, one per residue of `d mod 5`. So each excludes a fifth of
/// the directories, spread across the keyspace rather than gathered at one
/// end, which is the shape a real `node_modules` or `.git` pattern has. The
/// probe sweeps the excluded fraction by taking a prefix of this list.
const PRUNE_PATTERNS: [&str; 5] = ["jumps", "content", "revenue", "figure", "eta"];

/// The single-pattern case, and the one the stage table runs.
const PRUNE_PATTERN: &str = PRUNE_PATTERNS[0];

// ---------------------------------------------------------------------------
// Stages
// ---------------------------------------------------------------------------

/// Cumulative slices of the prune. Each does everything the one before it does.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Page,
    Cover,
    Files,
    FtsAll,
    FtsDone,
    Subtree,
}

impl Stage {
    const ALL: [Stage; 6] = [
        Stage::Page,
        Stage::Cover,
        Stage::Files,
        Stage::FtsAll,
        Stage::FtsDone,
        Stage::Subtree,
    ];

    fn label(self) -> &'static str {
        match self {
            Stage::Page => "page",
            Stage::Cover => "+cover",
            Stage::Files => "+files",
            Stage::FtsAll => "+fts(all)",
            Stage::FtsDone => "+fts(done)",
            Stage::Subtree => "+subtree",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Stage::Page => "page",
            Stage::Cover => "cover",
            Stage::Files => "files",
            Stage::FtsAll => "fts-all",
            Stage::FtsDone => "fts-done",
            Stage::Subtree => "subtree",
        }
    }

    /// Whether the doomed rows leave `files`.
    fn deletes_rows(self) -> bool {
        self >= Stage::Files
    }

    /// Whether the doomed ids are tombstoned in FTS.
    fn deletes_fts(self) -> bool {
        self >= Stage::FtsAll
    }

    /// Whether the FTS delete is narrowed to ids that can hold a posting.
    fn fts_filtered(self) -> bool {
        self >= Stage::FtsDone
    }
}

/// What one stage cost, split by phase.
#[derive(Default)]
struct Timing {
    page: Duration,
    cover: Duration,
    files: Duration,
    fts: Duration,
    commit: Duration,
    total: Duration,
    examined: usize,
    deleted: usize,
    /// Rows skipped by a range delete rather than paged through.
    skipped: usize,
    io: Io,
    misses: i64,
}

// ---------------------------------------------------------------------------
// The measured loop
// ---------------------------------------------------------------------------

/// Whether the walker would still descend into the stored parent `parent`.
///
/// `Scope::covers` judges a path component by component, which for a
/// directory asks what the walker asks before recursing. The trailing
/// separator every stored parent carries goes first, or the leaf component
/// would be empty.
fn dir_covered(scope: &Scope, root: &Path, parent: &str) -> bool {
    let trimmed = parent.trim_end_matches(['/', '\\']);
    scope.covers(root, Path::new(trimmed))
}

/// Page the index as `scope::advance` does, doing `stage`'s share of the work
/// and timing each phase apart.
///
/// The deletes are spelled out here rather than routed through `repo`, so the
/// filtered and unfiltered FTS variants compare in one run and the stages stay
/// fixed when `repo` changes.
fn run_stage(conn: &Connection, config: &Config, stage: Stage, commit: Commit) -> Timing {
    let scope = Scope::from_config(config).expect("compile the scope");
    let root = PathBuf::from(ROOT);
    let range = RootRange::for_root(ROOT);

    let mut t = Timing::default();
    let io_before = Io::read();
    let (_, misses_before) = testutil::cache_stats(conn);
    let started = Instant::now();

    let mut after = (range.lo.clone(), String::new());
    let mut covered = scope::CoverCache::default();
    // `unchecked_transaction` borrows shared, so one transaction can end and
    // the next begin inside the loop, as in `testutil::seed_index`.
    let mut tx = conn.unchecked_transaction().expect("begin");
    let mut pages_open = 0usize;
    let mut tx_since = Instant::now();
    loop {
        let at = Instant::now();
        let rows =
            repo::rows_in_range_page(&tx, &after.0, &after.1, &range.hi, PAGE).expect("read a page");
        t.page += at.elapsed();
        let Some(last) = rows.last() else { break };
        after = (last.parent.clone(), last.name.clone());
        pages_open += 1;

        if stage == Stage::Page {
            t.examined += rows.len();
            continue;
        }

        // The subtree short-circuit. A page spans several directories, so each
        // distinct parent is tested as it appears; the first doomed one is
        // range-deleted and the scan re-seeks past it. Testing only `rows[0]`
        // would fire only on pages that begin inside an excluded directory.
        let mut rows: &[repo::ScopeRow] = &rows;
        if stage == Stage::Subtree {
            let at = Instant::now();
            let mut doomed_at = None;
            let mut last_dir: Option<(&str, bool)> = None;
            for (i, row) in rows.iter().enumerate() {
                let ok = match last_dir {
                    Some((dir, ok)) if dir == row.parent => ok,
                    _ => {
                        let ok = dir_covered(&scope, &root, &row.parent);
                        last_dir = Some((&row.parent, ok));
                        ok
                    }
                };
                if !ok {
                    doomed_at = Some(i);
                    break;
                }
            }
            t.cover += at.elapsed();
            if let Some(i) = doomed_at {
                let dir = rows[i].parent.trim_end_matches(['/', '\\']).to_string();
                let sub = RootRange::for_root(&dir);
                let (removed, fts, files) = delete_range(&tx, &sub.lo, &sub.hi);
                t.fts += fts;
                t.files += files;
                t.deleted += removed;
                t.skipped += removed;
                // The rows before the doomed directory still need deciding;
                // everything from it on is gone or re-read after the seek.
                // Both happen inside the open transaction, so the seek sees
                // the range delete without a commit.
                after = (sub.hi.clone(), String::new());
                rows = &rows[..i];
            }
        }

        let at = Instant::now();
        let mut doomed: Vec<i64> = Vec::new();
        let mut doomed_fts: Vec<i64> = Vec::new();
        for row in rows {
            if scope.covers_cached(&root, Path::new(&row.path), &mut covered) {
                continue;
            }
            doomed.push(row.id);
            if row.content_state == repo::STATE_DONE {
                doomed_fts.push(row.id);
            }
        }
        t.cover += at.elapsed();
        t.examined += rows.len();

        if stage.deletes_rows() && !doomed.is_empty() {
            let fts_ids: &[i64] = if stage.fts_filtered() {
                &doomed_fts
            } else {
                &doomed
            };
            if stage.deletes_fts() {
                let at = Instant::now();
                chunked(
                    &tx,
                    "DELETE FROM searchabletext WHERE rowid IN",
                    fts_ids,
                    false,
                );
                t.fts += at.elapsed();
            }
            let at = Instant::now();
            t.deleted += chunked(&tx, "DELETE FROM files WHERE id IN", &doomed, false);
            t.files += at.elapsed();
        }

        if commit.due(pages_open, tx_since) {
            let at = Instant::now();
            tx.commit().expect("commit");
            t.commit += at.elapsed();
            tx = conn.unchecked_transaction().expect("begin");
            pages_open = 0;
            tx_since = Instant::now();
        }
    }
    let at = Instant::now();
    tx.commit().expect("commit");
    t.commit += at.elapsed();

    t.total = started.elapsed();
    t.io = Io::read().since(&io_before);
    let (_, misses_after) = testutil::cache_stats(conn);
    t.misses = misses_after - misses_before;
    t
}

/// Delete a whole parent range, tombstoning only the ids that can hold a
/// posting. Returns `(rows, fts time, files time)`.
fn delete_range(tx: &Connection, lo: &str, hi: &str) -> (usize, Duration, Duration) {
    let at = Instant::now();
    tx.execute(
        "DELETE FROM searchabletext WHERE rowid IN \
         (SELECT id FROM files WHERE parent >= ?1 AND parent < ?2 AND content_state = ?3)",
        rusqlite::params![lo, hi, repo::STATE_DONE],
    )
    .expect("tombstone range");
    let fts = at.elapsed();
    let at = Instant::now();
    let removed = tx
        .execute(
            "DELETE FROM files WHERE parent >= ?1 AND parent < ?2",
            rusqlite::params![lo, hi],
        )
        .expect("delete range");
    (removed, fts, at.elapsed())
}

// ---------------------------------------------------------------------------
// Stale cleanup stages
// ---------------------------------------------------------------------------

/// Cumulative slices of a completed run's stale cleanup, which deletes by
/// *path* rather than deciding rows from a page it already read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StaleShape {
    /// Per-path deletes, three statements each, with FTS handed every id. The
    /// control row: probe code, so it must not move when
    /// `cleanup_stale_index_entries` changes.
    PerRow,
    /// ...resolve `(id, content_state)` per path instead, then chunked
    /// `IN (...)` deletes with FTS narrowed to ids that can hold a posting.
    Ids,
    /// ...and hold FTS5's delete-merging off for the whole pass.
    Burst,
}

impl StaleShape {
    const ALL: [StaleShape; 3] = [StaleShape::PerRow, StaleShape::Ids, StaleShape::Burst];

    fn label(self) -> &'static str {
        match self {
            StaleShape::PerRow => "per-row",
            StaleShape::Ids => "+ids",
            StaleShape::Burst => "+burst",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            StaleShape::PerRow => "row",
            StaleShape::Ids => "ids",
            StaleShape::Burst => "burst",
        }
    }
}

/// Delete `paths` the way stale cleanup does, one transaction per `PAGE`
/// chunk, timing each phase. Column mapping in the shared table: `cover` is
/// the id resolution, `files` the `files` deletes, `fts` the tombstones plus
/// the trailing consolidation.
fn run_stale(conn: &Connection, paths: &[String], shape: StaleShape) -> Timing {
    let mut t = Timing::default();
    let io_before = Io::read();
    let (_, misses_before) = testutil::cache_stats(conn);
    let started = Instant::now();

    if shape == StaleShape::Burst {
        fts_begin_tombstone_burst(conn);
    }
    for page in paths.chunks(PAGE as usize) {
        let tx = conn.unchecked_transaction().expect("begin");
        if shape == StaleShape::PerRow {
            for path in page {
                let Some((parent, name)) = split_db_path(path) else {
                    continue;
                };
                let at = Instant::now();
                let id: Option<i64> = tx
                    .prepare_cached(
                        "DELETE FROM files WHERE parent = ?1 AND name = ?2 RETURNING id",
                    )
                    .expect("prepare")
                    .query_row(rusqlite::params![parent, name], |r| r.get(0))
                    .optional()
                    .expect("delete row");
                t.files += at.elapsed();
                let Some(id) = id else { continue };
                t.deleted += 1;
                let at = Instant::now();
                tx.prepare_cached("DELETE FROM searchabletext WHERE rowid = ?1")
                    .expect("prepare")
                    .execute([id])
                    .expect("tombstone");
                t.fts += at.elapsed();
                let at = Instant::now();
                tx.prepare_cached("DELETE FROM documents_text WHERE file_id = ?1")
                    .expect("prepare")
                    .execute([id])
                    .expect("clear body");
                t.files += at.elapsed();
            }
        } else {
            let at = Instant::now();
            let mut ids: Vec<i64> = Vec::new();
            let mut with_postings: Vec<i64> = Vec::new();
            {
                let mut sel = tx
                    .prepare_cached(
                        "SELECT id, content_state FROM files WHERE parent = ?1 AND name = ?2",
                    )
                    .expect("prepare");
                for path in page {
                    let Some((parent, name)) = split_db_path(path) else {
                        continue;
                    };
                    let row: Option<(i64, i64)> = sel
                        .query_row(rusqlite::params![parent, name], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })
                        .optional()
                        .expect("resolve");
                    if let Some((id, state)) = row {
                        ids.push(id);
                        if state == repo::STATE_DONE {
                            with_postings.push(id);
                        }
                    }
                }
            }
            t.cover += at.elapsed();
            let at = Instant::now();
            chunked(
                &tx,
                "DELETE FROM searchabletext WHERE rowid IN",
                &with_postings,
                false,
            );
            t.fts += at.elapsed();
            let at = Instant::now();
            t.deleted += chunked(&tx, "DELETE FROM files WHERE id IN", &ids, false);
            t.files += at.elapsed();
        }
        let at = Instant::now();
        tx.commit().expect("commit");
        t.commit += at.elapsed();
    }
    let at = Instant::now();
    if shape == StaleShape::Burst {
        fts_end_tombstone_burst(conn);
    } else {
        fts_finalize_after_text_indexing(conn);
    }
    t.fts += at.elapsed();

    t.total = started.elapsed();
    t.io = Io::read().since(&io_before);
    let (_, misses_after) = testutil::cache_stats(conn);
    t.misses = misses_after - misses_before;
    t
}

// ---------------------------------------------------------------------------
// Corpus and arms
// ---------------------------------------------------------------------------

fn spec() -> SeedSpec {
    SeedSpec {
        files: env_usize("QSB_FILES", 200_000),
        dirs: env_usize("QSB_DIRS", 2_000),
        // Coprime with 5. `seed_index` places content every `content_every`
        // files and directories every `dirs`, and [`PRUNE_PATTERN`] excludes
        // the directories where `d mod 5 == 0`. The default of 10 shares the
        // factor 5, so every extracted document would land in an excluded
        // directory and the prune would delete all the postings. 7 puts a
        // fifth of the documents in the doomed set. `main` asserts the
        // fraction.
        content_every: env_usize("QSB_CONTENT_EVERY", 7),
        // At least two segments, so the second can carry the excluded name;
        // see `SeedSpec::dir_depth` for how depth sets row width.
        dir_depth: env_usize("QSB_DIR_DEPTH", 6).max(2),
        // A real run commits in slices, each flushing FTS5's hash to its own
        // segment; one transaction would leave a single segment and
        // understate every tombstone cost.
        commit_every: 500,
        ..SeedSpec::default()
    }
}

fn config_with(patterns: &[&str]) -> Config {
    let mut config = Config::default();
    config.paths.indexing_paths = vec![ROOT.to_string()];
    config.indexing.ignore_patterns = patterns.iter().map(|p| p.to_string()).collect();
    config.processing.batch_size = PAGE as usize;
    config
}

/// Open `arm` on a run's own writer profile, `PRAGMAS_FAST`, which stale
/// cleanup runs on.
fn open_run_writer(arm: &Arm) -> Connection {
    arm.with_key(|| {
        db::open::open_existing(&arm.path.to_string_lossy(), true).expect("open the copy")
    })
}

// ---------------------------------------------------------------------------

fn main() {
    let spec = spec();

    println!(
        "corpus: {} files across {} dirs, 1 in {} with content, pattern {:?}",
        spec.files, spec.dirs, spec.content_every, PRUNE_PATTERN
    );

    for keyed in common::arms() {
        let label = if keyed { "keyed" } else { "plain" };
        let master = Arm::seed(label, &format!("prune-{}", label), keyed, &spec);
        println!(
            "\n=== {} ===  seeded in {:.1}s, {}",
            label,
            master.seeded_in.as_secs_f64(),
            common::mib(master.size_bytes())
        );

        // Census: what the pattern takes, read off the index.
        {
            let conn = open(&master);
            let total = count(&conn, "SELECT COUNT(*) FROM files");
            let doomed = count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM files WHERE parent LIKE '%/{}/%'",
                    PRUNE_PATTERN
                ),
            );
            let doomed_fts = count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM files WHERE parent LIKE '%/{}/%' AND content_state = 1",
                    PRUNE_PATTERN
                ),
            );
            let fts = count(&conn, "SELECT COUNT(*) FROM searchabletext");
            let done = count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 1");
            assert!(
                doomed > 0,
                "the pattern excludes nothing — seed_index's directory naming moved"
            );
            assert_eq!(fts, done, "a posting exists exactly for content_state = 1");
            assert!(
                doomed_fts * 2 < done,
                "the prune takes {} of {} postings — the content and directory \
                 strides have collided again; see `spec`'s content_every",
                doomed_fts,
                done
            );
            println!(
                "  {} rows, {} doomed ({:.0}%), of which {} hold a posting ({:.0}%) — \
                 the rest hold nothing to tombstone",
                total,
                doomed,
                100.0 * doomed as f64 / total as f64,
                doomed_fts,
                100.0 * doomed_fts as f64 / doomed as f64,
            );
            println!(
                "  plan: {}",
                conn.query_row(
                    "EXPLAIN QUERY PLAN DELETE FROM searchabletext WHERE rowid IN (1,2,3)",
                    [],
                    |r| r.get::<_, String>(3),
                )
                .unwrap_or_else(|e| format!("unavailable: {}", e))
            );
            common::print_fts_census(&conn);
        }

        let config = config_with(&[PRUNE_PATTERN]);
        let header = || {
            println!(
                "\n  {:<11} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8}",
                "stage", "page", "cover", "files", "fts", "commit", "total", "misses", "deleted"
            );
        };
        let row = |name: &str, t: &Timing| {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            println!(
                "  {:<11} {:>7.0} {:>7.0} {:>7.0} {:>7.0} {:>8.0} {:>8.0} {:>9} {:>8}",
                name,
                ms(t.page),
                ms(t.cover),
                ms(t.files),
                ms(t.fts),
                ms(t.commit),
                ms(t.total),
                t.misses,
                t.deleted,
            );
        };

        header();
        for stage in Stage::ALL {
            let arm = clone_arm(&master, &format!("prune-{}-{}", label, stage.tag()));
            let conn = open(&arm);
            let t = run_stage(&conn, &config, stage, Commit::PerPage);
            row(stage.label(), &t);
            if stage == Stage::Subtree {
                println!(
                    "  {:<11} {} of those rows never reached a page",
                    "", t.skipped
                );
            }
            // Consolidation, priced apart: merge until `%_data` stops changing,
            // as `fts_end_tombstone_burst` ends a burst.
            if stage.deletes_fts() {
                print_merge(&conn, 11, "");
            }
            drop(conn);
            arm.discard();
        }

        // Is the cost per commit or per row? The first line commits per page;
        // if the cost collapses as pages fold together, commit cadence is the
        // lever and none of the stages are.
        println!("\n  transaction size, at +fts(all):");
        header();
        for pages in [1usize, 8, 64, 512] {
            let arm = clone_arm(&master, &format!("prune-{}-tx{}", label, pages));
            let conn = open(&arm);
            let t = run_stage(&conn, &config, Stage::FtsAll, Commit::Pages(pages));
            row(&format!("{} page/tx", pages), &t);
            drop(conn);
            arm.discard();
        }

        // Every lever at once: commit per `advance` slice, skip doomed
        // directories wholesale, tombstone only what can hold a posting. The
        // slice is already `advance`'s cancellation point, so this needs no
        // longer uninterruptible window.
        println!("\n  combined, committing once per {:?} slice:", scope::SLICE);
        header();
        {
            let arm = clone_arm(&master, &format!("prune-{}-combined", label));
            let conn = open(&arm);
            let t = run_stage(&conn, &config, Stage::Subtree, Commit::Slice(scope::SLICE));
            row("combined", &t);
            print_merge(&conn, 11, "");
            drop(conn);
            arm.discard();
        }

        // The page cache. `PRAGMAS_INCREMENTAL` gives the reconcile a flat
        // 4 MiB for a pass across the whole index. A delete also maintains
        // `idx_files_hash`, a b-tree in an order uncorrelated with
        // `(parent, name)`, so every doomed row scatters a page touch that a
        // 512-page cache cannot hold (three, before `idx_files_mtime` and
        // `idx_files_mime` were dropped). Read the `misses` column. Both commit policies are swept,
        // because a cache that holds the working set may make commit cadence
        // stop mattering.
        println!("\n  page cache, at +fts(all)  [{} = shipped]:", SHIPPED_MIB);
        header();
        for mib in [SHIPPED_MIB, 16, 32, 64, 128, 256] {
            for (name, commit) in [
                ("per page", Commit::PerPage),
                ("per slice", Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("prune-{}-c{}-{}", label, mib, name.len()));
                let conn = open(&arm);
                set_cache(&conn, mib);
                let t = run_stage(&conn, &config, Stage::FtsAll, commit);
                row(&format!("{} MiB, {}", mib, name), &t);
                drop(conn);
                arm.discard();
            }
        }

        // Measured and rejected: at its 1000-page default, autocheckpoint has
        // the committing thread copy the log back into the main file every
        // 8 MiB, re-encrypting every page on a keyed index. Deferring it to
        // one checkpoint at the end is a wash, because the pages move either
        // way.
        {
            let arm = clone_arm(&master, &format!("prune-{}-nockpt", label));
            let conn = open(&arm);
            conn.execute_batch("PRAGMA wal_autocheckpoint = 0;")
                .expect("autocheckpoint");
            let t = run_stage(&conn, &config, Stage::FtsAll, Commit::Slice(scope::SLICE));
            let at = Instant::now();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
            let ckpt = at.elapsed();
            row(
                "no autockpt",
                &Timing {
                    commit: t.commit + ckpt,
                    total: t.total + ckpt,
                    ..t
                },
            );
            drop(conn);
            arm.discard();
        }

        // The excluded fraction, swept. Past some fraction FTS5 stops
        // tombstoning and starts rewriting segments, which costs what building
        // the index did. `baseline` is `+fts(all)` committing per page;
        // `fixed` is the combined row above.
        println!("\n  excluded fraction (baseline -> fixed, both + merge to quiescence):");
        println!(
            "  {:<11} {:>8} {:>9} {:>9} {:>9} {:>11} {:>11}",
            "patterns", "deleted", "baseline", "fixed", "merge", "%_data", "postings"
        );
        for n in 1..=PRUNE_PATTERNS.len() {
            let patterns = &PRUNE_PATTERNS[..n];
            let config = config_with(patterns);
            let mut timings = Vec::new();
            for (tag, stage, commit) in [
                ("baseline", Stage::FtsAll, Commit::PerPage),
                ("fixed", Stage::Subtree, Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("prune-{}-f{}-{}", label, n, tag));
                let conn = open(&arm);
                let before = common::fts_data_rows(&conn);
                let t = run_stage(&conn, &config, stage, commit);
                let (merge, _, _, after) = merge_to_quiescence(&conn, MERGE_PAGES);
                let left = count(&conn, "SELECT COUNT(*) FROM searchabletext");
                timings.push((t, merge, before, after, left));
                drop(conn);
                arm.discard();
            }
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            println!(
                "  {:<11} {:>8} {:>8.0} {:>8.0} {:>8.0} {:>11} {:>11}",
                format!("{} of 5", n),
                timings[0].0.deleted,
                ms(timings[0].0.total),
                ms(timings[1].0.total),
                ms(timings[1].1),
                format!("{}->{}", timings[1].2, timings[1].3),
                timings[1].4,
            );
        }

        // The real `scope::advance`, driven to completion as the coordinator
        // drives it. It lands under every stage; see the module doc.
        {
            let arm = clone_arm(&master, &format!("prune-{}-live", label));
            let mut conn = open(&arm);
            let old = config_with(&[]);
            let actions = quicksearch_core::config::diff_actions(&old, &config);
            let mut cursor = WorkCursor::new(actions.work, &config).expect("plan");
            let registry = Registry::default_set();
            let run = std::sync::atomic::AtomicBool::new(false);
            let started = Instant::now();
            while !cursor.done() {
                scope::advance(
                    &mut conn,
                    &config,
                    &registry,
                    &mut cursor,
                    Instant::now() + scope::SLICE,
                    &run,
                )
                .expect("advance");
            }
            println!(
                "  {:<11} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8.0} {:>10} {:>9}   <- scope::advance",
                "live",
                "",
                "",
                "",
                "",
                "",
                started.elapsed().as_secs_f64() * 1000.0,
                "",
                cursor.deleted,
            );
            drop(conn);
            arm.discard();
        }

        // Stale cleanup, the pass a completed run ends with: the same doomed
        // rows, deleted by path, because the walk hands
        // `cleanup_stale_index_entries` the paths it did not see, in directory
        // order. It runs on the run's own writer connection
        // (`open_run_writer`); `open` would measure the reconcile's 4 MiB
        // cache instead.
        {
            let stale: Vec<String> = {
                let conn = open(&master);
                let mut stmt = conn
                    .prepare(
                        "SELECT parent || name FROM files WHERE parent LIKE ?1 \
                         ORDER BY parent, name",
                    )
                    .expect("prepare");
                let rows = stmt
                    .query_map([format!("%/{}/%", PRUNE_PATTERN)], |r| r.get(0))
                    .expect("query stale paths");
                rows.collect::<Result<Vec<_>, _>>().expect("read stale paths")
            };
            println!("\n  stale cleanup, {} doomed paths:", stale.len());
            header();
            for shape in StaleShape::ALL {
                let arm = clone_arm(&master, &format!("prune-{}-stale-{}", label, shape.tag()));
                let conn = open_run_writer(&arm);
                let t = run_stale(&conn, &stale, shape);
                row(shape.label(), &t);
                print_merge(&conn, 11, "");
                drop(conn);
                arm.discard();
            }

            // The shipped function, whole: the row that moves when
            // `file_handling::batch` changes.
            let arm = clone_arm(&master, &format!("prune-{}-stale-live", label));
            let conn = open_run_writer(&arm);
            let (_, misses_before) = testutil::cache_stats(&conn);
            let conn_mutex = std::sync::Arc::new(std::sync::Mutex::new(conn));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let started = Instant::now();
            let deleted = cleanup_stale_index_entries(&conn_mutex, &stale, &stop, &config)
                .expect("cleanup stale");
            let total = started.elapsed();
            let conn = std::sync::Arc::try_unwrap(conn_mutex)
                .map_err(|_| ())
                .expect("sole owner")
                .into_inner()
                .expect("unpoisoned");
            let (_, misses_after) = testutil::cache_stats(&conn);
            row(
                "live",
                &Timing {
                    total,
                    deleted,
                    misses: misses_after - misses_before,
                    ..Timing::default()
                },
            );
            print_merge(&conn, 11, "   <- cleanup_stale_index_entries");
            drop(conn);
            arm.discard();
        }

        master.discard();
    }
}
