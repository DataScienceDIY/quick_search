//! Legacy binary Office formats: `.doc`, `.xls`, `.ppt`.
//!
//! These are OLE2 compound files — a FAT-like container of named streams —
//! rather than the zip-of-XML their `x`-suffixed successors use, so nothing in
//! [`super::office`] can read them. What the three have in common is only the
//! container; inside, each stores its text a completely different way, so this
//! module is three parsers sharing a reader.
//!
//! # What "supported" means here
//!
//! The goal is the *text*, for a full-text index. Formatting, embedded
//! objects, revision history and deleted-but-retained text are all out of
//! scope, and the parsers deliberately read the minimum structure needed to
//! locate character data.
//!
//! # Hostile input
//!
//! Every offset in these formats comes from the file itself, including counts
//! that decide how much to allocate. A `.doc` claiming four billion text
//! pieces is a valid byte sequence. So: every read is bounds-checked against
//! the stream that actually exists, every declared length is clamped to what
//! remains, and nothing is preallocated from a declared count. A malformed
//! file yields `Err` — which lands as a `FAILED` row with a reason, visible in
//! `list-failed` — never a partial string of garbage, and never a panic.

use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Ceiling on extracted text from one legacy document.
///
/// The formats allow a document to declare far more text than it contains, and
/// the config's own `maximum_text_size` is applied later, by the caller. This
/// is the earlier, cruder bound that keeps a hostile header from turning into
/// an allocation.
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;

pub fn extract_ole_text(path: &Path, extension: &str) -> Result<String, Box<dyn Error>> {
    let mut cfb = cfb::CompoundFile::open(File::open(path)?)
        .map_err(|e| format!("not a readable OLE2 compound file: {}", e))?;
    match extension {
        "doc" => doc::extract(&mut cfb),
        "xls" => xls::extract(&mut cfb),
        "ppt" => ppt::extract(&mut cfb),
        other => Err(format!("no OLE2 parser for .{}", other).into()),
    }
}

/// Read one named stream whole. `None` when the stream is absent, which is a
/// question several callers ask before falling back to another name.
fn stream<F: Read + std::io::Seek>(cfb: &mut cfb::CompoundFile<F>, name: &str) -> Option<Vec<u8>> {
    let mut s = cfb.open_stream(name).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    Some(buf)
}

// Bounds-checked little-endian reads
//
// Every one returns `Option` rather than panicking on a short slice: the
// offsets these are called with are attacker-controlled.

fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let bytes = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let bytes = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Decode `bytes` as windows-1252 — the "compressed"/8-bit form all three
/// formats use for text that fits it.
fn cp1252(bytes: &[u8]) -> String {
    encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()
}

/// Decode `bytes` as UTF-16LE, the wide form. A trailing odd byte is dropped
/// rather than treated as an error: it means the declared length disagreed
/// with the stream, and half a code unit carries nothing.
fn utf16le(bytes: &[u8]) -> String {
    let even = bytes.len() - (bytes.len() % 2);
    encoding_rs::UTF_16LE.decode(&bytes[..even]).0.into_owned()
}

/// Map the control codes these formats use as structure into whitespace, and
/// drop the rest.
///
/// Word marks paragraphs with `\r` and table cells with `\x07`; both read as
/// line breaks. Field instructions live between `\x13` and `\x15` and are
/// markup, not prose — "HYPERLINK \\l foo" is not something a user searches
/// for. PowerPoint uses `\x0B` as a soft line break.
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

// .doc — Word 97-2003
//
// Word does not store its text contiguously. The `WordDocument` stream holds
// character data in arbitrarily ordered runs, and a *piece table* in the
// companion table stream says which run belongs where in the document. Reading
// the stream start-to-end therefore yields text in storage order, interleaved
// with whatever earlier edits left behind; only the piece table gives the
// document as it reads.
mod doc {
    use super::*;

    /// Offset of the flags word in the FIB base, whose bit 9 selects which of
    /// the two table streams is live.
    pub(super) const FIB_FLAGS: usize = 0x000A;
    pub(super) const FLAG_WHICH_TBL_STM: u16 = 0x0200;

    /// The FIB base is fixed-length; the variable-length arrays follow it.
    pub(super) const FIB_BASE_LEN: usize = 32;

