//! Row-level write helpers that keep the FTS5 contentless table in sync with
//! `files`/`documents_text`.

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};

use crate::mime::FileType;

pub const STATE_PENDING: i64 = 0;
pub const STATE_DONE: i64 = 1;
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

/// Set a file's content state and clear any failure record with it:
/// `list-failed` reads `failed_files` directly, so a stale entry keeps
/// reporting a file broken. [`set_content_failed`] does not route through here.
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
    if rows == 0 {
        return Ok(None);
    }
    Ok(Some(tx.last_insert_rowid()))
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
/// The clearing is narrowed by the row's *stored* state, the way
/// `scope::PagePlan` narrows: content rows exist exactly for `STATE_DONE`
/// (`repo_tests::leaving_done_always_takes_the_posting_with_it`) and
/// `failed_files` rows exactly for `STATE_FAILED`, so every other state has
/// nothing to clear and pays one statement instead of four — most changed
/// rows on a text-free tree are NA→NA, measured 1.24x on that shape
/// (`benches/index.rs`, group `update_narrowing`; DONE→PENDING is unchanged
/// by construction, the clearing happens either way).
/// `content_state` is deliberately **not
/// in the SET list**: `RETURNING` reports the post-update row, so assigning
/// it there would return the new state and the narrowing would read its own
/// write.
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
        // A plain assignment, not `set_state_clearing_failure`: the FAILED
        // arm above already took the failure record, and no other state can
        // hold one.
        exec(
            tx,
            "UPDATE files SET content_state = ?1 WHERE id = ?2",
            params![new_state, id],
            || format!("update reset content_state {}", id),
        )?;
    }
    Ok(Some(id))
}

/// Mark a file's content indexing as complete and write the extracted text
/// atomically. `text_zstd` is the pre-compressed `documents_text` sidecar
/// body, or `None` for no sidecar. Compression is the caller's job: this runs
/// inside the writer's transaction, so compress before taking the lock.
pub fn set_content_done(
    tx: &Transaction<'_>,
    file_id: i64,
    text: &str,
    text_zstd: Option<&[u8]>,
) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;
    set_content_done_fresh(tx, file_id, text, text_zstd)
}

/// [`set_content_done`] for a row that **provably holds no content yet**,
/// skipping the pre-delete.
///
/// Use [`set_content_done`] wherever the row's prior state is not known.
/// Getting this wrong leaves a duplicate FTS entry rather than a visible
/// error, so the rule is: skip the delete only where the *same transaction*
/// has already established there is nothing there.
pub fn set_content_done_fresh(
    tx: &Transaction<'_>,
    file_id: i64,
    text: &str,
    text_zstd: Option<&[u8]>,
) -> Result<(), String> {
    // Contentless FTS5 accepts values on INSERT — the tokenizer needs them —
    // it simply doesn't persist them.
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

    set_state_clearing_failure(tx, file_id, STATE_DONE, "update DONE")
}

/// Reusable decode buffer and context for the readers of `documents_text` —
/// the read side's mirror of [`DocEncoder`]. Measured; don't swap in
/// `zstd::decode_all` (per-row context + `Vec`).
pub struct DocDecoder {
    dctx: zstd::bulk::Decompressor<'static>,
    buf: Vec<u8>,
}

const INITIAL_DOC_CAPACITY: usize = 64 * 1024;

