//! End-to-end timing and syscall accounting for a full indexing run.
//!
//! [`walkprobe`](walkprobe.rs) covers phase 1 alone; this covers the whole
//! pipeline — parallel walk, `files` writes, and content extraction.
//!
//! ```text
//! cargo build -p quicksearch-core --example indexprobe --release
//! ./target/release/examples/indexprobe gen  /tmp/qs-bench
//! ./target/release/examples/indexprobe cold /tmp/qs-bench /tmp/qs-bench.db
//! ./target/release/examples/indexprobe warm /tmp/qs-bench /tmp/qs-bench.db
//! ```
//!
//! `cold` deletes the database first; `warm` re-runs untouched — the case
//! that must stay at one `stat` per file. The run modes inspect nothing
//! themselves, so every syscall a trace attributes to the tree is the indexer's:
//!
//! ```text
//! strace -f -y -o /tmp/t.log \
//!     -e trace=openat,statx,newfstatat,fstat,read,pread64,readlink,close,getdents64,lseek \
//!     ./target/release/examples/indexprobe cold /tmp/qs-bench /tmp/qs-bench.db
//! grep -oP '^\d+ \K[a-z0-9_]+' <(grep '/tmp/qs-bench/' /tmp/t.log) | sort | uniq -c
//! ```
//!
//! `QSB_HASH_LENGTH` overrides `[processing] hash_length` for a run — how
//! [`hashprobe`](hashprobe.rs) gets its end-to-end column.
//!
//! `QSB_KEY=<64 hex digits>` measures an encrypted index. It is not optional
//! dressing: without a key installed this probe polls the completion marker
//! through a plain open, which an encrypted index cannot answer, so the run
//! reports a three-hour hang instead of its actual time.

mod common;

use std::alloc::{GlobalAlloc, Layout};

// What `Counting` wraps: the allocator the shipped binaries install, or the
// throughput figures describe a build nobody runs.
use quicksearch_core::platform::Allocator as Inner;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use common::{evict, mib, Io};

// ---------------------------------------------------------------------------
// Allocation accounting
// ---------------------------------------------------------------------------

