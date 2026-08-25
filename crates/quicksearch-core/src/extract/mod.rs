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
/// worker instead of being hardcoded per format.
///
/// Every extractor used to carry its own 64 MiB constants, chosen
/// independently of the settings that actually bound the work: the content
/// pass never offers a file above `maximum_text_file_size` (2 MiB by
/// default), and everything an extractor produces past `maximum_text_size`
/// (256 KiB) is discarded by the caller moments later. A ceiling 256× above
/// the largest result that can be kept is not a safety margin, it is the
/// worst case a worker can reach — multiplied by the pool size.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest file body to read. A backstop rather than a policy: the pass
    /// already filtered on it, so this catches a file that grew since the
    /// walk sized it and a node whose `fstat` lies (procfs reports zero).
    pub read: usize,
    /// Largest text an extraction may produce. Past this the caller
    /// truncates, so producing more is work and memory spent to be thrown
    /// away — extractors stop here instead.
    pub text: usize,
    /// Largest a single container member may inflate to. A zip declares its
    /// sizes but the deflate stream is what gets read, so this is the only
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
/// set to. The budget is held **per worker** and the pools multiply it (up
/// to 64 workers, one pool per root), so it cannot be allowed to scale with
/// a setting freely. This is the old hardcoded per-member cap, kept as the
/// backstop it was always meant to be rather than the everyday value.
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

/// Buffers one worker reuses for every file it handles.
///
/// A pool member creates one of these before its loop and hands it to each
/// extraction; the buffers keep whatever capacity the largest file so far
/// needed, so per-file allocator traffic for the *intermediates* falls to
/// zero. What it deliberately does not hold is the extracted text: that
/// crosses a channel to the writer, so it is the payload rather than
/// scratch.
///
/// Sizing is lazy on purpose. `maximum_text_file_size` clamps at 4 GiB
/// ([`crate::config::Config::clamp_out_of_range`]), so reserving it eagerly
/// would let a configured ceiling nobody reaches allocate per worker.
pub struct Scratch {
    /// The head bytes a walk worker hashes and sniffs each file from.
    head: Vec<u8>,
    /// One container member or one whole file, for a parser that will not
    /// take a reader. Grows to the largest member the worker has met and
    /// stays there, bounded by [`Limits::inflate`].
    bytes: Vec<u8>,
    /// quick-xml's per-event buffer. Separate from `bytes` because a
    /// container walk holds both at once.
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

    /// The head buffer, to be read into. Reused across files: the walk
    /// hashes the head of every new or changed file, and a fresh
    /// `hash_length` buffer apiece was one allocation per file. The caller
    /// sizes it — [`crate::file_handling::get_file_hash`] does.
    pub(crate) fn head_buffer(&mut self) -> &mut Vec<u8> {
        &mut self.head
    }

    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    /// The raw-bytes buffer: one container member, one OLE stream, or one
    /// whole file for a format whose parser will not take a reader. The
    /// caller clears it before filling.
    pub(crate) fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// The member and event buffers together, which a container walk holds
    /// at once. Two `&mut self` accessors could not be.
    pub(crate) fn container_bufs(&mut self) -> (&mut Vec<u8>, &mut Vec<u8>) {
        (&mut self.bytes, &mut self.events)
    }

    /// The same pair plus the shared-string table — XLSX needs all three.
    /// The table is cleared but its entries keep their capacity, which is
    /// most of what a shared-string table costs to rebuild.
    pub(crate) fn xlsx_bufs(&mut self) -> (&mut Vec<u8>, &mut Vec<u8>, &mut Vec<String>) {
        self.strings.clear();
        (&mut self.bytes, &mut self.events, &mut self.strings)
    }
}

/// Run `f`, turning a panic into an [`ExtractError`] naming the file. The
/// extractors drive third-party parsers over bytes chosen by whoever wrote
/// the file, and several are documented to panic on malformed input.
fn contain_panic<T>(path: &Path, f: impl FnOnce() -> T) -> Result<T, ExtractError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| format!("extractor panicked on {}", path.display()))
}

/// A pluggable content extractor; stateless.
///
/// Text is **appended to `out`** rather than returned. The caller owns that
/// buffer — it is the row that crosses the channel to the writer — so a
/// returned `String` was one allocation handed over and, for the container
/// formats, another one inside for the intermediate. `scratch` carries the
/// intermediates and the [`Limits`] the extraction works under.
pub trait Extractor: Send + Sync {
    /// `mime` is normalized to lowercase before dispatch.
    fn supports(&self, mime: &str) -> bool;

