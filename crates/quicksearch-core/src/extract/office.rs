//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS, all zip
//! containers holding XML. Five share one event loop ([`collect_xml_text`],
//! driven by a per-format [`TextSpec`]); XLSX keeps its own two loops because
//! its text is not in the sheet at all but in a shared-string table the cells
//! index into.
//!
//! Each format has template and macro-enabled siblings holding the same parts
//! under the same names (`.pptx`, `.ppsx`, `.pptm`, `.potx` are one parser),
//! so every name an office suite writes, the legacy ones included, is routed
//! to its format's parser. Missing one fails silently: the MIME is still
//! claimed, so the file extracts to no text and is recorded as done, which
//! nothing retries.
//!
//! Dispatch is by file extension, not MIME, because the pre-2007 formats
//! give a document and its template one MIME between them: `.doc` and `.dot`
//! are both `application/msword`, and only the name says which parser to
//! run. [`is_office_mime`] decides *whether* to run, the extension decides
//! *what*, and the two tables have to stay in step; the test at the bottom
//! of this file holds them there.

use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use super::{ExtractError, Extractor, Scratch};

pub struct OfficeExtractor;

/// Every MIME this extractor claims, spelled lowercase because
/// [`super::Registry::find`] folds case before dispatch; the registered
/// spelling of the macro-enabled types is `macroEnabled`.
///
/// Membership only: it answers "is this an office document", never which
/// parser runs. See the module docs for why that is the extension's job.
fn is_office_mime(mime: &str) -> bool {
    matches!(
        mime,
        // Word: .doc/.dot, .docx/.dotx, .docm/.dotm
        "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
            | "application/vnd.ms-word.document.macroenabled.12"
            | "application/vnd.ms-word.template.macroenabled.12"
            // Excel: .xls/.xlt, .xlsx/.xltx, .xlsm/.xltm
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
            | "application/vnd.ms-excel.sheet.macroenabled.12"
            | "application/vnd.ms-excel.template.macroenabled.12"
            // PowerPoint: .ppt/.pps/.pot, .pptx/.ppsx/.potx, .pptm/.ppsm/.potm
            | "application/vnd.ms-powerpoint"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
            | "application/vnd.openxmlformats-officedocument.presentationml.template"
            | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
            | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
            | "application/vnd.ms-powerpoint.template.macroenabled.12"
            // OpenDocument: .odt/.ott, .ods/.ots, .odp/.otp
            | "application/vnd.oasis.opendocument.text"
            | "application/vnd.oasis.opendocument.text-template"
            | "application/vnd.oasis.opendocument.spreadsheet"
            | "application/vnd.oasis.opendocument.spreadsheet-template"
            | "application/vnd.oasis.opendocument.presentation"
            | "application/vnd.oasis.opendocument.presentation-template"
    )
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

/// Append what an `&entity;` or `&#1234;` reference stands for to `out`.
/// quick-xml reports a reference as its own event, so a reader that ignores
/// it silently drops every `&amp;`. Only the five predefined entities and
/// numeric references resolve without a DTD; anything else is an error,
/// since dropping it would take characters out of the indexed text silently.
/// Pushes rather than returns, so a reference costs no allocation.
fn push_entity_text(raw: &str, out: &mut String) -> Result<(), String> {
    let unknown = || format!("Error parsing XML: unknown entity &{};", raw);
    if let Some(digits) = raw.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => digits.parse::<u32>().ok(),
        };
        // `char::from_u32` accepts more than XML's character production does:
        // `&#0;` would put a literal NUL into an FTS5 column.
        return match code.and_then(char::from_u32) {
            Some(c) if !c.is_control() || matches!(c, '\t' | '\n' | '\r') => {
                out.push(c);
                Ok(())
            }
            _ => Err(unknown()),
        };
    }
    out.push_str(quick_xml::escape::resolve_predefined_entity(raw).ok_or_else(unknown)?);
    Ok(())
}

/// Close element `name`: the separator if it ends a run, then a line break
/// if it ends a paragraph.
fn close(spec: &TextSpec, name: &[u8], ends_run: bool, out: &mut String) {
    if ends_run {
        if let Some(sep) = spec.separator {
            out.push(sep);
        }
    }
    if spec.breaks.contains(&name) {
        out.push('\n');
    }
}

