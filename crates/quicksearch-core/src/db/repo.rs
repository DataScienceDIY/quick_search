//! Row-level write helpers that keep the FTS5 contentless table in sync with
//! `files`/`documents`/`properties`.
//!
//! FTS5 contentless tables store postings only. Updating them requires the
//! old row values to compute which terms to remove. These helpers centralize
//! that bookkeeping so callers never have to remember the order of operations.
//!
//! States (mirrors `basic_state` / `content_state` columns):
//!
//! | value | meaning |
//! |------:|---------|
//! |     0 | pending |
//! |     1 | done    |
//! |     2 | failed  |
//! |     3 | not applicable (content only) |

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::mime::FileType;

pub const STATE_PENDING: i64 = 0;
pub const STATE_DONE: i64 = 1;
pub const STATE_FAILED: i64 = 2;
pub const STATE_NA: i64 = 3;

/// Everything needed to insert a fresh file row.
#[derive(Debug, Clone)]
pub struct NewFile<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub parent: &'a str,
    pub size: u64,
    pub mtime: u64,
    pub inode: Option<u64>,
    pub device_id: Option<u64>,
    pub mime: Option<&'a str>,
    pub ftype: FileType,
    pub hash: Option<&'a [u8]>,
    /// Whether the content pass has work to do for this file. `false` means
    /// the row is born `STATE_NA` — nothing claims its MIME, the
    /// `content_extensions` filter excludes it, or it is over
    /// `maximum_text_file_size`. Decided at walk time by
    /// [`crate::file_handling::content_extractable`].
    pub needs_content: bool,
}

