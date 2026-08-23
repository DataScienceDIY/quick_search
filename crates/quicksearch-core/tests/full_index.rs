//! End-to-end phase-1 tests over a real tree and a real database.
//!

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use quicksearch_core::config::Config;
use quicksearch_core::file_handling::{
    count_extract_scope, mark_oversize_pending_na, ExtractCursor,
};
use quicksearch_core::indexing::{IndexingService, IndexingStatus, RootPhase};
use quicksearch_core::testutil::Scratch;

mod common;
use common::touch;

fn index_roots_once(roots: &[&Path], db: &Path, config: &Config) {
    common::IndexOnce {
        db,
        roots: roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect(),
        config,
        fresh_marker: true,
        encrypted: false,
    }
    .run()
}

fn index_once(root: &Path, db: &Path, config: &Config) {
    index_roots_once(&[root], db, config)
}

fn rows(db: &Path) -> Vec<(String, i64, i64)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT parent || name, mtime, content_state FROM files ORDER BY parent, name")
        .unwrap();
    let out = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
}

#[test]
fn reindexing_an_unchanged_tree_changes_nothing() {
    let root = Scratch::dir("stable");
    let (_db_dir, db) = Scratch::db("stable-db");
    let config = Config::default();

    touch(&root.join("a.txt"), b"alpha");
    touch(&root.join("sub/b.txt"), b"bravo");
    touch(&root.join("sub/deep/c.txt"), b"charlie");
    touch(&root.join("other/d.md"), b"delta");

    index_once(&root, &db, &config);
    let first = rows(&db);
    assert_eq!(first.len(), 4, "all four files indexed");

    index_once(&root, &db, &config);
    let second = rows(&db);

    assert_eq!(
        first, second,
        "an unchanged tree must re-index to an identical set"
    );
}

#[test]
fn deleted_files_are_removed_and_new_ones_added() {
    let root = Scratch::dir("churn");
    let (_db_dir, db) = Scratch::db("churn-db");
    let config = Config::default();

    touch(&root.join("keep.txt"), b"keep");
    touch(&root.join("remove.txt"), b"remove");
    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 2);

    std::fs::remove_file(root.join("remove.txt")).unwrap();
    touch(&root.join("added.txt"), b"added");
    index_once(&root, &db, &config);

    let names: Vec<String> = rows(&db)
        .into_iter()
        .map(|(p, _, _)| {
            Path::new(&p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        vec!["added.txt", "keep.txt"],
        "stale cleanup still works"
    );
}

#[test]
fn a_modified_file_is_updated_in_place() {
    let root = Scratch::dir("modify");
    let (_db_dir, db) = Scratch::db("modify-db");
    let config = Config::default();

    let target = root.join("doc.txt");
    touch(&target, b"first");
    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 1);

    touch(&target, b"second body, clearly different");
    let later = SystemTime::now() + Duration::from_secs(5);
    filetime_set(&target, later);

    index_once(&root, &db, &config);
    let after = rows(&db);
    assert_eq!(after.len(), 1, "still exactly one row");
    assert_ne!(before[0].1, after[0].1, "mtime was refreshed");
    assert_eq!(before[0].0, after[0].0, "same path");
}

fn filetime_set(path: &Path, when: SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
    f.sync_all().unwrap();
}

#[test]
#[cfg(unix)]
fn an_unreadable_directory_does_not_delete_its_rows() {
    use std::os::unix::fs::PermissionsExt;

    let root = Scratch::dir("blip");
    let (_db_dir, db) = Scratch::db("blip-db");
    let config = Config::default();

    touch(&root.join("visible.txt"), b"visible");
    let vault = root.join("vault");
    touch(&vault.join("secret.txt"), b"secret");
    touch(&vault.join("nested/deeper.txt"), b"deeper");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 3, "all three indexed while readable");

    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o000)).unwrap();
    index_once(&root, &db, &config);
    let during = rows(&db);
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        during.len(),
        3,
        "rows under an unreadable directory must survive, not be deleted"
    );

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 3);
}

#[test]
fn stopping_mid_run_deletes_nothing() {
    let root = Scratch::dir("stop");
    let (_db_dir, db) = Scratch::db("stop-db");
    let config = Config::default();

    for i in 0..1500 {
        touch(&root.join(format!("d{}/f{:04}.txt", i % 25, i)), b"body");
    }

    index_once(&root, &db, &config);
    let full = rows(&db);
    assert_eq!(full.len(), 1500);

    // Stop only once a snapshot shows a root still in flight — that is the
    // proof the stop landed mid-run.
    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let stopped_in_flight = loop {
        assert!(Instant::now() < deadline, "the run never became observable");
        match service.get_status() {
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            IndexingStatus::Preparing { .. } => {}
            IndexingStatus::Running { roots, .. }
                if roots.iter().any(|r| r.phase != RootPhase::Done) =>
            {
                break true;
            }
            _ => break false,
        }
        std::thread::sleep(Duration::from_micros(100));
    };
    assert!(
        stopped_in_flight,
        "the run finished before a stop could land mid-run; grow the fixture"
    );
    service.stop_indexing().unwrap();
    drop(service);
    std::thread::sleep(Duration::from_millis(250));

    let after = rows(&db);
    assert_eq!(
        after.len(),
        1500,
        "an interrupted run must not delete the rows it never got to"
    );
}

#[test]
fn a_stamped_run_has_finished_its_stale_cleanup() {
    // A stamp must always mean stale cleanup finished. The assertion is
    // one-sided on purpose, so timing can never make it fail spuriously:
    // whether the stop landed inside the window or never landed at all.
    let root = Scratch::dir("stop-stamp");
    let (_db_dir, db) = Scratch::db("stop-stamp-db");
    let mut config = Config::default();
    // No extraction, so the run's whole tail is the stale cleanup to interrupt.
    config.processing.maximum_text_file_size = 0;

    const FILES: usize = 8000;
    for i in 0..FILES {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 25, i)), b"body");
    }
    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), FILES);

    for i in 0..FILES {
        std::fs::remove_file(root.join(format!("d{}/f{:05}.txt", i % 25, i))).unwrap();
    }

    let marker = |db: &Path| -> Option<u64> {
        let conn = rusqlite::Connection::open(db).ok()?;
        quicksearch_core::db::repo::get_last_full_index(&conn)
    };

    let mut stamped = false;
    for delay_ms in [2u64, 5, 10, 20, 35, 60, 100, 200] {
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("DELETE FROM schema_info WHERE key = 'last_full_index'", [])
                .unwrap();
        }
        assert_eq!(marker(&db), None, "stamp cleared before the run");

        let service = IndexingService::new();
        service
            .start_indexing(
                vec![root.to_string_lossy().into_owned()],
                db.to_string_lossy().into_owned(),
                config.clone(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(delay_ms));
        service.stop_indexing().unwrap();
        drop(service);
        std::thread::sleep(Duration::from_millis(300));

        if marker(&db).is_some() {
            stamped = true;
            assert_eq!(
                rows(&db).len(),
                0,
                "delay {}ms: the run stamped itself complete but left stale rows behind",
                delay_ms
            );
            break;
        }
    }
    // Vacuity guard: at least one delay must have let a run finish and stamp.
    assert!(
        stamped,
        "no delay produced a stamped run; the fixture never exercised the \
         stamp-means-clean invariant"
    );
}