/// Append the text `spec` selects from `xml` to `out`, stopping once `out`
/// reaches `limit`. Text elements are counted, not flagged, because ODF nests
/// them; a run and its separator end when the outermost one closes.
fn collect_xml_text<R: BufRead>(
    xml: R,
    spec: &TextSpec,
    out: &mut String,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_reader(xml);
    // No `trim_text`: it trims each event, and an entity reference splits
    // character data into separate events, so `Jack &amp; Jill` would come
    // back as `Jack&Jill`. Whitespace inside a text-bearing element is
    // content; between elements it is ignored anyway.
    buf.clear();
    // Open text-bearing elements; the run ends at zero, not on the innermost
    // close.
    let mut depth = 0usize;

    loop {
        if out.len() >= limit {
            return Ok(());
        }
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) => {
                if spec.text.contains(&e.name().as_ref()) {
                    depth += 1;
                }
            }
            Ok(Event::Text(e)) if depth > 0 => {
                out.push_str(&e.decode()?);
            }
            Ok(Event::GeneralRef(e)) if depth > 0 => {
                push_entity_text(&e.decode()?, out)?;
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let text = spec.text.contains(&name.as_ref());
                if text {
                    depth = depth.saturating_sub(1);
                }
                close(spec, name.as_ref(), text && depth == 0, out);
            }
            // A self-closed element gets no `Start`/`End`, so `<text:p/>`,
            // ODF's blank line, would otherwise lose its paragraph break.
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let ends_run = depth == 0 && spec.text.contains(&name.as_ref());
                close(spec, name.as_ref(), ends_run, out);
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

/// Inflate one whole member into `buf`, the worker's buffer, reused across
/// members and files so members cost no allocation after the first document.
/// Bounded by `limit` ([`super::Limits::inflate`]); the buffer never shrinks,
/// so one hostile document leaves the worker holding up to `limit` for the
/// rest of the pass.
fn member_bytes<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    buf.clear();
    archive
        .by_name(name)?
        .take(limit as u64)
        .read_to_end(buf)?;
    Ok(())
}

/// Names of the `.xml` members under `prefix`, in archive order, not
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
fn single_member(
    path: &Path,
    member: &str,
    spec: &TextSpec,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(path)?;
    let (bytes, events) = scratch.container_bufs();
    member_bytes(&mut archive, member, limits.inflate, bytes)?;
    collect_xml_text(bytes.as_slice(), spec, out, limits.text, events)
}

/// Concatenate what `collect` gets out of each `.xml` member under `prefix`,
/// in archive order, stopping once the text reaches [`super::Limits::text`],
/// the point past which the caller would discard it anyway.
fn collect_members(
    archive: &mut Archive,
    prefix: &str,
    out: &mut String,
    limit: usize,
    mut collect: impl FnMut(&mut Archive, &str, &mut String) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    for name in xml_members_under(archive, prefix)? {
        if out.len() >= limit {
            break;
        }
        collect(archive, &name, out)?;
    }
    Ok(())
}

/// A deck's slides, then its speaker notes, which live in parts of their own
/// (`ppt/notesSlides/`).
///
/// Notes are appended in a block, not under their slides. A note is bound to
/// its slide by the slide's `.rels`, not by number: PowerPoint numbers notes
/// parts in creation order, so a deck whose only note is on slide 5 stores
/// it as `notesSlide1.xml`. Reading the relationships buys a search nothing,
/// and guessing from the number would file notes under the wrong slide in a
/// snippet.
fn extract_pptx(path: &Path, out: &mut String, scratch: &mut Scratch) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(path)?;
    let (bytes, events) = scratch.container_bufs();
    collect_members(
        &mut archive,
        "ppt/slides/slide",
        out,
        limits.text,
        |archive, name, out| {
            member_bytes(archive, name, limits.inflate, bytes)?;
            collect_xml_text(bytes.as_slice(), &PPTX, out, limits.text, events)?;
            out.push_str("\n--- New Slide ---\n");
            Ok(())
        },
    )?;
    // The marker leads here, where the slides' trails: it introduces the note
    // that follows it, rather than separating one from the next.
    collect_members(
        &mut archive,
        "ppt/notesSlides/notesSlide",
        out,
        limits.text,
        |archive, name, out| {
            member_bytes(archive, name, limits.inflate, bytes)?;
            out.push_str("\n--- Speaker Notes ---\n");
            collect_xml_text(bytes.as_slice(), &PPTX, out, limits.text, events)
        },
    )
}

