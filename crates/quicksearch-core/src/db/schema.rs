//! SQL strings for the current schema.
//!
//! Versioned by [`super::open::CURRENT_SCHEMA_VERSION`], but there is no
//! upgrade path: a database written under any other version is wiped and
//! recreated from [`SCHEMA_CURRENT`]. See [`super::open`] for why.
//!
//! # The pragma profiles, and why there are six of them
//!
//! Each profile below belongs to one kind of connection, and the only field
//! that differs between most of them is `cache_size`. They are separate
//! constants rather than one shared string because the page cache is the
//! largest thing a connection holds, and what the right size is depends
//! entirely on what the connection does and how long it lives — a question
//! with six different answers here.
//!
//! **A negative `cache_size` is KiB; a positive one would be a page count.**
//! Nothing sets `page_size`, so a page count would be an unfalsifiable claim
//! about SQLCipher's default, and SQLCipher reserves per-page bytes for the IV
//! and HMAC on top of that, so pages do not convert to bytes by a clean
//! multiply. Every value here is a ceiling in KiB, which is the thing actually
//! being reasoned about.
//!
//! It *is* a ceiling and not a reservation — a connection that touches ten
//! pages holds ten pages. What matters is which connections can reach the
//! ceiling, which is any of them that scans a table, and how long they hold it
//! afterwards. Page cache is `malloc`ed in 4 KiB units, far below glibc's mmap
//! threshold, so a filled cache is arena memory: closing the connection
//! returns it to the arena, not to the kernel. That is why these numbers show
//! up in an *idle* process's footprint at all, and why
//! [`crate::platform::release_free_heap`] exists alongside them.
//!
//! Note which way each profile is sized. Every one of them is small because
//! its connection either scans once or lives a long time — except
//! [`PRAGMAS_SEARCH`], which is large because it is the only cache that is
//! *reused* often enough to pay for itself, and which is released when
//! searching stops so that it never becomes part of the idle floor.
//!
//! | Profile | Connection | Lifetime | Cache |
//! |---|---|---|---|
//! | [`PRAGMAS_FAST`] | bulk indexer writer | one run | 8 MiB |
//! | [`PRAGMAS_INCREMENTAL`] | coordinator's writer | released when idle | 4 MiB |
//! | [`PRAGMAS_SEARCH`] | search worker | held across a typing session | 32 MiB |
//! | [`PRAGMAS_READONLY`] | one-shot readers | a single query | 4 MiB |
//! | [`PRAGMAS_MAINTENANCE`] | VACUUM | one bulk copy | 8 MiB |
//! | [`PRAGMAS_WALK_READER`] | per-root row prefetch | the walk | 1 MiB |
//!
//! `PRAGMA mmap_size` is deliberately absent from all of them. It is not
//! compile-disabled, and SQLCipher's codec only turns it off at runtime when a
//! key is set — so an unprotected index could use it. It stays off because
//! mapped pages still count in `VmRSS`, so it would not help the number this
//! is all about, and because it would make memory behaviour differ between
//! protected and unprotected installs, which is exactly the kind of silent
//! per-configuration difference [`crate::platform`] argues against.

