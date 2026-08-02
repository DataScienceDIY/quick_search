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
}

/// Insert a new file row, returning its id. `basic_state` is set to DONE
/// (row existing *is* the basic-index state); `content_state` is PENDING
/// unless the MIME maps to a type we won't extract text from, in which case
/// the caller can later set it to NA.
///
/// Uses `INSERT OR IGNORE` so a UNIQUE(path) collision (which indicates the
/// caller fed the same path twice in one run) becomes a silent no-op
/// returning `None` rather than aborting the whole batch. The walker is
/// expected to dedupe visits upstream; this is a defense-in-depth backstop.
pub fn insert_file(tx: &Transaction<'_>, f: &NewFile<'_>) -> Result<Option<i64>, String> {
    let rows = tx
        .execute(
            "INSERT OR IGNORE INTO files (
                name, path, parent, size, mtime, inode, device_id,
                mime, type, basic_state, content_state, hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
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
                STATE_PENDING,
                f.hash,
            ],
        )
        .map_err(|e| format!("insert file {}: {}", f.path, e))?;
    if rows == 0 {
        // Existing row with the same path (e.g. duplicate visit within the run).
        return Ok(None);
    }
    Ok(Some(tx.last_insert_rowid()))
}

/// Update a file's metadata in place (same path, changed size/mtime/hash).
/// Clears any extracted content so the text-indexing pass re-processes it.
pub fn update_file_basic(
    tx: &Transaction<'_>,
    path: &str,
    size: u64,
    mtime: u64,
    hash: Option<&[u8]>,
    mime: Option<&str>,
    ftype: FileType,
) -> Result<Option<i64>, String> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("lookup file id {}: {}", path, e))?;
    let Some(id) = id else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE files
            SET size = ?1, mtime = ?2, hash = ?3, mime = ?4, type = ?5,
                content_state = ?6, failure_msg = NULL
          WHERE id = ?7",
        params![
            size as i64,
            mtime as i64,
            hash,
            mime,
            ftype.bits() as i64,
            STATE_PENDING,
            id,
        ],
    )
    .map_err(|e| format!("update file {}: {}", path, e))?;
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
        tx.execute(
            "INSERT INTO properties(file_id, key, value) VALUES (?1, ?2, ?3)",
            params![file_id, k, v],
        )
        .map_err(|e| format!("insert property {}={}: {}", k, v, e))?;
    }
    let props_blob = encode_properties_for_fts(properties);
    // Contentless FTS5 still accepts values on INSERT — the tokenizer needs
    // them — it simply doesn't persist the raw column values.
    tx.execute(
        "INSERT INTO searchabletext(rowid, name, text, properties) VALUES (?1, ?2, ?3, ?4)",
        params![file_id, name, text, props_blob],
    )
    .map_err(|e| format!("insert FTS row {}: {}", file_id, e))?;

    // Skip the compressed sidecar when: the config disables snippet storage
    // outright, or there's no body text (e.g. an image whose extractor
    // returned only EXIF properties). The second case saves a zstd frame
    // on what would otherwise be an empty blob.
    if store_text && !text.is_empty() {
        let compressed = zstd::encode_all(text.as_bytes(), ZSTD_LEVEL)
            .map_err(|e| format!("zstd encode for file {}: {}", file_id, e))?;
        tx.execute(
            "INSERT INTO documents_text(file_id, text_zstd, text_len) VALUES (?1, ?2, ?3)",
            params![file_id, compressed, text.len() as i64],
        )
        .map_err(|e| format!("insert documents_text {}: {}", file_id, e))?;
    }

    tx.execute(
        "UPDATE files SET content_state = ?1, failure_msg = NULL WHERE id = ?2",
        params![STATE_DONE, file_id],
    )
    .map_err(|e| format!("update content_state DONE {}: {}", file_id, e))?;
    // Clear any prior failed-file record.
    tx.execute("DELETE FROM failed_files WHERE file_id = ?1", params![file_id])
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
pub fn set_content_failed(
    tx: &Transaction<'_>,
    file_id: i64,
    reason: &str,
) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    tx.execute(
        "UPDATE files SET content_state = ?1, failure_msg = ?2 WHERE id = ?3",
        params![STATE_FAILED, reason, file_id],
    )
    .map_err(|e| format!("update content_state FAILED {}: {}", file_id, e))?;
    tx.execute(
        "INSERT OR REPLACE INTO failed_files(file_id, reason, ts) VALUES (?1, ?2, ?3)",
        params![file_id, reason, now],
    )
    .map_err(|e| format!("insert failed_files {}: {}", file_id, e))?;
    Ok(())
}

