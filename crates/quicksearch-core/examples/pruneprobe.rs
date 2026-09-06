//! Where the time goes when an added ignore pattern scrubs the index.
//!
//! Adding one pattern sets `IndexWork { prune_scope: true, reindex: false }`
//! (`config::diff_actions`), and `scope::advance` then reads **every** stored
//! row under every root, decides each one, and deletes the losers by id. On a
//! large index that has been measured slower than building the index was. This
//! attributes that time to a phase.
//!
//! ```text
//! cargo build -p quicksearch-core --example pruneprobe --release
//! ./target/release/examples/pruneprobe
//! ```
//!
//! `QSB_FILES` / `QSB_DIRS` / `QSB_CONTENT_EVERY` size the corpus;
//! `QSB_ARMS=plain|keyed|both` picks the key states. A keyed arm is not
//! optional dressing — every page the delete touches is decrypted and
//! re-encrypted, and the two arms have disagreed before (see
//! `db::schema::PAGE_SIZE`).
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
//! **No stage is what ships any more.** `scope::advance` tombstones only the
//! ids that can hold a posting (`+fts(done)`), commits per slice rather than
//! per page, and turns FTS5's delete-merging off for the pass
//! (`file_handling::fts_begin_tombstone_burst`), so the `live` row below lands
//! under every stage rather than on one of them. The stages remain the
//! decomposition — they say where the time is — and `live` says what the sum
//! of the shipped decisions costs.
//!
//! Two of them were measured and **not** adopted, which is why they are still
//! here: `+subtree` bought nothing over `+fts(done)` (247 ms against 243 on the
//! 40k corpus, with all 8,080 doomed rows genuinely skipping the page loop), and
//! raising the page cache moved misses twelvefold while barely moving the clock.
//!
//! **Read the `commit` column, not `fts`.** FTS5 buffers a contentless delete
//! in memory and writes the tombstone pages when the transaction is flushed, so
//! the `DELETE` statement itself times as nearly free and the cost lands in the
//! commit. That is also why `-tx` sweeps the number of pages per transaction:
//! if the flush is priced per commit rather than per tombstone, transaction
//! size is the variable that matters and the row count is not.
//!
//! Every stage runs against a byte-identical copy of one seeded index rather
//! than a fresh seed: FTS5 segment layout is most of what decides delete cost,
//! and reseeding would let it drift between the rows of the table.
//!
//! The final table prices the *other* bulk withdrawal, a completed run's stale
//! cleanup (`file_handling::cleanup_stale_index_entries`): the same doomed
//! rows, deleted by path the way that pass does. Its stages are spelled out in
//! probe code for the same reason as above — they are the fixed decomposition
//! — and its `live` row is the shipped function, which is the row that moves
//! when `file_handling::batch` is reshaped.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};

use quicksearch_core::config::Config;
use quicksearch_core::db::{self, repo};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{
    cleanup_stale_index_entries, fts_begin_tombstone_burst, fts_end_tombstone_burst,
    fts_finalize_after_text_indexing, split_db_path, ExtractCursor,
};
use quicksearch_core::scope::{self, Scope, WorkCursor};
use quicksearch_core::testutil::{self, Arm, SeedSpec};

use common::Io;

/// The root every `seed_index` row hangs under; it need not exist on disk,
/// because nothing in the prune path stats a stored path on Unix (see
/// `platform::entry_hidden_reason`, which short-circuits on the dot prefix).
const ROOT: &str = "/seed";

/// The ignore patterns the probe can add, each excluding a fifth of the index.
///
/// `seed_index` names its second path segment `WORDS[(d * 7 + 13) % 35]`, and
/// because 7 and 35 share a factor only these five of the thirty-five words are
/// ever reachable — one per residue of `d mod 5`. So each excludes a fifth of
/// the directories, spread across the keyspace rather than gathered at one end,
/// which is the shape a real `node_modules` or `.git` pattern has. Taking a
/// prefix of this list is how the probe sweeps the excluded fraction. `main`
/// asserts the fractions rather than trusting this comment.
const PRUNE_PATTERNS: [&str; 5] = ["jumps", "content", "revenue", "figure", "eta"];

/// The single-pattern case, and the one the stage table runs.
const PRUNE_PATTERN: &str = PRUNE_PATTERNS[0];

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

