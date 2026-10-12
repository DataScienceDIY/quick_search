use super::*;
use crate::testutil::Scratch;

/// A database path that does not exist yet, in its own scratch directory.
fn db() -> (Scratch, String) {
    let (dir, p) = Scratch::db("open");
    (dir, p.to_str().unwrap().to_string())
}

/// `schema_info.{key}`, which must be present.
fn info(conn: &Connection, key: &str) -> String {
    conn.query_row("SELECT value FROM schema_info WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .unwrap()
}

/// The DDL the full-text table was created with.
fn fts_sql(conn: &Connection) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE name = 'searchabletext'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn file_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap()
}

/// The `schema_info` of a version-1 index, on whatever `conn` is.
fn write_v1_schema_info(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO schema_info(key,value) VALUES('version','1');",
    )
    .unwrap();
}

/// A plain version-1 index holding one `files` row named `name`.
fn write_v1_index(p: &str, name: &str) {
    let conn = Connection::open(p).unwrap();
    write_v1_schema_info(&conn);
    conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
        .unwrap();
    conn.execute("INSERT INTO files(name) VALUES(?1)", [name])
        .unwrap();
}

/// Another application's database, in the rollback journal mode SQLite
/// defaults to, so nothing of SQLite's sits beside it at rest.
fn write_foreign_db(p: &str) {
    Connection::open(p)
        .unwrap()
        .execute_batch(
            "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
             INSERT INTO moz_places(url) VALUES('https://example.invalid/');",
        )
        .unwrap();
}

/// The file SQLite keeps beside `p` under `suffix`.
fn sidecar(p: &str, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{p}{suffix}"))
}

/// Permission bits; `0` where there are none to read.
fn file_mode(p: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        0
    }
}

fn set_mode(p: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
    }
}

#[test]
fn fresh_db_gets_current_version() {
    let (_dir, p) = db();
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
}

#[test]
fn reopen_is_idempotent() {
    let (_dir, p) = db();
    drop(open_or_recreate(&p, &Default::default()).unwrap());
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
}

/// Rebuild warnings in the process log that name `p`. Every scratch path is
/// unique, so lines other tests log at the same time cannot match.
fn rebuild_warnings(p: &str) -> usize {
    crate::log::snapshot()
        .iter()
        .filter(|line| {
            line.level == crate::log::Level::Warn
                && line.text.contains(p)
                && line.text.contains("rebuilding")
        })
        .count()
}

/// A new index has nothing to rebuild, so creating one must not warn of a
/// schema mismatch, as it did in every CI run and on the GUI's first launch.
/// A real mismatch still warns.
#[test]
fn only_a_real_schema_mismatch_warns_of_a_rebuild() {
    let (_dir, fresh) = db();
    drop(open_or_recreate(&fresh, &Default::default()).unwrap());
    assert_eq!(rebuild_warnings(&fresh), 0, "a path that did not exist");

    let (_dir, keyed) = db();
    drop(open_or_recreate_keyed(&keyed, &Default::default(), Some(&test_key(0xa3))).unwrap());
    assert_eq!(rebuild_warnings(&keyed), 0, "a new encrypted index");

    let (_dir, empty) = db();
    std::fs::write(&empty, b"").unwrap();
    let conn = open_or_recreate(&empty, &Default::default()).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
    assert_eq!(rebuild_warnings(&empty), 0, "an empty file");

    let (_dir, old) = db();
    write_v1_index(&old, "a.txt");
    let conn = open_or_recreate(&old, &Default::default()).unwrap();
    assert_eq!(file_count(&conn), 0, "the mismatch is still rebuilt");
    assert_eq!(rebuild_warnings(&old), 1, "a real schema mismatch");
}

#[test]
fn older_versioned_db_is_wiped_and_recreated() {
    let (_dir, p) = db();
    write_v1_index(&p, "a.txt");
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
    assert_eq!(file_count(&conn), 0, "old rows should be wiped");
}

#[test]
fn legacy_layout_db_is_wiped_and_recreated() {
    let (_dir, p) = db();
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
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(file_count(&conn), 0);
    // An unknown column name fails to prepare.
    conn.prepare("SELECT content_state, type, mime FROM files LIMIT 0")
        .unwrap();
}

/// Trigram is the only tokenizer and `schema_info` no longer records one, so
/// an index a v11 build made under a word tokenizer goes with the version
/// bump and comes back as trigram.
#[test]
fn a_word_tokenizer_index_from_v11_is_rebuilt_as_trigram() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &Default::default()).unwrap();
        conn.execute_batch(
            "DROP TABLE searchabletext;
             CREATE VIRTUAL TABLE searchabletext USING fts5(
                 text, tokenize='unicode61', content='', contentless_delete=1);
             INSERT INTO schema_info(key, value) VALUES ('tokenize', 'unicode61');
             UPDATE schema_info SET value = '11' WHERE key = 'version';
             INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0);",
        )
        .unwrap();
    }
    assert!(index_needs_rebuild(&p), "the GUI announces the reset");
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(file_count(&conn), 0, "the word-tokenizer rows are wiped");
    assert!(fts_sql(&conn).contains("tokenize='trigram'"));
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_info WHERE key = 'tokenize'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 0, "nothing records a tokenizer now");
}

