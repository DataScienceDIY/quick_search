//! Text detection and charset decoding, shared by the MIME sniff and the
//! plaintext extractor.
//!
//! Both callers route through one classifier so they cannot drift: a head
//! that [`looks_like_text`] accepts is guaranteed to decode via
//! [`decode_text`] — every class except `Binary` decodes unconditionally
//! (strict UTF-8 after validation, BOM'd and legacy decodes are
//! lossy-with-replacement and never fail).
//!
//! Classification order is load-bearing:
//!
//! 1. **BOM** ([`encoding_rs::Encoding::for_bom`]) — before the binary
//!    guard, because UTF-16 text is full of NUL bytes the guard would
//!    reject.
//! 2. **Binary guard** — any NUL byte, or control bytes (outside
//!    `\t \n \r`, with ESC tolerated for ANSI-colored logs) above 10% of
//!    the buffer. UTF-16 without a BOM fails here by design.
//! 3. **Strict UTF-8** — with one tolerance for the sniff: a head is a
//!    prefix of the file (indexing hashes the first `hash_length` bytes and
//!    sniffs the same buffer), so a multibyte sequence cut off by the end
//!    of the buffer does not disqualify it.
//! 4. **Legacy** — everything else. [`decode_text`] runs charset detection
//!    (chardetng, windows-1252 floor) and decodes with replacement.
//!
//! The sniff sees only the head, so a file with a text head and a binary
//! tail classifies as text and then fails the whole-file decode; that lands
//! as a FAILED row with a reason, the normal shape of head-based
//! classification.

use std::path::Path;

