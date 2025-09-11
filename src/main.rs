use std::sync::Arc;
use dioxus::prelude::*;

mod frontend;
mod file_handling;
mod document_extraction;
mod indexing;

fn main() {
    // Launch the frontend with the indexing service
    launch(app);
}

fn app() -> Element {
    let indexing_service = Arc::new(indexing::IndexingService::new());
    
    rsx! {
        frontend::App {
            indexing_service: indexing_service
        }
    }
}

/*
SELECT name, hash, count(*) as cnt FROM files GROUP BY hash ORDER BY cnt DESC;

SELECT name, path, text, snippet(searchabletext, 2 , "<b>", "</b>", "...", 64) as "snip" FROM searchabletext WHERE text MATCH 'Terrasound' LIMIT 100;
*/