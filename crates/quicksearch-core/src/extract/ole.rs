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

// ---------------------------------------------------------------------------
// Bounds-checked little-endian reads
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// .doc — Word 97-2003
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// .xls — Excel 97-2003 (BIFF8)
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// .ppt — PowerPoint 97-2003
// ---------------------------------------------------------------------------
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
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// An OLE2 container holding `streams`, written to a scratch file.
    ///
    /// Built in-process rather than checked in as fixture blobs: the point of
    /// most of these tests is a *malformed* file, and hand-editing binary
    /// fixtures to be malformed in a specific way is unreviewable.
    fn container(tag: &str, ext: &str, streams: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
        let path = crate::testutil::scratch_dir(tag).join(format!("doc.{ext}"));
        let mut cfb = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for (name, body) in streams {
            let mut s = cfb.create_stream(name).unwrap();
            s.write_all(body).unwrap();
            s.flush().unwrap();
        }
        std::fs::write(&path, cfb.into_inner().into_inner()).unwrap();
        path
    }

    fn le16(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn le32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    // -- .doc ------------------------------------------------------------

    /// A minimal but structurally real Word 97 file: a FIB whose variable
    /// sections lead to a CLX, a CLX holding one `Pcdt`, and a piece table
    /// with `pieces` entries pointing into the character data.
    ///
    /// `pieces` are `(text, compressed)`.
    fn word_doc(pieces: &[(&str, bool)]) -> Vec<(&'static str, Vec<u8>)> {
        // Character data starts after the FIB; 2048 is comfortably past it.
        const TEXT_BASE: usize = 2048;
        let mut doc = vec![0u8; TEXT_BASE];
        doc[0..2].copy_from_slice(&le16(0xA5EC)); // wIdent
        doc[doc::FIB_FLAGS..doc::FIB_FLAGS + 2].copy_from_slice(&le16(doc::FLAG_WHICH_TBL_STM));

        // Variable sections: csw, rgW97, cslw, rgLw97, cbRgFcLcb, blob.
        let csw = 14u16;
        let cslw = 22u16;
        let pairs = 93u16;
        let mut off = doc::FIB_BASE_LEN;
        doc[off..off + 2].copy_from_slice(&le16(csw));
        off += 2 + csw as usize * 2;
        doc[off..off + 2].copy_from_slice(&le16(cslw));
        off += 2 + cslw as usize * 4;
        doc[off..off + 2].copy_from_slice(&le16(pairs));
        let blob = off + 2;

        // Lay the pieces' character data into the document stream.
        let mut cps = vec![0u32];
        let mut pcds = Vec::new();
        let mut cp = 0u32;
        for (text, compressed) in pieces {
            let start = doc.len();
            let chars = if *compressed {
                let bytes = encoding_rs::WINDOWS_1252.encode(text).0.into_owned();
                doc.extend_from_slice(&bytes);
                bytes.len()
            } else {
                let units: Vec<u16> = text.encode_utf16().collect();
                for u in &units {
                    doc.extend_from_slice(&le16(*u));
                }
                units.len()
            };
            cp += chars as u32;
            cps.push(cp);
            // A compressed piece's fc is the byte offset doubled, with the
            // compression bit set.
            let fc = if *compressed {
                ((start as u32) * 2) | doc::FC_COMPRESSED
            } else {
                start as u32
            };
            pcds.extend_from_slice(&le16(0)); // flags
            pcds.extend_from_slice(&le32(fc));
            pcds.extend_from_slice(&le16(0)); // prm
        }

        let mut plc = Vec::new();
        for c in &cps {
            plc.extend_from_slice(&le32(*c));
        }
        plc.extend_from_slice(&pcds);

        let mut clx = vec![doc::CLXT_PCDT];
        clx.extend_from_slice(&le32(plc.len() as u32));
        clx.extend_from_slice(&plc);

        // The table stream: the CLX at a known offset.
        let clx_at = 16usize;
        let mut table = vec![0u8; clx_at];
        table.extend_from_slice(&clx);
        doc[blob + doc::CLX_PAIR_INDEX * 8..blob + doc::CLX_PAIR_INDEX * 8 + 4]
            .copy_from_slice(&le32(clx_at as u32));
        doc[blob + doc::CLX_PAIR_INDEX * 8 + 4..blob + doc::CLX_PAIR_INDEX * 8 + 8]
            .copy_from_slice(&le32(clx.len() as u32));

        vec![("WordDocument", doc), ("1Table", table)]
    }

    #[test]
    fn doc_reads_a_compressed_piece() {
        let p = container("doc-cp", "doc", &word_doc(&[("Hello from Word\r", true)]));
        assert_eq!(extract_ole_text(&p, "doc").unwrap(), "Hello from Word\n");
    }

    #[test]
    fn doc_reads_a_wide_piece() {
        let p = container("doc-wide", "doc", &word_doc(&[("Καλημέρα\r", false)]));
        assert_eq!(extract_ole_text(&p, "doc").unwrap(), "Καλημέρα\n");
    }

    /// The whole reason the piece table exists: text is assembled in CP order,
    /// not in the order it happens to sit in the stream.
    #[test]
    fn doc_concatenates_pieces_in_document_order() {
        let p = container(
            "doc-mixed",
            "doc",
            &word_doc(&[("First ", true), ("δεύτερο ", false), ("third\r", true)]),
        );
        assert_eq!(
            extract_ole_text(&p, "doc").unwrap(),
            "First δεύτερο third\n"
        );
    }

    /// Field instructions are markup. `HYPERLINK "http://…"` between 0x13 and
    /// 0x14 must not reach the index, while the field's visible result must.
    #[test]
    fn doc_drops_field_instructions_but_keeps_results() {
        let body = "See \u{13}HYPERLINK \"http://example.com\"\u{14}the site\u{15} now\r";
        let p = container("doc-field", "doc", &word_doc(&[(body, true)]));
        let text = extract_ole_text(&p, "doc").unwrap();
        assert_eq!(text, "See the site now\n");
        assert!(!text.contains("HYPERLINK"), "{text}");
    }

    #[test]
    fn doc_without_a_table_stream_is_an_error() {
        let streams = word_doc(&[("x\r", true)]);
        let doc_only = vec![streams[0].clone()];
        let p = container("doc-notable", "doc", &doc_only);
        let err = extract_ole_text(&p, "doc").unwrap_err().to_string();
        assert!(err.contains("1Table"), "{err}");
    }

    /// A piece whose byte range lies outside the stream. The pieces before it
    /// are real text and are kept; nothing panics on the slice.
    #[test]
    fn doc_survives_a_piece_pointing_past_the_stream() {
        let mut streams = word_doc(&[("Good text\r", true), ("later", true)]);
        // Rewrite the second piece's fc to a wild offset.
        let table = &mut streams[1].1;
        let pcd_two = table.len() - 8;
        table[pcd_two + 2..pcd_two + 6].copy_from_slice(&le32(0x3FFF_0000 | doc::FC_COMPRESSED));
        let p = container("doc-oob", "doc", &streams);
        assert_eq!(extract_ole_text(&p, "doc").unwrap(), "Good text\n");
    }

    #[test]
    fn doc_with_a_truncated_fib_is_an_error() {
        let p = container("doc-trunc", "doc", &[("WordDocument", vec![0u8; 4])]);
        assert!(extract_ole_text(&p, "doc").is_err());
    }

    // -- .xls ------------------------------------------------------------

    fn biff(id: u16, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&le16(id));
        r.extend_from_slice(&le16(body.len() as u16));
        r.extend_from_slice(body);
        r
    }

    /// An `XLUnicodeRichExtendedString` with no rich or extended parts.
    fn sst_string(s: &str, wide: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if wide {
            let units: Vec<u16> = s.encode_utf16().collect();
            out.extend_from_slice(&le16(units.len() as u16));
            out.push(0x01);
            for u in units {
                out.extend_from_slice(&le16(u));
            }
        } else {
            let bytes = encoding_rs::WINDOWS_1252.encode(s).0.into_owned();
            out.extend_from_slice(&le16(bytes.len() as u16));
            out.push(0x00);
            out.extend_from_slice(&bytes);
        }
        out
    }

    fn labelsst(index: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&le16(0)); // row
        b.extend_from_slice(&le16(0)); // col
        b.extend_from_slice(&le16(0)); // ixfe
        b.extend_from_slice(&le32(index));
        b
    }

    #[test]
    fn xls_resolves_shared_strings() {
        let mut sst = Vec::new();
        sst.extend_from_slice(&le32(2)); // total
        sst.extend_from_slice(&le32(2)); // unique
        sst.extend_from_slice(&sst_string("Revenue", false));
        sst.extend_from_slice(&sst_string("Ω omega", true));

        let mut book = biff(xls::REC_SST, &sst);
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(1)));

        let p = container("xls-sst", "xls", &[("Workbook", book)]);
        let text = extract_ole_text(&p, "xls").unwrap();
        assert!(text.contains("Revenue"), "{text}");
        assert!(text.contains("Ω omega"), "{text}");
    }

    /// The classic correctness trap: a shared string cut across a `CONTINUE`
    /// boundary, where the continuation carries its own width flag. Getting
    /// this wrong reads the second half as the wrong encoding.
    #[test]
    fn xls_reads_a_string_split_across_a_continue_record() {
        let mut sst = Vec::new();
        sst.extend_from_slice(&le32(1));
        sst.extend_from_slice(&le32(1));
        // A 10-character compressed string, but only the first 4 characters
        // fit in the SST record; the rest continue.
        sst.extend_from_slice(&le16(10));
        sst.push(0x00); // compressed
        sst.extend_from_slice(b"ABCD");

        let mut cont = vec![0x00u8]; // still compressed after the boundary
        cont.extend_from_slice(b"EFGHIJ");

        let mut book = biff(xls::REC_SST, &sst);
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &cont));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

        let p = container("xls-cont", "xls", &[("Workbook", book)]);
        let text = extract_ole_text(&p, "xls").unwrap();
        assert!(text.contains("ABCDEFGHIJ"), "{text}");
    }

    /// A continuation may also switch width mid-string.
    #[test]
    fn xls_honours_a_width_change_at_a_continue_boundary() {
        let mut sst = Vec::new();
        sst.extend_from_slice(&le32(1));
        sst.extend_from_slice(&le32(1));
        sst.extend_from_slice(&le16(6));
        sst.push(0x00);
        sst.extend_from_slice(b"abc");

        let mut cont = vec![0x01u8]; // wide from here on
        for u in "ΔΕΖ".encode_utf16() {
            cont.extend_from_slice(&le16(u));
        }

        let mut book = biff(xls::REC_SST, &sst);
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &cont));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

        let p = container("xls-width", "xls", &[("Workbook", book)]);
        let text = extract_ole_text(&p, "xls").unwrap();
        assert!(text.contains("abcΔΕΖ"), "{text}");
    }

    #[test]
    fn xls_indexes_numbers_as_typed() {
        let mut number = Vec::new();
        number.extend_from_slice(&le16(0));
        number.extend_from_slice(&le16(0));
        number.extend_from_slice(&le16(0));
        number.extend_from_slice(&2024f64.to_le_bytes());

        let book = biff(xls::REC_NUMBER, &number);
        let p = container("xls-num", "xls", &[("Workbook", book)]);
        let text = extract_ole_text(&p, "xls").unwrap();
        assert!(text.contains("2024"), "{text}");
        assert!(
            !text.contains("2024.0"),
            "whole numbers read as typed: {text}"
        );
    }

    /// A cell indexing past the end of the shared-string table. Dropped, not
    /// panicked on, and the valid cells around it survive.
    #[test]
    fn xls_drops_an_out_of_range_shared_string_index() {
        let mut sst = Vec::new();
        sst.extend_from_slice(&le32(1));
        sst.extend_from_slice(&le32(1));
        sst.extend_from_slice(&sst_string("only", false));

        let mut book = biff(xls::REC_SST, &sst);
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(9999)));

        let p = container("xls-oob", "xls", &[("Workbook", book)]);
        assert_eq!(extract_ole_text(&p, "xls").unwrap().trim(), "only");
    }

    /// An SST whose declared count far exceeds the bytes present. The loop
    /// must be bounded by the data, not the header.
    #[test]
    fn xls_ignores_a_lying_shared_string_count() {
        let mut sst = Vec::new();
        sst.extend_from_slice(&le32(u32::MAX));
        sst.extend_from_slice(&le32(u32::MAX));
        sst.extend_from_slice(&sst_string("real", false));

        let mut book = biff(xls::REC_SST, &sst);
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

        let p = container("xls-liar", "xls", &[("Workbook", book)]);
        assert_eq!(extract_ole_text(&p, "xls").unwrap().trim(), "real");
    }

    #[test]
    fn xls_record_running_past_the_stream_is_not_fatal() {
        // A record header claiming 5000 bytes in a 10-byte stream.
        let mut book = Vec::new();
        book.extend_from_slice(&le16(xls::REC_LABEL));
        book.extend_from_slice(&le16(5000));
        book.extend_from_slice(b"short");
        let p = container("xls-past", "xls", &[("Workbook", book)]);
        // No readable text, reported as an error rather than a panic.
        assert!(extract_ole_text(&p, "xls").is_err());
    }

    #[test]
    fn xls_without_a_workbook_stream_is_an_error() {
        let p = container("xls-none", "xls", &[("Unrelated", vec![1, 2, 3])]);
        assert!(extract_ole_text(&p, "xls").is_err());
    }

    // -- .ppt ------------------------------------------------------------

    fn ppt_record(version: u16, rec_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&le16(version));
        r.extend_from_slice(&le16(rec_type));
        r.extend_from_slice(&le32(payload.len() as u32));
        r.extend_from_slice(payload);
        r
    }

    #[test]
    fn ppt_reads_both_atom_widths() {
        let bytes = encoding_rs::WINDOWS_1252
            .encode("Slide title")
            .0
            .into_owned();
        let mut wide = Vec::new();
        for u in "Ωmega body".encode_utf16() {
            wide.extend_from_slice(&le16(u));
        }
        let mut doc = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
        doc.extend_from_slice(&ppt_record(0x0000, ppt::TEXT_CHARS_ATOM, &wide));

        let p = container("ppt-atoms", "ppt", &[("PowerPoint Document", doc)]);
        let text = extract_ole_text(&p, "ppt").unwrap();
        assert!(text.contains("Slide title"), "{text}");
        assert!(text.contains("Ωmega body"), "{text}");
    }

    /// Atoms live inside nested containers; the walk has to descend to them.
    #[test]
    fn ppt_descends_into_containers() {
        let bytes = encoding_rs::WINDOWS_1252
            .encode("Nested deep")
            .0
            .into_owned();
        let atom = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
        let inner = ppt_record(0x000F, 0x0FF0, &atom);
        let outer = ppt_record(0x000F, 0x03E8, &inner);

        let p = container("ppt-nest", "ppt", &[("PowerPoint Document", outer)]);
        assert!(extract_ole_text(&p, "ppt").unwrap().contains("Nested deep"));
    }

    /// A container that claims to hold itself. The depth bound is what stops
    /// this from exhausting the stack.
    #[test]
    fn ppt_bounds_container_recursion() {
        // Each level wraps the last, well past MAX_DEPTH.
        let bytes = encoding_rs::WINDOWS_1252.encode("buried").0.into_owned();
        let mut rec = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
        for _ in 0..(ppt::MAX_DEPTH + 20) {
            rec = ppt_record(0x000F, 0x0FF0, &rec);
        }
        let p = container("ppt-deep", "ppt", &[("PowerPoint Document", rec)]);
        // Too deep to reach the text — an error, not a stack overflow.
        assert!(extract_ole_text(&p, "ppt").is_err());
    }

    #[test]
    fn ppt_record_running_past_the_stream_is_not_fatal() {
        let mut doc = Vec::new();
        doc.extend_from_slice(&le16(0x0000));
        doc.extend_from_slice(&le16(ppt::TEXT_BYTES_ATOM));
        doc.extend_from_slice(&le32(u32::MAX));
        doc.extend_from_slice(b"short");
        let p = container("ppt-past", "ppt", &[("PowerPoint Document", doc)]);
        assert!(extract_ole_text(&p, "ppt").is_err());
    }

    #[test]
    fn ppt_without_its_stream_is_an_error() {
        let p = container("ppt-none", "ppt", &[("Pictures", vec![0; 4])]);
        assert!(extract_ole_text(&p, "ppt").is_err());
    }

    // -- container level --------------------------------------------------

    #[test]
    fn a_non_compound_file_is_an_error() {
        let dir = crate::testutil::scratch_dir("ole-notcfb");
        let p = dir.join("doc.doc");
        crate::testutil::touch(&p, b"this is not an OLE2 compound file at all");
        let err = extract_ole_text(&p, "doc").unwrap_err().to_string();
        assert!(err.contains("compound file"), "{err}");
    }

    #[test]
    fn an_empty_file_is_an_error() {
        let dir = crate::testutil::scratch_dir("ole-empty");
        let p = dir.join("doc.xls");
        crate::testutil::touch(&p, b"");
        assert!(extract_ole_text(&p, "xls").is_err());
    }
}
