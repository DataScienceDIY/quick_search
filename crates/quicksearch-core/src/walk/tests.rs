use super::*;

use std::time::UNIX_EPOCH;

use crate::testutil::{touch, Scratch};

fn tmp_tree(tag: &str) -> Scratch {
    Scratch::dir(tag)
}

struct TestDb {
    _dir: Scratch,
    path: PathBuf,
}

impl std::ops::Deref for TestDb {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

/// An index holding a row per `(path, mtime)`, with the file's real size (0
/// for a path with no file) and a hash, as a run that read it would store.
fn db_with(tag: &str, rows: &[(String, u64)]) -> TestDb {
    let (dir, p) = Scratch::db(tag);
    let conn = crate::db::open_or_recreate(p.to_str().unwrap(), &Default::default()).unwrap();
    for (path, mtime) in rows {
        // Split the same way the indexer does, so the seeded parent carries
        // its trailing separator and the prefetcher's `parent = ?` finds it.
        let (parent, name) = crate::file_handling::split_db_path(path).expect("a file's path");
        let size = fs::metadata(path).map_or(0, |m| m.len());
        conn.execute(
            "INSERT INTO files (name, parent, size, mtime, type, content_state, hash)
             VALUES (?1, ?2, ?3, ?4, 0, 3, zeroblob(32))",
            rusqlite::params![name, parent, size as i64, *mtime as i64],
        )
        .unwrap();
    }
    TestDb { _dir: dir, path: p }
}

/// The one file under `root` a walk against `db` reports.
fn walk_one(root: &Path, db: &Path) -> WalkedFile {
    let mut files = walk(root, db);
    assert_eq!(files.len(), 1, "expected exactly one file");
    files.pop().unwrap()
}

fn empty_db(tag: &str) -> TestDb {
    db_with(tag, &[])
}

/// A four-worker walk of `roots` against the index at `db`; `stopped` starts
/// it with the stop flag already set.
fn start_with(
    roots: &[&Path],
    db: &Path,
    include_hidden: bool,
    ignore: &[&str],
    stopped: bool,
) -> ParallelWalk {
    let roots: Vec<String> = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    let ignore: Vec<String> = ignore.iter().map(|p| p.to_string()).collect();
    walk_indexable_files(
        &roots,
        include_hidden,
        IgnoreSet::compile(&ignore).unwrap(),
        db.to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(stopped)),
        4,
    )
}

/// A walk of `root` with the default settings.
fn start(root: &Path, db: &Path) -> ParallelWalk {
    start_with(&[root], db, false, &[], false)
}

fn walk(root: &Path, db: &Path) -> Vec<WalkedFile> {
    walk_with(root, db, false)
}

fn walk_with(root: &Path, db: &Path, include_hidden: bool) -> Vec<WalkedFile> {
    files_only(start_with(&[root], db, include_hidden, &[], false))
}

fn files_only(events: impl Iterator<Item = WalkEvent>) -> Vec<WalkedFile> {
    events
        .filter_map(|e| match e {
            WalkEvent::File(f) => Some(f),
            WalkEvent::Stale(_) => None,
        })
        .collect()
}

fn stale_only(events: impl Iterator<Item = WalkEvent>) -> Vec<String> {
    events
        .filter_map(|e| match e {
            WalkEvent::Stale(paths) => Some(paths),
            WalkEvent::File(_) => None,
        })
        .flatten()
        .collect()
}

