//! Scratch directories for tests. Public and `#[doc(hidden)]` rather than
//! `#[cfg(test)]`: the `tests/` binaries and the GUI crate are separate
//! compilation units, so a test-gated item would be invisible to them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The compressed body [`crate::db::repo::set_content_done`] wants.
pub fn zstd_of(text: &str) -> Option<Vec<u8>> {
    crate::db::repo::encode_one(text, true).expect("zstd encode")
}

/// Old enough that a failure investigated the next morning has its tree.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Whether `name` is one of [`scratch_dir`]'s own directories. Matched on the
/// *shape* — `quicksearch-{tag}-{pid}-{seq}` — not the prefix alone:
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
/// the way *in* keeps this run's evidence and yesterday's while nothing
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
/// Never cleaned up: a failing test's tree is most of the evidence. Long-dead
/// runs' trees are swept once per process instead — see [`sweep_stale`].
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
/// `to_string_lossy` yields exactly `{stem}\u{FFFD}{suffix}` — itself a name
/// a *different* file can really have, the collision the walk and watcher
/// screens prevent; pair with [`lossy_twin`] to reproduce it. Some
/// filesystems (FAT, exFAT) refuse the name — tests must tolerate that.
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

/// The genuinely-representable name [`unrepresentable_name`] collapses to
/// under `to_string_lossy`.
#[doc(hidden)]
pub fn lossy_twin(stem: &str, suffix: &str) -> String {
    format!("{}\u{FFFD}{}", stem, suffix)
}

/// Power-of-two bucket, so memory-map sizes group by what allocated them.
#[doc(hidden)]
pub fn size_class(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib < 1.0 {
        "< 1 MiB".to_string()
    } else {
        let bucket = 1u64 << (63 - (bytes / (1024 * 1024)).leading_zeros() as u64);
        format!("~{} MiB", bucket)
    }
}

#[doc(hidden)]
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// A scratch directory that removes itself on drop — **unless the thread is
/// panicking**, keeping [`scratch_dir`]'s policy that a failing test's tree
/// is the evidence.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn dir(tag: &str) -> Scratch {
        Scratch(scratch_dir(tag))
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

/// Deterministic word picker — a fixed seed makes two runs comparable.
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

/// A term planted only in document *bodies*, never in a file name — forces
/// the full-text pass to do real work: the filename `LIKE` finds nothing, and
/// every trigram candidate has to be decompressed and verified.
pub const BODY_TERM: &str = "chalcedony";

