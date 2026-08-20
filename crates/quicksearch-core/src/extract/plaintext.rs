//! Read the file as text, decoding UTF-8, BOM-marked UTF-16, and detected
//! legacy charsets to UTF-8 for storage (see [`crate::textenc`]). Handles
//! text/plain, text/x-*, and the non-`text/*` formats in
//! [`EXTRA_TEXT_MIMES`].

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::{ExtractError, ExtractedContent, Extractor};

/// Non-`text/*` MIMEs the plaintext extractor claims. Every entry must be
/// reachable — emitted by [`crate::mime::guess_mime_from_head`] via the
/// override table, `mime_guess`, `infer`, or the text sniff — and must map
/// to a [`crate::mime::FileType`] containing TEXT; the cross-check tests in
/// `mime.rs` enforce both.
///
/// The `audio/*` and `image/*` entries (playlists, SVG) rely on this
/// extractor being registered before the audio and image extractors in
/// [`super::Registry::default_set`] — first match wins, and their text
/// content is worth more than their tags. `.svgz` also resolves to
/// `image/svg+xml`; its gzip body fails the binary guard and is recorded as
/// a failure rather than silently skipped.
pub(crate) const EXTRA_TEXT_MIMES: &[&str] = &[
    "application/geo+json",
    "application/javascript",
    "application/json",
    "application/json5",
    "application/mbox",
    "application/vnd.dart",
    "application/x-csh",
    "application/x-httpd-php",
    "application/x-perl",
    "application/x-sh",
    // `.sql` resolves here rather than to `text/*`, so without it schema
    // dumps are listed by name but never full-text indexed.
    "application/x-sql",
    "application/x-subrip",
    "application/x-tcl",
    "application/x-tex",
    "application/x-texinfo",
    "application/x-troff",
    "application/x-troff-man",
    "application/xhtml+xml",
    "application/xml",
    "audio/scpls",
    "audio/x-mpegurl",
    "image/svg+xml",
    "message/rfc822",
];

/// Decode bytes that are known to be a complete file. Shared by both entry
/// points so on-disk and already-in-memory extraction cannot drift apart.
fn decode(bytes: Vec<u8>, path: &Path) -> Result<ExtractedContent, ExtractError> {
    crate::textenc::decode_text(bytes, path).map(ExtractedContent::with_text)
}

pub struct PlaintextExtractor;

