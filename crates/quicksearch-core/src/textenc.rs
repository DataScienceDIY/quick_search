//! Text detection and charset decoding, shared by the MIME sniff and the
//! plaintext extractor — one classifier, so they cannot drift.
//! [`decode_text`] decodes every class but `Binary`; [`looks_like_text`]
//! accepts only `Utf8` and `Bom`, the classes with positive proof.
//!
//! Classification order is load-bearing: **BOM** first (UTF-16 is full of
//! NULs the binary guard would reject), then the binary guard, then strict
//! UTF-8, then `Legacy`. The sniff rejects `Legacy` on purpose: chardetng's
//! windows-1252 floor never fails, so accepting it makes the sniff
//! unfalsifiable and stores NUL-free binary formats as mojibake.

/// How much of a file the charset detector is shown; chardetng's answer
/// stops moving well inside this.
const DETECT_PREFIX: usize = 64 * 1024;

use std::path::Path;

enum TextClass {
    Utf8,
    Bom(&'static encoding_rs::Encoding),
    /// Not UTF-8 but passes the binary guard: charset detection will decode.
    Legacy,
    Binary,
}

/// Ordinary whitespace, plus ESC because ANSI-colored logs are text.
fn is_benign_control(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\r' | 0x1B)
}

/// Classify `bytes` as text or binary. `truncated` marks a buffer that may
/// be a prefix of the file rather than its entirety.
fn classify(bytes: &[u8], truncated: bool) -> TextClass {
    if let Some((enc, _bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return TextClass::Bom(enc);
    }

    // Binary guard: NUL never appears in text of any supported encoding
    // (UTF-16 was handled above, by BOM or not at all).
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
/// An empty head proves nothing and answers `false`; without that guard
/// every zero-size procfs file would become `text/plain`.
pub fn looks_like_text(head: &[u8]) -> bool {
    !head.is_empty() && matches!(classify(head, true), TextClass::Utf8 | TextClass::Bom(_))
}

/// Decode a complete file's bytes to UTF-8 for storage. Accepts `Legacy`
/// besides what [`looks_like_text`] does: by the time this runs, something —
/// usually the extension — has already decided the file is text, so a
/// windows-1252 `.txt` or Shift-JIS `.csv` still decodes.
pub fn decode_text(bytes: Vec<u8>, path: &Path) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    match classify(&bytes, false) {
        // Cannot fail: classify ran strict validation with truncated=false.
        TextClass::Utf8 => Ok(String::from_utf8(bytes).expect("classified as UTF-8")),
        TextClass::Bom(enc) => {
            // Strips the BOM, replaces malformed sequences with U+FFFD.
            let (text, _, _) = enc.decode(&bytes);
            Ok(text.into_owned())
        }
        TextClass::Legacy => {
            // ISO-2022-JP detection is safe here: the browser caveat about
            // it concerns script-running web content, not indexed files.
            let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
            // A prefix, not the whole file: the detector converges in
            // kilobytes, and a 200 MiB log would cost a full extra pass.
            let prefix = &bytes[..bytes.len().min(DETECT_PREFIX)];
            det.feed(prefix, prefix.len() == bytes.len());
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
/// Filenames and extracted text can carry terminal escape sequences; printed
/// raw they rewrite the line, retitle the window, or via OSC 52 put text of
/// the writer's choosing on the user's clipboard. Tab survives, as does
/// everything above C1 — this is not a general sanitiser.
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

    /// Legacy charsets decode, but do not *sniff*.
    #[test]
    fn windows_1252_decodes_but_does_not_sniff() {
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

    /// Only the *detector*'s input is bounded; the tail is decoded in full.
    #[test]
    fn detection_prefix_is_bounded_and_the_tail_still_decodes() {
        let head = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9. ";
        let mut body = Vec::new();
        while body.len() < DETECT_PREFIX * 3 {
            body.extend_from_slice(head);
        }
        body.extend_from_slice(b"caf\xe9-tail-marker");
        let out = decode_text(body, &p()).unwrap();
        assert!(
            out.ends_with("café-tail-marker"),
            "the tail past the detection prefix must still be decoded"
        );
        assert!(out.starts_with("Le café près"), "got {:?}", &out[..24]);
    }

    #[test]
    fn detection_prefix_leaves_short_files_alone() {
        let body = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.".to_vec();
        assert!(body.len() < DETECT_PREFIX);
        assert_eq!(
            decode_text(body, &p()).unwrap(),
            "Le café près de la fenêtre est agréable en été."
        );
    }

    #[test]
    fn shift_jis_decodes_but_does_not_sniff() {
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

    /// UTF-16 without a BOM is out of scope by decision: its NULs trip the
    /// binary guard.
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
    /// "sniffed as text/plain" safe to act on. The `expect_sniff` column pins
    /// each head's side of the line so a change can't quietly weaken this.
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

    /// The regression this guard exists for: protobuf wire bytes clear the
    /// binary guard, and chardetng's windows-1252 floor then "decodes" them
    /// into mojibake. On a real 99k-file tree this single hole was 93% of
    /// all extracted text.
    #[test]
    fn protobuf_head_is_not_text() {
        let mut body = b"\x10\n\x02v1\x10\x01\x18\xe2\xe3\xfc\xd3\x9d\xca\x97\xe4\x189\x08\
                         \xbc\xf3\xf8\xd2\x9e\xca\x97\xe4\x18\x12*"
            .to_vec();
        body.extend_from_slice(b"$GNGGA,181558.00,,,,,0,00,99.99,,,,,,*78\r\n");
        body.extend_from_slice(b"\x18\xe3e>\x08\xeb\xac\xfb\xd2\x9e\xca\x97\xe4\x18\x12/");
        body.extend_from_slice(b"$GNGSA,M,1,,,,,,,,,,,,,99.99,99.99,99.99,1*3F\r\n");

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
