//! The PDF corpus file, written with `pdf-writer`.
//!
//! `pdf-writer` is typst's low-level writer. It shares no code with
//! `pdf-extract` — in particular no `lopdf`, which is what `extract::pdf`
//! parses with and what the unit tests in `src/extract/pdf.rs` build their
//! fixtures with.
//!
//! # Why the text is Latin-1
//!
//! Helvetica is one of the fourteen fonts every PDF reader ships, so no font
//! file has to be embedded and no glyph can be missing for the wrong reason.
//! Its repertoire under `WinAnsiEncoding` is cp1252 — which covers the
//! corpus's Latin-1 phrase and cannot express its Greek one. Hence
//! [`Charset::Latin1`]: demanding Greek here would be asserting against a
//! limit of the fixture rather than of the extractor.

use std::path::Path;

use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str};

use super::{BodyFn, Charset, Lcg, Sample};

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Latin1);

    let catalog_id = Ref::new(1);
    let page_tree_id = Ref::new(2);
    let page_id = Ref::new(3);
    let font_id = Ref::new(4);
    let content_id = Ref::new(5);
    let font_name = Name(b"F1");

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.pages(page_tree_id).kids([page_id]).count(1);

    let mut page = pdf.page(page_id);
    page.media_box(Rect::new(0.0, 0.0, 595.0, 842.0));
    page.parent(page_tree_id);
    page.contents(content_id);
    page.resources().fonts().pair(font_name, font_id);
    page.finish();

    // Without an explicit encoding the font falls back to StandardEncoding,
    // whose upper half is not Latin-1 at all — `é` would come back as an
    // acute accent on its own.
    pdf.type1_font(font_id)
        .base_font(Name(b"Helvetica"))
        .encoding_predefined(Name(b"WinAnsiEncoding"));

    let mut content = Content::new();
    content.begin_text();
    content.set_font(font_name, 12.0);
    // Leading set once, then one `next_line` per sentence: each sentence is a
    // single `Tj` so nothing can be interleaved into the middle of one.
    content.set_leading(16.0);
    content.next_line(56.0, 780.0);
    for (i, sentence) in b.sentences.iter().enumerate() {
        if i > 0 {
            content.next_line(0.0, 0.0);
        }
        content.show(Str(&super::plaintext::to_cp1252(sentence)));
    }
    content.end_text();
    pdf.stream(content_id, &content.finish());

    let path = super::write_file(dir, "prose.pdf", &pdf.finish());
    out.push(Sample::prose(path, "pdf", &b, false));
}