#[test]
fn starting_a_run_claims_the_status_before_it_returns() {
    let root = Scratch::dir("start-claims");
    let (_db_dir, db) = Scratch::db("start-claims-db");
    let config = Config::default();
    touch(&root.join("a.txt"), b"body");

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    // No sleep, no poll. `Preparing` is a claim: it holds the index exactly
    // as `Running` does.
    assert!(
        matches!(service.get_status(), IndexingStatus::Preparing { .. }),
        "status must be claimed synchronously, got {:?}",
        service.get_status()
    );

    let err = service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap_err();
    assert!(err.contains("already running"), "got: {}", err);

    service.stop_indexing().unwrap();
    drop(service);
    std::thread::sleep(Duration::from_millis(250));
}

#[test]
fn a_wide_tree_indexes_every_file_exactly_once() {
    let root = Scratch::dir("wide");
    let (_db_dir, db) = Scratch::db("wide-db");
    let config = Config::default();

    let count = 900;
    for i in 0..count {
        touch(&root.join(format!("d{}/f{:04}.txt", i % 13, i)), b"body");
    }

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), count, "every file indexed exactly once");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), count, "and the second run is stable");
}

#[test]
fn two_roots_walk_extract_and_clean_independently() {
    let root_a = Scratch::dir("multi-a");
    let root_b = Scratch::dir("multi-b");
    let (_db_dir, db) = Scratch::db("multi-db");
    let config = Config::default();

    for i in 0..60 {
        touch(
            &root_a.join(format!("a{:03}.txt", i)),
            b"alpha corpus xylophone",
        );
    }
    for i in 0..5 {
        touch(
            &root_b.join(format!("b{:03}.txt", i)),
            b"bravo corpus quagmire",
        );
    }

    index_roots_once(&[&root_a, &root_b], &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 65, "both roots fully walked");
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "per-root extraction drained both roots");
    for term in ["\"xylophone\"", "\"quagmire\""] {
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH ?1",
                [term],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            hits > 0,
            "content from both roots must be indexed ({})",
            term
        );
    }
    drop(conn);

    std::fs::remove_file(root_b.join("b000.txt")).unwrap();
    index_roots_once(&[&root_a, &root_b], &db, &config);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 64, "stale row swept across roots");
}

// ---------------------------------------------------------------------------
// Reconciliation without a global path set: classification and stale
// detection are per-directory, so these cover the cases a single directory
// read cannot see.
// ---------------------------------------------------------------------------

#[test]
fn a_deleted_directory_takes_its_whole_subtree_out_of_the_index() {
    let root = Scratch::dir("gone-dir");
    let (_db_dir, db) = Scratch::db("gone-dir-db");
    let config = Config::default();

    touch(&root.join("keep.txt"), b"stays");
    touch(&root.join("doomed/a.txt"), b"goes");
    touch(&root.join("doomed/b.txt"), b"goes");
    touch(&root.join("doomed/deeper/c.txt"), b"goes too");

    index_once(&root, &db, &config);
    assert_eq!(rows(&db).len(), 4, "all four indexed");

    std::fs::remove_dir_all(root.join("doomed")).unwrap();
    index_once(&root, &db, &config);

    let names: Vec<String> = rows(&db)
        .into_iter()
        .map(|(p, _, _)| {
            Path::new(&p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["keep.txt"], "the whole subtree is swept");
}

/// Only the target under the pruned directory exercises the alias exemption:
/// its parent is in the sweep's range yet legitimately absent from
/// `seen_dirs`. An out-of-root target is already outside the sweep's range.
#[test]
#[cfg(unix)]
fn a_symlink_target_in_an_unwalked_directory_survives_reindexing() {
    let root = Scratch::dir("alias-root");
    let outside = Scratch::dir("alias-outside");
    let db_dir = Scratch::dir("alias-db");
    let db = db_dir.join("index.sqlite");
    let mut config = Config::default();
    config.indexing.follow_symlinks = true;

    touch(&root.join("normal.txt"), b"inside the root");

    // In range, but under a hidden directory the walk prunes.
    let hidden_target = root.join(".pruned/inner.txt");
    touch(&hidden_target, b"only reachable through the link");
    std::os::unix::fs::symlink(&hidden_target, root.join("hidden_link.txt")).unwrap();

    // Out of range entirely.
    let outer_target = outside.join("target.txt");
    touch(&outer_target, b"outside the root entirely");
    std::os::unix::fs::symlink(&outer_target, root.join("outside_link.txt")).unwrap();

    index_once(&root, &db, &config);
    let first = rows(&db);
    assert_eq!(first.len(), 3, "both targets indexed under their own paths");
    assert!(
        first
            .iter()
            .any(|(p, _, _)| p.ends_with(".pruned/inner.txt")),
        "the pruned-directory target is stored under its canonical path"
    );

    index_once(&root, &db, &config);
    assert_eq!(rows(&db), first, "an aliased row must survive a re-index");

    // The other half of the setting: with links off, neither target is indexed.
    let db2 = db_dir.join("links-off.sqlite");
    index_once(&root, &db2, &Config::default());
    let off: Vec<String> = rows(&db2).into_iter().map(|(p, _, _)| p).collect();
    assert_eq!(off.len(), 1, "only the ordinary file: {:?}", off);
    assert!(off[0].ends_with("normal.txt"));
}

#[test]
#[cfg(unix)]
fn a_modified_symlink_target_is_updated_not_silently_ignored() {
    let root = Scratch::dir("alias-mod-root");
    let outside = Scratch::dir("alias-mod-outside");
    let (_db_dir, db) = Scratch::db("alias-mod-db");
    let mut config = Config::default();
    config.indexing.follow_symlinks = true;

    let target = outside.join("target.txt");
    touch(&target, b"first body");
    std::os::unix::fs::symlink(&target, root.join("link.txt")).unwrap();

    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 1);

    std::fs::write(&target, b"second body, quite different").unwrap();
    filetime_set(&target, SystemTime::now() + Duration::from_secs(120));

    index_once(&root, &db, &config);
    let after = rows(&db);
    assert_eq!(after.len(), 1, "still exactly one row");
    assert_eq!(after[0].0, before[0].0, "same path");
    assert_ne!(
        after[0].1, before[0].1,
        "mtime was refreshed, so it was re-read"
    );
}

#[test]
fn overlapping_roots_index_each_file_exactly_once() {
    let outer = Scratch::dir("overlap-outer");
    let (_db_dir, db) = Scratch::db("overlap-db");
    let config = Config::default();

    let inner = outer.join("inner");
    touch(&outer.join("top.txt"), b"in the outer root only");
    touch(&inner.join("shared.txt"), b"reachable from both roots");
    touch(&inner.join("also.txt"), b"likewise");

    index_roots_once(&[&outer, &inner], &db, &config);

    let all = rows(&db);
    assert_eq!(all.len(), 3, "three files, however many roots reach them");
    let shared: Vec<&(String, i64, i64)> = all
        .iter()
        .filter(|(p, _, _)| p.ends_with("shared.txt"))
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "the doubly-reachable file has exactly one row"
    );

    index_roots_once(&[&outer, &inner], &db, &config);
    assert_eq!(rows(&db), all, "a second overlapping run changes nothing");
}