/// The bulk indexer's write connection: one per run, dies with it.
///
/// 8 MiB of page cache, not the 40 MiB this profile used to take. The cache
/// buys a writer the chance to batch dirty pages before spilling mid
/// transaction, and a run commits every `batch_size` rows (200–500), so the
/// working set between commits is nowhere near 40 MiB. The old figure was
/// never chosen for this connection — it was chosen once and then inherited by
/// every other profile that copied this one.
///
/// WAL, not journal-off: auto-indexing writes continuously while searches
/// stream from their own read-only connections, and WAL is what lets those
/// readers proceed without ever blocking the writer (or vice versa).
/// `synchronous = NORMAL` under WAL risks only the last commit on power
/// loss — acceptable for an index that is re-derivable from disk. Only two
/// writers exist (full index runs and the coordinator's incremental
/// updates) and they're serialized by design; `busy_timeout` is a backstop,
/// not a coordination mechanism. A clean shutdown truncates the log via
/// [`super::repo::checkpoint_and_close`], and a long run truncates it
/// periodically as it goes (see `maximum_wal_size`) — SQLite's own
/// autocheckpoint backfills but cannot reset a log that readers are touching.
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
/// [`PRAGMAS_FAST`] but for `temp_store`, and that one difference is the whole
/// reason this profile exists. VACUUM builds the replacement database in the
/// temp store; SQLCipher is compiled `-DSQLITE_TEMP_STORE=2`, under which
/// anything but an explicit `FILE` puts that database in memory. Vacuuming a
/// multi-gigabyte index on the indexer's connection would try to hold the
/// entire rebuilt index in RAM. See [`super::repo::maintain`], which also
/// points the temp directory at the index's own volume.
///
/// The page cache is sized for one bulk copy followed by a close, which is
/// all this connection ever does.
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
/// [`PRAGMAS_FAST`] without the `journal_mode`/`synchronous` lines it does not
/// need to set again, and with a smaller cache, which is why it is its own
/// profile. This connection exists to apply a handful of single-row upserts
/// and deletes per batch of watcher events; it has no bulk phase to batch for.
///
/// The size matters more here than anywhere else because of the lifetime. This
/// is the one connection that lives as long as the process, so whatever it
/// reaches, it holds — and [`super::super::scope::advance`] runs a
/// forward-only scan of `files` through it after a config change, which is
/// exactly the access pattern that fills a cache to its ceiling. Before this
/// profile existed that meant an idle QuickSearch carried a full 40 MiB page
/// cache from a reconciliation the user did once. It is now also dropped
/// outright when the coordinator settles (see `Inner::go_idle`); this profile
/// bounds what it can reach *before* then.
pub const PRAGMAS_INCREMENTAL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The search worker's connection, which is held across requests.
///
/// A search fires on every character typed, and the worker keeps one
/// connection for the whole typing session rather than opening one per
/// keystroke (see [`crate::search`]). That inverts what the cache is for: it
/// is not there to absorb a single cold cascade, it is there to still be warm
/// when the next character arrives.
///
/// **This is the one profile that is deliberately large, and the size is
/// measured rather than reasoned.** `tests/search_perf.rs` sweeps it; on an
/// encrypted index the curve is not a gradient but a cliff, and the cliff is
/// at the working set:
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
/// again, which is the 2.5× above. One number covers both because sizing this
/// by whether a key happens to be set would make search latency depend on a
/// setting nobody would connect it to.
///
/// The ceiling only stands while someone is searching: the worker releases the
/// connection after [`crate::search`]'s idle window, and
/// [`crate::platform::release_free_heap`] returns the pages. So this buys warm
/// search latency without adding to what an idle process holds — which is the
/// trade the rest of these profiles are making in the other direction.
///
/// The knee tracks index size, so on a very large encrypted index even this
/// will not hold the working set. That degrades to the old behaviour rather
/// than to something worse, and the fix if it ever matters is a bigger number
/// here, informed by the same test.
pub const PRAGMAS_SEARCH: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -32768;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas safe to apply on a read-only connection, where `journal_mode`
/// and `synchronous` can't be changed on the file. Used by
/// [`super::open::open_existing`] for read-only opens; write paths get the
/// full [`PRAGMAS_FAST`] set.
///
/// This is the *one-shot* reader now that the search worker has
/// [`PRAGMAS_SEARCH`]: the CLI query helpers, the duplicates scan, the
/// coordinator's own small reads. Each opens, runs a single query, and closes.
/// A cache only pays for itself across queries, and these connections have no
/// across.
pub const PRAGMAS_READONLY: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// Pragmas for a walk's row-prefetch connection.
///
/// Identical to [`PRAGMAS_READONLY`] but for `cache_size`, and that one
/// difference is the point. One of these connections exists per indexing
/// root, so a cache sized for a connection that runs alone would be
/// multiplied by the root count — and this was the first profile to be sized
/// for its access pattern rather than copied from [`PRAGMAS_FAST`], which is
/// why the others now are too.
///
/// 1 MiB is enough to hold the upper levels of `idx_files_parent` hot, which
/// is all these queries touch: each one is a single index range lookup, and
/// the pages under it are read once and not revisited.
pub const PRAGMAS_WALK_READER: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -1024;
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
