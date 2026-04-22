//! Content extractors: text plus structured properties (title, artist, EXIF, …).
//!
//! An [`Extractor`] decides whether it can handle a given MIME type and, if
//! so, produces [`ExtractedContent`] for the file. The [`Registry`] picks the
//! first registered extractor that accepts the MIME and runs it. Callers can
//! also fall back to an extension-based match for files with no detected MIME.

use std::collections::HashMap;
use std::path::Path;

pub mod audio;
pub mod image;
pub mod office;
pub mod pdf;
pub mod plaintext;

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

    /// Look up a handler for `mime` and run it against `path`. Returns
    /// `Ok(None)` if no extractor claims the MIME — the caller should then
    /// decide whether the file is "not applicable" (text state NA).
    pub fn extract(
        &self,
        path: &Path,
        mime: &str,
    ) -> Result<Option<ExtractedContent>, ExtractError> {
        let lower = mime.to_ascii_lowercase();
        for e in &self.extractors {
            if e.supports(&lower) {
                return e.extract(path).map(Some);
            }
        }
        Ok(None)
    }

    /// The default set wired up for Set A: plaintext, office docs, PDF,
    /// audio tags, image EXIF.
    pub fn default_set() -> Self {
        Self::new()
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