#[test]
#[cfg(unix)]
fn a_directory_that_becomes_unreadable_deletes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let root = Scratch::dir("locked-later");
    let (_db_dir, db) = Scratch::db("locked-later-db");
    let config = Config::default();

    touch(&root.join("open.txt"), b"always readable");
    let vault = root.join("vault");
    touch(&vault.join("secret.txt"), b"readable for now");
    touch(&vault.join("deeper/also.txt"), b"and this one");

    index_once(&root, &db, &config);
    let before = rows(&db);
    assert_eq!(before.len(), 3, "all three indexed while readable");

    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o000)).unwrap();
    index_once(&root, &db, &config);
    let after = rows(&db);
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o755)).ok();

    assert_eq!(after, before, "an unreadable directory is not an empty one");
}

// ---------------------------------------------------------------------------
// Inline extraction: the walk finishes files whose head is the whole file.
// `hash_length` decides how much the walk reads, so 0 leaves nothing to
// inline and serves as the control the optimised path must match.
// ---------------------------------------------------------------------------

type ContentRow = (String, i64, Option<String>, Option<i64>);

fn content_rows(db: &Path) -> Vec<ContentRow> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT f.parent || f.name, f.content_state, ff.reason, LENGTH(d.text_zstd)
               FROM files f
               LEFT JOIN documents_text d ON d.file_id = f.id
               LEFT JOIN failed_files ff ON ff.file_id = f.id
              ORDER BY f.parent, f.name",
        )
        .unwrap();
    let out = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
}

fn stored_text(db: &Path, suffix: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT d.text_zstd FROM documents_text d
               JOIN files f ON f.id = d.file_id
              WHERE f.parent || f.name LIKE '%' || ?1",
            [suffix],
            |r| r.get(0),
        )
        .ok();
    blob.map(|b| String::from_utf8(zstd::decode_all(&b[..]).unwrap()).unwrap())
}

fn seed_mixed_tree(root: &Path) {
    let big = "lorem ipsum dolor sit amet ".repeat(600); // ~16 KiB, past any head
    touch(
        &root.join("small.txt"),
        b"a small plaintext body with xylophone in it",
    );
    touch(&root.join("large.txt"), big.as_bytes());
    touch(&root.join("empty.txt"), b"");
    // The NUL fails the binary guard, and the FF FE pair is not at offset 0,
    // so it is no BOM.
    touch(&root.join("bad.txt"), &[0x68, 0x69, 0xff, 0xfe, 0x00, 0x41]);
    // NUL soup: no extension table, magic, or text sniff claims it.
    touch(
        &root.join("blob.bin"),
        &[0x00, 0x01, 0x02, 0xfd, 0xfe, 0xff],
    );
    touch(
        &root.join("nested/deep/note.md"),
        b"# heading\n\nquagmire body text\n",
    );
}

#[test]
fn inline_extraction_produces_an_identical_index_to_the_two_pass_path() {
    let root = Scratch::dir("inline-equiv");
    let db_dir = Scratch::dir("inline-equiv-db");
    seed_mixed_tree(&root);

    // Control: hash_length 0 => empty head => nothing can be inlined.
    let mut control = Config::default();
    control.processing.hash_length = 0;
    let db_control = db_dir.join("control.sqlite");
    index_once(&root, &db_control, &control);

    // Optimised: the default head covers every small file in the tree.
    let optimised = Config::default();
    let db_opt = db_dir.join("optimised.sqlite");
    index_once(&root, &db_opt, &optimised);

    assert_eq!(
        content_rows(&db_control),
        content_rows(&db_opt),
        "inlining during the walk must not change a single indexed byte"
    );

    // And the bodies themselves round-trip identically, not just their lengths.
    for f in ["small.txt", "large.txt", "note.md"] {
        assert_eq!(
            stored_text(&db_control, f),
            stored_text(&db_opt, f),
            "stored body differs for {}",
            f
        );
    }
}

#[test]
fn the_head_boundary_decides_inlining_without_changing_the_result() {
    let root = Scratch::dir("inline-boundary");
    let db_dir = Scratch::dir("inline-boundary-db");

    let mut config = Config::default();
    config.processing.hash_length = 64;
    let at = "x".repeat(64);
    let past = "y".repeat(65);
    touch(&root.join("at.txt"), at.as_bytes());
    touch(&root.join("past.txt"), past.as_bytes());

    let db = db_dir.join("index.sqlite");
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE content_state != 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0, "both sides of the boundary end up extracted");
    drop(conn);

    assert_eq!(stored_text(&db, "at.txt").as_deref(), Some(at.as_str()));
    assert_eq!(stored_text(&db, "past.txt").as_deref(), Some(past.as_str()));
}

#[test]
fn undecodable_small_files_are_reported_as_failures_not_silently_skipped() {
    let root = Scratch::dir("inline-badutf8");
    let (_db_dir, db) = Scratch::db("inline-badutf8-db");

    // The NUL keeps this undecodable: without it these bytes would now
    // decode as windows-1252 and the test would assert nothing.
    touch(&root.join("bad.txt"), &[0x68, 0x00, 0x69, 0xff]);
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, msg): (i64, Option<String>) = conn
        .query_row(
            "SELECT f.content_state, ff.reason FROM files f \
               LEFT JOIN failed_files ff ON ff.file_id = f.id \
              WHERE f.parent || f.name LIKE '%bad.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, 2, "undecodable content is FAILED, not DONE or NA");
    assert!(
        msg.unwrap_or_default().contains("bad.txt"),
        "the failure names the file"
    );
}

