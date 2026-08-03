//! End-to-end index encryption through the public API: the process-global
//! key, a real indexing run over a real tree, and the enable→disable
//! rebuild cycle.
//!
//! Lives in its own integration-test binary on purpose: it mutates the
//! process-global key, which unit tests (sharing one process) must never
//! do. Everything runs inside a single #[test] so the key transitions are
//! strictly ordered.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::{IndexingService, IndexingStatus};
use quicksearch_core::security::{derive_key, salt_from_hex};

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "quicksearch-enc-{}-{}-{}",
        tag,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Run one full index over `root` and wait for the completion marker,
/// reading it through the keyed open so the poll works on encrypted DBs.
fn index_once(root: &Path, db_path: &Path, config: &Config) {
    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db_path.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut done = false;
    while Instant::now() < deadline {
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        if db_path.exists() {
            if let Ok(conn) = db::open_existing(&db_path.to_string_lossy(), false) {
                if quicksearch_core::db::repo::get_last_full_index(&conn).is_some() {
                    done = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(done, "indexing did not finish within the timeout");
    service.stop_indexing().unwrap();
}

fn header(db_path: &Path) -> [u8; 16] {
    let bytes = std::fs::read(db_path).unwrap();
    bytes[..16].try_into().unwrap()
}

fn match_count(db_path: &Path, term: &str) -> i64 {
    let conn = db::open_existing(&db_path.to_string_lossy(), false).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn encrypted_index_lifecycle() {
    let root = tmp_dir("tree");
    let data = tmp_dir("db");
    let db_path = data.join("index.sqlite");
    std::fs::write(root.join("note.txt"), "the zebrapayload roams the index").unwrap();
    std::fs::write(root.join("other.txt"), "unrelated content here").unwrap();

    let config = Config::default();
    let salt = salt_from_hex("00112233445566778899aabbccddeeff").unwrap();
    let key = derive_key("hunter2", &salt);
    let wrong_key = derive_key("hunter3", &salt);

    // --- Enabled: index is created encrypted and searchable. ---
    db::set_process_key(Some(key.clone()));
    index_once(&root, &db_path, &config);
    assert_ne!(
        &header(&db_path),
        b"SQLite format 3\0",
        "protected index must not carry the plaintext SQLite header"
    );
    assert_eq!(match_count(&db_path, "zebrapayload"), 1);

    // Raw bytes must not leak the indexed content anywhere in the file.
    let raw = std::fs::read(&db_path).unwrap();
    assert!(
        !raw.windows(b"zebrapayload".len()).any(|w| w == b"zebrapayload"),
        "plaintext content leaked into the encrypted file"
    );

    // --- Optimizing a keyed index: VACUUM keeps it encrypted. ---
    //
    // VACUUM rewrites the whole file through a temporary database that
    // SQLCipher has to key from the main one. If it did not, the rewrite would
    // hand back a plaintext index — silently, and only for protected users.
    //
    // The slack is manufactured: this tree is two files, and `maintain` only
    // rewrites a file with something to reclaim.
    {
        let conn = db::open_existing(&db_path.to_string_lossy(), true).unwrap();
        conn.execute_batch(
            "INSERT INTO files (name, path, parent, size, mtime, type, basic_state, content_state)
             WITH RECURSIVE n(i) AS (
                 SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000
             )
             SELECT 'p' || i, '/pad/' || i, '/pad', 0, 0, 0, 1, 3 FROM n;
             DELETE FROM files WHERE parent = '/pad';",
        )
        .unwrap();
        drop(conn);

        let conn = db::open::open_maintenance(&db_path.to_string_lossy()).unwrap();
        let dir = data.to_string_lossy().into_owned();
        assert!(
            quicksearch_core::db::repo::maintain(&conn, &dir).unwrap(),
            "that much slack should have been reclaimed"
        );
        drop(conn);

        assert_ne!(
            &header(&db_path),
            b"SQLite format 3\0",
            "the vacuum's replacement file must still be encrypted"
        );
        assert_eq!(
            match_count(&db_path, "zebrapayload"),
            1,
            "and still searchable under the same key"
        );
    }

    // --- Wrong password / no password: tagged error, file intact. ---
    let before = std::fs::read(&db_path).unwrap();
    db::set_process_key(Some(wrong_key.clone()));
    let err = db::verify_process_key(&db_path.to_string_lossy()).unwrap_err();
    assert!(err.starts_with(db::KEY_MISMATCH_PREFIX), "got: {err}");
    db::set_process_key(None);
    let err = db::verify_process_key(&db_path.to_string_lossy()).unwrap_err();
    assert!(err.starts_with(db::KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(
        before,
        std::fs::read(&db_path).unwrap(),
        "failed unlocks must never modify the index"
    );

    // --- A stale schema must not read as a locked index. ---
    //
    // The reported failure: after a schema bump, a password-protected install
    // could not start at all — the correct password was rejected with
    // "not a compatible QuickSearch index (schema v4 expected)", because the
    // unlock gate verified the key by opening the index the way a *consumer*
    // does, which also insists the schema be current. An unprotected install
    // in the same state starts and rebuilds on its first run; the protected
    // one had no way past the gate.
    //
    // Whether the schema is current belongs to the indexer, which answers it
    // by wiping and rebuilding. Unlocking only has to answer "does this key
    // open the file?".
    {
        db::set_process_key(Some(key.clone()));
        // Age the stored schema, exactly as a version bump would.
        let conn = db::open_existing(&db_path.to_string_lossy(), true).unwrap();
        conn.execute(
            "UPDATE schema_info SET value = '1' WHERE key = 'version'",
            [],
        )
        .unwrap();
        drop(conn);

        // The right password still unlocks...
        db::verify_process_key(&db_path.to_string_lossy()).expect(
            "a stale schema must not make the correct password look wrong",
        );
        // ...and the wrong one is still refused, with the same tagged error —
        // the relaxation must not have turned the check into a rubber stamp.
        db::set_process_key(Some(wrong_key.clone()));
        let err = db::verify_process_key(&db_path.to_string_lossy()).unwrap_err();
        assert!(err.starts_with(db::KEY_MISMATCH_PREFIX), "got: {err}");

        // Consumers still refuse a stale index, which is what sends the
        // indexer down its rebuild path.
        db::set_process_key(Some(key.clone()));
        let err = db::open_existing(&db_path.to_string_lossy(), false).unwrap_err();
        assert!(err.contains("not a compatible QuickSearch index"), "got: {err}");

        // And the rebuild comes back encrypted and searchable under the same
        // key, so the whole path a real user walks is covered.
        index_once(&root, &db_path, &config);
        assert_ne!(&header(&db_path), b"SQLite format 3\0");
        assert_eq!(match_count(&db_path, "zebrapayload"), 1);

        // Hand the next section the unkeyed state it expects.
        db::set_process_key(None);
    }

    // --- Disable: delete + rebuild produces a plaintext index. ---
    let service = IndexingService::new();
    service
        .delete_index_for_rebuild(&db_path.to_string_lossy())
        .unwrap();
    assert!(!db_path.exists());
    index_once(&root, &db_path, &config);
    assert_eq!(&header(&db_path), b"SQLite format 3\0");
    assert_eq!(match_count(&db_path, "zebrapayload"), 1);

    // The old key no longer opens it, with the precise "not encrypted"
    // diagnosis (the crash-between-config-save-and-rebuild scenario).
    db::set_process_key(Some(key));
    let err = db::verify_process_key(&db_path.to_string_lossy()).unwrap_err();
    assert!(err.starts_with(db::KEY_MISMATCH_PREFIX), "got: {err}");
    assert!(err.contains("not encrypted"), "got: {err}");

    db::set_process_key(None);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&data).ok();
}
