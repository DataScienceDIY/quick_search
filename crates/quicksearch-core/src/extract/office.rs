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

// The shared XML text walk

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

/// The text an `&entity;` or `&#1234;` reference stands for.
///
/// quick-xml 0.41 reports a reference as its own event instead of resolving
/// it inside the surrounding `Text`, so a reader that ignores this event
/// silently drops every `&amp;`, `&lt;` and `&#8217;` from the document —
/// no error, just missing characters in the index. Only the five predefined
/// entities and numeric references are resolvable without a DTD; anything
/// else is a document-defined entity we cannot expand, and is skipped.
fn entity_text(raw: &str) -> Option<String> {
    if let Some(digits) = raw.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        let c = char::from_u32(code)?;
        // `char::from_u32` accepts far more than XML's character production
        // does: every C0 control but tab, newline and carriage return is
        // forbidden, and `&#0;` in particular would put a literal NUL into the
        // indexed text and from there into an FTS5 column. `None` here reaches
        // the callers as the same "unknown entity" error an unexpandable name
        // gets — a `failed_files` row naming the file, which is the visible
        // outcome this extractor prefers to a quietly mangled document.
        let legal = !c.is_control() || matches!(c, '\t' | '\n' | '\r');
        return legal.then(|| String::from(c));
    }
    quick_xml::escape::resolve_predefined_entity(raw).map(String::from)
}

/// Append the text `spec` selects out of `xml` to `out`.
///
/// `in_text` is a flag rather than a depth count, which means a closing
/// `</text:span>` ends the run even though its enclosing `<text:p>` is still
/// open.
fn collect_xml_text(xml: &str, spec: &TextSpec, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_str(xml);
    // Deliberately no `trim_text`: it trims each *event*, and since 0.41 an
    // entity reference splits the character data around it into separate
    // events — so `Jack &amp; Jill` would come back as `Jack&Jill`, with the
    // spaces trimmed off the ends of the two fragments. Nothing needs it
    // either: whitespace between elements arrives while the `in_text`/`in_cell`
    // flag is false and is ignored there, and whitespace *inside* a
    // text-bearing element is content.
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
                out.push_str(&e.decode()?);
                if let Some(sep) = spec.separator {
                    out.push(sep);
                }
            }
            // An entity reference is its own event in 0.41; without this arm
            // every `&amp;` in a document would vanish from the index.
            Ok(Event::GeneralRef(e)) if in_text => {
                let raw = e.decode()?;
                // An entity nothing can expand is an error, as it was when
                // `unescape` resolved these inline: dropping it would take
                // characters out of the indexed text with nothing to show for
                // it, and this reader has no DTD to define one with.
                let text = entity_text(&raw)
                    .ok_or_else(|| format!("Error parsing XML: unknown entity &{};", raw))?;
                out.push_str(&text);
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
            // Propagated rather than ignored: the reader re-reports the same
            // error without advancing, so ignoring it is a hang.
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

/// Cap on the text taken from one *container*, mirroring [`ole::MAX_TEXT_BYTES`].
///
/// [`MAX_XML_BYTES`] bounds each member on its own, which is not the same
/// thing: a workbook or a deck holds one member per sheet or per slide, and
/// nothing stops a small archive from carrying dozens that each inflate to
/// that cap. The truncation to `maximum_text_size` happens only after the
/// whole string is built and handed back, so without a running total the peak
/// is members × 64 MiB — gigabytes from a file measured in megabytes, on every
/// extraction worker at once, and an allocation failure aborts rather than
/// unwinding.
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
        // Per-container budget: see `MAX_TEXT_BYTES`. Whole slides are kept or
        // dropped rather than cut mid-way, which is why the test is here
        // rather than inside the collector.
        if out.len() >= MAX_TEXT_BYTES {
            break;
        }
        let xml = member_text(&mut archive, &name)?;
        collect_xml_text(&xml, &PPTX, &mut out)?;
        out.push_str("\n--- New Slide ---\n");
    }
    Ok(out)
}

// XLSX: shared strings plus cells

