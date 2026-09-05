//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS — all zip
//! containers holding XML. Five share one event loop ([`collect_xml_text`],
//! driven by a per-format [`TextSpec`]); XLSX keeps its own two loops because
//! its text is not in the sheet at all but in a shared-string table the cells
//! index into. Dispatch is by file extension, not MIME: `.docm` carries the
//! same MIME as `.docx`, and the extension is what distinguishes them.

use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use super::{ExtractError, Extractor, Scratch};

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

/// Append what an `&entity;` or `&#1234;` reference stands for to `out`,
/// reporting whether it resolved. quick-xml 0.41 reports a reference as its
/// own event, so a reader that ignores it silently drops every `&amp;` from
/// the document. Only the five predefined entities and numeric references are
/// resolvable without a DTD.
///
/// Pushed rather than returned: a document is mostly `&amp;`s and `&#8217;`s,
/// and a `String` per reference was an allocation per *character* of output.
#[must_use]
fn push_entity_text(raw: &str, out: &mut String) -> bool {
    if let Some(digits) = raw.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => digits.parse::<u32>().ok(),
        };
        let Some(c) = code.and_then(char::from_u32) else {
            return false;
        };
        // `char::from_u32` accepts more than XML's character production does:
        // `&#0;` would put a literal NUL into an FTS5 column. `false` becomes
        // the same visible "unknown entity" error an unexpandable name gets.
        if c.is_control() && !matches!(c, '\t' | '\n' | '\r') {
            return false;
        }
        out.push(c);
        return true;
    }
    match quick_xml::escape::resolve_predefined_entity(raw) {
        Some(text) => {
            out.push_str(text);
            true
        }
        None => false,
    }
}

/// Append the text `spec` selects out of `xml` to `out`. Text-bearing
/// elements are counted, not flagged: ODF nests them, and a flag made a
/// span's close end the run, dropping everything up to the paragraph's
/// close. The separator belongs after a *run* — several events since 0.41.
///
/// Reads from a stream and stops at `limit`: the member is never held whole,
/// and a document with more text than the caller will keep is abandoned at
/// the point the surplus begins rather than parsed to the end and truncated.
fn collect_xml_text<R: BufRead>(
    xml: R,
    spec: &TextSpec,
    out: &mut String,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    let mut reader = Reader::from_reader(xml);
    // No `trim_text`: it trims each *event*, and since 0.41 an entity
    // reference splits the character data into separate events — `Jack &amp;
    // Jill` would come back as `Jack&Jill`. Whitespace inside a text-bearing
    // element is content; between elements it is ignored anyway.
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
            // Without this arm every `&amp;` would vanish from the index.
            Ok(Event::GeneralRef(e)) if depth > 0 => {
                let raw = e.decode()?;
                // An unexpandable entity is an error: dropping it takes
                // characters out of the indexed text silently.
                if !push_entity_text(&raw, out) {
                    return Err(format!("Error parsing XML: unknown entity &{};", raw).into());
                }
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

/// Inflate one member into `buf`, which is the **worker's** buffer, reused
/// member after member and file after file: after the first document a
/// container costs no allocation for its members at all.
///
/// A zip declares its sizes but the deflate stream is what actually gets
/// read, so `limit` — [`super::Limits::inflate`], derived from the config —
/// is the only real bound on what a crafted archive can expand to. It used
/// to be a hardcoded 64 MiB per member *and* another 64 MiB per container.
///
/// The buffer keeps whatever capacity the largest member so far needed and
/// does not shrink, so one hostile document leaves that worker holding up to
/// `limit` for the rest of the pass. That is the trade for never allocating
/// in the common case, and it is bounded where it used to be 16× larger.
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

/// A reader over bytes already in hand — what the XML parsers are driven
/// from, so quick-xml streams events out of the worker's buffer rather than
/// a copy of it.
fn xml_over(buf: &[u8]) -> Cursor<&[u8]> {
    Cursor::new(buf)
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
    collect_xml_text(xml_over(bytes), spec, out, limits.text, events)
}

/// Concatenate what `collect` gets out of each `.xml` member under `prefix`,
/// in archive order, stopping once the text reaches [`super::Limits::text`]
/// — the point past which the caller would discard it anyway.
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
            collect_xml_text(xml_over(bytes), &PPTX, out, limits.text, events)?;
            out.push_str("\n--- New Slide ---\n");
            Ok(())
        },
    )
}

