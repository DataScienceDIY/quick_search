use std::sync::{Mutex, Arc};
use std::ffi::OsString;
use std::fs::{File,read_to_string};
use std::io::{Read, Seek, SeekFrom};
use std::path::Component;
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;
use std::collections::HashMap;

use sha2::{Sha256, Digest};
use walkdir::{DirEntry, WalkDir};
use rusqlite::{params, Connection};

use crate::document_extraction::extract_document_text;
use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingFileEntry {
    pub moddate: u64,
}

pub const PLAINTEXT_EXTENSIONS_LIST: [&'static str; 84] = 
    ["c","cs","csx",                                // C
    "cpp","cc","cxx","hpp","hh","hxx","h",          // C++
    "cfg","conf","ini","gitattributes","gitignore", // Config (General)
    "toml","env","tf","tfvars",                     // Config (Infrastructure)
    "scss","sass","less",                           // CSS Preprocessors
    "dart",                                         // Dart
    "diff","patch",                                 // Diffs
    "go",                                           // Go
    "graphql","gql",                                // GraphQL
    "html","htm","xhtml","xht","jsp","asp","aspx",  // HTML
    "java",                                         // Java
    "js","cjs","mjs","jsx","ts","tsx",              // Javascript and TypeScript
    "vue","svelte",                                 // JS Frameworks
    "kt","kts",                                     // Kotlin
    "tex","bib",                                    // LaTeX
    "css","xml","md","json","yaml","yml",           // Markup
    "m",                                            // Objective-C
    "pl","pm","t",                                  // Perl
    "php","phtml",                                  // PHP
    "proto",                                        // Protocol Buffers
    "py","pyw","pyi","ipynb",                       // Python
    "r",                                            // R
    "rb",                                           // Ruby
    "rs",                                           // Rust
    "sh","bat","cmd","bash","ps1","psm1","psd1",    // Scripts
    "sql",                                          // SQL
    "csv",                                          // Spreadsheet
    "svg",                                          // SVG
    "swift",                                        // Swift
    "","txt","rtf","log",                           // Text Documents
    "wasm",                                         // Web Assembly
    ];

pub const SUPPORTED_DOCUMENT_EXTENSIONS_LIST: [&'static str; 9] = 
    ["odt", "docx", "doc", // Office Documents
    "ppt", "pptx", "odp", // Presentation
    "xls", "xlsx", "ods"]; // Spreadsheet

/// Load path and moddate per row for incremental classification (hash/size loaded only when updating a file).
pub fn load_existing_files(conn: &Connection) -> Result<HashMap<String, ExistingFileEntry>, rusqlite::Error> {
    let mut existing_files = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, moddate FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            ExistingFileEntry {
                moddate: row.get(1)?,
            },
        ))
    })?;

    for row in rows {
        let (path, entry) = row?;
        existing_files.insert(path, entry);
    }

    Ok(existing_files)
}

pub fn indexed_walk_file_entries(
    path: &str,
    follow_symlinks: bool,
) -> impl Iterator<Item = DirEntry> {
    WalkDir::new(path)
        .follow_links(follow_symlinks)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|entry| !entry.metadata().map(|m| m.is_dir()).unwrap_or(true))
}

pub fn path_has_hidden_component(path: &std::path::Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::Normal(name) if name.to_string_lossy().starts_with('.')
        )
    })
}

fn parse_wc_l_stdout(bytes: &[u8]) -> Result<usize, String> {
    let s = String::from_utf8_lossy(bytes);
    let token = s
        .trim()
        .split_whitespace()
        .next()
        .ok_or_else(|| "wc: empty output".to_string())?;
    token
        .parse()
        .map_err(|e| format!("wc: invalid count {:?}: {}", token, e))
}

