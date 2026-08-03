//! Build a password-protected index over a directory, for exercising the
//! CLI binaries against a real encrypted index without the GUI.
//!
//!     cargo run -p quicksearch-core --example build_protected_index -- \
//!         <root> <db_path> <salt_hex> <password>

use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};
use quicksearch_core::security::{derive_key, salt_from_hex};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [root, db_path, salt_hex, password] = args.as_slice() else {
        eprintln!("usage: build_protected_index <root> <db_path> <salt_hex> <password>");
        std::process::exit(2);
    };
    let salt = salt_from_hex(salt_hex).expect("valid salt hex");
    db::set_process_key(Some(derive_key(password, &salt)));

    let mut config = Config::default();
    config.indexing.auto_index = false;
    let service = IndexingService::new();
    service
        .start_indexing(vec![root.clone()], db_path.clone(), config)
        .expect("indexing starts");

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(Instant::now() < deadline, "indexing timed out");
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if let Ok(conn) = db::open_existing(db_path, false) {
            if db::repo::get_last_full_index(&conn).is_some() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    service.stop_indexing().expect("clean stop");
    println!("protected index built at {}", db_path);
}