#[test]
fn an_unreadable_legacy_office_file_fails_with_a_reason() {
    let root = Scratch::dir("legacy-doc");
    let (_db_dir, db) = Scratch::db("legacy-doc-db");

    touch(
        &root.join("broken.doc"),
        b"D0CF11E0 this is not really a compound file",
    );
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, msg): (i64, Option<String>) = conn
        .query_row(
            "SELECT f.content_state, ff.reason FROM files f \
               LEFT JOIN failed_files ff ON ff.file_id = f.id \
              WHERE f.parent || f.name LIKE '%broken.doc'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        state, 2,
        "an unreadable .doc is FAILED, not DONE-with-no-text"
    );
    let msg = msg.unwrap_or_default();
    assert!(msg.contains("broken.doc"), "names the file: {msg}");
    assert!(msg.contains("compound file"), "says what went wrong: {msg}");
}

/// The fix keeps size *and* mtime identical, so the walk sees an unchanged
/// file — only the run-start retry of failed files can rescue it.
#[test]
fn a_failed_file_is_retried_on_the_next_run() {
    let root = Scratch::dir("retry-failed");
    let (_db_dir, db) = Scratch::db("retry-failed-db");

    let flaky = root.join("flaky.txt");
    // The NUL keeps this undecodable (see the inline-badutf8 test).
    touch(&flaky, &[0x68, 0x00, 0x69, 0xff]);
    index_once(&root, &db, &Config::default());

    let probe = |db: &Path| -> (i64, i64) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(
            "SELECT f.content_state,
                    (SELECT COUNT(*) FROM failed_files) FROM files f \
              WHERE f.name = 'flaky.txt'",
            [],
            |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
        )
        .unwrap()
    };
    assert_eq!(probe(&db), (2, 1), "the first run records the failure");

    let mtime = std::fs::metadata(&flaky).unwrap().modified().unwrap();
    touch(&flaky, b"hiya");
    std::fs::File::options()
        .write(true)
        .open(&flaky)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();

    index_once(&root, &db, &Config::default());
    assert_eq!(
        probe(&db),
        (1, 0),
        "the retry re-extracted the fixed file and cleared the record"
    );
    assert_eq!(stored_text(&db, "flaky.txt").as_deref(), Some("hiya"));
}

#[test]
fn extensionless_text_files_are_indexed() {
    let root = Scratch::dir("extless");
    let (_db_dir, db) = Scratch::db("extless-db");

    touch(
        &root.join("README"),
        b"QuickSearch indexes zanzibar contents.\n",
    );
    touch(&root.join("Makefile"), b"all:\n\tcargo build --release\n");
    touch(&root.join("go.sum"), b"example.com/x v1.0.0 h1:abcdef=\n");
    touch(&root.join("blob"), &[0x00, 0x01, 0xfe, 0xff]);
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let state_of = |name: &str| -> i64 {
        conn.query_row(
            "SELECT content_state FROM files WHERE parent || name LIKE '%' || ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    };
    for name in ["README", "Makefile", "go.sum"] {
        assert_eq!(state_of(name), 1, "{} should be content-indexed", name);
    }
    assert_eq!(state_of("blob"), 3, "binary blob stays not-applicable");
    drop(conn);

    assert_eq!(
        stored_text(&db, "README").as_deref(),
        Some("QuickSearch indexes zanzibar contents.\n"),
        "the stored body round-trips"
    );
}

/// `stored_text` decodes the zstd sidecar with `String::from_utf8`, so a
/// `Some` result *is* the storage-is-UTF-8 assertion.
#[test]
fn utf16_files_are_stored_as_utf8() {
    let root = Scratch::dir("charset");
    let (_db_dir, db) = Scratch::db("charset-db");

    let reg_src =
        "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Xylograph]\r\n";
    let mut reg_body = vec![0xFF, 0xFE];
    reg_body.extend(reg_src.encode_utf16().flat_map(|u| u.to_le_bytes()));
    touch(&root.join("export.reg"), &reg_body);

    let mut extless = vec![0xFF, 0xFE];
    extless.extend(
        "utf16 notes about quokkas"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes()),
    );
    touch(&root.join("NOTES16"), &extless);

    touch(
        &root.join("legacy.txt"),
        b"un caf\xe9 tr\xe8s agr\xe9able pr\xe8s du mus\xe9e",
    );
    index_once(&root, &db, &Config::default());

    assert_eq!(stored_text(&db, "export.reg").as_deref(), Some(reg_src));
    assert_eq!(
        stored_text(&db, "NOTES16").as_deref(),
        Some("utf16 notes about quokkas")
    );
    assert_eq!(
        stored_text(&db, "legacy.txt").as_deref(),
        Some("un café très agréable près du musée")
    );
}

/// Covers both extraction paths: a small file the walk inlines, and one past
/// `hash_length` that the content pass opens.
#[test]
fn rtf_files_are_extracted() {
    let root = Scratch::dir("rtf");
    let (_db_dir, db) = Scratch::db("rtf-db");

    touch(
        &root.join("small.rtf"),
        br"{\rtf1\ansi Meeting notes about the pangolin budget.}",
    );
    let big_body = format!(
        r"{{\rtf1\ansi {}}}",
        r"paragraphs about the pangolin budget \par ".repeat(400)
    );
    assert!(big_body.len() > 8192, "must exceed the default head");
    touch(&root.join("big.rtf"), big_body.as_bytes());
    index_once(&root, &db, &Config::default());

    for name in ["small.rtf", "big.rtf"] {
        let text = stored_text(&db, name).unwrap_or_else(|| panic!("{} has no stored text", name));
        assert!(
            text.contains("pangolin budget"),
            "{}: {:?}",
            name,
            &text[..text.len().min(80)]
        );
        assert!(!text.contains(r"\rtf"), "{} stored control words", name);
    }
}

#[test]
fn the_extraction_denominator_counts_only_files_that_need_text() {
    let root = Scratch::dir("denominator");
    let (_db_dir, db) = Scratch::db("denominator-db");

    // `big.txt` exceeds `hash_length`, so it is the only row the content pass
    // opens; the NUL-bearing seven are claimed by nothing.
    for name in ["a.txt", "b.json"] {
        touch(&root.join(name), b"body bytes with no magic");
    }
    touch(&root.join("big.txt"), &vec![b'z'; 32 * 1024]);
    for name in ["d.mp4", "e.zip", "f.bin", "g.exe", "h.iso", "i.so", "j"] {
        touch(&root.join(name), b"\x00\x01body bytes\x00");
    }

    let config = Config::default();
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let count = |state: i64| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM files WHERE content_state = ?1",
            [state],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(0), 0, "a finished run leaves nothing pending");
    assert_eq!(count(1), 3, "the claimed files have text");
    assert_eq!(count(3), 7, "the rest are NA, and were NA from the walk on");
    drop(conn);

    // The exact calls the production path makes to fill
    // `RootProgress::extract_total`, in production order, against the finished
    // index — the live status cannot be sampled reliably: a ten-file tree
    // finishes between two polls.
    let conn = quicksearch_core::db::open_existing(db.to_str().unwrap(), true).unwrap();
    let cursor = ExtractCursor::for_root(root.to_str().unwrap());
    mark_oversize_pending_na(&conn, &cursor, &config).unwrap();
    let scope = count_extract_scope(&conn, &cursor, &config).unwrap();
    assert_eq!(
        (scope.pending, scope.already_done),
        (0, 3),
        "extract_total is the searchable set, not the file count"
    );
    assert_eq!(scope.pending + scope.already_done, 3);
    drop(conn);
}

