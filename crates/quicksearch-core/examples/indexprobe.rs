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

use std::path::{Path, PathBuf};
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

    // Spread across subdirectories so the walk does real directory work
    // rather than one enormous readdir.
    for i in 0..SMALL_TEXT {
        let dir = tree.join(format!("src/mod{}", i % 40));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ext = ["txt", "md", "rs", "json"][i % 4];
        let size = rng.in_range(200, 8 * 1024);
        let body = prose(&mut rng, size);
        small_bytes += body.len();
        std::fs::write(dir.join(format!("f{}.{}", i, ext)), body).expect("write");
    }

    for i in 0..LARGE_TEXT {
        let dir = tree.join(format!("docs/set{}", i % 10));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let size = rng.in_range(8 * 1024 + 1, 200 * 1024);
        let body = prose(&mut rng, size);
        large_bytes += body.len();
        std::fs::write(dir.join(format!("doc{}.md", i)), body).expect("write");
    }

    for i in 0..BINARY {
        let dir = tree.join(format!("assets/set{}", i % 10));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let n = rng.in_range(1024, 50 * 1024);
        let blob: Vec<u8> = (0..n).map(|_| (rng.next() & 0xff) as u8).collect();
        bin_bytes += blob.len();
        std::fs::write(dir.join(format!("blob{}.bin", i)), blob).expect("write");
    }

    let total = SMALL_TEXT + LARGE_TEXT + BINARY;
    eprintln!("generated {} files under {}", total, tree.display());
    eprintln!(
        "  text <= 8 KiB : {:5} files, {:8.1} MiB  (head covers the whole file)",
        SMALL_TEXT,
        small_bytes as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "  text >  8 KiB : {:5} files, {:8.1} MiB  (extraction must read it)",
        LARGE_TEXT,
        large_bytes as f64 / (1024.0 * 1024.0)
    );
    eprintln!(
        "  binary        : {:5} files, {:8.1} MiB  (no extractor; control group)",
        BINARY,
        bin_bytes as f64 / (1024.0 * 1024.0)
    );
}

fn prose(rng: &mut Rng, target: usize) -> String {
    let mut s = String::with_capacity(target + 16);
    while s.len() < target {
        s.push_str(WORDS[rng.next() as usize % WORDS.len()]);
        s.push(if rng.next().is_multiple_of(12) { '\n' } else { ' ' });
    }
    s.truncate(target);
    s
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
    service.stop_indexing().expect("stop");

    let total = SMALL_TEXT + LARGE_TEXT + BINARY;
    eprintln!(
        "{}: {:?} ({:.0} files/sec over {} files)",
        mode,
        elapsed,
        total as f64 / elapsed.as_secs_f64(),
        total
    );
}
