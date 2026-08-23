//! Content extractors: the searchable text of a file. The [`Registry`] runs
//! the first registered [`Extractor`] that accepts the MIME.
//!
//! Dispatch is by MIME only: "what is this file" is decided once, upstream in
//! [`crate::mime::guess_mime_from_head`]; nothing downstream reopens the
//! file to ask again.

use std::path::Path;

pub mod audio;
pub mod office;
pub mod ole;
pub mod pdf;
pub mod plaintext;
pub mod rtf;

/// The string reason is stored on the file row, so extractors should surface
/// human-readable messages.
pub type ExtractError = String;

/// Run `f`, turning a panic into an [`ExtractError`] naming the file. The
/// extractors drive third-party parsers over bytes chosen by whoever wrote
/// the file, and several are documented to panic on malformed input.
fn contain_panic<T>(path: &Path, f: impl FnOnce() -> T) -> Result<T, ExtractError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| format!("extractor panicked on {}", path.display()))
}

/// A pluggable content extractor; stateless.
pub trait Extractor: Send + Sync {
    /// `mime` is normalized to lowercase before dispatch.
    fn supports(&self, mime: &str) -> bool;

    /// Extracted text for the FTS5 `text` column. An [`ExtractError`] marks
    /// the file's content state failed (so it is not retried every run).
    /// Empty text is fine — filename search still works.
    fn extract(&self, path: &Path) -> Result<String, ExtractError>;

    /// Extract from bytes that are the file's *entire* contents, already in
    /// memory at walk time; keeps the text consistent with the size, mtime
    /// and hash read alongside it.
    ///
    /// The default `None` means "I need the file on disk" — formats that seek
    /// or read a trailer must keep it; `Some(Err(_))` is a real failure.
    /// `path` is only so failures name the file — nothing here may open it.
    fn extract_from_head(
        &self,
        _path: &Path,
        _head: &[u8],
    ) -> Option<Result<String, ExtractError>> {
        None
    }
}

