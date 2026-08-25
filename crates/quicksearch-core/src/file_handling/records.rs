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

/// Decide what Phase 1 should do with a file.
/// A file whose parent is *not* this directory — a resolved symlink target —
/// must not be classified here: it would read as `Insert` and `INSERT OR
/// IGNORE` would silently not update it. Use [`classify_by_mtime`] for those.
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

/// Truncate to at most `max_bytes`, backing up to a UTF-8 char boundary.
///
/// In place: this runs on documents up to `maximum_text_size`, and building
/// the prefix as a second `String` allocated and copied the whole thing only
/// to drop the original a line later.
pub(crate) fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }

    s.truncate(end);
}

/// A SHA-256 digest. An array, not a `Vec`: it is a fixed 32 bytes, it is
/// produced once per new or changed file, and a heap allocation apiece is
/// per-file allocator traffic for something that fits in a register pair's
/// worth of stack.
pub type FileHash = [u8; 32];

/// Identify a file as `sha256(size || first hash_length bytes)`, leaving the
/// head bytes in `head` for MIME sniffing. The cost is a collision class —
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

/// FTS5's automerge threshold (2..=16). 16 measured ~19% faster cold indexing
/// and a third fewer bytes written for no search cost; don't retune casually.
const WRITE_AUTOMERGE: u8 = 16;

/// Set FTS5's automerge threshold. Best-effort; failure is logged.
///
/// **This sets a parameter — it does not merge anything.** The merging
/// command is `'merge'`; see [`fts_finalize_after_text_indexing`]. It must
/// run *before* the bulk load: called only at the end of a run, it affects
/// nothing.
pub fn fts_set_automerge(conn: &Connection, segments: u8) {
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('automerge', ?1)",
        [segments as i64],
    ) {
        crate::log_warn!("FTS automerge failed (non-fatal): {}", e);
    }
}

/// FTS5's crisis-merge threshold (its default is 16). 32 measured a 13%
/// smaller, more build-stable index for no cost; 64 measured identically.
const WRITE_CRISISMERGE: u8 = 32;

/// Apply the write-side FTS5 settings, before a run starts writing.
///
/// `pgsz` is deliberately absent: it is not a per-run setting. Sweeping it
/// *unencrypted* only ever wrote more bytes for an index the same size, so the
/// default 4050 stands there. Keyed is the opposite — SQLCipher's page reserve
/// makes 4050 a cliff — and that case is handled once at schema creation; see
/// [`crate::db::schema::FTS_PGSZ_ENCRYPTED`].
pub fn fts_begin_bulk_write(conn: &Connection) {
    fts_set_automerge(conn, WRITE_AUTOMERGE);
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('crisismerge', ?1)",
        [WRITE_CRISISMERGE as i64],
    ) {
        crate::log_warn!("FTS crisismerge failed (non-fatal): {}", e);
    }
}

/// Output leaf pages [`fts_finalize_after_text_indexing`] may write.
///
/// FTS5's own `FTS5_OPT_WORK_UNIT`, which is the budget it gives one step of a
/// real `'optimize'`. **Unswept** — the other two constants here carry measured
/// tables and this one does not yet; it is a starting point chosen to match
/// SQLite's own unit of merge work, not a tuned figure.
///
/// The budget is not a hard ceiling. `fts5IndexMergeLevel` only tests it at a
/// term boundary, so one term with a very long doclist writes past it; that is
/// the shape to expect if this ever needs raising or lowering.
const FINALIZE_MERGE_PAGES: i64 = 1000;

/// Merge FTS5 segments once a run has finished writing — this is what
/// reclaims the tombstones a `contentless_delete` table accumulates.
/// Best-effort; an unconsolidated index is still correct.
///
/// **The sign of the argument picks a different algorithm**, which is worth
/// spelling out because reading it as a plain page budget cost a release. From
/// `sqlite3Fts5IndexMerge` in the amalgamation:
///
/// - **Positive** `N`: ordinary consolidation. Merges any level holding at
///   least `usermerge` segments (FTS5's default 4 — we set `automerge` and
///   `crisismerge`, never `usermerge`), for up to `N` output leaf pages.
/// - **Negative** `N`: `fts5IndexOptimizeStruct` with `nMin` forced to 1 —
///   that is `'optimize'`, hoisting every segment in the table into a single
///   level, merely rate-limited to `|N|` pages per call. It is built to be
///   called in a loop until `changes() == 0`. Called *once*, it leaves the
///   structure permanently mid-optimize, and that persists in `%_data`.
///
/// So this takes the positive form. Tombstone reclamation survives the switch:
/// `fts5IndexMerge` falls through to `fts5IndexFindDeleteMerge` when no level
/// has `nMin` segments, and that path keys off `deletemerge` whatever the sign.
pub fn fts_finalize_after_text_indexing(conn: &Connection) {
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('merge', ?1)",
        [FINALIZE_MERGE_PAGES],
    ) {
        crate::log_warn!("FTS merge failed (non-fatal): {}", e);
    }
}

