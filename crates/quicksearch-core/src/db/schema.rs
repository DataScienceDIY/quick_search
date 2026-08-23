//! SQL strings for the current schema; there is no upgrade path.
//!
//! # Pragma profiles
//!
//! **A negative `cache_size` is KiB; a positive one would be a page count.**
//! A filled cache is glibc arena memory that outlives the connection — see
//! [`crate::platform::release_free_heap`].
//!
//! | Profile | Connection | Lifetime | Cache |
//! |---|---|---|---|
//! | [`PRAGMAS_FAST`] | bulk indexer writer | one run | 8 MiB |
//! | [`PRAGMAS_INCREMENTAL`] | coordinator's writer | released when idle | 4 MiB |
//! | [`PRAGMAS_SEARCH`] | search worker | held across a typing session | 32 MiB |
//! | [`PRAGMAS_READONLY`] | one-shot readers | a single query | 4 MiB |
//! | [`PRAGMAS_MAINTENANCE`] | VACUUM | one bulk copy | 8 MiB |
//! | [`PRAGMAS_WALK_READER`] | per-root walk prefetch and content feeder | the run | 1 MiB |
//!
//! `PRAGMA mmap_size` is absent from all of them: it would make memory
//! behaviour differ between protected and unprotected installs.

/// The bulk indexer's write connection: one per run, dies with it.
/// `synchronous = NORMAL` under WAL risks only the last commit on power loss
/// — acceptable for an index re-derivable from disk.
pub const PRAGMAS_FAST: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// [`PRAGMAS_FAST`] but with `temp_store = FILE`: SQLCipher is compiled
/// `-DSQLITE_TEMP_STORE=2`, under which anything but an explicit `FILE` puts
/// temporary databases in memory — and VACUUM builds the replacement database
/// there, so the indexer's profile would hold a multi-gigabyte index in RAM.
pub const PRAGMAS_MAINTENANCE: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = FILE;
    PRAGMA foreign_keys = ON;
";

