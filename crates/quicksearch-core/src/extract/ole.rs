//! Legacy binary Office formats: `.doc`, `.xls`, `.ppt` and the template and
//! slideshow names for the same three OLE2 compound files, read by three
//! parsers sharing a reader, for the text alone. Every offset and count in
//! these formats is attacker-controlled, so every read is bounds-checked,
//! declared lengths are clamped, and nothing is preallocated from a declared
//! count. Nothing panics; corruption ends that structure's parse, keeping the
//! text before it; an encrypted workbook or document is an error. Excel
//! 5.0/95 (BIFF5) workbooks are read too, their `LABEL` and `RSTRING` cells
//! as cp1252.

use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::office::{parser_for, Parser};
use super::{ExtractError, Extractor, Limits, Scratch};

/// The legacy formats: the MIMEs whose [`Parser`] is an OLE2 one. Apart from
/// [`super::office::OfficeExtractor`], with which it shares the dispatch, so
/// that the registry can sandbox cfb's reader alone.
pub struct OleExtractor;

impl Extractor for OleExtractor {
    fn supports(&self, mime: &str) -> bool {
        parser_for(mime).is_some_and(Parser::is_ole)
    }

    fn extract(
        &self,
        path: &Path,
        mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        super::office::extract_office(path, mime, out, scratch)
    }
}

/// Extract a legacy Office document's text from `file` into `out`, stopping
/// at [`Limits::text`]; a workbook's shared strings also stop at
/// [`Limits::inflate`]. `parser` is the one its MIME named; any but the
/// three OLE2 ones is an error.
pub(crate) fn extract_ole_text(
    file: File,
    parser: Parser,
    out: &mut String,
    scratch: &mut Scratch,
) -> Result<(), Box<dyn Error>> {
    let limits = scratch.limits();
    let mut cfb = cfb::CompoundFile::open(file)
        .map_err(|e| format!("not a readable OLE2 compound file: {}", e))?;
    let text = match parser {
        Parser::Doc => doc::extract(&mut cfb, limits.text),
        Parser::Xls => xls::extract(&mut cfb, &limits),
        Parser::Ppt => ppt::extract(&mut cfb, limits.text),
        other => Err(format!("no OLE2 parser for {:?}", other).into()),
    }?;
    super::append(out, text);
    Ok(())
}

fn stream<F: Read + std::io::Seek>(cfb: &mut cfb::CompoundFile<F>, name: &str) -> Option<Vec<u8>> {
    let mut s = cfb.open_stream(name).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    Some(buf)
}

// Bounds-checked little-endian reads: every one returns `Option` on a short
// slice, because the offsets are attacker-controlled.

fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// windows-1252, the "compressed" 8-bit form all three formats use.
fn cp1252(bytes: &[u8]) -> String {
    encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()
}

/// UTF-16LE, the wide form. A trailing odd byte (declared length disagreeing
/// with the stream) is dropped; half a code unit carries nothing.
fn utf16le(bytes: &[u8]) -> String {
    let even = bytes.len() - (bytes.len() % 2);
    encoding_rs::UTF_16LE.decode(&bytes[..even]).0.into_owned()
}

