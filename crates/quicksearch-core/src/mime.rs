//! MIME type guessing and `FileType` bitmask classification.
//!
//! Two stages:
//! 1. [`guess_mime_from_head`] infers a MIME type — extension first via
//!    `mime_guess`, falling back to magic-byte sniffing via `infer` for files
//!    whose extension is missing or ambiguous.
//! 2. [`mime_to_type`] maps a MIME string to a [`FileType`] bitmask so a single
//!    file can belong to multiple categories (e.g. a `.docx` is Document|Text).
//!
//! The magic bytes are always ones the caller already holds. Indexing reads
//! the head of every new or changed file to hash it, and those are the same
//! bytes `infer` wants, so there is no path-based variant that goes back to
//! disk for them — that was a second open/read/close per undetectable file.

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

/// Extensions `mime_guess` gets wrong or does not know, and what they really
/// are.
///
/// Consulted *before* `mime_guess`, because for these the table is not a
/// fallback but a correction. Everything here is plain text that would
/// otherwise get no content indexing at all:
///
/// - `.ps1`/`.psm1`/`.psd1` and `.url` are simply absent from `mime_guess`,
///   and `infer` only knows binary magic, so they end up with no MIME — and
///   [`crate::extract::Registry`] has no extractor to offer, so the file is
///   marked "not applicable". PowerShell is the most common script type on a
///   Windows machine.
/// - `.bat` maps to `application/x-msdownload`, i.e. an executable. It is a
///   text file, and the plaintext extractor rightly refuses the executable
///   type. (`.cmd` already resolves to `text/plain`; it is listed so the pair
///   cannot drift.)
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
/// Extension first — an override table, then `mime_guess` — and magic bytes
/// only when those come up empty or say `application/octet-stream`.
///
/// `head` is whatever the caller already read; indexing passes the same buffer
/// it hashes. It bounds magic-byte detection, so a caller that supplies fewer
/// than 262 bytes (`infer`'s longest signature) can get `None` where a longer
/// head would have matched. The indexer's `hash_length` defaults to 8 KiB —
/// exactly what `infer` itself reads from a path — so at default config this
/// is as good as opening the file, and strictly cheaper.
///
/// A `None` result is a real answer, not a "don't know": the content pass
/// stores it and does not re-derive it (see
/// [`crate::file_handling::extract_and_store`]).
pub fn guess_mime_from_head(path: &Path, head: &[u8]) -> Option<String> {
    if let Some(m) = extension_override(path) {
        return Some(m.to_string());
    }
    if let Some(g) = mime_guess::from_path(path).first() {
        let s = g.essence_str();
        if !s.is_empty() && s != "application/octet-stream" {
            return Some(s.to_string());
        }
    }
    infer::get(head).map(|t| t.mime_type().to_string())
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
            // HTML counts as a document too in Baloo.
            if sub == "html" || sub == "xhtml+xml" {
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
        // application/xml is structured text
        "xml" | "json" | "javascript" | "x-shellscript" | "x-python" => {
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
        let t = mime_to_type(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        );
        assert!(t.contains(FileType::DOCUMENT));
    }

    #[test]
    fn xlsx_is_spreadsheet_and_document() {
        let t = mime_to_type(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        );
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
        for n in ["Audio", "Image", "Video", "Document", "Text", "Archive",
                  "Spreadsheet", "Presentation", "Folder"] {
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
            ("png", &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a], "image/png"),
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
    /// signature and detection legitimately degrades. Documented behaviour of
    /// a non-default `hash_length`, not a bug — but it must stay a `None`
    /// rather than a wrong guess.
    #[test]
    fn a_head_shorter_than_the_signature_declines_rather_than_guessing() {
        use std::path::PathBuf;
        let path = PathBuf::from("/tmp/qs-sniff-truncated");
        assert_eq!(guess_mime_from_head(&path, b"").as_deref(), None);
        assert_eq!(guess_mime_from_head(&path, &[0x89]).as_deref(), None);
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
}
