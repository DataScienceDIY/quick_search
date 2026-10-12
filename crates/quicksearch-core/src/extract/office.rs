//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS, all zip
//! containers holding XML. Five share one event loop ([`collect_xml_text`],
//! driven by a per-format [`TextSpec`]); XLSX keeps its own two loops because
//! its text is not in the sheet at all but in a shared-string table the cells
//! index into. The pre-2007 binary formats are [`super::ole`]'s.
//!
//! Dispatch is by MIME: each claimed MIME names exactly one parser
//! ([`parser_for`]), so a file reaches a parser under any name the sniff
//! resolves to that MIME. A format's template and macro-enabled siblings hold
//! the same parts under the same names and share its parser.

use std::cell::Cell;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use super::{ExtractError, Extractor, Scratch};

/// The zip formats. The legacy binary ones share [`parser_for`]'s table but
/// are [`super::ole::OleExtractor`]'s, which the registry sandboxes.
pub struct OfficeExtractor;

/// The parser a claimed MIME runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Parser {
    Docx,
    Xlsx,
    Pptx,
    /// ODT and ODP ([`ODF_TEXT`]).
    OdfText,
    OdfSheet,
    Doc,
    Xls,
    Ppt,
}

impl Parser {
    /// A pre-2007 binary format: an OLE2 compound file, not a zip.
    pub(crate) fn is_ole(self) -> bool {
        matches!(self, Parser::Doc | Parser::Xls | Parser::Ppt)
    }
}

/// The parser for `mime`, spelled lowercase because the registry folds case
/// before dispatch (the registered spelling of the macro-enabled types is
/// `macroEnabled`); `None` for a MIME this extractor does not claim.
pub(crate) fn parser_for(mime: &str) -> Option<Parser> {
    Some(match mime {
        // .docx/.dotx, .docm/.dotm
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
        | "application/vnd.ms-word.document.macroenabled.12"
        | "application/vnd.ms-word.template.macroenabled.12" => Parser::Docx,
        // .xlsx/.xltx, .xlsm/.xltm
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
        | "application/vnd.ms-excel.sheet.macroenabled.12"
        | "application/vnd.ms-excel.template.macroenabled.12" => Parser::Xlsx,
        // .pptx/.ppsx/.potx, .pptm/.ppsm/.potm
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
        | "application/vnd.openxmlformats-officedocument.presentationml.template"
        | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
        | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
        | "application/vnd.ms-powerpoint.template.macroenabled.12" => Parser::Pptx,
        // .odt/.ott, .odp/.otp
        "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.text-template"
        | "application/vnd.oasis.opendocument.presentation"
        | "application/vnd.oasis.opendocument.presentation-template" => Parser::OdfText,
        // .ods/.ots
        "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.oasis.opendocument.spreadsheet-template" => Parser::OdfSheet,
        // One MIME for all of a legacy format's names (.doc/.dot/.wbk/.wiz,
        // .xls/.xlt/.xla/.xlk, .ppt/.pps/.pot/.ppa/.pwz). `.xll` (a DLL) and
        // `.slk` (SYLK text) share Excel's and fail as no compound file.
        "application/msword" => Parser::Doc,
        "application/vnd.ms-excel" => Parser::Xls,
        "application/vnd.ms-powerpoint" => Parser::Ppt,
        _ => return None,
    })
}

// The shared XML text walk

/// Which elements of a format's XML carry text, and where words and lines
/// end. Names are local (`t`, not `w:t`): the prefix is whatever the
/// producer bound the namespace to, and nothing requires `w`, `a`, `x` or
/// `text`. The lists are matched independently: ODF's `text:p` both holds
/// text and breaks, `w:p` breaks but holds nothing directly, `text:span`
/// ends a run without ending the line.
struct TextSpec {
    text: &'static [&'static [u8]],
    /// Elements that end a line, emitting `'\n'` as they close: paragraphs,
    /// and line breaks such as `w:br`.
    breaks: &'static [&'static [u8]],
    /// Elements that stand for whitespace (`w:tab`, `text:s`), emitting one
    /// `' '` as they close unless the text is empty or already ends in
    /// whitespace. A paragraph's tab-stop definitions (`w:tabs/w:tab`) come
    /// before its text, right after the previous paragraph's `'\n'`, so they
    /// add nothing.
    spaces: &'static [&'static [u8]],
    /// Emitted after each text run; spreadsheets separate cells with it.
    separator: Option<char>,
}

const DOCX: TextSpec = TextSpec {
    text: &[b"t"],
    breaks: &[b"p", b"br", b"cr"],
    spaces: &[b"tab"],
    separator: None,
};

/// No `spaces`: DrawingML writes a tab as a literal `'\t'` inside `a:t`, and
/// its `a:tab` elements are tab-stop definitions.
const PPTX: TextSpec = TextSpec {
    text: &[b"t"],
    breaks: &[b"p", b"br"],
    spaces: &[],
    separator: None,
};

/// ODT and ODP are the same ODF prose as far as extraction is concerned.
const ODF_TEXT: TextSpec = TextSpec {
    text: &[b"p", b"h", b"span"],
    breaks: &[b"p", b"h", b"line-break"],
    spaces: &[b"tab", b"s"],
    separator: None,
};

const ODF_SHEET: TextSpec = TextSpec {
    text: &[b"p", b"span"],
    breaks: &[b"p", b"line-break"],
    spaces: &[b"tab", b"s"],
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

/// Close element `name`, a local name: the separator if it ends a run, then
/// a line break if it ends a line, or a space if it stands for one.
fn close(spec: &TextSpec, name: &[u8], ends_run: bool, out: &mut String) {
    if ends_run {
        if let Some(sep) = spec.separator {
            out.push(sep);
        }
    }
    if spec.breaks.contains(&name) {
        out.push('\n');
    } else if spec.spaces.contains(&name) && !out.is_empty() && !out.ends_with(char::is_whitespace)
    {
        out.push(' ');
    }
}

/// Append the text `spec` selects from `xml` to `out`, stopping once `out`
/// reaches `limit` or [`member`]'s budget cuts the XML off. Text elements are
/// counted, not flagged, because ODF nests them; a run and its separator end
/// when the outermost one closes.
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
                if spec.text.contains(&e.local_name().as_ref()) {
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
                let name = e.local_name();
                let text = spec.text.contains(&name.as_ref());
                if text {
                    depth = depth.saturating_sub(1);
                }
                close(spec, name.as_ref(), text && depth == 0, out);
            }
            // A self-closed element gets no `Start`/`End`, so `<text:p/>`,
            // ODF's blank line, would otherwise lose its paragraph break.
            Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let ends_run = depth == 0 && spec.text.contains(&name.as_ref());
                close(spec, name.as_ref(), ends_run, out);
            }
            Ok(Event::Eof) => break,
            // The member's inflate budget ran out: the text so far stands.
            Err(quick_xml::Error::Io(e)) if is_spent(&e) => break,
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

fn open_container(mut file: File) -> Result<Archive, Box<dyn Error>> {
    check_directory(&mut file)?;
    Ok(ZipArchive::new(BufReader::new(file))?)
}

/// The fixed part of a central directory header: the least room one entry
/// takes there.
const CENTRAL_HEADER_LEN: u64 = 46;

