//! SQL strings for the current schema; there is no upgrade path.
//!
//! # Pragma profiles
//!
//! A negative `cache_size` is KiB; a positive one would be a page count. A
//! filled cache is glibc arena memory that outlives the connection; see
//! [`crate::platform::release_free_heap`].
//!
//! | Profile | Connection | Lifetime | Cache |
//! |---|---|---|---|
//! | [`PRAGMAS_FAST`] | bulk indexer writer | one run | 8 MiB |
//! | [`PRAGMAS_INCREMENTAL`] | coordinator's writer | released when idle | 4 MiB |
//! | [`pragmas_search`] | search worker | held across a typing session | [`recommended_search_cache_mib`] |
//! | [`PRAGMAS_READONLY`] | one-shot readers | a single query | 4 MiB |
//! | [`PRAGMAS_MAINTENANCE`] | VACUUM | one bulk copy | 8 MiB |
//! | [`PRAGMAS_WALK_READER`] | per-root walk prefetch and content feeder | the run | 1 MiB |
//!
//! `PRAGMA mmap_size` is absent from all of them: it would make memory
//! behaviour differ between protected and unprotected installs.
//!
//! The three *writing* profiles set `PRAGMA journal_size_limit`; readers
//! cannot. Without it the `-wal` keeps its high-water size after any
//! checkpoint that is not a successful TRUNCATE, and TRUNCATE silently
//! downgrades when it cannot take the reset lock
//! ([`crate::db::repo::checkpoint_truncate`]). SQLite applies the limit at the
//! first commit after the log restarts, so the next writer reclaims the space.
//! Each profile spells the value out because pragma strings cannot
//! interpolate; the tests keep them equal to
//! [`crate::config::MINIMUM_WAL_SIZE`].

/// The bulk indexer's write connection: one per run, dies with it.
/// `synchronous = NORMAL` under WAL risks only the last commit on power loss,
/// which an index re-derivable from disk can afford.
pub const PRAGMAS_FAST: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
    PRAGMA journal_size_limit = 16777216;
";

/// [`PRAGMAS_FAST`] but with `temp_store = FILE`. SQLCipher is compiled
/// `-DSQLITE_TEMP_STORE=2`, under which anything but an explicit `FILE` puts
/// temporary databases in memory, and VACUUM builds its replacement database
/// there: under the indexer's profile it would hold a multi-gigabyte index in
/// RAM.
pub const PRAGMAS_MAINTENANCE: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -8192;
    PRAGMA temp_store = FILE;
    PRAGMA foreign_keys = ON;
    PRAGMA journal_size_limit = 16777216;
";

/// The coordinator's long-lived write connection. It holds whatever its cache
/// reaches until the coordinator settles, so this bounds it.
pub const PRAGMAS_INCREMENTAL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
    PRAGMA journal_size_limit = 16777216;
";

/// Cache ceiling for an unencrypted index, whatever its size.
///
/// `benches/search_perf.rs` swept 1–256 MiB on 200k–1M files and found no
/// knee on a plain index, where a page-cache miss is a `memcpy` from the OS
/// cache. Only SQLCipher makes a miss expensive: it caches pages decrypted,
/// so a miss pays the AES-CBC again.
pub const SEARCH_CACHE_PLAIN_MIB: i64 = 16;

/// Floor and ceiling for the derived value. The floor is the smallest the
/// sweep found sufficient.
///
/// The cap limits resident memory and binds at about 800k files, beyond which
/// an encrypted index sits under its knee: at 1M files (a 132 MiB `files`
/// table) a keystroke costs 205 ms at 128 MiB against 58 ms at 256. Trading a
/// third of a gigabyte for a 3.5x faster session is the user's call, through
/// [`SEARCH_CACHE_OVERRIDE_MAX_MIB`].
pub const SEARCH_CACHE_MIN_MIB: i64 = 16;
pub const SEARCH_CACHE_MAX_MIB: i64 = 128;

