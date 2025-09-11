use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;
use walkdir::WalkDir;
use rusqlite::Connection;

use crate::file_handling::{load_existing_files, analyze_files_for_batch_update, process_batch_updates, process_batch_inserts};

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    Running {
        files_processed: usize,
        total_files: Option<usize>,
        current_file: Option<String>,
        start_time: Instant,
    },
    Stopping,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum IndexingCommand {
    Start {
        path: String,
        db_path: String,
    },
    Stop,
}

pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
    command_tx: mpsc::Sender<IndexingCommand>,
    _handle: thread::JoinHandle<()>,
}

impl IndexingService {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(IndexingStatus::Idle));
        let (command_tx, command_rx) = mpsc::channel();
        
        let status_clone = status.clone();
        let handle = thread::spawn(move || {
            Self::indexing_thread(status_clone, command_rx);
        });

        IndexingService {
            status,
            command_tx,
            _handle: handle,
        }
    }

    pub fn start_indexing(&self, path: String, db_path: String) -> Result<(), String> {
        self.command_tx
            .send(IndexingCommand::Start { path, db_path })
            .map_err(|e| format!("Failed to send start command: {}", e))
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        self.command_tx
            .send(IndexingCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))
    }

    pub fn get_status(&self) -> IndexingStatus {
        self.status.lock().unwrap().clone()
    }

    fn indexing_thread(status: Arc<Mutex<IndexingStatus>>, command_rx: mpsc::Receiver<IndexingCommand>) {
        let stop_flag = Arc::new(Mutex::new(false));
        let mut indexing_handle: Option<thread::JoinHandle<()>> = None;
        
        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start { path, db_path } => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        continue; // Already running
                    }

                    // Join any previous indexing thread
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    *stop_flag.lock().unwrap() = false;
                    *status.lock().unwrap() = IndexingStatus::Running {
                        files_processed: 0,
                        total_files: None,
                        current_file: None,
                        start_time: Instant::now(),
                    };

                    // Run indexing in a separate thread
                    let status_clone = status.clone();
                    let stop_flag_clone = stop_flag.clone();
                    let path_owned = path.clone();
                    let db_path_owned = db_path.clone();
                    
                    indexing_handle = Some(thread::spawn(move || {
                        if let Err(e) = Self::run_indexing(&status_clone, &path_owned, &db_path_owned, &stop_flag_clone) {
                            *status_clone.lock().unwrap() = IndexingStatus::Error(e);
                        } else {
                            // Only set to Idle if we weren't stopped
                            if !*stop_flag_clone.lock().unwrap() {
                                *status_clone.lock().unwrap() = IndexingStatus::Idle;
                            }
                        }
                    }));
                }
                IndexingCommand::Stop => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        *status.lock().unwrap() = IndexingStatus::Stopping;
                        *stop_flag.lock().unwrap() = true;
                    }
                }
            }
        }
        
        // Clean up any remaining indexing thread
        if let Some(handle) = indexing_handle {
            let _ = handle.join();
        }
    }

    fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        path: &str,
        db_path: &str,
        stop_flag: &Arc<Mutex<bool>>,
    ) -> Result<(), String> {
        // Set up database
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        conn.execute_batch(
            "PRAGMA journal_mode = OFF;
             PRAGMA synchronous = 0;
             PRAGMA cache_size = 10000;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(|e| format!("Failed to set PRAGMA: {}", e))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                name    TEXT,
                path    TEXT,
                size    INTEGER,
                moddate INTEGER,
                hash    BLOB);",
            (),
        )
        .map_err(|e| format!("Failed to create files table: {}", e))?;

        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS searchabletext USING fts5 (name, path, text, tokenize = 'trigram');",
            (),
        )
        .map_err(|e| format!("Failed to create searchabletext table: {}", e))?;

        // Load existing files from database for incremental indexing
        let existing_files = {
            let conn_ref = &conn;
            load_existing_files(conn_ref)
                .map_err(|e| format!("Failed to load existing files: {}", e))?
        };

        let conn_mutex = Arc::new(Mutex::new(conn));

        // Collect all file entries
        let walker = WalkDir::new(path).into_iter();
        let entries: Vec<_> = walker
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.metadata().map(|m| m.is_dir()).unwrap_or(true))
            .collect();

        let total_file_count = entries.len();

        // Update status with total file count
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::Running { ref mut total_files, .. } = *status_guard {
                *total_files = Some(total_file_count);
            }
        }

        // Analyze which files need updates vs inserts
        let batch_update = analyze_files_for_batch_update(&entries, &existing_files);
        
        let total_work = batch_update.files_to_update.len() + batch_update.files_to_insert.len();
        
        // Update status to show actual work needed
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::Running { ref mut total_files, .. } = *status_guard {
                *total_files = Some(total_work);
            }
        }

        let mut work_completed = 0;

        // Process updated files in batches
        if !batch_update.files_to_update.is_empty() {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut current_file, .. } = *status_guard {
                    *current_file = Some(format!("Updating {} modified files...", batch_update.files_to_update.len()));
                }
            }

            // Create status callback to update current file and progress
            let status_clone = status.clone();
            let status_callback = Box::new(move |file_status: &str| {
                if let Ok(mut status_guard) = status_clone.lock() {
                    if let IndexingStatus::Running { ref mut current_file, ref mut files_processed, .. } = *status_guard {
                        *current_file = Some(file_status.to_string());
                        // Extract the current count from the status string
                        if let Some(slash_pos) = file_status.find('/') {
                            if let Some(space_pos) = file_status.rfind(' ') {
                                if space_pos + 1 < slash_pos {
                                    if let Ok(current) = file_status[space_pos + 1..slash_pos].parse::<usize>() {
                                        *files_processed = work_completed + current;
                                    }
                                }
                            }
                        }
                    }
                }
            });

            if let Err(e) = process_batch_updates(&conn_mutex, &batch_update.files_to_update, &stop_flag, Some(status_callback)) {
                return Err(format!("Failed to process batch updates: {}", e));
            }
            
            work_completed += batch_update.files_to_update.len();
            
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut files_processed, .. } = *status_guard {
                    *files_processed = work_completed;
                }
            }
        }

        // Check for stop signal
        if *stop_flag.lock().unwrap() {
            if let Ok(mut status_guard) = status.lock() {
                *status_guard = IndexingStatus::Idle;
            }
            return Ok(());
        }

        // Process new files in batches
        if !batch_update.files_to_insert.is_empty() {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut current_file, .. } = *status_guard {
                    *current_file = Some(format!("Indexing {} new files...", batch_update.files_to_insert.len()));
                }
            }

            // Create status callback to update current file and progress
            let status_clone = status.clone();
            let base_work_completed = work_completed;
            let status_callback = Box::new(move |file_status: &str| {
                if let Ok(mut status_guard) = status_clone.lock() {
                    if let IndexingStatus::Running { ref mut current_file, ref mut files_processed, .. } = *status_guard {
                        *current_file = Some(file_status.to_string());
                        // Extract the current count from the status string
                        if let Some(slash_pos) = file_status.find('/') {
                            if let Some(space_pos) = file_status.rfind(' ') {
                                if space_pos + 1 < slash_pos {
                                    if let Ok(current) = file_status[space_pos + 1..slash_pos].parse::<usize>() {
                                        *files_processed = base_work_completed + current;
                                    }
                                }
                            }
                        }
                    }
                }
            });

            if let Err(e) = process_batch_inserts(&conn_mutex, &batch_update.files_to_insert, &stop_flag, Some(status_callback)) {
                return Err(format!("Failed to process batch inserts: {}", e));
            }
            
            work_completed += batch_update.files_to_insert.len();
            
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut files_processed, .. } = *status_guard {
                    *files_processed = work_completed;
                }
            }
        }

        // If no incremental work was needed, show completion status
        if total_work == 0 {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut current_file, ref mut files_processed, .. } = *status_guard {
                    *current_file = Some("Index is up to date".to_string());
                    *files_processed = total_file_count;
                }
            }
        }

        Ok(())
    }
}

impl Default for IndexingService {
    fn default() -> Self {
        Self::new()
    }
}

