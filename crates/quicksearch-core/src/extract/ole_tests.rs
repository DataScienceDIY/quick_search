use super::*;
use std::io::{Cursor, Write};
use std::path::Path;

/// The one-file form: these assert on extracted text, not on the buffer
/// reuse a pool worker gets.
fn extract_ole_text(path: &Path, parser: Parser) -> Result<String, Box<dyn Error>> {
    let mut out = String::new();
    let mut scratch = Scratch::new(&crate::config::Config::default());
    super::extract_ole_text(File::open(path)?, parser, &mut out, &mut scratch).map(|()| out)
}

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

/// `s` encoded as windows-1252, the formats' 8-bit form.
fn cp1252_bytes(s: &str) -> Vec<u8> {
    encoding_rs::WINDOWS_1252.encode(s).0.into_owned()
}

/// The default limits with a text budget of `text`.
fn limits(text: usize) -> Limits {
    Limits {
        text,
        ..Limits::for_config(&crate::config::Config::default())
    }
}

// -- .doc ------------------------------------------------------------

/// A structurally real Word 97 file: a FIB whose variable sections lead to a
/// CLX holding one `Pcdt`, and a piece table of `(text, compressed)` pieces.
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

    let mut cps = vec![0u32];
    let mut pcds = Vec::new();
    let mut cp = 0u32;
    for (text, compressed) in pieces {
        let start = doc.len();
        let chars = if *compressed {
            let bytes = cp1252_bytes(text);
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
    assert_eq!(
        extract_ole_text(&p, Parser::Doc).unwrap(),
        "Hello from Word\n"
    );
}

#[test]
fn doc_reads_a_wide_piece() {
    let p = container("doc-wide", "doc", &word_doc(&[("Καλημέρα\r", false)]));
    assert_eq!(extract_ole_text(&p, Parser::Doc).unwrap(), "Καλημέρα\n");
}

#[test]
fn doc_concatenates_pieces_in_document_order() {
    let p = container(
        "doc-mixed",
        "doc",
        &word_doc(&[("First ", true), ("δεύτερο ", false), ("third\r", true)]),
    );
    assert_eq!(
        extract_ole_text(&p, Parser::Doc).unwrap(),
        "First δεύτερο third\n"
    );
}

/// Field instructions are markup. `HYPERLINK "http://…"` between 0x13 and
/// 0x14 must not reach the index, while the field's visible result must.
#[test]
fn doc_drops_field_instructions_but_keeps_results() {
    let body = "See \u{13}HYPERLINK \"http://example.com\"\u{14}the site\u{15} now\r";
    let p = container("doc-field", "doc", &word_doc(&[(body, true)]));
    let text = extract_ole_text(&p, Parser::Doc).unwrap();
    assert_eq!(text, "See the site now\n");
    assert!(!text.contains("HYPERLINK"), "{text}");
}

#[test]
fn doc_without_a_table_stream_is_an_error() {
    let streams = word_doc(&[("x\r", true)]);
    let doc_only = vec![streams[0].clone()];
    let p = container("doc-notable", "doc", &doc_only);
    let err = extract_ole_text(&p, Parser::Doc).unwrap_err().to_string();
    assert!(err.contains("1Table"), "{err}");
}

#[test]
fn doc_survives_a_piece_pointing_past_the_stream() {
    let mut streams = word_doc(&[("Good text\r", true), ("later", true)]);
    // Rewrite the second piece's fc to a wild offset.
    let table = &mut streams[1].1;
    let pcd_two = table.len() - 8;
    table[pcd_two + 2..pcd_two + 6].copy_from_slice(&le32(0x3FFF_0000 | doc::FC_COMPRESSED));
    let p = container("doc-oob", "doc", &streams);
    assert_eq!(extract_ole_text(&p, Parser::Doc).unwrap(), "Good text\n");
}

#[test]
fn doc_with_a_truncated_fib_is_an_error() {
    let p = container("doc-trunc", "doc", &[("WordDocument", vec![0u8; 4])]);
    assert!(extract_ole_text(&p, Parser::Doc).is_err());
}

/// The pieces here are plain text, so only the flag can refuse them; a real
/// encrypted file's pieces would index as noise.
#[test]
fn doc_with_fencrypted_is_an_error() {
    let mut streams = word_doc(&[("Hello from Word\r", true)]);
    let flags = doc::FLAG_WHICH_TBL_STM | doc::FLAG_ENCRYPTED;
    streams[0].1[doc::FIB_FLAGS..doc::FIB_FLAGS + 2].copy_from_slice(&le16(flags));
    let p = container("doc-encrypted", "doc", &streams);
    let err = extract_ole_text(&p, Parser::Doc).unwrap_err().to_string();
    assert!(err.contains("encrypted document"), "{err}");
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
        let bytes = cp1252_bytes(s);
        out.extend_from_slice(&le16(bytes.len() as u16));
        out.push(0x00);
        out.extend_from_slice(&bytes);
    }
    out
}

