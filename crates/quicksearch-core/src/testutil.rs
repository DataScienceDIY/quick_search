//! Scratch directories for tests. Public and `#[doc(hidden)]` rather than
//! `#[cfg(test)]`: the `tests/` binaries and the GUI crate are separate
//! compilation units, so a test-gated item would be invisible to them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The compressed body [`crate::db::repo::store_pending_text`] wants.
pub fn zstd_of(text: &str) -> Option<Vec<u8>> {
    crate::db::repo::encode_one(text).expect("zstd encode")
}

/// Old enough that a failure investigated the next morning has its tree.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Whether `name` is one of [`scratch_dir`]'s own directories, matched on the
/// shape `quicksearch-{tag}-{pid}-{seq}` rather than the prefix alone:
/// `packaging/capture.sh` keeps its output in `quicksearch-capture` in the
/// same directory, and a prefix match would eat a capture run's screenshots.
fn is_scratch_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("quicksearch-") else {
        return false;
    };
    let numeric = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut tail = rest.rsplitn(3, '-');
    // seq, then pid, and a tag must remain in front of them.
    numeric(tail.next()) && numeric(tail.next()) && tail.next().is_some_and(|tag| !tag.is_empty())
}

/// Remove scratch directories left by runs that are long over. Sweeping on
/// the way in keeps this run's evidence and yesterday's while nothing
/// accumulates without bound.
fn sweep_stale() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_scratch_name) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if stale {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}

/// A fresh, empty directory under the system temp dir, named for `tag`.
/// Never cleaned up, because a failing test's tree is most of the evidence;
/// long-dead runs' trees are swept once per process instead (see
/// [`sweep_stale`]).
#[doc(hidden)]
pub fn scratch_dir(tag: &str) -> PathBuf {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(sweep_stale);

    let mut p = std::env::temp_dir();
    p.push(format!(
        "quicksearch-{}-{}-{}",
        tag,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).expect("create scratch dir");
    p
}

/// [`scratch_dir`] canonicalized: on macOS `/tmp` is a symlink to
/// `/private/tmp`, so an uncanonicalized root and a walked path disagree.
#[doc(hidden)]
pub fn scratch_dir_canonical(tag: &str) -> PathBuf {
    std::fs::canonicalize(scratch_dir(tag)).expect("canonicalize scratch dir")
}

/// Write `body` to `path`, creating parent directories as needed.
#[doc(hidden)]
pub fn touch(path: &std::path::Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, body).expect("write file");
}

/// A filename that is legal on disk but cannot round-trip through the index:
/// `to_string_lossy` yields exactly `{stem}\u{FFFD}{suffix}`, itself a name a
/// different file can really have. That is the collision the walk and watcher
/// screens prevent; pair with [`lossy_twin`] to reproduce it. Some
/// filesystems (FAT, exFAT) refuse the name, and tests must tolerate that.
#[doc(hidden)]
pub fn unrepresentable_name(stem: &str, suffix: &str) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = stem.as_bytes().to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(suffix.as_bytes());
        std::ffi::OsString::from_vec(bytes)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut units: Vec<u16> = stem.encode_utf16().collect();
        // A high surrogate with nothing after it to pair with.
        units.push(0xD800);
        units.extend(suffix.encode_utf16());
        std::ffi::OsString::from_wide(&units)
    }
}

/// The representable name [`unrepresentable_name`] collapses to under
/// `to_string_lossy`.
#[doc(hidden)]
pub fn lossy_twin(stem: &str, suffix: &str) -> String {
    format!("{}\u{FFFD}{}", stem, suffix)
}

/// Power-of-two bucket, so memory-map sizes group by what allocated them.
#[doc(hidden)]
pub fn size_class(bytes: u64) -> String {
    match bytes / (1024 * 1024) {
        0 => "< 1 MiB".to_string(),
        whole => format!("~{} MiB", 1u64 << whole.ilog2()),
    }
}

