//! Batched DB writes: the insert/update paths every walk funnels into,
//! stale-row cleanup, and the extraction cursor/scope bookkeeping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::*;
use crate::config::Config;
use crate::db::repo::{self};

/// One writer's compressed sidecars: a batch's blobs end to end in `arena`,
/// with `slots[i]` saying where row `i`'s is — or that it has none, or that
/// its compression failed (kept per row rather than failing the batch).
///
/// Everything here is reused across chunks. The encoder because building a
/// zstd context per chunk is wasted CPU (`benches/index.rs`, `zstd_encode`);
/// the arena because a `Vec` per row was one allocation per indexed
/// document, and the writer sees every one of them.
struct Bodies {
    enc: repo::DocEncoder,
    arena: Vec<u8>,
    slots: Vec<Result<Option<std::ops::Range<usize>>, String>>,
}

impl Bodies {
    fn new() -> Result<Bodies, String> {
        Ok(Bodies {
            enc: repo::DocEncoder::new()?,
            arena: Vec::new(),
            slots: Vec::new(),
        })
    }

    /// Compress one chunk's bodies, **before the caller takes the
    /// connection**: the lock covers only the SQL.
    fn fill<'a>(&mut self, texts: impl Iterator<Item = Option<&'a str>>, config: &Config) {
        self.arena.clear();
        self.slots.clear();
        for text in texts {
            let slot = match text {
                Some(t) if config.processing.store_text_for_snippets && !t.is_empty() => {
                    self.enc.encode_into(t, &mut self.arena).map(Some)
                }
                _ => Ok(None),
            };
            self.slots.push(slot);
        }
    }

    /// Row `i`'s blob, or why there is none.
    fn get(&self, i: usize) -> Result<Option<&[u8]>, &str> {
        match &self.slots[i] {
            Ok(Some(at)) => Ok(Some(&self.arena[at.clone()])),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The sidecar blob for row `i`, or a logged skip if its compression failed.
macro_rules! body_or_skip {
    ($bodies:expr, $i:expr, $what:expr) => {
        match $bodies.get($i) {
            Ok(b) => b,
            Err(e) => {
                crate::log_warn!("compress text for {}: {}", $what, e);
                continue;
            }
        }
    };
}

/// The skeleton behind [`process_batch_updates`] and
/// [`process_batch_inserts`]: compress each chunk's bodies outside the lock,
/// write its rows in one transaction through `write_row`, and store any
/// inline text on a fresh row.
///
/// `_fresh` is sound here because both row writers leave a row that
/// **provably holds no content**: see their comments at the call sites.
fn write_prepared_records(
    conn_mutex: &Arc<Mutex<Connection>>,
    records: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
    chunk_size: usize,
    write_row: impl Fn(&rusqlite::Transaction<'_>, &OwnedNewFile) -> Result<Option<i64>, String>,
) -> Result<(), String> {
    // One set of buffers for every chunk this call writes.
    let mut bodies = Bodies::new()?;
    for batch in records.chunks(chunk_size) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Outside the lock — see `Bodies::fill`.
        bodies.fill(batch.iter().map(|r| r.inline_text.as_deref()), config);
        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, rec) in batch.iter().enumerate() {
            if stop_flag.load(Ordering::Relaxed) {
                drop(tx);
                drop(conn);
                return Ok(());
            }
            let id = write_row(&tx, rec)?;
            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                let zstd = body_or_skip!(bodies, i, rec.path());
                repo::set_content_done_fresh(&tx, id, text, zstd)?;
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Write already-prepared records for files whose content changed. No
/// filesystem I/O; records carrying inline text are stored complete here,
/// the rest stay pending.
pub fn process_batch_updates(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_update: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<(), String> {
    if files_to_update.is_empty() {
        return Ok(());
    }

    let fts_batch = config.processing.fts_update_batch_size.max(1);

    // The row leaves this closure `_fresh`: `update_file_basic` cleared its
    // content in this same transaction, and the insert fallback created the
    // row outright.
    write_prepared_records(
        conn_mutex,
        files_to_update,
        stop_flag,
        config,
        fts_batch,
        |tx, rec| {
            let updated = repo::update_file_basic(tx, &rec.as_new_file()).map_err(|e| {
                format!(
                    "Failed to update file record + clear stale content for {}: {}",
                    rec.path(),
                    e
                )
            })?;

            // No row matched: dropping the update would leave the mtime stale
            // and the file re-hashed on every run forever.
            match updated {
                None => {
                    crate::log_warn!(
                        "no indexed row matched {} during update; inserting instead",
                        rec.path()
                    );
                    repo::insert_file(tx, &rec.as_new_file())
                        .map_err(|e| format!("Failed to insert file record: {}", e))
                }
                some => Ok(some),
            }
        },
    )
}

/// Write already-prepared records for newly discovered files.
pub fn process_batch_inserts(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    // `.max(1)`: `chunks(0)` panics on the indexing thread, so a hand-edited
    // `batch_size = 0` would wedge indexing while the UI reads "Running".
    //
    // The row leaves this closure `_fresh`: `insert_file` returned `Some`
    // only by creating it, so it cannot carry content from anywhere.
    write_prepared_records(
        conn_mutex,
        files_to_insert,
        stop_flag,
        config,
        config.processing.batch_size.max(1),
        |tx, rec| {
            repo::insert_file(tx, &rec.as_new_file())
                .map_err(|e| format!("Failed to insert file record: {}", e))
        },
    )
}

/// Delete the rows a completed run found no file behind. Returns how many
/// went. A chunk either commits whole or is not begun, so a stop cannot
/// leave the index half-reconciled.
pub fn cleanup_stale_index_entries(
    conn_mutex: &Arc<Mutex<Connection>>,
    stale_paths: &[String],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<usize, String> {
    if stale_paths.is_empty() {
        return Ok(0);
    }
    let chunk = config.processing.batch_size.max(1);
    let mut deleted_count = 0usize;

    for batch in stale_paths.chunks(chunk) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(deleted_count);
        }
        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin stale cleanup transaction: {}", e))?;
        for path in batch {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            if repo::delete_file_by_path(&tx, path)
                .map_err(|e| format!("Failed to remove stale index entry for {}: {}", path, e))?
            {
                deleted_count += 1;
            }
        }
        tx.commit()
            .map_err(|e| format!("Failed to commit stale cleanup transaction: {}", e))?;
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(deleted_count);
        }
    }

    if deleted_count > 0 && !stop_flag.load(Ordering::Relaxed) {
        let conn = crate::lock_ok(conn_mutex);
        fts_finalize_after_text_indexing(&conn);
    }

    Ok(deleted_count)
}

/// Keyset cursor bounding everything stored beneath one directory: the
/// half-open range `[dir + SEP, dir + (SEP + 1))` over `files.parent`. It
/// covers `dir`'s own files because every stored parent ends in a separator
/// (see `dir_to_db_parent`).
///
/// The separator must be the platform's own: the successor of `/` is `'0'`
/// while the successor of `\` is `']'` — the Unix pair on Windows yields a
/// `hi` every stored parent sorts *above*, silently disabling content
/// extraction and the vanished-directory sweep.
#[derive(Debug, Clone)]
pub struct ExtractCursor {
    pub last_id: i64,
    pub lo: String,
    pub hi: String,
}

impl ExtractCursor {
    /// Cursor covering everything under `root`.
    pub fn for_root(root: &str) -> ExtractCursor {
        const SEP: char = std::path::MAIN_SEPARATOR;
        // Both separators are trimmed, not just the platform's: a config or
        // watcher event may spell a directory either way.
        let base = root.trim_end_matches(['/', '\\']);
        let next = char::from_u32(SEP as u32 + 1).expect("separator successor is a valid char");
        ExtractCursor {
            last_id: 0,
            lo: format!("{}{}", base, SEP),
            hi: format!("{}{}", base, next),
        }
    }
}

/// What a root's extraction scope holds. Both halves count only files an
/// extractor claims, so their sum is a denominator for the *work*, not for
/// every file under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractScope {
    pub pending: usize,
    pub already_done: usize,
}

pub(crate) fn max_text_file_size(config: &Config) -> i64 {
    i64::try_from(config.processing.maximum_text_file_size).unwrap_or(i64::MAX)
}

/// Flip a root's oversize pending rows to NA. Idempotent. Covers what
/// walk-time decisions cannot: a `maximum_text_file_size` *lowered* between
/// runs, and rows left pending by an older build.
///
/// `INDEXED BY`: the planner's choice fetches every table row in the range on
/// the writer thread; the partial index measured faster in every shape — this
/// cannot be left to statistics.
pub fn mark_oversize_pending_na(
    conn: &Connection,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<(), String> {
    conn.execute(
        "UPDATE files INDEXED BY idx_files_content_pending SET content_state = 3 \
         WHERE content_state = 0 AND size > ?1 AND parent >= ?2 AND parent < ?3",
        rusqlite::params![max_text_file_size(config), cursor.lo, cursor.hi],
    )
    .map_err(|e| format!("mark oversize files NA: {}", e))?;
    Ok(())
}

/// Count what a root's range holds, in one range scan. The content pass runs
/// it on its own read connection, never the writer's: on a large root it
/// takes seconds, and seconds of writer time stalls every other root's walk.
pub fn count_extract_scope(
    conn: &Connection,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<ExtractScope, String> {
    let (pending, already_done): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(content_state = 0 AND size <= ?1), 0), \
                    COALESCE(SUM(content_state = 1), 0) \
             FROM files WHERE parent >= ?2 AND parent < ?3",
            rusqlite::params![max_text_file_size(config), cursor.lo, cursor.hi],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("Failed to count text files: {}", e))?;
    Ok(ExtractScope {
        pending: pending.max(0) as usize,
        already_done: already_done.max(0) as usize,
    })
}

/// Rows per compression chunk and per transaction inside [`store_extracted`]:
/// half of what a writer turn may hand in (`pipeline::READY_TOPUP` is 64), so
/// a full turn commits twice — short holds of the connection being the point.
const STORE_CHUNK: usize = 32;

/// What one [`store_extracted`] call did with the rows it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stored {
    /// Rows the caller must now drop from its buffer, written or not.
    pub consumed: usize,
    /// Rows whose write succeeded — whose `content_state` moved.
    pub written: usize,
}

/// Write already-extracted rows — all that runs with the connection held —
/// until `deadline`. This is where FTS5 tokenization happens: the writer
/// thread's dominant cost. A turn overruns the deadline by at most one
/// document; at least one row is always consumed unless the run is stopped,
/// so a caller looping on this cannot spin. A row whose write fails is
/// logged and consumed; its `content_state` stays pending for the next run.
pub fn store_extracted(
    conn_mutex: &Arc<Mutex<Connection>>,
    rows: &[crate::content::ExtractedRow],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
    deadline: std::time::Instant,
) -> Result<Stored, String> {
    let mut done = Stored::default();
    // One set of buffers for every chunk this turn writes.
    let mut bodies = Bodies::new()?;
    for chunk in rows.chunks(STORE_CHUNK) {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Outside the lock — see `Bodies::fill`.
        bodies.fill(
            chunk
                .iter()
                .map(|r| crate::file_handling::outcome_body(&r.outcome)),
            config,
        );
        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        let mut cut = false;
        for (i, row) in chunk.iter().enumerate() {
            // Counted before anything can skip it: a failed row still leaves.
            done.consumed += 1;
            match bodies.get(i) {
                Err(e) => crate::log_warn!("compress text for {}: {}", row.name(), e),
                Ok(zstd) => match store_content_outcome(&tx, row.file_id, &row.outcome, zstd) {
                    Ok(()) => done.written += 1,
                    Err(e) => crate::log_warn!("content indexing for {}: {}", row.name(), e),
                },
            }
            if stop_flag.load(Ordering::Relaxed) || std::time::Instant::now() >= deadline {
                cut = true;
                break;
            }
        }
        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
        if cut {
            break;
        }
    }
    Ok(done)
}