/// An `SST` record body: the total and unique counts, both `strings.len()`,
/// then the strings.
fn sst(strings: &[Vec<u8>]) -> Vec<u8> {
    let n = le32(strings.len() as u32);
    [&n[..], &n[..], &strings.concat()].concat()
}

fn labelsst(index: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&le16(0)); // row
    b.extend_from_slice(&le16(0)); // col
    b.extend_from_slice(&le16(0)); // ixfe
    b.extend_from_slice(&le32(index));
    b
}

/// An `SST` record declaring `count` strings and holding only its header.
fn sst_header(count: u32) -> Vec<u8> {
    biff(xls::REC_SST, &[le32(count), le32(count)].concat())
}

/// `strings` packed into `CONTINUE` records of at most BIFF's 8224 bytes,
/// split only between strings.
fn continues(strings: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = Vec::new();
    for s in strings {
        if body.len() + s.len() > 8224 {
            out.extend_from_slice(&biff(xls::REC_CONTINUE, &body));
            body.clear();
        }
        body.extend_from_slice(s);
    }
    out.extend_from_slice(&biff(xls::REC_CONTINUE, &body));
    out
}

/// A `BOF` body naming `version` (`xls::BIFF5`, or 0x0600 for BIFF8) and a
/// workbook-globals substream.
fn bof(version: u16) -> Vec<u8> {
    [le16(version), le16(0x0005)].concat()
}

/// A BIFF5 `LABEL` body: cell coordinates, then a u16 count and the 8-bit
/// characters, with no flag byte.
fn biff5_label(s: &str) -> Vec<u8> {
    let bytes = cp1252_bytes(s);
    let mut b = [le16(0), le16(0), le16(0), le16(bytes.len() as u16)].concat();
    b.extend_from_slice(&bytes);
    b
}

/// A `MULRK` body: row 0 from column 1, one `(ixfe, RK)` pair per value,
/// then the last column.
fn mulrk(rks: &[u32]) -> Vec<u8> {
    let mut b = [le16(0), le16(1)].concat();
    for rk in rks {
        b.extend_from_slice(&le16(15));
        b.extend_from_slice(&le32(*rk));
    }
    b.extend_from_slice(&le16(rks.len() as u16));
    b
}

#[test]
fn xls_resolves_shared_strings() {
    let sst = sst(&[sst_string("Revenue", false), sst_string("Ω omega", true)]);
    let mut book = biff(xls::REC_SST, &sst);
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(1)));

    let p = container("xls-sst", "xls", &[("Workbook", book)]);
    let text = extract_ole_text(&p, Parser::Xls).unwrap();
    assert!(text.contains("Revenue"), "{text}");
    assert!(text.contains("Ω omega"), "{text}");
}

/// The continuation carries its own width flag; getting that wrong reads the
/// second half of the string as the wrong encoding.
#[test]
fn xls_reads_a_string_split_across_a_continue_record() {
    // Declares 10 compressed characters; only 4 sit in the SST record, the
    // rest continue.
    let sst = sst(&[[&le16(10)[..], &[0x00], b"ABCD"].concat()]);

    let mut cont = vec![0x00u8]; // still compressed after the boundary
    cont.extend_from_slice(b"EFGHIJ");

    let mut book = biff(xls::REC_SST, &sst);
    book.extend_from_slice(&biff(xls::REC_CONTINUE, &cont));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

    let p = container("xls-cont", "xls", &[("Workbook", book)]);
    let text = extract_ole_text(&p, Parser::Xls).unwrap();
    assert!(text.contains("ABCDEFGHIJ"), "{text}");
}

