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
    // A DB from a prior schema version is wiped, not migrated.
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
    // Pre-A layout with no `schema_info` at all. Same policy — wipe.
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
    // New columns should exist (just prepare the SELECT — an
    // unknown column name would parse-error here).
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
    // An index built with a non-default tokenizer, opened by a consumer that
    // only knows "trigram": `open_existing` must read it as-is, never wipe.
    let p = tmp_db_path();
    {
        let conn = open_or_recreate(p.to_str().unwrap(), "unicode61").unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('note', '/', 0, 0)",
            [],
        )
        .unwrap();
        // Seed the FTS index (rowid = the files row we just inserted) so a
        // MATCH query can be exercised against the on-disk tokenizer.
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
    // The on-disk tokenizer is used as-is: a MATCH against the stored term
    // returns the row.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // And the stored tokenizer is still the non-default one — proof we
    // neither rewrote the FTS table nor reset schema_info.
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
    // Fresh installs point at ~/.local/share/quicksearch/… which
    // doesn't exist yet; the owner open must create it.
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
    // WAL is persistent in the file: a later read-only consumer sees it
    // without being able to (or needing to) set it.
    let conn = open_existing(p.to_str().unwrap(), false).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// SQLCipher is compiled `-DSQLITE_TEMP_STORE=2`, under which temporary
/// databases live in memory for any `temp_store` but an explicit `FILE` (1);
/// VACUUM builds the replacement index there, so the maintenance profile must
/// keep temporaries on disk.
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

    // And the directory those temporaries land in is steerable, which is
    // what keeps them off a RAM-backed /tmp. Deprecated but present.
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

/// Drives the GUI's "your index is being reset" modal, so a false positive
/// announces a wipe that is not happening and a false negative lets one
/// happen in silence.
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

    // Age it, exactly as a version bump does.
    {
        let conn = open_existing(p.to_str().unwrap(), true).unwrap();
        conn.execute(
            "UPDATE schema_info SET value = '1' WHERE key = 'version'",
            [],
        )
        .unwrap();
    }
    assert!(index_needs_rebuild(p.to_str().unwrap()));

    // A pre-`schema_info` layout counts too.
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
    // A consumer opening a stale-schema DB must error and leave the file
    // untouched.
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
    // Encrypted at rest: the plaintext SQLite magic must be gone.
    let head = &file_bytes(&p)[..16];
    assert_ne!(head, b"SQLite format 3\0", "file must not be plaintext");

    // Reopens with the same key, both owner and consumer paths.
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
    // The owner path must error too — a wrong key is never a "schema
    // mismatch" to answer with a wipe.
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
    // The one case where the owner *should* still wipe: right key,
    // stale schema. The replacement must come back encrypted.
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
    // A replaced index file (random bytes, no SQLite header) must surface as
    // KEY_MISMATCH — indistinguishable from a wrong key — and never wipe.
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

/// The index, and the WAL and SHM it hands its mode to, must not be readable
/// by other users on the machine.
///
/// SQLite creates its database file 0644 and copies that mode to `-wal` and
/// `-shm`; with the near-universal umask 022 that leaves the names and full
/// text of everything under the configured roots — including documents whose
/// own files are 0600 — readable by every account on a shared machine. The
/// README's carve-out is about other *processes of the same user*, not other
/// users, so nothing else covers this.
#[cfg(unix)]
#[test]
fn a_fresh_index_and_its_sidecars_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    // A directory that does not exist yet, so the creation path is the one
    // under test: an existing directory keeps whatever mode its owner chose.
    let p = tmp_db_path()
        .parent()
        .unwrap()
        .join("data")
        .join("index.sqlite");
    let conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    // A write, so the WAL and SHM exist to be checked.
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
    // And the directory created for it, which would otherwise take the umask
    // and let any account list what is indexed.
    assert_eq!(mode_of(p.parent().unwrap()), 0o700);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// `maintain` must work on a *keyed* index, which is the one case the pragma
/// it reads does not answer with an integer.
///
/// SQLCipher intercepts `PRAGMA page_size` on a keyed connection, answers with
/// `cipher_page_size`, and hands that back as TEXT. Reading it straight into an
/// `i64` failed there and only there — so every unencrypted test passed while
/// every index with a password set silently skipped both its VACUUM and its
/// `PRAGMA optimize`. The assertion is simply that the call succeeds: it has to
/// get past all three pragma reads to return at all.
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
    // A two-row index has no slack worth reclaiming, so `false` is the
    // expected answer — what matters is that it is an answer and not an error.
    assert_eq!(
        crate::db::repo::maintain(&conn, &dir),
        Ok(false),
        "maintain must not fail on a keyed index"
    );

    // And the value itself has to be usable, not merely readable: a page size
    // that parsed as 0 would size the free-space check at zero bytes and wave
    // through a VACUUM that cannot fit.
    assert!(
        crate::db::repo::pragma_number(&conn, "page_size").unwrap() >= 512,
        "a real page size, not a silent zero"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}
