//! Where the time goes when a narrowed content filter scrubs the index.
//!
//! Narrowing `content_extensions` sets `IndexWork { reconcile_content: true }`
//! (`config::diff_actions`), and `scope::advance` then reads **every** stored
//! row under every root, re-decides each one, and for each row whose verdict
//! changed clears its content and flips `files.content_state`. On a large index
//! that has been reported taking excessively long. This attributes the time to
//! a phase and prices the shapes that could replace it.
//!
//! ```text
//! cargo build -p quicksearch-core --example contentprobe --release
//! ./target/release/examples/contentprobe
//! ```
//!
//! `QSB_FILES` / `QSB_DIRS` / `QSB_PENDING_EVERY` size the corpus;
//! `QSB_ARMS=plain|keyed|both` picks the key states. A keyed arm is not
//! optional dressing — every page a clear touches is decrypted and
//! re-encrypted, and the two arms have disagreed before (see
//! `db::schema::PAGE_SIZE`).
//!
//! # The sibling probe
//!
//! `pruneprobe` measures the *other* half of the same function: an added ignore
//! pattern, which deletes rows outright. Read them together. The paths differ in
//! how much of what they touch holds a posting — a narrowed content filter is
//! aimed squarely at rows that do (95% of the rewritten rows on this corpus,
//! against 14% of the doomed rows on that one), which is why this is the
//! pathological one and why it moved so much further.
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
//! **The FTS5 tombstone is the whole cost** — 786 ms of a 975 ms pass, against
//! 13 ms for `documents_text` and 1 ms for `failed_files`. Which is why the
//! table splits the three: as one `clear` column they average into a number
//! that suggests fixing the wrong thing.
//!
//! **Statement shape barely matters, and reading it wrong is easy.** Chunking
//! the clears takes `fts` from 786 ms to 19 ms — and puts 848 ms into `commit`,
//! for a 6% total. The work did not go away; SQLite spilled it to whichever
//! statement was executing when the page cache filled. Only `total` is safe to
//! compare across shapes. `+chunk state` is kept as the demonstration: it moves
//! the same ~850 ms onto an `UPDATE` that the query plans above show is a plain
//! rowid seek.
//!
//! **`deletemerge` is the lever**, and it is worth 6-7x. The knob sweep is the
//! table to read; `file_handling::fts_begin_tombstone_burst` carries the
//! result and the reasoning, and now ships it — so `live` lands far under every
//! stage rather than on `+clear(all)`.
//!
//! Every stage runs against a byte-identical copy of one seeded index rather
//! than a fresh seed: FTS5 segment layout is most of what decides clear cost,
//! and reseeding would let it drift between the rows of the table.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use quicksearch_core::config::Config;
use quicksearch_core::db::{self, repo};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{content_extractable, ExtractCursor};
use quicksearch_core::scope::{self, WorkCursor};
use quicksearch_core::testutil::{self, Arm, SeedSpec};

use common::Io;

/// The root every `seed_index` row hangs under; it need not exist on disk,
/// because nothing in the content re-decision stats a stored path —
/// `content_extractable` reads the extension off the string and the MIME off
/// the row.
const ROOT: &str = "/seed";

/// The corpus's extensions and the MIME each carries.
///
/// **Eleven entries, and the count is deliberate.** `seed_index` assigns
/// extensions by `i % len` and directories by `i % dirs`, so a length sharing a
/// factor with `dirs` (2000 by default) would confine each extension to a
/// fraction of the directories — the rewritten rows would arrive in a few dense
/// runs instead of spread through the keyspace, which is the easy case for page
/// locality and would flatter every figure. 11 is coprime with 2000, so every
/// directory holds every extension. `main` asserts it.
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

/// Which of [`EXT_MIX`] an extractor claims, and therefore which rows are
/// seeded with a posting: 3 of 11, so 3/11 of the corpus is searchable by
/// content.
///
/// `main` checks this against `Registry::default_set()` rather than trusting
/// it. Everything below hard-codes these three — [`NARROWED`] and the fraction
/// sweep are written in terms of them — so an extractor gaining or losing a
/// MIME must fail loudly here rather than quietly reshape every figure.
const EXTRACTABLE: [&str; 3] = ["txt", "md", "pdf"];

