//! Where the time goes when a narrowed content filter scrubs the index.
//!
//! Narrowing `content_extensions` sets `IndexWork { reconcile_content: true }`
//! (`config::diff_actions`). `scope::advance` then reads every stored row
//! under every root, re-decides each one, and for each row whose verdict
//! changed clears its content and flips `files.content_state`. This probe
//! attributes that time to a phase and prices the shapes that could replace it.
//!
//! ```text
//! cargo build -p quicksearch-core --example contentprobe --release
//! ./target/release/examples/contentprobe
//! ```
//!
//! `QSB_FILES`, `QSB_DIRS`, `QSB_DIR_DEPTH` and `QSB_PENDING_EVERY` size the
//! corpus; `QSB_ARMS=plain|keyed|both` picks the key states (see
//! `common::arms`).
//!
//! # The sibling probe
//!
//! `pruneprobe` measures the other half of the same function: an added ignore
//! pattern, which deletes rows outright. The two differ in how much of what
//! they touch holds a posting. A narrowed content filter targets rows that do
//! (95% of the rewritten rows on this corpus, against 14% of the doomed rows
//! on pruneprobe's), so this path is the slower one and gains the most from
//! the `deletemerge` lever below.
//!
//! The stages are cumulative, so consecutive rows subtract to a phase cost:
//!
//! ```text
//! page                 read the rows, decide nothing
//! +decide              ...and re-run content_extractable per row
//! +state               ...and flip content_state, per row
//! +clear(all)          ...and clear FTS/text/failure per row
//! +clear(done)         ...clearing only rows that can hold one
//! +chunked             ...issuing the clears as chunked IN (...) lists
//! +chunk state         ...and the state flip chunked too
//! ```
//!
//! # What it found
//!
//! The FTS5 tombstone dominates the clears: 786 ms of a 975 ms pass, against
//! 13 ms for `documents_text` and 1 ms for `failed_files`. The table splits
//! the three because one `clear` column would suggest fixing the wrong thing.
//!
//! Statement shape barely matters, and is easy to misread. Chunking the clears
//! takes `fts` from 786 ms to 19 ms and puts 848 ms into `commit`, for 6% off
//! `total`: SQLite spills the work to whichever statement is executing when
//! the page cache fills, so compare only `total` across shapes. `+chunk state`
//! demonstrates it, moving the same ~850 ms onto an `UPDATE` that the query
//! plans show is a plain rowid seek.
//!
//! `deletemerge` is the lever, worth 6-7x; read the knob sweep.
//! `file_handling::fts_begin_tombstone_burst` owns the result and ships it,
//! so `live` lands far under every stage.

// Timed on the allocator the shipped binaries install; see
// `platform::Allocator`.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use quicksearch_core::config::Config;
use quicksearch_core::db::repo;
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{content_extractable, RootRange, FTS_DELETEMERGE};
use quicksearch_core::scope::{self, WorkCursor};
use quicksearch_core::testutil::{self, Arm, SeedSpec};

use common::{
    chunked, clone_arm, count, env_usize, merge_to_quiescence, open, placeholders, print_merge,
    set_cache, Commit, Io, CHUNK, MERGE_PAGES, PAGE, SHIPPED_MIB,
};

/// The root every `seed_index` row hangs under. It need not exist on disk:
/// the content re-decision stats no stored path, reading the extension off
/// the string and the MIME off the row.
const ROOT: &str = "/seed";

/// The corpus's extensions and the MIME each carries.
///
/// Eleven, coprime with `dirs` (2000 by default). `seed_index` assigns
/// extensions by `i % len` and directories by `i % dirs`, so a shared factor
/// would confine each extension to some of the directories: the rewritten rows
/// would arrive in a few dense runs, the easy case for page locality, and
/// flatter every figure. `main` asserts it.
const EXT_MIX: &[(&str, &str)] = &[
    ("txt", "text/plain"),
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("md", "text/markdown"),
    ("zip", "application/zip"),
    ("mp4", "video/mp4"),
    ("pdf", "application/pdf"),
    ("gif", "image/gif"),
    ("bin", "application/octet-stream"),
    ("mov", "video/quicktime"),
    ("ttf", "font/ttf"),
];