/// Ceiling on an explicit `[search] cache_size_mib`, well above the automatic
/// cap: the override exists for a large encrypted index whose knee is past
/// [`SEARCH_CACHE_MAX_MIB`].
pub const SEARCH_CACHE_OVERRIDE_MAX_MIB: i64 = 1024;

/// Refuses to compile if the override ceiling drops to the automatic one,
/// which would re-cap the escape hatch.
const _: () = assert!(SEARCH_CACHE_OVERRIDE_MAX_MIB > SEARCH_CACHE_MAX_MIB);
const _: () = assert!(SEARCH_CACHE_MIN_MIB <= SEARCH_CACHE_MAX_MIB);

/// Cache bytes to allow per indexed file: 139 bytes per `files` row times the
/// 1.21x headroom the sweep found sufficient.
///
/// Every keystroke rescans the `files` table (filename queries are a `LIKE`
/// scan with no FTS), so the working set is that table and the warm curve's
/// knee sits at the first ceiling that holds it. The sweep, the knee per
/// corpus and the row-width caveat are in `benches/search_perf.rs`. The
/// figures predate `HMAC_MODE = Off` (see [`HMAC_MODE`]), which makes this
/// conservative. 139 B/row assumes ordinary path lengths; a much deeper tree
/// wants more, which `[search] cache_size_mib` covers.
pub const SEARCH_CACHE_BYTES_PER_FILE: i64 = 168;

/// The cache ceiling this index wants, in MiB.
pub fn recommended_search_cache_mib(files: i64, keyed: bool) -> i64 {
    if !keyed {
        return SEARCH_CACHE_PLAIN_MIB;
    }
    let want = files.max(0).saturating_mul(SEARCH_CACHE_BYTES_PER_FILE) / (1024 * 1024);
    want.clamp(SEARCH_CACHE_MIN_MIB, SEARCH_CACHE_MAX_MIB)
}

/// The search worker's connection, held across a typing session, at an
/// explicit ceiling: [`recommended_search_cache_mib`], or the user's override.
pub fn pragmas_search(cache_mib: i64) -> String {
    format!(
        "PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -{};
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
        // The override ceiling: a caller may ask for more than the automatic
        // cap, and only nonsense is refused here.
        cache_mib.clamp(SEARCH_CACHE_MIN_MIB, SEARCH_CACHE_OVERRIDE_MAX_MIB) * 1024
    )
}

/// The one-shot readers. Pragmas safe on a read-only connection, which cannot
/// change `journal_mode` or `synchronous` on the file.
pub const PRAGMAS_READONLY: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -4096;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// A root's own reader: the walk's row prefetch, then the content pass's
/// feeder. Two can exist per root, so the cache multiplies by root count;
/// raise it if the prefetcher falls behind. The feeder's cold scan is paid
/// here rather than on the writer, which would stall every other root's walk
/// while it ran.
pub const PRAGMAS_WALK_READER: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA cache_size = -1024;
    PRAGMA temp_store = MEMORY;
    PRAGMA foreign_keys = ON;
";

/// The full current schema, applied to a fresh or just-wiped DB. FTS5 is
/// contentless (see [`fts_create_sql`]); canonical extracted text lives in
/// `documents_text`, zstd-compressed.
///
/// `idx_files_parent` is row identity and the only index `parent` needs. It
/// does not cover `mtime`: the walk prefetcher's `SELECT name, mtime … WHERE
/// parent = ?` then fetches each row, which it can afford because it runs
/// ahead of the walk workers and a directory's rows are rowid-adjacent.
/// `mtime` cannot join the key, because `UNIQUE(parent, name, mtime)` would
/// admit the same file twice. If the prefetcher falls behind, raise
/// [`PRAGMAS_WALK_READER`] rather than adding a covering index.
///
/// `type` has no index: the kind filter's `(f.type & ?) != 0`
/// (`crate::query::translator`) is not sargable, so nothing could seek one.
/// Measured: an index cost ~10% on inserts and 805 pages at 300k rows, pages
/// that compete with the ones search wants resident.
///
/// Nor do `mtime` and `mime`, which only the `modified:` and `mime:` filters
/// could seek, while every insert, update and delete paid for both. Measured
/// (`indexprobe cold`, 100k tiny files): index 23.0 → 19.5 MiB, wall time
/// 0.47 → 0.41 s. The filters now cost what the `files` scan behind every
/// filename query already does: `searchtime` at 300k rows, 25 ms with or
/// without a filter, where the indexes had made a one-day filter 0.06 ms.
///
/// `idx_files_hash` is left out on purpose and built after the first run; see
/// [`crate::db::repo::build_hash_index`].
///
/// `documents_text` holds a row only when the extractor produced text (an
/// audio file with empty tags is done but has none), so `files.content_state`
/// is the authority on "content indexed". The uncompressed length is not
/// stored: zstd records it in the frame header, where
/// [`crate::db::repo::raw_text_len`] reads it.
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