/// An owned, fully-derived file record, produced by [`prepare_file_record`].
#[derive(Debug, Clone)]
pub struct OwnedNewFile {
    pub name: String,
    /// The containing directory, ending in the platform separator.
    pub parent: String,
    pub size: u64,
    pub mtime: u64,
    /// Borrowed from the static tables [`guess_mime_from_head`] answers out
    /// of; nothing here ever owns a MIME string.
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
    pub fn path(&self) -> String {
        format!("{}{}", self.parent, self.name)
    }

    pub fn as_new_file(&self) -> NewFile<'_> {
        NewFile {
            name: &self.name,
            parent: &self.parent,
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
/// once parsed back into a [`Path`] — this opens it by that string. A path
/// that only survived `to_string_lossy` does not qualify: the lossy spelling
/// of one name is the real name of another, so it would hash and index the
/// wrong file.
/// `scratch` holds the head buffer this reads into, reused across every file
/// a walk worker handles.
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
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())?;

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
    let (parent, name) = (parent.to_string(), name.to_string());
    let head = scratch.head();
    let mime = guess_mime_from_head(Path::new(path), head);
    let ftype = mime.map(mime_to_type).unwrap_or(FileType::EMPTY);

    let needs_content = !dehydrated
        && size <= config.processing.maximum_text_file_size
        && content_extractable(Path::new(path), mime, config, registry);

    // When the head is the whole file, an extractor that works from bytes can
    // finish the job now; otherwise the file stays pending.
    let inline_text = mime.filter(|_| needs_content).and_then(|m| {
        // Size 0 is excluded: procfs, sysfs and some FUSE mounts report it
        // for files that do have content, and inlining would store empty
        // text for them.
        if size == 0 || size > config.processing.hash_length as u64 {
            return None;
        }
        let mut text = String::new();
        // A panicking parser arrives here as `Some(Err(..))` — contained by
        // the registry, which is what keeps a walk worker alive.
        match registry.extract_complete_head(Path::new(path), m, head, &mut text) {
            Some(Ok(())) => {
                safe_truncate(&mut text, config.processing.maximum_text_size);
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

/// [`prepare_file_record`] for a path not yet resolved — the watcher path.
/// One file at a time, so it owns the scratch rather than being handed one.
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
    let mut scratch = Scratch::new(config);
    prepare_file_record(&db_path, &meta, config, registry, &mut scratch)
}

/// Extract content for one file and record the outcome on its row. `mime` is
/// authoritative, including when it is `None`: the head was already sniffed
/// by [`prepare_file_record`], and re-sniffing gives the same answer.
pub fn extract_and_store(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> Result<(), String> {
    // One file, called from the watcher and the CLI: a scratch per call is
    // the right scope — there is no loop for a reused one to amortize over.
    let mut scratch = Scratch::new(config);
    let outcome = decide_content(path, mime, registry, config, &mut scratch);
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

/// Read `path` and decide what its content row should say. No database
/// access, no locks held — this is the expensive half.
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
    if !content_extractable(p, mime, config, registry) {
        return ContentOutcome::NotApplicable;
    }
    let mut text = String::new();
    // A panicking parser is contained by the registry and arrives as `Err`,
    // which becomes this row's recorded failure reason, not a dead worker.
    let result = match mime {
        Some(m) => registry.extract(p, m, &mut text, scratch),
        None => Ok(false),
    };
    match result {
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

/// Apply a decision from [`decide_content`] — the cheap half, all that runs
/// with the connection held. `text_zstd`: see [`repo::set_content_done`].
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

/// The body a [`ContentOutcome`] would store, if any.
pub fn outcome_body(outcome: &ContentOutcome) -> Option<&str> {
    match outcome {
        ContentOutcome::Done { text } => Some(text),
        _ => None,
    }
}