/// Where the doubling stops: far above any legitimate document — past it, a
/// failure is a corrupt frame rather than a buffer that is too small.
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
        // than growing, so the room has to be there first. A frame carrying no
        // content size (any stream-based encoder's) MUST take the growth loop
        // below — do not fall back to `zstd::decode_all` for it, which
        // rebuilds a decoder and output buffer per row and once dominated a
        // fuzzy search's allocator traffic.
        if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(blob) {
            // Clamped: `size` comes straight out of the frame header, so a
            // hostile blob can ask for terabytes and `reserve` answers an
            // impossible request by aborting the process, not by failing.
            self.buf
                .reserve(usize::try_from(size).ok()?.min(MAX_DOC_CAPACITY));
        }
        loop {
            if self.buf.capacity() == 0 {
                self.buf.reserve(INITIAL_DOC_CAPACITY);
            }
            match self.dctx.decompress_to_buffer(blob, &mut self.buf) {
                Ok(_) => break,
                // Too small, or corrupt — the bulk API cannot tell us which.
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

/// Measured (`benches/index.rs`): level 9+ shaves a few percent at 10× the CPU.
const ZSTD_LEVEL: i32 = 3;

/// Reusable compression context for the `documents_text` sidecar; one per
/// batch. Measured (`benches/index.rs`, group `zstd_encode`).
pub struct DocEncoder {
    ctx: zstd::bulk::Compressor<'static>,
    /// One row's compressed output, reused. **`zstd`'s `WriteBuf` for `Vec`
    /// writes from offset 0 and sets the length** — it overwrites rather
    /// than appends — so a body cannot be compressed straight into a shared
    /// arena. It lands here and is copied across, which still costs no
    /// allocation once both buffers have grown.
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
    /// landed. The mirror of [`DocDecoder`]'s reused buffer on the write
    /// side: a whole batch's bodies share one allocation instead of taking
    /// one `Vec` each, which at a chunk per commit was an allocation per
    /// indexed document.
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

/// Compress one body, for the writers that handle a single row.
pub fn encode_one(text: &str, store_text: bool) -> Result<Option<Vec<u8>>, String> {
    if !store_text || text.is_empty() {
        return Ok(None);
    }
    DocEncoder::new()?.encode(text).map(Some)
}

/// The uncompressed size of a stored `documents_text` blob, from the zstd
/// frame header. Only the *header* is read, so `blob` may be a prefix.
pub fn raw_text_len(blob: &[u8]) -> Option<u64> {
    zstd::zstd_safe::get_frame_content_size(blob).ok().flatten()
}

/// Mark a file's content extraction as failed. Keeps the basic row in place.
///
/// Clears any posting and stored body first, which matters twice over. It is
/// what a re-extraction that fails *owes* the reader: the text that is there
/// came out of an earlier version of a file that has since changed, so leaving
/// it serves hits for content the file no longer has. And it is what makes
/// "a `searchabletext` row exists exactly when `content_state` is
/// `STATE_DONE`" true of every transition rather than of most of them —
/// an equivalence `count_root` reports from and `delete_files_matching` now
/// narrows on, so a transition that quietly broke it would leave postings
/// behind for files that are no longer indexed.
///
/// Every path that reaches here today is already re-extracting a row it has
/// just reset (`update_file_basic`) or that was born pending, so the two
/// deletes are normally no-ops; extraction failures are rare enough that
/// paying for the guarantee is not worth measuring.
pub fn set_content_failed(tx: &Transaction<'_>, file_id: i64, reason: &str) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;
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

/// Mark content extraction as not applicable; the row still serves filename
/// search.
///
/// Clears any posting and stored body first, for the reasons spelled out on
/// [`set_content_failed`] — a row arrives here because its content should no
/// longer be searchable, so leaving the old text behind contradicts the very
/// transition. Every caller already cleared first or had nothing to clear, so
/// this changes no behaviour; what it changes is that the invariant no longer
/// depends on all of them remembering.
pub fn set_content_na(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;
    set_state_clearing_failure(tx, file_id, STATE_NA, "update NA")
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
/// path is not indexed.
///
/// Read it in the same transaction as the decision it feeds: the caller that
/// builds [`delete_ids`]'s `with_postings` from the state must see the state
/// *at delete time*, and only the transaction makes that simultaneous.
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

/// Delete every row whose parent falls in `[lo, hi)`. Build the bounds with
/// [`crate::file_handling::ExtractCursor::for_root`], which makes them
/// separator-correct — the range covers the root's *own* files only because
/// every stored parent ends in a separator.
pub fn delete_subtree(tx: &Transaction<'_>, lo: &str, hi: &str) -> Result<usize, String> {
    delete_files_matching(
        tx,
        "parent >= ?1 AND parent < ?2",
        &[&lo, &hi],
        &format!("under {}", lo),
    )
}

/// Delete the `files` rows matching `files_where`, first tombstoning their
/// `searchabletext` rows. Returns how many `files` rows went.
///
/// `documents_text` and `failed_files` are left to `ON DELETE CASCADE`:
/// every connection profile sets `PRAGMA foreign_keys = ON` (`db::schema`),
/// kept honest by `repo_tests::deleting_a_file_row_cascades_the_fk_tables`
/// and reconcile's `orphans()` sweep. `searchabletext` cannot cascade — an
/// FTS5 virtual table takes no foreign key — so its contentless delete must
/// stay explicit.
///
/// **Deliberately *not* narrowed to `content_state = STATE_DONE`**, though the
/// rows it would exclude are provably the ones with nothing to tombstone (see
/// [`delete_ids`], which does narrow). The two are not the same trade: this one
/// works from a range rather than a list of decided rows, so reading
/// `content_state` costs a `files` row fetch per candidate — the very rows the
/// `DELETE` below is about to fetch anyway, but a second traversal of them all
/// the same. That is a certain cost against an uncertain saving, and the saving
/// it buys is one `%_docsize` seek into a b-tree far smaller than `files`.
/// `examples/pruneprobe.rs` prices the range form; narrow this when it says to.
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

/// Delete every row whose parent falls in *none* of `ranges` — a full scan of
/// `files`, reserved for the one transition that needs it: with
/// `follow_symlinks` off, rows left by a followed symlink fall outside every
/// root's range and no walk will ever visit them again.
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

/// Delete the given file ids and everything keyed to them. Returns how many
/// `files` rows went. Dependent tables: see `delete_files_matching`.
///
/// `with_postings` is the subset of `ids` whose `content_state` was
/// `STATE_DONE`, and so the only ones FTS5 can have anything to tombstone for —
/// `repo_tests::leaving_done_always_takes_the_posting_with_it` is what makes
/// that true of every transition. Handing FTS5 the rest is not free: each is a
/// `%_docsize` seek to discover an absence, and on an index where most rows
/// carry no text that is most of the list.
///
/// Taking it as a second argument rather than deriving it here is the point:
/// the caller decided these rows from a page it had already read, so it holds
/// `content_state` for nothing, where a `SELECT` back out of `files` would cost
/// a row fetch each (see `delete_files_matching`, which for that reason does
/// not narrow).
///
/// It must be a subset: an id left out keeps its posting after its `files` row
/// is gone, which surfaces as a hit for a file that is no longer indexed.
pub fn delete_ids(
    tx: &Transaction<'_>,
    ids: &[i64],
    with_postings: &[i64],
) -> Result<usize, String> {
    for chunk in with_postings.chunks(DELETE_IDS_CHUNK) {
        let sql = format!(
            "DELETE FROM searchabletext WHERE rowid IN ({})",
            placeholders(chunk.len())
        );
        exec(tx, &sql, params_from_iter(chunk.iter()), || {
            format!("delete searchabletext for {} ids", chunk.len())
        })?;
    }
    let mut removed = 0;
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let sql = format!(
            "DELETE FROM files WHERE id IN ({})",
            placeholders(chunk.len())
        );
        removed += exec(tx, &sql, params_from_iter(chunk.iter()), || {
            format!("delete {} file rows", chunk.len())
        })?;
    }
    Ok(removed)
}

/// Set `content_state` on many rows at once, leaving everything else alone.
///
/// The batch form of the `UPDATE` inside [`set_state_clearing_failure`], and
/// deliberately *without* its `failed_files` sweep: only a `STATE_FAILED` row
/// can hold such a record, so a caller that knows the stored states can clear
/// the few that need it with [`clear_failed_for_ids`] instead of paying a
/// delete per row. A caller that does not know them must call the per-row
/// helpers, which cannot get this wrong.
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
///
/// Pass only ids whose stored `content_state` was `STATE_DONE`; the others
/// have neither, and asking is what costs (see `delete_files_matching`).
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

/// Forget the failure records of many rows at once. `list-failed` reads
/// `failed_files` directly, so a stale entry keeps reporting a file broken —
/// this is the batch half of what [`set_state_clearing_failure`] does per row.
///
/// Pass only ids whose stored `content_state` was `STATE_FAILED`: nothing else
/// can hold a record here.
pub fn clear_failed_for_ids(tx: &Transaction<'_>, ids: &[i64]) -> Result<(), String> {
    for chunk in ids.chunks(DELETE_IDS_CHUNK) {
        let sql = format!(
            "DELETE FROM failed_files WHERE file_id IN ({})",
            placeholders(chunk.len())
        );
        exec(tx, &sql, params_from_iter(chunk.iter()), || {
            format!("clear failed_files for {} ids", chunk.len())
        })?;
    }
    Ok(())
}

/// Every indexed file directly inside `parent`, as `name -> mtime`. `parent`
/// must be in stored spelling — trailing separator and all; build it with
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
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (name, mtime) = row.map_err(|e| format!("read dir row under {}: {}", parent, e))?;
        out.insert(name, mtime);
    }
    Ok(out)
}

/// A stored path that remembers where its `name` begins — the whole
/// `parent + name` in **one** buffer rather than the two or three strings
/// carrying both halves would cost. The content pass moves one of these per
/// file from the feeder to the writer, so the saving is per indexed file.
#[derive(Debug)]
pub struct RowPath {
    path: String,
    name_at: usize,
}

impl RowPath {
    /// Join the two halves the index stores into one buffer. The pending-page
    /// query builds its own in place; this is for everyone assembling a row
    /// from parts they already hold.
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
        // `name_at` is the parent's length, taken as the buffer was built; a
        // stored parent is by construction a prefix of the path.
        &self.path[self.name_at..]
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
/// ordered by id. Keyset paging: a row is served exactly once even though the
/// writer is concurrently flipping `content_state` behind the reader.
pub fn pending_content_page(
    conn: &Connection,
    cursor: &crate::file_handling::ExtractCursor,
    max_size: i64,
    limit: i64,
) -> Result<Vec<PendingRow>, String> {
    let mut stmt = conn
        .prepare_cached(
            // `INDEXED BY`: left to itself the planner takes
            // `idx_files_parent` for the range and sorts the survivors per
            // page — quadratic over a run. The partial index is id-ordered
            // and holds only pending rows, and the planner never prefers it
            // before ANALYZE has run, so this cannot be left to statistics.
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

/// Count the rows in the half-open parent range `[lo, hi)` and, in the same
/// pass, how many of them have a full-text row.
///
/// `content_state = STATE_DONE` *is* "has a `searchabletext` row":
/// [`set_content_done`] holds the only insert into that table, and
/// [`remove_content_for_id`] clears the two together.
///
/// Not cheap — every row in the range is fetched. Call it where a run has
/// just read those rows anyway, not on a cadence.
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
/// parent range ending at `hi`, in `(parent, name)` order. A row is served at
/// most once even though the caller is deleting behind the reader. Seed the
/// cursor with `(lo, "")`.
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
/// `files` row intact: full-text search keeps working, only the snippet
/// source goes away.
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
/// metadata.
pub fn reset_content_pending(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    remove_content_for_id(tx, file_id)?;
    set_state_clearing_failure(tx, file_id, STATE_PENDING, "reset pending")
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

/// Distinct `parent` values within the half-open range `[lo, hi)`, streamed
/// to `f`. The root's own directory is included: its stored parent is
/// `root + SEP`, which is exactly `lo`.
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

/// Remove the FTS row and the compressed text blob for a given file id. Does
/// not touch the `files` row itself. Idempotent — a missing row is fine.
pub fn remove_content_for_id(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    // `contentless_delete=1` on the FTS5 table makes this work without
    // re-supplying the old column values (it tombstones the rowid).
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
/// The pragma's result row is the only place SQLite reports that a checkpoint
/// gave up — `execute`/`execute_batch` discard it. The signal is the *log*
/// column, not `busy`: a TRUNCATE that cannot take the writer lock silently
/// downgrades itself to PASSIVE and still reports `busy = 0` with the log
/// untouched. A database not in WAL mode reports -1, hence `<= 0`.
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

/// Read a `PRAGMA` that reports a number.
///
/// Not simply `r.get::<i64>(0)`: on a **keyed** connection SQLCipher
/// intercepts `PRAGMA page_size`, answers with `cipher_page_size` instead,
/// and returns it as TEXT — so asking for an integer fails with a type error.
/// Unencrypted it is an INTEGER as usual.
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
/// Read from `sqlite_stat1`, which the `PRAGMA optimize` in [`maintain`]
/// populates — a handful of rows, not the `COUNT(*)` full scan that sizing a
/// cache to avoid full scans has no business paying. The first token of each
/// `stat` string is the estimated row count.
///
/// **The maximum**, not the first row: `idx_files_content_pending` is partial
/// (`WHERE content_state = 0`), so it reports only the pending files and would
/// size the cache for a fraction of the table.
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

/// Checkpoint → VACUUM → `PRAGMA optimize` → checkpoint — the trailing
/// checkpoint matters because VACUUM's copy-back and `optimize` refill the
/// log. Returns whether it vacuumed.
///
/// Run on a connection from [`crate::db::open::open_maintenance`], never the
/// indexer's. VACUUM's temporary database goes in the index's own directory,
/// taken from `db_path` — default temp resolution can land on a RAM-backed
/// `/tmp`. Peak transient space is roughly three times the index.
///
/// The path rather than the directory, so the caller cannot pass one that is
/// not the index's, and so the readings below can see the log.
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
    // Check space first: on a full filesystem, writes to the `-shm` mmap come
    // back as SIGBUS rather than as an error — see
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

/// Per-step log size and elapsed time through [`maintain`], for `probe` builds.
///
/// The pass runs on its own connection after the indexer's has gone, so its
/// cost is invisible from the run's own instrumentation — and VACUUM's
/// copy-back is the single largest thing that writes to the log in a whole
/// run. Matches `indexing::pipeline`'s `tail` lines, which cover the half
/// before this one.
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

/// `PRAGMA optimize` acceptances, per index directory — per directory so
/// concurrent tests against separate scratch indexes cannot satisfy each
/// other's assertions.
static OPTIMIZED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn note_optimized(db_dir: &str) {
    *crate::lock_ok(&OPTIMIZED)
        .entry(db_dir.to_string())
        .or_insert(0) += 1;
}

/// How many times the index in `db_dir` has had `PRAGMA optimize` run against
/// it in this process — exists for the stop-optimize integration test. It
/// records that the statement was accepted, not what SQLite did.
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
/// `db_dir`; `None` if it has not run there in this process.
///
/// A latch in the same shape as [`optimize_count`], and for the same reason:
/// the value is gone by the time a test could sample it. It exists to pin the
/// invariant the indexer's tail checkpoints establish — `maintain` is handed
/// an *empty* log, so its VACUUM's copy-back is the only thing in it rather
/// than a second layer over a whole run's writing.
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

/// The `schema_info` key prefixes holding per-root figures. Every one of them
/// is swept by [`prune_root_stats`], so a new prefix belongs in this list or a
/// de-configured root leaves it behind forever.
const ROOT_STAT_PREFIXES: [&str; 2] = ["walk_count:", "counts:"];

fn root_key(prefix: &str, root: &str) -> String {
    format!("{}{}", prefix, root)
}

fn walk_count_key(root: &str) -> String {
    root_key(ROOT_STAT_PREFIXES[0], root)
}

fn counts_key(root: &str) -> String {
    root_key(ROOT_STAT_PREFIXES[1], root)
}

/// How many files the last clean walk of `root` reported — the progress bar's
/// denominator. Absent means the root has never been walked to completion.
/// See [`crate::indexing::RootProgress::walk_denominator`].
pub fn get_root_walk_count(conn: &Connection, root: &str) -> Option<usize> {
    get_info(conn, &walk_count_key(root)).and_then(|v| v.parse().ok())
}

/// Record `n` as `root`'s file count, for the next run's progress bar.
/// Written only after a walk that finished cleanly: a partial walk's count
/// would leave every later run dividing by a number that is too small.
pub fn set_root_walk_count(conn: &Connection, root: &str, n: usize) -> Result<(), String> {
    set_info(conn, &walk_count_key(root), &n.to_string())
        .map_err(|e| format!("write walk count for {}: {}", root, e))?;
    Ok(())
}

/// What the last completed run counted under `root`, if one has finished
/// since the root was configured.
pub fn get_root_counts(conn: &Connection, root: &str) -> Option<RootCounts> {
    let stored = get_info(conn, &counts_key(root))?;
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
        &counts_key(root),
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
