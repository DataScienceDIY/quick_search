use std::sync::{Arc, OnceLock};
use dioxus::prelude::*;
use quicksearch_core::{config, indexing, shutdown};
mod frontend;
mod search;

static INDEXING_SERVICE: OnceLock<Arc<indexing::IndexingService>> = OnceLock::new();

fn main() {
    let indexing_service = Arc::new(indexing::IndexingService::new());
    INDEXING_SERVICE
        .set(indexing_service.clone())
        .expect("Failed to set global indexing service");

    if let Err(e) = shutdown::install_signal_handler(indexing_service.clone()) {
        eprintln!("Warning: failed to install signal handler: {}", e);
    }

    LaunchBuilder::desktop()
        .with_cfg(
            dioxus_desktop::Config::new()
                .with_custom_head(format!("<style>{}</style>", include_str!("../assets/styles.css")))
                .with_window(dioxus_desktop::WindowBuilder::new()
                    .with_title("QuickSearch - File Indexer & Search")
                    .with_resizable(true)
                    .with_inner_size(dioxus_desktop::LogicalSize::new(1000.0, 700.0))
                )
        )
        .launch(app);
}


fn app() -> Element {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {}", e);
            return rsx! { div { "Failed to load configuration" } };
        }
    };

    let indexing_service = INDEXING_SERVICE
        .get()
        .expect("Indexing service not initialized")
        .clone();

    rsx! {
        frontend::App {
            indexing_service: indexing_service,
            config: cfg
        }
    }
}

