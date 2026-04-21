#![allow(non_snake_case)]

use std::sync::Arc;
use std::collections::VecDeque;
use std::time::Instant;
use dioxus::prelude::*;
use crate::indexing::{IndexingService, IndexingStatus};
use crate::config::Config;

#[derive(Debug, Clone)]
struct SpeedDataPoint {
    timestamp: Instant,
    files_processed: usize,
}

struct SpeedTracker {
    data_points: VecDeque<SpeedDataPoint>,
}

impl SpeedTracker {
    fn new() -> Self {
        Self {
            data_points: VecDeque::new(),
        }
    }

    fn add_data_point(&mut self, files_processed: usize) {
        let now = Instant::now();
        self.data_points.push_back(SpeedDataPoint {
            timestamp: now,
            files_processed,
        });
        
        // Prune data points older than 1 second
        while let Some(front) = self.data_points.front() {
            if now.duration_since(front.timestamp).as_secs_f64() > 1.0 {
                self.data_points.pop_front();
            } else {
                break;
            }
        }
    }

    fn calculate_files_per_second(&self) -> Option<f64> {
        if self.data_points.len() < 2 {
            return None;
        }

        let newest = self.data_points.back()?;
        let oldest = self.data_points.front()?;
        
        let time_span = newest.timestamp.duration_since(oldest.timestamp).as_secs_f64();
        if time_span < 0.1 { // Avoid division by very small numbers
            return None;
        }
        
        let files_diff = newest.files_processed.saturating_sub(oldest.files_processed);
        Some(files_diff as f64 / time_span)
    }
}

#[derive(Props, Clone)]
pub struct AppProps {
    pub indexing_service: Arc<IndexingService>,
    pub config: Config,
}

impl PartialEq for AppProps {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.indexing_service, &other.indexing_service) && self.config.paths.default_indexing_path == other.config.paths.default_indexing_path && self.config.paths.database_path == other.config.paths.database_path
    }
}