/// Insert a new file row, returning its id. `basic_state` is set to DONE
/// (row existing *is* the basic-index state); `content_state` comes from
/// `needs_content` — PENDING for a file an extractor will claim, NA for one it
/// won't. Deciding here rather than on a content worker is what keeps PENDING
/// meaning "real work outstanding", which the extraction progress denominator
/// and [`pending_content_page`] both depend on.
///
/// Uses `INSERT OR IGNORE` so a UNIQUE(path) collision (which indicates the
/// caller fed the same path twice in one run) becomes a silent no-op
/// returning `None` rather than aborting the whole batch. The walker is
/// expected to dedupe visits upstream; this is a defense-in-depth backstop.
pub fn insert_file(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    let rows = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO files (
                name, path, parent, size, mtime, inode, device_id,
                mime, type, basic_state, content_state, hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .and_then(|mut stmt| {
            stmt.execute(params![
                f.name,
                f.path,
                f.parent,
                f.size as i64,
                f.mtime as i64,
                f.inode.map(|x| x as i64),
                f.device_id.map(|x| x as i64),
                f.mime,
                f.ftype.bits() as i64,
                STATE_DONE,
                initial_content_state(f),
                f.hash,
            ])
        })
        .map_err(|e| format!("insert file {}: {}", f.path, e))?;
    if rows == 0 {
        // Existing row with the same path (e.g. duplicate visit within the run).
        return Ok(None);
    }
    Ok(Some(tx.last_insert_rowid()))
}

/// The `content_state` a freshly written row starts in. Shared by
/// [`insert_file`] and [`update_file_basic`] so a file that reappears as an
/// update lands in the same state it would have as an insert.
fn initial_content_state(f: &NewFile<'_>) -> i64 {
    if f.needs_content {
        STATE_PENDING
    } else {
        STATE_NA
    }
}

/// Update a file's metadata in place (same path, changed size/mtime/hash) and
/// reset its content state from `f.needs_content`, clearing any extracted
/// content so the text-indexing pass re-processes it.
///
/// Writes `size`, `mtime`, `hash`, `mime`, `type`, `content_state` and
/// `failure_msg` — and only those. `name`, `parent`, `inode` and `device_id`
/// are not refreshed here despite being present on the `NewFile`: the row is
/// found by path, and the first two are functions of it.
pub fn update_file_basic(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    // One statement, not a lookup then an update: `RETURNING` hands back the
    // id of the row it just wrote, and a miss is simply no row returned.
    let id: Option<i64> = tx
        .prepare_cached(
            "UPDATE files
                SET size = ?1, mtime = ?2, hash = ?3, mime = ?4, type = ?5,
                    content_state = ?6, failure_msg = NULL
              WHERE path = ?7
          RETURNING id",
        )
        .and_then(|mut stmt| {
            stmt.query_row(
                params![
                    f.size as i64,
                    f.mtime as i64,
                    f.hash,
                    f.mime,
                    f.ftype.bits() as i64,
                    initial_content_state(f),
                    f.path,
                ],
                |r| r.get(0),
            )
            .optional()
        })
        .map_err(|e| format!("update file {}: {}", f.path, e))?;
    let Some(id) = id else {
        return Ok(None);
    };
    // Any prior extracted content is stale — remove it along with its FTS row.
    remove_content_for_id(tx, id)?;
    Ok(Some(id))
}

/// Mark a file's content indexing as complete and write the extracted text +
/// properties atomically. The plaintext is fed to the contentless FTS5
/// tokenizer (which keeps only the inverted index) and — when `store_text`
/// is `true` — separately stored zstd-compressed in `documents_text` for
/// on-demand snippet rendering. When `store_text=false` the sidecar INSERT
/// is skipped: queries still match the right files but result rows can't
/// render snippets. `properties` are stored both as a structured side-
/// table (for exact retrieval) and concatenated into the FTS `properties`
/// column (for MATCH).
pub fn set_content_done(
    tx: &Transaction<'_>,
    file_id: i64,
    name: &str,
    text: &str,
    properties: &[(String, String)],
    store_text: bool,
) -> Result<(), String> {
    // Clear any previous extraction (in case of re-run).
    remove_content_for_id(tx, file_id)?;

    for (k, v) in properties {
        tx.prepare_cached("INSERT INTO properties(file_id, key, value) VALUES (?1, ?2, ?3)")
            .and_then(|mut stmt| stmt.execute(params![file_id, k, v]))
            .map_err(|e| format!("insert property {}={}: {}", k, v, e))?;
    }
    let props_blob = encode_properties_for_fts(properties);
    // Contentless FTS5 still accepts values on INSERT — the tokenizer needs
    // them — it simply doesn't persist the raw column values.
    tx.prepare_cached(
        "INSERT INTO searchabletext(rowid, name, text, properties) VALUES (?1, ?2, ?3, ?4)",
    )
    .and_then(|mut stmt| stmt.execute(params![file_id, name, text, props_blob]))
    .map_err(|e| format!("insert FTS row {}: {}", file_id, e))?;

    // Skip the compressed sidecar when: the config disables snippet storage
    // outright, or there's no body text (e.g. an image whose extractor
    // returned only EXIF properties). The second case saves a zstd frame
    // on what would otherwise be an empty blob.
    if store_text && !text.is_empty() {
        let compressed = zstd::encode_all(text.as_bytes(), ZSTD_LEVEL)
            .map_err(|e| format!("zstd encode for file {}: {}", file_id, e))?;
        tx.prepare_cached(
            "INSERT INTO documents_text(file_id, text_zstd, text_len) VALUES (?1, ?2, ?3)",
        )
        .and_then(|mut stmt| stmt.execute(params![file_id, compressed, text.len() as i64]))
        .map_err(|e| format!("insert documents_text {}: {}", file_id, e))?;
    }

    tx.prepare_cached("UPDATE files SET content_state = ?1, failure_msg = NULL WHERE id = ?2")
        .and_then(|mut stmt| stmt.execute(params![STATE_DONE, file_id]))
        .map_err(|e| format!("update content_state DONE {}: {}", file_id, e))?;
    // Clear any prior failed-file record.
    tx.prepare_cached("DELETE FROM failed_files WHERE file_id = ?1")
        .and_then(|mut stmt| stmt.execute(params![file_id]))
        .map_err(|e| format!("clear failed_files {}: {}", file_id, e))?;
    Ok(())
}

/// zstd level tuned for extracted-text prose. Level 3 hits ~3-5× on English
/// prose at high throughput (hundreds of MB/s) — level 9+ would shave a few
/// percent more but at 10× the CPU cost during indexing. Wrong knob to
/// tune: readers decompress far faster than writers compress, so keep
/// write-side cost low.
const ZSTD_LEVEL: i32 = 3;

/// Mark a file's content extraction as failed. Keeps the basic row in place.
pub fn set_content_failed(tx: &Transaction<'_>, file_id: i64, reason: &str) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    tx.prepare_cached("UPDATE files SET content_state = ?1, failure_msg = ?2 WHERE id = ?3")
        .and_then(|mut stmt| stmt.execute(params![STATE_FAILED, reason, file_id]))
        .map_err(|e| format!("update content_state FAILED {}: {}", file_id, e))?;
    tx.prepare_cached(
        "INSERT OR REPLACE INTO failed_files(file_id, reason, ts) VALUES (?1, ?2, ?3)",
    )
    .and_then(|mut stmt| stmt.execute(params![file_id, reason, now]))
    .map_err(|e| format!("insert failed_files {}: {}", file_id, e))?;
    Ok(())
}

/// Mark content extraction as not applicable (e.g. binary format we don't
/// support). The file row still contributes to filename search.
pub fn set_content_na(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    tx.prepare_cached("UPDATE files SET content_state = ?1, failure_msg = NULL WHERE id = ?2")
        .and_then(|mut stmt| stmt.execute(params![STATE_NA, file_id]))
        .map_err(|e| format!("update content_state NA {}: {}", file_id, e))?;
    tx.prepare_cached("DELETE FROM failed_files WHERE file_id = ?1")
        .and_then(|mut stmt| stmt.execute(params![file_id]))
        .map_err(|e| format!("clear failed_files {}: {}", file_id, e))?;
    Ok(())
}

/// Delete a file row by path, keeping FTS in sync. Returns whether a row was
/// removed.
pub fn delete_file_by_path(tx: &Transaction<'_>, path: &str) -> Result<bool, String> {
    // `RETURNING` gives the id of the row that went, so the dependent tables
    // can be cleared without a separate lookup first.
    let id: Option<i64> = tx
        .prepare_cached("DELETE FROM files WHERE path = ?1 RETURNING id")
        .and_then(|mut stmt| stmt.query_row(params![path], |r| r.get(0)).optional())
        .map_err(|e| format!("delete file {}: {}", path, e))?;
    let Some(id) = id else { return Ok(false) };
    remove_content_for_id(tx, id)?;
    Ok(true)
}

/// Delete every row whose path falls in the half-open range `[lo, hi)`,
/// keeping FTS, `documents_text`, `properties` and `failed_files` in step.
/// Returns how many `files` rows went.
///
/// The bulk counterpart to [`delete_file_by_path`], for removing a whole
/// directory at once. Five statements regardless of how many files the range
/// holds — where deleting them one at a time costs ~5 *per file* — and the
/// range is a plain index seek on `UNIQUE(files.path)`, so this is
/// `SEARCH … (path>? AND path<?)` rather than a scan. Build the bounds with
/// [`crate::file_handling::ExtractCursor::for_root`], which is what makes them
/// separator-correct.
///
/// All four dependent tables are deleted explicitly rather than left to their
/// `ON DELETE CASCADE` foreign keys. `searchabletext` is an FTS5 virtual table
/// and has no foreign key at all, so relying on cascade would leave the rule
/// half-declarative and half-manual, free to drift the moment someone opens a
/// connection without `PRAGMA foreign_keys`. (`contentless_delete=1` is what
/// lets the FTS rows go by rowid alone.)
pub fn delete_subtree(tx: &Transaction<'_>, lo: &str, hi: &str) -> Result<usize, String> {
    // Every dependent table is keyed by the file id, so they share one
    // sub-select; `files` itself goes last, once nothing references it.
    for (table, key) in [
        ("searchabletext", "rowid"),
        ("documents_text", "file_id"),
        ("properties", "file_id"),
        ("failed_files", "file_id"),
    ] {
        let sql = format!(
            "DELETE FROM {} WHERE {} IN \
             (SELECT id FROM files WHERE path >= ?1 AND path < ?2)",
            table, key
        );
        tx.prepare_cached(&sql)
            .and_then(|mut stmt| stmt.execute(params![lo, hi]))
            .map_err(|e| format!("delete {} under {}: {}", table, lo, e))?;
    }
    let removed = tx
        .prepare_cached("DELETE FROM files WHERE path >= ?1 AND path < ?2")
        .and_then(|mut stmt| stmt.execute(params![lo, hi]))
        .map_err(|e| format!("delete files under {}: {}", lo, e))?;
    Ok(removed)
}

/// Every indexed file directly inside `parent`, as `name -> mtime`.
///
/// The walk's unit of classification. Keyed by name rather than full path
/// because the parent is implied — at millions of files, not storing the
/// directory prefix once per entry is the difference this whole path exists
/// to make. Served by `idx_files_parent`, so this is one index range lookup.
pub fn dir_rows(
    conn: &Connection,
    parent: &str,
) -> Result<std::collections::HashMap<String, u64>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT name, mtime FROM files WHERE parent = ?1")
        .map_err(|e| format!("prepare dir rows for {}: {}", parent, e))?;
    let rows = stmt
        .query_map(params![parent], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })
        .map_err(|e| format!("query dir rows for {}: {}", parent, e))?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (name, mtime) = row.map_err(|e| format!("read dir row under {}: {}", parent, e))?;
        out.insert(name, mtime);
    }
    Ok(out)
}

