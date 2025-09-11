use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;
use rusqlite::Connection;

use crate::file_handling;

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
        
        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start { path, db_path } => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        continue; // Already running
                    }

                    *stop_flag.lock().unwrap() = false;
                    *status.lock().unwrap() = IndexingStatus::Running {
                        files_processed: 0,
                        total_files: None,
                        current_file: None,
                        start_time: Instant::now(),
                    };

                    // Run indexing
                    if let Err(e) = Self::run_indexing(&status, &path, &db_path, &stop_flag) {
                        *status.lock().unwrap() = IndexingStatus::Error(e);
                    } else {
                        *status.lock().unwrap() = IndexingStatus::Idle;
                    }
                }
                IndexingCommand::Stop => {
                    if matches!(*status.lock().unwrap(), IndexingStatus::Running { .. }) {
                        *status.lock().unwrap() = IndexingStatus::Stopping;
                        *stop_flag.lock().unwrap() = true;
                    }
                }
            }
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

        let conn_mutex = Arc::new(Mutex::new(conn));

        // Count total files for progress tracking
        let total_file_count = WalkDir::new(path)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.metadata().map(|m| m.is_dir()).unwrap_or(true))
            .count();

        // Update status with total file count
        if let Ok(mut status_guard) = status.lock() {
            if let IndexingStatus::Running { ref mut total_files, .. } = *status_guard {
                *total_files = Some(total_file_count);
            }
        }


        // Process files with periodic status updates
        let walker = WalkDir::new(path).into_iter();
        
        // Use a custom parallel iterator that checks for stop condition
        let entries: Vec<_> = walker.filter_map(|entry| entry.ok()).collect();
        
        for (i, entry) in entries.iter().enumerate() {
            if *stop_flag.lock().unwrap() {
                return Ok(());
            }

            // Update current file in status
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::Running { ref mut current_file, ref mut files_processed, .. } = *status_guard {
                    *current_file = Some(entry.path().to_string_lossy().to_string());
                    *files_processed = i;
                }
            }

            // Process the entry
            file_handling::process_entry(&conn_mutex, entry.clone());

            // Throttle status updates and check for stop signal more frequently
            if i % 10 == 0 {
                thread::sleep(Duration::from_millis(1));
                if *stop_flag.lock().unwrap() {
                    return Ok(());
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
