//! A lipsum corpus in every format QuickSearch claims to extract text from.
//!
//! The per-extractor unit tests build fixtures with the libraries that read
//! them back; a writer and reader sharing a wrong assumption agree with each
//! other. This module is the other half: well-formed documents from
//! *foreign* producers — every writer differs from its reader.
//!
//! | format | written by | read by |
//! |---|---|---|
//! | docx | `docx-rs` (+ `zip` 8.x) | `zip` 0.6 + `quick-xml` |
//! | xlsx | `rust_xlsxwriter` | `zip` 0.6 + `quick-xml` |
//! | pptx, odt, ods, odp | [`zipwriter`] + `format!` | `zip` 0.6 + `quick-xml` |
//! | pdf | `pdf-writer` (typst) | `pdf-extract`/`lopdf` |
//! | mp3 | `id3` over hand-rolled MPEG frames | `lofty` |
//! | flac | `metaflac` over a committed silent stream | `lofty` |
//! | rtf | hand-written control words | `rtf-parser` |
//! | plain text | `std`, plus a hand-rolled cp1252 encoder | `encoding_rs` |
//! | doc, xls, ppt | LibreOffice, committed — see [`legacy`] | `cfb` |
//!
//! Two places take committed bytes — the legacy OLE2 binaries (`cfb` is the
//! only OLE2 writer in Rust, and it is the reader; see
//! `tests/fixtures/legacy/README.md`) and the `.flac`'s fifty milliseconds
//! of silence (`lofty` reads a real frame for stream properties; see
//! [`audio`]). In both the *text* still comes from the generator.
//!
//! Everything on-the-fly comes from one LCG seeded by [`seed`]
//! (`QUICKSEARCH_CORPUS_SEED` overrides); failing assertions print the seed.

// Only `extraction_corpus.rs` compiles this, and each writer exposes a
// little more surface than any one test needs.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub mod audio;
pub mod legacy;
pub mod odf;
pub mod ooxml;
pub mod pdf;
pub mod plaintext;
pub mod rtf;
pub mod zipwriter;

pub const DEFAULT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// The active seed; override with `QUICKSEARCH_CORPUS_SEED=<u64>`.
pub fn seed() -> u64 {
    match std::env::var("QUICKSEARCH_CORPUS_SEED") {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("QUICKSEARCH_CORPUS_SEED must be a u64, got {v:?}")),
        Err(_) => DEFAULT_SEED,
    }
}

pub use quicksearch_core::testutil::Lcg;

/// Plain ASCII lowercase so it survives every encoding unchanged, and long
/// enough that a drawn sentence is effectively unique.
const WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "consectetur",
    "adipiscing",
    "eiusmod",
    "tempor",
    "incididunt",
    "labore",
    "dolore",
    "aliqua",
    "veniam",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "commodo",
    "consequat",
    "voluptate",
    "cillum",
    "occaecat",
    "cupidatat",
    "proident",
    "officia",
    "deserunt",
    "mollit",
    "laborum",
];

/// Latin-1 representable, so it survives cp1252 and the PDF's WinAnsi font.
const LATIN1_PHRASE: &str = "café résumé naïve";

const UNICODE_PHRASE: &str = "Καλημέρα κόσμε";

/// What characters a format's *writer* can round-trip: neither lax (ASCII
/// everywhere) nor wrong (demanding Greek from a WinAnsi font).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Ascii,
    Latin1,
    Unicode,
}

const SENTENCES: usize = 6;

const WORDS_PER_SENTENCE: usize = 7;

/// The lipsum planted in one file, plus the token that identifies it.
/// Sentences are the unit of assertion for every format: prose formats write
/// one per paragraph, spreadsheets one per cell, decks one per shape, which
/// is what lets a single `must_contain` list describe them all.
pub struct Body {
    pub sentences: Vec<String>,
    /// Planted in sentence 1 and never in the file name.
    pub needle: String,
}

/// What every needle starts with — the same word as `common::BODY_TERM`, so
/// a hit is attributable to extraction. This module stays self-contained;
/// `corpus_needles_use_the_shared_body_term` keeps the two from drifting.
pub const NEEDLE_PREFIX: &str = "chalcedony";

