//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS.
//!
//! All six are zip containers holding XML, and five of the six want the same
//! thing from it: the character data of a few named elements, with a newline
//! where a paragraph closes. That shape lives in [`collect_xml_text`], driven
//! by a per-format [`TextSpec`], so there is one event loop rather than one
//! per format.
//!
//! XLSX is the exception and keeps its own two loops: its text is not in the
//! sheet at all but in a shared-string table the cells index into, which is a
//! different machine, not a different table of element names.
//!
//! Dispatch is by file extension rather than MIME. `.docm` carries the same
//! MIME as `.docx` but needs the same reader, and the extension is what
//! distinguishes them.

use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use super::{ExtractError, ExtractedContent, Extractor};

pub struct OfficeExtractor;

fn mime_to_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        "application/vnd.ms-powerpoint" => Some("ppt"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        "application/vnd.oasis.opendocument.text" => Some("odt"),
        "application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
        "application/vnd.oasis.opendocument.presentation" => Some("odp"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The shared XML text walk
// ---------------------------------------------------------------------------

/// Which elements of a format's XML carry text, and where paragraphs end.
///
/// `text` and `breaks` are matched independently on a closing tag: an element
/// can be in both (ODF's `text:p` both holds text and ends a paragraph), in
/// only one (`w:p` breaks but holds nothing directly), or in `text` alone
/// (`text:span`, which ends a run without ending the line).
struct TextSpec {
    /// Elements whose character data is body text.
    text: &'static [&'static [u8]],
    /// Elements that close a paragraph, emitting `'\n'`.
    breaks: &'static [&'static [u8]],
    /// Emitted after each text run. Spreadsheets separate cells with it;
    /// prose formats leave it `None` so runs within a paragraph stay joined.
    separator: Option<char>,
}

const DOCX: TextSpec = TextSpec {
    text: &[b"w:t"],
    breaks: &[b"w:p"],
    separator: None,
};

const PPTX: TextSpec = TextSpec {
    text: &[b"a:t"],
    breaks: &[b"a:p"],
    separator: None,
};

/// ODT and ODP are the same format as far as text extraction is concerned —
/// both are ODF prose with headings, paragraphs and spans.
const ODF_TEXT: TextSpec = TextSpec {
    text: &[b"text:p", b"text:h", b"text:span"],
    breaks: &[b"text:p", b"text:h"],
    separator: None,
};

const ODF_SHEET: TextSpec = TextSpec {
    text: &[b"text:p", b"text:span"],
    breaks: &[b"text:p"],
    separator: Some(' '),
};

/// Append the text `spec` selects out of `xml` to `out`.
///
/// `in_text` is a flag rather than a depth count, which means a closing
/// `</text:span>` ends the run even though its enclosing `<text:p>` is still
/// open. That is how every one of the six extractors this replaces behaved.
fn collect_xml_text(xml: &str, spec: &TextSpec, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if spec.text.contains(&e.name().as_ref()) {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) if in_text => {
                out.push_str(&e.unescape()?);
                if let Some(sep) = spec.separator {
                    out.push(sep);
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if spec.text.contains(&name.as_ref()) {
                    in_text = false;
                }
                if spec.breaks.contains(&name.as_ref()) {
                    out.push('\n');
                }
            }
            Ok(Event::Eof) => break,
            // Propagated rather than ignored. Five of the six loops this
            // replaces had no error arm at all, so a malformed member sent
            // them round the loop on an error the reader kept re-reporting
            // without advancing — a hang on a file the user merely happened
            // to have on disk.
            Err(e) => return Err(format!("Error parsing XML: {}", e).into()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Container access
// ---------------------------------------------------------------------------

type Archive = ZipArchive<BufReader<File>>;

fn open_container(path: &Path) -> Result<Archive, Box<dyn Error>> {
    Ok(ZipArchive::new(BufReader::new(File::open(path)?))?)
}

/// One member's bytes as a string.
fn member_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    let mut member = archive.by_name(name)?;
    let mut body = String::new();
    member.read_to_string(&mut body)?;
    Ok(body)
}

/// Names of the `.xml` members under `prefix`, in archive order.
///
/// Indexed rather than taken from `file_names()`, which iterates a hash map:
/// slide order is the archive's order, and hashing it would shuffle the
/// slides of every presentation.
fn xml_members_under<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    prefix: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let name = archive.by_index(i)?.name().to_string();
        if name.starts_with(prefix) && name.ends_with(".xml") {
            names.push(name);
        }
    }
    Ok(names)
}

