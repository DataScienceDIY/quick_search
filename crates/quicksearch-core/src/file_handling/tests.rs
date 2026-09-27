use std::path::Path;

use crate::config::IgnoreSet;
use crate::testutil::touch;

use super::records::get_file_hash;
use super::*;
use std::path::MAIN_SEPARATOR;

fn tmp_tree() -> std::path::PathBuf {
    crate::testutil::scratch_dir("walk")
}

/// The entries' file names, sorted.
fn names(entries: impl Iterator<Item = walkdir::DirEntry>) -> Vec<String> {
    let mut names: Vec<String> = entries
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The sorted names of the files [`filtered_walk`] yields under `root`.
fn walked_names(root: &Path, follow: bool, hidden: bool, ignore: &IgnoreSet) -> Vec<String> {
    let failures = UnreadableDirs::default();
    names(filtered_walk(
        root.to_str().unwrap(),
        follow,
        hidden,
        ignore,
        &failures,
    ))
}

/// The sorted names of the directories [`filtered_dirs`] yields under `root`.
fn walked_dir_names(root: &Path, follow: bool, hidden: bool, ignore: &IgnoreSet) -> Vec<String> {
    let failures = UnreadableDirs::default();
    names(filtered_dirs(
        root.to_str().unwrap(),
        follow,
        hidden,
        ignore,
        &failures,
    ))
}

/// The stored `deletemerge` threshold, if FTS5 has one recorded.
fn deletemerge(conn: &rusqlite::Connection) -> Option<i64> {
    conn.query_row(
        "SELECT v FROM searchabletext_config WHERE k = 'deletemerge'",
        [],
        |r| r.get(0),
    )
    .ok()
}

#[cfg(unix)]
fn into_conn(m: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> rusqlite::Connection {
    std::sync::Arc::try_unwrap(m)
        .map_err(|_| ())
        .expect("sole owner")
        .into_inner()
        .unwrap()
}

#[test]
fn filtered_walk_prunes_hidden_and_ignored() {
    let root = tmp_tree();
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("sub/keep2.txt"), b"x");
    touch(&root.join("sub/skip.tmp"), b"x");
    touch(&root.join(".hidden/inside.txt"), b"x");
    touch(&root.join(".dotfile"), b"x");
    touch(&root.join("node_modules/dep/index.js"), b"x");
    touch(&root.join("secret/deep/file.txt"), b"x");

    let ignore = IgnoreSet::compile(&[
        "*.tmp".to_string(),
        "node_modules".to_string(),
        format!("{}/secret", root.display()),
    ])
    .unwrap();

    assert_eq!(
        walked_names(&root, false, false, &ignore),
        vec!["keep.txt", "keep2.txt"]
    );
    assert_eq!(
        walked_names(&root, false, true, &ignore),
        vec![".dotfile", "inside.txt", "keep.txt", "keep2.txt"]
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The watcher spends one inotify descriptor per directory this yields, so
/// it must prune exactly like [`filtered_walk`].
#[test]
fn filtered_dirs_yields_only_kept_directories() {
    let root = tmp_tree();
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("sub/nested/keep2.txt"), b"x");
    touch(&root.join(".hidden/inside.txt"), b"x");
    touch(&root.join("node_modules/dep/index.js"), b"x");

    let ignore = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();

    // The root itself is included (depth 0 is always kept).
    let root_name = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut want = vec![root_name, "nested".to_string(), "sub".to_string()];
    want.sort();
    assert_eq!(walked_dir_names(&root, false, false, &ignore), want);

    let names = walked_dir_names(&root, false, true, &ignore);
    assert!(names.contains(&".hidden".to_string()));
    assert!(
        !names.contains(&"node_modules".to_string()),
        "ignores still apply with include_hidden"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Every entry lands in exactly one of the two iterators.
#[test]
fn filtered_dirs_and_filtered_walk_do_not_overlap() {
    let root = tmp_tree();
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");
    let ignore = IgnoreSet::compile(&[]).unwrap();

    let files: Vec<_> = filtered_walk(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.path().to_path_buf())
    .collect();
    let dirs: Vec<_> = filtered_dirs(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.path().to_path_buf())
    .collect();

    assert_eq!(files.len(), 2);
    assert_eq!(dirs.len(), 2, "root + sub");
    assert!(
        files.iter().all(|f| !dirs.contains(f)),
        "a path must not be both a file and a directory"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn filtered_walk_hidden_root_still_walked() {
    // A hidden root dir must not silence the whole walk.
    let base = tmp_tree();
    let root = base.join(".config");
    touch(&root.join("app.conf"), b"x");
    let ignore = IgnoreSet::compile(&[]).unwrap();
    assert_eq!(walked_names(&root, false, false, &ignore), vec!["app.conf"]);
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn classify_uses_mtime_against_the_stored_row() {
    // No row: new. Same mtime: nothing to do. Changed: re-read.
    assert_eq!(classify_by_mtime(None, 100), FileIndexAction::Insert);
    assert_eq!(classify_by_mtime(Some(100), 100), FileIndexAction::Skip);
    assert_eq!(classify_by_mtime(Some(99), 100), FileIndexAction::Update);
}

#[test]
fn db_path_strips_windows_prefixes() {
    assert_eq!(
        path_to_db_string(Path::new("/plain/unix/path")),
        "/plain/unix/path"
    );
    assert_eq!(
        path_to_db_string(Path::new(r"\\?\C:\docs\a.txt")),
        r"C:\docs\a.txt"
    );
    // A share must come back as \\server\share, not UNC\server\share.
    assert_eq!(
        path_to_db_string(Path::new(r"\\?\UNC\server\share\a.txt")),
        r"\\server\share\a.txt"
    );
    // The `\\?\` prefix is load-bearing for a Volume{GUID} mount.
    assert_eq!(
        path_to_db_string(Path::new(r"\\?\Volume{9f8a}\data\a.txt")),
        r"\\?\Volume{9f8a}\data\a.txt"
    );
}

/// The range must bracket paths spelled with the *platform's* separator: a
/// hard-coded `'/' + 1` `hi` sorts below every `C:\…` path.
#[test]
fn root_range_brackets_paths_under_its_root() {
    use std::path::MAIN_SEPARATOR as SEP;

    let root = format!("{}Users{}me", root_prefix(), SEP);
    let c = RootRange::for_root(&root);
    let inside = format!("{}{}docs{}a.txt", root, SEP, SEP);
    assert!(
        inside.as_str() >= c.lo.as_str() && inside.as_str() < c.hi.as_str(),
        "{:?} must fall inside [{:?}, {:?})",
        inside,
        c.lo,
        c.hi
    );

    // The directory itself is *not* in the range; a prefix sibling is
    // outside it.
    assert!(root.as_str() < c.lo.as_str());
    let sibling = format!("{}Users{}mexico{}a.txt", root_prefix(), SEP, SEP);
    assert!(
        !(sibling.as_str() >= c.lo.as_str() && sibling.as_str() < c.hi.as_str()),
        "{:?} is a sibling of {:?}, not a child",
        sibling,
        root
    );

    for spelled in [format!("{}/", root), format!("{}\\", root)] {
        let c2 = RootRange::for_root(&spelled);
        assert_eq!((&c2.lo, &c2.hi), (&c.lo, &c.hi), "spelled {:?}", spelled);
    }
}

fn root_prefix() -> String {
    if cfg!(windows) {
        r"C:\".to_string()
    } else {
        "/".to_string()
    }
}

/// A Remove event names a path that is already gone.
#[test]
fn db_key_for_a_vanished_path_canonicalizes_what_remains() {
    let root = tmp_tree();
    let real = root.join("sub");
    std::fs::create_dir_all(&real).unwrap();

    let missing = real.join("gone").join("deeper.txt");
    let key = db_key_for_missing_path(&missing);

    let expected = path_to_db_string(&real.canonicalize().unwrap().join("gone").join("deeper.txt"));
    assert_eq!(key, expected, "existing prefix resolved, missing tail kept");

    // A redundant component in the *existing* part is collapsed.
    let odd = root.join("sub").join(".").join("gone.txt");
    let odd_key = db_key_for_missing_path(&odd);
    assert!(
        !odd_key.contains(&format!("{}.{}", MAIN_SEPARATOR, MAIN_SEPARATOR)),
        "unexpected `.` component in {}",
        odd_key
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The hazard every screen exists for, stated once here: the stored key for
/// an unrepresentable path is another file's key, and any use of it (open,
/// hash, delete) lands on that file instead.
#[test]
fn the_stored_spelling_of_an_unrepresentable_path_is_another_files_key() {
    let dir = Path::new("/docs");
    let bad = dir.join(crate::testutil::unrepresentable_name("report", ".txt"));
    let twin = dir.join(crate::testutil::lossy_twin("report", ".txt"));

    assert!(warn_if_unrepresentable(&bad));
    assert!(
        !warn_if_unrepresentable(&twin),
        "the twin is an ordinary name and must pass"
    );
    assert_eq!(
        path_to_db_string(&bad),
        path_to_db_string(&twin),
        "two different files, one stored key"
    );

    // The collision is inherited by everything beneath a bad *directory*.
    assert_eq!(
        path_to_db_string(&bad.join("child.txt")),
        path_to_db_string(&twin.join("child.txt"))
    );
    assert!(warn_if_unrepresentable(&bad.join("child.txt")));
}

#[test]
fn db_key_for_an_entirely_missing_path_falls_back_to_the_raw_spelling() {
    let nowhere = Path::new("relative-thing-that-does-not-exist.txt");
    assert_eq!(db_key_for_missing_path(nowhere), path_to_db_string(nowhere));
}

#[test]
fn unreadable_dirs_match_by_component_not_string_prefix() {
    let u = UnreadableDirs::default();
    assert!(!u.covers("/a/b/c.txt"), "an empty set covers nothing");

    u.record(std::path::PathBuf::from("/a/b"));
    assert!(u.covers("/a/b/c.txt"));
    assert!(u.covers("/a/b"));
    // /a/bc is a sibling of /a/b, and its rows must stay deletable.
    assert!(!u.covers("/a/bc/d.txt"));
    assert!(!u.covers("/a/other.txt"));
}

/// "Could not look" must stay distinguishable from "the files are gone": the
/// latter deletes index rows.
#[test]
#[cfg(unix)]
fn unreadable_directory_is_reported_rather_than_yielded_as_empty() {
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_tree();
    touch(&root.join("readable/a.txt"), b"x");
    let locked = root.join("locked");
    touch(&locked.join("hidden-from-us.txt"), b"x");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let failures = UnreadableDirs::default();
    let names = names(filtered_walk(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &failures,
    ));

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();

    assert_eq!(
        names,
        vec!["a.txt"],
        "the unreadable subtree yields nothing"
    );
    assert!(!failures.is_empty(), "and that failure must be recorded");
    assert!(
        failures.covers(locked.join("hidden-from-us.txt").to_str().unwrap()),
        "rows beneath it are protected from stale cleanup"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn hash_covers_size_and_head_only() {
    let root = tmp_tree();
    let a = root.join("a.bin");
    let b = root.join("b.bin");
    let c = root.join("c.bin");

    // Same size, same head, differing only in the tail: the documented
    // collision.
    std::fs::write(&a, [b"HEAD".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();
    std::fs::write(&b, [b"HEAD".as_slice(), &[0u8; 64], b"BBBB"].concat()).unwrap();
    std::fs::write(&c, [b"DIFF".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();

    let h = |p: &Path| {
        let mut head = Vec::new();
        get_file_hash(std::fs::metadata(p).unwrap().len(), p, 8, &mut head).unwrap()
    };
    assert_eq!(h(&a), h(&b), "tail differences are invisible by design");
    assert_ne!(h(&a), h(&c), "head differences are caught");

    // Size participates, so a prefix does not collide with its extension.
    let short = root.join("short.bin");
    std::fs::write(&short, b"HEAD").unwrap();
    assert_ne!(h(&a), h(&short));

    let mut head = Vec::new();
    get_file_hash(72, &a, 8, &mut head).unwrap();
    assert_eq!(head, b"HEAD\0\0\0\0", "exactly hash_length bytes");
    get_file_hash(4, &short, 8, &mut head).unwrap();
    assert_eq!(head, b"HEAD", "a short file hashes whole, and the buffer is reused");

    std::fs::remove_dir_all(&root).ok();
}

/// The invariant the whole schema rests on: `parent` and `name` concatenate
/// back into the path, with no separator logic at the join.
#[test]
fn a_split_path_concatenates_back_into_itself() {
    let root = std::path::Path::new(if cfg!(windows) { r"C:\" } else { "/" });
    let cases = [
        root.join("a.txt"),                        // a file at the very root
        root.join("home").join("me").join("x.md"), // the ordinary case
        root.join("dir with spaces").join("y"),
        root.join("weird.name").join("z.tar.gz"),
    ];
    for path in cases {
        let s = path_to_db_string(&path);
        let (parent, name) = split_db_path(&s).expect("a file's path");
        assert_eq!(
            format!("{}{}", parent, name),
            s,
            "{:?} must round-trip through its (parent, name) key",
            s
        );
        assert_eq!(
            name,
            path.file_name().unwrap().to_string_lossy(),
            "the name half is the file name"
        );
        assert_eq!(
            parent,
            dir_to_db_parent(path.parent().unwrap()),
            "the parent half is what `dir_to_db_parent` would store"
        );
        assert!(
            parent.ends_with(MAIN_SEPARATOR),
            "a stored parent always ends in a separator: {:?}",
            parent
        );
        assert!(
            !name.contains(MAIN_SEPARATOR),
            "a stored name never contains one: {:?}",
            name
        );
    }
}

/// A filesystem root already ends in a separator; a doubled one would store
/// every file directly at the root under `//` and never find it again.
#[test]
fn a_root_directory_does_not_get_a_doubled_separator() {
    let root = std::path::Path::new(if cfg!(windows) { r"C:\" } else { "/" });
    let parent = dir_to_db_parent(root);
    assert_eq!(parent, path_to_db_string(root));
    assert!(!parent.ends_with(&format!("{}{}", MAIN_SEPARATOR, MAIN_SEPARATOR)));
    assert_eq!(
        format!("{}{}", parent, "a.txt"),
        path_to_db_string(&root.join("a.txt"))
    );
}

/// Strings that are not a file's path have no key, and must say so.
#[test]
fn a_string_that_cannot_be_a_files_path_has_no_key() {
    assert_eq!(split_db_path("bare-name.txt"), None, "no separator at all");
    assert_eq!(
        split_db_path(&format!("{}dir{}", MAIN_SEPARATOR, MAIN_SEPARATOR)),
        None,
        "trailing separator: names a directory, not a file"
    );
    assert_eq!(split_db_path(""), None);
}

/// On Unix a backslash is an ordinary filename character; splitting on it
/// would leave the row unreachable by the path it was stored under.
#[test]
#[cfg(unix)]
fn a_backslash_is_just_a_character_on_unix() {
    let (parent, name) = split_db_path(r"/home/me/back\slash.txt").expect("a file's path");
    assert_eq!(parent, "/home/me/");
    assert_eq!(name, r"back\slash.txt");

    // A folder genuinely named `weird\` still gets its separator appended.
    assert_eq!(
        dir_to_db_parent(std::path::Path::new(r"/tmp/weird\")),
        r"/tmp/weird\/"
    );
}

/// walkdir defaults `follow_root_links` to true, independently of
/// `follow_links`, which would descend a symlinked root even with following
/// off and land rows where no sweep range reaches.
#[test]
#[cfg(unix)]
fn a_symlinked_root_is_not_descended_when_following_is_off() {
    let base = tmp_tree();
    let real = base.join("real");
    touch(&real.join("inside.txt"), b"x");
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let names = walked_names(&link, false, false, &ignore);
    assert!(
        names.is_empty(),
        "a symlinked root must not be descended: {names:?}"
    );
    assert_eq!(
        walked_names(&link, true, false, &ignore),
        vec!["inside.txt"]
    );

    std::fs::remove_dir_all(&base).ok();
}

/// A symlink *inside* a root is the same hazard one level down.
#[test]
#[cfg(unix)]
fn a_symlink_inside_a_root_is_skipped_when_following_is_off() {
    let base = tmp_tree();
    let root = base.join("root");
    touch(&root.join("real.txt"), b"x");
    let outside = base.join("outside");
    touch(&outside.join("target.txt"), b"x");
    std::os::unix::fs::symlink(outside.join("target.txt"), root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    assert_eq!(
        walked_names(&root, false, false, &ignore),
        vec!["real.txt"],
        "only the real file may be walked"
    );
    assert_eq!(
        walked_names(&root, true, false, &ignore),
        vec!["link.txt", "real.txt", "target.txt"]
    );

    std::fs::remove_dir_all(&base).ok();
}

/// The watcher's directory walk must not spend descriptors on a subtree
/// the indexer will discard.
#[test]
#[cfg(unix)]
fn a_symlinked_root_yields_no_directories_when_following_is_off() {
    let base = tmp_tree();
    let real = base.join("real");
    touch(&real.join("sub/inside.txt"), b"x");
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let dirs = walked_dir_names(&link, false, false, &ignore);
    assert!(
        dirs.is_empty(),
        "a symlinked root must yield no directories: {dirs:?}"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// A reconcile killed mid-burst leaves `deletemerge` at 0, where no later
/// `'merge'` would reclaim a tombstone again, and the next indexing run
/// repairs it. This is why [`fts_begin_bulk_write`] writes a value it never
/// lowers: the pairing in `scope::advance` covers the orderly cases, and this
/// covers the process not coming back.
#[test]
fn a_bulk_write_repairs_a_delete_merge_threshold_left_off() {
    let dir = crate::testutil::scratch_dir("deletemerge-repair");
    let db = dir.join("index.sqlite");
    let conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();

    // What a killed burst leaves behind.
    fts_begin_tombstone_burst(&conn);
    assert_eq!(deletemerge(&conn), Some(0), "the burst is in effect");

    fts_begin_bulk_write(&conn);
    assert_eq!(
        deletemerge(&conn),
        Some(i64::from(FTS_DELETEMERGE)),
        "the next run puts tombstone reclamation back"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Seed one row per content state under `/t/`, plus a survivor. Returns
/// `name -> id`. The two `done-*` rows hold postings and the others do not,
/// the distinction stale cleanup's `with_postings` narrowing draws.
#[cfg(unix)]
fn seed_stale_corpus(
    conn: &mut rusqlite::Connection,
) -> std::collections::HashMap<&'static str, i64> {
    use crate::db::repo::{self, NewFile};
    let tx = conn.transaction().unwrap();
    let mut ids = std::collections::HashMap::new();
    for (name, needs_content) in [
        ("done-stale.txt", true),
        ("done-kept.txt", true),
        ("na-stale.bin", false),
        ("pending-stale.txt", true),
        ("failed-stale.txt", true),
    ] {
        let id = repo::insert_file(
            &tx,
            &NewFile {
                name,
                parent: "/t/",
                size: 1,
                mtime: 1,
                mime: Some("text/plain"),
                ftype: crate::mime::FileType::TEXT,
                hash: None,
                needs_content,
            },
        )
        .unwrap()
        .expect("unique path");
        ids.insert(name, id);
    }
    repo::store_pending_text(&tx, ids["done-stale.txt"], "body one", None).unwrap();
    repo::store_pending_text(&tx, ids["done-kept.txt"], "body two", None).unwrap();
    let failed = crate::file_handling::ContentOutcome::Failed("bad parse".into());
    repo::complete_pending(&tx, ids["failed-stale.txt"], &failed, None).unwrap();
    tx.commit().unwrap();
    ids
}

/// The stale pass over rows in every content state: exactly the stale rows
/// go, a posting survives exactly for the surviving DONE row, and the
/// delete-merge threshold the pass held off is back afterwards. An orphan
/// posting would surface as a hit for a file no longer indexed, with no error
/// anywhere.
#[test]
#[cfg(unix)]
fn stale_cleanup_deletes_exactly_the_stale_rows_and_their_postings() {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    let dir = crate::testutil::scratch_dir("stale-cleanup");
    let db = dir.join("index.sqlite");
    let mut conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
    let ids = seed_stale_corpus(&mut conn);

    // `vanished.txt` was never indexed: a stale list can legitimately name a
    // path another writer already removed, and it must not fail the pass.
    let stale: Vec<String> = [
        "done-stale.txt",
        "na-stale.bin",
        "pending-stale.txt",
        "failed-stale.txt",
        "vanished.txt",
    ]
    .iter()
    .map(|n| format!("/t/{}", n))
    .collect();

    let conn_mutex = Arc::new(Mutex::new(conn));
    let stop = Arc::new(AtomicBool::new(false));
    let deleted =
        cleanup_stale_index_entries(&conn_mutex, &stale, &stop, &crate::config::Config::default())
            .unwrap();
    assert_eq!(deleted, 4);

    let conn = into_conn(conn_mutex);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM searchabletext"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM documents_text"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM failed_files"), 0);
    assert_eq!(
        crate::testutil::fts_rowids(&conn, "body"),
        vec![ids["done-kept.txt"]],
        "the survivor is still findable"
    );
    assert_eq!(
        deletemerge(&conn),
        Some(i64::from(FTS_DELETEMERGE)),
        "the pass restored the threshold it held off"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A stop mid-pass returns without restoring `deletemerge`: the abandonment
/// `a_bulk_write_repairs_a_delete_merge_threshold_left_off` exists for. If
/// this starts restoring it eagerly, that test loses its subject; if it
/// starts deleting under a stop, a stopped run reconciles.
#[test]
#[cfg(unix)]
fn a_stopped_stale_cleanup_deletes_nothing_and_leaves_the_burst() {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    let dir = crate::testutil::scratch_dir("stale-cleanup-stop");
    let db = dir.join("index.sqlite");
    let mut conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
    seed_stale_corpus(&mut conn);

    let conn_mutex = Arc::new(Mutex::new(conn));
    let stop = Arc::new(AtomicBool::new(true));
    let deleted = cleanup_stale_index_entries(
        &conn_mutex,
        &["/t/done-stale.txt".to_string()],
        &stop,
        &crate::config::Config::default(),
    )
    .unwrap();
    assert_eq!(deleted, 0);

    let conn = into_conn(conn_mutex);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 5, "a stop deletes nothing");
    assert_eq!(
        deletemerge(&conn),
        Some(0),
        "the burst is left for the next run's bulk write to repair"
    );

    std::fs::remove_dir_all(&dir).ok();
}
