//! The extraction sandbox: a crafted PDF, audio file or legacy Office
//! document is parsed in a child process of the `quicksearch` binary, so what
//! it reaches is that child's stack and that child's memory limit.
//!
//! These inputs are here and not in the extractors' unit tests because
//! in-process every one of them ends the process by design: a stack
//! overflow is not a catchable panic, and a failed allocation aborts. lopdf
//! bounds neither recursion (an indirect `/Length` naming another stream, a
//! `/Length` inside its own object stream) nor what a stream inflates to,
//! the Type1 font parser's recursion is unbounded too, and lofty sizes a
//! FLAC's comment list from the count the file declares. Each test's verdict
//! is that the file is answered for — failed, or extracted without the part
//! that could not be read — and that this process is still here to say so.
//! The ordinary files are the other half: the child's text is the
//! in-process text.
//!
//! The PDF fixtures are raw bytes with hand-computed xref offsets ([`Raw`]):
//! shapes like a `/Length` chain or an object stream holding its own length
//! are what a writer library exists to prevent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::extract::{sandbox, Registry, Scratch, CANCELLED};
use quicksearch_core::file_handling::{decide_content, ContentOutcome};

// ---------------------------------------------------------------------------
// The sandbox under test
// ---------------------------------------------------------------------------

/// Held around every extraction in this file. A stop kills every child in
/// the process ([`sandbox::kill_sandbox_children`]), and libtest runs these
/// tests on parallel threads, so the stop tests' kills would otherwise land
/// on another test's child.
static SERIAL: Mutex<()> = Mutex::new(());

/// [`SERIAL`], however a failed test left it.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

const PDF: &str = "application/pdf";

/// Write `pdf` under `tag` and extract it through a sandbox child of the
/// binary this test built, under the default limits.
fn extract(tag: &str, pdf: &[u8]) -> Result<String, String> {
    extract_file(&write_pdf(tag, pdf), PDF)
}

/// Extract `path` as `mime` through a sandbox child, under the default
/// limits.
fn extract_file(path: &Path, mime: &str) -> Result<String, String> {
    let _serial = serial();
    extract_with(path, mime, &mut Scratch::new(&Config::default()))
}

/// Parse in a sandbox child from now on. Idempotent, and every extraction
/// calls it: cargo runs these tests in one process, in any order.
fn sandboxed() {
    sandbox::enable_sandbox(PathBuf::from(env!("CARGO_BIN_EXE_quicksearch")));
}

/// [`extract_file`] through `scratch`, for a caller holding [`SERIAL`].
fn extract_with(path: &Path, mime: &str, scratch: &mut Scratch) -> Result<String, String> {
    sandboxed();
    let mut out = String::new();
    let claimed = Registry::default_set().extract(path, mime, &mut out, scratch)?;
    assert!(claimed, "nothing claims {mime}");
    Ok(out)
}

/// `path` as `mime` in this process, as core's own tests extract it.
fn extract_in_process(path: &Path, mime: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut scratch = Scratch::new(&Config::default());
    Registry::default_set().extract_in_process(path, mime, &mut out, &mut scratch)?;
    Ok(out)
}

fn write_file(tag: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let path = quicksearch_core::testutil::scratch_dir(tag).join(name);
    let mut file = std::fs::File::create(&path).expect("create fixture");
    file.write_all(bytes).expect("write fixture");
    path
}

fn write_pdf(tag: &str, pdf: &[u8]) -> PathBuf {
    write_file(tag, "fixture.pdf", pdf)
}

/// A crash, a limit kill or a signal, as the registry reports a child's.
fn assert_crashed(outcome: Result<String, String>) {
    let reason = outcome.expect_err("the file must fail");
    assert_crash_reason(&reason);
}

fn assert_crash_reason(reason: &str) {
    assert!(
        reason.starts_with("extractor crashed on "),
        "unexpected failure reason: {}",
        reason
    );
}

/// What containing a memory bomb looks like from here: either the child
/// died under its limit, or the allocation it asked for was refused and the
/// document yielded what it still could. Which of the two is lopdf's
/// business — its inflate asks the allocator through `try_reserve` in one
/// place and unconditionally in another — and neither reaches this process,
/// which is the whole verdict.
fn assert_contained(outcome: Result<String, String>) {
    if let Err(reason) = outcome {
        assert_crash_reason(&reason);
    }
}

// ---------------------------------------------------------------------------
// A raw-bytes PDF builder
// ---------------------------------------------------------------------------

