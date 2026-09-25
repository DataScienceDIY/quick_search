//! Shared plumbing for the `examples/*probe.rs` binaries.
//!
//! A `mod.rs` one directory down is not an auto-discovered target, so this
//! compiles only as a module of whichever probe declares `mod common;`. None
//! of it ships.

// The unused half is not dead code; it is another binary's.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use quicksearch_core::db;
use quicksearch_core::testutil::{self, size_class, Arm};

pub use quicksearch_core::testutil::mib;

// ---------------------------------------------------------------------------
// The machine: page cache, I/O and memory
// ---------------------------------------------------------------------------

/// Drop a tree, and optionally an index, from the page cache; returns the
/// files and bytes evicted. Without this every read figure is `0.0 MiB` and a
/// traversal or read cost measures memory. `fadvise(DONTNEED)` needs no root
/// and evicts only this tree, but leaves the dentry and inode caches warm; on
/// virtiofs or NFS the host cache survives, so "cold" there is a lower bound.
#[cfg(unix)]
pub fn evict(tree: &Path, db: Option<&Path>) -> (usize, u64) {
    use std::os::unix::io::AsRawFd;

    // Dirty pages cannot be dropped; flush before asking.
    unsafe { libc::sync() };

    fn drop_one(path: &Path) -> Option<u64> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        // (0, 0) means "to end of file".
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        (rc == 0).then_some(len)
    }

    fn walk(dir: &Path, files: &mut usize, bytes: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => walk(&path, files, bytes),
                Ok(ft) if ft.is_file() => {
                    if let Some(len) = drop_one(&path) {
                        *files += 1;
                        *bytes += len;
                    }
                }
                _ => {}
            }
        }
    }

    let (mut files, mut bytes) = (0usize, 0u64);
    walk(tree, &mut files, &mut bytes);
    if let Some(db) = db {
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", db.display(), suffix));
            if let Some(len) = drop_one(&p) {
                files += 1;
                bytes += len;
            }
        }
    }
    (files, bytes)
}

/// No equivalent worth having off Unix.
#[cfg(not(unix))]
pub fn evict(_tree: &Path, _db: Option<&Path>) -> (usize, u64) {
    eprintln!("evict: not supported on this platform");
    (0, 0)
}

/// This process's I/O accounting, from `/proc/self/io`. `read_bytes` counts
/// what reached the block layer (the disk, not the page cache); `syscr`
/// separates "read a lot" from "read a little, many times". Filesystems that
/// do not report a field leave it zero; the caller says so.
#[derive(Default, Clone, Copy)]
pub struct Io {
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled: u64,
}

impl Io {
    pub fn read() -> Io {
        let mut io = Io::default();
        let Ok(text) = std::fs::read_to_string("/proc/self/io") else {
            return io;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<u64>() else {
                continue;
            };
            match key {
                "rchar" => io.rchar = value,
                "wchar" => io.wchar = value,
                "syscr" => io.syscr = value,
                "syscw" => io.syscw = value,
                "read_bytes" => io.read_bytes = value,
                "write_bytes" => io.write_bytes = value,
                "cancelled_write_bytes" => io.cancelled = value,
                _ => {}
            }
        }
        io
    }

    pub fn since(&self, start: &Io) -> Io {
        Io {
            rchar: self.rchar.saturating_sub(start.rchar),
            wchar: self.wchar.saturating_sub(start.wchar),
            syscr: self.syscr.saturating_sub(start.syscr),
            syscw: self.syscw.saturating_sub(start.syscw),
            read_bytes: self.read_bytes.saturating_sub(start.read_bytes),
            write_bytes: self.write_bytes.saturating_sub(start.write_bytes),
            cancelled: self.cancelled.saturating_sub(start.cancelled),
        }
    }
}

/// A size field of `/proc/self/status`, such as `"VmRSS:"`, in bytes; `None`
/// where `/proc` is absent.
fn status_bytes(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with(key))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Resident set size now, in bytes.
pub fn rss() -> Option<u64> {
    status_bytes("VmRSS:")
}

/// Peak RSS in bytes, from the kernel's `VmHWM`. Unlike a sampled figure it
/// cannot miss a spike.
pub fn vm_hwm() -> Option<u64> {
    status_bytes("VmHWM:")
}