#[test]
fn xls_honours_a_width_change_at_a_continue_boundary() {
    let sst = sst(&[[&le16(6)[..], &[0x00], b"abc"].concat()]);

    let mut cont = vec![0x01u8]; // wide from here on
    for u in "ΔΕΖ".encode_utf16() {
        cont.extend_from_slice(&le16(u));
    }

    let mut book = biff(xls::REC_SST, &sst);
    book.extend_from_slice(&biff(xls::REC_CONTINUE, &cont));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

    let p = container("xls-width", "xls", &[("Workbook", book)]);
    let text = extract_ole_text(&p, Parser::Xls).unwrap();
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
    let text = extract_ole_text(&p, Parser::Xls).unwrap();
    assert!(text.contains("2024"), "{text}");
    assert!(
        !text.contains("2024.0"),
        "whole numbers read as typed: {text}"
    );
}

#[test]
fn xls_drops_an_out_of_range_shared_string_index() {
    let sst = sst(&[sst_string("only", false)]);
    let mut book = biff(xls::REC_SST, &sst);
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(9999)));

    let p = container("xls-oob", "xls", &[("Workbook", book)]);
    assert_eq!(extract_ole_text(&p, Parser::Xls).unwrap().trim(), "only");
}

#[test]
fn xls_ignores_a_lying_shared_string_count() {
    let mut sst = sst(&[sst_string("real", false)]);
    // Both counts claim u32::MAX strings.
    sst[..8].fill(0xFF);
    let mut book = biff(xls::REC_SST, &sst);
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));

    let p = container("xls-liar", "xls", &[("Workbook", book)]);
    assert_eq!(extract_ole_text(&p, Parser::Xls).unwrap().trim(), "real");
}

#[test]
fn xls_record_running_past_the_stream_is_not_fatal() {
    // A record header claiming 5000 bytes in a 10-byte stream.
    let mut book = Vec::new();
    book.extend_from_slice(&le16(xls::REC_LABEL));
    book.extend_from_slice(&le16(5000));
    book.extend_from_slice(b"short");
    let p = container("xls-past", "xls", &[("Workbook", book)]);
    assert!(extract_ole_text(&p, Parser::Xls).is_err());
}

#[test]
fn xls_without_a_workbook_stream_is_an_error() {
    let p = container("xls-none", "xls", &[("Unrelated", vec![1, 2, 3])]);
    assert!(extract_ole_text(&p, Parser::Xls).is_err());
}

/// Each read used to rescan the table's records from the first, so every
/// empty `CONTINUE` ahead of a string was paid for again by each read after
/// it: quadratic, and hours for a few million records.
#[test]
fn xls_sst_with_many_empty_continue_records_is_linear() {
    let text = crate::extract::on_worker(|| {
        let strings: Vec<Vec<u8>> = (0..50_000)
            .map(|i| sst_string(&format!("s{i:05}"), false))
            .collect();
        // Half the table, 200k empty CONTINUEs, then the other half.
        let mut book = sst_header(50_000);
        book.extend_from_slice(&continues(&strings[..25_000]));
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &[]).repeat(200_000));
        book.extend_from_slice(&continues(&strings[25_000..]));
        for i in 0..50_000 {
            book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(i)));
        }
        xls::extract_from_book(&book, &limits(1 << 20)).unwrap()
    });
    let expected: String = (0..50_000).map(|i| format!("s{i:05} ")).collect();
    assert!(text == expected + "\n", "not every string resolved");
}