/// Refuse a zip whose end records declare more entries than its central
/// directory has room for, or a directory that runs past the end of the
/// file. zip 0.6 sizes its member tables from the declared count before it
/// reads a header, and a zip64 record can declare billions: a 50 MiB x.xlsx
/// asks for gigabytes, and the failed allocation aborts the process. The
/// records are read where [`ZipArchive::new`] reads them: the last end
/// record in the final 64 KiB, the zip64 locator before it, and the zip64
/// record the locator points at. zip also searches on past that offset, for
/// data prepended to the archive, which no office document carries, so a
/// zip64 record anywhere else is refused. Any other damage is zip's to
/// report.
fn check_directory<R: Read + Seek>(reader: &mut R) -> Result<(), Box<dyn Error>> {
    // The end record's fixed part, then the most comment it can carry and
    // the zip64 locator, which sits right before it.
    const END: u64 = 22;
    const TAIL: u64 = END + u16::MAX as u64 + 20;
    let len = reader.seek(SeekFrom::End(0))?;
    if len < END {
        return Ok(());
    }
    let start = len.saturating_sub(TAIL);
    reader.seek(SeekFrom::Start(start))?;
    let mut tail = vec![0; (len - start) as usize];
    reader.read_exact(&mut tail)?;
    let at = |pos: u64| &tail[(pos - start) as usize..];
    let le = |b: &[u8]| b.iter().rev().fold(0, |n, &byte| n << 8 | u64::from(byte));

    // The one zip finds, searching back from the end.
    let lowest = len.saturating_sub(END + u16::MAX as u64);
    let Some(end) = (lowest..=len - END)
        .rev()
        .find(|&pos| at(pos).starts_with(b"PK\x05\x06"))
    else {
        return Ok(());
    };
    let record = at(end);
    // The entries on this disk, which zip counts, then the directory's size
    // and offset.
    let mut directory = (le(&record[8..10]), le(&record[12..16]), le(&record[16..20]));
    // Placed by the comment length back from the file's end, as zip does.
    let locator = (len - END)
        .checked_sub(20 + le(&record[20..22]))
        .map(at)
        .filter(|locator| locator.starts_with(b"PK\x06\x07"));
    if let Some(locator) = locator {
        let mut zip64 = [0u8; 56];
        reader.seek(SeekFrom::Start(le(&locator[8..16])))?;
        reader.read_exact(&mut zip64)?;
        if !zip64.starts_with(b"PK\x06\x06") {
            return Err("the zip64 end record is not where its locator says".into());
        }
        directory = (le(&zip64[32..40]), le(&zip64[40..48]), le(&zip64[48..56]));
    }
    let (count, size, offset) = directory;
    if count > size / CENTRAL_HEADER_LEN {
        return Err(format!(
            "the central directory declares {} entries in {} bytes",
            count, size
        )
        .into());
    }
    if offset.checked_add(size).is_none_or(|end| end > len) {
        return Err("the central directory runs past the end of the file".into());
    }
    Ok(())
}

/// The least opening a member costs the archive's budget on top of the bytes
/// it inflates to, so a member that inflates to nothing is not free: a
/// crafted central directory can name one small body thousands of times.
/// Bounds what [`Limits::container`](super::Limits::container) admits to
/// about 16k members.
const MEMBER_FEE: usize = 4 * 1024;

/// The error [`Metered`] reads as once a budget is spent. Neither EOF, which
/// would hand quick-xml a member cut mid-tag or mid-character to report as
/// malformed, nor `InvalidData`, which quick-xml takes for an encoding
/// error, nor `Interrupted`, which readers retry.
#[derive(Debug)]
struct Spent;

impl std::fmt::Display for Spent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("inflate budget spent")
    }
}

impl Error for Spent {}

/// Whether a parser's I/O error is [`Metered`]'s cut rather than a real one.
fn is_spent(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Spent>())
}

/// A member's inflated bytes, counted against its own cap and the budget
/// the whole archive shares, failing with [`Spent`] once either is gone.
struct Metered<'a, R> {
    inner: R,
    /// What this member may still inflate to.
    cap: usize,
    /// What the archive may still inflate to, across all its members.
    left: &'a Cell<usize>,
}

impl<R: Read> Read for Metered<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let allowed = self.cap.min(self.left.get());
        if allowed == 0 {
            return Err(io::Error::other(Spent));
        }
        let want = buf.len().min(allowed);
        let n = self.inner.read(&mut buf[..want])?;
        self.cap -= n;
        self.left.set(self.left.get() - n);
        Ok(n)
    }
}

/// One member, inflated as the parser reads it and bounded by `limit`
/// ([`super::Limits::inflate`]) and by what the archive's budget `left`
/// still holds. Opening it charges its compressed size, at least
/// [`MEMBER_FEE`]: reading the input costs the archive as inflating does,
/// and overlapping directory entries can name one large stream that inflates
/// to nothing. Inflation stops where parsing does, at the text limit, and
/// nothing holds a whole member. At either bound the reader fails with
/// [`Spent`], which the parsers take as the member's end, keeping the text
/// before the cut. Measured against inflating each member into a per-worker
/// buffer that kept its largest size (`extractprobe`, 1.25 MB `document.xml`
/// members): peak live per worker 2.3 → 0.4 MiB, VmHWM 20.5 → 14.4 MiB,
/// throughput unchanged.
fn member<'a, R: Read + Seek>(
    archive: &'a mut ZipArchive<R>,
    name: &str,
    limit: usize,
    left: &'a Cell<usize>,
) -> Result<impl BufRead + 'a, Box<dyn Error>> {
    let inner = archive.by_name(name)?;
    let input = usize::try_from(inner.compressed_size()).unwrap_or(usize::MAX);
    left.set(left.get().saturating_sub(input.max(MEMBER_FEE)));
    Ok(BufReader::new(Metered {
        inner,
        cap: limit,
        left,
    }))
}

/// Names of the `.xml` members under `prefix`, in number order: shorter
/// first, so `slide2.xml` comes before `slide10.xml`. From the central
/// directory the archive already holds; naming members through `by_index`
/// seeks to and reads each one's local header, about 3 `lseek` and 2 reads a
/// member, twice for a deck. Measured (`extractprobe`, 20 copies of a real
/// 86-member deck plus 4 other documents): `lseek` + `read` calls 22,289 →
/// 4,444, throughput 1,800 → 2,330 files/s.
fn xml_members_under<R: Read + Seek>(archive: &ZipArchive<R>, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|name| name.starts_with(prefix) && name.ends_with(".xml"))
        .map(str::to_string)
        .collect();
    names.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    names
}

/// A format whose whole text lives in one member under one spec.
fn single_member(
    file: File,
    name: &str,
    spec: &TextSpec,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(file)?;
    // One member: its own cap is the whole bound.
    let unbudgeted = Cell::new(usize::MAX);
    let xml = member(&mut archive, name, limits.inflate, &unbudgeted)?;
    collect_xml_text(xml, spec, out, limits.text, scratch.events())
}

/// Concatenate what `collect` gets out of each member in `names`, in order,
/// stopping once the text reaches [`super::Limits::text`], the point past
/// which the caller would discard it anyway, or once the archive's budget
/// `left` is spent: members holding no text never bring the text limit
/// closer, so without it a deck of empty slides would inflate every one.
fn collect_members<S: AsRef<str>>(
    archive: &mut Archive,
    names: &[S],
    out: &mut String,
    limit: usize,
    left: &Cell<usize>,
    mut collect: impl FnMut(&mut Archive, &str, &mut String) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    for name in names {
        if out.len() >= limit || left.get() == 0 {
            break;
        }
        collect(archive, name.as_ref(), out)?;
    }
    Ok(())
}

/// The parts besides the body that hold what a Word document's author
/// wrote, read after it in this order. Most documents have none of them.
/// Headers and footers are left out: they repeat on every page and carry
/// boilerplate (titles, page numbers, dates), not the document's text.
const DOCX_NOTES: [&str; 3] = [
    "word/footnotes.xml",
    "word/endnotes.xml",
    "word/comments.xml",
];

/// A Word document's body, then its footnotes, endnotes and comments, under
/// one text limit and one inflate budget. A missing body is an error; a
/// note part that is absent or will not open is skipped, as an absent
/// shared-string table is.
fn extract_docx(file: File, out: &mut String, scratch: &mut Scratch) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(file)?;
    let left = Cell::new(limits.container);
    let events = scratch.events();
    let xml = member(&mut archive, "word/document.xml", limits.inflate, &left)?;
    collect_xml_text(xml, &DOCX, out, limits.text, events)?;
    collect_members(
        &mut archive,
        &DOCX_NOTES,
        out,
        limits.text,
        &left,
        |archive, name, out| {
            let Ok(xml) = member(archive, name, limits.inflate, &left) else {
                return Ok(());
            };
            collect_xml_text(xml, &DOCX, out, limits.text, events)
        },
    )
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
fn extract_pptx(file: File, out: &mut String, scratch: &mut Scratch) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(file)?;
    // Slides and notes draw on one budget.
    let left = Cell::new(limits.container);
    let events = scratch.events();
    let slides = xml_members_under(&archive, "ppt/slides/slide");
    collect_members(
        &mut archive,
        &slides,
        out,
        limits.text,
        &left,
        |archive, name, out| {
            let xml = member(archive, name, limits.inflate, &left)?;
            collect_xml_text(xml, &PPTX, out, limits.text, events)?;
            out.push_str("\n--- New Slide ---\n");
            Ok(())
        },
    )?;
    // The marker leads here, where the slides' trails: it introduces the note
    // that follows it, rather than separating one from the next.
    let notes = xml_members_under(&archive, "ppt/notesSlides/notesSlide");
    collect_members(
        &mut archive,
        &notes,
        out,
        limits.text,
        &left,
        |archive, name, out| {
            let xml = member(archive, name, limits.inflate, &left)?;
            out.push_str("\n--- Speaker Notes ---\n");
            collect_xml_text(xml, &PPTX, out, limits.text, events)
        },
    )
}

