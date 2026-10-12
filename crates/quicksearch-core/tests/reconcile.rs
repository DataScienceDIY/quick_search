//! End-to-end tests for reconciling a real index against a changed
//! configuration.
//!
//! Every test that keeps its index also pins `schema_info.created_at`, which
//! only a wipe can change: losing the index file is otherwise silent, because
//! the next run rebuilds it.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use quicksearch_core::config::{diff_actions, Config};
use quicksearch_core::db;
use quicksearch_core::extract::Registry;
use quicksearch_core::file_handling::get_file_hash;
use quicksearch_core::scope::{advance, WorkCursor, SLICE};
use quicksearch_core::testutil::Scratch;

mod common;
use common::touch;

fn index_once(db: &Path, config: &Config) {
    common::IndexOnce {
        db,
        roots: config.paths.indexing_paths.clone(),
        config,
        fresh_marker: true,
        encrypted: false,
    }
    .run()
}

/// Apply the reconciliation `old -> new` implies, exactly as the coordinator
/// would: to completion, in slices, against the live index.
fn reconcile(db: &Path, old: &Config, new: &Config) -> (usize, usize) {
    let actions = diff_actions(old, new);
    assert!(
        !actions.requires_rebuild,
        "this change must not need a wipe"
    );
    let mut conn = db::open_existing(&db.to_string_lossy(), true).unwrap();
    let registry = Registry::default_set();
    let mut cursor = WorkCursor::new(actions.work, new).unwrap();
    let run = AtomicBool::new(false);
    while !cursor.done() {
        advance(
            &mut conn,
            new,
            &registry,
            &mut cursor,
            Instant::now() + SLICE,
            &run,
        )
        .unwrap();
    }
    (cursor.deleted, cursor.recontented)
}

fn conn(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).unwrap()
}

/// Every indexed path, sorted by path rather than by the index's own
/// `(parent, name)` order, which groups by directory.
fn paths(db: &Path) -> Vec<String> {
    let c = conn(db);
    let mut stmt = c.prepare("SELECT parent || name FROM files").unwrap();
    let mut out: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out.sort();
    out
}

