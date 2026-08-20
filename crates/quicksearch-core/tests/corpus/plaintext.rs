//! Plain-text corpus files: one per extension family, plus one per encoding.
//!
//! Written with `std` alone. The one place that needs an encoder — windows-1252
//! — gets a hand-rolled one rather than `encoding_rs`, which is what
//! `textenc::decode_text` reads it back with.
//!
//! The extension sweep is not decoration: it drives all three stages of
//! `mime::guess_mime_from_head`. `.bat` comes from the override table, most
//! come from `mime_guess`, and the extensionless `README` reaches the text
//! sniff with no other evidence at all. Several of the extensions here also
//! resolve to `EXTRA_TEXT_MIMES` entries (`.json`, `.sql`, `.svg`, `.m3u`,
//! `.eml`), which the plaintext extractor claims only because it is registered
//! ahead of the audio one.

use std::path::{Path, PathBuf};

use super::{BodyFn, Charset, Lcg, Sample};

/// Encode `s` as windows-1252, replacing anything the codepage cannot hold.
///
/// cp1252 is Latin-1 with 27 printable characters filled into the C1 range, so
/// the whole encoder is: identity below 0x100 except for that range, plus the
/// reverse of the table for it. Hand-rolled deliberately — `encoding_rs` is the
/// decoder under test.
///
/// Shared with [`super::pdf`], whose base-14 font is WinAnsi — the same
/// repertoire under a different name.
pub fn to_cp1252(s: &str) -> Vec<u8> {
    /// The 0x80-0x9F block, in order. `\u{FFFD}` marks the five unassigned
    /// slots, which nothing maps onto.
    const C1: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        if code < 0x80 || (0xA0..=0xFF).contains(&code) {
            out.push(code as u8);
        } else if let Some(i) = C1.iter().position(|&t| t == c && t != '\u{FFFD}') {
            out.push(0x80 + i as u8);
        } else {
            out.push(b'?');
        }
    }
    out
}

/// Extensions that exercise a distinct route through `guess_mime_from_head`,
/// paired with a wrapper that makes the file plausible for its type.
///
/// The wrapper matters less than it looks — the plaintext extractor decodes
/// rather than parses, so nothing here is validated as JSON or XML. It is
/// there so a human opening the scratch directory sees files, not lipsum with
/// a misleading suffix.
const EXTENSIONS: &[(&str, Wrapper)] = &[
    ("txt", Wrapper::Raw),
    ("md", Wrapper::Raw),
    ("log", Wrapper::Raw),
    ("csv", Wrapper::Csv),
    ("html", Wrapper::Html),
    ("xml", Wrapper::Xml),
    ("svg", Wrapper::Svg),
    ("json", Wrapper::Json),
    ("yml", Wrapper::Yaml),
    ("sql", Wrapper::Sql),
    ("sh", Wrapper::Hash),
    ("py", Wrapper::Hash),
    ("rs", Wrapper::Slashes),
    ("ini", Wrapper::Ini),
    ("srt", Wrapper::Srt),
    ("m3u", Wrapper::M3u),
    ("eml", Wrapper::Eml),
    // From `mime::EXTENSION_OVERRIDES`, not from `mime_guess`, which calls it
    // an executable.
    ("bat", Wrapper::Rem),
];

#[derive(Clone, Copy)]
enum Wrapper {
    Raw,
    Csv,
    Html,
    Xml,
    Svg,
    Json,
    Yaml,
    Sql,
    Hash,
    Slashes,
    Ini,
    Srt,
    M3u,
    Eml,
    Rem,
}

