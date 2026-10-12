//! Mutation fuzzing for the extractors. Every corpus sample is damaged at the
//! byte level, deterministically, and each damaged file must come back from
//! its extractor as text or an error: within the watchdog
//! ([`extract::on_worker`]), and never as a panic, not even one the
//! registry's guard contains. The PDF extractor is the one that may return a
//! panic as its error: it runs `pdf_extract`, which panics on malformed input
//! by design, behind a guard of its own (`extract::pdf::PdfExtractor`).
//!
//! The extractors run in-process here, as in core's other tests, while the
//! app and the MCP server parse PDF, audio and legacy Office files in a
//! sandbox child (`extract::sandbox`). A damaged file a parser answers by
//! aborting the process is the sandbox's to contain, and would end this run,
//! so a known one is skipped ([`aborts_in_process`]);
//! `quicksearch-gui/tests/extract_sandbox.rs` runs it through the sandbox.
//!
//! The damage is what a cut-short copy, a bad sector or a hostile writer
//! leaves: a byte flipped, the tail cut off, a run deleted, a run repeated in
//! place. A damaged file stays within a few KiB of its sample, so a hang or a
//! runaway allocation is the extractor's doing, not the input's size.
//!
//! Each damaged file is dispatched by the MIME its intact sample sniffs as,
//! so the damage reaches its own format's parser rather than whichever one a
//! damaged head sniffs as. It goes through both entry points the indexer
//! uses: the content pass ([`Registry::extract`]) and, for the formats that
//! opt in, the walk-time head ([`Registry::extract_complete_head`]).
//!
//! The damage comes from an LCG with a fixed seed per sample, so a failure
//! reproduces by rerunning: it names the sample and the iteration, and keeps
//! the damaged file. The kept file is the one reproduction for a sample
//! whose writer stamps the time into it, as `rust_xlsxwriter` does. An
//! abort (a stack overflow, a failed allocation) ends the process before a
//! case is named; the damaged file it leaves under the `extract-fuzz`
//! scratch directory is the evidence.
//!
//! ```text
//! cargo test --release -p quicksearch-core --test extract_fuzz
//! QSB_FUZZ_ITERS=2000 cargo test --release -p quicksearch-core \
//!     --test extract_fuzz -- --nocapture
//! ```

mod corpus;

use std::panic::AssertUnwindSafe;

use quicksearch_core::config::Config;
use quicksearch_core::extract::{self, on_worker, Registry};
use quicksearch_core::mime;
use quicksearch_core::testutil::{Lcg, Scratch};

/// Damaged files per sample unless `QSB_FUZZ_ITERS` says otherwise: the
/// reduced pass, under a second for the whole corpus in a debug build.
const ITERS_PER_SAMPLE: usize = 12;

/// A sample's damage is seeded with this plus its index in the corpus.
const SEED: u64 = 0xF022_E5A1;

/// Most damages stacked on one file.
const MAX_DAMAGES: u64 = 3;

/// Longest run one damage deletes or repeats, and the most copies of it a
/// repeat inserts.
const MAX_RUN: usize = 64;

const MAX_COPIES: usize = 16;

/// How much the MIME sniff is shown: the indexer's default, as in
/// `extraction_corpus.rs`.
const HEAD: usize = 8 * 1024;

/// How an extractor's panic reads once the registry's guard has caught it.
const CONTAINED_PANIC: &str = "extractor panicked on ";