/// One page of rows still awaiting content extraction under `cursor`'s range,
/// as `(id, name, path, mime)` ordered by id.
///
/// Keyset, not `OFFSET`: `id > cursor.last_id` means each page is an index
/// seek rather than a re-scan of everything already handed out, and — because
/// the cursor only moves forward — a row is served exactly once even though
/// the writer is concurrently flipping `content_state` behind the reader. The
/// `content_state = 0` predicate is the belt to that braces, not the
/// mechanism.
pub fn pending_content_page(
    conn: &Connection,
    cursor: &crate::file_handling::ExtractCursor,
    max_size: i64,
    limit: i64,
) -> Result<Vec<(i64, String, String, Option<String>)>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, name, path, mime FROM files
              WHERE content_state = 0 AND size <= ?1 AND id > ?2
                AND path >= ?3 AND path < ?4
              ORDER BY id
              LIMIT ?5",
        )
        .map_err(|e| format!("prepare pending content query: {}", e))?;
    let rows = stmt
        .query_map(
            params![max_size, cursor.last_id, cursor.lo, cursor.hi, limit],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|e| format!("query pending content: {}", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read pending content row: {}", e))
}

/// The stored mtime for one exact path, or `None` if it isn't indexed.
///
/// For files the walk reaches by a spelling whose parent isn't the directory
/// being read — a resolved symlink target — where [`dir_rows`] would not
/// have them.
pub fn mtime_for_path(conn: &Connection, path: &str) -> Result<Option<u64>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT mtime FROM files WHERE path = ?1")
        .map_err(|e| format!("prepare mtime lookup for {}: {}", path, e))?;
    stmt.query_row(params![path], |r| r.get::<_, i64>(0))
        .optional()
        .map(|o| o.map(|m| m.max(0) as u64))
        .map_err(|e| format!("mtime lookup for {}: {}", path, e))
}

