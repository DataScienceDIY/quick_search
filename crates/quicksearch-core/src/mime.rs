//! MIME guessing and `FileType` bitmask classification. Inference is
//! extension first, magic bytes next, then a strict text sniff (see
//! [`crate::textenc`]); [`AMBIGUOUS_EXTENSIONS`] invert the order.

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

    /// Parse a Baloo-style category name; `EMPTY` for unknown names.
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
/// bytes say. `.bat` maps in `mime_guess` to an executable MIME, which would
/// leave batch files never content-indexed; the rest are absent from
/// `mime_guess` and pinned so they classify deterministically.
const EXTENSION_OVERRIDES: &[(&str, &str)] = &[
    ("bat", "text/plain"),
    ("cmd", "text/plain"),
    ("inf", "text/plain"),
    ("ps1", "text/plain"),
    ("psd1", "text/plain"),
    ("psm1", "text/plain"),
    ("url", "text/plain"),
];

/// Extensions `mime_guess` maps to a binary format that is at least as often
/// a text file (`.ts` TypeScript vs MPEG transport stream, `.mod` go.mod vs
/// `video/mpeg`, …). For these the content decides; the extension's MIME
/// stands only if magic bytes and the text sniff both decline.
const AMBIGUOUS_EXTENSIONS: &[&str] = &["mod", "mts", "org", "pot", "scm", "ts", "vhd"];

fn extension_is_ambiguous(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        AMBIGUOUS_EXTENSIONS
            .iter()
            .any(|a| a.eq_ignore_ascii_case(e))
    })
}

fn extension_override(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    EXTENSION_OVERRIDES
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|(_, mime)| *mime)
}

/// The essence of a raw MIME — everything before any `;` parameter — as a
/// borrow of the same static. `Mime::essence_str` would do this too, but only
/// off an owned `Mime`, which is why the raw form is what gets asked for.
fn essence(raw: &'static str) -> &'static str {
    match raw.split_once(';') {
        Some((essence, _)) => essence.trim_end(),
        None => raw,
    }
}

/// Infer a MIME type from a path plus the file's leading bytes.
///
/// `head` bounds both content checks: under 262 bytes (`infer`'s longest
/// signature) some formats become undetectable.
///
/// A `None` result is a real answer, not a "don't know": the content pass
/// stores it and never re-derives it.
///
/// `&'static str` rather than `String`: every answer comes from one of three
/// static tables ([`EXTENSION_OVERRIDES`], `mime_guess`'s, `infer`'s) or is a
/// literal, and this runs once per indexed file — an owned copy here was a
/// heap allocation per file for a string nobody mutates.
pub fn guess_mime_from_head(path: &Path, head: &[u8]) -> Option<&'static str> {
    if let Some(m) = extension_override(path) {
        return Some(m);
    }
    // `first_raw`, not `first`: the owned `Mime` exists only to be borrowed
    // from, and its `essence_str` cannot outlive it.
    let by_extension = mime_guess::from_path(path).first_raw().and_then(|raw| {
        let s = essence(raw);
        (!s.is_empty() && s != "application/octet-stream").then_some(s)
    });
    if !extension_is_ambiguous(path) && by_extension.is_some() {
        return by_extension;
    }
    if let Some(t) = infer::get(head) {
        let magic = t.mime_type();
        // `infer`'s generic OLE-container answer is less specific than the
        // extension's: a real `.pot` must resolve to vnd.ms-powerpoint
        // (which the office extractor claims), not a MIME nothing claims.
        if magic == "application/x-ole-storage" && by_extension.is_some() {
            return by_extension;
        }
        return Some(magic);
    }
    if crate::textenc::looks_like_text(head) {
        return Some("text/plain");
    }
    // Only an ambiguous extension still has an answer left to fall back on.
    by_extension
}

/// The longest MIME any table here holds is 73 bytes; 128 leaves room and
/// keeps [`LowerMime`] a stack value. Anything longer names no format this
/// classifies, so it is matched as it came rather than growing a heap copy.
const MAX_MIME_LEN: usize = 128;

/// A MIME lowercased without allocating.
///
/// Nearly every MIME reaching the classifiers is already lowercase —
/// [`guess_mime_from_head`] answers from static tables — so the common path
/// borrows and only a genuinely mixed-case string is copied into the buffer.
/// This runs a few times per indexed file; `to_ascii_lowercase` there was a
/// heap allocation apiece.
pub(crate) struct LowerMime {
    buf: [u8; MAX_MIME_LEN],
    len: usize,
    /// Set when the input was already lowercase (or too long to copy), in
    /// which case [`LowerMime::as_str`] hands the original straight back.
    borrowed: bool,
}

