//! Programmatic read-only query helpers.
//!
//! Pure functions that open a DB, run a query, and return structured data.
//! No stdout, no CLI framing — callers (GUI, future CLI binaries, Set B
//! `balooctl`) format the result as they see fit. Mutating operations live
//! on [`crate::indexing::IndexingService`] since they require a running
//! worker thread.

use rusqlite::{params, OptionalExtension};

use crate::db::open_and_migrate;
use crate::db::repo::{STATE_DONE, STATE_FAILED, STATE_NA, STATE_PENDING};

/// Per-file indexing status, mirroring Baloo's multi-state reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    NotIndexed,
    Pending,
    Done,
    Failed,
    NotApplicable,
}

impl From<i64> for IndexState {
    fn from(v: i64) -> Self {
        match v {
            x if x == STATE_PENDING => IndexState::Pending,
            x if x == STATE_DONE => IndexState::Done,
            x if x == STATE_FAILED => IndexState::Failed,
            x if x == STATE_NA => IndexState::NotApplicable,
            _ => IndexState::Pending,
        }
    }
}

/// Indexing status for a single file. `basic` is the metadata row state
/// (indexed or not); `content` is the extractor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub path: String,
    pub basic: IndexState,
    pub content: IndexState,
    pub failure_reason: Option<String>,
}

/// Per-file entry returned by [`list_failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedEntry {
    pub file_id: i64,
    pub path: String,
    pub reason: Option<String>,
    pub ts: i64,
}

/// Storage footprint report. "Partitions" correspond to SQL tables for our
/// SQLite layout (Baloo's LMDB has named sub-DBs; our equivalent is per-table
/// row/size counts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeReport {
    pub file_size_bytes: u64,
    pub files_row_count: i64,
    pub properties_row_count: i64,
    pub failed_files_row_count: i64,
    pub searchabletext_row_count: i64,
}

/// Query the per-file indexing status. Returns `FileStatus` with
/// `basic == NotIndexed` if the path isn't in the database.
pub fn status_for_path(db_path: &str, path: &str) -> Result<FileStatus, String> {
    let conn = open_and_migrate(db_path, "trigram")?;
    let row: Option<(i64, i64, Option<String>)> = conn
        .query_row(
            "SELECT basic_state, content_state, failure_msg FROM files WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| format!("status_for_path({}): {}", path, e))?;
    Ok(match row {
        None => FileStatus {
            path: path.to_string(),
            basic: IndexState::NotIndexed,
            content: IndexState::NotIndexed,
            failure_reason: None,
        },
        Some((basic, content, reason)) => FileStatus {
            path: path.to_string(),
            basic: IndexState::from(basic),
            content: IndexState::from(content),
            failure_reason: reason,
        },
    })
}

/// Return every file that failed content extraction, newest first.
pub fn list_failed(db_path: &str, limit: Option<u32>) -> Result<Vec<FailedEntry>, String> {
    let conn = open_and_migrate(db_path, "trigram")?;
    let limit_sql = match limit {
        Some(n) => format!(" LIMIT {}", n),
        None => String::new(),
    };
    let sql = format!(
        "SELECT ff.file_id, f.path, ff.reason, ff.ts \
         FROM failed_files ff \
         JOIN files f ON f.id = ff.file_id \
         ORDER BY ff.ts DESC{}",
        limit_sql
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("list_failed prepare: {}", e))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(FailedEntry {
                file_id: r.get(0)?,
                path: r.get(1)?,
                reason: r.get(2)?,
                ts: r.get(3)?,
            })
        })
        .map_err(|e| format!("list_failed query: {}", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("list_failed row: {}", e))
}

/// Return a rough size breakdown of the database on disk and by table.
pub fn index_size_breakdown(db_path: &str) -> Result<SizeReport, String> {
    let file_size_bytes = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let conn = open_and_migrate(db_path, "trigram")?;
    let count = |table: &str| -> Result<i64, String> {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .map_err(|e| format!("count {}: {}", table, e))
    };
    Ok(SizeReport {
        file_size_bytes,
        files_row_count: count("files")?,
        properties_row_count: count("properties")?,
        failed_files_row_count: count("failed_files")?,
        searchabletext_row_count: count("searchabletext")?,
    })
}

