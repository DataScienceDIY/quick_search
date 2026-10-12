use super::*;
use crate::testutil::eventually;
use std::sync::Mutex as StdMutex;

/// How long a positive waits for the watcher: an event or a registration
/// lands in milliseconds, but a loaded machine can starve the loop for long.
const PATIENCE: Duration = Duration::from_secs(20);

fn sink_to_vec() -> (EventSink, Arc<StdMutex<Vec<FsEvent>>>) {
    let v: Arc<StdMutex<Vec<FsEvent>>> = Arc::new(StdMutex::new(Vec::new()));
    let v_clone = v.clone();
    let s: EventSink = Arc::new(move |e| v_clone.lock().unwrap().push(e));
    (s, v)
}

/// Whether the sink receives an event `accept` takes within [`PATIENCE`].
fn receives(got: &StdMutex<Vec<FsEvent>>, accept: impl Fn(&FsEvent) -> bool) -> bool {
    eventually(PATIENCE, || got.lock().unwrap().iter().any(&accept))
}

/// A `Create` or a `Modify` of `path`: what a write reports, either or both
/// depending on the backend.
fn wrote(path: &Path) -> impl Fn(&FsEvent) -> bool + '_ {
    move |e: &FsEvent| matches!(e, FsEvent::Create(p) | FsEvent::Modify(p) if p == path)
}

/// A `Create` of `path`.
fn created(path: &Path) -> impl Fn(&FsEvent) -> bool + '_ {
    move |e: &FsEvent| matches!(e, FsEvent::Create(p) if p == path)
}

/// The watched-directory count once it moves off `from`, or `from` if it has
/// not within [`PATIENCE`]: for a change that is one event, whose
/// registrations or releases land in one store.
fn dirs_after(w: &Watcher, from: usize) -> usize {
    eventually(PATIENCE, || w.watched_dirs() != from);
    w.watched_dirs()
}

fn default_filters() -> WatchFilters {
    WatchFilters {
        include_hidden: false,
        ignore: Arc::new(
            IgnoreSet::compile(&[".git".to_string(), "node_modules".to_string()]).unwrap(),
        ),
    }
}

/// The default config with a directory budget of `n`.
fn capped(n: usize) -> WatcherConfig {
    WatcherConfig {
        max_watched_dirs: n,
        ..WatcherConfig::default()
    }
}

/// A cancel flag nobody sets.
fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Start a watcher on `dir` alone, with the default filters.
fn watch_with(dir: &Path, config: WatcherConfig, sink: EventSink) -> Result<Watcher, WatchError> {
    Watcher::start(
        vec![dir.to_path_buf()],
        default_filters(),
        config,
        sink,
        no_cancel(),
    )
}

/// [`watch_with`] at the defaults, into a sink nobody reads.
fn watch(dir: &Path) -> Watcher {
    watch_with(dir, WatcherConfig::default(), sink_to_vec().0).unwrap()
}

/// Seeds `dirs` directly so set logic is tested without real kernel watches.
fn registry_with(dirs: &[&str]) -> WatchRegistry {
    let raw = RecommendedWatcher::new(|_res| {}, NotifyConfig::default()).unwrap();
    WatchRegistry {
        raw,
        dirs: dirs.iter().map(PathBuf::from).collect(),
        cap: 64,
    }
}

#[test]
fn remove_tree_skips_the_scan_for_a_path_that_is_not_watched() {
    let mut reg = registry_with(&["/a/b", "/a/b/c", "/a/bc"]);

    assert_eq!(reg.remove_tree(Path::new("/a/b/file.txt")), 0);
    assert_eq!(reg.dirs.len(), 3, "the watch set is untouched");

    assert_eq!(reg.remove_tree(Path::new("/elsewhere")), 0);
    assert_eq!(reg.dirs.len(), 3);

    // /a/bc is a name-prefix sibling, not a child.
    assert_eq!(reg.remove_tree(Path::new("/a/b")), 2);
    assert_eq!(
        reg.dirs.iter().collect::<Vec<_>>(),
        vec![&PathBuf::from("/a/bc")]
    );
}