impl LowerMime {
    pub(crate) fn new(mime: &str) -> LowerMime {
        let mut lower = LowerMime {
            buf: [0; MAX_MIME_LEN],
            len: mime.len(),
            borrowed: true,
        };
        if mime.len() <= MAX_MIME_LEN && mime.bytes().any(|b| b.is_ascii_uppercase()) {
            lower.buf[..mime.len()].copy_from_slice(mime.as_bytes());
            // ASCII-only folding: a multi-byte sequence is left untouched, so
            // what comes out is still the UTF-8 that went in.
            lower.buf[..mime.len()].make_ascii_lowercase();
            lower.borrowed = false;
        }
        lower
    }

    pub(crate) fn as_str<'a>(&'a self, original: &'a str) -> &'a str {
        if self.borrowed {
            return original;
        }
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or(original)
    }
}

/// Map a MIME string to a [`FileType`] bitmask. Ported from Baloo's
/// `basicindexingjob.cpp:typesForMimeType`.
pub fn mime_to_type(mime: &str) -> FileType {
    let lower = LowerMime::new(mime);
    let lower = lower.as_str(mime);
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
            // HTML counts as a document too in Baloo.
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
        // Outlook saved messages and compiled HTML help are documents.
        "vnd.ms-outlook" | "vnd.ms-htmlhelp" => {
            t |= FileType::DOCUMENT;
        }
        // Archives, including the Windows installer/cabinet formats.
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
        // XHTML is text and a document, whichever top level it arrives under.
        "xhtml+xml" => {
            t |= FileType::TEXT | FileType::DOCUMENT;
        }
        // Everything the plaintext extractor claims beyond `text/*` (see
        // `EXTRA_TEXT_MIMES` and the cross-check test below). Keyed on the
        // subtype alone, so playlists stay AUDIO|TEXT and SVG IMAGE|TEXT.
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

    #[test]
    fn guess_mime_by_extension() {
        use std::path::PathBuf;
        let by_ext = |n: &str| guess_mime_from_head(&PathBuf::from(n), b"").unwrap_or_default();
        assert_eq!(by_ext("a.txt"), "text/plain");
        assert_eq!(by_ext("a.png"), "image/png");
        assert_eq!(by_ext("a.mp3"), "audio/mpeg");
    }

    /// Every override must land on a type the plaintext extractor accepts.
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
                PlaintextExtractor.supports(mime),
                "{} -> {} is not extractable as text",
                name,
                mime
            );
        }
    }

    #[test]
    fn extension_overrides_are_case_insensitive() {
        use std::path::PathBuf;
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("DEPLOY.PS1"), b""),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("Build.Bat"), b""),
            Some("text/plain")
        );
    }

    /// A `.ps1` holding something `infer` would recognise is still a script.
    #[test]
    fn extension_overrides_beat_magic_bytes() {
        use std::path::PathBuf;
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("a.ps1"), b"Write-Host hi"),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("a.ps1"), b"%PDF-1.7"),
            Some("text/plain")
        );
    }

    #[test]
    fn sql_dumps_are_extractable() {
        use crate::extract::{plaintext::PlaintextExtractor, Extractor};
        use std::path::PathBuf;
        let mime = guess_mime_from_head(&PathBuf::from("schema.sql"), b"").unwrap();
        assert!(PlaintextExtractor.supports(mime), "{}", mime);
    }

    /// The content pass trusts the stored MIME and never reopens the file —
    /// sound only if a `hash_length`-sized head recognises a format from its
    /// magic bytes. Pinned for extensionless files, where bytes are all.
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
            let path = PathBuf::from(format!("/tmp/qs-sniff-{}", tag));
            let mut body = magic.to_vec();
            body.resize(head_bytes, 0);
            assert_eq!(
                guess_mime_from_head(&path, &body),
                Some(*expected),
                "{} must be detectable from a default-sized head",
                tag
            );
        }
    }

    /// A head starved below `infer`'s longest signature legitimately degrades
    /// — but a binary head must stay `None` rather than become a wrong guess.
    #[test]
    fn a_head_shorter_than_the_signature_declines_rather_than_guessing() {
        use std::path::PathBuf;
        let path = PathBuf::from("/tmp/qs-sniff-truncated");
        assert_eq!(guess_mime_from_head(&path, b""), None);
        // A PNG magic truncated to two bytes: no magic match, no text guess.
        assert_eq!(guess_mime_from_head(&path, &[0x89, 0x00]), None);
        assert_eq!(
            guess_mime_from_head(&path, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
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

    /// Real dispatch (`extract_complete_head`, not `supports`), so the
    /// svg/m3u/pls cases prove the plaintext-first registration *order*.
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
                .extract_head_to_string(&path, mime, head)
                .unwrap_or_else(|| {
                    panic!(
                        "{} -> {} not claimed by a head-capable extractor",
                        name, mime
                    )
                })
                .unwrap_or_else(|e| panic!("{} -> {} failed to extract: {}", name, mime, e));
            assert!(
                !extracted.is_empty(),
                "{} -> {} extracted no text",
                name,
                mime
            );
        }
    }

    #[test]
    fn extensionless_files_sniff_by_content() {
        use std::path::PathBuf;
        let readme = PathBuf::from("README");
        assert_eq!(
            guess_mime_from_head(&readme, b"QuickSearch indexes your files.\n"),
            Some("text/plain")
        );
        let makefile = PathBuf::from("Makefile");
        assert_eq!(
            guess_mime_from_head(&makefile, b"all:\n\tcargo build\n"),
            Some("text/plain")
        );
        let blob = PathBuf::from("blob");
        assert_eq!(
            guess_mime_from_head(&blob, &[0x00, 0x01, 0x02, 0xFF]),
            None
        );
    }

    /// Formats made of high bytes clear the binary guard yet are not text;
    /// before the strict sniff they were stored as mojibake.
    #[test]
    fn high_byte_binary_is_not_sniffed_as_text() {
        use std::path::PathBuf;

        // Head of a real protobuf-framed GPS log; nothing else claims `.pb`,
        // so this reaches the sniff.
        let mut pb = b"\x10\n\x02v1\x10\x01\x18\xe2\xe3\xfc\xd3\x9d\xca\x97\xe4\x189\x08".to_vec();
        pb.extend_from_slice(b"\x12*$GNGGA,181558.00,,,,,0,00,99.99,,,,,,*78\r\n");
        assert_eq!(guess_mime_from_head(&PathBuf::from("rtk.pb"), &pb), None);

        // An extension the MIME table knows never reaches the sniff, so
        // legacy-encoded documents still type as text.
        let latin1 = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.";
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("notes.txt"), latin1),
            Some("text/plain")
        );

        // A `.pb` that really is UTF-8 text still indexes.
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("notes.pb"), b"just some words\n"),
            Some("text/plain")
        );
    }

    #[test]
    fn ambiguous_extensions_resolve_by_content_both_ways() {
        use std::path::PathBuf;

        let ts_source = b"export function hi(): string { return 'hi'; }\n";
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("app.ts"), ts_source),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("APP.TS"), ts_source),
            Some("text/plain")
        );
        // An MPEG transport stream: no magic matcher, fails the text sniff,
        // so the extension answers.
        let mut ts_video = vec![0u8; 376];
        ts_video[0] = 0x47;
        ts_video[188] = 0x47;
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("clip.ts"), &ts_video),
            Some("video/vnd.dlna.mpeg-tts")
        );

        assert_eq!(
            guess_mime_from_head(
                &PathBuf::from("go.mod"),
                b"module example.com/x\n\ngo 1.22\n"
            ),
            Some("text/plain")
        );

        // gettext template vs PowerPoint template.
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("app.pot"), b"msgid \"hello\"\nmsgstr \"\"\n"),
            Some("text/plain")
        );
        let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0x00, 0x00];
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("slides.pot"), &ole),
            Some("application/vnd.ms-powerpoint")
        );

        assert_eq!(
            guess_mime_from_head(&PathBuf::from("cpu.vhd"), b"entity cpu is\nend cpu;\n"),
            Some("text/plain")
        );
        assert_eq!(
            guess_mime_from_head(&PathBuf::from("disk.vhd"), &[0x00, 0x01, 0x02, 0x03]),
            Some("application/x-virtualbox-vhd")
        );
    }

    /// Everything the plaintext extractor claims must carry the TEXT bit, or
    /// `type:Text` silently misses content-indexed files. Iterates the actual
    /// claim list so the two can never drift apart.
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
