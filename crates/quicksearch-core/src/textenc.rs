//! Text detection and charset decoding, shared by the MIME sniff and the
//! plaintext extractor through one classifier, so they cannot drift.
//! [`decode_text`] decodes every class but `Binary`; [`looks_like_text`]
//! accepts only `Utf8` and `Bom`, the classes with positive proof.
//!
//! Classification order matters: the BOM first, then a NUL check on the
//! BOM's code units (UTF-16 is full of NUL bytes the binary guard would
//! reject, but never holds a NUL unit), then the binary guard, then strict
//! UTF-8, then `Legacy`. The sniff rejects `Legacy` because chardetng's
//! windows-1252 floor never fails: accepting it would make the sniff
//! unfalsifiable and store NUL-free binary formats as mojibake. A `Legacy`
//! file that is mostly valid UTF-8 is damaged UTF-8, not a legacy charset,
//! and decodes lossily as UTF-8.

/// How much of a file the charset detector is shown; chardetng's answer
/// stops moving well inside this.
const DETECT_PREFIX: usize = 64 * 1024;

use std::path::Path;

enum TextClass {
    Utf8,
    Bom(&'static encoding_rs::Encoding),
    /// Not UTF-8 but passes the binary guard: decoded lossily as UTF-8 when
    /// mostly valid UTF-8 ([`mostly_utf8`]), otherwise by charset detection.
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
    let (bom, body) = match encoding_rs::Encoding::for_bom(bytes) {
        Some((enc, bom_len)) => (Some(enc), &bytes[bom_len..]),
        None => (None, bytes),
    };
    if let Some(enc) = bom.filter(|&enc| enc != encoding_rs::UTF_8) {
        // UTF-16 text never holds a NUL code unit. UTF-32LE fails here: its
        // BOM reads as UTF-16LE's followed by one. An odd byte a truncated
        // head cut off is not a unit.
        if body.chunks_exact(2).any(|unit| unit == [0, 0]) {
            return TextClass::Binary;
        }
        return TextClass::Bom(enc);
    }

    // Binary guard: NUL never appears in text of any other supported
    // encoding (UTF-16 was handled above, by BOM or not at all).
    let mut suspect = 0usize;
    for &b in body {
        if b == 0 {
            return TextClass::Binary;
        }
        if (b < 0x20 && !is_benign_control(b)) || b == 0x7F {
            suspect += 1;
        }
    }
    if suspect * 10 > body.len() {
        return TextClass::Binary;
    }
    if let Some(enc) = bom {
        return TextClass::Bom(enc);
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => TextClass::Utf8,
        // `error_len() == None` means the only defect is a multibyte
        // sequence running off the end of the buffer; for a truncated head
        // that is the buffer boundary's fault, not the file's.
        Err(e) if truncated && e.error_len().is_none() => TextClass::Utf8,
        Err(_) => TextClass::Legacy,
    }
}

/// Whether `bytes`, which are not valid UTF-8, are damaged UTF-8 rather than
/// a legacy charset: their valid non-ASCII characters outnumber their
/// malformed sequences more than two to one. ASCII is valid either way and
/// is not counted. Legacy CJK only now and then forms a valid sequence:
/// GBK, Shift-JIS and EUC-KR prose all score under 0.3 to one, about 7x
/// below the line, while a short file with a few accents and one stray byte
/// clears it.
fn mostly_utf8(bytes: &[u8]) -> bool {
    let (mut good, mut bad) = (0usize, 0usize);
    for chunk in bytes.utf8_chunks() {
        good += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
        bad += usize::from(!chunk.invalid().is_empty());
    }
    good > 2 * bad
}

/// Whether `head`, a possibly truncated prefix of a file, is provably text:
/// valid UTF-8, or BOM-marked.
///
/// An empty head proves nothing and answers `false`; without that guard
/// every zero-size procfs file would become `text/plain`.
pub fn looks_like_text(head: &[u8]) -> bool {
    !head.is_empty() && matches!(classify(head, true), TextClass::Utf8 | TextClass::Bom(_))
}

/// Decode a complete file's bytes to UTF-8 for storage. Accepts `Legacy`
/// besides what [`looks_like_text`] does: by the time this runs, something
/// (usually the extension) has already decided the file is text, so a
/// windows-1252 `.txt` or Shift-JIS `.csv` still decodes.
///
/// Takes the buffer by value so a UTF-8 file, the overwhelming majority,
/// becomes its `String` with no copy. Callers holding only a borrow want
/// [`decode_borrowed_text`].
///
/// `truncated` marks a prefix of the file rather than all of it: a multibyte
/// character cut by the read is dropped instead of demoting the file to
/// `Legacy`. The other encodings replace a cut tail with one U+FFFD.
pub fn decode_text(mut bytes: Vec<u8>, path: &Path, truncated: bool) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    match classify(&bytes, truncated) {
        TextClass::Utf8 => {
            // `classify` forgave only a cut final character; drop its stub.
            if let Err(e) = std::str::from_utf8(&bytes) {
                bytes.truncate(e.valid_up_to());
            }
            Ok(String::from_utf8(bytes).expect("classified as UTF-8"))
        }
        _ => decode_borrowed_text(&bytes, path),
    }
}

