use std::sync::{Mutex, Arc};

use walkdir::WalkDir;
use tqdm;
use rusqlite::Connection;
use dioxus::prelude::*;
use dpc_pariter::IteratorExt as _;

mod frontend;
mod file_handling;
mod document_extraction;

fn main() {
    // launch(frontend::App);
    let path: &str = "G:\\datasets\\preprocessed_ch_100";
    let db_path: &str = "GDrive.db";

    let conn = Connection::open(db_path).unwrap();
    // let conn = Connection::open_in_memory().unwrap();
    // PRAGMA cache_size is in number of pages with 1024 byte page size by default
    conn.execute_batch(
        "PRAGMA journal_mode = OFF;
              PRAGMA synchronous = 0;
              PRAGMA cache_size = 10000;
              PRAGMA temp_store = MEMORY;",
    )
    //               PRAGMA locking_mode = EXCLUSIVE;
    .expect("PRAGMA");
    conn.execute("CREATE TABLE IF NOT EXISTS files (
                        name    TEXT,
                        path    TEXT,
                        size    INTEGER,
                        moddate INTEGER,
                        hash    BLOB);", ()).unwrap();
    // https://sqlite.org/fts5.html
    conn.execute("CREATE VIRTUAL TABLE IF NOT EXISTS searchabletext USING fts5 (name, path, text, tokenize = 'trigram');", ()).unwrap();

    let conn_mutex = Arc::new(Mutex::new(conn));

    tqdm::tqdm(WalkDir::new(path).into_iter()).parallel_map(move |entry| {
        match entry {
            Ok(e) => file_handling::process_entry(&conn_mutex, e),
            Err(error) => {
                println!("Error with directory walk: {:?}", error);
                return;
            },
        };
    }).for_each(drop);

/*
SELECT name, hash, count(*) as cnt FROM files GROUP BY hash ORDER BY cnt DESC;

SELECT name, path, text, snippet(searchabletext, 2 , "<b>", "</b>", "...", 64) as "snip" FROM searchabletext WHERE text MATCH 'Terrasound' LIMIT 100;
*/

}