    /// Extract this file's searchable text into `out`. An [`ExtractError`]
    /// marks the file's content state failed (so it is not retried every
    /// run). Empty text is fine — filename search still works.
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
    /// The default `None` means "I need the file on disk" — formats that seek
    /// or read a trailer must keep it; `Some(Err(_))` is a real failure.
    /// `path` is only so failures name the file — nothing here may open it.
    ///
    /// No `Scratch`: `head` is itself the walk worker's reused buffer, so an
    /// extractor taking both would be holding two borrows of the same thing.
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
    pub fn new() -> Self {
        Self {
            extractors: Vec::new(),
        }
    }

    /// The lowercasing is a stack copy, not a heap one: `find` runs two or
    /// three times per indexed file (`content_extractable` alone calls it
    /// twice), and a `String` apiece was pure per-file allocator traffic.
    fn find(&self, mime: &str) -> Option<&dyn Extractor> {
        let lower = crate::mime::LowerMime::new(mime);
        let lower = lower.as_str(mime);
        self.extractors
            .iter()
            .find(|e| e.supports(lower))
            .map(|e| &**e)
    }

    /// Whether any extractor claims `mime`, without touching the file — lets
    /// the walk decide a row's `content_state` up front.
    pub fn supports(&self, mime: &str) -> bool {
        self.find(mime).is_some()
    }

    /// Run the handler for `mime` against `path`, appending its text to
    /// `out`. `Ok(false)` means no extractor claims the MIME and `out` was
    /// not touched.
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
    /// Contained the same way — and this is the one on a walk worker.
    pub fn extract_complete_head(
        &self,
        path: &Path,
        mime: &str,
        head: &[u8],
        out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        let extractor = self.find(mime)?;
        // The guard wraps the whole `Option` so a panic becomes
        // `Some(Err(..))` — a failure this file is charged with, not a
        // deferral to the content pass that would meet the same panic.
        match contain_panic(path, || extractor.extract_from_head(path, head, out)) {
            Ok(outcome) => outcome,
            Err(e) => Some(Err(e)),
        }
    }

    /// [`Registry::extract`] into a `String` of its own, under default
    /// limits.
    ///
    /// For callers holding **one** file — probes, tests, a CLI invocation —
    /// where there is no loop for a reused buffer to amortize over. Anything
    /// in a pool should own a [`Scratch`] and call [`Registry::extract`], or
    /// it pays the per-file allocations this exists to avoid.
    pub fn extract_to_string(
        &self,
        path: &Path,
        mime: &str,
        config: &crate::config::Config,
    ) -> Result<Option<String>, ExtractError> {
        let mut out = String::new();
        let mut scratch = Scratch::new(config);
        match self.extract(path, mime, &mut out, &mut scratch)? {
            true => Ok(Some(out)),
            false => Ok(None),
        }
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
        match self.extract_complete_head(path, mime, head, &mut out) {
            Some(Ok(())) => Some(Ok(out)),
            Some(Err(e)) => Some(Err(e)),
            None => None,
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

    fn cfg() -> crate::config::Config {
        crate::config::Config::default()
    }

    /// The one-file form; the pool's buffer reuse is not what these assert.
    fn extract(r: &Registry, path: &Path, mime: &str) -> Result<Option<String>, ExtractError> {
        r.extract_to_string(path, mime, &cfg())
    }

    fn extract_complete_head(
        r: &Registry,
        path: &Path,
        mime: &str,
        head: &[u8],
    ) -> Option<Result<String, ExtractError>> {
        r.extract_head_to_string(path, mime, head)
    }

    #[test]
    fn empty_registry_returns_none() {
        let r = Registry::new();
        let out = extract(&r, Path::new("/tmp/x"), "text/plain").expect("no error");
        assert!(out.is_none());
    }

    #[test]
    fn complete_head_extraction_dispatches_only_to_extractors_that_opt_in() {
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever");

        let out = extract_complete_head(&r, p, "text/plain", b"hello");
        assert!(matches!(out, Some(Ok(ref c)) if c == "hello"));

        // A format that seeks or reads a trailer must not be handed a buffer.
        assert!(extract_complete_head(&r, p, "application/pdf", b"%PDF-1.4").is_none());
        assert!(extract_complete_head(&r, p, "image/png", b"\x89PNG").is_none());

        assert!(extract_complete_head(&r, p, "application/x-nonesuch", b"..").is_none());
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
                extract_complete_head(&r, p, mime, b"x").is_some(),
                "{} should extract from a head",
                mime
            );
        }
        for mime in ["application/rtf", "text/rtf"] {
            assert!(
                extract_complete_head(&r, p, mime, br"{\rtf1 x}").is_some(),
                "{} should extract from a head",
                mime
            );
        }
    }

    #[test]
    fn text_rtf_reaches_the_rtf_extractor_not_plaintext() {
        let r = Registry::default_set();
        let p = Path::new("/tmp/whatever.rtf");
        let out = extract_complete_head(&r, p, "text/rtf", br"{\rtf1\ansi Hello {\b World}}")
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
            let claimed = !matches!(extract(&r, missing, mime), Ok(None));
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
