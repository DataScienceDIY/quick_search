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
//!
//! `PRAGMA journal_size_limit` is on the three *writing* profiles and on none
//! of the readers, which cannot set it. Without it the `-wal` keeps its
//! high-water mark on disk after every checkpoint that is not a successful
//! TRUNCATE — and a TRUNCATE silently downgrades whenever it cannot take the
//! reset lock (see [`crate::db::repo::checkpoint_truncate`]), which is most
//! likely in exactly the case that grew the log. One bad run then leaves a
//! multi-gigabyte file for every later reader to page around.
//!
//! The limit is a backstop, not a second checkpoint: SQLite applies it at the
//! first commit *after* the log restarts, so the space comes back from
//! whichever writer touches the index next, with no successful TRUNCATE
//! anywhere in the story. `repo::journal_size_limit_gives_the_space_back_after_a_blocked_truncate`
//! is the demonstration. It is spelled out in each profile rather than shared,
//! because a pragma string cannot interpolate a constant;
//! `every_writing_profile_bounds_the_log` is what keeps the three in step with
//! [`crate::config::MINIMUM_WAL_SIZE`].

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
    PRAGMA journal_size_limit = 16777216;
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
    PRAGMA journal_size_limit = 16777216;
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
    PRAGMA journal_size_limit = 16777216;
";

/// Cache ceiling for an **unencrypted** index, whatever its size.
///
/// `benches/search_perf.rs` swept 1 MiB → 256 MiB against corpora from 200k to
/// 1M files and found no knee at all on a plain index: warm search was flat
/// within 10% across the whole range, because a page-cache miss there is a
/// `memcpy` from the OS cache. Only SQLCipher makes a miss expensive — it
/// caches pages *decrypted*, so a miss re-pays the AES-CBC.
/// Spending more than this on a plain index buys nothing measurable.
pub const SEARCH_CACHE_PLAIN_MIB: i64 = 16;

/// Floor and ceiling for the **derived** value. The floor is the smallest the
/// sweep ever found sufficient.
///
/// The cap is a deliberate limit on resident memory, and it does bind: it is
/// reached at about 800k files, and beyond that an encrypted index sits under
/// its knee. Measured at 1M files, whose `files` table is 132 MiB: 205 ms per
/// keystroke at 128 MiB against 58 ms at 256. That is the trade — a third of a
/// gigabyte held for the life of a search session, or a 3.5x slower one — and
/// it is the user's to make, which is what [`SEARCH_CACHE_OVERRIDE_MAX_MIB`]
/// is for.
pub const SEARCH_CACHE_MIN_MIB: i64 = 16;
pub const SEARCH_CACHE_MAX_MIB: i64 = 128;

/// Ceiling on an **explicit** `[search] cache_size_mib`, well above the
/// automatic cap: capping a manual override at the automatic limit would deny
/// it in exactly the case that needs it, a large encrypted index whose knee is
/// past 128 MiB.
pub const SEARCH_CACHE_OVERRIDE_MAX_MIB: i64 = 1024;

/// Lowering the override ceiling to the automatic one would quietly re-cap the
/// escape hatch; this refuses to compile instead.
const _: () = assert!(SEARCH_CACHE_OVERRIDE_MAX_MIB > SEARCH_CACHE_MAX_MIB);
const _: () = assert!(SEARCH_CACHE_MIN_MIB <= SEARCH_CACHE_MAX_MIB);

