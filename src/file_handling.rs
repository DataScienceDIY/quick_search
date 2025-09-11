use std::sync::{Mutex, Arc};
use std::ffi::OsString;
use std::fs::{File,read_to_string};
use std::io::{Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;

use sha2::{Sha256, Digest};
use walkdir::DirEntry;
use rusqlite::{params, Connection};

use crate::document_extraction::extract_document_text;


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