/// Distinct `parent` values within the half-open path range `[lo, hi)`,
/// streamed to `f` rather than collected.
///
/// Callers use this to find directories the walk never visited, so it must
/// not itself materialize a list proportional to the tree — the whole point
/// of the change that introduced it. `idx_files_parent` makes this an
/// index-only scan.
pub fn for_each_parent_in_range<F: FnMut(String)>(
    conn: &Connection,
    lo: &str,
    hi: &str,
    mut f: F,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT parent FROM files WHERE parent >= ?1 AND parent < ?2")
        .map_err(|e| format!("prepare parent scan: {}", e))?;
    let rows = stmt
        .query_map(params![lo, hi], |r| r.get::<_, String>(0))
        .map_err(|e| format!("parent scan: {}", e))?;
    for row in rows {
        f(row.map_err(|e| format!("read parent row: {}", e))?);
    }
    Ok(())
}

/// Paths of every file directly inside `parent`.
///
/// The companion to [`for_each_parent_in_range`]: once a parent is known to
/// be unvisited, this is what its rows are.
pub fn paths_in_dir(conn: &Connection, parent: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT path FROM files WHERE parent = ?1")
        .map_err(|e| format!("prepare paths in {}: {}", parent, e))?;
    let rows = stmt
        .query_map(params![parent], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query paths in {}: {}", parent, e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read path under {}: {}", parent, e))
}

/// Remove the FTS row, compressed text blob, and any `properties` rows for
/// a given file id. Does not touch the `files` row itself. Idempotent — a
/// missing row is fine.
pub fn remove_content_for_id(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    // `contentless_delete=1` on the FTS5 table makes this work without
    // re-supplying the old column values (it tombstones the rowid).
    for (table, key) in [
        ("searchabletext", "rowid"),
        ("documents_text", "file_id"),
        ("properties", "file_id"),
    ] {
        let sql = format!("DELETE FROM {} WHERE {} = ?1", table, key);
        tx.prepare_cached(&sql)
            .and_then(|mut stmt| stmt.execute(params![file_id]))
            .map_err(|e| format!("delete {} for {}: {}", table, file_id, e))?;
    }
    Ok(())
}

/// Serialize properties for the FTS `properties` column. `key:value` pairs
/// separated by spaces so `MATCH 'properties:artist:beatles'` works.
fn encode_properties_for_fts(props: &[(String, String)]) -> String {
    let mut buf = String::new();
    for (i, (k, v)) in props.iter().enumerate() {
        if i > 0 {
            buf.push(' ');
        }
        buf.push_str(k);
        buf.push(':');
        buf.push_str(v);
    }
    buf
}

/// Free pages, as a percentage of the file, that make a [`maintain`] VACUUM
/// worth its cost. Below this the file is rewritten for nothing: a run that
/// changed little leaves little slack, and rewriting a multi-gigabyte index
/// to reclaim a few megabytes is minutes of I/O for no gain.
const VACUUM_MIN_SLACK_PERCENT: i64 = 10;

/// Flush the whole WAL into the main database and truncate the log to zero
/// bytes. `Err` means the log was *not* emptied.
///
/// `execute`/`execute_batch` discard the pragma's result row, and that row is
/// the only place SQLite reports that a checkpoint gave up — an incomplete
/// checkpoint is not an error, it is a number in a row nobody read. That is
/// how a log can grow past the index it journals without a word in the logs.
///
/// The signal is the *log* column (frames left in the WAL), not `busy`: a
/// TRUNCATE that cannot take the writer lock silently downgrades itself to
/// PASSIVE and still reports `busy = 0` with the log untouched. A real restart
/// sets `mxFrame` to 0. A database not in WAL mode reports -1, hence `<= 0`.
pub fn checkpoint_truncate(conn: &Connection) -> Result<(), String> {
    let (busy, log): (i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|e| format!("wal checkpoint: {}", e))?;
    if log <= 0 {
        Ok(())
    } else {
        Err(format!(
            "wal checkpoint incomplete: busy={}, {} frames left in the log",
            busy, log
        ))
    }
}

/// Flush the WAL into the main DB file and close. Call on clean shutdown so
/// the next open starts with an empty log. WAL mode itself is persistent in
/// the file — deliberately left on.
pub fn checkpoint_and_close(conn: Connection) {
    if let Err(e) = checkpoint_truncate(&conn) {
        crate::log_warn!("{}", e);
    }
    drop(conn);
}