/// A record boundary inside character data is followed by a flag byte even
/// when the record holds nothing else: a 1-byte `CONTINUE` carries only a
/// flag, and the characters resume, behind a flag of their own, in the next.
#[test]
fn xls_sst_with_one_byte_continue_records_reads_across_them() {
    let text = crate::extract::on_worker(|| {
        // "ΔeΖh": Δ in the SST record, then each character behind an empty
        // record and a flag-only one naming the other width.
        let mut book = biff(
            xls::REC_SST,
            &[&le32(2)[..], &le32(2), &le16(4), &[0x01], &le16('Δ' as u16)].concat(),
        );
        for (wide, unit) in [(false, 'e' as u16), (true, 'Ζ' as u16), (false, 'h' as u16)] {
            book.extend_from_slice(&biff(xls::REC_CONTINUE, &[]));
            book.extend_from_slice(&biff(xls::REC_CONTINUE, &[u8::from(!wide)]));
            let mut body = vec![u8::from(wide)];
            if wide {
                body.extend_from_slice(&le16(unit));
            } else {
                body.push(unit as u8);
            }
            book.extend_from_slice(&biff(xls::REC_CONTINUE, &body));
        }
        // The next string starts a record of its own, with no flag before it.
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &sst_string("next", false)));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(1)));
        xls::extract_from_book(&book, &limits(4096)).unwrap()
    });
    assert_eq!(text, "ΔeΖh next \n");
}

/// Formatting runs and the phonetic block are skipped by length, and they
/// continue into the next record with no flag byte.
#[test]
fn xls_rich_and_phonetic_blocks_crossing_a_continue_are_skipped() {
    let text = crate::extract::on_worker(|| {
        // "rich", compressed, with 2 runs (8 bytes) and a 10-byte phonetic
        // block; 5 of the runs' bytes fit in the SST record.
        let mut first = le16(4).to_vec();
        first.push(0x0C);
        first.extend_from_slice(&le16(2));
        first.extend_from_slice(&le32(10));
        first.extend_from_slice(b"rich");
        first.extend_from_slice(&[0xAA; 5]);
        let mut book = biff(xls::REC_SST, &[&le32(3)[..], &le32(3), &first].concat());
        // The runs' last 3 bytes and 3 of the phonetic block, an empty
        // record, then its last 7 bytes and the next string.
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &[[0xAA; 3], [0xBB; 3]].concat()));
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &[]));
        book.extend_from_slice(&biff(
            xls::REC_CONTINUE,
            &[&[0xBB; 7][..], &sst_string("after", false)].concat(),
        ));
        // A phonetic block claiming to run past the table: the text before
        // it is whole, so it is kept.
        let mut last = le16(4).to_vec();
        last.push(0x04);
        last.extend_from_slice(&le32(u32::MAX));
        last.extend_from_slice(b"tail");
        book.extend_from_slice(&biff(xls::REC_CONTINUE, &last));
        for i in 0..3 {
            book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(i)));
        }
        xls::extract_from_book(&book, &limits(4096)).unwrap()
    });
    assert_eq!(text, "rich after tail \n");
}

/// What follows a `FILEPASS` is ciphertext, so the `NUMBER` would be stored
/// as a garbage number.
#[test]
fn xls_with_filepass_is_an_error() {
    let mut number = [le16(0), le16(0), le16(0)].concat();
    number.extend_from_slice(&2024f64.to_le_bytes());
    let mut book = biff(xls::REC_BOF, &bof(0x0600));
    // wEncryptionType 1 (RC4); the key data is not read.
    book.extend_from_slice(&biff(xls::REC_FILEPASS, &[&le16(1)[..], &[0; 52]].concat()));
    book.extend_from_slice(&biff(xls::REC_NUMBER, &number));
    let p = container("xls-filepass", "xls", &[("Workbook", book)]);
    let err = extract_ole_text(&p, Parser::Xls).unwrap_err().to_string();
    assert!(err.contains("encrypted workbook"), "{err}");
}

/// Read as BIFF8, a BIFF5 label's first character was taken for the flag
/// byte and the cell was dropped.
#[test]
fn biff5_label_is_read() {
    let mut book = biff(xls::REC_BOF, &bof(xls::BIFF5));
    book.extend_from_slice(&biff(xls::REC_LABEL, &biff5_label("abc")));
    book.extend_from_slice(&biff(xls::REC_LABEL, &biff5_label("café")));
    // A count past the body is clamped to it.
    let mut lying = biff5_label("trunc");
    lying[6..8].copy_from_slice(&le16(500));
    book.extend_from_slice(&biff(xls::REC_LABEL, &lying));
    let p = container("xls-biff5", "xls", &[("Book", book)]);
    assert_eq!(
        extract_ole_text(&p, Parser::Xls).unwrap(),
        "abc café trunc \n"
    );
}

