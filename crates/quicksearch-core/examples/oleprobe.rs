//! Print what the legacy-Office extractor gets out of a real file.
//!
//! The unit tests build their own fixtures, which proves the parsers agree
//! with the format specs as read. This runs them against files a real
//! producer wrote, which is the other half of the question.
//!
//!     cargo run --example oleprobe -- some.doc some.xls some.ppt

use std::path::Path;

use quicksearch_core::extract::{Extractor, Registry};

fn main() {
    let mut failures = 0;
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        println!("=== {} ===", path.display());
        match quicksearch_core::extract::office::OfficeExtractor.extract(path) {
            Ok(content) => {
                let text = content.text;
                println!("{} chars", text.chars().count());
                let preview: String = text.chars().take(400).collect();
                println!("{}", preview);
            }
            Err(e) => {
                failures += 1;
                println!("FAILED: {}", e);
            }
        }
        // The dispatch a real index would take: MIME, not extension.
        let registry = Registry::default_set();
        let mime = mime_guess::from_path(path)
            .first()
            .map(|m| m.essence_str().to_string())
            .unwrap_or_default();
        println!("(mime {} claimed: {})", mime, registry.supports(&mime));
        println!();
    }
    std::process::exit(if failures > 0 { 1 } else { 0 });
}
