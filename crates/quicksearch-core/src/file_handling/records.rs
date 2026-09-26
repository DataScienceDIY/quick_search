//! Building one file's index record: classification against stored rows,
//! hashing, MIME sniffing, and the inline-content decision.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::*;
use crate::config::Config;
use crate::db::repo::{self, NewFile};
use crate::extract::{Registry, Scratch};
use crate::mime::{guess_mime_from_head, mime_to_type, FileType};

/// One directory's indexed files, as `name -> mtime`.
pub type DirRows = HashMap<String, u64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIndexAction {
    Skip,
    Update,
    Insert,
}

/// Decide what Phase 1 should do with a file. A file whose parent is not this
/// directory (a resolved symlink target) must not be classified here: it
/// would read as `Insert`, and `INSERT OR IGNORE` would silently not update
/// it. Use [`classify_by_mtime`] for those.
pub fn classify_for_indexing(name: &str, mtime: u64, rows: &DirRows) -> FileIndexAction {
    classify_by_mtime(rows.get(name).copied(), mtime)
}

pub fn classify_by_mtime(stored: Option<u64>, mtime: u64) -> FileIndexAction {
    match stored {
        Some(known) if known == mtime => FileIndexAction::Skip,
        Some(_) => FileIndexAction::Update,
        None => FileIndexAction::Insert,
    }
}

/// Truncate in place to at most `max_bytes`, backing up to a UTF-8 char
/// boundary, and give back the capacity: the text goes on to wait in the
/// content pass's queues, where a 40 MiB log truncated without the shrink
/// would still hold 40 MiB per queued row.
pub(crate) fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        s.truncate(s.floor_char_boundary(max_bytes));
        s.shrink_to_fit();
    }
}

/// A file's modified time in whole seconds, the unit `files.mtime` stores and
/// every unchanged-file check compares; `None` when the platform has none.
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// A SHA-256 digest, held inline rather than as a `Vec` so hashing allocates
/// nothing per file.
pub type FileHash = [u8; 32];

/// Identify a file as `sha256(size || first hash_length bytes)`, leaving the
/// head bytes in `head` for MIME sniffing. The cost is a collision class:
/// same-size files with identical heads read as duplicates
/// (`examples/hashprobe.rs` has the study); `crate::verify` is the way out.
///
/// `head` is the caller's buffer, reused file after file; it is resized to
/// the bytes actually read and its previous contents are discarded.
pub fn get_file_hash(
    size: u64,
    path: &Path,
    hash_length: usize,
    head: &mut Vec<u8>,
) -> Result<FileHash, std::io::Error> {
    // The caller's `is_file()` came from a `stat` taken before this open; a
    // FIFO renamed over the name in between would block this walk worker
    // forever and park the pool behind it. See `platform::open_regular_file`.
    let mut f: File = crate::platform::open_regular_file(path)?;
    head.clear();
    head.resize(size.min(hash_length as u64) as usize, 0);
    f.read_exact(head)?;

    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());
    hasher.update(&*head);
    Ok(hasher.finalize().into())
}

/// Run the FTS5 special command `cmd` with `value`. Best-effort: failure is
/// logged, and an index without the setting is still correct.
fn fts_command(conn: &Connection, cmd: &str, value: i64) {
    if let Err(e) = conn.execute(
        &format!("INSERT INTO searchabletext(searchabletext, rank) VALUES('{cmd}', ?1)"),
        [value],
    ) {
        crate::log_warn!("FTS {} failed (non-fatal): {}", cmd, e);
    }
}

/// FTS5's automerge threshold (2..=16). 16 measured ~19% faster cold indexing
/// and a third fewer bytes written for no search cost; don't retune casually.
const WRITE_AUTOMERGE: u8 = 16;

/// FTS5's crisis-merge threshold (its default is 16). 32 measured a 13%
/// smaller, more build-stable index for no cost; 64 measured identically.
const WRITE_CRISISMERGE: u8 = 32;

