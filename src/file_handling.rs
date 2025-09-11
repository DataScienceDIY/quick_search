use std::sync::{Mutex, Arc};
use std::ffi::OsString;
use std::fs::{File,read_to_string};
use std::io::{Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;
use std::collections::HashMap;

use sha2::{Sha256, Digest};
use walkdir::DirEntry;
use rusqlite::{params, Connection};

use crate::document_extraction::extract_document_text;
use crate::config::Config;

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub path: String,
    pub size: u64,
    pub moddate: u64,
    pub hash: Vec<u8>,
}

#[derive(Debug)]
pub struct BatchUpdate {
    pub files_to_update: Vec<(DirEntry, FileMetadata)>,
    pub files_to_insert: Vec<DirEntry>,
}

pub const PLAINTEXT_EXTENSIONS_LIST: [&'static str; 86] = 
    ["","txt","rtf","log", // Text Documents
    "csv", // Spreadsheet
    "sh","bat","cmd","bash","ps1","psm1","psd1","pssc","psrc", // Scripts
    "c","cpp","i","cs","csx","caki", // C#
    "cpp","cc","cxx","c++","hpp","hh","hxx","h","ii", // C++
    "tex","bib","bbx","cbx", // LaTeX
    "css","xml","md","json","yaml","yml", // Markup Languages and others
    "html","htm","shtml","xhtml","xht","mdoc","jsp","asp","aspx","jshtm", // HTML
    "js","cjs","mjs","es6","es","jsx","ts","tsx", // Javascript and TypeScript
    "cfg","conf","ini","gitattributes","gitignore", // Config and related files
    "java","jav", // Java
    "pl","pm","pod","t","psgi", // Perl
    "php","php4","php5","phtml","ctp", // PHP
    "py","rpy","pyw","cpy","gyp","gypi","pyi","ipy","pyt","ipynb", // Python
    "wasm","wat", // Web Assembly
    ];

pub const SUPPORTED_DOCUMENT_EXTENSIONS_LIST: [&'static str; 9] = 
    ["odt", "docx", "doc", // Office Documents
    "ppt", "pptx", "odp", // Presentation
    "xls", "xlsx", "ods"]; // Spreadsheet

/// Load existing file metadata from database indexed by path
pub fn load_existing_files(conn: &Connection) -> Result<HashMap<String, FileMetadata>, rusqlite::Error> {
    let mut existing_files = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, size, moddate, hash FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok(FileMetadata {
            path: row.get(0)?,
            size: row.get(1)?,
            moddate: row.get(2)?,
            hash: row.get(3)?,
        })
    })?;

    for row in rows {
        let metadata = row?;
        existing_files.insert(metadata.path.clone(), metadata);
    }
    
    Ok(existing_files)
}

/// Analyze files and determine which need updates vs inserts
pub fn analyze_files_for_batch_update(
    entries: &[DirEntry], 
    existing_files: &HashMap<String, FileMetadata>
) -> BatchUpdate {
    let mut files_to_update = Vec::new();
    let mut files_to_insert = Vec::new();

    for entry in entries {
        let meta = match entry.metadata() {
            Ok(m) if !m.is_dir() => m,
            _ => continue,
        };

        let fpath = match entry.path().canonicalize() {
            Ok(fp) => fp.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        let fmodified = match meta.modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())) {
            Some(time) => time,
            None => continue,
        };

        if let Some(existing_metadata) = existing_files.get(&fpath) {
            // File exists in database, check if modification date changed
            if existing_metadata.moddate != fmodified {
                files_to_update.push((entry.clone(), existing_metadata.clone()));
            }
            // If moddate is same, skip processing this file entirely
        } else {
            // New file, needs to be inserted
            files_to_insert.push(entry.clone());
        }
    }

    BatchUpdate {
        files_to_update,
        files_to_insert,
    }
}

/// Safely truncate a string to at most max_bytes bytes while respecting UTF-8 character boundaries
fn safe_truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    
    // Find the last valid UTF-8 character boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    
    s[..end].to_string()
}

