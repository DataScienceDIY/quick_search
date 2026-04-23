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
    let effective = effective_tokenizer(tokenizer);
    format!(
        "CREATE VIRTUAL TABLE searchabletext USING fts5(\
            name, text, properties, \
            tokenize='{}'\
        );",
        effective.replace('\'', "''")
    )
}

/// Map a user-facing tokenizer name to the actual FTS5 option string we
/// apply. The default `trigram` gets `remove_diacritics 1` appended so an
/// ASCII query like `cafe` matches indexed `café`, and vice versa.
/// Without this, the default trigram tokenizer would emit disjoint
/// trigram sets for the two spellings and `MATCH` would miss one of them.
/// Users who want precise match semantics can pass the full option string
/// (e.g. `"trigram case_sensitive 1 remove_diacritics 0"`) and we'll use
/// it verbatim.
pub fn effective_tokenizer(tokenizer: &str) -> String {
    let trimmed = tokenizer.trim();
    if trimmed.eq_ignore_ascii_case("trigram") {
        // Explicit default includes accent stripping. `case_sensitive 0`
        // is FTS5's default too; we repeat it here for clarity.
        "trigram remove_diacritics 1".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_trigram_gets_accent_stripping() {
        assert_eq!(effective_tokenizer("trigram"), "trigram remove_diacritics 1");
        assert_eq!(effective_tokenizer("  trigram  "), "trigram remove_diacritics 1");
    }

    #[test]
    fn explicit_tokenizers_pass_through() {
        assert_eq!(
            effective_tokenizer("trigram remove_diacritics 0"),
            "trigram remove_diacritics 0"
        );
        assert_eq!(effective_tokenizer("porter"), "porter");
        assert_eq!(effective_tokenizer("unicode61"), "unicode61");
    }
}
