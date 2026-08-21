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
use crate::extract::Registry;
use crate::mime::{guess_mime_from_head, mime_to_type, FileType};

/// One directory's indexed files, as `name -> mtime`. Produced by
/// [`crate::db::repo::dir_rows`].
pub type DirRows = HashMap<String, u64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIndexAction {
    Skip,
    Update,
    Insert,
}

/// Decide what Phase 1 should do with a file, given its name within the
/// directory being walked and the file's mtime.
///
/// A file whose parent is *not* this directory — a resolved symlink target —
/// must not be classified here: it would miss, read as
/// [`FileIndexAction::Insert`], and `insert_file`'s `INSERT OR IGNORE` would
/// then silently not update it. Use [`classify_by_mtime`] for those.
pub fn classify_for_indexing(name: &str, mtime: u64, rows: &DirRows) -> FileIndexAction {
    classify_by_mtime(rows.get(name).copied(), mtime)
}

/// The same decision from an already-resolved stored mtime; used directly for
/// resolved symlink targets.
pub fn classify_by_mtime(stored: Option<u64>, mtime: u64) -> FileIndexAction {
    match stored {
        Some(known) if known == mtime => FileIndexAction::Skip,
        Some(_) => FileIndexAction::Update,
        None => FileIndexAction::Insert,
    }
}

/// Truncate to at most `max_bytes` bytes, backing up to a UTF-8 character
/// boundary. Cuts short of the budget rather than over it.
fn safe_truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }

    s[..end].to_string()
}

/// Identify a file as `sha256(size || first hash_length bytes)`, returning
/// the head bytes alongside the digest so the caller can sniff a MIME type
/// without opening the file again.
///
/// The cost is a collision class: two files of identical size whose first
/// `hash_length` bytes match hash identically — pre-allocated VM disk images
/// (fixed-size VHDs keep their unique footer at the *end*; fresh raw/qcow2
/// images are zeros at the head) are reported as duplicates when they are
/// not. Duplicate listing is advisory, so this is a display artifact.
pub(super) fn get_file_hash(
    size: u64,
    path: &Path,
    hash_length: usize,
) -> Result<(Vec<u8>, Vec<u8>), std::io::Error> {
    // The caller's `is_file()` came from a `stat` taken before this open; a
    // FIFO renamed over the name in between would block this walk worker
    // forever and park the pool behind it. See `platform::open_regular_file`.
    let mut f: File = crate::platform::open_regular_file(path)?;
    // Files shorter than the window hash whole; `min` keeps the cast sound
    // for large files on 32-bit targets.
    let mut head = vec![0u8; size.min(hash_length as u64) as usize];
    f.read_exact(&mut head)?;

    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());
    hasher.update(&head);
    Ok((hasher.finalize().to_vec(), head))
}

/// FTS5's automerge threshold, applied before a run writes anything.
///
/// The number of index segments that must accumulate at one level before FTS5
/// merges them: 2..=16, or 0 to disable incremental merging. Every merge
/// rewrites segments to disk, so this is a **write-amplification** knob rather
/// than a CPU one, and the FTS index is where essentially all of an indexing
/// run's writing goes — `searchabletext_data` measured 228 MiB of a 265 MiB
/// index, against 2.8 MiB for all six `files` indexes put together.
///
/// 16 is the maximum FTS5 accepts and is measured, on a 10,000-file tree, three
/// builds per setting:
///
/// | automerge | cold index | written | search (unlimited) |
/// |---:|---:|---:|---:|
/// | 4 (FTS5 default) | 8.53 / 8.65 / 8.58 s | 1862 / 1890 / 1818 MiB | 763–772 ms |
/// | 16 | 7.10 / 7.10 / 6.67 s | 1235 / 1221 / 1213 MiB | 756–769 ms |
///
/// **19% faster and a third fewer bytes written, for no search cost.** The
/// search column has to be read carefully, and is the reason this comment
/// exists: measured at the *default* display limit the same six indexes span
/// 40 ms to 540 ms, and none of that spread is automerge. `scan_pass` stops as
/// soon as the limit fills and streams FTS candidates in rowid order — which is
/// `file_id` order, which is whatever order the concurrent walk inserted rows
/// in — so an index where the large documents drew low ids decompresses
/// megabytes to fill 1000 hits where another reads a few hundred kilobytes.
/// Two builds of one configuration differed by 13x that way. Comparing
/// anything about FTS against a limited search measures that lottery instead;
/// raise the limit past the corpus so every index examines every candidate.
const WRITE_AUTOMERGE: u8 = 16;

