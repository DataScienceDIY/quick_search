//! What indexing one file allocates: how many trips to the allocator it
//! costs and how big the transient peak behind them is. `index.rs` measures
//! how long a step takes; `extractprobe` measures how much RAM a pool needs.
//!
//! The counting allocator (`alloc`) sees Rust-side allocations only, which
//! leaves the walk's and the extractors' own churn. The fixtures are the
//! shared extraction corpus (`tests/corpus/`): every format QuickSearch
//! claims appears once, written by a library other than the one reading it
//! back.
//!
//! Reading the numbers: `allocs` is per-file allocator traffic, the figure a
//! scratch buffer removes; `bytes` is churn; `peak` is the transient
//! high-water one file reaches, which multiplies by the worker count
//! (`walk::thread_count_for`, one pool per root). A `peak` far above the
//! file's own size is amplification inside a parser.
//!
//! Gated by `QSB_INDEX_ALLOC`:
//!
//! ```text
//! QSB_INDEX_ALLOC=1 cargo bench -p quicksearch-core --bench index_alloc
//! ```

mod alloc;

use quicksearch_core::config::Config;
use quicksearch_core::db::repo::{self, DocEncoder};
use quicksearch_core::extract::{Registry, Scratch};
use quicksearch_core::file_handling::{decide_content, prepare_file_record};
use quicksearch_core::testutil::{sniff, Lcg};

use alloc::{Counters, Usage};

// The fixtures the extraction tests assert on, rather than a second set that
// could drift.
#[path = "../tests/corpus/mod.rs"]
mod corpus;

/// Measure one closure, dropping its result inside the region so the drop
/// is charged there too; a result kept alive would report the peak of the
/// next case instead.
fn measure<T>(f: impl FnOnce() -> T) -> Usage {
    let start = Counters::start();
    drop(std::hint::black_box(f()));
    start.since()
}

fn enabled() -> bool {
    std::env::var("QSB_INDEX_ALLOC").is_ok()
}

fn kib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

