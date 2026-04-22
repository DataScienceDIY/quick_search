use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, BufReader};

use zip::ZipArchive;
use quick_xml::Reader;
use quick_xml::events::Event;

/// Extract text from DOCX files by parsing the word/document.xml
pub fn extract_text_from_docx(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut document_xml = archive.by_name("word/document.xml")?;
    let mut content = String::new();
    document_xml.read_to_string(&mut content)?;
    
    let mut reader = Reader::from_str(&content);
    reader.trim_text(true);
    
    let mut text_content = String::new();
    let mut buf = Vec::new();
    let mut in_text = false;
    
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == b"w:t" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) if in_text => {
                text_content.push_str(&e.unescape()?.into_owned());
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"w:t" {
                    in_text = false;
                } else if e.name().as_ref() == b"w:p" {
                    text_content.push('\n');
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("Error parsing XML: {}", e).into()),
            _ => {}
        }
        buf.clear();
    }
    
    Ok(text_content)
}

/// Extract text from XLSX files by parsing worksheet XML files
pub fn extract_text_from_xlsx(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut text_content = String::new();
    
    // First, read shared strings if they exist
    let mut shared_strings = Vec::new();
    if let Ok(mut shared_strings_xml) = archive.by_name("xl/sharedStrings.xml") {
        let mut content = String::new();
        shared_strings_xml.read_to_string(&mut content)?;
        
        let mut reader = Reader::from_str(&content);
        reader.trim_text(true);
        let mut buf = Vec::new();
        let mut in_text = false;
        
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    if e.name().as_ref() == b"t" {
                        in_text = true;
                    }
                }
                Ok(Event::Text(e)) if in_text => {
                    shared_strings.push(e.unescape()?.into_owned());
                }
                Ok(Event::End(ref e)) => {
                    if e.name().as_ref() == b"t" {
                        in_text = false;
                    }
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }
    }
    
    // Read worksheets
    for i in 0..archive.len() {
        let file_name = archive.by_index(i)?.name().to_string();
        if file_name.starts_with("xl/worksheets/sheet") && file_name.ends_with(".xml") {
            let mut sheet_xml = archive.by_index(i)?;
            let mut content = String::new();
            sheet_xml.read_to_string(&mut content)?;
            
            let mut reader = Reader::from_str(&content);
            reader.trim_text(true);
            let mut buf = Vec::new();
            let mut in_cell = false;
            let mut cell_type = String::new();
            
            loop {
                match reader.read_event_into(&mut buf) {
                    Ok(Event::Start(ref e)) => {
                        if e.name().as_ref() == b"c" {
                            in_cell = true;
                            cell_type.clear();
                            for attr in e.attributes() {
                                let attr = attr?;
                                if attr.key.as_ref() == b"t" {
                                    cell_type = String::from_utf8_lossy(&attr.value).to_string();
                                }
                            }
                        } else if e.name().as_ref() == b"v" && in_cell {
                            // Value element
                        }
                    }
                    Ok(Event::Text(e)) if in_cell => {
                        let text = e.unescape()?.into_owned();
                        if cell_type == "s" {
                            // Shared string reference
                            if let Ok(index) = text.parse::<usize>() {
                                if index < shared_strings.len() {
                                    text_content.push_str(&shared_strings[index]);
                                    text_content.push(' ');
                                }
                            }
                        } else {
                            text_content.push_str(&text);
                            text_content.push(' ');
                        }
                    }
                    Ok(Event::End(ref e)) => {
                        if e.name().as_ref() == b"c" {
                            in_cell = false;
                        } else if e.name().as_ref() == b"row" {
                            text_content.push('\n');
                        }
                    }
                    Ok(Event::Eof) => break,
                    _ => {}
                }
                buf.clear();
            }
        }
    }
    
    Ok(text_content)
}