/// Nothing holds an event back: a directory's second change reaches the
/// sink as promptly as its first. A per-directory throttle once held it for
/// 30 s; the coordinator's queue does the debouncing now.
#[test]
fn every_event_reaches_the_sink_as_it_happens() {
    let dir = crate::testutil::scratch_dir("prompt");
    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();

    let (first, second_file) = (dir.join("first.txt"), dir.join("second.txt"));
    std::fs::write(&first, "a").unwrap();
    assert!(receives(&got, wrote(&first)));
    let second = std::time::Instant::now();
    std::fs::write(&second_file, "b").unwrap();
    assert!(receives(&got, wrote(&second_file)));
    assert!(
        second.elapsed() < Duration::from_secs(2),
        "the second event in a directory took {:?}",
        second.elapsed()
    );

    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn e2e_create_modify_remove_surfaces() {
    let dir = crate::testutil::scratch_dir("e2e");
    let (sink, got) = sink_to_vec();

    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();

    let f = dir.join("hello.txt");
    std::fs::write(&f, "hi").unwrap();
    // Some backends emit Create+Modify for `write`, so accept either.
    let has_create_or_modify = receives(&got, wrote(&f));
    std::fs::write(&f, "hi again").unwrap();
    std::fs::remove_file(&f).unwrap();
    let has_remove = receives(&got, |e| matches!(e, FsEvent::Remove(p) if p == &f));

    w.stop();

    let events = got.lock().unwrap().clone();
    assert!(has_create_or_modify, "no create/modify in {:?}", events);
    assert!(has_remove, "no remove in {:?}", events);

    std::fs::remove_dir_all(&dir).ok();
}

/// An event that got through would be applied to whichever different file
/// owns the lossy spelling (see `path_to_db_string`). The control file rules
/// out a watcher that simply saw nothing.
#[test]
fn events_for_an_unrepresentable_name_never_surface() {
    let dir = crate::testutil::scratch_dir("e2e-unrepresentable");
    let bad = dir.join(crate::testutil::unrepresentable_name("report", ".txt"));
    if std::fs::write(&bad, "hi").is_err() {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    std::fs::remove_file(&bad).unwrap();

    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();

    std::fs::write(&bad, "hi").unwrap();
    std::fs::remove_file(&bad).unwrap();
    let good = dir.join("ordinary.txt");
    std::fs::write(&good, "hi").unwrap();
    // Events arrive in order: once the control's is in, any for the bad name
    // would be too.
    let control = receives(&got, wrote(&good));

    w.stop();

    let events = got.lock().unwrap().clone();
    assert!(
        control,
        "the control file produced no event, so this test proves nothing: {:?}",
        events
    );
    let leaked: Vec<&FsEvent> = events
        .iter()
        .filter(|e| {
            let paths: Vec<&PathBuf> = match e {
                FsEvent::Create(p) | FsEvent::Modify(p) | FsEvent::Remove(p) => vec![p],
                FsEvent::Rename { from, to } => vec![from, to],
            };
            paths.iter().any(|p| p.to_str().is_none())
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "an unrepresentable path reached the sink: {:?}",
        leaked
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ignored_and_hidden_dirs_are_not_registered() {
    let dir = crate::testutil::scratch_dir("filter");
    for sub in [
        "keep",
        "keep/nested",
        ".git",
        ".git/objects",
        "node_modules",
        "node_modules/pkg",
        ".hidden",
    ] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }

    let w = watch(&dir);

    assert_eq!(w.watched_dirs(), 3, "expected root, keep, keep/nested only");
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn include_hidden_registers_dotted_dirs() {
    let dir = crate::testutil::scratch_dir("hidden");
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();

    let filters = WatchFilters {
        include_hidden: true,
        ..default_filters()
    };
    let w = Watcher::start(
        vec![dir.clone()],
        filters,
        WatcherConfig::default(),
        sink_to_vec().0,
        no_cancel(),
    )
    .unwrap();

    assert_eq!(w.watched_dirs(), 2, "root + .hidden");
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exceeding_the_cap_fails_all_or_nothing() {
    let dir = crate::testutil::scratch_dir("cap");
    for sub in ["a", "b", "c", "d"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }

    let err = watch_with(&dir, capped(2), sink_to_vec().0).unwrap_err();

    assert_eq!(
        err,
        WatchError::TooManyDirectories { dirs: 2, cap: 2 },
        "5 directories under a cap of 2 must fail"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[cfg(unix)]
fn an_unreadable_directory_is_skipped_not_fatal() {
    let dir = crate::testutil::scratch_dir("denied");
    std::fs::create_dir_all(dir.join("open")).unwrap();
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    crate::platform::deny_read(&locked).unwrap();

    let started = watch_with(&dir, WatcherConfig::default(), sink_to_vec().0);
    crate::platform::restore_read(&locked).ok();
    let w = started.expect("an unreadable directory must not fail the watcher");

    assert_eq!(w.watched_dirs(), 2, "root + open; locked is skipped");
    assert_eq!(w.degraded_reason(), None);
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[cfg(unix)]
fn the_cap_outranks_an_unreadable_directory() {
    let dir = crate::testutil::scratch_dir("denied-cap");
    for sub in ["a", "b", "c", "d"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    crate::platform::deny_read(&locked).unwrap();

    let started = watch_with(&dir, capped(2), sink_to_vec().0);
    crate::platform::restore_read(&locked).ok();

    assert_eq!(
        started.unwrap_err(),
        WatchError::TooManyDirectories { dirs: 2, cap: 2 },
        "the reported reason must be the cap, not the unreadable folder"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_tree_inside_the_cap_registers() {
    let dir = crate::testutil::scratch_dir("undercap");
    std::fs::create_dir_all(dir.join("a")).unwrap();

    let w = watch_with(&dir, capped(2), sink_to_vec().0).unwrap();
    assert_eq!(w.watched_dirs(), 2);
    assert_eq!(w.degraded_reason(), None);
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_directory_created_after_start_is_watched() {
    let dir = crate::testutil::scratch_dir("newdir");
    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();
    assert_eq!(w.watched_dirs(), 1, "only the root to begin with");

    let sub = dir.join("later");
    std::fs::create_dir(&sub).unwrap();
    assert_eq!(dirs_after(&w, 1), 2, "the new directory must be watched");

    let f = sub.join("inside.txt");
    std::fs::write(&f, "hi").unwrap();
    let seen = receives(&got, wrote(&f));
    w.stop();

    assert!(
        seen,
        "no event for the file in the new directory: {:?}",
        got.lock().unwrap()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_moved_in_tree_registers_every_directory() {
    let staging = crate::testutil::scratch_dir("staging");
    let dir = crate::testutil::scratch_dir("movein");
    std::fs::create_dir_all(staging.join("tree/one/two")).unwrap();

    let mut w = watch(&dir);
    assert_eq!(w.watched_dirs(), 1);

    std::fs::rename(staging.join("tree"), dir.join("tree")).unwrap();

    assert_eq!(
        dirs_after(&w, 1),
        4,
        "root + tree + tree/one + tree/one/two"
    );
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&staging).ok();
}

#[test]
fn a_removed_directory_releases_its_watches() {
    let dir = crate::testutil::scratch_dir("rmdir");
    std::fs::create_dir_all(dir.join("gone/deep")).unwrap();

    let mut w = watch(&dir);
    assert_eq!(w.watched_dirs(), 3, "root + gone + gone/deep");

    std::fs::remove_dir_all(dir.join("gone")).unwrap();
    // Two events, `deep`'s and then `gone`'s.
    eventually(PATIENCE, || w.watched_dirs() == 1);

    assert_eq!(w.watched_dirs(), 1, "descendants released with the parent");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// `Path::starts_with` compares components, not byte prefixes.
#[test]
fn remove_tree_does_not_match_name_prefixes() {
    let dir = crate::testutil::scratch_dir("prefix");
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::create_dir_all(dir.join("bc")).unwrap();

    let mut w = watch(&dir);
    assert_eq!(w.watched_dirs(), 3);

    std::fs::remove_dir_all(dir.join("b")).unwrap();

    assert_eq!(dirs_after(&w, 3), 2, "root + bc; /a/bc is not under /a/b");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn running_out_of_budget_later_records_the_reason() {
    let dir = crate::testutil::scratch_dir("degrade");
    let mut w = watch_with(&dir, capped(2), sink_to_vec().0).unwrap();
    assert_eq!(
        w.degraded_reason(),
        None,
        "one directory is under the cap of 2"
    );

    std::fs::create_dir(dir.join("fits")).unwrap();
    assert_eq!(dirs_after(&w, 1), 2, "the first new directory fits");
    std::fs::create_dir(dir.join("overflows")).unwrap();
    eventually(PATIENCE, || w.degraded_reason().is_some());

    assert_eq!(
        w.degraded_reason(),
        Some(WatchError::TooManyDirectories { dirs: 2, cap: 2 }),
        "the cap, not a kernel limit"
    );
    assert_eq!(w.watched_dirs(), 2, "never registers past the cap");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// A superseded start stops at its first directory; the error drops the
/// registry, and with it every watch taken.
#[test]
fn a_cancelled_registration_registers_nothing() {
    let dir = crate::testutil::scratch_dir("cancelled");
    std::fs::create_dir_all(dir.join("sub")).unwrap();

    let started = Watcher::start(
        vec![dir.clone()],
        default_filters(),
        WatcherConfig::default(),
        sink_to_vec().0,
        Arc::new(AtomicBool::new(true)),
    );

    assert!(
        matches!(started, Err(WatchError::Other(_))),
        "a cancelled start must fail, got {:?}",
        started
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The `None` a stop sends queues behind every event already forwarded; the
/// flag ends the loop at the next one instead. Counted, not timed: the sink
/// holds the loop on its first event while the backlog queues, and lets go
/// only once the stop has set the flag.
#[test]
fn stop_does_not_wait_out_the_event_backlog() {
    const FILES: usize = 2_000;
    let dir = crate::testutil::scratch_dir("backlog");
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = StdMutex::new(release_rx);
    let delivered = Arc::new(AtomicUsize::new(0));
    let count = delivered.clone();
    let sink: EventSink = Arc::new(move |_: FsEvent| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = entered_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        }
    });
    let cancel = no_cancel();
    let mut w = Watcher::start(
        vec![dir.clone()],
        default_filters(),
        WatcherConfig::default(),
        sink,
        cancel.clone(),
    )
    .unwrap();

    std::fs::write(dir.join("first.txt"), "a").unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the first event never reached the sink");
    for i in 0..FILES {
        std::fs::write(dir.join(format!("{}.txt", i)), "a").unwrap();
    }
    let stopper = thread::spawn(move || w.stop());
    while !cancel.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }
    release_tx.send(()).unwrap();
    stopper.join().unwrap();

    let delivered = delivered.load(Ordering::SeqCst);
    assert!(
        delivered < FILES / 10,
        "the stop waited out the backlog: {} events after it",
        delivered
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A root's whole subtree shares one change buffer on Windows. A burst that
/// outgrows it must be reported, not dropped: either every `Create` arrived
/// or the overflow was recorded. A machine that keeps up passes without one.
#[test]
#[cfg(windows)]
fn a_burst_larger_than_the_change_buffer_is_reported_not_dropped() {
    let dir = crate::testutil::scratch_dir("burst");
    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();

    // 140 bytes of change record per name: 5,000 fill the 16 KiB buffer
    // forty times over.
    let files: Vec<PathBuf> = (0..5_000)
        .map(|i| dir.join(format!("{:060}.txt", i)))
        .collect();
    for f in &files {
        std::fs::write(f, "").unwrap();
    }
    let all_created = || {
        let got = got.lock().unwrap();
        let created: std::collections::HashSet<&PathBuf> = got
            .iter()
            .filter_map(|e| match e {
                FsEvent::Create(p) => Some(p),
                _ => None,
            })
            .collect();
        files.iter().all(|f| created.contains(f))
    };
    let overflowed = |w: &Watcher| w.degraded_reason() == Some(WatchError::Overflowed);
    eventually(Duration::from_secs(30), || all_created() || overflowed(&w));
    let (complete, reported) = (all_created(), overflowed(&w));
    w.stop();

    assert!(
        complete || reported,
        "part of the burst was lost and no overflow was reported"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_created_ignored_directory_is_not_watched() {
    let dir = crate::testutil::scratch_dir("newignored");
    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();

    std::fs::create_dir(dir.join("node_modules")).unwrap();
    std::fs::create_dir(dir.join(".cache")).unwrap();
    // The control. Events are handled in order, and each reaches the sink
    // after its registration, so once the control's arrives the two above
    // have been judged.
    let visible = dir.join("visible");
    std::fs::create_dir(&visible).unwrap();
    assert!(receives(&got, created(&visible)));

    // A recursive root watch registers nothing per directory.
    let want = if crate::platform::WATCH_ROOTS_RECURSIVELY {
        1
    } else {
        2
    };
    assert_eq!(
        w.watched_dirs(),
        want,
        "only the control counts: neither ignored nor hidden dirs do"
    );
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// Names are tested below the root only, as the walk tests them: a root
/// inside `node_modules` gets watches for its new directories, and their
/// events reach the sink.
#[test]
fn an_event_under_a_root_nested_in_node_modules_is_forwarded() {
    let base = crate::testutil::scratch_dir("nested-root");
    let root = base.join("node_modules").join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&root, WatcherConfig::default(), sink).unwrap();

    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let want_dirs = if crate::platform::WATCH_ROOTS_RECURSIVELY {
        1
    } else {
        2
    };
    eventually(PATIENCE, || w.watched_dirs() >= want_dirs);
    assert_eq!(
        w.watched_dirs(),
        want_dirs,
        "the new directory is unwatched"
    );

    let f = sub.join("inside.txt");
    std::fs::write(&f, "hi").unwrap();
    let seen = receives(&got, wrote(&f));
    w.stop();

    assert!(
        seen,
        "no event under the nested root: {:?}",
        got.lock().unwrap()
    );
    std::fs::remove_dir_all(&base).ok();
}

/// On Linux the watch budget is a shared kernel resource; none of it is spent
/// on a subtree the indexer discards, at startup or when a link appears later.
#[test]
#[cfg(unix)]
fn a_symlinked_directory_is_never_registered() {
    let dir = crate::testutil::scratch_dir("symlink-reg");
    std::fs::create_dir_all(dir.join("real/nested")).unwrap();
    let outside = crate::testutil::scratch_dir("symlink-reg-target");
    std::fs::create_dir_all(outside.join("deep")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

    let (sink, got) = sink_to_vec();
    let mut w = watch_with(&dir, WatcherConfig::default(), sink).unwrap();
    assert_eq!(
        w.watched_dirs(),
        3,
        "the symlinked directory must not be registered"
    );

    std::os::unix::fs::symlink(&outside, dir.join("later")).unwrap();
    // The control: once its event arrives, the link's has been judged.
    let control = dir.join("control");
    std::fs::create_dir(&control).unwrap();
    assert!(receives(&got, created(&control)));
    assert_eq!(
        w.watched_dirs(),
        4,
        "nor one linked in after start: only the control counts"
    );

    w.stop();
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// The user chose the root, so a root that is itself a link is watched like
/// the walk that indexes it, and its events reach the sink.
#[test]
#[cfg(unix)]
fn a_symlinked_root_is_watched() {
    let base = crate::testutil::scratch_dir("symlink-root");
    std::fs::create_dir_all(base.join("real/sub")).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(base.join("real"), &link).unwrap();
    let (sink, got) = sink_to_vec();

    let mut w = watch_with(&link, WatcherConfig::default(), sink).unwrap();
    assert_eq!(w.watched_dirs(), 2, "the link root and its subdirectory");

    std::fs::write(base.join("real/sub/inside.txt"), "hi").unwrap();
    let seen = receives(&got, wrote(&link.join("sub/inside.txt")));
    w.stop();

    assert!(
        seen,
        "no event under the link root: {:?}",
        got.lock().unwrap()
    );
    std::fs::remove_dir_all(&base).ok();
}

/// FSEvents reports every rename as `Name(Any)`, naming neither end. Read
/// as a `Modify`, a directory moved into a root was never walked, and a
/// file moved out kept its row; the disk says which end each path is.
#[test]
fn a_rename_that_names_no_end_is_judged_by_the_disk() {
    use notify::event::{AccessKind, CreateKind, DataChange, RemoveKind};
    let dir = crate::testutil::scratch_dir("op-for");
    let moved_in = dir.join("moved-in");
    std::fs::create_dir(&moved_in).unwrap();
    let gone = dir.join("moved-away.txt");
    let name = |mode| EventKind::Modify(ModifyKind::Name(mode));

    for mode in [RenameMode::Any, RenameMode::Other] {
        assert_eq!(op_for(&name(mode), &moved_in), Some(Op::Create), "{mode:?}");
        assert_eq!(op_for(&name(mode), &gone), Some(Op::Remove), "{mode:?}");
    }
    // The ends a backend does name are taken at its word, whatever the disk
    // says by the time the event is read.
    for path in [&moved_in, &gone] {
        assert_eq!(op_for(&name(RenameMode::To), path), Some(Op::Create));
        assert_eq!(op_for(&name(RenameMode::From), path), Some(Op::Remove));
        assert_eq!(
            op_for(&EventKind::Create(CreateKind::Any), path),
            Some(Op::Create)
        );
        assert_eq!(
            op_for(&EventKind::Remove(RemoveKind::Any), path),
            Some(Op::Remove)
        );
        assert_eq!(
            op_for(&EventKind::Modify(ModifyKind::Data(DataChange::Any)), path),
            Some(Op::Modify)
        );
        // A `Both` without its two paths is a change to one of them.
        assert_eq!(op_for(&name(RenameMode::Both), path), Some(Op::Modify));
        assert_eq!(op_for(&EventKind::Access(AccessKind::Any), path), None);
        assert_eq!(op_for(&EventKind::Any, path), None);
    }
    std::fs::remove_dir_all(&dir).ok();
}
