//! PDF text extraction.
//!
//! One `Document::load` per file, then the text is taken off it.
//! `pdf_extract` can panic or hard-error on malformed files; any failure is
//! surfaced to the caller and marks the file's content state as failed.
//!
//! `lopdf` is reached through `pdf_extract`'s own `pub use lopdf::*` and must
//! **not** be declared in `Cargo.toml` again: a direct declaration resolved a
//! *second, older* copy, dragging `rayon` — and a global thread pool that is
//! never torn down — plus `chrono`, `time`, `md5` and a second `nom` into the
//! build. The re-export is not a semver-guaranteed surface, but a break in it
//! is a compile error rather than a silent behaviour change.

use std::cell::Cell;
use std::path::Path;
use std::sync::OnceLock;

use pdf_extract::{Document, PlainTextOutput};

use super::{ExtractError, ExtractedContent, Extractor};

thread_local! {
    /// True while this thread is inside a contained `pdf_extract` call.
    static SUPPRESS_PANIC_PRINT: Cell<bool> = const { Cell::new(false) };
}

/// Chain a process panic hook (once) that swallows the default
/// "thread panicked at …" report while this thread is inside a *contained*
/// PDF extraction — those panics are expected on malformed PDFs, caught,
/// and recorded as the file's failure reason, so printing each one is pure
/// console spam. Panics anywhere else print exactly as before.
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

/// Human-readable message from a caught panic payload; lands in
/// `failed_files.reason`.
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

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        // Catch panics from pdf_extract (some PDFs crash its parser). The
        // whole operation is inside the guard, document loading included — a
        // panic in the parser outside it would reach the process hook and
        // take the thread with it.
        install_quiet_panic_hook();
        let path_buf = path.to_path_buf();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result = std::panic::catch_unwind(move || extract_one_pass(&path_buf));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        result.map_err(|panic| format!("pdf_extract panicked: {}", panic_message(&*panic)))?
    }
}

// properties (parked) — the `Info` dictionary read. See
// `super::ExtractedContent`; reviving this also needs the `Object` import
// above and `object_to_string` below.
//
// /// The six `Info` keys worth keeping, in the order they are written.
// const INFO_KEYS: [&str; 6] = [
//     "Title", "Author", "Subject", "Keywords", "Creator", "Producer",
// ];

/// Load the document once and take the text off it. This is
/// `pdf_extract::extract_text`'s body (load, decrypt, `output_doc`) spelled
/// out, which is also what kept the document in scope for the `Info` read.
fn extract_one_pass(path: &Path) -> Result<ExtractedContent, ExtractError> {
    let mut doc = Document::load(path).map_err(|e| format!("pdf_extract: {}", e))?;
    // Decryption must happen before either the content streams or the `Info`
    // strings mean anything. Empty password only: a real one is the user's
    // to supply and nothing on this path can ask for it.
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|e| format!("pdf_extract: {}", e))?;
    }

    let mut text = String::new();
    {
        let mut sink = PlainTextOutput::new(&mut text);
        pdf_extract::output_doc(&doc, &mut sink).map_err(|e| format!("pdf_extract: {}", e))?;
    }
    // properties (parked): the `Info` dictionary was read here, soft-failing
    // when absent because the text is the half that matters. It is all that
    // held `doc` open past `output_doc`.
    //
    //  let info = doc
    //      .trailer
    //      .get(b"Info")
    //      .ok()
    //      .and_then(|o| o.as_reference().ok())
    //      .and_then(|id| doc.get_object(id).ok())
    //      .and_then(|o| o.as_dict().ok());
    //  if let Some(dict) = info {
    //      for key in INFO_KEYS {
    //          if let Some(s) = dict.get(key.as_bytes()).ok().and_then(object_to_string) {
    //              if !s.is_empty() {
    //                  out.properties.insert(key.to_ascii_lowercase(), s);
    //              }
    //          }
    //      }
    //  }
    //
    //  fn object_to_string(obj: &Object) -> Option<String> {
    //      match obj {
    //          Object::String(bytes, _) => Some(String::from_utf8_lossy(bytes).into_owned()),
    //          Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
    //          _ => None,
    //      }
    //  }
    Ok(ExtractedContent::with_text(text))
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
        // Panics outside the suppression window keep printing: the flag is
        // thread-local and cleared, so nothing here can silence other
        // threads or later tests.
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
    /// dictionary, and return the path.
    ///
    /// Everything needed is public through `pdf_extract`'s `lopdf` re-export
    /// — the same surface the extractor uses — so if that re-export moves,
    /// these fail to compile alongside it. The page is minimal but complete:
    /// `output_doc` walks Catalog → Pages → Page and needs `MediaBox`, a
    /// resolvable `Resources` font and a content stream; Helvetica is a
    /// base-14 font with built-in encoding tables, so no font file is
    /// involved.
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

    /// A document *with* an `Info` dictionary still extracts its text — the
    /// dictionary is no longer read, and must not get in the way.
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
            out.text.contains("Hello QuickSearch"),
            "drawn text missing from {:?}",
            out.text
        );
    }

    /// The soft-fail path: no `Info` dictionary is not an extraction failure,
    /// because the text is the half that matters.
    #[test]
    fn missing_info_dictionary_still_yields_text() {
        let path = write_pdf("pdf-noinfo", "Body Only", None);
        let out = PdfExtractor.extract(&path).expect("extract");
        assert!(out.text.contains("Body Only"));
    }

    /// Malformed input must come back as an error, not take the process
    /// down — the case the `catch_unwind` around the document load exists
    /// for.
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
}
