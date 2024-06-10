use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;
use std::sync::{Mutex, Arc};

use sha2::{Sha256, Digest};
use walkdir::{WalkDir, DirEntry};
use tqdm;
use rusqlite::{params, Connection};
use dpc_pariter::IteratorExt as _;

const HASHLEN:usize = 1024*8;


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

fn process_entry(conn_mutex: &Arc<Mutex<Connection>>, entry: DirEntry) {
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
        
        let fsize = meta.len();
        let fmodified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();

        // let fhash = get_file_hash(fsize, fpath.clone())?;
        let fhash_result = get_file_hash(fsize, fpath.clone());
        let fhash = match fhash_result {
            Ok(fh) => fh,
            Err(error) => {
                println!("Error digesting hash: {:?}", error);
                return;
            },
        };
        // let fhash = b"";
        
        let query = "INSERT INTO files VALUES (?1,?2,?3,?4,?5)";
        let conn = conn_mutex.lock().unwrap();
        let mut stmt = conn.prepare_cached(query).unwrap();
        let fname = entry.path().file_name().unwrap().to_os_string();

        // stmt.execute(params![fname.to_str(), fpath.to_str(), fsize, fmodified, fhash])?;
        let stmt_result = stmt.execute(params![fname.to_str(), fpath.to_str(), fsize, fmodified, fhash]);
        match stmt_result {
            Ok(us) => us,
            Err(error) => {
                println!("Error with sqlite transaction: {:?}", error);
                return;
            },
        };
    }
}

fn main() {
    let path: &str = "Y:\\";
    let db_path: &str = "YDrive.db";

    let conn = Connection::open(db_path).unwrap();
    // let conn = Connection::open_in_memory().unwrap();
    // PRAGMA cache_size is in number of pages with 1024 byte page size by default
    conn.execute_batch(
        "PRAGMA journal_mode = OFF;
              PRAGMA synchronous = 0;
              PRAGMA cache_size = 10000;
              PRAGMA locking_mode = EXCLUSIVE;
              PRAGMA temp_store = MEMORY;",
    )
    .expect("PRAGMA");
    conn.execute("CREATE TABLE IF NOT EXISTS files (
                        name    TEXT,
                        path    TEXT,
                        size    INTEGER,
                        moddate INTEGER,
                        hash    BLOB)", ()).unwrap();

    let conn_mutex = Arc::new(Mutex::new(conn));

    tqdm::tqdm(WalkDir::new(path).into_iter()).parallel_map(move |entry| {
        match entry {
            Ok(e) => process_entry(&conn_mutex, e),
            Err(error) => {
                println!("Error with directory walk: {:?}", error);
                return;
            },
        };
    }).for_each(drop);

/*
select name, hash, count(hash) as cnt from files group by hash
ORDER BY cnt DESC;
*/

}