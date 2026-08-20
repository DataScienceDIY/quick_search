//! Row-level write helpers that keep the FTS5 contentless table in sync with
//! `files`/`documents_text`.
//!
//! States (mirrors the `content_state` column):
//!
//! | value | meaning |
//! |------:|---------|
//! |     0 | pending |
//! |     1 | done    |
//! |     2 | failed  |
//! |     3 | not applicable (content only) |

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::mime::FileType;

pub const STATE_PENDING: i64 = 0;
pub const STATE_DONE: i64 = 1;
pub const STATE_FAILED: i64 = 2;
pub const STATE_NA: i64 = 3;

/// `prepare_cached` + `execute`, returning the affected row count. `what` is
/// lazy so the message is built only on the error path.
fn exec(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    what: impl FnOnce() -> String,
) -> Result<usize, String> {
    conn.prepare_cached(sql)
        .and_then(|mut stmt| stmt.execute(params))
        .map_err(|e| format!("{}: {}", what(), e))
}

/// Set a file's content state and clear any failure record with it, as one
/// operation: `list-failed` reads `failed_files` directly, so a stale entry
/// tells the user a file is broken after it stopped being.
/// [`set_content_failed`] — the one transition that *writes* a failure
/// record — does not route through here.
fn set_state_clearing_failure(
    tx: &Transaction<'_>,
    file_id: i64,
    state: i64,
    transition: &'static str,
) -> Result<(), String> {
    exec(
        tx,
        "UPDATE files SET content_state = ?1 WHERE id = ?2",
        params![state, file_id],
        || format!("{} content_state {}", transition, file_id),
    )?;
    exec(
        tx,
        "DELETE FROM failed_files WHERE file_id = ?1",
        params![file_id],
        || format!("clear failed_files {}", file_id),
    )?;
    Ok(())
}

/// Everything needed to insert a fresh file row.
#[derive(Debug, Clone)]
pub struct NewFile<'a> {
    pub name: &'a str,
    /// The containing directory, ending in the platform separator — see
    /// [`crate::file_handling::split_db_path`], which produces the pair.
    pub parent: &'a str,
    pub size: u64,
    pub mtime: u64,
    pub mime: Option<&'a str>,
    pub ftype: FileType,
    pub hash: Option<&'a [u8]>,
    /// `false` means the row is born `STATE_NA`; decided at walk time by
    /// [`crate::file_handling::content_extractable`].
    pub needs_content: bool,
}

impl NewFile<'_> {
    /// The file's path, for a log or error message. Not stored; see
    /// [`super::schema::SCHEMA_CURRENT`].
    fn path(&self) -> String {
        format!("{}{}", self.parent, self.name)
    }
}

/// Insert a new file row, returning its id. `content_state` comes from
/// `needs_content`; there is no separate basic state, because the row
/// existing *is* the basic-index state. `INSERT OR IGNORE`: a
/// `UNIQUE(parent, name)` collision returns `None` rather than aborting the
/// batch.
pub fn insert_file(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    let rows = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO files (
                name, parent, size, mtime, mime, type, content_state, hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .and_then(|mut stmt| {
            stmt.execute(params![
                f.name,
                f.parent,
                f.size as i64,
                f.mtime as i64,
                f.mime,
                f.ftype.bits() as i64,
                initial_content_state(f),
                f.hash,
            ])
        })
        .map_err(|e| format!("insert file {}: {}", f.path(), e))?;
    if rows == 0 {
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
/// content so the text-indexing pass re-processes it. Writes `size`, `mtime`,
/// `hash`, `mime`, `type` and `content_state` — and only those; `name` and
/// `parent` are the key it matches on, so they cannot change here.
pub fn update_file_basic(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    let id: Option<i64> = tx
        .prepare_cached(
            "UPDATE files
                SET size = ?1, mtime = ?2, hash = ?3, mime = ?4, type = ?5,
                    content_state = ?6
              WHERE parent = ?7 AND name = ?8
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
                    f.parent,
                    f.name,
                ],
                |r| r.get(0),
            )
            .optional()
        })
        .map_err(|e| format!("update file {}: {}", f.path(), e))?;
    let Some(id) = id else {
        return Ok(None);
    };
    remove_content_for_id(tx, id)?;
    Ok(Some(id))
}

