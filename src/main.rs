use std::sync::Arc;
use dioxus::prelude::*;

mod frontend;
mod file_handling;
mod document_extraction;
mod indexing;
mod config;

fn main() {
    launch(app);
}

fn app() -> Element {
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed to load config: {}", e);
            return rsx! { div { "Failed to load configuration" } };
        }
    };
    
    let indexing_service = Arc::new(indexing::IndexingService::new());
    
    rsx! {
        frontend::App {
            indexing_service: indexing_service,
            config: config
        }
    }
}

/*
SELECT name, hash, count(*) as cnt FROM files GROUP BY hash ORDER BY cnt DESC;

SELECT name, path, text, snippet(searchabletext, 2 , "<b>", "</b>", "...", 64) as "snip" FROM searchabletext WHERE text MATCH 'Terrasound' LIMIT 100;
*/