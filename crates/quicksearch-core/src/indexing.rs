use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use std::process::Command;
use std::collections::HashSet;
use rusqlite::{Connection, params};
use walkdir::DirEntry;

use crate::file_handling::{
    classify_dir_entry_for_indexing,
    cleanup_stale_index_entries,
    count_tree_entries_fast,
    indexed_walk_file_entries,
    load_existing_files,
    path_has_hidden_component,
    process_batch_inserts_files_only,
    process_batch_updates_files_only,
    process_text_indexing,
    FileIndexAction,
};
use crate::config::Config;
use crate::db;

#[derive(Debug, Clone)]
pub struct SearchResultRow {
    pub values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub columns: Vec<String>,
    pub rows: Vec<SearchResultRow>,
}

#[derive(Debug, Clone)]
pub enum IndexingStatus {
    Idle,
    CountingFiles {
        _entries_scanned: usize,
        _indexable_files_counted: usize,
        current_file: Option<String>,
        start_time: Instant,
    },
    RunningFileIndex {
        files_processed: usize,
        total_files: Option<usize>,
        current_file: Option<String>,
        start_time: Instant,
    },
    RunningTextIndex {
        files_processed: usize,
        current_file: Option<String>,
        start_time: Instant,
    },
    Stopping,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum IndexingCommand {
    Start {
        /// One or more directory roots to index. Order determines walk order;
        /// duplicates are silently dropped at run time.
        paths: Vec<String>,
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
    suspend_flag: Arc<AtomicBool>,
    _handle: thread::JoinHandle<()>,
}

/// Polling interval for `should_abort` while suspended.
const SUSPEND_POLL_MS: u64 = 100;

/// Combined stop/suspend check used by worker loops. Returns `true` iff the
/// caller should abort the operation. While the suspend flag is set and stop
/// is not, this parks the thread by sleeping in short increments so a later
/// `resume()` unblocks it. Cheap to call in tight loops.
pub(crate) fn should_abort(
    stop: &Arc<Mutex<bool>>,
    suspend: &Arc<AtomicBool>,
) -> bool {
    loop {
        if *stop.lock().unwrap() {
            return true;
        }
        if !suspend.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(SUSPEND_POLL_MS));
    }
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
        let suspend_flag = Arc::new(AtomicBool::new(false));

        let status_clone = status.clone();
        let db_connection_clone = db_connection.clone();
        let suspend_clone = suspend_flag.clone();
        let handle = thread::spawn(move || {
            Self::indexing_thread(status_clone, command_rx, db_connection_clone, suspend_clone);
        });

        IndexingService {
            status,
            command_tx,
            db_connection,
            suspend_flag,
            _handle: handle,
        }
    }

    /// Pause the indexer. All worker loops that call [`should_abort`] will
    /// block until [`resume`](Self::resume) is called. No-op if already
    /// suspended. Does not stop the worker — stop_indexing is still the way
    /// to abort.
    pub fn suspend(&self) {
        self.suspend_flag.store(true, Ordering::Relaxed);
    }

    /// Resume indexing after [`suspend`](Self::suspend). No-op if not
    /// suspended.
    pub fn resume(&self) {
        self.suspend_flag.store(false, Ordering::Relaxed);
    }

    pub fn is_suspended(&self) -> bool {
        self.suspend_flag.load(Ordering::Relaxed)
    }

    /// Start indexing one or more roots. Paths are walked in order; duplicate
    /// or nested roots are de-duplicated by the indexer. At least one path is
    /// required.
    pub fn start_indexing(
        &self,
        paths: Vec<String>,
        db_path: String,
        config: Config,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("start_indexing requires at least one path".into());
        }
        self.command_tx
            .send(IndexingCommand::Start { paths, db_path, config })
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

