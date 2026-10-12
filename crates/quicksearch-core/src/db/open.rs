//! Open-or-recreate: the sole entry point into the on-disk database.
//! Policy: any schema mismatch wipes the database file and recreates it from
//! scratch. There are no in-place migrations.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::repo::get_info;
use super::schema::{
    fts_create_sql, fts_set_pgsz, pragmas_search, recommended_search_cache_mib, Profile,
    PRAGMAS_FAST, PRAGMAS_INCREMENTAL, PRAGMAS_READONLY, PRAGMAS_WALK_READER, SCHEMA_CURRENT,
};
use crate::config::ProcessingConfig;
use crate::security::IndexKey;

/// Prefix tagging every "the key does not fit this file" error, so a wrong
/// password reads apart from corruption or schema drift. Nothing treats it as
/// a reason to wipe or "recover" the database.
pub const KEY_MISMATCH_PREFIX: &str = "KEY_MISMATCH: ";

/// Bump on any schema change, and in any release that changes what a row
/// holds for a file that has not changed: the digest ([`get_file_hash`]),
/// MIME detection (`files.mime`, `files.type`), or an extractor added or
/// fixed (`content_state` and the stored text). All are decided when a file
/// is first seen and never again while its stamp holds, and
/// `config_validation` records only settings, so only a wipe applies new
/// code everywhere.
///
/// v11 changed the cipher profile ([`schema::HMAC_MODE`] became `Off`), not a
/// table. Keyed indexes are rebuilt anyway via the profile retry in
/// [`open_probed`]; the bump rebuilds unprotected ones too.
///
/// v12 dropped the `mtime` and `mime` indexes (see [`schema::SCHEMA_CURRENT`]).
/// Its rebuild also clears the rows a v1.1 index kept for symlink targets
/// outside every root, which nothing removes now that links are not followed,
/// replaces an index built under a word tokenizer (trigram is now the only
/// one, so `schema_info` no longer records it), and carries the release's
/// extractor and MIME fixes to rows already indexed.
///
/// [`get_file_hash`]: crate::file_handling::get_file_hash
/// [`schema::HMAC_MODE`]: super::schema::HMAC_MODE
/// [`schema::SCHEMA_CURRENT`]: super::schema::SCHEMA_CURRENT
pub const CURRENT_SCHEMA_VERSION: u32 = 12;

