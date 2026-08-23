//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS — all zip
//! containers holding XML. Five share one event loop ([`collect_xml_text`],
//! driven by a per-format [`TextSpec`]); XLSX keeps its own two loops because
//! its text is not in the sheet at all but in a shared-string table the cells
//! index into. Dispatch is by file extension, not MIME: `.docm` carries the
//! same MIME as `.docx`, and the extension is what distinguishes them.

use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use super::{ExtractError, Extractor};

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

// The shared XML text walk

/// Which elements of a format's XML carry text, and where paragraphs end.
/// `text` and `breaks` are matched independently: ODF's `text:p` is in both,
/// `w:p` breaks but holds nothing directly, `text:span` ends a run without
/// ending the line.
struct TextSpec {
    text: &'static [&'static [u8]],
    /// Elements that close a paragraph, emitting `'\n'`.
    breaks: &'static [&'static [u8]],
    /// Emitted after each text run; spreadsheets separate cells with it.
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

/// ODT and ODP are the same ODF prose as far as extraction is concerned.
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

/// The text an `&entity;` or `&#1234;` reference stands for. quick-xml 0.41
/// reports a reference as its own event, so a reader that ignores it silently
/// drops every `&amp;` from the document. Only the five predefined entities
/// and numeric references are resolvable without a DTD.
fn entity_text(raw: &str) -> Option<String> {
    if let Some(digits) = raw.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        let c = char::from_u32(code)?;
        // `char::from_u32` accepts more than XML's character production does:
        // `&#0;` would put a literal NUL into an FTS5 column. `None` becomes
        // the same visible "unknown entity" error an unexpandable name gets.
        let legal = !c.is_control() || matches!(c, '\t' | '\n' | '\r');
        return legal.then(|| String::from(c));
    }
    quick_xml::escape::resolve_predefined_entity(raw).map(String::from)
}

/// Append the text `spec` selects out of `xml` to `out`. Text-bearing
/// elements are counted, not flagged: ODF nests them, and a flag made a
/// span's close end the run, dropping everything up to the paragraph's
/// close. The separator belongs after a *run* — several events since 0.41.
fn collect_xml_text(xml: &str, spec: &TextSpec, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_str(xml);
    // No `trim_text`: it trims each *event*, and since 0.41 an entity
    // reference splits the character data into separate events — `Jack &amp;
    // Jill` would come back as `Jack&Jill`. Whitespace inside a text-bearing
    // element is content; between elements it is ignored anyway.
    let mut buf = Vec::new();
    // Open text-bearing elements; the run ends at zero, not on the innermost
    // close.
    let mut depth = 0usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if spec.text.contains(&e.name().as_ref()) {
                    depth += 1;
                }
            }
            Ok(Event::Text(e)) if depth > 0 => {
                out.push_str(&e.decode()?);
            }
            // Without this arm every `&amp;` would vanish from the index.
            Ok(Event::GeneralRef(e)) if depth > 0 => {
                let raw = e.decode()?;
                // An unexpandable entity is an error: dropping it takes
                // characters out of the indexed text silently.
                let text = entity_text(&raw)
                    .ok_or_else(|| format!("Error parsing XML: unknown entity &{};", raw))?;
                out.push_str(&text);
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if spec.text.contains(&name.as_ref()) {
                    depth = depth.saturating_sub(1);
                    // Closing the outermost one closes the run.
                    if depth == 0 {
                        if let Some(sep) = spec.separator {
                            out.push(sep);
                        }
                    }
                }
                if spec.breaks.contains(&name.as_ref()) {
                    out.push('\n');
                }
            }
            // A self-closed element gets no `Start`/`End`, so `<text:p/>` —
            // ODF's blank line — would otherwise lose its paragraph break.
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                if depth == 0 && spec.text.contains(&name.as_ref()) {
                    if let Some(sep) = spec.separator {
                        out.push(sep);
                    }
                }
                if spec.breaks.contains(&name.as_ref()) {
                    out.push('\n');
                }
            }
            Ok(Event::Eof) => break,
            // Ignoring it is a hang: the reader re-reports without advancing.
            Err(e) => return Err(format!("Error parsing XML: {}", e).into()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

// Container access

type Archive = ZipArchive<BufReader<File>>;

fn open_container(path: &Path) -> Result<Archive, Box<dyn Error>> {
    Ok(ZipArchive::new(BufReader::new(File::open(path)?))?)
}

/// Cap on one decompressed member, mirroring `ole::MAX_TEXT_BYTES`: the zip
/// header declares sizes, but the deflate stream is what we actually read, so
/// a tiny archive can inflate without bound.
const MAX_XML_BYTES: usize = 64 * 1024 * 1024;

/// Cap on the text taken from one *container*. [`MAX_XML_BYTES`] bounds each
/// member on its own, and a small archive can carry dozens that each inflate
/// to that cap: without a running total the peak is members × 64 MiB per
/// worker, and an allocation failure aborts rather than unwinding.
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// One member's bytes as a string. An over-cap member keeps its prefix.
fn member_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    let mut body = Vec::new();
    archive
        .by_name(name)?
        .take(MAX_XML_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    let truncated = body.len() > MAX_XML_BYTES;
    body.truncate(MAX_XML_BYTES);
    match String::from_utf8(body) {
        Ok(text) => Ok(text),
        // Only a cut at the cap may split a character; invalid UTF-8 anywhere
        // else still fails the extraction, as `read_to_string` always did.
        Err(e) if truncated && e.utf8_error().valid_up_to() >= MAX_XML_BYTES - 3 => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            Ok(String::from_utf8(bytes)?)
        }
        Err(e) => Err(e.into()),
    }
}