CREATE UNIQUE INDEX idx_files_parent ON files(parent, name);
CREATE INDEX idx_files_content_pending ON files(id) WHERE content_state = 0;

CREATE TABLE failed_files (
    file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    reason  TEXT,
    ts      INTEGER NOT NULL
);

CREATE TABLE documents_text (
    file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    text_zstd  BLOB    NOT NULL
);

CREATE TABLE config_validation (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// The database page size, applied at creation and fixed for the file's life.
///
/// 8192 rather than the SQLite and SQLCipher default of 4096:
/// `benches/page_geometry.rs` swept 1024→65536 on a keyed 200k-file index, and
/// this balances two opposing costs:
///
/// | page | size | index rows/s | `files` scan | scattered rows | cold name |
/// |---|---|---|---|---|---|
/// | 1024 | 179.9 MiB | 1194 | 221 ms | 51 ms | 79.4 ms |
/// | 4096 | 154.8 MiB | 3289 | 100 ms | 43 ms | 44.4 ms |
/// | **8192** | **151.4 MiB** | **4955** | **55 ms** | **45 ms** | **35.7 ms** |
/// | 16384 | 151.4 MiB | 5592 | 37 ms | 58 ms | 34.2 ms |
/// | 65536 | 158.4 MiB | 7712 | 23 ms | 61 ms | 38.1 ms |
///
/// A scattered row fetch decrypts a whole page to read one ~130-byte row, so
/// it wants a small page and degrades past 8192 (43 ms → 73 ms by 32768). The
/// sequential `files` scan behind every filename query wants a large one: the
/// bytes decrypted stay ~9 MiB either way while per-page codec and pager work
/// falls. 8192 keeps scattered reads within 3% of their optimum, halves the
/// scan, ties for the smallest file, and indexes 1.5x faster than 4096.
///
/// The keyed columns predate `HMAC_MODE = Off` (see [`HMAC_MODE`]). Both costs
/// are per-page work, so dropping the authenticator scales them together, and
/// the plain arm picked 8192 too.
///
/// **Changing this is not a free edit**; see [`PROFILES_PREVIOUS`].
pub const PAGE_SIZE: i64 = 8192;

/// Which authenticator SQLCipher applies to every page, and so how much of
/// each page it spends on one.
///
/// **The shipped mode is a weakened setting; see [`HMAC_MODE`].** The cipher
/// is fixed (SQLCipher 4 dropped `PRAGMA cipher` and the provider hard-codes
/// AES-256-CBC), so this is the only lever the build has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacMode {
    /// No per-page authenticator. The reserve is the IV alone.
    Off = 0,
    Sha256 = 1,
    /// SQLCipher's own default.
    Sha512 = 2,
}

impl HmacMode {
    /// Bytes taken off the end of every page: a 16-byte IV plus the digest,
    /// rounded up to the 16-byte AES block. Both digests here are already a
    /// multiple of it, so nothing rounds.
    pub const fn reserve(self) -> i64 {
        match self {
            HmacMode::Off => 16,
            HmacMode::Sha256 => 16 + 32,
            HmacMode::Sha512 => 16 + 64,
        }
    }