/// Mark content extraction as not applicable (e.g. binary format we don't
/// support). The file row still contributes to filename search.
pub fn set_content_na(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    tx.execute(
        "UPDATE files SET content_state = ?1, failure_msg = NULL WHERE id = ?2",
        params![STATE_NA, file_id],
    )
    .map_err(|e| format!("update content_state NA {}: {}", file_id, e))?;
    tx.execute("DELETE FROM failed_files WHERE file_id = ?1", params![file_id])
        .map_err(|e| format!("clear failed_files {}: {}", file_id, e))?;
    Ok(())
}

/// Delete a file row by path, keeping FTS in sync. Returns whether a row was
/// removed.
pub fn delete_file_by_path(tx: &Transaction<'_>, path: &str) -> Result<bool, String> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("lookup {} for delete: {}", path, e))?;
    let Some(id) = id else { return Ok(false) };
    remove_content_for_id(tx, id)?;
    tx.execute("DELETE FROM files WHERE id = ?1", params![id])
        .map_err(|e| format!("delete file {}: {}", path, e))?;
    Ok(true)
}

/// Remove the FTS row, compressed text blob, and any `properties` rows for
/// a given file id. Does not touch the `files` row itself. Idempotent — a
/// missing row is fine.
pub fn remove_content_for_id(tx: &Transaction<'_>, file_id: i64) -> Result<(), String> {
    // `contentless_delete=1` on the FTS5 table makes this work without
    // re-supplying the old column values (it tombstones the rowid).
    tx.execute(
        "DELETE FROM searchabletext WHERE rowid = ?1",
        params![file_id],
    )
    .map_err(|e| format!("FTS delete row {}: {}", file_id, e))?;
    tx.execute(
        "DELETE FROM documents_text WHERE file_id = ?1",
        params![file_id],
    )
    .map_err(|e| format!("delete documents_text {}: {}", file_id, e))?;
    tx.execute("DELETE FROM properties WHERE file_id = ?1", params![file_id])
        .map_err(|e| format!("delete properties {}: {}", file_id, e))?;
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

/// Flush the WAL into the main DB file and close. Call on clean shutdown so
/// the next open starts with an empty log. WAL mode itself is persistent in
/// the file — deliberately left on.
pub fn checkpoint_and_close(conn: Connection) {
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
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
    fn update_resets_content_state() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let id = {
            let tx = conn.transaction().unwrap();
            let id = insert_file(
                &tx,
                &NewFile {
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
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(&tx, id, "a.txt", "old text", &[], true).unwrap();
            tx.commit().unwrap();
            id
        };

        {
            let tx = conn.transaction().unwrap();
            let got = update_file_basic(
                &tx,
                "/tmp/a.txt",
                20,
                2,
                None,
                Some("text/plain"),
                FileType::TEXT,
            )
            .unwrap();
            assert_eq!(got, Some(id));
            tx.commit().unwrap();
        }

        let (state, content): (i64, i64) = conn
            .query_row(
                "SELECT basic_state, content_state FROM files WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, STATE_DONE);
        assert_eq!(content, STATE_PENDING);

        // FTS row for the stale content should be gone.
        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 0);

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
