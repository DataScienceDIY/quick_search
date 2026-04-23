//! Open-or-recreate: the sole entry point into the on-disk database.
//!
//! **Policy**: any schema mismatch — wrong `schema_info.version`, wrong
//! stored `tokenize` string, absent `schema_info` table, or any other
//! drift from what this build expects — wipes the database file and
//! recreates it from scratch. There are deliberately **no** in-place
//! migrations.
//!
//! The tradeoff: users pay a re-index cost every time the shipped schema
//! changes. Our indexing is fast (see `bench/`) and schema changes are
//! rare in practice, so the code-complexity cost of maintaining real
//! migration paths wasn't worth it. A single `open_or_recreate` replaces
//! what used to be version detection + tokenizer-drift FTS rebuild +
//! legacy-layout recovery, all of which ultimately wiped anyway.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use super::schema::{effective_tokenizer, fts_create_sql, PRAGMAS_FAST, SCHEMA_CURRENT};

/// Bump this whenever [`SCHEMA_CURRENT`] or [`fts_create_sql`] changes in
/// a way that makes an old DB unreadable by new code. Any such bump
/// causes existing indexes to be wiped on next open — there's no
/// migration path by design.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Open `db_path`, applying fast-path pragmas, and ensure the on-disk
/// schema matches what this build expects. If it doesn't, delete the
/// file and recreate it empty — callers will need to re-index.
///
/// `tokenizer` is passed to FTS5's `tokenize=` option when (re)creating
/// `searchabletext`. Changing it against an existing DB counts as a
/// schema mismatch and triggers the wipe-and-recreate path.
pub fn open_or_recreate(db_path: &str, tokenizer: &str) -> Result<Connection, String> {
    let path = Path::new(db_path).to_path_buf();
    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    if db_matches_current(&conn, tokenizer)? {
        return Ok(conn);
    }

    // Schema is present but stale, or pre-existing rows belong to an
    // older layout, or the tokenizer drifted. Log once so the rebuild
    // isn't silent, then wipe + recreate.
    eprintln!(
        "QuickSearch: database at {} does not match current schema; rebuilding. \
         Existing rows will be re-scanned on next indexing run.",
        db_path
    );
    let conn = wipe_and_reopen(conn, &path)?;
    apply_current_schema(&conn, tokenizer)?;
    Ok(conn)
}

/// True iff the DB has `schema_info` with the current version *and* the
/// effective-tokenizer string this caller asked for. Anything else —
/// missing table, wrong version, different tokenizer — returns false.
fn db_matches_current(conn: &Connection, tokenizer: &str) -> Result<bool, String> {
    let has_info: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_info'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(|e| format!("sqlite_master schema_info: {}", e))?
        .unwrap_or(false);
    if !has_info {
        return Ok(false);
    }

    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key = 'version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("read schema_info.version: {}", e))?;
    let version_ok = version.as_deref() == Some(&CURRENT_SCHEMA_VERSION.to_string());
    if !version_ok {
        return Ok(false);
    }

    let stored_tokenize: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key = 'tokenize'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("read schema_info.tokenize: {}", e))?;
    let want_tokenize = effective_tokenizer(tokenizer);
    Ok(stored_tokenize.as_deref() == Some(&*want_tokenize))
}

/// Drop the current connection, delete the DB file + its WAL/SHM/journal
/// sidecars, reopen a fresh file, re-apply pragmas.
fn wipe_and_reopen(conn: Connection, path: &Path) -> Result<Connection, String> {
    drop(conn);
    // Primary file may already be absent (fresh open that just needed
    // the table applied). Ignore NotFound; anything else is an error.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Failed to remove old database: {}", e)),
    }
    // Sidecars are optional — delete best-effort.
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path.with_file_name(format!(
            "{}{}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            suffix
        ));
        let _ = std::fs::remove_file(sidecar);
    }
    let conn = Connection::open(path)
        .map_err(|e| format!("Failed to reopen database after rebuild: {}", e))?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas after rebuild: {}", e))?;
    Ok(conn)
}

/// Apply [`SCHEMA_CURRENT`] + [`fts_create_sql`] to a blank DB and seed
/// `schema_info` with the matching version/tokenize markers.
fn apply_current_schema(conn: &Connection, tokenizer: &str) -> Result<(), String> {
    conn.execute_batch(SCHEMA_CURRENT)
        .map_err(|e| format!("Failed to create current schema tables: {}", e))?;
    let fts = fts_create_sql(tokenizer);
    conn.execute_batch(&fts)
        .map_err(|e| format!("Failed to create searchabletext: {}", e))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let effective = effective_tokenizer(tokenizer);
    conn.execute(
        "INSERT INTO schema_info(key, value) VALUES ('version', ?1), ('created_at', ?2), ('tokenize', ?3)",
        params![
            CURRENT_SCHEMA_VERSION.to_string(),
            now.to_string(),
            effective
        ],
    )
    .map_err(|e| format!("Failed to seed schema_info: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn fresh_db_gets_current_version() {
        let p = tmp_db_path();
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn reopen_is_idempotent() {
        let p = tmp_db_path();
        {
            let _ = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        }
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn older_versioned_db_is_wiped_and_recreated() {
        // Simulate a DB from a prior schema version. Our policy is to
        // wipe without attempting any migration.
        let p = tmp_db_path();
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(
                "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schema_info(key,value) VALUES('version','1')",
                [],
            )
            .unwrap();
            conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
                .unwrap();
            conn.execute("INSERT INTO files(name) VALUES('a.txt')", [])
                .unwrap();
        }
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "old rows should be wiped");
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn legacy_layout_db_is_wiped_and_recreated() {
        // Pre-A layout with no `schema_info` at all. Same policy — wipe.
        let p = tmp_db_path();
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(
                "CREATE TABLE files (name TEXT, path TEXT, size INTEGER, moddate INTEGER, hash BLOB)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files VALUES ('a.txt', '/tmp/a.txt', 1, 2, X'00')",
                [],
            )
            .unwrap();
        }
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        // Old row should be gone.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // New columns should exist (just prepare the SELECT — an
        // unknown column name would parse-error here).
        let _ = conn
            .query_row(
                "SELECT basic_state, content_state, type, mime FROM files LIMIT 0",
                [],
                |_| Ok(()),
            )
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .unwrap();
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn tokenizer_drift_wipes_db() {
        // Previously this was "rebuild FTS in place and reset
        // content_state". New policy: full wipe.
        let p = tmp_db_path();
        let first_effective = {
            let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime) \
                 VALUES ('x', '/x', '/', 0, 0)",
                [],
            )
            .unwrap();
            let stored: String = conn
                .query_row(
                    "SELECT value FROM schema_info WHERE key='tokenize'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            drop(conn);
            stored
        };
        // Second open with a different tokenizer.
        let conn = open_or_recreate(p.to_str().unwrap(), "unicode61").unwrap();
        let files_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files_count, 0, "tokenizer drift should wipe rows");
        let new_stored: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='tokenize'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(first_effective, new_stored);
        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