pub fn App(props: AppProps) -> Element {
    let mut indexing_path = use_signal(|| props.config.paths.default_indexing_path.clone());
    let mut db_path = use_signal(|| props.config.paths.database_path.clone());
    let mut status_text = use_signal(|| "Idle".to_string());
    let mut show_config_dialog = use_signal(|| false);
    let mut config_changes = use_signal(|| Vec::<String>::new());
    let speed_tracker = use_signal(|| SpeedTracker::new());
    

    let indexing_service_for_start = props.indexing_service.clone();
    let indexing_service_for_start_dialog = props.indexing_service.clone();
    let indexing_service_for_stop = props.indexing_service.clone();
    let indexing_service_for_timer = props.indexing_service.clone();
    let config_for_start = props.config.clone();
    let config_for_dialog = props.config.clone();


    // Automatic status updates every second
    {
        let mut status_text_clone = status_text.clone();
        let mut speed_tracker_clone = speed_tracker.clone();
        let service_clone = indexing_service_for_timer.clone();
        use_future(move || {
            let service = service_clone.clone();
            async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    
                    let status = service.get_status();
                    let status_str = match status {
                        IndexingStatus::Idle => {
                            // Reset speed tracker when idle
                            speed_tracker_clone.set(SpeedTracker::new());
                            "Idle".to_string()
                        },
                        IndexingStatus::CountingFiles {
                            current_file,
                            start_time,
                            ..
                        } => {
                            let elapsed = start_time.elapsed();
                            let current_file_display = current_file
                                .as_ref()
                                .map(|f| format!("{}", f))
                                .unwrap_or_else(|| "...".to_string());
                            format!(
                                "Phase 0 - Counting paths (shell) - {:.1}s elapsed\n{}",
                                elapsed.as_secs_f64(),
                                current_file_display
                            )
                        }
                        IndexingStatus::RunningFileIndex { files_processed, total_files, current_file, start_time } => {
                            // Add data point to speed tracker
                            speed_tracker_clone.with_mut(|tracker| {
                                tracker.add_data_point(files_processed);
                            });
                            
                            let elapsed = start_time.elapsed();
                            let current_file_display = current_file
                                .as_ref()
                                .map(|f| format!("Current: {}", f))
                                .unwrap_or_default();
                            
                            // Calculate speed
                            let speed_display = speed_tracker_clone.with(|tracker| {
                                tracker.calculate_files_per_second()
                                    .map(|fps| format!(" - {:.1} files/sec", fps))
                                    .unwrap_or_default()
                            });
                            
                            if let Some(total) = total_files {
                                let percentage = if total > 0 { 
                                    (files_processed as f64 / total as f64 * 100.0) as u32 
                                } else { 0 };
                                format!(
                                    "Phase 1 - File Index: {}/{} files ({}%) - {:.1}s elapsed{}\n{}",
                                    files_processed,
                                    total,
                                    percentage,
                                    elapsed.as_secs_f64(),
                                    speed_display,
                                    current_file_display
                                )
                            } else {
                                format!(
                                    "Phase 1 - File Index: {} files processed - {:.1}s elapsed{}\n{}",
                                    files_processed,
                                    elapsed.as_secs_f64(),
                                    speed_display,
                                    current_file_display
                                )
                            }
                        }
                        IndexingStatus::RunningTextIndex { files_processed, total_files, current_file, start_time } => {
                            // Add data point to speed tracker
                            speed_tracker_clone.with_mut(|tracker| {
                                tracker.add_data_point(files_processed);
                            });
                            
                            let elapsed = start_time.elapsed();
                            let current_file_display = current_file
                                .as_ref()
                                .map(|f| format!("Current: {}", f))
                                .unwrap_or_default();
                            
                            // Calculate speed
                            let speed_display = speed_tracker_clone.with(|tracker| {
                                tracker.calculate_files_per_second()
                                    .map(|fps| format!(" - {:.1} files/sec", fps))
                                    .unwrap_or_default()
                            });
                            
                            if let Some(total) = total_files {
                                let percentage = if total > 0 { 
                                    (files_processed as f64 / total as f64 * 100.0) as u32 
                                } else { 0 };
                                format!(
                                    "Phase 2 - Text Index: {}/{} files ({}%) - {:.1}s elapsed{}\n{}",
                                    files_processed,
                                    total,
                                    percentage,
                                    elapsed.as_secs_f64(),
                                    speed_display,
                                    current_file_display
                                )
                            } else {
                                format!(
                                    "Phase 2 - Text Index: {} files processed - {:.1}s elapsed{}\n{}",
                                    files_processed,
                                    elapsed.as_secs_f64(),
                                    speed_display,
                                    current_file_display
                                )
                            }
                        }
                        IndexingStatus::Stopping => "Indexing Stopped".to_string(),
                        IndexingStatus::Error(ref e) => format!("Error: {}", e),
                    };
                    status_text_clone.set(status_str);
                }
            }
        });
    }

    rsx! {
        div { 
            class: "app-container",
            
            div {
                class: "app-header",
                h1 { "QuickSearch File Indexer" }
            }
            
            div {
                class: "app-content",
            
            div { 
                class: "section",
                h2 { "Indexing Controls" }
                
                div { 
                    class: "form-group",
                    label { "Path to index:" }
                    input { 
                        class: "form-control",
                        r#type: "text",
                        value: "{indexing_path}",
                        oninput: move |evt| indexing_path.set(evt.value())
                    }
                }
                
                div { 
                    class: "form-group",
                    label { "Database path:" }
                    input { 
                        class: "form-control",
                        r#type: "text",
                        value: "{db_path}",
                        oninput: move |evt| db_path.set(evt.value())
                    }
                }
                
                div {
                    class: "form-group",
                    button { 
                        class: "btn btn-primary",
                        onclick: move |_| {
                            let service = indexing_service_for_start.clone();
                            let config = config_for_start.clone();
                            let path = indexing_path().clone();
                            let db = db_path().clone();
                            
                            // Check for configuration validation
                            match service.check_config_validation(&db, &config, &path) {
                                Ok(Some(changes)) => {
                                    // Configuration changes detected, show dialog
                                    config_changes.set(changes);
                                    show_config_dialog.set(true);
                                }
                                Ok(None) => {
                                    // No configuration issues, start indexing
                                    let _ = service.start_indexing(path, db, config);
                                }
                                Err(e) => {
                                    status_text.set(format!("Configuration validation error: {}", e));
                                }
                            }
                        },
                        "Start Indexing"
                    }
                    button { 
                        class: "btn btn-danger",
                        onclick: move |_| {
                            let _ = indexing_service_for_stop.stop_indexing();
                        },
                        "Stop Indexing"
                    }
                }
            }
            
            div {
                class: "section",
                h2 { "Status" }
                pre { 
                    class: "status-display",
                    "{status_text}"
                }
            }
            
            crate::search::Search {
                indexing_service: props.indexing_service.clone(),
                db_path: db_path().clone()
            }
            
            } // Close app-content
        }
        
        // Configuration validation dialog
        if show_config_dialog() {
            div {
                class: "modal-backdrop",
                div {
                    class: "modal-dialog",
                    h3 { 
                        style: "margin-top: 0; color: #d32f2f;",
                        "⚠️ Configuration Changes Detected"
                    }
                    p { 
                        style: "margin: 15px 0;",
                        "The following configuration changes require deleting and rebuilding the search index:"
                    }
                    ul {
                        style: "margin: 15px 0; padding-left: 20px;",
                        for change in config_changes().iter() {
                            li { 
                                style: "margin: 5px 0; font-family: monospace; background-color: #f5f5f5; padding: 5px; border-radius: 3px;",
                                "{change}"
                            }
                        }
                    }
                    p {
                        style: "margin: 15px 0; font-weight: bold;",
                        "This will delete the existing index and rebuild it from scratch."
                    }
                    div {
                        style: "display: flex; gap: 10px; margin-top: 20px;",
                        button {
                            style: "padding: 10px 20px; background-color: #d32f2f; color: white; border: none; border-radius: 5px; cursor: pointer;",
                            onclick: move |_| {
                                let service = indexing_service_for_start_dialog.clone();
                                let config = config_for_dialog.clone();
                                let path = indexing_path().clone();
                                let db = db_path().clone();
                                
                                show_config_dialog.set(false);
                                status_text.set("Stopping indexing and deleting database...".to_string());
                                
                                // Delete database file and restart indexing
                                let service_clone = service.clone();
                                let path_clone = path.clone();
                                let db_clone = db.clone();
                                let config_clone = config.clone();
                                let mut status_clone = status_text.clone();
                                
                                spawn(async move {
                                    match service_clone.delete_index_for_rebuild(&db_clone) {
                                        Ok(()) => {
                                            status_clone.set("Database deleted. Starting fresh indexing...".to_string());
                                            let _ = service_clone.start_indexing(path_clone, db_clone, config_clone);
                                        }
                                        Err(e) => {
                                            status_clone.set(format!("Error deleting database: {}", e));
                                        }
                                    }
                                });
                            },
                            "Yes, Rebuild Index"
                        }
                        button {
                            style: "padding: 10px 20px; background-color: #666; color: white; border: none; border-radius: 5px; cursor: pointer;",
                            onclick: move |_| {
                                show_config_dialog.set(false);
                            },
                            "Cancel"
                        }
                    }
                }
            }
        }
    }
}