/// FTS5's default `deletemerge`: the tombstone percentage at which a level is
/// rewritten, the dominant cost of bulk content removal. `0` disables
/// delete-merging, even for an explicit `'merge'`, so only
/// [`fts_begin_tombstone_burst`] sets it, always paired with a restore.
pub const FTS_DELETEMERGE: u8 = 10;

/// Stop FTS5 reclaiming tombstones while a pass creates them, for a caller
/// about to delete many postings at once; it calls [`fts_end_tombstone_burst`]
/// when done.
///
/// Every contentless delete counts toward the write counter that drives
/// `fts5IndexAutomerge`, and once a level passes [`FTS_DELETEMERGE`],
/// `fts5IndexFindDeleteMerge` rewrites the whole level, inline with the scan
/// and again as the deletes go on: the pass keeps rebuilding the index.
///
/// # Measurements
///
/// `examples/contentprobe.rs`, 40k rows of which 7,273 lose their content
/// (6,909 of them holding a posting), chunked deletes, committing per
/// `scope::SLICE`, each pair from one run:
///
/// | arm | | fts | commit | pass | + merge | misses |
/// |---|---|---|---|---|---|---|
/// | plain | `deletemerge` 10 | 734 ms | 96 ms | **891 ms** | 892 ms | 97,130 |
/// | plain | `deletemerge` 0 | 86 ms | 11 ms | **141 ms** | 218 ms | 9,654 |
/// | keyed | `deletemerge` 10 | 1174 ms | 263 ms | **1576 ms** | 1579 ms | 96,181 |
/// | keyed | `deletemerge` 0 | 128 ms | 17 ms | **211 ms** | 373 ms | 9,476 |
///
/// 6.3x on the pass plain and 7.5x keyed; 4.1x and 4.2x once the trailing
/// merge is counted. Page-cache misses fall tenfold, so the keyed arm, which
/// decrypts and re-encrypts every such page, gains more. The index also comes
/// out smaller (`%_data` 1,753 rows against 1,886): one merge at the end
/// consolidates better than many mid-pass ones.
///
/// End to end (`scope::advance` driven to completion as the coordinator
/// drives it, on an idle machine), this plus `scope`'s slice-long
/// transactions and narrowed writes is worth 4.3x to 5.6x, and the ratio
/// grows with the index because the needlessly rewritten levels grow with it:
///
/// | corpus | arm | before | after | |
/// |---|---|---|---|---|
/// | 40k | plain | 1,066 ms | **248 ms** | 4.3x |
/// | 40k | keyed | 1,814 ms | **417 ms** | 4.4x |
/// | 200k | plain | 4,600 ms | **925 ms** | 5.0x |
/// | 200k | keyed | 8,598 ms | **1,530 ms** | 5.6x |
///
/// (The 40k pair is an A/B against a build without these changes, controlled
/// by `contentprobe`'s `+clear(all)` stage, which reproduces the unchanged
/// shape in the probe's own code and did not move: 1000 → 996 plain,
/// 1801 → 1770 keyed. The 200k rows use that stage as the "before".)
///
/// Measured and not adopted: sorting each page's ids into rowid order before
/// deleting moved nothing (898 ms against 891 plain, 1565 against 1576
/// keyed), because FTS5 picks a tombstone page by hashing the rowid.
///
/// # The pairing is load-bearing
///
/// `deletemerge` persists in FTS5's `%_config` shadow table, so a value left
/// at `0` outlives the process and no later `'merge'` would reclaim a
/// tombstone. [`fts_end_tombstone_burst`] restores it, and
/// [`fts_begin_bulk_write`] sets it unconditionally, so the next indexing run
/// repairs a crash between the two.
pub fn fts_begin_tombstone_burst(conn: &Connection) {
    fts_command(conn, "deletemerge", 0);
}

/// Rounds of [`fts_finalize_after_text_indexing`] a burst may spend
/// consolidating before leaving the rest to the next run. The measured corpus
/// needed three; the cap only keeps a pathological index from holding the
/// coordinator thread indefinitely.
const BURST_MERGE_ROUNDS: u32 = 8;

