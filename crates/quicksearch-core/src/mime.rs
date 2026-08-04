//! MIME type guessing and `FileType` bitmask classification.
//!
//! [`guess_mime_from_head`] infers a MIME type in three stages: extension
//! first (an override table, then `mime_guess`), magic-byte sniffing via
//! `infer` next, and finally a text sniff ([`crate::textenc`]) that answers
//! `text/plain` for anything whose head reads as text — which is how
//! extensionless files (README, Makefile) and source extensions no MIME
//! table knows (`.go`, `.zig`) get their contents indexed. Extensions in
//! [`AMBIGUOUS_EXTENSIONS`] invert the order: content decides, and the
//! extension's MIME is only a fallback.
//!
//! [`mime_to_type`] then maps a MIME string to a [`FileType`] bitmask so a
//! single file can belong to multiple categories (e.g. a `.docx` is
//! Document|Text).
//!
//! The head bytes are always ones the caller already holds. Indexing reads
//! the head of every new or changed file to hash it, and those are the same
//! bytes `infer` and the text sniff want, so there is no path-based variant
//! that goes back to disk for them — that was a second open/read/close per
//! undetectable file.

use std::path::Path;

/// Bit-flag category for a file. Unlike the MIME string this is designed for
/// cheap bitmask queries like `type & FileType::AUDIO != 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType(pub u32);

impl FileType {
    pub const EMPTY: FileType = FileType(0);
    pub const AUDIO: FileType = FileType(1 << 0);
    pub const IMAGE: FileType = FileType(1 << 1);
    pub const VIDEO: FileType = FileType(1 << 2);
    pub const DOCUMENT: FileType = FileType(1 << 3);
    pub const TEXT: FileType = FileType(1 << 4);
    pub const ARCHIVE: FileType = FileType(1 << 5);
    pub const PRESENTATION: FileType = FileType(1 << 6);
    pub const SPREADSHEET: FileType = FileType(1 << 7);
    pub const FOLDER: FileType = FileType(1 << 8);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: FileType) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Parse a single Baloo-style category name (`Audio`, `Image`, ...).
    /// Case-insensitive. Returns `EMPTY` for unknown names.
    pub fn from_name(s: &str) -> FileType {
        match s.to_ascii_lowercase().as_str() {
            "audio" => FileType::AUDIO,
            "image" => FileType::IMAGE,
            "video" => FileType::VIDEO,
            "document" => FileType::DOCUMENT,
            "text" => FileType::TEXT,
            "archive" => FileType::ARCHIVE,
            "presentation" => FileType::PRESENTATION,
            "spreadsheet" => FileType::SPREADSHEET,
            "folder" => FileType::FOLDER,
            _ => FileType::EMPTY,
        }
    }
}

