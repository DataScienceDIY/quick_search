//! SQL strings for the current schema, versioned by
//! [`super::open::CURRENT_SCHEMA_VERSION`]; there is no upgrade path.
//!
//! # Pragma profiles
//!
//! One profile per kind of connection; the field that differs is almost
//! always `cache_size`.
//!
//! **A negative `cache_size` is KiB; a positive one would be a page count**
//! (and SQLCipher reserves per-page IV/HMAC bytes, so pages never convert to
//! bytes by a clean multiply). The ceiling is not a reservation, but page
//! cache is `malloc`ed in 4 KiB units — far below glibc's mmap threshold —
//! so a filled cache is arena memory: closing the connection returns it to
//! the arena, not the kernel. That is why these numbers appear in an *idle*
//! process's footprint, and why [`crate::platform::release_free_heap`]
//! exists alongside them.
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
//! `PRAGMA mmap_size` is absent from all of them: SQLCipher's codec disables
//! mmap at runtime only when a key is set, mapped pages still count in
//! `VmRSS`, and using it would make memory behaviour differ between
//! protected and unprotected installs.

/// The bulk indexer's write connection: one per run, dies with it.
///
/// `synchronous = NORMAL` under WAL risks only the last commit on power
/// loss — acceptable for an index re-derivable from disk. The two writers
/// (full runs, coordinator) are serialized by design; `busy_timeout` is a
/// backstop. A clean shutdown truncates the WAL via
/// [`super::repo::checkpoint_and_close`] and a long run truncates it
/// periodically (see `maximum_wal_size`) — SQLite's own autocheckpoint
/// backfills but cannot reset a log that readers are touching.
pub const PRAGMAS_FAST: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas for the connection that compacts the index after a run.
///
/// [`PRAGMAS_FAST`] but with `temp_store = FILE`: SQLCipher is compiled
/// `-DSQLITE_TEMP_STORE=2`, under which anything but an explicit `FILE` puts
/// temporary databases in memory — and VACUUM builds the replacement database
/// there, so the indexer's profile would hold a rebuilt multi-gigabyte index
/// in RAM. See [`super::repo::maintain`], which also points the temp
/// directory at the index's own volume.
pub const PRAGMAS_MAINTENANCE: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = FILE;
    PRAGMA foreign_keys = ON;
";

/// The coordinator's long-lived write connection.
///
/// The one connection that lives as long as the process, so whatever its
/// cache reaches, it holds — and [`super::super::scope::advance`] runs a
/// forward-only scan of `files` through it after a config change, exactly
/// the pattern that fills a cache to its ceiling. It is also dropped
/// outright when the coordinator settles (see `Inner::go_idle`); this
/// profile bounds what it can reach *before* then.
pub const PRAGMAS_INCREMENTAL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The search worker's connection, held across a typing session (see
/// [`crate::search`]): the cache is there to still be warm when the next
/// character arrives.
///
/// **The one profile that is deliberately large, and the size is measured
/// rather than reasoned.** `tests/search_perf.rs` sweeps it; on an encrypted
/// index the curve is not a gradient but a cliff, and the cliff is at the
/// working set:
///
/// | ceiling | warm, unencrypted | warm, encrypted |
/// |---|---|---|
/// | 32–40 MiB | ~19 ms | **~19 ms** |
/// | 1–16 MiB | ~20 ms | **~47 ms** |
///
/// Unencrypted, the ceiling makes no difference at all — a miss is a `pread`
/// from the OS page cache and a `memcpy`. Encrypted, SQLCipher caches pages
/// *decrypted*, so a hit skips an AES-CBC decrypt and an HMAC-SHA512 verify
/// per 4 KiB; below the working set every warm query pays for all of them
/// again, which is the 2.5× above.
///
/// The ceiling only stands while someone is searching: the worker releases
/// the connection after [`crate::search`]'s idle window, and
/// [`crate::platform::release_free_heap`] returns the pages. The knee tracks
/// index size; if a larger index ever needs it, the fix is a bigger number
/// here, informed by the same test.
pub const PRAGMAS_SEARCH: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -32768;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas safe to apply on a read-only connection, where `journal_mode`
/// and `synchronous` can't be changed on the file. The *one-shot* readers
/// (CLI query helpers, duplicates scan, the coordinator's small reads): each
/// opens, runs a single query, and closes.
pub const PRAGMAS_READONLY: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas for a root's own reader: the walk's row prefetch, and then the
/// content pass's feeder ([`crate::content`]), which reuses this profile for
/// the rest of the run.
///
/// Two of these can exist per indexing root, so the cache size is multiplied
/// by the root count. 1 MiB is sized for the walk's queries, which each read
/// one range of `idx_files_parent` once and never revisit it. The feeder's
/// paging is the same shape, but its one-off `count_extract_scope` at pass
/// start is not: that scans the root's whole path range fetching a row per
/// entry, so on a large root it is a cold read all the way through. It is
/// deliberately here rather than on the writer — the writer holding still for
/// it stopped every other root's walk — and this is the connection that pays
/// for that, once per root.
pub const PRAGMAS_WALK_READER: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -1024;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The full current schema, applied to a fresh or just-wiped DB.
///
/// FTS5 is *contentless* (see [`fts_create_sql`]): the inverted index is
/// kept but column values aren't stored. Canonical extracted text lives in
/// `documents_text`, zstd-compressed; snippets are rendered in Rust from it
/// (see `crate::snippet`).
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
    mime          TEXT,
    type          INTEGER NOT NULL DEFAULT 0,
    content_state INTEGER NOT NULL DEFAULT 0,   -- 0=pending 1=done 2=failed 3=n/a
    hash          BLOB
);

-- Covering, not just `(parent)`. The walk's row prefetcher issues
-- `SELECT name, mtime FROM files WHERE parent = ?` once per directory — the
-- hottest read in a full run — and with the bare index that is an index probe
-- plus a table-row fetch per entry. Those fetches are cold by design: the walk
-- reader deliberately runs on a 1 MiB page cache (see `PRAGMAS_WALK_READER`).
-- Carrying `name` and `mtime` in the index makes it an index-only scan.
-- `parent` stays leading, so `SELECT DISTINCT parent` range scans and
-- `paths_in_dir` are unaffected.
CREATE INDEX idx_files_parent ON files(parent, name, mtime);
CREATE INDEX idx_files_mtime  ON files(mtime);
CREATE INDEX idx_files_type   ON files(type);
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
/// ever MATCHes: the cascade pins its query to the body
/// (`crate::search::cascade::passes`) and filename ranks come from scanning
/// `files.name`, which the trigram index of a `name` column here would only
/// duplicate — at (len − 2) postings per file indexed.
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

/// Map a user-facing tokenizer name to the actual FTS5 option string we
/// apply. The default `trigram` gets `remove_diacritics 1` appended so an
/// ASCII query like `cafe` matches indexed `café`; any explicit option
/// string is used verbatim.
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
