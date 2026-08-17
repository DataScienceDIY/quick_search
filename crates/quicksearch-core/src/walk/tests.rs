use super::*;

fn tmp_tree(tag: &str) -> PathBuf {
    crate::testutil::scratch_dir(tag)
}

fn touch(p: &Path) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"x").unwrap();
}

/// A database seeded with `rows` as already-indexed files.
fn db_with(tag: &str, rows: &[(String, u64)]) -> PathBuf {
    let p = crate::testutil::scratch_dir(tag).join("index.sqlite");
    let conn = crate::db::open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
    for (path, mtime) in rows {
        let as_path = Path::new(path);
        conn.execute(
            "INSERT INTO files (name, path, parent, size, mtime, type, \
                                basic_state, content_state)
             VALUES (?1, ?2, ?3, 0, ?4, 0, 1, 3)",
            rusqlite::params![
                as_path.file_name().unwrap().to_string_lossy(),
                path,
                as_path.parent().unwrap().to_string_lossy(),
                *mtime as i64,
            ],
        )
        .unwrap();
    }
    p
}

fn empty_db(tag: &str) -> PathBuf {
    db_with(tag, &[])
}

fn walk(root: &Path, db: &Path) -> Vec<WalkedFile> {
    walk_with(root, db, false, false)
}

fn walk_with(
    root: &Path,
    db: &Path,
    follow_symlinks: bool,
    include_hidden: bool,
) -> Vec<WalkedFile> {
    files_only(walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        follow_symlinks,
        include_hidden,
        IgnoreSet::compile(&[]).unwrap(),
        db.to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ))
}

/// Drop the reconciliation events; most tests are about which files the
/// walk reports.
fn files_only(walk: ParallelWalk) -> Vec<WalkedFile> {
    walk.filter_map(|e| match e {
        WalkEvent::File(f) => Some(f),
        WalkEvent::Stale(_) => None,
    })
    .collect()
}

