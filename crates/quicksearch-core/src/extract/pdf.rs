//! PDF text extraction via `pdf_extract`.
//! `lopdf` must come from `pdf_extract`'s `pub use lopdf::*`, never declared in
//! `Cargo.toml` too: that resolved a second, older copy (plus a rayon pool).

use std::cell::Cell;
use std::path::Path;
use std::sync::OnceLock;

use pdf_extract::{Document, PlainTextOutput};

use super::{ExtractError, Extractor};

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

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    fn extract(&self, path: &Path) -> Result<String, ExtractError> {
        // Loading is inside the guard too — a panic outside it takes the thread.
        install_quiet_panic_hook();
        let path_buf = path.to_path_buf();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result = std::panic::catch_unwind(move || extract_one_pass(&path_buf));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        result.map_err(|panic| format!("pdf_extract panicked: {}", panic_message(&*panic)))?
    }
}

/// `pdf_extract::extract_text`'s body (load, decrypt, `output_doc`) spelled out.
fn extract_one_pass(path: &Path) -> Result<String, ExtractError> {
    let mut doc = Document::load(path).map_err(|e| format!("pdf_extract: {}", e))?;
    // Empty-password decrypt only; nothing on this path can prompt for a real one.
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|e| format!("pdf_extract: {}", e))?;
    }

    let mut text = String::new();
    {
        let mut sink = PlainTextOutput::new(&mut text);
        pdf_extract::output_doc(&doc, &mut sink).map_err(|e| format!("pdf_extract: {}", e))?;
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contained_panics_are_caught_quietly_with_reason() {
        install_quiet_panic_hook();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result = std::panic::catch_unwind(|| panic!("synthetic pdf failure"));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        let payload = result.expect_err("must panic");
        assert_eq!(panic_message(&*payload), "synthetic pdf failure");
        assert!(!SUPPRESS_PANIC_PRINT.with(|flag| flag.get()));
    }

    #[test]
    fn supports_pdf_mime() {
        assert!(PdfExtractor.supports("application/pdf"));
        assert!(!PdfExtractor.supports("application/zip"));
    }

    use pdf_extract::{dictionary, Dictionary, Object, Stream, StringFormat};
    use std::path::PathBuf;

    /// Write a one-page PDF drawing `body`, with `info` as its `Info`
    /// dictionary, using only `pdf_extract`'s `lopdf` re-export — a move in
    /// that surface fails to compile here alongside the extractor. Helvetica
    /// is a base-14 font with built-in encoding tables, so no font file.
    fn write_pdf(tag: &str, body: &str, info: Option<Dictionary>) -> PathBuf {
        let mut doc = Document::with_version("1.5");
        let font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font },
        });
        let content = format!("BT /F1 24 Tf 72 720 Td ({}) Tj ET", body);
        let contents = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page.into()],
                "Count" => 1,
                "Resources" => resources,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);
        if let Some(info) = info {
            let info_id = doc.add_object(Object::Dictionary(info));
            doc.trailer.set("Info", info_id);
        }

        let path = crate::testutil::scratch_dir(tag).join("fixture.pdf");
        doc.save(&path).expect("write fixture pdf");
        path
    }

    fn text_string(s: &str) -> Object {
        Object::String(s.as_bytes().to_vec(), StringFormat::Literal)
    }

    #[test]
    fn extracts_text_from_a_document_with_an_info_dictionary() {
        let path = write_pdf(
            "pdf-full",
            "Hello QuickSearch",
            Some(dictionary! {
                "Title" => text_string("The Title"),
                "Author" => text_string("An Author"),
                "Subject" => text_string("A Subject"),
                "Keywords" => text_string("alpha beta"),
                "Creator" => text_string("A Creator"),
                "Producer" => text_string("A Producer"),
            }),
        );

        let out = PdfExtractor.extract(&path).expect("extract");
        assert!(
            out.contains("Hello QuickSearch"),
            "drawn text missing from {:?}",
            out
        );
    }

    #[test]
    fn missing_info_dictionary_still_yields_text() {
        let path = write_pdf("pdf-noinfo", "Body Only", None);
        let out = PdfExtractor.extract(&path).expect("extract");
        assert!(out.contains("Body Only"));
    }

    #[test]
    fn malformed_pdf_fails_without_panicking_the_process() {
        let path = crate::testutil::scratch_dir("pdf-malformed").join("broken.pdf");
        std::fs::write(&path, b"%PDF-1.4\n\x00\x01\x02 not a pdf at all \xff\xfe").unwrap();

        let err = PdfExtractor
            .extract(&path)
            .expect_err("malformed pdf must fail");
        assert!(
            err.starts_with("pdf_extract"),
            "unexpected failure reason: {}",
            err
        );
    }

    /// A page whose `/Parent` is itself must be skipped, not fatal. Upstream
    /// `get_inherited` walked `/Parent` with no depth bound; a cycle overflows
    /// the stack, which aborts — `catch_unwind` cannot hold it — so
    /// `vendor/pdf-extract` bounds the walk.
    #[test]
    fn a_self_referential_page_parent_is_contained() {
        let mut doc = Document::with_version("1.5");
        let contents = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.new_object_id();
        // No `Resources` or `MediaBox`, so both lookups follow `/Parent` — into itself.
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
        let _ = PdfExtractor.extract(&path);
    }

    /// A Form XObject drawing itself must be skipped — the second unbounded
    /// recursion (`process_stream`'s `Do` arm); the bound also caps the
    /// 2^depth branching shape.
    #[test]
    fn a_self_drawing_form_xobject_is_contained() {
        let mut doc = Document::with_version("1.5");
        let form_id = doc.new_object_id();
        // Its own resources name it, so `/X0 Do` inside it re-enters itself.
        doc.objects.insert(
            form_id,
            Object::Stream(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => dictionary! {
                        "XObject" => dictionary! { "X0" => form_id },
                    },
                },
                b"/X0 Do".to_vec(),
            )),
        );
        let resources = doc.add_object(dictionary! {
            "XObject" => dictionary! { "X0" => form_id },
        });
        let contents = doc.add_object(Stream::new(dictionary! {}, b"/X0 Do".to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page.into()],
                "Count" => 1,
                "Resources" => resources,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);

        let path = crate::testutil::scratch_dir("pdf-xobject-cycle").join("cycle.pdf");
        doc.save(&path).expect("write fixture pdf");

        let _ = PdfExtractor.extract(&path);
    }
}
