#![allow(non_snake_case)]

use std::sync::Arc;
use dioxus::prelude::*;
use crate::indexing::{IndexingService, IndexingStatus};

#[derive(Props, Clone)]
pub struct AppProps {
    pub indexing_service: Arc<IndexingService>,
}

impl PartialEq for AppProps {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.indexing_service, &other.indexing_service)
    }
}

pub fn App(props: AppProps) -> Element {
    let mut indexing_path = use_signal(|| "C:\\".to_string());
    let mut db_path = use_signal(|| "QuickSearch.db".to_string());
    let mut status_text = use_signal(|| "Idle".to_string());

    let indexing_service_for_start = props.indexing_service.clone();
    let indexing_service_for_stop = props.indexing_service.clone();
    let indexing_service_for_refresh = props.indexing_service.clone();
    let indexing_service_for_timer = props.indexing_service.clone();

    // Manual status refresh function
    let refresh_status = move |_| {
        let status = indexing_service_for_refresh.get_status();
        let status_str = match status {
            IndexingStatus::Idle => "Idle".to_string(),
            IndexingStatus::Running { files_processed, total_files, current_file, start_time } => {
                let elapsed = start_time.elapsed();
                let current_file_display = current_file
                    .as_ref()
                    .map(|f| format!("Current: {}", f.split('\\').last().unwrap_or(f)))
                    .unwrap_or_default();
                
                if let Some(total) = total_files {
                    let percentage = if total > 0 { 
                        (files_processed as f64 / total as f64 * 100.0) as u32 
                    } else { 0 };
                    format!(
                        "Running: {}/{} files ({}%) - {:.1}s elapsed\n{}",
                        files_processed,
                        total,
                        percentage,
                        elapsed.as_secs_f64(),
                        current_file_display
                    )
                } else {
                    format!(
                        "Running: {} files processed - {:.1}s elapsed\n{}",
                        files_processed,
                        elapsed.as_secs_f64(),
                        current_file_display
                    )
                }
            }
            IndexingStatus::Stopping => "Stopping...".to_string(),
            IndexingStatus::Error(ref e) => format!("Error: {}", e),
        };
        status_text.set(status_str);
    };

    // Automatic status updates every second
    {
        let mut status_text_clone = status_text.clone();
        let service_clone = indexing_service_for_timer.clone();
        use_future(move || {
            let service = service_clone.clone();
            async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    
                    let status = service.get_status();
                    let status_str = match status {
                        IndexingStatus::Idle => "Idle".to_string(),
                        IndexingStatus::Running { files_processed, total_files, current_file, start_time } => {
                            let elapsed = start_time.elapsed();
                            let current_file_display = current_file
                                .as_ref()
                                .map(|f| format!("Current: {}", f.split('\\').last().unwrap_or(f)))
                                .unwrap_or_default();
                            
                            if let Some(total) = total_files {
                                let percentage = if total > 0 { 
                                    (files_processed as f64 / total as f64 * 100.0) as u32 
                                } else { 0 };
                                format!(
                                    "Running: {}/{} files ({}%) - {:.1}s elapsed\n{}",
                                    files_processed,
                                    total,
                                    percentage,
                                    elapsed.as_secs_f64(),
                                    current_file_display
                                )
                            } else {
                                format!(
                                    "Running: {} files processed - {:.1}s elapsed\n{}",
                                    files_processed,
                                    elapsed.as_secs_f64(),
                                    current_file_display
                                )
                            }
                        }
                        IndexingStatus::Stopping => "Stopping...".to_string(),
                        IndexingStatus::Error(ref e) => format!("Error: {}", e),
                    };
                    status_text_clone.set(status_str);
                }
            }
        });
    }

    rsx! {
        div { 
            style: "padding: 20px; font-family: Arial, sans-serif;",
            
            h1 { "QuickSearch File Indexer" }
            
            div { 
                style: "margin-bottom: 20px;",
                h2 { "Indexing Controls" }
                
                div { 
                    style: "margin-bottom: 10px;",
                    label { 
                        style: "display: block; margin-bottom: 5px;",
                        "Path to index:" 
                    }
                    input { 
                        style: "width: 400px; padding: 5px;",
                        r#type: "text",
                        value: "{indexing_path}",
                        oninput: move |evt| indexing_path.set(evt.value())
                    }
                }
                
                div { 
                    style: "margin-bottom: 10px;",
                    label { 
                        style: "display: block; margin-bottom: 5px;",
                        "Database path:" 
                    }
                    input { 
                        style: "width: 400px; padding: 5px;",
                        r#type: "text",
                        value: "{db_path}",
                        oninput: move |evt| db_path.set(evt.value())
                    }
                }
                
                div {
                    style: "margin-bottom: 20px;",
                    button { 
                        style: "margin-right: 10px; padding: 10px 20px; background-color: #4CAF50; color: white; border: none; cursor: pointer;",
                        onclick: move |_| {
                            let _ = indexing_service_for_start.start_indexing(
                                indexing_path().clone(),
                                db_path().clone()
                            );
                        },
                        "Start Indexing"
                    }
                    button { 
                        style: "padding: 10px 20px; background-color: #f44336; color: white; border: none; cursor: pointer;",
                        onclick: move |_| {
                            let _ = indexing_service_for_stop.stop_indexing();
                        },
                        "Stop Indexing"
                    }
                    button { 
                        style: "margin-left: 10px; padding: 10px 20px; background-color: #2196F3; color: white; border: none; cursor: pointer;",
                        onclick: refresh_status,
                        "Refresh Status"
                    }
                }
            }
            
            div {
                h2 { "Status" }
                pre { 
                    style: "background-color: #f5f5f5; padding: 10px; border-radius: 5px; font-family: monospace;",
                    "{status_text}"
                }
            }
        }
    }
}