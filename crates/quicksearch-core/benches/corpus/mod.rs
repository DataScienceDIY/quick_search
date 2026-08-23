//! Shared fixtures for the `search` and `index` benchmarks.
//!
//! Deterministic — the same LCG `search_perf.rs` and `indexprobe.rs` use —
//! so two runs are comparable. Corpora are built once per process: Divan
//! re-runs a closure thousands of times, and generating 256 KiB inside that
//! loop would measure the generator.

// Both bench binaries compile the whole module but each uses only part of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::LazyLock;

pub use quicksearch_core::testutil::Lcg;

/// Excludes [`NEEDLE`] and every prefix of it past two characters, so a
/// "zero hits" corpus really has zero.
const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "sigma", "report", "summary", "meeting", "invoice", "contract", "budget", "revenue",
    "quarter", "planning", "review", "draft", "final", "notes", "appendix", "figure",
];

/// Nine bytes, no overlap with [`WORDS`], clears the trigram floor.
pub const NEEDLE: &str = "quartzite";

/// 1 KiB, 16 KiB and 256 KiB — the last is `maximum_text_size`, the worst
/// case a full-text pass survives per row.
pub const SIZES: [usize; 3] = [1 << 10, 16 << 10, 256 << 10];

/// Zero is the important one: the trigram index matches character triples,
/// so a full-text pass verifies far more rows than it accepts.
pub const HITS: [usize; 3] = [0, 4, 64];

/// ~`size` bytes with [`NEEDLE`] planted `hits` times at even spacing: a
/// count must scan to the end and a snippet window never holds every match,
/// which is what the cascade actually does.
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

/// [`document`] with the planted term capitalised: the stage-6/tier-4 row —
/// the case the cascade pays a fold for.
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

/// Capitalised planted terms: case-sensitive misses, folded hits.
pub fn text_mixed(size: usize, hits: usize) -> &'static str {
    &MIXED[&(size, hits)]
}

/// [`text_mixed`] pre-folded, for haystacks the caller already lowered.
pub fn text_folded(size: usize, hits: usize) -> &'static str {
    &FOLDED[&(size, hits)]
}

/// [`text`] as stored: zstd level 3, exactly what `db/repo.rs` writes.
pub fn blob(size: usize, hits: usize) -> &'static [u8] {
    &BLOBS[&(size, hits)]
}

/// Realistic names and paths for the filename pass, which scans the whole
/// table — what matters is the per-row cost on a *miss*.
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
            // Mixed case in the directory portion, so the folded tiers resolve.
            let path = format!("/home/user/Documents/Quartzite/{:03}/{}", i % 40, name);
            Row { name, path }
        })
        .collect()
});

pub fn rows() -> &'static [Row] {
    &ROWS
}

/// The head of a plain-text file, as the walk reads it.
pub fn text_head() -> &'static [u8] {
    static HEAD: LazyLock<Vec<u8>> =
        LazyLock::new(|| document(8 << 10, 2).into_bytes()[..8 << 10].to_vec());
    &HEAD
}

/// A binary head no extractor claims — the MIME sniff's control group.
pub fn binary_head() -> &'static [u8] {
    static HEAD: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let mut lcg = Lcg::new(0xbeef);
        (0..8 << 10).map(|_| (lcg.next() & 0xff) as u8).collect()
    });
    &HEAD
}