#[doc(hidden)]
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// A scratch directory that removes itself on drop, unless the thread is
/// panicking: [`scratch_dir`]'s policy that a failing test's tree is the
/// evidence still holds.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn dir(tag: &str) -> Scratch {
        Scratch(scratch_dir(tag))
    }

    /// [`Scratch::dir`], canonicalized; see [`scratch_dir_canonical`].
    pub fn canonical(tag: &str) -> Scratch {
        Scratch(scratch_dir_canonical(tag))
    }

    /// A scratch database path; the guard owns the directory, so SQLite's
    /// sidecars are removed with it.
    pub fn db(tag: &str) -> (Scratch, PathBuf) {
        let dir = Scratch::dir(tag);
        let db = dir.join("index.sqlite");
        (dir, db)
    }
}

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for Scratch {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Seeded synthetic corpora, shared by tests/, benches/ and examples/.
// ---------------------------------------------------------------------------

/// Deterministic word picker: a fixed seed makes two runs comparable.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    /// Not `next`: an inherent method by that name reads as `Iterator`'s, and
    /// this one is infinite and returns a bare `u64` rather than an `Option`.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }

    pub fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.next_u64() as usize % from.len()]
    }
}

/// A scratch database path under a fresh directory.
pub fn scratch_db(tag: &str) -> PathBuf {
    scratch_dir(tag).join("index.sqlite")
}

/// The rare term a seeded index is searched for. Nine bytes: clears the
/// trigram floor, and sits exactly on the boundary where a
/// `fuzzy_max_edits = 2` pigeonhole split into three-char chunks becomes legal.
pub const NEEDLE: &str = "quartzite";

/// A term planted only in document bodies, never in a file name, so the
/// full-text pass does real work: the filename `LIKE` finds nothing, and every
/// trigram candidate is decompressed and verified.
pub const BODY_TERM: &str = "chalcedony";

/// Filler vocabulary for seeded indexes. It shares no trigram with
/// [`NEEDLE`]: with a vocabulary that merely resembled the needle, every query
/// would fill the display limit in the first few hundred rows, the cascade
/// would stop after pass A, and passes B–D would never run while the harness
/// looked healthy.
pub const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "brown", "fox", "jumps", "lazy", "index", "search", "cascade", "snippet", "document",
    "content", "extract", "summary", "meeting", "invoice", "contract", "budget", "revenue",
    "planning", "review", "draft", "final", "notes", "appendix", "figure",
];

/// What [`seed_index`] should build.
#[derive(Clone, Copy)]
pub struct SeedSpec {
    pub files: usize,
    /// One file in every `content_every` gets extracted text.
    pub content_every: usize,
    /// Words in each stored document body.
    pub body_words: usize,
    /// Directories to spread the rows across.
    pub dirs: usize,
    /// Path segments in each stored `parent`. `1` is the short `/seed/NNN/`
    /// the search harnesses use. `parent` is stored per row, so this mostly
    /// decides the width of a `files` row and how much cache a scan of it
    /// needs; raise it when calibrating against real-world row size.
    pub dir_depth: usize,
    /// File names carrying [`NEEDLE`]. Kept far below any sane display limit:
    /// an early-exiting query measures how fast the cascade gives up.
    pub needle_names: usize,
    /// Document bodies carrying [`NEEDLE`], on top of the names.
    pub needle_docs: usize,
    /// Document bodies carrying [`BODY_TERM`]; sized by the caller to stay
    /// under the display limit.
    pub body_term_docs: usize,
    /// One row in every `dup_every` repeats its predecessor's content hash,
    /// giving [`crate::search::find_duplicate_groups`] real groups to rank.
    /// `0` leaves every hash NULL, the shape the search harnesses seed and
    /// measured their row width against.
    pub dup_every: usize,
    /// Commit every N files instead of wrapping the whole seed in one
    /// transaction (`0`). Each commit flushes FTS5's in-memory hash to its own
    /// segment, which gives a later `merge` real work, as in a production run
    /// whose writer commits in slices. Harnesses that only want rows fast
    /// leave it at `0`.
    pub commit_every: usize,
    /// Build the index at this database page size instead of
    /// [`crate::db::schema::PAGE_SIZE`]. Installed as a process-global
    /// override for the whole seed and left installed, because a keyed file
    /// cannot be reopened without it (see
    /// [`crate::db::set_page_size_override`]).
    pub page_size: Option<i64>,
    /// Override FTS5's `pgsz` before a single row is written. `None` keeps the
    /// schema's choice for this key state, as a harness measuring the product
    /// wants; `Some` lets a benchmark pin FTS5's default 4050 and price
    /// [`crate::db::schema::fts_pgsz_for`] against it on one corpus.
    pub pgsz: Option<i64>,
    /// Build the index under this per-page authenticator instead of
    /// [`crate::db::schema::HMAC_MODE`]. A process-global override like
    /// `page_size`: it sets the page reserve, so a keyed file cannot be
    /// reopened without it. Ignored on a plain arm, which has no reserve.
    pub hmac: Option<crate::db::schema::HmacMode>,
    /// `(extension, mime)` pairs cycled across the rows, deciding what a
    /// `content_extensions` filter can select. The default, [`EXT_PLAIN`],
    /// makes every row `.txt`/`text/plain`, as the search and indexing
    /// harnesses want.
    ///
    /// For `contentprobe`, where a filter taking all of the index or none of
    /// it answers nothing. A mix whose length shares a factor with
    /// `content_every` puts every document behind the same few extensions
    /// (the degenerate-corpus trap `pruneprobe` documents against its own
    /// strides), so a harness using it should assert the fractions it gets.
    pub ext_mix: &'static [(&'static str, &'static str)],
    /// One row in every `pending_every` that would hold content is left
    /// pending instead: born `STATE_PENDING` and never extracted, the residue
    /// of an interrupted content pass. `0` (the default) seeds none.
    ///
    /// Such a row is where a re-decision can skip the most work: it is
    /// neither `STATE_NA` (a narrowed filter must still flip it) nor
    /// `STATE_DONE` (it has no posting or stored text to clear). Without any,
    /// a corpus cannot show whether clearing rows that hold no content costs
    /// anything.
    pub pending_every: usize,
}

