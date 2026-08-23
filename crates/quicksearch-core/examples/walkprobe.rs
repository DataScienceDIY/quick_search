//! Times and syscall-counts the phase-1 walk, without touching a database.
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
//! Expect ~one `statx` per unchanged file, open/read/close for new or
//! modified ones, and `readlink` only for the roots — a per-file `readlink`
//! means a `canonicalize` crept back. Inline extraction is CPU, visible in
//! files/sec, not the trace. Compare each mode's second pass.
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
    // Phase 1 in isolation: a scratch index, so every file classifies as new.
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
        // As the real walk: a non-UTF-8 name is skipped; seen, never prepared.
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