/// Where an object lives, as the cross-reference table must say.
enum Entry {
    /// At this byte offset in the file.
    At(usize),
    /// At this index inside that object stream.
    In { container: usize, index: usize },
}

/// A PDF assembled byte by byte. Objects are numbered from 1 in the order
/// they are added, so a forward reference is `next_id() + n`.
struct Raw {
    bytes: Vec<u8>,
    /// One per object, from object 1.
    entries: Vec<Entry>,
}

impl Raw {
    fn new() -> Raw {
        Raw {
            bytes: b"%PDF-1.5\n".to_vec(),
            entries: Vec::new(),
        }
    }

    /// The number the next object added will get.
    fn next_id(&self) -> usize {
        self.entries.len() + 1
    }

    fn push(&mut self, bytes: impl AsRef<[u8]>) {
        self.bytes.extend_from_slice(bytes.as_ref());
    }

    /// Append `body` as the next indirect object.
    fn object(&mut self, body: impl AsRef<[u8]>) -> usize {
        let id = self.next_id();
        self.entries.push(Entry::At(self.bytes.len()));
        self.push(format!("{} 0 obj\n", id));
        self.push(body);
        self.push(b"\nendobj\n");
        id
    }

    /// Append a stream whose `dict` entries are followed by the `/Length`
    /// its content needs.
    fn stream(&mut self, dict: &str, content: &[u8]) -> usize {
        self.stream_with_length(&format!("{} /Length {}", dict, content.len()), content)
    }

    /// [`Self::stream`] with `dict` carrying the `/Length` itself, which a
    /// fixture whose length is a reference needs.
    fn stream_with_length(&mut self, dict: &str, content: &[u8]) -> usize {
        let mut body = format!("<< {} >>\nstream\n", dict).into_bytes();
        body.extend_from_slice(content);
        body.extend_from_slice(b"\nendstream");
        self.object(body)
    }

    /// Number an object that lives inside the object stream `container`,
    /// which only a cross-reference *stream* can express.
    fn compressed(&mut self, container: usize, index: usize) -> usize {
        let id = self.next_id();
        self.entries.push(Entry::In { container, index });
        id
    }

    /// A one-page document showing `contents` with `resources`, finished
    /// with a classic cross-reference table.
    fn one_page(mut self, resources: &str, contents: usize) -> Vec<u8> {
        let pages = self.next_id() + 1;
        let page = self.object(format!(
            "<< /Type /Page /Parent {} 0 R /Resources {} /Contents {} 0 R \
             /MediaBox [0 0 612 792] >>",
            pages, resources, contents
        ));
        let pages = self.object(format!("<< /Type /Pages /Count 1 /Kids [{} 0 R] >>", page));
        let root = self.object(format!("<< /Type /Catalog /Pages {} 0 R >>", pages));
        self.finish(root)
    }

    /// Write the cross-reference table, trailer and `startxref`.
    fn finish(mut self, root: usize) -> Vec<u8> {
        let start = self.bytes.len();
        let size = self.entries.len() + 1;
        self.push(format!("xref\n0 {}\n0000000000 65535 f \n", size));
        for entry in &self.entries {
            let offset = match entry {
                Entry::At(offset) => *offset,
                Entry::In { .. } => panic!("a compressed object needs finish_xref_stream"),
            };
            self.bytes
                .extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        self.push(format!(
            "trailer\n<< /Size {} /Root {} 0 R >>\nstartxref\n{}\n%%EOF\n",
            size, root, start
        ));
        self.bytes
    }

    /// [`Self::finish`] as a cross-reference *stream*, the only form that can
    /// place an object inside an object stream. Fields are `/W [1 4 2]`:
    /// the entry type, then an offset or container, then a generation or
    /// index.
    fn finish_xref_stream(mut self, root: usize) -> Vec<u8> {
        // The stream is an object of its own, and describes itself.
        let xref_id = self.next_id();
        let xref_offset = self.bytes.len();
        let size = xref_id + 1;
        // Object 0 is the head of the free list.
        let mut table = vec![0, 0, 0, 0, 0, 0xFF, 0xFF];
        for entry in &self.entries {
            let (kind, second, third) = match *entry {
                Entry::At(offset) => (1u8, offset as u32, 0u16),
                Entry::In { container, index } => (2, container as u32, index as u16),
            };
            table.push(kind);
            table.extend_from_slice(&second.to_be_bytes());
            table.extend_from_slice(&third.to_be_bytes());
        }
        table.push(1);
        table.extend_from_slice(&(xref_offset as u32).to_be_bytes());
        table.extend_from_slice(&0u16.to_be_bytes());

        let dict = format!(
            "/Type /XRef /Size {} /W [1 4 2] /Root {} 0 R /Length {}",
            size,
            root,
            table.len()
        );
        self.stream_with_length(&dict, &table);
        self.push(format!("startxref\n{}\n%%EOF\n", xref_offset));
        self.bytes
    }
}

/// A font with built-in metrics and encoding, so no font file is read.
const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// `text` shown at the top of the page in the resources' `/F1`.
fn show(text: &str) -> Vec<u8> {
    format!("BT /F1 24 Tf 72 720 Td ({}) Tj ET\n", text).into_bytes()
}

// ---------------------------------------------------------------------------
// A stream whose encoded size says nothing about what it inflates to
// ---------------------------------------------------------------------------

/// Bits as deflate packs them.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    /// Bits of the last byte already written.
    filled: u32,
}

