//! Batched DB writes: the insert/update paths every walk funnels into,
//! stale-row cleanup, and the extraction cursor/scope bookkeeping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::*;
use crate::config::Config;
use crate::db::repo::{self};

/// The compressed sidecar for one row, or `None` where there is none to write
/// — an empty body, or `store_text_for_snippets` turned off.
///
/// `Err` is kept per row rather than failing the batch, because every caller
/// here already logs and skips a row whose write fails.
type Body = Result<Option<Vec<u8>>, String>;

/// Compress a batch's bodies through one context, before the caller takes the
/// connection.
///
/// Compression used to run inside the transaction, so a chunk's worth of it —
/// measured at ~8 ms per 500 documents — sat inside the `conn_mutex` hold as
/// pure CPU. Hoisting it here leaves the lock covering only the SQL, and
/// reusing one [`repo::DocEncoder`] across the batch cuts the compression
/// itself by ~4.7x (`benches/index.rs`, group `zstd_encode`).
///
/// What that lock does *not* gate, so the benefit is not overclaimed: search
/// holds its own connection (`db::open::open_search_reader`) and the database
/// is WAL, where a reader never blocks on a writer. Nor does it separate one
/// root from another — every root's writes already run on the single writer
/// thread, so two of them are never inside the lock at once. What it actually
/// serializes the run against is WAL checkpointing, which `run_indexing`
/// forces from the same thread between turns. A whole-tree wall-clock run is
/// dominated by FTS5 trigram tokenization and does not move measurably from
/// this change; it is the length of the hold that improves, not throughput.
fn compress_bodies<'a>(
    texts: impl Iterator<Item = Option<&'a str>>,
    config: &Config,
) -> Result<Vec<Body>, String> {
    let mut enc = repo::DocEncoder::new()?;
    Ok(texts
        .map(|text| match text {
            Some(t) if config.processing.store_text_for_snippets && !t.is_empty() => {
                enc.encode(t).map(Some)
            }
            _ => Ok(None),
        })
        .collect())
}

