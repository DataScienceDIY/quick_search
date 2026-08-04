//! RTF text extraction via the `rtf-parser` crate.
//!
//! Claims `application/rtf` (what both `mime_guess` and `infer`'s magic
//! matcher emit) and `text/rtf` (a common alias). Registered *before* the
//! plaintext extractor in [`super::Registry::default_set`], because
//! plaintext claims every `text/*` and would otherwise swallow `text/rtf`
//! and index the control-word noise raw.

use std::path::Path;

use rtf_parser::document::RtfDocument;

use super::{ExtractError, ExtractedContent, Extractor};

/// Parse a complete RTF file's bytes. Shared by both entry points so
/// on-disk and already-in-memory extraction cannot drift apart.
///
/// RTF is 7-bit ASCII by design — non-ASCII characters travel as `\'hh` and
/// `\uN` escapes — so a lossy UTF-8 view loses nothing from a well-formed
/// document, and a malformed one fails in the parser with a real reason
/// rather than in the decode.
fn parse(bytes: Vec<u8>, path: &Path) -> Result<ExtractedContent, ExtractError> {
    let source = String::from_utf8_lossy(&bytes);
    match RtfDocument::try_from(source.as_ref()) {
        Ok(doc) => Ok(ExtractedContent::with_text(doc.get_text())),
        Err(e) => Err(format!("rtf parse {}: {}", path.display(), e)),
    }
}

pub struct RtfExtractor;

impl Extractor for RtfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/rtf" || mime == "text/rtf"
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        // Plain read: RTF files are rare and small enough that plaintext's
        // sized-read syscall trimming would be tuning without a workload.
        let bytes =
            std::fs::read(path).map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
        parse(bytes, path)
    }

    /// RTF has no trailer and needs no seeking, so a head that is the whole
    /// file parses exactly like the on-disk path.
    fn extract_from_head(
        &self,
        path: &Path,
        head: &[u8],
    ) -> Option<Result<ExtractedContent, ExtractError>> {
        Some(parse(head.to_vec(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-rtf-{}-{}-{}.rtf",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn extracts_text_without_control_words() {
        let body = br"{\rtf1\ansi Hello {\b World}!}";
        let p = tmp("basic", body);
        let c = RtfExtractor.extract(&p).unwrap();
        assert_eq!(c.text, "Hello World!");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn head_extraction_matches_reading_the_file() {
        // `\'e9` is the RTF hex escape for an e-acute: the literal itself
        // stays 7-bit ASCII while the extracted text does not.
        let body = br"{\rtf1\ansi caf\'e9 at noon}";
        let p = tmp("agree", body);
        let from_disk = RtfExtractor.extract(&p).unwrap();
        let from_head = RtfExtractor.extract_from_head(&p, body).unwrap().unwrap();
        assert_eq!(from_disk.text, from_head.text);
        assert!(from_disk.text.contains("café"), "{:?}", from_disk.text);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn malformed_input_errors_and_names_the_file() {
        let p = tmp("broken", br"{\rtf1 truncated");
        let err = RtfExtractor.extract(&p).unwrap_err();
        assert!(err.contains("qs-rtf-broken"), "must name the file: {}", err);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn supports_rtf_mimes_only() {
        let e = RtfExtractor;
        assert!(e.supports("application/rtf"));
        assert!(e.supports("text/rtf"));
        assert!(!e.supports("text/plain"));
        assert!(!e.supports("application/pdf"));
    }
}