/// The workbook's shared-string table, in index order. Absent or unreadable
/// is not an error: a sheet of nothing but numbers has no table at all.
fn shared_strings<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Vec<String> {
    let Ok(xml) = member_text(archive, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    let mut reader = Reader::from_str(&xml);
    // Deliberately no `trim_text`: it trims each *event*, and since 0.41 an
    // entity reference splits the character data around it into separate
    // events — so `Jack &amp; Jill` would come back as `Jack&Jill`, with the
    // spaces trimmed off the ends of the two fragments. Nothing needs it
    // either: whitespace between elements arrives while the `in_text`/`in_cell`
    // flag is false and is ignored there, and whitespace *inside* a
    // text-bearing element is content.
    let mut buf = Vec::new();
    let mut strings = Vec::new();
    let mut in_text = false;
    // One `<t>` is one shared string, but it is not one event: an entity
    // reference inside it arrives separately and splits the character data
    // around it. Accumulated here and pushed on the closing tag, or a cell
    // containing `&amp;` would become three table entries and every later
    // index would point at the wrong one.
    let mut current = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"t" => {
                in_text = true;
                current.clear();
            }
            Ok(Event::Text(e)) if in_text => match e.decode() {
                Ok(s) => current.push_str(&s),
                Err(_) => return strings,
            },
            Ok(Event::GeneralRef(e)) if in_text => {
                // Unlike the other two readers this one cannot fail — a
                // missing table is not an error here — so an entity nothing
                // can expand is simply left out.
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
    // Deliberately no `trim_text`: it trims each *event*, and since 0.41 an
    // entity reference splits the character data around it into separate
    // events — so `Jack &amp; Jill` would come back as `Jack&Jill`, with the
    // spaces trimmed off the ends of the two fragments. Nothing needs it
    // either: whitespace between elements arrives while the `in_text`/`in_cell`
    // flag is false and is ignored there, and whitespace *inside* a
    // text-bearing element is content.
    let mut buf = Vec::new();
    let mut in_cell = false;
    let mut cell_type = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"c" => {
                in_cell = true;
                cell_type.clear();
                // `with_checks(false)`: the default duplicate-attribute-name
                // check compares each name against every name already seen on
                // the tag, which is quadratic in the count and has no bound
                // but the tag's own size (RUSTSEC-2026-0194). A member may be
                // 64 MiB of inflated XML, so one crafted `<c>` can hold
                // millions of attributes and hold this worker for hours —
                // uncancellably, since the stop flag is only read between
                // files. Rejecting duplicate names was never this extractor's
                // job; it wants one attribute and stops at it.
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
                    // A shared-string reference. An index past the end of the
                    // table is a corrupt workbook, not something to guess at.
                    //
                    // `trim` because this reader no longer sets `trim_text`
                    // (see the comment above): a generator that indents its
                    // XML hands `<v>` over as "\n  0\n", and an untrimmed
                    // parse would fail and drop the string with nothing to
                    // show for it. Whitespace around an integer index is not
                    // content, unlike whitespace inside a `<t>`.
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
                    // Whitespace-only fragments are the indentation *between*
                    // a cell's child elements, which reaches this arm now that
                    // the reader no longer sets `trim_text`. Skipped rather
                    // than pushed: `in_cell` is a flag, so it cannot tell an
                    // indent from a value, and a cell whose entire content is
                    // whitespace contributes nothing to a search index either
                    // way. The value itself is pushed whole — no `trim` — so a
                    // deliberate `xml:space="preserve"` inline string keeps
                    // its shape.
                    out.push_str(&text);
                    out.push(' ');
                }
            }
            // See `entity_text`. Only inline values can carry one: a `t="s"`
            // cell's text is an integer index, and an entity inside it would
            // be a corrupt workbook rather than a character to recover.
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
    let mut out = String::new();
    for name in xml_members_under(&mut archive, "xl/worksheets/sheet")? {
        // Per-container budget: see `MAX_TEXT_BYTES`.
        if out.len() >= MAX_TEXT_BYTES {
            break;
        }
        let xml = member_text(&mut archive, &name)?;
        collect_sheet(&xml, &strings, &mut out)?;
    }
    Ok(out)
}

// Dispatch

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

    /// Entity references must survive extraction.
    ///
    /// quick-xml 0.41 reports `&amp;` as its own `GeneralRef` event instead of
    /// resolving it into the surrounding text, so a reader that only handles
    /// `Event::Text` loses the character with no error to show for it. This is
    /// the test that makes that visible: it fails by producing "Blake  Co"
    /// rather than by failing to compile.
    #[test]
    fn entity_references_survive_extraction() {
        let body = "<w:document><w:body><w:p><w:r>\
             <w:t>Blake &amp; Co &lt;tags&gt; &#8217;24 &#x2019;25</w:t>\
             </w:r></w:p></w:body></w:document>";
        let path = container("docx-entities", "docx", &[("word/document.xml", body)]);
        let out = OfficeExtractor.extract(&path).expect("extract");
        assert!(
            out.text.contains("Blake & Co"),
            "predefined entity lost: {:?}",
            out.text
        );
        assert!(
            out.text.contains("<tags>"),
            "angle-bracket entities lost: {:?}",
            out.text
        );
        assert!(
            out.text.contains('\u{2019}'),
            "numeric entities lost: {:?}",
            out.text
        );
        assert!(
            !out.text.contains("&amp;") && !out.text.contains("&#"),
            "entities left unresolved: {:?}",
            out.text
        );
    }

    /// The same, through the shared-string table an `.xlsx` cell indexes into
    /// — a separate reader, and so a separate chance to drop the character.
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
            out.text.contains("Jack & Jill"),
            "entity lost through the shared-string table: {:?}",
            out.text
        );
    }

    /// A shared-string reference must survive an indented `<v>`.
    ///
    /// The reader deliberately does not set `trim_text` (an entity reference
    /// splits the character data around it, and trimming each fragment would
    /// eat the spaces at the split). A `t="s"` cell's `<v>` is an integer
    /// index, though, so a generator that pretty-prints its sheet XML hands
    /// this reader `"\n      0\n    "` — and an untrimmed `parse::<usize>()`
    /// fails, dropping the cell's text with no error and no `failed_files`
    /// row. Whitespace-only fragments between a cell's children must not
    /// reach the output either.
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
            out.text.contains("Marmalade"),
            "the shared string was dropped by an indented index: {:?}",
            out.text
        );
        assert!(
            out.text.contains("17"),
            "the inline value was dropped: {:?}",
            out.text
        );
        // The indentation itself is not content: every run of whitespace in
        // the output should be a separator this reader put there, never a
        // line of the source XML's own layout.
        assert!(
            !out.text.contains("\n  "),
            "sheet indentation reached the indexed text: {:?}",
            out.text
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

    /// Malformed XML is an error, not a hang. An undefined entity inside a
    /// text run is the cheapest way to reach the error arm, and it is a real
    /// shape: tools do emit HTML entities into OOXML.
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

    /// Mismatched tags are caught too — quick_xml checks closing names.
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
