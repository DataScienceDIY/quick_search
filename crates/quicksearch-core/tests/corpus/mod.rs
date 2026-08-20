//! A lipsum corpus in every format QuickSearch claims to extract text from.
//!
//! # Why this exists
//!
//! The per-extractor unit tests in `src/extract/` build their fixtures with
//! the same libraries that read them back — `zip` 0.6 for the OOXML/ODF
//! containers, `cfb` for OLE2, `lopdf` (through `pdf-extract`) for PDF. That
//! is the right choice there, because those tests aim at *malformed* input and
//! need to control every byte. But it means a writer and a reader that share a
//! wrong assumption agree with each other and the test passes.
//!
//! This module is the other half: well-formed documents from *foreign*
//! producers. Every writer here is a different implementation from the reader
//! it feeds —
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
//! Two places take committed bytes, for two different reasons, and in both the
//! *text* still comes from the generator:
//!
//! * The three legacy binaries are what nothing in Rust can fix: `cfb` is the
//!   only crate that writes OLE2 compound files, and it is the reader. Those
//!   are LibreOffice's output, and they are also the one part of the corpus
//!   whose text is fixed rather than seeded — see
//!   `tests/fixtures/legacy/README.md`.
//! * The `.flac` borrows fifty milliseconds of committed silence because
//!   `lofty` reads a real audio frame to derive stream properties. The fixture
//!   carries no text; `metaflac` writes the seeded lipsum into a copy of it.
//!   See [`audio`].
//!
//! # Determinism
//!
//! Everything on-the-fly is generated from one LCG seeded by [`seed`], which
//! is [`DEFAULT_SEED`] unless `QUICKSEARCH_CORPUS_SEED` says otherwise. A
//! failing assertion prints the seed it ran with, so a red CI job reproduces
//! locally with one environment variable.

// Only `extraction_corpus.rs` compiles this, and it uses most but not all of
// it; the writers each expose a little more surface than any one test needs.
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

/// The seed used when the environment says nothing. Arbitrary; what matters
/// is that it does not change between runs.
pub const DEFAULT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// The active seed. Override with `QUICKSEARCH_CORPUS_SEED=<u64>` to shake the
/// corpus without editing anything.
pub fn seed() -> u64 {
    match std::env::var("QUICKSEARCH_CORPUS_SEED") {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("QUICKSEARCH_CORPUS_SEED must be a u64, got {v:?}")),
        Err(_) => DEFAULT_SEED,
    }
}

/// The same LCG `benches/corpus/mod.rs` uses. Copied rather than shared:
/// `benches/` is not reachable from `tests/`, and the bench corpus is
/// deliberately frozen so its numbers stay comparable across runs.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.next() as usize % from.len()]
    }
}

/// Filler vocabulary. Plain ASCII lowercase so it survives every encoding in
/// the corpus unchanged, and long enough that a sentence drawn from it is
/// effectively unique.
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

/// A phrase every format can carry: Latin-1 representable, so it survives
/// cp1252 and the base-14 WinAnsi font the PDF writer uses.
const LATIN1_PHRASE: &str = "café résumé naïve";

/// A phrase only formats with a Unicode text model can carry.
const UNICODE_PHRASE: &str = "Καλημέρα κόσμε";

/// What characters a format's *writer* can round-trip. Gates the non-ASCII
/// coverage so the corpus is neither lax (ASCII everywhere) nor wrong
/// (demanding Greek from a WinAnsi font).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// 7-bit only.
    Ascii,
    /// Adds [`LATIN1_PHRASE`].
    Latin1,
    /// Adds [`UNICODE_PHRASE`] on top.
    Unicode,
}

/// How many sentences every generated document carries.
const SENTENCES: usize = 6;

/// Words per sentence.
const WORDS_PER_SENTENCE: usize = 7;