fn main() {
    alloc::the_counters_track_real_allocations();
    measure_charges_the_drop();
    if !enabled() {
        eprintln!("skipping: set QSB_INDEX_ALLOC=1 to run");
        return;
    }
    let (dir, samples) = corpus::build("index-alloc");
    let config = Config::default();
    let registry = Registry::default_set();

    extraction_traffic_per_file(&samples, &config, &registry);
    walk_traffic_per_file(&samples, &config, &registry);
    compression_traffic_per_chunk(&config);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The content pass's half: a `qs-extract` worker runs `decide_content` per
/// file, and that is its whole per-file cost outside the connection.
fn extraction_traffic_per_file(samples: &[corpus::Sample], config: &Config, registry: &Registry) {
    println!(
        "extraction — decide_content, the content worker's per-file work\n\n\
         {:<10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>8}",
        "format", "file (KiB)", "allocs", "reallocs", "bytes (KiB)", "peak (KiB)", "amp"
    );

    // One scratch for the whole table, as a `qs-extract` worker holds one
    // for a whole pass: the figures below are a worker's *steady state*, not
    // its first file.
    let mut scratch = Scratch::new(config);
    let mut rows: Vec<(&str, u64, Usage)> = Vec::new();
    for sample in samples {
        let path = sample.path.to_string_lossy().into_owned();
        let size = std::fs::metadata(&sample.path).map(|m| m.len()).unwrap_or(0);
        let Some(mime) = sniff(&sample.path, config.processing.hash_length) else {
            continue;
        };
        // Warm once: a format's first call may build a lazy static the file
        // after it does not pay for.
        let _ = decide_content(&path, Some(mime), registry, config, &mut scratch);
        let usage = measure(|| decide_content(&path, Some(mime), registry, config, &mut scratch));
        rows.push((sample.label, size, usage));
    }

    for (label, size, usage) in &rows {
        println!(
            "{:<10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>8}",
            label,
            kib(*size),
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
            // How far above the file's own size the transient peak reached.
            // This is the figure that multiplies by the worker count.
            match size {
                0 => "-".to_string(),
                n => format!("{:.1}x", usage.peak as f64 / *n as f64),
            }
        );
    }
    println!(
        "\n  maximum_text_file_size {} KiB, maximum_text_size {} KiB — what every\n  \
         extractor's ceilings are now derived from.\n",
        config.processing.maximum_text_file_size / 1024,
        config.processing.maximum_text_size / 1024,
    );
}

/// The walk's half: one `stat`'s metadata in, one finished record out,
/// including hashing, the MIME sniff and the inline-text shortcut.
fn walk_traffic_per_file(samples: &[corpus::Sample], config: &Config, registry: &Registry) {
    println!(
        "walk — prepare_file_record, the walk worker's per-file work\n\n\
         {:<10} {:>10} {:>10} {:>10} {:>12} {:>12}",
        "format", "file (KiB)", "allocs", "reallocs", "bytes (KiB)", "peak (KiB)"
    );

    // One scratch for the whole table; see `extraction_traffic_per_file`.
    let mut scratch = Scratch::new(config);
    for sample in samples {
        let path = sample.path.to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(&sample.path) else {
            continue;
        };
        let _ = prepare_file_record(&path, &meta, config, registry, &mut scratch);
        let usage = measure(|| prepare_file_record(&path, &meta, config, registry, &mut scratch));
        println!(
            "{:<10} {:>10} {:>10} {:>10} {:>12} {:>12}",
            sample.label,
            kib(meta.len()),
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
        );
    }
    println!(
        "\n  A file under hash_length ({} KiB) is extracted here rather than by the\n  \
         content pass, so its row carries inline text.\n",
        config.processing.hash_length / 1024
    );
}

/// Rows per writer chunk: `batch::STORE_CHUNK`, the unit `Bodies::fill` is
/// handed.
const CHUNK: usize = 32;

/// The writer's half that runs outside the connection lock: one output
/// `Vec<u8>` per row against one arena per chunk.
///
/// The zstd context does not appear here: `zstd::bulk::Compressor::new`
/// allocates through zstd's own C allocator, so its cost is CPU only
/// (`benches/index.rs`, group `zstd_encode`).
fn compression_traffic_per_chunk(config: &Config) {
    // `maximum_text_size` is the worst case a row can carry.
    let doc = lipsum(config.processing.maximum_text_size);
    println!(
        "compression — one {}-row chunk of {} KiB documents\n\n{:<24} {:>10} {:>10} {:>12} {:>12}",
        CHUNK,
        kib(doc.len() as u64),
        "shape",
        "allocs",
        "reallocs",
        "bytes (KiB)",
        "peak (KiB)"
    );

    let per_row = measure(|| {
        (0..CHUNK)
            .map(|_| repo::encode_one(&doc, true).expect("encode"))
            .collect::<Vec<_>>()
    });
    // What `Bodies::fill` does: one encoder and one arena for the chunk.
    let arena = measure(|| {
        let mut enc = DocEncoder::new().expect("encoder");
        let mut arena = Vec::new();
        (0..CHUNK)
            .map(|_| enc.encode_into(&doc, &mut arena).expect("encode"))
            .collect::<Vec<_>>()
            .len()
    });

    for (shape, usage) in [("a Vec per row", per_row), ("one arena, chunk", arena)] {
        println!(
            "{:<24} {:>10} {:>10} {:>12} {:>12}",
            shape,
            usage.allocs,
            usage.reallocs,
            kib(usage.bytes),
            kib(usage.peak),
        );
    }
    println!(
        "\n  The arena is reused across every chunk a writer call handles, so after the\n  \
         first its growth is zero too. The zstd context is invisible here — see above.\n"
    );
}

/// Deterministic filler; the compression figures must not move between runs.
fn lipsum(size: usize) -> String {
    const WORDS: &[&str] = &[
        "lorem", "ipsum", "dolor", "consectetur", "adipiscing", "tempor", "incididunt", "labore",
    ];
    let mut lcg = Lcg::new(0x5eed);
    let mut out = String::with_capacity(size + 16);
    while out.len() < size {
        out.push_str(lcg.pick::<&str>(WORDS));
        out.push(' ');
    }
    out.truncate(size);
    out
}

/// Verify that [`measure`] charges the value's drop to its own region.
fn measure_charges_the_drop() {
    let held = measure(|| Vec::<u8>::with_capacity(1 << 20));
    assert!(held.peak >= 1 << 20, "the allocation is inside the region");
    let after = Counters::start();
    std::hint::black_box(1u64 + 1);
    assert_eq!(
        after.since().peak,
        0,
        "and it was freed before the next one"
    );
}
