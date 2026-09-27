//! Plain-text extraction: UTF-8, BOM-marked UTF-16, and detected legacy
//! charsets are decoded to UTF-8 for storage (see [`crate::textenc`]).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::{ExtractError, Extractor, Scratch};

/// Non-`text/*` MIMEs the plaintext extractor claims. Every entry must be
/// emitted by some MIME source and map to a [`crate::mime::FileType`]
/// containing TEXT; the cross-check tests in `mime.rs` enforce both. The
/// `audio/*` and `image/*` entries rely on this extractor registering before
/// the audio and image extractors; first match wins.
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

/// Decode `bytes`, a prefix of the file when `truncated`, into `out`. Valid
/// UTF-8 is moved, not copied, which is why plaintext reads into a fresh
/// buffer rather than the worker's scratch: reusing one would trade an
/// allocation for a full-length `memcpy` per file.
fn decode_into(
    bytes: Vec<u8>,
    path: &Path,
    truncated: bool,
    out: &mut String,
) -> Result<(), ExtractError> {
    super::append(out, crate::textenc::decode_text(bytes, path, truncated)?);
    Ok(())
}

pub struct PlaintextExtractor;

/// Read `size` bytes from `f`, never more than `cap`. A short read is not an
/// error: a file that shrank keeps its prefix.
///
/// Zero-filled, and still the faster shape: `take(size).read_to_end` into a
/// reserved `Vec` skips the fill but starts at 8 KiB reads and doubles, so
/// over `/usr/include` it made 18,431 `read` calls against 15,419 here and
/// ran 8% slower (`extractprobe`). Measured and rejected.
fn read_sized(f: &mut File, size: usize, cap: usize, path: &Path) -> Result<Vec<u8>, ExtractError> {
    let size = size.min(cap);
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
    Ok(buf)
}

