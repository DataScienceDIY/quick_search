//! An RTF corpus file, written as control words by hand — the only Rust RTF
//! library in the tree is `rtf-parser`, the reader under test.
//!
//! Both non-ASCII escape forms are exercised, as separate parser code paths
//! a real document contains both of: `\'hh` for anything the codepage
//! holds, `\uN?` for the rest.

use std::path::Path;

use super::{BodyFn, Charset, Lcg, Sample};

/// Escape one character into RTF source: structural characters below 0x80,
/// `\'hh` (matching `\ansicpg1252`) up to 0xFF, `\uN` UTF-16 units above.
///
/// The fallback is `\'3f` rather than a literal `?` because that is what
/// LibreOffice writes, and checking a real producer's form is the point of
/// this corpus. The literal form is equally legal and used to lose the rest
/// of the word — pinned by `rtf_unicode_escapes_survive_extraction` in
/// `tests/extraction_corpus.rs`, which covers all three spellings.
fn escape(c: char, out: &mut String) {
    let code = c as u32;
    match c {
        '\\' | '{' | '}' => {
            out.push('\\');
            out.push(c);
        }
        _ if code < 0x80 => out.push(c),
        _ if (0xA0..=0xFF).contains(&code) => out.push_str(&format!("\\'{code:02x}")),
        _ => {
            // Surrogate pairs are two `\uN`: the escape carries a UTF-16
            // code unit, not a scalar; units above 0x7FFF go negative.
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{}\\'3f", *unit as i16));
            }
        }
    }
}

fn document(sentences: &[String]) -> String {
    let mut out = String::from("{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0 Helvetica;}}\n");
    for (i, sentence) in sentences.iter().enumerate() {
        // A control word swallows exactly one following space as its
        // delimiter, so `\par ` puts nothing of its own into the text.
        if i > 0 {
            out.push_str("\\par ");
        }
        for c in sentence.chars() {
            escape(c, &mut out);
        }
        out.push('\n');
    }
    out.push('}');
    out
}

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let path = super::write_file(dir, "prose.rtf", document(&b.sentences).as_bytes());
    // No trailer, no seeking: a complete buffer parses exactly like the file.
    out.push(Sample::prose(path, "rtf", &b, true));
}
