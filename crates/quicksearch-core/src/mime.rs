//! MIME type guessing and `FileType` bitmask classification.
//!
//! Two stages:
//! 1. [`guess_mime`] infers a MIME type from a path — extension first via
//!    `mime_guess`, falling back to magic-byte sniffing via `infer` for files
//!    whose extension is missing or ambiguous.
//! 2. [`mime_to_type`] maps a MIME string to a [`FileType`] bitmask so a single
//!    file can belong to multiple categories (e.g. a `.docx` is Document|Text).

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

/// Guess a MIME type for a path on disk.
///
/// Tries extension-based lookup via `mime_guess` first (cheap, no I/O). If
/// that returns nothing or a generic `application/octet-stream`, and the file
/// is readable, falls back to `infer` magic-byte detection (reads a small
/// prefix of the file).
///
/// Returns `None` if no guess can be made.
pub fn guess_mime(path: &Path) -> Option<String> {
    if let Some(g) = mime_guess::from_path(path).first() {
        let s = g.essence_str();
        if !s.is_empty() && s != "application/octet-stream" {
            return Some(s.to_string());
        }
    }
    // Magic-byte fallback. `infer::get_from_path` handles errors by returning None.
    if let Ok(Some(t)) = infer::get_from_path(path) {
        return Some(t.mime_type().to_string());
    }
    None
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
        // Archives
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
        | "x-rpm" => {
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

    #[test]
    fn guess_mime_by_extension() {
        use std::path::PathBuf;
        assert_eq!(guess_mime(&PathBuf::from("a.txt")).as_deref(), Some("text/plain"));
        assert_eq!(guess_mime(&PathBuf::from("a.png")).as_deref(), Some("image/png"));
        assert_eq!(guess_mime(&PathBuf::from("a.mp3")).as_deref(), Some("audio/mpeg"));
    }
}
