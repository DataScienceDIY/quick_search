//! The three OpenDocument corpus files: `.odt`, `.ods`, `.odp`.
//!
//! No Rust crate writes ODF, so these are assembled from
//! [`super::zipwriter`] and `format!` — independent of the `zip` 0.6 and
//! `quick-xml` the reader uses.
//!
//! The `mimetype` member comes first and uncompressed, as the ODF packaging
//! spec requires and as LibreOffice writes it. `extract::office` locates
//! `content.xml` by name and never looks at it, but a package that violates
//! the spec is not the thing the corpus is supposed to be testing against.

use std::path::Path;

use super::zipwriter::{self, xml_escape, Entry};
use super::{BodyFn, Charset, Lcg, Sample};

/// Namespace declarations shared by all three documents. Only `office` and
/// `text` are load-bearing for extraction; `table` is needed for the
/// spreadsheet's grid.
const NS: &str = "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
     xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
     xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
     xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
     office:version=\"1.3\"";

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    odt(dir, lcg, body, out);
    ods(dir, lcg, body, out);
    odp(dir, lcg, body, out);
}

/// Package `content.xml` as an ODF container with the right `mimetype`.
fn package(dir: &Path, name: &str, mime: &str, content: &str) -> std::path::PathBuf {
    let entries = [
        Entry {
            name: "mimetype",
            body: mime.as_bytes(),
        },
        Entry {
            name: "content.xml",
            body: content.as_bytes(),
        },
    ];
    super::write_file(dir, name, &zipwriter::archive(&entries))
}

/// Prose, alternating `<text:h>` and `<text:p>` and wrapping one sentence in a
/// `<text:span>`.
///
/// The span is the interesting case: `ODF_TEXT` lists `text:span` as
/// text-bearing but *not* paragraph-breaking, and the reader counts open
/// text elements rather than flagging them. A span nested in a paragraph is
/// what distinguishes those two behaviours.
fn odt(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let mut xml = String::new();
    for (i, sentence) in b.sentences.iter().enumerate() {
        let escaped = xml_escape(sentence);
        match i % 3 {
            0 => xml.push_str(&format!(
                "<text:h text:outline-level=\"1\">{escaped}</text:h>"
            )),
            1 => xml.push_str(&format!("<text:p>{escaped}</text:p>")),
            _ => xml.push_str(&format!(
                "<text:p><text:span>{escaped}</text:span></text:p>"
            )),
        }
    }
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <office:document-content {NS}><office:body><office:text>{xml}\
         </office:text></office:body></office:document-content>"
    );
    let path = package(
        dir,
        "prose.odt",
        "application/vnd.oasis.opendocument.text",
        &content,
    );
    out.push(Sample::prose(path, "odt", &b, false));
}

/// One sentence per cell, one cell per row — the same shape as the `.xlsx`,
/// read by `ODF_SHEET` with its `Some(' ')` cell separator.
fn ods(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let rows: String = b
        .sentences
        .iter()
        .map(|s| {
            format!(
                "<table:table-row><table:table-cell office:value-type=\"string\">\
                 <text:p>{}</text:p></table:table-cell></table:table-row>",
                xml_escape(s)
            )
        })
        .collect();
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <office:document-content {NS}><office:body><office:spreadsheet>\
         <table:table table:name=\"Corpus\">{rows}</table:table>\
         </office:spreadsheet></office:body></office:document-content>"
    );
    let path = package(
        dir,
        "sheet.ods",
        "application/vnd.oasis.opendocument.spreadsheet",
        &content,
    );
    out.push(Sample::prose(path, "ods", &b, false));
}

/// Sentences across two `<draw:page>`s, in text boxes — the shape LibreOffice
/// writes and the shape `ODF_TEXT` reads, since `.odp` and `.odt` share a spec.
fn odp(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    const PAGES: usize = 2;
    let b = body(lcg, Charset::Unicode);
    let mut xml = String::new();
    // Contiguous halves rather than round-robin: the reader concatenates
    // pages in document order, so this keeps the expectation equal to
    // `b.sentences` and leaves the reordering check to the `.pptx`.
    let per_page = b.sentences.len().div_ceil(PAGES);
    for (page, chunk) in b.sentences.chunks(per_page).enumerate() {
        let frames: String = chunk
            .iter()
            .map(|s| {
                format!(
                    "<draw:frame><draw:text-box><text:p>{}</text:p></draw:text-box></draw:frame>",
                    xml_escape(s)
                )
            })
            .collect();
        xml.push_str(&format!(
            "<draw:page draw:name=\"page{}\">{frames}</draw:page>",
            page + 1
        ));
    }
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <office:document-content {NS}><office:body><office:presentation>{xml}\
         </office:presentation></office:body></office:document-content>"
    );
    let path = package(
        dir,
        "deck.odp",
        "application/vnd.oasis.opendocument.presentation",
        &content,
    );
    out.push(Sample::prose(path, "odp", &b, false));
}