#[test]
fn an_empty_file_is_done_with_no_snippet_sidecar() {
    let root = Scratch::dir("inline-empty");
    let (_db_dir, db) = Scratch::db("inline-empty-db");

    touch(&root.join("empty.txt"), b"");
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let (state, sidecars): (i64, i64) = conn
        .query_row(
            "SELECT f.content_state, (SELECT COUNT(*) FROM documents_text d WHERE d.file_id = f.id)
               FROM files f WHERE f.parent || f.name LIKE '%empty.txt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, 1, "an empty file is extracted, not failed");
    assert_eq!(sidecars, 0, "no zstd frame for an empty body");
}

#[test]
fn the_content_extension_filter_still_excludes_small_text_files() {
    let root = Scratch::dir("inline-filter");
    let (_db_dir, db) = Scratch::db("inline-filter-db");

    let mut config = Config::default();
    config.indexing.content_extensions = vec!["md".into()];
    touch(&root.join("kept.md"), b"kept quagmire body");
    touch(&root.join("skipped.txt"), b"skipped xylophone body");
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let states: Vec<(String, i64)> = conn
        .prepare("SELECT parent || name, content_state FROM files ORDER BY parent, name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (path, state) in &states {
        if path.ends_with("kept.md") {
            assert_eq!(*state, 1, "an allowed extension is extracted");
        } else {
            assert_eq!(*state, 3, "a filtered extension is NA, never inlined");
        }
    }
    drop(conn);
    assert_eq!(
        stored_text(&db, "skipped.txt"),
        None,
        "no body stored for a filtered file"
    );
}

#[test]
fn contentless_mode_still_indexes_inlined_files_without_storing_bodies() {
    let root = Scratch::dir("inline-contentless");
    let (_db_dir, db) = Scratch::db("inline-contentless-db");

    let mut config = Config::default();
    config.processing.store_text_for_snippets = false;
    touch(&root.join("small.txt"), b"searchable xylophone body");
    index_once(&root, &db, &config);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let sidecars: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents_text", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sidecars, 0, "contentless mode stores no bodies");
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH '\"xylophone\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        hits, 1,
        "an inlined file is still searchable in contentless mode"
    );
}

// ---------------------------------------------------------------------------
// How the stall tests measure interleaving.
//
// Shared by `a_heavy_root_does_not_stall_a_light_one` and its `_at_the_writer`
// sibling; `observe_overlap` collects the counters.
//
// A stall is counted in *work*, not milliseconds: while the heavy root's
// content pass runs, how many files the light root's walk was drained of,
// against how many heavy rows landed. Both counters are advanced by the same
// writer loop (`service_walking` / `service_extracting` in
// `indexing/pipeline.rs`), each root's turn bounded by one slice, so their
// ratio *is* the interleaving — and every way a host can be slow freezes both
// counters together and cancels out, which the wall-clock budget this
// replaced did not.
//
// `writer_turn_slice_ms = 0` makes the verdict arithmetic rather than a
// measurement of the host: with no time in a turn, a writer round is exactly
// one bounded piece of work per root — `service_walking` runs one
// `batch_size` quantum, `store_extracted` consumes exactly one row. The
// round, not the second, is the unit, the interleave floor is `QUANTUM : 1`
// by construction on any host, and load can only raise it. It also makes the
// ratio uniform across the pass, which is what lets the observed window be a
// partial sample of it.
//
// The bound: the serialised design — the regression, where the writer read
// the heavy batch itself and drained nobody meanwhile — manages one quantum
// of each per round, so it obeys `light < heavy + quantum`. Each test demands
// `light_drained ≥ MIN_INTERLEAVE × (heavy_stored + QUANTUM)` with
// `MIN_INTERLEAVE = 3`: three times a bound the broken design provably cannot
// reach, while the built one floors at `QUANTUM : 1` — 16:1 here.
//
// MIN_ROWS_SAMPLED: a partial window must not be degenerate. A window of `n`
// rows drains `QUANTUM × n` light files at the floor and must beat
// `MIN_INTERLEAVE × (n + QUANTUM)`, so under four rows the constant
// `+ QUANTUM` term decides the comparison instead of the design.
// ---------------------------------------------------------------------------

/// What one watch of a heavy/light overlap saw; see [`observe_overlap`].
struct Overlap {
    /// Light files drained and heavy rows stored, across the window in which
    /// the heavy root extracted while the light root walked.
    light_drained: usize,
    heavy_stored: usize,
    /// The heavy root's `extract_total` and pool size, for the fixture guards.
    heavy_pending: usize,
    heavy_pool: usize,
    /// Publications in which the heavy root's row count moved. Reported,
    /// never asserted on: it is really `min(writer rounds, watcher polls)`,
    /// and is here only to make a surprising run legible.
    heavy_steps: usize,
    /// Longest this watcher went between polls: distinguishes a writer that
    /// gulped the pass in one turn from a watcher descheduled past it.
    worst_gap: Duration,
}

