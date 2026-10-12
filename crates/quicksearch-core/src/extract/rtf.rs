//! RTF text extraction: a first-party, single-pass scan of the bytes. It
//! registers before the plaintext extractor, which claims every `text/*` and
//! would index `text/rtf` raw.
//!
//! Framing: the document is the group opened by the first `{`, which `\rtf`
//! must follow. Anything before it (a BOM, junk) and after its closing `}`
//! (a NUL, Ctrl-Z, stray braces) is ignored, and a truncated file keeps the
//! text read up to the cut. Besides a missing `\rtf`, the one parse error is
//! nesting deeper than `MAX_DEPTH` groups.
//!
//! Text is what the body's groups hold, plus the separators and punctuation
//! control words stand for (`\par`, `\tab`, `\cell`, `\rquote`, ...).
//! Destinations that are not document text are skipped whole: `\colortbl`,
//! `\stylesheet`, `\info`, `\pict`, `\fldinst` (a field keeps its `\fldrslt`),
//! `\pntext`, `\listtext`, `\revtbl`, `\NeXTGraphic` and every `\*` group.
//! `\fonttbl` is read for its codepages and emits nothing. `\binN` data is
//! skipped in any group.
//!
//! Non-ASCII arrives three ways. `\uN` is a UTF-16 unit, followed by `\ucN`
//! fallback units that are skipped. `\'hh` and literal bytes are codepage
//! bytes: the selected font's `\cpgN` or `\fcharsetN`, else the document's
//! `\ansicpgN`, else windows-1252. They are decoded together at the next
//! codepage change, group end or emitted character, never split by size, so
//! a DBCS pair whose trail byte is ASCII stays whole. Literal bytes >= 0x80 in
//! a file that is valid UTF-8 throughout are UTF-8 text instead, and a
//! fallback unit there is a whole character.
//!
//! Time is linear in the input. The scan stops once the text reaches
//! [`super::Limits::text`], so memory is the read buffer plus O(text), plus a
//! stack of `MAX_DEPTH` eight-byte group states.

use std::io::Read;
use std::path::Path;

use encoding_rs::Encoding;

use super::{ExtractError, Extractor, Scratch};

/// Deepest group nesting a document may open; one more is an error. Real
/// documents stay in the tens.
const MAX_DEPTH: usize = 1024;

/// Font-table entries kept. Later definitions are ignored, and text in those
/// fonts decodes with the document codepage.
const MAX_FONTS: usize = 256;

/// What one `{` level carries; its `}` restores the level outside it.
#[derive(Clone, Copy)]
struct Group {
    /// A destination that is not document text: only braces and `\binN`
    /// still count.
    skip: bool,
    /// Inside `\fonttbl`: `\fN` defines an entry, and nothing is text.
    fonttbl: bool,
    /// `\ucN`: how many fallback units follow each `\uN`.
    uc: u16,
    /// The font `\fN` selected; `None` is the `\deffN` font.
    font: Option<u16>,
}

/// The state a document starts in.
const ROOT: Group = Group {
    skip: false,
    fonttbl: false,
    uc: 1,
    font: None,
};

/// A font-table entry: the codepage its text is in, where it says.
#[derive(Clone, Copy)]
struct Font {
    number: u16,
    /// `\fcharsetN`.
    charset: Option<&'static Encoding>,
    /// `\cpgN`, which wins over the charset.
    cpg: Option<&'static Encoding>,
}

struct Reader<'a> {
    src: &'a [u8],
    pos: usize,
    path: &'a Path,
    out: &'a mut String,
    /// Stop once `out` and `pending` together reach this.
    limit: usize,
    /// Codepage bytes not yet decoded.
    pending: Vec<u8>,
    cur: Group,
    /// The groups outside `cur`, innermost last.
    outer: Vec<Group>,
    fonts: Vec<Font>,
    /// Whether the last `\fN` in the font table made an entry, which its
    /// `\fcharsetN` and `\cpgN` then describe.
    defining: bool,
    /// `\ansicpgN` or `\mac`.
    codepage: &'static Encoding,
    /// `\deffN`.
    deff: u16,
    /// Whether the whole input is valid UTF-8.
    utf8: bool,
    /// Fallback units still to skip after a `\uN`.
    fallback: u16,
    /// A `\uN` high surrogate, waiting for its low half.
    high: Option<u16>,
}