/// Names of the `.xml` members under `prefix`, in archive order — not
/// `file_names()`, which iterates a hash map and would shuffle the slides of
/// every presentation.
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

/// Concatenate what `collect` gets out of each `.xml` member under `prefix`,
/// in archive order, bounded by [`MAX_TEXT_BYTES`]; whole members are kept or
/// dropped, never cut mid-way.
fn collect_members(
    archive: &mut Archive,
    prefix: &str,
    mut collect: impl FnMut(&str, &mut String) -> Result<(), Box<dyn Error>>,
) -> Result<String, Box<dyn Error>> {
    let mut out = String::new();
    for name in xml_members_under(archive, prefix)? {
        if out.len() >= MAX_TEXT_BYTES {
            break;
        }
        let xml = member_text(archive, &name)?;
        collect(&xml, &mut out)?;
    }
    Ok(out)
}

fn extract_pptx(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut archive = open_container(path)?;
    collect_members(&mut archive, "ppt/slides/slide", |xml, out| {
        collect_xml_text(xml, &PPTX, out)?;
        out.push_str("\n--- New Slide ---\n");
        Ok(())
    })
}

// XLSX: shared strings plus cells

/// The workbook's shared-string table, in index order. Absent or unreadable
/// is not an error: a sheet of nothing but numbers has no table at all.
fn shared_strings<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Vec<String> {
    let Ok(xml) = member_text(archive, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    let mut reader = Reader::from_str(&xml);
    // No `trim_text`; see `collect_xml_text`.
    let mut buf = Vec::new();
    let mut strings = Vec::new();
    let mut in_text = false;
    // One `<t>` is one shared string but not one event (an entity reference
    // splits it); accumulated and pushed on the closing tag, or a cell with
    // `&amp;` would become three table entries.
    let mut current = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"t" => {
                in_text = true;
                current.clear();
            }
            // `<t/>` — an empty cell, which LibreOffice, openpyxl and POI
            // all write for a blank. Without this arm the entry is never
            // pushed and **every later index is off by one**, silently
            // rendering real strings for the wrong cells.
            Ok(Event::Empty(ref e)) if e.name().as_ref() == b"t" => {
                strings.push(String::new());
            }
            Ok(Event::Text(e)) if in_text => match e.decode() {
                Ok(s) => current.push_str(&s),
                Err(_) => return strings,
            },
            Ok(Event::GeneralRef(e)) if in_text => {
                // This reader cannot fail; an unexpandable entity is left out.
                if let Ok(raw) = e.decode() {
                    if let Some(text) = entity_text(&raw) {
                        current.push_str(&text);
                    }
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"t" => {
                in_text = false;
                strings.push(std::mem::take(&mut current));
            }
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
    // No `trim_text`; see `collect_xml_text`.
    let mut buf = Vec::new();
    let mut in_cell = false;
    let mut cell_type = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"c" => {
                in_cell = true;
                cell_type.clear();
                // `with_checks(false)`: the duplicate-attribute-name check is
                // quadratic with no bound but the tag's size
                // (RUSTSEC-2026-0194), so one crafted `<c>` in 64 MiB of
                // inflated XML could hold this worker for hours,
                // uncancellably. This extractor wants one attribute anyway.
                for attr in e.attributes().with_checks(false) {
                    let attr = attr?;
                    if attr.key.as_ref() == b"t" {
                        cell_type = String::from_utf8_lossy(&attr.value).to_string();
                        break;
                    }
                }
            }
            Ok(Event::Text(e)) if in_cell => {
                let text = e.decode()?;
                if cell_type == "s" {
                    // A shared-string reference; an out-of-range index is a
                    // corrupt workbook, not something to guess at. `trim`
                    // because an indenting generator hands `<v>` over as
                    // "\n  0\n" and an untrimmed parse silently drops the
                    // string.
                    if let Some(s) = text
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| strings.get(i))
                    {
                        out.push_str(s);
                        out.push(' ');
                    }
                } else if !text.trim().is_empty() {
                    // Whitespace-only fragments are indentation between a
                    // cell's children. The value itself is pushed whole — no
                    // `trim` — so `xml:space="preserve"` keeps its shape.
                    out.push_str(&text);
                    out.push(' ');
                }
            }
            // Only inline values can carry one; a `t="s"` cell's is an index.
            Ok(Event::GeneralRef(e)) if in_cell && cell_type != "s" => {
                let raw = e.decode()?;
                let text = entity_text(&raw)
                    .ok_or_else(|| format!("Error parsing XML: unknown entity &{};", raw))?;
                out.push_str(&text);
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
    collect_members(&mut archive, "xl/worksheets/sheet", |xml, out| {
        collect_sheet(xml, &strings, out)
    })
}

// Dispatch

/// Extract text from an office document, chosen by lowercase extension. An
/// unhandled extension yields empty text: the MIME was claimed, so the file
/// was simply named unlike its type.
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

    fn extract(&self, path: &Path) -> Result<String, ExtractError> {
        // From the path, not the MIME: `.docm` and `.docx` share a MIME.
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let text = extract_document_text(path, &ext)
            .map_err(|e| format!("office extractor {}: {}", path.display(), e))?;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A reader that only handles `Event::Text` loses `&amp;` with no error;
    /// this fails by producing "Blake  Co".
    #[test]
    fn entity_references_survive_extraction() {
        let body = "<w:document><w:body><w:p><w:r>\
             <w:t>Blake &amp; Co &lt;tags&gt; &#8217;24 &#x2019;25</w:t>\
             </w:r></w:p></w:body></w:document>";
        let path = container("docx-entities", "docx", &[("word/document.xml", body)]);
        let out = OfficeExtractor.extract(&path).expect("extract");
        assert!(
            out.contains("Blake & Co"),
            "predefined entity lost: {:?}",
            out
        );
        assert!(
            out.contains("<tags>"),
            "angle-bracket entities lost: {:?}",
            out
        );
        assert!(
            out.contains('\u{2019}'),
            "numeric entities lost: {:?}",
            out
        );
        assert!(
            !out.contains("&amp;") && !out.contains("&#"),
            "entities left unresolved: {:?}",
            out
        );
    }

    /// The same, through the shared-string table — a separate reader.
    #[test]
    fn entity_references_survive_shared_strings() {
        let shared = "<sst><si><t>Jack &amp; Jill</t></si></sst>";
        let sheet = "<worksheet><sheetData><row>\
             <c t=\"s\"><v>0</v></c></row></sheetData></worksheet>";
        let path = container(
            "xlsx-entities",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", shared),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let out = OfficeExtractor.extract(&path).expect("extract");
        assert!(
            out.contains("Jack & Jill"),
            "entity lost through the shared-string table: {:?}",
            out
        );
    }

    /// A pretty-printing generator hands `<v>` over as "\n      0\n    ";
    /// an untrimmed `parse::<usize>()` fails and drops the cell's text with
    /// no error and no `failed_files` row.
    #[test]
    fn an_indented_shared_string_reference_still_resolves() {
        let shared = "<sst><si><t>Marmalade</t></si></sst>";
        let sheet = "<worksheet>\n  <sheetData>\n    <row>\n      \
             <c t=\"s\">\n        <v>\n          0\n        </v>\n      </c>\n      \
             <c t=\"n\">\n        <v>17</v>\n      </c>\n    \
             </row>\n  </sheetData>\n</worksheet>";
        let path = container(
            "xlsx-indented",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", shared),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let out = OfficeExtractor.extract(&path).expect("extract");
        assert!(
            out.contains("Marmalade"),
            "the shared string was dropped by an indented index: {:?}",
            out
        );
        assert!(
            out.contains("17"),
            "the inline value was dropped: {:?}",
            out
        );
        // Every run of whitespace in the output should be a separator this
        // reader put there, never the source XML's own layout.
        assert!(
            !out.contains("\n  "),
            "sheet indentation reached the indexed text: {:?}",
            out
        );
    }

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

    /// The first entry is a blank `<si><t/></si>`; it still occupies index 0.
    const XLSX_SHARED_WITH_BLANK: &str = "<sst><si><t/></si><si><t>Second</t></si></sst>";

    const XLSX_SHEET_INDEX_1: &str = "<worksheet><sheetData>\
         <row><c t=\"s\"><v>1</v></c></row>\
         </sheetData></worksheet>";

    const XLSX_SHEET: &str = "<worksheet><sheetData>\
         <row><c t=\"s\"><v>0</v></c><c t=\"n\"><v>42</v></c></row>\
         <row><c t=\"s\"><v>1</v></c></row>\
         </sheetData></worksheet>";

    // The golden set: pins the exact extraction shapes.

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

    /// A workbook of pure numbers has no shared-string table at all.
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

    /// An undefined entity is the cheapest way to reach the error arm, and a
    /// real shape: tools do emit HTML entities into OOXML.
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

    #[test]
    fn mismatched_tags_are_an_error() {
        let p = container(
            "mismatch",
            "docx",
            &[("word/document.xml", "<w:body><w:t>x</w:body>")],
        );
        assert!(extract_document_text(&p, "docx").is_err());
    }

    #[test]
    fn a_missing_member_is_an_error() {
        let p = container("empty", "docx", &[("unrelated.xml", "<x/>")]);
        assert!(extract_document_text(&p, "docx").is_err());
    }

    #[test]
    fn a_non_container_is_an_error() {
        let dir = crate::testutil::scratch_dir("notzip");
        let p = dir.join("doc.docx");
        crate::testutil::touch(&p, b"this is not a zip archive");
        assert!(extract_document_text(&p, "docx").is_err());
    }

    /// Skip a self-closed `<t/>` and every later index slides by one: clean
    /// extraction, no error, wrong content.
    #[test]
    fn a_blank_shared_string_still_occupies_its_index() {
        let p = container(
            "xlsx-blank-si",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", XLSX_SHARED_WITH_BLANK),
                ("xl/worksheets/sheet1.xml", XLSX_SHEET_INDEX_1),
            ],
        );
        assert_eq!(
            extract_document_text(&p, "xlsx").unwrap(),
            "Second \n",
            "index 1 must still be the second entry"
        );
    }

    /// The same shape one level up: an empty `<si>`.
    #[test]
    fn an_empty_si_still_occupies_its_index() {
        let p = container(
            "xlsx-empty-si",
            "xlsx",
            &[
                (
                    "xl/sharedStrings.xml",
                    "<sst><si><t></t></si><si><t>Second</t></si></sst>",
                ),
                ("xl/worksheets/sheet1.xml", XLSX_SHEET_INDEX_1),
            ],
        );
        assert_eq!(extract_document_text(&p, "xlsx").unwrap(), "Second \n");
    }

    /// The separator belongs to the run, so the cell must read `A&B` — not
    /// `A &B`, and not `A & B`.
    #[test]
    fn an_entity_does_not_split_an_ods_cell() {
        let body = "<office:document-content><office:body><office:spreadsheet>\
             <table:table><table:table-row>\
             <table:table-cell><text:p>A&amp;B</text:p></table:table-cell>\
             </table:table-row></table:table>\
             </office:spreadsheet></office:body></office:document-content>";
        let p = container("ods-entity", "ods", &[("content.xml", body)]);
        assert_eq!(extract_document_text(&p, "ods").unwrap(), "A&B \n");
    }

    /// A span closing inside a paragraph ends the span, not the paragraph.
    #[test]
    fn text_after_a_nested_span_is_not_dropped() {
        let body = "<office:document-content><office:body><office:text>\
             <text:p>before<text:span>inside</text:span>after</text:p>\
             </office:text></office:body></office:document-content>";
        let p = container("odt-span-tail", "odt", &[("content.xml", body)]);
        assert_eq!(
            extract_document_text(&p, "odt").unwrap(),
            "beforeinsideafter\n"
        );
    }

    /// A self-closed `<text:p/>` has no `End` to hang the break on.
    #[test]
    fn a_self_closed_paragraph_still_breaks_the_line() {
        let body = "<office:document-content><office:body><office:text>\
             <text:p>first</text:p><text:p/><text:p>third</text:p>\
             </office:text></office:body></office:document-content>";
        let p = container("odt-empty-p", "odt", &[("content.xml", body)]);
        assert_eq!(
            extract_document_text(&p, "odt").unwrap(),
            "first\n\nthird\n"
        );
    }
}