/// Filler vocabulary for seeded indexes. **Deliberately shares no trigram
/// with [`NEEDLE`]**: with a vocabulary that merely resembled the needle,
/// every query would fill the display limit in the first few hundred rows,
/// the cascade would break out of pass A, and passes B–D would never run —
/// while the harness looked perfectly healthy.
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
    /// Path segments in each stored `parent`. `1` is `/seed/NNN/` — short, and
    /// what the search harnesses have always used. A real tree is nested, and
    /// `parent` is stored per row, so this is most of what decides the width
    /// of a `files` row and therefore how much cache a scan of it needs. Raise
    /// it when calibrating anything against real-world row size.
    pub dir_depth: usize,
    /// File names carrying [`NEEDLE`]. Kept far below any sane display limit
    /// — an early-exiting query measures how fast the cascade gives up.
    pub needle_names: usize,
    /// Document bodies carrying [`NEEDLE`], on top of the names.
    pub needle_docs: usize,
    /// Document bodies carrying [`BODY_TERM`]; sized by the caller to stay
    /// under the display limit.
    pub body_term_docs: usize,
    /// One row in every `dup_every` repeats its predecessor's content hash,
    /// giving [`crate::search::find_duplicate_groups`] real groups to rank.
    /// `0` leaves every hash NULL — the shape the search harnesses seed, and
    /// the one whose row width their numbers were taken against.
    pub dup_every: usize,
    /// Commit every N files instead of wrapping the whole seed in one
    /// transaction (`0`). Each commit flushes FTS5's in-memory hash to its own
    /// segment, so this is what gives a later `merge` real work — the shape a
    /// production run has, where the writer commits in slices. Harnesses that
    /// only want rows as fast as possible leave it at `0`.
    pub commit_every: usize,
    /// Build the index at this database page size instead of
    /// [`crate::db::schema::PAGE_SIZE`]. Installed as a process-global
    /// override for the whole seed *and left installed*, because a keyed file
    /// cannot be reopened without it — see
    /// [`crate::db::set_page_size_override`].
    pub page_size: Option<i64>,
    /// Override FTS5's `pgsz` before a single row is written. `None` keeps
    /// whatever the schema chose for this key state, which is what every
    /// harness measuring the *product* wants. It exists so a benchmark can
    /// pin FTS5's default 4050 on a keyed index and price
    /// [`crate::db::schema::fts_pgsz_for`] against it in one process on
    /// one corpus.
    pub pgsz: Option<i64>,
    /// Build the index under this per-page authenticator instead of
    /// [`crate::db::schema::HMAC_MODE`]. A process-global override for the
    /// same reason `page_size` is one — it sets the page reserve, so a keyed
    /// file cannot be reopened without it. Ignored on a plain arm, which has
    /// no reserve.
    pub hmac: Option<crate::db::schema::HmacMode>,
    /// `(extension, mime)` pairs cycled across the rows, deciding what a
    /// `content_extensions` filter can select. [`EXT_PLAIN`] — one pair, so
    /// every row is `.txt`/`text/plain` — is what every harness measuring
    /// search or indexing wants, and it is the default so their corpora are
    /// byte-identical to what they have always been.
    ///
    /// It exists for `contentprobe`, where a filter that either takes the
    /// whole index or none of it answers nothing. **A mix whose length shares
    /// a factor with `content_every` puts every document behind the same few
    /// extensions** — the degenerate-corpus trap `pruneprobe` documents
    /// against its own strides — so a harness using this should assert the
    /// fractions it ends up with rather than trusting the arithmetic.
    pub ext_mix: &'static [(&'static str, &'static str)],
    /// One row in every `pending_every` that *would* hold content is left in
    /// the pending queue instead — born `STATE_PENDING` and never extracted,
    /// which is the residue an interrupted content pass leaves behind. `0`
    /// (the default) seeds none.
    ///
    /// It exists because such a row is the case a re-decision can skip the
    /// most work on: it is neither `STATE_NA` (so a narrowed filter must still
    /// flip it) nor `STATE_DONE` (so it has no posting and no stored text to
    /// clear). A corpus without any cannot tell whether clearing content for
    /// rows that cannot hold it costs anything.
    pub pending_every: usize,
}

/// The single-extension corpus every harness but `contentprobe` seeds.
pub const EXT_PLAIN: &[(&str, &str)] = &[("txt", "text/plain")];