/// The filter the stage table narrows *to*, from an unfiltered index. It keeps
/// `txt` and drops `md` and `pdf`, so two of the three extractable extensions
/// lose their content: 2/11 of all rows are rewritten and 2/3 of the postings
/// are tombstoned. Substantial without being total — a filter that takes every
/// posting is a different problem, and the fraction sweep at the end covers it.
const NARROWED: &[&str] = &["txt"];

/// The `files.name` test matching the rows [`NARROWED`] withdraws content from,
/// for the census that reports what the stage table is about to do.
const NARROWED_AWAY: &str = "(name LIKE '%.md' OR name LIKE '%.pdf')";

/// Greatest common divisor, for the stride assertion in `main`.
fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Rows per page, matching `processing.batch_size`'s default — the figure
/// `scope::advance` runs at.
const PAGE: i64 = 500;

/// Ids per `IN (...)` list, matching `repo::DELETE_IDS_CHUNK`.
const CHUNK: usize = 512;

/// Output leaf pages per `'merge'` call, matching `FINALIZE_MERGE_PAGES`.
const MERGE_PAGES: i64 = 1000;

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

    /// Whether the `content_state` flip is chunked too. Kept as its own step
    /// because it does not behave like the clears — see the plan `main` prints.
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
    /// The three clears, apart. They are one phase conceptually and three very
    /// different pieces of work: an FTS5 contentless delete writes a tombstone
    /// into every segment covering the row's origin, a `documents_text` delete
    /// frees a compressed body's overflow pages, and a `failed_files` delete is
    /// a seek into a table that is nearly always empty.
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
    /// Bytes the block layer served this pass; the arm was just copied, so a
    /// figure near zero means the whole working set was already in memory and
    /// every duration below is CPU and page-cache work.
    read_bytes: u64,
    misses: i64,
}

// ---------------------------------------------------------------------------
// The measured loop
// ---------------------------------------------------------------------------

fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// When the pass commits.
#[derive(Clone, Copy)]
enum Commit {
    /// One transaction per page — what ships.
    PerPage,
    /// One per `n` pages; the sweep's variable.
    Pages(usize),
    /// One per elapsed slice, which is what `scope::advance` already uses as
    /// its unit of interruptible work.
    Slice(Duration),
}

impl Commit {
    fn due(self, pages_open: usize, since: Instant) -> bool {
        match self {
            Commit::PerPage => true,
            Commit::Pages(n) => pages_open >= n,
            Commit::Slice(d) => since.elapsed() >= d,
        }
    }
}

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
    /// Every id above, in the order the shipped per-row loop visits them.
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

    /// Ascending, which for these ids is rowid order — see [`Knobs::sort_ids`].
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
///
/// Neither changes what the index ends up holding — a tombstone is a tombstone
/// and the final `'merge'` reclaims them either way — so both are free to try.
#[derive(Clone, Copy, Default)]
struct Knobs {
    /// Set FTS5's `deletemerge` to 0 for the pass, restoring it after.
    ///
    /// It defaults to 10, meaning "once a level is 10% tombstones, merge it".
    /// During a mass withdrawal that threshold is crossed early and then
    /// repeatedly, and each crossing rewrites a whole level of the full-text
    /// index *inline with the scan* (`fts5IndexFindDeleteMerge`, reached from
    /// `fts5IndexAutomerge` — every contentless delete counts into the same
    /// write-counter that drives it). Off, the tombstones simply accumulate
    /// and the merge at the end does the consolidation once.
    no_deletemerge: bool,
    /// Sort each page's ids ascending before deleting.
    ///
    /// The scan serves rows in `(parent, name)` order, which is uncorrelated
    /// with rowid, so `fts5StorageContentlessDelete`'s `%_docsize` lookup lands
    /// somewhere new every time. Sorting restores locality *within* a page.
    /// It cannot restore it across pages — only scanning by rowid could, which
    /// is a change to the scan and not to this.
    sort_ids: bool,
}

impl Knobs {
    fn label(self) -> String {
        match (self.no_deletemerge, self.sort_ids) {
            (false, false) => "as shipped".into(),
            (true, false) => "deletemerge off".into(),
            (false, true) => "rowid-sorted".into(),
            (true, true) => "both".into(),
        }
    }

    /// FTS5 persists `deletemerge` in `%_config`, so this has to be put back —
    /// the arm is discarded either way, but a leaked setting would make any
    /// later reading of the same file mean something else.
    fn apply(self, conn: &Connection) {
        if self.no_deletemerge {
            set_deletemerge(conn, 0);
        }
    }