/// Open `db_path`; on a schema mismatch, a full-text layout other than
/// `processing` asks for (positions or none), or hashes recorded under
/// another `hash_length`, delete the file and recreate it empty, for the
/// caller to re-index.
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
    // Nothing touches the file until it is known to be ours: a chmod or
    // `journal_mode = WAL` on another application's database is the damage
    // the refusal in `db_matches_current` exists to prevent.
    let (conn, matched) = open_probed(db_path, key, want, |p| {
        Connection::open(p).map_err(|e| format!("Failed to open database at {}: {}", p, e))
    })?;

    let why = match matched {
        // Neither the page size nor the page reserve changes in place, so a
        // file that opened only under an older profile is rebuilt whatever
        // its schema says.
        ProfileMatch::Previous(found) => Some(format!(
            "was built with {} and this build uses {}",
            found, want
        )),
        ProfileMatch::Current if db_matches_current(&conn, processing)? => {
            narrow_index_files(&path);
            conn.execute_batch(PRAGMAS_FAST)
                .map_err(|e| format!("Failed to apply pragmas: {}", e))?;
            return Ok(conn);
        }
        // A path just created, or an empty file: nothing is rebuilt, so
        // creating the schema is not worth a warning.
        ProfileMatch::Current if holds_no_schema(&conn)? => None,
        ProfileMatch::Current => Some("does not match current schema".to_string()),
    };
    if let Some(why) = why {
        crate::log_warn!(
            "database at {} {}; rebuilding. Existing rows will be re-scanned on next indexing run.",
            db_path,
            why
        );
    }
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
/// table every keystroke rescans; the file count is the one the last completed
/// run stored ([`super::repo::stored_file_count`]). The connection opens on
/// the read-only profile, reads that count, and then the search profile is
/// applied; `PRAGMA cache_size` can change at any time, so the smaller ceiling
/// lasts one `schema_info` read.
pub fn open_search_reader(db_path: &str) -> Result<Connection, String> {
    let conn = open_profiled(db_path, false, PRAGMAS_READONLY)?;
    let cache_mib = super::key::search_cache_override().unwrap_or_else(|| {
        // No completed run yet means no counts and the floor, right for a
        // fresh index.
        let files = super::repo::stored_file_count(&conn);
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

    // Turn away a file that opened only under a previous profile:
    // `open_or_recreate` is about to wipe it. The version check alone is not
    // enough, because a profile change need not come with a schema bump. The
    // error is the re-index one, not [`KEY_MISMATCH_PREFIX`]: the password was
    // right. Before the pragmas: `journal_mode = WAL` writes the header.
    if !schema_version_current(&conn)? || matched != ProfileMatch::Current {
        return Err(format!(
            "index at {} is not a compatible QuickSearch index (schema v{} expected); \
             refusing to modify it. Re-index to rebuild.",
            db_path, CURRENT_SCHEMA_VERSION
        ));
    }
    conn.execute_batch(pragmas)
        .map_err(|e| format!("Failed to apply pragmas: {}", e))?;
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
/// `false` for anything this cannot positively establish, or a file the
/// indexer would refuse as not ours: announcing a reset that is not happening
/// would be worse than saying nothing.
pub fn index_needs_rebuild(db_path: &str) -> bool {
    let Ok((conn, matched)) = probe_read_only(db_path, super::key::process_key().as_ref()) else {
        return false;
    };
    // A file under an older profile is certain to be rebuilt: neither the page
    // size nor the page reserve can be changed in place.
    if matched != ProfileMatch::Current {
        return true;
    }
    // Only `Ok` answers count: an `Err` means we could not tell. An empty
    // file is only given the schema, which is no reset.
    matches!(schema_version_current(&conn), Ok(false))
        && matches!(is_ours_or_empty(&conn), Ok(true))
        && matches!(holds_no_schema(&conn), Ok(false))
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
        Err(e) if is_notadb(&e) => Err(notadb_message(db_path, key.is_some())),
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
    /// No key was applied and the file wants one. Only for a file with no
    /// plaintext header: one that has it is damaged, not encrypted.
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

/// Why SQLite answered `NOTADB`. An unencrypted SQLite file keeps its
/// plaintext magic, which tells "wrong password" from "the index was never
/// encrypted" and, with no key applied, a damaged header from an encrypted
/// one. The damaged case is untagged: no password helps, clearing does. So is
/// a file with neither the magic nor whole pages: it is no index at all, and
/// [`check_replaceable`] refuses to clear it.
fn notadb_message(db_path: &str, had_key: bool) -> String {
    let path = Path::new(db_path);
    let plaintext = has_sqlite_magic(path).unwrap_or(false);
    let (cause, detail) = match (had_key, plaintext) {
        (true, true) => (
            KeyMismatch::NotEncrypted,
            "password protection is enabled but the index is not encrypted; \
             rebuild the index to encrypt it",
        ),
        // Neither a header nor whole pages: no password opens it.
        (_, false) if std::fs::metadata(path).is_ok_and(|meta| meta.len() % MIN_PAGE_SIZE != 0) => {
            return format!(
                "{} is not a QuickSearch index (no SQLite header, and not whole pages as an \
                 encrypted index is); point [paths] database_path elsewhere or move it away",
                db_path
            )
        }
        (true, false) => (
            KeyMismatch::WrongPassword,
            "wrong password (or the file is not a QuickSearch index)",
        ),
        (false, true) => {
            return format!(
                "index at {} is damaged (SQLite rejects its header); clear the index to \
                 rebuild it",
                db_path
            )
        }
        (false, false) => (
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

/// Whether the file holds no schema at all: a path just created, or an empty
/// file. `sqlite_%` names are excluded, as in [`is_ours_or_empty`].
fn holds_no_schema(conn: &Connection) -> Result<bool, String> {
    conn.query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("read sqlite_master: {}", e))
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
    let version = get_info(conn, "version")?;
    Ok(version.as_deref() == Some(&CURRENT_SCHEMA_VERSION.to_string()))
}

/// True iff the DB has the current schema version, hashes recorded under
/// this caller's `hash_length` (or none recorded), *and* the full-text layout
/// it asked for.
fn db_matches_current(conn: &Connection, processing: &ProcessingConfig) -> Result<bool, String> {
    if !schema_version_current(conn)? {
        // Refuse rather than wipe unless the file is recognisably ours:
        // `database_path` is free text with no confirmation, and a typo
        // naming some other application's SQLite file would otherwise delete
        // it on the next indexing run. [`check_replaceable`] asks again at
        // the delete, for every route there.
        if !is_ours_or_empty(conn)? {
            return Err(foreign_database_message(conn)?);
        }
        return Ok(false);
    }
    // No scan re-hashes the rows a walk skips as unchanged, so a declined
    // `hash_length` change rebuilds here, at the next run. Each run records
    // it in `config_validation`, not `schema_info`. None recorded (a fresh
    // or never-run index) matches; any other value, numeric or not, does not.
    let hash_length: Option<String> = conn
        .query_row(
            "SELECT value FROM config_validation WHERE key = 'hash_length'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("read config_validation.hash_length: {}", e))?;
    if hash_length.is_some_and(|v| v != processing.hash_length.to_string()) {
        return Ok(false);
    }
    let positions = (!processing.compact_text_index).to_string();
    Ok(get_info(conn, "fts_positions")?.as_deref() == Some(&*positions))
}

/// SQLite's files beside the index at `path`: the `-` entries of
/// [`INDEX_SIDECAR_SUFFIXES`](crate::file_handling::INDEX_SIDECAR_SUFFIXES).
/// The instance's `.lock` is not one; it outlives every index at the path.
fn sqlite_sidecars(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    crate::file_handling::INDEX_SIDECAR_SUFFIXES
        .iter()
        .filter(|suffix| suffix.starts_with('-'))
        .map(move |suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        })
}

/// Narrow the index at `path`, and each sidecar present beside it, to
/// owner-only access, before a single row is written: SQLite creates the file
/// 0644 (inherited by `-wal`/`-shm`), and the index holds the full text of
/// files whose own permissions are 0600. The sidecars are narrowed
/// explicitly: an index already loose has loose ones, which narrowing the
/// main file no longer reaches.
fn narrow_index_files(path: &Path) {
    crate::platform::restrict_to_owner(path);
    for sidecar in sqlite_sidecars(path) {
        crate::platform::restrict_to_owner(&sidecar);
    }
}

/// The smallest page any index of ours has been built at. An encrypted index
/// has no header to recognise it by, but it is a whole number of pages.
const MIN_PAGE_SIZE: u64 = {
    let profiles = super::schema::PROFILES_PREVIOUS;
    let mut min = super::schema::PROFILE.page_size;
    let mut i = 0;
    while i < profiles.len() {
        if profiles[i].page_size < min {
            min = profiles[i].page_size;
        }
        i += 1;
    }
    min as u64
};

/// How long [`check_replaceable`] waits out another connection's lock before
/// calling the file in use.
const PROBE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether the file at `path` may be deleted as an index: missing, empty,
/// recognisably ours or too damaged to tell, or (with no SQLite header)
/// indistinguishable from an encrypted index. `Err` says why not. A file
/// another program holds open, or one this process cannot read, is never
/// deleted.
///
/// Every delete passes here, because `database_path` is free text: Clear, a
/// rebuild, a settings change, the security change and the unlock reset
/// would otherwise delete whatever file it names. Without the key, a
/// non-SQLite file that happens to be whole pages still reads as an encrypted
/// index and is deleted.
fn check_replaceable(path: &Path) -> Result<(), String> {
    let meta = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "Cannot inspect {}: {}; refusing to delete it",
                path.display(),
                e
            ))
        }
        Ok(meta) => meta,
    };
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file; refusing to delete it",
            path.display()
        ));
    }
    if meta.len() == 0 {
        return Ok(());
    }
    let plaintext = match has_sqlite_magic(path) {
        Ok(plaintext) => plaintext,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
        // Unreadable (another user's file, say) is not "no header".
        Err(e) => {
            return Err(format!(
                "Cannot read {}: {}; refusing to delete it",
                path.display(),
                e
            ))
        }
    };
    if !plaintext {
        return if meta.len() % MIN_PAGE_SIZE == 0 {
            Ok(())
        } else {
            Err(format!(
                "{} is not a QuickSearch index; refusing to delete it",
                path.display()
            ))
        };
    }
    // Unkeyed: a file with the plaintext header is not encrypted.
    let probe = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|conn| {
        conn.busy_timeout(PROBE_BUSY_TIMEOUT)?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })?;
        Ok(conn)
    });
    let conn = match probe {
        Ok(conn) => conn,
        Err(e)
            if matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            ) =>
        {
            return Err(format!(
                "{} is in use by another program; refusing to delete it",
                path.display()
            ))
        }
        // A hot rollback journal: some program died mid-transaction, and a
        // read-only connection may not roll it back. An index is always in
        // WAL mode, so this file is not one.
        Err(e)
            if e.sqlite_error().map(|e| e.extended_code)
                == Some(rusqlite::ffi::SQLITE_READONLY_ROLLBACK) =>
        {
            return Err(format!(
                "{} holds another program's unfinished transaction; refusing to delete it",
                path.display()
            ))
        }
        // Too damaged to read (NOTADB, CORRUPT, a short file): if it was ever
        // an index, clearing is how it gets rebuilt.
        Err(_) => return Ok(()),
    };
    // `conn` closes before the caller deletes: Windows refuses while it is open.
    match is_ours_or_empty(&conn) {
        Ok(false) => Err(foreign_database_message(&conn)?),
        Ok(true) | Err(_) => Ok(()),
    }
}

