//! RTF text extraction via the `rtf-parser` crate.
//!
//! Claims `application/rtf` (what both `mime_guess` and `infer`'s magic
//! matcher emit) and `text/rtf` (a common alias). Registered *before* the
//! plaintext extractor in [`super::Registry::default_set`], because
//! plaintext claims every `text/*` and would otherwise swallow `text/rtf`
//! and index the control-word noise raw.
//!
//! `rtf-parser` resolves to `vendor/rtf-parser`, a patched copy — its lexer
//! ended a control word at whitespace and nowhere else, which silently dropped
//! text from documents LibreOffice and Word produce. The `[patch.crates-io]`
//! note in the workspace manifest is where that is written up; the tests below
//! and `tests/extraction_corpus.rs` are what keep it fixed.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rtf_parser::document::RtfDocument;

use super::{ExtractError, ExtractedContent, Extractor};

/// Ceiling on a single read; see [`super::plaintext`], same reasoning.
const MAX_READ: usize = 64 * 1024 * 1024;

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

/// Read at most `cap` bytes of `path`.
///
/// Bounded rather than `fs::read`: the size gate that admitted this file was
/// applied to what the walk recorded, and the file may have grown since.
/// `rtf-parser` also amplifies its input several-fold in heap, so an unbounded
/// read here is unbounded twice over.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, ExtractError> {
    let file = File::open(path).map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
    let mut bytes = Vec::new();
    file.take(cap)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
    Ok(bytes)
}

impl Extractor for RtfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/rtf" || mime == "text/rtf"
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        parse(read_capped(path, MAX_READ as u64)?, path)
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

    /// A `\\u` escape naming a lone UTF-16 surrogate costs one character, not
    /// the document and not the thread.
    ///
    /// `rtf-parser` reached `String::from_utf16(..).unwrap()` with whatever
    /// `\\uN` supplied and screened nothing for the surrogate range, so a
    /// fifteen-byte document could panic. RTF is one of the two extractors that
    /// also run at *walk* time, off `extract_from_head`, where a panicking
    /// worker costs the root its entire content pass and disables stale
    /// cleanup run-wide — so the panic was contained in `decide_content` and
    /// `prepare_file_record`, and the file recorded as FAILED.
    ///
    /// `vendor/rtf-parser` decodes lossily instead (LOCAL PATCH, see
    /// `Parser::flush_unicode`), which beats either outcome: the bad escape
    /// becomes one `U+FFFD` and the rest of the document is indexed. Both
    /// entry points are still exercised, because the containment above them
    /// has to keep working for every other way a parser can panic.
    #[test]
    fn a_lone_surrogate_escape_costs_one_character() {
        // `\u55296` is a high surrogate with no low half to follow it. The `?`
        // is its ANSI fallback, written the way a real producer writes one —
        // spelled with a space delimiter instead, the `a` of `after` would be
        // the fallback and would correctly be eaten.
        let body = "{\\rtf1\\ansi before \\u55296?after}".as_bytes();
        let p = tmp("surrogate", body);

        // The on-disk path, as the content pass reaches it.
        let outcome = crate::file_handling::decide_content(
            p.to_str().unwrap(),
            Some("application/rtf"),
            &crate::extract::Registry::default_set(),
            &crate::config::Config::default(),
        );
        let text = match &outcome {
            crate::file_handling::ContentOutcome::Done { text } => text.clone(),
            other => panic!("a malformed escape must not fail the document: {other:?}"),
        };
        assert!(
            text.contains("before") && text.contains("after"),
            "the rest of the document must survive: {text:?}"
        );
        assert!(
            text.contains('\u{FFFD}'),
            "the bad escape must leave a replacement character: {text:?}"
        );

        // And the head path, as a walk worker reaches it: through the registry,
        // which is where the containment for any *other* panicking input lives.
        let head = crate::extract::Registry::default_set().extract_complete_head(
            &p,
            "application/rtf",
            body,
        );
        assert_eq!(
            head.expect("claimed").expect("parsed").text,
            text,
            "head and disk extraction must agree"
        );

        std::fs::remove_file(&p).ok();
    }

    /// `\\par` ends a paragraph, so it has to reach the text as a line break.
    ///
    /// It used to emit nothing, and every paragraph boundary closed up:
    /// a LibreOffice document came back as `...do eiusmod.The needle...`.
    /// No text was lost, but the join invents word and sentence boundaries
    /// that are not in the document — which a snippet then shows to the user,
    /// and which a phrase query can match across. Fixed in
    /// `vendor/rtf-parser` (LOCAL PATCH), alongside `\\line`, which always
    /// did the right thing.
    #[test]
    fn paragraph_breaks_reach_the_text() {
        let body = br"{\rtf1\ansi First paragraph.\par Second paragraph.\par}";
        let p = tmp("par", body);
        let text = RtfExtractor.extract(&p).unwrap().text;
        assert!(
            text.contains("First paragraph.\nSecond paragraph."),
            "paragraphs ran together: {text:?}"
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

    /// `rtf-parser` amplifies its input several-fold in heap, so the read that
    /// feeds it has to be bounded independently of what the walk recorded.
    #[test]
    fn a_read_stops_at_the_cap() {
        let body = vec![b'x'; 4096];
        let p = tmp("cap", &body);
        assert_eq!(
            read_capped(&p, 100).unwrap().len(),
            100,
            "read past the cap"
        );
        assert_eq!(
            read_capped(&p, MAX_READ as u64).unwrap().len(),
            4096,
            "a file under the cap must be read whole"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let p = crate::testutil::scratch_dir("rtf-missing").join("nope.rtf");
        let err = read_capped(&p, MAX_READ as u64).unwrap_err();
        assert!(err.contains(&p.display().to_string()), "{err}");
    }
}
