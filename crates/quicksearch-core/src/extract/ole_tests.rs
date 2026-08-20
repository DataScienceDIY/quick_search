use super::*;
use std::io::{Cursor, Write};

/// An OLE2 container holding `streams`, written to a scratch file.
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

// -- decode budgets ---------------------------------------------------

/// Nothing in the format requires pieces to be disjoint, so a piece table may
/// point every entry at the same span: a small file that decodes gigabytes.
/// `clean` drops the whole C0 range, so a run of control bytes produces no
/// output at all and a brake on the emitted text never fires. The budget has
/// to charge what was *read*.
#[test]
fn doc_overlapping_control_pieces_stop_at_the_budget() {
    // One 4 KiB span of control bytes, pointed at over and over.
    const SPAN: usize = 4 * 1024;
    let mut doc = vec![0x01u8; SPAN];
    let marker_at = doc.len();
    doc.extend_from_slice(b"MARKER");

    let mut pieces: Vec<doc::Piece> = (0..64)
        .map(|_| doc::Piece {
            start: 0,
            end: SPAN,
            compressed: true,
        })
        .collect();
    // Reachable only if the budget did not stop the walk first.
    pieces.push(doc::Piece {
        start: marker_at,
        end: doc.len(),
        compressed: true,
    });

    // A budget of half what those pieces decode.
    let out = doc::decode_pieces(&doc, &pieces, SPAN * 32);
    assert!(
        !out.contains("MARKER"),
        "the walk ran past its budget: {out:?}"
    );
    assert!(
        out.trim().is_empty(),
        "control bytes must not survive `clean`: {out:?}"
    );
}

/// The same table under a budget it fits inside must be extracted whole —
/// the brake must not fire early.
#[test]
fn doc_pieces_within_the_budget_are_all_decoded() {
    const SPAN: usize = 4 * 1024;
    let mut doc = vec![0x01u8; SPAN];
    let marker_at = doc.len();
    doc.extend_from_slice(b"MARKER");

    let mut pieces: Vec<doc::Piece> = (0..4)
        .map(|_| doc::Piece {
            start: 0,
            end: SPAN,
            compressed: true,
        })
        .collect();
    pieces.push(doc::Piece {
        start: marker_at,
        end: doc.len(),
        compressed: true,
    });

    let out = doc::decode_pieces(&doc, &pieces, SPAN * 32);
    assert!(out.contains("MARKER"), "stopped early: {out:?}");
}

/// A workbook whose cells are all control characters: every `LABELSST` is six
/// bytes of record and resolves to a shared string that `clean` erases, so the
/// emitted-text brake never advances however many of them there are.
#[test]
fn xls_control_character_cells_stop_at_the_budget() {
    let control: String = std::iter::repeat('\u{1}').take(4096).collect();

    let mut sst = Vec::new();
    sst.extend_from_slice(&le32(2)); // total
    sst.extend_from_slice(&le32(2)); // unique
    sst.extend_from_slice(&sst_string(&control, false));
    sst.extend_from_slice(&sst_string("MARKER", false));

    let mut book = biff(xls::REC_SST, &sst);
    for _ in 0..64 {
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    }
    // Reachable only if the budget did not stop the scan first.
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(1)));

    let out = xls::extract_from_book(&book, 4096 * 32)
        .unwrap_err()
        .to_string();
    assert!(
        out.contains("no readable cell text"),
        "expected the scan to stop before the marker, got: {out}"
    );

    // Under a budget it fits inside, the same workbook reads normally.
    let out = xls::extract_from_book(&book, 4096 * 1024).unwrap();
    assert!(out.contains("MARKER"), "stopped early: {out:?}");
}