/// Whether the walker would still descend into the stored parent `parent`.
///
/// `Scope::covers` judges a path component by component, which for a directory
/// is exactly "would the walk enter it" — the same question the walker asks
/// before recursing. The trailing separator every stored parent carries has to
/// go first, or the leaf component is empty.
fn dir_covered(scope: &Scope, root: &Path, parent: &str) -> bool {
    let trimmed = parent.trim_end_matches(['/', '\\']);
    scope.covers(root, Path::new(trimmed))
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

/// Page the index the way `scope::advance` does, doing `stage`'s share of the
/// work and timing each phase separately.
///
/// The deletes are spelled out here rather than routed through `repo` so that
/// the filtered and unfiltered FTS variants can be compared in one run — and
/// so this keeps measuring the same thing after `repo` is reshaped around
/// whichever variant wins.
fn run_stage(conn: &Connection, config: &Config, stage: Stage, commit: Commit) -> Timing {
    let scope = Scope::from_config(config).expect("compile the scope");
    let root = PathBuf::from(ROOT);
    let range = ExtractCursor::for_root(ROOT);

    let mut t = Timing::default();
    let io_before = Io::read();
    let (_, misses_before) = testutil::cache_stats(conn);
    let started = Instant::now();

    let mut after = (range.lo.clone(), String::new());
    let mut covered = scope::CoverCache::default();
    // `unchecked_transaction` borrows shared, which is what lets one end and
    // the next begin inside the loop — the same trick `testutil::seed_index`
    // uses for `commit_every`.
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

        // The subtree short-circuit. A page spans several directories, so the
        // verdict is taken per distinct parent *as it appears* — stopping at
        // the first doomed one, range-deleting it, and re-seeking past it.
        // Testing only `rows[0]` would fire only on the pages that happen to
        // begin inside an excluded directory.
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
                let sub = ExtractCursor::for_root(&dir);
                let (removed, fts, files) = delete_range(&tx, &sub.lo, &sub.hi);
                t.fts += fts;
                t.files += files;
                t.deleted += removed;
                t.skipped += removed;
                // The rows before the doomed directory still need deciding;
                // everything from it on is gone or will be re-read after the
                // seek. Both happen inside the open transaction, so the seek
                // sees the range delete without needing a commit first.
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
                for chunk in fts_ids.chunks(CHUNK) {
                    let at = Instant::now();
                    tx.execute(
                        &format!(
                            "DELETE FROM searchabletext WHERE rowid IN ({})",
                            placeholders(chunk.len())
                        ),
                        rusqlite::params_from_iter(chunk.iter()),
                    )
                    .expect("tombstone");
                    t.fts += at.elapsed();
                }
            }
            for chunk in doomed.chunks(CHUNK) {
                let at = Instant::now();
                t.deleted += tx
                    .execute(
                        &format!(
                            "DELETE FROM files WHERE id IN ({})",
                            placeholders(chunk.len())
                        ),
                        rusqlite::params_from_iter(chunk.iter()),
                    )
                    .expect("delete rows");
                t.files += at.elapsed();
            }
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
    /// Per-path deletes, three statements each, FTS handed every id — the
    /// pass as originally shipped. This is the control row: it reproduces
    /// that shape in probe code, so it must not move when
    /// `cleanup_stale_index_entries` is reshaped.
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
            for chunk in with_postings.chunks(CHUNK) {
                let at = Instant::now();
                tx.execute(
                    &format!(
                        "DELETE FROM searchabletext WHERE rowid IN ({})",
                        placeholders(chunk.len())
                    ),
                    rusqlite::params_from_iter(chunk.iter()),
                )
                .expect("tombstone");
                t.fts += at.elapsed();
            }
            for chunk in ids.chunks(CHUNK) {
                let at = Instant::now();
                t.deleted += tx
                    .execute(
                        &format!(
                            "DELETE FROM files WHERE id IN ({})",
                            placeholders(chunk.len())
                        ),
                        rusqlite::params_from_iter(chunk.iter()),
                    )
                    .expect("delete rows");
                t.files += at.elapsed();
            }
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

/// How much data FTS5 is holding — the only quiescence signal that works.
///
/// `sqlite3_changes()` after a `'merge'` does **not** report whether the merge
/// did any work: measured here, it reports non-zero forever, so a
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
        // **Coprime with 5, and that is the whole point.** `seed_index` places
        // content every `content_every` files and directories every `dirs`,
        // and [`PRUNE_PATTERN`] excludes the directories where `d mod 5 == 0`.
        // The shipped default of 10 shares the factor 5 with that, so *every*
        // extracted document lands in an excluded directory: the prune deletes
        // 100% of the postings, the FTS index goes empty, and every question
        // about tombstone cost is answered by a degenerate corpus. 7 puts a
        // fifth of the documents in the doomed set, which is the real shape.
        // `main` asserts the fraction.
        content_every: env_usize("QSB_CONTENT_EVERY", 7),
        // At least two segments, so the second can carry the excluded name.
        // Depth is most of what decides stored row width, and therefore how
        // many pages the scan reads — see `SeedSpec::dir_depth`.
        dir_depth: env_usize("QSB_DIR_DEPTH", 6).max(2),
        // A real run commits in slices, and each commit flushes FTS5's hash to
        // its own segment. Seeding in one transaction would leave a single
        // segment and understate every tombstone cost below.
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

/// A writer knob worth sweeping, applied over [`open`]'s profile.
#[derive(Clone, Copy)]
struct Knobs {
    label: &'static str,
    /// `None` leaves SQLite's 1000-page default: the committing thread
    /// checkpoints — copying WAL pages back into the main file, re-encrypting
    /// every one on a keyed index — every 8 MiB of log.
    autocheckpoint: Option<i64>,
    /// MiB of page cache; `None` keeps `PRAGMAS_INCREMENTAL`'s 4.
    cache_mib: Option<i64>,
}

const SHIPPED: Knobs = Knobs {
    label: "as shipped",
    autocheckpoint: None,
    cache_mib: None,
};

/// What `PRAGMAS_INCREMENTAL` gives the reconcile today, in MiB. Named so the
/// sweep's first row is labelled with the figure it is arguing against rather
/// than repeating it as a literal.
const SHIPPED_MIB: i64 = 4;

fn apply(conn: &Connection, knobs: Knobs) {
    if let Some(n) = knobs.autocheckpoint {
        conn.execute_batch(&format!("PRAGMA wal_autocheckpoint = {};", n))
            .expect("autocheckpoint");
    }
    if let Some(mib) = knobs.cache_mib {
        conn.execute_batch(&format!("PRAGMA cache_size = -{};", mib * 1024))
            .expect("cache_size");
    }
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

    println!(
        "corpus: {} files across {} dirs, 1 in {} with content, pattern {:?}",
        spec.files, spec.dirs, spec.content_every, PRUNE_PATTERN
    );

    for keyed in arms {
        let label = if keyed { "keyed" } else { "plain" };
        let master = Arm::seed(label, &format!("prune-{}", label), keyed, &spec);
        println!(
            "\n=== {} ===  seeded in {:.1}s, {}",
            label,
            master.seeded_in.as_secs_f64(),
            common::mib(master.size_bytes())
        );

        // What the pattern is worth, read off the index rather than assumed.
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
                 the rest are tombstoned today for nothing",
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
            // What the tombstone actually has to find its way into. `%_data`
            // holds the segment leaves, `%_idx` one row per segment b-tree
            // node, `%_docsize` the origin a contentless delete looks up.
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
            // Consolidation, priced apart: today one 1000-page `'merge'` runs
            // and the scrub stops, whatever it left behind.
            if stage.deletes_fts() {
                let (took, rounds, before, after) = merge_to_quiescence(&conn);
                println!(
                    "  {:<11} merge: {:.0} ms over {} rounds, %_data {} -> {} rows",
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
        // one-transaction-per-page is the first line; if it collapses as pages
        // are folded together, commit cadence is the lever and none of the
        // stages above are.
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

        // The projected end state: commit on the slice `advance` already runs
        // to, skip doomed directories wholesale, tombstone only what can hold
        // a posting. Nothing here needs a longer uninterruptible window than
        // ships today — the slice boundary is already the cancellation point.
        println!("\n  combined, committing once per {:?} slice:", scope::SLICE);
        header();
        {
            let arm = clone_arm(&master, &format!("prune-{}-combined", label));
            let conn = open(&arm);
            let t = run_stage(&conn, &config, Stage::Subtree, Commit::Slice(scope::SLICE));
            row("combined", &t);
            let (took, rounds, before, after) = merge_to_quiescence(&conn);
            println!(
                "  {:<11} merge: {:.0} ms over {} rounds, %_data {} -> {} rows",
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
        // 4 MiB while it scans and deletes across the *whole* index — and a
        // delete does not only touch the index the scan reads in order. It
        // maintains `idx_files_mtime`, `idx_files_mime` and `idx_files_hash`
        // too, three b-trees in orders uncorrelated with `(parent, name)`, so
        // every doomed row scatters ~3 page touches that a 512-page cache
        // cannot hold. `misses` is the column to read.
        //
        // Both commit policies are swept, because a cache that holds the
        // working set may make the commit cadence stop mattering.
        println!("\n  page cache, at +fts(all)  [{} = shipped]:", SHIPPED_MIB);
        header();
        for mib in [SHIPPED_MIB, 16, 32, 64, 128, 256] {
            for (name, commit) in [
                ("per page", Commit::PerPage),
                ("per slice", Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("prune-{}-c{}-{}", label, mib, name.len()));
                let conn = open(&arm);
                apply(
                    &conn,
                    Knobs {
                        cache_mib: Some(mib),
                        ..SHIPPED
                    },
                );
                let t = run_stage(&conn, &config, Stage::FtsAll, commit);
                row(&format!("{} MiB, {}", mib, name), &t);
                drop(conn);
                arm.discard();
            }
        }

        // Measured and rejected, so the table records it: with autocheckpoint
        // at its 1000-page default the committing thread copies the log back
        // into the main file every 8 MiB and re-encrypts every page it moves,
        // which looked like an obvious suspect. Deferring it to one checkpoint
        // at the end is a wash — the pages have to move either way.
        {
            let arm = clone_arm(&master, &format!("prune-{}-nockpt", label));
            let conn = open(&arm);
            apply(
                &conn,
                Knobs {
                    autocheckpoint: Some(0),
                    ..SHIPPED
                },
            );
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

        // How much of the index the pattern takes, swept. A prune that removes
        // most of the postings is a different problem from one that removes a
        // few: past some fraction FTS5 stops tombstoning and starts rewriting
        // segments, and rewriting the full-text index is what building it was.
        // `today` is what ships; `fixed` is the projection above.
        println!("\n  excluded fraction (today -> fixed, both + merge to quiescence):");
        println!(
            "  {:<11} {:>8} {:>9} {:>9} {:>9} {:>11} {:>11}",
            "patterns", "deleted", "today", "fixed", "merge", "%_data", "postings"
        );
        for n in 1..=PRUNE_PATTERNS.len() {
            let patterns = &PRUNE_PATTERNS[..n];
            let config = config_with(patterns);
            let mut timings = Vec::new();
            for (tag, stage, commit) in [
                ("today", Stage::FtsAll, Commit::PerPage),
                ("fixed", Stage::Subtree, Commit::Slice(scope::SLICE)),
            ] {
                let arm = clone_arm(&master, &format!("prune-{}-f{}-{}", label, n, tag));
                let conn = open(&arm);
                let before = fts_data_rows(&conn);
                let t = run_stage(&conn, &config, stage, commit);
                let (merge, _, _, after) = merge_to_quiescence(&conn);
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

        // The reference number every stage above is decomposing: the real
        // `scope::advance`, driven to completion the way the coordinator drives
        // it. It lands *under* every stage — see the header for which decisions
        // put it there.
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

        // Stale cleanup, the pass a completed run ends with. Same doomed rows
        // as the tables above, but deleted by *path* — the walk hands
        // `cleanup_stale_index_entries` a list of paths it did not see, in
        // directory order. Runs on `PRAGMAS_FAST` via `open_existing(_, true)`
        // because that is the run's own writer connection, the one the real
        // pass executes on — `open` here would measure the reconcile's 4 MiB
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
                let conn = arm.with_key(|| {
                    db::open::open_existing(&arm.path.to_string_lossy(), true)
                        .expect("open the copy")
                });
                let t = run_stale(&conn, &stale, shape);
                row(shape.label(), &t);
                let (took, rounds, before, after) = merge_to_quiescence(&conn);
                println!(
                    "  {:<11} merge: {:.0} ms over {} rounds, %_data {} -> {} rows",
                    "",
                    took.as_secs_f64() * 1000.0,
                    rounds,
                    before,
                    after
                );
                drop(conn);
                arm.discard();
            }

            // The shipped function, whole. The row that moves when
            // `file_handling::batch` is reshaped; the stages above must not.
            let arm = clone_arm(&master, &format!("prune-{}-stale-live", label));
            let conn = arm.with_key(|| {
                db::open::open_existing(&arm.path.to_string_lossy(), true).expect("open the copy")
            });
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
            let (took, rounds, before, after) = merge_to_quiescence(&conn);
            println!(
                "  {:<11} merge: {:.0} ms over {} rounds, %_data {} -> {} rows   <- cleanup_stale_index_entries",
                "",
                took.as_secs_f64() * 1000.0,
                rounds,
                before,
                after
            );
            drop(conn);
            arm.discard();
        }

        master.discard();
    }
}
