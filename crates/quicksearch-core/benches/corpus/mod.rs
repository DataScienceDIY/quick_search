//! Shared fixtures for the `search` and `index` benchmarks.
//!
//! Everything here is deterministic — the same LCG `tests/search_perf.rs` and
//! `examples/indexprobe.rs` use, seeded identically. A fixed corpus is what
//! makes two runs comparable, so a number that moved is a real change rather
//! than a different document.
//!
//! Corpora are built once per process and shared by every benchmark that
//! wants them. Divan re-runs a benchmarked closure thousands of times; paying
//! 256 KiB of text generation inside that loop would measure the generator.

// Both bench binaries compile the whole module but each uses only part of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::LazyLock;

/// Deterministic pseudo-random word picker.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }
}

/// Filler vocabulary. Deliberately excludes [`NEEDLE`] and every prefix of it
/// past two characters, so the only occurrences in a document are the ones
/// planted on purpose and a "zero hits" corpus really has zero.
const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "sigma", "report", "summary", "meeting", "invoice", "contract", "budget", "revenue",
    "quarter", "planning", "review", "draft", "final", "notes", "appendix", "figure",
];

/// The term every search benchmark looks for. Nine bytes, no overlap with
/// [`WORDS`], and long enough to clear the trigram floor the full-text pass
/// applies.
pub const NEEDLE: &str = "quartzite";

/// The three document sizes every size-swept benchmark uses, in bytes.
///
/// 1 KiB is a short note, 16 KiB a typical source file or README, and 256 KiB
/// is `maximum_text_size` — the ceiling `config.processing` lets into the
/// index, and so the worst case a full-text pass has to survive per row.
pub const SIZES: [usize; 3] = [1 << 10, 16 << 10, 256 << 10];

/// Occurrence counts to sweep. Zero is the important one: a full-text pass
/// verifies far more candidate rows than it accepts, because the trigram
/// index matches on character triples rather than the whole term.
pub const HITS: [usize; 3] = [0, 4, 64];

/// Word text of about `size` bytes with [`NEEDLE`] planted `hits` times at
/// even spacing.
///
/// Even spacing matters: it means a count has to scan to the end of the
/// document, and a snippet window never contains every match. Both are what
/// the cascade actually does.
pub fn document(size: usize, hits: usize) -> String {
    let mut lcg = Lcg::new(0x5eed);
    let mut out = String::with_capacity(size + 16);
    let stride = if hits == 0 { usize::MAX } else { size / hits };
    let mut next_plant = stride;
    while out.len() < size {
        if out.len() >= next_plant {
            out.push_str(NEEDLE);
            out.push(' ');
            next_plant = next_plant.saturating_add(stride);
            continue;
        }
        out.push_str(WORDS[lcg.next() as usize % WORDS.len()]);
        out.push(' ');
    }
    out
}

/// [`document`], but with the planted term capitalised so a case-sensitive
/// scan misses and only the folded one hits.
///
/// This is the stage-6 row and the tier-4 row — the case the cascade pays a
/// fold for, and the one a corpus of all-lowercase text would never produce.
pub fn document_mixed_case(size: usize, hits: usize) -> String {
    let mut needle = NEEDLE.to_string();
    needle.replace_range(0..1, &NEEDLE[0..1].to_uppercase());
    document(size, hits).replace(NEEDLE, &needle)
}

type Corpus = HashMap<(usize, usize), String>;

fn build(f: fn(usize, usize) -> String) -> Corpus {
    let mut m = HashMap::new();
    for size in SIZES {
        for hits in HITS {
            m.insert((size, hits), f(size, hits));
        }
    }
    m
}

static LOWER: LazyLock<Corpus> = LazyLock::new(|| build(document));
static MIXED: LazyLock<Corpus> = LazyLock::new(|| build(document_mixed_case));
static FOLDED: LazyLock<Corpus> = LazyLock::new(|| {
    MIXED
        .iter()
        .map(|(k, v)| (*k, v.to_ascii_lowercase()))
        .collect()
});
static BLOBS: LazyLock<HashMap<(usize, usize), Vec<u8>>> = LazyLock::new(|| {
    LOWER
        .iter()
        .map(|(k, v)| (*k, zstd::encode_all(v.as_bytes(), 3).expect("encode")))
        .collect()
});

/// An all-lowercase document: a case-sensitive scan finds every planted term.
pub fn text(size: usize, hits: usize) -> &'static str {
    &LOWER[&(size, hits)]
}

/// A document whose planted terms are capitalised: case-sensitive misses,
/// folded hits.
pub fn text_mixed(size: usize, hits: usize) -> &'static str {
    &MIXED[&(size, hits)]
}

/// [`text_mixed`] pre-folded, for the benchmarks that measure a search
/// against a haystack the caller already lowered.
pub fn text_folded(size: usize, hits: usize) -> &'static str {
    &FOLDED[&(size, hits)]
}

/// [`text`] as stored: zstd level 3, exactly what `db/repo.rs` writes.
pub fn blob(size: usize, hits: usize) -> &'static [u8] {
    &BLOBS[&(size, hits)]
}

/// Realistic file names and full paths, for the benchmarks that model the
/// filename pass rather than the full-text one.
///
/// The name pass scans the whole `files` table, so what matters is the
/// per-row cost on a *miss* — most rows match the SQL `LIKE` on some
/// directory component and then fail every name tier.
pub struct Row {
    pub name: String,
    pub path: String,
}

static ROWS: LazyLock<Vec<Row>> = LazyLock::new(|| {
    let mut lcg = Lcg::new(0xd00d);
    (0..2000)
        .map(|i| {
            let w1 = WORDS[lcg.next() as usize % WORDS.len()];
            let w2 = WORDS[lcg.next() as usize % WORDS.len()];
            let name = format!("{}-{}-{:05}.txt", w1, w2, i);
            // Mixed case in the directory portion, so the folded tiers are
            // the ones that resolve — the common shape on real trees.
            let path = format!("/home/user/Documents/Quartzite/{:03}/{}", i % 40, name);
            Row { name, path }
        })
        .collect()
});

pub fn rows() -> &'static [Row] {
    &ROWS
}

/// The head of a plain-text file, as the walk reads it: `hash_length`
/// (8 KiB) bytes or the whole file, whichever is smaller.
pub fn text_head() -> &'static [u8] {
    static HEAD: LazyLock<Vec<u8>> =
        LazyLock::new(|| document(8 << 10, 2).into_bytes()[..8 << 10].to_vec());
    &HEAD
}

/// A binary head that no extractor claims — the control group for the MIME
/// sniff, which has to reject it by scanning.
pub fn binary_head() -> &'static [u8] {
    static HEAD: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let mut lcg = Lcg::new(0xbeef);
        (0..8 << 10).map(|_| (lcg.next() & 0xff) as u8).collect()
    });
    &HEAD
}
