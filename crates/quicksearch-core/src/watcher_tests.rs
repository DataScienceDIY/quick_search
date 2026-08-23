use super::*;
use std::sync::Mutex as StdMutex;

fn sink_to_vec() -> (EventSink, Arc<StdMutex<Vec<FsEvent>>>) {
    let v: Arc<StdMutex<Vec<FsEvent>>> = Arc::new(StdMutex::new(Vec::new()));
    let v_clone = v.clone();
    let s: EventSink = Arc::new(move |e| v_clone.lock().unwrap().push(e));
    (s, v)
}

fn default_filters() -> WatchFilters {
    WatchFilters {
        include_hidden: false,
        follow_symlinks: false,
        ignore: Arc::new(
            IgnoreSet::compile(&[".git".to_string(), "node_modules".to_string()]).unwrap(),
        ),
    }
}

fn fast_config() -> WatcherConfig {
    WatcherConfig {
        throttle_window: Duration::from_millis(50),
        tick_interval: Duration::from_millis(20),
        ..WatcherConfig::default()
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    crate::testutil::scratch_dir(tag)
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

#[test]
fn enqueue_create_then_remove_cancels() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    let p = PathBuf::from("/tmp/a.txt");
    enqueue(&mut map, p.clone(), QueuedOp::Create);
    enqueue(&mut map, p.clone(), QueuedOp::Remove);
    let entry = map.get(p.parent().unwrap()).unwrap();
    assert!(entry.queue.is_empty(), "Create then Remove should cancel");
}

#[test]
fn modify_after_modify_is_one() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    let p = PathBuf::from("/tmp/a.txt");
    enqueue(&mut map, p.clone(), QueuedOp::Modify);
    enqueue(&mut map, p.clone(), QueuedOp::Modify);
    let entry = map.get(p.parent().unwrap()).unwrap();
    assert_eq!(entry.queue.len(), 1);
}

#[test]
fn flush_ready_leading_edge_fires_immediately() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    enqueue(&mut map, PathBuf::from("/tmp/a.txt"), QueuedOp::Create);
    let (sink, got) = sink_to_vec();
    let config = WatcherConfig::default();
    flush_ready(&mut map, &sink, &config);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0], FsEvent::Create(_)));
}

#[test]
fn flush_ready_respects_max_dirs_per_tick() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    for i in 0..10 {
        enqueue(
            &mut map,
            PathBuf::from(format!("/dir{}/a", i)),
            QueuedOp::Create,
        );
    }
    let (sink, got) = sink_to_vec();
    let config = WatcherConfig {
        max_dirs_per_tick: 3,
        ..WatcherConfig::default()
    };
    flush_ready(&mut map, &sink, &config);
    // Each dir contributes one event because each entry has one path.
    assert_eq!(got.lock().unwrap().len(), 3);
}

#[test]
fn prune_stale_drops_empty_old_entries() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    map.insert(
        PathBuf::from("/tmp"),
        DirThrottleEntry {
            record_time: Instant::now() - Duration::from_secs(3600),
            queue: HashMap::new(),
            immediate: false,
        },
    );
    prune_stale(&mut map, Duration::from_secs(1));
    assert!(map.is_empty());
}

#[test]
fn prune_stale_keeps_active_entries() {
    let mut map: HashMap<PathBuf, DirThrottleEntry> = HashMap::new();
    let mut queue = HashMap::new();
    queue.insert(PathBuf::from("/tmp/a"), QueuedOp::Modify);
    map.insert(
        PathBuf::from("/tmp"),
        DirThrottleEntry {
            record_time: Instant::now() - Duration::from_secs(3600),
            queue,
            immediate: false,
        },
    );
    prune_stale(&mut map, Duration::from_secs(1));
    assert_eq!(map.len(), 1);
}

