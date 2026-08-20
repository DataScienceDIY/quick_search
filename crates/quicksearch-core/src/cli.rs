//! Programmatic query helpers.
//!
//! Pure functions that open a DB, run a query, and return structured data.
//! No stdout, no CLI framing — the GUI and `quicksearch-cli` format the
//! result as they see fit. Indexing operations live on
//! [`crate::indexing::IndexingService`] since they require a running worker
//! thread.
//!
//! # These are consumed from outside this repository
//!
//! QuickSearch is a sub-repo, and **nothing in this module has a caller in
//! this tree**. [`status_for_path`], [`list_failed`], [`index_size_breakdown`],
//! [`pending_content_count`] and [`clear_path`] are called by the parent
//! repository's Baloo compat daemon, which is what reports them to `balooctl`
//! and mirrors them into LMDB.
//!
//! So they are **not dead code**, and their signatures are a compatibility
//! surface rather than an internal detail: a search of this repository alone
//! will not turn up the callers that break when one changes.
//!
//! One exception to the "query helpers" framing: [`clear_path`] mutates. It
//! opens its own writer, which sidesteps the single-writer discipline the
//! coordinator maintains, so it is safe only against an index no local
//! coordinator is running against.

use rusqlite::{params, OptionalExtension};

use crate::db::open_existing;
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

/// Storage footprint report: per-table row/size counts (the equivalent of
/// Baloo's LMDB sub-DBs). `documents_text_*` fields cover the
/// zstd-compressed extracted-text sidecar; ratio is `compressed / raw`, so
/// ~0.3 means ~70% saved vs storing the plaintext verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct SizeReport {
    pub file_size_bytes: u64,
    pub files_row_count: i64,
    pub failed_files_row_count: i64,
    pub searchabletext_row_count: i64,
    pub documents_text_row_count: i64,
    pub documents_text_raw_bytes: i64,
    pub documents_text_compressed_bytes: i64,
}

impl SizeReport {
    /// Compressed:raw ratio for the stored extracted text. `None` when no
    /// rows have been written yet (avoids divide-by-zero).
    pub fn documents_text_ratio(&self) -> Option<f64> {
        if self.documents_text_raw_bytes <= 0 {
            return None;
        }
        Some(self.documents_text_compressed_bytes as f64 / self.documents_text_raw_bytes as f64)
    }
}

/// Query the per-file indexing status. Returns `FileStatus` with
/// `basic == NotIndexed` if the path isn't in the database.
///
/// There is no stored basic state: a `files` row exists only once its
/// metadata has been read, so the row *is* the basic-indexed state. The
/// failure reason comes from `failed_files`, the one place it is written.
pub fn status_for_path(db_path: &str, path: &str) -> Result<FileStatus, String> {
    let conn = open_existing(db_path, false)?;
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT f.content_state, ff.reason \
               FROM files f \
               LEFT JOIN failed_files ff ON ff.file_id = f.id \
              WHERE f.path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?)),
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
        Some((content, reason)) => FileStatus {
            path: path.to_string(),
            basic: IndexState::Done,
            content: IndexState::from(content),
            failure_reason: reason,
        },
    })
}