/// The position setting is part of the table definition: a change wipes,
/// and the layout built is the one recorded, which is what every search
/// reads back.
#[test]
fn position_drift_wipes_db_and_the_layout_is_recorded() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &Default::default()).unwrap();
        assert!(fts_sql(&conn).contains("detail=none"), "compact by default");
        assert_eq!(info(&conn, "fts_positions"), "false");
        assert!(!crate::db::repo::fts_positions(&conn));
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let full = ProcessingConfig {
        compact_text_index: false,
        ..Default::default()
    };
    let conn = open_or_recreate(&p, &full).unwrap();
    assert_eq!(file_count(&conn), 0, "a layout change wipes the rows");
    assert!(!fts_sql(&conn).contains("detail"));
    assert_eq!(info(&conn, "fts_positions"), "true");
    assert!(crate::db::repo::fts_positions(&conn));
}

/// An index from before the layout was recorded kept positions.
#[test]
fn an_unrecorded_layout_reads_as_positions() {
    let (_dir, p) = db();
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    conn.execute("DELETE FROM schema_info WHERE key = 'fts_positions'", [])
        .unwrap();
    assert!(crate::db::repo::fts_positions(&conn));
}

/// An index holding one `files` row whose record names `hash_length` as
/// `recorded`, as a run's stamp does; no record for `None`.
fn with_recorded_hash_length(p: &str, recorded: Option<&str>) {
    let conn = open_or_recreate(p, &Default::default()).unwrap();
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
        [],
    )
    .unwrap();
    if let Some(value) = recorded {
        conn.execute(
            "INSERT INTO config_validation (key, value) VALUES ('hash_length', ?1)",
            [value],
        )
        .unwrap();
    }
}

fn hashing(hash_length: usize) -> ProcessingConfig {
    ProcessingConfig {
        hash_length,
        ..Default::default()
    }
}

/// Stored hashes cover `hash_length` bytes and no scan re-hashes an
/// unchanged file, so a declined change is a rebuild at the next open.
#[test]
fn hash_length_drift_wipes_db() {
    let (_dir, p) = db();
    with_recorded_hash_length(&p, Some("8192"));
    let conn = open_or_recreate(&p, &hashing(8192)).unwrap();
    assert_eq!(file_count(&conn), 1, "the same length keeps the rows");
    drop(conn);

    let conn = open_or_recreate(&p, &hashing(4096)).unwrap();
    assert_eq!(file_count(&conn), 0, "hash_length drift should wipe rows");
    let records: i64 = conn
        .query_row("SELECT COUNT(*) FROM config_validation", [], |r| r.get(0))
        .unwrap();
    assert_eq!(records, 0, "nothing is recorded until a run");
}

/// A fresh or never-run index has no record to differ from.
#[test]
fn an_unrecorded_hash_length_keeps_the_db() {
    let (_dir, p) = db();
    with_recorded_hash_length(&p, None);
    let conn = open_or_recreate(&p, &hashing(4096)).unwrap();
    assert_eq!(file_count(&conn), 1);
}

/// A record no length can match is a difference, not a match.
#[test]
fn an_unreadable_recorded_hash_length_wipes_db() {
    let (_dir, p) = db();
    with_recorded_hash_length(&p, Some("x"));
    let conn = open_or_recreate(&p, &hashing(8192)).unwrap();
    assert_eq!(file_count(&conn), 0);
}

/// `open_existing` has no settings to compare against, so it never rebuilds
/// an index whose layout is not the default.
#[test]
fn open_existing_reads_a_nondefault_layout_without_wiping() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &positional()).unwrap();
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

    let conn = open_existing(&p, false).unwrap();
    assert_eq!(
        file_count(&conn),
        1,
        "open_existing must not wipe a non-default-layout DB"
    );
    // Positions: a phrase longer than one trigram, which `detail=none`
    // refuses, still matches.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH '\"lo wor\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // Proof we neither rewrote the FTS table nor reset schema_info.
    assert!(!fts_sql(&conn).contains("detail"));
    assert_eq!(info(&conn, "fts_positions"), "true");
}

#[test]
fn open_or_recreate_creates_missing_parent_dirs() {
    let dir = Scratch::dir("mkdir");
    let db = dir.join("nested/deeper/index.sqlite");
    drop(open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap());
    assert!(db.exists());
}

#[test]
fn writable_opens_use_wal_and_it_persists() {
    let (_dir, p) = db();
    let mode = |conn: &Connection| -> String {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    };
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(mode(&conn).to_lowercase(), "wal");
    drop(conn);
    // WAL is persistent in the file: a later read-only consumer sees it.
    let conn = open_existing(&p, false).unwrap();
    assert_eq!(mode(&conn).to_lowercase(), "wal");
}

/// `maintain` moves temporaries to disk for VACUUM alone. Left on `FILE`, the
/// connection's next large sort would spill to a raw, unkeyed file, and the
/// directory is a process-wide global.
#[test]
fn maintain_leaves_memory_temporaries() {
    let temp_store = |conn: &Connection| -> i64 {
        conn.query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap()
    };
    // No row while unset. Another test's VACUUM may set its own directory
    // meanwhile, but never this one's.
    let temp_dir = |conn: &Connection| -> Option<String> {
        conn.query_row("PRAGMA temp_store_directory", [], |r| r.get(0))
            .optional()
            .unwrap()
    };
    for slack in [false, true] {
        let (_dir, p) = db();
        let conn = open_or_recreate(&p, &Default::default()).unwrap();
        if slack {
            conn.execute_batch(
                "INSERT INTO files (name, parent, size, mtime)
                 WITH RECURSIVE n(i) AS (
                     SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000
                 )
                 SELECT 'p' || i, '/pad/', 0, 0 FROM n;
                 DELETE FROM files WHERE parent = '/pad/';",
            )
            .unwrap();
        }
        drop(conn);

        let conn = open_existing(&p, true).unwrap();
        assert_eq!(temp_store(&conn), 2, "the writer's profile is MEMORY");
        assert_eq!(
            crate::db::repo::maintain(&conn, &p),
            Ok(slack),
            "vacuumed only with slack"
        );
        assert_eq!(temp_store(&conn), 2, "vacuumed: {slack}");
        let own = Path::new(&p).parent().unwrap().to_string_lossy();
        assert_ne!(temp_dir(&conn).as_deref(), Some(&*own), "vacuumed: {slack}");
    }
}

