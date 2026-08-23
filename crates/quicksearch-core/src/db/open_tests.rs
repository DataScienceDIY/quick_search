use super::*;

fn tmp_db_path() -> std::path::PathBuf {
    crate::testutil::scratch_dir("open").join("index.sqlite")
}

#[test]
fn fresh_db_gets_current_version() {
    let p = tmp_db_path();
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn reopen_is_idempotent() {
    let p = tmp_db_path();
    {
        let _ = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    }
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn older_versioned_db_is_wiped_and_recreated() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_info(key,value) VALUES('version','1')",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO files(name) VALUES('a.txt')", [])
            .unwrap();
    }
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "old rows should be wiped");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn legacy_layout_db_is_wiped_and_recreated() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE files (name TEXT, path TEXT, size INTEGER, moddate INTEGER, hash BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files VALUES ('a.txt', '/tmp/a.txt', 1, 2, X'00')",
            [],
        )
        .unwrap();
    }
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    // An unknown column name would parse-error here.
    conn.query_row(
        "SELECT content_state, type, mime FROM files LIMIT 0",
        [],
        |_| Ok(()),
    )
    .or_else(|e| {
        if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            Ok(())
        } else {
            Err(e)
        }
    })
    .unwrap();
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn tokenizer_drift_wipes_db() {
    let p = tmp_db_path();
    let first_effective = {
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
        let stored: String = conn
            .query_row(
                "SELECT value FROM schema_info WHERE key='tokenize'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        stored
    };
    let conn = open_or_recreate(p.to_str().unwrap(), "unicode61").unwrap();
    let files_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files_count, 0, "tokenizer drift should wipe rows");
    let new_stored: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='tokenize'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(first_effective, new_stored);
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn open_existing_reads_nondefault_tokenizer_without_wiping() {
    let p = tmp_db_path();
    {
        let conn = open_or_recreate(p.to_str().unwrap(), "unicode61").unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('note', '/', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO searchabletext (rowid, text) \
             VALUES (last_insert_rowid(), 'hello world')",
            [],
        )
        .unwrap();
    }

    let conn = open_existing(p.to_str().unwrap(), false).unwrap();
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        files, 1,
        "open_existing must not wipe a non-default-tokenizer DB"
    );
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // Proof we neither rewrote the FTS table nor reset schema_info.
    let tok: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='tokenize'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tok, "unicode61");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn open_or_recreate_creates_missing_parent_dirs() {
    let dir = crate::testutil::scratch_dir("mkdir");
    let db = dir.join("nested/deeper/index.sqlite");
    let conn = open_or_recreate(db.to_str().unwrap(), "trigram").unwrap();
    drop(conn);
    assert!(db.exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writable_opens_use_wal_and_it_persists() {
    let p = tmp_db_path();
    {
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
    // WAL is persistent in the file: a later read-only consumer sees it.
    let conn = open_existing(p.to_str().unwrap(), false).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Under `-DSQLITE_TEMP_STORE=2`, VACUUM builds the replacement index in
/// memory unless `temp_store` is explicitly `FILE`.
#[test]
fn maintenance_opens_keep_temporaries_on_disk() {
    let p = tmp_db_path();
    {
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let indexer: i64 = conn
            .query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexer, 2, "the indexer's own profile is MEMORY");
    }

    let conn = open_maintenance(p.to_str().unwrap()).unwrap();
    let store: i64 = conn
        .query_row("PRAGMA temp_store", [], |r| r.get(0))
        .unwrap();
    assert_eq!(store, 1, "maintenance must build its temporaries on disk");

    // The directory those temporaries land in is steerable — what keeps
    // them off a RAM-backed /tmp.
    let dir = p.parent().unwrap().to_string_lossy().into_owned();
    conn.execute_batch(&format!("PRAGMA temp_store_directory = '{}';", dir))
        .unwrap();
    let set: String = conn
        .query_row("PRAGMA temp_store_directory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(set, dir);
    conn.execute_batch("PRAGMA temp_store_directory = '';")
        .unwrap();

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Drives the GUI's "your index is being reset" modal: a false positive
/// announces a wipe that is not happening.
#[test]
fn index_needs_rebuild_only_when_the_schema_really_differs() {
    let p = tmp_db_path();
    assert!(
        !index_needs_rebuild(p.to_str().unwrap()),
        "no file yet is a fresh install, not a reset"
    );

    {
        let _ = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    }
    assert!(
        !index_needs_rebuild(p.to_str().unwrap()),
        "a current index is not going to be rebuilt"
    );

    {
        let conn = open_existing(p.to_str().unwrap(), true).unwrap();
        conn.execute(
            "UPDATE schema_info SET value = '1' WHERE key = 'version'",
            [],
        )
        .unwrap();
    }
    assert!(index_needs_rebuild(p.to_str().unwrap()));

    std::fs::remove_file(&p).ok();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
    }
    assert!(index_needs_rebuild(p.to_str().unwrap()));

    // Not a database at all: we cannot tell, so we say nothing.
    std::fs::write(&p, [0x5a; 4096]).unwrap();
    assert!(!index_needs_rebuild(p.to_str().unwrap()));

    std::fs::remove_file(&p).ok();
}

#[test]
fn open_existing_errors_on_missing_file() {
    let p = tmp_db_path();
    assert!(!p.exists());
    let res = open_existing(p.to_str().unwrap(), false);
    assert!(res.is_err(), "missing file must error, not be created");
    assert!(!p.exists(), "open_existing must not create the file");
}

#[test]
fn open_existing_errors_on_version_mismatch_without_wiping() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_info(key,value) VALUES('version','1')",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO files(name) VALUES('sentinel')", [])
            .unwrap();
    }
    let res = open_existing(p.to_str().unwrap(), false);
    assert!(res.is_err(), "stale schema version must error");
    let conn = Connection::open(&p).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "open_existing must never delete on version mismatch");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

fn test_key(seed: u8) -> IndexKey {
    IndexKey::from_hex(&format!("{:02x}", seed).repeat(32)).unwrap()
}

fn file_bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn keyed_create_reopen_and_header_is_encrypted() {
    let p = tmp_db_path();
    let key = test_key(0xa1);
    {
        let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('secret', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let head = &file_bytes(&p)[..16];
    assert_ne!(head, b"SQLite format 3\0", "file must not be plaintext");

    {
        let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "keyed reopen must see existing rows, not wipe");
    }
    let conn = open_existing_keyed(p.to_str().unwrap(), false, Some(&key)).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wrong_key_errors_without_wiping() {
    let p = tmp_db_path();
    {
        let conn =
            open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1))).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let before = file_bytes(&p);
    for write in [false, true] {
        let err =
            open_existing_keyed(p.to_str().unwrap(), write, Some(&test_key(0xb2))).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    }
    // A wrong key is never a "schema mismatch" to answer with a wipe.
    let err =
        open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xb2))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p), "file must be byte-identical");
    std::fs::remove_file(&p).ok();
}

