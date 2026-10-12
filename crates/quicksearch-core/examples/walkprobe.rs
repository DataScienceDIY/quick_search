//! Times and syscall-counts the phase-1 walk against an empty scratch index.
//!
//! ```text
//! cargo build -p quicksearch-core --example walkprobe --release
//! ./target/release/examples/walkprobe <root> parallel   # the threaded walker
//! ./target/release/examples/walkprobe <root> serial     # one thread, for comparison
//! ```
//!
//! A network share is round-trip bound, so what matters is syscalls per file:
//!
//! ```text
//! strace -f -c -e trace=openat,statx,newfstatat,readlink,getdents64,read,lseek,close \
//!     ./target/release/examples/walkprobe <root> parallel
//! ```
//!
//! Expect about one `statx` and an open/read/close per file (all are new
//! against the scratch index; `indexprobe warm` prices unchanged ones), and
//! `readlink` only for the roots; a per-file `readlink` means something calls
//! `canonicalize`. Inline extraction is CPU, visible in files/sec, not the
//! trace.

// Timed on the allocator the shipped binaries install; see
// `platform::Allocator`.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use quicksearch_core::config::{Config, IgnoreSet};
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::{filtered_walk, prepare_file_record, UnreadableDirs};
use quicksearch_core::walk::{walk_indexable_files, WalkEvent};

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let mode = std::env::args().nth(2).unwrap_or_else(|| "parallel".into());
    let config = Config::default();
    // Phase 1 in isolation: a scratch index, so every file classifies as new.
    let db = std::env::temp_dir().join(format!(
        "quicksearch-walkprobe-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);
    quicksearch_core::db::open_or_recreate(db.to_str().unwrap(), &config.processing)
        .expect("scratch index");

    let start = Instant::now();
    let (seen, prepared) = match mode.as_str() {
        "serial" => serial(&root, &config),
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

/// Prepares every file, as the parallel walk does against the empty index.
fn serial(root: &str, config: &Config) -> (usize, usize) {
    let ignore = IgnoreSet::compile(&[]).unwrap();
    let registry = Registry::default_set();
    // One for the whole walk, as a walk worker holds one.
    let mut scratch = quicksearch_core::extract::Scratch::new(config);
    let (mut seen, mut prepared) = (0, 0);
    for entry in filtered_walk(
        root,
        false,
        &ignore,
        &UnreadableDirs::default(),
        None,
        false,
    ) {
        seen += 1;
        // As the real walk: a non-UTF-8 name is skipped; seen, never prepared.
        let Some(path) = entry.path().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if prepare_file_record(&path, &meta, config, &registry, &mut scratch).is_some() {
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
