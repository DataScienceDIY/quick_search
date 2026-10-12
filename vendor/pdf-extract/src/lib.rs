extern crate lopdf;

use encoding_rs::UTF_16BE;
use lopdf::content::{Content, Operation};
pub use lopdf::*;
use euclid::*;
use lopdf::encryption::DecryptionError;
use std::fmt::{Debug, Formatter};
extern crate encoding_rs;
extern crate euclid;
extern crate type1_encoding_parser;
extern crate unicode_normalization;
use euclid::vec2;
use unicode_normalization::UnicodeNormalization;
use std::fmt;
use std::str;
use std::fs::File;
use std::slice::Iter;
use std::collections::HashMap;
use std::rc::Rc;
use std::marker::PhantomData;
use std::result::Result;
use log::{warn, error, debug};
mod core_fonts;
mod glyphnames;
mod zapfglyphnames;
mod encodings;

pub struct Space;
pub type Transform = Transform2D<f64, Space, Space>;

#[derive(Debug)]
pub enum OutputError
{
    FormatError(std::fmt::Error),
    IoError(std::io::Error),
    PdfError(lopdf::Error),
    /// LOCAL PATCH (QuickSearch): the document spent [`MAX_PDF_WORK`] and
    /// rendering stopped. Whatever was output before the stop stands.
    Budget,
}

impl std::fmt::Display for OutputError
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            OutputError::FormatError(e) => write!(f, "Formating error: {}", e),
            OutputError::IoError(e) => write!(f, "IO error: {}", e),
            OutputError::PdfError(e) => write!(f, "PDF error: {}", e),
            OutputError::Budget => write!(f, "work budget of {} bytes spent", MAX_PDF_WORK),
        }
    }
}

impl std::error::Error for OutputError{}

impl From<std::fmt::Error> for OutputError {
    fn from(e: std::fmt::Error) -> Self {
        OutputError::FormatError(e)
    }
}

impl From<std::io::Error> for OutputError {
    fn from(e: std::io::Error) -> Self {
        OutputError::IoError(e)
    }
}

impl From<lopdf::Error> for OutputError {
    fn from(e: lopdf::Error) -> Self {
        OutputError::PdfError(e)
    }
}

macro_rules! dlog {
    ($($e:expr),*) => { {} }
    //($($t:tt)*) => { println!($($t)*) }
}

fn get_info(doc: &Document) -> Option<&Dictionary> {
    match doc.trailer.get(b"Info") {
        Ok(&Object::Reference(ref id)) => {
            match doc.get_object(*id) {
                Ok(&Object::Dictionary(ref info)) => { return Some(info); }
                _ => {}
            }
        }
        _ => {}
    }
    None
}

fn get_catalog(doc: &Document) -> &Dictionary {
    match doc.trailer.get(b"Root").unwrap() {
        &Object::Reference(ref id) => {
            match doc.get_object(*id) {
                Ok(&Object::Dictionary(ref catalog)) => { return catalog; }
                _ => {}
            }
        }
        _ => {}
    }
    panic!();
}

fn get_pages(doc: &Document) -> &Dictionary {
    let catalog = get_catalog(doc);
    match catalog.get(b"Pages").unwrap() {
        &Object::Reference(ref id) => {
            match doc.get_object(*id) {
                Ok(&Object::Dictionary(ref pages)) => { return pages; }
                other => {dlog!("pages: {:?}", other)}
            }
        }
        other => { dlog!("pages: {:?}", other)}
    }
    dlog!("catalog {:?}", catalog);
    panic!();
}

#[allow(non_upper_case_globals)]
const PDFDocEncoding: &'static [u16] = &[
    0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008,
    0x0009, 0x000a, 0x000b, 0x000c, 0x000d, 0x000e, 0x000f, 0x0010, 0x0011,
    0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0017, 0x02d8, 0x02c7, 0x02c6,
    0x02d9, 0x02dd, 0x02db, 0x02da, 0x02dc, 0x0020, 0x0021, 0x0022, 0x0023,
    0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002a, 0x002b, 0x002c,
    0x002d, 0x002e, 0x002f, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035,
    0x0036, 0x0037, 0x0038, 0x0039, 0x003a, 0x003b, 0x003c, 0x003d, 0x003e,
    0x003f, 0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047,
    0x0048, 0x0049, 0x004a, 0x004b, 0x004c, 0x004d, 0x004e, 0x004f, 0x0050,
    0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059,
    0x005a, 0x005b, 0x005c, 0x005d, 0x005e, 0x005f, 0x0060, 0x0061, 0x0062,
    0x0063, 0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x006a, 0x006b,
    0x006c, 0x006d, 0x006e, 0x006f, 0x0070, 0x0071, 0x0072, 0x0073, 0x0074,
    0x0075, 0x0076, 0x0077, 0x0078, 0x0079, 0x007a, 0x007b, 0x007c, 0x007d,
    0x007e, 0x0000, 0x2022, 0x2020, 0x2021, 0x2026, 0x2014, 0x2013, 0x0192,
    0x2044, 0x2039, 0x203a, 0x2212, 0x2030, 0x201e, 0x201c, 0x201d, 0x2018,
    0x2019, 0x201a, 0x2122, 0xfb01, 0xfb02, 0x0141, 0x0152, 0x0160, 0x0178,
    0x017d, 0x0131, 0x0142, 0x0153, 0x0161, 0x017e, 0x0000, 0x20ac, 0x00a1,
    0x00a2, 0x00a3, 0x00a4, 0x00a5, 0x00a6, 0x00a7, 0x00a8, 0x00a9, 0x00aa,
    0x00ab, 0x00ac, 0x0000, 0x00ae, 0x00af, 0x00b0, 0x00b1, 0x00b2, 0x00b3,
    0x00b4, 0x00b5, 0x00b6, 0x00b7, 0x00b8, 0x00b9, 0x00ba, 0x00bb, 0x00bc,
    0x00bd, 0x00be, 0x00bf, 0x00c0, 0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5,
    0x00c6, 0x00c7, 0x00c8, 0x00c9, 0x00ca, 0x00cb, 0x00cc, 0x00cd, 0x00ce,
    0x00cf, 0x00d0, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6, 0x00d7,
    0x00d8, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x00de, 0x00df, 0x00e0,
    0x00e1, 0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x00e7, 0x00e8, 0x00e9,
    0x00ea, 0x00eb, 0x00ec, 0x00ed, 0x00ee, 0x00ef, 0x00f0, 0x00f1, 0x00f2,
    0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x00f7, 0x00f8, 0x00f9, 0x00fa, 0x00fb,
    0x00fc, 0x00fd, 0x00fe, 0x00ff];

fn pdf_to_utf8(s: &[u8]) -> String {
    if s.len() > 2 && s[0] == 0xfe && s[1] == 0xff {
        return UTF_16BE.decode_without_bom_handling_and_without_replacement(&s[2..]).unwrap().to_string()
    } else {
        let r : Vec<u8> = s.iter().map(|x| *x).flat_map(|x| {
               let k = PDFDocEncoding[x as usize];
               vec![(k>>8) as u8, k as u8].into_iter()}).collect();
        return UTF_16BE.decode_without_bom_handling_and_without_replacement(&r).unwrap().to_string()
    }
}

fn to_utf8(encoding: &[u16], s: &[u8]) -> String {
    if s.len() > 2 && s[0] == 0xfe && s[1] == 0xff {
        return UTF_16BE.decode_without_bom_handling_and_without_replacement(&s[2..]).unwrap().to_string()
    } else {
        let r : Vec<u8> = s.iter().map(|x| *x).flat_map(|x| {
            let k = encoding[x as usize];
            vec![(k>>8) as u8, k as u8].into_iter()}).collect();
        return UTF_16BE.decode_without_bom_handling_and_without_replacement(&r).unwrap().to_string()
    }
}


fn maybe_deref<'a>(doc: &'a Document, o: &'a Object) -> &'a Object {
    match o {
        &Object::Reference(r) => doc.get_object(r).expect("missing object reference"),
        _ => o
    }
}

fn maybe_get_obj<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    dict.get(key).map(|o| maybe_deref(doc, o)).ok()
}

// an intermediate trait that can be used to chain conversions that may have failed
trait FromOptObj<'a> {
    fn from_opt_obj(doc: &'a Document, obj: Option<&'a Object>, key: &[u8]) -> Self;
}

// conditionally convert to Self returns None if the conversion failed
trait FromObj<'a> where Self: std::marker::Sized {
    fn from_obj(doc: &'a Document, obj: &'a Object) -> Option<Self>;
}

impl<'a, T: FromObj<'a>> FromOptObj<'a> for Option<T> {
    fn from_opt_obj(doc: &'a Document, obj: Option<&'a Object>, _key: &[u8]) -> Self {
        obj.and_then(|x| T::from_obj(doc,x))
    }
}

impl<'a, T: FromObj<'a>> FromOptObj<'a> for T {
    fn from_opt_obj(doc: &'a Document, obj: Option<&'a Object>, key: &[u8]) -> Self {
        T::from_obj(doc, obj.expect(&String::from_utf8_lossy(key))).expect("wrong type")
    }
}

// we follow the same conventions as pdfium for when to support indirect objects:
// on arrays, streams and dicts
impl<'a, T: FromObj<'a>> FromObj<'a> for Vec<T> {
    fn from_obj(doc: &'a Document, obj: &'a Object) -> Option<Self> {
        maybe_deref(doc, obj).as_array().map(|x| x.iter()
            .map(|x| T::from_obj(doc, x).expect("wrong type"))
            .collect()).ok()
    }
}

impl<'a> FromObj<'a> for f64 {
    fn from_obj(_doc: &Document, obj: &Object) -> Option<Self> {
        match obj {
            &Object::Integer(i) => Some(i as f64),
            &Object::Real(f) => Some(f.into()),
            _ => None
        }
    }
}

impl<'a> FromObj<'a> for i64 {
    fn from_obj(_doc: &Document, obj: &Object) -> Option<Self> {
        match obj {
            &Object::Integer(i) => Some(i),
            _ => None
        }
    }
}

impl<'a> FromObj<'a> for &'a Dictionary {
    fn from_obj(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
        maybe_deref(doc, obj).as_dict().ok()
    }
}

impl<'a> FromObj<'a> for &'a Stream {
    fn from_obj(doc: &'a Document, obj: &'a Object) -> Option<&'a Stream> {
        maybe_deref(doc, obj).as_stream().ok()
    }
}

impl<'a> FromObj<'a> for &'a Object {
    fn from_obj(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
        Some(maybe_deref(doc, obj))
    }
}

fn get<'a, T: FromOptObj<'a>>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> T {
    T::from_opt_obj(doc, dict.get(key).ok(), key)
}

fn maybe_get<'a, T: FromObj<'a>>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<T> {
    maybe_get_obj(doc, dict, key).and_then(|o| T::from_obj(doc, o))
}

fn get_name_string<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> String {
    pdf_to_utf8(dict.get(key).map(|o| maybe_deref(doc, o)).unwrap_or_else(|_| panic!("deref")).as_name().expect("name"))
}

#[allow(dead_code)]
fn maybe_get_name_string<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<String> {
    maybe_get_obj(doc, dict, key).and_then(|n| n.as_name().ok()).map(|n| pdf_to_utf8(n))
}

fn maybe_get_name<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    maybe_get_obj(doc, dict, key).and_then(|n| n.as_name().ok())
}

fn maybe_get_array<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Vec<Object>> {
    maybe_get_obj(doc, dict, key).and_then(|n| n.as_array().ok())
}

#[derive(Clone)]
struct PdfSimpleFont<'a> {
    font: &'a Dictionary,
    doc: &'a Document,
    encoding: Option<Vec<u16>>,
    unicode_map: Option<UnicodeMap>,
    widths: HashMap<CharCode, f64>, // should probably just use i32 here
    missing_width: f64,
}

/// LOCAL PATCH (QuickSearch): a simple font's unicode map in layers, looked
/// up in this order: its ToUnicode CMap, its CFF font file's glyph names,
/// then what its `Differences` add for codes neither maps. The first two are
/// shared with every font naming the same stream (see [`FontParts`]).
/// Upstream merged the three into one map per font, so each font sharing a
/// large CMap copied it; the order keeps the merge's precedence.
#[derive(Clone, Debug)]
struct UnicodeMap {
    to_unicode: Option<Rc<HashMap<u32, String>>>,
    cff: Option<Rc<HashMap<u32, String>>>,
    differences: HashMap<u32, String>,
}