#[test]
fn biff5_rstring_is_read() {
    // The text, then one formatting run: a count byte and 2 bytes a run.
    let mut rstring = biff5_label("rich five");
    rstring.extend_from_slice(&[1, 0, 1]);
    let mut book = biff(xls::REC_BOF, &bof(xls::BIFF5));
    book.extend_from_slice(&biff(xls::REC_RSTRING, &rstring));
    let p = container("xls-biff5-rstring", "xls", &[("Book", book)]);
    assert_eq!(extract_ole_text(&p, Parser::Xls).unwrap(), "rich five \n");
}

/// Excel and LibreOffice store a row's adjacent numbers as one `MULRK`.
#[test]
fn xls_reads_mulrk_cells() {
    // An integer, one scaled by 100, and a double's top 30 bits.
    let rks = [
        (7 << 2) | 0x02,
        (1234 << 2) | 0x03,
        (2.5f64.to_bits() >> 32) as u32,
    ];
    let body = mulrk(&rks);
    let text = crate::extract::on_worker(move || {
        let read = |body: &[u8]| {
            xls::extract_from_book(&biff(xls::REC_MULRK, body), &limits(4096)).unwrap()
        };
        // Whole; without its last column; cut inside the third pair.
        [
            read(&body),
            read(&body[..4 + 6 * 3]),
            read(&body[..4 + 6 * 2 + 3]),
        ]
    });
    assert_eq!(text, ["7 12.34 2.5 \n", "7 12.34 2.5 \n", "7 12.34 \n"]);
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
    let bytes = cp1252_bytes("Slide title");
    let mut wide = Vec::new();
    for u in "Ωmega body".encode_utf16() {
        wide.extend_from_slice(&le16(u));
    }
    let mut doc = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
    doc.extend_from_slice(&ppt_record(0x0000, ppt::TEXT_CHARS_ATOM, &wide));

    let p = container("ppt-atoms", "ppt", &[("PowerPoint Document", doc)]);
    let text = extract_ole_text(&p, Parser::Ppt).unwrap();
    assert!(text.contains("Slide title"), "{text}");
    assert!(text.contains("Ωmega body"), "{text}");
}

/// The text of `path` through the registry, under the MIME sniffed from its
/// name and bytes, as a real index reads it; and the parser that MIME names.
fn extract_as_indexed(path: &Path) -> (Option<Parser>, String) {
    let head = std::fs::read(path).unwrap();
    let mime = crate::mime::guess_mime_from_head(path, &head).expect("sniffed as something");
    let parser = crate::extract::office::parser_for(&mime.to_ascii_lowercase());
    let text = crate::extract::Registry::default_set()
        .extract_to_string(path, mime, &crate::config::Config::default())
        .unwrap_or_else(|e| panic!("{} failed: {e}", path.display()))
        .unwrap_or_else(|| {
            panic!(
                "{} is sniffed as {mime}, which nothing claims",
                path.display()
            )
        });
    (parser, text)
}