/// Restore delete-merging, then merge to quiescence; the mirror of
/// [`fts_begin_tombstone_burst`]. The threshold goes back first, since the
/// merge reclaims nothing at `0`, and even with nothing to merge, since a `0`
/// would outlive the process.
///
/// Unlike [`fts_finalize_after_text_indexing`]'s single merge, which the next
/// run finishes, this merges until nothing changes: a burst leaves a backlog
/// several times larger than a run does (`%_data` 4,931 rows against 1,886),
/// and the pass that built it should pay for it, not later searches. It costs
/// 77 ms plain and 162 ms keyed against the ~750 ms and ~1,450 ms the burst
/// saved. Quiescence is read off the `%_data` row count, because
/// `sqlite3_changes()` after a `'merge'` stays non-zero forever
/// (`examples/pruneprobe.rs`).
pub fn fts_end_tombstone_burst(conn: &Connection) {
    fts_command(conn, "deletemerge", FTS_DELETEMERGE.into());
    let mut last = fts_data_rows(conn);
    for _ in 0..BURST_MERGE_ROUNDS {
        fts_finalize_after_text_indexing(conn);
        let now = fts_data_rows(conn);
        if now == last {
            return;
        }
        last = now;
    }
}

/// Rows in FTS5's `%_data` shadow table: how much the full-text index
/// physically holds, tombstones and all. `None` if it cannot be read, which
/// stops [`fts_end_tombstone_burst`]'s loop rather than spinning it.
fn fts_data_rows(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT COUNT(*) FROM searchabletext_data", [], |r| r.get(0))
        .ok()
}

/// Apply the write-side FTS5 settings before a run starts writing. These set
/// parameters and merge nothing (the merge is
/// [`fts_finalize_after_text_indexing`]); `automerge` in particular must be
/// set before the bulk load, since set at the end of a run it affects nothing.
///
/// `pgsz` is not a per-run setting. Sweeping it unencrypted only ever wrote
/// more bytes for an index the same size, so the default 4050 stands there;
/// keyed, SQLCipher's page reserve makes 4050 a cliff, and schema creation
/// handles that once ([`crate::db::schema::fts_pgsz_for`]).
///
/// `deletemerge` is set even though this never lowers it: it gives an index
/// whose reconcile was killed mid-burst its tombstone reclamation back (see
/// [`fts_begin_tombstone_burst`]).
pub fn fts_begin_bulk_write(conn: &Connection) {
    fts_command(conn, "automerge", WRITE_AUTOMERGE.into());
    fts_command(conn, "deletemerge", FTS_DELETEMERGE.into());
    fts_command(conn, "crisismerge", WRITE_CRISISMERGE.into());
}

/// Output leaf pages [`fts_finalize_after_text_indexing`] may write: FTS5's own
/// `FTS5_OPT_WORK_UNIT`, unswept, unlike the constants above. It is tested only
/// at a term boundary, so one term with a very long doclist writes past it.
const FINALIZE_MERGE_PAGES: i64 = 1000;

/// Merge FTS5 segments once a run has finished writing, reclaiming the
/// tombstones a `contentless_delete` table accumulates. Best-effort; an
/// unconsolidated index is still correct.
///
/// The sign of the argument picks the algorithm (`sqlite3Fts5IndexMerge`):
///
/// - Positive `N`: ordinary consolidation. Merges any level holding at least
///   `usermerge` segments (FTS5's default 4; we set `automerge` and
///   `crisismerge`, never `usermerge`), for up to `N` output leaf pages.
/// - Negative `N`: `fts5IndexOptimizeStruct` with `nMin` forced to 1, i.e.
///   `'optimize'`, hoisting every segment into one level at `|N|` pages per
///   call. It must be looped until `changes() == 0`; called once, it leaves
///   the structure permanently mid-optimize, persisted in `%_data`.
///
/// So this takes the positive form. Tombstone reclamation still happens:
/// `fts5IndexMerge` falls through to `fts5IndexFindDeleteMerge` when no level
/// has `nMin` segments, and that path keys off `deletemerge` whatever the sign.
pub fn fts_finalize_after_text_indexing(conn: &Connection) {
    fts_command(conn, "merge", FINALIZE_MERGE_PAGES);
}

