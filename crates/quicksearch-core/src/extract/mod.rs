//! Content extractors: text plus structured properties (title, artist, EXIF, …).
//!
//! An [`Extractor`] decides whether it can handle a given MIME type and, if
//! so, produces [`ExtractedContent`] for the file. The [`Registry`] picks the
//! first registered extractor that accepts the MIME and runs it.
//!
//! Dispatch is by MIME only — a file with no detected type is recorded as
//! "not applicable" rather than guessed at again here. Extensions that
//! `mime_guess` misses or mistypes are corrected upstream instead, in
//! [`crate::mime::guess_mime_from_head`], so there is one place where "what is
//! this file" gets decided, and it is decided once: the walk sniffs the head it
//! already read, stores the answer, and nothing downstream reopens the file to
//! ask again.

use std::collections::HashMap;
use std::path::Path;

pub mod audio;
pub mod image;
pub mod office;
pub mod pdf;
pub mod plaintext;
pub mod rtf;

/// Result of a successful extraction. `text` feeds the FTS5 `text` column;
/// `properties` feeds both the `properties` FTS5 column (as `key:value`
/// tokens) and the structured `properties` table for later retrieval.
///
/// Extractors may return an empty `text` when the file has no narrative
/// content (e.g. an image where only EXIF matters). Filename search still
/// works in that case.
#[derive(Debug, Default, Clone)]
pub struct ExtractedContent {
    pub text: String,
    pub properties: HashMap<String, String>,
}

