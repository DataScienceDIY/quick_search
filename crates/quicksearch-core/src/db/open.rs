//! Open-or-recreate: the sole entry point into the on-disk database.
//! **Policy**: any schema mismatch wipes the database file and recreates it
//! from scratch. There are **no** in-place migrations.

use std::path::Path;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::schema::{
    effective_tokenizer, fts_create_sql, PRAGMAS_FAST, PRAGMAS_INCREMENTAL, PRAGMAS_MAINTENANCE,
    PRAGMAS_READONLY, PRAGMAS_SEARCH, PRAGMAS_WALK_READER, SCHEMA_CURRENT,
};
use crate::security::IndexKey;

/// Prefix tagging every "the key doesn't fit this file" error, telling a
/// wrong password apart from corruption or schema drift — critically, nothing
/// treats it as a reason to wipe or "recover" the database.
pub const KEY_MISMATCH_PREFIX: &str = "KEY_MISMATCH: ";

/// Bump on any schema change — and on classifier changes: `files.mime`,
/// `files.type` and `content_state` are computed at walk time and never
/// re-derived for unchanged files, so only the wipe applies them everywhere.
pub const CURRENT_SCHEMA_VERSION: u32 = 8;

/// Open `db_path`; on any schema/tokenizer mismatch, delete the file and
/// recreate it empty — callers will need to re-index.
pub fn open_or_recreate(db_path: &str, tokenizer: &str) -> Result<Connection, String> {
    open_or_recreate_keyed(db_path, tokenizer, super::key::process_key().as_ref())
}

pub(crate) fn open_or_recreate_keyed(
    db_path: &str,
    tokenizer: &str,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    let path = Path::new(db_path).to_path_buf();
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            crate::platform::create_dir_private(dir)
                .map_err(|e| format!("Failed to create database dir {}: {}", dir.display(), e))?;
        }
    }
    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;
    // Before a single row is written: SQLite creates the file 0644 (inherited
    // by `-wal`/`-shm`), and the index holds the full text of files whose own
    // permissions are 0600.
    crate::platform::restrict_to_owner(&path);
    key_and_probe(&conn, db_path, key)?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    if db_matches_current(&conn, tokenizer)? {
        return Ok(conn);
    }

    crate::log_warn!(
        "database at {} does not match current schema; rebuilding. \
         Existing rows will be re-scanned on next indexing run.",
        db_path
    );
    let conn = wipe_and_reopen(conn, &path, key)?;
    apply_current_schema(&conn, tokenizer)?;
    Ok(conn)
}

/// Open an *existing* index: any schema mismatch is an error instead of a
/// wipe. Every *consumer* uses this; only the indexer's own write path uses
/// [`open_or_recreate`].
pub fn open_existing(db_path: &str, write: bool) -> Result<Connection, String> {
    open_existing_keyed(db_path, write, super::key::process_key().as_ref())
}

fn open_profiled(db_path: &str, write: bool, pragmas: &str) -> Result<Connection, String> {
    open_keyed_with_pragmas(db_path, write, super::key::process_key().as_ref(), pragmas)
}

/// A read-only connection for one walk's row prefetcher.
pub fn open_walk_reader(db_path: &str) -> Result<Connection, String> {
    open_profiled(db_path, false, PRAGMAS_WALK_READER)
}

/// The search worker's connection, held across requests.
pub fn open_search_reader(db_path: &str) -> Result<Connection, String> {
    open_profiled(db_path, false, PRAGMAS_SEARCH)
}

/// The coordinator's write connection for watcher events and reconciles.
pub fn open_incremental_writer(db_path: &str) -> Result<Connection, String> {
    open_profiled(db_path, true, PRAGMAS_INCREMENTAL)
}

/// A writable connection for post-run compaction; the only one that may
/// VACUUM.
pub fn open_maintenance(db_path: &str) -> Result<Connection, String> {
    open_profiled(db_path, true, PRAGMAS_MAINTENANCE)
}

pub(crate) fn open_existing_keyed(
    db_path: &str,
    write: bool,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    let pragmas = if write {
        PRAGMAS_FAST
    } else {
        PRAGMAS_READONLY
    };
    open_keyed_with_pragmas(db_path, write, key, pragmas)
}

fn open_keyed_with_pragmas(
    db_path: &str,
    write: bool,
    key: Option<&IndexKey>,
    pragmas: &str,
) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
        | if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
    let conn = Connection::open_with_flags(db_path, flags)
        .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;
    key_and_probe(&conn, db_path, key)?;
    conn.execute_batch(pragmas)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    if !schema_version_current(&conn)? {
        return Err(format!(
            "index at {} is not a compatible QuickSearch index (schema v{} expected); \
             refusing to modify it. Re-index to rebuild.",
            db_path, CURRENT_SCHEMA_VERSION
        ));
    }
    Ok(conn)
}

