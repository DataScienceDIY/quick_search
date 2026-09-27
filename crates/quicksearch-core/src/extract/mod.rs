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

/// The ceilings one extraction works under, read from the config once per
/// worker. They follow the settings that already bound the work: the content
/// pass never offers a file above `maximum_text_file_size`, and the caller
/// discards text past `maximum_text_size`, so any higher ceiling would only
/// raise the worst case a worker, times the pool size, can reach.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest file body to read. A backstop: the pass already filtered on
    /// it, so this catches a file that grew since the walk sized it and a
    /// node whose `fstat` lies (procfs reports zero).
    pub read: usize,
    /// Largest text an extraction may produce. The caller truncates past
    /// this, so extractors stop here.
    pub text: usize,
    /// Largest a single container member may inflate to. A zip declares its
    /// sizes, but reading follows the deflate stream, so this is the only
    /// bound on a crafted archive.
    pub inflate: usize,
}

/// Headroom between the text kept and the markup carrying it. A document
/// whose *extracted text* is `maximum_text_size` arrives as several times
/// that in XML — runs, properties and namespaces — but not sixteen times,
/// and the early stop at [`Limits::text`] means the whole budget is reached
/// only by an archive built to reach it.
const INFLATE_FACTOR: usize = 16;

/// Absolute ceiling on the inflation budget, whatever `maximum_text_size` is
/// set to. The budget is held per worker and the pools multiply it (up to 64
/// workers, one pool per root), so it cannot scale freely with a setting.
const MAX_INFLATE: usize = 64 * 1024 * 1024;

impl Limits {
    pub fn for_config(config: &crate::config::Config) -> Limits {
        let text = config.processing.maximum_text_size.max(1);
        Limits {
            read: usize::try_from(config.processing.maximum_text_file_size).unwrap_or(usize::MAX),
            text,
            inflate: text.saturating_mul(INFLATE_FACTOR).min(MAX_INFLATE),
        }
    }
}

/// Buffers one worker reuses for every file it handles. Each keeps the
/// capacity the largest file so far needed, so intermediates cost no per-file
/// allocation. The extracted text is not here: it crosses a channel to the
/// writer.
///
/// Sized lazily: `maximum_text_file_size` clamps at 4 GiB
/// ([`crate::config::Config::clamp_out_of_range`]), so reserving it eagerly
/// would let a ceiling nobody reaches allocate per worker.
pub struct Scratch {
    /// The head bytes a walk worker hashes and sniffs each file from.
    head: Vec<u8>,
    /// One whole file, for a parser that will not take a reader. Grows to
    /// the largest file the worker has met and stays there.
    bytes: Vec<u8>,
    /// quick-xml's per-event buffer.
    events: Vec<u8>,
    /// XLSX's shared-string table. Cleared between files; the entries keep
    /// their capacity, which is most of what a table costs.
    strings: Vec<String>,
    limits: Limits,
}

impl Scratch {
    pub fn new(config: &crate::config::Config) -> Scratch {
        Scratch {
            head: Vec::new(),
            bytes: Vec::new(),
            events: Vec::new(),
            strings: Vec::new(),
            limits: Limits::for_config(config),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The head buffer, to be read into, reused across files. The caller
    /// sizes it ([`crate::file_handling::get_file_hash`] does).
    pub(crate) fn head_buffer(&mut self) -> &mut Vec<u8> {
        &mut self.head
    }

    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    /// The raw-bytes buffer: one whole file for a format whose parser will
    /// not take a reader. The caller clears it before filling.
    pub(crate) fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// quick-xml's event buffer, for a container's XML members.
    pub(crate) fn events(&mut self) -> &mut Vec<u8> {
        &mut self.events
    }

    /// The event buffer plus the shared-string table, cleared, which XLSX
    /// needs at once; two `&mut self` accessors could not lend both.
    pub(crate) fn xlsx_bufs(&mut self) -> (&mut Vec<u8>, &mut Vec<String>) {
        self.strings.clear();
        (&mut self.events, &mut self.strings)
    }
}

/// Run `f`, turning a panic into an [`ExtractError`] naming the file. The
/// extractors drive third-party parsers over bytes chosen by whoever wrote
/// the file, and several are documented to panic on malformed input.
fn contain_panic<T>(path: &Path, f: impl FnOnce() -> T) -> Result<T, ExtractError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| format!("extractor panicked on {}", path.display()))
}

/// Append `text` to `out`, moving it rather than copying when `out` is empty,
/// as it is for every caller today.
fn append(out: &mut String, text: String) {
    if out.is_empty() {
        *out = text;
    } else {
        out.push_str(&text);
    }
}

/// A stateless content extractor. Text is appended to `out`, the caller's
/// buffer, which becomes the row sent to the writer; `scratch` holds reusable
/// intermediates and the [`Limits`].
pub trait Extractor: Send + Sync {
    /// `mime` is normalized to lowercase before dispatch.
    fn supports(&self, mime: &str) -> bool;

