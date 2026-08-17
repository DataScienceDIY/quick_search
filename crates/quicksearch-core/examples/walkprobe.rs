//! Times and syscall-counts the phase-1 walk, without touching a database.
//!
//! ```text
//! cargo build -p quicksearch-core --example walkprobe --release
//! ./target/release/examples/walkprobe <root> parallel   # the threaded walker
//! ./target/release/examples/walkprobe <root> serial     # one thread, for comparison
//! ```
//!
//! Indexing a network share is bound by round trips, not bandwidth: every
//! metadata operation that misses the client cache costs one, and throughput
//! is round-trips-in-flight divided by latency. So the number that matters is
//! syscalls per file, which this makes directly visible:
//!
//! ```text
//! strace -f -c -e trace=openat,statx,newfstatat,readlink,getdents64,read,lseek,close \
//!     ./target/release/examples/walkprobe <root> parallel
//! ```
//!
//! Expect roughly one `statx` per unchanged file, plus open/read/close for
//! files that are new or modified, and `readlink` only for resolving the roots
//! themselves — a per-file `readlink` count means a `canonicalize` has crept
//! back into the hot path.
//!
//! Those four syscalls also now cover the *whole* cost of a small text file:
//! the head read for the hash is the file's entire contents, so the walk
//! extracts its text there and the content pass never opens it again. That
//! work is CPU, not syscalls, so it shows up in files/sec here and not in the
//! trace. Use [`indexprobe`](indexprobe.rs) to see both phases together.
//!
//! Both modes report files/sec. Run each twice: the first pass warms the page
//! cache (or, on a share, the client's attribute cache), so the second is the
//! one to compare.
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use quicksearch_core::config::{Config, IgnoreSet};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{
    classify_for_indexing, filtered_walk, prepare_file_record, DirRows, FileIndexAction,
    UnreadableDirs,
};
use quicksearch_core::walk::{walk_indexable_files, WalkEvent};

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let mode = std::env::args().nth(2).unwrap_or_else(|| "parallel".into());
    let config = Config::default();
    // Phase 1 in isolation: an empty index, so every file classifies as new.
    // The parallel walker reads its classification data from a database now,
    // so it gets a scratch one rather than an empty map.
    let db = std::env::temp_dir().join(format!(
        "quicksearch-walkprobe-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);
    quicksearch_core::db::open_or_recreate(db.to_str().unwrap(), &config.processing.tokenize)
        .expect("scratch index");
    let existing = DirRows::new();

    let start = Instant::now();
    let (seen, prepared) = match mode.as_str() {
        "serial" => serial(&root, &config, &existing),
        _ => parallel(&root, &config, db.to_str().unwrap()),
    };
    let _ = std::fs::remove_file(&db);
    let elapsed = start.elapsed();

    eprintln!(
        "{mode}: {seen} files, {prepared} prepared in {:?} ({:.0} files/sec)",
        elapsed,
        seen as f64 / elapsed.as_secs_f64()
    );
}

fn serial(root: &str, config: &Config, existing: &DirRows) -> (usize, usize) {
    let ignore = IgnoreSet::compile(&[]).unwrap();
    let registry = Registry::default_set();
    let (mut seen, mut prepared) = (0, 0);
    for entry in filtered_walk(root, false, false, &ignore, &UnreadableDirs::default()) {
        seen += 1;
        // Same rule as the real walk: a name that is not valid UTF-8 cannot be
        // stored in `files.path` and reopened by it, so it is skipped before
        // anything tries to hash it. Counted as seen, never prepared.
        let Some(path) = entry.path().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        let Some(mtime) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
        else {
            continue;
        };
        // Keyed by name within its directory, as the real walk now is; with
        // an empty index the answer is Insert either way.
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if classify_for_indexing(&name, mtime, existing) != FileIndexAction::Skip
            && prepare_file_record(&path, &meta, config, &registry).is_some()
        {
            prepared += 1;
        }
    }
    (seen, prepared)
}

fn parallel(root: &str, config: &Config, db_path: &str) -> (usize, usize) {
    let (mut seen, mut prepared) = (0, 0);
    for event in walk_indexable_files(
        &[root.to_string()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        db_path,
        config.clone(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ) {
        let WalkEvent::File(file) = event else {
            continue;
        };
        seen += 1;
        if file.record.is_some() {
            prepared += 1;
        }
    }
    (seen, prepared)
}