/// The single-extension corpus every harness but `contentprobe` seeds.
pub const EXT_PLAIN: &[(&str, &str)] = &[("txt", "text/plain")];

impl Default for SeedSpec {
    fn default() -> SeedSpec {
        SeedSpec {
            files: 50_000,
            content_every: 10,
            // ~2 KB per document: tiny documents would make the fuzzy
            // full-text pass, the cascade's most expensive, look free.
            body_words: 300,
            dirs: 500,
            dir_depth: 1,
            needle_names: 50,
            needle_docs: 50,
            body_term_docs: 500,
            dup_every: 0,
            commit_every: 0,
            page_size: None,
            pgsz: None,
            hmac: None,
            ext_mix: EXT_PLAIN,
            pending_every: 0,
        }
    }
}

/// Seed an index with synthetic rows, in one transaction. Shared by the
/// measurement harnesses so they all describe the same corpus.
pub fn seed_index(path: &std::path::Path, spec: &SeedSpec) {
    use crate::db::repo::{insert_file, store_pending_text, NewFile};
    use crate::mime::mime_to_type;

    // Before the open: the profile decides how the file is created, and on a
    // keyed file whether it can be read at all.
    if let Some(page_size) = spec.page_size {
        crate::db::set_page_size_override(page_size);
    }
    if let Some(hmac) = spec.hmac {
        crate::db::set_hmac_mode_override(hmac);
    }
    let conn = crate::db::open_or_recreate(path.to_str().unwrap(), &Default::default()).unwrap();
    // Before the first row: `pgsz` decides how leaves are built, so setting it
    // afterwards would only affect segments merged later.
    if let Some(pgsz) = spec.pgsz {
        conn.execute(
            "INSERT INTO searchabletext(searchabletext, rank) VALUES('pgsz', ?1)",
            [pgsz],
        )
        .unwrap();
    }
    let mut rng = Lcg::new(0x5eed);
    let ext_mix = if spec.ext_mix.is_empty() {
        EXT_PLAIN
    } else {
        spec.ext_mix
    };
    // A row gets content only if an extractor would claim its MIME, so the
    // seeded `content_state` is what a real run under an unfiltered config
    // leaves. Otherwise a mixed corpus would disagree with its own
    // configuration, and the first reconcile would spend its time repairing
    // the seed rather than applying the edit.
    let registry = crate::extract::Registry::default_set();
    // Spacing, not a random draw: a cluster at the front would let a pass
    // stop early and report a fraction of the work a real rare query costs.
    let name_stride = spec.files / spec.needle_names.max(1);
    let doc_stride = spec.files / spec.needle_docs.max(1);
    let body_stride = spec.files / spec.body_term_docs.max(1);
    // `unchecked_transaction` borrows the connection shared, so
    // `commit_every` can end one and start the next inside the loop.
    let mut tx = conn.unchecked_transaction().unwrap();
    for i in 0..spec.files {
        let w1 = rng.pick(WORDS);
        let w2 = rng.pick(WORDS);
        // Extension and MIME move together: a row named `.pdf` with MIME
        // `text/plain` would let `content_extractable`'s two halves disagree,
        // and a content filter tests exactly that agreement.
        let (ext, mime) = ext_mix[i % ext_mix.len()];
        let name = if spec.needle_names > 0 && i % name_stride.max(1) == 0 {
            format!("{}-{}-{:07}.{}", w1, NEEDLE, i, ext)
        } else {
            format!("{}-{}-{:07}.{}", w1, w2, i, ext)
        };
        // Stored parents always end in a separator; see `dir_to_db_parent`.
        // Deeper segments are derived from the directory index, not the file
        // index, so files continue to share parents the way a real tree does.
        let d = i % spec.dirs.max(1);
        let mut dir = format!("/seed/{:03}", d);
        for segment in 1..spec.dir_depth.max(1) {
            dir.push('/');
            dir.push_str(WORDS[(d * 7 + segment * 13) % WORDS.len()]);
        }
        dir.push('/');
        // Every `dup_every`-th row takes the hash of the one before it, so the
        // groups are pairs of equal-sized rows, the shape
        // `find_duplicate_groups` prices, since a hash covers the size.
        let hash = (spec.dup_every > 0).then(|| {
            let group = if i % spec.dup_every == spec.dup_every - 1 {
                i.saturating_sub(1)
            } else {
                i
            };
            std::array::from_fn::<u8, 32, _>(|b| {
                ((group as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (b % 8 * 8)) as u8
            })
        });
        // `needs_content` rows are born `STATE_PENDING`, so skipping the
        // `store_pending_text` below leaves one in the queue.
        let needs_content = i % spec.content_every.max(1) == 0 && registry.supports(mime);
        let extracted = needs_content && (spec.pending_every == 0 || i % spec.pending_every != 0);
        let id = insert_file(
            &tx,
            &NewFile {
                name: &name,
                parent: &dir,
                size: 4096,
                mtime: 1_700_000_000 + i as u64,
                mime: Some(mime),
                ftype: mime_to_type(mime),
                hash: hash.as_ref().map(|h| h.as_slice()),
                needs_content,
            },
        )
        .unwrap()
        .expect("unique path");
        if extracted {
            let mut body: Vec<&str> = (0..spec.body_words).map(|_| *rng.pick(WORDS)).collect();
            if spec.needle_docs > 0 && i % doc_stride.max(1) == 0 {
                // Mid-body, so a snippet window has to be cut around it.
                body[spec.body_words / 2] = NEEDLE;
            }
            if spec.body_term_docs > 0 && i % body_stride.max(1) == 0 {
                // Two thirds in, so verifying it scans most of the document.
                body[spec.body_words * 2 / 3] = BODY_TERM;
            }
            let body = body.join(" ");
            store_pending_text(&tx, id, &body, zstd_of(&body).as_deref()).unwrap();
        }
        if spec.commit_every > 0 && (i + 1) % spec.commit_every == 0 {
            tx.commit().unwrap();
            tx = conn.unchecked_transaction().unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
}

/// A raw 32-byte key for the measurement harnesses. Not an Argon2id
/// derivation: the KDF costs half a second in release and minutes in debug,
/// and proves nothing about page work. Both forms reach SQLCipher as raw hex
/// (see `db::open::key_and_probe`), so a keyed arm measures a real unlocked
/// index.
const MEASUREMENT_KEY_HEX: &str =
    "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

pub fn measurement_key() -> crate::security::IndexKey {
    crate::security::IndexKey::from_hex(MEASUREMENT_KEY_HEX).expect("a 64-hex-digit key")
}

/// Run one cascade query to completion on `conn`; returns the time the
/// cascade took (the split is not timed) and how many hits it streamed.
pub fn run_cascade(
    conn: &rusqlite::Connection,
    query: &str,
    options: &crate::search::SearchOptions,
) -> (std::time::Duration, usize) {
    let split = crate::query::split_for_cascade(query).expect("query parses");
    let latest = std::sync::atomic::AtomicU64::new(1);
    let mut hits = 0usize;
    let mut sink = |batch: Vec<crate::search::SearchHit>| hits += batch.len();
    let start = std::time::Instant::now();
    crate::search::cascade::run(conn, &split, options, 1, &latest, &mut sink)
        .expect("cascade runs");
    (start.elapsed(), hits)
}

/// The MIME the walk would record for `path`, sniffed from its first
/// `hash_length` bytes.
pub fn sniff(path: &std::path::Path, hash_length: usize) -> Option<&'static str> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; hash_length];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    crate::mime::guess_mime_from_head(path, &head)
}

/// The rows the full-text index matches for `term`, asked for as a search
/// would ask ([`crate::search::prefilter::fts_literal`], by the layout the
/// index records): a bare `MATCH 'word'` is a phrase of several trigrams,
/// which an index without positions refuses. Unverified, so on such an index
/// these are candidates, a superset of the documents containing `term`.
pub fn fts_rowids(conn: &rusqlite::Connection, term: &str) -> Vec<i64> {
    let expr = crate::search::prefilter::fts_literal(term, crate::db::repo::fts_positions(conn))
        .expect("a term of three characters or more");
    let mut stmt = conn
        .prepare("SELECT rowid FROM searchabletext WHERE searchabletext MATCH ?1")
        .expect("prepare fts query");
    let rows = stmt
        .query_map([expr], |r| r.get(0))
        .expect("fts query")
        .collect::<Result<Vec<i64>, _>>()
        .expect("fts rows");
    rows
}

/// How many rows [`fts_rowids`] finds.
pub fn fts_count(conn: &rusqlite::Connection, term: &str) -> i64 {
    fts_rowids(conn, term).len() as i64
}

/// `(leaf, overflow)` pages in `searchabletext_data`, from `dbstat`. An
/// overflow page is a second fetch and decrypt on every read of its leaf;
/// [`crate::db::schema::fts_pgsz_for`] keeps the count at zero.
pub fn fts_page_counts(conn: &rusqlite::Connection) -> (i64, i64) {
    let count = |pagetype: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM dbstat \
             WHERE name = 'searchabletext_data' AND pagetype = ?1",
            [pagetype],
            |r| r.get(0),
        )
        .expect("dbstat")
    };
    (count("leaf"), count("overflow"))
}