// XLSX: shared strings plus cells

/// The shared-string table, in index order, into `strings`; an absent or
/// unreadable member leaves it empty (a sheet of plain numbers has none).
/// Parsed in full, bounded by [`Limits::inflate`](super::Limits::inflate)
/// rather than `Limits::text`, because cells hold indexes into it: a
/// truncated table would silently mislabel every later cell.
fn shared_strings<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limit: usize,
    bytes: &mut Vec<u8>,
    events: &mut Vec<u8>,
    strings: &mut Vec<String>,
) {
    if member_bytes(archive, "xl/sharedStrings.xml", limit, bytes).is_err() {
        return;
    }
    let mut reader = Reader::from_reader(bytes.as_slice());
    // No `trim_text`; see `collect_xml_text`.
    events.clear();
    let mut in_text = false;
    // One `<t>` is one shared string but not one event (an entity reference
    // splits it); accumulated and pushed on the closing tag, or a cell with
    // `&amp;` would become three table entries.
    let mut current = String::new();
    loop {
        match reader.read_event_into(events) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"t" => {
                in_text = true;
                current.clear();
            }
            // `<t/>`: an empty cell, which LibreOffice, openpyxl and POI all
            // write for a blank. Without this arm the entry is never pushed
            // and every later index is off by one, silently rendering real
            // strings for the wrong cells.
            Ok(Event::Empty(ref e)) if e.name().as_ref() == b"t" => {
                strings.push(String::new());
            }
            Ok(Event::Text(e)) if in_text => match e.decode() {
                Ok(s) => current.push_str(&s),
                Err(_) => return,
            },
            Ok(Event::GeneralRef(e)) if in_text => {
                // This reader cannot fail; an unexpandable entity is left out.
                if let Ok(raw) = e.decode() {
                    let _ = push_entity_text(&raw, &mut current);
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"t" => {
                in_text = false;
                strings.push(std::mem::take(&mut current));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        events.clear();
    }
}

/// One worksheet's cells. A `t="s"` cell holds an index into `strings`
/// rather than text of its own; every other type holds its value inline.
fn collect_sheet<R: BufRead>(
    xml: R,
    strings: &[String],
    out: &mut String,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    /// Where the reader is relative to a `<c>`.
    enum Cell {
        Outside,
        /// A cell holding its value inline.
        Inline,
        /// A `t="s"` cell, whose value is a shared-string index.
        SharedRef,
    }

    let mut reader = Reader::from_reader(xml);
    // No `trim_text`; see `collect_xml_text`.
    buf.clear();
    let mut cell = Cell::Outside;

    loop {
        if out.len() >= limit {
            return Ok(());
        }
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"c" => {
                cell = Cell::Inline;
                // `with_checks(false)`: the duplicate-attribute-name check is
                // quadratic with no bound but the tag's size
                // (RUSTSEC-2026-0194), so one crafted `<c>` in the inflation
                // budget's worth of XML could hold this worker for hours,
                // uncancellably. This extractor wants one attribute anyway.
                for attr in e.attributes().with_checks(false) {
                    let attr = attr?;
                    if attr.key.as_ref() == b"t" {
                        if attr.value.as_ref() == b"s" {
                            cell = Cell::SharedRef;
                        }
                        break;
                    }
                }
            }
            Ok(Event::Text(e)) if !matches!(cell, Cell::Outside) => {
                let text = e.decode()?;
                if matches!(cell, Cell::SharedRef) {
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
                    // cell's children. The value itself is pushed whole (no
                    // `trim`), so `xml:space="preserve"` keeps its shape.
                    out.push_str(&text);
                    out.push(' ');
                }
            }
            // Only inline values can carry one; a `t="s"` cell's is an index.
            Ok(Event::GeneralRef(e)) if matches!(cell, Cell::Inline) => {
                push_entity_text(&e.decode()?, out)?;
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"c" {
                    cell = Cell::Outside;
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

fn extract_xlsx(path: &Path, out: &mut String, scratch: &mut Scratch) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(path)?;
    // All three at once: the sheets are read while the table is live, and
    // separate `&mut scratch` borrows cannot overlap.
    let (bytes, events, strings) = scratch.xlsx_bufs();
    shared_strings(&mut archive, limits.inflate, bytes, events, strings);
    collect_members(
        &mut archive,
        "xl/worksheets/sheet",
        out,
        limits.text,
        |archive, name, out| {
            member_bytes(archive, name, limits.inflate, bytes)?;
            collect_sheet(bytes.as_slice(), strings, out, limits.text, events)
        },
    )
}

// Dispatch

/// Extract text from an office document, chosen by lowercase extension, with
/// each format's aliases (see the module doc). An unhandled extension yields
/// empty text: the MIME was claimed, so the file is simply named unlike its
/// type.
fn extract_document_text(
    path: &Path,
    extension: &str,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    match extension {
        "docx" | "docm" | "dotx" | "dotm" => {
            single_member(path, "word/document.xml", &DOCX, out, scratch)
        }
        "xlsx" | "xlsm" | "xltx" | "xltm" => extract_xlsx(path, out, scratch),
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => extract_pptx(path, out, scratch),
        "odt" | "ott" | "odp" | "otp" => {
            single_member(path, "content.xml", &ODF_TEXT, out, scratch)
        }
        "ods" | "ots" => single_member(path, "content.xml", &ODF_SHEET, out, scratch),
        // Pre-2007 binary formats: a different container entirely.
        "doc" | "dot" | "xls" | "xlt" | "ppt" | "pps" | "pot" => {
            super::ole::extract_ole_text(path, extension, out, scratch)
        }
        _ => Ok(()),
    }
}

impl Extractor for OfficeExtractor {
    fn supports(&self, mime: &str) -> bool {
        is_office_mime(mime)
    }

    fn extract(
        &self,
        path: &Path,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        // From the path, not the MIME: `.docm` and `.docx` share a MIME.
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        extract_document_text(path, &ext, out, scratch)
            .map_err(|e| format!("office extractor {}: {}", path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch() -> Scratch {
        Scratch::new(&crate::config::Config::default())
    }

    /// The one-file forms: these assert on extracted text, not on the buffer
    /// reuse a pool worker gets.
    fn extract_document_text(path: &Path, extension: &str) -> Result<String, Box<dyn Error>> {
        let mut out = String::new();
        super::extract_document_text(path, extension, &mut out, &mut scratch()).map(|()| out)
    }

    /// A reader that only handles `Event::Text` loses `&amp;` with no error;
    /// this fails by producing "Blake  Co".
    #[test]
    fn entity_references_survive_extraction() {
        let body = "<w:document><w:body><w:p><w:r>\
             <w:t>Blake &amp; Co &lt;tags&gt; &#8217;24 &#x2019;25</w:t>\
             </w:r></w:p></w:body></w:document>";
        let path = container("docx-entities", "docx", &[("word/document.xml", body)]);
        let out = crate::extract::extract_text(&OfficeExtractor, &path).expect("extract");
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
        assert!(out.contains('\u{2019}'), "numeric entities lost: {:?}", out);
        assert!(
            !out.contains("&amp;") && !out.contains("&#"),
            "entities left unresolved: {:?}",
            out
        );
    }

    /// The same, through the shared-string table, a separate reader.
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
        let out = crate::extract::extract_text(&OfficeExtractor, &path).expect("extract");
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
        let out = crate::extract::extract_text(&OfficeExtractor, &path).expect("extract");
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

    /// A docx whose text is far larger than any limit under test, and whose
    /// XML compresses to almost nothing: the shape a hostile archive has.
    fn oversized_docx(tag: &str, runs: usize) -> std::path::PathBuf {
        let mut body = String::from("<w:document><w:body>");
        for i in 0..runs {
            body.push_str("<w:p><w:r><w:t>");
            // Distinguishable, so a truncated result can be located.
            body.push_str(&format!("paragraph{:08} ", i));
            body.push_str("</w:t></w:r></w:p>");
        }
        body.push_str("</w:body></w:document>");
        container(tag, "docx", &[("word/document.xml", &body)])
    }

    fn limited(text: usize) -> Scratch {
        let mut config = crate::config::Config::default();
        config.processing.maximum_text_size = text;
        Scratch::new(&config)
    }

    /// Extraction stops at `maximum_text_size` instead of running the
    /// document to its end for the caller to truncate. The upper bound makes
    /// this a real assertion: a result the size of the document would pass a
    /// "≥ limit" check.
    #[test]
    fn a_document_larger_than_the_limit_stops_at_it() {
        // ~2 MiB of text; the limit is 4 KiB, so 99.8% must never be built.
        let path = oversized_docx("docx-oversize", 100_000);
        let mut out = String::new();
        let mut scratch = limited(4096);
        OfficeExtractor
            .extract(&path, &mut out, &mut scratch)
            .expect("extract");

        assert!(
            out.len() >= 4096,
            "stopped short of the limit: {} bytes",
            out.len()
        );
        // One paragraph of overshoot is the documented allowance: the check
        // is per event, not per byte.
        assert!(
            out.len() < 4096 * 2,
            "ran past the limit rather than stopping at it: {} bytes",
            out.len()
        );
        assert!(
            out.starts_with("paragraph00000000"),
            "the kept text is the document's start: {:?}",
            &out[..out.len().min(40)]
        );
    }

    /// The same document with the shipped limits: still bounded, and still
    /// the document's beginning rather than an arbitrary window.
    #[test]
    fn the_default_limits_bound_an_oversized_document() {
        let path = oversized_docx("docx-oversize-default", 100_000);
        let config = crate::config::Config::default();
        let mut out = String::new();
        let mut scratch = Scratch::new(&config);
        OfficeExtractor
            .extract(&path, &mut out, &mut scratch)
            .expect("extract");
        assert!(
            out.len() < config.processing.maximum_text_size * 2,
            "{} bytes for a {}-byte limit",
            out.len(),
            config.processing.maximum_text_size
        );
    }

    const DOCX_BODY: &str = "<w:document><w:body>\
         <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>\
         <w:p><w:r><w:t>Second</w:t></w:r></w:p>\
         </w:body></w:document>";

    const PPTX_SLIDE: &str = "<p:sld><p:cSld><p:spTree><p:sp><p:txBody>\
         <a:p><a:r><a:t>Title</a:t></a:r></a:p>\
         <a:p><a:r><a:t>Body</a:t></a:r></a:p>\
         </p:txBody></p:sp></p:spTree></p:cSld></p:sld>";

    /// A notes part: the note itself, then the slide-number field every
    /// notes slide carries. The field's text is indexed like any other:
    /// suppressing it would mean suppressing dates and footers too.
    const PPTX_NOTES: &str = "<p:notes><p:cSld><p:spTree>\
         <p:sp><p:txBody><a:p><a:r><a:t>Mention the budget</a:t></a:r></a:p></p:txBody></p:sp>\
         <p:sp><p:txBody><a:p><a:fld><a:t>3</a:t></a:fld></a:p></p:txBody></p:sp>\
         </p:spTree></p:cSld></p:notes>";

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

    /// Speaker notes live in parts of their own that the slide text never
    /// mentions.
    #[test]
    fn pptx_extracts_speaker_notes() {
        let p = container(
            "pptx-notes",
            "pptx",
            &[
                ("ppt/slides/slide1.xml", PPTX_SLIDE),
                ("ppt/notesSlides/notesSlide1.xml", PPTX_NOTES),
            ],
        );
        assert_eq!(
            extract_document_text(&p, "pptx").unwrap(),
            "Title\nBody\n\n--- New Slide ---\n\n--- Speaker Notes ---\nMention the budget\n3\n"
        );
    }

    /// The notes prefix must select notes *slides* and nothing that sits
    /// beside them: the master is boilerplate on every deck, and a `.rels`
    /// is a manifest whose text is filenames.
    #[test]
    fn the_notes_master_and_rels_are_not_notes() {
        let p = container(
            "pptx-notes-neighbours",
            "pptx",
            &[
                ("ppt/slides/slide1.xml", PPTX_SLIDE),
                (
                    "ppt/notesMasters/notesMaster1.xml",
                    "<p:notesMaster><a:p><a:r><a:t>MASTERTEXT</a:t></a:r></a:p></p:notesMaster>",
                ),
                (
                    "ppt/notesSlides/_rels/notesSlide1.xml.rels",
                    "<Relationships><Relationship Target=\"RELSTEXT\"/></Relationships>",
                ),
                ("ppt/notesSlides/notesSlide1.xml", PPTX_NOTES),
            ],
        );
        let out = extract_document_text(&p, "pptx").unwrap();
        assert!(out.contains("Mention the budget"), "{:?}", out);
        assert!(
            !out.contains("MASTERTEXT"),
            "the notes master leaked: {:?}",
            out
        );
        assert!(
            !out.contains("RELSTEXT"),
            "a rels manifest leaked: {:?}",
            out
        );
    }

    /// Every alias reaches its format's parser (see the module doc). This is
    /// also the golden set pinning each format's exact extraction shape.
    #[test]
    fn template_and_macro_enabled_names_reach_the_same_parser() {
        /// One parser's aliases: the names, the members of a container to
        /// try them against, and the text every one of them must produce.
        type AliasCase = (
            &'static [&'static str],
            &'static [(&'static str, &'static str)],
            &'static str,
        );
        let cases: &[AliasCase] = &[
            (
                &["docx", "docm", "dotx", "dotm"],
                &[("word/document.xml", DOCX_BODY)],
                "Helloworld\nSecond\n",
            ),
            // No notes part, so no notes marker and no trailing blank run.
            (
                &["pptx", "pptm", "ppsx", "ppsm", "potx", "potm"],
                &[("ppt/slides/slide1.xml", PPTX_SLIDE)],
                "Title\nBody\n\n--- New Slide ---\n",
            ),
            (
                &["xlsx", "xlsm", "xltx", "xltm"],
                &[
                    ("xl/sharedStrings.xml", XLSX_SHARED),
                    ("xl/worksheets/sheet1.xml", XLSX_SHEET),
                ],
                "Shared 42 \nSecond \n",
            ),
            (
                &["odt", "ott", "odp", "otp"],
                &[("content.xml", ODT_BODY)],
                "Heading\nParaspan\n",
            ),
            (&["ods", "ots"], &[("content.xml", ODS_BODY)], "A1 \nB1 \n"),
        ];
        for (extensions, members, expected) in cases {
            for ext in *extensions {
                let p = container(&format!("alias-{ext}"), ext, members);
                assert_eq!(
                    &extract_document_text(&p, ext).unwrap(),
                    expected,
                    ".{ext} did not reach the same parser as its siblings"
                );
            }
        }
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

    /// The separator belongs to the run, so the cell must read `A&B`, not
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

    /// Every name this extractor handles, walked end to end: the sniffer
    /// names the file, [`is_office_mime`] decides whether to run, and the
    /// dispatch in [`extract_document_text`] decides what runs. All three
    /// have to agree, and a disagreement is invisible at either end: an
    /// unclaimed MIME is never offered to an extractor, and a claimed one the
    /// dispatch does not know yields empty text on a file recorded as done.
    const CLAIMED: &[&str] = &[
        "docx", "docm", "dotx", "dotm", "xlsx", "xlsm", "xltx", "xltm", "pptx", "pptm", "ppsx",
        "ppsm", "potx", "potm", "odt", "ott", "ods", "ots", "odp", "otp", "doc", "dot", "xls",
        "xlt", "ppt", "pps",
    ];

    #[test]
    fn every_claimed_name_is_sniffed_claimed_and_parsed() {
        // Nothing opens it: a dispatched extension fails on a missing file,
        // while the fall-through arm reports success with no text. That is
        // what separates "no parser" from "parser found nothing".
        let missing = Path::new("/nonexistent/quicksearch-office-probe");
        for ext in CLAIMED {
            let named = std::path::PathBuf::from(format!("probe.{ext}"));
            let guessed = crate::mime::guess_mime_from_head(&named, b"")
                .unwrap_or_else(|| panic!(".{ext} is sniffed as nothing at all"));
            // Lowercased the way the registry lowercases before dispatch:
            // the registered spelling is `macroEnabled`.
            assert!(
                OfficeExtractor.supports(&guessed.to_ascii_lowercase()),
                ".{ext} is sniffed as {guessed}, which no extractor claims"
            );
            assert!(
                extract_document_text(missing, ext).is_err(),
                ".{ext} is claimed but falls through to the no-op arm, so a real \
                 one would be stored as holding no text"
            );
        }
    }

    /// `.pot` is the one name shared with an unrelated text format (gettext
    /// templates), so it is sniffed from content rather than the name; the
    /// PowerPoint one has to survive that.
    #[test]
    fn a_gettext_template_is_not_a_powerpoint_template() {
        let text = b"msgid \"\"\nmsgstr \"\"\n\"Project-Id-Version: x\\n\"\n";
        let mime = crate::mime::guess_mime_from_head(Path::new("messages.pot"), text);
        assert_eq!(mime, Some("text/plain"));
        assert!(!OfficeExtractor.supports("text/plain"));
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