/// Map the control codes these formats use as structure into whitespace, and
/// drop the rest. Word marks paragraphs with `\r` and table cells with
/// `\x07`; field instructions (markup, not prose) live between `\x13` and
/// `\x15`; PowerPoint uses `\x0B` as a soft line break.
fn clean(raw: &str, out: &mut String) {
    let mut in_field_instruction = false;
    for ch in raw.chars() {
        match ch {
            '\u{13}' => in_field_instruction = true,
            // 0x14 ends the instruction and begins the field's *result*, which
            // is real text; 0x15 ends the field entirely.
            '\u{14}' | '\u{15}' => in_field_instruction = false,
            _ if in_field_instruction => {}
            '\r' | '\u{07}' | '\u{0B}' => out.push('\n'),
            '\t' | '\n' => out.push(ch),
            // Picture anchors, chunk separators, and the rest of the C0 range.
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}

// .doc, Word 97-2003. Text is not contiguous: the `WordDocument` stream
// holds arbitrarily ordered runs, and a *piece table* in the companion table
// stream says which run belongs where. Start-to-end reading yields storage
// order interleaved with leftovers of earlier edits; only the piece table
// gives the document as it reads.
mod doc {
    use super::*;

    /// Flags word in the FIB base; bit 9 selects the live table stream.
    pub(super) const FIB_FLAGS: usize = 0x000A;
    pub(super) const FLAG_WHICH_TBL_STM: u16 = 0x0200;
    /// Bit 8, `fEncrypted`: the text is encrypted or XOR-obfuscated.
    pub(super) const FLAG_ENCRYPTED: u16 = 0x0100;

    /// The FIB base is fixed-length; the variable-length arrays follow it.
    pub(super) const FIB_BASE_LEN: usize = 32;

    /// Index of `fcClx`/`lcbClx` within `fibRgFcLcb97`, an array of
    /// (u32 fc, u32 lcb) pairs; the CLX is where the piece table is.
    pub(super) const CLX_PAIR_INDEX: usize = 33;

    /// `Pcdt`, the piece-table element of a CLX.
    pub(super) const CLXT_PCDT: u8 = 0x02;
    /// `Prc`, a formatting element that precedes the piece table.
    pub(super) const CLXT_PRC: u8 = 0x01;

    /// A piece descriptor is 8 bytes; each CP in the accompanying array is 4.
    pub(super) const PCD_LEN: usize = 8;
    pub(super) const CP_LEN: usize = 4;

    /// Set in a `PCD`'s `fc` field when the piece is 8-bit rather than UTF-16.
    pub(super) const FC_COMPRESSED: u32 = 0x4000_0000;
    pub(super) const FC_ADDRESS_MASK: u32 = 0x3FFF_FFFF;

    pub fn extract<F: Read + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
        budget: usize,
    ) -> Result<String, Box<dyn Error>> {
        let doc = stream(cfb, "WordDocument").ok_or("no WordDocument stream")?;
        let flags = u16_at(&doc, FIB_FLAGS).ok_or("truncated FIB")?;
        // Its pieces would decode as noise, not text.
        if flags & FLAG_ENCRYPTED != 0 {
            return Err("encrypted document".into());
        }
        // Word rewrites its two table streams alternately; reading the wrong
        // one gives a piece table from a previous save.
        let table_name = if flags & FLAG_WHICH_TBL_STM != 0 {
            "1Table"
        } else {
            "0Table"
        };
        let table = stream(cfb, table_name)
            .ok_or_else(|| format!("no {} stream (Word 6/95 file?)", table_name))?;

        let (fc_clx, lcb_clx) = clx_location(&doc)?;
        let clx = table
            .get(fc_clx..fc_clx.checked_add(lcb_clx).ok_or("CLX length overflows")?)
            .ok_or("CLX runs past the end of the table stream")?;
        let pieces = piece_table(clx)?;

        let out = decode_pieces(&doc, &pieces, budget);
        if out.trim().is_empty() {
            return Err("no text found in the piece table".into());
        }
        Ok(out)
    }

    /// Decode `pieces` out of the document stream, stopping at `budget`.
    ///
    /// Two budgets, because the output one cannot bound the input: a piece of
    /// control characters decodes and appends nothing, and nothing requires
    /// pieces to be disjoint or ordered: a table can point every piece at the
    /// same span, so a 2 MiB file can name tens of GiB of decoding. Charging
    /// the characters decoded bounds both. A piece is decoded only as far as
    /// the allowance left, so a wide piece gets as many characters as a
    /// compressed one, and one huge piece stops where the budget does.
    pub(super) fn decode_pieces(doc: &[u8], pieces: &[Piece], budget: usize) -> String {
        let mut out = String::new();
        let mut decoded = 0usize;
        for piece in pieces {
            if out.len() >= budget || decoded >= budget {
                break;
            }
            let Some(bytes) = doc.get(piece.start..piece.end) else {
                // Corruption; the pieces before it were real, keep them.
                break;
            };
            let width = if piece.compressed { 1 } else { 2 };
            let take = (bytes.len() / width).min(budget - decoded);
            decoded += take;
            let bytes = &bytes[..take * width];
            let text = if piece.compressed {
                cp1252(bytes)
            } else {
                utf16le(bytes)
            };
            clean(&text, &mut out);
        }
        out
    }

    /// Walk the FIB's variable-length sections to find `fcClx`/`lcbClx`.
    /// Each is preceded by its own count, so no version table is needed.
    fn clx_location(doc: &[u8]) -> Result<(usize, usize), Box<dyn Error>> {
        // csw: count of 16-bit values in rgW97.
        let csw = u16_at(doc, FIB_BASE_LEN).ok_or("truncated FIB (csw)")? as usize;
        let after_rgw = FIB_BASE_LEN + 2 + csw * 2;
        // cslw: count of 32-bit values in rgLw97.
        let cslw = u16_at(doc, after_rgw).ok_or("truncated FIB (cslw)")? as usize;
        let after_rglw = after_rgw + 2 + cslw * 4;
        // cbRgFcLcb: count of (fc, lcb) *pairs*, not bytes.
        let pairs = u16_at(doc, after_rglw).ok_or("truncated FIB (cbRgFcLcb)")? as usize;
        if pairs <= CLX_PAIR_INDEX {
            return Err("FIB has no fcClx entry (pre-Word 97 file?)".into());
        }
        let blob = after_rglw + 2;
        let entry = blob + CLX_PAIR_INDEX * 8;
        let fc = u32_at(doc, entry).ok_or("truncated FIB (fcClx)")? as usize;
        let lcb = u32_at(doc, entry + 4).ok_or("truncated FIB (lcbClx)")? as usize;
        if lcb == 0 {
            return Err("document has an empty piece table".into());
        }
        Ok((fc, lcb))
    }

    /// One run of characters in the `WordDocument` stream.
    pub(super) struct Piece {
        pub(super) start: usize,
        pub(super) end: usize,
        pub(super) compressed: bool,
    }

    /// Locate the `Pcdt` inside the CLX and decode its `PlcPcd`.
    fn piece_table(clx: &[u8]) -> Result<Vec<Piece>, Box<dyn Error>> {
        let mut i = 0usize;
        // The CLX is zero or more Prc elements followed by exactly one Pcdt.
        loop {
            match u8_at(clx, i).ok_or("CLX ends before its piece table")? {
                CLXT_PRC => {
                    // Prc: type byte, i16 length, then that many bytes.
                    let len = u16_at(clx, i + 1).ok_or("truncated Prc")? as usize;
                    i = i
                        .checked_add(3)
                        .and_then(|i| i.checked_add(len))
                        .ok_or("Prc length overflows")?;
                }
                CLXT_PCDT => {
                    let len = u32_at(clx, i + 1).ok_or("truncated Pcdt")? as usize;
                    let start = i + 5;
                    let plc = clx
                        .get(start..start.checked_add(len).ok_or("Pcdt overflows")?)
                        .ok_or("Pcdt runs past the end of the CLX")?;
                    return decode_plc_pcd(plc);
                }
                other => return Err(format!("unknown CLX element 0x{:02x}", other).into()),
            }
        }
    }

    /// A `PlcPcd` is `n+1` character positions then `n` piece descriptors,
    /// so its length determines `n`.
    fn decode_plc_pcd(plc: &[u8]) -> Result<Vec<Piece>, Box<dyn Error>> {
        if plc.len() < CP_LEN + PCD_LEN {
            return Err("piece table holds no pieces".into());
        }
        let n = (plc.len() - CP_LEN) / (CP_LEN + PCD_LEN);
        let pcd_base = (n + 1) * CP_LEN;

        let mut pieces = Vec::new();
        for k in 0..n {
            let cp = u32_at(plc, k * CP_LEN).ok_or("truncated CP array")? as usize;
            let cp_next = u32_at(plc, (k + 1) * CP_LEN).ok_or("truncated CP array")? as usize;
            // CPs must advance; a backwards table would underflow the count.
            let chars = cp_next.saturating_sub(cp);
            if chars == 0 {
                continue;
            }
            let fc_raw = u32_at(plc, pcd_base + k * PCD_LEN + 2).ok_or("truncated PCD")?;
            let compressed = fc_raw & FC_COMPRESSED != 0;
            let address = (fc_raw & FC_ADDRESS_MASK) as usize;
            // A compressed piece stores one byte per character at fc/2; a wide
            // one stores two bytes per character at fc.
            let (start, width) = if compressed {
                (address / 2, 1)
            } else {
                (address, 2)
            };
            let end = start
                .checked_add(chars.checked_mul(width).ok_or("piece length overflows")?)
                .ok_or("piece end overflows")?;
            pieces.push(Piece {
                start,
                end,
                compressed,
            });
        }
        if pieces.is_empty() {
            return Err("piece table holds no non-empty pieces".into());
        }
        Ok(pieces)
    }
}

// .xls, Excel 97-2003 (BIFF8) and Excel 5.0/95 (BIFF5): a flat sequence of
// records. BIFF8 pools cell text in a shared-string table (`SST`) the cells
// index into. The SST can overflow BIFF's 8224-byte record ceiling into
// `CONTINUE` records, cutting a string mid-way and resuming with a fresh
// width flag; getting that boundary wrong is the classic way to read an
// Excel file as mojibake. BIFF5 has no SST: its cell text is inline, 8-bit.
mod xls {
    use super::*;

    pub(super) const REC_BOF: u16 = 0x0809;
    pub(super) const REC_EOF: u16 = 0x000A;
    pub(super) const REC_FILEPASS: u16 = 0x002F;
    pub(super) const REC_SST: u16 = 0x00FC;
    pub(super) const REC_CONTINUE: u16 = 0x003C;
    pub(super) const REC_LABELSST: u16 = 0x00FD;
    pub(super) const REC_LABEL: u16 = 0x0204;
    pub(super) const REC_RSTRING: u16 = 0x00D6;
    pub(super) const REC_NUMBER: u16 = 0x0203;
    pub(super) const REC_RK: u16 = 0x027E;
    pub(super) const REC_MULRK: u16 = 0x00BD;

    /// The version word that opens a BIFF5 `BOF` body; BIFF8 writes 0x0600.
    pub(super) const BIFF5: u16 = 0x0500;

    /// A record header is a 2-byte id and a 2-byte length.
    pub(super) const REC_HEADER_LEN: usize = 4;

    pub fn extract<F: Read + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
        limits: &Limits,
    ) -> Result<String, Box<dyn Error>> {
        // BIFF8 names the stream "Workbook"; BIFF5 and earlier used "Book".
        // The first BOF, not the name, says which layout the cells use.
        let book = stream(cfb, "Workbook")
            .or_else(|| stream(cfb, "Book"))
            .ok_or("no Workbook stream")?;
        extract_from_book(&book, limits)
    }

    pub(super) fn extract_from_book(
        book: &[u8],
        limits: &Limits,
    ) -> Result<String, Box<dyn Error>> {
        let biff5 = Records { book, pos: 0 }
            .find(|&(id, _)| id == REC_BOF)
            .and_then(|(_, body)| u16_at(body, 0))
            == Some(BIFF5);

        // Pass 1: encryption and the shared strings.
        let mut strings = None;
        let mut records = Records { book, pos: 0 };
        while let Some((id, body)) = records.next() {
            match id {
                // Every record after it is encrypted, so its cells would be
                // stored as noise: numbers above all, which nothing rejects.
                REC_FILEPASS => return Err("encrypted workbook".into()),
                REC_SST if strings.is_none() => {
                    let at = Cursor {
                        book,
                        pos: records.pos - body.len(),
                        end: records.pos,
                        follow: true,
                    };
                    strings = Some(shared_strings(at, limits.inflate));
                }
                _ => {}
            }
        }

        // Pass 2: the cells.
        let strings = strings.unwrap_or_default();
        let out = decode_cells(Records { book, pos: 0 }, &strings, biff5, limits.text);
        if out.trim().is_empty() {
            return Err("workbook holds no readable cell text".into());
        }
        Ok(out)
    }

    /// Render every cell-bearing record, stopping at `budget`, with the same
    /// two budgets as [`super::doc::decode_pieces`]: `clean` can consume a
    /// whole cell and emit nothing, and one six-byte `LABELSST` is free to
    /// name the same 64 KiB shared string as thousands of others. A BIFF5
    /// workbook (`biff5`) stores `LABEL` and `RSTRING` text in its own
    /// layout.
    fn decode_cells(
        records: Records<'_>,
        strings: &[String],
        biff5: bool,
        budget: usize,
    ) -> String {
        let mut out = String::new();
        let mut decoded = 0usize;
        for (id, body) in records {
            if out.len() >= budget || decoded >= budget {
                break;
            }
            let cell = match id {
                REC_LABELSST => u32_at(body, 6)
                    .and_then(|i| strings.get(i as usize))
                    .cloned(),
                // An inline string: cell coordinates, then the string itself.
                REC_LABEL | REC_RSTRING if biff5 => biff5_string(body),
                REC_LABEL | REC_RSTRING => read_string(&mut Cursor::within(body, 6)).ok(),
                REC_NUMBER => number_at(body, 6).map(fmt_number),
                REC_RK => rk_at(body, 6).map(fmt_number),
                // Adjacent RK cells of one row: row and first column, then an
                // (ixfe u16, RK u32) pair per cell, then the last column.
                // Every whole pair is read, so a truncated record keeps them.
                REC_MULRK => {
                    let pairs = body.len().saturating_sub(4) / 6;
                    for n in (0..pairs).filter_map(|k| rk_at(body, 6 + 6 * k)) {
                        push_cell(&mut out, &mut decoded, &fmt_number(n));
                    }
                    None
                }
                // Sheet boundaries: cells from different sheets must not join.
                // A row is open while the last thing written was a cell.
                REC_EOF | REC_BOF => {
                    if out.ends_with(' ') {
                        out.push('\n');
                    }
                    None
                }
                _ => None,
            };
            if let Some(text) = cell {
                push_cell(&mut out, &mut decoded, &text);
            }
        }
        if out.ends_with(' ') {
            out.push('\n');
        }
        out
    }

    /// Append one cell and its separator, charging the text it decoded.
    fn push_cell(out: &mut String, decoded: &mut usize, text: &str) {
        *decoded = decoded.saturating_add(text.len());
        clean(text, out);
        out.push(' ');
    }

    /// The records of a stream as `(id, body)`, in order, ending at the first
    /// header or body that does not fit: a truncated file keeps whatever
    /// records were whole.
    struct Records<'a> {
        book: &'a [u8],
        /// Where the next header starts: the end of the last body returned.
        pos: usize,
    }

    impl<'a> Iterator for Records<'a> {
        type Item = (u16, &'a [u8]);

        fn next(&mut self) -> Option<(u16, &'a [u8])> {
            let id = u16_at(self.book, self.pos)?;
            let len = u16_at(self.book, self.pos + 2)? as usize;
            let start = self.pos + REC_HEADER_LEN;
            let body = self.book.get(start..start + len)?;
            self.pos = start + len;
            Some((id, body))
        }
    }

    /// The shared-string table, in index order, read from `at`, the `SST`
    /// record's body. Missing or malformed is not fatal: a numbers-only
    /// workbook has no SST at all, and a damaged one still has readable
    /// inline strings.
    ///
    /// Bounded by the bytes that exist, not the declared count: a malformed
    /// entry ends the table, earlier ones are still correct, and cells
    /// indexing past the end are dropped. Bounded by `inflate` too, charging
    /// the bytes walked, record headers included, plus each string's slot,
    /// so a table of empty strings or of empty `CONTINUE` records ends there
    /// as well. Excel writes the table in first-use order, so the strings a
    /// cut drops belong to the last cells.
    fn shared_strings(mut at: Cursor<'_>, inflate: usize) -> Vec<String> {
        let origin = at.pos;
        // SST header: total string count, then unique string count.
        let Ok(unique) = at.u32().and_then(|_| at.u32()) else {
            return Vec::new();
        };
        (0..unique as usize)
            .map_while(|decoded| {
                if at.pos - origin + decoded * size_of::<String>() >= inflate {
                    return None;
                }
                read_string(&mut at).ok()
            })
            .collect()
    }

    /// BIFF5's inline string after the cell coordinates: a u16 count, then
    /// that many 8-bit characters, clamped to the body. Read as cp1252,
    /// whatever the workbook's `CODEPAGE` says.
    fn biff5_string(body: &[u8]) -> Option<String> {
        let cch = u16_at(body, 6)? as usize;
        let chars = body.get(8..)?;
        Some(cp1252(&chars[..cch.min(chars.len())]))
    }

    /// Read an `XLUnicodeRichExtendedString` at the cursor. The width flag is
    /// per-record, not per-string: character data crossing into a `CONTINUE`
    /// resumes with a fresh flag byte, read after the cursor steps into that
    /// record, and that width.
    fn read_string(at: &mut Cursor<'_>) -> Result<String, String> {
        let cch = at.u16()? as usize;
        let grbit = at.u8()?;
        let mut wide = grbit & 0x01 != 0;
        let rich = grbit & 0x08 != 0;
        let ext = grbit & 0x04 != 0;
        let runs = if rich { at.u16()? as usize } else { 0 };
        let ext_len = if ext { at.u32()? as usize } else { 0 };

        let mut text = String::new();
        let mut remaining = cch;
        loop {
            let width = if wide { 2 } else { 1 };
            // Rounded up, so half a character before the record's end fails
            // the take instead of being read as the next flag.
            let take = remaining.min(at.left().div_ceil(width));
            let bytes = at.take(take * width)?;
            text.push_str(&if wide { utf16le(bytes) } else { cp1252(bytes) });
            remaining -= take;
            if remaining == 0 {
                break;
            }
            // Crossed into a CONTINUE: its first byte is a new flag.
            wide = at.u8()? & 0x01 != 0;
        }
        // Formatting runs and the extended (phonetic) block are not text.
        at.skip(runs * 4);
        at.skip(ext_len);
        Ok(text)
    }

    /// A byte cursor over one record body, `pos..end` of `book`. A cursor
    /// that follows (the SST's) steps over the `CONTINUE` records after the
    /// body as it reaches each end, walking the stream itself, so its cost
    /// is the bytes it passes; any other record ends it.
    struct Cursor<'a> {
        book: &'a [u8],
        pos: usize,
        end: usize,
        follow: bool,
    }

    impl<'a> Cursor<'a> {
        /// The inline string at `off` in `body`, a record that never
        /// continues.
        fn within(body: &'a [u8], off: usize) -> Cursor<'a> {
            Cursor {
                book: body,
                pos: off.min(body.len()),
                end: body.len(),
                follow: false,
            }
        }

        /// Bytes left in the current record.
        fn left(&self) -> usize {
            self.end - self.pos
        }

        /// At a record's end, step into the next non-empty `CONTINUE`, if
        /// the SST goes on.
        fn step(&mut self) {
            while self.follow && self.pos == self.end {
                let mut next = Records {
                    book: self.book,
                    pos: self.end,
                };
                let Some((REC_CONTINUE, _)) = next.next() else {
                    return;
                };
                self.pos = self.end + REC_HEADER_LEN;
                self.end = next.pos;
            }
        }

        /// The next `n` bytes, stepping first if the record is spent. They
        /// must lie in one record.
        fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            self.step();
            let slice = self
                .book
                .get(self.pos..self.end)
                .and_then(|record| record.get(..n))
                .ok_or("SST read crosses a record")?;
            self.pos += n;
            Ok(slice)
        }

        fn u8(&mut self) -> Result<u8, String> {
            Ok(self.take(1)?[0])
        }

        fn u16(&mut self) -> Result<u16, String> {
            let b = self.take(2)?;
            Ok(u16::from_le_bytes([b[0], b[1]]))
        }

        fn u32(&mut self) -> Result<u32, String> {
            let b = self.take(4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }

        /// Skip `n` bytes, which may span records: formatting runs and the
        /// phonetic block continue without a flag byte. Stops at the end of
        /// the SST, so the next read fails instead.
        fn skip(&mut self, mut n: usize) {
            while n > 0 {
                self.step();
                let k = n.min(self.left());
                if k == 0 {
                    return;
                }
                self.pos += k;
                n -= k;
            }
        }
    }

    fn number_at(body: &[u8], off: usize) -> Option<f64> {
        Some(f64::from_le_bytes(body.get(off..off + 8)?.try_into().ok()?))
    }

    /// An `RK` packs a number into 32 bits: bit 0 = scaled by 100, bit 1 =
    /// integer, else the top 30 bits of a double's mantissa.
    fn rk_at(body: &[u8], off: usize) -> Option<f64> {
        let raw = u32_at(body, off)?;
        let mut value = if raw & 0x02 != 0 {
            ((raw as i32) >> 2) as f64
        } else {
            f64::from_bits(((raw & 0xFFFF_FFFC) as u64) << 32)
        };
        if raw & 0x01 != 0 {
            value /= 100.0;
        }
        Some(value)
    }

    /// Whole values without a trailing `.0`, so "2024" finds the cell.
    fn fmt_number(n: f64) -> String {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        }
    }
}

