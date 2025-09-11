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

const HASHLEN:usize = 1024 * 8;
const MAXIMUM_TEXT_SIZE:usize = 1024 * 512;
const MAXIMUM_FILE_SIZE:u64 = 1024 * 1024 * 50;
const PLAINTEXT_EXTENSIONS_LIST: [&'static str; 86] = 
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

const SUPPORTED_DOCUMENT_EXTENSIONS_LIST: [&'static str; 9] = 
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

/// Get a hash of a file by reading the first and last HASHLEN bytes of the file
fn get_file_hash(size: u64, path: OsString) -> Result<Vec<u8>, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut f: File = File::open(path)?;
    hasher.update(&size.to_le_bytes());
    if size > HASHLEN as u64 {
        let mut file_start_block = [0u8; HASHLEN];
        f.read_exact(&mut file_start_block)?;
        hasher.update(file_start_block);
        f.seek(SeekFrom::End(0 - HASHLEN as i64))?;
        let mut file_end_block = [0u8; HASHLEN];
        f.read_exact(&mut file_end_block)?;
        hasher.update(file_end_block);
    } else if size > 0 {
        let mut file_block = Vec::new();
        f.read_to_end(&mut file_block)?;
        hasher.update(file_block);
    }
    drop(f);
    Ok(hasher.finalize().to_vec()) 
}

/// Process updated files in batch with transaction
pub fn process_batch_updates(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_update: &[(DirEntry, FileMetadata)],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>
) -> Result<(), String> {
    if files_to_update.is_empty() {
        return Ok(());
    }

    const BATCH_SIZE: usize = 1000;
    let total_files = files_to_update.len();

    // Process files in batches of BATCH_SIZE
    for (batch_idx, batch) in files_to_update.chunks(BATCH_SIZE).enumerate() {
        // Check stop flag at the start of each batch
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, (entry, _old_metadata)) in batch.iter().enumerate() {
            let global_index = batch_idx * BATCH_SIZE + i + 1;
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
                callback(&format!("Updating file {}/{}: {}", global_index, total_files, filename));
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

        let fhash = get_file_hash(fsize, fpath.clone())
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

        // Delete old searchable text entry
        tx.execute(
            "DELETE FROM searchabletext WHERE path = ?1",
            params![fpath.to_string_lossy()]
        ).map_err(|e| format!("Failed to delete old searchable text: {}", e))?;

        // Insert new searchable text if applicable
        if fsize <= MAXIMUM_FILE_SIZE {
            let default_ext = OsString::new();
            let file_extension = entry.path().extension().unwrap_or(&default_ext)
                .to_ascii_lowercase().to_str().unwrap_or("").to_string();
            let ext_str = file_extension.as_str();
            
            if PLAINTEXT_EXTENSIONS_LIST.contains(&ext_str) {
                if let Ok(file_string) = read_to_string(&fpath) {
                    let trimmed_file_string = if file_string.len() > MAXIMUM_TEXT_SIZE {
                        file_string[..MAXIMUM_TEXT_SIZE].to_string()
                    } else {
                        file_string
                    };

                    let fname = entry.path().file_name().unwrap().to_os_string();
                    tx.execute(
                        "INSERT INTO searchabletext VALUES (?1, ?2, ?3)",
                        params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]
                    ).map_err(|e| format!("Failed to insert searchable text: {}", e))?;
                }
            } else if SUPPORTED_DOCUMENT_EXTENSIONS_LIST.contains(&ext_str) {
                if let Ok(extracted_text) = extract_document_text(&fpath, ext_str) {
                    if !extracted_text.trim().is_empty() {
                        let trimmed_file_string = if extracted_text.len() > MAXIMUM_TEXT_SIZE {
                            extracted_text[..MAXIMUM_TEXT_SIZE].to_string()
                        } else {
                            extracted_text
                        };

                        let fname = entry.path().file_name().unwrap().to_os_string();
                        tx.execute(
                            "INSERT INTO searchabletext VALUES (?1, ?2, ?3)",
                            params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]
                        ).map_err(|e| format!("Failed to insert document text: {}", e))?;
                    }
                }
            }
        }
        }

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}

/// Process new files in batch with transaction
pub fn process_batch_inserts(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[DirEntry],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    const BATCH_SIZE: usize = 1000;
    let total_files = files_to_insert.len();

    // Process files in batches of BATCH_SIZE
    for (batch_idx, batch) in files_to_insert.chunks(BATCH_SIZE).enumerate() {
        // Check stop flag at the start of each batch
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (i, entry) in batch.iter().enumerate() {
            let global_index = batch_idx * BATCH_SIZE + i + 1;
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
                callback(&format!("Indexing file {}/{}: {}", global_index, total_files, filename));
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

        let fhash = get_file_hash(fsize, fpath.clone())
            .map_err(|e| format!("Failed to calculate hash: {}", e))?;

        let fname = entry.path().file_name().unwrap().to_os_string();

        // Insert into files table
        tx.execute(
            "INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5)",
            params![fname.to_str(), fpath.to_string_lossy(), fsize, fmodified, fhash]
        ).map_err(|e| format!("Failed to insert file record: {}", e))?;

        // Insert searchable text if applicable
        if fsize <= MAXIMUM_FILE_SIZE {
            let default_ext = OsString::new();
            let file_extension = entry.path().extension().unwrap_or(&default_ext)
                .to_ascii_lowercase().to_str().unwrap_or("").to_string();
            let ext_str = file_extension.as_str();
            
            if PLAINTEXT_EXTENSIONS_LIST.contains(&ext_str) {
                if let Ok(file_string) = read_to_string(&fpath) {
                    let trimmed_file_string = if file_string.len() > MAXIMUM_TEXT_SIZE {
                        file_string[..MAXIMUM_TEXT_SIZE].to_string()
                    } else {
                        file_string
                    };

                    tx.execute(
                        "INSERT INTO searchabletext VALUES (?1, ?2, ?3)",
                        params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]
                    ).map_err(|e| format!("Failed to insert searchable text: {}", e))?;
                }
            } else if SUPPORTED_DOCUMENT_EXTENSIONS_LIST.contains(&ext_str) {
                if let Ok(extracted_text) = extract_document_text(&fpath, ext_str) {
                    if !extracted_text.trim().is_empty() {
                        let trimmed_file_string = if extracted_text.len() > MAXIMUM_TEXT_SIZE {
                            extracted_text[..MAXIMUM_TEXT_SIZE].to_string()
                        } else {
                            extracted_text
                        };

                        tx.execute(
                            "INSERT INTO searchabletext VALUES (?1, ?2, ?3)",
                            params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]
                        ).map_err(|e| format!("Failed to insert document text: {}", e))?;
                    }
                }
            }
        }
        }

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}