/// An owned, fully-derived file record, produced by [`prepare_file_record`].
#[derive(Debug, Clone)]
pub struct OwnedNewFile {
    /// Parent and name in one buffer; the parent ends in the platform
    /// separator. One allocation rather than two: `indexprobe cold`, 100k
    /// files, 7.2 → 6.2 allocations per file.
    pub path: repo::RowPath,
    pub size: u64,
    pub mtime: u64,
    /// Borrowed from the static tables [`guess_mime_from_head`] answers from.
    pub mime: Option<&'static str>,
    pub ftype: FileType,
    /// `None` only for a dehydrated cloud placeholder. Stored as SQL NULL,
    /// which keeps such files out of duplicate detection: an empty or zero
    /// hash would make every one of them look identical.
    pub hash: Option<FileHash>,
    /// Text extracted from the head bytes during the walk, for files small
    /// enough that the head *was* the whole file. `Some` means the content
    /// pass never has to open this file; `None` leaves it pending.
    pub inline_text: Option<String>,
    /// `false` means the row is born `STATE_NA`.
    pub needs_content: bool,
}

impl OwnedNewFile {
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    pub fn as_new_file(&self) -> NewFile<'_> {
        NewFile {
            name: self.path.name(),
            parent: self.path.parent(),
            size: self.size,
            mtime: self.mtime,
            mime: self.mime,
            ftype: self.ftype,
            hash: self.hash.as_ref().map(|h| &h[..]),
            needs_content: self.needs_content,
        }
    }
}

/// "Cannot hash" warnings allowed per run before only the count is kept.
static HASH_FAILURES: crate::log::Throttle = crate::log::Throttle::new(20);

/// Arm the per-run warning throttles, at the start of an indexing run.
pub fn reset_run_warnings() {
    HASH_FAILURES.reset();
}

/// How many files could not be hashed this run, and how many went unlogged.
pub fn hash_failure_counts() -> (u64, u64) {
    (HASH_FAILURES.seen(), HASH_FAILURES.suppressed())
}

/// Build the `files` row for one on-disk file from a `stat` the caller
/// already holds. Returns `None` for anything that isn't a readable regular
/// file.
///
/// `path` must be canonical, in stored spelling, and must still name the file
/// once parsed back into a [`Path`], since this opens it by that string; a
/// lossy spelling does not qualify (see `path_to_db_string`). `scratch` holds
/// the head buffer this reads into, reused across every file a walk worker
/// handles.
pub fn prepare_file_record(
    path: &str,
    meta: &std::fs::Metadata,
    config: &Config,
    registry: &Registry,
    scratch: &mut Scratch,
) -> Option<OwnedNewFile> {
    if !meta.is_file() {
        return None;
    }

    let size = meta.len();
    let mtime = mtime_secs(meta)?;

    // A dehydrated cloud file is indexed from its metadata alone: reading
    // even the first byte would block on downloading the whole file.
    let dehydrated = crate::platform::is_cloud_placeholder(meta);

    let hash = if dehydrated {
        // No head either: an empty buffer sniffs to the extension's answer,
        // which is all a placeholder can be classified by.
        scratch.head_buffer().clear();
        None
    } else {
        match get_file_hash(
            size,
            Path::new(path),
            config.processing.hash_length,
            scratch.head_buffer(),
        ) {
            Ok(hash) => Some(hash),
            Err(e) => {
                // Throttled: on Windows a file another process holds open
                // fails here as a matter of course.
                if HASH_FAILURES.allow() {
                    crate::log_warn!("Skipping file (cannot hash) {}: {}", path, e);
                }
                return None;
            }
        }
    };

    let (parent, name) = split_db_path(path)?;
    let row_path = repo::RowPath::new(parent, name);
    let head = scratch.head();
    let mime = guess_mime_from_head(Path::new(path), head);
    let ftype = mime.map(mime_to_type).unwrap_or(FileType::EMPTY);

    let needs_content = !dehydrated
        && size <= config.processing.maximum_text_file_size
        && content_extractable(Path::new(path), mime, config, registry);

    // When the head is the whole file, an extractor that works from bytes can
    // finish the job now; otherwise the file stays pending. Size 0 is
    // excluded: procfs, sysfs and some FUSE mounts report it for files that
    // do have content. A failure, including a panic the registry contained,
    // also stays pending: recording one needs a file id the walk does not
    // have.
    let inline_text = mime
        .filter(|_| needs_content && size != 0 && size <= config.processing.hash_length as u64)
        .and_then(|m| {
            let mut text = String::new();
            registry
                .extract_complete_head(Path::new(path), m, head, &mut text)?
                .ok()?;
            safe_truncate(&mut text, config.processing.maximum_text_size);
            Some(text)
        });

    Some(OwnedNewFile {
        path: row_path,
        size,
        mtime,
        mime,
        ftype,
        hash,
        inline_text,
        needs_content,
    })
}

