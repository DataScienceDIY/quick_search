//! End-to-end tests for reconciling a real index against a changed
//! configuration.
//!
//! The thing every test here really asserts is that the index file *survived*.
//! Losing it is silent — the next run rebuilds and everything looks fine, only
//! hours later and with every extracted document read again — so each test
//! pins `schema_info.created_at`, which only a wipe can change. Without that
//! assertion a regression that quietly reintroduces the rebuild would pass
//! every other check in this file.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use quicksearch_core::config::{diff_actions, Config};
use quicksearch_core::db;
use quicksearch_core::extract::Registry;
use quicksearch_core::scope::{advance, WorkCursor, SLICE};

mod common;
use common::{scratch_dir_canonical as tmp_dir, touch};

/// Run one full index over `config`'s roots and wait for it to finish.
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

fn paths(db: &Path) -> Vec<String> {
    let c = conn(db);
    let mut stmt = c.prepare("SELECT path FROM files ORDER BY path").unwrap();
    let out = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out
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

/// Rows in a dependent table whose file is gone.
///
/// `searchabletext` is the one that matters and the one this exists for: it
/// is an FTS5 virtual table with no foreign key, so a delete that forgets it
/// leaves postings behind — and a contentless table happily keeps serving a
/// rowid nothing can resolve, which surfaces as a search hit for a file that
/// is no longer indexed. The other three cascade, and are checked so that a
/// future connection opened without `PRAGMA foreign_keys` cannot make this
/// quietly untrue.
fn orphans(db: &Path) -> i64 {
    [
        ("searchabletext", "rowid"),
        ("documents_text", "file_id"),
        ("properties", "file_id"),
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
    // Start with nothing excluded, so each test narrows from a full index.
    config.indexing.ignore_patterns = vec![];
    config
}

/// Adding an ignore pattern must remove exactly the entries it matches —
/// their name row, their FTS postings and their extracted text — and leave
/// the index file itself alone.
#[test]
fn adding_an_ignore_pattern_prunes_instead_of_rebuilding() {
    let root = tmp_dir("ignore-add");
    let db_dir = tmp_dir("ignore-add-db");
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
    assert_eq!(
        paths(&db)
            .iter()
            .map(|p| Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned())
            .collect::<Vec<_>>(),
        vec!["keep2.txt", "keep.txt", "notes.md"]
    );
    assert_eq!(
        residue(&db),
        (3, 3),
        "the FTS postings and the extracted text went with the rows"
    );
    assert_eq!(orphans(&db), 0);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'echo'"
        ),
        0,
        "the ignored file's content is no longer findable"
    );
    assert_eq!(created_at(&db), born, "the index was not rebuilt");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Removing a pattern only ever *adds* files, so it must delete nothing and
/// ask for a walk. Running that walk brings the entries back without the
/// index having been thrown away in between.
#[test]
fn removing_an_ignore_pattern_reindexes_and_deletes_nothing() {
    let root = tmp_dir("ignore-remove");
    let db_dir = tmp_dir("ignore-remove-db");
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

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Removing a folder takes its entries and only its entries. This used to be
/// a full wipe, so a user with two roots paid for both to be walked again to
/// stop indexing one of them.
#[test]
fn removing_a_root_takes_only_its_own_entries() {
    let base = tmp_dir("roots");
    let kept = base.join("kept");
    let dropped = base.join("dropped");
    let db_dir = tmp_dir("roots-db");
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

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Adding a folder is pure widening: nothing stored is wrong, there is just
/// more to find. The existing root's rows must not even be re-examined.
#[test]
fn adding_a_root_keeps_everything_already_indexed() {
    let base = tmp_dir("root-add");
    let first = base.join("first");
    let second = base.join("second");
    let db_dir = tmp_dir("root-add-db");
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

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Narrowing the content filter costs the excluded files their text, not
/// their existence: they must stay findable by name. Widening it queues them
/// for extraction again.
#[test]
fn narrowing_content_extensions_keeps_the_file_findable_by_name() {
    let root = tmp_dir("content");
    let db_dir = tmp_dir("content-db");
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
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'markdown'"
        ),
        0
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM files WHERE content_state = 3"),
        1,
        "the excluded file is parked, not pending"
    );

    // Widening again queues it for another extraction, which the next run
    // performs.
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

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// `store_text_for_snippets` off throws the blobs away and keeps full-text
/// search working; on again re-extracts, because the text of files already
/// indexed was never kept.
#[test]
fn store_text_toggles_without_losing_full_text_search() {
    let root = tmp_dir("store-text");
    let db_dir = tmp_dir("store-text-db");
    let db = db_dir.join("index.sqlite");
    touch(&root.join("a.txt"), b"alpha unique-token");

    let on = base_config(&root, &db);
    index_once(&db, &on);
    let born = created_at(&db);
    assert_eq!(residue(&db).1, 1, "text stored");

    let mut off = on.clone();
    off.processing.store_text_for_snippets = false;
    reconcile(&db, &on, &off);
    assert_eq!(residue(&db).1, 0, "blobs dropped");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM searchabletext WHERE searchabletext MATCH 'unique'"
        ),
        1,
        "full-text search is unaffected"
    );

    reconcile(&db, &off, &on);
    index_once(&db, &on);
    assert_eq!(residue(&db).1, 1, "text re-extracted");
    assert_eq!(created_at(&db), born, "the index was never rebuilt");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Turning hidden files off must reach entries several levels inside a hidden
/// directory, not just the dot-name itself.
#[test]
fn turning_hidden_files_off_prunes_whole_hidden_subtrees() {
    let root = tmp_dir("hidden");
    let db_dir = tmp_dir("hidden-db");
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
    assert_eq!(
        paths(&db)
            .iter()
            .map(|p| Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned())
            .collect::<Vec<_>>(),
        vec!["visible.txt"]
    );
    assert_eq!(residue(&db), (1, 1));
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// A followed symlink's target is stored under its own canonical path, which
/// can be outside every configured root. Such a row has no owning root and
/// therefore no filtering rules that could be applied to it — a prune that
/// tested it anyway would delete it on every config change and the next run
/// would put it straight back, forever.
///
/// The patterns here are the hostile half: `**` and `../*` describe the whole
/// filesystem, and one names the neighbour tree outright. None of them may
/// reach a row the scan never visits.
#[cfg(unix)]
#[test]
fn a_symlink_target_outside_every_root_survives_a_prune() {
    let base = tmp_dir("alias");
    let indexed = base.join("indexed");
    let neighbour = base.join("neighbour");
    let db_dir = tmp_dir("alias-db");
    let db = db_dir.join("index.sqlite");
    touch(&indexed.join("a.txt"), b"alpha");
    touch(&indexed.join("sub/b.log"), b"bravo");
    touch(&neighbour.join("target.txt"), b"charlie outside");
    std::os::unix::fs::symlink(neighbour.join("target.txt"), indexed.join("link.txt")).unwrap();

    let mut old = base_config(&indexed, &db);
    old.indexing.follow_symlinks = true;
    index_once(&db, &old);
    let born = created_at(&db);
    let target = neighbour.join("target.txt").to_string_lossy().into_owned();
    assert!(
        paths(&db).contains(&target),
        "the alias was indexed under the target's own path: {:?}",
        paths(&db)
    );

    let mut new = old.clone();
    new.indexing.ignore_patterns = vec![
        "*.log".into(),
        "**".into(),
        "../*".into(),
        neighbour.join("*").to_string_lossy().into_owned(),
    ];
    reconcile(&db, &old, &new);

    // `**` matches every component, so everything under the configured root
    // goes — and only that. The target, which lives outside it, stays.
    assert_eq!(paths(&db), vec![target]);
    assert_eq!(orphans(&db), 0, "nothing left behind");
    assert_eq!(created_at(&db), born, "the index was not rebuilt");

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Turning symlink following off leaves rows for targets that live outside
/// every root, which no walk and no stale sweep ever reaches — the reason
/// this setting used to force a wipe.
#[cfg(unix)]
#[test]
fn turning_symlinks_off_reaches_targets_outside_the_roots() {
    let base = tmp_dir("links-off");
    let indexed = base.join("indexed");
    let neighbour = base.join("neighbour");
    let db_dir = tmp_dir("links-off-db");
    let db = db_dir.join("index.sqlite");
    touch(&indexed.join("a.txt"), b"alpha");
    touch(&neighbour.join("target.txt"), b"bravo outside");
    std::os::unix::fs::symlink(neighbour.join("target.txt"), indexed.join("link.txt")).unwrap();

    let mut on = base_config(&indexed, &db);
    on.indexing.follow_symlinks = true;
    index_once(&db, &on);
    let born = created_at(&db);
    assert_eq!(paths(&db).len(), 2);

    let mut off = on.clone();
    off.indexing.follow_symlinks = false;
    let (deleted, _) = reconcile(&db, &on, &off);

    assert_eq!(deleted, 1);
    assert_eq!(paths(&db), vec![indexed.join("a.txt").to_string_lossy()]);
    assert_eq!(orphans(&db), 0);
    assert_eq!(created_at(&db), born, "the index was not rebuilt");

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// A config changed while the app was not running is reconciled by the next
/// run, from the `config_validation` record of what the index was built with
/// — the only thing that knows a root was dropped.
#[test]
fn a_run_reconciles_a_config_edited_while_it_was_closed() {
    let base = tmp_dir("offline");
    let kept = base.join("kept");
    let dropped = base.join("dropped");
    let db_dir = tmp_dir("offline-db");
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

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}

/// Reconciling twice must be a no-op the second time. The run-start pass and
/// the coordinator's pass can both fire for one edit, and a plan that is not
/// idempotent would delete rows the walk had just re-added.
#[test]
fn reconciling_is_idempotent() {
    let root = tmp_dir("idempotent");
    let db_dir = tmp_dir("idempotent-db");
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

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&db_dir).ok();
}
