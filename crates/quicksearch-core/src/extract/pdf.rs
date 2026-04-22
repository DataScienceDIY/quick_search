//! PDF text extraction.
//!
//! Primary path: [`pdf_extract::extract_text`], which handles most modern PDFs
//! and is simple to call. It can panic or hard-error on malformed files; any
//! failure is surfaced to the caller and marks the file's content state as
//! failed. Properties (title, author, etc.) from the PDF `Info` dictionary
//! are pulled via `lopdf` where available.

use std::path::Path;

use lopdf::{Document as LopdfDocument, Object};

use super::{ExtractError, ExtractedContent, Extractor};

pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        // Text. Catch panics from pdf_extract (some PDFs crash its parser).
        let path_buf = path.to_path_buf();
        let text = std::panic::catch_unwind(move || pdf_extract::extract_text(&path_buf))
            .map_err(|_| "pdf_extract panicked".to_string())?
            .map_err(|e| format!("pdf_extract: {}", e))?;

        let mut out = ExtractedContent::with_text(text);

        // Info dictionary via lopdf. Soft-fail: if lopdf can't open the file
        // we still return the text.
        if let Ok(doc) = LopdfDocument::load(path) {
            if let Ok(info_ref) = doc.trailer.get(b"Info") {
                if let Ok(info_id) = info_ref.as_reference() {
                    if let Ok(info) = doc.get_object(info_id) {
                        if let Ok(dict) = info.as_dict() {
                            for key in ["Title", "Author", "Subject", "Keywords", "Creator", "Producer"] {
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
    fn supports_pdf_mime() {
        assert!(PdfExtractor.supports("application/pdf"));
        assert!(!PdfExtractor.supports("application/zip"));
    }
}