/// Mark a file's content indexing as complete and write the extracted text
/// atomically. The plaintext feeds the contentless FTS5 tokenizer; when
/// `store_text` is `true` it is also stored zstd-compressed in
/// `documents_text` for snippet rendering (`false`: matches still work, but
/// result rows can't render snippets).
/// `text_zstd` is the already-compressed body for the `documents_text`
/// sidecar, or `None` to write no sidecar at all (an empty body, or
/// `store_text_for_snippets` off).
///
/// Compression is the caller's job, and deliberately so: it is the expensive
/// half of a content write, and this runs inside the writer's transaction
/// with the shared connection held. Callers on the indexing path compress a
/// whole batch through one [`DocEncoder`] *before* taking the lock, so the
/// transaction only binds finished blobs.
pub fn set_content_done(
    tx: &Transaction<'_>,
    file_id: i64,
    text: &str,
    text_zstd: Option<&[u8]>,
) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;

    // Contentless FTS5 still accepts values on INSERT — the tokenizer needs
    // them — it simply doesn't persist the raw column values.
    exec(
        tx,
        "INSERT INTO searchabletext(rowid, text) VALUES (?1, ?2)",
        params![file_id, text],
        || format!("insert FTS row {}", file_id),
    )?;

    // No sidecar row for empty body text (an audio file whose tags are all
    // empty, say) — the caller passes `None` for that.
    if let Some(compressed) = text_zstd {
        exec(
            tx,
            "INSERT INTO documents_text(file_id, text_zstd) VALUES (?1, ?2)",
            params![file_id, compressed],
            || format!("insert documents_text {}", file_id),
        )?;
    }

    set_state_clearing_failure(tx, file_id, STATE_DONE, "update DONE")
}

/// Reusable decode buffer and decompression context for the readers of
/// `documents_text` — the read side's mirror of [`DocEncoder`].
///
/// Shared by the cascade's full-text passes and by [`crate::live`], which
/// re-reads one row's body when a file under a visible result changes.
///
/// `zstd::decode_all` builds and tears down a `ZSTD_DCtx` *and* allocates a
/// fresh output `Vec` on every call, and it is called once per candidate row.
/// One context and one buffer, reused across a whole scan, make that a
/// per-scan cost instead of a per-row one.
pub struct DocDecoder {
    dctx: zstd::bulk::Decompressor<'static>,
    buf: Vec<u8>,
}

/// Where [`DocDecoder::decode`]'s buffer starts before it has seen a document.
/// Most extracted text is well under this, so the doubling below rarely runs.
const INITIAL_DOC_CAPACITY: usize = 64 * 1024;

/// Where the doubling stops. Stored text is capped at
/// `processing.maximum_text_size` (256 KiB by default), so this is far above
/// any legitimate document even if that setting is raised — past it, a failure
/// is a corrupt frame rather than a buffer that is too small.
const MAX_DOC_CAPACITY: usize = 64 * 1024 * 1024;

impl DocDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(DocDecoder {
            dctx: zstd::bulk::Decompressor::new().map_err(|e| e.to_string())?,
            buf: Vec::new(),
        })
    }

    /// Decompress `blob` and borrow the result as text.
    ///
    /// Returns `None` for a corrupt frame or non-UTF-8 content. Nothing is
    /// copied: the indexer stores UTF-8, so the bytes are borrowed in place
    /// rather than run through `String::from_utf8_lossy(..).into_owned()`,
    /// which duplicated the whole document even when it was already valid.
    pub fn decode(&mut self, blob: &[u8]) -> Option<&str> {
        self.buf.clear();
        // `decompress_to_buffer` writes into spare capacity and fails rather
        // than growing, so the room has to be there first.
        //
        // [`DocEncoder`] compresses through `ZSTD_compress2`, which knows the
        // whole input up front and records its length in the frame header, so
        // this reservation is normally exact and the loop below runs once.
        //
        // The loop is still the fallback, and it is not optional: a frame
        // written by a *stream*-based encoder carries no content size, and
        // falling back to `zstd::decode_all` for those looked harmless and was
        // not — it builds a streaming decoder per call, which measured as one
        // ~2.4 MiB allocation per document and 27 of the 30 GiB a fuzzy search
        // moved through the allocator. Growing and reusing this buffer instead
        // settles at the largest document in the scan within the first few
        // rows, after which decoding a row allocates nothing at all.
        if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(blob) {
            self.buf.reserve(usize::try_from(size).ok()?);
        }
        loop {
            if self.buf.capacity() == 0 {
                self.buf.reserve(INITIAL_DOC_CAPACITY);
            }
            match self.dctx.decompress_to_buffer(blob, &mut self.buf) {
                Ok(_) => break,
                // Too small, or corrupt — the bulk API cannot tell us which.
                // Growing is only worth trying while the buffer is still
                // smaller than any document could legitimately be.
                Err(_) if self.buf.capacity() < MAX_DOC_CAPACITY => {
                    let bigger = self.buf.capacity().saturating_mul(2);
                    self.buf.clear();
                    self.buf.reserve(bigger);
                }
                Err(_) => return None,
            }
        }
        std::str::from_utf8(&self.buf).ok()
    }
}

