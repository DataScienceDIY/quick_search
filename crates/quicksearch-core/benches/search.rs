//! Search-path microbenchmarks.
//!
//! Every group pairs two ways of doing the same work in one run, so the
//! delta is a measurement rather than an estimate; a losing arm records
//! that an obvious-looking idea was measured and did not pay.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p quicksearch-core --bench search
//! ```
//!
//! Sizes come from `corpus::SIZES` — 1 KiB, 16 KiB and 256 KiB, the last
//! being `maximum_text_size`, the worst case a full-text row can present.

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

/// Decompressing the stored body — the first thing every full-text row does.
/// Reusing one context and pre-sizing the output is ~4x at 1 KiB, the size
/// most documents are; why `DocDecoder` exists.
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

/// Turning decompressed bytes into a `&str`: `from_utf8_lossy` copies and
/// scan-validates; `String::from_utf8` vector-validates and *moves*. A
/// 16-28x gap; `DocDecoder` borrows instead.
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

    /// The decompressed `Vec` is unshared, so `from_utf8` can move it;
    /// the lossy fallback keeps corrupt-row behaviour exactly.
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

/// ASCII-folding the document. Folding into a reused buffer is *not* faster:
/// `fold_into` is chosen for what it does to the allocator, not the clock —
/// do not "optimize" the other direction.
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
            // `make_ascii_lowercase` is byte-length preserving — the same
            // invariant the cascade relies on for folded offsets.
            buf.make_ascii_lowercase();
            buf.len()
        });
    }
}

/// Substring search: std's Two-Way against `memmem`'s SIMD prefilter. The
/// miss case matters most (a pass verifies far more rows than it accepts):
/// 111 µs against 2.4 µs at 256 KiB. `match_indices` stays as the guard —
/// convergence means the SIMD path stopped being selected.
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

    /// Hoisted `memmem::Finder`: measured, indistinguishable — the
    /// precompute is O(needle) and a term is a handful of bytes.
    #[divan::bench(args = corpus::SIZES)]
    fn memmem_per_call_miss(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 0).as_bytes();
        bencher.bench(|| {
            memchr::memmem::find_iter(divan::black_box(text), corpus::NEEDLE.as_bytes()).count()
        });
    }

    #[divan::bench(args = corpus::SIZES)]
    fn memmem_per_call_hits(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 64).as_bytes();
        bencher.bench(|| {
            memchr::memmem::find_iter(divan::black_box(text), corpus::NEEDLE.as_bytes()).count()
        });
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

    /// What `pass_fulltext` runs per row on the literal path: a
    /// case-sensitive count, then one folded extraction yielding both.
    #[divan::bench(args = corpus::SIZES)]
    fn cascade_row_sweeps(bencher: Bencher, size: usize) {
        let pattern = literal(corpus::NEEDLE);
        let text = corpus::text_mixed(size, 4);
        let folded = corpus::text_folded(size, 4);
        let opts = snippet::Options { approx_chars: 600 };
        bencher.bench(|| {
            let (s, b) = snippet::extract_folded(
                divan::black_box(text),
                divan::black_box(folded),
                &[corpus::NEEDLE],
                &opts,
            );
            let a = pattern.count(text, false);
            (a, b, s.ranges.len())
        });
    }

    /// The shape it replaced: a separate folded count sweeps the document a
    /// third time for a number the extraction already knew.
    #[divan::bench(args = corpus::SIZES)]
    fn cascade_row_sweeps_separate_count(bencher: Bencher, size: usize) {
        let pattern = literal(corpus::NEEDLE);
        let text = corpus::text_mixed(size, 4);
        let folded = corpus::text_folded(size, 4);
        let opts = snippet::Options { approx_chars: 600 };
        bencher.bench(|| {
            let a = pattern.count(divan::black_box(text), false);
            let b = pattern.count_folded(divan::black_box(folded));
            let (s, _) = snippet::extract_folded(text, folded, &[corpus::NEEDLE], &opts);
            (a, b, s.ranges.len())
        });
    }
}

/// Snippet extraction against a pre-folded haystack — now the only folded
/// sweep of a row, and the source of its count.
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

/// The filename pass's per-row ladder over 2000 realistic rows. The two
/// obvious fixes each measure *worse* alone (reused fold buffer, prebuilt
/// `Finder`); only together do they win, and only by ~1.2x — short strings
/// do not behave like document bodies.
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

    /// Isolates the searcher from the allocation.
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

    /// memchr2 candidate scan: measured, not worth it on short haystacks.
    #[divan::bench]
    fn find_first_ci_memchr2(bencher: Bencher) {
        let rows = corpus::rows();
        let needle = corpus::NEEDLE.as_bytes();
        fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
            let (lo, up) = (
                needle[0].to_ascii_lowercase(),
                needle[0].to_ascii_uppercase(),
            );
            let last = hay.len().checked_sub(needle.len())?;
            let mut at = 0usize;
            while at <= last {
                let i = at + memchr::memchr2(lo, up, &hay[at..=last])?;
                if hay[i..i + needle.len()].eq_ignore_ascii_case(needle) {
                    return Some(i);
                }
                at = i + 1;
            }
            None
        }
        bencher.bench(|| {
            let mut found = 0usize;
            for row in divan::black_box(rows) {
                if find(row.name.as_bytes(), needle).is_some()
                    || find(row.path.as_bytes(), needle).is_some()
                {
                    found += 1;
                }
            }
            found
        });
    }
}

/// Bitap, the fuzzy passes' inner loop — whole-table scans, so it runs over
/// every row when fuzzy is on. `step` takes `&mut [u64]`, so indices are
/// bounds-checked and the trip count is opaque to the optimizer.
mod bitap {
    use super::*;

    #[divan::bench(args = corpus::SIZES)]
    fn count_and_first_k2(bencher: Bencher, size: usize) {
        let bitap = Bitap::new(corpus::NEEDLE.as_bytes(), 2).unwrap();
        let hay = corpus::text(size, 4).as_bytes();
        bencher.bench(|| bitap.count_and_first(divan::black_box(hay)));
    }

    /// The filename pass's shape: many short haystacks, the register memset
    /// amortized over very little work.
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