#[test]
fn missing_key_on_encrypted_db_errors_without_wiping() {
    let p = tmp_db_path();
    {
        let _ =
            open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1))).unwrap();
    }
    let before = file_bytes(&p);
    let err = open_existing_keyed(p.to_str().unwrap(), false, None).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert!(err.contains("password-protected"), "got: {err}");
    let err = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", None).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
    std::fs::remove_file(&p).ok();
}

#[test]
fn key_on_plaintext_db_errors_without_wiping() {
    let p = tmp_db_path();
    {
        let _ = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    }
    let before = file_bytes(&p);
    let err = open_existing_keyed(p.to_str().unwrap(), false, Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    // The sniffed plaintext header yields the precise diagnosis.
    assert!(err.contains("not encrypted"), "got: {err}");
    let err =
        open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
    std::fs::remove_file(&p).ok();
}

#[test]
fn schema_mismatch_under_key_wipes_and_recreates_encrypted() {
    // Right key, stale schema: the replacement must come back encrypted.
    let p = tmp_db_path();
    let key = test_key(0xa1);
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key.to_hex()))
            .unwrap();
        conn.execute(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_info(key,value) VALUES('version','1')",
            [],
        )
        .unwrap();
    }
    let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
    drop(conn);
    let head = &file_bytes(&p)[..16];
    assert_ne!(
        head, b"SQLite format 3\0",
        "rebuilt index must still be encrypted"
    );
    std::fs::remove_file(&p).ok();
}