impl Bits {
    /// `n` low bits of `value`, least significant first: deflate's order for
    /// everything but a Huffman code.
    fn low(&mut self, value: u32, n: u32) {
        for i in 0..n {
            if self.filled == 0 {
                self.bytes.push(0);
            }
            let bit = (value >> i) & 1;
            *self.bytes.last_mut().expect("a byte to fill") |= (bit as u8) << self.filled;
            self.filled = (self.filled + 1) % 8;
        }
    }

    /// An `n`-bit Huffman code, most significant bit first.
    fn code(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.low((value >> i) & 1, 1);
        }
    }
}

/// The first distance each deflate distance code covers.
const DISTANCE_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Raw deflate (fixed Huffman codes): `prefix` as literals, then `copies`
/// back-references of 258 bytes each at `distance`. Thirteen bits buy 258
/// bytes, so the result inflates to about 158 times its own size — and
/// twice over, through a `/Filter` of two `/FlateDecode`s.
fn deflate_repeat(prefix: &[u8], copies: usize, distance: u32) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.low(1, 1); // The last block,
    bits.low(1, 2); // with the fixed code tables.
    for &byte in prefix {
        match byte {
            0..=143 => bits.code(0b0011_0000 + u32::from(byte), 8),
            _ => bits.code(0b1_1001_0000 + u32::from(byte) - 144, 9),
        }
    }
    let code = DISTANCE_BASE
        .iter()
        .rposition(|&base| base <= distance)
        .expect("a distance of at least 1");
    let extra = (code as u32 / 2).saturating_sub(1);
    for _ in 0..copies {
        bits.code(0b1100_0101, 8); // Length code 285: 258 bytes, no extra bits.
        bits.code(code as u32, 5);
        bits.low(distance - DISTANCE_BASE[code], extra);
    }
    bits.code(0, 7); // End of block.
    bits.bytes
}

/// `data` as a zlib stream, with the Adler-32 of what it inflates to.
fn zlib(data: Vec<u8>, adler: u32) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&data);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn adler32(bytes: &[u8]) -> u32 {
    let (a, b) = bytes.iter().fold((1u32, 0u32), |(a, b), &byte| {
        let a = (a + u32::from(byte)) % 65521;
        (a, (b + a) % 65521)
    });
    (b << 16) | a
}

/// Bytes that inflate twice over to `decoded` zero bytes: the inner stream
/// expands by back-references, and the outer one by the inner stream's
/// 13-byte period. Under a hundred kilobytes for two gigabytes.
fn flate_bomb(decoded: usize) -> Vec<u8> {
    let copies = decoded / 258;
    // Zeros, so the Adler-32 is `1` and the byte count: every byte leaves
    // the running sum at 1.
    let inner = zlib(
        deflate_repeat(&[0], copies, 1),
        ((decoded as u32 % 65521) << 16) | 1,
    );
    // Thirteen bits a copy means the inner stream repeats every 13 bytes
    // (eight copies, 104 bits) from its fourth byte, past the two-byte zlib
    // header and the three block-header bits, so a back-reference at that
    // distance replays it once a whole period has been written out. The
    // tail past the last whole copy is dropped: a stream cut short inflates
    // to all but its last few bytes.
    let prefix = 3 + 13;
    let outer = deflate_repeat(&inner[..prefix], (inner.len() - prefix) / 258, 13);
    zlib(outer, adler32(&inner))
}

/// Twice the memory limit a sandbox child runs under, so what the inflate
/// asks for cannot be granted however gradually it grows.
const BOMB_BYTES: usize = 2 << 30;