/// Paths the walk decided no longer have a file behind them.
fn stale_only(walk: ParallelWalk) -> Vec<String> {
    walk.filter_map(|e| match e {
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
    touch(&root.join("a.txt"));
    touch(&root.join("sub/b.txt"));
    touch(&root.join("sub/deep/c.txt"));
    touch(&root.join("other/d.txt"));

    let files = walk(&root, &empty_db("nested"));
    assert_eq!(names(&files), vec!["a.txt", "b.txt", "c.txt", "d.txt"]);

    let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    assert_eq!(unique.len(), files.len(), "no path may be yielded twice");
    fs::remove_dir_all(&root).ok();
}

/// A name that is not valid UTF-8 must be skipped, not deleted. Unix only:
/// on Windows `OsString` comes from UTF-16 and the case cannot be built.
#[cfg(unix)]
#[test]
fn a_non_utf8_name_is_skipped_and_never_prepared() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = tmp_tree("nonutf8");
    touch(&root.join("plain.txt"));
    // 0xFF only survives `to_string_lossy` as U+FFFD.
    let bad = root.join(OsStr::from_bytes(b"DRH257\xff~X.MP4"));
    touch(&bad);
    assert!(bad.symlink_metadata().is_ok(), "the file really is on disk");

    let files = walk(&root, &empty_db("nonutf8"));

    // Both are yielded, so neither reads as deleted...
    assert_eq!(files.len(), 2, "the bad name is still reported as seen");
    // ...but only the representable one is prepared for insertion.
    let prepared: Vec<&WalkedFile> = files.iter().filter(|f| f.record.is_some()).collect();
    assert_eq!(prepared.len(), 1);
    assert!(prepared[0].path.ends_with("plain.txt"));

    let skipped = files.iter().find(|f| f.record.is_none()).unwrap();
    assert!(matches!(skipped.action, FileIndexAction::Skip));
    assert!(
        skipped.path.contains('\u{FFFD}'),
        "stored spelling is the lossy one"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn wide_directory_is_split_across_workers_without_loss() {
    // More than FILES_PER_JOB in one flat directory, so the chunking path
    // and the termination protocol both run under real contention.
    let root = tmp_tree("wide");
    let count = FILES_PER_JOB * 4 + 7;
    for i in 0..count {
        touch(&root.join(format!("f{:05}.txt", i)));
    }

    let files = walk(&root, &empty_db("wide"));
    assert_eq!(files.len(), count, "every file is yielded exactly once");
    let unique: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    assert_eq!(unique.len(), count, "and none is yielded twice");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn terminates_on_an_empty_root() {
    // The "queue empty at t=0" corner: every worker must observe the walk
    // as finished rather than waiting for work that will never arrive.
    let root = tmp_tree("empty");
    assert!(walk(&root, &empty_db("empty")).is_empty());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn unchanged_files_are_never_opened() {
    // The property the whole SMB story rests on: a re-index of an
    // unchanged tree must cost one stat per file and no file opens.
    let root = tmp_tree("skip");
    touch(&root.join("a.txt"));
    touch(&root.join("sub/b.txt"));

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
    fs::remove_dir_all(&root).ok();
}

/// The walk's mtime and a `stat`'s mtime must be the same number: on Windows
/// the walk reads mtime out of the directory entry while the watcher writes
/// its rows from `fs::metadata`, and if the two disagreed every run would
/// reclassify files nothing had touched.
#[test]
fn a_walk_agrees_with_a_stat_seeded_index() {
    let root = tmp_tree("stat-seeded");
    touch(&root.join("a.txt"));
    touch(&root.join("sub/b.txt"));

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
    fs::remove_dir_all(&root).ok();
}

/// Windows: the fields `prepare` and `prepare_file_record` read out of the
/// cached buffer, pinned against what a `stat` would have said.
#[test]
#[cfg(windows)]
fn cached_directory_metadata_matches_a_stat_field_for_field() {
    let root = tmp_tree("cached-meta");
    touch(&root.join("a.txt"));
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
    fs::remove_dir_all(&root).ok();
}

#[test]
fn every_seen_file_is_reported_even_when_it_cannot_be_read() {
    // A path missing from the stream gets its index row deleted, so
    // "couldn't process it" must still be reported as seen.
    let root = tmp_tree("unreadable-file");
    touch(&root.join("fine.txt"));
    let bad = root.join("bad.txt");
    touch(&bad);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();

        let files = walk(&root, &empty_db("unreadable-file"));
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).ok();

        assert_eq!(names(&files), vec!["bad.txt", "fine.txt"]);
        let bad_entry = files.iter().find(|f| f.path.ends_with("bad.txt")).unwrap();
        assert!(bad_entry.record.is_none(), "unopenable, so no record");
    }
    fs::remove_dir_all(&root).ok();
}

#[test]
fn unreadable_directory_is_recorded_not_silently_empty() {
    let root = tmp_tree("unreadable-dir");
    touch(&root.join("visible.txt"));
    let locked = root.join("locked");
    touch(&locked.join("inside.txt"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let mut w = walk_indexable_files(
            &[root.to_string_lossy().into_owned()],
            false,
            false,
            IgnoreSet::compile(&[]).unwrap(),
            empty_db("unreadable-dir").to_str().unwrap(),
            Config::default(),
            Arc::new(Registry::default_set()),
            Arc::new(AtomicBool::new(false)),
            4,
        );
        let files: Vec<WalkedFile> = w
            .by_ref()
            .filter_map(|e| match e {
                WalkEvent::File(f) => Some(f),
                WalkEvent::Stale(_) => None,
            })
            .collect();
        let recorded = !w.unreadable().is_empty();
        let covers = w
            .unreadable()
            .covers(locked.join("inside.txt").to_str().unwrap());

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

        assert_eq!(names(&files), vec!["visible.txt"]);
        assert!(recorded, "the failure must be recorded");
        assert!(covers, "so rows beneath it survive stale cleanup");
    }
    fs::remove_dir_all(&root).ok();
}

#[test]
#[cfg(unix)]
fn symlink_loop_terminates() {
    // A hand-rolled walker has none of walkdir's cycle detection; the
    // canonical-directory set is what stands in for it.
    let root = tmp_tree("loop");
    touch(&root.join("real.txt"));
    std::os::unix::fs::symlink(&root, root.join("self_link")).unwrap();

    let files = walk_with(&root, &empty_db("loop"), true, false);
    assert_eq!(names(&files), vec!["real.txt"], "the cycle is visited once");
    fs::remove_dir_all(&root).ok();
}

#[test]
#[cfg(unix)]
fn symlinked_file_resolves_to_its_target_path() {
    // The walk reaches this file twice — directly, and through the alias —
    // and reports it twice: the walker dedupes *directories*, while the
    // caller's `seen_paths` dedupes files. Both routes must agree on the
    // canonical path for that dedup to work.
    let root = tmp_tree("symlink-file");
    touch(&root.join("real/target.txt"));
    fs::create_dir_all(root.join("links")).unwrap();
    std::os::unix::fs::symlink(root.join("real/target.txt"), root.join("links/alias.txt")).unwrap();

    let files = walk_with(&root, &empty_db("symlink-file"), true, false);
    let paths: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    assert_eq!(paths.len(), 1, "both routes report one canonical path");

    let canonical = path_to_db_string(&root.join("real/target.txt").canonicalize().unwrap());
    assert_eq!(
        *paths.into_iter().next().unwrap(),
        canonical,
        "the target, not the alias"
    );

    // The alias itself is still reported, so its row is never mistaken for
    // deleted — it is reported under the *target's* path.
    assert_eq!(files.len(), 2, "seen twice, spelled once");
    assert!(files.iter().any(|f| f.aliased), "the link route is marked");
    fs::remove_dir_all(&root).ok();
}

/// With links off, file targets are not followed either: `filtered_walk`
/// follows neither kind, so a link followed only here would be re-indexed by
/// every full run and never updated between them.
#[test]
#[cfg(unix)]
fn a_file_symlink_is_not_followed_when_links_are_off() {
    let root = tmp_tree("symlink-off");
    touch(&root.join("real/target.txt"));
    fs::create_dir_all(root.join("links")).unwrap();
    std::os::unix::fs::symlink(root.join("real/target.txt"), root.join("links/alias.txt")).unwrap();
    // A target outside the walked tree: with links off it must not be
    // reachable at all.
    let outside = tmp_tree("symlink-off-outside");
    touch(&outside.join("elsewhere.txt"));
    std::os::unix::fs::symlink(outside.join("elsewhere.txt"), root.join("links/out.txt")).unwrap();

    let files = walk_with(&root, &empty_db("symlink-off"), false, false);
    assert_eq!(
        names(&files),
        vec!["target.txt"],
        "only the real file, reached directly"
    );
    assert!(!files.iter().any(|f| f.aliased), "nothing was resolved");

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&outside).ok();
}

/// Windows: `canonicalize` spells a junction's target `\\?\C:\…`, and the
/// walker must strip that before storing — otherwise full-path ignore
/// patterns never match beneath the junction and the canonical-directory
/// dedup fails.
#[test]
#[cfg(windows)]
fn a_followed_junction_stores_plain_paths() {
    let root = tmp_tree("junction");
    touch(&root.join("real").join("target.txt"));
    // Junctions need no privileges, unlike symlinks; still, skip cleanly
    // on filesystems where mklink refuses.
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
        fs::remove_dir_all(&root).ok();
        return;
    }

    let files = walk_with(&root, &empty_db("junction"), true, false);
    for f in &files {
        assert!(
            !f.path.starts_with(r"\\?\"),
            "stored path leaked a verbatim prefix: {}",
            f.path
        );
    }
    // A leaked prefix would spell the directory twice and report the file
    // twice.
    assert_eq!(names(&files), vec!["target.txt"], "visited once");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn hidden_and_ignored_entries_are_pruned() {
    let root = tmp_tree("prune");
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/keep2.txt"));
    touch(&root.join("sub/skip.tmp"));
    touch(&root.join(".hidden/inside.txt"));
    touch(&root.join(".dotfile"));
    touch(&root.join("node_modules/dep/index.js"));

    let ignore = IgnoreSet::compile(&["*.tmp".to_string(), "node_modules".to_string()]).unwrap();
    let files: Vec<WalkedFile> = files_only(walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        ignore,
        empty_db("prune").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ));
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    let files = walk_with(&root, &empty_db("prune-hidden"), false, true);
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
    fs::remove_dir_all(&root).ok();
}

/// The counters behind the one-line summary a run logs: a pruned *directory*
/// costs one increment, not one per file beneath it.
#[test]
fn pruned_entries_are_counted_by_reason() {
    let root = tmp_tree("prune-counts");
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/keep2.txt"));
    touch(&root.join("sub/skip.tmp"));
    // Two files below, one prune.
    touch(&root.join(".hidden/inside.txt"));
    touch(&root.join(".hidden/also-inside.txt"));
    touch(&root.join(".dotfile"));
    // Three levels below, still one prune.
    touch(&root.join("node_modules/dep/lib/index.js"));

    let ignore = IgnoreSet::compile(&["*.tmp".to_string(), "node_modules".to_string()]).unwrap();
    let mut walk = walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        ignore,
        empty_db("prune-counts").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    );
    let files: Vec<WalkedFile> = (&mut walk)
        .filter_map(|e| match e {
            WalkEvent::File(f) => Some(f),
            WalkEvent::Stale(_) => None,
        })
        .collect();
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    let pruned = walk.pruned();
    assert_eq!(
        pruned.dot_named.load(Ordering::Relaxed),
        2,
        "`.hidden` and `.dotfile` — not the two files inside `.hidden`"
    );
    assert_eq!(
        pruned.ignored.load(Ordering::Relaxed),
        2,
        "`skip.tmp` and `node_modules` — not `index.js` three levels down"
    );
    // Attributes are a Windows concept; on Linux nothing can reach this
    // counter, and on Windows a temp tree carries no Hidden bit.
    assert_eq!(pruned.attribute.load(Ordering::Relaxed), 0);
    assert_eq!(pruned.total(), 4);

    let summary = pruned.summary().expect("something was pruned");
    assert!(summary.contains("4 entries"), "{}", summary);

    fs::remove_dir_all(&root).ok();
}

/// A clean tree adds no summary line to the log.
#[test]
fn a_tree_with_nothing_pruned_reports_no_summary() {
    let root = tmp_tree("prune-none");
    touch(&root.join("keep.txt"));
    touch(&root.join("sub/keep2.txt"));

    let mut walk = walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        empty_db("prune-none").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    );
    let files: Vec<WalkedFile> = (&mut walk)
        .filter_map(|e| match e {
            WalkEvent::File(f) => Some(f),
            WalkEvent::Stale(_) => None,
        })
        .collect();
    assert_eq!(names(&files), vec!["keep.txt", "keep2.txt"]);

    assert_eq!(walk.pruned().total(), 0);
    assert!(walk.pruned().summary().is_none());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_directory_reports_rows_with_no_file_behind_them() {
    // The per-directory diff, at the level it is computed: one listing
    // against one directory's rows, before any splitting.
    let root = tmp_tree("reconcile");
    touch(&root.join("kept.txt"));
    touch(&root.join("sub/nested.txt"));

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

    let mut stale = stale_only(walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        db.to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ));
    stale.sort();

    let mut want = vec![gone, gone_nested];
    want.sort();
    assert_eq!(
        stale, want,
        "exactly the rows with no file, from both directories"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
#[cfg(unix)]
fn an_unreadable_directory_reports_nothing_stale() {
    use std::os::unix::fs::PermissionsExt;

    // A failed read leaves an empty listing, which must never be diffed:
    // every row under it would look deleted.
    let root = tmp_tree("reconcile-locked");
    let locked = root.join("locked");
    touch(&locked.join("inside.txt"));

    let db = db_with(
        "reconcile-locked",
        &[(path_to_db_string(&locked.join("inside.txt")), 1)],
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let stale = stale_only(walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        db.to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).ok();

    assert!(
        stale.is_empty(),
        "an unreadable directory is not an empty one"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn hidden_root_is_still_walked() {
    // Roots are chosen explicitly, so the hidden rule must not silence one.
    let base = tmp_tree("hidden-root");
    let root = base.join(".config");
    touch(&root.join("app.conf"));

    let files = walk(&root, &empty_db("hidden-root"));
    assert_eq!(names(&files), vec!["app.conf"]);
    fs::remove_dir_all(&base).ok();
}

#[test]
fn stop_flag_ends_the_walk_without_hanging() {
    let root = tmp_tree("stop");
    for i in 0..500 {
        touch(&root.join(format!("f{:04}.txt", i)));
    }

    let stop = Arc::new(AtomicBool::new(true));
    let files: Vec<WalkedFile> = files_only(walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        empty_db("stop").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        stop,
        4,
    ));

    assert!(
        files.len() < 500,
        "an already-stopped walk does not run to completion"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn dropping_the_walk_early_does_not_hang() {
    // Workers blocked in `send` must be released by the receiver going
    // away, or `Drop` would join threads that never wake.
    let root = tmp_tree("early-drop");
    for i in 0..2000 {
        touch(&root.join(format!("sub{}/f{}.txt", i % 10, i)));
    }

    let mut w = walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        empty_db("early-drop").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    );
    assert!(w.next().is_some());
    drop(w); // must return, not deadlock

    fs::remove_dir_all(&root).ok();
}

#[test]
fn overlapping_roots_yield_each_file_once() {
    let root = tmp_tree("overlap");
    touch(&root.join("sub/a.txt"));

    let files: Vec<WalkedFile> = files_only(walk_indexable_files(
        &[
            root.to_string_lossy().into_owned(),
            root.join("sub").to_string_lossy().into_owned(),
        ],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        empty_db("overlap").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    ));

    assert_eq!(files.len(), 1, "the nested root must not double-index");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn repeated_walks_agree_on_the_result_set() {
    // The termination protocol is racy by nature; run it enough times
    // under contention that a premature exit would show up.
    let root = tmp_tree("repeat");
    for i in 0..40 {
        touch(&root.join(format!("d{}/f{}.txt", i % 7, i)));
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
    fs::remove_dir_all(&root).ok();
}

#[test]
fn finish_reports_a_clean_walk_and_is_idempotent() {
    // The caller gates stale-row deletion on this: a walk whose workers
    // died yields a partial file set, and "not seen" would otherwise be
    // read as "deleted".
    let root = tmp_tree("finish");
    touch(&root.join("a.txt"));
    touch(&root.join("sub/b.txt"));

    let mut w = walk_indexable_files(
        &[root.to_string_lossy().into_owned()],
        false,
        false,
        IgnoreSet::compile(&[]).unwrap(),
        empty_db("finish").to_str().unwrap(),
        Config::default(),
        Arc::new(Registry::default_set()),
        Arc::new(AtomicBool::new(false)),
        4,
    );
    let files: Vec<WalkedFile> = w
        .by_ref()
        .filter_map(|e| match e {
            WalkEvent::File(f) => Some(f),
            WalkEvent::Stale(_) => None,
        })
        .collect();
    assert_eq!(files.len(), 2);
    assert!(w.finish(), "no worker panicked");
    // Drop calls it again; joining an already-drained handle list must be
    // a no-op rather than a panic.
    assert!(w.finish());
    drop(w);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn local_temp_dir_is_not_detected_as_network() {
    let root = tmp_tree("fstype");
    assert_eq!(
        thread_count_for(&[root.to_string_lossy().into_owned()]),
        LOCAL_THREADS
    );
    fs::remove_dir_all(&root).ok();
}