impl UnicodeMap {
    fn get(&self, code: &u32) -> Option<&String> {
        self.to_unicode.as_ref().and_then(|map| map.get(code))
            .or_else(|| self.cff.as_ref().and_then(|map| map.get(code)))
            .or_else(|| self.differences.get(code))
    }
}

#[derive(Clone)]
struct PdfType3Font<'a> {
    font: &'a Dictionary,
    doc: &'a Document,
    encoding: Option<Vec<u16>>,
    unicode_map: Option<Rc<HashMap<CharCode, String>>>,
    widths: HashMap<CharCode, f64>, // should probably just use i32 here
}


fn make_font<'a>(doc: &'a Document, font: &'a Dictionary, parts: &mut FontParts) -> Rc<dyn PdfFont + 'a> {
    let subtype = get_name_string(doc, font, b"Subtype");
    dlog!("MakeFont({})", subtype);
    if subtype == "Type0" {
        Rc::new(PdfCIDFont::new(doc, font, parts))
    } else if subtype == "Type3" {
        Rc::new(PdfType3Font::new(doc, font, parts))
    } else {
        Rc::new(PdfSimpleFont::new(doc, font, parts))
    }
}

fn is_core_font(name: &str) -> bool {
    match name {
        "Courier-Bold" |
        "Courier-BoldOblique" |
        "Courier-Oblique" |
        "Courier" |
        "Helvetica-Bold" |
        "Helvetica-BoldOblique" |
        "Helvetica-Oblique" |
        "Helvetica" |
        "Symbol" |
        "Times-Bold" |
        "Times-BoldItalic" |
        "Times-Italic" |
        "Times-Roman" |
        "ZapfDingbats" => true,
        _ => false,
    }
}

fn encoding_to_unicode_table(name: &[u8]) -> Vec<u16> {
    let encoding = match &name[..] {
        b"MacRomanEncoding" => encodings::MAC_ROMAN_ENCODING,
        b"MacExpertEncoding" => encodings::MAC_EXPERT_ENCODING,
        b"WinAnsiEncoding" => encodings::WIN_ANSI_ENCODING,
        _ => panic!("unexpected encoding {:?}", pdf_to_utf8(name))
    };
    let encoding_table = encoding.iter()
        .map(|x| if let &Some(x) = x { glyphnames::name_to_unicode(x).unwrap() } else { 0 })
        .collect();
    encoding_table
}

/* "Glyphs in the font are selected by single-byte character codes obtained from a string that
    is shown by the text-showing operators. Logically, these codes index into a table of 256
    glyphs; the mapping from codes to glyphs is called the font’s encoding. Each font program
    has a built-in encoding. Under some circumstances, the encoding can be altered by means
    described in Section 5.5.5, “Character Encoding.”
*/
impl<'a> PdfSimpleFont<'a> {
    fn new(doc: &'a Document, font: &'a Dictionary, parts: &mut FontParts) -> PdfSimpleFont<'a> {
        let base_name = get_name_string(doc, font, b"BaseFont");
        let subtype = get_name_string(doc, font, b"Subtype");

        let encoding: Option<&Object> = get(doc, font, b"Encoding");
        dlog!("base_name {} {} enc:{:?} {:?}", base_name, subtype, encoding, font);
        let descriptor: Option<&Dictionary> = get(doc, font, b"FontDescriptor");
        let mut type1_file = None;
        let mut cff = None;
        if let Some(descriptor) = descriptor {
            dlog!("descriptor {:?}", descriptor);
            if subtype == "Type1" {
                let file = maybe_get_obj(doc, descriptor, b"FontFile");
                match file {
                    // LOCAL PATCH (QuickSearch): read below, only if the font
                    // has no `/Encoding` of its own; it was parsed here
                    // whether or not anything used it.
                    Some(&Object::Stream(ref s)) => { type1_file = Some(s); }
                    _ => { dlog!("font file {:?}", file) }
                }
            }
            // LOCAL PATCH (QuickSearch): a TrueType `FontFile2` was inflated
            // here and dropped unread.

            let font_file3 = get::<Option<&Object>>(doc, descriptor, b"FontFile3");
            match font_file3 {
                Some(&Object::Stream(ref s)) => {
                    let subtype = get_name_string(doc, &s.dict, b"Subtype");
                    dlog!("font file {}, {:?}", subtype, s);
                    // LOCAL PATCH (QuickSearch): parsed once per font file
                    // (see FontParts); a file of another subtype was inflated
                    // here and dropped unread.
                    if subtype == "Type1C" {
                        cff = Some(parse_once(&mut parts.cff, &mut parts.parsed, s, |s| {
                            let table = cff_parser::Table::parse(s).unwrap();
                            let encoding = table.encoding.get_code_to_sid_table(&table.charset);

                            let mapping: HashMap<u32, String> = encoding.into_iter().filter_map(|(cid, sid)| {
                                let name = cff_parser::string_by_id(&table, sid).unwrap();
                                if name == ".notdef" {
                                    return None;
                                }
                                let unicode = glyphnames::name_to_unicode(&name).or_else(|| {
                                    zapfglyphnames::zapfdigbats_names_to_unicode(name)
                                });
                                if unicode.is_none() {
                                    warn!("Couldn't find unicode for {}", name);
                                    return None;
                                }
                                let str = String::from_utf16(&[unicode.unwrap()]).unwrap();
                                Some((cid as u32, str))
                            }).collect();
                            Rc::new(mapping)
                        }));
                    }
                }
                None => {}
                _ => { dlog!("unexpected") }
            }

            let charset = maybe_get_obj(doc, descriptor, b"CharSet");
            let _charset = match charset {
                Some(&Object::String(ref s, _)) => { Some(pdf_to_utf8(&s)) }
                _ => { None }
            };
            //dlog!("charset {:?}", charset);
        }

        // LOCAL PATCH (QuickSearch): layered, not merged; see UnicodeMap.
        let to_unicode = get_unicode_map(doc, font, parts);
        let mut unicode_map = if to_unicode.is_some() || cff.is_some() {
            Some(UnicodeMap { to_unicode, cff, differences: HashMap::new() })
        } else {
            None
        };