/// `(hits, misses)` in this connection's page cache since it was opened.
///
/// A miss costs money on a keyed index: the page is read and AES-CBC
/// decrypted before a row can come out of it, where a hit is a pointer into
/// memory. Counting misses per query shape attributes cost to the table that
/// caused it, which timing alone cannot do.
///
/// `sqlite3_db_status` has no safe wrapper in rusqlite; the raw binding and
/// `Connection::handle` are both public, and neither the pointer nor the
/// out-params outlive this call.
pub fn cache_stats(conn: &rusqlite::Connection) -> (i64, i64) {
    use rusqlite::ffi;
    let mut hits = (0i32, 0i32);
    let mut misses = (0i32, 0i32);
    unsafe {
        let handle = conn.handle();
        ffi::sqlite3_db_status(
            handle,
            ffi::SQLITE_DBSTATUS_CACHE_HIT,
            &mut hits.0,
            &mut hits.1,
            0,
        );
        ffi::sqlite3_db_status(
            handle,
            ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut misses.0,
            &mut misses.1,
            0,
        );
    }
    (hits.0 as i64, misses.0 as i64)
}

/// FTS5's own default `pgsz`, pinned on the `_4050` arms of [`seed_arms`] so
/// its cost is priced in the same run as the derived value.
use crate::db::schema::FTS5_DEFAULT_PGSZ;

