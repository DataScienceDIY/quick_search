//! End-to-end timing and syscall accounting for a full indexing run.
//!
//! [`walkprobe`](walkprobe.rs) covers phase 1 alone, without a database. This
//! covers the whole pipeline — parallel walk, `files` writes, and content
//! extraction — because the interesting redundancy lives *between* the two
//! phases: the walk reads a file's head to hash it and sniff its MIME, and
//! extraction then reopens the same file and reads it again.
//!
//! ```text
//! cargo build -p quicksearch-core --example indexprobe --release
//! ./target/release/examples/indexprobe gen  /tmp/qs-bench
//! ./target/release/examples/indexprobe cold /tmp/qs-bench /tmp/qs-bench.db
//! ./target/release/examples/indexprobe warm /tmp/qs-bench /tmp/qs-bench.db
//! ```
//!
//! `cold` deletes the database first, so every file is new: the walk hashes
//! it and extraction reads it. `warm` re-runs over the existing database with
//! the tree untouched, which is the case that has to stay at one `stat` per
//! file — see [`crate::file_handling::classify_for_indexing`].
//!
//! For syscalls per file, trace a run and bucket by the tree's paths:
//!
//! ```text
//! strace -f -y -o /tmp/t.log \
//!     -e trace=openat,statx,newfstatat,fstat,read,pread64,readlink,close,getdents64,lseek \
//!     ./target/release/examples/indexprobe cold /tmp/qs-bench /tmp/qs-bench.db
//! grep -oP '^\d+ \K[a-z0-9_]+' <(grep '/tmp/qs-bench/' /tmp/t.log) | sort | uniq -c
//! ```
//!
//! Group by thread id instead (`grep -oP '^\d+ [a-z0-9_]+'`) to see the split
//! between the walk workers and the extraction thread.
//!
//! The run modes deliberately do no filesystem inspection of their own — no
//! progress walk, no size survey — so that every syscall the trace attributes
//! to the tree came from the indexer. The size histogram is printed by `gen`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Allocation accounting
// ---------------------------------------------------------------------------

/// `System`, counting. A global allocator is **per binary**, so this affects
/// only this probe — the shipped `quicksearch` is untouched.
///
/// Global atomics rather than the per-thread `Cell`s `tests/search_alloc.rs`
/// uses, and for the opposite reason. There the work was synchronous on one
/// thread and other *tests* ran concurrently, so per-thread counting was both
/// necessary and more precise. Here the work is spread over a walk pool, an
/// extraction pool, a feeder and a writer — per-thread counting would report a
/// fraction of it — and nothing else is running in this process, so a global
/// count is exactly the run.
///
/// The atomics cost every allocation a contended RMW, which is real overhead
/// and shows in the wall-clock line. That is acceptable because both sides of a
/// before/after comparison carry the same instrumentation; it is not acceptable
/// to quote these timings against numbers from an uninstrumented build.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK_LIVE: AtomicU64 = AtomicU64::new(0);

