//! Indexing-path microbenchmarks.
//!
//! Same convention as `benches/search.rs`: each group pairs two ways of doing
//! the same work, measured together, so a choice is justified rather than
//! asserted — and a losing arm records something already tried.
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

/// Compressing an extracted document, which `repo::set_content_done` used to
/// do itself — on the single writer thread, inside the transaction, and so
/// inside the `conn_mutex` hold `store_extracted` takes across a whole chunk.
///
/// `encode_all` allocates and tears down a fresh `ZSTD_CCtx` — window, hash
/// and chain tables — per document, and for the small documents that dominate
/// a real tree that setup costs more than the compression. `batch_*` below
/// measures a whole chunk of it, which is what `compress_bodies` now does
/// before taking the lock.
mod zstd_encode {
    use super::*;

    /// Level 3, matching `db/repo.rs`'s `ZSTD_LEVEL`. Not a variable here:
    /// the level is argued in place and this measures the machinery around
    /// it, not the level.
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

    /// One writer chunk: `processing.batch_size` documents of the size most
    /// documents are.
    ///
    /// The `encode_all` figure is how much pure CPU used to sit inside the
    /// `conn_mutex` hold. That lock serializes the indexer against itself —
    /// not against search, which holds its own connection and reads through
    /// WAL — so this is a contention figure, not a throughput one. An
    /// end-to-end cold index of an 80 MiB tree does not move measurably: FTS5
    /// trigram tokenization dominates it, and all compression together is
    /// under 1% of the run.
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

/// What a small text file costs between the MIME sniff and the decode.
///
/// The walk reads an 8 KiB head, sniffs it (`looks_like_text` → `classify`, a
/// control-byte scan plus a UTF-8 validation), and then — for a file the head
/// covers entirely — decodes it inline (`decode_text` → `classify` again, on
/// the same bytes). So `classify` runs twice on identical bytes, and
/// `sniff_only` says the second pass is about half of `sniff_then_decode`.
///
/// Left alone deliberately. The two calls pass different `truncated` flags and
/// genuinely disagree on a file ending mid-multibyte-sequence, so sharing a
/// verdict means threading `TextClass` through `mime.rs`, the `Extractor`
/// trait and `plaintext.rs`. Against a cold `indexprobe` run that whole
/// pipeline is under 1% of wall time — the redundancy is real and still not
/// worth the coupling. Re-measure here before deciding otherwise.
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

    /// Includes the `head.to_vec()` at `extract/plaintext.rs:113` — a full
    /// copy of the head made only because `decode_text` takes ownership.
    #[divan::bench]
    fn decode_only(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| textenc::decode_text(divan::black_box(head).to_vec(), path));
    }
}

/// The MIME sniff itself, once per new or changed file.
///
/// It carries several `to_ascii_lowercase` allocations for extension and MIME
/// comparisons that `eq_ignore_ascii_case` would do without allocating — and
/// removing them is not worth doing: the whole sniff is ~180 ns on a text head,
/// against ~2.2 µs for the `classify` next to it. This group exists to keep
/// that ratio visible.
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

    /// No extension to go on, so the sniff falls all the way through to the
    /// magic-byte scan and the text classifier.
    #[divan::bench]
    fn no_extension(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/LICENSE");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }
}

/// A SHA-256 over the path string, per file, on every run — including the
/// unchanged steady state, where it is the only compute a file costs beyond
/// its `statx`.
///
/// Reference measurement only. The plan does not propose changing it: its
/// rationale is collision resistance against adversarial filenames on shared
/// volumes, and this is here to confirm the cost is small enough that the
/// argument stands unchallenged.
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