#[test]
fn garbage_file_with_key_reports_mismatch_not_corruption() {
    let p = tmp_db_path();
    std::fs::write(&p, [0x5a; 4096]).unwrap();
    let before = file_bytes(&p);
    let err = open_existing_keyed(p.to_str().unwrap(), false, Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
    std::fs::remove_file(&p).ok();
}

#[test]
fn open_existing_rw_allows_delete() {
    let p = tmp_db_path();
    {
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('a', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let conn = open_existing(p.to_str().unwrap(), true).unwrap();
    let removed = conn
        .execute("DELETE FROM files WHERE parent = '/' AND name = 'a'", [])
        .unwrap();
    assert_eq!(removed, 1);
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// SQLite creates its database 0644 (copied to `-wal`/`-shm`); under umask
/// 022 that leaves the full text of 0600 documents readable by every account
/// on the machine.
#[cfg(unix)]
#[test]
fn a_fresh_index_and_its_sidecars_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    // A directory that does not exist yet, so the creation path is under
    // test: an existing directory keeps whatever mode its owner chose.
    let p = tmp_db_path()
        .parent()
        .unwrap()
        .join("data")
        .join("index.sqlite");
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime) \
         VALUES ('a', '/', 0, 0)",
        [],
    )
    .unwrap();

    let mode_of = |path: &std::path::Path| {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode_of(&p), 0o600, "index at {}", p.display());
    for suffix in ["-wal", "-shm"] {
        let sidecar = std::path::PathBuf::from(format!("{}{}", p.display(), suffix));
        if sidecar.exists() {
            assert_eq!(mode_of(&sidecar), 0o600, "sidecar {}", sidecar.display());
        }
    }
    // The created directory would otherwise take the umask.
    assert_eq!(mode_of(p.parent().unwrap()), 0o700);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// `maintain` must work on a *keyed* index — the one case where SQLCipher
/// answers `PRAGMA page_size` as TEXT. Every unencrypted test passed while
/// every password-protected index silently skipped its VACUUM and optimize.
#[test]
fn maintain_reads_its_pragmas_on_a_keyed_index() {
    let p = tmp_db_path();
    let key = test_key(0xc3);
    let dir = p.parent().unwrap().to_string_lossy().into_owned();
    {
        let conn = open_or_recreate_keyed(p.to_str().unwrap(), "trigram", Some(&key)).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let conn = open_keyed_with_pragmas(p.to_str().unwrap(), true, Some(&key), PRAGMAS_MAINTENANCE)
        .unwrap();
    // What matters is an answer, not an error; a tiny index has no slack.
    assert_eq!(
        crate::db::repo::maintain(&conn, &dir),
        Ok(false),
        "maintain must not fail on a keyed index"
    );

    // A page size parsed as 0 would size the free-space check at zero bytes.
    assert!(
        crate::db::repo::pragma_number(&conn, "page_size").unwrap() >= 512,
        "a real page size, not a silent zero"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// A typo naming another application's database used to delete it on the
/// next indexing run, because "no `schema_info`" read as "an old index of
/// ours".
#[test]
fn a_foreign_database_is_refused_not_wiped() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_places(url) VALUES('https://example.invalid/')",
            [],
        )
        .unwrap();
    }
    let err = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap_err();
    assert!(err.starts_with(FOREIGN_DB_PREFIX), "got: {err}");
    assert!(
        err.contains("moz_places"),
        "the message must name what is in the way: {err}"
    );

    let conn = Connection::open(&p).unwrap();
    let url: String = conn
        .query_row("SELECT url FROM moz_places", [], |r| r.get(0))
        .expect("the foreign database must survive intact");
    assert_eq!(url, "https://example.invalid/");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// The refusal must not extend to a file that is genuinely ours to create.
#[test]
fn an_empty_database_file_is_still_ours_to_build() {
    let p = tmp_db_path();
    // An empty but real SQLite file, header and all.
    drop(Connection::open(&p).unwrap());
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM schema_info WHERE key='version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, CURRENT_SCHEMA_VERSION.to_string());
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// Each pre-`schema_info` table name marks a file as ours — wiped, not
/// refused.
#[test]
fn a_legacy_table_marks_a_file_as_ours() {
    for table in LEGACY_TABLES {
        let p = tmp_db_path();
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(&format!("CREATE TABLE {} (x INTEGER)", table), [])
                .unwrap();
        }
        let conn = open_or_recreate(p.to_str().unwrap(), "trigram")
            .unwrap_or_else(|e| panic!("{table} should read as ours, got: {e}"));
        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}

/// A `schema_info` with our columns but no version row is an interrupted
/// creation: ours, and rebuilding it is right.
#[test]
fn a_schema_info_without_a_version_row_is_still_ours() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
    }
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").expect("half-built index is ours");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// A table that merely *shares the name* `schema_info` is not ours.
#[test]
fn a_foreign_schema_info_is_refused() {
    let p = tmp_db_path();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute("CREATE TABLE schema_info (revision INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO schema_info(revision) VALUES(3)", [])
            .unwrap();
    }
    let err = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap_err();
    assert!(err.starts_with(FOREIGN_DB_PREFIX), "got: {err}");

    let conn = Connection::open(&p).unwrap();
    let revision: i64 = conn
        .query_row("SELECT revision FROM schema_info", [], |r| r.get(0))
        .expect("the foreign database must survive intact");
    assert_eq!(revision, 3);
    drop(conn);
    std::fs::remove_file(&p).ok();
}