fn names(files: &[WalkedFile]) -> Vec<String> {
    let mut n: Vec<String> = files
        .iter()
        .map(|f| {
            Path::new(&f.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    n.sort();
    n
}

#[test]
fn walks_a_nested_tree_exactly_once() {
    let root = tmp_tree("nested");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");
    touch(&root.join("sub/deep/c.txt"), b"x");
    touch(&root.join("other/d.txt"), b"x");

    let files = walk(&root, &empty_db("nested"));
    assert_eq!(names(&files), vec!["a.txt", "b.txt", "c.txt", "d.txt"]);

    let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    assert_eq!(unique.len(), files.len(), "no path may be yielded twice");
}

/// FAT, exFAT and some network redirectors reject the names
/// [`crate::testutil::unrepresentable_name`] builds; callers skip when so.
fn try_touch(path: &Path) -> bool {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"x").is_ok() && path.symlink_metadata().is_ok()
}

fn try_mkdir(path: &Path) -> bool {
    fs::create_dir_all(path).is_ok() && path.symlink_metadata().is_ok()
}

/// Windows hits this too: `OsString` there is WTF-8 over UTF-16, and
/// `OsString::from_wide(&[0xD800])` builds an unpaired surrogate that NTFS
/// stores happily; WSL and Samba trees produce them.
#[test]
fn a_non_utf8_name_is_dropped_at_the_entry() {
    let root = tmp_tree("nonutf8");
    touch(&root.join("plain.txt"), b"x");
    let bad = root.join(crate::testutil::unrepresentable_name("DRH257", "~X.MP4"));
    if !try_touch(&bad) {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        return;
    }

    let files = walk(&root, &empty_db("nonutf8"));

    assert_eq!(names(&files), vec!["plain.txt"]);
    assert!(
        files.iter().all(|f| !f.path.contains('\u{FFFD}')),
        "no lossy spelling may reach the writer: {:?}",
        files.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
}

/// A `continue` in `read_directory` leaves the name's row in `rows`, and
/// every row left there is deleted; the row spelled with the lossy name
/// belongs to a different file, which must take its own row out.
#[test]
fn dropping_a_bad_entry_deletes_nothing() {
    let root = tmp_tree("nonutf8-stale");
    let twin = crate::testutil::lossy_twin("DRH257", "~X.MP4");
    touch(&root.join("plain.txt"), b"x");
    touch(&root.join(&twin), b"x");
    let bad = root.join(crate::testutil::unrepresentable_name("DRH257", "~X.MP4"));
    if !try_touch(&bad) {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        return;
    }

    let db = db_with(
        "nonutf8-stale",
        &[
            (path_to_db_string(&root.join("plain.txt")), 0),
            (path_to_db_string(&root.join(&twin)), 0),
        ],
    );
    let stale = stale_only(start(&root, &db));

    assert!(
        stale.is_empty(),
        "both files are on disk; nothing may be deleted: {:?}",
        stale
    );
}

/// Canonicalizing follows symlinks, so a UTF-8 root can resolve onto a name
/// the index cannot spell; stored lossily it names some other directory. It
/// must be recorded as unreadable, which keeps stale cleanup off it.
#[cfg(unix)]
#[test]
fn a_root_that_resolves_onto_an_unrepresentable_name_is_not_walked() {
    let base = tmp_tree("nonutf8-root");
    let real = base.join(crate::testutil::unrepresentable_name("target", ""));
    if !try_mkdir(&real) {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        return;
    }
    touch(&real.join("inside.txt"), b"x");

    let link = base.join("docs");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let root = link.to_str().expect("the configured root is spellable");

    let db = empty_db("nonutf8-root");
    let mut walk = start(Path::new(root), &db);
    let events: Vec<WalkEvent> = walk.by_ref().collect();
    assert!(
        events.is_empty(),
        "nothing under an unspellable root may be walked: {:?}",
        events
    );
    assert!(
        !walk.unreadable().is_empty(),
        "the root must read as unreadable, or its rows fall to stale cleanup"
    );
}

/// A `WalkedFile`'s path is the key its row is written under, so a bad name
/// yielded under its lossy spelling would claim the twin's row, and whichever
/// of the two files the walk reached second would fall out of the index.
#[test]
fn a_bad_name_cannot_stand_in_for_its_lossy_twin() {
    let root = tmp_tree("nonutf8-twin");
    let twin = crate::testutil::lossy_twin("x", ".txt");
    touch(&root.join(&twin), b"x");
    let bad = root.join(crate::testutil::unrepresentable_name("x", ".txt"));
    if !try_touch(&bad) {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        return;
    }

    let files = walk(&root, &empty_db("nonutf8-twin"));

    let prepared: Vec<&WalkedFile> = files.iter().filter(|f| f.record.is_some()).collect();
    assert_eq!(
        prepared.len(),
        1,
        "exactly the representable file is prepared"
    );
    assert_eq!(
        prepared[0].path,
        path_to_db_string(&root.join(&twin)),
        "and it is the real one, under its own name"
    );
}

/// The row prefetcher keys on the directory's parent string, so a bad
/// directory beside a real one with the colliding lossy name would be handed
/// the real one's rows and report every one of them stale.
#[test]
fn a_bad_directory_is_pruned_with_its_whole_subtree() {
    let root = tmp_tree("nonutf8-dir");
    let twin = crate::testutil::lossy_twin("dir", "");
    touch(&root.join(&twin).join("a.txt"), b"x");
    let bad = root.join(crate::testutil::unrepresentable_name("dir", ""));
    if !try_mkdir(&bad) {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        return;
    }
    for n in 0..5 {
        touch(&bad.join(format!("b{}.txt", n)), b"x");
    }
    touch(&bad.join("deep/c.txt"), b"x");

    let db = db_with(
        "nonutf8-dir",
        &[(path_to_db_string(&root.join(&twin).join("a.txt")), 0)],
    );
    let stale = stale_only(start(&root, &db));
    assert!(
        stale.is_empty(),
        "the real directory's row must survive its bad-named sibling: {:?}",
        stale
    );

    let files = walk(&root, &db);
    assert_eq!(
        names(&files),
        vec!["a.txt"],
        "nothing under the bad directory is walked"
    );
}

#[test]
fn wide_directory_is_split_across_workers_without_loss() {
    let root = tmp_tree("wide");
    let count = FILES_PER_JOB * 4 + 7;
    for i in 0..count {
        touch(&root.join(format!("f{:05}.txt", i)), b"x");
    }

    let files = walk(&root, &empty_db("wide"));
    assert_eq!(files.len(), count, "every file is yielded exactly once");
    let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    assert_eq!(unique.len(), count, "and none is yielded twice");
}

#[test]
fn terminates_on_an_empty_root() {
    let root = tmp_tree("empty");
    assert!(walk(&root, &empty_db("empty")).is_empty());
}

/// A mountpoint with nothing mounted lists as empty: walked, every row under
/// it, at any depth, would read as deleted.
#[test]
fn an_empty_root_is_recorded_not_walked() {
    let root = tmp_tree("empty-mount");
    let top = path_to_db_string(&root.join("a.txt"));
    let deep = path_to_db_string(&root.join("sub/b.txt"));
    let db = db_with("empty-mount", &[(top.clone(), 1), (deep.clone(), 1)]);

    let mut walk = start(&root, &db);
    let events: Vec<WalkEvent> = walk.by_ref().collect();
    assert!(events.is_empty(), "nothing may read as stale: {:?}", events);
    assert!(walk.unreadable().covers(&top));
    assert!(
        walk.unreadable().covers(&deep),
        "the vanished sweep must leave the deeper parents alone too"
    );
    assert!(walk.finish(), "a held root still ends the walk cleanly");
}

/// Only an entry-less listing is held: a dot file, though never walked, is
/// something mounted, so the root is walked and its missing rows go.
#[test]
fn a_root_holding_only_a_dot_file_is_walked_normally() {
    let root = tmp_tree("dot-only");
    touch(&root.join(".hidden"), b"x");
    let gone = path_to_db_string(&root.join("a.txt"));
    let db = db_with("dot-only", &[(gone.clone(), 1)]);

    let mut walk = start(&root, &db);
    let stale = stale_only(walk.by_ref());
    assert_eq!(stale, vec![gone]);
    assert!(walk.unreadable().is_empty());
}

#[test]
fn unchanged_files_are_never_opened() {
    // One stat per file and no opens make SMB re-indexing bearable.
    let root = tmp_tree("skip");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");

    let first = walk(&root, &empty_db("skip-first"));
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|f| f.action == FileIndexAction::Insert));

    let indexed: Vec<(String, u64)> = first
        .iter()
        .map(|f| (f.path.clone(), f.record.as_ref().unwrap().mtime))
        .collect();

    let second = walk(&root, &db_with("skip-second", &indexed));
    assert_eq!(
        second.len(),
        2,
        "unchanged files are still reported as seen"
    );
    for f in &second {
        assert_eq!(f.action, FileIndexAction::Skip);
        assert!(f.record.is_none(), "an unchanged file is never hashed");
    }
}

/// A rewrite inside the stored second that changes the size is a change, as
/// the live view reports it: comparing mtime alone would discard the
/// correction the live view feeds back, and flag the row on every showing.
#[test]
fn a_same_second_rewrite_that_changes_size_is_updated() {
    let root = tmp_tree("same-second");
    let p = root.join("a.txt");
    touch(&p, b"x");
    let mtime = crate::file_handling::mtime_secs(&fs::metadata(&p).unwrap());
    let db = db_with("same-second", &[(path_to_db_string(&p), mtime)]);

    fs::write(&p, b"a longer body").unwrap();
    crate::testutil::set_mtime(&p, UNIX_EPOCH + std::time::Duration::from_secs(mtime)).unwrap();

    let f = walk_one(&root, &db);
    assert_eq!(f.action, FileIndexAction::Update);
    let rec = f.record.expect("re-read");
    assert_eq!((rec.size, rec.mtime), (13, mtime));
}

/// A row stored without a hash, as a dehydrated cloud file's is, owes one:
/// once the file reads as an ordinary file it is hashed, and from then on it
/// is unchanged.
#[test]
fn a_row_stored_without_a_hash_is_hashed_once_readable() {
    let root = tmp_tree("unhashed-row");
    let p = root.join("a.txt");
    touch(&p, b"hydrated now");
    let key = path_to_db_string(&p);
    let mtime = crate::file_handling::mtime_secs(&fs::metadata(&p).unwrap());
    let db = db_with("unhashed-row", &[(key.clone(), mtime)]);
    rusqlite::Connection::open(&*db)
        .unwrap()
        .execute("UPDATE files SET hash = NULL", [])
        .unwrap();

    let f = walk_one(&root, &db);
    assert_eq!(f.action, FileIndexAction::Update);
    assert!(f.record.expect("re-read").hash.is_some());

    let f = walk_one(&root, &db_with("hashed-row", &[(key, mtime)]));
    assert_eq!(f.action, FileIndexAction::Skip, "a hashed row is unchanged");
}

/// A time before 1970 clamps to 0 rather than costing the file its row, and
/// the clamped row then reads as unchanged.
#[test]
fn a_file_dated_before_1970_is_indexed() {
    let root = tmp_tree("pre-1970");
    let p = root.join("old.txt");
    touch(&p, b"punched cards");
    if !crate::testutil::backdate_before_1970(&p) {
        return; // The filesystem keeps no pre-1970 time; nothing to test.
    }

    let f = walk_one(&root, &empty_db("pre-1970-first"));
    assert_eq!(f.action, FileIndexAction::Insert);
    assert_eq!(f.record.expect("a record").mtime, 0);

    let f = walk_one(&root, &db_with("pre-1970-second", &[(f.path, 0)]));
    assert_eq!(f.action, FileIndexAction::Skip);
}

/// NTFS's directory entry lags a file open for writing, so the entry and the
/// row disagree while the file itself matches the row. The handle settles it:
/// the row is left alone instead of rewritten, which would clear its content.
#[test]
#[cfg(windows)]
fn a_file_held_open_for_writing_is_not_rewritten_with_its_stale_entry() {
    use std::io::Write;

    let root = tmp_tree("held-open");
    let p = root.join("log.txt");
    touch(&p, b"first line\n");
    let mut held = fs::OpenOptions::new().append(true).open(&p).unwrap();
    held.write_all(b"second line\n").unwrap();
    held.flush().unwrap();
    // The row a watcher event wrote from the file itself.
    let now = held.metadata().unwrap();
    let (dir, db) = Scratch::db("held-open");
    let conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
    let key = path_to_db_string(&p);
    let (parent, name) = crate::file_handling::split_db_path(&key).unwrap();
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime, type, content_state, hash)
         VALUES (?1, ?2, ?3, ?4, 0, 3, zeroblob(32))",
        rusqlite::params![
            name,
            parent,
            now.len() as i64,
            crate::file_handling::mtime_secs(&now) as i64
        ],
    )
    .unwrap();
    drop(conn);
    let db = TestDb {
        _dir: dir,
        path: db,
    };

    let f = walk_one(&root, &db);
    drop(held);
    assert_eq!(f.action, FileIndexAction::Skip);
    assert!(f.record.is_none(), "the row would have been rewritten");
}

