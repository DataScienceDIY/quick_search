// use std::borrow::Borrow;
use std::ffi::OsString;
use std::time::Instant;
// use std::path::Path;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;

use sha2::{Sha256, Digest};
use walkdir::WalkDir;
use tqdm;
use rusqlite::{params, Connection};


const HASHLEN:usize = 1024*8;

// struct Finfo {
//     name: String,
//     path: String,
//     size: u64,
//     modified: u64,
//     hash: [u8; 64]
// }

fn get_file_hash(size: u64, path: OsString) -> Result<Vec<u8>, std::io::Error> {
    let mut hasher = Sha256::new();
    let mut f = File::open(path)?;
    let mut data = [0u8; 8+2*HASHLEN];
    data[..8].copy_from_slice(&size.to_le_bytes());
    if size > HASHLEN as u64 {
        let mut file_start_block = [0u8; HASHLEN];
        f.read_exact(&mut file_start_block)?;
        for i in 0..HASHLEN {
            data[8+i] = file_start_block[i];
        }
        f.seek(SeekFrom::End(0 - HASHLEN as i64))?;
        let mut file_end_block = [0u8; HASHLEN];
        f.read_exact(&mut file_end_block)?;
        for i in 0..HASHLEN {
            let i: usize = i;
            data[8+i+HASHLEN] = file_end_block[i];
        }
    } else if size > 0 {
        let mut file_block = Vec::new();
        f.read_to_end(&mut file_block)?;
        for i in 0..(size as usize) {
            let i: usize = i;
            data[8+i] = file_block[i];
        }
    }
    hasher.update(data);
    Ok(hasher.finalize().to_vec())
        
}


fn main() {
    let path: &str = "G:\\datasets\\preprocessed_ch_100";
    let db_path: &str = "GDrive.db";

    let mut conn = Connection::open(db_path).unwrap();
    // conn.execute_batch(
    //     "PRAGMA journal_mode = OFF;
    //           PRAGMA synchronous = 0;
    //           PRAGMA cache_size = 1000000;
    //           PRAGMA locking_mode = EXCLUSIVE;
    //           PRAGMA temp_store = MEMORY;",
    // )
    // .expect("PRAGMA");
    conn.execute("CREATE TABLE IF NOT EXISTS files (
                        name    TEXT,
                        path    TEXT,
                        size    INTEGER,
                        moddate INTEGER,
                        hash    BLOB)", ()).unwrap();
    let tx = conn.transaction().unwrap();
    let st1 = "INSERT INTO files VALUES (?1,?2,?3,?4,?5)";
    let mut stmt = tx.prepare(st1).unwrap();
    let mut filecount: i64 = 0;
    let start_time = Instant::now();

    for entry in tqdm::tqdm(WalkDir::new(path).into_iter().filter_map(|e| e.ok())) {
        let meta = entry.metadata().unwrap();
        if !meta.is_dir() {
            let fname = entry.path().file_name().unwrap().to_os_string();
            let fpath = entry.path().canonicalize().unwrap().into_os_string();
            let fsize = meta.len();
            let fmodified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
            let fhash = get_file_hash(fsize, fpath.clone()).unwrap();
            filecount += 1;
            stmt.execute(params![fname.to_str(), fpath.to_str(), fsize, fmodified, fhash]).unwrap();
            if filecount >= 100 {
                break;
            }
        }
    }

    let elapsed_time = start_time.elapsed();
    println!("{} files enumerated in {} seconds", filecount, elapsed_time.as_secs());
    println!("{} files per second", filecount as f32 / elapsed_time.as_secs() as f32);
}