impl Body {
    /// The needle is derived from the index alone, not from the LCG: it must
    /// stay distinct from every other file's under any seed, and under the
    /// trigram tokenizer "distinct" means "not a substring of another".
    pub fn new(lcg: &mut Lcg, index: usize, charset: Charset) -> Body {
        let needle = format!("{NEEDLE_PREFIX}{index:04}");
        let mut sentences = Vec::with_capacity(SENTENCES);
        for i in 0..SENTENCES {
            let mut words: Vec<String> = (0..WORDS_PER_SENTENCE)
                .map(|_| lcg.pick(WORDS).to_string())
                .collect();
            // Planted at fixed positions so a reordering bug fails the match.
            match i {
                1 => words.insert(0, needle.clone()),
                2 if charset != Charset::Ascii => words.insert(3, LATIN1_PHRASE.to_string()),
                3 if charset == Charset::Unicode => words.insert(3, UNICODE_PHRASE.to_string()),
                _ => {}
            }
            sentences.push(words.join(" "));
        }
        Body { sentences, needle }
    }
}

pub struct Sample {
    pub path: PathBuf,
    pub label: &'static str,
    /// Fragments that must appear in the extracted text, in this order.
    /// Ordered containment, not equality (readers drag in boilerplate and
    /// separators) and not set membership (text assembled out of order is
    /// exactly what a mis-read Word piece table produces, and must fail).
    pub must_contain: Vec<String>,
    /// Planted in the body and absent from the file name.
    pub needle: String,
    /// Whether the walk may extract this format without reopening the file
    /// (`Extractor::extract_from_head`); only plaintext and RTF do.
    pub head_path: bool,
}

impl Sample {
    fn prose(path: PathBuf, label: &'static str, body: &Body, head_path: bool) -> Sample {
        Sample {
            path,
            label,
            must_contain: body.sentences.clone(),
            needle: body.needle.clone(),
            head_path,
        }
    }
}

/// Where `fragments` stop matching `text` as an ordered subsequence: `Err`
/// names the first fragment not found, and where the scan had got to.
pub fn match_in_order(text: &str, fragments: &[String]) -> Result<(), String> {
    let mut cursor = 0usize;
    for (i, fragment) in fragments.iter().enumerate() {
        match text[cursor..].find(fragment.as_str()) {
            Some(at) => cursor += at + fragment.len(),
            None => {
                let seen = &text[..cursor.min(text.len())];
                let rest = &text[cursor.min(text.len())..];
                return Err(format!(
                    "fragment {i} not found after byte {cursor}\n  \
                     wanted: {fragment:?}\n  \
                     matched so far (tail): {:?}\n  \
                     remaining text (head): {:?}",
                    tail(seen, 120),
                    head(rest, 400),
                ));
            }
        }
    }
    Ok(())
}

fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Build the whole corpus into one directory and return it with its samples.
pub fn build(tag: &str) -> (PathBuf, Vec<Sample>) {
    let dir = quicksearch_core::testutil::scratch_dir(tag);
    let mut lcg = Lcg::new(seed());
    let mut samples = Vec::new();
    let mut next = 0usize;
    let mut body = |lcg: &mut Lcg, charset: Charset| {
        let b = Body::new(lcg, next, charset);
        next += 1;
        b
    };

    plaintext::write_all(&dir, &mut lcg, &mut body, &mut samples);
    rtf::write_all(&dir, &mut lcg, &mut body, &mut samples);
    ooxml::write_all(&dir, &mut lcg, &mut body, &mut samples);
    odf::write_all(&dir, &mut lcg, &mut body, &mut samples);
    pdf::write_all(&dir, &mut lcg, &mut body, &mut samples);
    audio::write_all(&dir, &mut lcg, &mut body, &mut samples);
    // The committed OLE2 fixtures, copied in so the whole corpus is one tree.
    samples.extend(legacy::copy_into(&dir));

    // Every needle must identify exactly one file, or the search proves
    // nothing: under the trigram tokenizer no needle may be a substring of
    // another, which a bad `Body::new` index would produce silently.
    for (i, a) in samples.iter().enumerate() {
        for b in samples.iter().skip(i + 1) {
            assert!(
                !a.needle.contains(&b.needle) && !b.needle.contains(&a.needle),
                "needles {:?} ({}) and {:?} ({}) are not distinguishable",
                a.needle,
                a.label,
                b.needle,
                b.label
            );
        }
    }

    (dir, samples)
}

/// A closure rather than a method so the file index keeps counting across
/// modules and every needle in the corpus stays unique.
pub type BodyFn<'a> = dyn FnMut(&mut Lcg, Charset) -> Body + 'a;

pub fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    path
}