/// On Windows the walk reads mtime out of the directory entry while the
/// watcher writes rows from `fs::metadata`; if the two disagreed every run
/// would reclassify files nothing had touched.
#[test]
fn a_walk_agrees_with_a_stat_seeded_index() {
    let root = tmp_tree("stat-seeded");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");

    let seeded: Vec<(String, u64)> = [root.join("a.txt"), root.join("sub/b.txt")]
        .iter()
        .map(|p| {
            let mtime = fs::metadata(p)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            (path_to_db_string(p), mtime)
        })
        .collect();

    let files = walk(&root, &db_with("stat-seeded", &seeded));
    assert_eq!(files.len(), 2);
    for f in &files {
        assert_eq!(
            f.action,
            FileIndexAction::Skip,
            "the directory read disagreed with a stat about {}",
            f.path
        );
        assert!(f.record.is_none());
    }
}

#[test]
#[cfg(windows)]
fn cached_directory_metadata_matches_a_stat_field_for_field() {
    let root = tmp_tree("cached-meta");
    touch(&root.join("a.txt"), b"x");
    fs::write(root.join("b.bin"), vec![0u8; 5000]).unwrap();

    for entry in fs::read_dir(&root).unwrap() {
        let entry = entry.unwrap();
        let cached = crate::platform::entry_cached_metadata(|| entry.metadata().ok())
            .expect("a plain file is served from the directory read");
        let fresh = fs::metadata(entry.path()).unwrap();
        assert_eq!(cached.is_file(), fresh.is_file());
        assert_eq!(cached.len(), fresh.len());
        assert_eq!(cached.modified().unwrap(), fresh.modified().unwrap());
    }
}

