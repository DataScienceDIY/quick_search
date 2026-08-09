//! Search-path microbenchmarks.
//!
//! Every group pairs two ways of doing the same work in one run, so the delta
//! is a measurement rather than an estimate. Some pairs justify a choice the
//! code has already made; others record something tried and rejected. Both are
//! worth keeping — a losing arm is the cheapest documentation there is that an
//! obvious-looking idea was measured and did not pay.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p quicksearch-core --bench search
//! ```
//!
//! Sizes come from `corpus::SIZES` — 1 KiB, 16 KiB and 256 KiB, the last
//! being `maximum_text_size`, the largest document the index will hold and so
//! the worst case a full-text row can present.

mod corpus;

use divan::Bencher;
use quicksearch_core::query::pattern::{TermPart, TermPattern};
use quicksearch_core::search::fuzzy::Bitap;
use quicksearch_core::snippet;

fn main() {
    divan::main();
}

fn literal(term: &str) -> TermPattern {
    TermPattern::build(&[TermPart {
        text: term.to_string(),
        glob: false,
    }])
    .expect("literal patterns always compile")
}

/// Decompressing the stored document body — the first thing every full-text
/// row does, at `search/cascade/passes.rs:241`, `:425` and `:528`.
///
/// `decode_all` builds a fresh `ZSTD_DCtx` and a ~131 KB `BufReader` per call,
/// then grows an unsized `Vec` as it goes; the other arm reuses one context and
/// sizes the output up front. Measured at 8.4/16.0/102 µs against
/// 1.6/8.0/89 µs — a 4.4x gap at 1 KiB, which is the size most documents are.
/// This is why `DocDecoder` exists.
mod zstd_decode {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn decode_all(bencher: Bencher, size: usize) {
        let blob = corpus::blob(size, 4);
        bencher.bench(|| zstd::decode_all(divan::black_box(blob)).unwrap());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn bulk_reused(bencher: Bencher, size: usize) {
        let blob = corpus::blob(size, 4);
        let capacity = corpus::text(size, 4).len();
        let mut dec = zstd::bulk::Decompressor::new().unwrap();
        bencher.bench_local(move || dec.decompress(divan::black_box(blob), capacity).unwrap());
    }
}

/// Turning decompressed bytes into a `&str`.
///
/// `from_utf8_lossy(..).into_owned()` copies the whole document even when the
/// bytes are already valid UTF-8 — and they always are, since
/// `textenc::decode_text` is the only thing that writes them. It is also far
/// slower than it looks: its validation is a scanning loop, where
/// `String::from_utf8` uses the vectorized one and *moves* the buffer it
/// validates. 230/6360/52200 ns against 15/227/3200 ns, a 16-28x gap that is
/// mostly validation rather than the copy. `DocDecoder` borrows instead.
mod utf8 {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn lossy_into_owned(bencher: Bencher, size: usize) {
        let raw = corpus::text(size, 4).as_bytes();
        bencher.bench(|| String::from_utf8_lossy(divan::black_box(raw)).into_owned());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn lossy_borrowed(bencher: Bencher, size: usize) {
        let raw = corpus::text(size, 4).as_bytes();
        bencher.bench(|| {
            let cow = String::from_utf8_lossy(divan::black_box(raw));
            cow.len()
        });
    }