/// Builds the file at a path, or leaves it absent.
type Fixture = fn(&str);

/// Drives the GUI's "your index is being reset" modal: a false positive
/// announces a wipe that is not happening.
#[test]
fn index_needs_rebuild_only_when_the_schema_really_differs() {
    let cases: [(Fixture, bool, &str); 8] = [
        (|_| {}, false, "no file yet is a fresh install, not a reset"),
        (
            |p| std::fs::write(p, b"").unwrap(),
            false,
            "an empty file holds nothing to reset",
        ),
        (
            |p| drop(open_or_recreate(p, &Default::default()).unwrap()),
            false,
            "a current index is not going to be rebuilt",
        ),
        (
            |p| {
                drop(open_or_recreate(p, &Default::default()).unwrap());
                open_existing(p, true)
                    .unwrap()
                    .execute(
                        "UPDATE schema_info SET value = '1' WHERE key = 'version'",
                        [],
                    )
                    .unwrap();
            },
            true,
            "an older schema version is rebuilt",
        ),
        (
            |p| {
                Connection::open(p)
                    .unwrap()
                    .execute("CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT)", [])
                    .unwrap();
            },
            true,
            "a legacy layout is rebuilt",
        ),
        (
            |p| std::fs::write(p, [0x5a; 4096]).unwrap(),
            false,
            "not a database at all: we cannot tell, so we say nothing",
        ),
        (
            write_foreign_db,
            false,
            "another application's database is refused, not rebuilt",
        ),
        (
            |p| {
                Connection::open(p)
                    .unwrap()
                    .execute_batch(
                        "CREATE TABLE schema_info (revision INTEGER);
                         INSERT INTO schema_info(revision) VALUES(3);",
                    )
                    .unwrap();
            },
            false,
            "a foreign table that only shares the name is not ours either",
        ),
    ];
    for (build, expected, why) in cases {
        let (_dir, p) = db();
        build(&p);
        assert_eq!(index_needs_rebuild(&p), expected, "{}", why);
    }
}

#[test]
fn open_existing_errors_on_missing_file() {
    let (_dir, p) = db();
    let res = open_existing(&p, false);
    assert!(res.is_err(), "missing file must error, not be created");
    assert!(
        !Path::new(&p).exists(),
        "open_existing must not create the file"
    );
}

#[test]
fn open_existing_errors_on_version_mismatch_without_wiping() {
    let (_dir, p) = db();
    write_v1_index(&p, "sentinel");
    let res = open_existing(&p, false);
    assert!(res.is_err(), "stale schema version must error");
    assert_eq!(
        file_count(&Connection::open(&p).unwrap()),
        1,
        "open_existing must never delete on version mismatch"
    );
}

fn test_key(seed: u8) -> IndexKey {
    IndexKey::from_hex(&format!("{:02x}", seed).repeat(32)).unwrap()
}

fn file_bytes(p: &str) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn keyed_create_reopen_and_header_is_encrypted() {
    let (_dir, p) = db();
    let key = test_key(0xa1);
    {
        let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&key)).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('secret', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let head = &file_bytes(&p)[..16];
    assert_ne!(head, b"SQLite format 3\0", "file must not be plaintext");

    let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&key)).unwrap();
    assert_eq!(
        file_count(&conn),
        1,
        "keyed reopen must see existing rows, not wipe"
    );
    drop(conn);
    let conn = open_existing_keyed(&p, false, Some(&key)).unwrap();
    assert_eq!(file_count(&conn), 1);
}

#[test]
fn wrong_key_errors_without_wiping() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&test_key(0xa1))).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let before = file_bytes(&p);
    for write in [false, true] {
        let err = open_existing_keyed(&p, write, Some(&test_key(0xb2))).unwrap_err();
        assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    }
    // A wrong key is never a "schema mismatch" to answer with a wipe.
    let err = open_or_recreate_keyed(&p, &Default::default(), Some(&test_key(0xb2))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p), "file must be byte-identical");
}

#[test]
fn missing_key_on_encrypted_db_errors_without_wiping() {
    let (_dir, p) = db();
    drop(open_or_recreate_keyed(&p, &Default::default(), Some(&test_key(0xa1))).unwrap());
    let before = file_bytes(&p);
    let err = open_existing_keyed(&p, false, None).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert!(err.contains("password-protected"), "got: {err}");
    let err = open_or_recreate_keyed(&p, &Default::default(), None).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
}

#[test]
fn key_on_plaintext_db_errors_without_wiping() {
    let (_dir, p) = db();
    drop(open_or_recreate(&p, &Default::default()).unwrap());
    let before = file_bytes(&p);
    let err = open_existing_keyed(&p, false, Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    // The sniffed plaintext header yields the precise diagnosis.
    assert!(err.contains("not encrypted"), "got: {err}");
    let err = open_or_recreate_keyed(&p, &Default::default(), Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
}

#[test]
fn schema_mismatch_under_key_wipes_and_recreates_encrypted() {
    // Right key, stale schema: the replacement must come back encrypted.
    let (_dir, p) = db();
    let key = test_key(0xa1);
    {
        // Built through `key_and_probe` under a named previous profile, as a
        // file from an older build is. A bare `PRAGMA key` would inherit the
        // `cipher_default_*` globals another test last installed and could
        // land under a layout `PROFILES_PREVIOUS` does not list, which reads
        // as a wrong password.
        let previous = crate::db::schema::PROFILES_PREVIOUS[0];
        let conn = Connection::open(&p).unwrap();
        key_and_probe(&conn, &p, Some(&key), previous).unwrap();
        write_v1_schema_info(&conn);
    }
    let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&key)).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
    drop(conn);
    let head = &file_bytes(&p)[..16];
    assert_ne!(
        head, b"SQLite format 3\0",
        "rebuilt index must still be encrypted"
    );
}