/// A path missing from the stream gets its index row deleted.
#[test]
#[cfg(unix)]
fn every_seen_file_is_reported_even_when_it_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_tree("unreadable-file");
    touch(&root.join("fine.txt"), b"x");
    let bad = root.join("bad.txt");
    touch(&bad, b"x");
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();

    let files = walk(&root, &empty_db("unreadable-file"));
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).ok();

    assert_eq!(names(&files), vec!["bad.txt", "fine.txt"]);
    let bad_entry = files.iter().find(|f| f.path.ends_with("bad.txt")).unwrap();
    assert!(bad_entry.record.is_none(), "unopenable, so no record");
}

#[test]
#[cfg(unix)]
fn unreadable_directory_is_recorded_not_silently_empty() {
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_tree("unreadable-dir");
    touch(&root.join("visible.txt"), b"x");
    let locked = root.join("locked");
    touch(&locked.join("inside.txt"), b"x");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let db = empty_db("unreadable-dir");
    let mut w = start(&root, &db);
    let files = files_only(w.by_ref());
    let recorded = !w.unreadable().is_empty();
    let covers = w
        .unreadable()
        .covers(locked.join("inside.txt").to_str().unwrap());

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

    assert_eq!(names(&files), vec!["visible.txt"]);
    assert!(recorded, "the failure must be recorded");
    assert!(covers, "so rows beneath it survive stale cleanup");
}

