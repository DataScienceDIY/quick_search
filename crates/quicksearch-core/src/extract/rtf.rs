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
        let p = crate::testutil::scratch_dir(tag).join("sample.rtf");
        crate::testutil::touch(&p, body);
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

    /// A `\\u` escape naming a lone UTF-16 surrogate must fail the file, not
    /// the thread.
    ///
    /// `rtf-parser` reaches `String::from_utf16(..).unwrap()` with whatever
    /// `\\uN` supplied, and screens nothing for the surrogate range. RTF is one
    /// of the two extractors that also run at *walk* time, off
    /// `extract_from_head`, where a panicking worker costs the root its entire
    /// content pass and disables stale cleanup run-wide — so this is contained
    /// in `decide_content` and `prepare_file_record` rather than left to the
    /// parser. Both entry points are exercised here.
    #[test]
    fn a_lone_surrogate_escape_is_contained() {
        let body = br"{\rtf1\u55296 }";
        let p = tmp("surrogate", body);

        // The on-disk path, as the content pass reaches it.
        let outcome = crate::file_handling::decide_content(
            p.to_str().unwrap(),
            Some("application/rtf"),
            &crate::extract::Registry::default_set(),
            &crate::config::Config::default(),
        );
        assert!(
            matches!(outcome, crate::file_handling::ContentOutcome::Failed(_)),
            "a panicking parser must record a failure, not unwind: {:?}",
            outcome
        );

        // And the head path, as a walk worker reaches it: through the
        // registry, which is where the containment lives. The raw
        // `RtfExtractor::extract_from_head` below it still panics — that is
        // third-party code doing what it does, and the point is that no
        // caller in this crate is exposed to it.
        let head = crate::extract::Registry::default_set().extract_complete_head(
            &p,
            "application/rtf",
            body,
        );
        assert!(
            matches!(head, Some(Err(_))),
            "a panicking parser must be charged to the file, not the worker: {:?}",
            head.map(|r| r.map(|c| c.text))
        );

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn malformed_input_errors_and_names_the_file() {
        let p = tmp("broken", br"{\rtf1 truncated");
        let err = RtfExtractor.extract(&p).unwrap_err();
        // The path itself, not a fixed prefix: this is the message a user sees
        // in `list-failed`, and it is useless without naming the file.
        assert!(
            err.contains(&p.display().to_string()),
            "must name the file: {}",
            err
        );
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
