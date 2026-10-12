//! RTF corpus files, written as control words by hand and read by
//! `extract/rtf.rs`. LibreOffice's own `.rtf` is committed in
//! [`super::legacy`].
//!
//! Both non-ASCII escape forms are exercised, as separate reader code paths
//! a real document contains both of: `\'hh` for anything the codepage
//! holds, `\uN?` for the rest. One file is in windows-1252, the codepage
//! Word writes most; one in windows-1251, a codepage whose `\'hh` bytes
//! mean something else.

use std::path::Path;

use super::{BodyFn, Charset, Lcg, Sample};

/// Planted in the windows-1251 file, where it travels as `\'hh` escapes.
const CYRILLIC_PHRASE: &str = "Съешь же ещё этих булок";

/// `c`'s windows-1251 byte, for the Cyrillic letters that block holds in
/// alphabet order, plus Ё and ё; hand-rolled because `encoding_rs` is the
/// decoder under test.
fn cp1251_byte(c: char) -> Option<u8> {
    match c {
        '\u{0410}'..='\u{044F}' => Some((c as u32 - 0x0410 + 0xC0) as u8),
        'Ё' => Some(0xA8),
        'ё' => Some(0xB8),
        _ => None,
    }
}

/// Escape one character into RTF source: structural characters and ASCII
/// as themselves, `\'hh` for what the codepage's `encode` holds, `\uN` UTF-16
/// units for the rest.
///
/// The fallback is `\'3f` rather than a literal `?` because that is what
/// LibreOffice writes, and checking a real producer's form is the point of
/// this corpus. The literal form is equally legal;
/// `rtf_unicode_escapes_survive_extraction` in `tests/extraction_corpus.rs`
/// covers all three spellings.
fn escape(c: char, encode: fn(char) -> Option<u8>, out: &mut String) {
    match c {
        '\\' | '{' | '}' => {
            out.push('\\');
            out.push(c);
        }
        _ if c.is_ascii() => out.push(c),
        _ => match encode(c) {
            Some(byte) => out.push_str(&format!("\\'{byte:02x}")),
            None => {
                // Surrogate pairs are two `\uN`: the escape carries a UTF-16
                // code unit, not a scalar; units above 0x7FFF go negative.
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{}\\'3f", *unit as i16));
                }
            }
        },
    }
}

fn document(codepage: u16, encode: fn(char) -> Option<u8>, sentences: &[String]) -> String {
    let mut out =
        format!("{{\\rtf1\\ansi\\ansicpg{codepage}\\deff0{{\\fonttbl{{\\f0 Helvetica;}}}}\n");
    for (i, sentence) in sentences.iter().enumerate() {
        // A control word swallows exactly one following space as its
        // delimiter, so `\par ` puts nothing of its own into the text.
        if i > 0 {
            out.push_str("\\par ");
        }
        for c in sentence.chars() {
            escape(c, encode, &mut out);
        }
        out.push('\n');
    }
    out.push('}');
    out
}

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let doc = document(1252, super::plaintext::cp1252_byte, &b.sentences);
    let path = super::write_file(dir, "prose.rtf", doc.as_bytes());
    // No trailer, no seeking: a complete buffer parses exactly like the file.
    out.push(Sample::prose(path, "rtf", &b, true));

    let mut b = body(lcg, Charset::Unicode);
    b.sentences[4] = format!("{CYRILLIC_PHRASE} {}", b.sentences[4]);
    let doc = document(1251, cp1251_byte, &b.sentences);
    let path = super::write_file(dir, "cyrillic.rtf", doc.as_bytes());
    out.push(Sample::prose(path, "rtf-cp1251", &b, true));
}