/// Return every file that failed content extraction, newest first.
pub fn list_failed(db_path: &str, limit: Option<u32>) -> Result<Vec<FailedEntry>, String> {
    let conn = open_existing(db_path, false)?;
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
    let file_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let conn = open_existing(db_path, false)?;
    let count = |table: &str| -> Result<i64, String> {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
            .map_err(|e| format!("count {}: {}", table, e))
    };
    let dt_row_count: i64 = count("documents_text")?;
    // The uncompressed length is not a column: zstd records it in each frame's
    // header, so this reads it back (see `repo::raw_text_len`). Only the
    // header is wanted, and 18 bytes is the most one can occupy — projecting
    // the prefix keeps this off the document bodies themselves.
    let mut stmt = conn
        .prepare("SELECT substr(text_zstd, 1, 18), LENGTH(text_zstd) FROM documents_text")
        .map_err(|e| format!("documents_text size sum prepare: {}", e))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| format!("documents_text size sum: {}", e))?;
    let (mut dt_raw, mut dt_compressed) = (0i64, 0i64);
    for row in rows {
        let (header, compressed) = row.map_err(|e| format!("documents_text size row: {}", e))?;
        // A frame with no recorded content size contributes nothing rather
        // than skewing the ratio with a guess.
        dt_raw += crate::db::repo::raw_text_len(&header).unwrap_or(0) as i64;
        dt_compressed += compressed;
    }
    drop(stmt);

    Ok(SizeReport {
        file_size_bytes,
        files_row_count: count("files")?,
        failed_files_row_count: count("failed_files")?,
        searchabletext_row_count: count("searchabletext")?,
        documents_text_row_count: dt_row_count,
        documents_text_raw_bytes: dt_raw,
        documents_text_compressed_bytes: dt_compressed,
    })
}

/// Count files with `content_state = 0` (pending) — files an extractor claims
/// whose text has not been read yet. Files nothing extracts (binary formats,
/// too-large files) are written `content_state = 3` (NA) when the walk records
/// them and are never counted here, so this is outstanding work rather than
/// `files_row_count − searchabletext_row_count`, which counts those forever.
///
/// Used by the Baloo compat daemon to report the "Files waiting for content
/// indexing" figure both to balooctl and to the LMDB mirror.
pub fn pending_content_count(db_path: &str) -> Result<i64, String> {
    let conn = open_existing(db_path, false)?;
    conn.query_row(
        "SELECT COUNT(*) FROM files WHERE content_state = ?1",
        rusqlite::params![crate::db::repo::STATE_PENDING],
        |r| r.get(0),
    )
    .map_err(|e| format!("pending_content_count: {}", e))
}

