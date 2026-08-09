//! Batched DB writes: the insert/update paths every walk funnels into,
//! stale-row cleanup, and the extraction cursor/scope bookkeeping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::*;
use crate::config::Config;
use crate::db::repo::{self};
use crate::indexing::should_abort;

/// Write already-prepared records for files whose content changed.
///
/// The records arrive fully built (see [`prepare_file_record`]), so this does
/// no filesystem I/O; rows are chunked so each transaction, and therefore
/// each hold of the connection lock, stays short. Records that already carry
/// their text ([`OwnedNewFile::inline_text`]) are stored complete here; the
/// rest stay pending.
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

    for batch in files_to_update.chunks(fts_batch) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for rec in batch.iter() {
            if stop_flag.load(Ordering::Relaxed) {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            let updated = repo::update_file_basic(&tx, &rec.as_new_file()).map_err(|e| {
                format!(
                    "Failed to update file record + clear stale content for {}: {}",
                    rec.path, e
                )
            })?;

            // No row matched: the path spelling we're writing disagrees with
            // the stored one. Dropping the update would leave the mtime stale
            // and the file re-hashed on every run forever.
            let id = match updated {
                None => {
                    crate::log_warn!(
                        "no indexed row matched {} during update; inserting instead",
                        rec.path
                    );
                    repo::insert_file(&tx, &rec.as_new_file())
                        .map_err(|e| format!("Failed to insert file record: {}", e))?
                }
                some => some,
            };

            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                store_inline_text(&tx, id, rec, text, config)?;
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Store text the walk already extracted, so the content pass skips this row.
/// Same [`repo::set_content_done`] the content pass calls, so a row finished
/// here is indistinguishable from one finished there.
pub(crate) fn store_inline_text(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    rec: &OwnedNewFile,
    text: &str,
    config: &Config,
) -> Result<(), String> {
    repo::set_content_done(
        tx,
        file_id,
        &rec.name,
        text,
        &[],
        config.processing.store_text_for_snippets,
    )
}

/// Write already-prepared records for newly discovered files. Silent, like
/// [`process_batch_updates`], and likewise stores any text the walk already
/// extracted.
pub fn process_batch_inserts(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    for batch in files_to_insert.chunks(config.processing.batch_size) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for rec in batch.iter() {
            if stop_flag.load(Ordering::Relaxed) {
                drop(tx);
                drop(conn);
                return Ok(());
            }
            let id = repo::insert_file(&tx, &rec.as_new_file())
                .map_err(|e| format!("Failed to insert file record: {}", e))?;
            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                store_inline_text(&tx, id, rec, text, config)?;
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Delete the rows a completed run found no file behind, in chunked
/// transactions. Returns how many went.
///
/// `should_abort` *blocks* while the indexer is suspended, so it must only be
/// observed between chunks with nothing held — checking it mid-transaction
/// pins the shared connection for the whole suspension and freezes the GUI.
/// The stop flag, which never blocks, guards the inner loop.
pub fn cleanup_stale_index_entries(
    conn_mutex: &Arc<Mutex<Connection>>,
    stale_paths: &[String],
    stop_flag: &Arc<AtomicBool>,
    suspend_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<usize, String> {
    if stale_paths.is_empty() {
        return Ok(0);
    }
    let chunk = config.processing.batch_size.max(1);
    let mut deleted_count = 0usize;

    for batch in stale_paths.chunks(chunk) {
        // Outside the lock, so a suspend parks here rather than mid-transaction.
        if should_abort(stop_flag, suspend_flag) {
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

    if deleted_count > 0 && !should_abort(stop_flag, suspend_flag) {
        let conn = crate::lock_ok(conn_mutex);
        fts_finalize_after_text_indexing(&conn);
    }

    Ok(deleted_count)
}

/// Keyset cursor bounding everything stored beneath one directory.
///
/// `lo`/`hi` are the half-open path range `[dir + SEP, dir + (SEP + 1))`, so
/// the pair is a pure index range on `UNIQUE(files.path)`.
///
/// The separator must be the platform's own: `files.path` stores native
/// separators, and the successor of `/` (`0x2F`) is `'0'` while the successor
/// of `\` (`0x5C`) is `']'` — the Unix pair on Windows yields
/// `hi = "C:\Users\me0"`, which every stored path sorts *above*, silently
/// disabling content extraction and the vanished-directory sweep.
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
        // Both separators are trimmed, not just the platform's: a config or a
        // watcher event may spell a directory either way, and a trailing one
        // would otherwise be doubled into the bounds.
        let base = root.trim_end_matches(['/', '\\']);
        let next = char::from_u32(SEP as u32 + 1).expect("separator successor is a valid char");
        ExtractCursor {
            last_id: 0,
            lo: format!("{}{}", base, SEP),
            hi: format!("{}{}", base, next),
        }
    }
}

/// What a root's extraction scope holds: rows still to extract this run,
/// and rows whose text is already searchable from earlier runs.
///
/// Both halves count only files an extractor claims — rows nothing will
/// extract are written `NA` at walk time (see [`content_extractable`]) — so
/// their sum is a denominator for the *work*, not for every file under the
/// root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractScope {
    pub pending: usize,
    pub already_done: usize,
}

/// Prepare a root's extraction scope: flip oversize pending rows to NA
/// (idempotent) and count what is pending vs. already extracted in the range.
///
/// The oversize sweep covers what walk-time decisions cannot: a
/// `maximum_text_file_size` *lowered* between runs (which does not force a
/// rebuild), and rows left pending by an older build.
pub fn extract_scope_prepare(
    conn_mutex: &Arc<Mutex<Connection>>,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<ExtractScope, String> {
    let max_size = i64::try_from(config.processing.maximum_text_file_size).unwrap_or(i64::MAX);
    let conn = crate::lock_ok(conn_mutex);
    conn.execute(
        "UPDATE files SET content_state = 3 \
         WHERE content_state = 0 AND size > ?1 AND path >= ?2 AND path < ?3",
        rusqlite::params![max_size, cursor.lo, cursor.hi],
    )
    .map_err(|e| format!("mark oversize files NA: {}", e))?;
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files \
             WHERE content_state = 0 AND size <= ?1 AND path >= ?2 AND path < ?3",
            rusqlite::params![max_size, cursor.lo, cursor.hi],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count pending text files: {}", e))?;
    let already_done: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files \
             WHERE content_state = 1 AND path >= ?1 AND path < ?2",
            rusqlite::params![cursor.lo, cursor.hi],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count extracted files: {}", e))?;
    Ok(ExtractScope {
        pending: pending.max(0) as usize,
        already_done: already_done.max(0) as usize,
    })
}

/// Write a batch of already-extracted rows — the cheap half of the content
/// pass, and all that runs with the connection held. Chunked so each
/// transaction stays short.
///
/// Returns how many rows were written. A row whose write fails is logged and
/// skipped rather than failing the run: its `content_state` stays pending, so
/// the next run retries it.
pub fn store_extracted(
    conn_mutex: &Arc<Mutex<Connection>>,
    rows: &[crate::content::ExtractedRow],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<usize, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    for batch in rows.chunks(config.processing.batch_size.max(1)) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(written);
        }
        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        for row in batch {
            if let Err(e) = store_content_outcome(&tx, row.file_id, &row.name, &row.outcome, config)
            {
                crate::log_warn!("content indexing for {}: {}", row.name, e);
                continue;
            }
            written += 1;
        }
        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    Ok(written)
}