impl ExtractedContent {
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            properties: HashMap::new(),
        }
    }

    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Convert properties into the `Vec<(String, String)>` shape expected by
    /// [`crate::db::repo::set_content_done`]. Keys are sorted for determinism
    /// in tests and snapshots.
    pub fn properties_sorted(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

/// Boxed error type for extractor failures. A string reason is stored on the
/// file row (see [`crate::db::repo::set_content_failed`]), so extractors
/// should surface human-readable messages.
pub type ExtractError = String;

/// A pluggable content extractor. Stateless; implementors should not hold
/// file handles across calls.
pub trait Extractor: Send + Sync {
    /// Whether this extractor can handle the given MIME type. `mime` is
    /// normalized to lowercase before dispatch.
    fn supports(&self, mime: &str) -> bool;

    /// Read the file at `path` and return its extracted content. Return an
    /// [`ExtractError`] to mark the file's content state as failed (so it
    /// won't be retried every run).
    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError>;

    /// Extract from bytes the caller already holds, when those bytes are the
    /// file's *entire* contents.
    ///
    /// Indexing hashes the head of every new or changed file, so for anything
    /// no larger than `hash_length` the whole file is already in memory by the
    /// time the walk classifies it. An extractor that can work from that buffer
    /// saves the content pass an open/read/close — several round trips per
    /// file on a network share — and closes a consistency gap, because the
    /// text then comes from the same `read` as the size, mtime and hash stored
    /// alongside it.
    ///
    /// The default is `None`: "I need the file on disk." Formats that seek,
    /// or that read a central directory at the end of the file, must keep it.
    /// Returning `Some(Err(_))` is a real extraction failure, recorded like
    /// any other; returning `None` simply defers to [`Extractor::extract`].
    ///
    /// `path` is passed only so failures name the same file the on-disk path
    /// would — nothing here may open it.
    fn extract_from_head(
        &self,
        _path: &Path,
        _head: &[u8],
    ) -> Option<Result<ExtractedContent, ExtractError>> {
        None
    }
}

/// An ordered dispatch table of extractors. The first extractor whose
/// [`supports`](Extractor::supports) returns true for the MIME is used.
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { extractors: Vec::new() }
    }

    pub fn with(mut self, e: impl Extractor + 'static) -> Self {
        self.extractors.push(Box::new(e));
        self
    }

    /// The extractor that claims `mime`, if any. The one place dispatch
    /// happens, so every question about a MIME — "who handles it", "does
    /// anyone handle it", "handle it from these bytes" — gets the same answer.
    fn find(&self, mime: &str) -> Option<&dyn Extractor> {
        let lower = mime.to_ascii_lowercase();
        self.extractors
            .iter()
            .find(|e| e.supports(&lower))
            .map(|e| &**e)
    }

    /// Whether any extractor claims `mime` — the same question
    /// [`Registry::extract`] answers by returning `Ok(None)`, asked without
    /// touching the file. This is what lets the walk decide a row's
    /// `content_state` up front instead of queueing it for a worker that will
    /// only mark it not-applicable (see
    /// [`crate::file_handling::content_extractable`]).
    pub fn supports(&self, mime: &str) -> bool {
        self.find(mime).is_some()
    }

    /// Look up a handler for `mime` and run it against `path`. Returns
    /// `Ok(None)` if no extractor claims the MIME — the caller should then
    /// decide whether the file is "not applicable" (text state NA).
    pub fn extract(
        &self,
        path: &Path,
        mime: &str,
    ) -> Result<Option<ExtractedContent>, ExtractError> {
        self.find(mime).map(|e| e.extract(path)).transpose()
    }

    /// [`Registry::extract`] for a file whose complete contents the caller
    /// already holds. `Ok(None)` when no extractor claims the MIME, and
    /// `None` when the one that does needs the file on disk after all —
    /// both mean "leave this to the content pass".
    ///
    /// Dispatch stays here rather than at the call site so there is still
    /// exactly one place that decides what an extractor sees for a given MIME.
    pub fn extract_complete_head(
        &self,
        path: &Path,
        mime: &str,
        head: &[u8],
    ) -> Option<Result<ExtractedContent, ExtractError>> {
        self.find(mime).and_then(|e| e.extract_from_head(path, head))
    }

    /// The default set: RTF, plaintext, office docs, PDF, audio tags,
    /// image EXIF.
    ///
    /// Order matters — the first extractor whose `supports` accepts a MIME
    /// wins. RTF precedes plaintext because plaintext claims every `text/*`
    /// and would swallow `text/rtf` as raw control words. Plaintext
    /// precedes audio and image because it deliberately claims playlist
    /// (`audio/x-mpegurl`, `audio/scpls`) and SVG MIMEs whose text is worth
    /// more than their tags.
    pub fn default_set() -> Self {
        Self::new()
            .with(rtf::RtfExtractor)
            .with(plaintext::PlaintextExtractor)
            .with(office::OfficeExtractor)
            .with(pdf::PdfExtractor)
            .with(audio::AudioExtractor)
            .with(image::ImageExtractor)
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

        // Plaintext opts in, so a small text file never reaches the disk pass.
        let out = r.extract_complete_head(p, "text/plain", b"hello");
        assert!(matches!(out, Some(Ok(ref c)) if c.text == "hello"));

        // A format that seeks or reads a trailer must not be handed a buffer.
        // `None` here is what routes it back to the on-disk extractor.
        assert!(r.extract_complete_head(p, "application/pdf", b"%PDF-1.4").is_none());
        assert!(r.extract_complete_head(p, "image/png", b"\x89PNG").is_none());

        // No extractor claims the MIME at all.
        assert!(r.extract_complete_head(p, "application/x-nonesuch", b"..").is_none());
    }

    #[test]
    fn complete_head_extraction_matches_the_on_disk_dispatch() {
        // Both entry points must pick the same extractor for a MIME, or a
        // file's text would depend on which pass happened to handle it.
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever");
        for mime in ["text/plain", "TEXT/PLAIN", "application/json", "application/x-sql"] {
            assert!(
                r.extract_complete_head(p, mime, b"x").is_some(),
                "{} should extract from a head", mime
            );
        }
        for mime in ["application/rtf", "text/rtf"] {
            assert!(
                r.extract_complete_head(p, mime, br"{\rtf1 x}").is_some(),
                "{} should extract from a head", mime
            );
        }
    }

    /// `text/rtf` must dispatch to the RTF extractor, not to plaintext's
    /// `text/*` claim — i.e. the registration order does its job. The RTF
    /// parser strips control words; plaintext would keep them.
    #[test]
    fn text_rtf_reaches_the_rtf_extractor_not_plaintext() {
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever.rtf");
        let out = r
            .extract_complete_head(p, "text/rtf", br"{\rtf1\ansi Hello {\b World}}")
            .expect("claimed")
            .expect("parsed");
        assert_eq!(out.text, "Hello World");
    }

    #[test]
    fn supports_agrees_with_extract_dispatch() {
        // `supports` is the cheap form of the question `extract` answers with
        // `Ok(None)`. They must agree for every MIME, or the walk would write
        // a content state the content pass then contradicts. The path does not
        // exist, so a claimed MIME surfaces as `Err`, not `Ok(None)` — which is
        // exactly the distinction under test.
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
            // Real MIMEs with no extractor: the population the fix is about.
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
    fn properties_sorted_is_deterministic() {
        let c = ExtractedContent::with_text("hi")
            .with_property("b", "2")
            .with_property("a", "1");
        assert_eq!(
            c.properties_sorted(),
            vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]
        );
    }
}
