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
/// the head bytes alongside the digest for MIME sniffing. The cost is a
/// collision class — same-size files with identical heads read as duplicates
/// (`examples/hashprobe.rs` has the study); `crate::verify` is the way out.
pub fn get_file_hash(
    size: u64,
    path: &Path,
    hash_length: usize,
) -> Result<(Vec<u8>, Vec<u8>), std::io::Error> {
    // The caller's `is_file()` came from a `stat` taken before this open; a
    // FIFO renamed over the name in between would block this walk worker
    // forever and park the pool behind it. See `platform::open_regular_file`.
    let mut f: File = crate::platform::open_regular_file(path)?;
    let mut head = vec![0u8; size.min(hash_length as u64) as usize];
    f.read_exact(&mut head)?;

    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());
    hasher.update(&head);
    Ok((hasher.finalize().to_vec(), head))
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
/// `pgsz` was swept here too and **rejected**: more bytes written for an
/// index the same size.
pub fn fts_begin_bulk_write(conn: &Connection) {
    fts_set_automerge(conn, WRITE_AUTOMERGE);
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('crisismerge', ?1)",
        [WRITE_CRISISMERGE as i64],
    ) {
        crate::log_warn!("FTS crisismerge failed (non-fatal): {}", e);
    }
}

/// Merge FTS5 segments once a run has finished writing — this is what
/// reclaims the tombstones a `contentless_delete` table accumulates.
/// Deliberately **not** `'optimize'`: measured, far more time and writes for
/// no search gain. Best-effort; an unconsolidated index is still correct.
pub fn fts_finalize_after_text_indexing(conn: &Connection) {
    // A negative page budget means "keep merging until nothing is left worth
    // merging".
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('merge', -16)",
        [],
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
            mime: self.mime.as_deref(),
            ftype: self.ftype,
            hash: self.hash.as_deref(),
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

    // A dehydrated cloud file is indexed from its metadata alone: reading
    // even the first byte would block on downloading the whole file.
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

    let (parent, name) = split_db_path(path)?;
    let (parent, name) = (parent.to_string(), name.to_string());
    let mime = guess_mime_from_head(Path::new(path), &head);
    let ftype = mime.as_deref().map(mime_to_type).unwrap_or(FileType::EMPTY);

    let needs_content = !dehydrated
        && size <= config.processing.maximum_text_file_size
        && content_extractable(Path::new(path), mime.as_deref(), config, registry);

    // When the head is the whole file, an extractor that works from bytes can
    // finish the job now; otherwise the file stays pending.
    let inline_text = mime.as_deref().filter(|_| needs_content).and_then(|m| {
        // Size 0 is excluded: procfs, sysfs and some FUSE mounts report it
        // for files that do have content, and inlining would store empty
        // text for them.
        if size == 0 || size > config.processing.hash_length as u64 {
            return None;
        }
        // A panicking parser arrives here as `Some(Err(..))` — contained by
        // the registry, which is what keeps a walk worker alive.
        match registry.extract_complete_head(Path::new(path), m, &head) {
            Some(Ok(mut text)) => {
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

/// [`prepare_file_record`] for a path not yet resolved — the watcher path.
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
    // A panicking parser is contained by the registry and arrives as `Err`,
    // which becomes this row's recorded failure reason, not a dead worker.
    let result = match mime {
        Some(m) => registry.extract(p, m),
        None => Ok(None),
    };
    match result {
        Ok(Some(mut text)) => {
            if text.len() > config.processing.maximum_text_size {
                text = safe_truncate_string(&text, config.processing.maximum_text_size);
            }
            ContentOutcome::Done { text }
        }
        Ok(None) => ContentOutcome::NotApplicable,
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