pub fn process_entry(conn_mutex: &Arc<Mutex<Connection>>, entry: DirEntry) {
    let meta = entry.metadata().unwrap();
    if !meta.is_dir() {
        // let fpath = entry.path().canonicalize()?.into_os_string();
        let fpath_result = entry.path().canonicalize();
        let fpath = match fpath_result {
            Ok(fp) => fp.into_os_string(),
            Err(error) => {
                println!("Error converting fpath: {:?}", error);
                return;
            },
        };
        
        // Get basic file properties
        let fsize = meta.len();
        let fmodified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();

        // Get file hash
        let fhash_result = get_file_hash(fsize, fpath.clone());
        let fhash = match fhash_result {
            Ok(fh) => fh,
            Err(error) => {
                println!("Error digesting hash: {:?}", error);
                return;
            },
        };
        // let fhash = b"";
        
        // Insert results into database
        let query = "INSERT INTO files VALUES (?1,?2,?3,?4,?5)";
        let conn = conn_mutex.lock().unwrap();
        let mut stmt = conn.prepare_cached(query).unwrap();
        let fname = entry.path().file_name().unwrap().to_os_string();

        let stmt_result = stmt.execute(params![fname.to_str(), fpath.to_string_lossy(), fsize, fmodified, fhash]);
        match stmt_result {
            Ok(us) => us,
            Err(error) => {
                println!("Error with sqlite transaction: {:?}", error);
                return;
            },
        };
        std::mem::drop(stmt); // Free the mutex lock so that other threads can access the database
        std::mem::drop(conn);

        // Generate searchable plain text for file if applicable
        if fsize <= MAXIMUM_FILE_SIZE {
            let default_ext = OsString::new();
            let file_extension = entry.path().extension().unwrap_or(&default_ext).to_ascii_lowercase().to_str().unwrap().to_string();
            let ext_str = file_extension.as_str();
            if PLAINTEXT_EXTENSIONS_LIST.contains(&ext_str) {
                let file_contents_result = read_to_string(fpath.clone());
                let file_string = match file_contents_result {
                    Ok(fs) => fs,
                    Err(_error) => {
                        // println!("Error reading file to string: {:?}", error);
                        return;
                    },
                };

                let trimmed_file_string;
                // Trim file contents if too large
                if file_string.len() > MAXIMUM_TEXT_SIZE {
                    trimmed_file_string = file_string[..MAXIMUM_TEXT_SIZE].to_string();
                } else {
                    trimmed_file_string = file_string;
                }

                // Insert file contents into database
                let query2 = "INSERT INTO searchabletext VALUES (?1,?2,?3)";
                let conn2 = conn_mutex.lock().unwrap();
                let mut stmt2 = conn2.prepare_cached(query2).unwrap();

                let stmt_result2 = stmt2.execute(params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]);
                match stmt_result2 {
                    Ok(us) => us,
                    Err(error) => {
                        println!("Error with sqlite transaction 2: {:?}", error);
                        return;
                    },
                };
            }
            else if SUPPORTED_DOCUMENT_EXTENSIONS_LIST.contains(&ext_str) {
                // Extract text from office documents
                match extract_document_text(&fpath, ext_str) {
                    Ok(extracted_text) => {
                        if !extracted_text.trim().is_empty() {
                            let trimmed_file_string;
                            // Trim file contents if too large
                            if extracted_text.len() > MAXIMUM_TEXT_SIZE {
                                trimmed_file_string = extracted_text[..MAXIMUM_TEXT_SIZE].to_string();
                            } else {
                                trimmed_file_string = extracted_text;
                            }

                            // Insert file contents into database
                            let query2 = "INSERT INTO searchabletext VALUES (?1,?2,?3)";
                            let conn2 = conn_mutex.lock().unwrap();
                            let mut stmt2 = conn2.prepare_cached(query2).unwrap();

                            let stmt_result2 = stmt2.execute(params![fname.to_str(), fpath.to_string_lossy(), trimmed_file_string]);
                            match stmt_result2 {
                                Ok(_) => {},
                                Err(error) => {
                                    println!("Error with sqlite transaction for document: {:?}", error);
                                },
                            };
                        }
                    }
                    Err(error) => {
                        println!("Error extracting text from document {}: {:?}", fpath.to_string_lossy(), error);
                    }
                }
            }
        }

    }
}