/// Watch a two-root run until the heavy root has finished extracting, report
/// how the two counters moved while both were in flight, then stop the run.
/// Removing the fixture is the caller's job.
///
/// The window opens on the first snapshot holding both roots in flight and
/// closes when the heavy root leaves `Extracting` — not when the light root
/// finishes walking, which CI showed to be a race between two rates with no
/// fixed ratio across hosts. A light walk that ends early just stops
/// contributing, understating the interleaving, never overstating it.
/// Panics if the window never opened.
fn observe_overlap(service: &IndexingService, heavy_tag: &str, light_tag: &str) -> Overlap {
    let mut opened: Option<(usize, usize)> = None; // (light.walked, heavy.extracted)
    let mut last = (0usize, 0usize);
    let mut heavy_pending = 0usize;
    let mut heavy_pool = 0usize;
    let mut heavy_steps = 0usize;
    let mut stepped_at = 0usize;
    let mut worst_gap = Duration::ZERO;
    let mut polled_at = Instant::now();
    // Published (heavy, light) phase pairs, kept only for the failure diagnosis.
    let mut phases: Vec<(RootPhase, RootPhase)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let mut in_window = false;
        worst_gap = worst_gap.max(polled_at.elapsed());
        polled_at = Instant::now();
        match service.get_status() {
            IndexingStatus::Running { roots, .. } => {
                let heavy_p = roots.iter().find(|r| r.root.contains(heavy_tag));
                let light_p = roots.iter().find(|r| r.root.contains(light_tag));
                if let (Some(h), Some(l)) = (heavy_p, light_p) {
                    if phases.last() != Some(&(h.phase, l.phase)) {
                        phases.push((h.phase, l.phase));
                    }
                    // Opening takes both roots in flight; staying open takes
                    // only the heavy root's pass.
                    in_window = if opened.is_none() {
                        h.phase == RootPhase::Extracting && l.phase == RootPhase::Walking
                    } else {
                        h.phase == RootPhase::Extracting
                    };
                    if in_window {
                        last = (l.walked, h.extracted);
                        if opened.is_none() {
                            opened = Some(last);
                            stepped_at = h.extracted;
                        }
                        // A fresh publication, not a fresh poll.
                        if h.extracted > stepped_at {
                            stepped_at = h.extracted;
                            heavy_steps += 1;
                        }
                        if let Some(total) = h.extract_total {
                            heavy_pending = total;
                        }
                        heavy_pool = h.total_workers;
                    }
                }
            }
            // Claimed but not yet walking; nothing to sample yet.
            IndexingStatus::Preparing { .. } => {}
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => break,
        }
        // Both phases are monotone, so a closed window will not reopen.
        if opened.is_some() && !in_window {
            break;
        }
        // Poll finer than a writer round, or this loop sets the window's edges.
        std::thread::sleep(Duration::from_micros(500));
    }
    service.stop_indexing().unwrap();

    let Some((light_open, heavy_open)) = opened else {
        panic!(
            "never observed the heavy root extracting while the light root walked. \
             Published (heavy, light) phases: {:?}. An empty list means neither \
             root matched the tags {:?}/{:?}; a list with no heavy Extracting in \
             it means the heavy root's content pass began and ended between two \
             status publications, so lower `writer_turn_slice_ms` until a writer \
             round is shorter than that pass.",
            phases, heavy_tag, light_tag
        );
    };
    Overlap {
        light_drained: last.0 - light_open,
        heavy_stored: last.1 - heavy_open,
        heavy_pending,
        heavy_pool,
        heavy_steps,
        worst_gap,
    }
}

/// A slow root must not stall the others: one root's heavy extraction used to
/// occupy the single writer thread for a whole batch at a time, during which
/// no other root's walk was drained.
///
/// The assertion is about *stalls*, not throughput: writing is serial by
/// construction (one SQLite connection), so a wall-clock comparison would
/// mostly measure the machine. The measurement and bound are derived above
/// [`Overlap`].
#[test]
fn a_heavy_root_does_not_stall_a_light_one() {
    // The writer's round-robin quantum: the bound below is arithmetic in it,
    // and a default 500-file round is a coarse enough publish interval to
    // look like a stall on its own.
    const QUANTUM: usize = 16;
    // Few files, each big enough that reading one is real work, with a small
    // `maximum_text_size` so the cost lands in extraction: the bound grows
    // with the row count, while what the light root drains does not. Sized
    // for the guards — enough that the rows arrive over separate writer
    // rounds, cheap enough that a loaded runner still finishes in seconds.
    const HEAVY_FILES: usize = 12;
    // A wide tree of tiny files, inlined by their walk workers, so this root
    // has no extraction phase of its own to confuse the window with. It must
    // still be walking when the heavy pass starts.
    const LIGHT_FILES: usize = 16_000;
    // Light files drained per (heavy row + quantum); derived above `Overlap`.
    const MIN_INTERLEAVE: usize = 3;
    // Fewest heavy rows a window needs; derived above `Overlap`.
    const MIN_ROWS_SAMPLED: usize = 4;

    let heavy = Scratch::dir("stall-heavy");
    // ~2 MiB each, 24 MB total.
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(58_000)
        .into_bytes();
    for i in 0..HEAVY_FILES {
        touch(&heavy.join(format!("d{}/big{:04}.txt", i % 4, i)), &body);
    }
    let light = Scratch::dir("stall-light");
    for i in 0..LIGHT_FILES {
        touch(&light.join(format!("d{}/f{:05}.txt", i % 60, i)), b"x");
    }

    let db_dir = Scratch::dir("stall-db");
    let db = db_dir.join("index.sqlite");
    let roots = vec![
        heavy.to_string_lossy().into_owned(),
        light.to_string_lossy().into_owned(),
    ];

    let mut config = Config::default();
    config.processing.maximum_text_size = 1024;
    // Above the heavy files, or `mark_oversize_pending_na` writes them off as
    // N/A before the pass starts and there is no extraction phase at all.
    config.processing.maximum_text_file_size = 4 * 1024 * 1024;
    config.processing.batch_size = QUANTUM;
    // Zero: the round becomes the unit and the interleave floor is
    // `quantum : 1` on any host — the derivation above `Overlap`.
    config.processing.writer_turn_slice_ms = 0;
    // One extraction thread for the heavy root. `root_workers` is keyed by
    // the `indexing_paths` spelling; both sides canonicalize before matching.
    config.paths.indexing_paths = roots.clone();
    config.indexing.root_workers.insert(roots[0].clone(), 1);
    // The default WAL cap is far above anything this run writes; that stops
    // being true if the fixture grows by an order of magnitude.

    let service = IndexingService::new();
    service
        .start_indexing(roots, db.to_string_lossy().into_owned(), config.clone())
        .unwrap();
    let seen = observe_overlap(&service, "stall-heavy", "stall-light");
    drop(service);

    // Removed explicitly before the assertions, unlike the rest of this file:
    // the fixture is generated and identical every run, and leaving tens of
    // MB in a RAM-backed /tmp behind a failure is itself a reason to fail.
    std::fs::remove_dir_all(&heavy).ok();
    std::fs::remove_dir_all(&light).ok();
    std::fs::remove_dir_all(&db_dir).ok();

    // Fixture guards: each silently costs a factor of the margin below if it
    // stops holding.
    assert_eq!(
        seen.heavy_pool, 1,
        "the heavy root must extract on the single worker root_workers asked for; \
         with the default four its pass is four times shorter and so is the margin"
    );
    assert_eq!(
        seen.heavy_pending, HEAVY_FILES,
        "every heavy file must reach the content pass; one inlined by its walk \
         worker never produces an extraction phase to overlap with"
    );
    // The window may be a sub-sample of the pass — every round contributes
    // the same ratio — but below four rows the bound's `+ QUANTUM` term
    // dominates and a passing ratio would be arithmetic, not evidence.
    assert!(
        seen.heavy_stored >= MIN_ROWS_SAMPLED,
        "only {} of {} heavy rows landed inside the observed window, fewer than \
         the {} a verdict needs (worst watcher gap {:?}, rows seen over {} \
         rounds) — a gap near the pass's own length means this watcher was \
         descheduled past it, not that the writer gulped it",
        seen.heavy_stored,
        HEAVY_FILES,
        MIN_ROWS_SAMPLED,
        seen.worst_gap,
        seen.heavy_steps
    );

    eprintln!(
        "light files drained while the heavy root extracted: {} against {} heavy \
         rows (quantum {}) landing over {} rounds, worst watcher gap {:?} — \
         {}x the {}x required; the \
         serialised design cannot exceed 1x",
        seen.light_drained,
        seen.heavy_stored,
        QUANTUM,
        seen.heavy_steps,
        seen.worst_gap,
        seen.light_drained / (seen.heavy_stored + QUANTUM),
        MIN_INTERLEAVE
    );
    assert!(
        seen.light_drained >= MIN_INTERLEAVE * (seen.heavy_stored + QUANTUM),
        "the light root was drained of only {} files while the heavy root landed \
         {} rows; one quantum of each per round is all a writer that extracts \
         inline can manage, so anything near {} means the extraction is back on \
         the writer thread",
        seen.light_drained,
        seen.heavy_stored,
        seen.heavy_stored + QUANTUM
    );
}