/// The [`EXT_MIX`] entries an extractor claims, and so the rows seeded with a
/// posting: 3 of 11.
///
/// [`NARROWED`] and the fraction sweep are written in terms of these three, so
/// `main` checks them against `Registry::default_set()`: an extractor gaining
/// or losing a MIME fails there instead of reshaping every figure.
const EXTRACTABLE: [&str; 3] = ["txt", "md", "pdf"];

/// The filter the stage table narrows to, from an unfiltered index. Keeping
/// `txt` and dropping `md` and `pdf` rewrites 2/11 of all rows and tombstones
/// 2/3 of the postings; the fraction sweep covers a filter that takes every
/// posting.
const NARROWED: &[&str] = &["txt"];

/// The `files.name` test for the rows [`NARROWED`] withdraws content from,
/// for the census.
const NARROWED_AWAY: &str = "(name LIKE '%.md' OR name LIKE '%.pdf')";

/// Greatest common divisor, for the stride assertion in `main`.
fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

// ---------------------------------------------------------------------------
// Stages
// ---------------------------------------------------------------------------

/// Cumulative slices of the re-decision. Each does everything the one before
/// it does.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Page,
    Decide,
    State,
    ClearAll,
    ClearDone,
    Chunked,
    ChunkedState,
}

impl Stage {
    const ALL: [Stage; 7] = [
        Stage::Page,
        Stage::Decide,
        Stage::State,
        Stage::ClearAll,
        Stage::ClearDone,
        Stage::Chunked,
        Stage::ChunkedState,
    ];

    fn label(self) -> &'static str {
        match self {
            Stage::Page => "page",
            Stage::Decide => "+decide",
            Stage::State => "+state",
            Stage::ClearAll => "+clear(all)",
            Stage::ClearDone => "+clear(done)",
            Stage::Chunked => "+chunked",
            Stage::ChunkedState => "+chunk state",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Stage::Page => "page",
            Stage::Decide => "decide",
            Stage::State => "state",
            Stage::ClearAll => "clear-all",
            Stage::ClearDone => "clear-done",
            Stage::Chunked => "chunked",
            Stage::ChunkedState => "chunked-state",
        }
    }

    /// Whether the re-decision runs at all.
    fn decides(self) -> bool {
        self >= Stage::Decide
    }

    /// Whether `files.content_state` is written.
    fn writes_state(self) -> bool {
        self >= Stage::State
    }

    /// Whether the row's content is cleared alongside the state flip.
    fn clears(self) -> bool {
        self >= Stage::ClearAll
    }

    /// Whether the clear is narrowed to rows whose stored state says they can
    /// hold something to clear.
    fn clear_filtered(self) -> bool {
        self >= Stage::ClearDone
    }

    /// Whether the clears go out as chunked `IN (...)` lists instead of one
    /// bound statement per row.
    fn chunked_clears(self) -> bool {
        self >= Stage::Chunked
    }

    /// Whether the `content_state` flip is chunked too. A separate step
    /// because it does not behave like the clears; see the query plans `main`
    /// prints.
    fn chunked_state(self) -> bool {
        self >= Stage::ChunkedState
    }
}

/// What one stage cost, split by phase.
#[derive(Default)]
struct Timing {
    page: Duration,
    decide: Duration,
    state: Duration,
    /// The three clears, timed apart: one phase, three different pieces of
    /// work. An FTS5 contentless delete writes a tombstone into every segment
    /// covering the row's origin, a `documents_text` delete frees a compressed
    /// body's overflow pages, and a `failed_files` delete seeks into a table
    /// that is nearly always empty.
    fts: Duration,
    text: Duration,
    failed: Duration,
    commit: Duration,
    total: Duration,
    examined: usize,
    /// Rows whose content was withdrawn.
    to_na: usize,
    /// Rows put back in the pending queue.
    to_pending: usize,
    /// Bytes the block layer served this pass. The arm was just copied, so
    /// near zero means the working set was in memory and every duration is
    /// CPU and page-cache work.
    read_bytes: u64,
    misses: i64,
}

// ---------------------------------------------------------------------------
// The measured loop
// ---------------------------------------------------------------------------

/// The ids one page decided to write, split by what each one needs.
#[derive(Default)]
struct Pending {
    /// Rows moving to `STATE_NA`.
    to_na: Vec<i64>,
    /// Rows moving to `STATE_PENDING`.
    to_pending: Vec<i64>,
    /// Of the two above, those whose *stored* state was `STATE_DONE` and so
    /// can hold a posting and a stored body.
    had_posting: Vec<i64>,
    /// ...and those whose stored state was `STATE_FAILED`, the only ones that
    /// can hold a `failed_files` row.
    had_failure: Vec<i64>,
    /// Every id above, in the order the per-row loop visits them.
    all: Vec<i64>,
}