/// Level 3 hits ~3-5× on English prose at high throughput (hundreds of
/// MB/s); level 9+ would shave a few percent more at 10× the CPU cost, and
/// readers decompress far faster than writers compress.
const ZSTD_LEVEL: i32 = 3;

/// Reusable compression context for the `documents_text` sidecar — the write
/// side's mirror of the cascade's `DocDecoder`.
///
/// `zstd::encode_all` builds and tears down a `ZSTD_CCtx` — window, hash and
/// chain tables — on every call, and the writer calls it once per extracted
/// document. At 1 KiB, the size most documents actually are, that setup costs
/// more than the compression: 16.4 µs against 3.4 µs for the same bytes
/// through a context that already exists (`benches/index.rs`, group
/// `zstd_encode`). One encoder per batch makes it a per-batch cost.
pub struct DocEncoder(zstd::bulk::Compressor<'static>);

impl DocEncoder {
    pub fn new() -> Result<DocEncoder, String> {
        zstd::bulk::Compressor::new(ZSTD_LEVEL)
            .map(DocEncoder)
            .map_err(|e| format!("zstd encoder: {}", e))
    }

    /// Compress `text` for [`set_content_done`]'s `text_zstd` argument.
    pub fn encode(&mut self, text: &str) -> Result<Vec<u8>, String> {
        self.0
            .compress(text.as_bytes())
            .map_err(|e| format!("zstd encode: {}", e))
    }
}

/// Compress one body, for the writers that handle a single row.
///
/// The batch writers reuse one [`DocEncoder`] across a chunk and run it
/// outside the connection lock. The single-row paths — the watcher, and a
/// walk-time inline body — write one row per transaction, so there is no
/// batch to amortize a context over and this builds one for the document.
pub fn encode_one(text: &str, store_text: bool) -> Result<Option<Vec<u8>>, String> {
    if !store_text || text.is_empty() {
        return Ok(None);
    }
    DocEncoder::new()?.encode(text).map(Some)
}

/// The uncompressed size of a stored `documents_text` blob, read out of the
/// zstd frame header instead of from a column beside it.
///
/// [`DocEncoder`] compresses through `ZSTD_compress2`, which is handed the
/// whole document at once and writes its length into the frame header. A
/// `text_len` column would have stored that same number a second time for
/// every row, to serve one figure in the size report.
///
/// `None` for a frame that carries no content size — nothing this writer
/// produces — or a corrupt one. The caller only needs the frame *header*, so
/// `blob` may be a prefix of the stored value.
pub fn raw_text_len(blob: &[u8]) -> Option<u64> {
    zstd::zstd_safe::get_frame_content_size(blob).ok().flatten()
}

/// Mark a file's content extraction as failed. Keeps the basic row in place.
///
/// The reason is written once, to `failed_files` — which also carries the
/// timestamp, and which `list-failed` and `status` both read.
pub fn set_content_failed(tx: &Transaction<'_>, file_id: i64, reason: &str) -> Result<(), String> {
    let now = crate::log::now_unix() as i64;
    exec(
        tx,
        "UPDATE files SET content_state = ?1 WHERE id = ?2",
        params![STATE_FAILED, file_id],
        || format!("update content_state FAILED {}", file_id),
    )?;
    exec(
        tx,
        "INSERT OR REPLACE INTO failed_files(file_id, reason, ts) VALUES (?1, ?2, ?3)",
        params![file_id, reason, now],
        || format!("insert failed_files {}", file_id),
    )?;
    Ok(())
}

/// Mark content extraction as not applicable (e.g. binary format we don't
/// support). The file row still contributes to filename search.
pub fn set_content_na(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    set_state_clearing_failure(tx, file_id, STATE_NA, "update NA")
}

/// Delete a file row by path, keeping FTS in sync. Returns whether a row was
/// removed — including `false` for a string that cannot be a file's path at
/// all, which is not in the index by construction.
pub fn delete_file_by_path(tx: &Transaction<'_>, path: &str) -> Result<bool, String> {
    let Some((parent, name)) = crate::file_handling::split_db_path(path) else {
        return Ok(false);
    };
    let id: Option<i64> = tx
        .prepare_cached("DELETE FROM files WHERE parent = ?1 AND name = ?2 RETURNING id")
        .and_then(|mut stmt| {
            stmt.query_row(params![parent, name], |r| r.get(0))
                .optional()
        })
        .map_err(|e| format!("delete file {}: {}", path, e))?;
    let Some(id) = id else { return Ok(false) };
    remove_content_for_id(tx, id)?;
    Ok(true)
}

/// Delete every row whose parent falls in the half-open range `[lo, hi)`,
/// keeping the dependent tables in step. Returns how many `files` rows went.
///
/// Four statements regardless of how many files the range holds, and the
/// range is an index seek on `UNIQUE(files.parent, files.name)`. Build the
/// bounds with [`crate::file_handling::ExtractCursor::for_root`], which is
/// what makes them separator-correct — and note the range covers the root's
/// *own* files only because every stored parent ends in a separator (see
/// `dir_to_db_parent`).
pub fn delete_subtree(tx: &Transaction<'_>, lo: &str, hi: &str) -> Result<usize, String> {
    for (table, key) in DEPENDENT_TABLES {
        let sql = format!(
            "DELETE FROM {} WHERE {} IN \
             (SELECT id FROM files WHERE parent >= ?1 AND parent < ?2)",
            table, key
        );
        exec(tx, &sql, params![lo, hi], || {
            format!("delete {} under {}", table, lo)
        })?;
    }
    exec(
        tx,
        "DELETE FROM files WHERE parent >= ?1 AND parent < ?2",
        params![lo, hi],
        || format!("delete files under {}", lo),
    )
}

/// Delete every row whose parent falls in *none* of `ranges`. Returns how many
/// `files` rows went.
///
/// A scan of `files` rather than a seek, reserved for the one transition that
/// needs it: with `follow_symlinks` off, rows left by a followed symlink fall
/// outside every root's range and no walk will ever visit them again. An
/// empty `ranges` (no roots configured — e.g. a half-written config) deletes
/// nothing.
pub fn delete_outside_ranges(
    tx: &Transaction<'_>,
    ranges: &[(String, String)],
) -> Result<usize, String> {
    if ranges.is_empty() {
        return Ok(0);
    }
    let mut predicate = String::new();
    for i in 0..ranges.len() {
        if i > 0 {
            predicate.push_str(" AND ");
        }
        predicate.push_str(&format!(
            "NOT (parent >= ?{} AND parent < ?{})",
            i * 2 + 1,
            i * 2 + 2
        ));
    }
    let bounds: Vec<&String> = ranges.iter().flat_map(|(lo, hi)| [lo, hi]).collect();
    for (table, key) in DEPENDENT_TABLES {
        let sql = format!(
            "DELETE FROM {} WHERE {} IN (SELECT id FROM files WHERE {})",
            table, key, predicate
        );
        exec(tx, &sql, params_from_iter(bounds.iter()), || {
            format!("delete {} outside the roots", table)
        })?;
    }
    let sql = format!("DELETE FROM files WHERE {}", predicate);
    exec(tx, &sql, params_from_iter(bounds.iter()), || {
        "delete files outside the roots".to_string()
    })
}

/// The tables a file id owns, in the order they must be cleared: everything
/// keyed to `files.id` first, then `files` itself. Not left to `ON DELETE
/// CASCADE`: `searchabletext` is an FTS5 virtual table with no foreign key at
/// all, and cascade only fires on connections with `PRAGMA foreign_keys` on.
const DEPENDENT_TABLES: [(&str, &str); 3] = [
    ("searchabletext", "rowid"),
    ("documents_text", "file_id"),
    ("failed_files", "file_id"),
];

/// How many ids [`delete_ids`] binds into one statement — fixed so
/// `prepare_cached` sees a bounded set of distinct SQL texts.
const DELETE_IDS_CHUNK: usize = 512;

/// `?,?,…` for an `IN (…)` clause binding `n` values.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Delete the given file ids and everything keyed to them. Returns how many
/// `files` rows went. For rows chosen by a predicate no SQL range can express
/// (a glob ignore pattern, say); five statements per [`DELETE_IDS_CHUNK`] ids
/// rather than five per file.
pub fn delete_ids(tx: &Transaction<'_>, ids: &[i64]) -> Result<usize, String> {
    let mut removed = 0;
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let placeholders = placeholders(chunk.len());
        for (table, key) in DEPENDENT_TABLES {
            let sql = format!("DELETE FROM {} WHERE {} IN ({})", table, key, placeholders);
            exec(tx, &sql, params_from_iter(chunk.iter()), || {
                format!("delete {} for {} ids", table, chunk.len())
            })?;
        }
        let sql = format!("DELETE FROM files WHERE id IN ({})", placeholders);
        removed += exec(tx, &sql, params_from_iter(chunk.iter()), || {
            format!("delete {} file rows", chunk.len())
        })?;
    }
    Ok(removed)
}

