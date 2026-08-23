//! Indexing-path microbenchmarks.
//!
//! Same convention as `benches/search.rs`: each group pairs two ways of
//! doing the same work, measured together.
//!
//! ```text
//! cargo bench -p quicksearch-core --bench index
//! ```

mod corpus;

use divan::Bencher;
use quicksearch_core::{mime, textenc, walk};

fn main() {
    divan::main();
}

/// Compressing an extracted document — pure CPU that used to sit inside the
/// `conn_mutex` hold; `batch_*` measures a whole chunk, which is what
/// `compress_bodies` now does before taking the lock.
mod zstd_encode {
    use super::*;

    /// Level 3, matching `db/repo.rs`'s `ZSTD_LEVEL`.
    const LEVEL: i32 = 3;

    #[divan::bench(args = corpus::SIZES)]
    fn encode_all(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 4).as_bytes();
        bencher.bench(|| zstd::encode_all(divan::black_box(text), LEVEL).unwrap());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn bulk_reused(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 4).as_bytes();
        let mut enc = zstd::bulk::Compressor::new(LEVEL).unwrap();
        bencher.bench_local(move || enc.compress(divan::black_box(text)).unwrap());
    }

    /// One writer chunk of typical documents. A contention figure, not a
    /// throughput one: the lock serializes the indexer against itself, and
    /// all compression together is under 1% of a cold run.
    const BATCH: usize = 500;

    #[divan::bench]
    fn batch_encode_all(bencher: Bencher) {
        let text = corpus::text(1 << 10, 4).as_bytes();
        bencher.bench(|| {
            (0..BATCH)
                .map(|_| {
                    zstd::encode_all(divan::black_box(text), LEVEL)
                        .unwrap()
                        .len()
                })
                .sum::<usize>()
        });
    }

    #[divan::bench]
    fn batch_bulk_reused(bencher: Bencher) {
        let text = corpus::text(1 << 10, 4).as_bytes();
        let mut enc = zstd::bulk::Compressor::new(LEVEL).unwrap();
        bencher.bench_local(move || {
            (0..BATCH)
                .map(|_| enc.compress(divan::black_box(text)).unwrap().len())
                .sum::<usize>()
        });
    }
}

/// What a small text file costs between the MIME sniff and the decode:
/// `classify` runs twice on identical bytes. Left alone deliberately — the
/// calls pass different `truncated` flags and genuinely disagree at a
/// mid-sequence EOF, and the redundancy is under 1% of a cold run;
/// re-measure here before coupling the layers to share a verdict.
mod text_pipeline {
    use super::*;
    use std::path::Path;

    #[divan::bench]
    fn sniff_then_decode(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| {
            let looks = textenc::looks_like_text(divan::black_box(head));
            let text = textenc::decode_text(head.to_vec(), path).unwrap();
            (looks, text.len())
        });
    }

    #[divan::bench]
    fn sniff_only(bencher: Bencher) {
        let head = corpus::text_head();
        bencher.bench(|| textenc::looks_like_text(divan::black_box(head)));
    }

    /// Includes the `head.to_vec()` made only because `decode_text` takes
    /// ownership.
    #[divan::bench]
    fn decode_only(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| textenc::decode_text(divan::black_box(head).to_vec(), path));
    }
}

/// The MIME sniff, once per new or changed file: ~180 ns on a text head
/// against ~2.2 µs for the `classify` next to it — its allocations are not
/// worth removing, and this group keeps that ratio visible.
mod mime_sniff {
    use super::*;
    use std::path::Path;

    #[divan::bench]
    fn text_head(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }

    #[divan::bench]
    fn binary_head(bencher: Bencher) {
        let head = corpus::binary_head();
        let path = Path::new("/tmp/bench/blob.bin");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }

    /// No extension, so the sniff falls through to magic bytes and the text
    /// classifier.
    #[divan::bench]
    fn no_extension(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/LICENSE");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }
}

/// SHA-256 over the path string, per file, every run. Reference only: it
/// confirms the cost is small enough that the collision-resistance argument
/// stands.
mod path_digest {
    use super::*;

    #[divan::bench]
    fn per_path(bencher: Bencher) {
        let rows = corpus::rows();
        bencher.bench(|| {
            let mut acc = 0u128;
            for row in divan::black_box(rows) {
                acc ^= walk::path_digest(&row.path);
            }
            acc
        });
    }
}