/// The lipsum planted in one file, plus the token that identifies it.
///
/// Sentences are the unit of assertion for every format: prose formats write
/// one per paragraph, spreadsheets one per cell, decks one per shape. Keeping
/// the granularity identical everywhere is what lets a single `must_contain`
/// list describe them all.
pub struct Body {
    pub sentences: Vec<String>,
    /// Unique to this file, planted in sentence 1 and never in the file name,
    /// so an end-to-end search hit is attributable to the body text.
    pub needle: String,
}

/// What every needle starts with.
///
/// The same word as `common::BODY_TERM`, and for the same reason that constant
/// exists: a term that reaches the index only through a document body and
/// never through a file name, so a hit for it is attributable to extraction.
/// This module stays self-contained rather than naming `common` — it has no
/// other reason to depend on the shared harness — so
/// `corpus_needles_use_the_shared_body_term` in `extraction_corpus.rs` is what
/// keeps the two from drifting.
pub const NEEDLE_PREFIX: &str = "chalcedony";

impl Body {
    /// Build a body for `index`-th file, carrying whatever `charset` allows.
    ///
    /// The needle is derived from the index alone, not from the LCG: it has to
    /// stay distinct from every other file's under any seed, and under the
    /// trigram tokenizer "distinct" means "not a substring of another".
    pub fn new(lcg: &mut Lcg, index: usize, charset: Charset) -> Body {
        let needle = format!("{NEEDLE_PREFIX}{index:04}");
        let mut sentences = Vec::with_capacity(SENTENCES);
        for i in 0..SENTENCES {
            let mut words: Vec<String> = (0..WORDS_PER_SENTENCE)
                .map(|_| lcg.pick(WORDS).to_string())
                .collect();
            // One planted item per sentence, at a fixed position so a
            // reordering bug shows up as a failed match rather than a pass.
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

/// One corpus file plus what its extracted text must contain.
pub struct Sample {
    pub path: PathBuf,
    /// A label for assertion messages — the format, not the file name.
    pub label: &'static str,
    /// Fragments that must appear in the extracted text, in this order, with
    /// anything permitted between them.
    ///
    /// Ordered containment, not equality, and not set membership. Equality is
    /// unusable: LibreOffice's `.ppt` filter drags master-slide boilerplate
    /// ("Click to edit the title text format", "___PPT10") into the text
    /// stream, and every spreadsheet reader puts its cell separators
    /// somewhere slightly different. Set membership is too weak: text
    /// assembled out of order is exactly what a mis-read Word piece table
    /// produces, and that has to fail.
    pub must_contain: Vec<String>,
    /// Planted in the body and absent from the file name.
    pub needle: String,
    /// Whether this format implements `Extractor::extract_from_head` — i.e.
    /// whether the walk may extract it without reopening the file. Only
    /// plaintext and RTF do.
    pub head_path: bool,
}

impl Sample {
    /// The prose case, where the sentences *are* the expectation.
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

/// Where `fragments` stop matching `text` as an ordered subsequence.
///
/// `Ok(())` when every fragment is found in turn. `Err` names the first one
/// that is not, which is the only diagnostic worth printing: "the text does
/// not contain X" plus where the scan had got to.
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

/// First `n` characters of `s`, for an error message.
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Last `n` characters of `s`, for an error message.
fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Build the whole corpus into one directory and return it with its samples.
///
/// The directory is a `testutil::scratch_dir`, so it survives a failing run
/// for inspection and is swept on a later day like every other test's tree.
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

    // Every needle has to identify exactly one file, or the end-to-end search
    // proves nothing. Under the trigram tokenizer that means no needle may be
    // a substring of another, which a bad `Body::new` index would produce
    // silently.
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

/// The signature the per-format writers take for "give me a fresh body".
/// A closure rather than a method so the file index keeps counting across
/// modules and every needle in the corpus stays unique.
pub type BodyFn<'a> = dyn FnMut(&mut Lcg, Charset) -> Body + 'a;

/// Write `bytes` to `dir/name` and return the path.
pub fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    path
}
