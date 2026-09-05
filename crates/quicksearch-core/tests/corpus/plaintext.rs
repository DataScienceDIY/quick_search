//! Plain-text corpus files: one per extension family, plus one per
//! encoding. Written with `std` alone; the one encoder needed —
//! windows-1252 — is hand-rolled rather than `encoding_rs`, the decoder
//! under test.
//!
//! The extension sweep drives all three stages of `guess_mime_from_head`:
//! `.bat` from the override table, most from `mime_guess`, the
//! extensionless `README` from the text sniff; several resolve to
//! `EXTRA_TEXT_MIMES` entries the plaintext extractor claims only by
//! registration order.

use std::path::{Path, PathBuf};

use super::{BodyFn, Charset, Lcg, Sample};

/// Encode `s` as windows-1252 (Latin-1 plus 27 printables in the C1 range);
/// `encoding_rs` is the decoder under test. Shared with [`super::pdf`],
/// whose WinAnsi font is the same repertoire.
pub fn to_cp1252(s: &str) -> Vec<u8> {
    /// The 0x80-0x9F block; `\u{FFFD}` marks the five unassigned slots.
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

/// Extensions exercising distinct routes through `guess_mime_from_head`,
/// each with a wrapper making the file plausible — nothing is validated as
/// JSON or XML; it is for a human opening the scratch directory.
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
    // From `mime::EXTENSION_OVERRIDES`; `mime_guess` calls it an executable.
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
    /// Render `sentences` in this file type's clothing; every sentence must
    /// come out contiguous — fragments are asserted verbatim.
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
            // Not `serde_json`: an escape would break the verbatim match.
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

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    for (ext, wrapper) in EXTENSIONS {
        let b = body(lcg, Charset::Unicode);
        let text = wrapper.render(&b.sentences);
        let path = super::write_file(dir, &format!("prose-{ext}.{ext}"), text.as_bytes());
        out.push(Sample::prose(path, ext, &b, true));
    }

    // No extension: only `textenc::looks_like_text` can answer. UTF-8 with
    // no BOM is the one class it accepts on proof rather than evidence.
    let b = body(lcg, Charset::Unicode);
    let path = super::write_file(dir, "README", b.sentences.join("\n").as_bytes());
    out.push(Sample::prose(path, "extensionless", &b, true));

    write_encodings(dir, lcg, body, out);
    write_oversized(dir, lcg, body, out);
}

fn write_encodings(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    // UTF-8 with a BOM: `Encoding::for_bom` classifies it before the binary guard.
    let b = body(lcg, Charset::Unicode);
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b.sentences.join("\n").as_bytes());
    let path = super::write_file(dir, "encoding-utf8-bom.txt", &bytes);
    out.push(Sample::prose(path, "utf-8 + BOM", &b, true));

    // UTF-16, both endiannesses: full of NUL, exactly why the BOM check
    // precedes the binary guard.
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

    // windows-1252: no BOM, not valid UTF-8 — decoded only because `.txt`
    // established it is text. Latin1: the codepage cannot hold Greek.
    let b = body(lcg, Charset::Latin1);
    let path = super::write_file(
        dir,
        "encoding-cp1252.txt",
        &to_cp1252(&b.sentences.join("\n")),
    );
    out.push(Sample::prose(path, "windows-1252", &b, true));
}

/// Past the default `hash_length` of 8 KiB, so the content pass runs the
/// on-disk read — only this sample proves that path yields the planted text.
fn write_oversized(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let mut text = String::new();
    // Padding first: a reader that silently stopped at the head finds nothing.
    while text.len() < 12 * 1024 {
        text.push_str("padding filler ligula quis bibendum auctor nisi elit\n");
    }
    text.push_str(&b.sentences.join("\n"));
    let path = super::write_file(dir, "oversized.txt", text.as_bytes());
    // `head_path` stays true: `extract_from_head` only ever gets a *complete*
    // buffer, so the agreement assertion passes it the whole file.
    out.push(Sample::prose(path, "oversized text", &b, true));
}

pub fn read_to_string(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