/// One mapping in an `smaps` file.
pub struct Region {
    pub lo: u64,
    pub hi: u64,
    pub anonymous: bool,
    /// The mapped path. Anonymous mappings are named by size class, so a
    /// hundred 8 MiB regions read as one line.
    pub name: String,
    /// Resident bytes.
    pub rss: u64,
}

/// The mappings in `smaps` text, each with its resident bytes.
pub fn smaps_regions(smaps: &str) -> impl Iterator<Item = Region> + '_ {
    let mut current: Option<Region> = None;
    smaps.lines().filter_map(move |line| {
        if let Some(rss_kib) = line.strip_prefix("Rss:") {
            let kib: u64 = rss_kib
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let region = current.take()?;
            return Some(Region {
                rss: kib * 1024,
                ..region
            });
        }
        if let Some(region) = parse_map_header(line) {
            current = Some(region);
        }
        None
    })
}

/// Parse one `smaps` header line.
fn parse_map_header(line: &str) -> Option<Region> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let (lo, hi) = range.split_once('-')?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    // Fields 2-5 are perms, offset, dev, inode; anything after is the path.
    let path = fields.nth(4).unwrap_or("");
    Some(Region {
        lo,
        hi,
        anonymous: path.is_empty(),
        name: if path.is_empty() {
            format!("anon {}", size_class(hi.saturating_sub(lo)))
        } else {
            path.to_string()
        },
        rss: 0,
    })
}

/// `(name, resident bytes)` pairs summed by name, largest first; names
/// holding nothing are dropped.
pub fn resident_by_name(pairs: impl Iterator<Item = (String, u64)>) -> Vec<(String, u64)> {
    let mut by_name: HashMap<String, u64> = HashMap::new();
    for (name, bytes) in pairs {
        *by_name.entry(name).or_default() += bytes;
    }
    let mut entries: Vec<(String, u64)> = by_name.into_iter().filter(|(_, b)| *b > 0).collect();
    entries.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    entries
}

// ---------------------------------------------------------------------------
// Seeded-index probes: contentprobe and pruneprobe
// ---------------------------------------------------------------------------

/// Rows per page, matching `processing.batch_size`'s default: the figure
/// `scope::advance` runs at.
pub const PAGE: i64 = 500;

/// Ids per `IN (...)` list, matching `repo::DELETE_IDS_CHUNK`.
pub const CHUNK: usize = 512;

/// Output leaf pages per `'merge'` call, matching `FINALIZE_MERGE_PAGES`.
pub const MERGE_PAGES: i64 = 1000;

/// The page cache `PRAGMAS_INCREMENTAL` gives a reconcile, in MiB: the first
/// row of a cache sweep.
pub const SHIPPED_MIB: i64 = 4;

/// The key states `QSB_ARMS=plain|keyed|both` selects, as `keyed` flags.
/// Both by default: on a keyed index every page a write touches is decrypted
/// and re-encrypted, and the two arms can disagree (see
/// `db::schema::PAGE_SIZE`).
pub fn arms() -> Vec<bool> {
    match std::env::var("QSB_ARMS").as_deref() {
        Ok("plain") => vec![false],
        Ok("keyed") => vec![true],
        _ => vec![false, true],
    }
}

/// `key` from the environment as a number, or `default`.
pub fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// When a pass commits.
#[derive(Clone, Copy)]
pub enum Commit {
    /// One transaction per page.
    PerPage,
    /// One per `n` pages; the sweep's variable.
    Pages(usize),
    /// One per elapsed slice, `scope::advance`'s unit of interruptible work.
    Slice(Duration),
}

impl Commit {
    /// Whether the transaction open since `since`, holding `pages_open`
    /// pages, commits now.
    pub fn due(self, pages_open: usize, since: Instant) -> bool {
        match self {
            Commit::PerPage => true,
            Commit::Pages(n) => pages_open >= n,
            Commit::Slice(d) => since.elapsed() >= d,
        }
    }
}

/// A byte-identical copy of `master`, under its own scratch directory.
///
/// Every stage runs on a copy rather than a fresh seed: FTS5's segment layout
/// decides most of a write's cost, and reseeding would let it drift between
/// the rows of a table. `seed_index` ends with a TRUNCATE checkpoint, so the
/// one file holds the whole index.
pub fn clone_arm(master: &Arm, tag: &str) -> Arm {
    let path = testutil::scratch_db(tag);
    std::fs::copy(&master.path, &path).expect("copy the seeded index");
    Arm {
        what: master.what.clone(),
        keyed: master.keyed,
        pgsz: master.pgsz,
        page_size: master.page_size,
        hmac: master.hmac,
        path,
        seeded_in: Duration::ZERO,
    }
}