/// Cache bytes to allow per indexed file, from `benches/search_perf.rs`.
///
/// What every keystroke rescans is the `files` table — `search/cascade/passes`
/// answers filename queries with `WHERE f.name LIKE '%…%'`, a full table scan
/// with no FTS in it, and the fuzzy pass rescans with `WHERE 1=1`. So the
/// working set is that table, its size is linear in row count, and the knee in
/// the warm curve sits at the first ceiling that holds it. Below the knee an
/// encrypted index re-decrypts the table on every keystroke and runs 2.4–2.7x
/// slower; at it, keyed and plain are within 2% of each other.
///
/// 139 bytes per row, times the 1.21x the sweep found sufficient. Both halves
/// were measured, and the product lands on the observed knee exactly:
///
/// | corpus | `files` table | knee | ratio | this formula |
/// |---|---|---|---|---|
/// | 200k | 26.5 MiB | 32 MiB | 1.21x | 32 MiB |
/// | 600k | 79.5 MiB | 96 MiB | 1.21x | 96 MiB |
///
/// The step is not subtle — keyed at 600k ran 121–130 ms at every ceiling up
/// to 64 MiB and 33.7 ms at 96.
///
/// **Calibrated while [`HMAC_MODE`] was still HMAC-SHA512, and not re-swept
/// since.** A cache miss is now materially cheaper — `benches/cipher_hmac.rs`
/// measured warm search 1.78x faster with the authenticator gone — so the
/// knee this reaches for is shallower than it was, and the recommendation is
/// therefore *conservative*: it asks for at least as much cache as it needs,
/// never less. Erring that way is safe for latency and costs only resident
/// memory. Re-running `benches/search_perf.rs` would likely let both the
/// bytes-per-file figure and [`SEARCH_CACHE_MAX_MIB`] come down; until someone
/// does, do not quote the 2.4–2.7x above as current.
///
/// **139 B/row assumes ordinary path lengths.** `parent` is stored per row, so
/// a tree far deeper than the measured one (`/seed/NNN/` plus five segments)
/// has wider rows and wants more; the narrow shape the older harnesses seeded
/// measured 69.5 B/row, half of this, and calibrating against it would have
/// under-sized every index by two. That is what `[search] cache_size_mib`
/// overrides, and why the GUI shows the recommendation beside it rather than
/// hiding the arithmetic.
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
/// explicit ceiling — see [`recommended_search_cache_mib`] for why it is not a
/// constant. The one deliberately large profile.
pub fn pragmas_search(cache_mib: i64) -> String {
    format!(
        "PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -{};
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
        // The *override* ceiling: a caller may legitimately ask for more than
        // the automatic cap, and only nonsense is refused here.
        cache_mib.clamp(SEARCH_CACHE_MIN_MIB, SEARCH_CACHE_OVERRIDE_MAX_MIB) * 1024
    )
}

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

/// The database page size, applied at creation and fixed for the file's life.
///
/// 8192, not SQLite's and SQLCipher's default of 4096, because
/// `benches/page_geometry.rs` swept 1024→65536 on a keyed 200k-file index and
/// this is where the two opposing costs balance:
///
/// | page | size | index rows/s | `files` scan | scattered rows | cold name |
/// |---|---|---|---|---|---|
/// | 1024 | 179.9 MiB | 1194 | 221 ms | 51 ms | 79.4 ms |
/// | 4096 | 154.8 MiB | 3289 | 100 ms | 43 ms | 44.4 ms |
/// | **8192** | **151.4 MiB** | **4955** | **55 ms** | **45 ms** | **35.7 ms** |
/// | 16384 | 151.4 MiB | 5592 | 37 ms | 58 ms | 34.2 ms |
/// | 65536 | 158.4 MiB | 7712 | 23 ms | 61 ms | 38.1 ms |
///
/// The table above was swept while [`HMAC_MODE`] was still SQLCipher's
/// HMAC-SHA512, so the keyed columns overstate today's per-page cost. The
/// *shape* of the trade is unchanged — both arms of it are per-page work, so
/// removing the authenticator scales them together rather than moving the
/// balance — and the plain arm, which never had an HMAC, picked 8192 too.
///
/// A scattered row fetch decrypts a whole page to read one ~130-byte row out
/// of it, so it wants a *small* page and degrades past 8192 (43 ms → 73 ms by
/// 32768). The `files` scan behind every filename query is sequential and
/// wants a *large* one, improving all the way — because the bytes decrypted
/// stay the same (~9 MiB either way) while the number of per-page codec and
/// pager operations falls. 8192 keeps scattered reads within
/// 3% of their optimum, halves the scan, is tied for the smallest file, and
/// indexes 1.5x faster than 4096.
///
/// **Changing this is not a free edit** — see [`PROFILES_PREVIOUS`].
pub const PAGE_SIZE: i64 = 8192;