/// Indices into [`seed_arms`]'s fixed order. The two `_4050` arms exist only
/// to price [`crate::db::schema::fts_pgsz_for`] against FTS5's default; the
/// other two are the shipped product.
pub const ARM_PLAIN_4050: usize = 0;
pub const ARM_PLAIN: usize = 1;
pub const ARM_KEYED_4050: usize = 2;
pub const ARM_KEYED: usize = 3;

/// `(label, keyed, pgsz, path suffix)`, in [`seed_arms`] order.
const ARM_SHAPES: [(&str, bool, Option<i64>, &str); 4] = [
    (
        "plain, pgsz 4050",
        false,
        Some(FTS5_DEFAULT_PGSZ),
        "plain-4050",
    ),
    ("plain, as shipped", false, None, "plain"),
    (
        "keyed, pgsz 4050",
        true,
        Some(FTS5_DEFAULT_PGSZ),
        "keyed-4050",
    ),
    ("keyed, as shipped", true, None, "keyed"),
];

/// One seeded index in a plain-vs-keyed comparison: `tests/encrypted_perf.rs`
/// gates four of them on size, and `benches/page_geometry.rs` sweeps page
/// sizes across them. Defined once so the harnesses report on the same shape.
pub struct Arm {
    pub what: String,
    pub keyed: bool,
    /// `None` takes the schema's choice for this key state, as shipped. `Some`
    /// pins a value, to price FTS5's default against it.
    pub pgsz: Option<i64>,
    /// The database page size this arm was built at, and the one every open
    /// of it must re-install: a keyed file's header is ciphertext, so it
    /// cannot be read back off the file.
    pub page_size: Option<i64>,
    /// The per-page authenticator this arm was built under, re-installed on
    /// every open for the same reason `page_size` is: it sets the page
    /// reserve, which the header cannot be read without.
    pub hmac: Option<crate::db::schema::HmacMode>,
    pub path: PathBuf,
    /// How long seeding spent writing it: the database-write half of indexing
    /// (rows, zstd bodies, FTS postings), the half a page geometry can change.
    /// The walk and the extractors are not in it.
    pub seeded_in: std::time::Duration,
}

