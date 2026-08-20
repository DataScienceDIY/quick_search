//! The three OOXML corpus files: `.docx`, `.xlsx`, `.pptx`.
//!
//! `docx-rs` and `rust_xlsxwriter` are complete, independent implementations
//! of their formats — including the container, since `docx-rs` carries `zip`
//! 8.x against the 0.6 `extract::office` reads with. There is no comparable
//! crate for PowerPoint, so the `.pptx` is assembled from [`super::zipwriter`]
//! and `format!`, which is independent of both `zip` 0.6 and `quick-xml`.

use std::path::Path;

use super::zipwriter::{self, Entry};
use super::{BodyFn, Charset, Lcg, Sample};

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    docx(dir, lcg, body, out);
    xlsx(dir, lcg, body, out);
    pptx(dir, lcg, body, out);
}

/// One paragraph per sentence, one run per paragraph — so each sentence lands
/// in a single `<w:t>` and reaches the reader contiguous.
fn docx(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    use docx_rs::*;

    let b = body(lcg, Charset::Unicode);
    let mut docx = Docx::new();
    for sentence in &b.sentences {
        docx = docx.add_paragraph(Paragraph::new().add_run(Run::new().add_text(sentence)));
    }
    let path = dir.join("prose.docx");
    let file = std::fs::File::create(&path).expect("create docx");
    docx.build().pack(file).expect("pack docx");
    out.push(Sample::prose(path, "docx", &b, false));
}

/// One sentence per cell, one cell per row.
///
/// `rust_xlsxwriter` puts strings in a real `xl/sharedStrings.xml` table and
/// has the cells index into it, which is the path `extract_xlsx` exists for —
/// an inline-string writer would leave the shared-string reader untested.
fn xlsx(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    use rust_xlsxwriter::Workbook;

    let b = body(lcg, Charset::Unicode);
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    for (row, sentence) in b.sentences.iter().enumerate() {
        sheet
            .write_string(row as u32, 0, sentence)
            .expect("write cell");
    }
    let path = dir.join("sheet.xlsx");
    workbook.save(&path).expect("save xlsx");
    out.push(Sample::prose(path, "xlsx", &b, false));
}

/// Sentences dealt across three slides, so `extract_pptx`'s per-slide loop and
/// its `--- New Slide ---` marker are both exercised rather than a single
/// slide's happy path.
fn pptx(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    const SLIDES: usize = 3;
    let b = body(lcg, Charset::Unicode);

    let mut slides: Vec<String> = Vec::with_capacity(SLIDES);
    for slide in 0..SLIDES {
        let paragraphs: String = b
            .sentences
            .iter()
            .skip(slide)
            .step_by(SLIDES)
            .map(|s| {
                format!(
                    "<a:p><a:r><a:t>{}</a:t></a:r></a:p>",
                    zipwriter::xml_escape(s)
                )
            })
            .collect();
        slides.push(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" \
                    xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
             <p:cSld><p:spTree><p:sp><p:txBody>{paragraphs}</p:txBody></p:sp></p:spTree></p:cSld>\
             </p:sld>"
        ));
    }

    let names: Vec<String> = (1..=SLIDES)
        .map(|i| format!("ppt/slides/slide{i}.xml"))
        .collect();
    let mut entries = vec![Entry {
        name: "[Content_Types].xml",
        body: CONTENT_TYPES.as_bytes(),
    }];
    for (name, xml) in names.iter().zip(&slides) {
        entries.push(Entry {
            name,
            body: xml.as_bytes(),
        });
    }

    let path = super::write_file(dir, "deck.pptx", &zipwriter::archive(&entries));

    // Dealing round-robin means slide order, not sentence order, decides what
    // the reader emits — so the expectation has to be rebuilt in the order the
    // slides are read, not copied from `b.sentences`.
    let mut expected = Vec::with_capacity(b.sentences.len());
    for slide in 0..SLIDES {
        expected.extend(b.sentences.iter().skip(slide).step_by(SLIDES).cloned());
    }
    out.push(Sample {
        path,
        label: "pptx",
        must_contain: expected,
        needle: b.needle.clone(),
        head_path: false,
    });
}

/// A `[Content_Types].xml` good enough to make the archive a real package.
/// The reader never opens it — it goes straight for `ppt/slides/slide*.xml` —
/// but a package without one is not a `.pptx`, and the corpus should not be
/// asserting against something no other tool would accept.
const CONTENT_TYPES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
     <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
     <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
     <Override PartName=\"/ppt/slides/slide1.xml\" \
       ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\
     <Override PartName=\"/ppt/slides/slide2.xml\" \
       ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\
     <Override PartName=\"/ppt/slides/slide3.xml\" \
       ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\
     </Types>";
