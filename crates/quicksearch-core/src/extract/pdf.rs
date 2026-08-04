//! PDF text extraction.
//!
//! Primary path: [`pdf_extract::extract_text`], which handles most modern PDFs
//! and is simple to call. It can panic or hard-error on malformed files; any
//! failure is surfaced to the caller and marks the file's content state as
//! failed. Properties (title, author, etc.) from the PDF `Info` dictionary
//! are pulled via `lopdf` where available.

use std::cell::Cell;
use std::path::Path;
use std::sync::OnceLock;

use lopdf::{Document as LopdfDocument, Object};

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
        // Text. Catch panics from pdf_extract (some PDFs crash its parser)
        // and keep the default hook from spamming stderr about them.
        install_quiet_panic_hook();
        let path_buf = path.to_path_buf();
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(true));
        let result = std::panic::catch_unwind(move || pdf_extract::extract_text(&path_buf));
        SUPPRESS_PANIC_PRINT.with(|flag| flag.set(false));
        let text = result
            .map_err(|panic| format!("pdf_extract panicked: {}", panic_message(&*panic)))?
            .map_err(|e| format!("pdf_extract: {}", e))?;

        let mut out = ExtractedContent::with_text(text);

        // Info dictionary via lopdf. Soft-fail: if lopdf can't open the file
        // we still return the text.
        if let Ok(doc) = LopdfDocument::load(path) {
            if let Ok(info_ref) = doc.trailer.get(b"Info") {
                if let Ok(info_id) = info_ref.as_reference() {
                    if let Ok(info) = doc.get_object(info_id) {
                        if let Ok(dict) = info.as_dict() {
                            for key in [
                                "Title", "Author", "Subject", "Keywords", "Creator", "Producer",
                            ] {
                                if let Ok(val) = dict.get(key.as_bytes()) {
                                    if let Some(s) = object_to_string(val) {
                                        if !s.is_empty() {
                                            out.properties.insert(key.to_ascii_lowercase(), s);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(out)
    }
}

fn object_to_string(obj: &Object) -> Option<String> {
    match obj {
        Object::String(bytes, _) => {
            // Try UTF-8; fall back to lossy decoding.
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
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
}