/// Remove a single file from the index. Returns whether a row was deleted.
/// Keeps FTS and `documents_text` in sync via the repo helpers.
pub fn clear_path(db_path: &str, path: &str) -> Result<bool, String> {
    let mut conn = open_existing(db_path, true)?;
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
    use crate::db::open_or_recreate;
    use crate::db::repo::{insert_file, set_content_done, set_content_failed, NewFile};
    use crate::mime::FileType;
    use crate::testutil::zstd_of;

    fn tmp_path() -> std::path::PathBuf {
        crate::testutil::scratch_dir("cli").join("index.sqlite")
    }

    fn seed_fixture(db_path: &str) -> (i64, i64) {
        let mut conn = open_or_recreate(db_path, "trigram").unwrap();
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
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            set_content_done(&tx, a, "hello", zstd_of("hello").as_deref()).unwrap();
            let b = insert_file(
                &tx,
                &NewFile {
                    name: "b.bin",
                    path: "/tmp/b.bin",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    mime: None,
                    ftype: FileType::EMPTY,
                    hash: None,
                    needs_content: false,
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

    /// The count is "outstanding extraction work", which is a narrower thing
    /// than "rows without text": a file nothing extracts is settled, not
    /// waiting, and would otherwise be reported as a backlog that never
    /// drains.
    #[test]
    fn pending_content_count_counts_only_outstanding_work() {
        let p = tmp_path();
        let dbp = p.to_str().unwrap();
        // a is Done, b is Failed — both resolved, neither pending.
        let _ = seed_fixture(dbp);
        assert_eq!(pending_content_count(dbp).unwrap(), 0);

        let mut conn = open_or_recreate(dbp, "trigram").unwrap();
        {
            let tx = conn.transaction().unwrap();
            // Claimed by an extractor, text not read yet: this is the backlog.
            insert_file(
                &tx,
                &NewFile {
                    name: "c.txt",
                    path: "/tmp/c.txt",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            // Nothing extracts this one, so it is NA on arrival and must not
            // inflate the figure.
            insert_file(
                &tx,
                &NewFile {
                    name: "d.bin",
                    path: "/tmp/d.bin",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    mime: None,
                    ftype: FileType::EMPTY,
                    hash: None,
                    needs_content: false,
                },
            )
            .unwrap()
            .expect("unique path");
            tx.commit().unwrap();
        }
        drop(conn);

        assert_eq!(
            pending_content_count(dbp).unwrap(),
            1,
            "only the file awaiting extraction counts"
        );

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pending_content_count_on_a_missing_db_is_an_error() {
        let missing = crate::testutil::scratch_dir("cli-missing").join("nope.sqlite");
        assert!(pending_content_count(missing.to_str().unwrap()).is_err());
    }

    #[test]
    fn index_size_breakdown_counts_rows() {
        let p = tmp_path();
        let _ = seed_fixture(p.to_str().unwrap());

        let r = index_size_breakdown(p.to_str().unwrap()).unwrap();
        assert!(r.file_size_bytes > 0);
        assert_eq!(r.files_row_count, 2);
        assert_eq!(r.failed_files_row_count, 1);
        // File a got content ("hello"); file b failed. Only one documents_text row.
        assert_eq!(r.documents_text_row_count, 1);
        assert_eq!(r.documents_text_raw_bytes, "hello".len() as i64);
        assert!(r.documents_text_compressed_bytes > 0);

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn documents_text_ratio_reports_savings_on_compressible_prose() {
        // Feed highly-compressible prose (lots of repeated words) and verify
        // the reported ratio reflects real savings. Guards against anyone
        // silently swapping the compression step for a pass-through.
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        {
            let tx = conn.transaction().unwrap();
            let id = insert_file(
                &tx,
                &NewFile {
                    name: "big.txt",
                    path: "/tmp/big.txt",
                    parent: "/tmp",
                    size: 1,
                    mtime: 1,
                    mime: Some("text/plain"),
                    ftype: FileType::TEXT,
                    hash: None,
                    needs_content: true,
                },
            )
            .unwrap()
            .expect("unique path");
            let prose = "the quick brown fox jumps over the lazy dog. ".repeat(500);
            set_content_done(&tx, id, &prose, zstd_of(&prose).as_deref()).unwrap();
            tx.commit().unwrap();
        }
        drop(conn);

        let r = index_size_breakdown(p.to_str().unwrap()).unwrap();
        let ratio = r.documents_text_ratio().expect("has rows");
        // Repeating a 44-byte sentence 500x → zstd should hit <20% ratio
        // trivially. Loose bound protects the test from zstd version churn.
        assert!(
            ratio < 0.3,
            "ratio too high: {ratio} raw={} comp={}",
            r.documents_text_raw_bytes,
            r.documents_text_compressed_bytes
        );

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

    #[test]
    fn clear_path_on_nondefault_tokenizer_db_removes_only_target() {
        // On an index built with a non-default tokenizer, clear_path must
        // delete only the target row — never trigger the owner's
        // schema-mismatch wipe.
        let p = tmp_path();
        let dbp = p.to_str().unwrap();
        {
            let mut conn = open_or_recreate(dbp, "unicode61").unwrap();
            let tx = conn.transaction().unwrap();
            for (name, path) in [("a.txt", "/tmp/a.txt"), ("b.txt", "/tmp/b.txt")] {
                insert_file(
                    &tx,
                    &NewFile {
                        name,
                        path,
                        parent: "/tmp",
                        size: 1,
                        mtime: 1,
                        mime: Some("text/plain"),
                        ftype: FileType::TEXT,
                        hash: None,
                        needs_content: true,
                    },
                )
                .unwrap()
                .expect("unique path");
            }
            tx.commit().unwrap();
        }

        assert!(clear_path(dbp, "/tmp/a.txt").unwrap());
        // The other row must survive — proof we deleted one row, not wiped.
        assert_eq!(
            status_for_path(dbp, "/tmp/b.txt").unwrap().basic,
            IndexState::Done
        );
        assert_eq!(
            status_for_path(dbp, "/tmp/a.txt").unwrap().basic,
            IndexState::NotIndexed
        );

        std::fs::remove_file(&p).ok();
    }
}
