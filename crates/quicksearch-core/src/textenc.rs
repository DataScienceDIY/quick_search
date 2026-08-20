//! Text detection and charset decoding, shared by the MIME sniff and the
//! plaintext extractor.
//!
//! Both callers route through one classifier so they cannot drift, but they
//! accept different amounts of it, because they are answering different
//! questions:
//!
//! - [`decode_text`] is asked "this file is text — render it". Something
//!   else already established that, usually the extension. Every class but
//!   `Binary` decodes unconditionally.
//! - [`looks_like_text`] is asked "is this text at all?", by a caller that
//!   has *no other evidence*: no known extension, no magic bytes. It
//!   accepts only `Utf8` and `Bom`, the two classes that carry positive
//!   proof.
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
//!    (chardetng, windows-1252 floor) and decodes with replacement. The
//!    sniff **rejects** this class, and that asymmetry is the whole point:
//!    chardetng's windows-1252 floor means it never fails, so treating
//!    `Legacy` as proof of text makes the sniff unfalsifiable. The binary
//!    guard only rejects NUL and control bytes, so any format built out of
//!    `0x80-0xFF` — protobuf varints, packed binary telemetry — walks
//!    straight through it and gets stored as mojibake. Measured on a
//!    99k-file tree, that one leak was 93% of all extracted text; the
//!    legitimate `Legacy`-decoding files it costs us are the ones with no
//!    extension *and* no magic bytes, which measured 5 files and 0.1 MB.
//!    A file with a known text extension is unaffected: it is typed by
//!    `mime_guess`, never reaches the sniff, and still decodes as legacy.
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

/// Whether `head` — a possibly-truncated prefix of a file — is *provably*
/// text: valid UTF-8, or BOM-marked.
///
/// The sniff behind [`crate::mime::guess_mime_from_head`]'s `text/plain`
/// catch-all; see the module docs on why `TextClass::Legacy` is rejected
/// here but accepted by [`decode_text`].
///
/// An empty head proves nothing and answers `false`; without that guard
/// `classify` would call it valid UTF-8 and every zero-size procfs file
/// would become `text/plain`.
pub fn looks_like_text(head: &[u8]) -> bool {
    !head.is_empty() && matches!(classify(head, true), TextClass::Utf8 | TextClass::Bom(_))
}

/// Decode a complete file's bytes to UTF-8 for storage.
///
/// Accepts every class [`looks_like_text`] does and `Legacy` besides: by the
/// time this runs, something has already decided the file is text — usually
/// its extension, which is evidence the sniff does not have — so a
/// windows-1252 `.txt` or a Shift-JIS `.csv` still decodes here. `path` is
/// used only to name the file in the error.
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
            let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
            det.feed(&bytes, true);
            // Deny UTF-8: strict UTF-8 was already ruled out, so a UTF-8
            // guess could only mean malformed UTF-8.
            let enc = det.guess(None, chardetng::Utf8Detection::Deny);
            let (text, _, _) = enc.decode(&bytes);
            Ok(text.into_owned())
        }
        TextClass::Binary => Err(format!("plaintext read {}: binary content", path.display())),
    }
}