/// Neither kind of link is followed, nor a loop back to the root; a row
/// stored under a link's own name has no file behind it.
#[test]
#[cfg(unix)]
fn a_symlink_is_never_followed() {
    // Canonical, so the seeded row below is spelled as the walk spells it.
    let root = Scratch::canonical("symlink-never");
    touch(&root.join("real/target.txt"), b"x");
    fs::create_dir_all(root.join("links")).unwrap();
    std::os::unix::fs::symlink(root.join("real/target.txt"), root.join("links/alias.txt")).unwrap();
    let outside = tmp_tree("symlink-never-outside");
    touch(&outside.join("elsewhere.txt"), b"x");
    touch(&outside.join("deep/below.txt"), b"x");
    std::os::unix::fs::symlink(outside.join("elsewhere.txt"), root.join("links/out.txt")).unwrap();
    std::os::unix::fs::symlink(&*outside, root.join("links/outdir")).unwrap();
    std::os::unix::fs::symlink(&*root, root.join("links/loop")).unwrap();

    let files = walk(&root, &empty_db("symlink-never"));
    assert_eq!(
        names(&files),
        vec!["target.txt"],
        "only the real file, reached directly"
    );

    let link_row = path_to_db_string(&root.join("links/alias.txt"));
    let db = db_with("symlink-never-row", &[(link_row.clone(), 1)]);
    assert_eq!(stale_only(start(&root, &db)), vec![link_row]);
}

/// std reports a junction as a link, so it is skipped like one.
#[test]
#[cfg(windows)]
fn a_junction_is_not_followed() {
    let root = tmp_tree("junction");
    touch(&root.join("real").join("target.txt"), b"x");
    // Junctions need no privileges, unlike Windows symlinks.
    let made = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            root.join("jlink").to_str().unwrap(),
            root.join("real").to_str().unwrap(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        return;
    }

    let files = walk(&root, &empty_db("junction"));
    let real = path_to_db_string(&root.join("real").join("target.txt").canonicalize().unwrap());
    assert_eq!(
        files.iter().map(|f| &f.path).collect::<Vec<_>>(),
        vec![&real],
        "only the real file, reached directly"
    );
}

#[test]
fn hidden_and_ignored_entries_are_pruned() {
    let root = tmp_tree("prune");
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("sub/keep2.txt"), b"x");
    touch(&root.join("sub/skip.tmp"), b"x");
    touch(&root.join(".hidden/inside.txt"), b"x");
    touch(&root.join(".dotfile"), b"x");
    touch(&root.join("node_modules/dep/index.js"), b"x");

    let ignore = ["*.tmp", "node_modules"];
    let db = empty_db("prune");
    let files = files_only(start_with(&[&root], &db, false, &ignore, false));
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    let files = walk_with(&root, &empty_db("prune-hidden"), true);
    assert_eq!(
        names(&files),
        vec![
            ".dotfile",
            "index.js",
            "inside.txt",
            "keep.txt",
            "keep2.txt",
            "skip.tmp"
        ],
        "include_hidden with no ignore patterns keeps everything"
    );
}

#[test]
fn pruned_entries_are_counted_by_reason() {
    let root = tmp_tree("prune-counts");
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("sub/keep2.txt"), b"x");
    touch(&root.join("sub/skip.tmp"), b"x");
    touch(&root.join(".hidden/inside.txt"), b"x");
    touch(&root.join(".hidden/also-inside.txt"), b"x");
    touch(&root.join(".dotfile"), b"x");
    touch(&root.join("node_modules/dep/lib/index.js"), b"x");

    let ignore = ["*.tmp", "node_modules"];
    let db = empty_db("prune-counts");
    let mut walk = start_with(&[&root], &db, false, &ignore, false);
    let files = files_only(walk.by_ref());
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    let pruned = walk.pruned();
    assert_eq!(
        pruned.dot_named.load(Ordering::Relaxed),
        2,
        "`.hidden` and `.dotfile`, not the two files inside `.hidden`"
    );
    assert_eq!(
        pruned.ignored.load(Ordering::Relaxed),
        2,
        "`skip.tmp` and `node_modules`, not `index.js` three levels down"
    );
    // Attributes are a Windows concept; on Linux nothing can reach this
    // counter, and on Windows a temp tree carries no Hidden bit.
    assert_eq!(pruned.attribute.load(Ordering::Relaxed), 0);
    assert_eq!(pruned.total(), 4);

    let summary = pruned.summary().expect("something was pruned");
    assert!(summary.contains("4 entries"), "{}", summary);
}

#[test]
fn a_tree_with_nothing_pruned_reports_no_summary() {
    let root = tmp_tree("prune-none");
    touch(&root.join("keep.txt"), b"x");
    touch(&root.join("sub/keep2.txt"), b"x");

    let db = empty_db("prune-none");
    let mut walk = start(&root, &db);
    let files = files_only(walk.by_ref());
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    assert_eq!(walk.pruned().total(), 0);
    assert!(walk.pruned().summary().is_none());
}