// ---------------------------------------------------------------------------
// Recursion
// ---------------------------------------------------------------------------

/// A stream's `/Length` may be an indirect reference, which lopdf resolves
/// by parsing the object it names — another stream, whose own `/Length` is
/// a reference. Each link is a nested parse, about 1.5 KB of stack, so a
/// few tens of kilobytes of file exhaust a worker's 512 KiB.
#[test]
fn a_length_reference_chain_is_contained() {
    const LINKS: usize = 2_000;
    let mut raw = Raw::new();
    for _ in 0..LINKS {
        let next = raw.next_id() + 1;
        raw.stream_with_length(&format!("/Length {} 0 R", next), b"");
    }
    // The tail: a length that is a number, so the chain ends rather than
    // failing early at a missing object.
    raw.object("0");
    let font = raw.object(HELVETICA);
    let contents = raw.stream("", &show("chained"));
    let pdf = raw.one_page(&format!("<< /Font << /F1 {} 0 R >> >>", font), contents);

    assert_crashed(extract("pdf-length-chain", &pdf));
}

/// An object stream holding the very object its `/Length` names: lopdf
/// resolves the length by reading the object, which means inflating the
/// container, whose length is the object again. Each turn starts a fresh
/// cycle check, so the cycle is never seen.
#[test]
fn an_objstm_whose_length_lives_inside_it_is_contained() {
    let mut raw = Raw::new();
    let length = raw.next_id() + 1;
    // "2 0 " indexes object 2 at offset 0 past `/First`, where it is `42`.
    let objstm = raw.stream_with_length(
        &format!("/Type /ObjStm /N 1 /First 4 /Length {} 0 R", length),
        b"2 0 42",
    );
    assert_eq!(raw.compressed(objstm, 0), length);
    let contents = raw.stream("", &show("objstm"));
    let page = raw.next_id();
    let pages = page + 1;
    raw.object(format!(
        "<< /Type /Page /Parent {} 0 R /Resources << >> /Contents {} 0 R \
         /MediaBox [0 0 612 792] >>",
        pages, contents
    ));
    raw.object(format!("<< /Type /Pages /Count 1 /Kids [{} 0 R] >>", page));
    let root = raw.object(format!("<< /Type /Catalog /Pages {} 0 R >>", pages));
    let pdf = raw.finish_xref_stream(root);

    assert_crashed(extract("pdf-objstm-length", &pdf));
}

/// A Type1 font file's built-in encoding is read by a PostScript parser
/// whose procedure rule recurses once per `{`, with nothing bounding it.
#[test]
fn a_deeply_nested_type1_fontfile_is_contained() {
    let mut raw = Raw::new();
    let file = raw.stream("/Length1 0 /Length2 0 /Length3 0", &b"{".repeat(100_000));
    let descriptor = raw.object(format!(
        "<< /Type /FontDescriptor /FontName /Fixture /Flags 4 /FontFile {} 0 R >>",
        file
    ));
    // No `/Encoding`, so the font file's own encoding is what is read.
    let font = raw.object(format!(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Fixture /FontDescriptor {} 0 R >>",
        descriptor
    ));
    let contents = raw.stream("", &show("type1"));
    let pdf = raw.one_page(&format!("<< /Font << /F1 {} 0 R >> >>", font), contents);

    assert_crashed(extract("pdf-type1-nesting", &pdf));
}

/// A ToUnicode CMap of 100,000 nested arrays. lopdf's content tokenizer
/// stops at a nesting depth of 100, but one of its frames is large enough
/// that a hundred of them overflow a 512 KiB worker stack in an
/// unoptimized build, so which way this ends is the child's frame size:
/// either the CMap is rejected whole, which reads like a font with no
/// ToUnicode and still yields the page's text, or the child dies and the
/// file fails. Both are containment; neither reaches this process.
#[test]
fn a_deeply_nested_tounicode_is_contained() {
    let mut raw = Raw::new();
    let mut cmap = b"[".repeat(100_000);
    cmap.extend_from_slice(&b"]".repeat(100_000));
    let to_unicode = raw.stream("", &cmap);
    let font = raw.object(format!(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode {} 0 R >>",
        to_unicode
    ));
    let contents = raw.stream("", &show("nested"));
    let pdf = raw.one_page(&format!("<< /Font << /F1 {} 0 R >> >>", font), contents);

    match extract("pdf-tounicode-nesting", &pdf) {
        Ok(out) => assert!(out.contains("nested"), "got {:?}", out),
        Err(reason) => assert_crash_reason(&reason),
    }
}