/// Which authenticator SQLCipher applies to every page — and so how much of
/// every page it spends on one.
///
/// **This is a deliberately weakened setting; see [`HMAC_MODE`].** The
/// encryption itself is not a choice: SQLCipher 4 dropped `PRAGMA cipher` and
/// the provider hard-codes AES-256-CBC, so the HMAC is the only lever the
/// build has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacMode {
    /// No per-page authenticator. The reserve is the IV alone.
    Off,
    Sha256,
    /// SQLCipher's own default.
    Sha512,
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

    /// The pragmas selecting it, to be run **before** `PRAGMA key`.
    ///
    /// These are SQLCipher's process-*default* settings, not the
    /// per-connection `cipher_use_hmac` / `cipher_hmac_algorithm`, and the
    /// difference is not stylistic: the per-connection forms cannot lower a
    /// reserve that `PRAGMA key` has already installed. See
    /// `db::open::key_and_probe`, which owns the lock they need.
    ///
    /// Both statements are emitted for every mode, so the globals are fully
    /// specified whatever the previous open left behind.
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

    /// For logs, and for the "Show database key" dialog, which has to tell
    /// another SQLCipher tool what to expect.
    pub const fn label(self) -> &'static str {
        match self {
            HmacMode::Off => "off",
            HmacMode::Sha256 => "HMAC_SHA256",
            HmacMode::Sha512 => "HMAC_SHA512",
        }
    }
}

/// The per-page authenticator this build writes: **none**.
///
/// # Why it is safe to drop
///
/// A threat-model argument, not a benchmark alone. The index holds text read
/// out of files the same user can already read, so anything positioned to
/// *tamper* with the index could read the originals instead — what the HMAC
/// defends is nearly empty, and it is paid on every page read and every page
/// write. Nor does dropping it cost integrity relative to the product's own
/// baseline: an unprotected index has no per-page authentication either, and
/// never has, so this makes the two behave alike rather than putting the
/// protected one behind. Structural damage is still caught by SQLite's own
/// page-header and cell checks, in both key states, and the index is
/// re-derivable from disk regardless.
///
/// The encryption is untouched. SQLCipher 4 removed `PRAGMA cipher` and the
/// provider hard-codes AES-256-CBC, so pages are as confidential as before;
/// only the authenticator is gone.
///
/// # What it buys
///
/// `benches/cipher_hmac.rs`, 200k files, all four arms in one process. Warm
/// total is six cascade shapes summed — the steady state of a typing session:
///
/// | arm | seed | warm total | duplicates | size |
/// |---|---|---|---|---|
/// | plain (no password) | 30.9 s | 125.6 ms | 206 ms | 151.5 MiB |
/// | **keyed, HMAC off** | **37.7 s** | **146.2 ms** | **221 ms** | **151.5 MiB** |
/// | keyed, HMAC_SHA256 | 40.0 s | 209.0 ms | 250 ms | 150.6 MiB |
/// | keyed, HMAC_SHA512 | 41.3 s | 259.5 ms | 262 ms | 151.4 MiB |
///
/// Against SQLCipher's default that is **1.78x on warm search**, and it takes
/// encrypted-over-plain from 2.07x to 1.16x — 84% of the penalty for having a
/// password at all. Per shape it is widest where it matters most: a filename
/// query went 32.6 ms → 13.8 ms and a rare body term 39.8 → 18.6.
///
/// **SHA-256 is the arm to understand, because it is the one that
/// disappoints.** It halves the digest and gives back 32 bytes of every page,
/// yet recovers only 1.24x of the 2.07x. The reason is that the digest is not
/// where the money goes: `sqlcipher_openssl_hmac` calls
/// `EVP_MAC_fetch(NULL, "HMAC", NULL)`, `EVP_MAC_CTX_new` and an
/// `EVP_MAC_init` that fetches the digest *by name* — two OpenSSL 3 provider
/// lookups per page, paid whichever digest is chosen. Only `Off` removes them,
/// which is why the middle ground is worth so much less than it looks.
///
/// Size is unmoved either way (151.5 against 151.4 MiB): `fts_pgsz_for`
/// re-derives the record size from the new reserve, so the 64 bytes handed
/// back per page go into leaves rather than into the file.
///
/// # Changing it is not a free edit
///
/// The reserve is part of the on-disk format, so a file written under another
/// mode does not decrypt at all. See [`PROFILES_PREVIOUS`].
pub const HMAC_MODE: HmacMode = HmacMode::Off;