#[test]
fn a_directory_reports_rows_with_no_file_behind_them() {
    let root = tmp_tree("reconcile");
    touch(&root.join("kept.txt"), b"x");
    touch(&root.join("sub/nested.txt"), b"x");

    let gone = path_to_db_string(&root.join("removed.txt"));
    let gone_nested = path_to_db_string(&root.join("sub/vanished.txt"));
    let kept = path_to_db_string(&root.join("kept.txt"));
    let db = db_with(
        "reconcile",
        &[
            (gone.clone(), 1),
            (gone_nested.clone(), 1),
            (kept.clone(), 1),
        ],
    );

    let mut stale = stale_only(start(&root, &db));
    stale.sort();

    let mut want = vec![gone, gone_nested];
    want.sort();
    assert_eq!(
        stale, want,
        "exactly the rows with no file, from both directories"
    );
}

/// A directory wider than one job is spread across the pool, and each file
/// takes its stored stamp with it: every chunk must classify against the
/// file's own row, and only the row with no file is stale.
#[test]
fn every_file_of_a_wide_directory_is_classified_against_its_own_row() {
    let root = tmp_tree("wide");
    let total = FILES_PER_JOB * 2 + 7;
    let path = |i: usize| root.join(format!("f{:04}.txt", i));
    for i in 0..total {
        touch(&path(i), b"x");
    }
    let mtime = |i: usize| crate::file_handling::mtime_secs(&fs::metadata(path(i)).unwrap());
    // Thirds: unchanged, changed, never indexed.
    let mut rows: Vec<(String, u64)> = (0..total)
        .filter(|i| i % 3 != 2)
        .map(|i| (path_to_db_string(&path(i)), mtime(i) + (i % 3) as u64))
        .collect();
    let gone = path_to_db_string(&root.join("gone.txt"));
    rows.push((gone.clone(), 1));
    let db = db_with("wide", &rows);

    let (mut files, mut stale) = (Vec::new(), Vec::new());
    for event in start(&root, &db) {
        match event {
            WalkEvent::File(f) => files.push(f),
            WalkEvent::Stale(paths) => stale.extend(paths),
        }
    }

    assert_eq!(files.len(), total);
    for f in &files {
        let name = Path::new(&f.path).file_name().unwrap().to_str().unwrap();
        let i: usize = name[1..5].parse().unwrap();
        let want = [
            FileIndexAction::Skip,
            FileIndexAction::Update,
            FileIndexAction::Insert,
        ][i % 3];
        assert_eq!(f.action, want, "{}", name);
    }
    assert_eq!(stale, vec![gone], "only the row with no file behind it");
}