impl Arm {
    /// Seed one arm from `spec` and time the write. `spec.page_size`,
    /// `spec.hmac` and `spec.pgsz` define the geometry; `tag` names its
    /// scratch directory.
    pub fn seed(what: impl Into<String>, tag: &str, keyed: bool, spec: &SeedSpec) -> Arm {
        let arm = Arm {
            what: what.into(),
            keyed,
            pgsz: spec.pgsz,
            page_size: spec.page_size,
            hmac: spec.hmac,
            path: scratch_db(tag),
            seeded_in: std::time::Duration::ZERO,
        };
        let path = arm.path.clone();
        let spec = *spec;
        let start = std::time::Instant::now();
        arm.with_key(|| seed_index(&path, &spec));
        Arm {
            seeded_in: start.elapsed(),
            ..arm
        }
    }

    /// Run `f` with this arm's key and profile installed process-wide, then
    /// restore the shipped ones. Every open must be wrapped: all three are
    /// process-globals, and an index seeded under them and opened without them
    /// fails as a wrong password.
    pub fn with_key<T>(&self, f: impl FnOnce() -> T) -> T {
        crate::db::set_process_key(self.keyed.then(measurement_key));
        crate::db::set_page_size_override(self.page_size.unwrap_or(crate::db::schema::PAGE_SIZE));
        crate::db::set_hmac_mode_override(self.hmac.unwrap_or(crate::db::schema::HMAC_MODE));
        let out = f();
        crate::db::set_process_key(None);
        crate::db::set_page_size_override(crate::db::schema::PAGE_SIZE);
        crate::db::set_hmac_mode_override(crate::db::schema::HMAC_MODE);
        out
    }

