use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;
use walkdir::WalkDir;
use rusqlite::{Connection, params};

use crate::file_handling::{load_existing_files, analyze_files_for_batch_update, process_batch_updates_files_only, process_batch_inserts_files_only, process_text_indexing};
use crate::config::Config;

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    RunningFileIndex {
        files_processed: usize,
        total_files: Option<usize>,
        current_file: Option<String>,
        start_time: Instant,
    },
    RunningTextIndex {
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
        config: Config,
    },
    Stop,
}

#[derive(Debug)]
pub struct IndexingService {
    status: Arc<Mutex<IndexingStatus>>,
    command_tx: mpsc::Sender<IndexingCommand>,
    db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    _handle: thread::JoinHandle<()>,
}

/// Set process priority for background operation
// fn set_background_priority() {
//     #[cfg(windows)]
//     {
//         use std::os::windows::raw::HANDLE;
        
//         // Windows implementation
//         extern "system" {
//             fn GetCurrentProcess() -> HANDLE;
//             fn SetPriorityClass(hprocess: HANDLE, dwpriorityclass: u32) -> i32;
//         }
        
//         const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
//         unsafe {
//             SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
//         }
//     }
    
//     #[cfg(unix)]
//     {
//         // Unix implementation  
//         use std::os::unix::process::CommandExt;
//         unsafe {
//             libc::nice(10); // Lower priority
//         }
//     }
// }

impl IndexingService {
    pub fn new() -> Self {
        let status = Arc::new(Mutex::new(IndexingStatus::Idle));
        let (command_tx, command_rx) = mpsc::channel();
        let db_connection = Arc::new(Mutex::new(None));
        
        let status_clone = status.clone();
        let db_connection_clone = db_connection.clone();
        let handle = thread::spawn(move || {
            Self::indexing_thread(status_clone, command_rx, db_connection_clone);
        });

        IndexingService {
            status,
            command_tx,
            db_connection,
            _handle: handle,
        }
    }

    pub fn start_indexing(&self, path: String, db_path: String, config: Config) -> Result<(), String> {
        self.command_tx
            .send(IndexingCommand::Start { path, db_path, config })
            .map_err(|e| format!("Failed to send start command: {}", e))
    }

    pub fn stop_indexing(&self) -> Result<(), String> {
        // First send the stop command
        self.command_tx
            .send(IndexingCommand::Stop)
            .map_err(|e| format!("Failed to send stop command: {}", e))?;

        // Wait for indexing to transition to stopping state
        let mut attempts = 0;
        while attempts < 50 { // Wait up to 5 seconds
            match self.get_status() {
                IndexingStatus::Stopping => break,
                IndexingStatus::Idle => return Ok(()), // Already stopped
                IndexingStatus::Error(_) => return Ok(()), // Consider error state as stopped
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
            }
        }

        // Flush and close database connection if it exists
        if let Ok(mut db_opt) = self.db_connection.lock() {
            if let Some(db_conn_arc) = db_opt.take() {
                if let Ok(conn) = db_conn_arc.lock() {
                    // Re-enable journal mode and synchronous writes for proper flushing
                    let _ = conn.execute_batch(
                        "PRAGMA journal_mode = DELETE;
                         PRAGMA synchronous = FULL;"
                    );
                    
                    // Force a checkpoint to flush any remaining WAL data
                    let _ = conn.execute("PRAGMA wal_checkpoint(FULL);", ());
                    
                    // Explicitly close the connection by dropping it
                    drop(conn);
                }
            }
        }

        Ok(())
    }

    pub fn get_status(&self) -> IndexingStatus {
        self.status.lock().unwrap().clone()
    }

    /// Force graceful shutdown - used for signal handling
    pub fn graceful_shutdown(&self) -> Result<(), String> {
        self.stop_indexing()
    }

