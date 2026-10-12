//! PDF text extraction via `pdf_extract`. `lopdf` comes from its
//! `pub use lopdf::*`; see the note in `Cargo.toml` for why it is never
//! declared directly.
//!
//! The registry parses PDFs in a sandbox child once a binary enables one
//! ([`super::sandbox`]): lopdf's loader still recurses without bound on
//! crafted input, so does the Type1 font parser, and loading inflates
//! without a cap. [`PdfExtractor`] itself always parses in the calling
//! process, the child's included.

use std::cell::Cell;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use pdf_extract::{Document, OutputError, PlainTextOutput};

use super::{ExtractError, Extractor, Scratch};

thread_local! {
    /// True while this thread is inside a contained `pdf_extract` call.
    static SUPPRESS_PANIC_PRINT: Cell<bool> = const { Cell::new(false) };
}

/// Install (once) a panic hook that stays quiet while this thread is inside a
/// contained PDF extraction; malformed-PDF panics are expected and recorded.
fn install_quiet_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !SUPPRESS_PANIC_PRINT.with(|flag| flag.get()) {
                previous(info);
            }
        }));
    });
}

pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    /// The one format with no streaming option: the file is read whole, at
    /// most `limits().read` bytes, and loading builds the whole object graph
    /// before a byte of text comes out; a 2 MiB file has been measured
    /// holding tens of megabytes. Only the sandbox bounds what the load
    /// builds or inflates (its child's memory limit, [`super::sandbox`]),
    /// and it is `pdf_extract`'s with no scratch to reuse, so peak for PDFs
    /// alone is `workers × amplification`, one pool per root. The rendering
    /// is bounded: pages stop once the text reaches `limits().text`, and
    /// interpretation stops once the document spends
    /// `pdf_extract::MAX_PDF_WORK`, keeping the text so far.
    ///
    /// `pdf_extract` panics on malformed input by design, so its panics, the
    /// load's included, are contained here under a hook that keeps them off
    /// stderr.
    fn extract(
        &self,
        path: &Path,
        _mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        let limits = scratch.limits();
        install_quiet_panic_hook();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result = crate::contained(|| extract_one_pass(path, limits.read, limits.text));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        let text = result.map_err(|m| format!("pdf_extract panicked: {}", m))??;
        super::append(out, text);
        Ok(())
    }
}

/// The text sink, full at `limit` bytes. Once the text reaches what the
/// caller keeps, the next write fails, which ends `output_doc` rather than
/// rendering pages whose text would only be truncated away. Measured
/// (`extractprobe`, text-only PDFs of 684 and ~340 pages at the default
/// 256 KiB limit): 1.7 s → 0.8 s of CPU.
struct Capped<'a> {
    text: &'a mut String,
    limit: usize,
}

impl std::fmt::Write for Capped<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.text.len() >= self.limit {
            return Err(std::fmt::Error);
        }
        self.text.push_str(s);
        Ok(())
    }
}

impl pdf_extract::ConvertToFmt for Capped<'_> {
    type Writer = Self;
    fn convert(self) -> Self {
        self
    }
}