        let mut encoding_table = None;
        match encoding {
            Some(&Object::Name(ref encoding_name)) => {
                dlog!("encoding {:?}", pdf_to_utf8(encoding_name));
                encoding_table = Some(encoding_to_unicode_table(encoding_name));
            }
            Some(&Object::Dictionary(ref encoding)) => {
                //dlog!("Encoding {:?}", encoding);
                let mut table = if let Some(base_encoding) = maybe_get_name(doc, encoding, b"BaseEncoding") {
                    dlog!("BaseEncoding {:?}", base_encoding);
                    encoding_to_unicode_table(base_encoding)
                } else {
                    Vec::from(PDFDocEncoding)
                };
                let differences = maybe_get_array(doc, encoding, b"Differences");
                if let Some(differences) = differences {
                    dlog!("Differences");
                    let mut code = 0;
                    for o in differences {
                        let o = maybe_deref(doc, o);
                        match o {
                            &Object::Integer(i) => { code = i; },
                            &Object::Name(ref n) => {
                                let name = pdf_to_utf8(&n);
                                // XXX: names of Type1 fonts can map to arbitrary strings instead of real
                                // unicode names, so we should probably handle this differently
                                let unicode = glyphnames::name_to_unicode(&name);
                                if let Some(unicode) = unicode{
                                    table[code as usize] = unicode;
                                    if let Some(ref mut unicode_map) = unicode_map {
                                        let be = [unicode];
                                        match unicode_map.get(&(code as u32)) {
                                            // If there's a unicode table entry missing use one based on the name
                                            None => { unicode_map.differences.insert(code as u32, String::from_utf16(&be).unwrap()); }
                                            Some(e) => {
                                                if e != &String::from_utf16(&be).unwrap() {
                                                    let normal_match  = e.nfkc().eq(String::from_utf16(&be).unwrap().nfkc());
                                                    if !normal_match {
                                                        warn!("Unicode mismatch {} {} {:?} {:?} {:?}", normal_match, name, e, String::from_utf16(&be), be);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    match unicode_map {
                                        Some(ref mut unicode_map) if base_name.contains("FontAwesome") => {
                                            // the fontawesome tex package will use glyph names that don't have a corresponding unicode
                                            // code point, so we'll use an empty string instead. See issue #76
                                            match unicode_map.get(&(code as u32)) {
                                                None => { unicode_map.differences.insert(code as u32, "".to_owned()); }
                                                Some(_) => {
                                                    panic!("unexpected entry in unicode map")
                                                }
                                            }
                                        }
                                        _ => {
                                            warn!("unknown glyph name '{}' for font {}", name, base_name);
                                        }
                                    }
                                }
                                dlog!("{} = {} ({:?})", code, name, unicode);
                                if let Some(ref mut unicode_map) = unicode_map {
                                    // The unicode map might not have the code in it, but the code might
                                    // not be used so we don't want to panic here.
                                    // An example of this is the 'suppress' character in the TeX Latin Modern font.
                                    // This shows up in https://arxiv.org/pdf/2405.01295v1.pdf
                                    dlog!("{} {:?}", code, unicode_map.get(&(code as u32)));
                                }
                                code += 1;
                            }
                            _ => { panic!("wrong type {:?}", o); }
                        }
                    }
                }
                // "Type" is optional
                let name = encoding.get(b"Type").and_then(|x| x.as_name()).and_then(|x| Ok(pdf_to_utf8(x)));
                dlog!("name: {}", name);

                encoding_table = Some(table);
            }
            None => {
                if let Some(type1_file) = type1_file {
                    // LOCAL PATCH (QuickSearch): the table, not the parse, is
                    // what is kept per font file (see FontParts), so a font
                    // sharing the file costs no pass over its glyph names.
                    encoding_table = Some(parse_once(&mut parts.type1, &mut parts.parsed, type1_file, |s| {
                        let type1_encoding = type1_encoding_parser::get_encoding_map(s).expect("encoding");
                        let mut table = Vec::from(PDFDocEncoding);
                        dlog!("type1encoding");
                        for (code, name) in type1_encoding {
                            let unicode = glyphnames::name_to_unicode(&pdf_to_utf8(&name));
                            if let Some(unicode) = unicode {
                                table[code as usize] = unicode;
                            } else {
                                dlog!("unknown character {}", pdf_to_utf8(&name));
                            }
                        }
                        table
                    }))
                } else if subtype == "TrueType" {
                    encoding_table = Some(encodings::WIN_ANSI_ENCODING.iter()
                        .map(|x| if let &Some(x) = x { glyphnames::name_to_unicode(x).unwrap() } else { 0 })
                        .collect());
                }
            }
            _ => { panic!() }
        }

        let mut width_map = HashMap::new();
        /* "Ordinarily, a font dictionary that refers to one of the standard fonts
            should omit the FirstChar, LastChar, Widths, and FontDescriptor entries.
            However, it is permissible to override a standard font by including these
            entries and embedding the font program in the PDF file."

            Note: some PDFs include a descriptor but still don't include these entries */

        // If we have widths prefer them over the core font widths. Needed for https://dkp.de/wp-content/uploads/parteitage/Sozialismusvorstellungen-der-DKP.pdf
        if let (Some(first_char), Some(last_char), Some(widths)) = (maybe_get::<i64>(doc, font, b"FirstChar"), maybe_get::<i64>(doc, font, b"LastChar"), maybe_get::<Vec<f64>>(doc, font, b"Widths")) {
            // Some PDF's don't have these like fips-197.pdf
            let mut i: i64 = 0;
            dlog!("first_char {:?}, last_char: {:?}, widths: {} {:?}", first_char, last_char, widths.len(), widths);

            for w in widths {
                width_map.insert((first_char + i) as CharCode, w);
                i += 1;
            }
            assert_eq!(first_char + i - 1, last_char);
        } else {
            let name = if is_core_font(&base_name) {
                &base_name
            } else {
                warn!("no widths and not core font {:?}", base_name);

                // This situation is handled differently by different readers
                // but basically we try to substitute the best font that we can.

                // Poppler/Xpdf:
                // this is technically an error -- the Widths entry is required
                // for all but the Base-14 fonts -- but certain PDF generators
                // apparently don't include widths for Arial and TimesNewRoman

                // Pdfium: CFX_FontMapper::FindSubstFont

                // mupdf: pdf_load_substitute_font

                // We can try to do a better job guessing at a font by looking at the flags
                // or the basename but for now we'll just use Helvetica
                "Helvetica"
            };
            for font_metrics in core_fonts::metrics().iter() {
                if font_metrics.0 == base_name {
                    if let Some(ref encoding) = encoding_table {
                        dlog!("has encoding");
                        for w in font_metrics.2 {
                            let c = glyphnames::name_to_unicode(w.2).unwrap();
                            for i in 0..encoding.len() {
                                if encoding[i] == c {
                                    width_map.insert(i as CharCode, w.1 as f64);
                                }
                            }
                        }
                    } else {
                        // Instead of using the encoding from the core font we'll just look up all
                        // of the character names. We should probably verify that this produces the
                        // same result.

                        let mut table = vec![0; 256];
                        for w in font_metrics.2 {
                            dlog!("{} {}", w.0, w.2);
                            // -1 is "not encoded"
                            if w.0 != -1 {
                                table[w.0 as usize] = if base_name == "ZapfDingbats" {
                                    zapfglyphnames::zapfdigbats_names_to_unicode(w.2).unwrap_or_else(|| panic!("bad name {:?}", w))
                                } else {
                                    glyphnames::name_to_unicode(w.2).unwrap()
                                }
                            }
                        }

                        let encoding = &table[..];
                        for w in font_metrics.2 {
                            width_map.insert(w.0 as CharCode, w.1 as f64);
                            // -1 is "not encoded"
                        }
                        encoding_table = Some(encoding.to_vec());
                    }
                    /* "Ordinarily, a font dictionary that refers to one of the standard fonts
                        should omit the FirstChar, LastChar, Widths, and FontDescriptor entries.
                        However, it is permissible to override a standard font by including these
                        entries and embedding the font program in the PDF file."

                        Note: some PDFs include a descriptor but still don't include these entries */
                    // assert!(maybe_get_obj(doc, font, b"FirstChar").is_none());
                    // assert!(maybe_get_obj(doc, font, b"LastChar").is_none());
                    // assert!(maybe_get_obj(doc, font, b"Widths").is_none());
                }
            }
        }

        let missing_width = get::<Option<f64>>(doc, font, b"MissingWidth").unwrap_or(0.);
        PdfSimpleFont {doc, font, widths: width_map, encoding: encoding_table, missing_width, unicode_map}
    }

    #[allow(dead_code)]
    fn get_type(&self) -> String {
        get_name_string(self.doc, self.font, b"Type")
    }
    #[allow(dead_code)]
    fn get_basefont(&self) -> String {
        get_name_string(self.doc, self.font, b"BaseFont")
    }
    #[allow(dead_code)]
    fn get_subtype(&self) -> String {
        get_name_string(self.doc, self.font, b"Subtype")
    }
    #[allow(dead_code)]
    fn get_widths(&self) -> Option<&Vec<Object>> {
        maybe_get_obj(self.doc, self.font, b"Widths").map(|widths| widths.as_array().expect("Widths should be an array"))
    }
    /* For type1: This entry is obsolescent and its use is no longer recommended. (See
     * implementation note 42 in Appendix H.) */
    #[allow(dead_code)]
    fn get_name(&self) -> Option<String> {
        maybe_get_name_string(self.doc, self.font, b"Name")
    }

    #[allow(dead_code)]
    fn get_descriptor(&self) -> Option<PdfFontDescriptor> {
        maybe_get_obj(self.doc, self.font, b"FontDescriptor").and_then(|desc| desc.as_dict().ok()).map(|desc| PdfFontDescriptor{desc: desc, doc: self.doc})
    }
}



impl<'a> PdfType3Font<'a> {
    fn new(doc: &'a Document, font: &'a Dictionary, parts: &mut FontParts) -> PdfType3Font<'a> {

        let unicode_map = get_unicode_map(doc, font, parts);
        let encoding: Option<&Object> = get(doc, font, b"Encoding");

        let encoding_table;
        match encoding {
            Some(&Object::Name(ref encoding_name)) => {
                dlog!("encoding {:?}", pdf_to_utf8(encoding_name));
                encoding_table = Some(encoding_to_unicode_table(encoding_name));
            }
            Some(&Object::Dictionary(ref encoding)) => {
                //dlog!("Encoding {:?}", encoding);
                let mut table = if let Some(base_encoding) = maybe_get_name(doc, encoding, b"BaseEncoding") {
                    dlog!("BaseEncoding {:?}", base_encoding);
                    encoding_to_unicode_table(base_encoding)
                } else {
                    Vec::from(PDFDocEncoding)
                };
                let differences = maybe_get_array(doc, encoding, b"Differences");
                if let Some(differences) = differences {
                    dlog!("Differences");
                    let mut code = 0;
                    for o in differences {
                        match o {
                            &Object::Integer(i) => { code = i; },
                            &Object::Name(ref n) => {
                                let name = pdf_to_utf8(&n);
                                // XXX: names of Type1 fonts can map to arbitrary strings instead of real
                                // unicode names, so we should probably handle this differently
                                let unicode = glyphnames::name_to_unicode(&name);
                                if let Some(unicode) = unicode{
                                    table[code as usize] = unicode;
                                }
                                dlog!("{} = {} ({:?})", code, name, unicode);
                                if let Some(ref unicode_map) = unicode_map {
                                    dlog!("{} {:?}", code, unicode_map.get(&(code as u32)));
                                }
                                code += 1;
                            }
                            _ => { panic!("wrong type"); }
                        }
                    }
                }
                let name_encoded = encoding.get(b"Type");
                if let Ok(Object::Name(name)) = name_encoded {
                    dlog!("name: {}", pdf_to_utf8(name));
                } else {
                    dlog!("name not found");
                }

                encoding_table = Some(table);
            }
            _ => { panic!() }
        }

        let first_char: i64 = get(doc, font, b"FirstChar");
        let last_char: i64 = get(doc, font, b"LastChar");
        let widths: Vec<f64> = get(doc, font, b"Widths");

        let mut width_map = HashMap::new();

        let mut i = 0;
        dlog!("first_char {:?}, last_char: {:?}, widths: {} {:?}", first_char, last_char, widths.len(), widths);

        for w in widths {
            width_map.insert((first_char + i) as CharCode, w);
            i += 1;
        }
        assert_eq!(first_char + i - 1, last_char);
        PdfType3Font {doc, font, widths: width_map, encoding: encoding_table, unicode_map}
    }
}

type CharCode = u32;

struct PdfFontIter<'a>
{
    i: Iter<'a, u8>,
    font: &'a dyn PdfFont,
}

impl<'a> Iterator for PdfFontIter<'a> {
    type Item = (CharCode, u8);
    fn next(&mut self) -> Option<(CharCode, u8)> {
        self.font.next_char(&mut self.i)
    }
}

trait PdfFont : Debug {
    fn get_width(&self, id: CharCode) -> f64;
    fn next_char(&self, iter: &mut Iter<u8>) -> Option<(CharCode, u8)>;
    fn decode_char(&self, char: CharCode) -> String;

        /*fn char_codes<'a>(&'a self, chars: &'a [u8]) -> PdfFontIter {
            let p = self;
            PdfFontIter{i: chars.iter(), font: p as &PdfFont}
        }*/

}

impl<'a> dyn PdfFont + 'a {
    fn char_codes(&'a self, chars: &'a [u8]) -> PdfFontIter {
        PdfFontIter{i: chars.iter(), font: self}
    }
    fn decode(&self, chars: &[u8]) -> String {
        let strings = self.char_codes(chars).map(|x| self.decode_char(x.0)).collect::<Vec<_>>();
        strings.join("")
    }

}


impl<'a> PdfFont for PdfSimpleFont<'a> {
    fn get_width(&self, id: CharCode) -> f64 {
        // LOCAL PATCH (QuickSearch): a miss collected and sorted every width
        // to feed a `dlog!` that expands to nothing.
        self.widths.get(&id).copied().unwrap_or(self.missing_width)
    }
    /*fn decode(&self, chars: &[u8]) -> String {
        let encoding = self.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
        to_utf8(encoding, chars)
    }*/

    fn next_char(&self, iter: &mut Iter<u8>) -> Option<(CharCode, u8)> {
        iter.next().map(|x| (*x as CharCode, 1))
    }
    fn decode_char(&self, char: CharCode) -> String {
        let slice = [char as u8];
        if let Some(ref unicode_map) = self.unicode_map {
            let s = unicode_map.get(&char);
            let s = match s {
                None => {
                    debug!("missing char {:?} in unicode map {:?} for {:?}", char, unicode_map, self.font);
                    // some pdf's like http://arxiv.org/pdf/2312.00064v1 are missing entries in their unicode map but do have
                    // entries in the encoding.
                    let encoding = self.encoding.as_ref().map(|x| &x[..]).expect("missing unicode map and encoding");
                    let s = to_utf8(encoding, &slice);
                    debug!("falling back to encoding {} -> {:?}", char, s);
                    s
                }
                Some(s) => { s.clone() }
            };
            return s
        }
        let encoding = self.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
        //dlog!("char_code {:?} {:?}", char, self.encoding);
        let s = to_utf8(encoding, &slice);
        s
    }
}



impl<'a> fmt::Debug for PdfSimpleFont<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.font.fmt(f)
    }
}

impl<'a> PdfFont for PdfType3Font<'a> {
    fn get_width(&self, id: CharCode) -> f64 {
        let width = self.widths.get(&id);
        if let Some(width) = width {
            return *width;
        } else {
            panic!("missing width for {} {:?}", id, self.font);
        }
    }
    /*fn decode(&self, chars: &[u8]) -> String {
        let encoding = self.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
        to_utf8(encoding, chars)
    }*/

    fn next_char(&self, iter: &mut Iter<u8>) -> Option<(CharCode, u8)> {
        iter.next().map(|x| (*x as CharCode, 1))
    }
    fn decode_char(&self, char: CharCode) -> String {
        let slice = [char as u8];
        if let Some(ref unicode_map) = self.unicode_map {
            let s = unicode_map.get(&char);
            let s = match s {
                None => {
                    debug!("missing char {:?} in unicode map {:?} for {:?}", char, unicode_map, self.font);
                    // some pdf's like http://arxiv.org/pdf/2312.00577v1 are missing entries in their unicode map but do have
                    // entries in the encoding.
                    let encoding = self.encoding.as_ref().map(|x| &x[..]).expect("missing unicode map and encoding");
                    let s = to_utf8(encoding, &slice);
                    debug!("falling back to encoding {} -> {:?}", char, s);
                    s
                }
                Some(s) => { s.clone() }
            };
            return s
        }
        let encoding = self.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
        //dlog!("char_code {:?} {:?}", char, self.encoding);
        let s = to_utf8(encoding, &slice);
        s
    }
}



impl<'a> fmt::Debug for PdfType3Font<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.font.fmt(f)
    }
}

struct PdfCIDFont<'a> {
    font: &'a Dictionary,
    #[allow(dead_code)]
    doc: &'a Document,
    #[allow(dead_code)]
    encoding: Rc<ByteMapping>,
    to_unicode: Option<Rc<HashMap<u32, String>>>,
    widths: HashMap<CharCode, f64>, // should probably just use i32 here
    default_width: Option<f64>, // only used for CID fonts and we should probably brake out the different font types
}

/// LOCAL PATCH (QuickSearch): the code-to-CID mapping of a Type0 font's
/// `/Encoding`, which adobe-cmap-parser used to supply. `width` is in bytes;
/// that parser stored half of it, so an embedded two-byte CMap was read one
/// byte at a time.
struct ByteMapping {
    /// Scanned for every character shown, up to four times (once per code
    /// width), so at most [`MAX_CODESPACE_RANGES`].
    codespace: Vec<CodeRange>,
    /// Sorted by `src_code_lo`, so a code's range is found by binary search.
    cid: Vec<CIDRange>,
}

struct CodeRange {
    width: u32,
    start: u32,
    end: u32,
}

struct CIDRange {
    src_code_lo: u32,
    src_code_hi: u32,
    dst_cid_lo: u32,
    /// LOCAL PATCH (QuickSearch): the highest `src_code_hi` of this range and
    /// of every range sorted before it (see PdfCIDFont::cid).
    reach: u32,
}