/// Every name the sniff gives a legacy format's document, template,
/// slideshow, backup or wizard reaches that format's parser, end to end.
#[test]
fn legacy_alias_names_reach_the_same_parser() {
    let bytes = cp1252_bytes("Slide title");
    let deck = vec![(
        "PowerPoint Document",
        ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes),
    )];
    // The trailing `\r` is Word's paragraph mark, which extracts as `\n`.
    let prose = word_doc(&[("Hello from Word\r", true)]);
    let mut book = biff(xls::REC_SST, &sst(&[sst_string("Revenue", false)]));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    let sheet = vec![("Workbook", book)];
    type AliasCase<'a> = (&'a [(&'a str, Vec<u8>)], Parser, &'a [&'a str], &'a str);
    let cases: &[AliasCase] = &[
        (
            &deck,
            Parser::Ppt,
            &["ppt", "pps", "pot", "ppa", "pwz"],
            "Slide title",
        ),
        (
            &prose,
            Parser::Doc,
            &["doc", "dot", "wbk", "wiz"],
            "Hello from Word\n",
        ),
        (&sheet, Parser::Xls, &["xls", "xlt", "xlk"], "Revenue"),
    ];
    for (streams, parser, names, expected) in cases {
        for ext in *names {
            let p = container(&format!("ole-alias-{ext}"), ext, streams);
            let (reached, text) = extract_as_indexed(&p);
            assert_eq!(reached, Some(*parser), ".{ext} reached the wrong parser");
            assert!(text.contains(expected), ".{ext} gave {text:?}");
        }
    }
}

/// An add-in workbook is sniffed as `application/vnd.ms-excel` like any
/// workbook; dispatch by extension knew no `.xla`, so it was stored as done
/// and empty.
#[test]
fn an_xla_named_workbook_reaches_the_xls_parser() {
    let mut book = biff(xls::REC_SST, &sst(&[sst_string("Add-in macros", false)]));
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    let p = container("ole-xla", "xla", &[("Workbook", book)]);
    let (parser, text) = extract_as_indexed(&p);
    assert_eq!(parser, Some(Parser::Xls));
    assert_eq!(text.trim(), "Add-in macros");
}

#[test]
fn ppt_descends_into_containers() {
    let bytes = cp1252_bytes("Nested deep");
    let atom = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
    let inner = ppt_record(0x000F, 0x0FF0, &atom);
    let outer = ppt_record(0x000F, 0x03E8, &inner);

    let p = container("ppt-nest", "ppt", &[("PowerPoint Document", outer)]);
    assert!(extract_ole_text(&p, Parser::Ppt)
        .unwrap()
        .contains("Nested deep"));
}

#[test]
fn ppt_bounds_container_recursion() {
    let bytes = cp1252_bytes("buried");
    let mut rec = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &bytes);
    for _ in 0..(ppt::MAX_DEPTH + 20) {
        rec = ppt_record(0x000F, 0x0FF0, &rec);
    }
    let p = container("ppt-deep", "ppt", &[("PowerPoint Document", rec)]);
    assert!(extract_ole_text(&p, Parser::Ppt).is_err());
}

#[test]
fn ppt_record_running_past_the_stream_is_not_fatal() {
    let mut doc = Vec::new();
    doc.extend_from_slice(&le16(0x0000));
    doc.extend_from_slice(&le16(ppt::TEXT_BYTES_ATOM));
    doc.extend_from_slice(&le32(u32::MAX));
    doc.extend_from_slice(b"short");
    let p = container("ppt-past", "ppt", &[("PowerPoint Document", doc)]);
    assert!(extract_ole_text(&p, Parser::Ppt).is_err());
}

#[test]
fn ppt_without_its_stream_is_an_error() {
    let p = container("ppt-none", "ppt", &[("Pictures", vec![0; 4])]);
    assert!(extract_ole_text(&p, Parser::Ppt).is_err());
}

#[test]
fn ppt_zero_length_records_still_advance() {
    let mut deck = ppt_record(0x0000, 0x0FF5, &[]).repeat(1000);
    deck.extend_from_slice(&ppt_record(0x000F, 0x0FF0, &[]));
    deck.extend_from_slice(&ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, b"after"));
    let out = crate::extract::on_worker(move || {
        let mut out = String::new();
        ppt::walk(&deck, 0, &mut out, 4096);
        out
    });
    assert_eq!(out, "after\n");
}

// -- container level --------------------------------------------------

#[test]
fn a_non_compound_file_is_an_error() {
    let dir = crate::testutil::scratch_dir("ole-notcfb");
    let p = dir.join("doc.doc");
    crate::testutil::touch(&p, b"this is not an OLE2 compound file at all");
    let err = extract_ole_text(&p, Parser::Doc).unwrap_err().to_string();
    assert!(err.contains("compound file"), "{err}");
}

#[test]
fn an_empty_file_is_an_error() {
    let dir = crate::testutil::scratch_dir("ole-empty");
    let p = dir.join("doc.xls");
    crate::testutil::touch(&p, b"");
    assert!(extract_ole_text(&p, Parser::Xls).is_err());
}

// -- decode budgets ---------------------------------------------------

/// Nothing in the format requires pieces to be disjoint, so a small file can
/// decode gigabytes; `clean` drops the whole C0 range, so an emitted-text
/// brake never fires. The budget has to charge what was *read*.
#[test]
fn doc_decoding_charges_the_bytes_read_to_the_budget() {
    const SPAN: usize = 4 * 1024;
    let mut doc = vec![0x01u8; SPAN];
    let marker_at = doc.len();
    doc.extend_from_slice(b"MARKER");

    // (overlapping control pieces, what survives): the budget below is 32
    // spans, so 64 exhaust it before the marker piece and 4 do not.
    for (overlapping, expected) in [(64, ""), (4, "MARKER")] {
        let mut pieces: Vec<doc::Piece> = (0..overlapping)
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
        assert_eq!(out.trim(), expected, "{overlapping} overlapping pieces");
    }
}