/// Land the log, reclaim the file's slack, and refresh the query planner's
/// statistics. Returns whether it vacuumed.
///
/// The sequence is checkpoint → VACUUM → `PRAGMA optimize` → checkpoint. The
/// trailing checkpoint is not a repeat of the first: VACUUM's copy-back pushes
/// every page of the rebuilt file through the log, and `optimize` writes
/// `sqlite_stat1`, so leaving without one would trade the slack just reclaimed
/// for a log the size of the index.
///
/// Run on a connection from [`crate::db::open::open_maintenance`], never on
/// the indexer's: VACUUM builds the replacement database in the temp store,
/// and under the indexer's `temp_store = MEMORY` that means building the whole
/// index in RAM.
///
/// `db_dir` is where the temporary database goes, and it must be the index's
/// own directory. `temp_store = FILE` alone resolves through `SQLITE_TMPDIR`,
/// `TMPDIR`, `/var/tmp`, then `/tmp` — and `/tmp` is a RAM-backed tmpfs on many
/// Linux systems, which would put us straight back where we started. Keeping
/// the temporary database beside the index also puts it on a volume already
/// known to hold one.
///
/// Peak transient space on that volume is roughly three times the index: the
/// original, the replacement being built beside it, and the log that VACUUM's
/// copy-back runs through. Running out is a failed VACUUM, not a damaged
/// index — the transaction rolls back.
pub fn maintain(conn: &Connection, db_dir: &str) -> Result<bool, String> {
    // Best-effort: a reader that briefly pins the log is no reason to skip
    // the compaction below, which does not need the log empty to start.
    if let Err(e) = checkpoint_truncate(conn) {
        crate::log_warn!("{}", e);
    }

    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .map_err(|e| format!("read page_count: {}", e))?;
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .map_err(|e| format!("read freelist_count: {}", e))?;

    let vacuumed = freelist * 100 >= page_count * VACUUM_MIN_SLACK_PERCENT;
    if vacuumed {
        // `temp_store_directory` is a deprecated pragma that writes a global,
        // so it is set for the VACUUM and cleared straight after rather than
        // left standing for every other connection in the process.
        let escaped = db_dir.replace('\'', "''");
        conn.execute_batch(&format!("PRAGMA temp_store_directory = '{}';", escaped))
            .map_err(|e| format!("set temp dir for vacuum: {}", e))?;
        let outcome = conn
            .execute_batch("VACUUM;")
            .map_err(|e| format!("vacuum: {}", e));
        let _ = conn.execute_batch("PRAGMA temp_store_directory = '';");
        outcome?;
    }

    // Re-analyses only the tables whose shape has drifted far enough to matter,
    // so it is close to free on a run that changed little. A run that added
    // millions of rows is exactly when the planner's old statistics start
    // choosing the wrong index for a search.
    conn.execute_batch("PRAGMA optimize;")
        .map_err(|e| format!("optimize: {}", e))?;

    checkpoint_truncate(conn)?;
    Ok(vacuumed)
}