/// The sibling of [`a_heavy_root_does_not_stall_a_light_one`] for the cost
/// that test keeps small: the writer's own tokenising. Here each heavy row
/// carries the default 256 KiB of text and its FTS5 trigram insert runs on
/// the writer thread, inside the transaction, where nothing can take it off.
/// Before turns had a slice, an extraction turn wrote everything it found and
/// the ratio below came in under one.
#[test]
fn a_heavy_root_does_not_stall_a_light_one_at_the_writer() {
    const QUANTUM: usize = 16;
    // Over the walk's inline threshold, with the full 256 KiB of stored
    // text: the tokenising is what is measured.
    const HEAVY_FILES: usize = 32;
    // Wide enough that the light walk outlasts enough of the heavy pass.
    const LIGHT_FILES: usize = 16_000;
    // Three times a bound the unsliced writer cannot reach.
    const MIN_INTERLEAVE: usize = 3;
    // Derived above `Overlap`.
    const MIN_ROWS_SAMPLED: usize = 4;

    let heavy = Scratch::dir("stall-writer-heavy");
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(9_000)
        .into_bytes();
    for i in 0..HEAVY_FILES {
        touch(&heavy.join(format!("d{}/big{:04}.txt", i % 8, i)), &body);
    }
    let light = Scratch::dir("stall-writer-light");
    for i in 0..LIGHT_FILES {
        touch(&light.join(format!("d{}/f{:05}.txt", i % 60, i)), b"x");
    }

    let db_dir = Scratch::dir("stall-writer-db");
    let db = db_dir.join("index.sqlite");
    let roots = vec![
        heavy.to_string_lossy().into_owned(),
        light.to_string_lossy().into_owned(),
    ];

    let mut config = Config::default();
    config.processing.batch_size = QUANTUM;
    // As in the sibling: at zero the round is the unit and the floor is
    // `quantum : 1` whatever the host does.
    config.processing.writer_turn_slice_ms = 0;
    config.paths.indexing_paths = roots.clone();
    // Four readers, so the ready channel is full when the writer's turn comes.
    config.indexing.root_workers.insert(roots[0].clone(), 4);

    let service = IndexingService::new();
    service
        .start_indexing(roots, db.to_string_lossy().into_owned(), config.clone())
        .unwrap();
    let seen = observe_overlap(&service, "stall-writer-heavy", "stall-writer-light");
    drop(service);

    std::fs::remove_dir_all(&heavy).ok();
    std::fs::remove_dir_all(&light).ok();
    std::fs::remove_dir_all(&db_dir).ok();

    assert_eq!(
        seen.heavy_pool, 4,
        "the heavy root must extract on four workers"
    );
    assert_eq!(
        seen.heavy_pending, HEAVY_FILES,
        "every heavy file must reach the content pass"
    );
    // As in the sibling: the window may be partial, but not degenerate.
    assert!(
        seen.heavy_stored >= MIN_ROWS_SAMPLED,
        "only {} of {} heavy rows landed inside the observed window, fewer than \
         the {} a verdict needs (worst watcher gap {:?}, rows seen over {} \
         rounds) — a gap near the pass's own length means this watcher was \
         descheduled past it, not that the writer gulped it",
        seen.heavy_stored,
        HEAVY_FILES,
        MIN_ROWS_SAMPLED,
        seen.worst_gap,
        seen.heavy_steps
    );

    eprintln!(
        "light files drained while the heavy root tokenised: {} against {} heavy \
         rows (quantum {}) landing over {} rounds, worst watcher gap {:?} — \
         {}x the {}x required",
        seen.light_drained,
        seen.heavy_stored,
        QUANTUM,
        seen.heavy_steps,
        seen.worst_gap,
        seen.light_drained / (seen.heavy_stored + QUANTUM),
        MIN_INTERLEAVE
    );
    assert!(
        seen.light_drained >= MIN_INTERLEAVE * (seen.heavy_stored + QUANTUM),
        "the light root was drained of only {} files while the heavy root landed \
         {} rows; an extraction turn is writing to the end of its batch again \
         instead of yielding at its slice",
        seen.light_drained,
        seen.heavy_stored
    );
}

