//! Schema version detection and upgrade.
//!
//! Callers should always enter the DB via [`open_and_migrate`]. It applies
//! pragmas, detects the on-disk schema version, and upgrades or recreates as
//! required.
//!
//! Current policy: Set A introduces schema v1 and is the first versioned
//! release. Any pre-A database (has the legacy `files(name, path, size,
//! moddate, hash)` shape and no `schema_info` table) is wiped and rebuilt —
//! the user will re-index. A prominent log line is printed so the behavior is
//! not silent. Future migrations should prefer ALTER TABLE.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use super::schema::{fts_create_sql, PRAGMAS_FAST, SCHEMA_CURRENT};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Open the database at `db_path`, apply pragmas, and ensure the schema is at
/// [`CURRENT_SCHEMA_VERSION`]. Recreates the DB if a pre-versioned layout is
/// detected.
///
/// `tokenizer` is used when (re)creating the FTS5 virtual table. It has no
/// effect on an already-current DB.
pub fn open_and_migrate(db_path: &str, tokenizer: &str) -> Result<Connection, String> {
    let path_for_rebuild = Path::new(db_path).to_path_buf();
    let mut conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;

    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    let version = read_schema_version(&conn)?;
    match version {
        Some(v) if v == CURRENT_SCHEMA_VERSION => {}
        Some(v) if v > CURRENT_SCHEMA_VERSION => {
            return Err(format!(
                "Database schema version {} is newer than this build ({}). \
                 Use a newer QuickSearch or move the database aside.",
                v, CURRENT_SCHEMA_VERSION
            ));
        }
        Some(v) => {
            // Older schema version. Set A's upgrade policy: wipe and rebuild.
            // When we start adding ALTER-based migrations this match arm
            // will gain a proper stepwise runner.
            eprintln!(
                "QuickSearch: database at {} is schema v{}; rebuilding to v{}. \
                 Existing rows will be re-scanned.",
                db_path, v, CURRENT_SCHEMA_VERSION
            );
            conn = wipe_and_reopen(conn, &path_for_rebuild)?;
            apply_current_schema(&conn, tokenizer)?;
        }
        None => {
            // No schema_info row. Either an empty DB (good — just create) or a
            // legacy pre-A layout (detected by presence of the old `files`
            // table). Legacy layouts are wiped.
            if has_legacy_layout(&conn)? {
                eprintln!(
                    "QuickSearch: legacy database detected at {}; rebuilding index with schema v{}. \
                     Existing file rows will be re-scanned.",
                    db_path, CURRENT_SCHEMA_VERSION
                );
                conn = wipe_and_reopen(conn, &path_for_rebuild)?;
            }
            apply_current_schema(&conn, tokenizer)?;
        }
    }

    Ok(conn)
}

fn wipe_and_reopen(
    conn: Connection,
    path_for_rebuild: &std::path::Path,
) -> Result<Connection, String> {
    drop(conn);
    std::fs::remove_file(path_for_rebuild)
        .map_err(|e| format!("Failed to remove old database: {}", e))?;
    // Remove WAL/SHM/journal sidecars defensively even though journal_mode=OFF.
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path_for_rebuild.with_file_name(format!(
            "{}{}",
            path_for_rebuild
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            suffix
        ));
        let _ = std::fs::remove_file(sidecar);
    }
    let conn = Connection::open(path_for_rebuild)
        .map_err(|e| format!("Failed to reopen database after rebuild: {}", e))?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas after rebuild: {}", e))?;
    Ok(conn)
}

fn read_schema_version(conn: &Connection) -> Result<Option<u32>, String> {
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
        return Ok(None);
    }
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key = 'version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("read schema_info.version: {}", e))?;
    match v {
        Some(s) => s
            .parse::<u32>()
            .map(Some)
            .map_err(|e| format!("invalid schema_info.version {:?}: {}", s, e)),
        None => Ok(None),
    }
}

fn has_legacy_layout(conn: &Connection) -> Result<bool, String> {
    // Old layout has a `files` table without an `id INTEGER PRIMARY KEY`.
    let has_files: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(|e| format!("sqlite_master files: {}", e))?
        .unwrap_or(false);
    if !has_files {
        return Ok(false);
    }
    // Check whether the columns match the legacy shape.
    let mut stmt = conn
        .prepare("PRAGMA table_info(files)")
        .map_err(|e| format!("pragma table_info: {}", e))?;
    let has_id = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("table_info query: {}", e))?
        .filter_map(|r| r.ok())
        .any(|name| name == "id");
    Ok(!has_id)
}

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
    conn.execute(
        "INSERT INTO schema_info(key, value) VALUES ('version', ?1), ('created_at', ?2), ('tokenize', ?3)",
        params![CURRENT_SCHEMA_VERSION.to_string(), now.to_string(), tokenizer],
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
        let conn = open_and_migrate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM schema_info WHERE key='version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn reopen_is_idempotent() {
        let p = tmp_db_path();
        {
            let _ = open_and_migrate(p.to_str().unwrap(), "trigram").unwrap();
        }
        let conn = open_and_migrate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM schema_info WHERE key='version'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn older_versioned_db_is_wiped_and_recreated() {
        // Simulate a DB that was created at a previous schema version.
        let p = tmp_db_path();
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute("CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
            conn.execute(
                "INSERT INTO schema_info(key,value) VALUES('version','1')",
                [],
            )
            .unwrap();
            conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
            conn.execute("INSERT INTO files(name) VALUES('a.txt')", []).unwrap();
        }
        let conn = open_and_migrate(p.to_str().unwrap(), "trigram").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM schema_info WHERE key='version'", [], |r| r.get(0))
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
    fn legacy_db_is_wiped_and_recreated() {
        let p = tmp_db_path();
        {
            // Simulate a pre-A database.
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
        let conn = open_and_migrate(p.to_str().unwrap(), "trigram").unwrap();
        // Old row should be gone.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // New columns should exist.
        let _ = conn
            .query_row("SELECT basic_state, content_state, type, mime FROM files LIMIT 0", [], |_| Ok(()))
            .or_else(|e| if matches!(e, rusqlite::Error::QueryReturnedNoRows) { Ok(()) } else { Err(e) })
            .unwrap();
        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