#[test]
fn garbage_file_with_key_reports_mismatch_not_corruption() {
    let (_dir, p) = db();
    std::fs::write(&p, [0x5a; 4096]).unwrap();
    let before = file_bytes(&p);
    let err = open_existing_keyed(&p, false, Some(&test_key(0xa1))).unwrap_err();
    assert!(err.starts_with(KEY_MISMATCH_PREFIX), "got: {err}");
    assert_eq!(before, file_bytes(&p));
}

#[test]
fn open_existing_rw_allows_delete() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &Default::default()).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('a', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let conn = open_existing(&p, true).unwrap();
    let removed = conn
        .execute("DELETE FROM files WHERE parent = '/' AND name = 'a'", [])
        .unwrap();
    assert_eq!(removed, 1);
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
    let dir = Scratch::dir("open");
    let p = dir.join("data").join("index.sqlite");
    let conn = open_or_recreate(p.to_str().unwrap(), &Default::default()).unwrap();
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
}

/// `maintain` must work on a keyed index, the one case where SQLCipher answers
/// `PRAGMA page_size` as TEXT; a failure there skips VACUUM and optimize on
/// every password-protected index while every unencrypted test passes.
#[test]
fn maintain_reads_its_pragmas_on_a_keyed_index() {
    let (_dir, p) = db();
    let key = test_key(0xc3);
    {
        let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&key)).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let conn = open_existing_keyed(&p, true, Some(&key)).unwrap();
    // What matters is an answer, not an error; a tiny index has no slack.
    assert_eq!(
        crate::db::repo::maintain(&conn, &p),
        Ok(false),
        "maintain must not fail on a keyed index"
    );

    // A page size parsed as 0 would size the free-space check at zero bytes.
    assert!(
        crate::db::repo::pragma_number(&conn, "page_size").unwrap() >= 512,
        "a real page size, not a silent zero"
    );
}

/// A layout with positions: `pgsz` caps a leaf the same in either layout,
/// and position lists fill leaves soonest, so the geometry tests below need
/// the smallest corpus this way.
fn positional() -> ProcessingConfig {
    ProcessingConfig {
        compact_text_index: false,
        ..Default::default()
    }
}

