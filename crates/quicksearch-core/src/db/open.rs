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

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::schema::{
    effective_tokenizer, fts_create_sql, PRAGMAS_FAST, PRAGMAS_READONLY, PRAGMAS_WALK_READER,
    SCHEMA_CURRENT,
};
use crate::security::IndexKey;

/// Prefix tagging every "the key doesn't fit this file" error. Callers use
/// it to tell a wrong password apart from real corruption or schema drift:
/// the GUI re-prompts, the CLI retries, and — critically — nothing treats
/// it as a reason to wipe or "recover" the database.
pub const KEY_MISMATCH_PREFIX: &str = "KEY_MISMATCH: ";

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
    open_or_recreate_keyed(db_path, tokenizer, super::key::process_key().as_ref())
}

pub(crate) fn open_or_recreate_keyed(
    db_path: &str,
    tokenizer: &str,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    let path = Path::new(db_path).to_path_buf();
    // The owner creates the directory too — a fresh install's default
    // XDG data dir doesn't exist until first use.
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Failed to create database dir {}: {}", dir.display(), e))?;
        }
    }
    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database at {}: {}", db_path, e))?;
    key_and_probe(&conn, db_path, key)?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    if db_matches_current(&conn, tokenizer)? {
        return Ok(conn);
    }

    // Schema is present but stale, or pre-existing rows belong to an
    // older layout, or the tokenizer drifted. Log once so the rebuild
    // isn't silent, then wipe + recreate.
    crate::log_warn!(
        "database at {} does not match current schema; rebuilding. \
         Existing rows will be re-scanned on next indexing run.",
        db_path
    );
    let conn = wipe_and_reopen(conn, &path, key)?;
    apply_current_schema(&conn, tokenizer)?;
    Ok(conn)
}

/// Open an *existing* index without ever recreating it. Verifies the schema
/// version matches this build; on any mismatch — missing file, no
/// `schema_info`, wrong version — returns an error instead of wiping. The
/// on-disk FTS tokenizer is used as-is: a tokenizer difference is never a
/// reason to destroy a readable index.
///
/// `write == false` opens read-only; `write == true` opens read-write (for
/// row-level deletes like `clear`) but still never creates or wipes — there
/// is no `SQLITE_OPEN_CREATE`, so a missing file is a clean error.
///
/// Use this for every *consumer* (search, status, size, `clear`). Only the
/// indexer's own write path uses [`open_or_recreate`], which may wipe on a
/// genuine schema/tokenizer change it owns.
pub fn open_existing(db_path: &str, write: bool) -> Result<Connection, String> {
    open_existing_keyed(db_path, write, super::key::process_key().as_ref())
}

/// A read-only connection for one walk's row prefetcher.
///
/// Identical to `open_existing(_, false)` except for the pragma profile: see
/// [`PRAGMAS_WALK_READER`] for why these connections must not take the
/// 40 MiB page cache the other profiles use.
pub fn open_walk_reader(db_path: &str) -> Result<Connection, String> {
    open_keyed_with_pragmas(
        db_path,
        false,
        super::key::process_key().as_ref(),
        PRAGMAS_WALK_READER,
    )
}