    /// Index of the `fcClx`/`lcbClx` pair within `fibRgFcLcb97`, which is an
    /// array of (u32 fc, u32 lcb) pairs. The CLX is where the piece table is.
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
    ) -> Result<String, Box<dyn Error>> {
        let doc = stream(cfb, "WordDocument").ok_or("no WordDocument stream")?;
        let flags = u16_at(&doc, FIB_FLAGS).ok_or("truncated FIB")?;
        // Word keeps two table streams and rewrites them alternately; the flag
        // says which one the current FIB refers to. Reading the wrong one
        // gives a piece table from a previous save.
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

        let mut out = String::new();
        for piece in pieces {
            if out.len() >= MAX_TEXT_BYTES {
                break;
            }
            let Some(bytes) = doc.get(piece.start..piece.end) else {
                // A piece pointing outside the stream is corruption, but the
                // pieces before it were real: keep them rather than discarding
                // a recoverable document.
                break;
            };
            let text = if piece.compressed {
                cp1252(bytes)
            } else {
                utf16le(bytes)
            };
            clean(&text, &mut out);
        }
        if out.trim().is_empty() {
            return Err("no text found in the piece table".into());
        }
        Ok(out)
    }

    /// Walk the FIB's variable-length sections to find `fcClx`/`lcbClx`.
    ///
    /// The sections are self-describing — each is preceded by its own count —
    /// so this works across the FIB versions without a version table.
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
    struct Piece {
        start: usize,
        end: usize,
        compressed: bool,
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

    /// A `PlcPcd` is `n+1` character positions followed by `n` piece
    /// descriptors, so its length determines `n`.
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
            // CPs must advance; a table that goes backwards is corrupt and
            // would otherwise underflow the character count.
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

// .xls — Excel 97-2003 (BIFF8)
//
// The workbook is a flat sequence of records. Cell text is not stored in the
// cells: repeated strings are pooled in a shared-string table (`SST`) and the
// cells hold indices into it. The SST is also the record most likely to
// overflow BIFF's 8224-byte record ceiling, in which case it continues into
// `CONTINUE` records — and a string may be cut mid-way, resuming with a fresh
// width flag. Getting that boundary wrong is the classic way to read an
// Excel file as mojibake.
mod xls {
    use super::*;

    pub(super) const REC_SST: u16 = 0x00FC;
    pub(super) const REC_CONTINUE: u16 = 0x003C;
    pub(super) const REC_LABELSST: u16 = 0x00FD;
    pub(super) const REC_LABEL: u16 = 0x0204;
    pub(super) const REC_RSTRING: u16 = 0x00D6;
    pub(super) const REC_NUMBER: u16 = 0x0203;
    pub(super) const REC_RK: u16 = 0x027E;
    pub(super) const REC_EOF: u16 = 0x000A;
    pub(super) const REC_BOF: u16 = 0x0809;

    /// A record header is a 2-byte id and a 2-byte length.
    pub(super) const REC_HEADER_LEN: usize = 4;

    pub fn extract<F: Read + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
    ) -> Result<String, Box<dyn Error>> {
        // BIFF8 names the stream "Workbook"; BIFF5 and earlier used "Book".
        let book = stream(cfb, "Workbook")
            .or_else(|| stream(cfb, "Book"))
            .ok_or("no Workbook stream")?;

        let records = split_records(&book);
        let strings = shared_strings(&records);

        let mut out = String::new();
        let mut row_open = false;
        for rec in &records {
            if out.len() >= MAX_TEXT_BYTES {
                break;
            }
            let cell = match rec.id {
                REC_LABELSST => u32_at(rec.body, 6)
                    .and_then(|i| strings.get(i as usize))
                    .cloned(),
                // An inline string: cell coordinates, then the string itself.
                REC_LABEL | REC_RSTRING => read_string(&[Segment(rec.body)], &mut 6).ok(),
                REC_NUMBER => number_at(rec.body, 6).map(fmt_number),
                REC_RK => rk_at(rec.body, 6).map(fmt_number),
                // Sheet boundaries: end the line so cells from different
                // sheets do not run together.
                REC_EOF | REC_BOF => {
                    if row_open {
                        out.push('\n');
                        row_open = false;
                    }
                    None
                }
                _ => None,
            };
            if let Some(text) = cell {
                clean(&text, &mut out);
                out.push(' ');
                row_open = true;
            }
        }
        if row_open {
            out.push('\n');
        }
        if out.trim().is_empty() {
            return Err("workbook holds no readable cell text".into());
        }
        Ok(out)
    }

    struct Record<'a> {
        id: u16,
        body: &'a [u8],
    }

    /// Split the stream into records, stopping at the first header that does
    /// not fit — a truncated file keeps whatever records were whole.
    fn split_records(book: &[u8]) -> Vec<Record<'_>> {
        let mut records = Vec::new();
        let mut i = 0usize;
        while let (Some(id), Some(len)) = (u16_at(book, i), u16_at(book, i + 2)) {
            let start = i + REC_HEADER_LEN;
            let Some(body) = book.get(start..start + len as usize) else {
                break;
            };
            records.push(Record { id, body });
            i = start + len as usize;
        }
        records
    }

    /// One contiguous run of SST bytes. A string may straddle two of these,
    /// and the width flag is re-read at every crossing.
    struct Segment<'a>(&'a [u8]);

    /// The shared-string table, in index order.
    ///
    /// Missing or malformed is not fatal: a workbook of nothing but numbers
    /// has no SST at all, and a damaged one still has readable inline strings.
    fn shared_strings(records: &[Record<'_>]) -> Vec<String> {
        let Some(sst_pos) = records.iter().position(|r| r.id == REC_SST) else {
            return Vec::new();
        };
        // The SST and every CONTINUE immediately following it are one logical
        // buffer, but the segment boundaries stay significant.
        let mut segments = vec![Segment(records[sst_pos].body)];
        for rec in &records[sst_pos + 1..] {
            if rec.id != REC_CONTINUE {
                break;
            }
            segments.push(Segment(rec.body));
        }

        // SST header: total string count, then unique string count.
        let Some(unique) = u32_at(segments[0].0, 4) else {
            return Vec::new();
        };
        let mut cursor = 8usize;
        let mut strings = Vec::new();
        // Bounded by the bytes that exist, not by the declared count: `unique`
        // is attacker-controlled and would otherwise size the loop.
        for _ in 0..unique {
            match read_string(&segments, &mut cursor) {
                Ok(s) => strings.push(s),
                // A malformed entry ends the table; the ones before it are
                // still correct, and cells indexing past the end are dropped.
                Err(_) => break,
            }
        }
        strings
    }

    /// Read an `XLUnicodeRichExtendedString` starting at `*cursor`, a byte
    /// offset into the concatenation of `segments`.
    ///
    /// The width flag is per-segment, not per-string: when the character data
    /// crosses into a `CONTINUE`, the continuation begins with a fresh flag
    /// byte and the remaining characters use that width.
    fn read_string(segments: &[Segment<'_>], cursor: &mut usize) -> Result<String, String> {
        let mut at = Cursor {
            segments,
            pos: *cursor,
        };
        let cch = at.u16()? as usize;
        let grbit = at.u8()?;
        let mut wide = grbit & 0x01 != 0;
        let rich = grbit & 0x08 != 0;
        let ext = grbit & 0x04 != 0;
        let runs = if rich { at.u16()? as usize } else { 0 };
        let ext_len = if ext { at.u32()? as usize } else { 0 };

        let mut text = String::new();
        let mut remaining = cch;
        while remaining > 0 {
            // How many characters are left in the segment the cursor is in.
            let in_segment = at.remaining_in_segment()? / if wide { 2 } else { 1 };
            let take = remaining.min(in_segment.max(1));
            let bytes = at.take(take * if wide { 2 } else { 1 })?;
            text.push_str(&if wide { utf16le(bytes) } else { cp1252(bytes) });
            remaining -= take;
            if remaining > 0 {
                // Crossed a CONTINUE boundary: the next byte is a new flag.
                wide = at.u8()? & 0x01 != 0;
            }
        }
        // Formatting runs and the extended (phonetic) block are not text.
        at.skip(runs * 4)?;
        at.skip(ext_len)?;
        *cursor = at.pos;
        Ok(text)
    }

    /// A byte cursor over the SST's segments, aware of where they join.
    struct Cursor<'a, 'b> {
        segments: &'b [Segment<'a>],
        pos: usize,
    }

    impl<'a> Cursor<'a, '_> {
        /// The segment containing `pos`, and the offset within it.
        fn locate(&self) -> Result<(usize, usize), String> {
            let mut left = self.pos;
            for (i, seg) in self.segments.iter().enumerate() {
                if left < seg.0.len() {
                    return Ok((i, left));
                }
                left -= seg.0.len();
            }
            Err("SST cursor past the end".to_string())
        }

        fn remaining_in_segment(&self) -> Result<usize, String> {
            let (i, off) = self.locate()?;
            Ok(self.segments[i].0.len() - off)
        }

        /// `n` bytes, which must not straddle a segment boundary. Callers size
        /// their reads with [`Cursor::remaining_in_segment`] first.
        fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            let (i, off) = self.locate()?;
            let seg = self.segments[i].0;
            let end = off.checked_add(n).ok_or("SST read overflows")?;
            let slice = seg.get(off..end).ok_or("SST read crosses a segment")?;
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

        fn skip(&mut self, n: usize) -> Result<(), String> {
            self.pos = self.pos.checked_add(n).ok_or("SST skip overflows")?;
            Ok(())
        }
    }

    fn number_at(body: &[u8], off: usize) -> Option<f64> {
        let b = body.get(off..off + 8)?;
        Some(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// An `RK` value packs a number into 32 bits: bit 0 says it was scaled by
    /// 100, bit 1 says it is an integer rather than the top 30 bits of a
    /// double's mantissa.
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

    /// Numbers are indexed as the user would type them: whole values without a
    /// trailing `.0`, so a search for "2024" finds the cell holding 2024.
    fn fmt_number(n: f64) -> String {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        }
    }
}