    /// The pragmas selecting it, to run before `PRAGMA key`.
    ///
    /// They set SQLCipher's process defaults rather than the per-connection
    /// `cipher_use_hmac`/`cipher_hmac_algorithm`, which cannot lower a reserve
    /// `PRAGMA key` has already installed; see `db::open::key_and_probe`, which
    /// holds the lock they need. Both statements are emitted for every mode,
    /// so the globals never keep what a previous open left.
    pub const fn default_pragmas(self) -> &'static str {
        match self {
            HmacMode::Off => {
                "PRAGMA cipher_default_use_hmac = OFF; \
                 PRAGMA cipher_default_hmac_algorithm = HMAC_SHA512;"
            }
            HmacMode::Sha256 => {
                "PRAGMA cipher_default_use_hmac = ON; \
                 PRAGMA cipher_default_hmac_algorithm = HMAC_SHA256;"
            }
            HmacMode::Sha512 => {
                "PRAGMA cipher_default_use_hmac = ON; \
                 PRAGMA cipher_default_hmac_algorithm = HMAC_SHA512;"
            }
        }
    }

    /// For logs, and for the "Show database key" dialog, which tells another
    /// SQLCipher tool what to expect.
    pub const fn label(self) -> &'static str {
        match self {
            HmacMode::Off => "off",
            HmacMode::Sha256 => "HMAC_SHA256",
            HmacMode::Sha512 => "HMAC_SHA512",
        }
    }
}

/// The per-page authenticator this build writes: none.
///
/// # Why it is safe to drop
///
/// The index holds text read from files the same user can already read, so
/// anything positioned to tamper with the index could read the originals
/// instead; the HMAC defends almost nothing, yet costs every page read and
/// write. An unprotected index has never had per-page authentication, so this
/// makes the two alike rather than putting the protected one behind. SQLite's
/// page-header and cell checks still catch structural damage in both key
/// states, and the index is re-derivable from disk. The encryption is
/// unchanged: pages stay AES-256-CBC.
///
/// # What it buys
///
/// `benches/cipher_hmac.rs`, 200k files, all four arms in one process. Warm
/// total is six cascade shapes summed, the steady state of a typing session:
///
/// | arm | seed | warm total | duplicates | size |
/// |---|---|---|---|---|
/// | plain (no password) | 30.9 s | 125.6 ms | 206 ms | 151.5 MiB |
/// | **keyed, HMAC off** | **37.7 s** | **146.2 ms** | **221 ms** | **151.5 MiB** |
/// | keyed, HMAC_SHA256 | 40.0 s | 209.0 ms | 250 ms | 150.6 MiB |
/// | keyed, HMAC_SHA512 | 41.3 s | 259.5 ms | 262 ms | 151.4 MiB |
///
/// Against SQLCipher's default that is 1.78x on warm search, and it takes
/// encrypted-over-plain from 2.07x to 1.16x, 84% of the cost of having a
/// password. A filename query went 32.6 ms → 13.8 ms and a rare body term
/// 39.8 → 18.6.
///
/// SHA-256 recovers only 1.24x of the 2.07x despite halving the digest,
/// because the digest is not the cost: `sqlcipher_openssl_hmac` calls
/// `EVP_MAC_fetch(NULL, "HMAC", NULL)`, `EVP_MAC_CTX_new` and an
/// `EVP_MAC_init` that fetches the digest by name, two OpenSSL 3 provider
/// lookups per page whichever digest is chosen. Only `Off` removes them.
///
/// Size is unmoved (151.5 against 151.4 MiB): [`fts_pgsz_for`] re-derives the
/// record size from the new reserve, so the 64 bytes handed back per page go
/// into leaves.
///
/// # Figures taken under HMAC-SHA512
///
/// [`PAGE_SIZE`]'s sweep, [`SEARCH_CACHE_BYTES_PER_FILE`], and the keyed
/// figures in `benches/search_perf.rs`, `benches/page_geometry.rs`,
/// `tests/encrypted_perf.rs` and `search::IDLE_RELEASE` were measured under
/// HMAC-SHA512 and not re-swept. A keyed page miss is now cheaper, so those
/// keyed costs are overstated and the cache sizing errs large.
///
/// # Changing it is not a free edit
///
/// The reserve is part of the on-disk format, so a file written under another
/// mode does not decrypt at all. See [`PROFILES_PREVIOUS`].
pub const HMAC_MODE: HmacMode = HmacMode::Off;

