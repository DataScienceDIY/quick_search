//! Print what the legacy-Office extractor gets out of a real file.
//!
//! The unit tests build their own fixtures, which proves the parsers agree
//! with the format specs as read. This runs them against files a real
//! producer wrote, which is the other half of the question.
//!
//!     cargo run --example oleprobe -- some.doc some.xls some.ppt

use std::path::Path;

use quicksearch_core::config::Config;
use quicksearch_core::extract::{Extractor, Registry, Scratch};

fn main() {
    let mut failures = 0;
    // The extractor is called directly, by extension, so a file whose MIME
    // the sniff would get wrong still says what the parser makes of it.
    let config = Config::default();
    let mut scratch = Scratch::new(&config);
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        println!("=== {} ===", path.display());
        let mut text = String::new();
        match quicksearch_core::extract::office::OfficeExtractor
            .extract(path, &mut text, &mut scratch)
        {
            Ok(()) => {
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