impl std::ops::BitOr for FileType {
    type Output = FileType;
    fn bitor(self, rhs: FileType) -> FileType {
        FileType(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for FileType {
    fn bitor_assign(&mut self, rhs: FileType) {
        self.0 |= rhs.0;
    }
}

/// Extensions whose MIME is pinned regardless of what `mime_guess` or the
/// file's bytes say.
///
/// Consulted *before* everything else, because for these the table is not a
/// fallback but a correction or a guarantee:
///
/// - `.bat` maps in `mime_guess` to `application/x-msdownload`, i.e. an
///   executable, and a non-empty `mime_guess` answer would preempt the text
///   sniff — so without this entry batch files are never content-indexed.
///   (`.cmd` already resolves to `text/plain`; it is listed so the pair
///   cannot drift.)
/// - `.ps1`/`.psm1`/`.psd1`, `.inf` and `.url` are absent from `mime_guess`.
///   The text sniff would usually catch them, but pinning them costs
///   nothing and classifies them deterministically, whatever their head
///   bytes happen to look like.
///
/// Platform-neutral on purpose: a `.ps1` copied to a Linux box should classify
/// the same way.
const EXTENSION_OVERRIDES: &[(&str, &str)] = &[
    ("bat", "text/plain"),
    ("cmd", "text/plain"),
    ("inf", "text/plain"),
    ("ps1", "text/plain"),
    ("psd1", "text/plain"),
    ("psm1", "text/plain"),
    ("url", "text/plain"),
];

/// Extensions `mime_guess` maps to a binary format that is, on a modern
/// disk, at least as often a text file: `.ts`/`.mts` TypeScript vs MPEG
/// transport stream, `.mod` go.mod vs `video/mpeg`, `.org` Org-mode vs
/// Lotus Organizer, `.scm` Scheme vs Lotus ScreenCam, `.pot` gettext
/// template vs PowerPoint template, `.vhd` VHDL source vs VirtualBox disk
/// image.
///
/// For these the content decides: magic bytes first, then the text sniff,
/// and only if both decline does `mime_guess`'s extension answer stand — so
/// a real MPEG-TS recording still classifies as video.
const AMBIGUOUS_EXTENSIONS: &[&str] = &["mod", "mts", "org", "pot", "scm", "ts", "vhd"];

/// Whether `path`'s extension is in [`AMBIGUOUS_EXTENSIONS`].
/// ASCII-case-insensitive, like [`extension_override`].
fn extension_is_ambiguous(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| AMBIGUOUS_EXTENSIONS.contains(&e.as_str()))
}

/// Look up [`EXTENSION_OVERRIDES`] for `path`. Extension comparison is
/// ASCII-case-insensitive, which matters more on Windows where `REPORT.BAT` is
/// as common as the lowercase spelling.
fn extension_override(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    EXTENSION_OVERRIDES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
}

/// Infer a MIME type from a path plus the file's leading bytes.
///
/// Extension first — an override table, then `mime_guess` — then magic
/// bytes when those come up empty or say `application/octet-stream`, and
/// finally a text sniff that answers `text/plain` for any head that reads
/// as text ([`crate::textenc::looks_like_text`]). For
/// [`AMBIGUOUS_EXTENSIONS`] the `mime_guess` answer is demoted to a last
/// resort behind both content checks.
///
/// `head` is whatever the caller already read; indexing passes the same buffer
/// it hashes. It bounds magic-byte detection, so a caller that supplies fewer
/// than 262 bytes (`infer`'s longest signature) can get `None` where a longer
/// head would have matched. The indexer's `hash_length` defaults to 8 KiB —
/// exactly what `infer` itself reads from a path — so at default config this
/// is as good as opening the file, and strictly cheaper. The same buffer
/// bounds the text sniff, which tolerates a multibyte character cut off at
/// the buffer's end.
///
/// A `None` result is a real answer, not a "don't know": the content pass
/// stores it and does not re-derive it (see
/// [`crate::file_handling::extract_and_store`]).
pub fn guess_mime_from_head(path: &Path, head: &[u8]) -> Option<String> {
    if let Some(m) = extension_override(path) {
        return Some(m.to_string());
    }
    let by_extension = mime_guess::from_path(path).first().and_then(|g| {
        let s = g.essence_str();
        (!s.is_empty() && s != "application/octet-stream").then(|| s.to_string())
    });
    if !extension_is_ambiguous(path) && by_extension.is_some() {
        return by_extension;
    }
    if let Some(t) = infer::get(head) {
        let magic = t.mime_type();
        // For an ambiguous extension, `infer`'s generic OLE-container answer
        // is less specific than the extension's: a real PowerPoint `.pot`
        // template must resolve to vnd.ms-powerpoint (which the office
        // extractor claims), not to a container MIME nothing claims.
        if magic == "application/x-ole-storage" && by_extension.is_some() {
            return by_extension;
        }
        return Some(magic.to_string());
    }
    if crate::textenc::looks_like_text(head) {
        return Some("text/plain".to_string());
    }
    // Only an ambiguous extension still has an answer left to fall back on.
    by_extension
}

/// Map a MIME string to a [`FileType`] bitmask. Ported from Baloo's
/// `basicindexingjob.cpp:typesForMimeType`.
pub fn mime_to_type(mime: &str) -> FileType {
    let lower = mime.to_ascii_lowercase();
    let (top, sub) = match lower.split_once('/') {
        Some(pair) => pair,
        None => return FileType::EMPTY,
    };
    let mut t = FileType::EMPTY;
    match top {
        "audio" => t |= FileType::AUDIO,
        "image" => t |= FileType::IMAGE,
        "video" => t |= FileType::VIDEO,
        "text" => {
            t |= FileType::TEXT;
            // HTML counts as a document too in Baloo. (xhtml+xml is handled
            // in the subtype match below, whatever its top level.)
            if sub == "html" {
                t |= FileType::DOCUMENT;
            }
        }
        _ => {}
    }
    // Subtype-based classification for the `application/*` grab bag.
    match sub {
        // Office formats
        "msword"
        | "vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "vnd.oasis.opendocument.text"
        | "rtf"
        | "pdf"
        | "epub+zip"
        | "x-mobipocket-ebook" => {
            t |= FileType::DOCUMENT;
        }
        "vnd.ms-excel"
        | "vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "vnd.oasis.opendocument.spreadsheet" => {
            t |= FileType::DOCUMENT | FileType::SPREADSHEET;
        }
        "vnd.ms-powerpoint"
        | "vnd.openxmlformats-officedocument.presentationml.presentation"
        | "vnd.oasis.opendocument.presentation" => {
            t |= FileType::DOCUMENT | FileType::PRESENTATION;
        }
        // Outlook saved messages and compiled HTML help are documents; both
        // are ordinary things to find in a Windows home directory.
        "vnd.ms-outlook" | "vnd.ms-htmlhelp" => {
            t |= FileType::DOCUMENT;
        }
        // Archives. The Windows installer/cabinet formats are containers in
        // exactly the same sense as the rest of this list.
        "zip"
        | "x-tar"
        | "x-7z-compressed"
        | "x-rar"
        | "x-rar-compressed"
        | "gzip"
        | "x-bzip"
        | "x-bzip2"
        | "x-xz"
        | "vnd.debian.binary-package"
        | "x-rpm"
        | "vnd.ms-cab-compressed"
        | "x-msi" => {
            t |= FileType::ARCHIVE;
        }
        // XHTML is text and, like HTML above, a document in Baloo's model —
        // whichever top level it arrives under.
        "xhtml+xml" => {
            t |= FileType::TEXT | FileType::DOCUMENT;
        }
        // Structured text: everything the plaintext extractor claims beyond
        // `text/*` (see `extract::plaintext::EXTRA_TEXT_MIMES` and the
        // cross-check test below). Keyed on the subtype alone, so playlists
        // stay AUDIO|TEXT and SVG stays IMAGE|TEXT.
        "xml" | "json" | "json5" | "geo+json" | "javascript" | "mbox" | "rfc822" | "vnd.dart"
        | "x-csh" | "x-httpd-php" | "x-perl" | "x-sh" | "x-sql" | "x-subrip" | "x-tcl"
        | "x-tex" | "x-texinfo" | "x-troff" | "x-troff-man" | "x-mpegurl" | "scpls" | "svg+xml" => {
            t |= FileType::TEXT;
        }
        _ => {}
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_mime() {
        assert!(mime_to_type("audio/mpeg").contains(FileType::AUDIO));
        assert!(mime_to_type("audio/flac").contains(FileType::AUDIO));
    }

    #[test]
    fn image_mime() {
        assert!(mime_to_type("image/jpeg").contains(FileType::IMAGE));
        assert!(mime_to_type("image/png").contains(FileType::IMAGE));
    }

    #[test]
    fn docx_is_document_and_office() {
        let t =
            mime_to_type("application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert!(t.contains(FileType::DOCUMENT));
    }

    #[test]
    fn xlsx_is_spreadsheet_and_document() {
        let t = mime_to_type("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
        assert!(t.contains(FileType::DOCUMENT));
        assert!(t.contains(FileType::SPREADSHEET));
    }

    #[test]
    fn pptx_is_presentation() {
        let t = mime_to_type(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        );
        assert!(t.contains(FileType::PRESENTATION));
    }

    #[test]
    fn html_is_text_and_document() {
        let t = mime_to_type("text/html");
        assert!(t.contains(FileType::TEXT));
        assert!(t.contains(FileType::DOCUMENT));
    }

    #[test]
    fn plain_text() {
        let t = mime_to_type("text/plain");
        assert!(t.contains(FileType::TEXT));
        assert!(!t.contains(FileType::DOCUMENT));
    }

    #[test]
    fn zip_is_archive() {
        assert!(mime_to_type("application/zip").contains(FileType::ARCHIVE));
    }

    #[test]
    fn unknown_mime_is_empty() {
        assert_eq!(mime_to_type("weird/blob"), FileType::EMPTY);
    }

    #[test]
    fn from_name_round_trip() {
        for n in [
            "Audio",
            "Image",
            "Video",
            "Document",
            "Text",
            "Archive",
            "Spreadsheet",
            "Presentation",
            "Folder",
        ] {
            assert_ne!(FileType::from_name(n), FileType::EMPTY, "{}", n);
        }
        assert_eq!(FileType::from_name("Weird"), FileType::EMPTY);
    }

    /// Extension resolution happens before magic bytes are consulted, so an
    /// empty head is enough to exercise it.
    #[test]
    fn guess_mime_by_extension() {
        use std::path::PathBuf;
        let by_ext = |n: &str| guess_mime_from_head(&PathBuf::from(n), b"").unwrap_or_default();
        assert_eq!(by_ext("a.txt"), "text/plain");
        assert_eq!(by_ext("a.png"), "image/png");
        assert_eq!(by_ext("a.mp3"), "audio/mpeg");
    }

    /// Every override must land on a type the plaintext extractor accepts —
    /// the point of the table is that these files get their contents indexed.
    #[test]
    fn windows_script_types_reach_the_plaintext_extractor() {
        use crate::extract::{plaintext::PlaintextExtractor, Extractor};
        use std::path::PathBuf;

        for name in [
            "deploy.ps1",
            "Module.psm1",
            "Module.psd1",
            "build.bat",
            "build.cmd",
            "driver.inf",
            "bookmark.url",
        ] {
            let mime = guess_mime_from_head(&PathBuf::from(name), b"")
                .unwrap_or_else(|| panic!("{} has no MIME", name));
            assert!(
                PlaintextExtractor.supports(&mime),
                "{} -> {} is not extractable as text",
                name,
                mime
            );
        }
    }

    #[test]
    fn extension_overrides_are_case_insensitive() {
        use std::path::PathBuf;
        // Uppercase extensions are ordinary on Windows.
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("DEPLOY.PS1"), b"").as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("Build.Bat"), b"").as_deref(),
            Some("text/plain")
        );
    }

    /// The override table must win over the file's actual content: a `.ps1`
    /// holding something `infer` would recognise is still a script.
    #[test]
    fn extension_overrides_beat_magic_bytes() {
        use std::path::PathBuf;
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("a.ps1"), b"Write-Host hi").as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("a.ps1"), b"%PDF-1.7").as_deref(),
            Some("text/plain")
        );
    }

    #[test]
    fn sql_dumps_are_extractable() {
        use crate::extract::{plaintext::PlaintextExtractor, Extractor};
        use std::path::PathBuf;
        let mime = guess_mime_from_head(&PathBuf::from("schema.sql"), b"").unwrap();
        assert!(PlaintextExtractor.supports(&mime), "{}", mime);
    }

    /// The content pass trusts the MIME the walk stored, including `None`, and
    /// never reopens the file to second-guess it. That is only sound if a
    /// `hash_length`-sized head is enough to recognise a format from its magic
    /// bytes — `infer`'s longest signature is 262 bytes and the default head is
    /// 8 KiB, so it is by a wide margin. This pins that for extensionless
    /// files, where magic bytes are the only signal there is.
    #[test]
    fn a_default_sized_head_is_enough_for_magic_byte_detection() {
        use std::path::PathBuf;
        let head_bytes = crate::config::ProcessingConfig::default().hash_length;

        let samples: &[(&str, &[u8], &str)] = &[
            (
                "png",
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "image/png",
            ),
            ("gif", b"GIF89a", "image/gif"),
            ("pdf", b"%PDF-1.7", "application/pdf"),
            ("zip", &[0x50, 0x4b, 0x03, 0x04], "application/zip"),
            ("gz", &[0x1f, 0x8b, 0x08], "application/gzip"),
        ];

        for (tag, magic, expected) in samples {
            // No extension at all, so nothing but the bytes can answer.
            let path = PathBuf::from(format!("/tmp/qs-sniff-{}", tag));
            let mut body = magic.to_vec();
            body.resize(head_bytes, 0);
            assert_eq!(
                guess_mime_from_head(&path, &body).as_deref(),
                Some(*expected),
                "{} must be detectable from a default-sized head",
                tag
            );
        }
    }

    /// The other side of that bound: starve the head below `infer`'s longest
    /// signature and magic detection legitimately degrades. Documented
    /// behaviour of a non-default `hash_length`, not a bug — but a binary
    /// head must stay a `None` rather than become a wrong guess. (A head
    /// that *reads as text* is a different case: the text sniff answers for
    /// it, however short.)
    #[test]
    fn a_head_shorter_than_the_signature_declines_rather_than_guessing() {
        use std::path::PathBuf;
        let path = PathBuf::from("/tmp/qs-sniff-truncated");
        assert_eq!(guess_mime_from_head(&path, b"").as_deref(), None);
        // A PNG magic truncated to two bytes: not a magic match, and the
        // NUL fails the binary guard, so no text guess either.
        assert_eq!(guess_mime_from_head(&path, &[0x89, 0x00]).as_deref(), None);
        // Enough bytes, and it resolves.
        assert_eq!(
            guess_mime_from_head(&path, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
                .as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn windows_container_and_document_types_classify() {
        assert!(mime_to_type("application/vnd.ms-cab-compressed").contains(FileType::ARCHIVE));
        assert!(mime_to_type("application/x-msi").contains(FileType::ARCHIVE));
        assert!(mime_to_type("application/vnd.ms-outlook").contains(FileType::DOCUMENT));
        assert!(mime_to_type("application/vnd.ms-htmlhelp").contains(FileType::DOCUMENT));
    }

    /// Every extension fixed by this round of coverage work must reach the
    /// plaintext extractor through real dispatch — `extract_complete_head`
    /// rather than `supports` — so the svg/m3u/pls cases prove the
    /// plaintext-first registration *order*, not just the MIME claim.
    #[test]
    fn newly_claimed_extensions_reach_the_plaintext_extractor() {
        use crate::extract::Registry;
        use std::path::PathBuf;

        let registry = Registry::default_set();
        let samples: &[(&str, &[u8])] = &[
            ("deploy.sh", b"echo hi"),
            ("env.csh", b"setenv X 1"),
            ("script.pl", b"print 1;"),
            ("Module.pm", b"package M;"),
            ("index.php", b"<?php echo 1;"),
            ("paper.tex", b"\\documentclass{article}"),
            ("page.xhtml", b"<html/>"),
            ("notes.json5", b"{a: 1}"),
            ("map.geojson", b"{}"),
            ("subs.srt", b"1\n00:00:01 --> 00:00:02\nhi\n"),
            ("run.tcl", b"puts hi"),
            ("main.dart", b"void main() {}"),
            ("page.man", b".TH TEST 1"),
            ("test.t", b"use Test::More;"),
            ("doc.texi", b"@node Top"),
            ("mail.eml", b"Subject: hi\n\nbody"),
            ("inbox.mbox", b"From a@b\n\nbody"),
            ("icon.svg", b"<svg xmlns='x'/>"),
            ("list.m3u", b"#EXTM3U\ntrack.mp3"),
            ("radio.pls", b"[playlist]"),
        ];
        for (name, head) in samples {
            let path = PathBuf::from(name);
            let mime =
                guess_mime_from_head(&path, head).unwrap_or_else(|| panic!("{} has no MIME", name));
            let extracted = registry
                .extract_complete_head(&path, &mime, head)
                .unwrap_or_else(|| {
                    panic!(
                        "{} -> {} not claimed by a head-capable extractor",
                        name, mime
                    )
                })
                .unwrap_or_else(|e| panic!("{} -> {} failed to extract: {}", name, mime, e));
            assert!(
                !extracted.text.is_empty(),
                "{} -> {} extracted no text",
                name,
                mime
            );
        }
    }

    /// Extensionless files are decided by their bytes: text heads index,
    /// binary heads stay unclassified.
    #[test]
    fn extensionless_files_sniff_by_content() {
        use std::path::PathBuf;
        let readme = PathBuf::from("README");
        assert_eq!(
            guess_mime_from_head(&readme, b"QuickSearch indexes your files.\n").as_deref(),
            Some("text/plain")
        );
        let makefile = PathBuf::from("Makefile");
        assert_eq!(
            guess_mime_from_head(&makefile, b"all:\n\tcargo build\n").as_deref(),
            Some("text/plain")
        );
        let blob = PathBuf::from("blob");
        assert_eq!(
            guess_mime_from_head(&blob, &[0x00, 0x01, 0x02, 0xFF]).as_deref(),
            None
        );
    }

    /// Ambiguous extensions resolve by content in both directions: source
    /// code beats the extension table, real binary keeps the extension's
    /// MIME as the fallback.
    #[test]
    fn ambiguous_extensions_resolve_by_content_both_ways() {
        use std::path::PathBuf;

        let ts_source = b"export function hi(): string { return 'hi'; }\n";
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("app.ts"), ts_source).as_deref(),
            Some("text/plain")
        );
        // Uppercase, as Windows likes it.
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("APP.TS"), ts_source).as_deref(),
            Some("text/plain")
        );
        // An MPEG transport stream: 0x47 sync bytes with NUL-heavy payloads.
        // No magic matcher, fails the text sniff, so the extension answers.
        let mut ts_video = vec![0u8; 376];
        ts_video[0] = 0x47;
        ts_video[188] = 0x47;
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("clip.ts"), &ts_video).as_deref(),
            Some("video/vnd.dlna.mpeg-tts")
        );

        assert_eq!(
            guess_mime_from_head(
                &PathBuf::from("go.mod"),
                b"module example.com/x\n\ngo 1.22\n"
            )
            .as_deref(),
            Some("text/plain")
        );

        // gettext template vs PowerPoint template: text decides one way,
        // binary bytes fall back to the extension's office MIME (whether
        // infer's OLE matcher fires or the guard rejects, the answer agrees).
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("app.pot"), b"msgid \"hello\"\nmsgstr \"\"\n")
                .as_deref(),
            Some("text/plain")
        );
        let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0x00, 0x00];
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("slides.pot"), &ole).as_deref(),
            Some("application/vnd.ms-powerpoint")
        );

        assert_eq!(
            guess_mime_from_head(&PathBuf::from("cpu.vhd"), b"entity cpu is\nend cpu;\n")
                .as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("disk.vhd"), &[0x00, 0x01, 0x02, 0x03]).as_deref(),
            Some("application/x-virtualbox-vhd")
        );
    }

    /// Everything the plaintext extractor claims must carry the TEXT bit,
    /// or `type:Text` silently misses content-indexed files (the pre-fix
    /// state of `.sql`). Iterates the actual claim list so the two can
    /// never drift apart.
    #[test]
    fn every_plaintext_claim_carries_the_text_bit() {
        for mime in crate::extract::plaintext::EXTRA_TEXT_MIMES {
            assert!(
                mime_to_type(mime).contains(FileType::TEXT),
                "{} is extractable as text but lacks FileType::TEXT",
                mime
            );
        }
        // The multi-category cases keep their native category too.
        let svg = mime_to_type("image/svg+xml");
        assert!(svg.contains(FileType::IMAGE) && svg.contains(FileType::TEXT));
        let m3u = mime_to_type("audio/x-mpegurl");
        assert!(m3u.contains(FileType::AUDIO) && m3u.contains(FileType::TEXT));
        let xhtml = mime_to_type("application/xhtml+xml");
        assert!(xhtml.contains(FileType::TEXT) && xhtml.contains(FileType::DOCUMENT));
        // And the `text/` prefix arm still covers the rest.
        assert!(mime_to_type("text/x-toml").contains(FileType::TEXT));
    }
}