/// A page size and an HMAC mode: everything about a keyed file's layout that
/// must be known before it can be read, because its header is ciphertext
/// until SQLCipher has been told both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub page_size: i64,
    pub hmac: HmacMode,
}

impl Profile {
    /// Bytes reserved on every page. Zero unencrypted: a plain file has
    /// neither an IV nor an authenticator.
    pub const fn reserve(self, keyed: bool) -> i64 {
        if keyed {
            self.hmac.reserve()
        } else {
            0
        }
    }
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}-byte pages, HMAC {}",
            self.page_size,
            self.hmac.label()
        )
    }
}

/// The layout this build creates files at.
pub const PROFILE: Profile = Profile {
    page_size: PAGE_SIZE,
    hmac: HMAC_MODE,
};

/// Layouts earlier versions created files at, newest first.
///
/// A keyed file under another profile does not decrypt at all, so its version
/// cannot be read and `open::key_and_probe` would report
/// `KEY_MISMATCH: wrong-password`. `db::open` retries each entry first; a file
/// that opens under one is wiped and rebuilt at [`PROFILE`]. Unencrypted
/// files open under any profile and take the same path. Drop an entry only
/// when indexes still at that layout may be reported as a wrong password.
pub const PROFILES_PREVIOUS: &[Profile] = &[
    // Every protected index built before [`HMAC_MODE`] became `Off`: the
    // shipped page size under SQLCipher's own authenticator. First because it
    // is by far the common case, and the probe stops at the first profile that
    // answers.
    Profile {
        page_size: PAGE_SIZE,
        hmac: HmacMode::Sha512,
    },
    Profile {
        page_size: 4096,
        hmac: HmacMode::Sha512,
    },
];

/// A profile may not be both previous and current: the probe would retry the
/// layout it just failed on, and `ProfileMatch::Previous` would mean nothing.
const _: () = {
    let mut i = 0;
    while i < PROFILES_PREVIOUS.len() {
        assert!(
            !(PROFILES_PREVIOUS[i].page_size == PROFILE.page_size
                && PROFILES_PREVIOUS[i].hmac as u8 == PROFILE.hmac as u8),
            "PROFILES_PREVIOUS repeats the current profile"
        );
        i += 1;
    }
};

/// FTS5's `pgsz` for `profile`, set once at creation and persisted in
/// `searchabletext_config`.
///
/// A table leaf holds `page − reserve − 35` bytes inline and an FTS5 record
/// takes `pgsz + 2`, so `pgsz ≤ page − reserve − 37`; `MARGIN` adds slack.
/// Missing the limit is silent and expensive: every full leaf spills to an
/// overflow page, a second fetch and decrypt per read. FTS5's default of 4050
/// fits only a plain 4096-byte page (keyed under HMAC-SHA512 it overflowed
/// 10954 of 12354 leaves) and wastes half of an 8192-byte page, so the value
/// follows both page size and [`HmacMode`].
pub fn fts_pgsz_for(profile: Profile, keyed: bool) -> i64 {
    /// Slack under the inline limit, in bytes.
    const MARGIN: i64 = 9;
    profile.page_size - profile.reserve(keyed) - 37 - MARGIN
}

/// Set [`fts_pgsz_for`] on a freshly created `searchabletext`, even when it
/// equals FTS5's default: the row is then redundant but harmless, and one
/// code path is simpler.
pub fn fts_set_pgsz(
    conn: &rusqlite::Connection,
    profile: Profile,
    keyed: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('pgsz', ?1)",
        [fts_pgsz_for(profile, keyed)],
    )
    .map(|_| ())
}