// XLSX: shared strings plus cells

/// The shared-string table, in index order, into `strings`; an absent or
/// unreadable member leaves it empty (a sheet of plain numbers has none).
/// One entry per `<si>`, whatever it holds: a rich-text string is several
/// `<t>` runs, and a phonetic reading (`<rPh>`) carries a `<t>` of its own
/// that is not the string's text. Cells hold indexes into the table, so an
/// entry miscounted mislabels every later cell. Parsed in full, bounded by
/// the inflate cap and the archive's budget rather than `Limits::text`,
/// because the sheets may reach any entry; a table cut short only drops the
/// cells whose entries were cut, their indexes falling out of range.
fn shared_strings<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limit: usize,
    left: &Cell<usize>,
    events: &mut Vec<u8>,
    strings: &mut Vec<String>,
) {
    let Ok(xml) = member(archive, "xl/sharedStrings.xml", limit, left) else {
        return;
    };
    let mut reader = Reader::from_reader(xml);
    // No `trim_text`; see `collect_xml_text`.
    events.clear();
    let mut in_phonetic = false;
    let mut in_text = false;
    // An entry is many events (runs, and entity references splitting each);
    // accumulated and pushed when its `<si>` closes.
    let mut current = String::new();
    loop {
        match reader.read_event_into(events) {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"si" => current.clear(),
                b"rPh" => in_phonetic = true,
                b"t" => in_text = !in_phonetic,
                _ => {}
            },
            // `<si/>` is still an entry, or every later index is off by one.
            Ok(Event::Empty(ref e)) if e.local_name().as_ref() == b"si" => {
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
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"si" => strings.push(std::mem::take(&mut current)),
                b"rPh" => in_phonetic = false,
                b"t" => in_text = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        events.clear();
    }
}

/// One worksheet's cells, values only: the text of each `<v>`, or of an
/// inline string's `<t>` runs outside their phonetic readings (`<rPh>`),
/// with one separator after each cell that produced any. Formulas (`<f>`)
/// are not indexed; the value is what the sheet shows. A `t="s"` cell's
/// value is an index into `strings` rather than text of its own.
fn collect_sheet<R: BufRead>(
    xml: R,
    strings: &[String],
    out: &mut String,
    limit: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    /// Where the reader is relative to a `<c>`.
    enum At {
        Outside,
        /// A cell holding its value inline.
        Inline,
        /// A `t="s"` cell, whose value is a shared-string index.
        SharedRef,
    }

    let mut reader = Reader::from_reader(xml);
    // No `trim_text`; see `collect_xml_text`.
    buf.clear();
    let mut at = At::Outside;
    // Where the open cell's text starts, to tell whether it produced any.
    let mut cell_start = 0;
    let mut in_phonetic = false;
    // In a `<v>`, or a `<t>` outside any `<rPh>`: the only text taken, so
    // the whitespace indenting a cell's children never is.
    let mut in_value = false;

    loop {
        if out.len() >= limit {
            return Ok(());
        }
        match reader.read_event_into(buf) {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"c" => {
                    at = At::Inline;
                    cell_start = out.len();
                    // `with_checks(false)`: the duplicate-attribute-name check
                    // is quadratic with no bound but the tag's size
                    // (RUSTSEC-2026-0194), so one crafted `<c>` in the
                    // inflation budget's worth of XML could hold this worker
                    // for hours, uncancellably. This extractor wants one
                    // attribute anyway.
                    for attr in e.attributes().with_checks(false) {
                        let attr = attr?;
                        if attr.key.as_ref() == b"t" {
                            if attr.value.as_ref() == b"s" {
                                at = At::SharedRef;
                            }
                            break;
                        }
                    }
                }
                b"v" => in_value = true,
                b"t" => in_value = !in_phonetic,
                b"rPh" => in_phonetic = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_value && !matches!(at, At::Outside) => {
                let text = e.decode()?;
                if matches!(at, At::SharedRef) {
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
                    }
                } else {
                    // Whole, no `trim`: `xml:space="preserve"` keeps its shape,
                    // and the space between two rich-text runs is a run.
                    out.push_str(&text);
                }
            }
            // Only inline values can carry one; a `t="s"` cell's is an index.
            Ok(Event::GeneralRef(e)) if in_value && matches!(at, At::Inline) => {
                push_entity_text(&e.decode()?, out)?;
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"c" => {
                    if out.len() > cell_start {
                        out.push(' ');
                    }
                    at = At::Outside;
                }
                b"row" => out.push('\n'),
                b"v" | b"t" => in_value = false,
                b"rPh" => in_phonetic = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            // The member's inflate budget ran out: the cells so far stand.
            Err(quick_xml::Error::Io(e)) if is_spent(&e) => break,
            Err(e) => return Err(format!("Error parsing XML: {}", e).into()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn extract_xlsx(file: File, out: &mut String, scratch: &mut Scratch) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut archive = open_container(file)?;
    // The table and the sheets draw on one budget.
    let left = Cell::new(limits.container);
    // Both at once: the sheets are read while the table is live, and
    // separate `&mut scratch` borrows cannot overlap.
    let (events, strings) = scratch.xlsx_bufs();
    shared_strings(&mut archive, limits.inflate, &left, events, strings);
    let sheets = xml_members_under(&archive, "xl/worksheets/sheet");
    collect_members(
        &mut archive,
        &sheets,
        out,
        limits.text,
        &left,
        |archive, name, out| {
            let xml = member(archive, name, limits.inflate, &left)?;
            collect_sheet(xml, strings, out, limits.text, events)
        },
    )
}

// Dispatch

/// Run `parser` over `file`, an opened office document.
fn extract_document(
    file: File,
    parser: Parser,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    match parser {
        Parser::Docx => extract_docx(file, out, scratch),
        Parser::Xlsx => extract_xlsx(file, out, scratch),
        Parser::Pptx => extract_pptx(file, out, scratch),
        Parser::OdfText => single_member(file, "content.xml", &ODF_TEXT, out, scratch),
        Parser::OdfSheet => single_member(file, "content.xml", &ODF_SHEET, out, scratch),
        // Pre-2007 binary formats: a different container entirely.
        Parser::Doc | Parser::Xls | Parser::Ppt => {
            super::ole::extract_ole_text(file, parser, out, scratch)
        }
    }
}

/// What both office extractors run: the parser `mime` names, over the file
/// [`super::open_for_extract`] opens.
pub(super) fn extract_office(
    path: &Path,
    mime: &str,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), ExtractError> {
    // Unreachable through the registry, which offers only what `supports`
    // accepted.
    let parser = parser_for(mime).ok_or_else(|| format!("no office parser for {}", mime))?;
    let (file, _) = super::open_for_extract(path, scratch.limits().read)?;
    extract_document(file, parser, out, scratch)
        .map_err(|e| format!("office extractor {}: {}", path.display(), e))
}

impl Extractor for OfficeExtractor {
    fn supports(&self, mime: &str) -> bool {
        parser_for(mime).is_some_and(|parser| !parser.is_ole())
    }

    fn extract(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        extract_office(path, mime, out, scratch)
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
    fn extract_with(path: &Path, parser: Parser) -> Result<String, Box<dyn Error>> {
        let mut out = String::new();
        extract_document(File::open(path)?, parser, &mut out, &mut scratch()).map(|()| out)
    }

    /// A reader that only handles `Event::Text` loses `&amp;` with no error;
    /// this fails by producing "Blake  Co".
    #[test]
    fn entity_references_survive_extraction() {
        let body = "<w:document><w:body><w:p><w:r>\
             <w:t>Blake &amp; Co &lt;tags&gt; &#8217;24 &#x2019;25</w:t>\
             </w:r></w:p></w:body></w:document>";
        let path = container("docx-entities", "docx", &[("word/document.xml", body)]);
        let out = extract_with(&path, Parser::Docx).expect("extract");
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
        let out = extract_with(&path, Parser::Xlsx).expect("extract");
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
        let out = extract_with(&path, Parser::Xlsx).expect("extract");
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

    /// The zip formats here, the legacy ones in the OLE extractor, which the
    /// registry sandboxes: every office MIME is claimed by exactly one.
    #[test]
    fn supports_docx_and_friends() {
        let ole = super::super::ole::OleExtractor;
        for m in [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.oasis.opendocument.text",
        ] {
            assert!(OfficeExtractor.supports(m), "should support {}", m);
            assert!(!ole.supports(m), "the OLE extractor claims {}", m);
        }
        for m in [
            "application/msword",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
        ] {
            assert!(ole.supports(m), "the OLE extractor should support {}", m);
            assert!(!OfficeExtractor.supports(m), "claims {}", m);
        }
        assert!(!OfficeExtractor.supports("image/png"));
        assert!(!ole.supports("image/png"));
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

    /// The default limits with a container budget of `container` bytes, so a
    /// test of it inflates a few hundred KiB rather than the shipped 64 MiB.
    fn budgeted(container: usize) -> Scratch {
        let mut scratch = scratch();
        scratch.limits.container = container;
        scratch
    }

    /// Extract `path` with `parser` under `scratch`, on a watched worker:
    /// these inputs are built to run long, and a hang must fail the test.
    fn extract_on_worker(
        path: &Path,
        parser: Parser,
        mut scratch: Scratch,
    ) -> Result<String, String> {
        let path = path.to_path_buf();
        crate::extract::on_worker(move || {
            let mut out = String::new();
            let file = File::open(&path).map_err(|e| e.to_string())?;
            extract_document(file, parser, &mut out, &mut scratch)
                .map(|()| out)
                .map_err(|e| e.to_string())
        })
    }

    /// Element `root` around `pad` bytes of whitespace: a member that
    /// inflates to that much, holds no text and deflates to almost nothing.
    fn textless(root: &str, pad: usize) -> String {
        format!("<{root}>{}</{root}>", " ".repeat(pad))
    }

    /// Each of `names` paired with its body in `bodies`, for the many-member
    /// archives the budget tests build.
    fn pairs<'a>(names: &'a [String], bodies: &[&'a str]) -> Vec<(&'a str, &'a str)> {
        names
            .iter()
            .zip(bodies)
            .map(|(name, body)| (name.as_str(), *body))
            .collect()
    }

    /// A zip whose central directory lists every one of `names`, all
    /// pointing at one local header at offset 0: many members from one
    /// body's bytes on disk. No zip writer will produce it. The body is
    /// deflated behind `padding` empty stored blocks, 5 bytes of input each
    /// that inflate to nothing.
    fn shared_body_zip(
        tag: &str,
        ext: &str,
        names: &[String],
        body: &[u8],
        padding: usize,
    ) -> std::path::PathBuf {
        let data = crate::extract::padded_deflate(body, padding);
        let crc = crc32fast::hash(body).to_le_bytes();
        let compressed = (data.len() as u32).to_le_bytes();
        let size = (body.len() as u32).to_le_bytes();
        let mut zip = Vec::new();
        zip.extend_from_slice(b"PK\x03\x04");
        // Version needed, flags, method (deflate), time, date (1980-01-01).
        zip.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0x21, 0]);
        zip.extend_from_slice(&crc);
        zip.extend_from_slice(&compressed);
        zip.extend_from_slice(&size);
        zip.extend_from_slice(&(names[0].len() as u16).to_le_bytes());
        zip.extend_from_slice(&[0, 0]); // extra length
        zip.extend_from_slice(names[0].as_bytes());
        zip.extend_from_slice(&data);
        let directory = zip.len();
        for name in names {
            zip.extend_from_slice(b"PK\x01\x02");
            // Version made by, version needed, flags, method, time, date.
            zip.extend_from_slice(&[20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0x21, 0]);
            zip.extend_from_slice(&crc);
            zip.extend_from_slice(&compressed);
            zip.extend_from_slice(&size);
            zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
            // Extra and comment lengths, disk, internal and external
            // attributes, then the local header's offset: 0 for every entry.
            zip.extend_from_slice(&[0; 16]);
            zip.extend_from_slice(name.as_bytes());
        }
        let count = (names.len() as u16).to_le_bytes();
        let directory_size = (zip.len() - directory) as u32;
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0; 4]); // disk numbers
        zip.extend_from_slice(&count);
        zip.extend_from_slice(&count);
        zip.extend_from_slice(&directory_size.to_le_bytes());
        zip.extend_from_slice(&(directory as u32).to_le_bytes());
        zip.extend_from_slice(&[0, 0]); // comment length
        let path = crate::testutil::scratch_dir(tag).join(format!("doc.{ext}"));
        std::fs::write(&path, zip).unwrap();
        path
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
            .extract(&path, DOCX_MIME, &mut out, &mut scratch)
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
            .extract(&path, DOCX_MIME, &mut out, &mut scratch)
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

    /// A slide of one paragraph reading `text`.
    fn pptx_slide(text: &str) -> String {
        format!(
            "<p:sld><p:cSld><p:spTree><p:sp><p:txBody>\
             <a:p><a:r><a:t>{text}</a:t></a:r></a:p>\
             </p:txBody></p:sp></p:spTree></p:cSld></p:sld>"
        )
    }

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
    fn pptx_marks_each_slide() {
        let p = container(
            "pptx",
            "pptx",
            &[
                ("ppt/slides/slide1.xml", PPTX_SLIDE),
                ("ppt/slides/slide2.xml", PPTX_SLIDE),
            ],
        );
        assert_eq!(
            extract_with(&p, Parser::Pptx).unwrap(),
            "Title\nBody\n\n--- New Slide ---\nTitle\nBody\n\n--- New Slide ---\n"
        );
    }

    /// Slides come out in number order whatever order the archive stores
    /// them in, and `slide10` is not read between `slide1` and `slide2`.
    #[test]
    fn pptx_slides_come_out_in_number_order() {
        let (s10, s2, s1) = (pptx_slide("ten"), pptx_slide("two"), pptx_slide("one"));
        let p = container(
            "pptx-order",
            "pptx",
            &[
                ("ppt/slides/slide10.xml", s10.as_str()),
                ("ppt/slides/slide2.xml", s2.as_str()),
                ("ppt/slides/slide1.xml", s1.as_str()),
            ],
        );
        let text = extract_with(&p, Parser::Pptx).unwrap();
        let slides: Vec<&str> = text
            .split("\n--- New Slide ---\n")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(slides, ["one", "two", "ten"]);
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
            extract_with(&p, Parser::Pptx).unwrap(),
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
        let out = extract_with(&p, Parser::Pptx).unwrap();
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

    const DOCX_MIME: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

    /// Every MIME a format is claimed under reaches its parser, whatever the
    /// file is called. This is also the golden set pinning each format's
    /// exact extraction shape.
    #[test]
    fn template_and_macro_enabled_mimes_reach_the_same_parser() {
        /// One parser's MIMEs: the MIMEs, the members of a container to try
        /// them against, and the text every one of them must produce.
        type AliasCase = (
            &'static [&'static str],
            &'static [(&'static str, &'static str)],
            &'static str,
        );
        let cases: &[AliasCase] = &[
            (
                &[
                    DOCX_MIME,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
                    "application/vnd.ms-word.document.macroenabled.12",
                    "application/vnd.ms-word.template.macroenabled.12",
                ],
                &[("word/document.xml", DOCX_BODY)],
                "Helloworld\nSecond\n",
            ),
            // No notes part, so no notes marker and no trailing blank run.
            (
                &[
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
                    "application/vnd.openxmlformats-officedocument.presentationml.template",
                    "application/vnd.ms-powerpoint.presentation.macroenabled.12",
                    "application/vnd.ms-powerpoint.slideshow.macroenabled.12",
                    "application/vnd.ms-powerpoint.template.macroenabled.12",
                ],
                &[("ppt/slides/slide1.xml", PPTX_SLIDE)],
                "Title\nBody\n\n--- New Slide ---\n",
            ),
            (
                &[
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
                    "application/vnd.ms-excel.sheet.macroenabled.12",
                    "application/vnd.ms-excel.template.macroenabled.12",
                ],
                &[
                    ("xl/sharedStrings.xml", XLSX_SHARED),
                    ("xl/worksheets/sheet1.xml", XLSX_SHEET),
                ],
                "Shared 42 \nSecond \n",
            ),
            (
                &[
                    "application/vnd.oasis.opendocument.text",
                    "application/vnd.oasis.opendocument.text-template",
                    "application/vnd.oasis.opendocument.presentation",
                    "application/vnd.oasis.opendocument.presentation-template",
                ],
                &[("content.xml", ODT_BODY)],
                "Heading\nParaspan\n",
            ),
            (
                &[
                    "application/vnd.oasis.opendocument.spreadsheet",
                    "application/vnd.oasis.opendocument.spreadsheet-template",
                ],
                &[("content.xml", ODS_BODY)],
                "A1 \nB1 \n",
            ),
        ];
        for (c, (mimes, members, expected)) in cases.iter().enumerate() {
            for (m, mime) in mimes.iter().enumerate() {
                let p = container(&format!("alias-{c}-{m}"), "bin", members);
                let mut out = String::new();
                OfficeExtractor
                    .extract(&p, mime, &mut out, &mut scratch())
                    .unwrap();
                assert_eq!(&out, expected, "{mime} did not reach its format's parser");
            }
        }
    }

    /// A MIME the extractor does not claim fails rather than coming back
    /// empty, so a file is never stored as done and textless for want of a
    /// parser; the registry never offers one.
    #[test]
    fn an_unclaimed_mime_is_an_error() {
        let p = container("unclaimed", "docx", &[("word/document.xml", DOCX_BODY)]);
        let err = OfficeExtractor
            .extract(&p, "application/zip", &mut String::new(), &mut scratch())
            .unwrap_err();
        assert!(err.contains("no office parser"), "{err}");
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
        assert_eq!(extract_with(&p, Parser::Xlsx).unwrap(), "7 \n");
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
        assert_eq!(extract_with(&p, Parser::Xlsx).unwrap(), "only \n");
    }

    /// An undefined entity is the cheapest way to reach the error arm, and a
    /// real shape: tools do emit HTML entities into OOXML.
    #[test]
    fn malformed_xml_returns_an_error_rather_than_looping() {
        for (parser, member, body) in [
            (
                Parser::Docx,
                "word/document.xml",
                "<w:t>bad &nonsuch; entity</w:t>",
            ),
            (
                Parser::OdfText,
                "content.xml",
                "<text:p>bad &nonsuch; entity</text:p>",
            ),
            (
                Parser::OdfSheet,
                "content.xml",
                "<text:p>bad &nonsuch; entity</text:p>",
            ),
            (
                Parser::Pptx,
                "ppt/slides/slide1.xml",
                "<a:t>bad &nonsuch; entity</a:t>",
            ),
        ] {
            let p = container(&format!("bad-{parser:?}"), "bin", &[(member, body)]);
            assert!(
                extract_with(&p, parser).is_err(),
                "{parser:?} should report malformed XML"
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
        assert!(extract_with(&p, Parser::Docx).is_err());
    }

    #[test]
    fn a_missing_member_is_an_error() {
        let p = container("empty", "docx", &[("unrelated.xml", "<x/>")]);
        assert!(extract_with(&p, Parser::Docx).is_err());
    }

    #[test]
    fn a_non_container_is_an_error() {
        let dir = crate::testutil::scratch_dir("notzip");
        let p = dir.join("doc.docx");
        crate::testutil::touch(&p, b"this is not a zip archive");
        assert!(extract_with(&p, Parser::Docx).is_err());
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
            extract_with(&p, Parser::Xlsx).unwrap(),
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
        assert_eq!(extract_with(&p, Parser::Xlsx).unwrap(), "Second \n");
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
        assert_eq!(extract_with(&p, Parser::OdfSheet).unwrap(), "A&B \n");
    }

    /// A span closing inside a paragraph ends the span, not the paragraph.
    #[test]
    fn text_after_a_nested_span_is_not_dropped() {
        let body = "<office:document-content><office:body><office:text>\
             <text:p>before<text:span>inside</text:span>after</text:p>\
             </office:text></office:body></office:document-content>";
        let p = container("odt-span-tail", "odt", &[("content.xml", body)]);
        assert_eq!(
            extract_with(&p, Parser::OdfText).unwrap(),
            "beforeinsideafter\n"
        );
    }

    /// Every name an office suite writes, sniffed as the walk sniffs it, names
    /// a MIME whose parser is that format's. A name sniffed to a MIME this
    /// extractor does not claim is never offered to it, and a wrong parser
    /// fails the file; either way, at either end, nothing says so.
    #[test]
    fn every_office_name_reaches_its_parser() {
        let names: &[(Parser, &[&str])] = &[
            (Parser::Docx, &["docx", "docm", "dotx", "dotm"]),
            (Parser::Xlsx, &["xlsx", "xlsm", "xltx", "xltm"]),
            (
                Parser::Pptx,
                &["pptx", "pptm", "ppsx", "ppsm", "potx", "potm"],
            ),
            (Parser::OdfText, &["odt", "ott", "odp", "otp"]),
            (Parser::OdfSheet, &["ods", "ots"]),
            (Parser::Doc, &["doc", "dot", "wbk", "wiz"]),
            (Parser::Xls, &["xls", "xlt", "xla", "xlk"]),
            (Parser::Ppt, &["ppt", "pps", "pot", "ppa", "pwz"]),
        ];
        for (expected, extensions) in names {
            for ext in *extensions {
                let named = std::path::PathBuf::from(format!("probe.{ext}"));
                let mime = crate::mime::guess_mime_from_head(&named, b"")
                    .unwrap_or_else(|| panic!(".{ext} is sniffed as nothing at all"));
                // Lowercased the way the registry lowercases before dispatch:
                // the registered spelling is `macroEnabled`.
                assert_eq!(
                    parser_for(&mime.to_ascii_lowercase()),
                    Some(*expected),
                    ".{ext} is sniffed as {mime}"
                );
            }
        }
    }

    /// The sniff types a misnamed document from its bytes, and its MIME
    /// picks the parser: the name never decides.
    #[test]
    fn a_docx_named_bak_is_parsed() {
        let p = container("docx-bak", "bak", &[("word/document.xml", DOCX_BODY)]);
        let head = std::fs::read(&p).unwrap();
        let mime = crate::mime::guess_mime_from_head(&p, &head).unwrap();
        let text = crate::extract::Registry::default_set()
            .extract_to_string(&p, mime, &crate::config::Config::default())
            .unwrap()
            .unwrap_or_else(|| panic!("sniffed as {mime}, which nothing claims"));
        assert_eq!(text, "Helloworld\nSecond\n");
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
            extract_with(&p, Parser::OdfText).unwrap(),
            "first\n\nthird\n"
        );
    }

    // Shared strings

    /// The text of a workbook holding `shared` as its shared-string table and
    /// `sheet` as its one worksheet.
    fn workbook_text(tag: &str, shared: &str, sheet: &str) -> String {
        let p = container(
            tag,
            "xlsx",
            &[
                ("xl/sharedStrings.xml", shared),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        extract_with(&p, Parser::Xlsx).unwrap()
    }

    /// A worksheet of one row holding `cells`.
    fn one_row(cells: &str) -> String {
        format!("<worksheet><sheetData><row>{cells}</row></sheetData></worksheet>")
    }

    /// Two runs are one string, so the entry after it keeps index 1.
    #[test]
    fn a_rich_text_shared_string_is_one_entry() {
        let shared = "<sst><si><r><rPr><b/></rPr><t>Bold</t></r>\
             <r><t xml:space=\"preserve\"> plain</t></r></si>\
             <si><t>Second</t></si></sst>";
        let sheet = one_row("<c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c>");
        assert_eq!(
            workbook_text("xlsx-rich-si", shared, &sheet),
            "Bold plain Second \n"
        );
    }

    /// A reading's `<t>` is neither part of the string nor an entry.
    #[test]
    fn a_phonetic_reading_is_not_a_shared_string() {
        let shared = "<sst><si><t>東京</t><rPh sb=\"0\" eb=\"2\"><t>トウキョウ</t></rPh>\
             <phoneticPr fontId=\"1\"/></si><si><t>Second</t></si></sst>";
        let sheet = one_row("<c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c>");
        assert_eq!(workbook_text("xlsx-rph", shared, &sheet), "東京 Second \n");
    }

    #[test]
    fn a_self_closed_si_still_occupies_its_index() {
        assert_eq!(
            workbook_text(
                "xlsx-self-closed-si",
                "<sst><si/><si><t>Second</t></si></sst>",
                XLSX_SHEET_INDEX_1
            ),
            "Second \n"
        );
    }

    #[test]
    fn an_entity_split_across_runs_stays_one_entry() {
        let shared = "<sst><si><r><t>Jack &amp;</t></r>\
             <r><t xml:space=\"preserve\"> Jill</t></r></si>\
             <si><t>Second</t></si></sst>";
        assert_eq!(
            workbook_text("xlsx-entity-runs", shared, XLSX_SHEET),
            "Jack & Jill 42 \nSecond \n"
        );
    }

    /// [`XLSX_SHARED`] and [`XLSX_SHEET`] as a producer that binds the
    /// SpreadsheetML namespace to `x:` writes them.
    const XLSX_SHARED_X: &str =
        "<x:sst xmlns:x=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <x:si><x:t>Shared</x:t></x:si><x:si><x:t>Second</x:t></x:si></x:sst>";

    const XLSX_SHEET_X: &str =
        "<x:worksheet xmlns:x=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <x:sheetData>\
         <x:row><x:c t=\"s\"><x:v>0</x:v></x:c><x:c t=\"n\"><x:v>42</x:v></x:c></x:row>\
         <x:row><x:c t=\"s\"><x:v>1</x:v></x:c></x:row>\
         </x:sheetData></x:worksheet>";

    #[test]
    fn an_x_prefixed_shared_string_table_is_read() {
        assert_eq!(
            workbook_text("xlsx-x-sst", XLSX_SHARED_X, XLSX_SHEET),
            "Shared 42 \nSecond \n"
        );
    }

    #[test]
    fn an_x_prefixed_workbook_is_read() {
        assert_eq!(
            workbook_text("xlsx-x-all", XLSX_SHARED_X, XLSX_SHEET_X),
            "Shared 42 \nSecond \n"
        );
    }

    /// [`DOCX_BODY`] with WordprocessingML bound to `ns0:`, as some
    /// generators write it.
    #[test]
    fn a_renamed_prefix_docx_is_read() {
        let body = "<ns0:document \
             xmlns:ns0=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
             <ns0:body>\
             <ns0:p><ns0:r><ns0:t>Hello</ns0:t></ns0:r><ns0:r><ns0:t>world</ns0:t></ns0:r></ns0:p>\
             <ns0:p><ns0:r><ns0:t>Second</ns0:t></ns0:r></ns0:p>\
             </ns0:body></ns0:document>";
        let p = container("docx-ns0", "docx", &[("word/document.xml", body)]);
        assert_eq!(
            extract_with(&p, Parser::Docx).unwrap(),
            "Helloworld\nSecond\n"
        );
    }

    // Separators

    #[test]
    fn breaks_and_tabs_separate_words() {
        let cases = [
            (
                Parser::Docx,
                "word/document.xml",
                "<w:document><w:body><w:p><w:r>\
                 <w:t>one</w:t><w:br/><w:t>two</w:t><w:tab/><w:t>three</w:t>\
                 <w:cr/><w:t>four</w:t>\
                 </w:r></w:p></w:body></w:document>",
                "one\ntwo three\nfour\n",
            ),
            // A break carrying run properties has an `End` of its own.
            (
                Parser::Pptx,
                "ppt/slides/slide1.xml",
                "<p:sld><p:cSld><p:spTree><p:sp><p:txBody><a:p>\
                 <a:r><a:t>one</a:t></a:r><a:br><a:rPr lang=\"en-US\"/></a:br>\
                 <a:r><a:t>two</a:t></a:r>\
                 </a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>",
                "one\ntwo\n\n--- New Slide ---\n",
            ),
            (
                Parser::OdfText,
                "content.xml",
                "<office:document-content><office:body><office:text>\
                 <text:p>one<text:line-break/>two<text:tab/>three<text:s/>four\
                 <text:s text:c=\"3\"/>five</text:p>\
                 </office:text></office:body></office:document-content>",
                "one\ntwo three four five\n",
            ),
            (
                Parser::OdfSheet,
                "content.xml",
                "<office:document-content><office:body><office:spreadsheet>\
                 <table:table><table:table-row><table:table-cell>\
                 <text:p>one<text:s/>two</text:p>\
                 </table:table-cell></table:table-row></table:table>\
                 </office:spreadsheet></office:body></office:document-content>",
                "one two \n",
            ),
        ];
        for (parser, member, body, expected) in cases {
            let p = container(&format!("separators-{parser:?}"), "bin", &[(member, body)]);
            assert_eq!(extract_with(&p, parser).unwrap(), expected, "{parser:?}");
        }
    }

    /// `w:tab` is also how a paragraph defines its tab stops, before any of
    /// its text: as the first paragraph and after a break, it adds nothing.
    #[test]
    fn tab_stop_definitions_add_no_text() {
        let tabs = "<w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/>\
             <w:tab w:val=\"right\" w:pos=\"9360\"/></w:tabs></w:pPr>";
        let body = format!(
            "<w:document><w:body>\
             <w:p>{tabs}<w:r><w:t>First</w:t></w:r></w:p>\
             <w:p>{tabs}<w:r><w:t>Second</w:t></w:r></w:p>\
             </w:body></w:document>"
        );
        let p = container("docx-tab-stops", "docx", &[("word/document.xml", &body)]);
        assert_eq!(extract_with(&p, Parser::Docx).unwrap(), "First\nSecond\n");
    }

    // Cells

    #[test]
    fn formula_text_is_not_indexed() {
        let sheet = one_row(
            "<c r=\"A4\"><f>SUM(A1:A3)</f><v>6</v></c>\
             <c r=\"B4\" t=\"str\"><f>CONCAT(\"a\",\"b\")</f><v>ab</v></c>",
        );
        let p = container(
            "xlsx-formula",
            "xlsx",
            &[("xl/worksheets/sheet1.xml", &sheet)],
        );
        assert_eq!(extract_with(&p, Parser::Xlsx).unwrap(), "6 ab \n");
    }

    /// Rich runs make one cell's text, and a phonetic reading none of it.
    #[test]
    fn a_rich_inline_string_is_one_cell() {
        let sheet = one_row(
            "<c t=\"inlineStr\"><is><r><rPr><b/></rPr><t>Bold</t></r>\
             <r><t xml:space=\"preserve\"> plain</t></r></is></c>\
             <c t=\"inlineStr\"><is><t>東京</t>\
             <rPh sb=\"0\" eb=\"2\"><t>トウキョウ</t></rPh></is></c>",
        );
        let p = container(
            "xlsx-rich-inline",
            "xlsx",
            &[("xl/worksheets/sheet1.xml", &sheet)],
        );
        assert_eq!(
            extract_with(&p, Parser::Xlsx).unwrap(),
            "Bold plain 東京 \n"
        );
    }

    /// Self-closed, valueless, and a reference to a blank string.
    #[test]
    fn an_empty_cell_adds_no_separator() {
        let sheet = one_row(
            "<c r=\"A1\" s=\"1\"/><c r=\"B1\"><v></v></c>\
             <c r=\"C1\" t=\"s\"><v>0</v></c><c r=\"D1\"><v>5</v></c>",
        );
        assert_eq!(
            workbook_text("xlsx-empty-cells", "<sst><si><t/></si></sst>", &sheet),
            "5 \n"
        );
    }

    // Word notes and comments

    /// A footnotes part as Word writes one: the separator pseudo-note, an
    /// empty paragraph, ahead of the real one.
    const DOCX_FOOTNOTES: &str = "<w:footnotes>\
         <w:footnote w:type=\"separator\" w:id=\"-1\">\
         <w:p><w:r><w:separator/></w:r></w:p></w:footnote>\
         <w:footnote w:id=\"1\"><w:p><w:r><w:t>Footnote marmalade</w:t></w:r></w:p></w:footnote>\
         </w:footnotes>";

    const DOCX_ENDNOTES: &str = "<w:endnotes>\
         <w:endnote w:id=\"1\"><w:p><w:r><w:t>Endnote quince</w:t></w:r></w:p></w:endnote>\
         </w:endnotes>";

    const DOCX_COMMENTS: &str = "<w:comments>\
         <w:comment w:id=\"0\" w:author=\"Tester\"><w:p>\
         <w:r><w:annotationRef/></w:r><w:r><w:t>Comment persimmon</w:t></w:r>\
         </w:p></w:comment></w:comments>";

    /// In a fixed order whatever the archive's, and each only if present.
    #[test]
    fn docx_notes_and_comments_follow_the_body() {
        let p = container(
            "docx-notes",
            "docx",
            &[
                ("word/comments.xml", DOCX_COMMENTS),
                ("word/endnotes.xml", DOCX_ENDNOTES),
                ("word/footnotes.xml", DOCX_FOOTNOTES),
                ("word/document.xml", DOCX_BODY),
            ],
        );
        assert_eq!(
            extract_with(&p, Parser::Docx).unwrap(),
            "Helloworld\nSecond\n\nFootnote marmalade\nEndnote quince\nComment persimmon\n"
        );

        let p = container(
            "docx-comments-only",
            "docx",
            &[
                ("word/document.xml", DOCX_BODY),
                ("word/comments.xml", DOCX_COMMENTS),
            ],
        );
        assert_eq!(
            extract_with(&p, Parser::Docx).unwrap(),
            "Helloworld\nSecond\nComment persimmon\n"
        );
    }

    #[test]
    fn docx_headers_and_footers_are_left_out() {
        let p = container(
            "docx-headers",
            "docx",
            &[
                ("word/document.xml", DOCX_BODY),
                (
                    "word/header1.xml",
                    "<w:hdr><w:p><w:r><w:t>HEADERTEXT</w:t></w:r></w:p></w:hdr>",
                ),
                (
                    "word/footer1.xml",
                    "<w:ftr><w:p><w:r><w:t>FOOTERTEXT</w:t></w:r></w:p></w:ftr>",
                ),
            ],
        );
        assert_eq!(
            extract_with(&p, Parser::Docx).unwrap(),
            "Helloworld\nSecond\n"
        );
    }

    /// A note part that opens but does not parse fails the document, as a
    /// malformed body does.
    #[test]
    fn a_malformed_note_part_is_an_error() {
        let p = container(
            "docx-bad-notes",
            "docx",
            &[
                ("word/document.xml", DOCX_BODY),
                (
                    "word/footnotes.xml",
                    "<w:footnotes><w:t>bad &nonsuch; entity</w:t></w:footnotes>",
                ),
            ],
        );
        assert!(extract_with(&p, Parser::Docx).is_err());
    }

    #[test]
    fn docx_notes_stop_at_the_text_limit() {
        let mut body = String::from("<w:document><w:body>");
        for i in 0..1000 {
            body.push_str(&format!("<w:p><w:r><w:t>paragraph{i:08}</w:t></w:r></w:p>"));
        }
        body.push_str("</w:body></w:document>");
        let p = container(
            "docx-notes-text-limit",
            "docx",
            &[
                ("word/document.xml", &body),
                ("word/footnotes.xml", DOCX_FOOTNOTES),
            ],
        );
        let mut out = String::new();
        let file = File::open(&p).unwrap();
        extract_document(file, Parser::Docx, &mut out, &mut limited(4096)).unwrap();
        assert!(!out.contains("Footnote"), "read past the text limit");
    }

    #[test]
    fn docx_notes_share_the_documents_budget() {
        let body = format!(
            "<w:document><w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p>{}</w:body></w:document>",
            " ".repeat(64 * 1024)
        );
        let p = container(
            "docx-notes-budget",
            "docx",
            &[
                ("word/document.xml", &body),
                ("word/footnotes.xml", DOCX_FOOTNOTES),
            ],
        );
        let roomy = extract_on_worker(&p, Parser::Docx, scratch()).expect("extract");
        assert!(roomy.contains("Footnote marmalade"), "{roomy:?}");
        let out = extract_on_worker(&p, Parser::Docx, budgeted(32 * 1024))
            .expect("a spent budget is not a failure");
        assert_eq!(out, "Body\n", "the notes had a budget of their own");
    }

    // The inflate cap

    /// The cap falls inside a tag. A cut is where the member ends, not
    /// malformed XML: the text before it stands.
    #[test]
    fn a_member_cut_mid_tag_by_the_inflate_cap_keeps_its_text() {
        let scratch = limited(4096);
        let cap = scratch.limits().inflate;
        let mut body = String::from("<w:document><w:body><w:p><w:r><w:t>kept</w:t></w:r></w:p>");
        while body.len() < cap - 16 {
            body.push_str("<w:x/>");
        }
        body.push_str(&format!("<w:sectPr w:rsidR=\"{}\"/>", "0".repeat(64)));
        body.push_str("<w:p><w:r><w:t>lost</w:t></w:r></w:p></w:body></w:document>");
        let p = container("docx-cut-tag", "docx", &[("word/document.xml", &body)]);
        let out = extract_on_worker(&p, Parser::Docx, scratch).expect("a cut is not malformed XML");
        assert_eq!(out, "kept\n");
    }

    /// The cap falls one byte into a three-byte character, which must not
    /// surface as invalid UTF-8.
    #[test]
    fn a_member_cut_mid_character_keeps_its_text() {
        let scratch = limited(4096);
        let cap = scratch.limits().inflate;
        let mut body = String::from(
            "<w:document><w:body><w:p><w:r><w:t>kept</w:t></w:r></w:p><w:p><w:r><w:t>",
        );
        while (cap - body.len()) % 3 != 1 {
            body.push('x');
        }
        while body.len() <= cap {
            body.push('漢');
        }
        body.push_str("</w:t></w:r></w:p></w:body></w:document>");
        let p = container("docx-cut-char", "docx", &[("word/document.xml", &body)]);
        let out = extract_on_worker(&p, Parser::Docx, scratch).expect("a cut is not invalid UTF-8");
        assert_eq!(out, "kept\n");
    }

    /// Numbers carry tens of bytes of markup per byte of text, so an honest
    /// sheet reaches the cap well inside the text limit.
    #[test]
    fn an_ods_of_numbers_past_the_cap_keeps_its_text() {
        let scratch = limited(8192);
        let cap = scratch.limits().inflate;
        let mut body = String::from(
            "<office:document-content><office:body><office:spreadsheet>\
             <table:table><table:table-row>",
        );
        let mut cells = 0;
        while body.len() < 2 * cap {
            body.push_str(&format!(
                "<table:table-cell table:style-name=\"ce1\" office:value-type=\"float\" \
                 office:value=\"{cells}\" calcext:value-type=\"float\">\
                 <text:p>{cells}</text:p></table:table-cell>"
            ));
            cells += 1;
        }
        body.push_str(
            "</table:table-row></table:table>\
             </office:spreadsheet></office:body></office:document-content>",
        );
        let p = container("ods-numbers", "ods", &[("content.xml", &body)]);
        let out = extract_on_worker(&p, Parser::OdfSheet, scratch)
            .expect("the cap truncates, it does not fail");
        assert!(
            out.starts_with("0 \n1 \n2 \n"),
            "{:?}",
            &out[..out.len().min(40)]
        );
        assert!(out.len() < 8192, "the text limit stopped it, not the cap");
        assert!(
            out.contains(&format!("\n{} \n", cells / 4)),
            "cells before the cut were lost"
        );
        assert!(
            !out.contains(&format!("\n{} \n", cells - 1)),
            "read past the cap"
        );
    }

    /// One sheet cut short by the cap, mid-tag, between two that are not.
    #[test]
    fn a_sheet_cut_by_the_cap_keeps_earlier_sheets() {
        let scratch = limited(4096);
        let cap = scratch.limits().inflate;
        let inline =
            |text: &str| one_row(&format!("<c t=\"inlineStr\"><is><t>{text}</t></is></c>"));
        let mut long = String::from("<worksheet><sheetData>");
        let mut rows = 0;
        // Mostly markup, so the cap comes long before the text limit.
        while long.len() < cap - 600 {
            long.push_str(&format!(
                "<row ht=\"{}\"><c><v>{rows}</v></c></row>",
                "0".repeat(200)
            ));
            rows += 1;
        }
        long.push_str(&format!("<row ht=\"{}\">", "0".repeat(1000)));
        long.push_str("<c><v>lost</v></c></row></sheetData></worksheet>");
        let (first, third) = (inline("first"), inline("third"));
        let p = container(
            "xlsx-cut-sheet",
            "xlsx",
            &[
                ("xl/worksheets/sheet1.xml", &first),
                ("xl/worksheets/sheet2.xml", &long),
                ("xl/worksheets/sheet3.xml", &third),
            ],
        );
        let out = extract_on_worker(&p, Parser::Xlsx, scratch)
            .expect("the cap truncates, it does not fail");
        assert!(out.starts_with("first \n0 \n1 \n"), "{out:?}");
        assert!(out.contains(&format!("\n{} \n", rows - 1)), "{out:?}");
        assert!(out.ends_with("\nthird \n"), "{out:?}");
        assert!(!out.contains("lost"), "{out:?}");
    }

    // The container budget

    /// Slides without text never bring the text limit closer, so only the
    /// budget stops a deck of them.
    #[test]
    fn textless_slides_stop_at_the_container_budget() {
        const BUDGET: usize = 256 * 1024;
        const BLANK: usize = 16 * 1024;
        let names: Vec<String> = (1..=42)
            .map(|i| format!("ppt/slides/slide{i}.xml"))
            .collect();
        let (first, blank, last) = (
            pptx_slide("first"),
            textless("p:sld", BLANK),
            pptx_slide("last"),
        );
        let mut bodies = vec![first.as_str()];
        bodies.extend(std::iter::repeat(blank.as_str()).take(40));
        bodies.push(&last);
        let p = container("pptx-blank-budget", "pptx", &pairs(&names, &bodies));
        let out = extract_on_worker(&p, Parser::Pptx, budgeted(BUDGET))
            .expect("a spent budget is not a failure");
        assert!(out.starts_with("first\n"), "{out:?}");
        assert!(!out.contains("last"), "read past the budget");
        let read = out.matches("--- New Slide ---").count();
        assert!(
            (2..=BUDGET / BLANK + 1).contains(&read),
            "{read} of 42 slides opened on a {BUDGET}-byte budget"
        );
    }

    #[test]
    fn notes_share_the_slides_budget() {
        let names: Vec<String> = (1..=20)
            .map(|i| format!("ppt/slides/slide{i}.xml"))
            .collect();
        let blank = textless("p:sld", 16 * 1024);
        let mut members = pairs(&names, &[blank.as_str(); 20]);
        members.push(("ppt/notesSlides/notesSlide1.xml", PPTX_NOTES));
        let p = container("pptx-notes-budget", "pptx", &members);
        let roomy = extract_on_worker(&p, Parser::Pptx, scratch()).expect("extract");
        assert!(roomy.contains("Mention the budget"), "{roomy:?}");
        // Twenty 16 KiB slides spend 128 KiB before the notes.
        let out = extract_on_worker(&p, Parser::Pptx, budgeted(128 * 1024))
            .expect("a spent budget is not a failure");
        assert!(
            !out.contains("Speaker Notes"),
            "the notes had a budget of their own: {out:?}"
        );
    }

    #[test]
    fn empty_sheets_stop_at_the_container_budget() {
        let names: Vec<String> = (1..=42)
            .map(|i| format!("xl/worksheets/sheet{i}.xml"))
            .collect();
        let inline =
            |text: &str| one_row(&format!("<c t=\"inlineStr\"><is><t>{text}</t></is></c>"));
        let (first, blank, last) = (
            inline("first"),
            textless("worksheet", 16 * 1024),
            inline("last"),
        );
        let mut bodies = vec![first.as_str()];
        bodies.extend(std::iter::repeat(blank.as_str()).take(40));
        bodies.push(&last);
        let p = container("xlsx-blank-budget", "xlsx", &pairs(&names, &bodies));
        let out = extract_on_worker(&p, Parser::Xlsx, budgeted(256 * 1024))
            .expect("a spent budget is not a failure");
        assert_eq!(out, "first \n", "read past the budget");
    }

    /// The table is read first, so a table that spends the budget leaves
    /// none for the sheets.
    #[test]
    fn the_shared_string_table_draws_on_the_container_budget() {
        let shared = format!("<sst><si><t>Shared</t></si>{}</sst>", " ".repeat(64 * 1024));
        let sheet = one_row("<c t=\"s\"><v>0</v></c><c><v>7</v></c>");
        let p = container(
            "xlsx-sst-budget",
            "xlsx",
            &[
                ("xl/sharedStrings.xml", &shared),
                ("xl/worksheets/sheet1.xml", &sheet),
            ],
        );
        let roomy = extract_on_worker(&p, Parser::Xlsx, scratch()).expect("extract");
        assert_eq!(roomy, "Shared 7 \n");
        let out = extract_on_worker(&p, Parser::Xlsx, budgeted(32 * 1024))
            .expect("a spent budget is not a failure");
        assert_eq!(out, "", "the table had a budget of its own");
    }

    /// Thousands of directory entries naming one small body: each read is
    /// charged again, and the fee bounds them even though the body is tiny.
    #[test]
    fn overlapping_central_directory_entries_are_charged_each_time() {
        const BUDGET: usize = 1024 * 1024;
        const ENTRIES: usize = 2000;
        let names: Vec<String> = (1..=ENTRIES)
            .map(|i| format!("ppt/slides/slide{i}.xml"))
            .collect();
        let body = pptx_slide("overlap");
        let p = shared_body_zip("pptx-overlap", "pptx", &names, body.as_bytes(), 0);
        let out = extract_on_worker(&p, Parser::Pptx, budgeted(BUDGET))
            .expect("a spent budget is not a failure");
        let read = out.matches("overlap").count();
        let most = BUDGET / (MEMBER_FEE + body.len()) + 1;
        assert!(
            (most / 2..=most).contains(&read),
            "{read} of {ENTRIES} entries read; {most} fit the budget"
        );
    }

    /// The same body behind 64 KiB of deflate input that inflates to
    /// nothing: each entry makes the inflater read all of it, so each is
    /// charged its compressed size, not only the fee.
    #[test]
    fn overlapping_entries_are_charged_their_compressed_input() {
        const BUDGET: usize = 1024 * 1024;
        const ENTRIES: usize = 200;
        const PADDING: usize = 13_000;
        let names: Vec<String> = (1..=ENTRIES)
            .map(|i| format!("ppt/slides/slide{i}.xml"))
            .collect();
        let body = pptx_slide("overlap");
        let p = shared_body_zip(
            "pptx-overlap-input",
            "pptx",
            &names,
            body.as_bytes(),
            PADDING,
        );
        let out = extract_on_worker(&p, Parser::Pptx, budgeted(BUDGET))
            .expect("a spent budget is not a failure");
        let read = out.matches("overlap").count();
        let most = BUDGET / (5 * PADDING) + 1;
        assert!(
            (most / 2..=most).contains(&read),
            "{read} of {ENTRIES} entries read; {most} fit the budget"
        );
    }

    /// The end of an archive as zip64 writes it: a zip64 record declaring
    /// `count` entries in a directory of `size` bytes at `offset`, a locator
    /// placing that record at `at`, and an end record deferring every field
    /// to it.
    fn zip64_end(count: u64, size: u64, offset: u64, at: u64) -> Vec<u8> {
        let mut end = Vec::new();
        end.extend_from_slice(b"PK\x06\x06");
        end.extend_from_slice(&44u64.to_le_bytes()); // the record's remaining length
        end.extend_from_slice(&[45, 0, 45, 0]); // versions made by and needed
        end.extend_from_slice(&[0; 8]); // disk numbers
        end.extend_from_slice(&count.to_le_bytes()); // entries on this disk
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&size.to_le_bytes());
        end.extend_from_slice(&offset.to_le_bytes());
        end.extend_from_slice(b"PK\x06\x07");
        end.extend_from_slice(&[0; 4]); // the record's disk
        end.extend_from_slice(&at.to_le_bytes());
        end.extend_from_slice(&1u32.to_le_bytes()); // disks
        end.extend_from_slice(b"PK\x05\x06");
        end.extend_from_slice(&[0; 4]); // disk numbers
        end.extend_from_slice(&[0xFF; 12]); // counts, size and offset
        end.extend_from_slice(&[0, 0]); // comment length
        end
    }

    /// A container ending in zip64's records, as one past 65,535 members or
    /// 4 GiB does, still opens: its directory is checked where the zip64
    /// record puts it.
    #[test]
    fn a_zip64_container_is_read() {
        let body = "<w:document><w:body><w:p><w:r><w:t>sixty-four</w:t>\
             </w:r></w:p></w:body></w:document>";
        let p = container("docx-zip64", "docx", &[("word/document.xml", body)]);
        let plain = std::fs::read(&p).unwrap();
        // With no comment, the plain end record is the last 22 bytes.
        let (archive, end) = plain.split_at(plain.len() - 22);
        let u32_at = |at: usize| u64::from(u32::from_le_bytes(end[at..at + 4].try_into().unwrap()));
        let entries = u64::from(u16::from_le_bytes([end[10], end[11]]));
        let (size, offset) = (u32_at(12), u32_at(16));
        let mut zip = archive.to_vec();
        zip.extend(zip64_end(entries, size, offset, archive.len() as u64));
        std::fs::write(&p, zip).unwrap();

        let out = extract_on_worker(&p, Parser::Docx, scratch()).expect("a zip64 container");
        assert!(out.contains("sixty-four"), "{out:?}");
    }

    /// zip 0.6 sizes its member tables from the count a zip64 record
    /// declares, before it reads a header: 98 bytes declaring 52M entries
    /// asked for gigabytes once the file was as long as the count, and the
    /// failed allocation aborted the process. Refused whether the directory
    /// declared is too small for the count or too large for the file, and
    /// when the locator points anywhere but at the record.
    #[test]
    fn a_zip64_record_declaring_more_entries_than_fit_is_refused() {
        const ENTRIES: u64 = 52_000_000;
        const ROOM: u64 = ENTRIES * CENTRAL_HEADER_LEN;
        for (size, at, why) in [
            (4096, 0, "declares 52000000 entries in 4096 bytes"),
            (ROOM, 0, "runs past the end"),
            (ROOM, 1, "not where its locator says"),
        ] {
            let p = crate::testutil::scratch_dir("xlsx-zip64-count").join("x.xlsx");
            std::fs::write(&p, zip64_end(ENTRIES, size, 0, at)).unwrap();
            let refused = extract_on_worker(&p, Parser::Xlsx, scratch());
            assert!(
                refused.as_ref().is_err_and(|e| e.contains(why)),
                "{why}: {refused:?}"
            );
        }
    }
}