/// Write enough text that FTS5 emits several *full* leaves. A corpus of tiny
/// documents fits in one part-filled leaf, never reaches the inline-payload
/// limit, and would let the tests below pass on a broken geometry.
fn seed_searchable_text(conn: &Connection) {
    let words = crate::testutil::WORDS;
    conn.execute_batch("BEGIN").unwrap();
    for doc in 0..200usize {
        let body: Vec<&str> = (0..150)
            .map(|w| words[(doc * 31 + w * 7) % words.len()])
            .collect();
        conn.execute(
            "INSERT INTO searchabletext(rowid, text) VALUES (?1, ?2)",
            params![doc as i64 + 1, body.join(" ")],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn fts_stored_pgsz(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT v FROM searchabletext_config WHERE k = 'pgsz'",
        [],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// The inline payload limit for the shipped profile: `page − reserve − 35`,
/// with SQLCipher's page reserve when keyed.
fn max_inline(keyed: bool) -> i64 {
    use crate::db::schema::PROFILE;
    PROFILE.page_size - PROFILE.reserve(keyed) - 35
}

/// Assert one index's FTS5 leaves are inline. An overflowed leaf costs a
/// second page fetch and decrypt on every read, and only the page counts show
/// it.
fn assert_leaves_inline(conn: &Connection, keyed: bool) {
    let (leaf, overflow) = crate::testutil::fts_page_counts(conn);
    assert!(
        leaf > 20,
        "the corpus must fill real leaves for this to test anything: {} leaves",
        leaf
    );
    assert_eq!(
        overflow,
        0,
        "{} of {} FTS5 leaves spilled to overflow pages; the derived pgsz is \
         back above the {}-byte inline limit",
        overflow,
        leaf,
        max_inline(keyed)
    );
    // The limit itself, in case `dbstat` is ever unavailable or lies.
    let widest: i64 = conn
        .query_row(
            "SELECT MAX(LENGTH(block)) FROM searchabletext_data",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        widest <= max_inline(keyed),
        "a {}-byte record cannot sit inline under a {}-byte limit",
        widest,
        max_inline(keyed)
    );
}

/// SQLCipher reserves bytes of every page for its IV and any authenticator,
/// so a keyed index's inline payload limit is lower than a plain one's at the
/// same page size. FTS5's default record size ignores the reserve and would
/// send every full keyed leaf to an overflow page (12% more file, and a
/// second decrypt per leaf read); [`crate::db::schema::fts_pgsz_for`] keeps
/// them inline.
#[test]
fn keyed_fts_leaves_stay_inline() {
    let (_dir, p) = db();
    let key = test_key(0xd4);
    let conn = open_or_recreate_keyed(&p, &positional(), Some(&key)).unwrap();
    assert_eq!(
        fts_stored_pgsz(&conn),
        Some(crate::db::schema::fts_pgsz_for(
            crate::db::schema::PROFILE,
            true
        )),
        "a keyed index must pin pgsz at creation"
    );

    seed_searchable_text(&conn);
    assert_leaves_inline(&conn, true);
}

/// The derivation must clear the limit it is derived from at every size
/// `benches/page_geometry.rs` sweeps. Arithmetic only; the round trip through
/// a real file is the test below.
#[test]
fn derived_fts_pgsz_fits_inline_at_every_swept_page_size() {
    use crate::db::schema::{fts_pgsz_for, HmacMode, Profile};
    for page_size in [1024i64, 2048, 4096, 8192, 16384, 32768, 65536] {
        // Every HMAC mode, not just the shipped one: the reserve moves with
        // it, and `benches/cipher_hmac.rs` must measure authentication rather
        // than overflow.
        for hmac in [HmacMode::Off, HmacMode::Sha256, HmacMode::Sha512] {
            let profile = Profile { page_size, hmac };
            for keyed in [false, true] {
                // What a table leaf holds inline, and what FTS5 actually writes.
                let max_inline = page_size - profile.reserve(keyed) - 35;
                let widest_record = fts_pgsz_for(profile, keyed) + 2;
                assert!(
                    widest_record <= max_inline,
                    "page {} hmac={:?} keyed={}: a {}-byte record does not fit in {}",
                    page_size,
                    hmac,
                    keyed,
                    widest_record,
                    max_inline
                );
                // A pgsz so small the leaves stop holding useful runs would be
                // a different bug, and a silent one.
                assert!(
                    widest_record * 2 > max_inline,
                    "page {} hmac={:?} keyed={}: {} wastes over half of a {}-byte leaf",
                    page_size,
                    hmac,
                    keyed,
                    widest_record,
                    max_inline
                );
            }
        }
    }
    // Spelled out so a change to the formula has to be deliberate. The keyed
    // value is quoted per mode: the on-disk format depends on it.
    let at = |page_size, hmac| Profile { page_size, hmac };
    assert_eq!(fts_pgsz_for(at(4096, HmacMode::Sha512), true), 3970);
    assert_eq!(fts_pgsz_for(at(4096, HmacMode::Sha256), true), 4002);
    assert_eq!(fts_pgsz_for(at(4096, HmacMode::Off), true), 4034);
    assert_eq!(
        fts_pgsz_for(at(4096, HmacMode::Sha512), false),
        4050,
        "FTS5's own default; a plain file has no reserve, so the mode is moot"
    );
}

/// The mirror. A plain index gets a larger record than a keyed one at the
/// same page size, because it has no reserve to give up; the two differ by
/// exactly that, and both land inline.
#[test]
fn a_plain_index_gets_the_derived_pgsz_for_its_page_size() {
    use crate::db::schema::{fts_pgsz_for, PROFILE};
    let (_dir, p) = db();
    let conn = open_or_recreate(&p, &positional()).unwrap();
    assert_eq!(
        fts_stored_pgsz(&conn),
        Some(fts_pgsz_for(PROFILE, false)),
        "an unencrypted index gets the derivation for its page size"
    );
    assert_eq!(
        fts_pgsz_for(PROFILE, false) - fts_pgsz_for(PROFILE, true),
        PROFILE.hmac.reserve(),
        "the plain and keyed records differ by exactly the reserve"
    );

    seed_searchable_text(&conn);
    assert_leaves_inline(&conn, false);
}

/// Another application's database at `database_path` is refused, not wiped:
/// "no `schema_info`" must not read as "an old index of ours".
#[test]
fn a_foreign_database_is_refused_not_wiped() {
    let (_dir, p) = db();
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
    let err = open_or_recreate(&p, &Default::default()).unwrap_err();
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
}

/// The refusal must not extend to a file that is ours to create.
#[test]
fn an_empty_database_file_is_still_ours_to_build() {
    let (_dir, p) = db();
    // An empty but real SQLite file, header and all.
    drop(Connection::open(&p).unwrap());
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(info(&conn, "version"), CURRENT_SCHEMA_VERSION.to_string());
}

/// Each pre-`schema_info` table name marks a file as ours: wiped, not
/// refused.
#[test]
fn a_legacy_table_marks_a_file_as_ours() {
    for table in LEGACY_TABLES {
        let (_dir, p) = db();
        Connection::open(&p)
            .unwrap()
            .execute(&format!("CREATE TABLE {} (x INTEGER)", table), [])
            .unwrap();
        open_or_recreate(&p, &Default::default())
            .unwrap_or_else(|e| panic!("{table} should read as ours, got: {e}"));
    }
}

/// A `schema_info` with our columns but no version row is an interrupted
/// creation: ours, and rebuilding it is right.
#[test]
fn a_schema_info_without_a_version_row_is_still_ours() {
    let (_dir, p) = db();
    Connection::open(&p)
        .unwrap()
        .execute(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
    open_or_recreate(&p, &Default::default()).expect("half-built index is ours");
}

/// A table that merely *shares the name* `schema_info` is not ours.
#[test]
fn a_foreign_schema_info_is_refused() {
    let (_dir, p) = db();
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute("CREATE TABLE schema_info (revision INTEGER)", [])
            .unwrap();
        conn.execute("INSERT INTO schema_info(revision) VALUES(3)", [])
            .unwrap();
    }
    let err = open_or_recreate(&p, &Default::default()).unwrap_err();
    assert!(err.starts_with(FOREIGN_DB_PREFIX), "got: {err}");

    let conn = Connection::open(&p).unwrap();
    let revision: i64 = conn
        .query_row("SELECT revision FROM schema_info", [], |r| r.get(0))
        .expect("the foreign database must survive intact");
    assert_eq!(revision, 3);
}

/// What a refusal must leave as it found: the file's bytes (as length and
/// digest, so a failure prints legibly) and mode, and which of SQLite's
/// sidecars exist beside it.
#[derive(Debug, PartialEq)]
struct AsFound {
    bytes: (usize, u64),
    mode: u32,
    sidecars: Vec<PathBuf>,
}

fn as_found(p: &str) -> AsFound {
    use std::hash::{Hash, Hasher};
    let bytes = file_bytes(p);
    let mut digest = std::hash::DefaultHasher::new();
    bytes.hash(&mut digest);
    AsFound {
        bytes: (bytes.len(), digest.finish()),
        mode: file_mode(Path::new(p)),
        sidecars: sqlite_sidecars(Path::new(p))
            .filter(|s| s.exists())
            .collect(),
    }
}

/// Opening a file is not permission to change it: before the refusal, a
/// chmod to 0600 and `journal_mode = WAL` (which rewrites the header and
/// leaves a `-wal` and `-shm` behind) used to land on whatever
/// `database_path` named.
#[test]
fn a_refused_foreign_database_is_left_byte_identical() {
    type Opener = fn(&str, Option<&IndexKey>) -> Result<Connection, String>;
    let openers: [(&str, Opener); 3] = [
        ("open_or_recreate", |p, key| {
            open_or_recreate_keyed(p, &Default::default(), key)
        }),
        ("open_existing(write)", |p, key| {
            open_existing_keyed(p, true, key)
        }),
        ("open_incremental_writer", |p, key| {
            open_keyed_with_pragmas(p, true, key, PRAGMAS_INCREMENTAL)
        }),
    ];
    let files: [(&str, Fixture, u32); 2] = [
        ("another application's database", write_foreign_db, 0o644),
        (
            "an executable text file",
            |p| std::fs::write(p, "#!/bin/sh\necho not a database\n".repeat(40)).unwrap(),
            0o755,
        ),
    ];
    let key = test_key(0xe5);
    for (what, build, mode) in files {
        for key in [None, Some(&key)] {
            for (opener, open) in openers {
                let (_dir, p) = db();
                build(&p);
                set_mode(Path::new(&p), mode);
                let before = as_found(&p);
                let result = open(&p, key);
                assert!(
                    result.is_err(),
                    "{what} was accepted by {opener} (keyed: {})",
                    key.is_some()
                );
                assert_eq!(
                    as_found(&p),
                    before,
                    "{what} was modified by {opener} (keyed: {})",
                    key.is_some()
                );
            }
        }
    }
}

/// An index whose permissions were loosened since it was built is narrowed
/// again on the next open, and so are the sidecars that open created: they
/// take the main file's mode as it was when SQLite made them.
#[cfg(unix)]
#[test]
fn an_existing_loose_index_is_tightened_sidecars_included() {
    let (_dir, p) = db();
    drop(open_or_recreate(&p, &Default::default()).unwrap());
    set_mode(Path::new(&p), 0o644);

    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    let present: Vec<PathBuf> = sqlite_sidecars(Path::new(&p))
        .filter(|s| s.exists())
        .collect();
    assert!(
        !present.is_empty(),
        "the reopen left no sidecar, so this proves nothing about them"
    );
    assert_eq!(file_mode(Path::new(&p)), 0o600, "index at {p}");
    for sidecar in &present {
        assert_eq!(file_mode(sidecar), 0o600, "sidecar {}", sidecar.display());
    }
    drop(conn);
}

/// A plaintext index whose header SQLite rejects is damaged, not encrypted:
/// asking for a password would leave the user no way out.
#[test]
fn a_damaged_plaintext_header_reads_as_damaged_not_password_protected() {
    type Spoil = fn(&mut Vec<u8>);
    // SQLite reads a short first page as zeros, so a cut file passes the
    // header checks and fails later as corrupt: the corruption path, which
    // offers the same way out.
    let damage: [(&str, Spoil, &str); 3] = [
        ("an unknown read version", |b| b[19] = 9, "damaged"),
        ("zeroed payload fractions", |b| b[21..24].fill(0), "damaged"),
        ("a file cut to 40 bytes", |b| b.truncate(40), "malformed"),
    ];
    for (what, spoil, says) in damage {
        let (_dir, p) = db();
        drop(open_or_recreate(&p, &Default::default()).unwrap());
        let mut bytes = file_bytes(&p);
        spoil(&mut bytes);
        std::fs::write(&p, &bytes).unwrap();

        let err = open_existing_keyed(&p, false, None).unwrap_err();
        assert!(!err.starts_with(KEY_MISMATCH_PREFIX), "{what}: {err}");
        assert!(err.contains(says), "{what}: {err}");
        assert!(
            open_or_recreate_keyed(&p, &Default::default(), None).is_err(),
            "{what}: a damaged index is not silently replaced"
        );
        assert_eq!(
            file_bytes(&p),
            bytes,
            "{what}: the damaged file was modified"
        );

        // With a key, the plaintext header still tells "never encrypted".
        if what == "an unknown read version" {
            let err = open_existing_keyed(&p, false, Some(&test_key(0xa1))).unwrap_err();
            assert_eq!(
                key_mismatch_parts(&err).map(|(cause, _)| cause),
                Some(KeyMismatch::NotEncrypted),
                "{what}: {err}"
            );
        }
    }
}

/// Clear, every rebuild and the unlock reset end here: a typo in
/// `database_path` must not delete another application's database, or the
/// files SQLite keeps beside it.
#[test]
fn removing_refuses_a_foreign_sqlite_database() {
    let (_dir, p) = db();
    // In WAL mode and held open, so it has sidecars of its own to lose.
    let owner = Connection::open(&p).unwrap();
    owner
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
             INSERT INTO moz_places(url) VALUES('https://example.invalid/');",
        )
        .unwrap();
    let sidecars = [sidecar(&p, "-wal"), sidecar(&p, "-shm")];
    assert!(
        sidecars.iter().all(|s| s.exists()),
        "no sidecars to protect"
    );

    let err = remove_index_files(Path::new(&p)).unwrap_err();
    assert!(err.starts_with(FOREIGN_DB_PREFIX), "got: {err}");
    assert!(err.contains("moz_places"), "got: {err}");
    assert!(Path::new(&p).exists());
    for sidecar in &sidecars {
        assert!(sidecar.exists(), "{} was deleted", sidecar.display());
    }
    let url: String = owner
        .query_row("SELECT url FROM moz_places", [], |r| r.get(0))
        .expect("the foreign database must survive intact");
    assert_eq!(url, "https://example.invalid/");
}

/// A database another program has locked is in use, whatever it holds, and
/// the refusal comes after the busy timeout rather than a hang.
#[test]
fn removing_refuses_a_foreign_database_another_program_holds() {
    let (_dir, p) = db();
    write_foreign_db(&p);
    let before = file_bytes(&p);
    let holder = Connection::open(&p).unwrap();
    holder
        .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
        .unwrap();

    let started = std::time::Instant::now();
    let err = remove_index_files(Path::new(&p)).unwrap_err();
    let took = started.elapsed();
    assert!(err.contains("in use"), "got: {err}");
    assert!(
        took < PROBE_BUSY_TIMEOUT + std::time::Duration::from_secs(2),
        "the refusal took {took:?}"
    );
    drop(holder);
    assert_eq!(file_bytes(&p), before);
}

/// A program that died mid-transaction left a hot rollback journal, which no
/// index of ours ever has: the database and its journal both survive.
#[test]
fn removing_refuses_a_database_with_a_hot_journal() {
    let (_dir, live) = db();
    let conn = Connection::open(&live).unwrap();
    conn.execute_batch(
        "PRAGMA cache_size = 1;
         CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
         BEGIN;",
    )
    .unwrap();
    // More than the one-page cache holds, so the transaction spills to the
    // file and the journal is written.
    for _ in 0..200 {
        conn.execute("INSERT INTO moz_places(url) VALUES(?1)", ["x".repeat(500)])
            .unwrap();
    }
    // The pair copied mid-transaction is what a crash leaves: a journal with
    // no connection to roll it back.
    let (_dir2, p) = db();
    std::fs::copy(&live, &p).unwrap();
    std::fs::copy(sidecar(&live, "-journal"), sidecar(&p, "-journal")).unwrap();
    drop(conn);
    let before = (
        file_bytes(&p),
        std::fs::read(sidecar(&p, "-journal")).unwrap(),
    );

    let err = remove_index_files(Path::new(&p)).unwrap_err();
    assert!(err.contains("unfinished transaction"), "got: {err}");
    let after = (
        file_bytes(&p),
        std::fs::read(sidecar(&p, "-journal")).unwrap(),
    );
    assert!(after == before, "the database or its journal was changed");
}

#[test]
fn removing_refuses_a_non_sqlite_file_that_is_not_whole_pages() {
    let dir = Scratch::dir("open");
    let p = dir.join("notes.txt");
    std::fs::write(&p, [b'x'; 1000]).unwrap();
    let err = remove_index_files(&p).unwrap_err();
    assert!(err.contains("not a QuickSearch index"), "got: {err}");
    assert_eq!(std::fs::read(&p).unwrap(), [b'x'; 1000]);
}

/// A file this process cannot read (another user's, say) has no header it
/// can see, but that is not knowing it has none: whole pages of it are not
/// taken for an encrypted index.
#[cfg(unix)]
#[test]
fn removing_refuses_a_file_it_cannot_read() {
    // Root reads through any mode, so there is nothing to refuse.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (_dir, p) = db();
    std::fs::write(&p, [0x5a; 4096]).unwrap();
    set_mode(Path::new(&p), 0o000);
    let refused = remove_index_files(Path::new(&p));
    set_mode(Path::new(&p), 0o600);
    let err = refused.unwrap_err();
    assert!(
        err.contains("Cannot read") && err.contains("refusing to delete it"),
        "got: {err}"
    );
    assert_eq!(file_bytes(&p), [0x5a; 4096]);
}

/// A text file at `database_path` is no index, encrypted or not: neither a
/// password prompt nor "wrong password" is any way out of it.
#[test]
fn a_file_that_is_not_whole_pages_is_not_called_password_protected() {
    let (_dir, p) = db();
    std::fs::write(&p, [b'x'; 1000]).unwrap();
    for key in [None, Some(test_key(0xa1))] {
        let err = open_existing_keyed(&p, false, key.as_ref()).unwrap_err();
        assert!(
            !err.starts_with(KEY_MISMATCH_PREFIX),
            "keyed {}: {err}",
            key.is_some()
        );
        assert!(
            err.contains("is not a QuickSearch index") && err.contains("database_path"),
            "keyed {}: {err}",
            key.is_some()
        );
        assert_eq!(file_bytes(&p), [b'x'; 1000]);
    }
}

/// Neither is a file at all. Opening a FIFO blocks until a writer appears,
/// so each removal runs on a thread a regression would strand, not the suite.
#[test]
fn removing_refuses_a_directory_and_a_fifo() {
    let dir = Scratch::dir("open");
    let folder = dir.join("index.sqlite");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("keep.txt"), "kept").unwrap();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut targets = vec![folder.clone()];
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let fifo = dir.join("fifo.sqlite");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        targets.push(fifo);
    }
    for target in targets {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = target.clone();
        std::thread::spawn(move || tx.send(remove_index_files(&path)));
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("removing {} blocked", target.display()))
            .unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");
        assert!(target.exists(), "{} was deleted", target.display());
    }
    assert!(folder.join("keep.txt").exists());
}