    fn restore(self, conn: &Connection) {
        if self.no_deletemerge {
            set_deletemerge(conn, FTS5_DEFAULT_DELETEMERGE);
        }
    }
}

/// FTS5's own default, from `FTS5_DEFAULT_DELETE_AUTOMERGE` in the amalgamation.
const FTS5_DEFAULT_DELETEMERGE: i64 = 10;

fn set_deletemerge(conn: &Connection, percent: i64) {
    conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('deletemerge', ?1)",
        [percent],
    )
    .expect("set deletemerge");
}

/// Page the index the way `scope::advance` does, doing `stage`'s share of the
/// work and timing each phase separately.
///
/// The writes are spelled out here rather than routed through `repo` so that
/// the per-row and chunked variants can be compared in one run — and so this
/// keeps measuring the same thing after `repo` is reshaped around whichever
/// wins.
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
    let range = ExtractCursor::for_root(ROOT);
    let max_size = config.processing.maximum_text_file_size;

    let mut t = Timing::default();
    knobs.apply(conn);
    let io_before = Io::read();
    let (_, misses_before) = testutil::cache_stats(conn);
    let started = Instant::now();

    let mut after = (range.lo.clone(), String::new());
    let mut pending = Pending::default();
    // `unchecked_transaction` borrows shared, which is what lets one end and
    // the next begin inside the loop — the same trick `testutil::seed_index`
    // uses for `commit_every`.
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

        // `scope::apply_page`'s decision, verbatim. `restore_text` is false
        // here — this probe narrows the filter and nothing else — so the
        // second arm reduces to "wants content but has none".
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
    // Which ids the clear is offered. Unfiltered is what ships: every flipped
    // row, whatever its stored state said about whether it could hold anything.
    let (fts_ids, failed_ids): (&[i64], &[i64]) = if stage.clear_filtered() {
        (&pending.had_posting, &pending.had_failure)
    } else {
        (&pending.all, &pending.all)
    };

    // The clears. The shipped shape is `remove_content_for_id` followed by
    // `set_state_clearing_failure`'s `failed_files` sweep, one bound statement
    // at a time — prepared through the connection's cache, as `repo::exec`
    // does, since preparing afresh per row would price a mistake the product
    // does not make.
    if stage.clears() {
        // Each target timed on its own, and each loop run to completion before
        // the next starts: interleaving them the way `remove_content_for_id`
        // does would charge whichever statement happened to be executing when
        // the page cache spilled.
        let at = Instant::now();
        if stage.chunked_clears() {
            chunked(tx, "DELETE FROM searchabletext WHERE rowid IN", fts_ids);
        } else {
            for id in fts_ids {
                one(tx, "DELETE FROM searchabletext WHERE rowid = ?1", *id);
            }
        }
        t.fts += at.elapsed();

        let at = Instant::now();
        if stage.chunked_clears() {
            chunked(tx, "DELETE FROM documents_text WHERE file_id IN", fts_ids);
        } else {
            for id in fts_ids {
                one(tx, "DELETE FROM documents_text WHERE file_id = ?1", *id);
            }
        }
        t.text += at.elapsed();

        let at = Instant::now();
        if stage.chunked_clears() {
            chunked(tx, "DELETE FROM failed_files WHERE file_id IN", failed_ids);
        } else {
            for id in failed_ids {
                one(tx, "DELETE FROM failed_files WHERE file_id = ?1", *id);
            }
        }
        t.failed += at.elapsed();
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

/// `sql` is the statement up to and including `IN`; the list follows.
fn chunked(tx: &Connection, sql: &str, ids: &[i64]) {
    for chunk in ids.chunks(CHUNK) {
        let full = format!("{} ({})", sql, placeholders(chunk.len()));
        tx.prepare_cached(&full)
            .and_then(|mut s| s.execute(rusqlite::params_from_iter(chunk.iter())))
            .expect("chunked write");
    }
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

/// How much data FTS5 is holding — the only quiescence signal that works.
///
/// `sqlite3_changes()` after a `'merge'` does **not** report whether the merge
/// did any work: measured in `pruneprobe`, it reports non-zero forever, so a
/// `while changes() != 0` loop never terminates. Watching `%_data` shrink and
/// stop is what actually detects a consolidated index.
fn fts_data_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM searchabletext_data", [], |r| r.get(0))
        .unwrap_or(-1)
}

/// Merge until `%_data` stops moving, and report what that took.
///
/// The **positive** argument is deliberate: negative picks
/// `fts5IndexOptimizeStruct` instead, which is a different algorithm and a
/// documented trap (`file_handling::records::fts_finalize_after_text_indexing`).
fn merge_to_quiescence(conn: &Connection) -> (Duration, u32, i64, i64) {
    let before = fts_data_rows(conn);
    let started = Instant::now();
    let mut rounds = 0;
    let mut last = before;
    loop {
        conn.execute(
            "INSERT INTO searchabletext(searchabletext, rank) VALUES('merge', ?1)",
            [MERGE_PAGES],
        )
        .expect("merge");
        rounds += 1;
        let now = fts_data_rows(conn);
        if now == last || rounds > 500 {
            break;
        }
        last = now;
    }
    (started.elapsed(), rounds, before, last)
}

// ---------------------------------------------------------------------------
// Corpus and arms
// ---------------------------------------------------------------------------

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn spec() -> SeedSpec {
    SeedSpec {
        files: env_usize("QSB_FILES", 200_000),
        dirs: env_usize("QSB_DIRS", 2_000),
        // **One, and it has to be.** With a mix, extractability is decided by
        // the row's MIME, and `seed_index` intersects that with this stride.
        // Any other value would leave rows that an extractor claims sitting at
        // `STATE_NA` — a corpus born disagreeing with its own configuration,
        // whose first reconcile repairs the seed instead of applying the edit.
        content_every: 1,
        // One in twenty extractable rows never got its content: the residue an
        // interrupted run leaves. These are the rows the shipped code clears
        // FTS, stored text and a failure record for, all three of which are
        // provably absent — `+clear(done)` is what prices that.
        pending_every: env_usize("QSB_PENDING_EVERY", 20),
        ext_mix: EXT_MIX,
        // Depth is most of what decides stored row width, and therefore how
        // many pages the scan reads — see `SeedSpec::dir_depth`.
        dir_depth: env_usize("QSB_DIR_DEPTH", 6).max(2),
        // A real run commits in slices, and each commit flushes FTS5's hash to
        // its own segment. Seeding in one transaction would leave a single
        // segment and understate every clear cost below.
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

/// A byte-identical copy of `master`, under its own scratch directory.
///
/// `seed_index` ends with a TRUNCATE checkpoint, so the one file holds the
/// whole index and there is no `-wal` to carry across.
fn clone_arm(master: &Arm, tag: &str) -> Arm {
    let path = testutil::scratch_db(tag);
    std::fs::copy(&master.path, &path).expect("copy the seeded index");
    Arm {
        what: master.what.clone(),
        keyed: master.keyed,
        pgsz: master.pgsz,
        page_size: master.page_size,
        hmac: master.hmac,
        path,
        seeded_in: Duration::ZERO,
    }
}

/// The coordinator's own writer profile, which is what the reconcile actually
/// runs on — `PRAGMAS_INCREMENTAL`, a 4 MiB page cache. Opening this
/// `open_existing(.., true)` instead would measure `PRAGMAS_FAST`'s 8 MiB and
/// quietly flatter every figure below.
fn open(arm: &Arm) -> Connection {
    arm.with_key(|| {
        db::open::open_incremental_writer(&arm.path.to_string_lossy()).expect("open the copy")
    })
}

/// What `PRAGMAS_INCREMENTAL` gives the reconcile today, in MiB.
const SHIPPED_MIB: i64 = 4;

fn set_cache(conn: &Connection, mib: i64) {
    conn.execute_batch(&format!("PRAGMA cache_size = -{};", mib * 1024))
        .expect("cache_size");
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

// ---------------------------------------------------------------------------

fn main() {
    let spec = spec();
    let arms: Vec<bool> = match std::env::var("QSB_ARMS").as_deref() {
        Ok("plain") => vec![false],
        Ok("keyed") => vec![true],
        _ => vec![false, true],
    };
    let registry = Registry::default_set();

    // The corpus's premise, checked rather than trusted.
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

    for keyed in arms {
        let label = if keyed { "keyed" } else { "plain" };
        let master = Arm::seed(label, &format!("content-{}", label), keyed, &spec);
        println!(
            "\n=== {} ===  seeded in {:.1}s, {}",
            label,
            master.seeded_in.as_secs_f64(),
            common::mib(master.size_bytes())
        );

        // What the narrowing is worth, read off the index rather than assumed.
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
                 the other {} are cleared today for nothing",
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
            // Why the state flip is listed apart from the clears. A chunked
            // `IN (...)` is an unambiguous win for a `DELETE`, and the same
            // shape applied to the `UPDATE` is not — the plans say which of
            // them seeks and which scans, and no timing can be read without
            // knowing that.
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
            println!(
                "  fts: {} %_data rows ({}), {} %_idx, {} %_docsize",
                count(&conn, "SELECT COUNT(*) FROM searchabletext_data"),
                common::mib(
                    count(
                        &conn,
                        "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat \
                         WHERE name LIKE 'searchabletext%'"
                    )
                    .max(0) as u64
                ),
                count(&conn, "SELECT COUNT(*) FROM searchabletext_idx"),
                count(&conn, "SELECT COUNT(*) FROM searchabletext_docsize"),
            );
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
            // Consolidation, priced apart: today one 1000-page `'merge'` runs
            // and the scrub stops, whatever it left behind.
            if stage.clears() {
                let (took, rounds, before, after) = merge_to_quiescence(&conn);
                println!(
                    "  {:<16} merge: {:.0} ms over {} rounds, %_data {} -> {} rows",
                    "",
                    took.as_secs_f64() * 1000.0,
                    rounds,
                    before,
                    after
                );
            }
            drop(conn);
            arm.discard();
        }

        // Is the cost priced per commit or per row? Today's
        // one-transaction-per-page is the first line of each block; if it
        // collapses as pages are folded together, commit cadence is the lever
        // and the write shape above is not. Both shapes are swept, because the
        // two fixes are independent and either could subsume the other.
        for (what, stage) in [
            ("as shipped", Stage::ClearAll),
            ("+chunked", Stage::Chunked),
        ] {
            println!("\n  transaction size, at {}:", what);
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

        // FTS5's own machinery, which is where the time actually is once the
        // statement shape stops mattering. Both knobs leave the index in the
        // same state — see `Knobs` — so either is free to adopt if it pays.
        // Run at the chunked shape, so what is left to see is FTS5's and not
        // the per-row overhead's.
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
            let (took, rounds, before, after) = merge_to_quiescence(&conn);
            println!(
                "  {:<16} merge: {:.0} ms over {} rounds, %_data {} -> {} rows",
                "",
                took.as_secs_f64() * 1000.0,
                rounds,
                before,
                after
            );
            drop(conn);
            arm.discard();
        }

        // The page cache. `PRAGMAS_INCREMENTAL` gives the reconcile a flat
        // 4 MiB while it scans and rewrites across the *whole* index. The scan
        // reads `idx_files_parent` in order but fetches a table row per entry,
        // and the writes then scatter into `%_docsize`, `documents_text` and
        // FTS5's tombstone pages in orders uncorrelated with `(parent, name)`.
        // `misses` is the column to read.
        println!("\n  page cache, as shipped  [{} = shipped]:", SHIPPED_MIB);
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

        // How much of the index the narrowing takes, swept. A filter that
        // withdraws most of the postings is a different problem from one that
        // withdraws a few: past some fraction FTS5 stops tombstoning and starts
        // rewriting segments, and rewriting the full-text index is what
        // building it was. `today` is what ships; `fixed` is the projection.
        println!("\n  withdrawn fraction (today -> fixed, both + merge to quiescence):");
        println!(
            "  {:<13} {:>8} {:>9} {:>9} {:>9} {:>11} {:>11}",
            "keeping", "written", "today", "fixed", "merge", "%_data", "postings"
        );
        for keep in [&["txt", "md"][..], &["txt"][..], &["rtf"][..]] {
            let config = config_with(keep);
            let tag = keep.join("+");
            let mut timings = Vec::new();
            for (what, stage, commit) in [
                ("today", Stage::ClearAll, Commit::PerPage),
                ("fixed", Stage::Chunked, Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("content-{}-{}-{}", label, tag, what));
                let conn = open(&arm);
                let before = fts_data_rows(&conn);
                let t = run_stage(&conn, &config, &registry, stage, commit);
                let (merge, _, _, after) = merge_to_quiescence(&conn);
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

        // The reference number every stage above is decomposing: the real
        // `scope::advance`, driven to completion the way the coordinator drives
        // it. It must land on `+clear(all)`.
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