// ---------------------------------------------------------------------------
// Inflation
// ---------------------------------------------------------------------------

/// A page whose content stream inflates to gigabytes from under a hundred
/// kilobytes. The work budget charges the encoded bytes, which are cheap,
/// so only the child's memory limit stops this one: in-process the inflate
/// is granted and the indexer holds it.
#[test]
fn a_chained_flate_bomb_is_contained() {
    let mut raw = Raw::new();
    let contents = raw.stream(
        "/Filter [/FlateDecode /FlateDecode]",
        &flate_bomb(BOMB_BYTES),
    );
    let pdf = raw.one_page("<< >>", contents);

    assert_contained(extract("pdf-flate-bomb", &pdf));
}

/// The same stream as an object stream, which lopdf inflates while loading
/// the document — before any budget or page exists, and ignoring whether
/// the inflate worked, so the page after it still renders.
#[test]
fn an_oversized_objstm_is_contained() {
    let mut raw = Raw::new();
    raw.stream(
        "/Type /ObjStm /N 1 /First 4 /Filter [/FlateDecode /FlateDecode]",
        &flate_bomb(BOMB_BYTES),
    );
    let font = raw.object(HELVETICA);
    let contents = raw.stream("", &show("objstm"));
    let pdf = raw.one_page(&format!("<< /Font << /F1 {} 0 R >> >>", font), contents);

    assert_contained(extract("pdf-objstm-bomb", &pdf));
}

// ---------------------------------------------------------------------------
// The ordinary file, and a stop
// ---------------------------------------------------------------------------

/// Two lines of Helvetica text.
fn ordinary_pdf() -> Vec<u8> {
    let mut raw = Raw::new();
    let font = raw.object(HELVETICA);
    let mut content = show("Hello sandbox");
    content.extend_from_slice(b"BT /F1 12 Tf 72 600 Td (second line) Tj ET\n");
    let contents = raw.stream("", &content);
    raw.one_page(&format!("<< /Font << /F1 {} 0 R >> >>", font), contents)
}

/// What the sandbox is for is the files it does not change: the child's text
/// is the in-process text, byte for byte.
#[test]
fn an_ordinary_pdf_gives_the_same_text_as_in_process() {
    let path = write_pdf("pdf-ordinary", &ordinary_pdf());

    let sandboxed = extract_file(&path, PDF).expect("extract");
    assert!(sandboxed.contains("Hello sandbox"), "got {:?}", sandboxed);
    assert_eq!(
        sandboxed,
        extract_in_process(&path, PDF).expect("extract in process")
    );
}

/// A stop reaches a PDF mid-parse: the child is killed, so the read of its
/// output ends, and the file reads as cancelled, which leaves its row
/// pending rather than failed. The verdict is that outcome, which only a
/// child killed after the stop gives, not how long the call took.
#[test]
fn a_stop_kills_a_running_child() {
    // A form that draws itself twice at every level spends the whole work
    // budget (seconds), so there is a parse to interrupt.
    let mut raw = Raw::new();
    let form = raw.next_id();
    raw.stream(
        &format!(
            "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
             /Resources << /XObject << /X0 {} 0 R >> >>",
            form
        ),
        b"/X0 Do /X0 Do",
    );
    let contents = raw.stream("", b"/X0 Do");
    let pdf = raw.one_page(&format!("<< /XObject << /X0 {} 0 R >> >>", form), contents);
    let path = write_pdf("pdf-stop", &pdf);

    let _serial = serial();
    sandboxed();
    let extracting = std::thread::spawn(move || {
        let config = Config::default();
        let path = path.to_str().expect("a UTF-8 scratch path");
        let registry = Registry::default_set();
        decide_content(
            path,
            Some(PDF),
            &registry,
            &config,
            &mut Scratch::new(&config),
        )
    });
    // Repeated, so a child spawned after one kill is killed by the next; a
    // kill also pre-empts one not yet spawned, so this cannot miss.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !extracting.is_finished() && Instant::now() < deadline {
        sandbox::kill_sandbox_children();
        std::thread::sleep(Duration::from_millis(20));
    }
    let outcome = extracting.join().expect("the extracting thread survives");
    assert_eq!(outcome, ContentOutcome::Cancelled);
}