impl Wrapper {
    /// Render `sentences` in this file type's clothing. Every sentence must
    /// come out contiguous and unaltered — the fragments are asserted against
    /// the decoded text verbatim.
    fn render(self, sentences: &[String]) -> String {
        let lines = |prefix: &str| {
            sentences
                .iter()
                .map(|s| format!("{prefix}{s}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        match self {
            Wrapper::Raw => sentences.join("\n"),
            // One sentence per cell keeps it a single field; no sentence
            // contains a comma or a quote, so no quoting is needed.
            Wrapper::Csv => lines(""),
            Wrapper::Html => format!(
                "<!DOCTYPE html>\n<html><body>\n{}\n</body></html>",
                sentences
                    .iter()
                    .map(|s| format!("<p>{s}</p>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Wrapper::Xml => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<notes>\n{}\n</notes>",
                sentences
                    .iter()
                    .map(|s| format!("  <note>{s}</note>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Wrapper::Svg => format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"400\" height=\"200\">\n{}\n</svg>",
                sentences
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!("  <text x=\"10\" y=\"{}\">{s}</text>", 20 + i * 20))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            // Not `serde_json`: the sentences contain no character JSON would
            // escape, and an escape would break the verbatim fragment match.
            Wrapper::Json => format!(
                "{{\n  \"notes\": [\n{}\n  ]\n}}",
                sentences
                    .iter()
                    .map(|s| format!("    \"{s}\""))
                    .collect::<Vec<_>>()
                    .join(",\n")
            ),
            Wrapper::Yaml => format!("notes:\n{}", lines("  - ")),
            Wrapper::Sql => format!(
                "CREATE TABLE notes (body TEXT);\n{}",
                sentences
                    .iter()
                    .map(|s| format!("INSERT INTO notes VALUES ('{s}');"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Wrapper::Hash => format!("#!/bin/sh\n{}", lines("# ")),
            Wrapper::Slashes => format!("fn main() {{\n{}\n}}", lines("    // ")),
            Wrapper::Ini => format!("[notes]\n{}", lines("note = ")),
            Wrapper::Srt => sentences
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    format!(
                        "{}\n00:00:{:02},000 --> 00:00:{:02},000\n{s}\n",
                        i + 1,
                        i * 2,
                        i * 2 + 2
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Wrapper::M3u => format!("#EXTM3U\n{}", lines("#EXTINF:-1,")),
            Wrapper::Eml => format!(
                "From: corpus@example.invalid\nTo: reader@example.invalid\n\
                 Subject: lipsum\nContent-Type: text/plain; charset=utf-8\n\n{}",
                sentences.join("\n")
            ),
            Wrapper::Rem => format!("@echo off\n{}", lines("REM ")),
        }
    }
}

/// Every plaintext sample: the extension sweep, the encoding sweep, the
/// extensionless sniff case, and one file past `hash_length`.
pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    for (ext, wrapper) in EXTENSIONS {
        let b = body(lcg, Charset::Unicode);
        let text = wrapper.render(&b.sentences);
        let path = super::write_file(dir, &format!("prose-{ext}.{ext}"), text.as_bytes());
        out.push(Sample::prose(path, ext, &b, true));
    }

    // No extension at all: `mime_guess` has nothing, `infer` has nothing, and
    // only `textenc::looks_like_text` can answer. UTF-8 with no BOM is the
    // one class it accepts on proof rather than on evidence.
    let b = body(lcg, Charset::Unicode);
    let path = super::write_file(dir, "README", b.sentences.join("\n").as_bytes());
    out.push(Sample::prose(path, "extensionless", &b, true));

    write_encodings(dir, lcg, body, out);
    write_oversized(dir, lcg, body, out);
}

/// The same prose in five encodings. All five are `.txt`, so all five reach
/// the extractor identically and any difference is `textenc`'s alone.
fn write_encodings(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    // UTF-8 with a BOM: classified by `Encoding::for_bom` before the binary
    // guard ever runs.
    let b = body(lcg, Charset::Unicode);
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b.sentences.join("\n").as_bytes());
    let path = super::write_file(dir, "encoding-utf8-bom.txt", &bytes);
    out.push(Sample::prose(path, "utf-8 + BOM", &b, true));

    // UTF-16, both endiannesses, BOM-marked. Full of NUL bytes, which is
    // exactly why the BOM check has to precede the binary guard.
    for (label, name, big_endian) in [
        ("utf-16le + BOM", "encoding-utf16le.txt", false),
        ("utf-16be + BOM", "encoding-utf16be.txt", true),
    ] {
        let b = body(lcg, Charset::Unicode);
        let text = b.sentences.join("\n");
        let mut bytes = if big_endian {
            vec![0xFE, 0xFF]
        } else {
            vec![0xFF, 0xFE]
        };
        for unit in text.encode_utf16() {
            let pair = if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            };
            bytes.extend_from_slice(&pair);
        }
        let path = super::write_file(dir, name, &bytes);
        out.push(Sample::prose(path, label, &b, true));
    }

    // windows-1252: no BOM, not valid UTF-8, and decoded only because the
    // `.txt` extension already established it is text. `Charset::Latin1`
    // because the codepage cannot hold the Greek.
    let b = body(lcg, Charset::Latin1);
    let path = super::write_file(
        dir,
        "encoding-cp1252.txt",
        &to_cp1252(&b.sentences.join("\n")),
    );
    out.push(Sample::prose(path, "windows-1252", &b, true));
}

/// A file comfortably past the default `hash_length` of 8 KiB.
///
/// Under that size the walk hands the whole buffer to `extract_from_head` and
/// the file is never reopened; over it, the content pass runs the sized on-disk
/// read instead. Both paths must produce the planted text, and only this
/// sample proves the second one does.
fn write_oversized(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let mut text = String::new();
    // Padding first, so the planted sentences sit past the 8 KiB mark and a
    // reader that silently stopped at the head would find none of them.
    while text.len() < 12 * 1024 {
        text.push_str("padding filler ligula quis bibendum auctor nisi elit\n");
    }
    text.push_str(&b.sentences.join("\n"));
    let path = super::write_file(dir, "oversized.txt", text.as_bytes());
    // `head_path` stays true: `extract_from_head` is only ever called with a
    // *complete* buffer, so the agreement assertion passes it the whole file.
    out.push(Sample::prose(path, "oversized text", &b, true));
}

/// Not part of the corpus — used by the fixture assertions to read a committed
/// source file.
pub fn read_to_string(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
