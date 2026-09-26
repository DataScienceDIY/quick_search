//! Row-level write helpers that keep the FTS5 contentless table in sync with
//! `files`/`documents_text`.

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::file_handling::{ContentOutcome, ExtractCursor, RootRange};
use crate::mime::FileType;

pub const STATE_PENDING: i64 = 0;
/// Content extracted. A file has a `searchabletext` row exactly when it is in
/// this state, across every transition; [`count_root`], [`delete_ids`] and the
/// reconciler's batched clears rely on it.
pub const STATE_DONE: i64 = 1;
/// Extraction failed. A file has a `failed_files` row exactly when it is in
/// this state.
pub const STATE_FAILED: i64 = 2;
pub const STATE_NA: i64 = 3;

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

#[derive(Debug, Clone)]
pub struct NewFile<'a> {
    pub name: &'a str,
    /// The containing directory, ending in the platform separator; produced
    /// by [`crate::file_handling::split_db_path`].
    pub parent: &'a str,
    pub size: u64,
    pub mtime: u64,
    pub mime: Option<&'a str>,
    pub ftype: FileType,
    pub hash: Option<&'a [u8]>,
    /// `false` means the row is born `STATE_NA`.
    pub needs_content: bool,
}

impl NewFile<'_> {
    fn path(&self) -> String {
        format!("{}{}", self.parent, self.name)
    }
}

/// Insert a new file row, returning its id; `None` on a unique-key collision.
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
    Ok((rows > 0).then(|| tx.last_insert_rowid()))
}

fn initial_content_state(f: &NewFile<'_>) -> i64 {
    if f.needs_content {
        STATE_PENDING
    } else {
        STATE_NA
    }
}