#[inline]
fn note_alloc(size: usize) {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    PEAK_LIVE.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            let (old, new) = (l.size() as u64, new as u64);
            ALLOC_BYTES.fetch_add(new.saturating_sub(old), Ordering::Relaxed);
            let live = if new >= old {
                LIVE.fetch_add(new - old, Ordering::Relaxed) + (new - old)
            } else {
                LIVE.fetch_sub(old - new, Ordering::Relaxed) - (old - new)
            };
            PEAK_LIVE.fetch_max(live, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Peak resident set size, from the kernel's own high-water mark. Unlike a
/// sampled figure this cannot miss a spike.
fn vm_hwm_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .map(|kib| kib * 1024)
        .unwrap_or(0)
}
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

/// Files whose head the walk reads in full at the default 8 KiB
/// `hash_length`, i.e. the ones extraction never needs to reopen.
const SMALL_TEXT: usize = 800;
/// Text files past `hash_length`, which extraction must still read.
const LARGE_TEXT: usize = 100;
/// No extractor claims these, so extraction resolves them without touching
/// the disk. A control group: their cost must not move.
const BINARY: usize = 100;

/// Scale the generated tree by an integer factor (`QSB_SCALE`), keeping the
/// mix between the three groups fixed.
///
/// The default thousand files is enough to exercise every code path and far
/// too few to measure any of them: a run that size is dominated by fixed
/// start-up — opening the index, the config reconcile — and its per-file
/// figures carry the whole of SQLite's and FTS5's fixed structure spread over
/// a thousand rows. Anything claiming to be a per-file cost needs a tree where
/// the fixed part has been amortised away, and the difference between two
/// scales is the only way to tell the two apart.
fn scale() -> usize {
    std::env::var("QSB_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1)
}

const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "eta",
    "theta",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "indexer",
    "rust",
    "cargo",
    "sqlite",
    "baloo",
    "tokenizer",
    "trigram",
    "snippet",
    "ocean",
    "forest",
    "mountain",
    "river",
    "valley",
    "bridge",
    "tunnel",
    "morning",
    "afternoon",
    "evening",
    "midnight",
    "yesterday",
    "today",
];

/// Deterministic so two runs index byte-identical trees and their timings are
/// comparable. Plain LCG — this only has to spread, not to be random.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn in_range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo)
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let tree = PathBuf::from(
        std::env::args()
            .nth(2)
            .expect("usage: indexprobe <gen|cold|warm> <tree> [db]"),
    );

    match mode.as_str() {
        "gen" => generate(&tree),
        "cold" | "warm" => {
            let db = PathBuf::from(
                std::env::args()
                    .nth(3)
                    .expect("usage: indexprobe <cold|warm> <tree> <db>"),
            );
            if mode == "cold" {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{}", db.display(), suffix));
                }
            }
            run(&mode, &tree, &db);
        }
        _ => {
            eprintln!("usage: indexprobe <gen|cold|warm> <tree> [db]");
            std::process::exit(2);
        }
    }
}

/// Build a tree with a size mix that separates the three code paths, and
/// report it so results are self-describing.
fn generate(tree: &Path) {
    let _ = std::fs::remove_dir_all(tree);
    std::fs::create_dir_all(tree).expect("create tree");

    let mut rng = Rng(0x5eed);
    let (mut small_bytes, mut large_bytes, mut bin_bytes) = (0usize, 0usize, 0usize);
    let scale = scale();
    let (small_text, large_text, binary) = (SMALL_TEXT * scale, LARGE_TEXT * scale, BINARY * scale);

    // Spread across subdirectories so the walk does real directory work
    // rather than one enormous readdir.
    for i in 0..small_text {
        let dir = tree.join(format!("src/mod{}", i % (40 * scale)));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ext = ["txt", "md", "rs", "json"][i % 4];
        let size = rng.in_range(200, 8 * 1024);
        let body = prose(&mut rng, size);
        small_bytes += body.len();
        std::fs::write(dir.join(format!("f{}.{}", i, ext)), body).expect("write");
    }

    for i in 0..large_text {
        let dir = tree.join(format!("docs/set{}", i % (10 * scale)));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let size = rng.in_range(8 * 1024 + 1, 200 * 1024);
        let body = prose(&mut rng, size);
        large_bytes += body.len();
        std::fs::write(dir.join(format!("doc{}.md", i)), body).expect("write");
    }

    for i in 0..binary {
        let dir = tree.join(format!("assets/set{}", i % (10 * scale)));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let n = rng.in_range(1024, 50 * 1024);
        let blob: Vec<u8> = (0..n).map(|_| (rng.next() & 0xff) as u8).collect();
        bin_bytes += blob.len();
        std::fs::write(dir.join(format!("blob{}.bin", i)), blob).expect("write");
    }

    let total = small_text + large_text + binary;
    eprintln!("generated {} files under {}", total, tree.display());
    eprintln!(
        "  text <= 8 KiB : {:5} files, {:8.1} MiB  (head covers the whole file)",
        small_text,
        small_bytes as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "  text >  8 KiB : {:5} files, {:8.1} MiB  (extraction must read it)",
        large_text,
        large_bytes as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "  binary        : {:5} files, {:8.1} MiB  (no extractor; control group)",
        binary,
        bin_bytes as f64 / (1024.0 * 1024.0)
    );
}

