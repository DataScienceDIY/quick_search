//! An RTF corpus file, written as control words by hand.
//!
//! Hand-written rather than produced by a crate because RTF is a text format
//! whose writer side is trivial, and because the only Rust RTF library in the
//! tree is `rtf-parser` — the reader under test.
//!
//! Both non-ASCII escape forms are exercised, since they are separate code
//! paths in the parser and a document from a real word processor contains
//! both: `\'hh` for anything the declared codepage holds, `\uN?` for the rest.

use std::path::Path;

use super::{BodyFn, Charset, Lcg, Sample};

/// Escape one character into RTF source.
///
/// Below 0x80 only the three structural characters need escaping. From 0x80 to
/// 0xFF the `\'hh` hex form matches the `\ansicpg1252` declared in the header.
/// Above that, `\uN` carries the UTF-16 code unit.
///
/// # Why the fallback is `\'3f` and not a literal `?`
///
/// A `\uN` escape is followed by an ANSI fallback for readers that cannot do
/// Unicode, and the spec lets that be any character. This writer emits
/// `\uN\'3f` — the escaped form of `?` — because that is what LibreOffice
/// writes, and checking a file a real producer wrote is the point of this
/// corpus.
///
/// The literal form, `\uN?`, is equally legal and equally common, and it used
/// to lose the escape and the rest of the word. That is fixed — see
/// `rtf_unicode_escapes_survive_extraction` in `tests/extraction_corpus.rs`,
/// which covers all three spellings and is where that shape is now pinned.
/// Keeping this writer on LibreOffice's form keeps the corpus a check on real
/// output rather than a second copy of that test.
fn escape(c: char, out: &mut String) {
    let code = c as u32;
    match c {
        '\\' | '{' | '}' => {
            out.push('\\');
            out.push(c);
        }
        _ if code < 0x80 => out.push(c),
        // cp1252 and Latin-1 agree over 0xA0-0xFF, which is the whole range
        // the corpus's Latin-1 phrase uses.
        _ if (0xA0..=0xFF).contains(&code) => out.push_str(&format!("\\'{code:02x}")),
        _ => {
            // Surrogate pairs are written as two `\uN`, which is what the
            // format requires: the escape carries a UTF-16 code unit, not a
            // scalar value. Units above 0x7FFF are written negative.
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{}\\'3f", *unit as i16));
            }
        }
    }
}

/// The RTF source for `sentences`, one paragraph each.
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
    // RTF is the other format with an `extract_from_head`: it has no trailer
    // and needs no seeking, so a complete buffer parses exactly like the file.
    out.push(Sample::prose(path, "rtf", &b, true));
}