/// Append the text of the RTF document in `src` to `out`, stopping once it
/// reaches `limit` bytes.
fn parse(src: &[u8], path: &Path, limit: usize, out: &mut String) -> Result<(), ExtractError> {
    let not_rtf = || format!("rtf parse {}: no {{\\rtf header", path.display());
    let start = src.iter().position(|&b| b == b'{').ok_or_else(not_rtf)?;
    let body = src[start + 1..].trim_ascii_start();
    if !body.starts_with(br"\rtf") {
        return Err(not_rtf());
    }
    Reader {
        src,
        pos: src.len() - body.len(),
        path,
        out,
        limit,
        pending: Vec::new(),
        cur: ROOT,
        outer: Vec::new(),
        fonts: Vec::new(),
        defining: false,
        codepage: encoding_rs::WINDOWS_1252,
        deff: 0,
        utf8: std::str::from_utf8(src).is_ok(),
        fallback: 0,
        high: None,
    }
    .run()
}

impl Reader<'_> {
    fn run(&mut self) -> Result<(), ExtractError> {
        while self.pos < self.src.len() && self.out.len() + self.pending.len() < self.limit {
            match self.src[self.pos] {
                b'{' => {
                    self.pos += 1;
                    self.fallback = 0;
                    if self.outer.len() + 1 >= MAX_DEPTH {
                        return Err(format!(
                            "rtf parse {}: groups nested deeper than {MAX_DEPTH}",
                            self.path.display()
                        ));
                    }
                    self.outer.push(self.cur);
                }
                b'}' => {
                    self.pos += 1;
                    self.fallback = 0;
                    self.flush();
                    match self.outer.pop() {
                        Some(group) => self.cur = group,
                        None => return Ok(()),
                    }
                }
                b'\\' => self.control(),
                b'\r' | b'\n' => self.pos += 1,
                _ => self.literal(),
            }
        }
        self.flush();
        Ok(())
    }

    /// Spend one `\ucN` fallback unit, if any are owed.
    fn take_fallback(&mut self) -> bool {
        let owed = self.fallback > 0;
        self.fallback = self.fallback.saturating_sub(1);
        owed
    }

    /// Literal bytes up to the next brace, backslash or line break, and in a
    /// UTF-8 file, up to the next switch between ASCII and not.
    fn literal(&mut self) {
        let (src, utf8) = (self.src, self.utf8);
        let rest = &src[self.pos..];
        let wide = utf8 && rest[0] >= 0x80;
        if self.take_fallback() {
            // A fallback unit is one character: in a UTF-8 file, its whole
            // sequence, not just the lead byte.
            let unit = match rest[0] {
                0xF0.. if wide => 4,
                0xE0.. if wide => 3,
                0xC0.. if wide => 2,
                _ => 1,
            };
            self.pos += unit.min(rest.len());
            return;
        }
        let len = rest
            .iter()
            .position(|&b| {
                matches!(b, b'{' | b'}' | b'\\' | b'\r' | b'\n') || (utf8 && (b >= 0x80) != wide)
            })
            .unwrap_or(rest.len());
        if self.cur.skip || self.cur.fonttbl {
            self.pos += len;
            return;
        }
        // At least one byte: `rest[0]` is none of the stops, and the scan
        // runs only while there is room.
        let run = &rest[..len.min(self.limit - self.out.len() - self.pending.len())];
        self.pos += run.len();
        if wide {
            // A run cut short by `\binN` decodes lossily rather than panicking.
            self.flush();
            self.out.push_str(&String::from_utf8_lossy(run));
        } else {
            self.pending.extend_from_slice(run);
        }
    }

    /// A control symbol (`\` and one byte), `\'hh` or a control word.
    fn control(&mut self) {
        let Some(&next) = self.src.get(self.pos + 1) else {
            self.pos += 1;
            return;
        };
        if next.is_ascii_alphabetic() {
            return self.word();
        }
        self.pos += 2;
        let byte = (next == b'\'').then(|| self.hex()).flatten();
        if self.take_fallback() || self.cur.skip {
            return;
        }
        match next {
            b'*' => self.cur.skip = true,
            _ if self.cur.fonttbl => {}
            b'\'' => self.pending.extend(byte),
            b'\\' | b'{' | b'}' => self.pending.push(next),
            b'~' => self.text('\u{A0}'),
            b'_' => self.text('\u{2011}'),
            b'\r' | b'\n' => self.text('\n'),
            // `\-` (an optional hyphen) and every other symbol.
            _ => {}
        }
    }

    /// The two hex digits of a `\'hh`, consumed only if both are hex.
    fn hex(&mut self) -> Option<u8> {
        let digit = |i: usize| char::from(*self.src.get(self.pos + i)?).to_digit(16);
        let byte = digit(0)? << 4 | digit(1)?;
        self.pos += 2;
        Some(byte as u8)
    }

    /// `\` then `[A-Za-z]+`, an optional signed decimal parameter, and one
    /// space delimiter.
    fn word(&mut self) {
        let src = self.src;
        let start = self.pos + 1;
        let mut end = start;
        while src.get(end).is_some_and(u8::is_ascii_alphabetic) {
            end += 1;
        }
        let name = &src[start..end];
        let negative =
            src.get(end) == Some(&b'-') && src.get(end + 1).is_some_and(u8::is_ascii_digit);
        let digits = end + usize::from(negative);
        let mut p = digits;
        let mut value = 0i32;
        while let Some(d) = src.get(p).filter(|b| b.is_ascii_digit()) {
            value = value.saturating_mul(10).saturating_add(i32::from(d - b'0'));
            p += 1;
        }
        let param = (p > digits).then_some(if negative { -value } else { value });
        if src.get(p) == Some(&b' ') {
            p += 1;
        }
        if name == b"bin" {
            let len = usize::try_from(param.unwrap_or(0)).unwrap_or(0);
            p = p.saturating_add(len).min(src.len());
        }
        self.pos = p;
        if name == b"u" {
            // Never another `\uN`'s fallback: a writer that left the
            // fallbacks out must not cost a character.
            self.fallback = 0;
        }
        if !self.take_fallback() && !self.cur.skip {
            self.apply(name, param);
        }
    }

    fn apply(&mut self, name: &[u8], param: Option<i32>) {
        let n = param.unwrap_or(0);
        match name {
            b"par" | b"line" | b"sect" | b"page" | b"row" => self.text('\n'),
            b"tab" | b"cell" | b"nestcell" => self.text('\t'),
            b"emdash" => self.text('\u{2014}'),
            b"endash" => self.text('\u{2013}'),
            b"emspace" => self.text('\u{2003}'),
            b"enspace" => self.text('\u{2002}'),
            b"qmspace" => self.text('\u{2005}'),
            b"bullet" => self.text('\u{2022}'),
            b"lquote" => self.text('\u{2018}'),
            b"rquote" => self.text('\u{2019}'),
            b"ldblquote" => self.text('\u{201C}'),
            b"rdblquote" => self.text('\u{201D}'),
            b"zwj" => self.text('\u{200D}'),
            b"zwnj" => self.text('\u{200C}'),
            b"u" if param.is_some() && !self.cur.fonttbl => self.unicode(n),
            b"uc" => self.cur.uc = u16::try_from(n.max(0)).unwrap_or(u16::MAX),
            b"colortbl" | b"stylesheet" | b"info" | b"pict" | b"fldinst" | b"pntext"
            | b"listtext" | b"revtbl" | b"NeXTGraphic" => self.cur.skip = true,
            b"fonttbl" => {
                self.flush();
                self.cur.fonttbl = true;
            }
            b"f" if self.cur.fonttbl => {
                self.defining = self.fonts.len() < MAX_FONTS;
                if self.defining {
                    self.fonts.push(Font {
                        number: font_number(n),
                        charset: None,
                        cpg: None,
                    });
                }
            }
            b"fcharset" | b"cpg" if self.cur.fonttbl && self.defining => {
                if let Some(font) = self.fonts.last_mut() {
                    if name == b"cpg" {
                        font.cpg = Some(codepage(n));
                    } else {
                        font.charset = charset(n);
                    }
                }
            }
            b"f" => self.recode(|r| r.cur.font = Some(font_number(n))),
            b"plain" => self.recode(|r| r.cur.font = None),
            b"deff" => self.recode(|r| r.deff = font_number(n)),
            b"ansicpg" => self.recode(|r| r.codepage = codepage(n)),
            b"mac" => self.recode(|r| r.codepage = encoding_rs::MACINTOSH),
            _ => {}
        }
    }

    /// Change what decodes the codepage bytes, decoding those read so far
    /// under the old one first.
    fn recode(&mut self, change: impl FnOnce(&mut Self)) {
        self.flush();
        change(self);
    }

    /// `\uN`: a UTF-16 unit, negative above 0x7FFF.
    fn unicode(&mut self, n: i32) {
        self.fallback = self.cur.uc;
        let unit = u16::try_from(if n < 0 { n + 0x10000 } else { n }).unwrap_or(0xFFFD);
        if (0xDC00..0xE000).contains(&unit) && self.pending.is_empty() {
            if let Some(high) = self.high.take() {
                let pair = char::decode_utf16([high, unit]).next().and_then(Result::ok);
                self.out.push(pair.unwrap_or('\u{FFFD}'));
                return;
            }
        }
        self.flush();
        if (0xD800..0xDC00).contains(&unit) {
            self.high = Some(unit);
        } else {
            // A low half alone is no character.
            let c = char::from_u32(unit.into()).unwrap_or('\u{FFFD}');
            self.out.push(c);
        }
    }

    /// Emit `c`, after the text before it.
    fn text(&mut self, c: char) {
        if !self.cur.fonttbl {
            self.flush();
            self.out.push(c);
        }
    }

    /// Decode the pending codepage bytes into `out`, after a high surrogate
    /// that found no partner.
    fn flush(&mut self) {
        if self.high.take().is_some() {
            self.out.push('\u{FFFD}');
        }
        if self.pending.is_empty() {
            return;
        }
        let encoding = self.encoding();
        // Not `decode`, which would read a leading EF BB BF or FF FE as a BOM.
        let (text, _) = encoding.decode_without_bom_handling(&self.pending);
        self.out.push_str(&text);
        self.pending.clear();
    }

    /// The selected font's codepage, else the document's.
    fn encoding(&self) -> &'static Encoding {
        let number = self.cur.font.unwrap_or(self.deff);
        self.fonts
            .iter()
            .find(|f| f.number == number)
            .and_then(|f| f.cpg.or(f.charset))
            .unwrap_or(self.codepage)
    }
}