fn prose(rng: &mut Rng, target: usize) -> String {
    let mut s = String::with_capacity(target + 16);
    while s.len() < target {
        s.push_str(WORDS[rng.next() as usize % WORDS.len()]);
        s.push(if rng.next().is_multiple_of(12) {
            '\n'
        } else {
            ' '
        });
    }
    s.truncate(target);
    s
}

/// The kernel's own accounting for this process, from `/proc/self/io`.
///
/// `read_bytes`/`write_bytes` are what actually reached the block layer, so
/// they are the figures that describe the *disk* rather than the page cache —
/// a warm re-read shows as `rchar` without moving `read_bytes`. `syscr`/`syscw`
/// count the calls regardless, which is what separates "we read a lot" from
/// "we read a little, many times".
///
/// Zero everywhere on a filesystem that does not report it (virtiofs, some
/// network mounts); the caller says so rather than printing a confident 0.
#[derive(Default, Clone, Copy)]
struct Io {
    rchar: u64,
    wchar: u64,
    syscr: u64,
    syscw: u64,
    read_bytes: u64,
    write_bytes: u64,
    cancelled: u64,
}

impl Io {
    fn read() -> Io {
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

    fn since(&self, start: &Io) -> Io {
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

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// What the write-ahead log did during a run, sampled from outside the process.
///
/// The interesting part of write amplification is not the total — that is one
/// number from `/proc/self/io` — but how it splits between **frames appended to
/// the log** and **pages copied back into the database** by a checkpoint. The
/// two want opposite fixes: more frames means the load is rewriting pages, and
/// more copy-back means it is checkpointing too often. A page rewritten five
/// times between two checkpoints costs five frames and *one* copy-back, so
/// checkpointing less often can be strictly cheaper — which is the opposite of
/// what "keep the log small" suggests.
///
/// Sampled rather than instrumented: the log is a file, its size is a `stat`,
/// and a checkpoint truncates it. Growth between samples is frames appended; a
/// drop is a checkpoint, and the size it dropped *from* bounds what that
/// checkpoint copied. Nothing in the library has to know it is being watched.
#[derive(Default, Clone, Copy)]
struct WalStats {
    /// Largest the log ever got.
    peak: u64,
    /// Sum of every increase — bytes appended to the log over the run.
    appended: u64,
    /// Sum of the size before each truncation — an upper bound on the bytes
    /// each checkpoint wrote back into the database.
    copied_back: u64,
    checkpoints: u64,
}

/// Watch `path` until `stop` is set, at `SAMPLE`.
///
/// One millisecond, because a checkpoint of a small log is quick and a sampler
/// that misses the rise and the fall reports neither. It costs one `stat` per
/// millisecond, which is nothing next to what is being measured.
fn sample_wal(path: PathBuf, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<WalStats> {
    const SAMPLE: Duration = Duration::from_millis(1);
    std::thread::spawn(move || {
        let mut stats = WalStats::default();
        let mut last = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let now = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if now > last {
                stats.appended += now - last;
            } else if now < last {
                // A shrink is a checkpoint landing the log. `last` is the most
                // recent size seen before it, so it bounds the copy-back.
                stats.checkpoints += 1;
                stats.copied_back += last;
            }
            stats.peak = stats.peak.max(now);
            last = now;
            std::thread::sleep(SAMPLE);
        }
        stats
    })
}

/// Size of the index and the sidecars it leaves behind.
fn db_sizes(db: &Path) -> (u64, u64) {
    let len = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    (
        len(db.to_path_buf()),
        len(PathBuf::from(format!("{}-wal", db.display()))),
    )
}

fn run(mode: &str, tree: &Path, db: &Path) {
    let config = Config::default();

    // `run_indexing` writes this marker only on a successful finish, so it is
    // the one unambiguous completion signal — polling the status enum races,
    // because a small tree finishes between two polls and `Idle` then means
    // both "not started" and "already done".
    if db.exists() {
        let conn = rusqlite::Connection::open(db).expect("open db");
        conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
            .expect("clear marker");
    }

    // Cleared so the phase summaries below belong to this run alone.
    quicksearch_core::log::clear();
    let io_start = Io::read();
    let (db_before, wal_before) = db_sizes(db);

    let wal_path = PathBuf::from(format!("{}-wal", db.display()));
    let wal_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wal_sampler = sample_wal(wal_path, wal_stop.clone());

    let service = IndexingService::new();
    let start = Instant::now();
    service
        .start_indexing(
            vec![tree.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config,
        )
        .expect("start indexing");

    let deadline = Instant::now() + Duration::from_secs(600);
    let mut done = false;
    while Instant::now() < deadline {
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if db.exists() {
            if let Ok(conn) = rusqlite::Connection::open(db) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = start.elapsed();
    assert!(done, "indexing did not finish within the timeout");
    // The run's last checkpoint happens inside here, so the sampler outlives it.
    service.stop_indexing().expect("stop");
    wal_stop.store(true, Ordering::Relaxed);
    let wal = wal_sampler.join().unwrap_or_default();

    // Count what was actually indexed rather than assuming `gen`'s tree.
    // The constants describe the tree this probe builds; pointing it at any
    // other one made the rate a fiction.
    let total = rusqlite::Connection::open(db)
        .ok()
        .and_then(|c| quicksearch_core::db::repo::row_count(&c).ok())
        .unwrap_or(0);

    // Read after `stop_indexing`, so the optimize pass's checkpoint — which is
    // where a run's dirty pages actually reach the file — is inside the totals.
    let io = Io::read().since(&io_start);
    let (db_after, wal_after) = db_sizes(db);
    let per_file = |n: u64| {
        if total == 0 {
            "-".to_string()
        } else {
            format!("{:.0} B/file", n as f64 / total as f64)
        }
    };

    eprintln!(
        "\n{}: {:?} ({:.0} files/sec over {} files)",
        mode,
        elapsed,
        total as f64 / elapsed.as_secs_f64(),
        total
    );

    // The pipeline logs one line per root per phase; they are the walk/extract
    // split without a `perf` session.
    for line in quicksearch_core::log::snapshot() {
        let m = &line.text;
        if m.contains("walk done")
            || m.contains("walk ended early")
            || m.contains("content done")
            || m.contains("stale cleanup")
            || m.contains("indexing complete")
        {
            eprintln!("  phase   {}", m);
        }
    }

    let allocs = ALLOCS.load(Ordering::Relaxed);
    eprintln!(
        "  wal     peak {}, {} appended, {} copied back over {} checkpoint(s)",
        mib(wal.peak),
        mib(wal.appended),
        mib(wal.copied_back),
        wal.checkpoints,
    );
    eprintln!(
        "  memory  {} allocations ({:.1} per file), {} churned, peak live {}, VmHWM {}",
        allocs,
        allocs as f64 / total.max(1) as f64,
        mib(ALLOC_BYTES.load(Ordering::Relaxed)),
        mib(PEAK_LIVE.load(Ordering::Relaxed)),
        mib(vm_hwm_bytes()),
    );
    eprintln!(
        "  index   {} -> {}   wal {} -> {}",
        mib(db_before),
        mib(db_after),
        mib(wal_before),
        mib(wal_after),
    );
    eprintln!(
        "  syscall {} reads, {} writes   ({:.1} reads/file, {:.1} writes/file)",
        io.syscr,
        io.syscw,
        io.syscr as f64 / total.max(1) as f64,
        io.syscw as f64 / total.max(1) as f64,
    );
    eprintln!(
        "  bytes   rchar {} / wchar {}   (through the syscall layer, cache included)",
        mib(io.rchar),
        mib(io.wchar),
    );
    if io.read_bytes == 0 && io.write_bytes == 0 {
        eprintln!(
            "  disk    not reported for this filesystem (virtiofs/tmpfs); \
             use rchar/wchar and the index sizes above"
        );
    } else {
        eprintln!(
            "  disk    read {} / written {} (cancelled {})   ->  {} written",
            mib(io.read_bytes),
            mib(io.write_bytes),
            mib(io.cancelled),
            per_file(io.write_bytes.saturating_sub(io.cancelled)),
        );
    }
}