    /// Execute a search query against the database
    pub fn execute_search(&self, db_path: &str, query: &str) -> Result<Vec<SearchResult>, String> {
        let conn = db::open_and_migrate(db_path, "trigram")
            .map_err(|e| {
                if e.contains("corrupt") || e.contains("malformed") {
                    format!("DATABASE_CORRUPTED: {}", e)
                } else {
                    e
                }
            })?;

        let mut stmt = conn.prepare(query)
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("malformed") || error_msg.contains("corrupt") || error_msg.contains("database disk image is malformed") {
                    format!("DATABASE_CORRUPTED: {}", error_msg)
                } else if error_msg.contains("fts5: syntax error") {
                    format!("Search syntax error: The search term contains characters that cannot be processed. Please try a simpler search term.")
                } else {
                    format!("Failed to prepare query: {}", error_msg)
                }
            })?;
        
        let column_count = stmt.column_count();
        let column_names: Vec<String> = (0..column_count)
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();
        
        let rows = stmt.query_map([], |row| {
            let mut values = Vec::new();
            for i in 0..column_count {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => "NULL".to_string(),
                    rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                    rusqlite::types::ValueRef::Real(f) => f.to_string(),
                    rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).to_string(),
                    rusqlite::types::ValueRef::Blob(b) => format!("BLOB({} bytes)", b.len()),
                };
                values.push(value);
            }
            Ok(SearchResultRow { values })
        })
        .map_err(|e| {
            let error_msg = e.to_string();
            if error_msg.contains("malformed") || error_msg.contains("corrupt") || error_msg.contains("database disk image is malformed") {
                format!("DATABASE_CORRUPTED: {}", error_msg)
            } else if error_msg.contains("fts5: syntax error") {
                format!("Search syntax error: The search term contains characters that cannot be processed. Please try a simpler search term.")
            } else {
                format!("Failed to execute query: {}", error_msg)
            }
        })?;
        
        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(search_row) => results.push(search_row),
                Err(e) => {
                    let error_msg = e.to_string();
                    if error_msg.contains("malformed") || error_msg.contains("corrupt") || error_msg.contains("database disk image is malformed") {
                        return Err(format!("DATABASE_CORRUPTED: {}", error_msg));
                    } else if error_msg.contains("fts5: syntax error") {
                        return Err(format!("Search syntax error: The search term contains characters that cannot be processed. Please try a simpler search term."));
                    } else {
                        return Err(format!("Error reading row: {}", error_msg));
                    }
                }
            }
        }
        
        Ok(vec![SearchResult {
            columns: column_names,
            rows: results,
        }])
    }

    /// Open file explorer to the directory containing the specified file path
    pub fn open_file_explorer(&self, file_path: &str) -> Result<(), String> {
        #[cfg(windows)]
        {
            Command::new("explorer")
                .arg("/select,")
                .arg(file_path)
                .spawn()
                .map_err(|e| format!("Failed to open file explorer: {}", e))?;
        }

        #[cfg(target_os = "macos")]
        {
            Command::new("open")
                .arg("-R")
                .arg(file_path)
                .spawn()
                .map_err(|e| format!("Failed to open file explorer: {}", e))?;
        }

        #[cfg(target_os = "linux")]
        {
            let path = std::path::Path::new(file_path);
            let dir_path = if path.is_file() {
                path.parent().unwrap_or(path)
            } else {
                path
            };
            
            // Try different file managers
            let managers = ["xdg-open", "nautilus", "dolphin", "thunar", "pcmanfm"];
            let mut success = false;
            
            for manager in &managers {
                if let Ok(_) = Command::new(manager)
                    .arg(dir_path)
                    .spawn() {
                    success = true;
                    break;
                }
            }
            
            if !success {
                return Err("No suitable file manager found".to_string());
            }
        }

        Ok(())
    }

    /// Clean up Windows UNC prefixes (\\?\) from existing `files.path`
    /// entries. Relic of pre-A layouts where paths were stored with the
    /// prefix; current code strips them at insert time, so this is a
    /// one-shot cleanup users can invoke manually if needed.
    #[allow(dead_code)]
    pub fn clean_unc_prefixes(&self, db_path: &str) -> Result<(), String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        conn.execute(
            "UPDATE files SET path = SUBSTR(path, 5) WHERE path LIKE '\\\\?\\%'",
            (),
        )
        .map_err(|e| format!("Failed to update files table: {}", e))?;
        Ok(())
    }

    /// Check if the database is corrupted or malformed
    #[allow(dead_code)]
    pub fn check_database_health(&self, db_path: &str) -> Result<bool, String> {
        match Connection::open(db_path) {
            Ok(conn) => {
                // Try to run integrity check
                match conn.prepare("PRAGMA integrity_check") {
                    Ok(mut stmt) => {
                        match stmt.query_row([], |row| {
                            let result: String = row.get(0)?;
                            Ok(result == "ok")
                        }) {
                            Ok(is_ok) => Ok(is_ok),
                            Err(_) => Ok(false)
                        }
                    },
                    Err(_) => Ok(false)
                }
            },
            Err(_) => Ok(false)
        }
    }

    /// Check if configuration changes require index recreation
    pub fn check_config_validation(&self, db_path: &str, config: &Config, indexing_path: &str) -> Result<Option<Vec<String>>, String> {
        let conn = db::open_and_migrate(db_path, &config.processing.tokenize)?;
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
                IndexingStatus::Stopping
                | IndexingStatus::CountingFiles { .. }
                | IndexingStatus::RunningFileIndex { .. }
                | IndexingStatus::RunningTextIndex { .. } => {
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
        db_connection: Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
        suspend_flag: Arc<AtomicBool>,
    ) {
        let stop_flag = Arc::new(Mutex::new(false));
        let mut indexing_handle: Option<thread::JoinHandle<()>> = None;
        
        while let Ok(command) = command_rx.recv() {
            match command {
                IndexingCommand::Start { paths, db_path, config } => {
                    if matches!(
                        *status.lock().unwrap(),
                        IndexingStatus::CountingFiles { .. }
                            | IndexingStatus::RunningFileIndex { .. }
                            | IndexingStatus::RunningTextIndex { .. }
                    ) {
                        continue; // Already running
                    }

                    // Join any previous indexing thread
                    if let Some(handle) = indexing_handle.take() {
                        let _ = handle.join();
                    }

                    *stop_flag.lock().unwrap() = false;
                    *status.lock().unwrap() = if config.processing.precount_files_for_progress {
                        IndexingStatus::CountingFiles {
                            _entries_scanned: 0,
                            _indexable_files_counted: 0,
                            current_file: Some("Preparing database...".to_string()),
                            start_time: Instant::now(),
                        }
                    } else {
                        IndexingStatus::RunningFileIndex {
                            files_processed: 0,
                            total_files: None,
                            current_file: None,
                            start_time: Instant::now(),
                        }
                    };

                    // Run indexing in a separate thread
                    let status_clone = status.clone();
                    let stop_flag_clone = stop_flag.clone();
                    let paths_owned = paths.clone();
                    let db_path_owned = db_path.clone();
                    let config_owned = config.clone();

                    let db_connection_clone = db_connection.clone();
                    let suspend_clone = suspend_flag.clone();
                    indexing_handle = Some(thread::spawn(move || {
                        if let Err(e) = Self::run_indexing(&status_clone, &paths_owned, &db_path_owned, &stop_flag_clone, &suspend_clone, &config_owned, &db_connection_clone) {
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
                    if matches!(
                        *status.lock().unwrap(),
                        IndexingStatus::CountingFiles { .. }
                            | IndexingStatus::RunningFileIndex { .. }
                            | IndexingStatus::RunningTextIndex { .. }
                    ) {
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

    fn file_index_status_callback(
        status: &Arc<Mutex<IndexingStatus>>,
    ) -> Box<dyn Fn(&str) + Send + Sync> {
        let st = status.clone();
        Box::new(move |file_status: &str| {
            if let Ok(mut status_guard) = st.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard
                {
                    *current_file = Some(file_status.to_string());
                }
            }
        })
    }

    fn run_indexing(
        status: &Arc<Mutex<IndexingStatus>>,
        paths: &[String],
        db_path: &str,
        stop_flag: &Arc<Mutex<bool>>,
        suspend_flag: &Arc<AtomicBool>,
        config: &Config,
        db_connection: &Arc<Mutex<Option<Arc<Mutex<Connection>>>>>,
    ) -> Result<(), String> {
        if paths.is_empty() {
            return Err("run_indexing: no paths provided".into());
        }

        // De-duplicate while preserving order. Roots are canonicalized first
        // so `/home/jeremy` and `/home/jeremy/` (or a symlink to either)
        // collapse to one walk. Pure nested-root deduplication (skip a root
        // that is a prefix of an already-walked root) is handled by the
        // per-file `seen_paths` set below.
        let mut seen_roots = HashSet::new();
        let roots: Vec<String> = paths
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .canonicalize()
                    .ok()
                    .map(|c| {
                        let s = c.to_string_lossy().to_string();
                        if s.starts_with("\\\\?\\") {
                            s[4..].to_string()
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| p.clone())
            })
            .filter(|p| seen_roots.insert(p.clone()))
            .collect();

        // Open and migrate the database to the current schema version.
        let conn = db::open_and_migrate(db_path, &config.processing.tokenize)?;

        // Update configuration (for new installations or when no validation issues).
        // `indexing_path` in the validation table stores the joined list so
        // adding/removing a root triggers the same rebuild prompt as changing
        // the legacy single path did.
        Self::update_config(&conn, config, &roots.join("\n"))?;

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

        let progress_display_total: Option<usize> =
            if config.processing.precount_files_for_progress {
                if *stop_flag.lock().unwrap() {
                    if let Ok(mut status_guard) = status.lock() {
                        *status_guard = IndexingStatus::Idle;
                    }
                    return Ok(());
                }
                if let Ok(mut g) = status.lock() {
                    if let IndexingStatus::CountingFiles { ref mut current_file, .. } = *g {
                        *current_file = Some("Counting paths (shell)...".to_string());
                    }
                }
                let mut n: usize = 0;
                for root in &roots {
                    if *stop_flag.lock().unwrap() {
                        break;
                    }
                    // A failing precount on one root (e.g. permission denied)
                    // shouldn't abort all indexing; just skip its contribution.
                    match count_tree_entries_fast(root) {
                        Ok(c) => n += c,
                        Err(e) => eprintln!("Warning: precount for {}: {}", root, e),
                    }
                }
                if *stop_flag.lock().unwrap() {
                    if let Ok(mut status_guard) = status.lock() {
                        *status_guard = IndexingStatus::Idle;
                    }
                    return Ok(());
                }
                if let Ok(mut status_guard) = status.lock() {
                    *status_guard = IndexingStatus::RunningFileIndex {
                        files_processed: 0,
                        total_files: Some(n),
                        current_file: None,
                        start_time: Instant::now(),
                    };
                }
                Some(n)
            } else {
                None
            };

        let batch_size = config.processing.batch_size;
        let mut pending_updates: Vec<(DirEntry, usize)> = Vec::new();
        let mut pending_inserts: Vec<(DirEntry, usize)> = Vec::new();
        // Every path seen during this walk, regardless of outcome. Dedupes
        // duplicate visits that can happen when two roots overlap (e.g.
        // `/home` and `/home/user`) or when canonicalization collapses
        // symlinks. Also feeds the stale-entry cleanup at the end of the walk.
        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut visit: usize = 0;
        let mut had_incremental_work = false;
        let flush_updates = |buf: &mut Vec<(DirEntry, usize)>| -> Result<(), String> {
            if buf.is_empty() {
                return Ok(());
            }
            process_batch_updates_files_only(
                &conn_mutex,
                buf.as_slice(),
                stop_flag,
                Some(Self::file_index_status_callback(status)),
                None,
                config,
                progress_display_total,
            )?;
            buf.clear();
            Ok(())
        };

        let flush_inserts = |buf: &mut Vec<(DirEntry, usize)>| -> Result<(), String> {
                if buf.is_empty() {
                    return Ok(());
                }
                process_batch_inserts_files_only(
                    &conn_mutex,
                    buf.as_slice(),
                    stop_flag,
                    Some(Self::file_index_status_callback(status)),
                    None,
                    config,
                    progress_display_total,
                )?;
                buf.clear();
                Ok(())
            };

        let root_iter = roots
            .iter()
            .flat_map(|r| indexed_walk_file_entries(r, config.processing.follow_symlinks));
        for entry in root_iter {
            if should_abort(stop_flag, suspend_flag) {
                flush_updates(&mut pending_updates)?;
                flush_inserts(&mut pending_inserts)?;
                if let Ok(mut status_guard) = status.lock() {
                    *status_guard = IndexingStatus::Idle;
                }
                return Ok(());
            }

            visit += 1;
            if let Ok(mut g) = status.lock() {
                if let IndexingStatus::RunningFileIndex {
                    ref mut files_processed,
                    ..
                } = *g
                {
                    *files_processed = visit;
                }
            }

            if !config.processing.include_hidden && path_has_hidden_component(entry.path()) {
                continue;
            }

            let action = classify_dir_entry_for_indexing(&entry, &existing_files);
            let Some(action) = action else {
                continue;
            };
            let current_path = entry
                .path()
                .canonicalize()
                .ok()
                .map(|fp| {
                    let path_str = fp.to_string_lossy().to_string();
                    if path_str.starts_with("\\\\?\\") {
                        path_str[4..].to_string()
                    } else {
                        path_str
                    }
                });

            // Skip this entry if its canonical path was already queued in
            // this run. Prevents UNIQUE(path) violations when overlapping
            // roots or symlinks lead the walker to the same file twice.
            let path_for_dedup = match &current_path {
                Some(p) => p.clone(),
                None => continue,
            };
            if !seen_paths.insert(path_for_dedup) {
                continue;
            }

            match action {
                FileIndexAction::Skip => {}
                FileIndexAction::Update => {
                    had_incremental_work = true;
                    pending_updates.push((entry, visit));
                    if pending_updates.len() >= batch_size {
                        flush_updates(&mut pending_updates)?;
                    }
                }
                FileIndexAction::Insert => {
                    had_incremental_work = true;
                    pending_inserts.push((entry, visit));
                    if pending_inserts.len() >= batch_size {
                        flush_inserts(&mut pending_inserts)?;
                    }
                }
            }
        }

        flush_updates(&mut pending_updates)?;
        flush_inserts(&mut pending_inserts)?;

        let stale_paths: Vec<String> = existing_files
            .keys()
            .filter(|p| !seen_paths.contains(*p))
            .cloned()
            .collect();
        let stale_deleted = cleanup_stale_index_entries(
            &conn_mutex,
            stale_paths.as_slice(),
            stop_flag,
            suspend_flag,
            Some(Self::file_index_status_callback(status)),
        )?;
        if stale_deleted > 0 {
            had_incremental_work = true;
        }

        if !had_incremental_work {
            if let Ok(mut status_guard) = status.lock() {
                if let IndexingStatus::RunningFileIndex { ref mut current_file, .. } = *status_guard
                {
                    *current_file = Some("File index is up to date".to_string());
                }
            }
        }

        // Check for stop signal (and park if suspended) before starting text indexing.
        if should_abort(stop_flag, suspend_flag) {
            if let Ok(mut status_guard) = status.lock() {
                *status_guard = IndexingStatus::Idle;
            }
            return Ok(());
        }

        // Phase 2: Text indexing
        if let Ok(mut status_guard) = status.lock() {
            *status_guard = IndexingStatus::RunningTextIndex {
                files_processed: 0,
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
        if let Err(e) = process_text_indexing(&conn_mutex, &stop_flag, suspend_flag, Some(text_status_callback), Some(text_progress_callback), config) {
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
        let include_hidden = config.processing.include_hidden.to_string();
        let normalized_path = {
            let path = std::path::Path::new(indexing_path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(indexing_path))
                .to_string_lossy()
                .to_string();
            // Remove Windows UNC prefix \\?\
            if path.starts_with("\\\\?\\") {
                path[4..].to_string()
            } else {
                path
            }
        };
        
        // Check stored configuration values
        let mut stored_hash_length: Option<String> = None;
        let mut stored_indexing_path: Option<String> = None;
        let mut stored_tokenize: Option<String> = None;
        let mut stored_include_hidden: Option<String> = None;
        
        if let Ok(mut stmt) = conn.prepare("SELECT key, value FROM config_validation WHERE key IN ('hash_length', 'indexing_path', 'tokenize', 'include_hidden')") {
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
                        "include_hidden" => stored_include_hidden = Some(row.1),
                        _ => {}
                    }
                }
            }
        }
        
        // Check if configuration is invalid
        let hash_length_changed = stored_hash_length.as_ref().map_or(false, |stored| stored != &hash_length);
        let indexing_path_changed = stored_indexing_path.as_ref().map_or(false, |stored| stored != &normalized_path);
        let tokenize_changed = stored_tokenize.as_ref().map_or(false, |stored| stored != &tokenize);
        let include_hidden_changed = stored_include_hidden.as_ref().map_or(false, |stored| stored != &include_hidden);
        
        if hash_length_changed || indexing_path_changed || tokenize_changed || include_hidden_changed {
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
            if include_hidden_changed {
                changes.push(format!(
                    "include_hidden: {} -> {}",
                    stored_include_hidden.unwrap_or_else(|| "unknown".to_string()),
                    include_hidden
                ));
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
        let include_hidden = config.processing.include_hidden.to_string();
        let normalized_path = {
            let path = std::path::Path::new(indexing_path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(indexing_path))
                .to_string_lossy()
                .to_string();
            // Remove Windows UNC prefix \\?\
            if path.starts_with("\\\\?\\") {
                path[4..].to_string()
            } else {
                path
            }
        };

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
        
        conn.execute(
            "INSERT OR REPLACE INTO config_validation (key, value) VALUES ('include_hidden', ?1)",
            params![include_hidden],
        ).map_err(|e| format!("Failed to store include_hidden config: {}", e))?;

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