/// LOCAL PATCH (QuickSearch): the widest `beginbfrange` range read, in codes;
/// a wider one is skipped. A full two-byte range is exactly this wide.
const MAX_CMAP_RANGE: u32 = 1 << 16;

/// LOCAL PATCH (QuickSearch): the most entries one CMap defines, or CID
/// ranges an `/Encoding` defines. Reading stops there and keeps what it has.
const MAX_CMAP_ENTRIES: usize = 1 << 17;

/// LOCAL PATCH (QuickSearch): the most codespace ranges an `/Encoding`
/// defines; later ones are dropped. The spec allows 100 per
/// `begincodespacerange` block, and real CMaps define a handful.
const MAX_CODESPACE_RANGES: usize = 256;

/// LOCAL PATCH (QuickSearch): the longest ToUnicode destination string read,
/// in bytes, the PDF spec's limit; an entry mapping to a longer one is
/// skipped.
const MAX_CMAP_DST: usize = 512;

/// LOCAL PATCH (QuickSearch): the most text one ToUnicode CMap maps, in
/// destination bytes summed over its entries. A range copies its destination
/// for every code it spans, so the caps on entries and on a destination's
/// length still admit 64 MiB. Reading stops there and keeps what it has, as
/// at [`MAX_CMAP_ENTRIES`].
const MAX_CMAP_TEXT: usize = 16 << 20;

/// LOCAL PATCH (QuickSearch): a CMap's operations, tokenized by lopdf's
/// content parser, which replaces adobe-cmap-parser: that one panicked on a
/// CMap it could not parse and expanded `beginbfrange` code by code, so a
/// 4-byte full range was 2^32 inserts. Each `end...` operator receives the
/// entries before it as operands. `None` if the tokenizer rejects the
/// stream; one it reads only partway yields what it read.
fn cmap_operations(content: &[u8]) -> Option<Vec<Operation>> {
    Content::decode(content).ok().map(|c| c.operations)
}

/// A CMap code: the string's bytes, big-endian.
fn cmap_code(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |code, &b| (code << 8) | b as u32)
}

/// A CMap destination string as UTF-16 code units. An odd last byte is a
/// unit of its own. `None` past [`MAX_CMAP_DST`] bytes.
fn cmap_utf16(bytes: &[u8]) -> Option<Vec<u16>> {
    if bytes.len() > MAX_CMAP_DST {
        return None;
    }
    Some(bytes.chunks(2).map(|unit| cmap_code(unit) as u16).collect())
}

/// Map `code` to the UTF-16 text `units`, spending one of the `left` entries
/// and the text's bytes from `text_left`; false once either is spent.
/// Invalid UTF-16, such as a lone surrogate, maps nothing.
fn put_unicode(map: &mut HashMap<u32, String>, left: &mut usize, text_left: &mut usize, code: u32, units: &[u16]) -> bool {
    let bytes = 2 * units.len();
    if *left == 0 || *text_left < bytes {
        return false;
    }
    *left -= 1;
    *text_left -= bytes;
    if let Ok(s) = String::from_utf16(units) {
        map.insert(code, s);
    }
    true
}

// "It must use the beginbfchar, endbfchar, beginbfrange, and endbfrange operators to
// define the mapping from character codes to Unicode character sequences expressed in
// UTF-16BE encoding."
fn to_unicode_map(content: &[u8]) -> Option<HashMap<u32, String>> {
    let mut map = HashMap::new();
    let mut left = MAX_CMAP_ENTRIES;
    let mut text_left = MAX_CMAP_TEXT;
    'cmap: for op in cmap_operations(content)? {
        match op.operator.as_str() {
            "endbfchar" => {
                for entry in op.operands.chunks_exact(2) {
                    if let (Ok(code), Ok(Some(dst))) = (entry[0].as_str(), entry[1].as_str().map(cmap_utf16)) {
                        if !put_unicode(&mut map, &mut left, &mut text_left, cmap_code(code), &dst) {
                            break 'cmap;
                        }
                    }
                }
            }
            "endbfrange" => {
                for entry in op.operands.chunks_exact(3) {
                    let (Ok(lo), Ok(hi)) = (entry[0].as_str(), entry[1].as_str()) else { continue };
                    let (lo, hi) = (cmap_code(lo), cmap_code(hi));
                    if hi < lo || hi - lo >= MAX_CMAP_RANGE {
                        continue;
                    }
                    match &entry[2] {
                        // Each code after the first advances the last unit by one.
                        Object::String(dst, _) => {
                            let Some(mut units) = cmap_utf16(dst) else { continue };
                            for code in lo..=hi {
                                if !put_unicode(&mut map, &mut left, &mut text_left, code, &units) {
                                    break 'cmap;
                                }
                                if let Some(last) = units.last_mut() {
                                    *last = last.wrapping_add(1);
                                }
                            }
                        }
                        Object::Array(dsts) => {
                            for (code, dst) in (lo..=hi).zip(dsts) {
                                if let Ok(Some(dst)) = dst.as_str().map(cmap_utf16) {
                                    if !put_unicode(&mut map, &mut left, &mut text_left, code, &dst) {
                                        break 'cmap;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(map)
}

/// A Type0 font's `/Encoding` CMap. One the tokenizer rejects maps no codes,
/// so the font's strings decode to nothing.
fn byte_mapping(content: &[u8]) -> ByteMapping {
    let mut mapping = ByteMapping { codespace: Vec::new(), cid: Vec::new() };
    for op in cmap_operations(content).unwrap_or_default() {
        match op.operator.as_str() {
            "endcodespacerange" => {
                for entry in op.operands.chunks_exact(2) {
                    if let (Ok(start), Ok(end)) = (entry[0].as_str(), entry[1].as_str()) {
                        mapping.codespace.push(CodeRange { width: start.len() as u32, start: cmap_code(start), end: cmap_code(end) });
                    }
                }
            }
            "endcidrange" => {
                for entry in op.operands.chunks_exact(3) {
                    if let (Ok(lo), Ok(hi), Ok(cid)) = (entry[0].as_str(), entry[1].as_str(), entry[2].as_i64()) {
                        mapping.cid.push(CIDRange { src_code_lo: cmap_code(lo), src_code_hi: cmap_code(hi), dst_cid_lo: cid as u32, reach: 0 });
                    }
                }
            }
            _ => {}
        }
    }
    mapping.codespace.truncate(MAX_CODESPACE_RANGES);
    mapping.cid.truncate(MAX_CMAP_ENTRIES);
    mapping.cid.sort_by_key(|range| range.src_code_lo);
    let mut reach = 0;
    for range in &mut mapping.cid {
        reach = reach.max(range.src_code_hi);
        range.reach = reach;
    }
    mapping
}

/// LOCAL PATCH (QuickSearch): what fonts parse out of their sub-streams,
/// keyed by the stream's address as `Processor.font_table` keys fonts, so
/// font dictionaries naming one ToUnicode CMap, `/Encoding` CMap or font file
/// parse it once per document and share the result. Upstream parsed it again
/// for each font dictionary, uncharged. `parsed` adds up the decoded bytes of
/// the parses since the `Tf` that last built a font, which charges them.
#[derive(Default)]
struct FontParts {
    to_unicode: HashMap<usize, Option<Rc<HashMap<u32, String>>>>,
    encodings: HashMap<usize, Rc<ByteMapping>>,
    /// A Type1 font file's built-in encoding, as the table it gives.
    type1: HashMap<usize, Vec<u16>>,
    /// A Type1C font file's glyph names, as unicode.
    cff: HashMap<usize, Rc<HashMap<u32, String>>>,
    parsed: u64,
}

/// LOCAL PATCH (QuickSearch): `parse` of `stream`'s decoded content, from
/// `cache` or parsed now, adding the decoded length to `parsed`.
fn parse_once<T: Clone>(cache: &mut HashMap<usize, T>, parsed: &mut u64, stream: &Stream, parse: impl FnOnce(&[u8]) -> T) -> T {
    cache.entry(std::ptr::from_ref::<Stream>(stream) as usize).or_insert_with(|| {
        let content = get_contents(stream);
        *parsed += content.len() as u64;
        parse(&content)
    }).clone()
}

fn get_unicode_map<'a>(doc: &'a Document, font: &'a Dictionary, parts: &mut FontParts) -> Option<Rc<HashMap<u32, String>>> {
    let to_unicode = maybe_get_obj(doc, font, b"ToUnicode");
    dlog!("ToUnicode: {:?}", to_unicode);
    match to_unicode {
        Some(&Object::Stream(ref stream)) => {
            parse_once(&mut parts.to_unicode, &mut parts.parsed, stream, |s| to_unicode_map(s).map(Rc::new))
        }
        None => None,
        Some(&Object::Name(ref name)) => {
            let name = pdf_to_utf8(name);
            if name != "Identity-H" {
                todo!("unsupported ToUnicode name: {:?}", name);
            }
            None
        }
        _ => { panic!("unsupported cmap {:?}", to_unicode)}
    }
}


impl<'a> PdfCIDFont<'a> {
    fn new(doc: &'a Document, font: &'a Dictionary, parts: &mut FontParts) -> PdfCIDFont<'a> {
        let base_name = get_name_string(doc, font, b"BaseFont");
        let descendants = maybe_get_array(doc, font, b"DescendantFonts").expect("Descendant fonts required");
        let ciddict = maybe_deref(doc, &descendants[0]).as_dict().expect("should be CID dict");
        let encoding = maybe_get_obj(doc, font, b"Encoding").expect("Encoding required in type0 fonts");
        dlog!("base_name {} {:?}", base_name, font);

        let encoding = match encoding {
            &Object::Name(ref name) => {
                let name = pdf_to_utf8(name);
                dlog!("encoding {:?}", name);
                if name == "Identity-H" || name == "Identity-V" {
                    Rc::new(ByteMapping { codespace: vec![CodeRange{width: 2, start: 0, end: 0xffff }], cid: vec![CIDRange{ src_code_lo: 0, src_code_hi: 0xffff, dst_cid_lo: 0, reach: 0xffff }]})
                } else {
                    panic!("unsupported encoding {}", name);
                }
            }
            &Object::Stream(ref stream) => {
                parse_once(&mut parts.encodings, &mut parts.parsed, stream, |s| Rc::new(byte_mapping(s)))
            }
            _ => { panic!("unsupported encoding {:?}", encoding)}
        };

        // Sometimes a Type0 font might refer to the same underlying data as regular font. In this case we may be able to extract some encoding
        // data.
        // We should also look inside the truetype data to see if there's a cmap table. It will help us convert as well.
        // This won't work if the cmap has been subsetted. A better approach might be to hash glyph contents and use that against
        // a global library of glyph hashes
        let unicode_map = get_unicode_map(doc, font, parts);

        dlog!("descendents {:?} {:?}", descendants, ciddict);

        let font_dict = maybe_get_obj(doc, ciddict, b"FontDescriptor").expect("required");
        dlog!("{:?}", font_dict);
        let _f = font_dict.as_dict().expect("must be dict");
        let default_width = get::<Option<i64>>(doc, ciddict, b"DW").unwrap_or(1000);
        let w: Option<Vec<&Object>> = get(doc, ciddict, b"W");
        dlog!("widths {:?}", w);
        let mut widths = HashMap::new();
        let mut i = 0;
        if let Some(w) = w {
            while i < w.len() {
                if let &Object::Array(ref wa) = w[i+1] {
                    let cid = w[i].as_i64().expect("id should be num");
                    let mut j = 0;
                    dlog!("wa: {:?} -> {:?}", cid, wa);
                    for w in wa {
                        widths.insert((cid + j) as CharCode, as_num(w) );
                        j += 1;
                    }
                    i += 2;
                } else {
                    let c_first = w[i].as_i64().expect("first should be num");
                    let c_last = w[i].as_i64().expect("last should be num");
                    let c_width = as_num(&w[i]);
                    for id in c_first..c_last {
                        widths.insert(id as CharCode, c_width);
                    }
                    i += 3;
                }
            }
        }
        PdfCIDFont{doc, font, widths, to_unicode: unicode_map, encoding, default_width: Some(default_width as f64) }
    }
}

impl<'a> PdfCIDFont<'a> {
    /// LOCAL PATCH (QuickSearch): the CID `/Encoding` maps `code` to: its
    /// offset into its range from the range's first CID. Upstream added the
    /// code itself to that CID, which only an Identity CMap (every range from
    /// code 0 to CID 0) gets right. The ranges are sorted (see byte_mapping),
    /// so the one to try is the last starting at or below the code; upstream
    /// scanned every range for every character. When that range ends below
    /// the code, an earlier, wider one may still hold it (`<0000> <FFFF>`
    /// around a nested `<0020> <007E>`): the first whose `reach` gets there
    /// does, since `reach` only grows.
    fn cid(&self, code: u32) -> Option<u32> {
        let cid = &self.encoding.cid;
        let below = &cid[..cid.partition_point(|range| range.src_code_lo <= code)];
        let last = below.last()?;
        let range = if code <= last.src_code_hi {
            last
        } else {
            below.get(below.partition_point(|range| range.reach < code))?
        };
        (code - range.src_code_lo).checked_add(range.dst_cid_lo)
    }
}

impl<'a> PdfFont for PdfCIDFont<'a> {
    /// LOCAL PATCH (QuickSearch): `id` is the character code (see
    /// next_char); `/W` is keyed by CID.
    fn get_width(&self, id: CharCode) -> f64 {
        let width = self.cid(id).and_then(|cid| self.widths.get(&cid));
        if let Some(width) = width {
            dlog!("GetWidth {} -> {}", id, *width);
            return *width;
        } else {
            dlog!("missing width for {} falling back to default_width", id);
            return self.default_width.unwrap();
        }
    }/*
    fn decode(&self, chars: &[u8]) -> String {
        self.char_codes(chars);

        //let utf16 = Vec::new();

        let encoding = self.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
        to_utf8(encoding, chars)
    }*/

    fn next_char(&self, iter: &mut Iter<u8>) -> Option<(CharCode, u8)> {
        let mut c = *iter.next()? as u32;
        let mut code = None;
        'outer: for width in 1..=4 {
            for range in &self.encoding.codespace {
                if c as u32 >= range.start && c as u32 <= range.end && range.width == width {
                    code = Some((c as u32, width));
                    break 'outer;
                }
            }
            let next = *iter.next()?;
            c = ((c as u32) << 8) | next as u32;
        }
        let code = code?;
        // LOCAL PATCH (QuickSearch): the code itself, not its CID: ToUnicode
        // maps codes, and the word-spacing test in show_text reads code 32.
        // Upstream returned the CID, so a font whose `/Encoding` is not an
        // Identity CMap looked its ToUnicode entries up under the wrong key.
        // A code no CID range maps still ends the string, as upstream.
        self.cid(code.0)?;
        Some((code.0, code.1 as u8))
    }
    fn decode_char(&self, char: CharCode) -> String {
        let s = self.to_unicode.as_ref().and_then(|x| x.get(&char));
        if let Some(s) = s {
            s.clone()
        } else {
            dlog!("Unknown character {:?} in {:?} {:?}", char, self.font, self.to_unicode);
            "".to_string()
        }
    }
}

impl<'a> fmt::Debug for PdfCIDFont<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.font.fmt(f)
    }
}



#[derive(Copy, Clone)]
struct PdfFontDescriptor<'a> {
    desc: &'a Dictionary,
    doc: &'a Document
}

impl<'a> PdfFontDescriptor<'a> {
    #[allow(dead_code)]
    fn get_file(&self) -> Option<&'a Object> {
        maybe_get_obj(self.doc, self.desc, b"FontFile")
    }
}

impl<'a> fmt::Debug for PdfFontDescriptor<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.desc.fmt(f)
    }
}