/// Cheaply check that the process key (or its absence) actually opens the
/// index. Answers **only** the key question, not the schema check: conflating
/// the two made every schema bump look like an unlock failure.
pub fn verify_process_key(db_path: &str) -> Result<(), String> {
    verify_key(db_path, super::key::process_key().as_ref())
}

/// Whether the next indexing run will discard and rebuild an existing index.
/// `false` for anything this cannot positively establish: announcing a reset
/// that is not happening would be worse than saying nothing.
pub fn index_needs_rebuild(db_path: &str) -> bool {
    let Ok(conn) = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return false;
    };
    if key_and_probe(&conn, db_path, super::key::process_key().as_ref()).is_err() {
        return false;
    }
    // Only `Ok(false)`: an `Err` means we could not tell.
    matches!(schema_version_current(&conn), Ok(false))
}

pub(crate) fn verify_key(db_path: &str, key: Option<&IndexKey>) -> Result<(), String> {
    // Read-only and no CREATE: verifying a key must never bring a database
    // into existence, and must never modify one.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;
    key_and_probe(&conn, db_path, key)
}

/// Apply the SQLCipher key (if any) and force the first page off disk.
///
/// Ordering is load-bearing twice over: SQLCipher requires `PRAGMA key`
/// before anything else touches the file, and the probe must run before any
/// schema comparison so a wrong key surfaces as [`KEY_MISMATCH_PREFIX`] —
/// never as a "schema mismatch" that [`open_or_recreate`] answers by wiping.
/// The raw-key `x'…'` form bypasses SQLCipher's per-connection PBKDF2.
fn key_and_probe(conn: &Connection, db_path: &str, key: Option<&IndexKey>) -> Result<(), String> {
    if let Some(key) = key {
        // `cipher_log_level = NONE` mutes SQLCipher's stderr HMAC trace on
        // wrong-password attempts; it must follow `PRAGMA key`, which has to
        // be the first statement on the connection.
        conn.execute_batch(&format!(
            "PRAGMA key = \"x'{}'\"; PRAGMA cipher_log_level = NONE;",
            key.to_hex()
        ))
        .map_err(|e| format!("Failed to apply encryption key: {}", e))?;
    }
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(e) if is_notadb(&e) => Err(key_mismatch_message(db_path, key.is_some())),
        Err(e) => Err(format!("Failed to read database at {}: {}", db_path, e)),
    }
}

/// SQLITE_NOTADB is what an undecryptable first page looks like: the
/// decrypted header bytes are noise.
fn is_notadb(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::NotADatabase,
                ..
            },
            _,
        )
    )
}

/// Why a keyed open failed, as something the caller can branch on — the three
/// cases want three different things from a user, and matching on message
/// prose breaks the moment a database path contains that prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMismatch {
    /// A key was applied and the file did not accept it.
    WrongPassword,
    /// A key was applied but the file is not encrypted at all — the rebuild
    /// that would encrypt it did not finish.
    NotEncrypted,
    /// No key was applied and the file wants one.
    PasswordRequired,
}

impl KeyMismatch {
    fn token(self) -> &'static str {
        match self {
            KeyMismatch::WrongPassword => "wrong-password",
            KeyMismatch::NotEncrypted => "not-encrypted",
            KeyMismatch::PasswordRequired => "password-required",
        }
    }

    fn from_token(token: &str) -> Option<KeyMismatch> {
        match token {
            "wrong-password" => Some(KeyMismatch::WrongPassword),
            "not-encrypted" => Some(KeyMismatch::NotEncrypted),
            "password-required" => Some(KeyMismatch::PasswordRequired),
            _ => None,
        }
    }
}

/// Split a tagged mismatch message into its cause and the human detail;
/// `None` for any message that is not one.
pub fn key_mismatch_parts(message: &str) -> Option<(KeyMismatch, &str)> {
    let rest = message.strip_prefix(KEY_MISMATCH_PREFIX)?;
    let (token, detail) = rest.split_once(' ')?;
    let token = token.strip_suffix(':')?;
    Some((KeyMismatch::from_token(token)?, detail))
}

fn key_mismatch_message(db_path: &str, had_key: bool) -> String {
    // An unencrypted SQLite file still has its plaintext magic; sniffing it
    // distinguishes "wrong password" from "the index was never encrypted".
    let plaintext = std::fs::File::open(db_path)
        .ok()
        .and_then(|mut f| {
            use std::io::Read;
            let mut magic = [0u8; 16];
            f.read_exact(&mut magic).ok()?;
            Some(&magic == b"SQLite format 3\0")
        })
        .unwrap_or(false);
    let (cause, detail) = match (had_key, plaintext) {
        (true, true) => (
            KeyMismatch::NotEncrypted,
            "password protection is enabled but the index is not encrypted; \
             rebuild the index to encrypt it",
        ),
        (true, false) => (
            KeyMismatch::WrongPassword,
            "wrong password (or the file is not a QuickSearch index)",
        ),
        (false, _) => (
            KeyMismatch::PasswordRequired,
            "the index is password-protected; a password is required",
        ),
    };
    format!(
        "{}{}: index at {}: {}",
        KEY_MISMATCH_PREFIX,
        cause.token(),
        db_path,
        detail
    )
}