fn font_number(n: i32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Windows codepage `n` (`\ansicpgN`, `\cpgN`). Anything without an
/// ASCII-compatible encoding (UTF-16, ISO-2022, the DOS pages) decodes as
/// windows-1252, which is what a document that names none is in.
fn codepage(n: i32) -> &'static Encoding {
    use encoding_rs::*;
    match n {
        866 => IBM866,
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        10000 => MACINTOSH,
        10007 => X_MAC_CYRILLIC,
        20866 => KOI8_R,
        21866 => KOI8_U,
        54936 => GB18030,
        65001 => UTF_8,
        _ => WINDOWS_1252,
    }
}

/// The codepage `\fcharsetN` names. `None` leaves the font in the document's
/// codepage: ANSI and default (0 and 1) mean it, and so does a charset with
/// no ASCII-compatible encoding (Symbol, OEM, Johab).
fn charset(n: i32) -> Option<&'static Encoding> {
    use encoding_rs::*;
    Some(match n {
        77 => MACINTOSH,
        78 | 128 => SHIFT_JIS,
        79 | 129 => EUC_KR,
        80 | 134 => GBK,
        81 | 136 => BIG5,
        89 => X_MAC_CYRILLIC,
        161 => WINDOWS_1253,
        162 => WINDOWS_1254,
        163 => WINDOWS_1258,
        177 => WINDOWS_1255,
        178 => WINDOWS_1256,
        186 => WINDOWS_1257,
        204 => WINDOWS_1251,
        222 => WINDOWS_874,
        238 => WINDOWS_1250,
        _ => return None,
    })
}