// .ppt — PowerPoint 97-2003
//
// A tree of records, where containers nest and atoms hold data. Slide text
// sits in two atom types that differ only in width. Rather than follow the
// slide-persistence directory to visit slides in order, this walks the tree
// and takes every text atom it finds: order within the file is close enough
// for an index, and the simpler traversal has far less to get wrong.
mod ppt {
    use super::*;

    pub(super) const TEXT_CHARS_ATOM: u16 = 0x0FA0;
    pub(super) const TEXT_BYTES_ATOM: u16 = 0x0FA8;
    /// `CString`, used for titles and notes in some producers.
    pub(super) const CSTRING_ATOM: u16 = 0x0FBA;

    /// A record header is: version/instance u16, type u16, length u32.
    pub(super) const REC_HEADER_LEN: usize = 8;
    /// A record whose low nibble of the first word is 0xF holds child records
    /// rather than data.
    pub(super) const VERSION_CONTAINER: u16 = 0x000F;

    /// Deepest container nesting followed. Real decks are a handful deep; the
    /// bound exists so a file that claims to contain itself cannot recurse
    /// until the stack runs out.
    pub(super) const MAX_DEPTH: u32 = 32;

    pub fn extract<F: Read + std::io::Seek>(
        cfb: &mut cfb::CompoundFile<F>,
    ) -> Result<String, Box<dyn Error>> {
        let doc = stream(cfb, "PowerPoint Document").ok_or("no PowerPoint Document stream")?;
        let mut out = String::new();
        walk(&doc, 0, &mut out);
        if out.trim().is_empty() {
            return Err("no text atoms found in the presentation".into());
        }
        Ok(out)
    }