// .ppt, PowerPoint 97-2003: a tree of records where containers nest and
// atoms hold data; slide text sits in two atom types differing only in
// width. This walks the whole tree instead of the slide-persistence
// directory: file order is close enough for an index.
mod ppt {
    use super::*;

    pub(super) const TEXT_CHARS_ATOM: u16 = 0x0FA0;
    pub(super) const TEXT_BYTES_ATOM: u16 = 0x0FA8;
    /// `CString`, used for titles and notes in some producers.
    pub(super) const CSTRING_ATOM: u16 = 0x0FBA;

    /// Version/instance u16, type u16, length u32.
    pub(super) const REC_HEADER_LEN: usize = 8;
    /// Low nibble 0xF in the first word marks a container of child records.
    pub(super) const VERSION_CONTAINER: u16 = 0x000F;

    /// Real decks are a handful deep; the bound keeps a file that claims to
    /// contain itself from recursing until the stack runs out.
    pub(super) const MAX_DEPTH: u32 = 32;

    pub fn extract<F: Read + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
        budget: usize,
    ) -> Result<String, Box<dyn Error>> {
        let doc = stream(cfb, "PowerPoint Document").ok_or("no PowerPoint Document stream")?;
        let mut out = String::new();
        walk(&doc, 0, &mut out, budget);
        if out.trim().is_empty() {
            return Err("no text atoms found in the presentation".into());
        }
        Ok(out)
    }

    /// Append the text atoms in `body` and the containers below it. The
    /// budget is checked before every record, so a container of many atoms
    /// stops where the text does, not where the container ends.
    pub(super) fn walk(body: &[u8], depth: u32, out: &mut String, budget: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut i = 0usize;
        while let (Some(version), Some(rec_type), Some(len)) =
            (u16_at(body, i), u16_at(body, i + 2), u32_at(body, i + 4))
        {
            if out.len() >= budget {
                return;
            }
            let start = i + REC_HEADER_LEN;
            // A length past the end is corruption; earlier records are good.
            let Some(payload) = body.get(start..start.saturating_add(len as usize)) else {
                return;
            };
            if version & 0x000F == VERSION_CONTAINER {
                walk(payload, depth + 1, out, budget);
            } else {
                let text = match rec_type {
                    TEXT_CHARS_ATOM | CSTRING_ATOM => Some(utf16le(payload)),
                    TEXT_BYTES_ATOM => Some(cp1252(payload)),
                    _ => None,
                };
                if let Some(text) = text {
                    clean(&text, out);
                    out.push('\n');
                }
            }
            i = start + payload.len();
        }
    }
}

#[cfg(test)]
#[path = "ole_tests.rs"]
mod tests;