/// The write-ahead log must not grow for the length of a run: autocheckpoint
/// can only reset the log at an instant no reader holds a read mark, and a
/// run keeps a reader per root querying continuously (the prompting case: a
/// 12.5 GiB index carrying a 21.6 GiB log).
///
/// The assertion is about the peak *while running* — `stop_indexing` and the
/// post-run maintenance both truncate the log on the way out, so a reading
/// taken afterwards proves nothing.
#[test]
fn the_wal_stays_bounded_during_a_run() {
    let root = Scratch::dir("wal-bound");
    // Text-heavy: the FTS index is what fills the log.
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(200)
        .into_bytes();
    for i in 0..4000 {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 40, i)), &body);
    }

    let db_dir = Scratch::dir("wal-bound-db");
    let db = db_dir.join("index.sqlite");
    let wal = db_dir.join("index.sqlite-wal");
    let mut config = Config::default();
    // The floor `MINIMUM_WAL_SIZE` clamps to, so the cap is exercised many
    // times rather than once at the end.
    config.processing.maximum_wal_size = 16 * 1024 * 1024;

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            config.clone(),
        )
        .unwrap();

    let mut peak = 0u64;
    let mut checkpointed = false;
    let mut last = 0u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        peak = peak.max(len);
        // Vacuity guard: without a mid-run checkpoint the bound could be met
        // by the fixture being too small.
        if len + 1024 * 1024 < last {
            checkpointed = true;
        }
        last = len;
        match service.get_status() {
            IndexingStatus::Running { .. } | IndexingStatus::Preparing { .. } => {}
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => break,
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let idle_by = Instant::now() + Duration::from_secs(120);
    while Instant::now() < idle_by && !matches!(service.get_status(), IndexingStatus::Idle) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    drop(service);

    eprintln!("peak WAL during the run: {} bytes", peak);
    assert!(
        checkpointed,
        "the log never shrank mid-run; the fixture is not exercising the cap"
    );
    // Generously above the cap: the check runs between rounds, so a round's
    // commits can land on top, and a checkpoint that loses its lock race
    // defers to the next cap of growth.
    assert!(
        peak < 96 * 1024 * 1024,
        "the log peaked at {} bytes against a 16 MiB cap",
        peak
    );
    assert_eq!(after, 0, "the optimize pass leaves an empty log behind");
}

/// Stopping a run does not skip the optimize pass: a run cut short is exactly
/// when the log is largest and nothing else will land it.
///
/// Asserted through [`repo::optimize_count`] — a latch, not a sample: polling
/// for the transient `Optimizing` status missed it more often the faster the
/// indexer got.
#[test]
fn a_stopped_run_is_still_optimized() {
    let root = Scratch::dir("stop-optimize");
    let body: Vec<u8> = "sphinx of black quartz judge my vow "
        .repeat(200)
        .into_bytes();
    for i in 0..4000 {
        touch(&root.join(format!("d{}/f{:05}.txt", i % 40, i)), &body);
    }

    let db_dir = Scratch::dir("stop-optimize-db");
    let db = db_dir.join("index.sqlite");
    let wal = db_dir.join("index.sqlite-wal");
    let dir_key = db_dir.to_string_lossy().into_owned();

    // Per-directory, so a sibling test optimizing its own scratch index on
    // another thread cannot satisfy this.
    let before = quicksearch_core::db::repo::optimize_count(&dir_key);

    let service = IndexingService::new();
    service
        .start_indexing(
            vec![root.to_string_lossy().into_owned()],
            db.to_string_lossy().into_owned(),
            Config::default(),
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 512 * 1024 {
            break;
        }
        if let IndexingStatus::Error(e) = service.get_status() {
            panic!("indexing failed: {}", e);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    service.request_stop();

    // Idle is the *end* of the pass, and unlike Optimizing it is a resting
    // state — waiting for it cannot miss it however fast the pass was.
    let idle_by = Instant::now() + Duration::from_secs(120);
    loop {
        match service.get_status() {
            IndexingStatus::Idle => break,
            IndexingStatus::Error(e) => panic!("indexing failed: {}", e),
            _ => {}
        }
        assert!(
            Instant::now() < idle_by,
            "the stopped run never reached Idle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        quicksearch_core::db::repo::optimize_count(&dir_key),
        before + 1,
        "a stopped run must still run PRAGMA optimize against its index"
    );
    assert_eq!(
        std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
        0,
        "the optimize pass must land the stopped run's log"
    );
    drop(service);
}

/// High-byte binaries (protobuf and friends: no NUL, no control bytes) clear
/// the binary guard; the tightened sniff must still refuse them. The row must
/// survive in `files` while acquiring no `documents_text` sidecar and no
/// `failed_files` entry — no text is not a failure — and the `.txt` twin with
/// the same bytes must still extract.
#[test]
fn high_byte_binaries_are_listed_but_not_text_extracted() {
    let root = Scratch::dir("sniff-binary");
    let (_db_dir, db) = Scratch::db("sniff-binary-db");

    // Head of a real protobuf-framed GPS log: varint framing around ASCII
    // NMEA sentences. No NUL, no control-byte density — it clears the binary
    // guard on its own.
    let mut pb = b"\x10\n\x02v1\x10\x01\x18\xe2\xe3\xfc\xd3\x9d\xca\x97\xe4\x189\x08".to_vec();
    pb.extend_from_slice(b"\x12*$GNGGA,181558.00,,,,,0,00,99.99,,,,,,*78\r\n");
    assert!(!pb.contains(&0u8), "fixture must not trip the NUL guard");

    let legacy = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.";

    touch(&root.join("rtk.pb"), &pb);
    touch(&root.join("legacy.txt"), legacy);
    touch(&root.join("notes.md"), b"ordinary utf-8 prose");
    index_once(&root, &db, &Config::default());

    let conn = rusqlite::Connection::open(&db).unwrap();
    let probe = |suffix: &str| -> (i64, i64, i64) {
        conn.query_row(
            "SELECT f.content_state,
                    (SELECT COUNT(*) FROM documents_text d WHERE d.file_id = f.id),
                    (SELECT COUNT(*) FROM failed_files x WHERE x.file_id = f.id)
               FROM files f WHERE f.parent || f.name LIKE '%' || ?1",
            [suffix],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or_else(|e| panic!("{suffix} must be indexed: {e}"))
    };

    // 3 = not applicable.
    assert_eq!(
        probe("rtk.pb"),
        (3, 0, 0),
        "a high-byte binary must be listed, not extracted, and not a failure"
    );

    let (state, sidecars, failures) = probe("legacy.txt");
    assert_eq!(
        (state, failures),
        (1, 0),
        "a legacy-charset .txt must still extract"
    );
    assert_eq!(sidecars, 1, "and must still store its text");

    assert_eq!(probe("notes.md"), (1, 1, 0), "ordinary UTF-8 is unaffected");
}
