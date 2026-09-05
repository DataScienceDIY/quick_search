//! RTF text extraction via `rtf-parser` (patched — `vendor/rtf-parser`, see
//! the workspace `[patch.crates-io]` note). Registers before the plaintext
//! extractor, which claims every `text/*` and would index `text/rtf` raw.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rtf_parser::document::RtfDocument;

use super::{ExtractError, Extractor, Scratch};

/// RTF is 7-bit ASCII by design — non-ASCII travels as `\'hh` and `\uN`
/// escapes — so the lossy UTF-8 view loses nothing from a well-formed file.
///
/// Borrowed: `from_utf8_lossy` borrows an already-valid buffer, so an owned
/// argument bought nothing and cost the head path a full copy.
fn parse(bytes: &[u8], path: &Path, out: &mut String) -> Result<(), ExtractError> {
    let source = String::from_utf8_lossy(bytes);
    match RtfDocument::try_from(source.as_ref()) {
        Ok(doc) => {
            // `get_text` builds its own string; taking it whole is one move
            // when `out` is empty, which it is for every caller today.
            let text = doc.get_text();
            if out.is_empty() {
                *out = text;
            } else {
                out.push_str(&text);
            }
            Ok(())
        }
        Err(e) => Err(format!("rtf parse {}: {}", path.display(), e)),
    }
}

pub struct RtfExtractor;

/// Read at most `cap` bytes of `path` into `buf`; `rtf-parser` amplifies its
/// input several-fold in heap, so the read stays bounded whatever the walk
/// recorded. `buf` is the worker's, reused file after file.
fn read_capped(path: &Path, cap: u64, buf: &mut Vec<u8>) -> Result<(), ExtractError> {
    let file = File::open(path).map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
    buf.clear();
    file.take(cap)
        .read_to_end(buf)
        .map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
    Ok(())
}

impl Extractor for RtfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/rtf" || mime == "text/rtf"
    }

    fn extract(
        &self,
        path: &Path,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        let cap = scratch.limits().read as u64;
        read_capped(path, cap, scratch.bytes_mut())?;
        parse(scratch.bytes_mut(), path, out)
    }

    /// RTF has no trailer and needs no seeking; a complete head parses like disk.
    fn extract_from_head(
        &self,
        path: &Path,
        head: &[u8],
        out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        Some(parse(head, path, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Scratch {
        Scratch::new(&crate::config::Config::default())
    }

    /// The default `maximum_text_file_size`, which is what bounds a read now.
    fn max_read() -> u64 {
        scratch().limits().read as u64
    }

    fn read_capped_to_vec(path: &Path, cap: u64) -> Result<Vec<u8>, ExtractError> {
        let mut buf = Vec::new();
        read_capped(path, cap, &mut buf).map(|()| buf)
    }

    fn extract(path: &Path) -> Result<String, ExtractError> {
        let mut out = String::new();
        RtfExtractor.extract(path, &mut out, &mut scratch()).map(|()| out)
    }

    fn extract_from_head(path: &Path, head: &[u8]) -> Option<Result<String, ExtractError>> {
        let mut out = String::new();
        match RtfExtractor.extract_from_head(path, head, &mut out) {
            Some(Ok(())) => Some(Ok(out)),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }

    fn tmp(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = crate::testutil::scratch_dir(tag).join("sample.rtf");
        crate::testutil::touch(&p, body);
        p
    }

    #[test]
    fn extracts_text_without_control_words() {
        let body = br"{\rtf1\ansi Hello {\b World}!}";
        let p = tmp("basic", body);
        let c = extract(&p).unwrap();
        assert_eq!(c, "Hello World!");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn head_extraction_matches_reading_the_file() {
        // `\'e9` is the RTF hex escape for an e-acute: the literal stays 7-bit ASCII.
        let body = br"{\rtf1\ansi caf\'e9 at noon}";
        let p = tmp("agree", body);
        let from_disk = extract(&p).unwrap();
        let from_head = extract_from_head(&p, body).unwrap().unwrap();
        assert_eq!(from_disk, from_head);
        assert!(from_disk.contains("café"), "{:?}", from_disk);
        std::fs::remove_file(&p).ok();
    }

    /// A `\\uN` escape naming a lone UTF-16 surrogate costs one character, not
    /// the document: upstream reached `String::from_utf16(..).unwrap()` with it
    /// unscreened and panicked. `vendor/rtf-parser` (LOCAL PATCH,
    /// `Parser::flush_unicode`) decodes lossily — one `U+FFFD`, rest indexed.
    /// Both entry points stay exercised; the containment above them must keep
    /// working for every other way a parser can panic.
    #[test]
    fn a_lone_surrogate_escape_costs_one_character() {
        // `\u55296` is a lone high surrogate; the `?` is its ANSI fallback,
        // delimited the way a real producer writes one.
        let body = "{\\rtf1\\ansi before \\u55296?after}".as_bytes();
        let p = tmp("surrogate", body);

        // The on-disk path, as the content pass reaches it.
        let outcome = crate::file_handling::decide_content(
            p.to_str().unwrap(),
            Some("application/rtf"),
            &crate::extract::Registry::default_set(),
            &crate::config::Config::default(),
            &mut scratch(),
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

        // The head path, through the registry, where containment for other panics lives.
        let head = crate::extract::Registry::default_set().extract_head_to_string(
            &p,
            "application/rtf",
            body,
        );
        assert_eq!(
            head.expect("claimed").expect("parsed"),
            text,
            "head and disk extraction must agree"
        );

        std::fs::remove_file(&p).ok();
    }

    /// `\\par` ends a paragraph and must reach the text as a line break; it
    /// used to emit nothing and paragraph boundaries closed up. Fixed in
    /// `vendor/rtf-parser` (LOCAL PATCH), alongside `\\line`.
    #[test]
    fn paragraph_breaks_reach_the_text() {
        let body = br"{\rtf1\ansi First paragraph.\par Second paragraph.\par}";
        let p = tmp("par", body);
        let text = extract(&p).unwrap();
        assert!(
            text.contains("First paragraph.\nSecond paragraph."),
            "paragraphs ran together: {text:?}"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn malformed_input_errors_and_names_the_file() {
        let p = tmp("broken", br"{\rtf1 truncated");
        let err = extract(&p).unwrap_err();
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

    #[test]
    fn a_read_stops_at_the_cap() {
        let body = vec![b'x'; 4096];
        let p = tmp("cap", &body);
        assert_eq!(
            read_capped_to_vec(&p, 100).unwrap().len(),
            100,
            "read past the cap"
        );
        assert_eq!(
            read_capped_to_vec(&p, max_read()).unwrap().len(),
            4096,
            "a file under the cap must be read whole"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let p = crate::testutil::scratch_dir("rtf-missing").join("nope.rtf");
        let err = read_capped_to_vec(&p, max_read()).unwrap_err();
        assert!(err.contains(&p.display().to_string()), "{err}");
    }
}