#[test]
#[cfg(unix)]
fn an_unreadable_directory_reports_nothing_stale() {
    use std::os::unix::fs::PermissionsExt;

    let root = tmp_tree("reconcile-locked");
    let locked = root.join("locked");
    touch(&locked.join("inside.txt"), b"x");

    let db = db_with(
        "reconcile-locked",
        &[(path_to_db_string(&locked.join("inside.txt")), 1)],
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let stale = stale_only(start(&root, &db));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

    assert!(
        stale.is_empty(),
        "an unreadable directory is not an empty one"
    );
}

#[test]
fn hidden_root_is_still_walked() {
    let base = tmp_tree("hidden-root");
    let root = base.join(".config");
    touch(&root.join("app.conf"), b"x");

    let files = walk(&root, &empty_db("hidden-root"));
    assert_eq!(names(&files), vec!["app.conf"]);
}

#[test]
fn stop_flag_ends_the_walk_without_hanging() {
    let root = tmp_tree("stop");
    for i in 0..500 {
        touch(&root.join(format!("f{:04}.txt", i)), b"x");
    }

    let db = empty_db("stop");
    let files = files_only(start_with(&[&root], &db, false, &[], true));

    assert!(files.is_empty(), "an already-stopped walk delivers nothing");
}

#[test]
fn dropping_the_walk_early_does_not_hang() {
    // Workers blocked in `send` must be released by the receiver going
    // away, or `Drop` would join threads that never wake.
    let root = tmp_tree("early-drop");
    for i in 0..2000 {
        touch(&root.join(format!("sub{}/f{}.txt", i % 10, i)), b"x");
    }

    let db = empty_db("early-drop");
    let mut w = start(&root, &db);
    assert!(w.next().is_some());
    drop(w); // must return, not deadlock
}

#[test]
fn overlapping_roots_yield_each_file_once() {
    let root = tmp_tree("overlap");
    touch(&root.join("sub/a.txt"), b"x");

    let sub = root.join("sub");
    let db = empty_db("overlap");
    let files = files_only(start_with(&[&root, &sub], &db, false, &[], false));

    assert_eq!(files.len(), 1, "the nested root must not double-index");
}

#[test]
fn repeated_walks_agree_on_the_result_set() {
    // Enough runs under contention for a racy premature exit to show up.
    let root = tmp_tree("repeat");
    for i in 0..40 {
        touch(&root.join(format!("d{}/f{}.txt", i % 7, i)), b"x");
    }
    let expected = names(&walk(&root, &empty_db("determinism-base")));
    assert_eq!(expected.len(), 40);

    for run in 0..30 {
        assert_eq!(
            names(&walk(&root, &empty_db(&format!("determinism-{}", run)))),
            expected,
            "run {}",
            run
        );
    }
}

#[test]
fn finish_reports_a_clean_walk_and_is_idempotent() {
    // The caller gates stale-row deletion on this: a partial walk's "not
    // seen" would otherwise read as "deleted".
    let root = tmp_tree("finish");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");

    let db = empty_db("finish");
    let mut w = start(&root, &db);
    let files = files_only(w.by_ref());
    assert_eq!(files.len(), 2);
    assert!(w.finish(), "no worker panicked");
    assert!(w.finish());
    drop(w);
}

/// Without rows no directory is read, and the vanished sweep would take an
/// unread directory's subtree for deleted.
#[test]
fn a_walk_whose_row_reader_cannot_open_is_not_clean() {
    let root = tmp_tree("reader-open");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");

    let missing = Scratch::dir("reader-open-missing");
    let foreign = Scratch::dir("reader-open-foreign");
    let foreign_db = foreign.join("index.sqlite");
    let body = b"plain text, not an index\n".repeat(200);
    fs::write(&foreign_db, &body).unwrap();

    for db in [missing.join("index.sqlite"), foreign_db.clone()] {
        let mut w = start(&root, &db);
        let stale = stale_only(w.by_ref());
        assert!(!w.finish(), "{}: a walk without rows", db.display());
        assert!(stale.is_empty(), "{}: {:?}", db.display(), stale);
    }
    assert!(
        !missing.join("index.sqlite").exists(),
        "the reader creates nothing"
    );
    assert_eq!(
        fs::read(&foreign_db).unwrap(),
        body,
        "and modifies no foreign file"
    );
}

#[test]
fn a_failed_row_query_ends_the_walk_unclean() {
    let root = tmp_tree("reader-query");
    touch(&root.join("a.txt"), b"x");
    touch(&root.join("sub/b.txt"), b"x");
    let db = db_with(
        "reader-query",
        &[
            (path_to_db_string(&root.join("a.txt")), 1),
            (path_to_db_string(&root.join("sub/b.txt")), 1),
        ],
    );
    // A real index that opens, whose every row query then fails.
    crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default())
        .unwrap()
        .execute_batch("ALTER TABLE files RENAME TO files_gone")
        .unwrap();

    let mut w = start(&root, &db);
    let stale = stale_only(w.by_ref());
    assert!(!w.finish(), "a directory went unread");
    assert!(stale.is_empty(), "{:?}", stale);
}

#[test]
fn a_stopped_walk_is_not_clean() {
    let root = tmp_tree("stopped-unclean");
    for i in 0..500 {
        touch(&root.join(format!("f{:04}.txt", i)), b"x");
    }

    let db = empty_db("stopped-unclean");
    let mut w = start_with(&[&root], &db, false, &[], true);
    files_only(w.by_ref());
    assert!(!w.finish(), "a stopped walk did not run out of work");
}

/// A prefetcher that dies mid-query leaves `prefetching` set, so no idle
/// proof can fire; its guard has to release the parked workers.
#[test]
fn a_prefetcher_that_dies_releases_the_pool() {
    let shared = Shared {
        queue: Mutex::new(Queue {
            needs_rows: vec![PathBuf::from("queued")],
            prefetching: true,
            ..Queue::default()
        }),
        idle: Condvar::new(),
        stats: WorkerStats::new(1),
    };
    std::thread::scope(|s| {
        let prefetcher = s.spawn(|| {
            let _end = EndWalk(&shared);
            panic!("the prefetcher died mid-query");
        });
        assert!(prefetcher.join().is_err());
    });

    assert!(shared.take().is_none(), "the workers are released");
    assert!(
        !crate::lock_ok(&shared.queue).exhausted,
        "a walk that ended this way did not run out of work"
    );
}

/// `read_directory`'s context, with the default settings.
fn test_ctx() -> Ctx {
    Ctx {
        include_hidden: false,
        ignore: IgnoreSet::compile(&[]).unwrap(),
        index_files: HashSet::new(),
        pruned: PruneCounts::default(),
        config: Config::default(),
        registry: Arc::new(Registry::default_set()),
        unreadable: UnreadableDirs::default(),
        stop_flag: Arc::new(AtomicBool::new(false)),
        roots: Vec::new(),
    }
}

/// `read_directory` on `dir` with one stored row, `x`; returns the stale set.
fn read_with_row(dir: &Path, ctx: &Ctx) -> Vec<String> {
    let (mut found, mut stale) = (Vec::new(), Vec::new());
    let stamp = Stamp {
        mtime: 1,
        size: 1,
        hashed: true,
    };
    let rows = DirRows::from([("x".to_string(), stamp)]);
    let files = read_directory(dir, rows, ctx, &mut found, &mut stale);
    assert!(files.is_empty() && found.is_empty(), "nothing was listed");
    stale
}

