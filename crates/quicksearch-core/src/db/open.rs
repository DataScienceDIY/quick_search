//! Open-or-recreate: the sole entry point into the on-disk database.
//! Policy: any schema mismatch wipes the database file and recreates it from
//! scratch. There are no in-place migrations.

use std::path::Path;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::schema::{
    fts_create_sql, fts_set_pgsz, pragmas_search, recommended_search_cache_mib, Profile,
    PRAGMAS_FAST, PRAGMAS_INCREMENTAL, PRAGMAS_MAINTENANCE, PRAGMAS_READONLY, PRAGMAS_WALK_READER,
    SCHEMA_CURRENT,
};
use crate::config::ProcessingConfig;
use crate::security::IndexKey;

/// Prefix tagging every "the key does not fit this file" error, so a wrong
/// password reads apart from corruption or schema drift. Nothing treats it as
/// a reason to wipe or "recover" the database.
pub const KEY_MISMATCH_PREFIX: &str = "KEY_MISMATCH: ";

/// Bump on any schema change, and on classifier changes: `files.mime`,
/// `files.type` and `content_state` are set at walk time and never recomputed
/// for unchanged files, so only a wipe applies a new classifier everywhere.
///
/// v11 changed the cipher profile ([`schema::HMAC_MODE`] became `Off`), not a
/// table. Keyed indexes are rebuilt anyway via the profile retry in
/// [`open_probed`]; the bump rebuilds unprotected ones too.
///
/// v12 dropped the `mtime` and `mime` indexes (see [`schema::SCHEMA_CURRENT`]).
///
/// [`schema::HMAC_MODE`]: super::schema::HMAC_MODE
/// [`schema::SCHEMA_CURRENT`]: super::schema::SCHEMA_CURRENT
pub const CURRENT_SCHEMA_VERSION: u32 = 12;

/// Open `db_path`; on a schema mismatch, or a full-text layout other than
/// `processing` asks for (tokenizer, positions), delete the file and
/// recreate it empty, for the caller to re-index.
pub fn open_or_recreate(
    db_path: &str,
    processing: &ProcessingConfig,
) -> Result<Connection, String> {
    open_or_recreate_keyed(db_path, processing, super::key::process_key().as_ref())
}

pub(crate) fn open_or_recreate_keyed(
    db_path: &str,
    processing: &ProcessingConfig,
    key: Option<&IndexKey>,
) -> Result<Connection, String> {
    let path = Path::new(db_path).to_path_buf();
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            crate::platform::create_dir_private(dir)
                .map_err(|e| format!("Failed to create database dir {}: {}", dir.display(), e))?;
        }
    }
    let want = super::key::current_profile();
    let (conn, matched) = open_probed(db_path, key, want, |p| {
        let conn =
            Connection::open(p).map_err(|e| format!("Failed to open database at {}: {}", p, e))?;
        // Before a single row is written: SQLite creates the file 0644
        // (inherited by `-wal`/`-shm`), and the index holds the full text of
        // files whose own permissions are 0600.
        crate::platform::restrict_to_owner(Path::new(p));
        Ok(conn)
    })?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    let why = match matched {
        // Neither the page size nor the page reserve changes in place, so a
        // file that opened only under an older profile is rebuilt whatever
        // its schema says.
        ProfileMatch::Previous(found) => {
            format!("was built with {} and this build uses {}", found, want)
        }
        ProfileMatch::Current if db_matches_current(&conn, processing)? => return Ok(conn),
        ProfileMatch::Current => "does not match current schema".to_string(),
    };
    crate::log_warn!(
        "database at {} {}; rebuilding. Existing rows will be re-scanned on next indexing run.",
        db_path,
        why
    );
    let conn = wipe_and_reopen(conn, &path, key, want)?;
    apply_current_schema(&conn, processing, key, want)?;
    Ok(conn)
}

/// Which layout the file on disk answered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileMatch {
    Current,
    /// Opened only under this [`super::schema::PROFILES_PREVIOUS`] entry.
    Previous(Profile),
}

