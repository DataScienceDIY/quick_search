//! A minimal ZIP writer, stored (uncompressed) entries only.
//!
//! Exists so the pptx and ODF containers are not built by `zip` 0.6 — the same
//! crate `extract::office` reads them back with. It is about sixty lines of
//! well-specified structure (APPNOTE 4.3), which is a smaller thing to get
//! wrong than the agreement it is here to test.
//!
//! Stored rather than deflated because the reader accepts both (see the `zip`
//! entry in `Cargo.toml`) and stored needs no compressor. `crc32fast` supplies
//! the one field that cannot be hand-waved.
//!
//! No zip64, no data descriptors, no unicode path extra field: every corpus
//! member has an ASCII name and is a few kilobytes.

/// One member of the archive.
pub struct Entry<'a> {
    pub name: &'a str,
    pub body: &'a [u8],
}

/// Serialize `entries` into a complete `.zip`.
pub fn archive(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    // (crc, size, local header offset) per entry, for the central directory.
    let mut placed: Vec<(u32, usize, usize)> = Vec::with_capacity(entries.len());

    for entry in entries {
        let offset = out.len();
        let crc = crc32fast::hash(entry.body);
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
                                                    // A fixed DOS timestamp — 1980-01-01 00:00:00, the epoch of the
                                                    // format. Nothing reads it, and a real clock would make two runs of
                                                    // the same seed produce different bytes.
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0x0021u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(entry.body.len() as u32).to_le_bytes()); // compressed
        out.extend_from_slice(&(entry.body.len() as u32).to_le_bytes()); // uncompressed
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(entry.body);
        placed.push((crc, entry.body.len(), offset));
    }

    let cd_start = out.len();
    for (entry, (crc, size, offset)) in entries.iter().zip(&placed) {
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0x0021u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(*size as u32).to_le_bytes());
        out.extend_from_slice(&(*size as u32).to_le_bytes());
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&(*offset as u32).to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
    }
    let cd_size = out.len() - cd_start;

    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central directory
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd_size as u32).to_le_bytes());
    out.extend_from_slice(&(cd_start as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// Escape `s` for XML character data.
///
/// The corpus plants `&` and `<` nowhere by default, but the writers below
/// route every piece of body text through this so a future sentence
/// containing one cannot silently produce an unparseable container.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}