impl Extractor for PlaintextExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime.starts_with("text/") || EXTRA_TEXT_MIMES.contains(&mime)
    }

    /// A file that shrank since the `fstat` keeps its prefix; one that grew is
    /// read to the sized length; its mtime moved, so the next run re-extracts.
    fn extract(
        &self,
        path: &Path,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        // Only the prefix that can survive truncation to `limits().text` is
        // read: UTF-16 is the widest input per byte of UTF-8 output (2:1), so
        // no file contributes more than twice the limit. Reading a 40 MiB log
        // whole to keep 256 KiB of it was the content pass's peak memory.
        let limits = scratch.limits();
        let cap = limits.read.min(limits.text.saturating_mul(2));
        let mut f =
            File::open(path).map_err(|e| format!("plaintext read {}: {}", path.display(), e))?;
        let size = f
            .metadata()
            .map_err(|e| format!("plaintext read {}: {}", path.display(), e))?
            .len() as usize;

        // procfs/sysfs/some FUSE mounts report zero size for files with content;
        // only they pay the read-to-EOF probe, capped against endless streams.
        if size == 0 {
            let mut buf = Vec::new();
            f.take(cap as u64)
                .read_to_end(&mut buf)
                .map_err(|e| format!("plaintext read {}: {}", path.display(), e))?;
            let truncated = buf.len() == cap;
            return decode_into(buf, path, truncated, out);
        }

        let buf = read_sized(&mut f, size, cap, path)?;
        decode_into(buf, path, size > cap, out)
    }

    /// The head belongs to the walk worker and is reused for the next file,
    /// so it is decoded borrowed rather than copied to be given away.
    fn extract_from_head(
        &self,
        path: &Path,
        head: &[u8],
        out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        Some(crate::textenc::decode_borrowed_text(head, path).map(|text| super::append(out, text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Scratch {
        Scratch::new(&crate::config::Config::default())
    }

    /// The default `maximum_text_file_size`, which bounds a read.
    fn max_read() -> usize {
        scratch().limits().read
    }

    fn extract(path: &Path) -> Result<String, ExtractError> {
        crate::extract::extract_text(&PlaintextExtractor, path)
    }

    fn extract_from_head(path: &Path, head: &[u8]) -> Option<Result<String, ExtractError>> {
        let mut out = String::new();
        let outcome = PlaintextExtractor.extract_from_head(path, head, &mut out);
        outcome.map(|r| r.map(|()| out))
    }

    fn tmp(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = crate::testutil::scratch_dir(tag).join("sample.txt");
        crate::testutil::touch(&p, body);
        p
    }

    #[test]
    fn reads_utf8_file() {
        let p = tmp("basic", b"hello world");
        let c = extract(&p).unwrap();
        assert_eq!(c, "hello world");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn head_extraction_matches_reading_the_file() {
        let p = tmp(
            "agree",
            b"shared body with unicode: caf\xc3\xa9 \xe2\x9c\x93",
        );
        let from_disk = extract(&p).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        let from_head = extract_from_head(&p, &bytes)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk, from_head);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn both_paths_reject_binary_and_name_the_file() {
        // A NUL keeps this undecodable now that legacy charsets decode.
        let body = [0x68, 0x69, 0x00, 0xff];
        let p = tmp("binary", &body);
        let disk_err = extract(&p).unwrap_err();
        let head_err = extract_from_head(&p, &body)
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
        let from_disk = extract(&p).unwrap();
        let from_head = extract_from_head(&p, body)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk, from_head);
        assert_eq!(from_disk, "une journée agréable près de la rivière");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn utf16le_bom_decodes_via_both_paths() {
        let src = "Windows Registry Editor Version 5.00\r\n[HKEY_CURRENT_USER\\Software]\r\n";
        let mut body = vec![0xFF, 0xFE];
        body.extend(src.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let p = tmp("utf16", &body);
        let from_disk = extract(&p).unwrap();
        let from_head = extract_from_head(&p, &body)
            .unwrap()
            .unwrap();
        assert_eq!(from_disk, from_head);
        assert_eq!(
            from_disk, src,
            "stored text is the UTF-8 decode, BOM stripped"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn reads_a_file_larger_than_one_buffer_completely() {
        let body = "abcdefgh".repeat(200 * 1024 / 8);
        let p = tmp("large", body.as_bytes());
        let c = extract(&p).unwrap();
        assert_eq!(c.len(), body.len());
        assert_eq!(c, body);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn an_empty_file_extracts_to_empty_text() {
        let p = tmp("empty", b"");
        assert_eq!(extract(&p).unwrap(), "");
        assert_eq!(extract_from_head(&p, &[]).unwrap().unwrap(), "");
        std::fs::remove_file(&p).ok();
    }

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
        let c = extract(p).unwrap();
        assert!(
            c.contains("Name:"),
            "content must survive a zero st_size, got {} bytes",
            c.len()
        );
    }

    #[test]
    fn a_file_that_shrank_after_sizing_keeps_its_prefix() {
        let p = tmp("shrink", &vec![b'x'; 4096]);
        let f = File::options().write(true).open(&p).unwrap();
        // A mid-extract truncate is not reproducible; assert the property directly.
        f.set_len(10).unwrap();
        drop(f);
        let c = extract(&p).unwrap();
        assert_eq!(c, "xxxxxxxxxx", "a shrunk file reads short, not fatal");
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

    #[test]
    fn a_sized_read_stops_at_the_cap() {
        let body = vec![b'x'; 4096];
        let p = tmp("cap", &body);
        let mut f = File::open(&p).unwrap();
        let out = read_sized(&mut f, body.len(), 100, &p).unwrap();
        assert_eq!(out.len(), 100, "read past the cap");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_short_read_keeps_what_was_there() {
        let p = tmp("short", b"only ten!!");
        let mut f = File::open(&p).unwrap();
        let out = read_sized(&mut f, 1_000_000, max_read(), &p).unwrap();
        assert_eq!(out, b"only ten!!");
        std::fs::remove_file(&p).ok();
    }

    /// Twice the text limit: the most any file can contribute to stored text.
    fn prefix_cap() -> usize {
        scratch().limits().text * 2
    }

    #[test]
    fn a_file_far_over_the_text_limit_reads_only_the_keepable_prefix() {
        let body = vec![b'z'; prefix_cap() * 3];
        let p = tmp("prefix", &body);
        let out = extract(&p).unwrap();
        assert_eq!(out.len(), prefix_cap(), "read past what can be kept");
        std::fs::remove_file(&p).ok();
    }

    /// A read that cuts a multibyte character must not demote a UTF-8 file to
    /// a legacy charset, which would decode the whole prefix as mojibake.
    #[test]
    fn a_character_cut_by_the_prefix_read_is_dropped_not_misdecoded() {
        let mut body = vec![b'a'; prefix_cap() - 1];
        body.extend_from_slice("é and more after the cut".as_bytes());
        let p = tmp("cutchar", &body);
        let out = extract(&p).unwrap();
        assert_eq!(out.len(), prefix_cap() - 1, "the cut é's stub is dropped");
        assert!(
            out.bytes().all(|b| b == b'a'),
            "decoded as UTF-8, not legacy"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_file_under_the_cap_is_read_whole() {
        let body = vec![b'y'; 4096];
        let p = tmp("uncapped", &body);
        let out = extract(&p).unwrap();
        assert_eq!(out.len(), 4096);
        std::fs::remove_file(&p).ok();
    }
}