/// Open through `make`, apply the key and `profile`, and probe. On a keyed
/// file that will not decrypt, retry under each
/// [`PROFILES_PREVIOUS`](super::schema::PROFILES_PREVIOUS) entry before giving
/// up: a profile change otherwise looks exactly like a wrong password, which
/// the user cannot act on.
fn open_probed(
    db_path: &str,
    key: Option<&IndexKey>,
    profile: Profile,
    make: impl Fn(&str) -> Result<Connection, String>,
) -> Result<(Connection, ProfileMatch), String> {
    let conn = make(db_path)?;
    let Err(failure) = key_and_probe(&conn, db_path, key, profile) else {
        return Ok((conn, ProfileMatch::Current));
    };
    // Only a profile mismatch is worth retrying, and only when a key hides
    // the layout: an unencrypted file reports its own page size and has no
    // reserve, so a failure there is real.
    if key.is_none() || !failure.starts_with(KEY_MISMATCH_PREFIX) {
        return Err(failure);
    }
    for &previous in super::schema::PROFILES_PREVIOUS
        .iter()
        .filter(|p| **p != profile)
    {
        // A fresh connection: after a failed decrypt the pager has formed an
        // opinion about the file, and `cipher_page_size` and the HMAC pragmas
        // are honoured only before the first read.
        let retry = make(db_path)?;
        if key_and_probe(&retry, db_path, key, previous).is_ok() {
            return Ok((retry, ProfileMatch::Previous(previous)));
        }
    }
    Err(failure)
}

/// A connection opened with `flags` plus `SQLITE_OPEN_NO_MUTEX`.
fn open_flags(p: &str, flags: OpenFlags) -> Result<Connection, String> {
    Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_NO_MUTEX | flags)
        .map_err(|e| format!("Failed to open database at {}: {}", p, e))
}

/// Open `db_path` read-only, never creating it, and probe it under the
/// current profile and then the previous ones.
fn probe_read_only(
    db_path: &str,
    key: Option<&IndexKey>,
) -> Result<(Connection, ProfileMatch), String> {
    open_probed(db_path, key, super::key::current_profile(), |p| {
        open_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)
    })
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
///
/// Its cache ceiling scales with the index, because it has to hold the `files`
/// table every keystroke rescans. The connection opens on the read-only
/// profile, the ceiling is derived from it, and then the search profile is
/// applied; `PRAGMA cache_size` can change at any time, so the smaller ceiling
/// lasts one `sqlite_stat1` read.
pub fn open_search_reader(db_path: &str) -> Result<Connection, String> {
    let conn = open_profiled(db_path, false, PRAGMAS_READONLY)?;
    let cache_mib = super::key::search_cache_override().unwrap_or_else(|| {
        // No stats yet means a fresh or never-optimised index; the floor is
        // right for one, and `repo::maintain` will have run by the time an
        // index is large enough for it to be wrong.
        let files = super::repo::analyzed_file_count(&conn).unwrap_or(0);
        recommended_search_cache_mib(files, super::key::process_key().is_some())
    });
    conn.execute_batch(&pragmas_search(cache_mib))
        .map_err(|e| format!("Failed to apply search pragmas: {}", e))?;
    Ok(conn)
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
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let (conn, matched) = open_probed(db_path, key, super::key::current_profile(), |p| {
        open_flags(p, flags)
    })?;
    conn.execute_batch(pragmas)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;

    // Turn away a file that opened only under a previous profile:
    // `open_or_recreate` is about to wipe it. The version check alone is not
    // enough, because a profile change need not come with a schema bump. The
    // error is the re-index one, not [`KEY_MISMATCH_PREFIX`]: the password was
    // right.
    if !schema_version_current(&conn)? || matched != ProfileMatch::Current {
        return Err(format!(
            "index at {} is not a compatible QuickSearch index (schema v{} expected); \
             refusing to modify it. Re-index to rebuild.",
            db_path, CURRENT_SCHEMA_VERSION
        ));
    }
    Ok(conn)
}

/// Cheaply check that the process key (or its absence) opens the index. It
/// answers the key question only, so a schema bump never reads as an unlock
/// failure. Read-only, no CREATE: verifying a key must never create or modify
/// a database. The profile is ignored: a key that opens the file under an
/// older profile is the right key, and the user must not be told otherwise.
pub fn verify_process_key(db_path: &str) -> Result<(), String> {
    probe_read_only(db_path, super::key::process_key().as_ref()).map(|_| ())
}

/// Whether the next indexing run will discard and rebuild an existing index.
/// `false` for anything this cannot positively establish: announcing a reset
/// that is not happening would be worse than saying nothing.
pub fn index_needs_rebuild(db_path: &str) -> bool {
    let Ok((conn, matched)) = probe_read_only(db_path, super::key::process_key().as_ref()) else {
        return false;
    };
    // A file under an older profile is certain to be rebuilt: neither the page
    // size nor the page reserve can be changed in place.
    if matched != ProfileMatch::Current {
        return true;
    }
    // Only `Ok(false)`: an `Err` means we could not tell.
    matches!(schema_version_current(&conn), Ok(false))
}