impl Pending {
    fn clear(&mut self) {
        self.to_na.clear();
        self.to_pending.clear();
        self.had_posting.clear();
        self.had_failure.clear();
        self.all.clear();
    }

    fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// Sort every list ascending, which for these ids is rowid order; see
    /// [`Knobs::sort_ids`].
    fn sort(&mut self) {
        for list in [
            &mut self.to_na,
            &mut self.to_pending,
            &mut self.had_posting,
            &mut self.had_failure,
            &mut self.all,
        ] {
            list.sort_unstable();
        }
    }
}

/// Levers over FTS5's tombstone machinery, applied for the length of the pass.
/// Neither changes what the index ends up holding: the final `'merge'`
/// reclaims the tombstones either way.
#[derive(Clone, Copy, Default)]
struct Knobs {
    /// Set FTS5's `deletemerge` to 0 for the pass and restore it after. At its
    /// default of 10, each level that passes 10% tombstones is rewritten
    /// inline with the scan; at 0 they accumulate for the final merge. See
    /// `file_handling::fts_begin_tombstone_burst`.
    no_deletemerge: bool,
    /// Sort each page's ids ascending before deleting.
    ///
    /// The scan serves rows in `(parent, name)` order, uncorrelated with
    /// rowid, so each `%_docsize` lookup in `fts5StorageContentlessDelete`
    /// lands somewhere new. Sorting restores locality within a page, not
    /// across pages. Measured and rejected; see
    /// `file_handling::fts_begin_tombstone_burst`.
    sort_ids: bool,
}

impl Knobs {
    fn label(self) -> String {
        match (self.no_deletemerge, self.sort_ids) {
            (false, false) => "neither".into(),
            (true, false) => "deletemerge off".into(),
            (false, true) => "rowid-sorted".into(),
            (true, true) => "both".into(),
        }
    }

    fn apply(self, conn: &Connection) {
        if self.no_deletemerge {
            set_deletemerge(conn, 0);
        }
    }

    /// Undo [`Knobs::apply`]. FTS5 persists `deletemerge` in `%_config`, so a
    /// leaked 0 would change what every later reading of the file means,
    /// including the merge that follows.
    fn restore(self, conn: &Connection) {
        if self.no_deletemerge {
            set_deletemerge(conn, i64::from(FTS_DELETEMERGE));
        }
    }
}

fn set_deletemerge(conn: &Connection, percent: i64) {
    conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('deletemerge', ?1)",
        [percent],
    )
    .expect("set deletemerge");
}

/// Page the index as `scope::advance` does, doing `stage`'s share of the work
/// and timing each phase apart.
///
/// The writes are spelled out here rather than routed through `repo`, so the
/// per-row and chunked variants compare in one run and the stages stay fixed
/// when `repo` changes.
fn run_stage(
    conn: &Connection,
    config: &Config,
    registry: &Registry,
    stage: Stage,
    commit: Commit,
) -> Timing {
    run_stage_with(conn, config, registry, stage, commit, Knobs::default())
}

