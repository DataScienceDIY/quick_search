//! The committed LibreOffice fixtures: `.doc` / `.xls` / `.ppt`, plus an
//! `.odt`, `.ods`, `.odp`, `.pptx` and `.rtf`.
//!
//! The one departure from generate-on-the-fly. `cfb` is the only Rust OLE2
//! writer and also the reader, so nothing else could write the first three.
//! The corpus does build its own of the other five, but by hand from the
//! spec, so these are the only ones a real producer wrote. LibreOffice
//! writes all eight once and the output is committed (see
//! `tests/fixtures/legacy/regen.sh`).
//!
//! Their text is fixed, not seeded, and the expectations are read out of
//! the committed sources, so a regenerated fixture that lost a line fails
//! instead of redefining what it contains. LibreOffice's `.ppt` drags
//! master-slide prompts into the text, which is why
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
/// The last five are converted from copies of the first three's sources
/// whose needle `regen.sh` renumbers, so each needle names one file.
const NEEDLES: [(&str, &str); 8] = [
    ("sample.doc", "chalcedony9001"),
    ("sample.xls", "chalcedony9002"),
    ("sample.ppt", "chalcedony9003"),
    ("sample.odp", "chalcedony9004"),
    ("sample.pptx", "chalcedony9005"),
    ("sample.odt", "chalcedony9006"),
    ("sample.rtf", "chalcedony9007"),
    ("sample.ods", "chalcedony9008"),
];

fn needle(file: &str) -> &'static str {
    NEEDLES
        .iter()
        .find(|(f, _)| *f == file)
        .expect("every fixture has a needle")
        .1
}

pub fn samples() -> Vec<Sample> {
    let prose = lines(&read("prose.txt"));
    let sheet = lines(&read("sheet.csv"));
    let deck = paragraphs(&read("deck.fodp"));
    // The copy `regen.sh` converts to `file`: the source of OLE2 fixture
    // `ole`, its needle renumbered to `file`'s.
    let renumbered = |source: &[String], ole: &str, file: &str| -> Vec<String> {
        source
            .iter()
            .map(|line| line.replace(needle(ole), needle(file)))
            .collect()
    };
    let expectations = [
        ("sample.doc", "doc", prose.clone()),
        ("sample.xls", "xls", sheet.clone()),
        ("sample.ppt", "ppt", deck.clone()),
        (
            "sample.odt",
            "odt-libreoffice",
            renumbered(&prose, "sample.doc", "sample.odt"),
        ),
        (
            "sample.ods",
            "ods-libreoffice",
            renumbered(&sheet, "sample.xls", "sample.ods"),
        ),
        (
            "sample.odp",
            "odp-libreoffice",
            renumbered(&deck, "sample.ppt", "sample.odp"),
        ),
        (
            "sample.pptx",
            "pptx-libreoffice",
            renumbered(&deck, "sample.ppt", "sample.pptx"),
        ),
        (
            "sample.rtf",
            "rtf-libreoffice",
            renumbered(&prose, "sample.doc", "sample.rtf"),
        ),
    ];

    expectations
        .into_iter()
        .map(|(file, label, must_contain)| {
            let needle = needle(file);
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
                // RTF reads front to back, so the walk may extract it from
                // its buffer. OLE2's directory can sit anywhere in the file,
                // and a zip's is at its end: never the walk-time buffer path.
                head_path: file.ends_with(".rtf"),
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
