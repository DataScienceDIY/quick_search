//! End-to-end index encryption through the public API. Lives in its own
//! integration-test binary: it mutates the process-global key, which unit
//! tests must never do; a single #[test] keeps the transitions ordered.

use std::path::Path;

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::indexing::IndexingService;
use quicksearch_core::security::{derive_key, salt_from_hex};

mod common;
use common::scratch_dir as tmp_dir;

/// Index `root` and wait for the marker through the keyed open. The marker
/// is deliberately not cleared: each rebuild starts from a fresh file.
fn index_once(root: &Path, db_path: &Path, config: &Config) {
    common::IndexOnce {
        db: db_path,
        roots: vec![root.to_string_lossy().into_owned()],
        config,
        fresh_marker: false,
        encrypted: true,
    }
    .run()
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
        !raw.windows(b"zebrapayload".len())
            .any(|w| w == b"zebrapayload"),
        "plaintext content leaked into the encrypted file"
    );

    // --- Optimizing a keyed index: VACUUM keeps it encrypted. ---
    //
    // VACUUM rewrites the file through a temp database SQLCipher must key
    // from the main one; otherwise the rewrite hands back a plaintext index.
    // The slack is manufactured: `maintain` only rewrites with something to
    // reclaim.
    {
        let conn = db::open_existing(&db_path.to_string_lossy(), true).unwrap();
        conn.execute_batch(
            "INSERT INTO files (name, parent, size, mtime, type, content_state)
             WITH RECURSIVE n(i) AS (
                 SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000
             )
             SELECT 'p' || i, '/pad/', 0, 0, 0, 3 FROM n;
             DELETE FROM files WHERE parent = '/pad/';",
        )
        .unwrap();
        drop(conn);

        let conn = db::open::open_maintenance(&db_path.to_string_lossy()).unwrap();
        assert!(
            quicksearch_core::db::repo::maintain(&conn, &db_path.to_string_lossy()).unwrap(),
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
    // After a schema bump the unlock gate rejected the correct password by
    // opening the index the way a *consumer* does, which insists the schema
    // be current — that belongs to the indexer; unlocking only answers
    // "does this key open the file?".
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

        db::verify_process_key(&db_path.to_string_lossy())
            .expect("a stale schema must not make the correct password look wrong");
        db::set_process_key(Some(wrong_key.clone()));
        let err = db::verify_process_key(&db_path.to_string_lossy()).unwrap_err();
        assert!(err.starts_with(db::KEY_MISMATCH_PREFIX), "got: {err}");

        db::set_process_key(Some(key.clone()));
        let err = db::open_existing(&db_path.to_string_lossy(), false).unwrap_err();
        assert!(
            err.contains("not a compatible QuickSearch index"),
            "got: {err}"
        );

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