/// Extract content for one pending file and record the outcome on its row
/// ([`repo::complete_pending`]). `mime` is authoritative, including when it
/// is `None`: the head was already sniffed by [`prepare_file_record`], and
/// re-sniffing gives the same answer.
pub fn extract_and_store(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> Result<(), String> {
    // One file, from the watcher or the CLI: no loop for a reused scratch to
    // amortize over.
    let mut scratch = Scratch::new(config);
    let outcome = decide_content(path, mime, registry, config, &mut scratch);
    let zstd = match outcome_body(&outcome) {
        Some(text) => repo::encode_one(text)?,
        None => None,
    };
    repo::complete_pending(tx, file_id, &outcome, zstd.as_deref()).map(|_| ())
}

/// What should be written for one file's content, decided without touching
/// the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentOutcome {
    /// Text, already truncated to `maximum_text_size`.
    Done {
        text: String,
    },
    NotApplicable,
    Failed(String),
}

/// Whether extraction will produce anything for this file. The single
/// predicate behind both the `content_state` a row is born with and
/// [`decide_content`]'s early-out: if they disagreed, a file the walk wrote
/// off as NA would silently never be full-text indexed.
pub fn content_extractable(
    path: &Path,
    mime: Option<&str>,
    config: &Config,
    registry: &Registry,
) -> bool {
    crate::config::content_allowed(path, config) && mime.is_some_and(|m| registry.supports(m))
}

/// Read `path` and decide what its content row should say: the expensive
/// half, with no database access and no locks held.
///
/// `scratch` is the calling worker's, reused for every file it handles; the
/// text is a fresh `String` because it goes on to cross a channel.
pub fn decide_content(
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
    scratch: &mut Scratch,
) -> ContentOutcome {
    let p = Path::new(path);
    let Some(m) = mime.filter(|_| content_extractable(p, mime, config, registry)) else {
        return ContentOutcome::NotApplicable;
    };
    let mut text = String::new();
    // A panicking parser is contained by the registry and arrives as `Err`,
    // which becomes this row's recorded failure reason, not a dead worker.
    match registry.extract(p, m, &mut text, scratch) {
        Ok(true) => {
            // Extractors stop at the limit themselves; this is the backstop
            // for the ones that can overshoot by a run or a slide.
            safe_truncate(&mut text, config.processing.maximum_text_size);
            ContentOutcome::Done { text }
        }
        Ok(false) => ContentOutcome::NotApplicable,
        Err(reason) => ContentOutcome::Failed(reason),
    }
}

/// The body a [`ContentOutcome`] would store, if any.
pub fn outcome_body(outcome: &ContentOutcome) -> Option<&str> {
    match outcome {
        ContentOutcome::Done { text } => Some(text),
        _ => None,
    }
}