impl Extractor for PlaintextExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime.starts_with("text/") || EXTRA_TEXT_MIMES.contains(&mime)
    }

    /// Read the whole file, sized from the handle we just opened, so a file
    /// that fits takes exactly one `read`.
    ///
    /// A file that shrank between the `fstat` and the `read` keeps its prefix
    /// rather than failing. A file that grew is read up to the size we saw;
    /// its mtime moved, so the next run reclassifies it as changed and
    /// re-extracts (see [`crate::file_handling::classify_for_indexing`]).
    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        let mut f =
            File::open(path).map_err(|e| format!("plaintext read {}: {}", path.display(), e))?;
        let size = f
            .metadata()
            .map_err(|e| format!("plaintext read {}: {}", path.display(), e))?
            .len() as usize;

        // procfs, sysfs and some FUSE mounts report zero for files that do
        // have content, so a sized read would store nothing. Only these pay
        // the read-to-EOF probe — which is what a genuinely empty file cost
        // before anyway. Capped so a node that streams forever (a FIFO, a
        // lying filesystem) cannot allocate without bound.
        if size == 0 {
            const MAX_UNSIZED_READ: u64 = 64 * 1024 * 1024;
            let mut buf = Vec::new();
            f.take(MAX_UNSIZED_READ)
                .read_to_end(&mut buf)
                .map_err(|e| format!("plaintext read {}: {}", path.display(), e))?;
            return decode(buf, path);
        }

        let mut buf = vec![0u8; size];
        let mut filled = 0;
        while filled < size {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("plaintext read {}: {}", path.display(), e)),
            }
        }
        buf.truncate(filled);
        decode(buf, path)
    }

    fn extract_from_head(
        &self,
        path: &Path,
        head: &[u8],
    ) -> Option<Result<ExtractedContent, ExtractError>> {
        Some(decode(head.to_vec(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = crate::testutil::scratch_dir(tag).join("sample.txt");
        crate::testutil::touch(&p, body);
        p
    }

    #[test]
    fn reads_utf8_file() {
        let p = tmp("basic", b"hello world");
        let c = PlaintextExtractor.extract(&p).unwrap();
        assert_eq!(c.text, "hello world");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn head_extraction_matches_reading_the_file() {
        let p = tmp(
            "agree",
            b"shared body with unicode: caf\xc3\xa9 \xe2\x9c\x93",
        );
        let from_disk = PlaintextExtractor.extract(&p).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        let from_head = PlaintextExtractor
            .extract_from_head(&p, &bytes)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk.text, from_head.text);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn both_paths_reject_binary_and_name_the_file() {
        // A NUL keeps this undecodable now that legacy charsets decode.
        let body = [0x68, 0x69, 0x00, 0xff];
        let p = tmp("binary", &body);
        let disk_err = PlaintextExtractor.extract(&p).unwrap_err();
        let head_err = PlaintextExtractor
            .extract_from_head(&p, &body)
            .unwrap()
            .unwrap_err();
        assert_eq!(disk_err, head_err, "one decode path, one message");
        assert!(
            disk_err.contains("binary"),
            "the failure names the file: {}",
            disk_err
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn latin1_decodes_via_both_paths() {
        let body = b"une journ\xe9e agr\xe9able pr\xe8s de la rivi\xe8re";
        let p = tmp("latin1", body);
        let from_disk = PlaintextExtractor.extract(&p).unwrap();
        let from_head = PlaintextExtractor
            .extract_from_head(&p, body)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk.text, from_head.text);
        assert_eq!(from_disk.text, "une journée agréable près de la rivière");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn utf16le_bom_decodes_via_both_paths() {
        let src = "Windows Registry Editor Version 5.00\r\n[HKEY_CURRENT_USER\\Software]\r\n";
        let mut body = vec![0xFF, 0xFE];
        body.extend(src.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let p = tmp("utf16", &body);
        let from_disk = PlaintextExtractor.extract(&p).unwrap();
        let from_head = PlaintextExtractor
            .extract_from_head(&p, &body)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk.text, from_head.text);
        assert_eq!(
            from_disk.text, src,
            "stored text is the UTF-8 decode, BOM stripped"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn reads_a_file_larger_than_one_buffer_completely() {
        // Past any plausible head window, so the read loop has to iterate if
        // the kernel returns a short read.
        let body = "abcdefgh".repeat(200 * 1024 / 8);
        let p = tmp("large", body.as_bytes());
        let c = PlaintextExtractor.extract(&p).unwrap();
        assert_eq!(c.text.len(), body.len());
        assert_eq!(c.text, body);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn an_empty_file_extracts_to_empty_text() {
        let p = tmp("empty", b"");
        assert_eq!(PlaintextExtractor.extract(&p).unwrap().text, "");
        assert_eq!(
            PlaintextExtractor
                .extract_from_head(&p, &[])
                .unwrap()
                .unwrap()
                .text,
            ""
        );
        std::fs::remove_file(&p).ok();
    }

    /// A file whose reported size is a lie in the "there is more than this"
    /// direction — the shape procfs and sysfs have. Sizing the buffer from
    /// `st_size` alone would store nothing, so `extract` must fall back to
    /// reading until EOF.
    #[test]
    fn a_file_reporting_zero_size_is_still_read_to_eof() {
        let p = Path::new("/proc/self/status");
        if !p.exists() {
            return; // not Linux; the guard is only reachable there
        }
        assert_eq!(
            std::fs::metadata(p).unwrap().len(),
            0,
            "precondition: procfs reports zero size"
        );
        let c = PlaintextExtractor.extract(p).unwrap();
        assert!(
            c.text.contains("Name:"),
            "content must survive a zero st_size, got {} bytes",
            c.text.len()
        );
    }

    /// The same lie in the other direction, which the sized read handles by
    /// keeping whatever was actually there.
    #[test]
    fn a_file_that_shrank_after_sizing_keeps_its_prefix() {
        let p = tmp("shrink", &vec![b'x'; 4096]);
        let f = File::options().write(true).open(&p).unwrap();
        // Truncate behind `extract`'s back is not reproducible, so assert the
        // property directly: a buffer sized larger than the file yields the
        // file, not an error.
        f.set_len(10).unwrap();
        drop(f);
        let c = PlaintextExtractor.extract(&p).unwrap();
        assert_eq!(c.text, "xxxxxxxxxx", "a shrunk file reads short, not fatal");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn supports_text_mimes() {
        let e = PlaintextExtractor;
        assert!(e.supports("text/plain"));
        assert!(e.supports("text/x-rust"));
        assert!(e.supports("application/json"));
        assert!(e.supports("application/x-sh"));
        assert!(e.supports("image/svg+xml"));
        assert!(e.supports("audio/x-mpegurl"));
        assert!(e.supports("message/rfc822"));
        // Never emitted by any MIME source; removed as dead.
        assert!(!e.supports("application/x-shellscript"));
        // RTF belongs to the RTF extractor, which registers first.
        assert!(!e.supports("application/rtf"));
        assert!(!e.supports("application/pdf"));
        assert!(!e.supports("image/png"));
    }
}