/// [`Inner`], counting — per binary, so the shipped `quicksearch` is
/// untouched. Global atomics, not `search_alloc`'s per-thread `Cell`s: the
/// work spreads over several pools and nothing else runs here, so a global
/// count is exactly the run. The contended RMW is fine when both sides of a
/// comparison carry it; never quote against an uninstrumented build.
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
        let p = unsafe { Inner.alloc(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { Inner.alloc_zeroed(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
        unsafe { Inner.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { Inner.realloc(p, l, new) };
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

use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};

/// Files whose head the walk reads in full — never reopened.
const SMALL_TEXT: usize = 800;
/// Text files past `hash_length`, which extraction must still read.
const LARGE_TEXT: usize = 100;
/// Unclaimed by any extractor — a control group whose cost must not move.
const BINARY: usize = 100;

/// Scale the generated tree (`QSB_SCALE`), keeping the mix fixed: the
/// default thousand files is dominated by fixed start-up, and the difference
/// between two scales is the only way to separate it from per-file cost.
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

/// Deterministic, so two runs index byte-identical trees.
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
            .expect("usage: indexprobe <gen|evict|cold|warm> <tree> [db]"),
    );

    match mode.as_str() {
        "gen" => generate(&tree),
        "evict" => {
            let db = std::env::args().nth(3).map(PathBuf::from);
            let (files, bytes) = evict(&tree, db.as_deref());
            eprintln!(
                "evicted {} files ({}) from the page cache",
                files,
                mib(bytes)
            );
        }
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
            eprintln!("usage: indexprobe <gen|evict|cold|warm> <tree> [db]");
            std::process::exit(2);
        }
    }
}

fn generate(tree: &Path) {
    let _ = std::fs::remove_dir_all(tree);
    std::fs::create_dir_all(tree).expect("create tree");

    let mut rng = Rng(0x5eed);
    let (mut small_bytes, mut large_bytes, mut bin_bytes) = (0usize, 0usize, 0usize);
    let scale = scale();
    let (small_text, large_text, binary) = (SMALL_TEXT * scale, LARGE_TEXT * scale, BINARY * scale);

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

/// What the WAL did during a run, sampled from outside the process: growth
/// between samples is frames appended, a drop is a checkpoint, and the size
/// it dropped *from* bounds the copy-back. The split matters: more frames
/// means the load rewrites pages, more copy-back means checkpointing too
/// often — checkpointing less can be strictly cheaper.
#[derive(Default, Clone, Copy)]
struct WalStats {
    peak: u64,
    appended: u64,
    /// Sum of sizes before each truncation — bounds checkpoint copy-back.
    copied_back: u64,
    checkpoints: u64,
}

/// Watch `path` until `stop`, at `SAMPLE` (1 ms): a sampler that misses a
/// small log's rise and fall reports neither.
fn sample_wal(
    path: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<WalStats> {
    const SAMPLE: Duration = Duration::from_millis(1);
    std::thread::spawn(move || {
        let mut stats = WalStats::default();
        let mut last = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let now = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if now > last {
                stats.appended += now - last;
            } else if now < last {
                // A shrink is a checkpoint; `last` bounds its copy-back.
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

fn db_sizes(db: &Path) -> (u64, u64) {
    let len = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    (
        len(db.to_path_buf()),
        len(PathBuf::from(format!("{}-wal", db.display()))),
    )
}

/// `[processing] hash_length` for this run, from `QSB_HASH_LENGTH`: charged
/// to the walk, credited back by the content pass, so the knob has to be
/// swept end-to-end. Out-of-range values are clamped, with a warning.
fn hash_length_override() -> Option<usize> {
    std::env::var("QSB_HASH_LENGTH")
        .ok()
        .and_then(|v| v.parse().ok())
}

/// `QSB_KEY=<64 hex digits>` measures an *encrypted* index: the key is
/// installed process-wide before any connection exists, exactly as the GUI
/// does after an unlock. Raw hex rather than a password, so no Argon2id
/// derivation lands inside a timed run.
fn install_key() -> bool {
    match std::env::var("QSB_KEY") {
        Ok(hex) => {
            let key = quicksearch_core::security::IndexKey::from_hex(hex.trim())
                .expect("QSB_KEY must be 64 hex digits");
            quicksearch_core::db::set_process_key(Some(key));
            true
        }
        Err(_) => false,
    }
}

/// Open the index the way this run's key demands.
///
/// **A plain open cannot read an encrypted index**, and
/// [`get_last_full_index`](quicksearch_core::db::repo::get_last_full_index)
/// reports that failure as `None` — indistinguishable from "not finished
/// yet". Polling an encrypted run through a plain open therefore never
/// observes its own completion and sits here until the deadline, which reads
/// as an indexing slowdown of several orders of magnitude rather than as the
/// probe defect it is.
fn probe_open(db: &Path, keyed: bool) -> Option<rusqlite::Connection> {
    if keyed {
        quicksearch_core::db::open_existing(&db.to_string_lossy(), false).ok()
    } else {
        rusqlite::Connection::open(db).ok()
    }
}

fn run(mode: &str, tree: &Path, db: &Path) {
    let mut config = Config::default();
    if let Some(n) = hash_length_override() {
        config.processing.hash_length = n;
    }
    let hash_length = config.processing.hash_length;
    let keyed = install_key();

    // The marker is the one unambiguous completion signal; polling the
    // status enum races on a small tree.
    if db.exists() {
        let conn = probe_open(db, keyed).expect("open db");
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

    // Generous, like `memprobe`'s: the scale sweep this probe exists for runs
    // hundreds of thousands of files, and a run that times out reports
    // nothing. Past this is a hang, not a slow disk.
    let deadline = Instant::now() + Duration::from_secs(3 * 3600);
    let mut done = false;
    while Instant::now() < deadline {
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if db.exists() {
            if let Some(conn) = probe_open(db, keyed) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = start.elapsed();
    assert!(
        done,
        "indexing did not finish within the timeout (keyed = {})",
        keyed
    );
    // The run's last checkpoint happens inside here, so the sampler outlives it.
    service.stop_indexing().expect("stop");
    wal_stop.store(true, Ordering::Relaxed);
    let wal = wal_sampler.join().unwrap_or_default();

    // Count what was actually indexed rather than assuming `gen`'s tree —
    // pointing the probe elsewhere made the rate a fiction.
    let total = probe_open(db, keyed)
        .and_then(|c| quicksearch_core::db::repo::row_count(&c).ok())
        .unwrap_or(0);

    // Read after `stop_indexing`, so the optimize pass's checkpoint is inside the totals.
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
        "\n{}: {:?} ({:.0} files/sec over {} files, hash_length {})",
        mode,
        elapsed,
        total as f64 / elapsed.as_secs_f64(),
        total,
        hash_length,
    );

    // One log line per root per phase: the walk/extract split without `perf`.
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
        mib(common::vm_hwm().unwrap_or(0)),
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