/// Everything under a pulled drive or a dropped share fails the same way,
/// parents included; the walk cannot tell that from a deletion, so it keeps
/// the rows.
#[test]
fn a_directory_gone_before_its_read_is_recorded_not_swept() {
    let base = tmp_tree("gone-before-read");
    let gone = base.join("gone");
    touch(&gone.join("sub/x"), b"x");
    fs::remove_dir_all(&gone).unwrap();

    let ctx = test_ctx();
    let sub = gone.join("sub");
    let stale = read_with_row(&sub, &ctx);

    assert!(stale.is_empty(), "no row is known gone: {:?}", stale);
    assert!(
        ctx.unreadable.covers(sub.join("x").to_str().unwrap()),
        "recorded, so the rows beneath survive the sweep"
    );
}

#[test]
fn not_found_counts_as_deleted_only_while_the_parent_resolves() {
    let base = tmp_tree("gone-parent");
    let gone = base.join("gone");
    touch(&gone.join("sub/x"), b"x");
    fs::remove_dir_all(&gone).unwrap();

    // `base` still resolves: `gone` was deleted, and its rows go with it.
    let ctx = test_ctx();
    assert_eq!(
        read_with_row(&gone, &ctx),
        vec![path_to_db_string(&gone.join("x"))]
    );
    assert!(ctx.unreadable.is_empty(), "a deletion is not a failure");

    // `gone` does not: nothing is known about `gone/sub`.
    let (ctx, sub) = (test_ctx(), gone.join("sub"));
    assert!(read_with_row(&sub, &ctx).is_empty());
    assert!(ctx.unreadable.covers(sub.join("x").to_str().unwrap()));
}

/// A root gone between the seed and its read (an unplugged drive, an autofs
/// expiry) leaves its parent resolving, and is still not a deletion: read as
/// one, it would empty the whole root.
#[test]
fn a_root_gone_before_its_read_is_recorded_not_swept() {
    let base = tmp_tree("gone-root");
    let root = base.join("root");
    touch(&root.join("x"), b"x");
    fs::remove_dir_all(&root).unwrap();

    let ctx = Ctx {
        roots: vec![root.clone()],
        ..test_ctx()
    };
    let stale = read_with_row(&root, &ctx);

    assert!(
        stale.is_empty(),
        "the root's rows read as deleted: {:?}",
        stale
    );
    assert!(
        ctx.unreadable.covers(root.join("x").to_str().unwrap()),
        "recorded, so the rows beneath survive the sweep"
    );
}

#[test]
fn local_temp_dir_is_not_detected_as_network() {
    let root = tmp_tree("fstype");
    assert_eq!(
        thread_count_for(&[root.to_string_lossy().into_owned()]),
        LOCAL_THREADS
    );
}

/// The walk must never open the index's own files (see
/// `file_handling::index_file_set`). `include_hidden` is on because the index
/// lives under dot-named `~/.local/share`, which a default walk skips.
#[test]
fn the_index_and_its_sidecars_are_never_walked() {
    let root = tmp_tree("walk-self-index");
    touch(&root.join("ordinary.txt"), b"x");

    let db = root.join("data").join("index.sqlite");
    let conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();
    // A write, so the WAL and SHM exist to be walked over.
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime, type, content_state)
         VALUES ('a', '/', 0, 0, 0, 3)",
        [],
    )
    .unwrap();

    let found = names(&walk_with(&root, &db, true));
    assert!(
        found.contains(&"ordinary.txt".to_string()),
        "the walk should still report ordinary files: {:?}",
        found
    );
    for name in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
        assert!(
            !found.contains(&name.to_string()),
            "{} was walked; found {:?}",
            name,
            found
        );
    }
    drop(conn);
}

/// A stored row for the index itself has to go, or the live watcher and
/// duplicate verification keep opening the file; a skip would keep the row,
/// so the walk leaves the name's row in the directory's `rows` instead.
#[test]
fn a_stored_row_for_the_index_falls_to_the_stale_sweep() {
    let root = crate::testutil::scratch_dir_canonical("walk-self-index-stale");
    let db = root.join("data").join("index.sqlite");
    let conn = crate::db::open_or_recreate(db.to_str().unwrap(), &Default::default()).unwrap();

    // A row for the database itself.
    let stored = path_to_db_string(&db);
    let (parent, name) = crate::file_handling::split_db_path(&stored).expect("a file's path");
    conn.execute(
        "INSERT INTO files (name, parent, size, mtime, type, content_state)
         VALUES (?1, ?2, 0, 0, 0, 3)",
        rusqlite::params![name, parent],
    )
    .unwrap();
    drop(conn);

    let stale = stale_only(start_with(&[&root], &db, true, &[], false));
    assert!(
        stale.contains(&stored),
        "the index's own row should be swept; got {:?}",
        stale
    );
}
