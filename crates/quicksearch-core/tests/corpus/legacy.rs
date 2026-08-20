//! The committed `.doc` / `.xls` / `.ppt` fixtures.
//!
//! These three are the corpus's one departure from generate-on-the-fly, and
//! the reason is narrow: `cfb` is the only Rust crate that writes OLE2
//! compound files, and `cfb` is what `extract::ole` reads them with. A fixture
//! built by the reader's own library proves only that the two agree with each
//! other. So LibreOffice writes them, once, and the output is committed —
//! see `tests/fixtures/legacy/regen.sh`.
//!
//! Two consequences worth being explicit about:
//!
//! * **Their text is fixed, not seeded.** `QUICKSEARCH_CORPUS_SEED` shakes the
//!   generated half of the corpus and leaves these alone.
//! * **The expectations are read out of the committed sources**, not written
//!   out here a second time. A regenerated fixture that lost a line therefore
//!   fails, instead of quietly redefining what it was supposed to contain.
//!
//! What the fixtures prove that a synthetic file cannot: LibreOffice's `.doc`
//! is a real FIB with a real piece table, its `.xls` a real BIFF stream with a
//! real shared-string table, and its `.ppt` drags the master slide's
//! placeholder prompts ("Click to edit the title text format", `___PPT10`)
//! into the text alongside the content. That last one is exactly why
//! [`super::match_in_order`] asserts containment rather than equality.

use std::path::{Path, PathBuf};

use super::Sample;

/// The directory the fixtures live in, resolved against the crate root so the
/// test does not care what the working directory is.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy")
}

fn read(name: &str) -> String {
    let path = dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}\n(fixtures are committed; regenerate with regen.sh)",
            path.display()
        )
    })
}

/// Non-empty, trimmed lines — the expectation for the line-per-record sources.
fn lines(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The text of every `<text:p>` in the flat-ODF deck, in document order.
///
/// A three-line hand parse rather than `quick-xml`, which is the reader's
/// library: the source is committed, its shape is fixed, and reaching for the
/// parser under test to compute the expectation would defeat the point.
fn paragraphs(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("<text:p>") {
        rest = &rest[start + "<text:p>".len()..];
        let Some(end) = rest.find("</text:p>") else {
            break;
        };
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    out
}

/// The needle planted in each source. Fixed rather than derived, and well
/// clear of the generated corpus's `chalcedony0000`-`chalcedony00NN` range —
/// under the trigram tokenizer "distinct" means "not a substring of another".
const NEEDLES: [(&str, &str); 3] = [
    ("sample.doc", "chalcedony9001"),
    ("sample.xls", "chalcedony9002"),
    ("sample.ppt", "chalcedony9003"),
];

/// The three fixtures as corpus samples.
pub fn samples() -> Vec<Sample> {
    let expectations = [
        ("sample.doc", "doc", lines(&read("prose.txt"))),
        ("sample.xls", "xls", lines(&read("sheet.csv"))),
        ("sample.ppt", "ppt", paragraphs(&read("deck.fodp"))),
    ];

    expectations
        .into_iter()
        .map(|(file, label, must_contain)| {
            let needle = NEEDLES
                .iter()
                .find(|(f, _)| *f == file)
                .expect("every fixture has a needle")
                .1;
            assert!(
                must_contain.iter().any(|f| f.contains(needle)),
                "{file}: source no longer carries {needle}; \
                 the end-to-end search would not be attributable"
            );
            Sample {
                path: dir().join(file),
                label,
                must_contain,
                needle: needle.to_string(),
                // OLE2 reads a directory that can sit anywhere in the file, so
                // this format never takes the walk-time buffer path.
                head_path: false,
            }
        })
        .collect()
}

/// Copy the fixtures into `dir` so the end-to-end run indexes them alongside
/// the generated corpus. Returns the samples with their paths rewritten to the
/// copies.
///
/// Copied rather than indexed in place: the indexer walks a directory tree,
/// and pointing it at the repository would pull in whatever else lives there.
pub fn copy_into(dir: &Path) -> Vec<Sample> {
    samples()
        .into_iter()
        .map(|sample| {
            let name = sample.path.file_name().expect("fixture has a name");
            let target = dir.join(name);
            std::fs::copy(&sample.path, &target).unwrap_or_else(|e| {
                panic!(
                    "copy {} -> {}: {e}",
                    sample.path.display(),
                    target.display()
                )
            });
            Sample {
                path: target,
                ..sample
            }
        })
        .collect()
}