/// Every `LABELSST` resolves to a shared string `clean` erases, so an
/// emitted-text brake never advances however many of them there are.
#[test]
fn xls_control_character_cells_stop_at_the_budget() {
    let control: String = std::iter::repeat_n('\u{1}', 4096).collect();

    let sst = sst(&[sst_string(&control, false), sst_string("MARKER", false)]);
    let mut book = biff(xls::REC_SST, &sst);
    for _ in 0..64 {
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
    }
    // Reachable only if the budget did not stop the scan first.
    book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(1)));

    let out = xls::extract_from_book(&book, &limits(4096 * 32))
        .unwrap_err()
        .to_string();
    assert!(
        out.contains("no readable cell text"),
        "expected the scan to stop before the marker, got: {out}"
    );

    let out = xls::extract_from_book(&book, &limits(4096 * 1024)).unwrap();
    assert!(out.contains("MARKER"), "stopped early: {out:?}");
}

/// Characters are charged, not bytes: charging bytes gave a wide piece, two
/// bytes a character, half the allowance of a compressed one.
#[test]
fn doc_wide_pieces_get_the_whole_allowance() {
    let mut doc: Vec<u8> = "a".repeat(600).encode_utf16().flat_map(le16).collect();
    let wide_end = doc.len();
    doc.extend_from_slice(b"MARKER");
    let pieces = [
        doc::Piece {
            start: 0,
            end: wide_end,
            compressed: false,
        },
        doc::Piece {
            start: wide_end,
            end: doc.len(),
            compressed: true,
        },
    ];
    let out = doc::decode_pieces(&doc, &pieces, 1000);
    assert_eq!(out, "a".repeat(600) + "MARKER");
}

#[test]
fn a_huge_single_piece_stops_at_the_allowance() {
    let (compressed, wide) = crate::extract::on_worker(|| {
        let doc = vec![b'a'; 4 << 20];
        let piece = |compressed| doc::Piece {
            start: 0,
            end: doc.len(),
            compressed,
        };
        (
            doc::decode_pieces(&doc, &[piece(true)], 4096),
            doc::decode_pieces(&doc, &[piece(false)], 4096),
        )
    });
    assert_eq!(compressed.len(), 4096);
    assert_eq!(wide.chars().count(), 4096);
}

/// Empty strings decode to nothing and cost a slot each, so the table's bound
/// charges the slot as well as the bytes walked.
#[test]
fn xls_sst_stops_at_the_inflate_budget() {
    let (cut, whole) = crate::extract::on_worker(|| {
        let mut strings = vec![sst_string("first", false)];
        strings.extend(std::iter::repeat_n(sst_string("", false), 99_998));
        strings.push(sst_string("last", false));
        let mut book = sst_header(100_000);
        book.extend_from_slice(&continues(&strings));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(0)));
        book.extend_from_slice(&biff(xls::REC_LABELSST, &labelsst(99_999)));
        let read = |inflate| {
            let limits = Limits {
                inflate,
                ..limits(4096)
            };
            xls::extract_from_book(&book, &limits).unwrap()
        };
        (read(64 * 1024), read(64 << 20))
    });
    assert_eq!(cut, "first \n", "the cell past the cut is dropped");
    assert_eq!(whole, "first last \n");
}

#[test]
fn ppt_stops_at_the_budget_inside_one_container() {
    let atom = ppt_record(0x0000, ppt::TEXT_BYTES_ATOM, &[b'x'; 100]);
    let deck = ppt_record(0x000F, 0x03E8, &atom.repeat(10_000));
    let out = crate::extract::on_worker(move || {
        let mut out = String::new();
        ppt::walk(&deck, 0, &mut out, 1000);
        out
    });
    // Ten atoms of 101 bytes reach the budget; the other 9 990 are not read.
    assert_eq!(out.len(), 10 * 101);
}