fn as_num(o: &Object) -> f64 {
    match o {
        &Object::Integer(i) => { i as f64 }
        &Object::Real(f) => { f.into() }
        _ => { panic!("not a number") }
    }
}

#[derive(Clone)]
struct TextState<'a>
{
    font: Option<Rc<dyn PdfFont + 'a>>,
    font_size: f64,
    character_spacing: f64,
    word_spacing: f64,
    horizontal_scaling: f64,
    leading: f64,
    rise: f64,
    tm: Transform,
}

// XXX: We'd ideally implement this without having to copy the uncompressed data
fn get_contents(contents: &Stream) -> Vec<u8> {
    if contents.filters().is_ok() {
        contents.decompressed_content().unwrap_or_else(|_|contents.content.clone())
    } else {
        contents.content.clone()
    }
}

/// LOCAL PATCH (QuickSearch): colour, colour space and soft mask are gone,
/// with everything that resolved them. Nothing read them, and resolving them
/// panicked on legitimate documents (an Indexed colour space, a `gs` naming
/// no ExtGState), which lost the whole document's text.
#[derive(Clone)]
struct GraphicsState<'a>
{
    ctm: Transform,
    ts: TextState<'a>,
    line_width: f64,
}

fn show_text(gs: &mut GraphicsState, s: &[u8],
             _tlm: &Transform,
             _flip_ctm: &Transform,
             output: &mut dyn OutputDev) -> Result<(), OutputError> {
    let ts = &mut gs.ts;
    let font = ts.font.as_ref().unwrap();
    //let encoding = font.encoding.as_ref().map(|x| &x[..]).unwrap_or(&PDFDocEncoding);
    dlog!("{:?}", font.decode(s));
    dlog!("{:?}", font.decode(s).as_bytes());
    dlog!("{:?}", s);
    output.begin_word()?;

    for (c, length) in font.char_codes(s) {
        // 5.3.3 Text Space Details
        let tsm = Transform2D::row_major(ts.horizontal_scaling,
                                                 0.,
                                                 0.,
                                                 1.0,
                                                 0.,
                                                 ts.rise);
        // Trm = Tsm × Tm × CTM
        let trm = tsm.post_transform(&ts.tm.post_transform(&gs.ctm));
        //dlog!("ctm: {:?} tm {:?}", gs.ctm, tm);
        //dlog!("current pos: {:?}", position);
        // 5.9 Extraction of Text Content


        //dlog!("w: {}", font.widths[&(*c as i64)]);
        let w0 = font.get_width(c) / 1000.;

        let mut spacing = ts.character_spacing;
        // "Word spacing is applied to every occurrence of the single-byte character code 32 in a
        //  string when using a simple font or a composite font that defines code 32 as a
        //  single-byte code. It does not apply to occurrences of the byte value 32 in
        //  multiple-byte codes."
        let is_space = c == 32 && length == 1;
        if is_space { spacing += ts.word_spacing }

        output.output_character(&trm, w0, spacing, ts.font_size, &font.decode_char(c))?;
        let tj = 0.;
        let ty = 0.;
        let tx = ts.horizontal_scaling * ((w0 - tj/1000.)* ts.font_size + spacing);
        dlog!("horizontal {} adjust {} {} {} {}", ts.horizontal_scaling, tx, w0, ts.font_size, spacing);
        // dlog!("w0: {}, tx: {}", w0, tx);
        ts.tm = ts.tm.pre_transform(&Transform2D::create_translation(tx, ty));
        let _trm = ts.tm.pre_transform(&gs.ctm);
        //dlog!("post pos: {:?}", trm);

    }
    output.end_word()?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct MediaBox {
    pub llx: f64,
    pub lly: f64,
    pub urx: f64,
    pub ury: f64
}

#[derive(Debug)]
pub enum PathOp {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    // XXX: is it worth distinguishing the different kinds of curve ops?
    CurveTo(f64, f64, f64, f64, f64, f64),
    Rect(f64, f64, f64, f64),
    Close,
}

#[derive(Debug)]
pub struct Path {
    pub ops: Vec<PathOp>
}

impl Path {
    fn new() -> Path {
        Path { ops: Vec::new() }
    }
    fn current_point(&self) -> (f64, f64) {
        match self.ops.last().unwrap() {
            &PathOp::MoveTo(x, y) => { (x, y) }
            &PathOp::LineTo(x, y) => { (x, y) }
            &PathOp::CurveTo(_, _, _, _, x, y) => { (x, y) }
            _ => { panic!() }
        }
    }
}

/// LOCAL PATCH (QuickSearch): the saved graphics states one content stream
/// holds. A `q` past this is counted instead of saved, and the `Q` matching
/// it only uncounts, so `q` and `Q` stay balanced; upstream saved a clone of
/// the state for every `q`, without limit.
const MAX_GS_STACK: usize = 256;

struct Processor<'a> {
    /// LOCAL PATCH (QuickSearch): fonts keyed by their dictionary's address,
    /// which is stable while the document is borrowed, not by resource name.
    /// Names are per resource dictionary: keyed by name, a later page's `/F1`
    /// decoded with an earlier page's, and every alias parsed its font again.
    font_table: HashMap<usize, Rc<dyn PdfFont + 'a>>,
    /// LOCAL PATCH (QuickSearch): what those fonts parsed out of their
    /// sub-streams.
    parts: FontParts,
    /// LOCAL PATCH (QuickSearch): nesting of `Do` (Form XObject) recursion.
    xobject_depth: u32,
    /// LOCAL PATCH (QuickSearch): the work this document has spent; see
    /// [`MAX_PDF_WORK`].
    work: u64,
    _none: PhantomData<&'a ()>,
}

