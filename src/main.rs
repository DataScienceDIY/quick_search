use std::sync::{Arc, OnceLock};
use dioxus::prelude::*;

mod frontend;
mod file_handling;
mod document_extraction;
mod indexing;
mod config;

// Global indexing service for signal handling
static INDEXING_SERVICE: OnceLock<Arc<indexing::IndexingService>> = OnceLock::new();

fn main() {
    // Initialize global indexing service
    let indexing_service = Arc::new(indexing::IndexingService::new());
    INDEXING_SERVICE.set(indexing_service.clone()).expect("Failed to set global indexing service");
    
    // Set up Ctrl-C signal handler
    ctrlc::set_handler(|| {
        eprintln!("Received Ctrl-C, shutting down gracefully...");
        if let Some(service) = INDEXING_SERVICE.get() {
            if let Err(e) = service.graceful_shutdown() {
                eprintln!("Error during graceful shutdown: {}", e);
            }
        }
        std::process::exit(0);
    }).expect("Error setting Ctrl-C handler");
    
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
    
    let indexing_service = INDEXING_SERVICE.get().expect("Indexing service not initialized").clone();
    
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