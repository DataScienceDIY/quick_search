//! SQL strings for the current schema. Versioned; [`migrate`](super::migrate)
//! drives the upgrade path.

/// Pragmas applied on every writable connection open.
///
/// WAL, not journal-off: auto-indexing writes continuously while searches
/// stream from their own read-only connections, and WAL is what lets those
/// readers proceed without ever blocking the writer (or vice versa).
/// `synchronous = NORMAL` under WAL risks only the last commit on power
/// loss — acceptable for an index that is re-derivable from disk. Only two
/// writers exist (full index runs and the coordinator's incremental
/// updates) and they're serialized by design; `busy_timeout` is a backstop,
/// not a coordination mechanism. A clean shutdown truncates the log via
/// [`super::repo::checkpoint_and_close`].
pub const PRAGMAS_FAST: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = 10000;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas safe to apply on a read-only connection, where `journal_mode`
/// and `synchronous` can't be changed on the file. Used by
/// [`super::open::open_existing`] for read-only opens; write paths get the
/// full [`PRAGMAS_FAST`] set.
pub const PRAGMAS_READONLY: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = 10000;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The full current schema. Applied by [`super::open::open_or_recreate`]
/// when the DB is fresh or has just been wiped because it drifted from
/// [`super::open::CURRENT_SCHEMA_VERSION`].
///
/// FTS5 is *contentless* (see [`fts_create_sql`]): the inverted index is kept
/// but the column values aren't stored. The canonical extracted text lives
/// in a separate `documents_text` table, zstd-compressed. Snippet rendering
/// for search results decompresses on demand and highlights matches in Rust
/// (see `crate::snippet`). This keeps the on-disk footprint close to Baloo's
/// LMDB-only size while still supporting snippet/highlight features.
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

-- Canonical extracted text for every successfully content-indexed file.
-- Compressed with zstd (see `crate::db::repo::set_content_done`). Only
-- written when the extractor produced text; absent rows mean "no body
-- text" (e.g. an image with only EXIF properties).
CREATE TABLE documents_text (
    file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    text_zstd  BLOB    NOT NULL,
    text_len   INTEGER NOT NULL   -- original byte length pre-compression
);

CREATE TABLE config_validation (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// FTS5 virtual table DDL. Separate because the tokenizer is config-driven.
///
/// *Contentless* FTS5 (`content=''`): the inverted index is built from the
/// column values supplied on INSERT, but those values are not stored. This
/// is the main lever that pulls our on-disk footprint down toward Baloo's.
/// `contentless_delete=1` (SQLite 3.43+) lets us `DELETE FROM … WHERE
/// rowid=?` without replaying the original row text, at the cost of a
/// modest tombstone bitmap. The tokenizer is config-driven; by default
/// (`trigram`) we append `remove_diacritics 1` so queries and stored text
/// fold the same way.
///
/// Snippet/highlight aren't available through SQLite's built-in `snippet()`
/// in contentless mode — we render them in Rust from the zstd-compressed
/// `documents_text` sidecar instead.
pub fn fts_create_sql(tokenizer: &str) -> String {
    let effective = effective_tokenizer(tokenizer);
    format!(
        "CREATE VIRTUAL TABLE searchabletext USING fts5(\
            name, text, properties, \
            tokenize='{}', \
            content='', \
            contentless_delete=1\
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