/// Whether `path` starts with SQLite's plaintext magic, which SQLCipher
/// encrypts with the rest of the file. Through
/// [`crate::platform::open_regular_file`], so a FIFO swapped in since the
/// caller's `stat` cannot block the read. A file too short to hold the magic
/// fails with `UnexpectedEof`; any other error means it could not be read,
/// which says nothing about what it holds.
fn has_sqlite_magic(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut magic = [0u8; 16];
    let (mut file, _) = crate::platform::open_regular_file(path)?;
    file.read_exact(&mut magic)?;
    Ok(&magic == b"SQLite format 3\0")
}

/// Delete the index at `path` and SQLite's sidecars beside it (`-wal`, `-shm`,
/// `-journal`); the instance's `.lock` is kept. Refused, with nothing
/// touched, unless [`check_replaceable`] allows it: the file must be empty,
/// recognisably an index, or indistinguishable from an encrypted one, and not
/// held by another program. A missing index is not an error, and sidecar
/// failures are ignored.
///
/// Every [`super::hold_index`] callback runs first, so this process's own
/// readers let go. Deletes retry because on Windows they fail while any handle
/// is open, most often an antivirus scanner's. The index epoch is bumped once
/// on the way out, refused and failed calls included: a reader that reopened
/// while this ran must not keep serving the deleted inode.
pub fn remove_index_files(path: &Path) -> Result<(), String> {
    remove_index_files_noting(path, "", std::time::Instant::now())
}