pub(crate) fn open_existing_keyed(
    db_path: &str,
    write: bool,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    let pragmas = if write { PRAGMAS_FAST } else { PRAGMAS_READONLY };
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
/// index. Used by the GUI unlock screen and the CLI prompt loop before any
/// service starts; the error carries [`KEY_MISMATCH_PREFIX`] on a wrong
/// password.
pub fn verify_process_key(db_path: &str) -> Result<(), String> {
    open_existing(db_path, false).map(|_| ())
}

/// Apply the SQLCipher key (if any) and force the first page off disk.
///
/// Ordering is load-bearing twice over: SQLCipher requires `PRAGMA key`
/// before anything else touches the file (our fast-path pragmas include
/// `journal_mode = WAL`, which reads the header), and the probe must run
/// before any schema comparison so that a wrong or missing key surfaces as
/// a tagged [`KEY_MISMATCH_PREFIX`] error — never as a "schema mismatch"
/// that [`open_or_recreate`] would answer by wiping the file.
///
/// The raw-key `x'…'` form bypasses SQLCipher's per-connection PBKDF2
/// (hundreds of ms); searches, status counts and duplicate scans each open
/// fresh connections, so the expensive KDF happens once at unlock, not per
/// open.
fn key_and_probe(conn: &Connection, db_path: &str, key: Option<&IndexKey>) -> Result<(), String> {
    if let Some(key) = key {
        // `cipher_log_level = NONE` mutes SQLCipher's own stderr logging
        // (an HMAC-failure trace on every wrong-password attempt); the
        // condition still surfaces through the API as SQLITE_NOTADB and is
        // reported cleanly below. It must follow `PRAGMA key`, which has to
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

/// SQLITE_NOTADB is what an undecryptable first page looks like: with the
/// wrong key (or none) the decrypted header bytes are noise, and SQLite
/// reports "file is not a database".
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

fn key_mismatch_message(db_path: &str, had_key: bool) -> String {
    // An unencrypted SQLite file still has its plaintext magic; sniffing it
    // distinguishes "wrong password" from "protection is enabled but the
    // index was never encrypted" (e.g. a crash between saving the config
    // and rebuilding the index).
    let plaintext = std::fs::File::open(db_path)
        .ok()
        .and_then(|mut f| {
            use std::io::Read;
            let mut magic = [0u8; 16];
            f.read_exact(&mut magic).ok()?;
            Some(&magic == b"SQLite format 3\0")
        })
        .unwrap_or(false);
    let detail = match (had_key, plaintext) {
        (true, true) => "password protection is enabled but the index is not encrypted; \
                         rebuild the index to encrypt it",
        (true, false) => "wrong password (or the file is not a QuickSearch index)",
        (false, _) => "the index is password-protected; a password is required",
    };
    format!("{}index at {}: {}", KEY_MISMATCH_PREFIX, db_path, detail)
}

/// True iff the DB has a `schema_info` table whose `version` equals
/// [`CURRENT_SCHEMA_VERSION`]. Shared by the wipe decision
/// ([`db_matches_current`]) and the non-destructive [`open_existing`] path.
/// Deliberately ignores the tokenizer — that's only the owner's concern.
fn schema_version_current(conn: &Connection) -> Result<bool, String> {
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
    Ok(version.as_deref() == Some(&CURRENT_SCHEMA_VERSION.to_string()))
}

/// True iff the DB has `schema_info` with the current version *and* the
/// effective-tokenizer string this caller asked for. Anything else —
/// missing table, wrong version, different tokenizer — returns false.
fn db_matches_current(conn: &Connection, tokenizer: &str) -> Result<bool, String> {
    if !schema_version_current(conn)? {
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
/// sidecars, reopen a fresh file, re-apply key and pragmas. Re-keying here
/// is essential: a rebuild of a protected index must come back encrypted,
/// never silently plaintext.
fn wipe_and_reopen(
    conn: Connection,
    path: &Path,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    drop(conn);
    // Primary file may already be absent (fresh open that just needed
    // the table applied). Ignore NotFound; anything else is an error.
    //
    // `remove_file_retrying` matters on Windows, where a delete fails while
    // *any* handle is open — most often an antivirus scanner reading the file
    // in the moment after we closed it. Unix `unlink` never hits this, so the
    // retry costs nothing there.
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
    // Sidecars are optional — delete best-effort.
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
    key_and_probe(&conn, &path.to_string_lossy(), key)?;
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

    #[test]
    fn open_existing_reads_nondefault_tokenizer_without_wiping() {
        // The exact scenario that previously caused data loss: an index built
        // with a non-default tokenizer, then opened by a *consumer* that only
        // knows "trigram". `open_existing` must read it as-is and never wipe.
        let p = tmp_db_path();
        {
            let conn = open_or_recreate(p.to_str().unwrap(), "unicode61").unwrap();
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime) \
                 VALUES ('note', '/note.txt', '/', 0, 0)",
                [],
            )
            .unwrap();
            // Seed the FTS index (rowid = the files row we just inserted) so a
            // MATCH query can be exercised against the on-disk tokenizer.
            conn.execute(
                "INSERT INTO searchabletext (rowid, name, text, properties) \
                 VALUES (last_insert_rowid(), 'note', 'hello world', '')",
                [],
            )
            .unwrap();
        }

        let conn = open_existing(p.to_str().unwrap(), false).unwrap();
        let files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            files, 1,
            "open_existing must not wipe a non-default-tokenizer DB"
        );
        // The on-disk tokenizer is used as-is: a MATCH against the stored term
        // returns the row.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
        // And the stored tokenizer is still the non-default one — proof we
        // neither rewrote the FTS table nor reset schema_info.
        let tok: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='tokenize'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tok, "unicode61");
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn open_or_recreate_creates_missing_parent_dirs() {
        // Fresh installs point at ~/.local/share/quicksearch/… which
        // doesn't exist yet; the owner open must create it.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "qs-mkdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = dir.join("nested/deeper/index.sqlite");
        let conn = open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
        drop(conn);
        assert!(db.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writable_opens_use_wal_and_it_persists() {
        let p = tmp_db_path();
        {
            let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode.to_lowercase(), "wal");
        }
        // WAL is persistent in the file: a later read-only consumer sees it
        // without being able to (or needing to) set it.
        let conn = open_existing(p.to_str().unwrap(), false).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn open_existing_errors_on_missing_file() {
        let p = tmp_db_path();
        assert!(!p.exists());
        let res = open_existing(p.to_str().unwrap(), false);
        assert!(res.is_err(), "missing file must error, not be created");
        assert!(!p.exists(), "open_existing must not create the file");
    }

    #[test]
    fn open_existing_errors_on_version_mismatch_without_wiping() {
        // A DB from a prior schema version. A consumer opening it must get an
        // error and leave the file untouched — the data is the owner's to
        // rebuild, never a reader's to destroy.
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
            conn.execute("INSERT INTO files(name) VALUES('sentinel')", [])
                .unwrap();
        }
        let res = open_existing(p.to_str().unwrap(), false);
        assert!(res.is_err(), "stale schema version must error");
        // Sentinel row still present → the file was not wiped.
        let conn = Connection::open(&p).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "open_existing must never delete on version mismatch");
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    fn test_key(seed: u8) -> IndexKey {
        IndexKey::from_hex(&format!("{:02x}", seed).repeat(32)).unwrap()
    }

    fn file_bytes(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    #[test]
    fn keyed_create_reopen_and_header_is_encrypted() {
        let p = tmp_db_path();
        let key = test_key(0xa1);
        {
            let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime) \
                 VALUES ('secret', '/secret.txt', '/', 0, 0)",
            [],
            )
            .unwrap();
        }
        // Encrypted at rest: the plaintext SQLite magic must be gone.
        let head = &file_bytes(&p)[..16];
        assert_ne!(head, b"SQLite format 3\0", "file must not be plaintext");

        // Reopens with the same key, both owner and consumer paths.
        {
            let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "keyed reopen must see existing rows, not wipe");
        }
        let conn = open_existing_keyed(p.to_str().unwrap(), false, Some(&key)).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(conn);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn wrong_key_errors_without_wiping() {
        let p = tmp_db_path();
        {
            let conn =
                open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1)))
                    .unwrap();
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime) \
                 VALUES ('x', '/x', '/', 0, 0)",
                [],
            )
            .unwrap();
        }
        let before = file_bytes(&p);
        for write in [false, true] {
            let err = open_existing_keyed(p.to_str().unwrap(), write, Some(&test_key(0xb2)))
                .unwrap_err();
            assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        }
        // The owner path must error too — a wrong key is never a "schema
        // mismatch" to answer with a wipe.
        let err = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xb2)))
            .unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        assert_eq!(before, file_bytes(&p), "file must be byte-identical");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_key_on_encrypted_db_errors_without_wiping() {
        let p = tmp_db_path();
        {
            let _ = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1)))
                .unwrap();
        }
        let before = file_bytes(&p);
        let err = open_existing_keyed(p.to_str().unwrap(), false, None).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        assert!(err.contains("password-protected"), "got: {err}");
        let err = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", None).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        assert_eq!(before, file_bytes(&p));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn key_on_plaintext_db_errors_without_wiping() {
        let p = tmp_db_path();
        {
            let _ = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        }
        let before = file_bytes(&p);
        let err =
            open_existing_keyed(p.to_str().unwrap(), false, Some(&test_key(0xa1))).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        // The sniffed plaintext header yields the precise diagnosis.
        assert!(err.contains("not encrypted"), "got: {err}");
        let err = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1)))
            .unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        assert_eq!(before, file_bytes(&p));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn schema_mismatch_under_key_wipes_and_recreates_encrypted() {
        // The one case where the owner *should* still wipe: right key,
        // stale schema. The replacement must come back encrypted.
        let p = tmp_db_path();
        let key = test_key(0xa1);
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key.to_hex()))
                .unwrap();
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
        }
        let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
        drop(conn);
        let head = &file_bytes(&p)[..16];
        assert_ne!(
            head, b"SQLite format 3\0",
            "rebuilt index must still be encrypted"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn garbage_file_with_key_reports_mismatch_not_corruption() {
        // A maliciously or accidentally replaced index file: random bytes,
        // no SQLite header. Must surface as KEY_MISMATCH (it is
        // indistinguishable from a wrong key), never wipe.
        let p = tmp_db_path();
        std::fs::write(&p, [0x5a; 4096]).unwrap();
        let before = file_bytes(&p);
        let err =
            open_existing_keyed(p.to_str().unwrap(), false, Some(&test_key(0xa1))).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
        assert_eq!(before, file_bytes(&p));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn open_existing_rw_allows_delete() {
        let p = tmp_db_path();
        {
            let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
            conn.execute(
                "INSERT INTO files (name, path, parent, size, mtime) \
                 VALUES ('a', '/a', '/', 0, 0)",
                [],
            )
            .unwrap();
        }
        let conn = open_existing(p.to_str().unwrap(), true).unwrap();
        let removed = conn
            .execute("DELETE FROM files WHERE path = '/a'", [])
            .unwrap();
        assert_eq!(removed, 1);
        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