/// Serialises SQLCipher's process-wide `cipher_default_*` settings, which
/// [`key_and_probe`] writes to select an
/// [`HmacMode`](super::schema::HmacMode), against the `PRAGMA key` that
/// consumes them, so two threads opening under different profiles cannot
/// interleave. Held for one pragma batch, never across a query.
static CIPHER_DEFAULTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply the SQLCipher key (if any) and the profile, then force the first
/// page off disk.
///
/// The order matters. The HMAC mode is chosen before `PRAGMA key`; `PRAGMA
/// key` precedes anything else that touches the file; `cipher_page_size`
/// follows the key and precedes the first read; and the probe runs before any
/// schema comparison, so a wrong key surfaces as [`KEY_MISMATCH_PREFIX`] and
/// never as a schema mismatch that [`open_or_recreate`] would answer by
/// wiping. The raw-key `x'…'` form skips SQLCipher's per-connection PBKDF2.
///
/// The HMAC mode goes through SQLCipher's process defaults
/// (`cipher_default_use_hmac`, `cipher_default_hmac_algorithm`), which
/// `sqlcipher_codec_ctx_init` reads before the btree is sized. A
/// per-connection `cipher_use_hmac = OFF` after the key has no effect:
/// `sqlite3BtreeSetPageSize` only ever raises a page reserve, and `PRAGMA key`
/// has already installed the default 80 bytes. No error is raised; FTS5 leaves
/// then overflow a limit 64 bytes smaller than the one they were built for.
///
/// The profile applies on every open, not only creating ones: a keyed file's
/// header is ciphertext, so SQLCipher needs the page size and the HMAC mode
/// (which sets the reserve) before it can read the file at all. Unencrypted,
/// `PRAGMA page_size` sizes a file about to be created and is ignored for one
/// that exists.
fn key_and_probe(
    conn: &Connection,
    db_path: &str,
    key: Option<&IndexKey>,
    profile: Profile,
) -> Result<(), String> {
    if let Some(key) = key {
        // `cipher_log_level = NONE` mutes SQLCipher's stderr HMAC trace on
        // wrong-password attempts; it must follow `PRAGMA key`, which has to
        // be the first statement to touch the file.
        let guard = crate::lock_ok(&CIPHER_DEFAULTS);
        let applied = conn.execute_batch(&format!(
            "{} PRAGMA key = \"x'{}'\"; PRAGMA cipher_log_level = NONE; \
             PRAGMA cipher_page_size = {};",
            profile.hmac.default_pragmas(),
            key.to_hex(),
            profile.page_size
        ));
        drop(guard);
        applied.map_err(|e| format!("Failed to apply encryption key: {}", e))?;
    } else {
        conn.execute_batch(&format!("PRAGMA page_size = {};", profile.page_size))
            .map_err(|e| format!("Failed to apply page size: {}", e))?;
    }
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(e) if is_notadb(&e) => Err(key_mismatch_message(db_path, key.is_some())),
        Err(e) => Err(format!("Failed to read database at {}: {}", db_path, e)),
    }
}

/// An undecryptable first page reads as SQLITE_NOTADB: the decrypted header
/// bytes are noise.
fn is_notadb(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(rusqlite::ErrorCode::NotADatabase)
}

/// Why a keyed open failed, for callers to branch on: each case asks
/// something different of the user, and matching on message prose breaks when
/// a database path contains that prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMismatch {
    /// A key was applied and the file did not accept it.
    WrongPassword,
    /// A key was applied but the file is not encrypted at all: the rebuild
    /// that would encrypt it did not finish.
    NotEncrypted,
    /// No key was applied and the file wants one.
    PasswordRequired,
}

impl KeyMismatch {
    const ALL: [KeyMismatch; 3] = [
        KeyMismatch::WrongPassword,
        KeyMismatch::NotEncrypted,
        KeyMismatch::PasswordRequired,
    ];