/// An ordered dispatch table: the first extractor claiming the MIME wins.
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            extractors: Vec::new(),
        }
    }

    fn find(&self, mime: &str) -> Option<&dyn Extractor> {
        let lower = mime.to_ascii_lowercase();
        self.extractors
            .iter()
            .find(|e| e.supports(&lower))
            .map(|e| &**e)
    }

    /// Whether any extractor claims `mime`, without touching the file — lets
    /// the walk decide a row's `content_state` up front.
    pub fn supports(&self, mime: &str) -> bool {
        self.find(mime).is_some()
    }

    /// Run the handler for `mime` against `path`; `Ok(None)` if no extractor
    /// claims the MIME.
    ///
    /// A panicking parser becomes an `Err` here, at the boundary: every
    /// caller has more than one file to lose (a walk worker's panic costs the
    /// root its whole content pass; the live watcher's costs every displayed
    /// row for the session), and containing at the boundary means a new
    /// caller cannot forget. This cannot help with a stack overflow, which
    /// aborts rather than unwinding — see `vendor/pdf-extract`, which bounds
    /// the recursion that made that reachable.
    pub fn extract(
        &self,
        path: &Path,
        mime: &str,
    ) -> Result<Option<String>, ExtractError> {
        let Some(extractor) = self.find(mime) else {
            return Ok(None);
        };
        contain_panic(path, || extractor.extract(path))
            .and_then(|r| r)
            .map(Some)
    }

    /// [`Registry::extract`] for a file whose complete contents the caller
    /// already holds. `None` means "leave this to the content pass".
    /// Contained the same way — and this is the one on a walk worker.
    pub fn extract_complete_head(
        &self,
        path: &Path,
        mime: &str,
        head: &[u8],
    ) -> Option<Result<String, ExtractError>> {
        let extractor = self.find(mime)?;
        // The guard wraps the whole `Option` so a panic becomes
        // `Some(Err(..))` — a failure this file is charged with, not a
        // deferral to the content pass that would meet the same panic.
        match contain_panic(path, || extractor.extract_from_head(path, head)) {
            Ok(outcome) => outcome,
            Err(e) => Some(Err(e)),
        }
    }

    /// The default set. Order matters: RTF precedes plaintext, which claims
    /// every `text/*` and would swallow `text/rtf` as raw control words;
    /// plaintext precedes audio because it deliberately claims playlist and
    /// SVG MIMEs whose text is worth more than their tags. No image
    /// extractor, deliberately: `image/*` unclaimed is what records images
    /// `STATE_NA` at walk time and keeps the content pass from opening them.
    pub fn default_set() -> Self {
        Self {
            extractors: vec![
                Box::new(rtf::RtfExtractor),
                Box::new(plaintext::PlaintextExtractor),
                Box::new(office::OfficeExtractor),
                Box::new(pdf::PdfExtractor),
                Box::new(audio::AudioExtractor),
            ],
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::default_set()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_returns_none() {
        let r = Registry::new();
        let out = r
            .extract(Path::new("/tmp/x"), "text/plain")
            .expect("no error");
        assert!(out.is_none());
    }

    #[test]
    fn complete_head_extraction_dispatches_only_to_extractors_that_opt_in() {
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever");

        let out = r.extract_complete_head(p, "text/plain", b"hello");
        assert!(matches!(out, Some(Ok(ref c)) if c == "hello"));

        // A format that seeks or reads a trailer must not be handed a buffer.
        assert!(r
            .extract_complete_head(p, "application/pdf", b"%PDF-1.4")
            .is_none());
        assert!(r
            .extract_complete_head(p, "image/png", b"\x89PNG")
            .is_none());

        assert!(r
            .extract_complete_head(p, "application/x-nonesuch", b"..")
            .is_none());
    }

    #[test]
    fn complete_head_extraction_matches_the_on_disk_dispatch() {
        // Or a file's text would depend on which pass happened to handle it.
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever");
        for mime in [
            "text/plain",
            "TEXT/PLAIN",
            "application/json",
            "application/x-sql",
        ] {
            assert!(
                r.extract_complete_head(p, mime, b"x").is_some(),
                "{} should extract from a head",
                mime
            );
        }
        for mime in ["application/rtf", "text/rtf"] {
            assert!(
                r.extract_complete_head(p, mime, br"{\rtf1 x}").is_some(),
                "{} should extract from a head",
                mime
            );
        }
    }

    #[test]
    fn text_rtf_reaches_the_rtf_extractor_not_plaintext() {
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever.rtf");
        let out = r
            .extract_complete_head(p, "text/rtf", br"{\rtf1\ansi Hello {\b World}}")
            .expect("claimed")
            .expect("parsed");
        assert_eq!(out, "Hello World");
    }

    #[test]
    fn supports_agrees_with_extract_dispatch() {
        // The two must agree for every MIME, or the walk would write a
        // content state the content pass then contradicts. The path does not
        // exist, so a claimed MIME surfaces as `Err`, not `Ok(None)`.
        let r = Registry::default_set();
        let missing = Path::new("/nonexistent/quicksearch-supports-probe");
        for mime in [
            "text/plain",
            "TEXT/PLAIN",
            "text/x-rust",
            "application/json",
            "APPLICATION/PDF",
            "application/pdf",
            "audio/mpeg",
            "Image/JPEG",
            "application/msword",
            "application/vnd.oasis.opendocument.text",
            "video/mp4",
            "application/zip",
            "application/x-executable",
            "application/octet-stream",
            "",
        ] {
            let claimed = !matches!(r.extract(missing, mime), Ok(None));
            assert_eq!(
                r.supports(mime),
                claimed,
                "supports and extract disagree about {:?}",
                mime
            );
        }
    }

    #[test]
    fn images_are_not_claimed_by_any_extractor() {
        let r = Registry::default_set();
        for mime in ["image/jpeg", "image/png", "Image/JPEG", "image/tiff"] {
            assert!(!r.supports(mime), "{} should be unclaimed", mime);
        }
    }
}