/// Prefix tagging the "this file is not a QuickSearch index" refusal, so a
/// caller can tell it from the schema drift that legitimately rebuilds.
pub const FOREIGN_DB_PREFIX: &str = "FOREIGN_DB: ";

/// Tables left behind by the pre-`schema_info` layout. `files` is the loose
/// end: another application's database with a table called `files` would be
/// taken for an ancient index of ours and wiped — but refusing a genuine
/// legacy index is the worse failure of the two, so it stays.
const LEGACY_TABLES: &[&str] = &["files", "files_fts", "documents_text", "failed_files"];

/// Whether `schema_info` exists *and* is shaped like ours. The shape, not the
/// contents: an index whose creation was interrupted before the version row
/// landed is still ours; a foreign table reusing the name is not.
fn has_our_schema_info(conn: &Connection) -> bool {
    conn.prepare("SELECT key, value FROM schema_info").is_ok()
}

/// Whether the file is one of ours, or empty enough to become one. Internal
/// `sqlite_%` names are excluded so an autoindex cannot make an
/// otherwise-empty file look occupied.
fn is_ours_or_empty(conn: &Connection) -> Result<bool, String> {
    if has_our_schema_info(conn) {
        return Ok(true);
    }
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
        .map_err(|e| format!("read sqlite_master: {}", e))?;
    let mut any = false;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("read sqlite_master: {}", e))?;
    for name in names {
        let name = name.map_err(|e| format!("read sqlite_master: {}", e))?;
        any = true;
        if LEGACY_TABLES.contains(&name.as_str()) {
            return Ok(true);
        }
    }
    Ok(!any)
}

/// The refusal message, naming a few of the tables in the way.
fn foreign_database_message(conn: &Connection) -> Result<String, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name LIMIT 4",
        )
        .map_err(|e| format!("read sqlite_master: {}", e))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("read sqlite_master: {}", e))?
        .filter_map(Result::ok)
        .collect();
    Ok(format!(
        "{}the file is a SQLite database, but not a QuickSearch index \
         (it holds {}). Refusing to replace it — point [paths] database_path \
         somewhere else, or move that file away first.",
        FOREIGN_DB_PREFIX,
        if names.is_empty() {
            "tables this program does not recognise".to_string()
        } else {
            names.join(", ")
        }
    ))
}

fn schema_version_current(conn: &Connection) -> Result<bool, String> {
    if !has_our_schema_info(conn) {
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
    Ok(version.as_deref() == Some(&CURRENT_SCHEMA_VERSION.to_string()))
}

/// True iff the DB has the current schema version *and* the
/// effective-tokenizer string this caller asked for.
fn db_matches_current(conn: &Connection, tokenizer: &str) -> Result<bool, String> {
    if !schema_version_current(conn)? {
        // Refuse rather than wipe unless the file is recognisably ours:
        // `database_path` is free text with no confirmation, and a typo
        // naming some other application's SQLite file would otherwise delete
        // it on the next indexing run.
        if !is_ours_or_empty(conn)? {
            return Err(foreign_database_message(conn)?);
        }
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

/// Delete the DB file + sidecars, reopen a fresh file, re-apply key and
/// pragmas. Re-keying here is essential: a rebuild of a protected index must
/// come back encrypted, never silently plaintext.
fn wipe_and_reopen(
    conn: Connection,
    path: &Path,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    drop(conn);
    // Before the delete, even if the removal fails partway.
    super::bump_index_epoch();
    // `remove_file_retrying` matters on Windows, where a delete fails while
    // *any* handle is open — most often an antivirus scanner.
    match crate::platform::remove_file_retrying(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "Failed to remove old database at {}: {}. \
                 Another QuickSearch instance may have the index open.",
                path.display(),
                e
            ))
        }
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path.with_file_name(format!(
            "{}{}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            suffix
        ));
        let _ = crate::platform::remove_file_retrying(&sidecar);
    }
    let conn = Connection::open(path)
        .map_err(|e| format!("Failed to reopen database after rebuild: {}", e))?;
    crate::platform::restrict_to_owner(path);
    key_and_probe(&conn, &path.to_string_lossy(), key)?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas after rebuild: {}", e))?;
    Ok(conn)
}

fn apply_current_schema(conn: &Connection, tokenizer: &str) -> Result<(), String> {
    conn.execute_batch(SCHEMA_CURRENT)
        .map_err(|e| format!("Failed to create current schema tables: {}", e))?;
    let fts = fts_create_sql(tokenizer);
    conn.execute_batch(&fts)
        .map_err(|e| format!("Failed to create searchabletext: {}", e))?;

    let now = crate::log::now_unix();
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
#[path = "open_tests.rs"]
mod tests;