fn run_stage_with(
    conn: &Connection,
    config: &Config,
    registry: &Registry,
    stage: Stage,
    commit: Commit,
    knobs: Knobs,
) -> Timing {
    let range = RootRange::for_root(ROOT);
    let max_size = config.processing.maximum_text_file_size;

    let mut t = Timing::default();
    knobs.apply(conn);
    let io_before = Io::read();
    let (_, misses_before) = testutil::cache_stats(conn);
    let started = Instant::now();

    let mut after = (range.lo.clone(), String::new());
    let mut pending = Pending::default();
    // `unchecked_transaction` borrows shared, so one transaction can end and
    // the next begin inside the loop, as in `testutil::seed_index`.
    let mut tx = conn.unchecked_transaction().expect("begin");
    let mut pages_open = 0usize;
    let mut tx_since = Instant::now();
    loop {
        let at = Instant::now();
        let rows = repo::rows_in_range_page(&tx, &after.0, &after.1, &range.hi, PAGE)
            .expect("read a page");
        t.page += at.elapsed();
        let Some(last) = rows.last() else { break };
        after = (last.parent.clone(), last.name.clone());
        pages_open += 1;
        t.examined += rows.len();

        if !stage.decides() {
            continue;
        }

        // `scope::apply_page`'s decision.
        let at = Instant::now();
        pending.clear();
        for row in &rows {
            let path = Path::new(&row.path);
            let wants = row.size <= max_size
                && content_extractable(path, row.mime.as_deref(), config, registry);
            if !wants && row.content_state != repo::STATE_NA {
                pending.to_na.push(row.id);
            } else if wants && row.content_state == repo::STATE_NA {
                pending.to_pending.push(row.id);
            } else {
                continue;
            }
            pending.all.push(row.id);
            if row.content_state == repo::STATE_DONE {
                pending.had_posting.push(row.id);
            } else if row.content_state == repo::STATE_FAILED {
                pending.had_failure.push(row.id);
            }
        }
        if knobs.sort_ids {
            pending.sort();
        }
        t.decide += at.elapsed();
        t.to_na += pending.to_na.len();
        t.to_pending += pending.to_pending.len();

        if stage.writes_state() && !pending.is_empty() {
            write_page(&tx, stage, &pending, &mut t);
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
    t.read_bytes = Io::read().since(&io_before).read_bytes;
    let (_, misses_after) = testutil::cache_stats(conn);
    t.misses = misses_after - misses_before;
    knobs.restore(conn);
    t
}

/// Apply one page's decisions, in `stage`'s shape, timing the clear and the
/// state flip apart.
fn write_page(tx: &Connection, stage: Stage, pending: &Pending, t: &mut Timing) {
    // The ids the clear is offered: those whose stored state can hold
    // something, or every flipped row.
    let (fts_ids, failed_ids): (&[i64], &[i64]) = if stage.clear_filtered() {
        (&pending.had_posting, &pending.had_failure)
    } else {
        (&pending.all, &pending.all)
    };

    // The per-row clears are `repo::remove_content_for_id` followed by a
    // `failed_files` delete, one bound statement at a time. They prepare through the connection's cache, as
    // `repo::exec` does; preparing afresh per row would price a mistake the
    // product does not make.
    if stage.clears() {
        // Each target runs to completion, timed on its own, before the next
        // starts: interleaving them as `remove_content_for_id` does would
        // charge whichever statement was executing when the page cache
        // spilled.
        for (in_list, per_row, ids, spent) in [
            (
                "DELETE FROM searchabletext WHERE rowid IN",
                "DELETE FROM searchabletext WHERE rowid = ?1",
                fts_ids,
                &mut t.fts,
            ),
            (
                "DELETE FROM documents_text WHERE file_id IN",
                "DELETE FROM documents_text WHERE file_id = ?1",
                fts_ids,
                &mut t.text,
            ),
            (
                "DELETE FROM failed_files WHERE file_id IN",
                "DELETE FROM failed_files WHERE file_id = ?1",
                failed_ids,
                &mut t.failed,
            ),
        ] {
            let at = Instant::now();
            if stage.chunked_clears() {
                chunked(tx, in_list, ids, true);
            } else {
                for id in ids {
                    one(tx, per_row, *id);
                }
            }
            *spent += at.elapsed();
        }
    }

    let at = Instant::now();
    for (state, ids) in [
        (repo::STATE_NA, &pending.to_na),
        (repo::STATE_PENDING, &pending.to_pending),
    ] {
        if stage.chunked_state() {
            chunked_state(tx, state, ids);
        } else {
            for id in ids {
                tx.prepare_cached("UPDATE files SET content_state = ?1 WHERE id = ?2")
                    .and_then(|mut s| s.execute(rusqlite::params![state, id]))
                    .expect("flip content_state");
            }
        }
    }
    t.state += at.elapsed();
}

fn one(tx: &Connection, sql: &str, id: i64) {
    tx.prepare_cached(sql)
        .and_then(|mut s| s.execute([id]))
        .expect("per-row write");
}

fn chunked_state(tx: &Connection, state: i64, ids: &[i64]) {
    for chunk in ids.chunks(CHUNK) {
        let sql = format!(
            "UPDATE files SET content_state = ?1 WHERE id IN ({})",
            placeholders(chunk.len())
        );
        let params = std::iter::once(&state).chain(chunk.iter());
        tx.prepare_cached(&sql)
            .and_then(|mut s| s.execute(rusqlite::params_from_iter(params)))
            .expect("chunked state flip");
    }
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

fn spec() -> SeedSpec {
    SeedSpec {
        files: env_usize("QSB_FILES", 200_000),
        dirs: env_usize("QSB_DIRS", 2_000),
        // Must be 1. With a mix, the row's MIME decides extractability and
        // `seed_index` intersects that with this stride, so any other value
        // leaves rows an extractor claims at `STATE_NA`: the first reconcile
        // would repair the seed instead of applying the edit.
        content_every: 1,
        // One in twenty extractable rows never got its content, the residue
        // of an interrupted run. The unfiltered clears still delete FTS,
        // stored text and a failure record for these rows, though none can
        // exist; `+clear(done)` prices that.
        pending_every: env_usize("QSB_PENDING_EVERY", 20),
        ext_mix: EXT_MIX,
        // Row width, and so pages scanned; see `SeedSpec::dir_depth`.
        dir_depth: env_usize("QSB_DIR_DEPTH", 6).max(2),
        // A real run commits in slices, each flushing FTS5's hash to its own
        // segment; one transaction would leave a single segment and
        // understate every clear cost.
        commit_every: 500,
        ..SeedSpec::default()
    }
}

fn config_with(extensions: &[&str]) -> Config {
    let mut config = Config::default();
    config.paths.indexing_paths = vec![ROOT.to_string()];
    config.indexing.content_extensions = extensions.iter().map(|e| e.to_string()).collect();
    config.processing.batch_size = PAGE as usize;
    config
}

// ---------------------------------------------------------------------------

fn main() {
    let spec = spec();
    let registry = Registry::default_set();

    // Check the corpus's premise.
    let claimed: Vec<&str> = EXT_MIX
        .iter()
        .filter(|(_, mime)| registry.supports(mime))
        .map(|(ext, _)| *ext)
        .collect();
    assert_eq!(
        claimed, EXTRACTABLE,
        "the registry no longer claims exactly the extensions this probe is \
         written around; NARROWED and the fraction sweep name them by hand"
    );
    assert_eq!(
        gcd(EXT_MIX.len(), spec.dirs),
        1,
        "the extension stride ({}) shares a factor with the directory stride \
         ({}), so each extension reaches only some of the directories — the \
         rewritten rows would cluster instead of spreading. See EXT_MIX",
        EXT_MIX.len(),
        spec.dirs
    );

    println!(
        "corpus: {} files across {} dirs, {} of {} extensions extractable ({}), \
         1 in {} of those left pending; narrowing to {:?}",
        spec.files,
        spec.dirs,
        EXTRACTABLE.len(),
        EXT_MIX.len(),
        EXTRACTABLE.join("/"),
        spec.pending_every,
        NARROWED
    );

    for keyed in common::arms() {
        let label = if keyed { "keyed" } else { "plain" };
        let master = Arm::seed(label, &format!("content-{}", label), keyed, &spec);
        println!(
            "\n=== {} ===  seeded in {:.1}s, {}",
            label,
            master.seeded_in.as_secs_f64(),
            common::mib(master.size_bytes())
        );

        // Census: what the narrowing touches, read off the index.
        {
            let conn = open(&master);
            let total = count(&conn, "SELECT COUNT(*) FROM files");
            let done = count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 1");
            let pending = count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 0");
            let na = count(&conn, "SELECT COUNT(*) FROM files WHERE content_state = 3");
            let fts = count(&conn, "SELECT COUNT(*) FROM searchabletext");
            assert_eq!(fts, done, "a posting exists exactly for content_state = 1");
            assert!(
                pending > 0,
                "no pending residue — QSB_PENDING_EVERY is 0, and +clear(done) \
                 has nothing to skip"
            );
            // The rows the narrowing rewrites: everything not already NA whose
            // extension leaves the filter.
            let rewritten = count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM files WHERE content_state != 3 AND {}",
                    NARROWED_AWAY
                ),
            );
            let with_posting = count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM files WHERE content_state = 1 AND {}",
                    NARROWED_AWAY
                ),
            );
            assert!(
                rewritten > 0,
                "the narrowing rewrites nothing — seed_index's naming moved"
            );
            assert!(
                with_posting < rewritten,
                "every rewritten row holds a posting, so +clear(done) has \
                 nothing to skip and the stage cannot answer anything"
            );
            println!(
                "  {} rows: {} done, {} pending, {} n/a. {} rewritten by the \
                 narrowing ({:.0}%), of which {} hold a posting ({:.0}%) — \
                 the other {} hold nothing to clear",
                total,
                done,
                pending,
                na,
                rewritten,
                100.0 * rewritten as f64 / total as f64,
                with_posting,
                100.0 * with_posting as f64 / rewritten as f64,
                rewritten - with_posting,
            );
            // The query plans, which a timing cannot show: whether each shape
            // seeks or scans.
            for (what, sql) in [
                (
                    "delete, per row  ",
                    "DELETE FROM searchabletext WHERE rowid = 1".to_string(),
                ),
                (
                    "delete, chunked  ",
                    "DELETE FROM searchabletext WHERE rowid IN (1,2,3)".to_string(),
                ),
                (
                    "state,  per row  ",
                    "UPDATE files SET content_state = 3 WHERE id = 1".to_string(),
                ),
                (
                    "state,  chunked  ",
                    "UPDATE files SET content_state = 3 WHERE id IN (1,2,3)".to_string(),
                ),
            ] {
                let plan: Vec<String> = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {}", sql))
                    .and_then(|mut s| {
                        s.query_map([], |r| r.get::<_, String>(3))?
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap_or_else(|e| vec![format!("unavailable: {}", e)]);
                println!("  plan {}: {}", what, plan.join(" | "));
            }
            common::print_fts_census(&conn);
        }

        let config = config_with(NARROWED);
        let header = || {
            println!(
                "\n  {:<16} {:>6} {:>6} {:>6} {:>7} {:>6} {:>6} {:>7} {:>7} {:>8} {:>7}",
                "stage",
                "page",
                "decide",
                "state",
                "fts",
                "text",
                "failed",
                "commit",
                "total",
                "misses",
                "written"
            );
        };
        let row = |name: &str, t: &Timing| {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            println!(
                "  {:<16} {:>6.0} {:>6.0} {:>6.0} {:>7.0} {:>6.0} {:>6.0} {:>7.0} {:>7.0} {:>8} {:>7}",
                name,
                ms(t.page),
                ms(t.decide),
                ms(t.state),
                ms(t.fts),
                ms(t.text),
                ms(t.failed),
                ms(t.commit),
                ms(t.total),
                t.misses,
                t.to_na + t.to_pending,
            );
        };

        header();
        for stage in Stage::ALL {
            let arm = clone_arm(&master, &format!("content-{}-{}", label, stage.tag()));
            let conn = open(&arm);
            let t = run_stage(&conn, &config, &registry, stage, Commit::PerPage);
            row(stage.label(), &t);
            // Consolidation, priced apart: merge until `%_data` stops changing,
            // as `fts_end_tombstone_burst` ends a burst.
            if stage.clears() {
                print_merge(&conn, 16, "");
            }
            drop(conn);
            arm.discard();
        }

        // Is the cost per commit or per row? The first line of each block
        // commits per page; if the cost collapses as pages fold together,
        // commit cadence is the lever, not the write shape. Both shapes are
        // swept because the two fixes are independent and either could
        // subsume the other.
        for stage in [Stage::ClearAll, Stage::Chunked] {
            println!("\n  transaction size, at {}:", stage.label());
            header();
            for pages in [1usize, 8, 64, 512] {
                let arm = clone_arm(
                    &master,
                    &format!("content-{}-{}-tx{}", label, stage.tag(), pages),
                );
                let conn = open(&arm);
                let t = run_stage(&conn, &config, &registry, stage, Commit::Pages(pages));
                row(&format!("{} page/tx", pages), &t);
                drop(conn);
                arm.discard();
            }
            let arm = clone_arm(&master, &format!("content-{}-{}-slice", label, stage.tag()));
            let conn = open(&arm);
            let t = run_stage(
                &conn,
                &config,
                &registry,
                stage,
                Commit::Slice(scope::SLICE),
            );
            row(&format!("{:?} slice", scope::SLICE), &t);
            drop(conn);
            arm.discard();
        }

        // FTS5's own machinery, where the time is once statement shape stops
        // mattering. Run at the chunked shape, so what remains is FTS5's cost
        // and not per-row overhead.
        println!("\n  FTS5 knobs, at +chunked (+ merge to quiescence):");
        header();
        for knobs in [
            Knobs::default(),
            Knobs {
                no_deletemerge: true,
                ..Knobs::default()
            },
            Knobs {
                sort_ids: true,
                ..Knobs::default()
            },
            Knobs {
                no_deletemerge: true,
                sort_ids: true,
            },
        ] {
            let arm = clone_arm(
                &master,
                &format!("content-{}-k{}", label, knobs.label().len()),
            );
            let conn = open(&arm);
            let t = run_stage_with(
                &conn,
                &config,
                &registry,
                Stage::Chunked,
                Commit::Slice(scope::SLICE),
                knobs,
            );
            row(&knobs.label(), &t);
            print_merge(&conn, 16, "");
            drop(conn);
            arm.discard();
        }

        // The page cache. `PRAGMAS_INCREMENTAL` gives the reconcile a flat
        // 4 MiB for a pass across the whole index. The scan reads
        // `idx_files_parent` in order but fetches a table row per entry, and
        // the writes scatter into `%_docsize`, `documents_text` and FTS5's
        // tombstone pages in orders uncorrelated with `(parent, name)`. Read
        // the `misses` column.
        println!(
            "\n  page cache, at +clear(all)  [{} = shipped]:",
            SHIPPED_MIB
        );
        header();
        for mib in [SHIPPED_MIB, 16, 32, 64, 128, 256] {
            for (name, commit) in [
                ("per page", Commit::PerPage),
                ("per slice", Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(
                    &master,
                    &format!("content-{}-c{}-{}", label, mib, name.len()),
                );
                let conn = open(&arm);
                set_cache(&conn, mib);
                let t = run_stage(&conn, &config, &registry, Stage::ClearAll, commit);
                row(&format!("{} MiB, {}", mib, name), &t);
                drop(conn);
                arm.discard();
            }
        }

        // The withdrawn fraction, swept. Past some fraction FTS5 stops
        // tombstoning and starts rewriting segments, which costs what building
        // the index did. `baseline` is `+clear(all)` committing per page;
        // `fixed` is `+chunked` committing per slice.
        println!("\n  withdrawn fraction (baseline -> fixed, both + merge to quiescence):");
        println!(
            "  {:<13} {:>8} {:>9} {:>9} {:>9} {:>11} {:>11}",
            "keeping", "written", "baseline", "fixed", "merge", "%_data", "postings"
        );
        for keep in [&["txt", "md"][..], &["txt"][..], &["rtf"][..]] {
            let config = config_with(keep);
            let tag = keep.join("+");
            let mut timings = Vec::new();
            for (what, stage, commit) in [
                ("baseline", Stage::ClearAll, Commit::PerPage),
                ("fixed", Stage::Chunked, Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("content-{}-{}-{}", label, tag, what));
                let conn = open(&arm);
                let before = common::fts_data_rows(&conn);
                let t = run_stage(&conn, &config, &registry, stage, commit);
                let (merge, _, _, after) = merge_to_quiescence(&conn, MERGE_PAGES);
                let left = count(&conn, "SELECT COUNT(*) FROM searchabletext");
                timings.push((t, merge, before, after, left));
                drop(conn);
                arm.discard();
            }
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            println!(
                "  {:<13} {:>8} {:>8.0} {:>8.0} {:>8.0} {:>11} {:>11}",
                tag,
                timings[0].0.to_na + timings[0].0.to_pending,
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
            let arm = clone_arm(&master, &format!("content-{}-live", label));
            let mut conn = open(&arm);
            let unfiltered = config_with(&[]);
            let actions = quicksearch_core::config::diff_actions(&unfiltered, &config);
            assert!(
                actions.work.reconcile_content && !actions.work.reindex,
                "the narrowing should reconcile content and ask for no walk, got {:?}",
                actions.work
            );
            let mut cursor = WorkCursor::new(actions.work, &config).expect("plan");
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
                "  {:<16} {:>6} {:>6} {:>6} {:>7} {:>6} {:>6} {:>7} {:>7.0} {:>8} {:>7}   <- scope::advance",
                "live",
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                started.elapsed().as_secs_f64() * 1000.0,
                "",
                cursor.recontented,
            );
            drop(conn);
            arm.discard();
        }

        master.discard();
    }
}