/// A page size and an HMAC mode: everything about a keyed file's layout that
/// has to be known *before* it can be read, because its header is ciphertext
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
/// A keyed file under another profile does not decrypt *at all*: the header
/// comes back as noise, and without this list `open::key_and_probe` would
/// report it as `KEY_MISMATCH: wrong-password`. No schema-version bump can
/// rescue that, because the version cannot be read either. So `db::open`
/// reopens under each of these before declaring a mismatch, and a file that
/// answers to one is treated as ordinary schema drift: wiped and rebuilt at
/// [`PROFILE`].
///
/// Unencrypted files never needed it — `PRAGMA page_size` against an existing
/// file is silently ignored and they have no reserve to get wrong, so they
/// open at whatever they were built with and the version check does the rest —
/// but they take the same path for free.
///
/// Anything appended here is a layout some user's index is still sitting at.
/// Entries can only be dropped when it is acceptable for those indexes to read
/// as a wrong password.
pub const PROFILES_PREVIOUS: &[Profile] = &[
    // Every protected index in the field is here: the shipped page size, under
    // SQLCipher's own authenticator, before [`HMAC_MODE`] became `Off`. It is
    // listed first because it is overwhelmingly the common case, and the probe
    // stops at the first profile that answers.
    Profile {
        page_size: PAGE_SIZE,
        hmac: HmacMode::Sha512,
    },
    Profile {
        page_size: 4096,
        hmac: HmacMode::Sha512,
    },
];

/// A profile may not be listed as previous *and* current — the probe would
/// then retry the layout it just failed on, and `ProfileMatch::Previous` would
/// mean nothing.
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

/// FTS5's `pgsz` for one profile, applied once at creation and then persisted
/// in `searchabletext_config`.
///
/// The arithmetic, in one line: a table leaf holds
/// `page − reserve − 35` bytes inline, and an FTS5 record runs to `pgsz + 2`,
/// so `pgsz ≤ page − reserve − 37`. `MARGIN` keeps a record that overruns by a
/// byte or two from falling off the cliff.
///
/// The cliff is worth stating because it is expensive and silent. FTS5's own
/// default is 4050 — a number SQLite chose so a full leaf fits inline in a
/// *plain* 4096-byte page. Keyed under SQLCipher's own HMAC-SHA512 the reserve
/// drops the limit to 3981 and that default misses it by 71 bytes, sending
/// **every** full leaf to an overflow page: a second fetch and decrypt on
/// every read of it. On the 60k-file corpus in `tests/encrypted_perf.rs` that
/// was 10954 of 12354 leaves and 70.7 MiB against the plain index's 65.2;
/// fitting them inline brought it to 65.5.
///
/// It cuts the other way too: at a page size of 8192 a 4050-byte record leaves
/// half of every page empty, because a second one will not fit. So this is
/// derived from the profile rather than pinned — and it has to follow
/// [`HmacMode`] as well as the page size, because the reserve moves with both.
pub fn fts_pgsz_for(profile: Profile, keyed: bool) -> i64 {
    /// Slack under the inline limit, in bytes.
    const MARGIN: i64 = 9;
    profile.page_size - profile.reserve(keyed) - 37 - MARGIN
}

/// Set [`fts_pgsz_for`] on a freshly created `searchabletext`. Written
/// unconditionally, including when it lands on FTS5's own default: the value
/// is the same either way, and one code path is worth more than a `pgsz` row
/// saved.
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

    /// Every profile that can write has to bound the log, and all three have to
    /// bound it at the same figure — which is [`crate::config::MINIMUM_WAL_SIZE`],
    /// spelled as a literal in each because a pragma string cannot interpolate.
    /// The read-only profiles must *not* carry it: `journal_size_limit` is a
    /// write to the file, and a read-only connection cannot make one.
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
    /// must not grow with one — the whole reason it is a separate constant.
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

    /// Between the clamps it has to actually track the corpus; a constant
    /// would satisfy every other test here.
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
    /// it found for each. Under the knee is the 2.4-4x regime this exists to
    /// avoid, so the recommendation has to reach it.
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

    /// An explicit setting has to be able to exceed the automatic cap: past
    /// ~800k files the derived value is capped *below* the measured knee, and
    /// the override is the only way to reach it.
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