/// Open `arm` on the coordinator's writer profile, `PRAGMAS_INCREMENTAL`,
/// which a reconcile runs on. `open_existing(.., true)` would measure
/// `PRAGMAS_FAST`'s 8 MiB page cache instead and flatter every figure.
pub fn open(arm: &Arm) -> Connection {
    arm.with_key(|| {
        db::open::open_incremental_writer(&arm.path.to_string_lossy()).expect("open the copy")
    })
}

/// Set `conn`'s page cache to `mib` MiB.
pub fn set_cache(conn: &Connection, mib: i64) {
    conn.execute_batch(&format!("PRAGMA cache_size = -{};", mib * 1024))
        .expect("cache_size");
}

/// The single integer `sql` returns, or -1.
pub fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

/// The `?,?,…` list for `n` parameters.
pub fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Run `sql`, a statement up to and including `IN`, once per [`CHUNK`] of
/// `ids`; returns the rows changed. `cached` prepares through the
/// connection's statement cache, as `repo` does; otherwise every chunk
/// compiles afresh.
pub fn chunked(tx: &Connection, sql: &str, ids: &[i64], cached: bool) -> usize {
    let mut changed = 0;
    for chunk in ids.chunks(CHUNK) {
        let full = format!("{} ({})", sql, placeholders(chunk.len()));
        let params = rusqlite::params_from_iter(chunk.iter());
        changed += if cached {
            tx.prepare_cached(&full).and_then(|mut s| s.execute(params))
        } else {
            tx.execute(&full, params)
        }
        .expect(sql);
    }
    changed
}

/// Rows in FTS5's `%_data`, the quiescence signal; see
/// `file_handling::fts_end_tombstone_burst` for why `sqlite3_changes()` is
/// not one.
pub fn fts_data_rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM searchabletext_data", [], |r| r.get(0))
        .unwrap_or(-1)
}

/// Merge until `%_data` stops changing; returns the time taken, the rounds,
/// and the `%_data` rows before and after.
///
/// `merge_pages` must be positive: a negative argument selects
/// `fts5IndexOptimizeStruct`, a different algorithm (see
/// `file_handling::fts_finalize_after_text_indexing`).
pub fn merge_to_quiescence(conn: &Connection, merge_pages: i64) -> (Duration, u32, i64, i64) {
    let before = fts_data_rows(conn);
    let started = Instant::now();
    let mut rounds = 0;
    let mut last = before;
    loop {
        conn.execute(
            "INSERT INTO searchabletext(searchabletext, rank) VALUES('merge', ?1)",
            [merge_pages],
        )
        .expect("merge");
        rounds += 1;
        let now = fts_data_rows(conn);
        if now == last || rounds > 500 {
            break;
        }
        last = now;
    }
    (started.elapsed(), rounds, before, last)
}

/// [`merge_to_quiescence`] at [`MERGE_PAGES`], printed as a row of a table
/// whose name column is `width` wide.
pub fn print_merge(conn: &Connection, width: usize, suffix: &str) {
    let (took, rounds, before, after) = merge_to_quiescence(conn, MERGE_PAGES);
    println!(
        "  {:<width$} merge: {:.0} ms over {} rounds, %_data {} -> {} rows{}",
        "",
        took.as_secs_f64() * 1000.0,
        rounds,
        before,
        after,
        suffix
    );
}

/// Print the FTS5 shadow tables a tombstone has to find its way into:
/// `%_data` holds the segment leaves, `%_idx` one row per segment b-tree
/// node, and `%_docsize` the origin a contentless delete looks up.
pub fn print_fts_census(conn: &Connection) {
    println!(
        "  fts: {} %_data rows ({}), {} %_idx, {} %_docsize",
        count(conn, "SELECT COUNT(*) FROM searchabletext_data"),
        mib(count(
            conn,
            "SELECT COALESCE(SUM(pgsize), 0) FROM dbstat \
             WHERE name LIKE 'searchabletext%'"
        )
        .max(0) as u64),
        count(conn, "SELECT COUNT(*) FROM searchabletext_idx"),
        count(conn, "SELECT COUNT(*) FROM searchabletext_docsize"),
    );
}