    /// Check if configuration changes require index recreation
    pub fn check_config_validation(&self, db_path: &str, config: &Config, indexing_path: &str) -> Result<Option<Vec<String>>, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        // Create config validation table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS config_validation (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL);",
            (),
        ).map_err(|e| format!("Failed to create config_validation table: {}", e))?;

        Self::validate_config(&conn, config, indexing_path)
    }

    /// Stop indexing and delete the database file for a clean rebuild
    pub fn delete_index_for_rebuild(&self, db_path: &str) -> Result<(), String> {
        // Stop any running indexing first
        self.stop_indexing()
            .map_err(|e| format!("Failed to stop indexing: {}", e))?;

        // Wait for indexing to actually stop
        let mut attempts = 0;
        while attempts < 50 { // Wait up to 5 seconds
            match self.get_status() {
                IndexingStatus::Idle => break,
                IndexingStatus::Stopping | IndexingStatus::RunningFileIndex { .. } | IndexingStatus::RunningTextIndex { .. } => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    attempts += 1;
                }
                IndexingStatus::Error(_) => break, // Consider error state as stopped
            }
        }

        // Delete the database file
        if std::path::Path::new(db_path).exists() {
            std::fs::remove_file(db_path)
                .map_err(|e| format!("Failed to delete database file: {}", e))?;
        }

        Ok(())
    }

    fn indexing_thread(
        status: Arc<Mutex<IndexingStatus>>, 
        command_rx: mpsc::Receiver<IndexingCommand>,
        db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>
    ) {
        let stop_flag = Arc::new(Mutex::new(false));
        let mut indexing_handle: Option<thread::JoinHandle<()>> = None;
        
        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start { path, db_path, config } => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::RunningFileIndex { .. } | IndexingStatus::RunningTextIndex { .. }) {
                        continue; // Already running
                    }

                    // Join any previous indexing thread
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    *stop_flag.lock().unwrap() = false;
                    *status.lock().unwrap() = IndexingStatus::RunningFileIndex {
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
                    let config_owned = config.clone();
                    
                    let db_connection_clone = db_connection.clone();
                    indexing_handle = Some(thread::spawn(move || {
                        if let Err(e) = Self::run_indexing(&status_clone, &path_owned, &db_path_owned, &stop_flag_clone, &config_owned, &db_connection_clone) {
                            *status_clone.lock().unwrap() = IndexingStatus::Error(e);
                        } else {
                            // Only set to Idle if we weren't stopped
                            if !*stop_flag_clone.lock().unwrap() {
                                *status_clone.lock().unwrap() = IndexingStatus::Idle;
                            }
                        }
                        
                        // Clear the database connection when indexing completes
                        if let Ok(mut db_opt) = db_connection_clone.lock() {
                            *db_opt = None;
                        }
                    }));
                }
                IndexingCommand::Stop => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::RunningFileIndex { .. } | IndexingStatus::RunningTextIndex { .. }) {
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
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
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

        let create_fts_sql = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS searchabletext USING fts5 (name, path, text, tokenize = '{}');",
            config.processing.tokenize
        );
        conn.execute(&create_fts_sql, ())
            .map_err(|e| format!("Failed to create searchabletext table: {}", e))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS config_validation (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL);",
            (),
        )
        .map_err(|e| format!("Failed to create config_validation table: {}", e))?;

        // Update configuration (for new installations or when no validation issues)
        Self::update_config(&conn, config, path)?;

        // Load existing files from database for incremental indexing
        let existing_files = {
            let conn_ref = &conn;
            load_existing_files(conn_ref)
                .map_err(|e| format!("Failed to load existing files: {}", e))?
        };

        let conn_mutex = Arc::new(Mutex::new(conn));
        
        // Store the database connection for proper cleanup on stop
        if let Ok(mut db_opt) = db_connection.lock() {
            *db_opt = Some(conn_mutex.clone());
        }

        // Collect all file entries
        let walker = WalkDir::new(path).into_iter();
        let entries: Vec<_> = walker
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.metadata().map(|m| m.is_dir()).unwrap_or(true))
            .collect();

        let total_file_count = entries.len();

        // Update status with total file count
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::RunningFileIndex { ref mut total_files, .. } = *status_guard {
                *total_files = Some(total_file_count);
            }
        }

        // Analyze which files need updates vs inserts
        let batch_update = analyze_files_for_batch_update(&entries, &existing_files);
        
        let total_work = batch_update.files_to_update.len() + batch_update.files_to_insert.len();
        
        // Update status to show actual work needed
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::RunningFileIndex { ref mut total_files, .. } = *status_guard {
                *total_files = Some(total_work);
            }
        }

        let mut work_completed = 0;

        // Process updated files in batches
        if !batch_update.files_to_update.is_empty() {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard {
                    *current_file = Some(format!("Updating {} modified files...", batch_update.files_to_update.len()));
                }
            }

            // Create status callback to update current file
            let status_clone_1 = status.clone();
            let status_callback = Box::new(move |file_status: &str| {
                if let Ok(mut status_guard) = status_clone_1.lock() {
                    if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard {
                        *current_file = Some(file_status.to_string());
                    }
                }
            });
            
            // Create progress callback to update files_processed
            let status_clone_2 = status.clone();
            let base_work_completed = work_completed;
            let progress_callback = Box::new(move |current_index: usize| {
                if let Ok(mut status_guard) = status_clone_2.lock() {
                    if let IndexingStatus::RunningFileIndex { ref mut files_processed, .. } = *status_guard {
                        *files_processed = base_work_completed + current_index;
                    }
                }
            });

            if let Err(e) = process_batch_updates_files_only(&conn_mutex, &batch_update.files_to_update, &stop_flag, Some(status_callback), Some(progress_callback), config) {
                return Err(format!("Failed to process batch updates: {}", e));
            }
            
            work_completed += batch_update.files_to_update.len();
            
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut files_processed, .. } = *status_guard {
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
                if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard {
                    *current_file = Some(format!("Indexing {} new files...", batch_update.files_to_insert.len()));
                }
            }

            // Create status callback for inserts
            let status_clone_3 = status.clone();
            let status_callback = Box::new(move |file_status: &str| {
                if let Ok(mut status_guard) = status_clone_3.lock() {
                    if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard {
                        *current_file = Some(file_status.to_string());
                    }
                }
            });
            
            // Create progress callback for inserts
            let status_clone_4 = status.clone();
            let base_work_completed = work_completed;
            let progress_callback = Box::new(move |current_index: usize| {
                if let Ok(mut status_guard) = status_clone_4.lock() {
                    if let IndexingStatus::RunningFileIndex { ref mut files_processed, .. } = *status_guard {
                        *files_processed = base_work_completed + current_index;
                    }
                }
            });

            if let Err(e) = process_batch_inserts_files_only(&conn_mutex, &batch_update.files_to_insert, &stop_flag, Some(status_callback), Some(progress_callback), config) {
                return Err(format!("Failed to process batch inserts: {}", e));
            }
            
            work_completed += batch_update.files_to_insert.len();
            
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut files_processed, .. } = *status_guard {
                    *files_processed = work_completed;
                }
            }
        }

        // If no incremental work was needed, show completion status for file indexing phase
        if total_work == 0 {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut current_file, ref mut files_processed, .. } = *status_guard {
                    *current_file = Some("File index is up to date".to_string());
                    *files_processed = total_file_count;
                }
            }
        }

        // Check for stop signal before starting text indexing
        if *stop_flag.lock().unwrap() {
            if let Ok(mut status_guard) = status.lock() {
                *status_guard = IndexingStatus::Idle;
            }
            return Ok(());
        }

        // Phase 2: Text indexing
        if let Ok(mut status_guard) = status.lock() {
            *status_guard = IndexingStatus::RunningTextIndex {
                files_processed: 0,
                total_files: None,
                current_file: Some("Starting text indexing...".to_string()),
                start_time: Instant::now(),
            };
        }

        // Create status callback for text indexing
        let status_clone_5 = status.clone();
        let text_status_callback = Box::new(move |file_status: &str| {
            if let Ok(mut status_guard) = status_clone_5.lock() {
                if let IndexingStatus::RunningTextIndex { ref mut current_file, .. } = *status_guard {
                    *current_file = Some(file_status.to_string());
                }
            }
        });
        
        // Create progress callback for text indexing
        let status_clone_6 = status.clone();
        let text_progress_callback = Box::new(move |current_index: usize| {
            if let Ok(mut status_guard) = status_clone_6.lock() {
                if let IndexingStatus::RunningTextIndex { ref mut files_processed, .. } = *status_guard {
                    *files_processed = current_index;
                }
            }
        });

        // Process text indexing
        if let Err(e) = process_text_indexing(&conn_mutex, &stop_flag, Some(text_status_callback), Some(text_progress_callback), config) {
            return Err(format!("Failed to process text indexing: {}", e));
        }

        // Mark text indexing as complete
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::RunningTextIndex { ref mut current_file, .. } = *status_guard {
                *current_file = Some("Text indexing complete".to_string());
            }
        }

        Ok(())
    }

    /// Validates configuration against stored values and returns validation results.
    /// Critical configuration changes that require index recreation:
    /// - hash_length: affects file hash computation, invalidates existing file metadata
    /// - indexing_path: changes the scope of indexed files
    /// - tokenize: changes FTS5 tokenization, invalidates text search index
    fn validate_config(conn: &Connection, config: &Config, indexing_path: &str) -> Result<Option<Vec<String>>, String> {
        // Critical configuration values that require index recreation
        let hash_length = config.processing.hash_length.to_string();
        let tokenize = config.processing.tokenize.clone();
        let normalized_path = std::path::Path::new(indexing_path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(indexing_path))
            .to_string_lossy()
            .to_string();
        
        // Check stored configuration values
        let mut stored_hash_length: Option<String> = None;
        let mut stored_indexing_path: Option<String> = None;
        let mut stored_tokenize: Option<String> = None;
        
        if let Ok(mut stmt) = conn.prepare("SELECT key, value FROM config_validation WHERE key IN ('hash_length', 'indexing_path', 'tokenize')") {
            if let Ok(rows) = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            }) {
                for row in rows.flatten() {
                    match row.0.as_str() {
                        "hash_length" => stored_hash_length = Some(row.1),
                        "indexing_path" => stored_indexing_path = Some(row.1),
                        "tokenize" => stored_tokenize = Some(row.1),
                        _ => {}
                    }
                }
            }
        }
        
        // Check if configuration is invalid
        let hash_length_changed = stored_hash_length.as_ref().map_or(false, |stored| stored != &hash_length);
        let indexing_path_changed = stored_indexing_path.as_ref().map_or(false, |stored| stored != &normalized_path);
        let tokenize_changed = stored_tokenize.as_ref().map_or(false, |stored| stored != &tokenize);
        
        if hash_length_changed || indexing_path_changed || tokenize_changed {
            let mut changes = Vec::new();
            if hash_length_changed {
                changes.push(format!("hash_length: {} -> {}", 
                    stored_hash_length.unwrap_or_else(|| "unknown".to_string()), hash_length));
            }
            if indexing_path_changed {
                changes.push(format!("indexing_path: {} -> {}", 
                    stored_indexing_path.unwrap_or_else(|| "unknown".to_string()), normalized_path));
            }
            if tokenize_changed {
                changes.push(format!("tokenize: {} -> {}", 
                    stored_tokenize.unwrap_or_else(|| "unknown".to_string()), tokenize));
            }
            
            return Ok(Some(changes));
        }
        
        // No configuration changes detected
        Ok(None)
    }


    /// Updates stored configuration values without clearing the index
    fn update_config(conn: &Connection, config: &Config, indexing_path: &str) -> Result<(), String> {
        let hash_length = config.processing.hash_length.to_string();
        let tokenize = config.processing.tokenize.clone();
        let normalized_path = std::path::Path::new(indexing_path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(indexing_path))
            .to_string_lossy()
            .to_string();

        // Update stored configuration values
        conn.execute(
            "INSERT OR REPLACE INTO config_validation (key, value) VALUES ('hash_length', ?1)",
            params![hash_length],
        ).map_err(|e| format!("Failed to store hash_length config: {}", e))?;
        
        conn.execute(
            "INSERT OR REPLACE INTO config_validation (key, value) VALUES ('indexing_path', ?1)",
            params![normalized_path],
        ).map_err(|e| format!("Failed to store indexing_path config: {}", e))?;
        
        conn.execute(
            "INSERT OR REPLACE INTO config_validation (key, value) VALUES ('tokenize', ?1)",
            params![tokenize],
        ).map_err(|e| format!("Failed to store tokenize config: {}", e))?;

        Ok(())
    }
}

impl Drop for IndexingService {
    fn drop(&mut self) {
        // Ensure graceful shutdown when the service is dropped
        let _ = self.stop_indexing();
    }
}

impl Default for IndexingService {
    fn default() -> Self {
        Self::new()
    }
}