/// `pdf_extract::extract_text`'s body (load, decrypt, `output_doc`) spelled
/// out, stopping once `limit` bytes of text are in or the document has spent
/// its work budget. The file is opened through
/// [`super::open_for_extract`] and read through `take(read)`.
fn extract_one_pass(path: &Path, read: usize, limit: usize) -> Result<String, ExtractError> {
    let mut doc = {
        let (file, meta) = super::open_for_extract(path, read)?;
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        file.take(read as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("pdf_extract: {}", e))?;
        // The bytes go once the object graph is built, as `Document::load`'s
        // own buffer does.
        Document::load_mem(&bytes).map_err(|e| format!("pdf_extract: {}", e))?
    };
    // Empty-password decrypt only; nothing on this path can prompt for a real one.
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|e| format!("pdf_extract: {}", e))?;
    }

    let mut text = String::new();
    let rendered = {
        let mut sink = PlainTextOutput::new(Capped {
            text: &mut text,
            limit,
        });
        pdf_extract::output_doc(&doc, &mut sink)
    };
    match rendered {
        Ok(()) => Ok(text),
        // The sink filled, or the work budget ran out: the stop is ours, not
        // a failure.
        Err(OutputError::Budget) => Ok(text),
        Err(_) if text.len() >= limit => Ok(text),
        Err(e) => Err(format!("pdf_extract: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use pdf_extract::{dictionary, Dictionary, Object, ObjectId, Stream, StringFormat};

    use crate::extract::on_worker;

    /// Extract `path` under the default limits, on a watched worker.
    fn extract(path: &Path) -> Result<String, ExtractError> {
        let path = path.to_path_buf();
        on_worker(move || crate::extract::extract_text(&PdfExtractor, &path))
    }

    #[test]
    fn contained_panics_are_caught_quietly() {
        install_quiet_panic_hook();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result: Result<(), _> = crate::contained(|| panic!("synthetic pdf failure"));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(!SUPPRESS_PANIC_PRINT.with(|flag| flag.get()));
    }

    #[test]
    fn supports_pdf_mime() {
        assert!(PdfExtractor.supports("application/pdf"));
        assert!(!PdfExtractor.supports("application/zip"));
    }

    // Fixtures are built with only `pdf_extract`'s `lopdf` re-export, so a
    // move in that surface fails to compile here alongside the extractor.

    /// A base-14 font: built-in metrics and encoding tables, so no font file.
    fn helvetica() -> Dictionary {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        }
    }

    /// Resources naming `font` `/F1`.
    fn with_f1(font: impl Into<Object>) -> Dictionary {
        dictionary! { "Font" => dictionary! { "F1" => font } }
    }

    /// `text` shown in `/F1` at the top of the page.
    fn show(text: &str) -> String {
        format!("BT /F1 24 Tf 72 720 Td ({}) Tj ET\n", text)
    }

    fn add_stream(doc: &mut Document, dict: Dictionary, content: impl Into<Vec<u8>>) -> ObjectId {
        doc.add_object(Stream::new(dict, content.into()))
    }

    /// A Form XObject drawing `content` with `resources`.
    fn form(resources: Dictionary, content: &str) -> Stream {
        Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => resources,
            },
            content.as_bytes().to_vec(),
        )
    }

    /// `stream` as FlateDecode behind `padding` empty stored blocks (see
    /// [`crate::extract::padded_deflate`]): 5 more bytes of input a block,
    /// and the same content decoded.
    fn padded(mut stream: Stream, padding: usize) -> Stream {
        let content = std::mem::take(&mut stream.content);
        // A zlib header (deflate, no dictionary), the deflate data, then the
        // Adler-32 of what it inflates to.
        let mut zlib = vec![0x78, 0x01];
        zlib.extend(crate::extract::padded_deflate(&content, padding));
        let (a, b) = content.iter().fold((1u32, 0u32), |(a, b), &byte| {
            let a = (a + u32::from(byte)) % 65521;
            (a, (b + a) % 65521)
        });
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
        stream.dict.set("Filter", "FlateDecode");
        stream.set_content(zlib);
        stream
    }

    /// `content` as a stream deflated the ordinary way, so a long repetitive
    /// one costs the fixture a few KB.
    fn compressed(content: impl Into<Vec<u8>>) -> Stream {
        let mut stream = Stream::new(dictionary! {}, content.into());
        stream.compress().expect("compress fixture stream");
        stream
    }

    /// A Form XObject that draws itself `draws` times, and resources naming
    /// it `/X0`.
    fn self_drawing_form(doc: &mut Document, draws: usize) -> Dictionary {
        let id = doc.new_object_id();
        let names = dictionary! { "XObject" => dictionary! { "X0" => id } };
        doc.objects
            .insert(id, form(names.clone(), &"/X0 Do ".repeat(draws)).into());
        names
    }

    /// A ToUnicode CMap with `body` between `begincmap` and `endcmap`, framed
    /// as producers write one.
    fn cmap(body: &str) -> String {
        format!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
             /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n{}\nendcmap\n\
             CMapName currentdict /CMap defineresource pop\nend\nend\n",
            body
        )
    }

    /// Helvetica with a ToUnicode CMap around `body`.
    fn mapped_helvetica(doc: &mut Document, body: &str) -> Dictionary {
        let mut font = helvetica();
        font.set("ToUnicode", add_stream(doc, dictionary! {}, cmap(body)));
        font
    }

    /// `text` as a hex string of two-byte codes, as an Identity-H font shows it.
    fn utf16_hex(text: &str) -> String {
        let hex: String = text.encode_utf16().map(|u| format!("{:04X}", u)).collect();
        format!("<{}>", hex)
    }

    /// A ToUnicode CMap mapping every two-byte code to itself.
    const IDENTITY_UCS: &str = "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
                                1 beginbfrange\n<0000> <FFFF> <0000>\nendbfrange";

    /// A Type0 font whose `/Encoding` is `encoding`, a CMap name or stream,
    /// and whose ToUnicode CMap is `to_unicode`.
    fn type0(encoding: impl Into<Object>, to_unicode: ObjectId) -> Dictionary {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "Fixture",
            "Encoding" => encoding,
            "ToUnicode" => to_unicode,
            "DescendantFonts" => vec![Object::Dictionary(dictionary! {
                "Type" => "Font",
                "Subtype" => "CIDFontType2",
                "BaseFont" => "Fixture",
                "CIDSystemInfo" => dictionary! {
                    "Registry" => text_string("Adobe"),
                    "Ordering" => text_string("Identity"),
                    "Supplement" => 0,
                },
                "FontDescriptor" => dictionary! {
                    "Type" => "FontDescriptor",
                    "FontName" => "Fixture",
                    "Flags" => 4,
                },
            })],
        }
    }

    /// Finish `doc` with one page per `(resources, contents)` and save it.
    fn save(tag: &str, mut doc: Document, pages: Vec<(Dictionary, Object)>) -> PathBuf {
        let pages_id = doc.new_object_id();
        let kids: Vec<Object> = pages
            .into_iter()
            .map(|(resources, contents)| {
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "Resources" => resources,
                    "Contents" => contents,
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                })
                .into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => kids.len() as i64,
                "Kids" => kids,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);

        let path = crate::testutil::scratch_dir(tag).join("fixture.pdf");
        doc.save(&path).expect("write fixture pdf");
        path
    }

    /// [`save`] for one page drawing a single content stream.
    fn save_page(
        tag: &str,
        mut doc: Document,
        resources: Dictionary,
        content: impl Into<Vec<u8>>,
    ) -> PathBuf {
        let contents = add_stream(&mut doc, dictionary! {}, content);
        save(tag, doc, vec![(resources, contents.into())])
    }

    /// A one-page document showing `body` in Helvetica, the font an indirect
    /// object as producers usually write it.
    fn write_pdf(tag: &str, mut doc: Document, body: &str) -> PathBuf {
        let font = doc.add_object(helvetica());
        save_page(tag, doc, with_f1(font), show(body))
    }

    fn text_string(s: &str) -> Object {
        Object::String(s.as_bytes().to_vec(), StringFormat::Literal)
    }

    #[test]
    fn extracts_text_from_a_document_with_an_info_dictionary() {
        let mut doc = Document::with_version("1.5");
        let info = doc.add_object(dictionary! {
            "Title" => text_string("The Title"),
            "Author" => text_string("An Author"),
            "Subject" => text_string("A Subject"),
            "Keywords" => text_string("alpha beta"),
            "Creator" => text_string("A Creator"),
            "Producer" => text_string("A Producer"),
        });
        doc.trailer.set("Info", info);
        let path = write_pdf("pdf-full", doc, "Hello QuickSearch");

        let out = extract(&path).expect("extract");
        assert!(
            out.contains("Hello QuickSearch"),
            "drawn text missing from {:?}",
            out
        );
    }

    #[test]
    fn missing_info_dictionary_still_yields_text() {
        let path = write_pdf("pdf-noinfo", Document::with_version("1.5"), "Body Only");
        let out = extract(&path).expect("extract");
        assert!(out.contains("Body Only"));
    }

    /// Rendering stops once the text limit is reached: a later page's text
    /// is never produced, not produced and cut.
    #[test]
    fn extraction_stops_at_the_text_limit() {
        let mut doc = Document::with_version("1.5");
        let pages = ["alphapage", "bravopage", "charliepage", "deltapage"]
            .iter()
            .map(|word| {
                let contents = add_stream(&mut doc, dictionary! {}, show(word));
                (with_f1(helvetica()), contents.into())
            })
            .collect();
        let path = save("pdf-limit", doc, pages);

        let whole = extract(&path).expect("extract");
        assert!(
            whole.contains("deltapage"),
            "every page renders: {:?}",
            whole
        );

        let out = on_worker(move || {
            let mut config = crate::config::Config::default();
            config.processing.maximum_text_size = 12;
            let mut out = String::new();
            PdfExtractor
                .extract(
                    &path,
                    "application/pdf",
                    &mut out,
                    &mut Scratch::new(&config),
                )
                .map(|()| out)
        })
        .expect("a stop at the limit is not a failure");
        assert!(out.contains("alphapage"), "got {:?}", out);
        assert!(
            !out.contains("deltapage"),
            "rendered past the limit: {:?}",
            out
        );
    }

    #[test]
    fn malformed_pdf_fails_without_panicking_the_process() {
        let path = crate::testutil::scratch_dir("pdf-malformed").join("broken.pdf");
        std::fs::write(&path, b"%PDF-1.4\n\x00\x01\x02 not a pdf at all \xff\xfe").unwrap();

        let err = extract(&path)
            .expect_err("malformed pdf must fail");
        assert!(
            err.starts_with("pdf_extract"),
            "unexpected failure reason: {}",
            err
        );
    }

    /// A page whose `/Parent` is itself must be skipped, not fatal. Upstream
    /// `get_inherited` walks `/Parent` with no depth bound, and a cycle
    /// overflows the stack, which aborts past `catch_unwind`, so
    /// `vendor/pdf-extract` bounds the walk.
    #[test]
    fn a_self_referential_page_parent_is_contained() {
        let mut doc = Document::with_version("1.5");
        let contents = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.new_object_id();
        // No `Resources` or `MediaBox`, so both lookups follow `/Parent`, into itself.
        doc.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => page_id,
                "Contents" => contents,
            }),
        );
        let pages_id = doc.add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);

        let path = crate::testutil::scratch_dir("pdf-parent-cycle").join("cycle.pdf");
        doc.save(&path).expect("write fixture pdf");

        // The verdict that matters is that we reach this line at all.
        let _ = extract(&path);
    }

    /// The two bounded recursions, at their deepest, on the stack the
    /// indexer and the sandbox child parse on
    /// ([`crate::platform::spawn_worker`]): `MAX_PDF_RECURSION_DEPTH` frames
    /// of `get_inherited`, of `process_stream`, and of `process_stream` with
    /// lopdf's content tokenizer at its own nesting limit inside each one.
    /// An overflow here aborts the process, so a build that fails this test
    /// wants a lower depth, not a bigger stack.
    ///
    /// Release only: a debug frame is several times larger (measured at
    /// 7,240 bytes for `process_stream`, which leaves little of 512 KiB at
    /// depth 64), and the shipped build is the one whose margin matters.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug frames are several times larger")]
    fn pdf_recursion_bounds_fit_the_worker_stack() {
        // `/Parent` pointing at itself: `get_inherited` to the depth bound.
        let mut doc = Document::with_version("1.5");
        let contents = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.new_object_id();
        doc.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => page_id,
                "Contents" => contents,
            }),
        );
        let pages_id = doc.add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        });
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);
        let path = crate::testutil::scratch_dir("pdf-depth-parent").join("parent.pdf");
        doc.save(&path).expect("write fixture pdf");
        let _ = extract(&path);

        // A form drawing itself: `process_stream` to the depth bound.
        let mut doc = Document::with_version("1.5");
        let resources = self_drawing_form(&mut doc, 1);
        let path = save_page("pdf-depth-form", doc, resources, "/X0 Do");
        assert_eq!(extract(&path), Ok(String::new()));

        // The same, with each level's content nesting operand arrays to
        // lopdf's limit (100) before the draw, so the tokenizer's recursion
        // sits inside every frame of ours.
        let mut doc = Document::with_version("1.5");
        // A dash pattern, so the nesting is an operand of something the
        // interpreter reads past: `Do` itself takes its name first.
        let nested = format!("{}{} 0 d /X0 Do", "[".repeat(99), "]".repeat(99));
        let id = doc.new_object_id();
        let names = dictionary! { "XObject" => dictionary! { "X0" => id } };
        doc.objects.insert(id, form(names.clone(), &nested).into());
        let path = save_page("pdf-depth-nested", doc, names, nested.as_str());
        assert_eq!(extract(&path), Ok(String::new()));
    }

    /// A Form XObject drawing itself must be skipped: the second unbounded
    /// recursion (`process_stream`'s `Do` arm). The depth bound caps the
    /// recursion; the work budget caps the 2^depth branching shape (see
    /// a_branching_form_xobject_is_bounded).
    #[test]
    fn a_self_drawing_form_xobject_is_contained() {
        let mut doc = Document::with_version("1.5");
        // Its own resources name it, so `/X0 Do` inside it re-enters itself.
        let resources = self_drawing_form(&mut doc, 1);
        let path = save_page("pdf-xobject-cycle", doc, resources, "/X0 Do");
        assert_eq!(extract(&path), Ok(String::new()));
    }

    /// A form drawing the next level twice costs 2^depth calls under the
    /// depth bound, and a text-free one never fills the text sink, so only
    /// the work budget stops it: a success, inside the watchdog.
    #[test]
    fn a_branching_form_xobject_is_bounded() {
        let mut doc = Document::with_version("1.5");
        let resources = self_drawing_form(&mut doc, 2);
        let path = save_page("pdf-branch-self", doc, resources, "/X0 Do");
        assert_eq!(extract(&path), Ok(String::new()));

        // A ring: X0 draws X1 twice and X1 draws X0 twice.
        let mut doc = Document::with_version("1.5");
        let (x0, x1) = (doc.new_object_id(), doc.new_object_id());
        let names = dictionary! { "XObject" => dictionary! { "X0" => x0, "X1" => x1 } };
        doc.objects
            .insert(x0, form(names.clone(), "/X1 Do /X1 Do").into());
        doc.objects
            .insert(x1, form(names.clone(), "/X0 Do /X0 Do").into());
        let path = save_page("pdf-branch-ring", doc, names, "/X0 Do");
        assert_eq!(extract(&path), Ok(String::new()));
    }

    /// An empty form behind 200 KB of deflate input, drawn 100,000 times.
    /// Charged only what it decodes to, every draw fit the budget, each one
    /// inflating it again: 20 GB. Not the branching shape: 64 nested draws,
    /// each inflating, overflow the worker's 512 KiB stack in a debug test
    /// build, whose flate2 is zlib-rs (a dev-dependency's feature); release
    /// builds inflate with miniz_oxide and need about 128 KiB.
    #[test]
    fn a_deflated_form_is_charged_its_encoded_size() {
        let mut doc = Document::with_version("1.5");
        let x0 = doc.add_object(padded(form(Dictionary::new(), ""), 40_000));
        let resources = dictionary! { "XObject" => dictionary! { "X0" => x0 } };
        let contents = doc.add_object(compressed("/X0 Do\n".repeat(100_000)));
        let path = save("pdf-form-deflated", doc, vec![(resources, contents.into())]);
        assert_eq!(extract(&path), Ok(String::new()));
    }

    /// Drawing one form 5,000 times is far inside the budget, and every draw
    /// keeps its text.
    #[test]
    fn a_form_drawn_thousands_of_times_keeps_its_text() {
        let mut doc = Document::with_version("1.5");
        let x0 = doc.add_object(form(with_f1(helvetica()), "BT /F1 12 Tf (formtext) Tj ET"));
        let mut resources = with_f1(helvetica());
        resources.set("XObject", dictionary! { "X0" => x0 });
        let path = save_page("pdf-form-5000", doc, resources, "/X0 Do\n".repeat(5_000));

        let out = extract(&path).expect("extract");
        assert_eq!(out.matches("formtext").count(), 5_000);
    }

    /// The budget stops the document, not the extraction: the text rendered
    /// before the stop is kept, and nothing after it renders.
    #[test]
    fn a_budget_stop_keeps_collected_text() {
        let mut doc = Document::with_version("1.5");
        let mut resources = self_drawing_form(&mut doc, 2);
        resources.set("Font", dictionary! { "F1" => helvetica() });
        let pages = [
            show("beforebudget"),
            "/X0 Do".to_string(),
            show("afterbudget"),
        ]
        .into_iter()
        .map(|content| {
            (
                resources.clone(),
                add_stream(&mut doc, dictionary! {}, content).into(),
            )
        })
        .collect();
        let path = save("pdf-budget-stop", doc, pages);

        let out = extract(&path).expect("a budget stop is not a failure");
        assert!(out.contains("beforebudget"), "got {:?}", out);
        assert!(
            !out.contains("afterbudget"),
            "rendered past the stop: {:?}",
            out
        );
    }

    /// `Do` on an image skips it: its samples are not content, even when
    /// they happen to read as operators.
    #[test]
    fn an_image_xobject_is_not_parsed_as_content() {
        let mut doc = Document::with_version("1.5");
        let image = add_stream(
            &mut doc,
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
            },
            "BT /F1 12 Tf (imagetext) Tj ET",
        );
        let mut resources = with_f1(helvetica());
        resources.set("XObject", dictionary! { "Im0" => image });
        let content = format!("/Im0 Do {}", show("pagetext"));
        let path = save_page("pdf-image-xobject", doc, resources, content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("pagetext"), "got {:?}", out);
        assert!(
            !out.contains("imagetext"),
            "image parsed as content: {:?}",
            out
        );
    }

    /// A million nested `q`s save at most `MAX_GS_STACK` states, and the
    /// text inside them still renders.
    #[test]
    fn a_deep_q_stack_is_bounded() {
        let content = format!(
            "{}{}{}",
            "q ".repeat(1_000_000),
            show("deepstate"),
            "Q ".repeat(1_000_000)
        );
        let doc = Document::with_version("1.5");
        let path = save_page("pdf-deep-q", doc, with_f1(helvetica()), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("deepstate"), "got {:?}", out);
    }

    /// The `Q` matching an unsaved `q` must not restore a saved state. Text
    /// moved 100pt down sits inside 301 nested `q`s, past the cap: balanced,
    /// it keeps its place, and the text after the outermost `Q` is back up,
    /// so the two land on separate lines.
    #[test]
    fn q_beyond_the_cap_stays_balanced() {
        let content = format!(
            "q 1 0 0 1 0 -100 cm {}BT /F1 12 Tf 72 700 Td (lowered) Tj ET {}Q \
             BT /F1 12 Tf 72 700 Td (raised) Tj ET",
            "q ".repeat(300),
            "Q ".repeat(300)
        );
        let doc = Document::with_version("1.5");
        let path = save_page("pdf-q-balance", doc, with_f1(helvetica()), content);

        let out = extract(&path).expect("extract");
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, ["lowered", "raised"], "got {:?}", out);
    }

    /// Colour is not interpreted, so a colour space it could not resolve no
    /// longer fails the document.
    #[test]
    fn an_indexed_fill_colour_space_no_longer_fails_the_document() {
        let palette = Object::String(vec![0, 0, 0, 255, 255, 255], StringFormat::Hexadecimal);
        let mut resources = with_f1(helvetica());
        resources.set(
            "ColorSpace",
            dictionary! {
                "CS0" => vec!["Indexed".into(), "DeviceRGB".into(), 1.into(), palette],
            },
        );
        let content = format!("/CS0 cs 1 sc {}", show("indexedcolour"));
        let path = save_page(
            "pdf-indexed-cs",
            Document::with_version("1.5"),
            resources,
            content,
        );

        let out = extract(&path).expect("an Indexed colour space is legitimate");
        assert!(out.contains("indexedcolour"), "got {:?}", out);
    }

    #[test]
    fn a_gs_naming_a_missing_extgstate_still_extracts() {
        let content = format!("/GS0 gs {}", show("nogstate"));
        let doc = Document::with_version("1.5");
        let path = save_page("pdf-missing-extgstate", doc, with_f1(helvetica()), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("nogstate"), "got {:?}", out);
    }

    /// `/Contents` naming one 40 KB stream 100,000 times concatenated every
    /// copy, 4 GB. Appending stops at the budget, which ends the document
    /// there: a success that keeps the page before it.
    #[test]
    fn a_page_repeating_one_content_stream_is_bounded() {
        let mut doc = Document::with_version("1.5");
        let first = add_stream(&mut doc, dictionary! {}, show("firstpage"));
        let repeated = add_stream(
            &mut doc,
            dictionary! {},
            show("repeated") + &" ".repeat(40_000),
        );
        let contents: Vec<Object> = vec![repeated.into(); 100_000];
        let pages = vec![
            (with_f1(helvetica()), first.into()),
            (with_f1(helvetica()), contents.into()),
        ];
        let path = save("pdf-repeated-contents", doc, pages);

        let out = extract(&path).expect("a budget stop is not a failure");
        assert!(out.contains("firstpage"), "got {:?}", out);
    }

    /// The same 20,000 times over a stream of 200 KB of deflate input that
    /// decodes to one short line. The decoded page never nears the budget,
    /// so only charging each copy's input stops it: 4 GB of inflating.
    #[test]
    fn a_page_repeating_one_deflated_stream_is_bounded() {
        let mut doc = Document::with_version("1.5");
        let first = add_stream(&mut doc, dictionary! {}, show("firstpage"));
        let line = Stream::new(dictionary! {}, show("repeated").into_bytes());
        let repeated = doc.add_object(padded(line, 40_000));
        let contents: Vec<Object> = vec![repeated.into(); 20_000];
        let pages = vec![
            (with_f1(helvetica()), first.into()),
            (with_f1(helvetica()), contents.into()),
        ];
        let path = save("pdf-repeated-deflated", doc, pages);

        let out = extract(&path).expect("a budget stop is not a failure");
        assert!(out.contains("firstpage"), "got {:?}", out);
        assert!(
            !out.contains("repeated"),
            "rendered past the stop: {:?}",
            out
        );
    }

    /// A page's content streams are one program: the font set in the first
    /// is the one the second shows text in.
    #[test]
    fn a_page_split_across_content_streams_keeps_its_font_state() {
        let mut doc = Document::with_version("1.5");
        let head = add_stream(&mut doc, dictionary! {}, "BT /F1 24 Tf 72 720 Td");
        let tail = add_stream(&mut doc, dictionary! {}, "(splitstate) Tj ET");
        let contents = Object::Array(vec![head.into(), tail.into()]);
        let path = save(
            "pdf-split-contents",
            doc,
            vec![(with_f1(helvetica()), contents)],
        );

        let out = extract(&path).expect("extract");
        assert!(out.contains("splitstate"), "got {:?}", out);
    }

    /// A 4-byte full `beginbfrange` was 2^32 inserts. It is skipped, and the
    /// rest of the CMap still maps.
    #[test]
    fn a_huge_tounicode_bfrange_is_contained() {
        let mut doc = Document::with_version("1.5");
        let font = mapped_helvetica(
            &mut doc,
            "2 beginbfrange\n<00000000> <FFFFFFFF> <0041>\n<61> <7A> <0041>\nendbfrange",
        );
        let path = save_page("pdf-huge-bfrange", doc, with_f1(font), show("contained"));

        let out = extract(&path).expect("extract");
        assert!(out.contains("CONTAINED"), "got {:?}", out);
    }

    /// A full two-byte range, the widest kept, is the shape an Identity-H
    /// font's CMap commonly takes.
    #[test]
    fn a_full_two_byte_bfrange_still_maps() {
        let mut doc = Document::with_version("1.5");
        let to_unicode = add_stream(&mut doc, dictionary! {}, cmap(IDENTITY_UCS));
        let font = type0("Identity-H", to_unicode);
        let content = format!("BT /F1 24 Tf 72 720 Td {} Tj ET", utf16_hex("fullrange"));
        let path = save_page("pdf-full-bfrange", doc, with_f1(font), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("fullrange"), "got {:?}", out);
    }

    /// Two `/Encoding` CMaps of 100,000 ranges each, both scanned for every
    /// character: 30,000 characters shown in each font is far past the
    /// watchdog. Codespace ranges past `MAX_CODESPACE_RANGES` are dropped,
    /// with the one the first font's codes fall in, so it shows nothing; CID
    /// ranges are sorted and searched, so the second font's one range among
    /// 100,000 still maps its text.
    #[test]
    fn a_type0_encoding_of_100k_ranges_is_bounded() {
        let mut doc = Document::with_version("1.5");
        let to_unicode = add_stream(&mut doc, dictionary! {}, cmap(IDENTITY_UCS));
        let late_codespace = doc.add_object(compressed(cmap(&format!(
            "1 begincodespacerange\n{}<0000> <FFFF>\nendcodespacerange\n\
             1 begincidrange\n<0000> <FFFF> 0\nendcidrange",
            "<FF> <FF>\n".repeat(100_000)
        ))));
        let late_cid = doc.add_object(compressed(cmap(&format!(
            "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
             1 begincidrange\n{}<0000> <FFFE> 0\nendcidrange",
            "<FFFF> <FFFF> 0\n".repeat(100_000)
        ))));
        let fonts = dictionary! {
            "F1" => type0(late_codespace, to_unicode),
            "F2" => type0(late_cid, to_unicode),
        };
        let shown = utf16_hex(&"cid".repeat(10_000));
        let content =
            format!("BT /F1 24 Tf 72 720 Td {shown} Tj ET BT /F2 24 Tf 72 600 Td {shown} Tj ET");
        let contents = doc.add_object(compressed(content));
        let path = save(
            "pdf-type0-ranges",
            doc,
            vec![(dictionary! { "Font" => fonts }, contents.into())],
        );

        let out = extract(&path).expect("extract");
        assert_eq!(
            out.matches("cid").count(),
            10_000,
            "got {:?}",
            &out[..out.len().min(80)]
        );
    }

    /// The array form names each code's text; these include multi-unit ones.
    #[test]
    fn a_bfrange_array_form_still_maps() {
        let mut doc = Document::with_version("1.5");
        let font = mapped_helvetica(
            &mut doc,
            "1 beginbfrange\n<01> <05> [<0061> <00720072> <00610079> <0066> <006F0072006D>]\n\
             endbfrange",
        );
        let content = "BT /F1 24 Tf 72 720 Td <0102030405> Tj ET";
        let path = save_page("pdf-bfrange-array", doc, with_f1(font), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("arrayform"), "got {:?}", out);
    }

    /// A range copies its destination for every code, so the text a CMap
    /// maps is bounded twice. A destination past the spec's 512 bytes is
    /// skipped and the CMap reads on: `/F1` maps `a` by the entry after it,
    /// and `b` falls back to the font's encoding. One of 512 bytes, over
    /// 65,438 codes, spends the 16 MiB a CMap may map halfway through, and
    /// the CMap ends there: `/F2` never reads the entry remapping `b`.
    #[test]
    fn a_tounicode_map_is_bounded_in_text() {
        let mut doc = Document::with_version("1.5");
        let long = mapped_helvetica(
            &mut doc,
            &format!(
                "1 beginbfrange\n<0000> <FFFF> <{}>\nendbfrange\n\
                 1 beginbfchar\n<61> <0042>\nendbfchar",
                "005A".repeat(300)
            ),
        );
        let wide = mapped_helvetica(
            &mut doc,
            &format!(
                "1 beginbfrange\n<0062> <FFFF> <{}>\nendbfrange\n\
                 1 beginbfchar\n<62> <0043>\nendbfchar",
                "0051".repeat(256)
            ),
        );
        let resources = dictionary! { "Font" => dictionary! { "F1" => long, "F2" => wide } };
        let content = "BT /F1 24 Tf 72 720 Td (ab) Tj ET BT /F2 24 Tf 72 600 Td (b) Tj ET";
        let path = save_page("pdf-tounicode-text", doc, resources, content);

        let out = extract(&path).expect("extract");
        assert!(
            out.contains("Bb"),
            "the long destination was mapped: {:?}",
            out
        );
        assert!(
            out.contains(&"Q".repeat(256)),
            "the range was dropped: {:?}",
            out
        );
        assert!(!out.contains('C'), "read past the text bound: {:?}", out);
    }

    /// Resource names are per page: the second page's `/F1` is its own font,
    /// not the first page's.
    #[test]
    fn two_pages_with_different_fonts_named_f1_decode_independently() {
        let mut doc = Document::with_version("1.5");
        let upper = mapped_helvetica(&mut doc, "1 beginbfrange\n<61> <7A> <0041>\nendbfrange");
        let first = add_stream(&mut doc, dictionary! {}, show("plainpage"));
        let second = add_stream(&mut doc, dictionary! {}, show("mappedpage"));
        let pages = vec![
            (with_f1(helvetica()), first.into()),
            (with_f1(upper), second.into()),
        ];
        let path = save("pdf-two-f1", doc, pages);

        let out = extract(&path).expect("extract");
        assert!(
            out.contains("plainpage") && out.contains("MAPPEDPAGE"),
            "got {:?}",
            out
        );
    }

    /// Fonts are cached per dictionary, so 10,000 names for one font parse
    /// it once. Its CMap holds 20,000 entries: parsing it once per name, as
    /// a cache keyed by name did, is far past the watchdog.
    #[test]
    fn many_names_aliasing_one_font_parse_once() {
        let mut doc = Document::with_version("1.5");
        let body = format!("1 beginbfchar\n{}endbfchar", "<61> <0061>\n".repeat(20_000));
        let font = mapped_helvetica(&mut doc, &body);
        let font = doc.add_object(font);
        let mut names = Dictionary::new();
        let mut content = String::from("BT 72 720 Td ");
        for i in 0..10_000 {
            names.set(format!("F{}", i), font);
            content.push_str(&format!("/F{} 12 Tf (alias) Tj ", i));
        }
        content.push_str("ET");
        let path = save_page(
            "pdf-font-aliases",
            doc,
            dictionary! { "Font" => names },
            content,
        );

        let out = extract(&path).expect("extract");
        assert_eq!(out.matches("alias").count(), 10_000);
    }

    /// Distinct font dictionaries are distinct fonts, but what they share
    /// is parsed once: 200 fonts naming one 3 MB ToUnicode CMap, which
    /// compresses to a few KB. Parsed again for each font, it is far past
    /// the watchdog.
    #[test]
    fn fonts_sharing_a_tounicode_stream_parse_it_once() {
        let mut doc = Document::with_version("1.5");
        let body = format!(
            "1 beginbfchar\n{}endbfchar",
            "<61> <0041>\n".repeat(250_000)
        );
        let to_unicode = doc.add_object(compressed(cmap(&body)));
        let mut fonts = Dictionary::new();
        let mut content = String::from("BT 72 720 Td ");
        for i in 0..200 {
            let mut font = helvetica();
            font.set("ToUnicode", to_unicode);
            fonts.set(format!("F{}", i), font);
            content.push_str(&format!("/F{} 12 Tf (a) Tj ", i));
        }
        content.push_str("ET");
        let path = save_page(
            "pdf-shared-tounicode",
            doc,
            dictionary! { "Font" => fonts },
            content,
        );

        let out = extract(&path).expect("extract");
        assert_eq!(out.matches('A').count(), 200, "got {:?}", out);
    }

    /// A font written inline in the resources has no object of its own; it
    /// is cached by address and decodes like any other.
    #[test]
    fn a_direct_font_dictionary_still_decodes() {
        let content = format!("{}BT /F1 12 Tf 72 600 Td (again) Tj ET", show("directfont"));
        let doc = Document::with_version("1.5");
        let path = save_page("pdf-direct-font", doc, with_f1(helvetica()), content);

        let out = extract(&path).expect("extract");
        assert!(
            out.contains("directfont") && out.contains("again"),
            "got {:?}",
            out
        );
    }

    /// A CID font whose embedded `/Encoding` CMap maps codes to CIDs at an
    /// offset, as every non-Identity CMap does. The code is what ToUnicode
    /// maps and the CID what `/W` is keyed by; upstream added the code to
    /// the range's first CID and looked ToUnicode up by that, so such a font
    /// decoded to nothing (LOCAL PATCH, `PdfCIDFont::cid`).
    #[test]
    fn a_cid_font_with_a_shifted_encoding_cmap_decodes_by_code() {
        let mut doc = Document::with_version("1.5");
        let to_unicode = add_stream(
            &mut doc,
            dictionary! {},
            cmap("2 beginbfchar\n<41> <0063>\n<42> <0064>\nendbfchar"),
        );
        // One-byte codes; `A`..`Z` are the CIDs from 100 up.
        let encoding = add_stream(
            &mut doc,
            dictionary! {},
            cmap(
                "1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                 1 begincidrange\n<41> <5A> 100\nendcidrange",
            ),
        );
        let mut font = type0(encoding, to_unicode);
        let descendants = font
            .get_mut(b"DescendantFonts")
            .and_then(Object::as_array_mut)
            .expect("the descendant font array");
        let cid_font = descendants[0].as_dict_mut().expect("the CID font dict");
        // Keyed by CID, as the spec has it.
        cid_font.set("W", vec![100.into(), vec![500.into(), 500.into()].into()]);
        let content = "BT /F1 24 Tf 72 720 Td (AB) Tj ET";
        let path = save_page("pdf-cid-shifted", doc, with_f1(font), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("cd"), "got {:?}", out);
    }

    /// A CID range nested in a wider one: a code past the nested range's end
    /// still maps through the wide range that holds it. The lookup tried only
    /// the last range starting at or below the code, so `0100` mapped to
    /// nothing and ended the string (LOCAL PATCH, `PdfCIDFont::cid`).
    #[test]
    fn a_code_outside_a_nested_cid_range_maps_through_the_wider_one() {
        let mut doc = Document::with_version("1.5");
        let to_unicode = add_stream(
            &mut doc,
            dictionary! {},
            cmap("3 beginbfchar\n<0100> <0078>\n<0041> <0079>\n<0200> <007A>\nendbfchar"),
        );
        let encoding = add_stream(
            &mut doc,
            dictionary! {},
            cmap(
                "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
                 2 begincidrange\n<0000> <FFFF> 0\n<0020> <007E> 1\nendcidrange",
            ),
        );
        let content = "BT /F1 24 Tf 72 720 Td <010000410200> Tj ET";
        let font = type0(encoding, to_unicode);
        let path = save_page("pdf-cid-nested", doc, with_f1(font), content);

        let out = extract(&path).expect("extract");
        assert!(out.contains("xyz"), "got {:?}", out);
    }

    /// A glyph missing from `/Widths` used to collect and sort every width
    /// for a log line that compiled to nothing: here 10,000 misses against
    /// 200,000 widths.
    #[test]
    fn a_glyph_missing_from_a_huge_widths_array_is_cheap() {
        let mut font = helvetica();
        font.set("FirstChar", 300);
        font.set("LastChar", 300 + 200_000 - 1);
        font.set("Widths", vec![Object::Integer(500); 200_000]);
        let content = format!("BT /F1 12 Tf 72 720 Td ({}) Tj ET", "w".repeat(10_000));
        let doc = Document::with_version("1.5");
        let path = save_page("pdf-huge-widths", doc, with_f1(font), content);

        let out = extract(&path).expect("extract");
        assert_eq!(out.matches('w').count(), 10_000);
    }

    /// Each font built reads its `/Widths` afresh, however many fonts share
    /// the array, and is charged for it before it is built: 10,000 fonts
    /// sharing 100,000 widths are a billion reads, past the watchdog, and the
    /// budget stops the document after a few hundred, keeping their text.
    #[test]
    fn fonts_sharing_a_huge_widths_array_are_charged_each_read() {
        const FONTS: usize = 10_000;
        const WIDTHS: i64 = 100_000;
        let mut doc = Document::with_version("1.5");
        let widths = doc.add_object(vec![Object::Integer(500); WIDTHS as usize]);
        let mut fonts = Dictionary::new();
        let mut content = String::from("BT 72 720 Td ");
        for i in 0..FONTS {
            let mut font = helvetica();
            font.set("FirstChar", 0);
            font.set("LastChar", WIDTHS - 1);
            font.set("Widths", widths);
            fonts.set(format!("F{}", i), font);
            content.push_str(&format!("/F{} 12 Tf (w) Tj ", i));
        }
        content.push_str("ET");
        let path = save_page(
            "pdf-shared-widths",
            doc,
            dictionary! { "Font" => fonts },
            content,
        );

        let out = extract(&path).expect("a budget stop is not a failure");
        let drawn = out.matches('w').count();
        assert!(
            drawn > 0 && drawn < FONTS,
            "{} of {} fonts drew",
            drawn,
            FONTS
        );
    }
}