/// A damaged index has to stay clearable: clearing is how the user gets a
/// working one back.
#[test]
fn a_damaged_plaintext_index_is_still_removable() {
    const MAGIC: &[u8] = b"SQLite format 3\0";
    let cases: [(&str, Fixture); 3] = [
        ("the magic and 4097 zero bytes", |p| {
            std::fs::write(p, [MAGIC, &[0u8; 4097]].concat()).unwrap()
        }),
        ("the magic and 8192 bytes of garbage", |p| {
            let garbage: Vec<u8> = (0..8192u32).map(|i| (i * 31 % 251) as u8).collect();
            std::fs::write(p, [MAGIC, &garbage].concat()).unwrap()
        }),
        ("an index cut to 40 bytes", |p| {
            drop(open_or_recreate(p, &Default::default()).unwrap());
            std::fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_len(40)
                .unwrap();
        }),
    ];
    for (what, build) in cases {
        let (_dir, p) = db();
        build(&p);
        remove_index_files(Path::new(&p)).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(!Path::new(&p).exists(), "{what} survived");
    }
}

#[test]
fn removing_deletes_our_index_and_sidecars_but_not_the_lock() {
    let (_dir, p) = db();
    drop(open_or_recreate(&p, &Default::default()).unwrap());
    // As a crash leaves them; at rest SQLite has removed its own.
    for suffix in ["-wal", "-shm", "-journal"] {
        std::fs::write(sidecar(&p, suffix), b"").unwrap();
    }
    let lock = crate::platform::IndexLock::path_for(Path::new(&p));
    std::fs::write(&lock, b"4242").unwrap();

    remove_index_files(Path::new(&p)).unwrap();
    assert!(!Path::new(&p).exists());
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!sidecar(&p, suffix).exists(), "{suffix} survived");
    }
    assert!(
        lock.exists(),
        "the instance lock is not the index's to delete"
    );
}