// XLSX: shared strings plus cells

/// The workbook's shared-string table, in index order, into `strings`.
/// Absent or unreadable is not an error: a sheet of nothing but numbers has
/// no table at all.
///
/// **This one member is read whole**, unlike every other: a `t="s"` cell
/// holds an *index* into the table, so a table cut short does not lose the
/// tail — it renders the wrong string for every cell past the cut, silently.
/// It is bounded by the same inflation budget and by nothing else.
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
    let mut reader = Reader::from_reader(xml_over(bytes));
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
            // `<t/>` — an empty cell, which LibreOffice, openpyxl and POI
            // all write for a blank. Without this arm the entry is never
            // pushed and **every later index is off by one**, silently
            // rendering real strings for the wrong cells.
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
    let mut reader = Reader::from_reader(xml);
    // No `trim_text`; see `collect_xml_text`.
    buf.clear();
    let mut in_cell = false;
    let mut cell_type = String::new();

    loop {
        if out.len() >= limit {
            return Ok(());
        }
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"c" => {
                in_cell = true;
                cell_type.clear();
                // `with_checks(false)`: the duplicate-attribute-name check is
                // quadratic with no bound but the tag's size
                // (RUSTSEC-2026-0194), so one crafted `<c>` in the inflation
                // budget's worth of XML could hold this worker for hours,
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
                if !push_entity_text(&raw, out) {
                    return Err(format!("Error parsing XML: unknown entity &{};", raw).into());
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
            collect_sheet(xml_over(bytes), strings, out, limits.text, events)
        },
    )
}

// Dispatch

/// Extract text from an office document, chosen by lowercase extension. An
/// unhandled extension yields empty text: the MIME was claimed, so the file
/// was simply named unlike its type.
fn extract_document_text(
    path: &Path,
    extension: &str,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    match extension {
        "docx" => single_member(path, "word/document.xml", &DOCX, out, scratch),
        "xlsx" => extract_xlsx(path, out, scratch),
        "pptx" => extract_pptx(path, out, scratch),
        "odt" | "odp" => single_member(path, "content.xml", &ODF_TEXT, out, scratch),
        "ods" => single_member(path, "content.xml", &ODF_SHEET, out, scratch),
        // Pre-2007 binary formats: a different container entirely.
        "doc" | "xls" | "ppt" => super::ole::extract_ole_text(path, extension, out, scratch),
        _ => Ok(()),
    }
}

impl Extractor for OfficeExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime_to_ext(mime).is_some()
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

    fn office_extract(path: &Path) -> Result<String, ExtractError> {
        let mut out = String::new();
        OfficeExtractor.extract(path, &mut out, &mut scratch()).map(|()| out)
    }

    /// A reader that only handles `Event::Text` loses `&amp;` with no error;
    /// this fails by producing "Blake  Co".
    #[test]
    fn entity_references_survive_extraction() {
        let body = "<w:document><w:body><w:p><w:r>\
             <w:t>Blake &amp; Co &lt;tags&gt; &#8217;24 &#x2019;25</w:t>\
             </w:r></w:p></w:body></w:document>";
        let path = container("docx-entities", "docx", &[("word/document.xml", body)]);
        let out = office_extract(&path).expect("extract");
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
        let out = office_extract(&path).expect("extract");
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
        let out = office_extract(&path).expect("extract");
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
    /// XML compresses to almost nothing — the shape a hostile archive has.
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

    /// Extraction **stops** at `maximum_text_size` instead of running the
    /// document to its end for the caller to truncate. The margin is what
    /// makes this a real assertion: the old code produced every byte, so a
    /// result the size of the document would pass a "≥ limit" check.
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
        // One paragraph of overshoot is the documented allowance — the check
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