/// Update a file's metadata in place and reset its content state, clearing
/// any extracted content. `None` if no row matches.
///
/// Clearing depends on the *stored* state: only [`STATE_DONE`] holds content
/// and only [`STATE_FAILED`] a failure record, so other states cost one
/// statement instead of four (1.24x on NA→NA, `benches/index.rs`
/// `update_narrowing`). `content_state` is not in the `SET` list because
/// `RETURNING` reports the post-update row, which would hide the old state.
pub fn update_file_basic(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    let row: Option<(i64, i64)> = tx
        .prepare_cached(
            "UPDATE files
                SET size = ?1, mtime = ?2, hash = ?3, mime = ?4, type = ?5
              WHERE parent = ?6 AND name = ?7
          RETURNING id, content_state",
        )
        .and_then(|mut stmt| {
            stmt.query_row(
                params![
                    f.size as i64,
                    f.mtime as i64,
                    f.hash,
                    f.mime,
                    f.ftype.bits() as i64,
                    f.parent,
                    f.name,
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .map_err(|e| format!("update file {}: {}", f.path(), e))?;
    let Some((id, old_state)) = row else {
        return Ok(None);
    };
    match old_state {
        STATE_DONE => remove_content_for_id(tx, id)?,
        // Without this, a changed file that stops needing content keeps
        // reading as "failed" in `list-failed` forever.
        STATE_FAILED => exec(
            tx,
            "DELETE FROM failed_files WHERE file_id = ?1",
            params![id],
            || format!("clear failed_files {}", id),
        )
        .map(|_| ())?,
        _ => {}
    }
    let new_state = initial_content_state(f);
    if old_state != new_state {
        // The FAILED arm above already took the failure record, and no
        // other state can hold one.
        exec(
            tx,
            "UPDATE files SET content_state = ?1 WHERE id = ?2",
            params![new_state, id],
            || format!("update reset content_state {}", id),
        )?;
    }
    Ok(Some(id))
}

/// Move a [`STATE_PENDING`] row to `state`; `false`, writing nothing, when
/// the row is no longer pending (or no longer exists).
fn claim_pending(tx: &Transaction<'_>, file_id: i64, state: i64) -> Result<bool, String> {
    let changed = exec(
        tx,
        "UPDATE files SET content_state = ?1 WHERE id = ?2 AND content_state = ?3",
        params![state, file_id, STATE_PENDING],
        || format!("set content_state {} on {}", state, file_id),
    )?;
    Ok(changed > 0)
}

/// Record the outcome of extracting a pending row: the one way content state
/// leaves [`STATE_PENDING`]. `text_zstd` is the pre-compressed
/// `documents_text` body for a `Done` outcome, or `None` for no sidecar;
/// compress before taking the writer's lock. Returns whether the row was
/// still pending, and so whether anything was written.
///
/// Pending is also what makes it cheap: a pending row provably holds no
/// posting, stored body or failure record (see [`STATE_DONE`] and
/// [`STATE_FAILED`]), so nothing is cleared first. A row that stopped being
/// pending while its extraction was in flight (stored by another route, or
/// deleted by stale cleanup) is left alone: writing it anyway would leave a
/// posting with no `files` row behind it, which a reused rowid inherits.
pub fn complete_pending(
    tx: &Transaction<'_>,
    file_id: i64,
    outcome: &ContentOutcome,
    text_zstd: Option<&[u8]>,
) -> Result<bool, String> {
    match outcome {
        ContentOutcome::Done { text } => store_pending_text(tx, file_id, text, text_zstd),
        ContentOutcome::NotApplicable => claim_pending(tx, file_id, STATE_NA),
        ContentOutcome::Failed(reason) => {
            if !claim_pending(tx, file_id, STATE_FAILED)? {
                return Ok(false);
            }
            exec(
                tx,
                "INSERT INTO failed_files(file_id, reason, ts) VALUES (?1, ?2, ?3)",
                params![file_id, reason, crate::log::now_unix() as i64],
                || format!("insert failed_files {}", file_id),
            )?;
            Ok(true)
        }
    }
}

/// [`complete_pending`] with extracted text, for callers holding it borrowed.
pub fn store_pending_text(
    tx: &Transaction<'_>,
    file_id: i64,
    text: &str,
    text_zstd: Option<&[u8]>,
) -> Result<bool, String> {
    if !claim_pending(tx, file_id, STATE_DONE)? {
        return Ok(false);
    }
    // Contentless FTS5 takes the text on INSERT for the tokenizer and does
    // not store it.
    exec(
        tx,
        "INSERT INTO searchabletext(rowid, text) VALUES (?1, ?2)",
        params![file_id, text],
        || format!("insert FTS row {}", file_id),
    )?;
    if let Some(compressed) = text_zstd {
        exec(
            tx,
            "INSERT INTO documents_text(file_id, text_zstd) VALUES (?1, ?2)",
            params![file_id, compressed],
            || format!("insert documents_text {}", file_id),
        )?;
    }
    Ok(true)
}

/// Reusable decode buffer and context for the readers of `documents_text`;
/// the read side's mirror of [`DocEncoder`]. Measured: `zstd::decode_all`
/// builds a context and a `Vec` per row.
pub struct DocDecoder {
    dctx: zstd::bulk::Decompressor<'static>,
    buf: Vec<u8>,
}

const INITIAL_DOC_CAPACITY: usize = 64 * 1024;

/// Where the doubling stops. Far above any legitimate document, so a failure
/// past it is a corrupt frame, not a buffer that is too small.
const MAX_DOC_CAPACITY: usize = 64 * 1024 * 1024;

impl DocDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(DocDecoder {
            dctx: zstd::bulk::Decompressor::new().map_err(|e| e.to_string())?,
            buf: Vec::new(),
        })
    }

    /// Decompress `blob` and borrow the result as text; `None` for a corrupt
    /// frame or non-UTF-8 content.
    pub fn decode(&mut self, blob: &[u8]) -> Option<&str> {
        self.buf.clear();
        // `decompress_to_buffer` writes into spare capacity and fails rather
        // than growing, so the room has to be there first. A frame with no
        // content size (any stream encoder's) takes the growth loop below;
        // `zstd::decode_all` would rebuild a decoder and buffer per row.
        if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(blob) {
            // Clamped: a hostile header can ask for terabytes, and `reserve`
            // answers that by aborting the process.
            self.buf
                .reserve(usize::try_from(size).ok()?.min(MAX_DOC_CAPACITY));
        }
        if self.buf.capacity() == 0 {
            self.buf.reserve(INITIAL_DOC_CAPACITY);
        }
        // Too small, or corrupt: the bulk API cannot say which.
        while self.dctx.decompress_to_buffer(blob, &mut self.buf).is_err() {
            if self.buf.capacity() >= MAX_DOC_CAPACITY {
                return None;
            }
            let bigger = self.buf.capacity().saturating_mul(2);
            self.buf.clear();
            self.buf.reserve(bigger);
        }
        std::str::from_utf8(&self.buf).ok()
    }
}

/// Measured (`benches/index.rs`): level 9+ shaves a few percent at 10× the CPU.
const ZSTD_LEVEL: i32 = 3;

/// Reusable compression context for the `documents_text` sidecar; one per
/// batch. Measured (`benches/index.rs`, group `zstd_encode`).
pub struct DocEncoder {
    ctx: zstd::bulk::Compressor<'static>,
    /// One row's compressed output, reused. `zstd`'s `WriteBuf` for `Vec`
    /// writes from offset 0 and sets the length, so a body cannot be
    /// compressed straight into a shared arena; it lands here and is copied
    /// across, with no allocation once both buffers have grown.
    row: Vec<u8>,
}

impl DocEncoder {
    pub fn new() -> Result<DocEncoder, String> {
        Ok(DocEncoder {
            ctx: zstd::bulk::Compressor::new(ZSTD_LEVEL)
                .map_err(|e| format!("zstd encoder: {}", e))?,
            row: Vec::new(),
        })
    }

    pub fn encode(&mut self, text: &str) -> Result<Vec<u8>, String> {
        self.ctx
            .compress(text.as_bytes())
            .map_err(|e| format!("zstd encode: {}", e))
    }

    /// Append the compressed form of `text` to `arena`, returning where it
    /// landed, so a whole batch's bodies share one allocation instead of one
    /// `Vec` per document.
    pub fn encode_into(
        &mut self,
        text: &str,
        arena: &mut Vec<u8>,
    ) -> Result<std::ops::Range<usize>, String> {
        self.row.clear();
        // `compress_to_buffer` writes into the buffer's capacity and fails
        // rather than growing it, so the room has to be there first.
        self.row.reserve(zstd::zstd_safe::compress_bound(text.len()));
        self.ctx
            .compress_to_buffer(text.as_bytes(), &mut self.row)
            .map_err(|e| format!("zstd encode: {}", e))?;
        let start = arena.len();
        arena.extend_from_slice(&self.row);
        Ok(start..arena.len())
    }
}

/// Compress one body, for the writers that handle a single row; `None` for
/// an empty one, which gets no `documents_text` row.
pub fn encode_one(text: &str) -> Result<Option<Vec<u8>>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    DocEncoder::new()?.encode(text).map(Some)
}

/// The uncompressed size of a stored `documents_text` blob, from the zstd
/// frame header. Only the *header* is read, so `blob` may be a prefix.
pub fn raw_text_len(blob: &[u8]) -> Option<u64> {
    zstd::zstd_safe::get_frame_content_size(blob).ok().flatten()
}

/// Delete a file row by path, keeping FTS in sync; returns whether a row went.
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

/// The stored id and `content_state` for one exact path, or `None` if the
/// path is not indexed. Read it in the transaction whose [`delete_ids`] call
/// it feeds, so `with_postings` reflects the state at delete time.
pub fn id_and_state_for_path(conn: &Connection, path: &str) -> Result<Option<(i64, i64)>, String> {
    let Some((parent, name)) = crate::file_handling::split_db_path(path) else {
        return Ok(None);
    };
    conn.prepare_cached("SELECT id, content_state FROM files WHERE parent = ?1 AND name = ?2")
        .and_then(|mut stmt| {
            stmt.query_row(params![parent, name], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(|e| format!("look up {}: {}", path, e))
}

/// Delete every row in `range`, the root's own files included.
pub fn delete_subtree(tx: &Transaction<'_>, range: &RootRange) -> Result<usize, String> {
    delete_files_matching(
        tx,
        "parent >= ?1 AND parent < ?2",
        &[&range.lo, &range.hi],
        &format!("under {}", range.lo),
    )
}

/// Delete the `files` rows matching `files_where`, tombstoning their
/// `searchabletext` rows first; returns how many `files` rows went.
///
/// `documents_text` and `failed_files` go by `ON DELETE CASCADE` (every
/// profile sets `foreign_keys = ON`); FTS5 cannot cascade, so that delete
/// stays explicit. Not narrowed to [`STATE_DONE`] as [`delete_ids`] is: from
/// a range, reading `content_state` would mean a second pass over every
/// candidate to save one small `%_docsize` seek each. Re-measure with
/// `examples/pruneprobe.rs` before changing this.
fn delete_files_matching(
    tx: &Transaction<'_>,
    files_where: &str,
    params: &[&dyn rusqlite::ToSql],
    what: &str,
) -> Result<usize, String> {
    let sql = format!(
        "DELETE FROM searchabletext WHERE rowid IN (SELECT id FROM files WHERE {})",
        files_where
    );
    exec(tx, &sql, params_from_iter(params.iter()), || {
        format!("delete searchabletext {}", what)
    })?;
    let sql = format!("DELETE FROM files WHERE {}", files_where);
    exec(tx, &sql, params_from_iter(params.iter()), || {
        format!("delete files {}", what)
    })
}

/// Delete every row whose parent falls in none of `ranges`. A full scan of
/// `files`, for the one transition that needs it: once `follow_symlinks` is
/// off, rows a followed symlink left fall outside every root's range and no
/// walk visits them again.
pub fn delete_outside_ranges(
    tx: &Transaction<'_>,
    ranges: &[(String, String)],
) -> Result<usize, String> {
    if ranges.is_empty() {
        return Ok(0);
    }
    let predicate = (0..ranges.len())
        .map(|i| format!("NOT (parent >= ?{} AND parent < ?{})", i * 2 + 1, i * 2 + 2))
        .collect::<Vec<_>>()
        .join(" AND ");
    let bounds: Vec<&dyn rusqlite::ToSql> = ranges
        .iter()
        .flat_map(|(lo, hi)| [lo as &dyn rusqlite::ToSql, hi as &dyn rusqlite::ToSql])
        .collect();
    delete_files_matching(tx, &predicate, &bounds, "outside the roots")
}

/// Fixed chunk size so `prepare_cached` sees a bounded set of SQL texts.
const DELETE_IDS_CHUNK: usize = 512;

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Run `prefix` plus `?,…,?)` once per [`DELETE_IDS_CHUNK`] of `ids`,
/// returning the rows changed in total. `prefix` ends in `IN (`; `what` names
/// a failed chunk from its length.
fn exec_chunked(
    tx: &Transaction<'_>,
    prefix: &str,
    ids: &[i64],
    what: impl Fn(usize) -> String,
) -> Result<usize, String> {
    let mut changed = 0;
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let sql = format!("{}{})", prefix, placeholders(chunk.len()));
        changed += exec(tx, &sql, params_from_iter(chunk.iter()), || {
            what(chunk.len())
        })?;
    }
    Ok(changed)
}

/// Delete `ids` and everything keyed to them; returns how many `files` rows
/// went.
///
/// `with_postings` must be the subset of `ids` whose stored state is
/// [`STATE_DONE`], the only rows with an FTS entry to tombstone. The caller
/// already knows `content_state` from the page it read; looking it up here
/// would cost a row fetch per id, and tombstoning every id a `%_docsize` seek
/// per id. An id missing from `with_postings` keeps its posting, so the
/// deleted file keeps appearing in results.
pub fn delete_ids(
    tx: &Transaction<'_>,
    ids: &[i64],
    with_postings: &[i64],
) -> Result<usize, String> {
    exec_chunked(
        tx,
        "DELETE FROM searchabletext WHERE rowid IN (",
        with_postings,
        |n| format!("delete searchabletext for {} ids", n),
    )?;
    exec_chunked(tx, "DELETE FROM files WHERE id IN (", ids, |n| {
        format!("delete {} file rows", n)
    })
}

/// Set `content_state` on many rows at once, leaving everything else alone.
///
/// No `failed_files` sweep: a caller that knows the stored states clears the
/// few [`STATE_FAILED`] rows with [`clear_failed_for_ids`], and their content
/// with [`clear_content_for_ids`].
pub fn set_content_state(tx: &Transaction<'_>, ids: &[i64], state: i64) -> Result<usize, String> {
    let mut changed = 0;
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let sql = format!(
            "UPDATE files SET content_state = ?1 WHERE id IN ({})",
            placeholders(chunk.len())
        );
        let params = params_from_iter(
            std::iter::once(&state as &dyn rusqlite::ToSql)
                .chain(chunk.iter().map(|id| id as &dyn rusqlite::ToSql)),
        );
        changed += exec(tx, &sql, params, || {
            format!("set content_state {} on {} rows", state, chunk.len())
        })?;
    }
    Ok(changed)
}

/// Drop the FTS posting and the stored body of many rows at once, leaving
/// their `files` rows in place: the batch form of [`remove_content_for_id`].
/// Pass only [`STATE_DONE`] ids; the others have neither, and asking costs a
/// seek each.
pub fn clear_content_for_ids(tx: &Transaction<'_>, ids: &[i64]) -> Result<(), String> {
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let list = placeholders(chunk.len());
        for (what, sql) in [
            (
                "searchabletext",
                format!("DELETE FROM searchabletext WHERE rowid IN ({})", list),
            ),
            (
                "documents_text",
                format!("DELETE FROM documents_text WHERE file_id IN ({})", list),
            ),
        ] {
            exec(tx, &sql, params_from_iter(chunk.iter()), || {
                format!("clear {} for {} ids", what, chunk.len())
            })?;
        }
    }
    Ok(())
}

/// Forget the failure records of many rows at once. Pass only
/// [`STATE_FAILED`] ids; nothing else holds a record.
pub fn clear_failed_for_ids(tx: &Transaction<'_>, ids: &[i64]) -> Result<(), String> {
    exec_chunked(
        tx,
        "DELETE FROM failed_files WHERE file_id IN (",
        ids,
        |n| format!("clear failed_files for {} ids", n),
    )
    .map(|_| ())
}

/// Every indexed file directly inside `parent`, as `name -> mtime`. `parent`
/// must be in stored spelling, trailing separator included; build it with
/// [`crate::file_handling::dir_to_db_parent`].
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
    rows.collect::<Result<_, _>>()
        .map_err(|e| format!("read dir row under {}: {}", parent, e))
}