fn iters_per_sample() -> usize {
    std::env::var("QSB_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ITERS_PER_SAMPLE)
}

/// Uniform in `0..n`.
fn below(lcg: &mut Lcg, n: usize) -> usize {
    lcg.next_u64() as usize % n
}

/// One damage, chosen and placed by `lcg`. Half are flipped bytes: a flip
/// keeps every offset, so it is the damage that gets past a zip's or an
/// OLE2 file's directory to the parser behind it; a cut, a deletion or a
/// repeat mostly breaks the directory. An empty file, which a cut can leave,
/// takes no further damage.
fn damage(bytes: &mut Vec<u8>, lcg: &mut Lcg) {
    if bytes.is_empty() {
        return;
    }
    let at = below(lcg, bytes.len());
    let run = 1 + below(lcg, MAX_RUN.min(bytes.len() - at));
    match below(lcg, 6) {
        0..=2 => bytes[at] ^= 1 + below(lcg, 255) as u8,
        3 => bytes.truncate(at),
        4 => {
            bytes.drain(at..at + run);
        }
        _ => {
            let copies = bytes[at..at + run].repeat(1 + below(lcg, MAX_COPIES));
            bytes.splice(at..at, copies);
        }
    }
}

/// Whether `bytes` are damage a parser is known to answer by aborting the
/// process, past any guard: lofty 0.19.2 sizes a FLAC's Vorbis comment list
/// from the count the block declares (`ogg/read.rs:79`), past its own
/// allocation limit, so a count the block cannot hold (each comment takes at
/// least its 4-byte length) asks for up to 200 GB. The blocks are walked as
/// lofty walks them; any other file is not one.
fn aborts_in_process(bytes: &[u8]) -> bool {
    const VORBIS_COMMENT: u8 = 4;
    let u32_le = |b: &[u8], at: usize| -> Option<usize> {
        Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
    };
    let Some(mut rest) = bytes.strip_prefix(b"fLaC") else {
        return false;
    };
    // Each block: a byte holding the last-block flag and the type, a 24-bit
    // big-endian length, then that many bytes.
    while let [header, a, b, c, tail @ ..] = rest {
        let len = u32::from_be_bytes([0, *a, *b, *c]) as usize;
        let Some(block) = tail.get(..len) else {
            return false;
        };
        if header & 0x7F == VORBIS_COMMENT {
            let declared = u32_le(block, 0).and_then(|vendor| {
                let count = u32_le(block, 4usize.checked_add(vendor)?)?;
                Some((count, block.len().saturating_sub(vendor.saturating_add(8))))
            });
            if declared.is_some_and(|(count, left)| count > left / 4) {
                return true;
            }
        }
        if header & 0x80 != 0 {
            return false;
        }
        rest = &tail[len..];
    }
    false
}

/// `entry`'s result must be text or an error that is not a contained panic.
fn check(case: &str, entry: &str, result: Result<(), String>) {
    if let Err(e) = result {
        assert!(!e.starts_with(CONTAINED_PANIC), "{case}: {entry} panicked: {e}");
    }
}

#[test]
fn every_mutated_sample_returns_without_panicking() {
    let (_corpus_dir, samples) = corpus::build("extract-fuzz-corpus");
    let damaged_dir = Scratch::dir("extract-fuzz");
    let iters = iters_per_sample();

    for (index, sample) in samples.iter().enumerate() {
        // Seeded per sample, so its damage does not depend on the bytes of
        // the samples before it, which are not all reproducible.
        let mut lcg = Lcg::new(SEED + index as u64);
        let intact = std::fs::read(&sample.path)
            .unwrap_or_else(|e| panic!("read {}: {e}", sample.path.display()));
        let mime = mime::guess_mime_from_head(&sample.path, &intact[..intact.len().min(HEAD)])
            .unwrap_or_else(|| panic!("{}: no MIME resolved", sample.label));
        // Or the damage would reach no parser at all.
        assert!(
            Registry::default_set().supports(mime),
            "{}: MIME {mime:?} is claimed by no extractor",
            sample.label
        );
        let path = damaged_dir.join(sample.path.file_name().expect("a sample has a name"));

        for iter in 0..iters {
            let mut bytes = intact.clone();
            for _ in 0..=lcg.next_u64() % MAX_DAMAGES {
                damage(&mut bytes, &mut lcg);
            }
            if aborts_in_process(&bytes) {
                continue;
            }
            std::fs::write(&path, &bytes)
                .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            let case = format!(
                "{} iteration {iter} of {iters} (QUICKSEARCH_CORPUS_SEED={}, damaged file {})",
                sample.label,
                corpus::seed(),
                path.display()
            );

            let job = {
                let path = path.clone();
                move || {
                    let registry = Registry::default_set();
                    let mut scratch = extract::Scratch::new(&Config::default());
                    let mut out = String::new();
                    let content = registry
                        .extract(&path, mime, &mut out, &mut scratch)
                        .map(drop);
                    out.clear();
                    let head = registry.extract_complete_head(&path, mime, &bytes, &mut out);
                    (content, head)
                }
            };
            // `on_worker` panics on a hang, or on a panic the registry did
            // not catch; caught here only to name the case.
            let (content, head) = std::panic::catch_unwind(AssertUnwindSafe(|| on_worker(job)))
                .unwrap_or_else(|_| panic!("{case}: hung, or panicked past the registry"));
            check(&case, "content extraction", content);
            if let Some(head) = head {
                check(&case, "head extraction", head);
            }
        }
    }

    // Vacuity guard: a corpus that stopped generating would pass everything above.
    assert!(
        samples.len() >= 35,
        "corpus shrank to {} samples",
        samples.len()
    );
}