/// The file names of [`paths`], in the same order.
fn names(db: &Path) -> Vec<String> {
    paths(db)
        .iter()
        .map(|p| {
            Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// FTS rows matching `term`, asked for as the index's layout allows.
fn fts_count(db: &Path, term: &str) -> i64 {
    quicksearch_core::testutil::fts_count(&conn(db), term)
}

fn count(db: &Path, sql: &str) -> i64 {
    conn(db).query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The index's birth certificate. A rebuild deletes the file, so this value
/// changing (or the row vanishing) is proof the index was thrown away.
fn created_at(db: &Path) -> String {
    conn(db)
        .query_row(
            "SELECT value FROM schema_info WHERE key = 'created_at'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// FTS postings and stored-text blobs still in the index.
fn residue(db: &Path) -> (i64, i64) {
    (
        count(db, "SELECT COUNT(*) FROM searchabletext"),
        count(db, "SELECT COUNT(*) FROM documents_text"),
    )
}

/// Rows in a dependent table whose file is gone. `searchabletext` is the one
/// that matters: FTS5 has no foreign key, so a forgotten delete leaves
/// postings that surface as hits for files no longer indexed. The other
/// three cascade, checked in case a connection skips `PRAGMA foreign_keys`.
fn orphans(db: &Path) -> i64 {
    [
        ("searchabletext", "rowid"),
        ("documents_text", "file_id"),
        ("failed_files", "file_id"),
    ]
    .iter()
    .map(|(table, key)| {
        count(
            db,
            &format!(
                "SELECT COUNT(*) FROM {0} t \
                 WHERE NOT EXISTS (SELECT 1 FROM files f WHERE f.id = t.{1})",
                table, key
            ),
        )
    })
    .sum()
}

fn tree(root: &Path) {
    touch(&root.join("keep.txt"), b"alpha keep");
    touch(&root.join("notes.md"), b"bravo notes");
    touch(&root.join("build/output.log"), b"charlie log");
    touch(&root.join("build/keep2.txt"), b"delta keep");
    touch(&root.join("node_modules/dep/index.js"), b"echo dep");
}

fn base_config(root: &Path, db: &Path) -> Config {
    let mut config = Config::default();
    config.paths.indexing_paths = vec![root.to_string_lossy().into_owned()];
    config.paths.database_path = db.to_string_lossy().into_owned();
    config.indexing.ignore_patterns = vec![];
    config
}

#[test]
fn adding_an_ignore_pattern_prunes_instead_of_rebuilding() {
    let root = Scratch::canonical("ignore-add");
    let db_dir = Scratch::canonical("ignore-add-db");
    let db = db_dir.join("index.sqlite");
    tree(&root);

    let old = base_config(&root, &db);
    index_once(&db, &old);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 5);
    assert_eq!(residue(&db), (5, 5), "every file was extracted and stored");

    let mut new = old.clone();
    new.indexing.ignore_patterns = vec!["*.log".into(), "node_modules".into()];
    let (deleted, _) = reconcile(&db, &old, &new);

    assert_eq!(deleted, 2, "the log and the dependency");
    assert_eq!(names(&db), vec!["keep2.txt", "keep.txt", "notes.md"]);
    assert_eq!(
        residue(&db),
        (3, 3),
        "the FTS postings and the extracted text went with the rows"
    );
    assert_eq!(orphans(&db), 0);
    assert_eq!(
        fts_count(&db, "echo"),
        0,
        "the ignored file's content is no longer findable"
    );
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn removing_an_ignore_pattern_reindexes_and_deletes_nothing() {
    let root = Scratch::canonical("ignore-remove");
    let db_dir = Scratch::canonical("ignore-remove-db");
    let db = db_dir.join("index.sqlite");
    tree(&root);

    let mut old = base_config(&root, &db);
    old.indexing.ignore_patterns = vec!["*.log".into()];
    index_once(&db, &old);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 4, "the log was never indexed");

    let mut new = old.clone();
    new.indexing.ignore_patterns = vec![];
    let actions = diff_actions(&old, &new);
    assert!(!actions.requires_rebuild);
    assert!(actions.work.reindex, "a walk must follow");
    let (deleted, recontented) = reconcile(&db, &old, &new);
    assert_eq!((deleted, recontented), (0, 0), "nothing was touched");
    assert_eq!(paths(&db).len(), 4, "still four until the walk runs");

    index_once(&db, &new);
    assert_eq!(paths(&db).len(), 5, "the walk found the log");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn removing_a_root_takes_only_its_own_entries() {
    let base = Scratch::canonical("roots");
    let kept = base.join("kept");
    let dropped = base.join("dropped");
    let db_dir = Scratch::canonical("roots-db");
    let db = db_dir.join("index.sqlite");
    touch(&kept.join("a.txt"), b"alpha");
    touch(&kept.join("sub/b.txt"), b"bravo");
    touch(&dropped.join("c.txt"), b"charlie");
    touch(&dropped.join("sub/deep/d.txt"), b"delta");

    let mut old = base_config(&kept, &db);
    old.paths.indexing_paths = vec![
        kept.to_string_lossy().into_owned(),
        dropped.to_string_lossy().into_owned(),
    ];
    index_once(&db, &old);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 4);

    let mut new = old.clone();
    new.paths.indexing_paths = vec![kept.to_string_lossy().into_owned()];
    let actions = diff_actions(&old, &new);
    assert!(!actions.work.reindex, "nothing new to find");
    let (deleted, _) = reconcile(&db, &old, &new);

    assert_eq!(deleted, 2);
    assert!(
        paths(&db)
            .iter()
            .all(|p| p.starts_with(kept.to_str().unwrap())),
        "only the kept root survives: {:?}",
        paths(&db)
    );
    assert_eq!(residue(&db), (2, 2), "the kept root keeps its text");
    assert_eq!(orphans(&db), 0, "nothing left behind");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn adding_a_root_keeps_everything_already_indexed() {
    let base = Scratch::canonical("root-add");
    let first = base.join("first");
    let second = base.join("second");
    let db_dir = Scratch::canonical("root-add-db");
    let db = db_dir.join("index.sqlite");
    touch(&first.join("a.txt"), b"alpha");
    touch(&second.join("b.txt"), b"bravo");

    let old = base_config(&first, &db);
    index_once(&db, &old);
    let born = created_at(&db);
    let before = paths(&db);
    assert_eq!(before.len(), 1);

    let mut new = old.clone();
    new.paths.indexing_paths = vec![
        first.to_string_lossy().into_owned(),
        second.to_string_lossy().into_owned(),
    ];
    let (deleted, recontented) = reconcile(&db, &old, &new);
    assert_eq!((deleted, recontented), (0, 0));
    assert_eq!(paths(&db), before);

    index_once(&db, &new);
    assert_eq!(paths(&db).len(), 2);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn narrowing_content_extensions_keeps_the_file_findable_by_name() {
    let root = Scratch::canonical("content");
    let db_dir = Scratch::canonical("content-db");
    let db = db_dir.join("index.sqlite");
    touch(&root.join("notes.md"), b"markdown body");
    touch(&root.join("readme.txt"), b"plain body");

    let old = base_config(&root, &db);
    index_once(&db, &old);
    let born = created_at(&db);
    assert_eq!(residue(&db).0, 2, "both extracted");

    let mut narrowed = old.clone();
    narrowed.indexing.content_extensions = vec!["txt".into()];
    let (deleted, recontented) = reconcile(&db, &old, &narrowed);
    assert_eq!(deleted, 0, "no file left the index");
    assert_eq!(recontented, 1);
    assert_eq!(paths(&db).len(), 2, "both rows are still there");
    assert_eq!(residue(&db).0, 1, "only the .txt keeps its postings");
    assert_eq!(fts_count(&db, "markdown"), 0);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM files WHERE content_state = 3"),
        1,
        "the excluded file is parked, not pending"
    );

    let (deleted, recontented) = reconcile(&db, &narrowed, &old);
    assert_eq!(deleted, 0);
    assert_eq!(recontented, 1);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM files WHERE content_state = 0"),
        1,
        "pending again"
    );
    index_once(&db, &old);
    assert_eq!(residue(&db).0, 2, "its text came back");
    assert_eq!(created_at(&db), born, "the index was never rebuilt");
}

#[test]
fn turning_hidden_files_off_prunes_whole_hidden_subtrees() {
    let root = Scratch::canonical("hidden");
    let db_dir = Scratch::canonical("hidden-db");
    let db = db_dir.join("index.sqlite");
    touch(&root.join("visible.txt"), b"alpha");
    touch(&root.join(".config/app/settings.txt"), b"bravo");
    touch(&root.join(".dotfile"), b"charlie");

    let mut on = base_config(&root, &db);
    on.indexing.include_hidden = true;
    index_once(&db, &on);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 3);

    let mut off = on.clone();
    off.indexing.include_hidden = false;
    let (deleted, _) = reconcile(&db, &on, &off);
    assert_eq!(deleted, 2);
    assert_eq!(names(&db), vec!["visible.txt"]);
    assert_eq!(residue(&db), (1, 1));
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

/// A config changed while the app was not running is reconciled by the next
/// run, from the `config_validation` record of what the index was built with.
#[test]
fn a_run_reconciles_a_config_edited_while_it_was_closed() {
    let base = Scratch::canonical("offline");
    let kept = base.join("kept");
    let dropped = base.join("dropped");
    let db_dir = Scratch::canonical("offline-db");
    let db = db_dir.join("index.sqlite");
    touch(&kept.join("a.txt"), b"alpha");
    touch(&dropped.join("b.txt"), b"bravo");
    touch(&dropped.join("sub/c.txt"), b"charlie");

    let mut old = base_config(&kept, &db);
    old.paths.indexing_paths = vec![
        kept.to_string_lossy().into_owned(),
        dropped.to_string_lossy().into_owned(),
    ];
    index_once(&db, &old);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 3);

    // No reconcile() here: the edit happened with nothing running, so the run
    // itself has to notice.
    let mut new = old.clone();
    new.paths.indexing_paths = vec![kept.to_string_lossy().into_owned()];
    index_once(&db, &new);

    assert_eq!(paths(&db), vec![kept.join("a.txt").to_string_lossy()]);
    assert_eq!(residue(&db), (1, 1));
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

/// The size cap is in the record too: raised while the app was closed, it
/// reaches a file the walk skips as unchanged.
#[test]
fn a_cap_raised_while_closed_reads_the_text_it_now_allows() {
    let root = Scratch::canonical("cap-raised");
    let db_dir = Scratch::canonical("cap-raised-db");
    let db = db_dir.join("index.sqlite");
    touch(&root.join("big.txt"), &b"golf ".repeat(820));

    let mut capped = base_config(&root, &db);
    capped.processing.maximum_text_file_size = 1024;
    index_once(&db, &capped);
    let born = created_at(&db);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM files WHERE content_state = 3"),
        1,
        "4 KiB is over the cap"
    );
    assert_eq!(fts_count(&db, "golf"), 0);

    index_once(&db, &base_config(&root, &db));
    assert_eq!(fts_count(&db, "golf"), 1, "the text is searchable");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

/// A `hash_length` edit no run applied, beside a dropped root: the next run
/// rebuilds rather than stamping the new length over hashes taken under the
/// old one, and the dropped root's rows go with the wipe. The one test here
/// whose index is meant to be replaced, so `created_at` is not pinned (it
/// has whole-second resolution, so it may not even change).
#[test]
fn a_hash_length_edited_while_closed_rebuilds_the_index() {
    let base = Scratch::canonical("rehash");
    let kept = base.join("kept");
    let dropped = base.join("dropped");
    let db_dir = Scratch::canonical("rehash-db");
    let db = db_dir.join("index.sqlite");
    // Past both lengths, so the two hashes of it differ.
    touch(&kept.join("a.txt"), &b"alpha ".repeat(2000));
    touch(&kept.join("b.txt"), b"bravo");
    touch(&dropped.join("c.txt"), b"charlie");

    let mut old = base_config(&kept, &db);
    old.paths.indexing_paths = vec![
        kept.to_string_lossy().into_owned(),
        dropped.to_string_lossy().into_owned(),
    ];
    index_once(&db, &old);
    assert_eq!(paths(&db).len(), 3);

    let mut new = base_config(&kept, &db);
    new.processing.hash_length = old.processing.hash_length / 2;
    index_once(&db, &new);

    assert_eq!(
        paths(&db),
        vec![
            kept.join("a.txt").to_string_lossy(),
            kept.join("b.txt").to_string_lossy()
        ]
    );
    assert_eq!(orphans(&db), 0);
    let c = conn(&db);
    let mut stmt = c.prepare("SELECT parent || name, hash FROM files").unwrap();
    let rows: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let mut head = Vec::new();
    for (path, stored) in rows {
        let (want, _) =
            get_file_hash(Path::new(&path), new.processing.hash_length, &mut head).unwrap();
        assert_eq!(stored, want, "{} is hashed at the new length", path);
    }
}

/// The run-start pass and the coordinator's pass can both fire for one edit;
/// a plan that is not idempotent would delete rows the walk just re-added.
#[test]
fn reconciling_is_idempotent() {
    let root = Scratch::canonical("idempotent");
    let db_dir = Scratch::canonical("idempotent-db");
    let db = db_dir.join("index.sqlite");
    tree(&root);

    let old = base_config(&root, &db);
    index_once(&db, &old);
    let mut new = old.clone();
    new.indexing.ignore_patterns = vec!["*.log".into()];
    new.indexing.content_extensions = vec!["txt".into()];

    let first = reconcile(&db, &old, &new);
    let after_first = paths(&db);
    assert!(first.0 > 0 && first.1 > 0, "the first pass did work");

    let second = reconcile(&db, &old, &new);
    assert_eq!(second, (0, 0), "the second pass found nothing left to do");
    assert_eq!(paths(&db), after_first);
}

// ---------------------------------------------------------------------------
// Offline roots: a root that cannot be resolved is spelled as configured, not
// canonically, and must not read as a removed one.
// ---------------------------------------------------------------------------

/// The roots the index's record names, in recorded spelling.
fn recorded_roots(db: &Path) -> Vec<String> {
    let value: String = conn(db)
        .query_row(
            "SELECT value FROM config_validation WHERE key = 'indexing_path'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    value.lines().map(str::to_string).collect()
}

/// `dir` spelled with a trailing separator, which canonicalizing drops.
fn with_trailing_separator(dir: &Path) -> String {
    format!("{}{}", dir.to_string_lossy(), std::path::MAIN_SEPARATOR)
}

#[cfg(unix)]
#[test]
fn an_offline_root_reached_through_a_symlink_keeps_its_entries() {
    let base = Scratch::canonical("offline-link");
    let target = base.join("disk");
    let link = base.join("docs");
    let db_dir = Scratch::canonical("offline-link-db");
    let db = db_dir.join("index.sqlite");
    tree(&target);
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let config = base_config(&link, &db);
    index_once(&db, &config);
    let born = created_at(&db);
    let before = paths(&db);
    assert_eq!(before.len(), 5);
    assert!(
        before.iter().all(|p| Path::new(p).starts_with(&target)),
        "rows are keyed under the link's target: {:?}",
        before
    );

    let away = base.join("disk-away");
    std::fs::rename(&target, &away).unwrap();
    index_once(&db, &config);
    assert_eq!(
        paths(&db),
        before,
        "a dangling link is an offline root, not a removed one"
    );

    std::fs::rename(&away, &target).unwrap();
    index_once(&db, &config);
    assert_eq!(paths(&db), before);
    assert_eq!(residue(&db), (5, 5), "no text was withdrawn either");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn an_offline_root_spelled_with_a_trailing_slash_keeps_its_entries() {
    let base = Scratch::canonical("offline-slash");
    let root = base.join("disk");
    let db_dir = Scratch::canonical("offline-slash-db");
    let db = db_dir.join("index.sqlite");
    tree(&root);

    let mut config = base_config(&root, &db);
    config.paths.indexing_paths = vec![with_trailing_separator(&root)];
    index_once(&db, &config);
    let born = created_at(&db);
    let before = paths(&db);
    assert_eq!(before.len(), 5);

    let away = base.join("disk-away");
    std::fs::rename(&root, &away).unwrap();
    index_once(&db, &config);
    assert_eq!(paths(&db), before, "an unmounted root is not a removed one");

    std::fs::rename(&away, &root).unwrap();
    index_once(&db, &config);
    assert_eq!(paths(&db), before);
    assert_eq!(residue(&db), (5, 5), "no text was withdrawn either");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

/// A stamp of only the offline spelling would lose the canonical one, and
/// with it the rows' owner and the root's progress figure.
#[test]
fn an_offline_run_keeps_the_recorded_spelling_in_the_stamp() {
    let base = Scratch::canonical("offline-stamp");
    let root = base.join("disk");
    let db_dir = Scratch::canonical("offline-stamp-db");
    let db = db_dir.join("index.sqlite");
    tree(&root);

    let mut config = base_config(&root, &db);
    config.paths.indexing_paths = vec![with_trailing_separator(&root)];
    index_once(&db, &config);
    let recorded = recorded_roots(&db);
    assert_eq!(recorded.len(), 1, "one canonical root: {:?}", recorded);
    let walk_count = |db: &Path| db::repo::get_root_walk_count(&conn(db), &recorded[0]);
    assert_eq!(walk_count(&db), Some(5));

    std::fs::rename(&root, base.join("disk-away")).unwrap();
    index_once(&db, &config);
    assert!(
        recorded_roots(&db).contains(&recorded[0]),
        "the held run dropped {} from the record: {:?}",
        recorded[0],
        recorded_roots(&db)
    );
    assert_eq!(walk_count(&db), Some(5), "the figure outlives the outage");
}

/// The drop waits out every offline run, then goes by the spelling recorded
/// while the root was online: the link no longer leads there.
#[cfg(unix)]
#[test]
fn a_root_removed_while_offline_is_dropped_by_its_recorded_spelling() {
    let base = Scratch::canonical("offline-removed");
    let target = base.join("disk");
    let link = base.join("docs");
    let other = base.join("other");
    let db_dir = Scratch::canonical("offline-removed-db");
    let db = db_dir.join("index.sqlite");
    tree(&target);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    touch(&other.join("o.txt"), b"other");

    let mut both = base_config(&link, &db);
    both.paths.indexing_paths = vec![
        link.to_string_lossy().into_owned(),
        other.to_string_lossy().into_owned(),
    ];
    index_once(&db, &both);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 6);

    std::fs::rename(&target, base.join("disk-away")).unwrap();
    index_once(&db, &both);
    assert_eq!(paths(&db).len(), 6, "held while the link dangles");

    // Removed while `other` is offline too: still held.
    let only_other = base_config(&other, &db);
    let other_away = base.join("other-away");
    std::fs::rename(&other, &other_away).unwrap();
    index_once(&db, &only_other);
    assert_eq!(
        paths(&db).len(),
        6,
        "a drop waits while any root is offline"
    );

    std::fs::rename(&other_away, &other).unwrap();
    index_once(&db, &only_other);
    assert_eq!(
        paths(&db),
        vec![other.join("o.txt").to_string_lossy()],
        "every configured root resolves, so the removed one goes"
    );
    assert_eq!(residue(&db), (1, 1));
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}

#[test]
fn a_removal_waits_while_another_root_is_offline() {
    let base = Scratch::canonical("offline-wait");
    let offline = base.join("offline");
    let removed = base.join("removed");
    let db_dir = Scratch::canonical("offline-wait-db");
    let db = db_dir.join("index.sqlite");
    touch(&offline.join("a.txt"), b"alpha");
    touch(&removed.join("b.txt"), b"bravo");
    touch(&removed.join("sub/c.txt"), b"charlie");

    let mut both = base_config(&offline, &db);
    both.paths.indexing_paths = vec![
        offline.to_string_lossy().into_owned(),
        removed.to_string_lossy().into_owned(),
    ];
    index_once(&db, &both);
    let born = created_at(&db);
    let before = paths(&db);
    assert_eq!(before.len(), 3);

    let away = base.join("offline-away");
    std::fs::rename(&offline, &away).unwrap();
    let narrowed = base_config(&offline, &db);
    index_once(&db, &narrowed);
    assert_eq!(
        paths(&db),
        before,
        "the removed root's rows wait for the offline one"
    );

    std::fs::rename(&away, &offline).unwrap();
    index_once(&db, &narrowed);
    assert_eq!(paths(&db), vec![offline.join("a.txt").to_string_lossy()]);
    assert_eq!(residue(&db), (1, 1));
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");
}