/// The sidecar blob for row `i`, or a logged skip if its compression failed.
macro_rules! body_or_skip {
    ($bodies:expr, $i:expr, $what:expr) => {
        match &$bodies[$i] {
            Ok(b) => b.as_deref(),
            Err(e) => {
                crate::log_warn!("compress text for {}: {}", $what, e);
                continue;
            }
        }
    };
}

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

        // Outside the lock — see `compress_bodies`.
        let bodies = compress_bodies(batch.iter().map(|r| r.inline_text.as_deref()), config)?;
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
                let zstd = body_or_skip!(bodies, i, rec.path);
                store_inline_text(&tx, id, rec, text, zstd)?;
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
    text_zstd: Option<&[u8]>,
) -> Result<(), String> {
    repo::set_content_done(tx, file_id, &rec.name, text, &[], text_zstd)
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

        // Outside the lock — see `compress_bodies`.
        let bodies = compress_bodies(batch.iter().map(|r| r.inline_text.as_deref()), config)?;
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
            let id = repo::insert_file(&tx, &rec.as_new_file())
                .map_err(|e| format!("Failed to insert file record: {}", e))?;
            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                let zstd = body_or_skip!(bodies, i, rec.path);
                store_inline_text(&tx, id, rec, text, zstd)?;
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
/// The stop flag is checked between chunks and again per path, never with a
/// transaction open: a chunk either commits whole or is not begun, so a stop
/// cannot leave the index half-reconciled.
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
        // Outside the lock, so a stop is seen before a transaction is begun.
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

/// The `maximum_text_file_size` bound as the SQL below compares it.
pub(crate) fn max_text_file_size(config: &Config) -> i64 {
    i64::try_from(config.processing.maximum_text_file_size).unwrap_or(i64::MAX)
}

/// Flip a root's oversize pending rows to NA. Idempotent.
///
/// Covers what walk-time decisions cannot: a `maximum_text_file_size`
/// *lowered* between runs (which does not force a rebuild), and rows left
/// pending by an older build. Rows this misses would stay pending forever, so
/// it runs on the writer before a root's content pass starts.
pub fn mark_oversize_pending_na(
    conn: &Connection,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<(), String> {
    conn.execute(
        "UPDATE files SET content_state = 3 \
         WHERE content_state = 0 AND size > ?1 AND path >= ?2 AND path < ?3",
        rusqlite::params![max_text_file_size(config), cursor.lo, cursor.hi],
    )
    .map_err(|e| format!("mark oversize files NA: {}", e))?;
    Ok(())
}

/// Count what a root's range holds: rows still to extract this run, and rows
/// whose text is already searchable from earlier runs.
///
/// One range scan for both figures. Deliberately callable on any connection
/// — the content pass runs it on its own read connection rather than on the
/// indexer's writer, because on a large root it takes seconds, and seconds of
/// writer time is every other root's walk standing still.
pub fn count_extract_scope(
    conn: &Connection,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<ExtractScope, String> {
    let (pending, already_done): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(content_state = 0 AND size <= ?1), 0), \
                    COALESCE(SUM(content_state = 1), 0) \
             FROM files WHERE path >= ?2 AND path < ?3",
            rusqlite::params![max_text_file_size(config), cursor.lo, cursor.hi],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("Failed to count text files: {}", e))?;
    Ok(ExtractScope {
        pending: pending.max(0) as usize,
        already_done: already_done.max(0) as usize,
    })
}

/// Rows per compression chunk and per transaction inside [`store_extracted`].
///
/// Half of what a writer turn may hand in (`pipeline::READY_TOPUP` is 64), so
/// a full turn commits twice rather than once — short holds of the connection
/// being the point. It also bounds the compression thrown away when the
/// deadline cuts a chunk short, to at most `STORE_CHUNK - 1` bodies.
///
/// Note the two buffers are additive: a root can hold `READY_TOPUP` extracted
/// rows waiting for the writer *and* `content::READY_CAP` more in its
/// channel, so in-flight text per root is bounded by their sum, not by either
/// alone.
const STORE_CHUNK: usize = 32;

/// What one [`store_extracted`] call did with the rows it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stored {
    /// Rows the caller must now drop from its buffer, written or not.
    pub consumed: usize,
    /// Rows whose write succeeded — whose `content_state` moved.
    pub written: usize,
}

/// Write already-extracted rows — the cheap half of the content pass, and all
/// that runs with the connection held — until `deadline`.
///
/// This is where a document's FTS5 trigram tokenization happens, up to
/// `maximum_text_size` of it per row, and it is the writer thread's dominant
/// cost. The deadline is checked after every row, so a turn on the writer
/// overruns it by at most one document; the rows not reached are left for the
/// caller to hand back next turn. At least one row is always consumed unless
/// the run is already stopped, so a caller looping on this cannot spin.
///
/// A row whose write fails is logged and consumed rather than failing the
/// run: its `content_state` stays pending, so the next run retries it.
pub fn store_extracted(
    conn_mutex: &Arc<Mutex<Connection>>,
    rows: &[crate::content::ExtractedRow],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
    deadline: std::time::Instant,
) -> Result<Stored, String> {
    let mut done = Stored::default();
    for chunk in rows.chunks(STORE_CHUNK) {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        // Outside the lock — see `compress_bodies`.
        let bodies = compress_bodies(
            chunk
                .iter()
                .map(|r| crate::file_handling::outcome_body(&r.outcome)),
            config,
        )?;
        let conn = crate::lock_ok(conn_mutex);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        let mut cut = false;
        for (i, row) in chunk.iter().enumerate() {
            // Counted before anything can skip it: consumed is what the
            // caller drains, and a row that failed still has to leave.
            done.consumed += 1;
            match &bodies[i] {
                Err(e) => crate::log_warn!("compress text for {}: {}", row.name, e),
                Ok(zstd) => match store_content_outcome(
                    &tx,
                    row.file_id,
                    &row.name,
                    &row.outcome,
                    zstd.as_deref(),
                ) {
                    Ok(()) => done.written += 1,
                    Err(e) => crate::log_warn!("content indexing for {}: {}", row.name, e),
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
