//! Print what the legacy-Office extractor gets out of a real file.
//!
//! The unit tests prove the parsers agree with the format specs as read, on
//! fixtures they build; this runs them against files a real producer wrote.
//!
//!     cargo run --example oleprobe -- some.doc some.xls some.ppt

use std::io::Read;
use std::path::Path;

use quicksearch_core::config::Config;
use quicksearch_core::extract::{Registry, Scratch};
use quicksearch_core::mime::guess_mime_from_head;

fn main() {
    let mut failures = 0;
    let config = Config::default();
    let registry = Registry::default_set();
    let mut scratch = Scratch::new(&config);
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        println!("=== {} ===", path.display());
        // The dispatch a real index takes: the MIME sniffed from the name and
        // the head the walk reads.
        let mut head = Vec::new();
        let opened = std::fs::File::open(path).and_then(|f| {
            f.take(config.processing.hash_length as u64)
                .read_to_end(&mut head)
        });
        if let Err(e) = opened {
            failures += 1;
            println!("FAILED: {}", e);
            println!();
            continue;
        }
        let mime = guess_mime_from_head(path, &head).unwrap_or_default();
        println!("(mime {})", mime);
        let mut text = String::new();
        match registry.extract(path, mime, &mut text, &mut scratch) {
            Ok(true) => {
                println!("{} chars", text.chars().count());
                let preview: String = text.chars().take(400).collect();
                println!("{}", preview);
            }
            Ok(false) => {
                failures += 1;
                println!("UNCLAIMED: no extractor claims {:?}", mime);
            }
            Err(e) => {
                failures += 1;
                println!("FAILED: {}", e);
            }
        }
        println!();
    }
    std::process::exit(if failures > 0 { 1 } else { 0 });
}
