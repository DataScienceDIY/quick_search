use std::path::Path;

use crate::config::IgnoreSet;

use super::records::get_file_hash;
use super::*;
use std::path::MAIN_SEPARATOR;

fn tmp_tree() -> std::path::PathBuf {
    crate::testutil::scratch_dir("walk")
}

fn touch(p: &Path) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
}

#[test]
fn filtered_walk_prunes_hidden_and_ignored() {
    let root = tmp_tree();
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/keep2.txt"));
    touch(&root.join("sub/skip.tmp"));
    touch(&root.join(".hidden/inside.txt"));
    touch(&root.join(".dotfile"));
    touch(&root.join("node_modules/dep/index.js"));
    touch(&root.join("secret/deep/file.txt"));

    let ignore = IgnoreSet::compile(&[
        "*.tmp".to_string(),
        "node_modules".to_string(),
        format!("{}/secret", root.display()),
    ])
    .unwrap();

    let mut names: Vec<String> = filtered_walk(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    names.sort();
    assert_eq!(names, vec!["keep.txt", "keep2.txt"]);

    // include_hidden brings back dotfiles but ignores still apply.
    let mut names: Vec<String> = filtered_walk(
        root.to_str().unwrap(),
        false,
        true,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    names.sort();
    assert_eq!(
        names,
        vec![".dotfile", "inside.txt", "keep.txt", "keep2.txt"]
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The watcher spends one inotify descriptor per directory this yields,
/// so it must prune exactly like [`filtered_walk`] — the pruned
/// subtrees are the whole saving.
#[test]
fn filtered_dirs_yields_only_kept_directories() {
    let root = tmp_tree();
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/nested/keep2.txt"));
    touch(&root.join(".hidden/inside.txt"));
    touch(&root.join("node_modules/dep/index.js"));

    let ignore = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();

    let mut names: Vec<String> = filtered_dirs(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    names.sort();
    // The root itself is included (depth 0 is always kept); `.hidden`,
    // `node_modules`, and `node_modules/dep` cost nothing.
    let root_name = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut want = vec![root_name.clone(), "nested".to_string(), "sub".to_string()];
    want.sort();
    assert_eq!(names, want);

    // include_hidden brings the dotted directory back.
    let names: Vec<String> = filtered_dirs(
        root.to_str().unwrap(),
        false,
        true,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    assert!(names.contains(&".hidden".to_string()));
    assert!(
        !names.contains(&"node_modules".to_string()),
        "ignores still apply with include_hidden"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Files and directories partition the walk: every entry lands in
/// exactly one of the two iterators.
#[test]
fn filtered_dirs_and_filtered_walk_do_not_overlap() {
    let root = tmp_tree();
    touch(&root.join("a.txt"));
    touch(&root.join("sub/b.txt"));
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
    // Users explicitly chose their roots — a hidden root dir must not
    // silence the whole walk.
    let base = tmp_tree();
    let root = base.join(".config");
    touch(&root.join("app.conf"));
    let ignore = IgnoreSet::compile(&[]).unwrap();
    let names: Vec<String> = filtered_walk(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    assert_eq!(names, vec!["app.conf"]);
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn classify_uses_mtime_against_the_existing_index() {
    let mut rows = DirRows::new();
    rows.insert("known.txt".to_string(), 100);

    assert_eq!(
        classify_for_indexing("new.txt", 100, &rows),
        FileIndexAction::Insert,
        "a name absent from the directory's rows is new"
    );
    assert_eq!(
        classify_for_indexing("known.txt", 100, &rows),
        FileIndexAction::Skip,
        "same mtime means nothing to do"
    );
    assert_eq!(
        classify_for_indexing("known.txt", 101, &rows),
        FileIndexAction::Update,
        "a changed mtime means re-read"
    );
}

#[test]
fn classify_by_mtime_matches_the_name_keyed_path() {
    // Resolved symlink targets take this route because their row is
    // found by exact path, not by name within the directory walked.
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
    // A share must come back as \\server\share, not UNC\server\share —
    // stripping a fixed four characters produces a path that cannot be
    // opened, and every file beneath it would be misfiled.
    assert_eq!(
        path_to_db_string(Path::new(r"\\?\UNC\server\share\a.txt")),
        r"\\server\share\a.txt"
    );
    // A volume mounted at a folder has no drive letter, so the prefix is
    // load-bearing: `Volume{...}\a.txt` is not a path anything can open.
    assert_eq!(
        path_to_db_string(Path::new(r"\\?\Volume{9f8a}\data\a.txt")),
        r"\\?\Volume{9f8a}\data\a.txt"
    );
}

/// The range must bracket paths spelled with the *platform's* separator: a
/// hard-coded `'/' + 1` `hi` sorts below every `C:\…` path, leaving an empty
/// range that silently disables extraction and the vanished-directory sweep.
#[test]
fn extract_cursor_brackets_paths_under_its_root() {
    use std::path::MAIN_SEPARATOR as SEP;

    let root = format!("{}Users{}me", root_prefix(), SEP);
    let c = ExtractCursor::for_root(&root);
    let inside = format!("{}{}docs{}a.txt", root, SEP, SEP);
    assert!(
        inside.as_str() >= c.lo.as_str() && inside.as_str() < c.hi.as_str(),
        "{:?} must fall inside [{:?}, {:?})",
        inside,
        c.lo,
        c.hi
    );

    // The directory itself is *not* in the range (the range is what lives
    // beneath it), and a prefix sibling is outside it.
    assert!(root.as_str() < c.lo.as_str());
    let sibling = format!("{}Users{}mexico{}a.txt", root_prefix(), SEP, SEP);
    assert!(
        !(sibling.as_str() >= c.lo.as_str() && sibling.as_str() < c.hi.as_str()),
        "{:?} is a sibling of {:?}, not a child",
        sibling,
        root
    );

    // A trailing separator of either flavour must not double up.
    for spelled in [format!("{}/", root), format!("{}\\", root)] {
        let c2 = ExtractCursor::for_root(&spelled);
        assert_eq!((&c2.lo, &c2.hi), (&c.lo, &c.hi), "spelled {:?}", spelled);
    }
}

/// An absolute-path prefix for the running platform.
fn root_prefix() -> String {
    if cfg!(windows) {
        r"C:\".to_string()
    } else {
        "/".to_string()
    }
}

/// A Remove event names a path that is already gone, so the key for it has
/// to be built from the deepest ancestor that still resolves.
#[test]
fn db_key_for_a_vanished_path_canonicalizes_what_remains() {
    let root = tmp_tree();
    let real = root.join("sub");
    std::fs::create_dir_all(&real).unwrap();

    let missing = real.join("gone").join("deeper.txt");
    let key = db_key_for_missing_path(&missing);

    let expected = path_to_db_string(&real.canonicalize().unwrap().join("gone").join("deeper.txt"));
    assert_eq!(key, expected, "existing prefix resolved, missing tail kept");

    // A redundant component in the *existing* part is collapsed, which is
    // the whole point — the stored key never contains one.
    let odd = root.join("sub").join(".").join("gone.txt");
    let odd_key = db_key_for_missing_path(&odd);
    assert!(
        !odd_key.contains(&format!("{}.{}", MAIN_SEPARATOR, MAIN_SEPARATOR)),
        "unexpected `.` component in {}",
        odd_key
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The hazard every screen in the codebase exists for, stated once here.
///
/// `path_to_db_string` is many-to-one, and the collapsed spelling is not
/// garbage — it is a perfectly ordinary filename that a *different* file can
/// really have. So the stored key for an unrepresentable path is another
/// file's key, and any use of it (open, hash, delete by path, delete a subtree
/// range) lands on that file instead. `warn_if_unrepresentable` is how a
/// caller holding a single path avoids ever building one.
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

    // And the same one component deeper, which is why a bad *directory* has to
    // be pruned rather than walked: the collision is inherited by everything
    // beneath it.
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
    // The bug a naive `str::starts_with` would introduce: /a/bc is a
    // sibling of /a/b, and its rows must stay deletable.
    assert!(!u.covers("/a/bc/d.txt"));
    assert!(!u.covers("/a/other.txt"));
}

#[test]
fn unreadable_directory_is_reported_rather_than_yielded_as_empty() {
    // A directory the walk cannot read must be recorded, so the caller
    // can tell "could not look" apart from "the files are gone" — the
    // latter deletes index rows.
    let root = tmp_tree();
    touch(&root.join("readable/a.txt"));
    let locked = root.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    touch(&locked.join("hidden-from-us.txt"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let ignore = IgnoreSet::compile(&[]).unwrap();
        let failures = UnreadableDirs::default();
        let names: Vec<String> =
            filtered_walk(root.to_str().unwrap(), false, false, &ignore, &failures)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();

        // Restore before asserting so a failure still cleans up.
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
    }

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn hash_covers_size_and_head_only() {
    let root = tmp_tree();
    let a = root.join("a.bin");
    let b = root.join("b.bin");
    let c = root.join("c.bin");

    // Same size, same head, differing only in the tail: the documented
    // collision (pre-allocated VM images are the real-world case).
    std::fs::write(&a, [b"HEAD".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();
    std::fs::write(&b, [b"HEAD".as_slice(), &[0u8; 64], b"BBBB"].concat()).unwrap();
    // Differs within the head window.
    std::fs::write(&c, [b"DIFF".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();

    let h = |p: &Path| {
        get_file_hash(std::fs::metadata(p).unwrap().len(), p, 8)
            .unwrap()
            .0
    };
    assert_eq!(h(&a), h(&b), "tail differences are invisible by design");
    assert_ne!(h(&a), h(&c), "head differences are caught");

    // Size participates, so a prefix does not collide with its extension.
    let short = root.join("short.bin");
    std::fs::write(&short, b"HEAD").unwrap();
    assert_ne!(h(&a), h(&short));

    // The head is returned for MIME sniffing rather than re-read.
    let (_, head) = get_file_hash(72, &a, 8).unwrap();
    assert_eq!(head, b"HEAD\0\0\0\0", "exactly hash_length bytes");
    let (_, head) = get_file_hash(4, &short, 8).unwrap();
    assert_eq!(head, b"HEAD", "a short file hashes whole");

    std::fs::remove_dir_all(&root).ok();
}

/// The invariant the whole schema rests on: `parent` and `name` concatenate
/// back into the path, with no separator logic at the join.
///
/// Checked against real paths built by `Path::join` rather than by string
/// formatting, so a platform whose separator is not `/` is tested too.
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

/// A filesystem root already ends in a separator, so `dir_to_db_parent` must
/// not add a second one — otherwise every file directly at the root would be
/// stored under `//` (or `C:\\`) and never found again.
#[test]
fn a_root_directory_does_not_get_a_doubled_separator() {
    let root = std::path::Path::new(if cfg!(windows) { r"C:\" } else { "/" });
    let parent = dir_to_db_parent(root);
    assert_eq!(parent, path_to_db_string(root));
    assert!(!parent.ends_with(&format!("{}{}", MAIN_SEPARATOR, MAIN_SEPARATOR)));
    // And the join still produces a path that names the same file.
    assert_eq!(
        format!("{}{}", parent, "a.txt"),
        path_to_db_string(&root.join("a.txt"))
    );
}

/// Strings that are not a file's path have no key, and must say so rather
/// than producing a half-formed one — every lookup helper reads `None` as
/// "not indexed".
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

/// On Unix a backslash is an ordinary filename character. Splitting on it
/// there would put half a name in the parent, and the row would be
/// unreachable by the path it was stored under.
#[test]
#[cfg(unix)]
fn a_backslash_is_just_a_character_on_unix() {
    let (parent, name) = split_db_path(r"/home/me/back\slash.txt").expect("a file's path");
    assert_eq!(parent, "/home/me/");
    assert_eq!(name, r"back\slash.txt");

    // Same on the directory side: a folder genuinely named `weird\` still
    // gets its own separator appended.
    assert_eq!(
        dir_to_db_parent(std::path::Path::new(r"/tmp/weird\")),
        r"/tmp/weird\/"
    );
}

/// walkdir defaults `follow_root_links` to **true**, independently of
/// `follow_links` — so a root that is itself a symlink was descended even with
/// following off. It matters more than a duplicate scan: the records are
/// stored under the canonicalized path, so a target outside every configured
/// root lands where no sweep range reaches and stays there until a rebuild.
#[test]
#[cfg(unix)]
fn a_symlinked_root_is_not_descended_when_following_is_off() {
    let base = tmp_tree();
    let real = base.join("real");
    touch(&real.join("inside.txt"));
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let names: Vec<String> = filtered_walk(
        link.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    assert!(
        names.is_empty(),
        "a symlinked root must not be descended: {names:?}"
    );

    // With following on it is descended, as it always was.
    let names: Vec<String> = filtered_walk(
        link.to_str().unwrap(),
        true,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    assert_eq!(names, vec!["inside.txt"]);

    std::fs::remove_dir_all(&base).ok();
}

/// A symlink *inside* a root is the same hazard one level down: yielding it
/// as a file indexes it under the canonicalized target path.
#[test]
#[cfg(unix)]
fn a_symlink_inside_a_root_is_skipped_when_following_is_off() {
    let base = tmp_tree();
    let root = base.join("root");
    touch(&root.join("real.txt"));
    let outside = base.join("outside");
    touch(&outside.join("target.txt"));
    std::os::unix::fs::symlink(outside.join("target.txt"), root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let mut names: Vec<String> = filtered_walk(
        root.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    names.sort();
    assert_eq!(names, vec!["real.txt"], "only the real file may be walked");

    // Following on: the link resolves and its target is walked through it.
    let mut names: Vec<String> = filtered_walk(
        root.to_str().unwrap(),
        true,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
    names.sort();
    assert_eq!(names, vec!["link.txt", "real.txt", "target.txt"]);

    std::fs::remove_dir_all(&base).ok();
}

/// The same for the directory walk the watcher registers from: it must not
/// spend descriptors on a subtree the indexer will discard.
#[test]
#[cfg(unix)]
fn a_symlinked_root_yields_no_directories_when_following_is_off() {
    let base = tmp_tree();
    let real = base.join("real");
    touch(&real.join("sub/inside.txt"));
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let ignore = IgnoreSet::compile(&[]).unwrap();
    let dirs: Vec<String> = filtered_dirs(
        link.to_str().unwrap(),
        false,
        false,
        &ignore,
        &UnreadableDirs::default(),
    )
    .map(|e| e.path().display().to_string())
    .collect();
    assert!(
        dirs.is_empty(),
        "a symlinked root must yield no directories: {dirs:?}"
    );

    std::fs::remove_dir_all(&base).ok();
}