/// [`remove_index_files`], with a failed delete tried again until `deadline`.
/// On Windows a handle this process cannot release, a running duplicates
/// scan's or an antivirus scanner's, fails the delete until it closes. Each
/// attempt runs the [`super::hold_index`] callbacks again, since a search may
/// have reopened in between. A refusal is final, and so is a failure on Unix,
/// where no open handle stops an unlink.
pub(crate) fn remove_index_files_until(
    path: &Path,
    deadline: std::time::Instant,
) -> Result<(), String> {
    remove_index_files_noting(path, "", deadline)
}

/// The pause between two attempts of [`remove_index_files_until`].
const DELETE_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// [`remove_index_files_until`], with `hint` appended when the delete itself
/// fails.
fn remove_index_files_noting(
    path: &Path,
    hint: &str,
    deadline: std::time::Instant,
) -> Result<(), String> {
    let removed = loop {
        super::release_holders(path);
        if let Err(refused) = check_replaceable(path) {
            break Err(refused);
        }
        match crate::platform::remove_file_retrying(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                if cfg!(windows) && std::time::Instant::now() < deadline {
                    std::thread::sleep(DELETE_RETRY_PAUSE);
                    continue;
                }
                break Err(format!(
                    "Failed to delete {}: {}{}",
                    path.display(),
                    e,
                    hint
                ));
            }
            _ => {}
        }
        for sidecar in sqlite_sidecars(path) {
            let _ = crate::platform::remove_file_retrying(&sidecar);
        }
        break Ok(());
    };
    super::bump_index_epoch();
    removed
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
    remove_index_files_noting(
        path,
        ". Another QuickSearch instance may have the index open.",
        std::time::Instant::now(),
    )?;
    let conn = Connection::open(path)
        .map_err(|e| format!("Failed to reopen database after rebuild: {}", e))?;
    narrow_index_files(path);
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
    let positions = !processing.compact_text_index;
    conn.execute_batch(&fts_create_sql(positions))
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
         ('fts_positions', ?3)",
        params![
            CURRENT_SCHEMA_VERSION.to_string(),
            now.to_string(),
            positions.to_string()
        ],
    )
    .map_err(|e| format!("Failed to seed schema_info: {}", e))?;

    Ok(())
}

#[cfg(test)]
#[path = "open_tests.rs"]
mod tests;
