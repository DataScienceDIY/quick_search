//! The committed `.doc` / `.xls` / `.ppt` fixtures.
//!
//! The one departure from generate-on-the-fly: `cfb` is the only Rust OLE2
//! writer and also the reader, so LibreOffice writes these once and the
//! output is committed (see `tests/fixtures/legacy/regen.sh`).
//!
//! Their text is fixed, not seeded, and the expectations are read out of
//! the committed sources — a regenerated fixture that lost a line fails
//! instead of redefining what it contains. LibreOffice's `.ppt` drags
//! master-slide prompts into the text, which is exactly why
//! [`super::match_in_order`] asserts containment rather than equality.

use std::path::{Path, PathBuf};

use super::Sample;

/// Resolved against the crate root, so cwd does not matter.
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

fn lines(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The text of every `<text:p>` in the flat-ODF deck, in document order — a
/// hand parse, not `quick-xml`: computing the expectation with the parser
/// under test would defeat the point.
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

/// Fixed rather than derived, and well clear of the generated
/// `chalcedony00NN` range — "distinct" means "not a substring of another".
const NEEDLES: [(&str, &str); 3] = [
    ("sample.doc", "chalcedony9001"),
    ("sample.xls", "chalcedony9002"),
    ("sample.ppt", "chalcedony9003"),
];

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
                // OLE2's directory can sit anywhere in the file: never the
                // walk-time buffer path.
                head_path: false,
            }
        })
        .collect()
}

/// Copy the fixtures into `dir`, paths rewritten. Copied rather than indexed
/// in place: pointing the indexer at the repository would pull in the rest.
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