enum TextClass {
    /// Strict UTF-8 (modulo the truncated-tail tolerance).
    Utf8,
    /// Starts with a BOM; decode with this encoding.
    Bom(&'static encoding_rs::Encoding),
    /// Not UTF-8 but passes the binary guard: charset detection will decode.
    Legacy,
    /// Fails the binary guard.
    Binary,
}

/// Control bytes tolerated in text: ordinary whitespace, plus ESC because
/// ANSI-colored logs are text worth indexing.
fn is_benign_control(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\r' | 0x1B)
}

/// Classify `bytes` as text or binary. `truncated` marks a buffer that may
/// be a prefix of the file rather than its entirety.
fn classify(bytes: &[u8], truncated: bool) -> TextClass {
    if let Some((enc, _bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return TextClass::Bom(enc);
    }

    // Binary guard. NUL never appears in text of any supported encoding
    // (UTF-16 was handled above, by BOM or not at all); a run of other
    // control bytes marks compressed or machine data that merely lacks NULs.
    let mut suspect = 0usize;
    for &b in bytes {
        if b == 0 {
            return TextClass::Binary;
        }
        if (b < 0x20 && !is_benign_control(b)) || b == 0x7F {
            suspect += 1;
        }
    }
    if suspect * 10 > bytes.len() {
        return TextClass::Binary;
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => TextClass::Utf8,
        // `error_len() == None` means the only defect is a multibyte
        // sequence running off the end of the buffer — for a truncated head
        // that is the file boundary's fault, not the file's.
        Err(e) if truncated && e.error_len().is_none() => TextClass::Utf8,
        Err(_) => TextClass::Legacy,
    }
}

/// Whether `head` — a possibly-truncated prefix of a file — reads as text.
///
/// Cheap: a byte scan plus UTF-8 validation, no charset detection. An empty
/// head proves nothing and answers `false`; that keeps zero-size
/// extensionless files (procfs included) unclassified rather than blanket
/// `text/plain`.
pub fn looks_like_text(head: &[u8]) -> bool {
    !head.is_empty() && !matches!(classify(head, true), TextClass::Binary)
}

/// Decode a complete file's bytes to UTF-8 for storage.
///
/// Takes ownership so the dominant valid-UTF-8 case is a zero-copy move.
/// `path` is used only to name the file in the error, matching the
/// extractor error convention.
pub fn decode_text(bytes: Vec<u8>, path: &Path) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    match classify(&bytes, false) {
        // Cannot fail: classify ran strict validation with truncated=false.
        TextClass::Utf8 => Ok(String::from_utf8(bytes).expect("classified as UTF-8")),
        TextClass::Bom(enc) => {
            // BOM-aware decode: strips the BOM, replaces malformed
            // sequences (e.g. a truncated trailing code unit) with U+FFFD.
            let (text, _, _) = enc.decode(&bytes);
            Ok(text.into_owned())
        }
        TextClass::Legacy => {
            // ISO-2022-JP detection is safe here: the browser caveat about
            // it concerns script-running web content, not indexed files.
            let mut det =
                chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
            det.feed(&bytes, true);
            // Deny UTF-8: strict UTF-8 was already ruled out, so a UTF-8
            // guess could only mean malformed UTF-8.
            let enc = det.guess(None, chardetng::Utf8Detection::Deny);
            let (text, _, _) = enc.decode(&bytes);
            Ok(text.into_owned())
        }
        TextClass::Binary => Err(format!(
            "plaintext read {}: binary content",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p() -> PathBuf {
        PathBuf::from("/tmp/textenc-test-file")
    }

    #[test]
    fn utf8_decodes_unchanged() {
        let body = "plain ascii and café über 日本語".as_bytes().to_vec();
        assert!(looks_like_text(&body));
        assert_eq!(decode_text(body, &p()).unwrap(), "plain ascii and café über 日本語");
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("hello".as_bytes());
        assert!(looks_like_text(&body));
        let text = decode_text(body, &p()).unwrap();
        assert_eq!(text, "hello", "BOM must not survive into stored text");
    }

    /// The shape of a Windows registry export: UTF-16LE with BOM.
    #[test]
    fn utf16le_bom_decodes() {
        let src = "Windows Registry Editor Version 5.00\r\n";
        let mut body = vec![0xFF, 0xFE];
        for unit in src.encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(looks_like_text(&body));
        assert_eq!(decode_text(body, &p()).unwrap(), src);
    }

    #[test]
    fn utf16be_bom_decodes() {
        let src = "big endian text";
        let mut body = vec![0xFE, 0xFF];
        for unit in src.encode_utf16() {
            body.extend_from_slice(&unit.to_be_bytes());
        }
        assert!(looks_like_text(&body));
        assert_eq!(decode_text(body, &p()).unwrap(), src);
    }

    #[test]
    fn windows_1252_decodes() {
        // A sentence long enough for chardetng to settle on a Western
        // single-byte encoding.
        let body = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.".to_vec();
        assert!(looks_like_text(&body));
        assert_eq!(
            decode_text(body, &p()).unwrap(),
            "Le café près de la fenêtre est agréable en été."
        );
    }

    #[test]
    fn shift_jis_decodes() {
        // "日本語のテキストです。これはシフトJISでエンコードされています。"
        let src = "日本語のテキストです。これはシフトJISでエンコードされています。";
        let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(src);
        assert!(!had_errors);
        let body = encoded.into_owned();
        assert!(looks_like_text(&body));
        assert_eq!(decode_text(body, &p()).unwrap(), src);
    }

    #[test]
    fn nul_bytes_are_binary() {
        let body = b"looks like text until\x00it does not".to_vec();
        assert!(!looks_like_text(&body));
        let err = decode_text(body, &p()).unwrap_err();
        assert!(err.contains("binary content"), "{err}");
        assert!(err.contains("textenc-test-file"), "error must name the file: {err}");
    }

    #[test]
    fn control_density_is_binary() {
        // 4 control bytes in 24 total = 16% > 10%.
        let body = b"abcdefghijklmnopqrst\x01\x02\x03\x04".to_vec();
        assert!(!looks_like_text(&body));
        assert!(decode_text(body, &p()).is_err());
    }

    #[test]
    fn ansi_log_is_text() {
        // ESC-heavy colored log output stays text.
        let body = b"\x1b[31mERROR\x1b[0m something failed\n\x1b[33mWARN\x1b[0m retrying\n".to_vec();
        assert!(looks_like_text(&body));
        assert!(decode_text(body, &p()).is_ok());
    }

    #[test]
    fn truncated_utf8_tail_still_sniffs_as_text() {
        let mut head = "ends mid-char: caf".as_bytes().to_vec();
        head.push(0xC3); // first byte of a two-byte sequence, cut off
        assert!(looks_like_text(&head));
        // The same bytes as a *complete* file are not valid UTF-8, so the
        // decoder treats them as legacy-encoded — still Ok, never a panic.
        assert!(decode_text(head, &p()).is_ok());
    }

    #[test]
    fn empty_head_is_not_text_but_empty_file_decodes() {
        assert!(!looks_like_text(b""));
        assert_eq!(decode_text(Vec::new(), &p()).unwrap(), "");
    }

    /// UTF-16 without a BOM is out of scope: its NULs trip the binary
    /// guard. This test documents the decision rather than a limitation we
    /// intend to lift.
    #[test]
    fn utf16_without_bom_is_rejected() {
        let mut body = Vec::new();
        for unit in "no bom here".encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(!looks_like_text(&body));
        assert!(decode_text(body, &p()).is_err());
    }

    /// Every head the sniff accepts must decode — the invariant that makes
    /// "sniffed as text/plain" safe to act on.
    #[test]
    fn sniffed_text_is_guaranteed_decodable() {
        let heads: Vec<Vec<u8>> = vec![
            b"ordinary ascii".to_vec(),
            "utf-8 caf\u{e9}".as_bytes().to_vec(),
            b"latin-1 caf\xe9 body".to_vec(),
            {
                let mut v = vec![0xFF, 0xFE];
                v.extend("utf16".encode_utf16().flat_map(|u| u.to_le_bytes()));
                v
            },
            {
                let mut v = "truncated tail caf".as_bytes().to_vec();
                v.push(0xC3);
                v
            },
        ];
        for head in heads {
            if looks_like_text(&head) {
                assert!(
                    decode_text(head.clone(), &p()).is_ok(),
                    "sniffed-as-text head failed to decode: {head:?}"
                );
            }
        }
    }
}
