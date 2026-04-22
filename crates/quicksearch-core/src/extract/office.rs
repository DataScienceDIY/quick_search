//! Office document extraction: DOCX, XLSX, PPTX, ODT, ODP, ODS.
//!
//! Delegates to [`crate::document_extraction`] — a single entry point based
//! on file extension rather than MIME. We translate the MIME to the
//! extension expected by that module.

use std::ffi::OsString;
use std::path::Path;

use crate::document_extraction::extract_document_text;

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

impl Extractor for OfficeExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime_to_ext(mime).is_some()
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        // Recompute the extension from MIME at call time by asking the path.
        // Using the filesystem extension directly is more robust than round-
        // tripping through MIME: `.docm` has the same MIME as `.docx` but
        // `extract_document_text` looks up by extension.
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let text = extract_document_text(&OsString::from(path.as_os_str()), &ext)
            .map_err(|e| format!("office extractor {}: {}", path.display(), e))?;
        Ok(ExtractedContent::with_text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