/// Replace control characters with `U+FFFD`, borrowing when there are none.
///
/// Filenames may contain any byte but NUL and the separator, and extracted
/// text is whatever was in the file — so both can carry terminal escape
/// sequences. Printed raw they rewrite the line, retitle the window, or on a
/// terminal with OSC 52 enabled put text of the writer's choosing on the
/// user's clipboard. `ls` has scrubbed for this reason for decades.
///
/// Tab survives: it is a legitimate part of a filename and harmless. So does
/// everything above C1 — this is not a general sanitiser, and the point is to
/// stay byte-for-byte faithful wherever there is nothing dangerous to remove.
pub fn scrub_controls(s: &str) -> std::borrow::Cow<'_, str> {
    fn dangerous(c: char) -> bool {
        (c.is_control() && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c)
    }
    if !s.chars().any(dangerous) {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(
        s.chars()
            .map(|c| if dangerous(c) { '\u{fffd}' } else { c })
            .collect(),
    )
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
        assert_eq!(
            decode_text(body, &p()).unwrap(),
            "plain ascii and café über 日本語"
        );
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

    /// Legacy charsets decode, but do not *sniff*: a `.txt` extension routes
    /// these bytes to `decode_text` and they render correctly, while the
    /// same bytes with no extension and no magic are not text enough to
    /// adopt on their own.
    #[test]
    fn windows_1252_decodes_but_does_not_sniff() {
        // A sentence long enough for chardetng to settle on a Western
        // single-byte encoding.
        let body = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.".to_vec();
        assert!(
            !looks_like_text(&body),
            "the sniff must not adopt a non-UTF-8 head on its own"
        );
        assert_eq!(
            decode_text(body, &p()).unwrap(),
            "Le café près de la fenêtre est agréable en été."
        );
    }

    #[test]
    fn shift_jis_decodes_but_does_not_sniff() {
        // "日本語のテキストです。これはシフトJISでエンコードされています。"
        let src = "日本語のテキストです。これはシフトJISでエンコードされています。";
        let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(src);
        assert!(!had_errors);
        let body = encoded.into_owned();
        assert!(!looks_like_text(&body));
        assert_eq!(decode_text(body, &p()).unwrap(), src);
    }

    #[test]
    fn nul_bytes_are_binary() {
        let body = b"looks like text until\x00it does not".to_vec();
        assert!(!looks_like_text(&body));
        let err = decode_text(body, &p()).unwrap_err();
        assert!(err.contains("binary content"), "{err}");
        assert!(
            err.contains("textenc-test-file"),
            "error must name the file: {err}"
        );
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
        let body =
            b"\x1b[31mERROR\x1b[0m something failed\n\x1b[33mWARN\x1b[0m retrying\n".to_vec();
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
    /// "sniffed as text/plain" safe to act on. The `expect_sniff` column
    /// pins which side of the UTF-8 line each head falls on, so a head that
    /// silently stops being sniffed can't quietly weaken this test.
    #[test]
    fn sniffed_text_is_guaranteed_decodable() {
        let heads: Vec<(bool, Vec<u8>)> = vec![
            (true, b"ordinary ascii".to_vec()),
            (true, "utf-8 caf\u{e9}".as_bytes().to_vec()),
            // Decodes, but only for a caller that already knows it's text.
            (false, b"latin-1 caf\xe9 body".to_vec()),
            (true, {
                let mut v = vec![0xFF, 0xFE];
                v.extend("utf16".encode_utf16().flat_map(|u| u.to_le_bytes()));
                v
            }),
            (true, {
                let mut v = "truncated tail caf".as_bytes().to_vec();
                v.push(0xC3);
                v
            }),
        ];
        for (expect_sniff, head) in heads {
            assert_eq!(
                looks_like_text(&head),
                expect_sniff,
                "sniff verdict changed for {head:?}"
            );
            assert!(
                decode_text(head.clone(), &p()).is_ok(),
                "head failed to decode: {head:?}"
            );
        }
    }

    /// The regression this guard exists for. Protobuf wire format is varint
    /// field tags and lengths — bytes in `0x80-0xFF`, which are neither NUL
    /// nor control bytes, so the binary guard passes them and chardetng's
    /// windows-1252 floor then "decodes" them into mojibake that never
    /// fails. On a real 99k-file tree this single hole was 93% of all
    /// extracted text. Bytes below are the head of an actual `.pb` GPS log:
    /// varint-framed records wrapping ASCII NMEA sentences.
    #[test]
    fn protobuf_head_is_not_text() {
        let mut body = b"\x10\n\x02v1\x10\x01\x18\xe2\xe3\xfc\xd3\x9d\xca\x97\xe4\x189\x08\
                         \xbc\xf3\xf8\xd2\x9e\xca\x97\xe4\x18\x12*"
            .to_vec();
        body.extend_from_slice(b"$GNGGA,181558.00,,,,,0,00,99.99,,,,,,*78\r\n");
        body.extend_from_slice(b"\x18\xe3e>\x08\xeb\xac\xfb\xd2\x9e\xca\x97\xe4\x18\x12/");
        body.extend_from_slice(b"$GNGSA,M,1,,,,,,,,,,,,,99.99,99.99,99.99,1*3F\r\n");

        // It clears the binary guard — that is exactly why the guard alone
        // was not enough — but it is not valid UTF-8, so the sniff declines.
        assert!(
            !looks_like_text(&body),
            "protobuf must not be adopted as text/plain"
        );
        assert!(
            std::str::from_utf8(&body).is_err(),
            "test fixture must be invalid UTF-8 or it proves nothing"
        );
        assert!(
            !body.contains(&0u8),
            "test fixture must have no NUL, or the old guard would have caught it"
        );
    }
}