impl<'a> Processor<'a> {
    fn new() -> Processor<'a> {
        Processor { font_table: HashMap::new(), parts: FontParts::default(), xobject_depth: 0, work: 0, _none: PhantomData }
    }

    /// LOCAL PATCH (QuickSearch): spend `work` of the document's budget; past
    /// [`MAX_PDF_WORK`], rendering stops with [`OutputError::Budget`].
    fn charge(&mut self, work: u64) -> Result<(), OutputError> {
        self.work += work;
        if self.work > MAX_PDF_WORK {
            return Err(OutputError::Budget);
        }
        Ok(())
    }

    /// LOCAL PATCH (QuickSearch): `Document::get_page_content` within the
    /// budget. The page's streams are concatenated newline-separated as lopdf
    /// does, so graphics and text state carry from one stream into the next.
    /// `/Contents` may name one stream any number of times, and lopdf inflated
    /// every copy: here each copy is charged its encoded bytes before it is
    /// inflated, and the buffer ends the document once `process_stream`
    /// could not afford it.
    fn page_content(&mut self, doc: &Document, page_id: ObjectId) -> Result<Vec<u8>, OutputError> {
        let mut content = Vec::new();
        for id in doc.get_page_contents(page_id) {
            if let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) {
                self.charge(stream.content.len() as u64)?;
                content.extend_from_slice(&get_contents(stream));
                content.push(b'\n');
                if self.work + content.len() as u64 > MAX_PDF_WORK {
                    return Err(OutputError::Budget);
                }
            }
        }
        Ok(content)
    }

    /// LOCAL PATCH (QuickSearch): the font for `font_dict`, from the table or
    /// built now. Building is charged what the font reads before it is built
    /// (see [`font_work`]), then the decoded bytes of the sub-streams it was
    /// first to parse (see [`FontParts`]). Kept out of `process_stream`,
    /// whose frame every nested `Do` stacks again.
    fn font(&mut self, doc: &'a Document, font_dict: &'a Dictionary) -> Result<Rc<dyn PdfFont + 'a>, OutputError> {
        let key = std::ptr::from_ref::<Dictionary>(font_dict) as usize;
        if let Some(font) = self.font_table.get(&key) {
            return Ok(font.clone());
        }
        self.charge(font_work(doc, font_dict))?;
        let font = make_font(doc, font_dict, &mut self.parts);
        self.font_table.insert(key, font.clone());
        let parsed = std::mem::take(&mut self.parts.parsed);
        self.charge(parsed)?;
        Ok(font)
    }

    fn process_stream(&mut self, doc: &'a Document, content: Vec<u8>, resources: &'a Dictionary, media_box: &MediaBox, output: &mut dyn OutputDev, page_num: u32) -> Result<(), OutputError> {
        // LOCAL PATCH (QuickSearch): charge the stream before decoding it.
        self.charge(content.len() as u64 + MIN_STREAM_WORK)?;
        let content = Content::decode(&content).unwrap();
        let mut gs: GraphicsState = GraphicsState {
            ts: TextState {
                font: None,
                font_size: std::f64::NAN,
                character_spacing: 0.,
                word_spacing: 0.,
                horizontal_scaling: 100. / 100.,
                leading: 0.,
                rise: 0.,
                tm: Transform2D::identity(),
            },
            line_width: 1.,
            ctm: Transform2D::identity(),
        };
        //let mut ts = &mut gs.ts;
        let mut gs_stack = Vec::new();
        // LOCAL PATCH (QuickSearch): `q`s past MAX_GS_STACK not yet matched.
        let mut gs_overflow = 0usize;
        let mut mc_stack = Vec::new();
        // XXX: replace tlm with a point for text start
        let mut tlm = Transform2D::identity();
        let mut path = Path::new();
        let flip_ctm = Transform2D::row_major(1., 0., 0., -1., 0., media_box.ury - media_box.lly);
        dlog!("MediaBox {:?}", media_box);
        for operation in &content.operations {
            //dlog!("op: {:?}", operation);

            match operation.operator.as_ref() {
                "BT" => {
                    tlm = Transform2D::identity();
                    gs.ts.tm = tlm;
                }
                "ET" => {
                    tlm = Transform2D::identity();
                    gs.ts.tm = tlm;
                }
                "cm" => {
                    assert!(operation.operands.len() == 6);
                    let m = Transform2D::row_major(as_num(&operation.operands[0]),
                                                   as_num(&operation.operands[1]),
                                                   as_num(&operation.operands[2]),
                                                   as_num(&operation.operands[3]),
                                                   as_num(&operation.operands[4]),
                                                   as_num(&operation.operands[5]));
                    gs.ctm = gs.ctm.pre_transform(&m);
                    dlog!("matrix {:?}", gs.ctm);
                }
                // LOCAL PATCH (QuickSearch): colour is not tracked; see GraphicsState.
                "CS" | "cs" | "SC" | "SCN" | "sc" | "scn" | "G" | "g" | "RG" | "rg" | "K" | "k" => {
                    dlog!("unhandled color operation {:?}", operation);
                }
                "TJ" => {
                    match operation.operands[0] {
                        Object::Array(ref array) => {
                            for e in array {
                                match e {
                                    &Object::String(ref s, _) => {
                                        show_text(&mut gs, s, &tlm, &flip_ctm, output)?;
                                    }
                                    &Object::Integer(i) => {
                                        let ts = &mut gs.ts;
                                        let w0 = 0.;
                                        let tj = i as f64;
                                        let ty = 0.;
                                        let tx = ts.horizontal_scaling * ((w0 - tj / 1000.) * ts.font_size);
                                        ts.tm = ts.tm.pre_transform(&Transform2D::create_translation(tx, ty));
                                        dlog!("adjust text by: {} {:?}", i, ts.tm);
                                    }
                                    &Object::Real(i) => {
                                        let ts = &mut gs.ts;
                                        let w0 = 0.;
                                        let tj = i as f64;
                                        let ty = 0.;
                                        let tx = ts.horizontal_scaling * ((w0 - tj / 1000.) * ts.font_size);
                                        ts.tm = ts.tm.pre_transform(&Transform2D::create_translation(tx, ty));
                                        dlog!("adjust text by: {} {:?}", i, ts.tm);
                                    }
                                    _ => { dlog!("kind of {:?}", e); }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "Tj" => {
                    match operation.operands[0] {
                        Object::String(ref s, _) => {
                            show_text(&mut gs, s, &tlm, &flip_ctm, output)?;
                        }
                        _ => { panic!("unexpected Tj operand {:?}", operation) }
                    }
                }
                "Tc" => {
                    gs.ts.character_spacing = as_num(&operation.operands[0]);
                }
                "Tw" => {
                    gs.ts.word_spacing = as_num(&operation.operands[0]);
                }
                "Tz" => {
                    gs.ts.horizontal_scaling = as_num(&operation.operands[0]) / 100.;
                }
                "TL" => {
                    gs.ts.leading = as_num(&operation.operands[0]);
                }
                "Tf" => {
                    let fonts: &Dictionary = get(&doc, resources, b"Font");
                    let name = operation.operands[0].as_name().unwrap();
                    let font_dict: &Dictionary = get(doc, fonts, name);
                    let font = self.font(doc, font_dict)?;
                    {
                        /*let file = font.get_descriptor().and_then(|desc| desc.get_file());
                    if let Some(file) = file {
                        let file_contents = filter_data(file.as_stream().unwrap());
                        let mut cursor = Cursor::new(&file_contents[..]);
                        //let f = Font::read(&mut cursor);
                        //dlog!("font file: {:?}", f);
                    }*/
                    }
                    gs.ts.font = Some(font);

                    gs.ts.font_size = as_num(&operation.operands[1]);
                    dlog!("font {} size: {} {:?}", pdf_to_utf8(name), gs.ts.font_size, operation);
                }
                "Ts" => {
                    gs.ts.rise = as_num(&operation.operands[0]);
                }
                "Tm" => {
                    assert!(operation.operands.len() == 6);
                    tlm = Transform2D::row_major(as_num(&operation.operands[0]),
                                                 as_num(&operation.operands[1]),
                                                 as_num(&operation.operands[2]),
                                                 as_num(&operation.operands[3]),
                                                 as_num(&operation.operands[4]),
                                                 as_num(&operation.operands[5]));
                    gs.ts.tm = tlm;
                    dlog!("Tm: matrix {:?}", gs.ts.tm);
                    output.end_line()?;
                }
                "Td" => {
                    /* Move to the start of the next line, offset from the start of the current line by (tx , ty ).
                   tx and ty are numbers expressed in unscaled text space units.
                   More precisely, this operator performs the following assignments:
                 */
                    assert!(operation.operands.len() == 2);
                    let tx = as_num(&operation.operands[0]);
                    let ty = as_num(&operation.operands[1]);
                    dlog!("translation: {} {}", tx, ty);

                    tlm = tlm.pre_transform(&Transform2D::create_translation(tx, ty));
                    gs.ts.tm = tlm;
                    dlog!("Td matrix {:?}", gs.ts.tm);
                    output.end_line()?;
                }

                "TD" => {
                    /* Move to the start of the next line, offset from the start of the current line by (tx , ty ).
                   As a side effect, this operator sets the leading parameter in the text state.
                 */
                    assert!(operation.operands.len() == 2);
                    let tx = as_num(&operation.operands[0]);
                    let ty = as_num(&operation.operands[1]);
                    dlog!("translation: {} {}", tx, ty);
                    gs.ts.leading = -ty;

                    tlm = tlm.pre_transform(&Transform2D::create_translation(tx, ty));
                    gs.ts.tm = tlm;
                    dlog!("TD matrix {:?}", gs.ts.tm);
                    output.end_line()?;
                }

                "T*" => {
                    let tx = 0.0;
                    let ty = -gs.ts.leading;

                    tlm = tlm.pre_transform(&Transform2D::create_translation(tx, ty));
                    gs.ts.tm = tlm;
                    dlog!("T* matrix {:?}", gs.ts.tm);
                    output.end_line()?;
                }
                "q" => {
                    if gs_stack.len() < MAX_GS_STACK {
                        gs_stack.push(gs.clone());
                    } else {
                        gs_overflow += 1;
                    }
                }
                "Q" => {
                    if gs_overflow > 0 {
                        gs_overflow -= 1;
                    } else if let Some(s) = gs_stack.pop() {
                        gs = s;
                    } else {
                        warn!("No state to pop");
                    }
                }
                "i" => { dlog!("unhandled graphics state flattness operator {:?}", operation); }
                "w" => { gs.line_width = as_num(&operation.operands[0]); }
                "J" | "j" | "M" | "d" | "ri" | "gs" => { dlog!("unknown graphics state operator {:?}", operation); }
                "m" => { path.ops.push(PathOp::MoveTo(as_num(&operation.operands[0]), as_num(&operation.operands[1]))) }
                "l" => { path.ops.push(PathOp::LineTo(as_num(&operation.operands[0]), as_num(&operation.operands[1]))) }
                "c" => {
                    path.ops.push(PathOp::CurveTo(
                        as_num(&operation.operands[0]),
                        as_num(&operation.operands[1]),
                        as_num(&operation.operands[2]),
                        as_num(&operation.operands[3]),
                        as_num(&operation.operands[4]),
                        as_num(&operation.operands[5])))
                }
                "v" => {
                    let (x, y) = path.current_point();
                    path.ops.push(PathOp::CurveTo(
                        x,
                        y,
                        as_num(&operation.operands[0]),
                        as_num(&operation.operands[1]),
                        as_num(&operation.operands[2]),
                        as_num(&operation.operands[3])))
                }
                "y" => {
                    path.ops.push(PathOp::CurveTo(
                        as_num(&operation.operands[0]),
                        as_num(&operation.operands[1]),
                        as_num(&operation.operands[2]),
                        as_num(&operation.operands[3]),
                        as_num(&operation.operands[2]),
                        as_num(&operation.operands[3])))
                }
                "h" => { path.ops.push(PathOp::Close) }
                "re" => {
                    path.ops.push(PathOp::Rect(as_num(&operation.operands[0]),
                                               as_num(&operation.operands[1]),
                                               as_num(&operation.operands[2]),
                                               as_num(&operation.operands[3])))
                }
                "s" | "f*" | "B" | "B*" | "b" => {
                    dlog!("unhandled path op {:?}", operation);
                }
                "S" => {
                    output.stroke(&gs.ctm, &path)?;
                    path.ops.clear();
                }
                "F" | "f" => {
                    output.fill(&gs.ctm, &path)?;
                    path.ops.clear();
                }
                "W" | "w*" => { dlog!("unhandled clipping operation {:?}", operation); }
                "n" => {
                    dlog!("discard {:?}", path);
                    path.ops.clear();
                }
                "BMC" | "BDC" => {
                    mc_stack.push(operation);
                }
                "EMC" => {
                    mc_stack.pop();
                }
                "Do" => {
                    // `Do` process an entire subdocument, so we do a recursive call to `process_stream`
                    // with the subdocument content and resources
                    let xobject: &Dictionary = get(&doc, resources, b"XObject");
                    let name = operation.operands[0].as_name().unwrap();
                    let xf: &Stream = get(&doc, xobject, name);
                    // LOCAL PATCH (QuickSearch): only a Form XObject is
                    // content; an image's samples were inflated and parsed as
                    // operators. A form that draws itself, directly or
                    // through a ring, recursed here without bound: the depth
                    // counter bounds the stack; Processor.work (MAX_PDF_WORK)
                    // bounds the branching case, where each level draws the
                    // next twice. Both tests come before anything is inflated.
                    if maybe_get_name(doc, &xf.dict, b"Subtype") != Some(&b"Form"[..]) {
                        dlog!("skipping Do: not a Form XObject");
                    } else if self.xobject_depth >= MAX_PDF_RECURSION_DEPTH {
                        dlog!("skipping Do: XObject nesting past {}", MAX_PDF_RECURSION_DEPTH);
                    } else {
                        let resources = maybe_get_obj(&doc, &xf.dict, b"Resources").and_then(|n| n.as_dict().ok()).unwrap_or(resources);
                        // LOCAL PATCH (QuickSearch): inflating costs the
                        // encoded bytes, which process_stream's charge of the
                        // decoded ones misses: a form of empty deflate blocks
                        // decodes to almost nothing, however often it is drawn.
                        self.charge(xf.content.len() as u64)?;
                        let contents = get_contents(xf);
                        self.xobject_depth += 1;
                        let r = self.process_stream(&doc, contents, resources, &media_box, output, page_num);
                        self.xobject_depth -= 1;
                        r?;
                    }
                }
                _ => { dlog!("unknown operation {:?}", operation); }

            }
        }
        Ok(())
    }
}


pub trait OutputDev {
    fn begin_page(&mut self, page_num: u32, media_box: &MediaBox, art_box: Option<(f64, f64, f64, f64)>)-> Result<(), OutputError>;
    fn end_page(&mut self)-> Result<(), OutputError>;
    fn output_character(&mut self, trm: &Transform, width: f64, spacing: f64, font_size: f64, char: &str) -> Result<(), OutputError>;
    fn begin_word(&mut self)-> Result<(), OutputError>;
    fn end_word(&mut self)-> Result<(), OutputError>;
    fn end_line(&mut self)-> Result<(), OutputError>;
    // LOCAL PATCH (QuickSearch): no colour arguments; see GraphicsState.
    fn stroke(&mut self, _ctm: &Transform, _path: &Path)-> Result<(), OutputError> {Ok(())}
    fn fill(&mut self, _ctm: &Transform, _path: &Path)-> Result<(), OutputError> {Ok(())}
}


pub struct HTMLOutput<'a>  {
    file: &'a mut dyn std::io::Write,
    flip_ctm: Transform,
    last_ctm: Transform,
    buf_ctm: Transform,
    buf_font_size: f64,
    buf: String
}

fn insert_nbsp(input: &str) -> String {
    let mut result = String::new();
    let mut word_end = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            if !word_end || chars.peek().filter(|x| **x != ' ').is_none() {
                result += "&nbsp;";
            } else {
                result += " ";
            }
            word_end = false;
        } else {
            word_end = true;
            result.push(c);
        }
    }
    result
}

impl<'a> HTMLOutput<'a> {
    pub fn new(file: &mut dyn std::io::Write) -> HTMLOutput {
        HTMLOutput {
            file,
            flip_ctm: Transform2D::identity(),
            last_ctm: Transform2D::identity(),
            buf_ctm: Transform2D::identity(),
            buf: String::new(),
            buf_font_size: 0.,
        }
    }
    fn flush_string(&mut self) -> Result<(), OutputError>{
        if self.buf.len() != 0 {

            let position = self.buf_ctm.post_transform(&self.flip_ctm);
            let transformed_font_size_vec = self.buf_ctm.transform_vector(vec2(self.buf_font_size, self.buf_font_size));
            // get the length of one sized of the square with the same area with a rectangle of size (x, y)
            let transformed_font_size = (transformed_font_size_vec.x * transformed_font_size_vec.y).sqrt();
            let (x, y) = (position.m31, position.m32);
            warn!("flush {} {:?}", self.buf, (x,y));

            write!(self.file, "<div style='position: absolute; left: {}px; top: {}px; font-size: {}px'>{}</div>\n",
                   x, y, transformed_font_size, insert_nbsp(&self.buf))?;
        }
        Ok(())
    }
}

type ArtBox = (f64, f64, f64, f64);

impl<'a> OutputDev for HTMLOutput<'a> {
    fn begin_page(&mut self, page_num: u32, media_box: &MediaBox, _: Option<ArtBox>) -> Result<(), OutputError> {
        write!(self.file, "<meta charset='utf-8' /> ")?;
        write!(self.file, "<!-- page {} -->", page_num)?;
        write!(self.file, "<div id='page{}' style='position: relative; height: {}px; width: {}px; border: 1px black solid'>", page_num, media_box.ury - media_box.lly, media_box.urx - media_box.llx)?;
        self.flip_ctm = Transform::row_major(1., 0., 0., -1., 0., media_box.ury - media_box.lly);
        Ok(())
    }
    fn end_page(&mut self) -> Result<(), OutputError> {
        self.flush_string()?;
        self.buf = String::new();
        self.last_ctm = Transform::identity();
        write!(self.file, "</div>")?;
        Ok(())
    }
    fn output_character(&mut self, trm: &Transform, width: f64, spacing: f64, font_size: f64, char: &str) -> Result<(), OutputError>{
        if trm.approx_eq(&self.last_ctm) {
            let position = trm.post_transform(&self.flip_ctm);
            let (x, y) = (position.m31, position.m32);

            warn!("accum {} {:?}", char, (x,y));
            self.buf += char;
        } else {
            warn!("flush {} {:?} {:?} {} {} {}", char, trm, self.last_ctm, width, font_size, spacing);
            self.flush_string()?;
            self.buf = char.to_owned();
            self.buf_font_size = font_size;
            self.buf_ctm = *trm;
        }
        let position = trm.post_transform(&self.flip_ctm);
        let transformed_font_size_vec = trm.transform_vector(vec2(font_size, font_size));
        // get the length of one sized of the square with the same area with a rectangle of size (x, y)
        let transformed_font_size = (transformed_font_size_vec.x * transformed_font_size_vec.y).sqrt();
        let (x, y) = (position.m31, position.m32);
        write!(self.file, "<div style='position: absolute; color: red; left: {}px; top: {}px; font-size: {}px'>{}</div>",
               x, y, transformed_font_size, char)?;
        self.last_ctm = trm.pre_transform(&Transform2D::create_translation(width * font_size + spacing, 0.));

        Ok(())
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {Ok(())}
    fn end_word(&mut self) -> Result<(), OutputError> {Ok(())}
    fn end_line(&mut self) -> Result<(), OutputError> {Ok(())}
}

pub struct SVGOutput<'a>  {
    file: &'a mut dyn std::io::Write
}
impl<'a> SVGOutput<'a> {
    pub fn new(file: &mut dyn std::io::Write) -> SVGOutput {
        SVGOutput{file}
    }
}

impl<'a> OutputDev for SVGOutput<'a> {
    fn begin_page(&mut self, _page_num: u32, media_box: &MediaBox, art_box: Option<(f64, f64, f64, f64)>) -> Result<(), OutputError> {
        let ver = 1.1;
        write!(self.file, "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n")?;
        if ver == 1.1 {
            write!(self.file, r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">"#)?;
        } else {
            write!(self.file, r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.0//EN" "http://www.w3.org/TR/2001/REC-SVG-20010904/DTD/svg10.dtd">"#)?;
        }
        if let Some(art_box) = art_box {
            let width = art_box.2 - art_box.0;
            let height = art_box.3 - art_box.1;
            let y = media_box.ury - art_box.1 - height;
            write!(self.file, "<svg width=\"{}\" height=\"{}\" xmlns=\"http://www.w3.org/2000/svg\" version=\"{}\" viewBox='{} {} {} {}'>", width, height, ver, art_box.0, y, width, height)?;
        } else {
            let width = media_box.urx - media_box.llx;
            let height = media_box.ury - media_box.lly;
            write!(self.file, "<svg width=\"{}\" height=\"{}\" xmlns=\"http://www.w3.org/2000/svg\" version=\"{}\" viewBox='{} {} {} {}'>", width, height, ver, media_box.llx, media_box.lly, width, height)?;
        }
        write!(self.file, "\n")?;
        type Mat = Transform;

        let ctm = Mat::create_scale(1., -1.).post_translate(vec2(0., media_box.ury));
        write!(self.file, "<g transform='matrix({}, {}, {}, {}, {}, {})'>\n",
               ctm.m11,
               ctm.m12,
               ctm.m21,
               ctm.m22,
               ctm.m31,
               ctm.m32,
        )?;
        Ok(())
    }
    fn end_page(&mut self) -> Result<(), OutputError>{
        write!(self.file, "</g>\n")?;
        write!(self.file, "</svg>")?;
        Ok(())
    }
    fn output_character(&mut self, _trm: &Transform, _width: f64, _spacing: f64, _font_size: f64, _char: &str) -> Result<(), OutputError>{
        Ok(())
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {Ok(())}
    fn end_word(&mut self) -> Result<(), OutputError> {Ok(())}
    fn end_line(&mut self) -> Result<(), OutputError> {Ok(())}
    fn fill(&mut self, ctm: &Transform, path: &Path) -> Result<(), OutputError>{
        write!(self.file, "<g transform='matrix({}, {}, {}, {}, {}, {})'>",
               ctm.m11,
               ctm.m12,
               ctm.m21,
               ctm.m22,
               ctm.m31,
               ctm.m32,
        )?;

        /*if path.ops.len() == 1 {
            if let PathOp::Rect(x, y, width, height) = path.ops[0] {
                write!(self.file, "<rect x={} y={} width={} height={} />\n", x, y, width, height);
                write!(self.file, "</g>");
                return;
            }
        }*/
        let mut d = Vec::new();
        for op in &path.ops {
            match op {
                &PathOp::MoveTo(x, y) => { d.push(format!("M{} {}", x, y))}
                &PathOp::LineTo(x, y) => { d.push(format!("L{} {}", x, y))},
                &PathOp::CurveTo(x1, y1, x2, y2, x, y) => { d.push(format!("C{} {} {} {} {} {}", x1, y1, x2, y2, x, y))},
                &PathOp::Close => { d.push(format!("Z"))},
                &PathOp::Rect(x, y, width, height) => {
                    d.push(format!("M{} {}", x, y));
                    d.push(format!("L{} {}", x + width, y));
                    d.push(format!("L{} {}", x + width, y + height));
                    d.push(format!("L{} {}", x, y + height));
                    d.push(format!("Z"));
                }

            }
        }
        write!(self.file, "<path d='{}' />", d.join(" "))?;
        write!(self.file, "</g>")?;
        write!(self.file, "\n")?;
        Ok(())
    }
}

/*
File doesn't implement std::fmt::Write so we have
to do some gymnastics to accept a File or String
See https://github.com/rust-lang/rust/issues/51305
*/

pub trait ConvertToFmt {
    type Writer: std::fmt::Write;
    fn convert(self) -> Self::Writer;
}

impl<'a> ConvertToFmt for &'a mut String {
    type Writer = &'a mut String;
    fn convert(self) -> Self::Writer {
        self
    }
}

pub struct WriteAdapter<W> {
    f: W,
}

impl<W: std::io::Write> std::fmt::Write for WriteAdapter<W> {
    fn write_str(&mut self, s: &str) -> Result<(), std::fmt::Error> {
        self.f.write_all(s.as_bytes()).map_err(|_| fmt::Error)
    }
}

impl<'a> ConvertToFmt for &'a mut dyn std::io::Write {
    type Writer = WriteAdapter<Self>;
    fn convert(self) -> Self::Writer {
        WriteAdapter { f: self }
    }
}

impl<'a> ConvertToFmt for &'a mut File {
    type Writer = WriteAdapter<Self>;
    fn convert(self) -> Self::Writer {
        WriteAdapter { f: self }
    }
}

pub struct PlainTextOutput<W: ConvertToFmt>   {
    writer: W::Writer,
    last_end: f64,
    last_y: f64,
    first_char: bool,
    flip_ctm: Transform,
}

impl<W: ConvertToFmt> PlainTextOutput<W> {
    pub fn new(writer: W) -> PlainTextOutput<W> {
        PlainTextOutput{
            writer: writer.convert(),
            last_end: 100000.,
            first_char: false,
            last_y: 0.,
            flip_ctm: Transform2D::identity(),
        }
    }
}

/* There are some structural hints that PDFs can use to signal word and line endings:
 * however relying on these is not likely to be sufficient. */
impl<W: ConvertToFmt> OutputDev for PlainTextOutput<W> {
    fn begin_page(&mut self, _page_num: u32, media_box: &MediaBox, _: Option<ArtBox>) -> Result<(), OutputError> {
        self.flip_ctm = Transform2D::row_major(1., 0., 0., -1., 0., media_box.ury - media_box.lly);
        Ok(())
    }
    fn end_page(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn output_character(&mut self, trm: &Transform, width: f64, _spacing: f64, font_size: f64, char: &str) -> Result<(), OutputError> {
        let position = trm.post_transform(&self.flip_ctm);
        let transformed_font_size_vec = trm.transform_vector(vec2(font_size, font_size));
        // get the length of one sized of the square with the same area with a rectangle of size (x, y)
        let transformed_font_size = (transformed_font_size_vec.x*transformed_font_size_vec.y).sqrt();
        let (x, y) = (position.m31, position.m32);
        use std::fmt::Write;
        //dlog!("last_end: {} x: {}, width: {}", self.last_end, x, width);
        if self.first_char {
            if (y - self.last_y).abs() > transformed_font_size * 1.5 {
                write!(self.writer, "\n")?;
            }

            // we've moved to the left and down
            if x < self.last_end && (y - self.last_y).abs() > transformed_font_size * 0.5 {
                write!(self.writer, "\n")?;
            }

            if x > self.last_end + transformed_font_size * 0.1 {
                dlog!("width: {}, space: {}, thresh: {}", width, x - self.last_end, transformed_font_size * 0.1);
                write!(self.writer, " ")?;
            }
        }
        //let norm = unicode_normalization::UnicodeNormalization::nfkc(char);
        write!(self.writer, "{}", char)?;
        self.first_char = false;
        self.last_y = y;
        self.last_end = x + width * transformed_font_size;
        Ok(())
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {
        self.first_char = true;
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {Ok(())}
    fn end_line(&mut self) -> Result<(), OutputError>{
        //write!(self.file, "\n");
        Ok(())
    }
}


pub fn print_metadata(doc: &Document) {
    dlog!("Version: {}", doc.version);
    if let Some(ref info) = get_info(&doc) {
        for (k, v) in *info {
            match v {
                &Object::String(ref s, StringFormat::Literal) => { dlog!("{}: {}", pdf_to_utf8(k), pdf_to_utf8(s)); }
                _ => {}
            }
        }
    }
    dlog!("Page count: {}", get::<i64>(&doc, &get_pages(&doc), b"Count"));
    dlog!("Pages: {:?}", get_pages(&doc));
    dlog!("Type: {:?}", get_pages(&doc).get(b"Type").and_then(|x| x.as_name()).unwrap());
}

/// Extract the text from a pdf at `path` and return a `String` with the results
pub fn extract_text<P: std::convert::AsRef<std::path::Path>>(path: P) -> Result<String, OutputError> {
    let mut s = String::new();
    {
        let mut output = PlainTextOutput::new(&mut s);
        let mut doc = Document::load(path)?;
        maybe_decrypt(&mut doc)?;
        output_doc(&doc, &mut output)?;
    }
    Ok(s)
}

fn maybe_decrypt(doc: &mut Document) -> Result<(), OutputError> {
    if ! doc.is_encrypted() {
        return Ok(());
    }

    if let Err(e) = doc.decrypt("") {
        if let Error::Decryption(DecryptionError::IncorrectPassword) = e {
            error!("Encrypted documents must be decrypted with a password using {{extract_text|extract_text_from_mem|output_doc}}_encrypted")
        }

        return Err(OutputError::PdfError(e));
    }

    Ok(())
}

pub fn extract_text_encrypted<P: std::convert::AsRef<std::path::Path>>(
    path: P,
    password: &str,
) -> Result<String, OutputError> {
    let mut s = String::new();
    {
        let mut output = PlainTextOutput::new(&mut s);
        let mut doc = Document::load(path)?;
        output_doc_encrypted(&mut doc, &mut output, password)?;
    }
    Ok(s)
}

pub fn extract_text_from_mem(buffer: &[u8]) -> Result<String, OutputError> {
    let mut s = String::new();
    {
        let mut output = PlainTextOutput::new(&mut s);
        let mut doc = Document::load_mem(buffer)?;
        maybe_decrypt(&mut doc)?;
        output_doc(&doc, &mut output)?;
    }
    Ok(s)
}

pub fn extract_text_from_mem_encrypted(
    buffer: &[u8],
    password: &str,
) -> Result<String, OutputError> {
    let mut s = String::new();
    {
        let mut output = PlainTextOutput::new(&mut s);
        let mut doc = Document::load_mem(buffer)?;
        output_doc_encrypted(&mut doc, &mut output, password)?;
    }
    Ok(s)
}


fn extract_text_by_page(doc: &Document, page_num: u32) -> Result<String, OutputError> {
    let mut s = String::new();
    {
        let mut output = PlainTextOutput::new(&mut s);
        output_doc_page(doc, &mut output, page_num)?;
    }
    Ok(s)
}

/// Extract the text from a pdf at `path` and return a `Vec<String>` with the results separately by page

pub fn extract_text_by_pages<P: std::convert::AsRef<std::path::Path>>(path: P) -> Result<Vec<String>, OutputError> {
    let mut v = Vec::new();
    {
        let mut doc = Document::load(path)?;
        maybe_decrypt(&mut doc)?;
        let mut page_num = 1;
        while let Ok(content) = extract_text_by_page(&doc, page_num) {
            v.push(content);
            page_num += 1;
        }
    }
    Ok(v)
}

pub fn extract_text_by_pages_encrypted<P: std::convert::AsRef<std::path::Path>>(path: P, password: &str) -> Result<Vec<String>, OutputError> {
    let mut v = Vec::new();
    {
        let mut doc = Document::load(path)?;
        doc.decrypt(password)?;
        let mut page_num = 1;
        while let Ok(content) = extract_text_by_page(&mut doc, page_num) {
            v.push(content);
            page_num += 1;
        }
    }
    Ok(v)
}

pub fn extract_text_from_mem_by_pages(buffer: &[u8]) -> Result<Vec<String>, OutputError> {
    let mut v = Vec::new();
    {
        let mut doc = Document::load_mem(buffer)?;
        maybe_decrypt(&mut doc)?;
        let mut page_num = 1;
        while let Ok(content) = extract_text_by_page(&doc, page_num) {
            v.push(content);
            page_num += 1;
        }
    }
    Ok(v)
}

pub fn extract_text_from_mem_by_pages_encrypted(buffer: &[u8], password: &str) -> Result<Vec<String>, OutputError> {
    let mut v = Vec::new();
    {
        let mut doc = Document::load_mem(buffer)?;
        doc.decrypt(password)?;
        let mut page_num = 1;
        while let Ok(content) = extract_text_by_page(&doc, page_num) {
            v.push(content);
            page_num += 1;
        }
    }
    Ok(v)
}


/// Depth bound for the two recursions in this file.
///
/// LOCAL PATCH (QuickSearch): a `/Parent` chain is a tree in a well-formed
/// document and can be a cycle in a crafted one, and a Form XObject may draw
/// itself. Both recursions were unbounded, so a few hundred bytes of PDF
/// could exhaust the stack — which is not a catchable panic: Rust's handler
/// aborts the process, which under the sandbox is the PDF child and without
/// it the indexer and the GUI. Real documents nest nothing here more than a
/// handful deep. The depth bounds the stack, not the work: a form drawing the
/// next level twice costs 2^depth calls, which [`MAX_PDF_WORK`] bounds.
///
/// That this depth fits the worker stack is tested, over the deepest shape
/// these recursions reach: `extract::pdf`'s
/// `pdf_recursion_bounds_fit_the_worker_stack`. It was 64, which fit a
/// release build but overflowed an unoptimized one, where a
/// `process_stream` frame measures about 7 KB and lopdf's content tokenizer
/// adds its own nesting inside each. lopdf's own recursions (an indirect
/// `/Length` chain, a `/Length` inside its own object stream) are not
/// bounded at all, which is why PDFs are parsed in a child process.
pub const MAX_PDF_RECURSION_DEPTH: u32 = 32;

/// LOCAL PATCH (QuickSearch): the work one document may spend rendering,
/// counted in bytes; past it, rendering stops with [`OutputError::Budget`].
/// Each content stream inflated is charged its encoded length (a page's
/// `/Contents` entries, a form each time it is drawn), each content stream
/// entered its decoded length plus [`MIN_STREAM_WORK`], and each font built
/// what [`font_work`] says it reads plus the decoded length of the font
/// streams it was first to parse. The 2^depth branching shape is the reason:
/// a text-free form never fills the caller's text sink, so nothing else stops
/// it. Here it stops after about half a million streams, measured at 0.3 to
/// 0.5 s in an optimized build (2026-10-10), or 0.8 s when the form is a tiny
/// FlateDecode stream, inflated afresh for each draw; an indexer thread
/// holding a write transaction can afford that. It also bounds what a page's
/// `/Contents` concatenates, however often one stream repeats.
pub const MAX_PDF_WORK: u64 = 256 << 20;

/// LOCAL PATCH (QuickSearch): the least a content stream entered costs
/// against [`MAX_PDF_WORK`], however small, so the budget bounds how many
/// there are as well as how many bytes are decoded.
const MIN_STREAM_WORK: u64 = 512;

/// LOCAL PATCH (QuickSearch): the least a font built costs against
/// [`MAX_PDF_WORK`]. Building a base-14 font's tables takes about 93 µs in an
/// optimized build, so the budget allows 16,384 fonts, about 1.5 s of them.
const FONT_WORK: u64 = 16 << 10;

/// LOCAL PATCH (QuickSearch): what a font built costs for each element of the
/// width and encoding arrays it reads; see [`font_work`].
const FONT_ELEMENT_WORK: u64 = 8;

/// LOCAL PATCH (QuickSearch): what building `font` costs against
/// [`MAX_PDF_WORK`], charged before it is built: [`FONT_WORK`], plus
/// [`FONT_ELEMENT_WORK`] for each element of the `/Widths`, `/W` (nested
/// arrays included) and `/Differences` arrays it reads. Any number of font
/// dictionaries may share one such array, and each reads it afresh. A lookup
/// that fails costs nothing, and does not panic: the build reports it.
fn font_work(doc: &Document, font: &Dictionary) -> u64 {
    fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
        doc.dereference(o).ok().map(|(_, o)| o)
    }
    fn array<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> &'a [Object] {
        let array = dict.get(key).ok().and_then(|o| deref(doc, o)?.as_array().ok());
        array.map_or(&[][..], Vec::as_slice)
    }
    let dict = |o| deref(doc, o).and_then(|o| o.as_dict().ok());
    let mut elements = array(doc, font, b"Widths").len();
    if let Some(encoding) = font.get(b"Encoding").ok().and_then(dict) {
        elements += array(doc, encoding, b"Differences").len();
    }
    if let Some(cid) = array(doc, font, b"DescendantFonts").first().and_then(dict) {
        let w = array(doc, cid, b"W");
        elements += w.len();
        elements += w.iter().filter_map(|o| deref(doc, o)?.as_array().ok()).map(Vec::len).sum::<usize>();
    }
    FONT_WORK + FONT_ELEMENT_WORK * elements as u64
}

fn get_inherited<'a, T: FromObj<'a>>(doc: &'a Document, dict: &'a Dictionary, key: &[u8], depth: u32) -> Option<T> {
    let o: Option<T> = get(doc, dict, key);
    if let Some(o) = o {
        Some(o)
    } else {
        // LOCAL PATCH (QuickSearch): stop before the guard page. Treating a
        // too-deep chain as "absent" matches what this function already does
        // for a chain that simply ends.
        if depth >= MAX_PDF_RECURSION_DEPTH {
            return None;
        }
        let parent = dict.get(b"Parent")
            .and_then(|parent| parent.as_reference())
            .and_then(|id| doc.get_dictionary(id)).ok()?;
        get_inherited(doc, parent, key, depth + 1)
    }
}

pub fn output_doc_encrypted(
    doc: &mut Document,
    output: &mut dyn OutputDev,
    password: &str,
) -> Result<(), OutputError> {
    doc.decrypt(password)?;
    output_doc(doc, output)
}

/// Parse a given document and output it to `output`
pub fn output_doc(doc: &Document, output: &mut dyn OutputDev) -> Result<(), OutputError> {
    if doc.is_encrypted() {
        error!("Encrypted documents must be decrypted with a password using {{extract_text|extract_text_from_mem|output_doc}}_encrypted");
    }
    let empty_resources = Dictionary::new();
    let pages = doc.get_pages();
    let mut p = Processor::new();
    for dict in pages {
        let page_num = dict.0;
        let object_id = dict.1;
        output_doc_inner(page_num, object_id, doc, &mut p, output, &empty_resources)?;
    }
    Ok(())
}

pub fn output_doc_page(doc: &Document, output: &mut dyn OutputDev, page_num: u32) -> Result<(), OutputError> {
    if doc.is_encrypted() {
        error!("Encrypted documents must be decrypted with a password using {{extract_text|extract_text_from_mem|output_doc}}_encrypted");
    }
    let empty_resources = Dictionary::new();
    let pages = doc.get_pages();
    let object_id = pages.get(&page_num).ok_or(lopdf::Error::PageNumberNotFound(page_num))?;
    let mut p = Processor::new();
    output_doc_inner(page_num, *object_id, doc, &mut p, output, &empty_resources)?;
    Ok(())
}

fn output_doc_inner<'a>(page_num: u32, object_id: ObjectId, doc: &'a Document, p: & mut Processor<'a>, output: &mut dyn OutputDev, empty_resources: &'a Dictionary) -> Result<(), OutputError> {
    let page_dict = doc.get_object(object_id).unwrap().as_dict().unwrap();
    dlog!("page {} {:?}", page_num, page_dict);
    // XXX: Some pdfs lack a Resources directory
    let resources = get_inherited(doc, page_dict, b"Resources", 0).unwrap_or(empty_resources);
    dlog!("resources {:?}", resources);
    // pdfium searches up the page tree for MediaBoxes as needed
    let media_box: Vec<f64> = get_inherited(doc, page_dict, b"MediaBox", 0).expect("MediaBox");
    let media_box = MediaBox { llx: media_box[0], lly: media_box[1], urx: media_box[2], ury: media_box[3] };
    let art_box = get::<Option<Vec<f64>>>(&doc, page_dict, b"ArtBox")
        .map(|x| (x[0], x[1], x[2], x[3]));
    output.begin_page(page_num, &media_box, art_box)?;
    let content = p.page_content(doc, object_id)?;
    p.process_stream(&doc, content, resources, &media_box, output, page_num)?;
    output.end_page()?;
    Ok(())
}
