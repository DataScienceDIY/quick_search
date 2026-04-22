use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, Arc};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;
use std::collections::HashMap;

use sha2::{Sha256, Digest};
use walkdir::{DirEntry, WalkDir};
use rusqlite::Connection;

use crate::config::Config;
use crate::db::repo::{self, NewFile};
use crate::extract::Registry;
use crate::indexing::should_abort;
use crate::mime::{guess_mime, mime_to_type, FileType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingFileEntry {
    pub mtime: u64,
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

/// Load path and mtime per row for incremental classification (hash/size loaded only when updating a file).
pub fn load_existing_files(conn: &Connection) -> Result<HashMap<String, ExistingFileEntry>, rusqlite::Error> {
    let mut existing_files = HashMap::new();
    let mut stmt = conn.prepare("SELECT path, mtime FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            ExistingFileEntry {
                mtime: row.get(1)?,
            },
        ))
    })?;

    for row in rows {
        let (path, entry) = row?;
        existing_files.insert(path, entry);
    }

    Ok(existing_files)
}

/// Derive (inode, device_id) from a `std::fs::Metadata` on platforms that
/// expose them. Returns `(None, None)` on Windows and other non-Unix targets.
fn inode_and_device(_meta: &std::fs::Metadata) -> (Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (Some(_meta.ino()), Some(_meta.dev()))
    }
    #[cfg(not(unix))]
    {
        (None, None)
    }
}

/// Parent directory of a path as a UTF-8 string, empty if root.
fn parent_str(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
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
        if existing.mtime != fmodified {
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

/// Nudge FTS5 to merge its index segments. Best-effort optimization; any
/// error is logged but not fatal.
pub fn fts_finalize_after_text_indexing(conn: &Connection) -> Result<(), String> {
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('automerge', 8)",
        [],
    ) {
        eprintln!("Warning: FTS automerge failed (non-fatal): {}", e);
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

            let p = Path::new(row.path_db.as_str());
            let guessed_mime = guess_mime(p);
            let ftype = guessed_mime
                .as_deref()
                .map(mime_to_type)
                .unwrap_or(FileType::EMPTY);
            let _ = repo::update_file_basic(
                &tx,
                &row.path_db,
                row.fsize,
                row.fmodified,
                Some(row.fhash.as_slice()),
                guessed_mime.as_deref(),
                ftype,
            )
            .map_err(|e| {
                format!(
                    "Failed to update file record + clear stale content for {}: {}",
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
            let fname_str = fname.to_string_lossy().into_owned();
            let fpath_str = fpath.to_string_lossy().into_owned();
            let parent = parent_str(&fpath_str);
            let (inode, device_id) = inode_and_device(&meta);
            let guessed_mime = guess_mime(Path::new(&fpath_str));
            let ftype = guessed_mime
                .as_deref()
                .map(mime_to_type)
                .unwrap_or(FileType::EMPTY);

            repo::insert_file(
                &tx,
                &NewFile {
                    name: &fname_str,
                    path: &fpath_str,
                    parent: &parent,
                    size: fsize,
                    mtime: fmodified,
                    inode,
                    device_id,
                    mime: guessed_mime.as_deref(),
                    ftype,
                    hash: Some(fhash.as_slice()),
                },
            )
            .map_err(|e| format!("Failed to insert file record: {}", e))?;
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
    suspend_flag: &Arc<AtomicBool>,
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
        if should_abort(stop_flag, suspend_flag) {
            let _ = tx.commit();
            drop(conn);
            return Ok(deleted_count);
        }

        if let Some(ref callback) = status_callback {
            callback(&format!("Removing stale index entry: {}", path));
        }

        if repo::delete_file_by_path(&tx, path).map_err(|e| {
            format!(
                "Failed to remove stale index entry for {}: {}",
                path, e
            )
        })? {
            deleted_count += 1;
        }
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit stale cleanup transaction: {}", e))?;

    if deleted_count > 0 && !should_abort(stop_flag, suspend_flag) {
        if let Some(ref callback) = status_callback {
            callback("Optimizing FTS index after stale cleanup...");
        }
        fts_finalize_after_text_indexing(&conn)?;
    }

    Ok(deleted_count)
}

/// Process text indexing for all files with `content_state = pending`. For
/// each file dispatches to the configured extractor [`Registry`], writes the
/// extracted text + properties via the repo helpers, then flips the row's
/// `content_state` to done/failed/na so it won't be retried next run.
pub fn process_text_indexing(
    conn_mutex: &Arc<Mutex<Connection>>,
    stop_flag: &Arc<Mutex<bool>>,
    suspend_flag: &Arc<AtomicBool>,
    status_callback: Option<Box<dyn Fn(&str) + Send + Sync>>,
    progress_callback: Option<Box<dyn Fn(usize) + Send + Sync>>,
    config: &Config,
) -> Result<(), String> {
    let registry = Registry::default_set();
    let max_size = config.processing.maximum_text_file_size;
    let batch_size = config.processing.batch_size;
    let batch_limit = batch_size as i64;

    if let Some(ref callback) = status_callback {
        callback("Counting files pending text index…");
    }

    let total_files: usize = {
        let conn = conn_mutex.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = 0 AND size <= ?1",
            [max_size],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count pending text files: {}", e))?
    };

    let mut cursor_id: i64 = 0;
    let mut global_index: usize = 0;

    loop {
        if should_abort(stop_flag, suspend_flag) {
            return Ok(());
        }

        let batch: Vec<(i64, String, String, Option<String>)> = {
            let conn = conn_mutex.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, path, mime FROM files
                      WHERE content_state = 0 AND size <= ?1 AND id > ?2
                      ORDER BY id
                      LIMIT ?3",
                )
                .map_err(|e| format!("Failed to prepare text indexing query: {}", e))?;
            let rows = stmt
                .query_map(
                    rusqlite::params![max_size, cursor_id, batch_limit],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
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

        cursor_id = batch.last().unwrap().0;

        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for (file_id, fname, fpath, fmime) in batch.iter() {
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

            let mime_str = fmime.clone().or_else(|| guess_mime(Path::new(fpath)));
            let result = match mime_str.as_deref() {
                Some(m) => registry.extract(Path::new(fpath), m),
                None => Ok(None),
            };
            match result {
                Ok(Some(mut content)) => {
                    // Truncate extracted text to configured limit before storage.
                    if content.text.len() > config.processing.maximum_text_size {
                        content.text =
                            safe_truncate_string(&content.text, config.processing.maximum_text_size);
                    }
                    let props = content.properties_sorted();
                    if let Err(e) =
                        repo::set_content_done(&tx, *file_id, fname, &content.text, &props)
                    {
                        eprintln!("Warning: set_content_done for {}: {}", fpath, e);
                    }
                }
                Ok(None) => {
                    if let Err(e) = repo::set_content_na(&tx, *file_id) {
                        eprintln!("Warning: set_content_na for {}: {}", fpath, e);
                    }
                }
                Err(reason) => {
                    if let Err(e) = repo::set_content_failed(&tx, *file_id, &reason) {
                        eprintln!("Warning: set_content_failed for {}: {}", fpath, e);
                    }
                }
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    if total_files > 0 && !should_abort(stop_flag, suspend_flag) {
        let conn = conn_mutex.lock().unwrap();
        fts_finalize_after_text_indexing(&conn)?;
    }

    Ok(())
}