/// A stored path that remembers where its `name` begins: `parent + name` in
/// one buffer instead of two or three strings. The content pass moves one per
/// file from the feeder to the writer.
#[derive(Debug, Clone)]
pub struct RowPath {
    path: String,
    name_at: usize,
}

impl RowPath {
    /// Join the two halves the index stores into one buffer, for callers
    /// that hold both parts.
    pub fn new(parent: &str, name: &str) -> RowPath {
        let mut path = String::with_capacity(parent.len() + name.len());
        path.push_str(parent);
        let name_at = path.len();
        path.push_str(name);
        RowPath { path, name_at }
    }

    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// The `files.name` half.
    pub fn name(&self) -> &str {
        // `name_at` is the parent's length, so it is a char boundary.
        &self.path[self.name_at..]
    }

    /// The `files.parent` half, trailing separator included.
    pub fn parent(&self) -> &str {
        &self.path[..self.name_at]
    }
}

/// One row the content pass has yet to extract.
#[derive(Debug)]
pub struct PendingRow {
    pub file_id: i64,
    pub path: RowPath,
    pub mime: Option<String>,
}

/// One page of rows still awaiting content extraction under `cursor`'s range,
/// ordered by id. Keyset paging serves each row once while the writer flips
/// `content_state` behind the reader.
pub fn pending_content_page(
    conn: &Connection,
    cursor: &ExtractCursor,
    max_size: i64,
    limit: i64,
) -> Result<Vec<PendingRow>, String> {
    let mut stmt = conn
        .prepare_cached(
            // `INDEXED BY`: left to itself the planner takes
            // `idx_files_parent` for the range and sorts the survivors per
            // page, quadratic over a run. The partial index is id-ordered and
            // holds only pending rows, and the planner never prefers it before
            // ANALYZE has run.
            "SELECT id, parent, name, mime FROM files INDEXED BY idx_files_content_pending
              WHERE content_state = 0 AND size <= ?1 AND id > ?2
                AND parent >= ?3 AND parent < ?4
              ORDER BY id
              LIMIT ?5",
        )
        .map_err(|e| format!("prepare pending content query: {}", e))?;
    let rows = stmt
        .query_map(
            params![max_size, cursor.last_id, cursor.range.lo, cursor.range.hi, limit],
            |row| {
                // The parent is grown into the path in place rather than
                // `format!`ed with the name into a third buffer.
                let mut path: String = row.get(1)?;
                let name_at = path.len();
                path.push_str(row.get_ref(2)?.as_str()?);
                Ok(PendingRow {
                    file_id: row.get(0)?,
                    path: RowPath { path, name_at },
                    mime: row.get(3)?,
                })
            },
        )
        .map_err(|e| format!("query pending content: {}", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read pending content row: {}", e))
}

/// A stored row as the scope reconciler sees it: enough to decide both
/// whether the path is still in scope and whether its content still is.
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

/// Count the rows in `range` and, in the same pass, how many of them have a
/// full-text row (see [`STATE_DONE`]).
///
/// Fetches every row in the range: call it where a run has just read those
/// rows anyway, not on a cadence.
pub fn count_root(conn: &Connection, range: &RootRange) -> Result<RootCounts, String> {
    conn.prepare_cached(
        "SELECT COUNT(*), COALESCE(SUM(content_state = ?3), 0) FROM files
          WHERE parent >= ?1 AND parent < ?2",
    )
    .and_then(|mut stmt| {
        stmt.query_row(params![range.lo, range.hi, STATE_DONE], |r| {
            Ok(RootCounts {
                files: r.get(0)?,
                fts: r.get(1)?,
            })
        })
    })
    .map_err(|e| format!("count root {}: {}", range.lo, e))
}

/// One page of rows sorting after `(after_parent, after_name)` and inside the
/// parent range ending at `hi`, in `(parent, name)` order. A row is served at
/// most once while the caller deletes behind the reader. Seed the cursor with
/// `(lo, "")`.
///
/// The row-value comparison keeps it one seek; spelled out as
/// `parent > ? OR (parent = ? AND name > ?)` the planner may scan.
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

/// Reset every failed file to pending and drop the failure records, so the
/// coming run re-attempts them. Returns how many rows were reset.
pub fn retry_failed_files(tx: &Transaction<'_>) -> Result<usize, String> {
    let reset = exec(
        tx,
        "UPDATE files SET content_state = ?1 WHERE content_state = ?2",
        params![STATE_PENDING, STATE_FAILED],
        || "reset failed files to pending".to_string(),
    )?;
    exec(tx, "DELETE FROM failed_files", params![], || {
        "clear failed_files".to_string()
    })?;
    Ok(reset)
}

/// The stored mtime for one exact path, or `None` if it isn't indexed.
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

/// Distinct `parent` values within `range`, streamed to `f`. The root's own
/// directory is included: its stored parent, `root + SEP`, is `range.lo`.
pub fn for_each_parent_in_range<F: FnMut(String)>(
    conn: &Connection,
    range: &RootRange,
    mut f: F,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT parent FROM files WHERE parent >= ?1 AND parent < ?2")
        .map_err(|e| format!("prepare parent scan: {}", e))?;
    let rows = stmt
        .query_map(params![range.lo, range.hi], |r| r.get::<_, String>(0))
        .map_err(|e| format!("parent scan: {}", e))?;
    for row in rows {
        f(row.map_err(|e| format!("read parent row: {}", e))?);
    }
    Ok(())
}

/// Paths of every file directly inside `parent`, which must carry its
/// trailing separator.
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

/// Remove the FTS row and the compressed text blob for a given file id,
/// leaving the `files` row. Idempotent: a missing row is fine.
pub fn remove_content_for_id(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    // `contentless_delete=1` tombstones the rowid without the old text.
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
/// worth its cost.
const VACUUM_MIN_SLACK_PERCENT: i64 = 20;

/// Flush the whole WAL into the main database and truncate the log to zero
/// bytes. `Err` means the log was *not* emptied.
///
/// Only the pragma's result row reports that a checkpoint gave up;
/// `execute`/`execute_batch` discard it. The signal is the *log* column, not
/// `busy`: a TRUNCATE that cannot take the writer lock downgrades itself to
/// PASSIVE and still reports `busy = 0` with the log untouched. A database not
/// in WAL mode reports -1, hence `<= 0`.
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
/// the next open starts with an empty log.
pub fn checkpoint_and_close(conn: Connection) {
    if let Err(e) = checkpoint_truncate(&conn) {
        crate::log_warn!("{}", e);
    }
    drop(conn);
}

/// Read a `PRAGMA` that reports a number. Not `r.get::<i64>(0)`: on a keyed
/// connection SQLCipher answers `PRAGMA page_size` with `cipher_page_size` as
/// TEXT.
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

/// The row count ANALYZE last recorded for `files`, or `None` if it never ran.
///
/// Read from `sqlite_stat1` (populated by [`maintain`]'s `PRAGMA optimize`),
/// not a `COUNT(*)` scan; each `stat` string starts with the estimated row
/// count. Takes the maximum across the table's indexes: the partial
/// `idx_files_content_pending` counts only pending rows.
pub fn analyzed_file_count(conn: &Connection) -> Option<i64> {
    let mut stmt = conn
        .prepare("SELECT stat FROM sqlite_stat1 WHERE tbl = 'files'")
        .ok()?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|stat| stat.split_whitespace().next()?.parse::<i64>().ok());
    rows.max()
}

/// Checkpoint, VACUUM, `PRAGMA optimize`, checkpoint; the trailing checkpoint
/// empties what VACUUM's copy-back and `optimize` wrote to the log. Returns
/// whether it vacuumed.
///
/// Run on a connection from [`crate::db::open::open_maintenance`], never the
/// indexer's. VACUUM's temporary database goes in the directory of `db_path`,
/// since default temp resolution can land on a RAM-backed `/tmp`; peak
/// transient space is about three times the index. Taking the index path, not
/// a directory, keeps that directory the index's and lets it read the log.
pub fn maintain(conn: &Connection, db_path: &str) -> Result<bool, String> {
    let db_dir = std::path::Path::new(db_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let db_dir = db_dir.as_str();

    note_log_on_entry(db_dir, wal_bytes(db_path));
    #[cfg(feature = "probe")]
    let probe = MaintainProbe::start(db_path);

    // Best-effort: compaction does not need the log empty to start.
    if let Err(e) = checkpoint_truncate(conn) {
        crate::log_warn!("{}", e);
    }
    #[cfg(feature = "probe")]
    probe.step("maintain checkpoint");

    let page_count = pragma_number(conn, "page_count")?;
    let freelist = pragma_number(conn, "freelist_count")?;

    let worth_it = freelist * 100 >= page_count * VACUUM_MIN_SLACK_PERCENT;
    // A full filesystem turns `-shm` writes into SIGBUS; see
    // `indexing::pipeline::DISK_FLOOR`.
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
        // `temp_store_directory` is a deprecated pragma that writes a
        // process-wide global, so it is cleared straight after the VACUUM.
        let escaped = db_dir.replace('\'', "''");
        conn.execute_batch(&format!("PRAGMA temp_store_directory = '{}';", escaped))
            .map_err(|e| format!("set temp dir for vacuum: {}", e))?;
        let outcome = conn
            .execute_batch("VACUUM;")
            .map_err(|e| format!("vacuum: {}", e));
        let _ = conn.execute_batch("PRAGMA temp_store_directory = '';");
        outcome?;
    }
    #[cfg(feature = "probe")]
    probe.step(if vacuumed { "VACUUM" } else { "VACUUM (skipped)" });

    conn.execute_batch("PRAGMA optimize;")
        .map_err(|e| format!("optimize: {}", e))?;
    note_optimized(db_dir);
    #[cfg(feature = "probe")]
    probe.step("optimize");

    checkpoint_truncate(conn)?;
    #[cfg(feature = "probe")]
    probe.step("maintain checkpoint");
    Ok(vacuumed)
}

/// Per-step log size and elapsed time through [`maintain`], for `probe`
/// builds. The pass runs on its own connection after the indexer's has gone,
/// so the run's instrumentation cannot see it, and VACUUM's copy-back is the
/// largest single write to the log in a run. The lines match
/// `indexing::pipeline`'s `tail` lines.
#[cfg(feature = "probe")]
struct MaintainProbe {
    db_path: String,
    started: std::time::Instant,
}

#[cfg(feature = "probe")]
impl MaintainProbe {
    fn start(db_path: &str) -> MaintainProbe {
        MaintainProbe {
            db_path: db_path.to_string(),
            started: std::time::Instant::now(),
        }
    }

    fn step(&self, what: &str) {
        crate::log_info!(
            "tail t={:.1}s  wal {}  after {}",
            self.started.elapsed().as_secs_f64(),
            crate::testutil::mib(wal_bytes(&self.db_path)),
            what
        );
    }
}

/// `PRAGMA optimize` acceptances, per index directory, so concurrent tests
/// on separate scratch indexes cannot satisfy each other's assertions.
static OPTIMIZED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn note_optimized(db_dir: &str) {
    *crate::lock_ok(&OPTIMIZED)
        .entry(db_dir.to_string())
        .or_insert(0) += 1;
}

/// How many times `PRAGMA optimize` was accepted against the index in
/// `db_dir` in this process, for the stop-optimize integration test.
pub fn optimize_count(db_dir: &str) -> u64 {
    crate::lock_ok(&OPTIMIZED).get(db_dir).copied().unwrap_or(0)
}

/// Log bytes [`maintain`] was last handed, per index directory.
static LOG_ON_ENTRY: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn note_log_on_entry(db_dir: &str, bytes: u64) {
    crate::lock_ok(&LOG_ON_ENTRY).insert(db_dir.to_string(), bytes);
}

/// How large the log was when [`maintain`] last started on the index in
/// `db_dir`; `None` if it has not run there in this process. A latch like
/// [`optimize_count`], for tests pinning that the indexer's tail checkpoint
/// hands `maintain` an empty log.
pub fn log_on_entry_to_maintain(db_dir: &str) -> Option<u64> {
    crate::lock_ok(&LOG_ON_ENTRY).get(db_dir).copied()
}

/// The `-wal` beside `db_path`, in bytes; 0 when there is none.
fn wal_bytes(db_path: &str) -> u64 {
    std::fs::metadata(format!("{}-wal", db_path))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn get_info(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM schema_info WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn set_info(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_info(key, value) VALUES (?1, ?2)",
        params![key, value],
    )
}

/// Whether the full-text index was built with positions, which decides how
/// a search may phrase its `MATCH` (`search::prefilter::fts_literal`). Read
/// from the index rather than the config, which may already ask for the
/// other layout before the rebuild that brings it. An index that never
/// recorded it predates the compact layout, so kept positions.
pub fn fts_positions(conn: &Connection) -> bool {
    get_info(conn, "fts_positions").is_none_or(|v| v != "false")
}

/// Unix seconds of the last *successful* full indexing run; `None` means
/// "never".
pub fn get_last_full_index(conn: &Connection) -> Option<u64> {
    get_info(conn, "last_full_index").and_then(|v| v.parse().ok())
}

/// Stamp `last_full_index` with `ts` (unix seconds).
pub fn set_last_full_index(conn: &Connection, ts: u64) -> Result<(), String> {
    set_info(conn, "last_full_index", &ts.to_string())
        .map_err(|e| format!("write last_full_index: {}", e))?;
    Ok(())
}

/// `schema_info` key prefix for [`get_root_walk_count`].
const WALK_COUNT: &str = "walk_count:";
/// `schema_info` key prefix for [`get_root_counts`].
const COUNTS: &str = "counts:";

/// The `schema_info` key prefixes holding per-root figures. [`prune_root_stats`]
/// sweeps only these, so a new prefix belongs here or a de-configured root
/// leaves it behind.
const ROOT_STAT_PREFIXES: [&str; 2] = [WALK_COUNT, COUNTS];

fn root_key(prefix: &str, root: &str) -> String {
    format!("{}{}", prefix, root)
}

/// How many files the last clean walk of `root` reported: the progress bar's
/// denominator. Absent means the root has never been walked to completion.
/// See [`crate::indexing::RootProgress::walk_denominator`].
pub fn get_root_walk_count(conn: &Connection, root: &str) -> Option<usize> {
    get_info(conn, &root_key(WALK_COUNT, root)).and_then(|v| v.parse().ok())
}

/// Record `n` as `root`'s file count, for the next run's progress bar.
/// Written only after a walk that finished cleanly: a partial walk's count
/// would leave every later run dividing by a number that is too small.
pub fn set_root_walk_count(conn: &Connection, root: &str, n: usize) -> Result<(), String> {
    set_info(conn, &root_key(WALK_COUNT, root), &n.to_string())
        .map_err(|e| format!("write walk count for {}: {}", root, e))?;
    Ok(())
}

/// What the last completed run counted under `root`, if one has finished
/// since the root was configured.
pub fn get_root_counts(conn: &Connection, root: &str) -> Option<RootCounts> {
    let stored = get_info(conn, &root_key(COUNTS, root))?;
    let (files, fts) = stored.split_once(',')?;
    Some(RootCounts {
        files: files.parse().ok()?,
        fts: fts.parse().ok()?,
    })
}

/// Record what `root` holds, for the folder list. Written only at the end of
/// a run that completed: a stopped run's partial count is worse than the
/// previous figure.
pub fn set_root_counts(conn: &Connection, root: &str, counts: RootCounts) -> Result<(), String> {
    set_info(
        conn,
        &root_key(COUNTS, root),
        &format!("{},{}", counts.files, counts.fts),
    )
    .map_err(|e| format!("write counts for {}: {}", root, e))?;
    Ok(())
}

/// Forget the stored figures of roots that are no longer configured.
pub fn prune_root_stats(conn: &Connection, keep: &[String]) -> Result<(), String> {
    let keep: std::collections::HashSet<String> = ROOT_STAT_PREFIXES
        .iter()
        .flat_map(|prefix| keep.iter().map(move |r| root_key(prefix, r)))
        .collect();
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
