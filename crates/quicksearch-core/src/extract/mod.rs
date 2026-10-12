//! Content extractors: the searchable text of a file. The [`Registry`] runs
//! the first registered [`Extractor`] that accepts the MIME.
//!
//! The registry dispatches by MIME, decided once upstream in
//! [`crate::mime::guess_mime_from_head`].

use std::fs::{File, Metadata};
use std::path::Path;

pub mod audio;
pub mod office;
pub mod ole;
pub mod pdf;
pub mod plaintext;
pub mod rtf;
pub mod sandbox;

/// The string reason is stored on the file row, so extractors should surface
/// human-readable messages.
pub type ExtractError = String;

/// The error of an extraction a stop cut short
/// ([`sandbox::kill_sandbox_children`]), or whose sandbox child could not be
/// spawned or reaped: not a verdict on the file, so its row stays pending
/// ([`crate::file_handling::ContentOutcome::Cancelled`]). No extractor's own
/// failure reads like this, in a sandbox child or out of one.
pub const CANCELLED: &str = "extraction cancelled by a stop";

/// The ceilings one extraction works under, read from the config once per
/// worker. They follow the settings that already bound the work: the content
/// pass never offers a file above `maximum_text_file_size`, and the caller
/// discards text past `maximum_text_size`, so any higher ceiling would only
/// raise the worst case a worker, times the pool size, can reach.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest file body an extractor opens: [`open_for_extract`] refuses a
    /// longer file, and plaintext, RTF and PDF read through `take(read)`,
    /// which catches a file that grew after the check or whose `fstat` lies
    /// (procfs reports zero).
    pub read: usize,
    /// Largest text an extraction may produce. The caller truncates past
    /// this, so extractors stop here.
    pub text: usize,
    /// Largest a single zip member (OOXML, ODF) may inflate to. A zip
    /// declares its sizes, but reading follows the deflate stream, so this
    /// and [`Limits::container`] are the only bounds on a crafted archive.
    /// Reaching it truncates the member, keeping the text read before the
    /// cut, as reaching [`Limits::text`] does. It also bounds an `.xls`
    /// shared-string table, which is cut there the same way. PDF streams and
    /// the other OLE streams are not bounded by it; their formats parse in
    /// the [`sandbox`], whose child's memory limit is what bounds them.
    pub inflate: usize,
    /// Largest a whole zip container may inflate to across every member it
    /// reads: a deck's slides and notes, a workbook's sheets and its
    /// shared-string table, a document's body and notes. Each member opened
    /// is charged its compressed size on top, at least a fixed fee, so a
    /// container of empty members, or of central-directory entries naming one
    /// body many times, is bounded too, however little that body inflates to.
    /// Reaching it stops the extraction as [`Limits::text`] does: nothing
    /// checks the stop flag inside a file, so this bounds how long one
    /// archive can hold a worker.
    pub container: usize,
}

/// Headroom between the text kept and the markup carrying it. Prose arrives
/// as several times its text in XML (runs, properties and namespaces), but a
/// sheet of numbers carries tens of bytes of markup per byte of text, and a
/// slide of pictures is markup alone, so real documents do reach the cap;
/// they are truncated there, not failed.
const INFLATE_FACTOR: usize = 16;

/// Absolute ceiling on [`Limits::inflate`], whatever `maximum_text_size` is
/// set to, and the whole of [`Limits::container`]. Members are streamed, not
/// held: what a worker holds is quick-xml's event buffer, which grows to the
/// longest event (a text run, a tag), up to this. The pools multiply that
/// (up to 64 workers, one pool per root), so it cannot scale freely with a
/// setting.
const MAX_INFLATE: usize = 64 * 1024 * 1024;

impl Limits {
    pub fn for_config(config: &crate::config::Config) -> Limits {
        Limits::new(
            usize::try_from(config.processing.maximum_text_file_size).unwrap_or(usize::MAX),
            config.processing.maximum_text_size,
        )
    }

    /// The limits for a `read` and a `text` ceiling; the rest follow from
    /// `text`, which is how a sandbox child rebuilds its parent's from two
    /// arguments.
    fn new(read: usize, text: usize) -> Limits {
        let text = text.max(1);
        Limits {
            read,
            text,
            inflate: text.saturating_mul(INFLATE_FACTOR).min(MAX_INFLATE),
            container: MAX_INFLATE,
        }
    }
}