/// Count files with `content_state = 0` (pending). Distinct from
/// `files_row_count − searchabletext_row_count`, which over-counts because
/// files whose content doesn't apply (binary formats, too-large files) sit
/// with `content_state = 3` (NA) forever and never become FTS rows.
///
/// Used by the Baloo compat daemon to report the "Files waiting for content
/// indexing" figure both to balooctl and to the LMDB mirror.
pub fn pending_content_count(db_path: &str) -> Result<i64, String> {
    let conn = open_and_migrate(db_path, "trigram")?;
    conn.query_row(
        "SELECT COUNT(*) FROM files WHERE content_state = ?1",
        rusqlite::params![crate::db::repo::STATE_PENDING],
        |r| r.get(0),
    )
    .map_err(|e| format!("pending_content_count: {}", e))
}

/// Remove a single file from the index. Returns whether a row was deleted.
/// Keeps FTS/documents/properties in sync via the repo helpers.
pub fn clear_path(db_path: &str, path: &str) -> Result<bool, String> {
    let mut conn = open_and_migrate(db_path, "trigram")?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("clear_path begin tx: {}", e))?;
    let removed = crate::db::repo::delete_file_by_path(&tx, path)?;
    tx.commit()
        .map_err(|e| format!("clear_path commit: {}", e))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repo::{insert_file, set_content_done, set_content_failed, NewFile};
    use crate::mime::FileType;

    fn tmp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-cli-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn seed_fixture(db_path: &str) -> (i64, i64) {
        let mut conn = open_and_migrate(db_path, "trigram").unwrap();
        let (a, b) = {
            let tx = conn.transaction().unwrap();
            let a = insert_file(
                &tx,
                &NewFile {
                    name: "a.txt",
                    path: "/tmp/a.txt",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    inode: None,
                    device_id: None,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(&tx, a, "a.txt", "hello", &[]).unwrap();
            let b = insert_file(
                &tx,
                &NewFile {
                    name: "b.bin",
                    path: "/tmp/b.bin",
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
            .unwrap()
            .expect("unique path");
            set_content_failed(&tx, b, "bad extract").unwrap();
            tx.commit().unwrap();
            (a, b)
        };
        drop(conn);
        (a, b)
    }

    #[test]
    fn status_for_path_returns_states() {
        let p = tmp_path();
        let (_a, _b) = seed_fixture(p.to_str().unwrap());

        let st_a = status_for_path(p.to_str().unwrap(), "/tmp/a.txt").unwrap();
        assert_eq!(st_a.basic, IndexState::Done);
        assert_eq!(st_a.content, IndexState::Done);

        let st_b = status_for_path(p.to_str().unwrap(), "/tmp/b.bin").unwrap();
        assert_eq!(st_b.basic, IndexState::Done);
        assert_eq!(st_b.content, IndexState::Failed);
        assert_eq!(st_b.failure_reason.as_deref(), Some("bad extract"));

        let st_missing = status_for_path(p.to_str().unwrap(), "/tmp/never.txt").unwrap();
        assert_eq!(st_missing.basic, IndexState::NotIndexed);

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn list_failed_returns_failed_rows() {
        let p = tmp_path();
        let (_a, b) = seed_fixture(p.to_str().unwrap());

        let failed = list_failed(p.to_str().unwrap(), None).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].file_id, b);
        assert_eq!(failed[0].path, "/tmp/b.bin");
        assert_eq!(failed[0].reason.as_deref(), Some("bad extract"));

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn index_size_breakdown_counts_rows() {
        let p = tmp_path();
        let _ = seed_fixture(p.to_str().unwrap());

        let r = index_size_breakdown(p.to_str().unwrap()).unwrap();
        assert!(r.file_size_bytes > 0);
        assert_eq!(r.files_row_count, 2);
        assert_eq!(r.failed_files_row_count, 1);

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn clear_path_removes_file_and_returns_true() {
        let p = tmp_path();
        let _ = seed_fixture(p.to_str().unwrap());

        assert!(clear_path(p.to_str().unwrap(), "/tmp/a.txt").unwrap());
        let st = status_for_path(p.to_str().unwrap(), "/tmp/a.txt").unwrap();
        assert_eq!(st.basic, IndexState::NotIndexed);
        assert!(!clear_path(p.to_str().unwrap(), "/tmp/a.txt").unwrap());

        std::fs::remove_file(&p).ok();
    }
}