/// A stop that lands after a worker's last stop check, before its next
/// child spawns, still kills that child: the worker's kill mark is read when
/// its scratch is made, before the check, not when the extraction starts.
#[test]
fn a_stop_before_the_spawn_kills_the_child() {
    let path = write_pdf("pdf-stop-before", &ordinary_pdf());
    let _serial = serial();

    let mut scratch = Scratch::new(&Config::default());
    sandbox::kill_sandbox_children();
    assert_eq!(
        extract_with(&path, PDF, &mut scratch),
        Err(CANCELLED.to_string())
    );

    // A scratch made after the stop is not under it.
    let mut scratch = Scratch::new(&Config::default());
    let text = extract_with(&path, PDF, &mut scratch).expect("extract");
    assert!(text.contains("Hello sandbox"), "got {:?}", text);
}

// ---------------------------------------------------------------------------
// Audio and legacy Office
// ---------------------------------------------------------------------------

/// A committed fixture of core's corpus, by its path under
/// `quicksearch-core/tests/fixtures`.
fn core_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../quicksearch-core/tests/fixtures")
        .join(name)
}

/// A FLAC block: the last-block flag and the type, a 24-bit big-endian
/// length, then `content`.
fn flac_block(kind: u8, last: bool, content: &[u8]) -> Vec<u8> {
    let len = u32::try_from(content.len()).expect("a small block");
    let mut block = vec![kind | if last { 0x80 } else { 0 }];
    block.extend_from_slice(&len.to_be_bytes()[1..]);
    block.extend_from_slice(content);
    block
}

/// Core's fifty milliseconds of committed silence with one Vorbis comment
/// block holding `comments` (`KEY=value`) but declaring `count` of them, in
/// place of its own blocks after the stream info.
fn flac(comments: &[&str], count: u32) -> Vec<u8> {
    let silence = std::fs::read(core_fixture("silence.flac")).expect("read silence.flac");
    let content_len = |at: usize| {
        u32::from_be_bytes([0, silence[at + 1], silence[at + 2], silence[at + 3]]) as usize
    };
    // "fLaC", then the stream info block, which comes first.
    let info = &silence[8..8 + content_len(4)];
    // The frames follow the last block.
    let mut at = 4;
    let frames = loop {
        let last = silence[at] & 0x80 != 0;
        at += 4 + content_len(at);
        if last {
            break &silence[at..];
        }
    };

    let vendor = b"quicksearch";
    let mut block = (vendor.len() as u32).to_le_bytes().to_vec();
    block.extend_from_slice(vendor);
    block.extend_from_slice(&count.to_le_bytes());
    for comment in comments {
        block.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        block.extend_from_slice(comment.as_bytes());
    }
    let mut out = b"fLaC".to_vec();
    out.extend(flac_block(0, false, info));
    out.extend(flac_block(4, true, &block));
    out.extend_from_slice(frames);
    out
}

/// lofty 0.19.2 sizes a FLAC's comment list from the count its block
/// declares, past its own allocation limit (`ogg/read.rs:79`): this one
/// asks for 80 GB, whose refusal aborts the process. In the child, it fails
/// the one file.
#[test]
fn a_flac_declaring_more_comments_than_it_holds_is_contained() {
    let path = write_file(
        "audio-comment-count",
        "track.flac",
        &flac(&["TITLE=Damaged"], 1_679_818_758),
    );
    assert_crashed(extract_file(&path, "audio/flac"));
}

#[test]
fn an_ordinary_flac_gives_the_same_text_as_in_process() {
    let comments = [
        "TITLE=Blue Monday",
        "ARTIST=New Order",
        "ALBUM=Power Corruption",
        "GENRE=Synthpop",
    ];
    let path = write_file("audio-ordinary", "track.flac", &flac(&comments, 4));

    let sandboxed = extract_file(&path, "audio/flac").expect("extract");
    assert_eq!(sandboxed, "Blue Monday New Order Power Corruption Synthpop");
    assert_eq!(
        sandboxed,
        extract_in_process(&path, "audio/flac").expect("extract in process")
    );
}

/// LibreOffice's workbook from core's corpus, through cfb in the child.
#[test]
fn an_ordinary_xls_gives_the_same_text_as_in_process() {
    let path = core_fixture("legacy/sample.xls");
    let mime = "application/vnd.ms-excel";

    let sandboxed = extract_file(&path, mime).expect("extract");
    assert!(sandboxed.contains("chalcedony9002"), "got {:?}", sandboxed);
    assert_eq!(
        sandboxed,
        extract_in_process(&path, mime).expect("extract in process")
    );
}