/// Buffers one worker reuses for every file it handles. Each keeps the
/// capacity the largest file so far needed. The extracted text is not here:
/// it crosses a channel to the writer.
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
    /// XLSX's shared-string table, cleared between files (only the slot
    /// array is kept).
    strings: Vec<String>,
    limits: Limits,
    /// Read when the scratch was made: a sandbox child spawned through it
    /// dies at once if a kill ran since ([`sandbox::KillMark`]).
    kills: sandbox::KillMark,
}

impl Scratch {
    /// Made before the owner's loop and its first stop check, which is what
    /// lets a stop reach its sandbox children ([`sandbox::KillMark`]).
    pub fn new(config: &crate::config::Config) -> Scratch {
        Scratch::since(config, sandbox::KillMark::now())
    }

    /// [`Scratch::new`] under a mark read earlier, before a stop check that
    /// came before this scratch.
    pub fn since(config: &crate::config::Config, kills: sandbox::KillMark) -> Scratch {
        Scratch::with_limits(Limits::for_config(config), kills)
    }

    fn with_limits(limits: Limits, kills: sandbox::KillMark) -> Scratch {
        Scratch {
            head: Vec::new(),
            bytes: Vec::new(),
            events: Vec::new(),
            strings: Vec::new(),
            limits,
            kills,
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

/// Run `f`, turning a panic into an [`ExtractError`] naming the file and
/// carrying the panic's message. The extractors drive third-party parsers
/// over bytes chosen by whoever wrote the file, and several are documented
/// to panic on malformed input.
fn contain_panic<T>(path: &Path, f: impl FnOnce() -> T) -> Result<T, ExtractError> {
    crate::contained(f).map_err(|m| format!("extractor panicked on {}: {}", path.display(), m))
}

/// Open `path` for an extractor: a regular file no longer than `read`
/// ([`Limits::read`]), or an error before a byte is read. A FIFO, a device,
/// or a link to one is refused without blocking
/// ([`crate::platform::open_regular_file`]), so a pending file swapped for a
/// link to `/dev/zero` cannot hold a worker. The `Metadata` is the opened
/// file's own.
pub(crate) fn open_for_extract(path: &Path, read: usize) -> Result<(File, Metadata), ExtractError> {
    let (file, meta) = crate::platform::open_regular_file(path)
        .map_err(|e| format!("open {}: {}", path.display(), e))?;
    if meta.len() > read as u64 {
        return Err(format!(
            "{} is {} bytes, over the {}-byte read limit",
            path.display(),
            meta.len(),
            read
        ));
    }
    Ok((file, meta))
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
    /// `mime` is the one [`Extractor::supports`] accepted, lowercase. The
    /// file is opened through [`open_for_extract`].
    ///
    /// An extractor should stop once `out` reaches `scratch.limits().text`:
    /// the caller truncates there, so anything beyond is produced to be
    /// discarded.
    fn extract(
        &self,
        path: &Path,
        mime: &str,
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

/// Where a registered extractor parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Isolation {
    /// In the caller's thread, under the registry's panic guard: first-party
    /// parsers that bound their own work.
    InProcess,
    /// In a [`sandbox`] child once the binary enables one, in-process until
    /// then: formats parsed mostly by third-party code that can abort the
    /// process.
    Sandboxed,
}

/// An ordered dispatch table: the first extractor claiming the MIME wins,
/// and parses where the table says.
pub struct Registry {
    extractors: Vec<(Box<dyn Extractor>, Isolation)>,
}

impl Registry {
    /// The first extractor claiming `lower`, a lowercased MIME. Callers fold
    /// with [`crate::mime::LowerMime`], on the stack: this runs several times
    /// per indexed file.
    fn find(&self, lower: &str) -> Option<(&dyn Extractor, Isolation)> {
        self.extractors
            .iter()
            .find(|(e, _)| e.supports(lower))
            .map(|(e, isolation)| (&**e, *isolation))
    }

    /// Whether any extractor claims `mime`, matched case-insensitively,
    /// without touching the file, so the walk can decide a row's
    /// `content_state` up front.
    pub fn supports(&self, mime: &str) -> bool {
        self.find(crate::mime::LowerMime::new(mime).as_str())
            .is_some()
    }

    /// Run the handler for `mime` against `path`, appending its text to
    /// `out`. `Ok(false)` means no extractor claims the MIME and `out` is
    /// untouched.
    ///
    /// A panicking parser becomes an `Err` here, at the boundary, so no caller
    /// can forget to contain it: a walk worker's panic would cost the root its
    /// content pass, the live watcher's every displayed row. A stack overflow
    /// or a failed allocation aborts rather than unwinding: under
    /// [`sandbox::enable_sandbox`] it aborts a sandboxed format's child,
    /// which records the file Failed.
    pub fn extract(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<bool, ExtractError> {
        self.extract_where(path, mime, out, scratch, sandbox::sandbox_exe())
    }

    /// [`Registry::extract`] in this process, whatever the sandbox: what a
    /// sandbox child runs, and what its text is compared with in tests.
    pub fn extract_in_process(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<bool, ExtractError> {
        self.extract_where(path, mime, out, scratch, None)
    }

    /// [`Registry::extract`], parsing a sandboxed format in a child of
    /// `sandbox` when there is one.
    fn extract_where(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
        sandbox: Option<&Path>,
    ) -> Result<bool, ExtractError> {
        let lower = crate::mime::LowerMime::new(mime);
        let mime = lower.as_str();
        let Some((extractor, isolation)) = self.find(mime) else {
            return Ok(false);
        };
        match sandbox.filter(|_| isolation == Isolation::Sandboxed) {
            Some(exe) => append(
                out,
                sandbox::extract_in_child(exe, path, mime, scratch.limits, scratch.kills)?,
            ),
            None => contain_panic(path, || extractor.extract(path, mime, out, scratch))??,
        }
        Ok(true)
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
        let (extractor, isolation) = self.find(crate::mime::LowerMime::new(mime).as_str())?;
        // A sandboxed format leaves even a complete head to the content
        // pass, so an extractor opting in later cannot bypass the child.
        if isolation == Isolation::Sandboxed {
            return None;
        }
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
    ///
    /// Sandboxed: PDF (lopdf), audio (lofty) and legacy Office (cfb), the
    /// formats whose parsing is mostly third-party code that can overflow
    /// the stack or allocate without bound ([`sandbox`]). The rest are
    /// first-party, budgeted parsers. A sandboxed file costs a process
    /// spawn, about 13 ms.
    pub fn default_set() -> Self {
        use Isolation::{InProcess, Sandboxed};
        Self {
            extractors: vec![
                (Box::new(rtf::RtfExtractor), InProcess),
                (Box::new(plaintext::PlaintextExtractor), InProcess),
                (Box::new(office::OfficeExtractor), InProcess),
                (Box::new(ole::OleExtractor), Sandboxed),
                (Box::new(pdf::PdfExtractor), Sandboxed),
                (Box::new(audio::AudioExtractor), Sandboxed),
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
/// one-file form the extractor tests assert text through. No MIME: only the
/// office and OLE extractors read one, and their tests call their parsers.
#[cfg(test)]
fn extract_text(e: &dyn Extractor, path: &Path) -> Result<String, ExtractError> {
    let mut out = String::new();
    let mut scratch = Scratch::new(&crate::config::Config::default());
    e.extract(path, "", &mut out, &mut scratch).map(|()| out)
}

/// How long one extraction may run on [`on_worker`] before its test fails as
/// a hang. The PDF budget tests spend the whole work budget, about 8 s an
/// extraction in a debug build, more on a loaded machine.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `f` on a [`crate::platform::spawn_worker`] thread, the stack the
/// indexer extracts on, failing if it has not finished within [`WATCHDOG`]:
/// a hang fails its test instead of wedging the run, and its thread is
/// abandoned. For the extractor tests that feed hostile input. Public and
/// `#[doc(hidden)]` rather than `#[cfg(test)]`, as [`crate::testutil`] is:
/// `tests/extract_fuzz.rs` is a separate compilation unit.
#[doc(hidden)]
pub fn on_worker<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    use std::sync::mpsc::RecvTimeoutError;

    let (tx, rx) = std::sync::mpsc::channel();
    crate::platform::spawn_worker("qs-extract-test", move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(WATCHDOG) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => panic!("still extracting after {:?}", WATCHDOG),
        Err(RecvTimeoutError::Disconnected) => panic!("the extraction worker panicked"),
    }
}

/// Raw deflate data inflating to `body` (at most 64 KiB) behind `padding`
/// empty stored blocks: 5 bytes of input each that inflate to nothing, the
/// shape of a stream whose encoded size dwarfs what it decodes to.
#[cfg(test)]
fn padded_deflate(body: &[u8], padding: usize) -> Vec<u8> {
    let len = u16::try_from(body.len()).expect("one stored block");
    // A stored block: a byte holding BFINAL and BTYPE 00, then LEN and its
    // complement, then LEN bytes.
    let mut data = [0, 0, 0, 0xFF, 0xFF].repeat(padding);
    data.push(1);
    data.extend_from_slice(&len.to_le_bytes());
    data.extend_from_slice(&(!len).to_le_bytes());
    data.extend_from_slice(body);
    data
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

    /// A parser's panic is a failure that names the file and says what went
    /// wrong.
    #[test]
    fn a_contained_panic_names_the_file_and_carries_the_message() {
        let got: Result<(), _> = contain_panic(Path::new("/x/bad.pdf"), || panic!("bad xref"));
        assert_eq!(
            got.unwrap_err(),
            "extractor panicked on /x/bad.pdf: bad xref"
        );
    }

    /// One MIME per extractor that opens the file, the two office parsers'
    /// containers apart.
    const OPENING: [&str; 6] = [
        "text/plain",
        "application/rtf",
        "application/pdf",
        "application/msword",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "audio/mpeg",
    ];

    /// `path` through the registry as `mime` under `config`'s limits, on a
    /// watched worker: an open or a read that blocks fails the test.
    fn extract_watched(
        path: &Path,
        mime: &'static str,
        config: crate::config::Config,
    ) -> Result<Option<String>, ExtractError> {
        let path = path.to_path_buf();
        on_worker(move || Registry::default_set().extract_to_string(&path, mime, &config))
    }

    /// A FIFO blocks an open for reading until a writer comes: refused
    /// before that, whatever the MIME says.
    #[cfg(unix)]
    #[test]
    fn a_fifo_fails_fast_for_every_extractor() {
        use std::os::unix::ffi::OsStrExt;

        let dir = crate::testutil::scratch_dir("extract-fifo");
        for (i, mime) in OPENING.into_iter().enumerate() {
            let fifo = dir.join(format!("fifo{i}"));
            let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            let err =
                extract_watched(&fifo, mime, crate::config::Config::default()).expect_err(mime);
            assert!(err.contains("not a regular file"), "{mime}: {err}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pending PDF swapped for a link to `/dev/zero`: lopdf would read it
    /// forever.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_device_fails_fast() {
        let dir = crate::testutil::scratch_dir("extract-dev-zero");
        let link = dir.join("x.pdf");
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        let err = extract_watched(&link, "application/pdf", crate::config::Config::default())
            .expect_err("a device is not a document");
        assert!(err.contains("not a regular file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every extractor refuses a file over `maximum_text_file_size` at open,
    /// before reading a byte, and says why.
    #[test]
    fn a_file_over_the_read_limit_fails_with_its_size() {
        let mut config = crate::config::Config::default();
        config.processing.maximum_text_file_size = 16;
        let dir = crate::testutil::scratch_dir("extract-over-read");
        let path = dir.join("big");
        std::fs::write(&path, [b'x'; 64]).unwrap();
        for mime in OPENING {
            let err = extract_watched(&path, mime, config.clone()).expect_err(mime);
            assert!(
                err.contains("is 64 bytes, over the 16-byte read limit"),
                "{mime}: {err}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A sandbox child that cannot be spawned (descriptors, processes or
    /// memory ran out; here, a binary that is not there) has not judged the
    /// file: the extraction reads as cancelled, which leaves the row pending,
    /// not failed.
    #[test]
    fn a_sandbox_child_that_cannot_spawn_leaves_its_file_unjudged() {
        let mut out = String::new();
        let mut scratch = Scratch::new(&crate::config::Config::default());
        let result = Registry::default_set().extract_where(
            Path::new("/nonexistent/quicksearch-sandboxed.pdf"),
            "application/pdf",
            &mut out,
            &mut scratch,
            Some(Path::new("/nonexistent/quicksearch-no-such-binary")),
        );
        assert_eq!(result, Err(CANCELLED.to_string()));
    }
}