    /// The decompressor hands back an owned `Vec<u8>` that nothing else
    /// references, so `String::from_utf8` can validate and *move* it rather
    /// than validate and copy. Falling back to `from_utf8_lossy` on error
    /// keeps the current behaviour for a corrupt row exactly.
    #[divan::bench(args = corpus::SIZES)]
    fn from_utf8_move(bencher: Bencher, size: usize) {
        let raw = corpus::text(size, 4).as_bytes();
        bencher
            .with_inputs(|| raw.to_vec())
            .bench_values(|owned| match String::from_utf8(owned) {
                Ok(s) => s,
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            });
    }
}

/// ASCII-folding the document, which every full-text row needs for the
/// case-insensitive count and the snippet.
///
/// A result worth keeping visible: folding into a reused buffer is *not*
/// faster. `to_ascii_lowercase` allocates and folds in one pass, where
/// clear + `push_str` + `make_ascii_lowercase` walks the bytes twice, and at
/// 256 KiB the reused buffer measures slightly behind. `fold_into` is chosen
/// for what it does to the allocator, not to the clock — do not "optimize" the
/// other direction on the assumption that removing an allocation must win.
mod fold {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn to_ascii_lowercase(bencher: Bencher, size: usize) {
        let text = corpus::text_mixed(size, 4);
        bencher.bench(|| divan::black_box(text).to_ascii_lowercase());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn into_reused_buffer(bencher: Bencher, size: usize) {
        let text = corpus::text_mixed(size, 4);
        let mut buf = String::new();
        bencher.bench_local(move || {
            buf.clear();
            buf.push_str(divan::black_box(text));
            // SAFETY-free equivalent of the in-place fold: `make_ascii_lowercase`
            // is byte-length preserving, which is the same invariant the
            // cascade already relies on for folded offsets.
            buf.make_ascii_lowercase();
            buf.len()
        });
    }
}

/// Substring search over a document body: `str::match_indices` (std's Two-Way
/// searcher) against `memchr::memmem` (Two-Way plus a SIMD prefilter).
///
/// The miss case matters most. The trigram index matches on character triples,
/// so a full-text pass verifies far more rows than it accepts, and a miss scans
/// the whole document before giving up. At 256 KiB that is 111 µs against
/// 2.4 µs — the measurement `snippet.rs` uses `memmem` for. `match_indices`
/// stays here as the regression guard: if these two ever converge, the SIMD
/// path has stopped being selected.
mod substring {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn match_indices_miss(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 0);
        bencher.bench(|| divan::black_box(text).match_indices(corpus::NEEDLE).count());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn memmem_miss(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 0).as_bytes();
        let finder = memchr::memmem::Finder::new(corpus::NEEDLE);
        bencher.bench(|| finder.find_iter(divan::black_box(text)).count());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn match_indices_hits(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 64);
        bencher.bench(|| divan::black_box(text).match_indices(corpus::NEEDLE).count());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn memmem_hits(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 64).as_bytes();
        let finder = memchr::memmem::Finder::new(corpus::NEEDLE);
        bencher.bench(|| finder.find_iter(divan::black_box(text)).count());
    }

    /// What `pass_fulltext` actually runs per row, through the real crate
    /// entry points: a case-sensitive count, then a folded count, then the
    /// snippet extraction. Three sweeps of the same document.
    #[divan::bench(args = corpus::SIZES)]
    fn cascade_row_sweeps(bencher: Bencher, size: usize) {
        let pattern = literal(corpus::NEEDLE);
        let text = corpus::text_mixed(size, 4);
        let folded = corpus::text_folded(size, 4);
        let opts = snippet::Options { approx_chars: 600 };
        bencher.bench(|| {
            let a = pattern.count(divan::black_box(text), false);
            let b = pattern.count_folded(divan::black_box(folded));
            let s = snippet::extract_folded(text, folded, &[corpus::NEEDLE], &opts);
            (a, b, s.ranges.len())
        });
    }
}

/// Snippet extraction against a pre-folded haystack, the third of those
/// sweeps. Also carries a per-call `term.to_ascii_lowercase()` at
/// `snippet.rs:81` for a needle the caller already holds folded.
mod snippet_extract {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn extract_folded(bencher: Bencher, size: usize) {
        let text = corpus::text_mixed(size, 64);
        let folded = corpus::text_folded(size, 64);
        let opts = snippet::Options { approx_chars: 600 };
        bencher.bench(|| {
            snippet::extract_folded(
                divan::black_box(text),
                divan::black_box(folded),
                &[corpus::NEEDLE],
                &opts,
            )
        });
    }
}

/// The filename pass's per-row ladder, over 2000 realistic name/path rows.
///
/// `pass_filename` scans the whole `files` table — its `LIKE '%term%'`
/// predicate can use no index — and tiers 4 and 10 both run a
/// case-insensitive find, so a row matching on its directory portion pays
/// twice.
///
/// The instructive part is that the two obvious fixes each make it *worse*
/// alone: a reused fold buffer measures ~2x slower than folding into a fresh
/// allocation, and a prebuilt `memmem::Finder` is slower than `str::find` on
/// haystacks this short. Only together do they win, and only by ~1.2x. Short
/// strings do not behave like document bodies; measure them separately.
mod filename_ladder {
    use super::*;