/// FTS5's own default `pgsz`, from `FTS5_DEFAULT_PAGE_SIZE` in the amalgamation.
pub const FTS5_DEFAULT_PGSZ: i64 = 4050;

/// FTS5 virtual table DDL. Separate because the tokenizer is config-driven.
///
/// Contentless FTS5 (`content=''`): column values are not stored.
/// `contentless_delete=1` (SQLite 3.43+) allows `DELETE FROM … WHERE rowid=?`
/// without replaying the original row text, at the cost of a modest tombstone
/// bitmap. Built-in `snippet()` is unavailable in contentless mode, so
/// snippets are cut in Rust from `documents_text`.
///
/// One column: only document bodies are ever MATCHed, and a `name` column
/// would cost (len − 2) postings per file.
///
/// `positions` is [`crate::config::ProcessingConfig::fts_positions`]. Without
/// them (`detail=none`, the default through `compact_text_index`) the table
/// answers only single-token phrases, so the search side asks for a term as
/// the AND of its trigrams (`search::prefilter::fts_literal`) and the
/// full-text pass drops the documents where they are not adjacent, which it
/// can because text is always stored. Measured on 1.1 GB of crate sources
/// (`indexprobe cold`, then `searchtime`), identical results either way:
///
/// | | positions | none |
/// |---|---|---|
/// | index | 1291 MiB | **219 MiB** |
/// | build | 37.4 s | **24.6 s** |
/// | written through the WAL | 4.1 GB | **0.7 GB** |
/// | candidates per true hit | 1 | 1.0–2x a word, 8–13x a phrase |
/// | warm search time | 1 | 0.9–1.8x a word, 2.8x a phrase |
///
/// The slowest was a phrase of common words (`impl Iterator for`, 24 → 67
/// ms), whose trigrams occur in many documents that lack the phrase.
///
/// The configured tokenizer is applied as written. Plain `trigram` once got
/// `remove_diacritics 1` appended, but full-text verification folds ASCII
/// only (`tests/cascade.rs`, `diacritic_fts_candidates_are_dropped`), so the
/// extra candidates it matched were always dropped again. Dropping it measured
/// neutral on indexing time (`indexprobe cold`, 5k accented documents).
pub fn fts_create_sql(tokenizer: &str, positions: bool) -> String {
    format!(
        "CREATE VIRTUAL TABLE searchabletext USING fts5(\
            text, \
            tokenize='{}', \
            content='', \
            contentless_delete=1{}\
        );",
        tokenizer.trim().replace('\'', "''"),
        if positions { "" } else { ", detail=none" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every writing profile bounds the log at
    /// [`crate::config::MINIMUM_WAL_SIZE`], spelled as a literal in each. The
    /// read-only profiles must not carry it: `journal_size_limit` is a write
    /// to the file, which a read-only connection cannot make.
    /// `repo::tests::journal_size_limit_gives_the_space_back_after_a_blocked_truncate`
    /// shows the limit working.
    #[test]
    fn every_writing_profile_bounds_the_log() {
        let want = format!(
            "PRAGMA journal_size_limit = {};",
            crate::config::MINIMUM_WAL_SIZE
        );
        for (name, pragmas) in [
            ("PRAGMAS_FAST", PRAGMAS_FAST),
            ("PRAGMAS_MAINTENANCE", PRAGMAS_MAINTENANCE),
            ("PRAGMAS_INCREMENTAL", PRAGMAS_INCREMENTAL),
        ] {
            assert!(pragmas.contains(&want), "{} is missing {}", name, want);
        }
        for (name, pragmas) in [
            ("PRAGMAS_READONLY", PRAGMAS_READONLY.to_string()),
            ("PRAGMAS_WALK_READER", PRAGMAS_WALK_READER.to_string()),
            ("PRAGMAS_SEARCH", pragmas_search(32)),
        ] {
            assert!(
                !pragmas.contains("journal_size_limit"),
                "{} is read-only and cannot set journal_size_limit",
                name
            );
        }
    }

    /// The sweep found no knee on a plain index at any corpus size, so this
    /// must not grow with one.
    #[test]
    fn a_plain_index_gets_the_same_ceiling_at_every_size() {
        for files in [0, 1_000, 200_000, 1_000_000, 50_000_000] {
            assert_eq!(
                recommended_search_cache_mib(files, false),
                SEARCH_CACHE_PLAIN_MIB,
                "{} files",
                files
            );
        }
    }

    #[test]
    fn a_keyed_index_is_clamped_at_both_ends() {
        assert_eq!(recommended_search_cache_mib(0, true), SEARCH_CACHE_MIN_MIB);
        assert_eq!(
            recommended_search_cache_mib(-1, true),
            SEARCH_CACHE_MIN_MIB,
            "a negative count is nonsense, not a reason to panic"
        );
        assert_eq!(
            recommended_search_cache_mib(i64::MAX, true),
            SEARCH_CACHE_MAX_MIB,
            "and an absurd one must not overflow into a small ceiling"
        );
    }

    /// Between the clamps it must track the corpus; a constant would satisfy
    /// every other test here.
    #[test]
    fn a_keyed_index_grows_with_the_corpus_between_the_clamps() {
        let small = recommended_search_cache_mib(300_000, true);
        let large = recommended_search_cache_mib(700_000, true);
        assert!(
            small < large,
            "300k wants {} MiB and 700k wants {} MiB",
            small,
            large
        );
        assert!(large <= SEARCH_CACHE_MAX_MIB);
    }

    /// The three corpora `benches/search_perf.rs` measured, against the knee
    /// it found for each. Below the knee a keyed search runs 3.4–3.9x slower,
    /// so the recommendation must reach it.
    #[test]
    fn the_recommendation_clears_every_measured_knee() {
        for (files, knee_mib) in [(200_000, 32), (600_000, 96), (1_000_000, 128)] {
            let got = recommended_search_cache_mib(files, true);
            assert!(
                got >= knee_mib.min(SEARCH_CACHE_MAX_MIB),
                "{} files: recommending {} MiB, under the measured knee of {} MiB",
                files,
                got,
                knee_mib
            );
        }
    }

    #[test]
    fn pragmas_search_writes_a_kib_ceiling_and_clamps_it() {
        assert!(pragmas_search(64).contains("cache_size = -65536"));
        assert!(
            pragmas_search(0).contains(&format!("-{}", SEARCH_CACHE_MIN_MIB * 1024)),
            "a zero must not reach SQLite, where it means its own default"
        );
        assert!(
            pragmas_search(999_999).contains(&format!("-{}", SEARCH_CACHE_OVERRIDE_MAX_MIB * 1024)),
            "nor must an absurd one"
        );
    }

    /// An explicit setting must be able to exceed the automatic cap: past ~800k
    /// files the derived value is capped below the measured knee, and the
    /// override is the only way to reach it.
    #[test]
    fn an_explicit_ceiling_may_exceed_the_automatic_cap() {
        let asked = SEARCH_CACHE_MAX_MIB * 2;
        assert!(
            pragmas_search(asked).contains(&format!("-{}", asked * 1024)),
            "{} MiB was asked for and must be applied verbatim",
            asked
        );
    }

    #[test]
    fn the_tokenizer_is_applied_as_written() {
        assert!(fts_create_sql("  trigram  ", true).contains("tokenize='trigram',"));
        assert!(fts_create_sql("unicode61", true).contains("tokenize='unicode61',"));
        assert!(
            fts_create_sql("trigram remove_diacritics 1", true)
                .contains("tokenize='trigram remove_diacritics 1',"),
            "an explicit option string still passes through"
        );
    }

    #[test]
    fn positions_decide_the_detail_option() {
        assert!(!fts_create_sql("trigram", true).contains("detail"));
        assert!(fts_create_sql("trigram", false).contains(", detail=none)"));
    }
}