pub struct RtfExtractor;

/// Read `path` into `buf`, the worker's, reused file after file. A file over
/// `read` bytes is refused at open ([`super::open_for_extract`]); the
/// `take(read)` stops one that grew since, or whose `fstat` says zero.
fn read_capped(path: &Path, read: usize, buf: &mut Vec<u8>) -> Result<(), ExtractError> {
    let (file, _) = super::open_for_extract(path, read)?;
    buf.clear();
    file.take(read as u64)
        .read_to_end(buf)
        .map_err(|e| format!("rtf read {}: {}", path.display(), e))?;
    Ok(())
}

impl Extractor for RtfExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime == "application/rtf" || mime == "text/rtf"
    }

    fn extract(
        &self,
        path: &Path,
        _mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        let limits = scratch.limits();
        let bytes = scratch.bytes_mut();
        read_capped(path, limits.read, bytes)?;
        parse(bytes, path, limits.text, out)
    }

    /// RTF has no trailer and needs no seeking; a complete head parses like
    /// disk. There is no text limit here: a head is at most `hash_length`
    /// (1 MiB), its text at most three times that, and the caller truncates.
    fn extract_from_head(
        &self,
        path: &Path,
        head: &[u8],
        out: &mut String,
    ) -> Option<Result<(), ExtractError>> {
        Some(parse(head, path, usize::MAX, out))
    }
}

#[cfg(test)]
#[path = "rtf_tests.rs"]
mod tests;