    /// Delete this arm's scratch directory. The sweep seeds a lot of large
    /// indexes; dropping each once measured keeps one resident at a time.
    pub fn discard(self) {
        if let Some(dir) = self.path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// A search connection on this arm, at the production pragma profile.
    pub fn open_search(&self) -> rusqlite::Connection {
        self.with_key(|| {
            crate::db::open::open_search_reader(&self.path.to_string_lossy()).expect("open arm")
        })
    }

    pub fn size_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Bytes `dbstat` attributes to one table: the number to compare a cache
    /// ceiling against. Every keystroke rescans `files`, so whether it fits
    /// decides if a typing session stays warm.
    pub fn table_bytes(&self, table: &str) -> u64 {
        let conn = self.open_search();
        conn.query_row(
            "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat WHERE name = ?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as u64
    }

    /// [`fts_page_counts`] for this arm.
    pub fn fts_pages(&self) -> (i64, i64) {
        fts_page_counts(&self.open_search())
    }
}

/// Seed the same corpus four times: plain and keyed, each on FTS5's default
/// `pgsz` and on the schema's. Content and insertion order are identical, so
/// arms differ only in those two variables; that lets a display-limited query
/// be compared at all, since the cascade stops when the limit fills and a
/// different rowid order would decide the answer instead of the encryption.
///
/// `spec.pgsz` is overridden per arm; everything else is the caller's.
pub fn seed_arms(tag: &str, spec: &SeedSpec) -> Vec<Arm> {
    ARM_SHAPES
        .iter()
        .map(|(what, keyed, pgsz, suffix)| {
            let spec = SeedSpec {
                pgsz: *pgsz,
                ..*spec
            };
            Arm::seed(*what, &format!("{}-{}", tag, suffix), *keyed, &spec)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_call_gets_its_own_empty_directory() {
        let a = scratch_dir("selftest");
        let b = scratch_dir("selftest");
        assert_ne!(a, b, "two calls must not collide");
        for d in [&a, &b] {
            assert!(d.is_dir());
            assert_eq!(std::fs::read_dir(d).unwrap().count(), 0, "starts empty");
        }
    }

    #[test]
    fn touch_creates_missing_parents() {
        let dir = scratch_dir("selftest-touch");
        let deep = dir.join("a/b/c.txt");
        touch(&deep, b"hi");
        assert_eq!(std::fs::read(&deep).unwrap(), b"hi");
    }

    /// The premise every collision test rests on, pinned per platform: if this
    /// stops holding, those tests assert nothing.
    #[test]
    fn the_unrepresentable_name_collapses_onto_its_twin() {
        let bad = unrepresentable_name("x", ".txt");
        assert!(
            bad.to_str().is_none(),
            "the name must not be representable: {:?}",
            bad
        );
        assert_eq!(
            bad.to_string_lossy(),
            lossy_twin("x", ".txt"),
            "the two names must collide under to_string_lossy"
        );
        assert!(
            std::path::Path::new(&lossy_twin("x", ".txt"))
                .as_os_str()
                .to_str()
                .is_some(),
            "the twin must itself be a perfectly ordinary name"
        );
    }

    /// The sweep runs against a shared temp directory, so what it matches is
    /// its whole safety argument.
    #[test]
    fn only_scratch_directories_are_swept() {
        for ours in [
            "quicksearch-coord-1234-0",
            "quicksearch-stall-heavy-1001402-7",
            "quicksearch-a-0-0",
            // Tags contain dashes of their own.
            "quicksearch-sniff-binary-db-2621744-1",
        ] {
            assert!(is_scratch_name(ours), "{ours} should be swept");
        }

        for theirs in [
            // The capture output directory, the reason this is a shape match.
            "quicksearch-capture",
            "quicksearch",
            "quicksearch-",
            "quicksearch-coord",
            "quicksearch-coord-1234",
            // Numbers, but nothing in front of them to be a tag.
            "quicksearch-1234-0",
            "cargo-install-abc-1-2",
            "tmp-quicksearch-coord-1-2",
        ] {
            assert!(!is_scratch_name(theirs), "{theirs} must not be swept");
        }
    }

    /// Fresh directories survive; only long-dead runs are collected.
    #[test]
    fn the_sweep_keeps_recent_trees_and_takes_old_ones() {
        let fresh = scratch_dir("sweep-fresh");
        touch(&fresh.join("evidence.txt"), b"kept");

        // Same shape, but back-dated past the threshold.
        let old = std::env::temp_dir().join(format!(
            "quicksearch-sweep-old-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&old).expect("create the aged directory");
        let long_ago =
            std::time::SystemTime::now() - STALE_AFTER - std::time::Duration::from_secs(60);
        std::fs::File::open(&old)
            .and_then(|d| {
                d.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(long_ago)
                        .set_modified(long_ago),
                )
            })
            .expect("back-date the aged directory");

        sweep_stale();

        assert!(fresh.exists(), "a fresh scratch tree was swept away");
        assert!(!old.exists(), "a long-dead scratch tree survived the sweep");

        std::fs::remove_dir_all(&fresh).ok();
    }
}