/// A format whose whole text lives in one member under one spec.
fn single_member(path: &Path, member: &str, spec: &TextSpec) -> Result<String, Box<dyn Error>> {
    let mut archive = open_container(path)?;
    let xml = member_text(&mut archive, member)?;
    let mut out = String::new();
    collect_xml_text(&xml, spec, &mut out)?;
    Ok(out)
}

fn extract_pptx(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut archive = open_container(path)?;
    let mut out = String::new();
    for name in xml_members_under(&mut archive, "ppt/slides/slide")? {
        let xml = member_text(&mut archive, &name)?;
        collect_xml_text(&xml, &PPTX, &mut out)?;
        out.push_str("\n--- New Slide ---\n");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// XLSX: shared strings plus cells
// ---------------------------------------------------------------------------

/// The workbook's shared-string table, in index order. Absent or unreadable
/// is not an error: a sheet of nothing but numbers has no table at all.
fn shared_strings<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Vec<String> {
    let Ok(xml) = member_text(archive, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut strings = Vec::new();
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"t" => in_text = true,
            Ok(Event::Text(e)) if in_text => match e.unescape() {
                Ok(s) => strings.push(s.into_owned()),
                Err(_) => return strings,
            },
            Ok(Event::End(ref e)) if e.name().as_ref() == b"t" => in_text = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    strings
}

/// One worksheet's cells. A `t="s"` cell holds an index into `strings`
/// rather than text of its own; every other type holds its value inline.
fn collect_sheet(xml: &str, strings: &[String], out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut in_cell = false;
    let mut cell_type = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"c" => {
                in_cell = true;
                cell_type.clear();
                for attr in e.attributes() {
                    let attr = attr?;
                    if attr.key.as_ref() == b"t" {
                        cell_type = String::from_utf8_lossy(&attr.value).to_string();
                    }
                }
            }
            Ok(Event::Text(e)) if in_cell => {
                let text = e.unescape()?;
                if cell_type == "s" {
                    // A shared-string reference. An index past the end of the
                    // table is a corrupt workbook, not something to guess at.
                    if let Some(s) = text.parse::<usize>().ok().and_then(|i| strings.get(i)) {
                        out.push_str(s);
                        out.push(' ');
                    }
                } else {
                    out.push_str(&text);
                    out.push(' ');
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"c" {
                    in_cell = false;
                } else if name.as_ref() == b"row" {
                    out.push('\n');
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("Error parsing XML: {}", e).into()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn extract_xlsx(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut archive = open_container(path)?;
    let strings = shared_strings(&mut archive);
    let mut out = String::new();
    for name in xml_members_under(&mut archive, "xl/worksheets/sheet")? {
        let xml = member_text(&mut archive, &name)?;
        collect_sheet(&xml, &strings, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Extract text from an office document, chosen by lowercase extension.
///
/// An extension nothing here handles yields empty text rather than an error:
/// the caller reaches this only for a MIME [`mime_to_ext`] claimed, so an
/// unrecognized extension means the file was named unlike its type.
fn extract_document_text(path: &Path, extension: &str) -> Result<String, Box<dyn Error>> {
    match extension {
        "docx" => single_member(path, "word/document.xml", &DOCX),
        "xlsx" => extract_xlsx(path),
        "pptx" => extract_pptx(path),
        "odt" | "odp" => single_member(path, "content.xml", &ODF_TEXT),
        "ods" => single_member(path, "content.xml", &ODF_SHEET),
        // Pre-2007 binary formats: a different container entirely.
        "doc" | "xls" | "ppt" => super::ole::extract_ole_text(path, extension),
        _ => Ok(String::new()),
    }
}

impl Extractor for OfficeExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime_to_ext(mime).is_some()
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        // The extension from the path, not from the MIME: `.docm` and `.docx`
        // share a MIME but the dispatch above is by extension.
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let text = extract_document_text(path, &ext)
            .map_err(|e| format!("office extractor {}: {}", path.display(), e))?;
        Ok(ExtractedContent::with_text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn supports_docx_and_friends() {
        let e = OfficeExtractor;
        for m in [
            "application/msword",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.oasis.opendocument.text",
        ] {
            assert!(e.supports(m), "should support {}", m);
        }
        assert!(!e.supports("image/png"));
    }

    /// A zip container holding `members`, written to a scratch file.
    fn container(tag: &str, ext: &str, members: &[(&str, &str)]) -> std::path::PathBuf {
        let path = crate::testutil::scratch_dir(tag).join(format!("doc.{ext}"));
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, body) in members {
            zip.start_file(*name, zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    const DOCX_BODY: &str = "<w:document><w:body>\
         <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>\
         <w:p><w:r><w:t>Second</w:t></w:r></w:p>\
         </w:body></w:document>";

    const PPTX_SLIDE: &str = "<p:sld><p:cSld><p:spTree><p:sp><p:txBody>\
         <a:p><a:r><a:t>Title</a:t></a:r></a:p>\
         <a:p><a:r><a:t>Body</a:t></a:r></a:p>\
         </p:txBody></p:sp></p:spTree></p:cSld></p:sld>";

    const ODT_BODY: &str = "<office:document-content><office:body><office:text>\
         <text:h>Heading</text:h>\
         <text:p>Para<text:span>span</text:span></text:p>\
         </office:text></office:body></office:document-content>";

    const ODS_BODY: &str = "<office:document-content><office:body><office:spreadsheet>\
         <table:table><table:table-row>\
         <table:table-cell><text:p>A1</text:p></table:table-cell>\
         <table:table-cell><text:p>B1</text:p></table:table-cell>\
         </table:table-row></table:table>\
         </office:spreadsheet></office:body></office:document-content>";

    const XLSX_SHARED: &str = "<sst><si><t>Shared</t></si><si><t>Second</t></si></sst>";

    const XLSX_SHEET: &str = "<worksheet><sheetData>\
         <row><c t=\"s\"><v>0</v></c><c t=\"n\"><v>42</v></c></row>\
         <row><c t=\"s\"><v>1</v></c></row>\
         </sheetData></worksheet>";

    // The golden set. These strings were recorded from the six hand-written
    // extractors this module replaced, so they pin the rewrite to exactly
    // what shipped rather than to what it ought to have produced.

    #[test]
    fn docx() {
        let p = container("docx", "docx", &[("word/document.xml", DOCX_BODY)]);
        assert_eq!(
            extract_document_text(&p, "docx").unwrap(),
            "Helloworld\nSecond\n"
        );
    }

    #[test]
    fn pptx_marks_each_slide_and_keeps_archive_order() {
        let p = container(
            "pptx",
            "pptx",
            &[
                ("ppt/slides/slide1.xml", PPTX_SLIDE),
                ("ppt/slides/slide2.xml", PPTX_SLIDE),
            ],
        );
        assert_eq!(
            extract_document_text(&p, "pptx").unwrap(),
            "Title\nBody\n\n--- New Slide ---\nTitle\nBody\n\n--- New Slide ---\n"
        );
    }

    #[test]
    fn odt_and_odp_are_the_same_extraction() {
        let odt = container("odt", "odt", &[("content.xml", ODT_BODY)]);
        let odp = container("odp", "odp", &[("content.xml", ODT_BODY)]);
        assert_eq!(
            extract_document_text(&odt, "odt").unwrap(),
            "Heading\nParaspan\n"
        );
        assert_eq!(
            extract_document_text(&odt, "odt").unwrap(),
            extract_document_text(&odp, "odp").unwrap(),
        );
    }

    #[test]
    fn ods_separates_cells_with_a_space() {
        let p = container("ods", "ods", &[("content.xml", ODS_BODY)]);
        assert_eq!(extract_document_text(&p, "ods").unwrap(), "A1 \nB1 \n");
    }

    #[test]
    fn xlsx_resolves_shared_strings() {
        let p = container(
            "xlsx",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", XLSX_SHARED),
                ("xl/worksheets/sheet1.xml", XLSX_SHEET),
            ],
        );
        assert_eq!(
            extract_document_text(&p, "xlsx").unwrap(),
            "Shared 42 \nSecond \n"
        );
    }

    #[test]
    fn an_extension_nothing_handles_is_empty() {
        let p = container("none", "bin", &[("whatever", "x")]);
        assert_eq!(extract_document_text(&p, "zzz").unwrap(), "");
    }

    /// A workbook of pure numbers has no shared-string table. Its absence is
    /// normal, not a failure.
    #[test]
    fn xlsx_without_a_shared_string_table_still_reads_its_cells() {
        let p = container(
            "xlsx-nosst",
            "xlsx",
            &[(
                "xl/worksheets/sheet1.xml",
                "<worksheet><sheetData>\
                <row><c t=\"n\"><v>7</v></c></row></sheetData></worksheet>",
            )],
        );
        assert_eq!(extract_document_text(&p, "xlsx").unwrap(), "7 \n");
    }

    /// A shared-string index past the end of the table is dropped rather than
    /// panicking on the slice.
    #[test]
    fn an_out_of_range_shared_string_index_is_dropped() {
        let p = container(
            "xlsx-oob",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", "<sst><si><t>only</t></si></sst>"),
                (
                    "xl/worksheets/sheet1.xml",
                    "<worksheet><sheetData><row>\
                     <c t=\"s\"><v>0</v></c><c t=\"s\"><v>99</v></c>\
                     </row></sheetData></worksheet>",
                ),
            ],
        );
        assert_eq!(extract_document_text(&p, "xlsx").unwrap(), "only \n");
    }

    /// Malformed XML is an error, not a hang. Five of the six extractors this
    /// replaced had no error arm, so the reader re-reported the same failure
    /// forever without advancing.
    ///
    /// An undefined entity inside a text run is the cheapest way to reach that
    /// arm, and it is a real shape: a document written by a tool that emitted
    /// HTML entities into OOXML.
    #[test]
    fn malformed_xml_returns_an_error_rather_than_looping() {
        for (ext, member, body) in [
            (
                "docx",
                "word/document.xml",
                "<w:t>bad &nonsuch; entity</w:t>",
            ),
            (
                "odt",
                "content.xml",
                "<text:p>bad &nonsuch; entity</text:p>",
            ),
            (
                "ods",
                "content.xml",
                "<text:p>bad &nonsuch; entity</text:p>",
            ),
            (
                "pptx",
                "ppt/slides/slide1.xml",
                "<a:t>bad &nonsuch; entity</a:t>",
            ),
        ] {
            let p = container(&format!("bad-{ext}"), ext, &[(member, body)]);
            assert!(
                extract_document_text(&p, ext).is_err(),
                "{ext} should report malformed XML"
            );
        }
    }

    /// Mismatched tags are caught too — quick_xml checks closing names, and
    /// that error now reaches the caller instead of being swallowed.
    #[test]
    fn mismatched_tags_are_an_error() {
        let p = container(
            "mismatch",
            "docx",
            &[("word/document.xml", "<w:body><w:t>x</w:body>")],
        );
        assert!(extract_document_text(&p, "docx").is_err());
    }

    /// A container missing the member the format is defined by.
    #[test]
    fn a_missing_member_is_an_error() {
        let p = container("empty", "docx", &[("unrelated.xml", "<x/>")]);
        assert!(extract_document_text(&p, "docx").is_err());
    }

    /// Not a zip file at all — the shape a truncated download or a
    /// misidentified file arrives in.
    #[test]
    fn a_non_container_is_an_error() {
        let dir = crate::testutil::scratch_dir("notzip");
        let p = dir.join("doc.docx");
        crate::testutil::touch(&p, b"this is not a zip archive");
        assert!(extract_document_text(&p, "docx").is_err());
    }
}