/// Read the `last_full_index` marker (unix seconds of the last *successful*
/// full indexing run) from `schema_info`. Absent key — fresh DB, or a DB
/// from before this marker existed — means "never".
pub fn get_last_full_index(conn: &Connection) -> Option<u64> {
    conn.query_row(
        "SELECT value FROM schema_info WHERE key = 'last_full_index'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|v| v.parse().ok())
}

/// Stamp `last_full_index` with `ts` (unix seconds). Called at the end of
/// every successful full indexing run; the coordinator reads it to schedule
/// periodic reindexing.
pub fn set_last_full_index(conn: &Connection, ts: u64) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_info(key, value) VALUES ('last_full_index', ?1)",
        params![ts.to_string()],
    )
    .map_err(|e| format!("write last_full_index: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::db::open_or_recreate;

    fn tmp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-repo-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn insert_update_delete_round_trip() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        {
            let tx = conn.transaction().unwrap();
            let id = insert_file(
                &tx,
                &NewFile {
                    name: "a.txt",
                    path: "/tmp/a.txt",
                    parent: "/tmp",
                    size: 42,
                    mtime: 1_700_000_000,
                    inode: Some(7),
                    device_id: Some(64768),
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: Some(&[1, 2, 3]),
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(
                &tx,
                id,
                "a.txt",
                "hello world",
                &[("title".to_string(), "hi".to_string())],
                true,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Text is findable via FTS.
        let hit: i64 = conn
            .query_row(
                "SELECT rowid FROM searchabletext WHERE searchabletext MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hit > 0);

        // Delete cleans up.
        {
            let tx = conn.transaction().unwrap();
            assert!(delete_file_by_path(&tx, "/tmp/a.txt").unwrap());
            tx.commit().unwrap();
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM searchabletext", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 0);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn insert_writes_content_state_from_needs_content() {
        // The whole point of the field: a row nothing will extract is born NA,
        // so "pending" downstream — the extraction denominator, the feeder's
        // page query, the Baloo pending count — means real outstanding work.
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let tx = conn.transaction().unwrap();
        let mut row = NewFile {
            name: "claimed.txt",
            path: "/tmp/claimed.txt",
            parent: "/tmp",
            size: 1,
            mtime: 1,
            inode: None,
            device_id: None,
            mime: Some("text/plain"),
            ftype: FileType::TEXT,
            hash: None,
            needs_content: true,
        };
        let claimed = insert_file(&tx, &row).unwrap().expect("unique path");
        row.name = "unclaimed.mp4";
        row.path = "/tmp/unclaimed.mp4";
        row.mime = Some("video/mp4");
        row.needs_content = false;
        let unclaimed = insert_file(&tx, &row).unwrap().expect("unique path");

        let state = |id: i64| -> i64 {
            tx.query_row(
                "SELECT content_state FROM files WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(state(claimed), STATE_PENDING);
        assert_eq!(state(unclaimed), STATE_NA);

        drop(tx);
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn update_writes_content_state_from_needs_content() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let mut row = NewFile {
            name: "a.txt",
            path: "/tmp/a.txt",
            parent: "/tmp",
            size: 10,
            mtime: 1,
            inode: None,
            device_id: None,
            mime: None,
            ftype: FileType::EMPTY,
            hash: None,
            needs_content: false,
        };
        let id = {
            let tx = conn.transaction().unwrap();
            let id = insert_file(&tx, &row).unwrap().expect("unique path");
            set_content_done(&tx, id, "a.txt", "old text", &[], true).unwrap();
            tx.commit().unwrap();
            id
        };

        let content_state = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT content_state FROM files WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };

        // Rewritten as something an extractor claims: back to pending, and the
        // stale FTS row goes with it.
        {
            let tx = conn.transaction().unwrap();
            row.size = 20;
            row.mtime = 2;
            row.mime = Some("text/plain");
            row.ftype = FileType::TEXT;
            row.needs_content = true;
            let got = update_file_basic(&tx, &row).unwrap();
            assert_eq!(got, Some(id));
            tx.commit().unwrap();
        }
        let basic: i64 = conn
            .query_row(
                "SELECT basic_state FROM files WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(basic, STATE_DONE);
        assert_eq!(content_state(&conn), STATE_PENDING);

        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 0);

        // And rewritten as something nothing claims: NA, not pending. Without
        // this the row would re-enter the content pass on every run.
        {
            let tx = conn.transaction().unwrap();
            row.mtime = 3;
            row.mime = Some("video/mp4");
            row.needs_content = false;
            update_file_basic(&tx, &row)
                .unwrap()
                .expect("row still there");
            tx.commit().unwrap();
        }
        assert_eq!(content_state(&conn), STATE_NA);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn insert_file_twice_on_same_path_is_idempotent() {
        // Defense-in-depth: when the walker visits a canonical path twice
        // (overlapping roots, symlink resolution quirks), the second INSERT
        // must be a silent no-op, not a run-ending error.
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let tx = conn.transaction().unwrap();
        let row = NewFile {
            name: "dup.txt",
            path: "/tmp/dup.txt",
            parent: "/tmp",
            size: 1,
            mtime: 1,
            inode: None,
            device_id: None,
            mime: Some("text/plain"),
            ftype: FileType::TEXT,
            hash: None,
            needs_content: true,
        };
        let id1 = insert_file(&tx, &row).unwrap().expect("first insert");
        let id2 = insert_file(&tx, &row).unwrap();
        assert!(id2.is_none(), "second insert of same path must return None");
        // Only one row exists.
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let (id_read,): (i64,) = tx
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params!["/tmp/dup.txt"],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(id_read, id1);
        tx.commit().unwrap();
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn delete_subtree_clears_every_dependent_table() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let add = |tx: &Transaction<'_>, path: &str| -> i64 {
            let name = path.rsplit('/').next().unwrap();
            let parent = &path[..path.rfind('/').unwrap()];
            let id = insert_file(
                tx,
                &NewFile {
                    name,
                    path,
                    parent,
                    size: 1,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(tx, id, name, "body text", &[("k".into(), "v".into())], true).unwrap();
            id
        };

        {
            let tx = conn.transaction().unwrap();
            add(&tx, "/tree/a.txt");
            add(&tx, "/tree/deep/b.txt");
            let failed = add(&tx, "/tree/deep/c.txt");
            set_content_failed(&tx, failed, "bad parse").unwrap();
            // Outside the range: a prefix sibling, and a LIKE-metacharacter
            // neighbour that a `LIKE 'tree_%'` sweep would have swallowed.
            add(&tx, "/tree2/keep.txt");
            add(&tx, "/treeX/keep.txt");
            tx.commit().unwrap();
        }

        let range = crate::file_handling::ExtractCursor::for_root("/tree");
        let removed = {
            let tx = conn.transaction().unwrap();
            // The directory row itself does not exist (only files are indexed),
            // so everything here comes from the range.
            let n = delete_subtree(&tx, &range.lo, &range.hi).unwrap();
            tx.commit().unwrap();
            n
        };
        assert_eq!(removed, 3, "three files under /tree");

        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM files"), 2, "siblings survive");
        assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 2);
        assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 2);
        assert_eq!(count("SELECT COUNT(*) FROM properties"), 2);
        assert_eq!(
            count("SELECT COUNT(*) FROM failed_files"),
            0,
            "the failed row went with its file"
        );

        let survivors: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT path FROM files ORDER BY path")
                .unwrap();
            let v = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        };
        assert_eq!(survivors, vec!["/tree2/keep.txt", "/treeX/keep.txt"]);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    /// The walk's row prefetcher runs `dir_rows` once per directory, against a
    /// deliberately tiny page cache, so it must not have to touch the table
    /// heap at all. `idx_files_parent` carries `name` and `mtime` for exactly
    /// this; trimming it back to `(parent)` would silently reintroduce a row
    /// fetch per entry.
    #[test]
    fn dir_rows_is_served_entirely_from_the_index() {
        let p = tmp_path();
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT name, mtime FROM files WHERE parent = ?1",
                params!["/some/dir"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("COVERING INDEX idx_files_parent"),
            "dir_rows must be index-only, got: {}",
            plan
        );
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    /// The whole point of the range form: it is an index seek. `LIKE … ESCAPE`
    /// cannot use the index (SQLite disables the LIKE optimisation whenever an
    /// ESCAPE clause is present), so the old sweep read every path in the table
    /// on every deletion event.
    #[test]
    fn the_subtree_range_is_an_index_seek_not_a_scan() {
        let p = tmp_path();
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN DELETE FROM files WHERE path >= ?1 AND path < ?2",
                params!["/tree/", "/tree0"],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("SEARCH") && !plan.contains("SCAN"),
            "range delete must seek, got: {}",
            plan
        );
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn last_full_index_round_trip() {
        let p = tmp_path();
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        assert_eq!(get_last_full_index(&conn), None, "fresh DB has no marker");
        set_last_full_index(&conn, 1_700_000_123).unwrap();
        assert_eq!(get_last_full_index(&conn), Some(1_700_000_123));
        // Overwrite, not accumulate.
        set_last_full_index(&conn, 1_700_000_999).unwrap();
        assert_eq!(get_last_full_index(&conn), Some(1_700_000_999));
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn checkpoint_and_close_truncates_wal() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        {
            let tx = conn.transaction().unwrap();
            insert_file(
                &tx,
                &NewFile {
                    name: "w.txt",
                    path: "/tmp/w.txt",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: None,
                    ftype: FileType::EMPTY,
                    hash: None,
                    needs_content: false,
                },
            )
            .unwrap();
            tx.commit().unwrap();
        }
        checkpoint_and_close(conn);
        // After a TRUNCATE checkpoint + close of the last connection the WAL
        // sidecar is gone or empty; the row lives in the main file.
        let wal = std::path::PathBuf::from(format!("{}-wal", p.display()));
        let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(wal_len, 0, "WAL should be truncated on clean close");
        let conn = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    /// Bulk rows, cheap to write, enough of them to make the file grow.
    fn seed_rows(conn: &mut Connection, range: std::ops::Range<usize>) {
        let tx = conn.transaction().unwrap();
        for i in range {
            let path = format!("/tmp/bulk/{}.txt", i);
            let name = format!("{}.txt", i);
            let id = insert_file(
                &tx,
                &NewFile {
                    name: &name,
                    path: &path,
                    parent: "/tmp/bulk",
                    size: 1,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .unwrap();
            set_content_done(
                &tx,
                id,
                &name,
                &"lorem ipsum dolor sit amet ".repeat(64),
                &[],
                true,
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    fn wal_bytes(p: &std::path::Path) -> u64 {
        std::fs::metadata(format!("{}-wal", p.display()))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    #[test]
    fn checkpoint_truncate_empties_the_log() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        seed_rows(&mut conn, 0..200);
        assert!(wal_bytes(&p) > 0, "the writes should be sitting in the log");

        checkpoint_truncate(&conn).expect("nothing is holding the log back");
        assert_eq!(wal_bytes(&p), 0, "a completed TRUNCATE leaves no log");

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    /// The case the old `execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")`
    /// reported as success: a reader pins the log, SQLite declines to reset
    /// it, and the only word of it is in the result row.
    #[test]
    fn checkpoint_truncate_reports_an_incomplete_checkpoint() {
        let p = tmp_path();
        let mut writer = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        seed_rows(&mut writer, 0..200);
        // Long enough to prove the point, short enough not to stall the suite.
        writer
            .busy_timeout(std::time::Duration::from_millis(100))
            .unwrap();

        let reader = crate::db::open_existing(p.to_str().unwrap(), false).unwrap();
        let mut stmt = reader.prepare("SELECT id FROM files").unwrap();
        let mut rows = stmt.query([]).unwrap();
        rows.next().unwrap().expect("a row to hold the snapshot on");

        let err = checkpoint_truncate(&writer).expect_err("a reader holds the log open");
        assert!(err.contains("incomplete"), "unexpected message: {}", err);
        assert!(wal_bytes(&p) > 0, "and the log is still there");

        drop(rows);
        drop(stmt);
        drop(reader);
        drop(writer);
        std::fs::remove_file(&p).ok();
    }

    /// The mechanism the whole in-run checkpoint exists for.
    ///
    /// SQLite's autocheckpoint copies committed frames into the database
    /// continuously, but the log is only *reset* when the writer opens a
    /// transaction at an instant no reader holds a read mark — and it tries
    /// that lock exactly once, with no retry. A reader querying back to back,
    /// which is what an indexing run's per-root prefetcher is, keeps that
    /// instant from arriving, so the log appends for as long as the run lasts.
    /// An explicit checkpoint retries the same lock under `busy_timeout` and
    /// gets it.
    #[test]
    fn a_busy_reader_defeats_the_autocheckpoint_but_not_a_forced_one() {
        // Bare `files` rows: no text, so no zstd and no tokenising. The log
        // grows from committed frames, and frames are what starves the reset —
        // paying for extraction here would only make the test slow.
        fn seed_bare(conn: &mut Connection, range: std::ops::Range<usize>) {
            let tx = conn.transaction().unwrap();
            for i in range {
                let path = format!("/tmp/bare/{}.txt", i);
                let name = format!("{}.txt", i);
                insert_file(
                    &tx,
                    &NewFile {
                        name: &name,
                        path: &path,
                        parent: "/tmp/bare",
                        size: i as u64,
                        mtime: 1,
                        inode: None,
                        device_id: None,
                        mime: None,
                        ftype: FileType::TEXT,
                        hash: Some(&[0u8; 32]),
                        needs_content: false,
                    },
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        fn run(p: &std::path::Path, force_every: usize) -> u64 {
            let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
            seed_bare(&mut conn, 0..500);

            // Stands in for a walk prefetcher: short reads, no gaps.
            let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
            let reader = {
                let (path, stop) = (p.to_path_buf(), stop.clone());
                std::thread::spawn(move || {
                    let conn = crate::db::open_existing(path.to_str().unwrap(), false).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        let _: i64 = conn
                            .query_row(
                                "SELECT COUNT(*) FROM files WHERE parent = '/tmp/bare'",
                                [],
                                |r| r.get(0),
                            )
                            .unwrap();
                    }
                })
            };

            let mut peak = 0u64;
            for batch in 0..60 {
                let lo = 1000 + batch * 300;
                seed_bare(&mut conn, lo..lo + 300);
                if force_every > 0 && batch % force_every == force_every - 1 {
                    let _ = checkpoint_truncate(&conn);
                }
                peak = peak.max(wal_bytes(p));
            }

            stop.store(true, Ordering::Relaxed);
            reader.join().unwrap();
            drop(conn);
            peak
        }

        let unbounded = tmp_path();
        let left_alone = run(&unbounded, 0);
        std::fs::remove_file(&unbounded).ok();

        let bounded = tmp_path();
        let forced = run(&bounded, 4);
        std::fs::remove_file(&bounded).ok();

        eprintln!(
            "peak WAL: autocheckpoint only {}, forced {}",
            left_alone, forced
        );
        assert!(
            forced * 2 < left_alone,
            "forcing checkpoints did not bound the log: {} vs {}",
            forced,
            left_alone
        );
    }

    #[test]
    fn maintain_vacuums_when_slack_is_significant() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        seed_rows(&mut conn, 0..2000);
        checkpoint_truncate(&conn).unwrap();
        let before = std::fs::metadata(&p).unwrap().len();

        {
            let tx = conn.transaction().unwrap();
            for i in 0..1900 {
                delete_file_by_path(&tx, &format!("/tmp/bulk/{}.txt", i)).unwrap();
            }
            tx.commit().unwrap();
        }
        drop(conn);

        let conn = crate::db::open::open_maintenance(p.to_str().unwrap()).unwrap();
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert!(freelist > 0, "the deletions should have freed pages");

        let dir = p.parent().unwrap().to_string_lossy().into_owned();
        assert!(
            maintain(&conn, &dir).unwrap(),
            "that much slack is worth a vacuum"
        );
        assert_eq!(
            wal_bytes(&p),
            0,
            "the vacuum's own writes are checkpointed too"
        );
        assert!(
            std::fs::metadata(&p).unwrap().len() < before,
            "the file should have shrunk"
        );
        // The surviving rows are still there and still searchable.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 100);
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'lorem'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 100, "the FTS index survived the rewrite");

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn maintain_skips_vacuum_on_a_tight_file() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        seed_rows(&mut conn, 0..200);
        drop(conn);

        let conn = crate::db::open::open_maintenance(p.to_str().unwrap()).unwrap();
        let dir = p.parent().unwrap().to_string_lossy().into_owned();
        assert!(
            !maintain(&conn, &dir).unwrap(),
            "a file with no slack is not worth rewriting"
        );
        // The checkpoint is not conditional on the vacuum, though.
        assert_eq!(wal_bytes(&p), 0);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn set_content_failed_writes_failed_table() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let id = {
            let tx = conn.transaction().unwrap();
            let id = insert_file(
                &tx,
                &NewFile {
                    name: "oops.bin",
                    path: "/tmp/oops.bin",
                    parent: "/tmp",
                    size: 0,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: None,
                    ftype: FileType::EMPTY,
                    hash: None,
                    needs_content: false,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_failed(&tx, id, "bad parse").unwrap();
            tx.commit().unwrap();
            id
        };

        let reason: String = conn
            .query_row(
                "SELECT reason FROM failed_files WHERE file_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "bad parse");
        let content_state: i64 = conn
            .query_row(
                "SELECT content_state FROM files WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content_state, STATE_FAILED);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
