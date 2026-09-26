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

/// Processing settings asking for `tokenize`, the rest default.
fn with_tokenizer(tokenize: &str) -> ProcessingConfig {
    ProcessingConfig {
        tokenize: tokenize.into(),
        ..Default::default()
    }
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

#[test]
fn tokenizer_drift_wipes_db() {
    let (_dir, p) = db();
    let first_effective = {
        let conn = open_or_recreate(&p, &Default::default()).unwrap();
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) \
             VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
        info(&conn, "tokenize")
    };
    let conn = open_or_recreate(&p, &with_tokenizer("unicode61")).unwrap();
    assert_eq!(file_count(&conn), 0, "tokenizer drift should wipe rows");
    assert_ne!(first_effective, info(&conn, "tokenize"));
}

/// The position setting is part of the table definition, like the
/// tokenizer: a change wipes, and the layout built is the one recorded,
/// which is what every search reads back.
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

/// A word tokenizer cannot be queried by trigrams, so it keeps positions
/// whatever the flag says, and the flag alone then changes nothing.
#[test]
fn a_word_tokenizer_keeps_positions_and_ignores_the_flag() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &with_tokenizer("unicode61")).unwrap();
        assert!(!fts_sql(&conn).contains("detail"));
        assert_eq!(info(&conn, "fts_positions"), "true");
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime) VALUES ('x', '/', 0, 0)",
            [],
        )
        .unwrap();
    }
    let flag_off = ProcessingConfig {
        compact_text_index: false,
        ..with_tokenizer("unicode61")
    };
    let conn = open_or_recreate(&p, &flag_off).unwrap();
    assert_eq!(file_count(&conn), 1, "the same layout: nothing to rebuild");
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

#[test]
fn open_existing_reads_nondefault_tokenizer_without_wiping() {
    let (_dir, p) = db();
    {
        let conn = open_or_recreate(&p, &with_tokenizer("unicode61")).unwrap();
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
        "open_existing must not wipe a non-default-tokenizer DB"
    );
    // A word tokenizer: `hello` is one token, so a bare MATCH is exact.
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // Proof we neither rewrote the FTS table nor reset schema_info.
    assert_eq!(info(&conn, "tokenize"), "unicode61");
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

/// Under `-DSQLITE_TEMP_STORE=2`, VACUUM builds the replacement index in
/// memory unless `temp_store` is explicitly `FILE`.
#[test]
fn maintenance_opens_keep_temporaries_on_disk() {
    let (_dir, p) = db();
    let temp_store = |conn: &Connection| -> i64 {
        conn.query_row("PRAGMA temp_store", [], |r| r.get(0))
            .unwrap()
    };
    let conn = open_or_recreate(&p, &Default::default()).unwrap();
    assert_eq!(temp_store(&conn), 2, "the indexer's own profile is MEMORY");
    drop(conn);

    let conn = open_maintenance(&p).unwrap();
    assert_eq!(
        temp_store(&conn),
        1,
        "maintenance must build its temporaries on disk"
    );

    // The directory those temporaries land in is steerable, which keeps them
    // off a RAM-backed /tmp.
    let dir = Path::new(&p)
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    conn.execute_batch(&format!("PRAGMA temp_store_directory = '{}';", dir))
        .unwrap();
    let set: String = conn
        .query_row("PRAGMA temp_store_directory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(set, dir);
    conn.execute_batch("PRAGMA temp_store_directory = '';")
        .unwrap();
}

/// Builds the file at a path, or leaves it absent.
type Fixture = fn(&str);

/// Drives the GUI's "your index is being reset" modal: a false positive
/// announces a wipe that is not happening.
#[test]
fn index_needs_rebuild_only_when_the_schema_really_differs() {
    let cases: [(Fixture, bool, &str); 5] = [
        (|_| {}, false, "no file yet is a fresh install, not a reset"),
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
    let conn = open_keyed_with_pragmas(&p, true, Some(&key), PRAGMAS_MAINTENANCE).unwrap();
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