    /// Extract this file's searchable text into `out`. An [`ExtractError`]
    /// marks the file's content state failed (so it is not retried every
    /// run). Empty text is fine: filename search still works.
    ///
    /// An extractor should stop once `out` reaches `scratch.limits().text`:
    /// the caller truncates there, so anything beyond is produced to be
    /// discarded.
    fn extract(
        &self,
        path: &Path,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError>;

    /// Extract from bytes that are the file's *entire* contents, already in
    /// memory at walk time; keeps the text consistent with the size, mtime
    /// and hash read alongside it.
    ///
    /// The default `None` means "I need the file on disk", which formats that
    /// seek or read a trailer must keep; `Some(Err(_))` is a real failure.
    /// `path` only names the file in failures; nothing here may open it.
    ///
    /// No `Scratch`: `head` is itself the walk worker's reused buffer, so an
    /// extractor taking both would hold two borrows of the same thing.
    fn extract_from_head(
        &self,
        _path: &Path,
        _head: &[u8],
        _out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        None
    }
}

/// An ordered dispatch table: the first extractor claiming the MIME wins.
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl Registry {
    /// The first extractor claiming `mime`, matched case-insensitively.
    /// Lowercases on the stack: this runs several times per indexed file.
    fn find(&self, mime: &str) -> Option<&dyn Extractor> {
        let lower = crate::mime::LowerMime::new(mime);
        let lower = lower.as_str();
        self.extractors
            .iter()
            .find(|e| e.supports(lower))
            .map(|e| &**e)
    }

    /// Whether any extractor claims `mime`, without touching the file, so the
    /// walk can decide a row's `content_state` up front.
    pub fn supports(&self, mime: &str) -> bool {
        self.find(mime).is_some()
    }

    /// Run the handler for `mime` against `path`, appending its text to
    /// `out`. `Ok(false)` means no extractor claims the MIME and `out` is
    /// untouched.
    ///
    /// A panicking parser becomes an `Err` here, at the boundary, so no caller
    /// can forget to contain it: a walk worker's panic would cost the root its
    /// content pass, the live watcher's every displayed row. A stack overflow
    /// aborts rather than unwinding; `vendor/pdf-extract` bounds the recursion
    /// that made one reachable.
    pub fn extract(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<bool, ExtractError> {
        let Some(extractor) = self.find(mime) else {
            return Ok(false);
        };
        contain_panic(path, || extractor.extract(path, out, scratch))
            .and_then(|r| r)
            .map(|()| true)
    }

    /// [`Registry::extract`] for a file whose complete contents the caller
    /// already holds. `None` means "leave this to the content pass".
    /// Contained the same way; this one runs on a walk worker.
    pub fn extract_complete_head(
        &self,
        path: &Path,
        mime: &str,
        head: &[u8],
        out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        let extractor = self.find(mime)?;
        // A panic becomes `Some(Err(..))`: a failure charged to this file, not
        // a deferral to the content pass, which would meet the same panic.
        contain_panic(path, || extractor.extract_from_head(path, head, out))
            .unwrap_or_else(|e| Some(Err(e)))
    }

    /// [`Registry::extract`] into a `String` of its own, under `config`'s
    /// limits. For callers holding one file (probes, tests, a CLI
    /// invocation); anything in a pool should own a [`Scratch`] and call
    /// [`Registry::extract`], or it pays per-file allocations.
    pub fn extract_to_string(
        &self,
        path: &Path,
        mime: &str,
        config: &crate::config::Config,
    ) -> Result<Option<String>, ExtractError> {
        let mut out = String::new();
        let mut scratch = Scratch::new(config);
        Ok(self
            .extract(path, mime, &mut out, &mut scratch)?
            .then_some(out))
    }

    /// [`Registry::extract_complete_head`] into a `String` of its own; see
    /// [`Registry::extract_to_string`] for when to reach for it.
    pub fn extract_head_to_string(
        &self,
        path: &Path,
        mime: &str,
        head: &[u8],
    ) -> Option<Result<String, ExtractError>> {
        let mut out = String::new();
        self.extract_complete_head(path, mime, head, &mut out)
            .map(|r| r.map(|()| out))
    }

    /// The default set. Order matters: RTF precedes plaintext, which claims
    /// every `text/*` and would swallow `text/rtf` as raw control words;
    /// plaintext precedes audio because it claims playlist and SVG MIMEs
    /// whose text is worth more than their tags. There is no image extractor:
    /// leaving `image/*` unclaimed records images `STATE_NA` at walk time and
    /// keeps the content pass from opening them.
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

/// Run `e` on `path` into a fresh `String` under default limits: the
/// one-file form the extractor tests assert text through.
#[cfg(test)]
fn extract_text(e: &dyn Extractor, path: &Path) -> Result<String, ExtractError> {
    let mut out = String::new();
    let mut scratch = Scratch::new(&crate::config::Config::default());
    e.extract(path, &mut out, &mut scratch).map(|()| out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_head_extraction_dispatches_only_to_extractors_that_opt_in() {
        let r = Registry::default_set();
        let head =
            |mime, bytes: &[u8]| r.extract_head_to_string(Path::new("/tmp/whatever"), mime, bytes);

        assert!(matches!(head("text/plain", b"hello"), Some(Ok(ref c)) if c == "hello"));

        // A format that seeks or reads a trailer must not be handed a buffer.
        assert!(head("application/pdf", b"%PDF-1.4").is_none());
        assert!(head("image/png", b"\x89PNG").is_none());

        assert!(head("application/x-nonesuch", b"..").is_none());
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
                r.extract_head_to_string(p, mime, b"x").is_some(),
                "{} should extract from a head",
                mime
            );
        }
        for mime in ["application/rtf", "text/rtf"] {
            assert!(
                r.extract_head_to_string(p, mime, br"{\rtf1 x}").is_some(),
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
            .extract_head_to_string(p, "text/rtf", br"{\rtf1\ansi Hello {\b World}}")
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
            let claimed = !matches!(
                r.extract_to_string(missing, mime, &crate::config::Config::default()),
                Ok(None)
            );
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
