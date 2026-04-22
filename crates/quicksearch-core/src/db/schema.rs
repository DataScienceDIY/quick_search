//! SQL strings for the current schema. Versioned; [`migrate`](super::migrate)
//! drives the upgrade path.

/// Pragmas applied on every connection open. Tuned for write throughput during
/// indexing; a clean shutdown re-enables journal_mode/synchronous via
/// [`super::repo::checkpoint_and_close`].
pub const PRAGMAS_FAST: &str = "
    PRAGMA journal_mode = OFF;
    PRAGMA synchronous = 0;
    PRAGMA cache_size = 10000;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The full current schema. Applied by [`migrate::open_and_migrate`] when
/// the DB is fresh or has been wiped during upgrade.
///
/// FTS5 is a *regular* (non-contentless) virtual table so `snippet()` and
/// `highlight()` can read the stored text. This also means there is no
/// separate `documents` table — FTS5 *is* the text store.
pub const SCHEMA_CURRENT: &str = r#"
CREATE TABLE schema_info (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    path          TEXT    NOT NULL UNIQUE,
    parent        TEXT    NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    inode         INTEGER,
    device_id     INTEGER,
    mime          TEXT,
    type          INTEGER NOT NULL DEFAULT 0,
    basic_state   INTEGER NOT NULL DEFAULT 0,   -- 0=pending 1=done 2=failed
    content_state INTEGER NOT NULL DEFAULT 0,   -- 0=pending 1=done 2=failed 3=n/a
    failure_msg   TEXT,
    hash          BLOB
);

CREATE INDEX idx_files_parent ON files(parent);
CREATE INDEX idx_files_mtime  ON files(mtime);
CREATE INDEX idx_files_type   ON files(type);
CREATE INDEX idx_files_mime   ON files(mime);
CREATE INDEX idx_files_hash   ON files(hash);
CREATE INDEX idx_files_content_pending ON files(id) WHERE content_state = 0;

CREATE TABLE properties (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    key     TEXT    NOT NULL,
    value   TEXT    NOT NULL,
    PRIMARY KEY (file_id, key)
);

CREATE TABLE failed_files (
    file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    reason  TEXT,
    ts      INTEGER NOT NULL
);

CREATE TABLE config_validation (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// FTS5 virtual table DDL. Separate because the tokenizer is config-driven.
///
/// Regular (not contentless, not external-content) FTS5: the table stores
/// its own text, which enables `snippet()`/`highlight()` and makes row-level
/// INSERT/UPDATE/DELETE work with normal SQL semantics. `rowid` is supplied
/// by the caller and must equal `files.id`.
pub fn fts_create_sql(tokenizer: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE searchabletext USING fts5(\
            name, text, properties, \
            tokenize='{}'\
        );",
        tokenizer.replace('\'', "''")
    )
}