/// [`decode_text`] for bytes the caller does not own, such as the walk's head
/// buffer, which is reused for the next file. Only the UTF-8 case differs: it
/// copies here, where every other class allocates its output either way.
pub fn decode_borrowed_text(bytes: &[u8], path: &Path) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    match classify(bytes, false) {
        // Cannot fail: classify ran strict validation with truncated=false.
        TextClass::Utf8 => Ok(std::str::from_utf8(bytes)
            .expect("classified as UTF-8")
            .to_string()),
        TextClass::Bom(enc) => {
            // Strips the BOM, replaces malformed sequences with U+FFFD.
            let (text, _, _) = enc.decode(bytes);
            Ok(text.into_owned())
        }
        TextClass::Legacy => {
            if mostly_utf8(bytes) {
                // Each malformed sequence becomes one U+FFFD.
                return Ok(String::from_utf8_lossy(bytes).into_owned());
            }
            // ISO-2022-JP detection is safe here: the browser caveat about
            // it concerns script-running web content, not indexed files.
            let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
            // A prefix, not the whole file: the detector converges in
            // kilobytes, and a 200 MiB log would cost a full extra pass. It
            // starts at the first malformed byte: while what it has been fed
            // is valid UTF-8, chardetng keeps its UTF-8 candidate, and `Deny`
            // then answers windows-1252 without scoring anything, so a long
            // ASCII head turned the Shift-JIS after it into mojibake.
            let start = bytes.utf8_chunks().next().map_or(0, |c| c.valid().len());
            let end = bytes.len().min(start + DETECT_PREFIX);
            det.feed(&bytes[start..end], end == bytes.len());
            // Deny UTF-8: `mostly_utf8` already ruled it out.
            let enc = det.guess(None, chardetng::Utf8Detection::Deny);
            let (text, _, _) = enc.decode(bytes);
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
/// everything above C1: this is not a general sanitiser.
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

/// `s` on one line: LF, CR and TAB become spaces, byte for byte, so byte
/// offsets into `s` (match ranges) still index the result. Borrows when
/// there are none.
///
/// epaint starts a new row at every LF, even in a one-row truncated cell,
/// draws CR as nothing and TAB four spaces wide: a name like `a.pdf` LF
/// `b.sh` would show only `a.pdf`. Flatten *before* [`scrub_controls`]: it
/// counts LF and CR as controls and would leave `U+FFFD` where a space
/// belongs (TAB it keeps, so for TAB the order does not matter). Every other
/// control passes through for the scrub to decide. U+2028, U+2029 and the
/// bidi controls are controls to neither, and pass through both unscrubbed.
pub fn one_line(s: &str) -> std::borrow::Cow<'_, str> {
    const BREAKS: [char; 3] = ['\n', '\r', '\t'];
    if !s.contains(BREAKS) {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(s.replace(BREAKS, " "))
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
            decode_text(body, &p(), false).unwrap(),
            "plain ascii and café über 日本語"
        );
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("hello".as_bytes());
        assert!(looks_like_text(&body));
        let text = decode_text(body, &p(), false).unwrap();
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
        assert_eq!(decode_text(body, &p(), false).unwrap(), src);
    }

    #[test]
    fn utf16be_bom_decodes() {
        let src = "big endian text";
        let mut body = vec![0xFE, 0xFF];
        for unit in src.encode_utf16() {
            body.extend_from_slice(&unit.to_be_bytes());
        }
        assert!(looks_like_text(&body));
        assert_eq!(decode_text(body, &p(), false).unwrap(), src);
    }

    /// encoding_rs has no UTF-32: UTF-32LE's BOM reads as UTF-16LE's, which
    /// once decoded every character followed by a U+0000.
    #[test]
    fn utf32le_bom_is_binary() {
        let mut body = vec![0xFF, 0xFE, 0x00, 0x00];
        body.extend("utf-32 text".chars().flat_map(|c| (c as u32).to_le_bytes()));
        assert!(!looks_like_text(&body));
        assert!(decode_text(body, &p(), false).is_err());
        // UTF-32BE's BOM starts with NUL and matches no BOM: the byte guard.
        let mut body = vec![0x00, 0x00, 0xFE, 0xFF];
        body.extend("utf-32 text".chars().flat_map(|c| (c as u32).to_be_bytes()));
        assert!(!looks_like_text(&body));
        assert!(decode_text(body, &p(), false).is_err());
    }

    #[test]
    fn utf16_bom_then_nul_unit_is_binary() {
        let orders: [([u8; 2], fn(u16) -> [u8; 2]); 2] = [
            ([0xFF, 0xFE], u16::to_le_bytes),
            ([0xFE, 0xFF], u16::to_be_bytes),
        ];
        for (bom, unit) in orders {
            let mut body = bom.to_vec();
            body.extend("before".encode_utf16().flat_map(unit));
            body.extend_from_slice(&[0, 0]);
            body.extend("after".encode_utf16().flat_map(unit));
            assert!(!looks_like_text(&body), "{bom:?}");
            assert!(decode_text(body, &p(), false).is_err(), "{bom:?}");
        }
    }

    /// The whole binary guard runs behind a UTF-8 BOM: NUL and control density.
    #[test]
    fn utf8_bom_then_nul_is_binary() {
        for body in [
            b"\xEF\xBB\xBFlooks like text until\x00it does not".to_vec(),
            b"\xEF\xBB\xBFabcdefghijklmnopqrst\x01\x02\x03\x04".to_vec(),
        ] {
            assert!(!looks_like_text(&body), "{body:?}");
            assert!(decode_text(body, &p(), false).is_err());
        }
    }

    /// A head cut right after its BOM, or through a UTF-16 unit, is still text.
    #[test]
    fn bom_only_and_odd_length_utf16_heads_stay_text() {
        for head in [vec![0xFF, 0xFE], vec![0xFE, 0xFF], vec![0xEF, 0xBB, 0xBF]] {
            assert!(looks_like_text(&head), "{head:?}");
            assert_eq!(decode_text(head, &p(), false).unwrap(), "");
        }
        // Cut through 'c', after its low byte (LE) or its high NUL byte (BE).
        let mut le = vec![0xFF, 0xFE];
        le.extend("ab".encode_utf16().flat_map(u16::to_le_bytes));
        le.push(b'c');
        let mut be = vec![0xFE, 0xFF];
        be.extend("ab".encode_utf16().flat_map(u16::to_be_bytes));
        be.push(0);
        for head in [le, be] {
            assert!(looks_like_text(&head), "{head:?}");
            assert_eq!(decode_text(head, &p(), false).unwrap(), "ab\u{FFFD}");
        }
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
            decode_text(body, &p(), false).unwrap(),
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
        let out = decode_text(body, &p(), false).unwrap();
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
            decode_text(body, &p(), false).unwrap(),
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
        assert_eq!(decode_text(body, &p(), false).unwrap(), src);
    }

    /// The detector still answers for legacy text, which is never mostly
    /// UTF-8.
    #[test]
    fn gbk_euc_kr_and_windows_1251_still_decode_exactly() {
        let fixtures = [
            (
                encoding_rs::GBK,
                "这是一段简体中文的文本。北京是中华人民共和国的首都，有着悠久的历史和灿烂的文化。",
            ),
            (
                encoding_rs::EUC_KR,
                "이것은 한국어 텍스트입니다. 서울은 대한민국의 수도이며 많은 사람들이 살고 있습니다.",
            ),
            (
                encoding_rs::WINDOWS_1251,
                "Это текст на русском языке. Москва является столицей Российской Федерации.",
            ),
        ];
        for (enc, src) in fixtures {
            let (encoded, _, had_errors) = enc.encode(src);
            assert!(!had_errors, "{}", enc.name());
            let body = encoded.into_owned();
            assert!(!mostly_utf8(&body), "{} read as damaged UTF-8", enc.name());
            assert_eq!(
                decode_text(body, &p(), false).unwrap(),
                src,
                "{}",
                enc.name()
            );
        }
    }

    /// One stray byte among accented UTF-8 is damage, not a legacy charset:
    /// it becomes one U+FFFD and the rest keeps its text.
    #[test]
    fn mostly_utf8_with_one_stray_byte_keeps_its_text() {
        let mut body = "Le café est très agréable, 日本語 ".as_bytes().to_vec();
        body.push(0xFF);
        body.extend_from_slice(" über alles".as_bytes());
        assert!(!looks_like_text(&body), "damaged UTF-8 is no proof of text");
        assert_eq!(
            decode_text(body, &p(), false).unwrap(),
            "Le café est très agréable, 日本語 \u{FFFD} über alles"
        );
    }

    /// A 3-byte character cut short mid-file is one malformed sequence.
    #[test]
    fn mid_file_cut_3_byte_sequence() {
        let mut body = "日本語 before ".as_bytes().to_vec();
        body.extend_from_slice(&"語".as_bytes()[..2]);
        body.extend_from_slice(" after 日本語".as_bytes());
        assert_eq!(
            decode_text(body, &p(), false).unwrap(),
            "日本語 before \u{FFFD} after 日本語"
        );
    }

    /// The detector's prefix was all valid UTF-8, so `Deny` answered
    /// windows-1252 unscored and the whole file decoded as mojibake.
    #[test]
    fn valid_utf8_prefix_over_64k_then_one_bad_byte_decodes_as_utf8() {
        let mut text = String::new();
        while text.len() <= DETECT_PREFIX {
            text.push_str("Le café près de la fenêtre, 日本語のテキスト。\n");
        }
        let mut body = text.clone().into_bytes();
        body.push(0xFE);
        body.extend_from_slice("tail 日本語".as_bytes());
        assert_eq!(
            decode_text(body, &p(), false).unwrap(),
            format!("{text}\u{FFFD}tail 日本語")
        );
    }

    /// The same unscored answer, with no damage at all: an ASCII head
    /// filling the prefix kept the UTF-8 candidate alive.
    #[test]
    fn ascii_head_over_64k_then_shift_jis_decodes_shift_jis() {
        let mut head = String::new();
        while head.len() <= DETECT_PREFIX {
            head.push_str("plain ascii padding line\n");
        }
        let src = "日本語のテキストです。これはシフトJISでエンコードされています。";
        let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(src);
        assert!(!had_errors);
        let mut body = head.clone().into_bytes();
        body.extend_from_slice(&encoded);
        assert_eq!(decode_text(body, &p(), false).unwrap(), head + src);
    }

    /// ASCII outvotes nothing: a lone stray byte in it is a legacy
    /// character, and chardetng names the charset.
    #[test]
    fn ascii_plus_one_stray_byte_still_goes_to_chardetng() {
        let body = b"plain ascii text with one cp1252 caf\xe9 and nothing else".to_vec();
        assert!(!mostly_utf8(&body));
        assert_eq!(
            decode_text(body, &p(), false).unwrap(),
            "plain ascii text with one cp1252 café and nothing else"
        );
    }

    /// More than two valid non-ASCII characters per malformed sequence.
    #[test]
    fn mostly_utf8_needs_more_than_two_good_per_bad() {
        let damaged = |good: &str, bad: &[u8]| [good.as_bytes(), bad].concat();
        assert!(!mostly_utf8(&damaged("ab é cd ü ", b"\xFF")));
        assert!(mostly_utf8(&damaged("ab é cd ü ö ", b"\xFF")));
        // Two stray bytes are two sequences; a cut 3-byte sequence is one.
        assert!(!mostly_utf8(&damaged("éüöä", b"\xFF\xFF")));
        assert!(!mostly_utf8(&damaged("日本", &"語".as_bytes()[..2])));
        assert!(mostly_utf8(&damaged("日本語", &"語".as_bytes()[..2])));
    }

    #[test]
    fn nul_bytes_are_binary() {
        let body = b"looks like text until\x00it does not".to_vec();
        assert!(!looks_like_text(&body));
        let err = decode_text(body, &p(), false).unwrap_err();
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
        assert!(decode_text(body, &p(), false).is_err());
    }

    #[test]
    fn ansi_log_is_text() {
        let body =
            b"\x1b[31mERROR\x1b[0m something failed\n\x1b[33mWARN\x1b[0m retrying\n".to_vec();
        assert!(looks_like_text(&body));
        assert!(decode_text(body, &p(), false).is_ok());
    }

    #[test]
    fn truncated_utf8_tail_still_sniffs_as_text() {
        let mut head = "ends mid-char: caf".as_bytes().to_vec();
        head.push(0xC3); // first byte of a two-byte sequence, cut off
        assert!(looks_like_text(&head));
        // The same bytes as a complete file are not valid UTF-8, so the
        // decoder treats them as legacy-encoded: still Ok, never a panic.
        assert!(decode_text(head, &p(), false).is_ok());
    }

    #[test]
    fn empty_head_is_not_text_but_empty_file_decodes() {
        assert!(!looks_like_text(b""));
        assert_eq!(decode_text(Vec::new(), &p(), false).unwrap(), "");
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
        assert!(decode_text(body, &p(), false).is_err());
    }

    /// Every head the sniff accepts must decode, which makes "sniffed as
    /// text/plain" safe to act on. The `expect_sniff` column pins each head's
    /// side of the line so a change cannot weaken this unnoticed.
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
                decode_text(head.clone(), &p(), false).is_ok(),
                "head failed to decode: {head:?}"
            );
        }
    }

    /// Protobuf wire bytes clear the binary guard, and chardetng's
    /// windows-1252 floor would "decode" them into mojibake. On a real
    /// 99k-file tree that hole was 93% of all extracted text.
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
            "test fixture must have no NUL, or the binary guard alone would catch it"
        );
    }

    #[test]
    fn one_line_keeps_byte_offsets_and_borrows_clean_text() {
        use std::borrow::Cow;

        // Multibyte text either side of every break, CRLF included.
        let s = "é\n日本\r\nb.sh\tz\r";
        let flat = one_line(s);
        assert!(matches!(flat, Cow::Owned(_)));
        assert_eq!(flat, "é 日本  b.sh z ");
        assert_eq!(flat.len(), s.len());
        for i in 0..=s.len() {
            assert_eq!(flat.is_char_boundary(i), s.is_char_boundary(i), "at {i}");
        }
        let at = s.find("日本").unwrap();
        assert_eq!(
            &flat[at..at + "日本".len()],
            "日本",
            "a range still marks its text"
        );

        // Other C0 and C1 controls (VT, FF, ESC, DEL, NEL, CSI) are left for
        // `scrub_controls`; the Unicode line separators and a bidi override
        // pass through both.
        let other = "a\x0bb\x0cc\x1b[2Jd\x7fe\u{85}f\u{9b}g\u{2028}h\u{2029}i\u{202e}j";
        assert!(matches!(one_line(other), Cow::Borrowed(t) if t == other));
        assert!(matches!(one_line(""), Cow::Borrowed("")));

        // A long name with one break, at the far end: found and kept in length.
        let long = format!("{}\n", "x".repeat(64 * 1024));
        let flat = one_line(&long);
        assert_eq!(flat.len(), long.len());
        assert!(flat.ends_with("x ") && !flat.contains('\n'));
        assert!(matches!(
            one_line(&long[..long.len() - 1]),
            Cow::Borrowed(_)
        ));
    }
}