    fn walk(body: &[u8], depth: u32, out: &mut String) {
        if depth > MAX_DEPTH || out.len() >= MAX_TEXT_BYTES {
            return;
        }
        let mut i = 0usize;
        while let (Some(version), Some(rec_type), Some(len)) =
            (u16_at(body, i), u16_at(body, i + 2), u32_at(body, i + 4))
        {
            let start = i + REC_HEADER_LEN;
            // A length that runs past the end is corruption; the records
            // already read are still good.
            let Some(payload) = body.get(start..start.saturating_add(len as usize)) else {
                return;
            };
            if version & 0x000F == VERSION_CONTAINER {
                walk(payload, depth + 1, out);
            } else {
                match rec_type {
                    TEXT_BYTES_ATOM | CSTRING_ATOM if rec_type == CSTRING_ATOM => {
                        clean(&utf16le(payload), out);
                        out.push('\n');
                    }
                    TEXT_BYTES_ATOM => {
                        clean(&cp1252(payload), out);
                        out.push('\n');
                    }
                    TEXT_CHARS_ATOM => {
                        clean(&utf16le(payload), out);
                        out.push('\n');
                    }
                    _ => {}
                }
            }
            // A zero-length record at depth would spin forever without this.
            let next = start.saturating_add(len as usize);
            if next <= i {
                return;
            }
            i = next;
        }
    }
}

#[cfg(test)]
#[path = "ole_tests.rs"]
mod tests;