/// The coordinator's long-lived write connection — whatever its cache
/// reaches it holds until the coordinator settles, so this bounds it.
pub const PRAGMAS_INCREMENTAL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The search worker's connection, held across a typing session. The one
/// deliberately large profile: SQLCipher caches pages *decrypted*, so on an
/// encrypted index an undersized cache re-pays AES-CBC + HMAC per 4 KiB and
/// warm queries run ~2.5× slower (`benches/search_perf.rs` sweeps it).
pub const PRAGMAS_SEARCH: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -32768;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The *one-shot* readers. Pragmas safe on a read-only connection, where
/// `journal_mode` and `synchronous` can't be changed on the file.
pub const PRAGMAS_READONLY: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// A root's own reader: the walk's row prefetch, then the content pass's
/// feeder. Two can exist per root, so the cache multiplies by root count;
/// **this is the number to raise** if the prefetcher ever falls behind. The
/// feeder's cold scan is deliberately paid here, not on the writer — the
/// writer holding still for it stopped every other root's walk.
pub const PRAGMAS_WALK_READER: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -1024;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The full current schema, applied to a fresh or just-wiped DB. FTS5 is
/// *contentless* (see [`fts_create_sql`]); canonical extracted text lives in
/// `documents_text`, zstd-compressed.
pub const SCHEMA_CURRENT: &str = r#"
CREATE TABLE schema_info (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- `parent` always ends in the platform separator; see `dir_to_db_parent` for
-- why the design requires that.
CREATE TABLE files (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    parent        TEXT    NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    mime          TEXT,
    type          INTEGER NOT NULL DEFAULT 0,
    content_state INTEGER NOT NULL DEFAULT 0,   -- 0=pending 1=done 2=failed 3=n/a
    hash          BLOB
);

-- The identity of a row, and the only index `parent` needs.
--
-- It replaces both of what came before — a `UNIQUE(path)` and a covering
-- `(parent, name, mtime)` — and dropping the second is the deliberate half.
-- The walk's row prefetcher issues `SELECT name, mtime FROM files WHERE
-- parent = ?` once per directory, the hottest read in a full run, and without
-- `mtime` in the index that is a table-row fetch per entry. It is affordable
-- because the prefetcher is one thread ahead of four walk workers that each
-- spend a `stat` *and* a SHA-256 of the path per file (`crate::walk`), so it
-- has budget to spend; because a directory's rows are written in one batch and
-- so are rowid-adjacent; and because the row is now narrow enough that a
-- 1 MiB cache holds ~7,200 of them. `mtime` cannot simply be appended here —
-- `UNIQUE(parent, name, mtime)` would let the same file be inserted twice
-- under two mtimes.
--
-- If the prefetcher is ever measured falling behind, raise
-- `PRAGMAS_WALK_READER` — paid for out of the space this index no longer
-- occupies — rather than restoring the covering one.
CREATE UNIQUE INDEX idx_files_parent ON files(parent, name);
CREATE INDEX idx_files_mtime  ON files(mtime);
-- No index on `type`. The only query that touches it is the kind filter's
-- `(f.type & ?) != 0` (`crate::query::translator`), and a bitmask test on the
-- left of the operator is not sargable, so SQLite scans regardless — nothing
-- can seek such an index and nothing orders or groups by the column. Carrying
-- one measured ~10% on inserts and 805 pages at 300k rows, and those pages
-- compete with the ones search wants resident.
CREATE INDEX idx_files_mime   ON files(mime);
CREATE INDEX idx_files_hash   ON files(hash);
CREATE INDEX idx_files_content_pending ON files(id) WHERE content_state = 0;

CREATE TABLE failed_files (
    file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    reason  TEXT,
    ts      INTEGER NOT NULL
);

-- Canonical extracted text for every successfully content-indexed file.
-- Compressed with zstd (see `crate::db::repo::set_content_done`). Only
-- written when the extractor produced text; absent rows mean "no body
-- text" (e.g. an audio file whose tags are all empty). "Has a row here" is
-- therefore *not* the same as "content indexed" — `files.content_state` is
-- the authority on that.
--
-- The uncompressed length is not stored: zstd records it in the frame
-- header, so `crate::db::repo::raw_text_len` reads it back for the one
-- caller (the size report) that wants it.
CREATE TABLE documents_text (
    file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    text_zstd  BLOB    NOT NULL
);

CREATE TABLE config_validation (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// FTS5 virtual table DDL. Separate because the tokenizer is config-driven.
///
/// *Contentless* FTS5 (`content=''`): column values are not stored.
/// `contentless_delete=1` (SQLite 3.43+) lets us `DELETE FROM … WHERE
/// rowid=?` without replaying the original row text, at the cost of a modest
/// tombstone bitmap. Built-in `snippet()` is unavailable in contentless
/// mode — snippets are rendered in Rust from `documents_text` instead.
///
/// **One column, deliberately.** Document bodies are the only thing anything
/// ever MATCHes; a `name` column would cost (len − 2) postings per file.
pub fn fts_create_sql(tokenizer: &str) -> String {
    let effective = effective_tokenizer(tokenizer);
    format!(
        "CREATE VIRTUAL TABLE searchabletext USING fts5(\
            text, \
            tokenize='{}', \
            content='', \
            contentless_delete=1\
        );",
        effective.replace('\'', "''")
    )
}

/// Map a user-facing tokenizer name to the FTS5 option string applied: plain
/// `trigram` gets `remove_diacritics 1` (so `cafe` matches `café`); any
/// explicit option string is used verbatim.
pub fn effective_tokenizer(tokenizer: &str) -> String {
    let trimmed = tokenizer.trim();
    if trimmed.eq_ignore_ascii_case("trigram") {
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
        assert_eq!(
            effective_tokenizer("trigram"),
            "trigram remove_diacritics 1"
        );
        assert_eq!(
            effective_tokenizer("  trigram  "),
            "trigram remove_diacritics 1"
        );
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