/// Set FTS5's automerge threshold. Best-effort; failure is logged.
///
/// **This sets a parameter — it does not merge anything.** With a value bound
/// to `rank`, `INSERT INTO ft(ft, rank) VALUES('automerge', N)` writes N into
/// the table's `%_config`, where it persists. The merging command is
/// `'merge'`, and the merge-everything command is `'optimize'`; see
/// [`fts_finalize_after_text_indexing`].
///
/// That distinction was worth a great deal. This used to be called once, at the
/// *end* of a run, under the name `fts_finalize_after_text_indexing` and the
/// comment "nudge FTS5 to merge its index segments" — so a fresh index did its
/// entire first bulk load at FTS5's default threshold of 4, every later run
/// silently inherited 8 from the config table, and no merge was ever performed
/// at all.
pub fn fts_set_automerge(conn: &Connection, segments: u8) {
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('automerge', ?1)",
        [segments as i64],
    ) {
        crate::log_warn!("FTS automerge failed (non-fatal): {}", e);
    }
}

/// FTS5's crisis-merge threshold: the segment count at which it stops deferring
/// and *forces* a merge, whatever `automerge` would have preferred.
///
/// Raised from FTS5's default of 16 to 32 — half way to the 64 that measured
/// identically, so the safety valve this is stays nearer where SQLite put it.
/// Two builds each, 10,000-file tree, everything else equal:
///
/// | crisismerge | index | written | cold | search (unlimited) |
/// |---:|---:|---:|---:|---:|
/// | 16 (default) | 311.8 / 289.9 MiB | 1008 / 973 MiB | 5.54 / 5.51 s | 755–769 ms |
/// | 32 | **268.2 / 266.1 MiB** | 946 / 964 MiB | 5.53 / 5.52 s | 756–762 ms |
///
/// A **13% smaller index for no cost in time or search**, and — the part worth
/// noticing — a far more *stable* one: the default's size swings 290–312 MiB
/// between builds where this lands within 2 MiB of itself. Fewer forced merges
/// mid-load leave the final merge a tidier structure to consolidate.
const WRITE_CRISISMERGE: u8 = 32;

/// Apply the write-side FTS5 settings, before a run starts writing.
///
/// `pgsz` was swept here too and **rejected**: at 8192 and 16384 it wrote
/// 1061 MiB and 1008 MiB against the default's 943 MiB, for an index the same
/// size. It is a runtime option like these two — settable on an existing table,
/// not creation-time-only — so trying it cost nothing and needed no schema
/// change; it simply does not pay on this workload.
pub fn fts_begin_bulk_write(conn: &Connection) {
    fts_set_automerge(conn, WRITE_AUTOMERGE);
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('crisismerge', ?1)",
        [WRITE_CRISISMERGE as i64],
    ) {
        crate::log_warn!("FTS crisismerge failed (non-fatal): {}", e);
    }
}

/// Merge FTS5 segments once a run has finished writing.
///
/// A real merge, which is what this function's name has always claimed and what
/// it never did. Cheap next to the load — measured at +0.2 s and +5 MiB on a
/// 20,000-file tree — and it is what reclaims the tombstones a
/// `contentless_delete` table accumulates, which is why the incremental callers
/// (`scope`, `cleanup_stale_index_entries`) want it after removing rows.
///
/// Deliberately **not** `'optimize'`. That merges everything into one segment
/// and costs, on the same tree, +1.8 s and +900 MiB written — for no measurable
/// search gain: it took the segment-index from 7,337 rows to 730 and left an
/// unlimited search within noise of where it started.
///
/// Best-effort; failure is logged and swallowed, because an unconsolidated
/// index is slower to search and still correct.
pub fn fts_finalize_after_text_indexing(conn: &Connection) {
    // A negative page budget means "keep merging until there is nothing left
    // worth merging", rather than doing a fixed slice of the work.
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('merge', -16)",
        [],
    ) {
        crate::log_warn!("FTS merge failed (non-fatal): {}", e);
    }
}

/// An owned, fully-derived file record: everything needed to insert or
/// update a `files` row, produced by [`prepare_file_record`].
#[derive(Debug, Clone)]
pub struct OwnedNewFile {
    pub name: String,
    /// The containing directory, ending in the platform separator. With
    /// [`OwnedNewFile::name`] it is both the row's key and, concatenated, its
    /// path — see [`super::paths::dir_to_db_parent`].
    pub parent: String,
    pub size: u64,
    pub mtime: u64,
    pub mime: Option<String>,
    pub ftype: FileType,
    /// `None` only for a dehydrated cloud placeholder. Stored as SQL NULL,
    /// which keeps such files out of duplicate detection: an empty or zero
    /// hash would make every one of them look identical.
    pub hash: Option<Vec<u8>>,
    /// Text extracted from the head bytes during the walk, for files small
    /// enough that the head *was* the whole file. `Some` means the content
    /// pass never has to open this file; `None` leaves it pending.
    pub inline_text: Option<String>,
    /// Whether the content pass has anything to do for this file — see
    /// [`content_extractable`], plus the `maximum_text_file_size` gate.
    /// `false` means the row is born `STATE_NA`.
    pub needs_content: bool,
}