/// Every indexed file directly inside `parent`, as `name -> mtime`.
///
/// `parent` must be in stored spelling — trailing separator and all; build it
/// with [`crate::file_handling::dir_to_db_parent`]. One `idx_files_parent`
/// range lookup for the names, then a row fetch each for the mtimes; see the
/// index's own comment for why that is the shape it is.
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

/// A row the content pass has yet to extract: `(id, name, path, mime)`. The
/// path is reassembled here rather than stored — see
/// [`super::schema::SCHEMA_CURRENT`] — because the pass opens the file by it.
pub type PendingContentRow = (i64, String, String, Option<String>);

/// One page of rows still awaiting content extraction under `cursor`'s range,
/// ordered by id.
///
/// Keyset, not `OFFSET`: each page is an index seek, and because the cursor
/// only moves forward a row is served exactly once even though the writer is
/// concurrently flipping `content_state` behind the reader.
pub fn pending_content_page(
    conn: &Connection,
    cursor: &crate::file_handling::ExtractCursor,
    max_size: i64,
    limit: i64,
) -> Result<Vec<PendingContentRow>, String> {
    let mut stmt = conn
        .prepare_cached(
            // `INDEXED BY` rather than a hint, because the planner gets this
            // one wrong exactly when it costs most. Left to itself it takes
            // `idx_files_parent` for the range and then sorts the survivors
            // into a temp b-tree to satisfy `ORDER BY id` — which means every
            // page walks the whole root's range and fetches each row's heap
            // entry to test `content_state`. At `FEED_PAGE` rows per page that
            // is quadratic over a run. The partial index below is already
            // id-ordered, so it answers `id > ?` and the ORDER BY together and
            // holds only pending rows. Measured on 500k rows with everything
            // pending — the first index of a tree, i.e. the case that matters:
            // 127 ms/page against 0.1 ms/page.
            //
            // The planner only prefers it once pending rows are a small
            // minority, and never before ANALYZE has run at all, which is why
            // this cannot be left to statistics.
            "SELECT id, parent, name, mime FROM files INDEXED BY idx_files_content_pending
              WHERE content_state = 0 AND size <= ?1 AND id > ?2
                AND parent >= ?3 AND parent < ?4
              ORDER BY id
              LIMIT ?5",
        )
        .map_err(|e| format!("prepare pending content query: {}", e))?;
    let rows = stmt
        .query_map(
            params![max_size, cursor.last_id, cursor.lo, cursor.hi, limit],
            |row| {
                let parent: String = row.get(1)?;
                let name: String = row.get(2)?;
                let path = format!("{}{}", parent, name);
                Ok((
                    row.get::<_, i64>(0)?,
                    name,
                    path,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|e| format!("query pending content: {}", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read pending content row: {}", e))
}

/// A stored row as the scope reconciler sees it: enough to decide both
/// whether the path is still in scope and whether its content still is.
///
/// `path` is `parent` and `name` joined, kept alongside them because the
/// reconciler tests it as a [`std::path::Path`] while the cursor resumes from
/// the key.
#[derive(Debug, Clone)]
pub struct ScopeRow {
    pub id: i64,
    pub path: String,
    pub parent: String,
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    pub content_state: i64,
}

/// How many files the index holds.
pub fn row_count(conn: &Connection) -> Result<usize, String> {
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as usize)
        .map_err(|e| format!("count indexed files: {}", e))
}

/// What one root holds: rows under it, and how many of those are searchable
/// by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootCounts {
    pub files: i64,
    /// Rows carrying a `searchabletext` entry.
    pub fts: i64,
}

/// Count the rows in the half-open parent range `[lo, hi)` and, in the same
/// pass, how many of them have a full-text row.
///
/// `content_state = STATE_DONE` *is* "has a `searchabletext` row":
/// [`set_content_done`] holds the only insert into that table and is what
/// writes the state, and [`remove_content_for_id`] clears the two together.
/// Asking `files` is what makes both figures one statement — the FTS table is
/// contentless and keyed by `rowid`, so it has no path to range-scan on.
///
/// One statement, but not a cheap one: `content_state` is not carried by the
/// `idx_files_parent` index the range seeks on, so every row in the range is
/// fetched. Call it where a run has just read those rows anyway, not on a
/// cadence.
pub fn count_root(conn: &Connection, lo: &str, hi: &str) -> Result<RootCounts, String> {
    conn.prepare_cached(
        "SELECT COUNT(*), COALESCE(SUM(content_state = ?3), 0) FROM files
          WHERE parent >= ?1 AND parent < ?2",
    )
    .and_then(|mut stmt| {
        stmt.query_row(params![lo, hi, STATE_DONE], |r| {
            Ok(RootCounts {
                files: r.get(0)?,
                fts: r.get(1)?,
            })
        })
    })
    .map_err(|e| format!("count root {}: {}", lo, e))
}

/// One page of rows sorting after `(after_parent, after_name)` and inside the
/// parent range ending at `hi`, in `(parent, name)` order.
///
/// Keyset on the `idx_files_parent` key itself: every page is an index walk
/// with no sort step, and a row is served at most once even though the caller
/// is deleting behind the reader. Seed the cursor with `(lo, "")` — no name is
/// empty, so that lands exactly on the first row of the range.
///
/// The row-value comparison is what keeps it one seek; spelled out as
/// `parent > ? OR (parent = ? AND name > ?)` the planner is free to scan.
pub fn rows_in_range_page(
    conn: &Connection,
    after_parent: &str,
    after_name: &str,
    hi: &str,
    limit: i64,
) -> Result<Vec<ScopeRow>, String> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent, name, size, mime, content_state FROM files
              WHERE (parent, name) > (?1, ?2) AND parent < ?3
              ORDER BY parent, name
              LIMIT ?4",
        )
        .map_err(|e| format!("prepare range page: {}", e))?;
    let rows = stmt
        .query_map(params![after_parent, after_name, hi, limit], |row| {
            let parent: String = row.get(1)?;
            let name: String = row.get(2)?;
            Ok(ScopeRow {
                id: row.get(0)?,
                path: format!("{}{}", parent, name),
                name,
                parent,
                size: row.get::<_, i64>(3)?.max(0) as u64,
                mime: row.get(4)?,
                content_state: row.get(5)?,
            })
        })
        .map_err(|e| format!("query range page after {}: {}", after_parent, e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read range page row: {}", e))
}

/// Drop the stored text of the given file ids, leaving their FTS row and
/// `files` row intact: full-text search keeps working, only the
/// snippet/occurrence source goes away.
pub fn drop_stored_text(tx: &Transaction<'_>, ids: &[i64]) -> Result<usize, String> {
    let mut removed = 0;
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let sql = format!(
            "DELETE FROM documents_text WHERE file_id IN ({})",
            placeholders(chunk.len())
        );
        removed += exec(tx, &sql, params_from_iter(chunk.iter()), || {
            format!("drop stored text for {} ids", chunk.len())
        })?;
    }
    Ok(removed)
}

/// Put a file's content back in the pending queue without touching its row's
/// metadata. For a config change that widens what gets extracted: the file
/// itself has not changed, but its content must be produced again.
pub fn reset_content_pending(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;
    set_state_clearing_failure(tx, file_id, STATE_PENDING, "reset pending")
}

/// The stored mtime for one exact path, or `None` if it isn't indexed. For
/// files whose parent isn't the directory being read (a resolved symlink
/// target), where [`dir_rows`] would not have them.
pub fn mtime_for_path(conn: &Connection, path: &str) -> Result<Option<u64>, String> {
    let Some((parent, name)) = crate::file_handling::split_db_path(path) else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare_cached("SELECT mtime FROM files WHERE parent = ?1 AND name = ?2")
        .map_err(|e| format!("prepare mtime lookup for {}: {}", path, e))?;
    stmt.query_row(params![parent, name], |r| r.get::<_, i64>(0))
        .optional()
        .map(|o| o.map(|m| m.max(0) as u64))
        .map_err(|e| format!("mtime lookup for {}: {}", path, e))
}

/// Distinct `parent` values within the half-open range `[lo, hi)`, streamed to
/// `f` so nothing proportional to the tree is materialized. `idx_files_parent`
/// makes this an index-only scan.
///
/// The root's own directory is included: its stored parent is `root + SEP`,
/// which is exactly `lo`. It was not, back when the same bounds were applied
/// to a `path` column and the root's parent was spelled without the trailing
/// separator.
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

/// Paths of every file directly inside `parent`, which must carry its trailing
/// separator — so the join below is a concatenation.
pub fn paths_in_dir(conn: &Connection, parent: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare_cached("SELECT name FROM files WHERE parent = ?1")
        .map_err(|e| format!("prepare paths in {}: {}", parent, e))?;
    let rows = stmt
        .query_map(params![parent], |r| {
            r.get::<_, String>(0)
                .map(|name| format!("{}{}", parent, name))
        })
        .map_err(|e| format!("query paths in {}: {}", parent, e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read path under {}: {}", parent, e))
}

/// Remove the FTS row and the compressed text blob for a given file id. Does
/// not touch the `files` row itself. Idempotent — a missing row is fine.
pub fn remove_content_for_id(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    // `contentless_delete=1` on the FTS5 table makes this work without
    // re-supplying the old column values (it tombstones the rowid).
    //
    // Spelled out rather than built from a (table, key) table: this runs for
    // every extracted document and every changed file, and `format!`ing two
    // constant strings per call also handed `prepare_cached` two freshly
    // allocated keys to hash.
    for (what, sql) in [
        (
            "searchabletext",
            "DELETE FROM searchabletext WHERE rowid = ?1",
        ),
        (
            "documents_text",
            "DELETE FROM documents_text WHERE file_id = ?1",
        ),
    ] {
        exec(tx, sql, params![file_id], || {
            format!("delete {} for {}", what, file_id)
        })?;
    }
    Ok(())
}

/// Free pages, as a percentage of the file, that make a [`maintain`] VACUUM
/// worth its cost: rewriting a multi-gigabyte index to reclaim a few
/// megabytes is minutes of I/O for no gain.
const VACUUM_MIN_SLACK_PERCENT: i64 = 20;

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
/// the file and left on.
pub fn checkpoint_and_close(conn: Connection) {
    if let Err(e) = checkpoint_truncate(&conn) {
        crate::log_warn!("{}", e);
    }
    drop(conn);
}

/// Read a `PRAGMA` that reports a number.
///
/// Not simply `r.get::<i64>(0)`, because SQLCipher does not always answer with
/// one. On a **keyed** connection it intercepts `PRAGMA page_size`, answers
/// with `cipher_page_size` instead, and returns that as TEXT — so asking for an
/// integer fails with a type error. Unencrypted it is an INTEGER as usual,
/// which is why this only ever broke protected installs, and only in
/// [`maintain`]: every index with a password set skipped its VACUUM *and* its
/// `PRAGMA optimize` from the moment the free-space check was added.
///
/// A value that is neither is an error rather than a guess — the callers here
/// size a disk-space check with it.
pub(super) fn pragma_number(conn: &Connection, pragma: &str) -> Result<i64, String> {
    use rusqlite::types::ValueRef;
    conn.query_row(&format!("PRAGMA {}", pragma), [], |r| {
        Ok(match r.get_ref(0)? {
            ValueRef::Integer(n) => Some(n),
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            _ => None,
        })
    })
    .map_err(|e| format!("read {}: {}", pragma, e))?
    .ok_or_else(|| format!("read {}: not a number", pragma))
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
/// Run on a connection from [`crate::db::open::open_maintenance`], never the
/// indexer's (see [`super::schema::PRAGMAS_MAINTENANCE`]).
///
/// `db_dir` is where the temporary database goes, and it must be the index's
/// own directory: `temp_store = FILE` alone resolves through `SQLITE_TMPDIR`,
/// `TMPDIR`, `/var/tmp`, then `/tmp` — and `/tmp` is a RAM-backed tmpfs on
/// many Linux systems.
///
/// Peak transient space on that volume is roughly three times the index: the
/// original, the replacement being built beside it, and the log that VACUUM's
/// copy-back runs through. Running out is a failed VACUUM, not a damaged
/// index — the transaction rolls back.
pub fn maintain(conn: &Connection, db_dir: &str) -> Result<bool, String> {
    // Best-effort: compaction does not need the log empty to start.
    if let Err(e) = checkpoint_truncate(conn) {
        crate::log_warn!("{}", e);
    }

    let page_count = pragma_number(conn, "page_count")?;
    let freelist = pragma_number(conn, "freelist_count")?;

    let worth_it = freelist * 100 >= page_count * VACUUM_MIN_SLACK_PERCENT;
    // The doc comment above puts VACUUM's peak transient need at roughly three
    // times the index. Checking first turns "the volume filled up mid-rebuild"
    // into a skipped compaction: a rollback is the *good* outcome there, and
    // the bad one is that writes to the `-shm` mmap on a full filesystem come
    // back as SIGBUS rather than as an error — see
    // `indexing::pipeline::DISK_FLOOR`. Unknown free space is not a reason to
    // skip.
    let page_size = pragma_number(conn, "page_size")?;
    let needed = (page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64) * 3;
    let room = match crate::platform::available_space(std::path::Path::new(db_dir)) {
        Some(free) if free < needed => {
            crate::log_warn!(
                "skipping VACUUM: it needs about {} MiB free in {} and there is {} MiB",
                needed / (1024 * 1024),
                db_dir,
                free / (1024 * 1024)
            );
            false
        }
        _ => true,
    };

    let vacuumed = worth_it && room;
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

    // Re-analyses only the tables whose shape has drifted far enough to
    // matter, so it is close to free on a run that changed little.
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

/// The `schema_info` key prefixes holding per-root figures. Every one of them
/// is swept by [`prune_root_stats`], so a new prefix belongs in this list or a
/// de-configured root leaves it behind forever.
const ROOT_STAT_PREFIXES: [&str; 2] = ["walk_count:", "counts:"];

/// `schema_info` key holding one root's figure of the given kind.
fn root_key(prefix: &str, root: &str) -> String {
    format!("{}{}", prefix, root)
}

/// `schema_info` key holding one root's last known file count.
fn walk_count_key(root: &str) -> String {
    root_key(ROOT_STAT_PREFIXES[0], root)
}

/// `schema_info` key holding one root's last completed run's [`RootCounts`].
fn counts_key(root: &str) -> String {
    root_key(ROOT_STAT_PREFIXES[1], root)
}

/// How many files the last clean walk of `root` reported — the progress bar's
/// denominator. Absent means the root has never been walked to completion.
///
/// Last run's *file* count rather than a tree-entry count: entry counts
/// include directories and ignore-pruned subtrees and so read high (over 1.6x
/// on a home directory). See
/// [`crate::indexing::RootProgress::walk_denominator`].
pub fn get_root_walk_count(conn: &Connection, root: &str) -> Option<usize> {
    conn.query_row(
        "SELECT value FROM schema_info WHERE key = ?1",
        params![walk_count_key(root)],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|v| v.parse().ok())
}

/// Record `n` as `root`'s file count, for the next run's progress bar.
/// Written only after a walk that finished cleanly: a partial walk's count
/// would leave every later run dividing by a number that is too small.
pub fn set_root_walk_count(conn: &Connection, root: &str, n: usize) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_info(key, value) VALUES (?1, ?2)",
        params![walk_count_key(root), n.to_string()],
    )
    .map_err(|e| format!("write walk count for {}: {}", root, e))?;
    Ok(())
}

/// What the last completed run counted under `root`, if one has finished
/// since the root was configured. Absent — never indexed, cleared, or a
/// value this build cannot parse — reads as `None`, like the walk count.
pub fn get_root_counts(conn: &Connection, root: &str) -> Option<RootCounts> {
    let stored: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key = ?1",
            params![counts_key(root)],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()?;
    let (files, fts) = stored.split_once(',')?;
    Some(RootCounts {
        files: files.parse().ok()?,
        fts: fts.parse().ok()?,
    })
}

/// Record what `root` holds, for the folder list to show once the run that
/// counted it is over.
///
/// Written only at the end of a run that completed: a stopped one has counted
/// part of a tree it was still changing, and the previous figure is closer to
/// the truth than that.
pub fn set_root_counts(conn: &Connection, root: &str, counts: RootCounts) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_info(key, value) VALUES (?1, ?2)",
        params![counts_key(root), format!("{},{}", counts.files, counts.fts)],
    )
    .map_err(|e| format!("write counts for {}: {}", root, e))?;
    Ok(())
}

/// Forget the stored figures of roots that are no longer configured, so a
/// root removed and later re-added does not start from stale ones.
pub fn prune_root_stats(conn: &Connection, keep: &[String]) -> Result<(), String> {
    let keep: std::collections::HashSet<String> = ROOT_STAT_PREFIXES
        .iter()
        .flat_map(|prefix| keep.iter().map(move |r| root_key(prefix, r)))
        .collect();
    // Filtered here rather than with a `LIKE` per prefix: `schema_info` holds
    // a handful of keys plus these, and a SQL pattern list would be a second
    // spelling of `ROOT_STAT_PREFIXES` to keep in step with the first.
    let mut stmt = conn
        .prepare("SELECT key FROM schema_info")
        .map_err(|e| format!("read root stats: {}", e))?;
    let stored: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("read root stats: {}", e))?
        .filter_map(|r| r.ok())
        .filter(|k| ROOT_STAT_PREFIXES.iter().any(|p| k.starts_with(p)))
        .collect();
    drop(stmt);
    for key in stored.iter().filter(|k| !keep.contains(*k)) {
        conn.execute("DELETE FROM schema_info WHERE key = ?1", params![key])
            .map_err(|e| format!("drop root stat {}: {}", key, e))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "repo_tests.rs"]
mod tests;