    fn token(self) -> &'static str {
        match self {
            KeyMismatch::WrongPassword => "wrong-password",
            KeyMismatch::NotEncrypted => "not-encrypted",
            KeyMismatch::PasswordRequired => "password-required",
        }
    }

    fn from_token(token: &str) -> Option<KeyMismatch> {
        Self::ALL.into_iter().find(|m| m.token() == token)
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
    // An unencrypted SQLite file keeps its plaintext magic, which tells
    // "wrong password" from "the index was never encrypted".
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
/// caller can tell it from schema drift, which rebuilds.
pub const FOREIGN_DB_PREFIX: &str = "FOREIGN_DB: ";

/// Tables of the pre-`schema_info` layout. `files` is a loose end: another
/// application's table called `files` reads as an ancient index of ours and is
/// wiped, but refusing a genuine legacy index is the worse failure.
const LEGACY_TABLES: &[&str] = &["files", "files_fts", "documents_text", "failed_files"];

/// Whether `schema_info` exists and is shaped like ours. The shape, not the
/// contents: an index whose creation stopped before the version row landed is
/// still ours; a foreign table reusing the name is not.
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

/// `schema_info.{key}`, or `None` when the row is absent.
fn stored_info(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row("SELECT value FROM schema_info WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .optional()
    .map_err(|e| format!("read schema_info.{}: {}", key, e))
}

fn schema_version_current(conn: &Connection) -> Result<bool, String> {
    if !has_our_schema_info(conn) {
        return Ok(false);
    }
    let version = stored_info(conn, "version")?;
    Ok(version.as_deref() == Some(&CURRENT_SCHEMA_VERSION.to_string()))
}

/// True iff the DB has the current schema version *and* the full-text layout
/// this caller asked for.
fn db_matches_current(conn: &Connection, processing: &ProcessingConfig) -> Result<bool, String> {
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
    Ok(
        stored_info(conn, "tokenize")?.as_deref() == Some(processing.tokenize.trim())
            && stored_info(conn, "fts_positions")?.as_deref()
                == Some(&*processing.fts_positions().to_string()),
    )
}

/// Delete the index at `path` and its SQLite sidecars (`-wal`, `-shm`,
/// `-journal`). A missing index is not an error, and sidecar failures are
/// ignored. Deletes retry because on Windows they fail while any handle is
/// open, most often an antivirus scanner's or an idle search connection's.
pub fn remove_index_files(path: &Path) -> std::io::Result<()> {
    match crate::platform::remove_file_retrying(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ =
            crate::platform::remove_file_retrying(&path.with_file_name(format!("{name}{suffix}")));
    }
    Ok(())
}

/// Delete the DB file and its sidecars, reopen a fresh file, and re-apply
/// key, profile and pragmas. Re-keying keeps a rebuilt protected index
/// encrypted. `profile` is the current one even when the replaced file
/// answered to an older one: the rebuild adopts the new layout.
fn wipe_and_reopen(
    conn: Connection,
    path: &Path,
    key: Option<&IndexKey>,
    profile: Profile,
) -> Result<Connection, String> {
    drop(conn);
    // Before the delete, even if the removal fails partway.
    super::bump_index_epoch();
    remove_index_files(path).map_err(|e| {
        format!(
            "Failed to remove old database at {}: {}. \
             Another QuickSearch instance may have the index open.",
            path.display(),
            e
        )
    })?;
    let conn = Connection::open(path)
        .map_err(|e| format!("Failed to reopen database after rebuild: {}", e))?;
    crate::platform::restrict_to_owner(path);
    key_and_probe(&conn, &path.to_string_lossy(), key, profile)?;
    conn.execute_batch(PRAGMAS_FAST)
        .map_err(|e| format!("Failed to apply pragmas after rebuild: {}", e))?;
    Ok(conn)
}

fn apply_current_schema(
    conn: &Connection,
    processing: &ProcessingConfig,
    key: Option<&IndexKey>,
    profile: Profile,
) -> Result<(), String> {
    conn.execute_batch(SCHEMA_CURRENT)
        .map_err(|e| format!("Failed to create current schema tables: {}", e))?;
    let positions = processing.fts_positions();
    let fts = fts_create_sql(&processing.tokenize, positions);
    conn.execute_batch(&fts)
        .map_err(|e| format!("Failed to create searchabletext: {}", e))?;
    // Only here: FTS5's leaf size must suit this file's page size and
    // reserve. Deciding it once is sound because the profile is fixed for the
    // file's life; toggling password protection always wipes and rebuilds.
    fts_set_pgsz(conn, profile, key.is_some())
        .map_err(|e| format!("Failed to set searchabletext pgsz: {}", e))?;

    let now = crate::log::now_unix();
    // `fts_positions` is read back by every search (`repo::fts_positions`),
    // which must phrase its queries for the table as it was built.
    conn.execute(
        "INSERT INTO schema_info(key, value) VALUES ('version', ?1), ('created_at', ?2), \
         ('tokenize', ?3), ('fts_positions', ?4)",
        params![
            CURRENT_SCHEMA_VERSION.to_string(),
            now.to_string(),
            processing.tokenize.trim(),
            positions.to_string()
        ],
    )
    .map_err(|e| format!("Failed to seed schema_info: {}", e))?;

    Ok(())
}

#[cfg(test)]
#[path = "open_tests.rs"]
mod tests;