    #[divan::bench]
    fn find_first_ci_current(bencher: Bencher) {
        let pattern = literal("quartzite");
        let rows = corpus::rows();
        bencher.bench(|| {
            let mut found = 0usize;
            for row in divan::black_box(rows) {
                if pattern.find_first(&row.name, true).is_some()
                    || pattern.find_first(&row.path, true).is_some()
                {
                    found += 1;
                }
            }
            found
        });
    }

    #[divan::bench]
    fn find_first_ci_scratch(bencher: Bencher) {
        let pattern = literal("quartzite");
        let rows = corpus::rows();
        let mut scratch = String::new();
        bencher.bench_local(move || {
            let mut found = 0usize;
            for row in divan::black_box(rows) {
                scratch.clear();
                scratch.push_str(&row.name);
                scratch.make_ascii_lowercase();
                if pattern.find_first_folded(&scratch).is_some() {
                    found += 1;
                    continue;
                }
                scratch.clear();
                scratch.push_str(&row.path);
                scratch.make_ascii_lowercase();
                if pattern.find_first_folded(&scratch).is_some() {
                    found += 1;
                }
            }
            found
        });
    }

    /// Fold as today, but search the folded copy with a `Finder` built once
    /// per query instead of `str::find`'s Two-Way. Isolates the searcher from
    /// the allocation: if this wins and `find_first_ci_scratch` does not, the
    /// fold was never the problem.
    #[divan::bench]
    fn find_first_ci_memmem(bencher: Bencher) {
        let finder = memchr::memmem::Finder::new("quartzite");
        let rows = corpus::rows();
        bencher.bench(|| {
            let mut found = 0usize;
            for row in divan::black_box(rows) {
                if finder
                    .find(row.name.to_ascii_lowercase().as_bytes())
                    .is_some()
                    || finder
                        .find(row.path.to_ascii_lowercase().as_bytes())
                        .is_some()
                {
                    found += 1;
                }
            }
            found
        });
    }

    /// Both at once: one reused fold buffer and a prebuilt `Finder`.
    #[divan::bench]
    fn find_first_ci_scratch_memmem(bencher: Bencher) {
        let finder = memchr::memmem::Finder::new("quartzite");
        let rows = corpus::rows();
        let mut scratch = String::new();
        bencher.bench_local(move || {
            let mut found = 0usize;
            for row in divan::black_box(rows) {
                scratch.clear();
                scratch.push_str(&row.name);
                scratch.make_ascii_lowercase();
                if finder.find(scratch.as_bytes()).is_some() {
                    found += 1;
                    continue;
                }
                scratch.clear();
                scratch.push_str(&row.path);
                scratch.make_ascii_lowercase();
                if finder.find(scratch.as_bytes()).is_some() {
                    found += 1;
                }
            }
            found
        });
    }
}

/// Bitap, the fuzzy passes' inner loop. Both fuzzy passes are whole-table
/// scans, so this runs over every row in the index when fuzzy is on.
///
/// `step` still takes `&mut [u64]` rather than `&mut [u64; MAX_REGISTERS]`,
/// so the register indices are bounds-checked and the trip count is opaque
/// to the optimizer.
mod bitap {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn count_and_first_k2(bencher: Bencher, size: usize) {
        let bitap = Bitap::new(corpus::NEEDLE.as_bytes(), 2).unwrap();
        let hay = corpus::text(size, 4).as_bytes();
        bencher.bench(|| bitap.count_and_first(divan::black_box(hay)));
    }

    /// The filename pass's shape: many short haystacks rather than one long
    /// one, with the per-call 176-byte register memset amortized over very
    /// little work.
    #[divan::bench]
    fn best_distance_over_names_k2(bencher: Bencher) {
        let bitap = Bitap::new(b"quartzite", 2).unwrap();
        let rows = corpus::rows();
        bencher.bench(|| {
            let mut hits = 0usize;
            for row in divan::black_box(rows) {
                if bitap.best_distance_and_first(row.name.as_bytes()).is_some() {
                    hits += 1;
                }
            }
            hits
        });
    }
}