impl OwnedNewFile {
    /// The file's path, rebuilt. Callers that only want it for a message
    /// should say so — nothing stores this.
    pub fn path(&self) -> String {
        format!("{}{}", self.parent, self.name)
    }

    pub fn as_new_file(&self) -> NewFile<'_> {
        NewFile {
            name: &self.name,
            parent: &self.parent,
            size: self.size,
            mtime: self.mtime,
            mime: self.mime.as_deref(),
            ftype: self.ftype,
            hash: self.hash.as_deref(),
            needs_content: self.needs_content,
        }
    }
}

/// Individual "cannot hash" warnings allowed per run before only the count is
/// kept. See [`crate::log::Throttle`]; [`reset_run_warnings`] arms it.
static HASH_FAILURES: crate::log::Throttle = crate::log::Throttle::new(20);

/// Arm the per-run warning throttles; called once at the start of an
/// indexing run.
pub fn reset_run_warnings() {
    HASH_FAILURES.reset();
}

/// How many files could not be hashed this run, and how many of those went
/// unlogged. `(0, 0)` when nothing failed.
pub fn hash_failure_counts() -> (u64, u64) {
    (HASH_FAILURES.seen(), HASH_FAILURES.suppressed())
}

/// Build the `files` row for one on-disk file from a `stat` the caller
/// already holds. The single implementation behind both full-run batches
/// and incremental watcher updates.
///
/// `path` must already be canonical and in stored spelling (see
/// [`path_to_db_string`]), and must still name the file once parsed back into
/// a [`Path`] — this opens it by that string. A path that only survived
/// `to_string_lossy` does not qualify: the lossy spelling of one name is the
/// real name of another, so it would hash and index the wrong file. The walk
/// screens for that on the directory entry, before the path is even built
/// (`crate::walk::read_directory`); [`prepare_file_record_from_path`], which
/// is what callers holding an unresolved path want, screens with
/// [`warn_if_unrepresentable`].
///
/// Returns `None` for anything that isn't a readable regular file, with a
/// warning when hashing fails.
pub fn prepare_file_record(
    path: &str,
    meta: &std::fs::Metadata,
    config: &Config,
    registry: &Registry,
) -> Option<OwnedNewFile> {
    if !meta.is_file() {
        return None;
    }

    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())?;

    // A dehydrated cloud file is indexed from its metadata alone: reading even
    // the first byte would block on downloading the whole file, so no hash, no
    // MIME sniff, no inline extraction. Hydration does not change the mtime,
    // so a later run picks it up once the attribute clears.
    let dehydrated = crate::platform::is_cloud_placeholder(meta);

    let (hash, head) = if dehydrated {
        (None, Vec::new())
    } else {
        match get_file_hash(size, Path::new(path), config.processing.hash_length) {
            Ok((hash, head)) => (Some(hash), head),
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

    // One split for both halves of the key, and the separator stays with the
    // parent so the two concatenate back into `path`. `None` here means the
    // caller handed us something that cannot be a file's path at all.
    let (parent, name) = split_db_path(path)?;
    let (parent, name) = (parent.to_string(), name.to_string());
    // Sniff from the bytes hashing already read; an empty head falls back to
    // the extension.
    let mime = guess_mime_from_head(Path::new(path), &head);
    let ftype = mime.as_deref().map(mime_to_type).unwrap_or(FileType::EMPTY);

    let needs_content = !dehydrated
        && size <= config.processing.maximum_text_file_size
        && content_extractable(Path::new(path), mime.as_deref(), config, registry);

    // When the head is the whole file, an extractor that works from bytes can
    // finish the job now; any condition that does not hold leaves this `None`
    // and the file stays pending.
    let inline_text = mime.as_deref().filter(|_| needs_content).and_then(|m| {
        // Size 0 is excluded: procfs, sysfs and some FUSE mounts report it
        // for files that do have content, and inlining would store empty
        // text for them.
        if size == 0 || size > config.processing.hash_length as u64 {
            return None;
        }
        // A panicking parser arrives here as `Some(Err(..))` — contained by
        // the registry, which is what keeps a walk worker alive. See
        // `Registry::extract`.
        match registry.extract_complete_head(Path::new(path), m, &head) {
            Some(Ok(content)) => {
                let mut text = content.text;
                if text.len() > config.processing.maximum_text_size {
                    text = safe_truncate_string(&text, config.processing.maximum_text_size);
                }
                Some(text)
            }
            // Recording a failure needs a file id the walk does not have;
            // leaving it pending keeps failure reporting in one place.
            Some(Err(_)) | None => None,
        }
    });

    Some(OwnedNewFile {
        name,
        parent,
        size,
        mtime,
        mime,
        ftype,
        hash,
        inline_text,
        needs_content,
    })
}

/// [`prepare_file_record`] for a path that has not been resolved yet — the
/// watcher path.
pub fn prepare_file_record_from_path(
    path: &Path,
    config: &Config,
    registry: &Registry,
) -> Option<OwnedNewFile> {
    let canonical = path.canonicalize().ok()?;
    if warn_if_unrepresentable(&canonical) {
        return None;
    }
    let db_path = path_to_db_string(&canonical);
    let meta = std::fs::metadata(&canonical).ok()?;
    prepare_file_record(&db_path, &meta, config, registry)
}

/// Extract content for one file and record the outcome on its row: text on
/// success, `NA` when no extractor applies or the `content_extensions`
/// filter excludes it, `FAILED` with a reason on extractor errors. The single implementation behind the full text-index
/// pass and incremental updates.
///
/// `mime` is authoritative, including when it is `None`: the head was already
/// sniffed by [`prepare_file_record`], and re-sniffing gives the same answer.
pub fn extract_and_store(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> Result<(), String> {
    let outcome = decide_content(path, mime, registry, config);
    let zstd = match outcome_body(&outcome) {
        Some(text) => repo::encode_one(text, config.processing.store_text_for_snippets)?,
        None => None,
    };
    store_content_outcome(tx, file_id, &outcome, zstd.as_deref())
}

/// What should be written for one file's content, decided without touching
/// the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentOutcome {
    /// Text, already truncated to `maximum_text_size`.
    Done { text: String },
    /// No extractor claims the MIME, or `content_extensions` excludes it.
    NotApplicable,
    /// The extractor ran and failed; the reason goes on the row.
    Failed(String),
}

/// Whether extraction will produce anything for this file: the
/// `content_extensions` filter allows it, and some extractor claims its MIME.
///
/// The single predicate behind both the `content_state` a row is born with
/// and [`decide_content`]'s not-applicable early-out: if they disagreed, a
/// file the walk wrote off as NA would silently never be full-text indexed.
/// Size-free — the `maximum_text_file_size` gate lives in the feeder's query.
pub fn content_extractable(
    path: &Path,
    mime: Option<&str>,
    config: &Config,
    registry: &Registry,
) -> bool {
    crate::config::content_allowed(path, config) && mime.is_some_and(|m| registry.supports(m))
}

/// Read `path` and decide what its content row should say. No database access,
/// no locks held — this is the expensive half.
///
/// `mime` is authoritative, including when it is `None`: see
/// [`extract_and_store`].
pub fn decide_content(
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> ContentOutcome {
    let p = Path::new(path);
    if !content_extractable(p, mime, config, registry) {
        return ContentOutcome::NotApplicable;
    }
    // `content_extractable` established that an extractor claims this MIME,
    // so the `Ok(None)` arm below is unreachable. A panicking parser is
    // contained by the registry and arrives as `Err`, which becomes this
    // row's recorded failure reason rather than a dead worker.
    let result = match mime {
        Some(m) => registry.extract(p, m),
        None => Ok(None),
    };
    match result {
        Ok(Some(mut content)) => {
            if content.text.len() > config.processing.maximum_text_size {
                content.text =
                    safe_truncate_string(&content.text, config.processing.maximum_text_size);
            }
            ContentOutcome::Done { text: content.text }
        }
        Ok(None) => ContentOutcome::NotApplicable,
        Err(reason) => ContentOutcome::Failed(reason),
    }
}

/// Apply a decision from [`decide_content`]. The cheap half: pure database
/// writes, so this is all that runs with the connection held.
///
/// `text_zstd` is the compressed body for a `Done` outcome, prepared by the
/// caller before it took the lock — see [`repo::set_content_done`].
pub fn store_content_outcome(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    outcome: &ContentOutcome,
    text_zstd: Option<&[u8]>,
) -> Result<(), String> {
    match outcome {
        ContentOutcome::Done { text } => repo::set_content_done(tx, file_id, text, text_zstd),
        ContentOutcome::NotApplicable => repo::set_content_na(tx, file_id),
        ContentOutcome::Failed(reason) => repo::set_content_failed(tx, file_id, reason),
    }
}

/// The body a [`ContentOutcome`] would store, if any — what the caller feeds
/// to its [`repo::DocEncoder`] ahead of the lock.
pub fn outcome_body(outcome: &ContentOutcome) -> Option<&str> {
    match outcome {
        ContentOutcome::Done { text } => Some(text),
        _ => None,
    }
}