/// Extract text from PPTX files by parsing slide XML files
pub fn extract_text_from_pptx(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut text_content = String::new();
    
    // Read all slide files
    for i in 0..archive.len() {
        let file_name = archive.by_index(i)?.name().to_string();
        if file_name.starts_with("ppt/slides/slide") && file_name.ends_with(".xml") {
            let mut slide_xml = archive.by_index(i)?;
            let mut content = String::new();
            slide_xml.read_to_string(&mut content)?;
            
            let mut reader = Reader::from_str(&content);
            reader.trim_text(true);
            let mut buf = Vec::new();
            let mut in_text = false;
            
            loop {
                match reader.read_event_into(&mut buf) {
                    Ok(Event::Start(ref e)) => {
                        if e.name().as_ref() == b"a:t" {
                            in_text = true;
                        }
                    }
                    Ok(Event::Text(e)) if in_text => {
                        text_content.push_str(&e.unescape()?.into_owned());
                    }
                    Ok(Event::End(ref e)) => {
                        if e.name().as_ref() == b"a:t" {
                            in_text = false;
                        } else if e.name().as_ref() == b"a:p" {
                            text_content.push('\n');
                        }
                    }
                    Ok(Event::Eof) => break,
                    _ => {}
                }
                buf.clear();
            }
            text_content.push_str("\n--- New Slide ---\n");
        }
    }
    
    Ok(text_content)
}

/// Extract text from ODT files (OpenDocument Text)
pub fn extract_text_from_odt(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut content_xml = archive.by_name("content.xml")?;
    let mut content = String::new();
    content_xml.read_to_string(&mut content)?;
    
    let mut reader = Reader::from_str(&content);
    reader.trim_text(true);
    
    let mut text_content = String::new();
    let mut buf = Vec::new();
    let mut in_text = false;
    
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" || name.as_ref() == b"text:h" || name.as_ref() == b"text:span" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) if in_text => {
                text_content.push_str(&e.unescape()?.into_owned());
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" || name.as_ref() == b"text:h" {
                    text_content.push('\n');
                    in_text = false;
                } else if name.as_ref() == b"text:span" {
                    in_text = false;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    
    Ok(text_content)
}

/// Extract text from ODP files (OpenDocument Presentation)
pub fn extract_text_from_odp(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut content_xml = archive.by_name("content.xml")?;
    let mut content = String::new();
    content_xml.read_to_string(&mut content)?;
    
    let mut reader = Reader::from_str(&content);
    reader.trim_text(true);
    
    let mut text_content = String::new();
    let mut buf = Vec::new();
    let mut in_text = false;
    
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" || name.as_ref() == b"text:h" || name.as_ref() == b"text:span" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) if in_text => {
                text_content.push_str(&e.unescape()?.into_owned());
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" || name.as_ref() == b"text:h" {
                    text_content.push('\n');
                    in_text = false;
                } else if name.as_ref() == b"text:span" {
                    in_text = false;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    
    Ok(text_content)
}

/// Extract text from ODS files (OpenDocument Spreadsheet)
pub fn extract_text_from_ods(file_path: &OsString) -> Result<String, Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    
    let mut content_xml = archive.by_name("content.xml")?;
    let mut content = String::new();
    content_xml.read_to_string(&mut content)?;
    
    let mut reader = Reader::from_str(&content);
    reader.trim_text(true);
    
    let mut text_content = String::new();
    let mut buf = Vec::new();
    let mut in_text = false;
    
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" || name.as_ref() == b"text:span" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) if in_text => {
                text_content.push_str(&e.unescape()?.into_owned());
                text_content.push(' ');
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                if name.as_ref() == b"text:p" {
                    text_content.push('\n');
                    in_text = false;
                } else if name.as_ref() == b"text:span" {
                    in_text = false;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    
    Ok(text_content)
}

/// Extract text from various document formats
pub fn extract_document_text(file_path: &OsString, extension: &str) -> Result<String, Box<dyn std::error::Error>> {
    match extension {
        "docx" => extract_text_from_docx(file_path),
        "doc" => {
            // DOC format is binary and complex to parse without external tools
            // For now, return an empty result
            Ok(String::new())
        }
        "xlsx" => extract_text_from_xlsx(file_path),
        "xls" => {
            // XLS format is binary and complex to parse without external tools
            Ok(String::new())
        }
        "pptx" => extract_text_from_pptx(file_path),
        "ppt" => {
            // PPT format is binary and complex to parse without external tools
            Ok(String::new())
        }
        "odt" => extract_text_from_odt(file_path),
        "odp" => extract_text_from_odp(file_path),
        "ods" => extract_text_from_ods(file_path),
        _ => Ok(String::new())
    }
}