impl Default for SeedSpec {
    fn default() -> SeedSpec {
        SeedSpec {
            files: 50_000,
            content_every: 10,
            // ~2 KB per document: a corpus of tiny documents makes the fuzzy
            // full-text pass look free when it is the cascade's most expensive.
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
    use crate::db::repo::{insert_file, set_content_done, NewFile};
    use crate::mime::mime_to_type;

    // Before the open, not after: the profile decides how the file is
    // *created*, and on a keyed file it decides whether it can be read at all.
    if let Some(page_size) = spec.page_size {
        crate::db::set_page_size_override(page_size);
    }
    if let Some(hmac) = spec.hmac {
        crate::db::set_hmac_mode_override(hmac);
    }
    let conn = crate::db::open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
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
    // A row gets content only if an extractor would have claimed its MIME, so
    // the seeded `content_state` is what a real run under an *unfiltered*
    // config would have left. Without this a corpus of mixed extensions is
    // born disagreeing with its own configuration, and the first reconcile
    // against it spends its time repairing the seed rather than applying the
    // edit. `EXT_PLAIN` is claimed by the plaintext extractor, so the
    // single-extension corpus every other harness seeds is unchanged.
    let registry = crate::extract::Registry::default_set();
    // Spacing, not a random draw: a cluster at the front would let a pass
    // stop early and report a fraction of the work a real rare query costs.
    let name_stride = spec.files / spec.needle_names.max(1);
    let doc_stride = spec.files / spec.needle_docs.max(1);
    let body_stride = spec.files / spec.body_term_docs.max(1);
    // `unchecked_transaction` borrows the connection shared, which is what
    // lets `commit_every` end one and start the next inside the loop.
    let mut tx = conn.unchecked_transaction().unwrap();
    for i in 0..spec.files {
        let w1 = rng.pick(WORDS);
        let w2 = rng.pick(WORDS);
        // Extension and MIME move together: a row whose name says `.pdf` and
        // whose MIME says `text/plain` would let `content_extractable`'s two
        // halves disagree, which is exactly what a content filter is testing.
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
        // groups are pairs of equal-sized rows — the shape `find_duplicate_groups`
        // prices, since a hash covers the size.
        let hash = (spec.dup_every > 0).then(|| {
            let group = if i % spec.dup_every == spec.dup_every - 1 {
                i.saturating_sub(1)
            } else {
                i
            };
            let mut bytes = [0u8; 32];
            for (b, slot) in bytes.iter_mut().enumerate() {
                *slot = ((group as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (b % 8 * 8)) as u8;
            }
            bytes
        });
        // `needs_content` is what the row is *born* as — `insert_file` gives it
        // `STATE_PENDING`. Skipping the `set_content_done` below is therefore
        // all it takes to leave one behind in the queue.
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
            set_content_done(&tx, id, &body, zstd_of(&body).as_deref()).unwrap();
        }
        if spec.commit_every > 0 && (i + 1) % spec.commit_every == 0 {
            tx.commit().unwrap();
            tx = conn.unchecked_transaction().unwrap();
        }
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
}

/// A raw 32-byte key for the measurement harnesses, deliberately **not** an
/// Argon2id derivation: the KDF costs half a second in release and minutes in
/// debug, and proves nothing about page work. It reaches SQLCipher as raw hex
/// either way (see `db::open::key_and_probe`), so a keyed arm measures what a
/// real unlocked index does.
pub const MEASUREMENT_KEY_HEX: &str =
    "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

pub fn measurement_key() -> crate::security::IndexKey {
    crate::security::IndexKey::from_hex(MEASUREMENT_KEY_HEX).expect("a 64-hex-digit key")
}

/// `(hits, misses)` in this connection's page cache since it was opened.
///
/// A *miss* is the unit that costs money on a keyed index: the page has to be
/// read and AES-CBC decrypted before a single row can be read out of it, where
/// a hit is a pointer into memory SQLite already holds. So
/// counting misses per query shape attributes cost to the table that caused
/// it, which timing alone cannot do.
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

/// FTS5's own default page size, which a keyed index used to inherit. Pinned
/// explicitly on the "before" arms of [`seed_arms`] so the cost of that
/// inheritance is priced in the same run as the fix, not remembered from
/// another one.
pub use crate::db::schema::FTS5_DEFAULT_PGSZ;

/// Indices into [`seed_arms`]'s fixed order. The two `_4050` arms exist only
/// to price [`crate::db::schema::FTS_PGSZ_ENCRYPTED`] against what came
/// before; the other two are the shipped product.
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
/// gates four of them on size, `benches/page_geometry.rs` sweeps page sizes
/// across them. Defined here, once, so the harnesses report on the same shape.
pub struct Arm {
    pub what: String,
    pub keyed: bool,
    /// `None` takes whatever the schema chose for this key state — the
    /// shipped behaviour. `Some` pins a value, only ever used to reproduce
    /// the old geometry.
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
    /// (rows, zstd bodies, FTS postings), which is the half a page geometry
    /// can change. The walk and the extractors are not in it.
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

    /// Run `f` with this arm's key *and profile* installed process-wide, then
    /// restore the shipped ones. Every open has to be wrapped: all three are
    /// process-globals, and an index seeded under them and opened without them
    /// fails as a wrong-password error rather than quietly.
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

    /// Bytes `dbstat` attributes to one table. The number to compare a cache
    /// ceiling against: `files` is what every keystroke rescans, so whether it
    /// fits is what decides if a typing session stays warm.
    pub fn table_bytes(&self, table: &str) -> u64 {
        let conn = self.open_search();
        conn.query_row(
            "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat WHERE name = ?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as u64
    }

    /// `(leaf, overflow)` pages in `searchabletext_data`. The overflow count
    /// is the whole diagnosis: SQLCipher's page reserve drops the inline
    /// payload limit below what a leaf built for another profile assumes, and
    /// each miss costs a second page — a second fetch and decrypt on every
    /// read of it. See `db::schema::fts_pgsz_for`, which is what keeps the
    /// count at zero.
    pub fn fts_pages(&self) -> (i64, i64) {
        let conn = self.open_search();
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
}

/// Seed the same corpus four times: plain and keyed, each on FTS5's default
/// page size and on whatever the schema picks. Identical content and identical
/// insertion order throughout, so arms differ *only* in those two variables —
/// which is what lets a display-limited query be compared at all (the cascade
/// stops when the limit fills, so a different rowid order would decide the
/// answer rather than the encryption).
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

    /// The premise every collision test rests on, pinned per platform: if
    /// this ever stops holding, those tests silently assert nothing.
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
    /// the whole safety argument.
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