#[test]
fn e2e_create_modify_remove_surfaces() {
    let dir = tmp_dir("e2e");
    let (sink, got) = sink_to_vec();

    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink,
    )
    .unwrap();

    let f = dir.join("hello.txt");
    std::fs::write(&f, "hi").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    std::fs::write(&f, "hi again").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    std::fs::remove_file(&f).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    w.stop();

    let events = got.lock().unwrap().clone();
    // Some backends emit Create+Modify for `write`, so accept either.
    let has_create_or_modify = events
        .iter()
        .any(|e| matches!(e, FsEvent::Create(_) | FsEvent::Modify(_)));
    let has_remove = events.iter().any(|e| matches!(e, FsEvent::Remove(_)));
    assert!(has_create_or_modify, "no create/modify in {:?}", events);
    assert!(has_remove, "no remove in {:?}", events);

    std::fs::remove_dir_all(&dir).ok();
}

/// The incremental side keys on lossy `path_to_db_string`, so an event that
/// got through would be applied to whichever *different* file owns the lossy
/// spelling. The control file rules out a watcher that simply saw nothing.
#[test]
fn events_for_an_unrepresentable_name_never_surface() {
    let dir = tmp_dir("e2e-unrepresentable");
    let bad = dir.join(crate::testutil::unrepresentable_name("report", ".txt"));
    if std::fs::write(&bad, "hi").is_err() {
        eprintln!("skipped: this filesystem will not store an unrepresentable name");
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    std::fs::remove_file(&bad).unwrap();

    let (sink, got) = sink_to_vec();
    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink,
    )
    .unwrap();

    std::fs::write(&bad, "hi").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    std::fs::remove_file(&bad).unwrap();
    let good = dir.join("ordinary.txt");
    std::fs::write(&good, "hi").unwrap();
    std::thread::sleep(Duration::from_millis(250));

    w.stop();

    let events = got.lock().unwrap().clone();
    assert!(
        events.iter().any(|e| matches!(
            e,
            FsEvent::Create(p) | FsEvent::Modify(p) if p == &good
        )),
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
    let dir = tmp_dir("filter");
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

    let w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();

    assert_eq!(w.watched_dirs(), 3, "expected root, keep, keep/nested only");
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn include_hidden_registers_dotted_dirs() {
    let dir = tmp_dir("hidden");
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();

    let filters = WatchFilters {
        include_hidden: true,
        ..default_filters()
    };
    let w = Watcher::start(
        std::iter::once(&dir),
        filters,
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();

    assert_eq!(w.watched_dirs(), 2, "root + .hidden");
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exceeding_the_cap_fails_all_or_nothing() {
    let dir = tmp_dir("cap");
    for sub in ["a", "b", "c", "d"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }

    let config = WatcherConfig {
        max_watched_dirs: 2,
        ..fast_config()
    };
    let err = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        config,
        sink_to_vec().0,
    )
    .unwrap_err();

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
    let dir = tmp_dir("denied");
    std::fs::create_dir_all(dir.join("open")).unwrap();
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    crate::platform::deny_read(&locked).unwrap();

    let started = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    );
    crate::platform::restore_read(&locked).ok();
    let w = started.expect("an unreadable directory must not fail the watcher");

    assert_eq!(w.watched_dirs(), 2, "root + open; locked is skipped");
    assert!(!w.is_degraded());
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[cfg(unix)]
fn the_cap_outranks_an_unreadable_directory() {
    let dir = tmp_dir("denied-cap");
    for sub in ["a", "b", "c", "d"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    crate::platform::deny_read(&locked).unwrap();

    let config = WatcherConfig {
        max_watched_dirs: 2,
        ..fast_config()
    };
    let started = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        config,
        sink_to_vec().0,
    );
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
    let dir = tmp_dir("undercap");
    std::fs::create_dir_all(dir.join("a")).unwrap();

    let config = WatcherConfig {
        max_watched_dirs: 2,
        ..fast_config()
    };
    let w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        config,
        sink_to_vec().0,
    )
    .unwrap();
    assert_eq!(w.watched_dirs(), 2);
    assert!(!w.is_degraded());
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_directory_created_after_start_is_watched() {
    let dir = tmp_dir("newdir");
    let (sink, got) = sink_to_vec();
    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink,
    )
    .unwrap();
    assert_eq!(w.watched_dirs(), 1, "only the root to begin with");

    let sub = dir.join("later");
    std::fs::create_dir(&sub).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(w.watched_dirs(), 2, "the new directory must be watched");

    let f = sub.join("inside.txt");
    std::fs::write(&f, "hi").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    w.stop();

    let events = got.lock().unwrap().clone();
    assert!(
        events.iter().any(|e| matches!(
            e,
            FsEvent::Create(p) | FsEvent::Modify(p) if p == &f
        )),
        "no event for the file in the new directory: {:?}",
        events
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_moved_in_tree_registers_every_directory() {
    let staging = tmp_dir("staging");
    let dir = tmp_dir("movein");
    std::fs::create_dir_all(staging.join("tree/one/two")).unwrap();

    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();
    assert_eq!(w.watched_dirs(), 1);

    std::fs::rename(staging.join("tree"), dir.join("tree")).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(w.watched_dirs(), 4, "root + tree + tree/one + tree/one/two");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&staging).ok();
}

#[test]
fn a_removed_directory_releases_its_watches() {
    let dir = tmp_dir("rmdir");
    std::fs::create_dir_all(dir.join("gone/deep")).unwrap();

    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();
    assert_eq!(w.watched_dirs(), 3, "root + gone + gone/deep");

    std::fs::remove_dir_all(dir.join("gone")).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(w.watched_dirs(), 1, "descendants released with the parent");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// `Path::starts_with` compares components, not byte prefixes.
#[test]
fn remove_tree_does_not_match_name_prefixes() {
    let dir = tmp_dir("prefix");
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::create_dir_all(dir.join("bc")).unwrap();

    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();
    assert_eq!(w.watched_dirs(), 3);

    std::fs::remove_dir_all(dir.join("b")).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(w.watched_dirs(), 2, "root + bc; /a/bc is not under /a/b");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn running_out_of_budget_later_records_the_reason() {
    let dir = tmp_dir("degrade");
    let config = WatcherConfig {
        max_watched_dirs: 2,
        ..fast_config()
    };
    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        config,
        sink_to_vec().0,
    )
    .unwrap();
    assert!(!w.is_degraded(), "one directory is under the cap of 2");

    std::fs::create_dir(dir.join("fits")).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    std::fs::create_dir(dir.join("overflows")).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        w.degraded_reason(),
        Some(WatchError::TooManyDirectories { dirs: 2, cap: 2 }),
        "the cap, not a kernel limit"
    );
    assert_eq!(w.watched_dirs(), 2, "never registers past the cap");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_created_ignored_directory_is_not_watched() {
    let dir = tmp_dir("newignored");
    let mut w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();

    std::fs::create_dir(dir.join("node_modules")).unwrap();
    std::fs::create_dir(dir.join(".cache")).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(w.watched_dirs(), 1, "neither ignored nor hidden dirs count");
    w.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// On Linux the watch budget is a shared kernel resource; none of it is spent
/// on a subtree the indexer discards.
#[test]
#[cfg(unix)]
fn a_symlinked_directory_is_not_registered_when_following_is_off() {
    let dir = tmp_dir("symlink-reg");
    std::fs::create_dir_all(dir.join("real/nested")).unwrap();
    let outside = tmp_dir("symlink-reg-target");
    std::fs::create_dir_all(outside.join("deep")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

    let w = Watcher::start(
        std::iter::once(&dir),
        default_filters(),
        fast_config(),
        sink_to_vec().0,
    )
    .unwrap();

    assert_eq!(
        w.watched_dirs(),
        if crate::platform::WATCH_ROOTS_RECURSIVELY {
            1
        } else {
            3
        },
        "the symlinked directory must not be registered"
    );
    drop(w);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&outside).ok();
}