/// Without the key the file is noise, but whole pages of it.
#[test]
fn removing_deletes_an_encrypted_index_without_its_key() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate_keyed(&p, &Default::default(), Some(&test_key(0xa7))).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let len = std::fs::metadata(&p).unwrap().len();
    assert!(len > 0 && len % MIN_PAGE_SIZE == 0, "{len} bytes");
    assert!(
        !has_sqlite_magic(Path::new(&p)).unwrap(),
        "the fixture is not encrypted"
    );

    remove_index_files(Path::new(&p)).unwrap();
    assert!(!Path::new(&p).exists());
}

#[test]
fn missing_or_empty_is_removable() {
    let (_dir, p) = db();
    remove_index_files(Path::new(&p)).expect("nothing there is nothing to refuse");
    std::fs::write(&p, b"").unwrap();
    remove_index_files(Path::new(&p)).unwrap();
    assert!(!Path::new(&p).exists());
}

/// Once per attempt, after it, whatever it came to: a reader that reopened
/// during the removal must see the epoch move.
#[test]
fn remove_index_files_bumps_the_epoch_once() {
    let bumps = || crate::db::EPOCH_BUMPS_HERE.with(|n| n.get());
    let removal = |p: &Path| {
        let before = bumps();
        let result = remove_index_files(p);
        (result, bumps() - before)
    };
    let (_dir, p) = db();
    let p = Path::new(&p);
    drop(open_or_recreate(&p.to_string_lossy(), &Default::default()).unwrap());

    let (result, bumped) = removal(p);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(bumped, 1, "a removal");

    let (result, bumped) = removal(p);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(bumped, 1, "a missing file");

    std::fs::write(p, [b'x'; 1000]).unwrap();
    let (result, bumped) = removal(p);
    assert!(result.is_err());
    assert_eq!(bumped, 1, "a refusal");

    // A directory that cannot be written fails the unlink, except for root.
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        std::fs::write(p, b"").unwrap();
        let dir = p.parent().unwrap();
        set_mode(dir, 0o500);
        let (result, bumped) = removal(p);
        set_mode(dir, 0o700);
        let err = result.unwrap_err();
        assert!(
            err.starts_with(&format!("Failed to delete {}: ", p.display())),
            "got: {err}"
        );
        assert_eq!(bumped, 1, "a failed delete");
    }
}

/// [`crate::db::hold_index`] callbacks run with the path before anything
/// is deleted, and stop once withdrawn.
#[test]
fn a_removal_releases_its_holders_first() {
    use std::sync::{Arc, Mutex};
    let (_dir, p) = db();
    drop(open_or_recreate(&p, &Default::default()).unwrap());
    let target = PathBuf::from(&p);
    // Whether the index still existed at each call for this path; removals
    // of other tests' paths run alongside and are ignored.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let holder = {
        let (calls, target) = (calls.clone(), target.clone());
        crate::db::hold_index(move |path| {
            if path == target {
                calls.lock().unwrap().push(path.exists());
            }
        })
    };
    remove_index_files(&target).unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [true],
        "called once, before the delete"
    );

    drop(holder);
    remove_index_files(&target).unwrap();
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "a withdrawn holder was called"
    );
}