/// Get a hash of a file by reading the first and last hash_length bytes of the file
fn get_file_hash(size: u64, path: OsString, hash_length: usize) -> Result<Vec<u8>, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut f: File = File::open(path)?;
    hasher.update(&size.to_le_bytes());
    if size > hash_length as u64 {
        let mut file_start_block = vec![0u8; hash_length];
        f.read_exact(&mut file_start_block)?;
        hasher.update(&file_start_block);
        f.seek(SeekFrom::End(0 - hash_length as i64))?;
        let mut file_end_block = vec![0u8; hash_length];
        f.read_exact(&mut file_end_block)?;
        hasher.update(&file_end_block);
    } else if size > 0 {
        let mut file_block = Vec::new();
        f.read_to_end(&mut file_block)?;
        hasher.update(file_block);
    }
    drop(f);
    Ok(hasher.finalize().to_vec()) 
}

/// Process updated files in batch with transaction - files table only (no text extraction)
pub fn process_batch_updates_files_only(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_update: &[(DirEntry, FileMetadata)],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config
) -> Result<(), String> {
    if files_to_update.is_empty() {
        return Ok(());
    }

    let batch_size = config.processing.batch_size;
    let total_files = files_to_update.len();

    // Process files in batches of batch_size
    for (batch_idx, batch) in files_to_update.chunks(batch_size).enumerate() {
        // Check stop flag at the start of each batch
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, (entry, _old_metadata)) in batch.iter().enumerate() {
            let global_index = batch_idx * batch_size + i + 1;
            // Check stop flag
            if *stop_flag.lock().unwrap() {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            // Update status with current file
            if let Some(ref callback) = status_callback {
                let filename = entry.path().file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                callback(&format!("Updating file metadata {}/{}: {}", global_index, total_files, filename));
            }
            
            // Update progress counter
            if let Some(ref progress_cb) = progress_callback {
                progress_cb(global_index);
            }

            let meta = entry.metadata().map_err(|e| format!("Failed to get metadata: {}", e))?;
            if meta.is_dir() {
                continue;
            }

            let fpath = match entry.path().canonicalize() {
                Ok(fp) => fp.into_os_string(),
                Err(_) => continue,
            };

            let fsize = meta.len();
            let fmodified = meta.modified()
                .map_err(|e| format!("Failed to get modified time: {}", e))?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("Failed to calculate duration: {}", e))?
                .as_secs();

            let fhash = get_file_hash(fsize, fpath.clone(), config.processing.hash_length)
                .map_err(|e| format!("Failed to calculate hash: {}", e))?;

            // Check stop flag after hash calculation
            if *stop_flag.lock().unwrap() {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            // Update files table
            tx.execute(
                "UPDATE files SET size = ?1, moddate = ?2, hash = ?3 WHERE path = ?4",
                params![fsize, fmodified, fhash, fpath.to_string_lossy()]
            ).map_err(|e| format!("Failed to update file record: {}", e))?;

            // Delete old searchable text entry for this file (will be re-added in text indexing phase)
            tx.execute(
                "DELETE FROM searchabletext WHERE path = ?1",
                params![fpath.to_string_lossy()]
            ).map_err(|e| format!("Failed to delete old searchable text: {}", e))?;
        }

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}

/// Process new files in batch with transaction - files table only (no text extraction)
pub fn process_batch_inserts_files_only(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[DirEntry],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    let batch_size = config.processing.batch_size;
    let total_files = files_to_insert.len();

    // Process files in batches of batch_size
    for (batch_idx, batch) in files_to_insert.chunks(batch_size).enumerate() {
        // Check stop flag at the start of each batch
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, entry) in batch.iter().enumerate() {
            let global_index = batch_idx * batch_size + i + 1;
            // Check stop flag
            if *stop_flag.lock().unwrap() {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            // Update status with current file
            if let Some(ref callback) = status_callback {
                let filename = entry.path().file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                callback(&format!("Indexing file metadata {}/{}: {}", global_index, total_files, filename));
            }
            
            // Update progress counter
            if let Some(ref progress_cb) = progress_callback {
                progress_cb(global_index);
            }

            let meta = entry.metadata().map_err(|e| format!("Failed to get metadata: {}", e))?;
            if meta.is_dir() {
                continue;
            }

            let fpath = match entry.path().canonicalize() {
                Ok(fp) => fp.into_os_string(),
                Err(_) => continue,
            };

            let fsize = meta.len();
            let fmodified = meta.modified()
                .map_err(|e| format!("Failed to get modified time: {}", e))?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("Failed to calculate duration: {}", e))?
                .as_secs();

            let fhash = get_file_hash(fsize, fpath.clone(), config.processing.hash_length)
                .map_err(|e| format!("Failed to calculate hash: {}", e))?;

            let fname = entry.path().file_name().unwrap().to_os_string();

            // Insert into files table
            tx.execute(
                "INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5)",
                params![fname.to_str(), fpath.to_string_lossy(), fsize, fmodified, fhash]
            ).map_err(|e| format!("Failed to insert file record: {}", e))?;
        }

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}

/// Process text indexing for files - adds entries to searchabletext table
pub fn process_text_indexing(
    conn_mutex: &Arc<Mutex<Connection>>,
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config
) -> Result<(), String> {
    let conn = conn_mutex.lock().unwrap();
    
    // Get all files from the files table that don't have corresponding searchabletext entries
    let mut stmt = conn.prepare(
        "SELECT f.name, f.path, f.size FROM files f 
         LEFT JOIN searchabletext s ON f.path = s.path 
         WHERE s.path IS NULL AND f.size <= ?1"
    ).map_err(|e| format!("Failed to prepare statement: {}", e))?;
    
    let file_rows = stmt.query_map([config.processing.maximum_file_size], |row| {
        Ok((
            row.get::<_, String>(0)?, // name
            row.get::<_, String>(1)?, // path
            row.get::<_, u64>(2)?     // size
        ))
    }).map_err(|e| format!("Failed to query files: {}", e))?;

    let files_to_process: Vec<_> = file_rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect files: {}", e))?;
    
    drop(stmt);
    drop(conn);

    let total_files = files_to_process.len();
    let batch_size = config.processing.batch_size;

    // Process files in batches
    for (batch_idx, batch) in files_to_process.chunks(batch_size).enumerate() {
        // Check stop flag at the start of each batch
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, (fname, fpath, _fsize)) in batch.iter().enumerate() {
            let global_index = batch_idx * batch_size + i + 1;
            
            // Check stop flag
            if *stop_flag.lock().unwrap() {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            // Update status
            if let Some(ref callback) = status_callback {
                callback(&format!("Extracting text for search indexing {}/{}: {}", global_index, total_files, fname));
            }
            
            // Update progress counter
            if let Some(ref progress_cb) = progress_callback {
                progress_cb(global_index);
            }

            let path = std::path::Path::new(fpath);
            let default_ext = OsString::new();
            let file_extension = path.extension().unwrap_or(&default_ext)
                .to_ascii_lowercase().to_str().unwrap_or("").to_string();
            let ext_str = file_extension.as_str();
            
            // Process text content with error handling for individual files
            let text_result = if PLAINTEXT_EXTENSIONS_LIST.contains(&ext_str) {
                match read_to_string(fpath) {
                    Ok(file_string) => {
                        let trimmed_file_string = safe_truncate_string(&file_string, config.processing.maximum_text_size);
                        Some(trimmed_file_string)
                    }
                    Err(e) => {
                        // eprintln!("Warning: Failed to read plaintext file {}: {}", fpath, e);
                        None
                    }
                }
            } else if SUPPORTED_DOCUMENT_EXTENSIONS_LIST.contains(&ext_str) {
                match extract_document_text(&std::ffi::OsString::from(fpath), ext_str) {
                    Ok(extracted_text) => {
                        if !extracted_text.trim().is_empty() {
                            let trimmed_file_string = safe_truncate_string(&extracted_text, config.processing.maximum_text_size);
                            Some(trimmed_file_string)
                        } else {
                            None
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to extract text from document {}: {}", fpath, e);
                        None
                    }
                }
            } else {
                None
            };

            // Insert the text content if we successfully extracted it
            if let Some(text_content) = text_result {
                if let Err(e) = tx.execute(
                    "INSERT INTO searchabletext VALUES (?1, ?2, ?3)",
                    params![fname, fpath, text_content]
                ) {
                    eprintln!("Warning: Failed to insert searchable text for {}: {}", fpath, e);
                }
            }
        }

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}