#[cfg(unix)]
fn count_find_pipe_wc(path: &str) -> Result<usize, String> {
    let mut find = Command::new("find")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("find: {}", e))?;
    let find_stdout = find.stdout.take().ok_or("find: stdout")?;
    let wc = Command::new("wc")
        .arg("-l")
        .stdin(find_stdout)
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("wc: {}", e))?;
    find.wait().map_err(|e| format!("find wait: {}", e))?;
    if !wc.status.success() {
        return Err(format!("wc exited with {}", wc.status));
    }
    parse_wc_l_stdout(&wc.stdout)
}

#[cfg(target_os = "linux")]
fn count_find_printf_wc(path: &str) -> Result<usize, String> {
    let mut find = Command::new("find")
        .arg(path)
        .arg("-printf")
        .arg("\n")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("find: {}", e))?;
    let find_stdout = find.stdout.take().ok_or("find: stdout")?;
    let wc = Command::new("wc")
        .arg("-l")
        .stdin(find_stdout)
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("wc: {}", e))?;
    find.wait().map_err(|e| format!("find wait: {}", e))?;
    if !wc.status.success() {
        return Err(format!("wc exited with {}", wc.status));
    }
    parse_wc_l_stdout(&wc.stdout)
}

#[cfg(windows)]
fn count_tree_entries_windows(path: &str) -> Result<usize, String> {
    let lit = path.replace('\'', "''");
    let ps = format!(
        "(Get-ChildItem -LiteralPath '{}' -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object).Count",
        lit
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .output()
        .map_err(|e| format!("powershell: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "powershell exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|e| format!("invalid count output: {}", e))
}

/// Rough tree entry count for progress totals (Linux: `find DIR -printf '\n' | wc -l` when GNU find is available, else `find DIR | wc -l`; macOS/other Unix: `find DIR | wc -l`; Windows: PowerShell `Get-ChildItem -Recurse`). Scope is not identical to the indexer’s classified file count.
pub fn count_tree_entries_fast(path: &str) -> Result<usize, String> {
    #[cfg(windows)]
    {
        return count_tree_entries_windows(path);
    }
    #[cfg(all(unix, target_os = "linux"))]
    {
        return count_find_printf_wc(path).or_else(|_| count_find_pipe_wc(path));
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        return count_find_pipe_wc(path);
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("tree entry count is not supported on this target".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIndexAction {
    Skip,
    Update,
    Insert,
}

/// Classify a walkdir entry for Phase 1 file indexing. Returns `None` if the path is not indexable.
pub fn classify_dir_entry_for_indexing(
    entry: &DirEntry,
    existing_files: &HashMap<String, ExistingFileEntry>,
) -> Option<FileIndexAction> {
    let fpath = match entry.path().canonicalize() {
        Ok(fp) => {
            let path_str = fp.to_string_lossy().to_string();
            if path_str.starts_with("\\\\?\\") {
                path_str[4..].to_string()
            } else {
                path_str
            }
        }
        Err(_) => return None,
    };

    let meta = match std::fs::metadata(&fpath) {
        Ok(m) if m.is_file() => m,
        _ => return None,
    };

    let fmodified = match meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()))
    {
        Some(time) => time,
        None => return None,
    };

    if let Some(existing) = existing_files.get(&fpath) {
        if existing.moddate != fmodified {
            Some(FileIndexAction::Update)
        } else {
            Some(FileIndexAction::Skip)
        }
    } else {
        Some(FileIndexAction::Insert)
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

fn format_progress_pair(visit_index: usize, progress_display_total: Option<usize>) -> String {
    match progress_display_total {
        Some(t) => format!("{}/{}", visit_index, t),
        None => format!("{}", visit_index),
    }
}

const FTS_SQL_AUTOMERGE_8: &str =
    "INSERT INTO searchabletext(searchabletext, rank) VALUES('automerge', 8)";
const FTS_SQL_REBUILD: &str = "INSERT INTO searchabletext(searchabletext) VALUES('rebuild')";

pub fn fts_finalize_after_text_indexing(conn: &Connection) -> Result<(), String> {
    conn.execute(FTS_SQL_AUTOMERGE_8, [])
        .map_err(|e| format!("FTS automerge(8): {}", e))?;
    conn.execute(FTS_SQL_REBUILD, [])
        .map_err(|e| format!("FTS rebuild: {}", e))?;
    Ok(())
}

fn fts_remove_document_for_path(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
) -> Result<(), String> {
    let id_opt: Option<i64> = match tx.query_row(
        "SELECT id FROM documents WHERE path = ?1",
        params![path],
        |r| r.get(0),
    ) {
        Ok(id) => Some(id),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(format!("documents id lookup: {}", e)),
    };
    if let Some(doc_id) = id_opt {
        tx.execute(
            "INSERT INTO searchabletext(searchabletext, rowid) VALUES('delete', ?1)",
            params![doc_id],
        )
        .map_err(|e| format!("FTS delete doc {}: {}", doc_id, e))?;
        tx.execute("DELETE FROM documents WHERE id = ?1", params![doc_id])
            .map_err(|e| format!("delete documents row: {}", e))?;
    }
    Ok(())
}

struct PreparedFileUpdate {
    path_db: String,
    fsize: u64,
    fmodified: u64,
    fhash: Vec<u8>,
    filename: String,
    visit_index: usize,
}

/// Process updated files in batch with transaction - files table only (no text extraction)
pub fn process_batch_updates_files_only(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_update: &[(DirEntry, usize)],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config,
    progress_display_total: Option<usize>,
) -> Result<(), String> {
    if files_to_update.is_empty() {
        return Ok(());
    }

    let fts_batch = config.processing.fts_update_batch_size.max(1);

    for batch in files_to_update.chunks(fts_batch) {
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let mut prepared: Vec<PreparedFileUpdate> = Vec::new();

        for (entry, visit_index) in batch.iter() {
            if *stop_flag.lock().unwrap() {
                return Ok(());
            }

            let filename = entry
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            if let Some(ref callback) = status_callback {
                let pair = format_progress_pair(*visit_index, progress_display_total);
                callback(&format!("Hashing changed files {}: {}", pair, filename));
            }

            if let Some(ref progress_cb) = progress_callback {
                progress_cb(*visit_index);
            }

            let fpath = match entry.path().canonicalize() {
                Ok(fp) => {
                    let path_str = fp.to_string_lossy().to_string();
                    if path_str.starts_with("\\\\?\\") {
                        std::ffi::OsString::from(&path_str[4..])
                    } else {
                        fp.into_os_string()
                    }
                }
                Err(_) => continue,
            };

            let meta = match std::fs::metadata(&fpath) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            let fsize = meta.len();
            let fmodified = meta
                .modified()
                .map_err(|e| format!("Failed to get modified time: {}", e))?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("Failed to calculate duration: {}", e))?
                .as_secs();

            let fhash = match get_file_hash(fsize, fpath.clone(), config.processing.hash_length) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "Warning: Skipping file (cannot hash) {}: {}",
                        fpath.to_string_lossy(),
                        e
                    );
                    continue;
                }
            };

            if *stop_flag.lock().unwrap() {
                return Ok(());
            }

            prepared.push(PreparedFileUpdate {
                path_db: fpath.to_string_lossy().into_owned(),
                fsize,
                fmodified,
                fhash,
                filename,
                visit_index: *visit_index,
            });
        }

        if prepared.is_empty() {
            continue;
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for row in &prepared {
            if *stop_flag.lock().unwrap() {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            if let Some(ref callback) = status_callback {
                let pair = format_progress_pair(row.visit_index, progress_display_total);
                callback(&format!(
                    "Applying index updates {}: {}",
                    pair, row.filename
                ));
            }

            tx.execute(
                "UPDATE files SET size = ?1, moddate = ?2, hash = ?3 WHERE path = ?4",
                params![row.fsize, row.fmodified, row.fhash, row.path_db],
            )
            .map_err(|e| format!("Failed to update file record: {}", e))?;

            fts_remove_document_for_path(&tx, &row.path_db).map_err(|e| {
                format!(
                    "Failed to remove old document / FTS entry for {}: {}",
                    row.path_db, e
                )
            })?;
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Process new files in batch with transaction - files table only (no text extraction)
pub fn process_batch_inserts_files_only(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[(DirEntry, usize)],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config,
    progress_display_total: Option<usize>,
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    let batch_size = config.processing.batch_size;

    // Process files in batches of batch_size
    for batch in files_to_insert.chunks(batch_size) {
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (entry, visit_index) in batch.iter() {
            // Check stop flag for early termination
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
                let pair = format_progress_pair(*visit_index, progress_display_total);
                callback(&format!("Indexing file metadata {}: {}", pair, filename));
            }
            
            // Update progress counter
            if let Some(ref progress_cb) = progress_callback {
                progress_cb(*visit_index);
            }

            let fpath = match entry.path().canonicalize() {
                Ok(fp) => {
                    let path_str = fp.to_string_lossy().to_string();
                    // Remove Windows UNC prefix \\?\
                    if path_str.starts_with("\\\\?\\") {
                        std::ffi::OsString::from(&path_str[4..])
                    } else {
                        fp.into_os_string()
                    }
                },
                Err(_) => continue,
            };

            let meta = match std::fs::metadata(&fpath) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            let fsize = meta.len();
            let fmodified = meta.modified()
                .map_err(|e| format!("Failed to get modified time: {}", e))?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("Failed to calculate duration: {}", e))?
                .as_secs();

            let fhash = match get_file_hash(fsize, fpath.clone(), config.processing.hash_length) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "Warning: Skipping file (cannot hash) {}: {}",
                        fpath.to_string_lossy(),
                        e
                    );
                    continue;
                }
            };

            let fname = entry.path().file_name().unwrap().to_os_string();

            // Insert into files table
            tx.execute(
                "INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5)",
                params![fname.to_str(), fpath.to_string_lossy(), fsize, fmodified, fhash]
            ).map_err(|e| format!("Failed to insert file record: {}", e))?;
        }

        // Update status with current file
        if let Some(ref callback) = status_callback {
            callback("Committing file updates to database…");
        }   

        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    
    Ok(())
}

pub fn cleanup_stale_index_entries(
    conn_mutex: &Arc<Mutex<Connection>>,
    stale_paths: &[String],
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
) -> Result<usize, String> {
    if stale_paths.is_empty() {
        return Ok(0);
    }

    let conn = conn_mutex.lock().unwrap();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Failed to begin stale cleanup transaction: {}", e))?;

    let mut deleted_count = 0usize;
    for path in stale_paths {
        if *stop_flag.lock().unwrap() {
            let _ = tx.commit();
            drop(conn);
            return Ok(deleted_count);
        }

        if let Some(ref callback) = status_callback {
            callback(&format!("Removing stale index entry: {}", path));
        }

        fts_remove_document_for_path(&tx, path).map_err(|e| {
            format!(
                "Failed to remove stale document / FTS entry for {}: {}",
                path, e
            )
        })?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])
            .map_err(|e| format!("Failed to delete stale file record {}: {}", path, e))?;
        deleted_count += 1;
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit stale cleanup transaction: {}", e))?;

    if deleted_count > 0 && !*stop_flag.lock().unwrap() {
        if let Some(ref callback) = status_callback {
            callback("Rebuilding FTS index after stale cleanup...");
        }
        fts_finalize_after_text_indexing(&conn)?;
    }

    Ok(deleted_count)
}

/// Process text indexing for files - writes `documents`; FTS rebuilt in `fts_finalize_after_text_indexing`.
pub fn process_text_indexing(
    conn_mutex: &Arc<Mutex<Connection>>,
    stop_flag: &Arc<Mutex<bool>>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config
) -> Result<(), String> {
    let max_size = config.processing.maximum_text_file_size;
    let batch_size = config.processing.batch_size;
    let batch_limit = batch_size as i64;

    if let Some(ref callback) = status_callback {
        callback("Counting files pending text index…");
    }

    let total_files: usize = {
        let conn = conn_mutex.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM files f
             LEFT JOIN documents d ON f.path = d.path
             WHERE d.path IS NULL AND f.size <= ?1",
            [max_size],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count pending text files: {}", e))?
    };

    let mut cursor_path = String::new();
    let mut global_index: usize = 0;

    loop {
        if *stop_flag.lock().unwrap() {
            return Ok(());
        }

        let batch: Vec<(String, String, u64)> = {
            let conn = conn_mutex.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT f.name, f.path, f.size FROM files f
                     LEFT JOIN documents d ON f.path = d.path
                     WHERE d.path IS NULL AND f.size <= ?1
                       AND (?2 = '' OR f.path > ?2)
                     ORDER BY f.path
                     LIMIT ?3",
                )
                .map_err(|e| format!("Failed to prepare text indexing query: {}", e))?;
            let rows = stmt
                .query_map(
                    rusqlite::params![max_size, cursor_path.as_str(), batch_limit],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u64>(2)?,
                        ))
                    },
                )
                .map_err(|e| format!("Failed to query files for text indexing: {}", e))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to read file row: {}", e))?
        };

        if batch.is_empty() {
            break;
        }

        let last_path = batch.last().unwrap().1.clone();
        cursor_path = last_path;

        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (fname, fpath, _fsize) in batch.iter() {
            if *stop_flag.lock().unwrap() {
                let _ = tx.commit();
                drop(conn);
                return Ok(());
            }

            global_index += 1;

            if let Some(ref callback) = status_callback {
                callback(&format!(
                    "Extracting text for search indexing {}/{}: {}",
                    global_index, total_files, fname
                ));
            }

            if let Some(ref progress_cb) = progress_callback {
                progress_cb(global_index);
            }

            let path = std::path::Path::new(fpath.as_str());
            let default_ext = OsString::new();
            let file_extension = path
                .extension()
                .unwrap_or(&default_ext)
                .to_ascii_lowercase()
                .to_str()
                .unwrap_or("")
                .to_string();
            let ext_str = file_extension.as_str();

            let text_result = if PLAINTEXT_EXTENSIONS_LIST.contains(&ext_str) {
                match read_to_string(fpath) {
                    Ok(file_string) => {
                        let trimmed_file_string =
                            safe_truncate_string(&file_string, config.processing.maximum_text_size);
                        Some(trimmed_file_string)
                    }
                    Err(_e) => None,
                }
            } else if SUPPORTED_DOCUMENT_EXTENSIONS_LIST.contains(&ext_str) {
                match extract_document_text(&std::ffi::OsString::from(fpath), ext_str) {
                    Ok(extracted_text) => {
                        if !extracted_text.trim().is_empty() {
                            Some(safe_truncate_string(
                                &extracted_text,
                                config.processing.maximum_text_size,
                            ))
                        } else {
                            None
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to extract text from document {}: {}",
                            fpath, e
                        );
                        None
                    }
                }
            } else {
                None
            };

            if let Some(text_content) = text_result {
                if let Err(e) = tx.execute(
                    "INSERT OR REPLACE INTO documents(name, path, text) VALUES (?1, ?2, ?3)",
                    params![fname, fpath, text_content],
                ) {
                    eprintln!("Warning: Failed to insert document row for {}: {}", fpath, e);
                }
            }
        }

        if let Some(ref callback) = status_callback {
            callback("Rebuilding FTS index after text addition...");
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    if total_files > 0 && !*stop_flag.lock().unwrap() {
        let conn = conn_mutex.lock().unwrap();
        fts_finalize_after_text_indexing(&conn)?;
    }

    Ok(())
}
