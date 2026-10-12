//! Tests for live result watching: pure [`classify`] tests feed the exact
//! event shapes `notify` 6.1 emits on each platform; the end-to-end tests
//! drive a real filesystem through a real watcher.

use super::*;

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

fn target(path: &str) -> (String, Target) {
    (
        path.to_string(),
        Target {
            path: path.to_string(),
            text: Some(ContentTier::Exact),
            size: 0,
            mtime: 0,
        },
    )
}

fn targets(paths: &[&str]) -> HashMap<String, Target> {
    paths.iter().map(|p| target(p)).collect()
}

fn event(kind: EventKind, paths: &[&str]) -> NotifyEvent {
    NotifyEvent {
        kind,
        paths: paths.iter().map(PathBuf::from).collect(),
        attrs: Default::default(),
    }
}

/// Feed a window of events and report what it decided.
fn window(targets: &HashMap<String, Target>, events: Vec<NotifyEvent>) -> HashMap<String, Op> {
    let mut pending = HashMap::new();
    let mut awaiting = None;
    for event in &events {
        classify(event, targets, &mut pending, &mut awaiting);
    }
    pending
}

/// One rename as Linux emits it: `From` and `To` carrying the inotify cookie,
/// then `Both`.
fn linux_rename(from: &str, to: &str, cookie: usize) -> Vec<NotifyEvent> {
    vec![
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &[from],
        )
        .set_tracker(cookie),
        event(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[to]).set_tracker(cookie),
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[from, to],
        )
        .set_tracker(cookie),
    ]
}

/// One rename as Windows emits it: `From` then `To`, no cookie, no `Both`.
fn windows_rename(from: &str, to: &str) -> Vec<NotifyEvent> {
    vec![
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &[from],
        ),
        event(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[to]),
    ]
}

/// An editor saves by writing a temporary and renaming it over the target. A
/// watch on the file itself would see none of this (its inode is the one
/// orphaned), so the watches are on directories.
#[test]
fn an_atomic_save_reads_as_a_content_change() {
    let t = targets(&["/docs/report.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/.report.txt.swp"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/report.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/docs/.report.txt.swp", "/docs/report.txt"],
            ),
        ],
    );
    assert_eq!(decided.get("/docs/report.txt"), Some(&Op::Changed));
    assert_eq!(decided.len(), 1, "nothing else was decided: {decided:?}");
}

/// Linux emits From, To and then Both for one in-directory rename.
#[test]
fn a_linux_rename_pairs_without_leaking_a_gone() {
    let t = targets(&["/docs/old.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/old.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/new.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/docs/old.txt", "/docs/new.txt"],
            ),
        ],
    );
    assert_eq!(
        decided.get("/docs/old.txt"),
        Some(&Op::Renamed(PathBuf::from("/docs/new.txt")))
    );
    assert!(
        !decided.values().any(|op| *op == Op::Gone),
        "a provisional Gone escaped: {decided:?}"
    );
}

/// Windows never emits `Both` and gives no cookie to pair the halves by,
/// but emits them back to back.
#[test]
fn a_windows_rename_pairs_by_adjacency() {
    let t = targets(&["/docs/old.txt"]);
    let decided = window(&t, windows_rename("/docs/old.txt", "/docs/new.txt"));
    assert_eq!(
        decided.get("/docs/old.txt"),
        Some(&Op::Renamed(PathBuf::from("/docs/new.txt")))
    );
}

/// Each rename's halves are adjacent, so two in one window cannot swap
/// destinations.
#[test]
fn two_windows_renames_in_one_window_each_keep_their_destination() {
    let t = targets(&["/docs/a.txt", "/docs/b.txt"]);
    let mut events = windows_rename("/docs/a.txt", "/docs/x.txt");
    events.extend(windows_rename("/docs/b.txt", "/docs/y.txt"));
    let decided = window(&t, events);
    assert_eq!(
        decided.get("/docs/a.txt"),
        Some(&Op::Renamed(PathBuf::from("/docs/x.txt")))
    );
    assert_eq!(
        decided.get("/docs/b.txt"),
        Some(&Op::Renamed(PathBuf::from("/docs/y.txt")))
    );
}

/// A shown row deleted while an unrelated file in its directory is renamed:
/// the rename's destination is not where the row went.
#[test]
fn a_deletion_beside_an_unrelated_linux_rename_stays_gone() {
    let t = targets(&["/docs/a.txt"]);
    let mut events = vec![event(EventKind::Remove(RemoveKind::File), &["/docs/a.txt"])];
    events.extend(linux_rename("/docs/c.txt", "/docs/d.txt", 7));
    let decided = window(&t, events);
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
    assert_eq!(decided.len(), 1, "{decided:?}");
}

#[test]
fn a_deletion_beside_an_unrelated_windows_rename_stays_gone() {
    let t = targets(&["/docs/a.txt"]);
    let mut events = vec![event(EventKind::Remove(RemoveKind::Any), &["/docs/a.txt"])];
    events.extend(windows_rename("/docs/c.txt", "/docs/d.txt"));
    let decided = window(&t, events);
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
    assert_eq!(decided.len(), 1, "{decided:?}");
}

#[test]
fn a_delete_and_a_shown_rename_in_one_window_do_not_share_a_destination() {
    let t = targets(&["/docs/a.txt", "/docs/x.txt"]);
    for rename in [
        linux_rename("/docs/a.txt", "/docs/d.txt", 7),
        windows_rename("/docs/a.txt", "/docs/d.txt"),
    ] {
        let mut events = vec![event(EventKind::Remove(RemoveKind::Any), &["/docs/x.txt"])];
        events.extend(rename);
        let decided = window(&t, events);
        assert_eq!(
            decided.get("/docs/a.txt"),
            Some(&Op::Renamed(PathBuf::from("/docs/d.txt")))
        );
        assert_eq!(decided.get("/docs/x.txt"), Some(&Op::Gone));
    }
}

/// Only the event right after a row's `From` can name where it went.
#[test]
fn a_from_not_followed_at_once_by_its_to_stays_gone() {
    let t = targets(&["/docs/a.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/a.txt"],
            ),
            event(EventKind::Modify(ModifyKind::Any), &["/docs/other.txt"]),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/d.txt"],
            ),
        ],
    );
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
}

/// A move out of the watched directories, then an unrelated move in: back to
/// back, but Linux gives them different cookies.
#[test]
fn a_move_out_and_an_unrelated_move_in_do_not_pair() {
    let t = targets(&["/docs/a.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/a.txt"],
            )
            .set_tracker(7),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/d.txt"],
            )
            .set_tracker(8),
        ],
    );
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
}

fn event_os(kind: EventKind, paths: &[PathBuf]) -> NotifyEvent {
    NotifyEvent {
        kind,
        paths: paths.to_vec(),
        attrs: Default::default(),
    }
}

fn twin_pair() -> (String, PathBuf) {
    let shown = format!("/docs/{}", crate::testutil::lossy_twin("report", ".txt"));
    let bad = PathBuf::from("/docs").join(crate::testutil::unrepresentable_name("report", ".txt"));
    assert_eq!(bad.to_string_lossy(), shown, "the two must collide");
    (shown, bad)
}

/// Keyed lossily, events for the unindexable file would land on the displayed
/// row that owns the same lossy spelling.
#[test]
fn events_for_an_unrepresentable_path_never_touch_its_lossy_twin() {
    let (shown, bad) = twin_pair();
    let t = targets(&[&shown]);
    let decided = window(
        &t,
        vec![
            event_os(
                EventKind::Remove(RemoveKind::File),
                std::slice::from_ref(&bad),
            ),
            event_os(
                EventKind::Create(CreateKind::File),
                std::slice::from_ref(&bad),
            ),
            event_os(EventKind::Modify(ModifyKind::Any), &[bad]),
        ],
    );
    assert!(
        decided.is_empty(),
        "the displayed row must be untouched: {decided:?}"
    );
}

/// `Op::Renamed` carries the destination and the GUI opens rows by it, so a
/// lossy destination would open some other file; `Gone` is the honest answer.
#[test]
fn a_rename_to_an_unrepresentable_name_reports_gone() {
    let (_, bad) = twin_pair();
    let t = targets(&["/docs/a.txt"]);
    let decided = window(
        &t,
        vec![event_os(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[PathBuf::from("/docs/a.txt"), bad],
        )],
    );
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
}

#[test]
fn a_split_rename_is_not_paired_with_an_unrepresentable_arrival() {
    let (_, bad) = twin_pair();
    let t = targets(&["/docs/a.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/a.txt"],
            ),
            event_os(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[bad]),
        ],
    );
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
}

#[test]
fn events_for_paths_that_are_not_shown_decide_nothing() {
    let t = targets(&["/docs/shown.txt"]);
    let decided = window(
        &t,
        vec![
            event(EventKind::Create(CreateKind::File), &["/docs/other.txt"]),
            event(EventKind::Modify(ModifyKind::Any), &["/docs/another.txt"]),
            event(EventKind::Remove(RemoveKind::File), &["/docs/third.txt"]),
        ],
    );
    assert!(decided.is_empty(), "{decided:?}");
}

#[test]
fn a_delete_marks_the_row_and_a_recreate_clears_it() {
    let t = targets(&["/docs/report.txt"]);
    let gone = window(
        &t,
        vec![event(
            EventKind::Remove(RemoveKind::File),
            &["/docs/report.txt"],
        )],
    );
    assert_eq!(gone.get("/docs/report.txt"), Some(&Op::Gone));

    let back = window(
        &t,
        vec![event(
            EventKind::Create(CreateKind::File),
            &["/docs/report.txt"],
        )],
    );
    assert_eq!(back.get("/docs/report.txt"), Some(&Op::Changed));
}

#[test]
fn repeated_writes_in_one_window_coalesce() {
    let t = targets(&["/docs/log.txt"]);
    let decided = window(
        &t,
        vec![
            event(EventKind::Modify(ModifyKind::Any), &["/docs/log.txt"]),
            event(EventKind::Modify(ModifyKind::Any), &["/docs/log.txt"]),
            event(EventKind::Modify(ModifyKind::Any), &["/docs/log.txt"]),
        ],
    );
    assert_eq!(decided.len(), 1);
    assert_eq!(decided.get("/docs/log.txt"), Some(&Op::Changed));
}

// --- end to end ----------------------------------------------------------

use crate::testutil::scratch_dir;

fn collect(rx: &mpsc::Receiver<LiveUpdate>, want: usize, timeout: Duration) -> Vec<LiveUpdate> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while out.len() < want {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(update) => out.push(update),
            Err(_) => break,
        }
    }
    out
}

/// Matches the disk exactly, so the arm-time sweep finds nothing to report
/// and the test sees only what it provokes afterwards.
fn current_target(path: &str, text: Option<ContentTier>) -> Target {
    let meta = std::fs::metadata(path).expect("target file exists");
    Target {
        path: path.to_string(),
        text,
        size: meta.len(),
        mtime: mtime_secs(&meta) as i64,
    }
}

fn watch_one(dir: &Path, name: &str) -> (LiveWatcher, mpsc::Receiver<LiveUpdate>, String) {
    watch_one_matching(dir, name, "hello world", None, "hello", &Config::default())
}

/// Write `body` to `dir/name` and watch it as a row the search for `query`
/// returned under `config`, returning once its watch is registered and its
/// sweep sent.
fn watch_one_matching(
    dir: &Path,
    name: &str,
    body: impl AsRef<[u8]>,
    text: Option<ContentTier>,
    query: &str,
    config: &Config,
) -> (LiveWatcher, mpsc::Receiver<LiveUpdate>, String) {
    let path = dir.join(name).to_string_lossy().into_owned();
    std::fs::write(&path, body).unwrap();
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(query, vec![current_target(&path, text)], config);
    watcher.sync();
    (watcher, rx, path)
}

/// Stop before removing the tree: pulling the directory out from under a live
/// inotify registration is a race.
fn stop_and_clean(mut watcher: LiveWatcher, dir: &Path) {
    watcher.stop();
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn e2e_a_rename_surfaces_with_the_new_name() {
    let dir = scratch_dir("live-rename");
    let (watcher, rx, path) = watch_one(&dir, "before.txt");
    let renamed = dir.join("after.txt");
    std::fs::rename(&path, &renamed).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let Some(LiveUpdate::Renamed {
        path: from, name, ..
    }) = updates.first()
    else {
        panic!("expected a rename, got {updates:?}");
    };
    assert_eq!(from, &path);
    assert_eq!(name, "after.txt");
}

#[test]
fn e2e_a_delete_surfaces_as_gone() {
    let dir = scratch_dir("live-delete");
    let (watcher, rx, path) = watch_one(&dir, "doomed.txt");
    std::fs::remove_file(&path).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    assert!(
        updates
            .iter()
            .any(|u| matches!(u, LiveUpdate::Gone { path: p } if *p == path)),
        "expected a Gone for {path}, got {updates:?}"
    );
}

#[test]
fn e2e_an_atomic_save_does_not_read_as_a_delete() {
    let dir = scratch_dir("live-atomic");
    let (watcher, rx, path) = watch_one(&dir, "report.txt");
    let tmp = dir.join("report.txt.tmp");
    std::fs::write(&tmp, "hello, replaced").unwrap();
    std::fs::rename(&tmp, &path).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    assert!(
        !updates.iter().any(|u| matches!(u, LiveUpdate::Gone { .. })),
        "an atomic save was reported as a deletion: {updates:?}"
    );
    let (size, _, _) = one_change(&updates, &path);
    assert_eq!(size, "hello, replaced".len() as u64);
}

#[test]
fn a_missing_directory_does_not_stop_the_others() {
    let dir = scratch_dir("live-missing-dir");
    let good = dir.join("present.txt");
    std::fs::write(&good, "hello world").unwrap();
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(
        "hello",
        vec![
            Target {
                path: dir
                    .join("nowhere")
                    .join("ghost.txt")
                    .to_string_lossy()
                    .into_owned(),
                text: None,
                size: 0,
                mtime: 0,
            },
            current_target(&good.to_string_lossy(), None),
        ],
        &Config::default(),
    );
    watcher.sync();
    std::fs::remove_file(&good).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    assert!(
        updates.iter().any(|u| matches!(u, LiveUpdate::Gone { .. })),
        "the reachable row stopped working: {updates:?}"
    );
}

// --- content, read from the file rather than from the index ---------------
// None of these open a database: a row on screen tracks the disk with no
// indexer involved.

fn one_change(updates: &[LiveUpdate], path: &str) -> (u64, i64, WindowUpdate) {
    let found = updates.iter().find_map(|u| match u {
        LiveUpdate::Changed {
            path: p,
            size,
            mtime,
            window,
        } if p == path => Some((*size, *mtime, window.clone())),
        _ => None,
    });
    found.unwrap_or_else(|| panic!("expected a Changed for {path}, got {updates:?}"))
}

#[test]
fn e2e_a_content_change_re_cuts_the_snippet_with_no_index() {
    let dir = scratch_dir("live-content");
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "notes.txt",
        "hello world",
        Some(ContentTier::Exact),
        "hello",
        &Config::default(),
    );
    std::fs::write(&path, "hello there, a much longer world").unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, mtime, window) = one_change(&updates, &path);
    assert_eq!(size, "hello there, a much longer world".len() as u64);
    assert!(mtime > 0, "no modified time was read");
    let WindowUpdate::Cut(snippet) = window else {
        panic!("the body still matches, so it should carry a window: {window:?}");
    };
    assert!(
        snippet.window.contains("much longer"),
        "the window is stale: {:?}",
        snippet.window
    );
    assert!(!snippet.ranges.is_empty(), "the match was not marked");
}

#[test]
fn e2e_an_edit_that_removes_the_match_clears_the_window() {
    let dir = scratch_dir("live-unmatch");
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "notes.txt",
        "hello world",
        Some(ContentTier::Exact),
        "hello",
        &Config::default(),
    );
    std::fs::write(&path, "nothing of interest here").unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(size, "nothing of interest here".len() as u64);
    assert_eq!(
        window,
        WindowUpdate::NoMatch,
        "a window survived the match it no longer has"
    );
}

#[test]
fn e2e_a_fuzzy_content_hit_is_re_cut_with_the_fuzzy_matcher() {
    let dir = scratch_dir("live-fuzzy");
    // "quartz" within one edit of "quarts": a stage-8 hit for that query.
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "notes.txt",
        "sphinx of black quarts judge my vow",
        Some(ContentTier::Fuzzy),
        "quartz",
        &Config::default(),
    );
    std::fs::write(&path, "the sphinx of black quarts judged my vow again").unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (_, _, window) = one_change(&updates, &path);
    let WindowUpdate::Cut(snippet) = window else {
        panic!("a fuzzy hit was re-judged as exact: {window:?}");
    };
    assert!(
        snippet.window.contains("quarts judged"),
        "the window was not re-cut from the new body: {:?}",
        snippet.window
    );
    assert!(!snippet.ranges.is_empty(), "the fuzzy match was not marked");
}

#[test]
fn e2e_a_file_with_no_extractable_text_still_reports_its_metadata() {
    let dir = scratch_dir("live-binary");
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "blob.bin",
        [0x00u8, 0x01, 0x02, 0xFF],
        Some(ContentTier::Exact),
        "hello",
        &Config::default(),
    );
    std::fs::write(&path, [0x00u8, 0x01, 0x02, 0xFF, 0xFE, 0xFD]).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(size, 6);
    // `Unchanged`, not `NoMatch`: nothing readable came back, so there is no
    // evidence the row's window is wrong, only that it is unverifiable.
    assert_eq!(window, WindowUpdate::Unchanged);
}

#[test]
fn e2e_a_file_over_the_text_size_limit_reports_size_but_no_window() {
    let dir = scratch_dir("live-oversize");
    let mut config = Config::default();
    config.processing.maximum_text_file_size = 16;
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "huge.txt",
        "hello world",
        Some(ContentTier::Exact),
        "hello",
        &config,
    );
    std::fs::write(&path, "hello world, and rather more of it besides").unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(
        size,
        "hello world, and rather more of it besides".len() as u64
    );
    // Not read, so not disproved: the window the search found stands.
    assert_eq!(window, WindowUpdate::Unchanged);
}

/// Re-cutting a dehydrated cloud file's window would recall it. The bytes
/// stay local here, so a read would report `NoMatch`. Windows-only, and not
/// run in CI.
#[cfg(windows)]
#[test]
fn e2e_a_change_to_an_offline_file_leaves_its_window_alone() {
    let dir = scratch_dir("live-offline");
    let (watcher, rx, path) = watch_one_matching(
        &dir,
        "notes.txt",
        "hello world",
        Some(ContentTier::Exact),
        "hello",
        &Config::default(),
    );
    std::fs::write(&path, "nothing of interest here").unwrap();
    // Inside the settle window, so the apply sees the attribute.
    crate::testutil::mark_offline(Path::new(&path)).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(size, "nothing of interest here".len() as u64);
    assert_eq!(window, WindowUpdate::Unchanged);
}

// --- the arm-time sweep ---------------------------------------------------

/// The arm-time sweep is also all that reports anything on a filesystem the
/// platform sends no events for.
#[test]
fn arming_corrects_a_row_that_went_stale_while_it_was_not_watched() {
    let dir = scratch_dir("live-sweep-stale");
    let path = dir.join("drifted.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello, a body the index never saw").unwrap();
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    // What a stale index row would have claimed.
    let rx = watcher.watch(
        "hello",
        vec![Target {
            path: path.clone(),
            text: Some(ContentTier::Exact),
            size: 5,
            mtime: 1,
        }],
        &Config::default(),
    );

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, mtime, window) = one_change(&updates, &path);
    assert_eq!(size, "hello, a body the index never saw".len() as u64);
    assert!(mtime > 1, "the stale modified time survived");
    assert!(
        matches!(window, WindowUpdate::Cut(_)),
        "the window was not re-cut: {window:?}"
    );
}

#[test]
fn arming_reports_a_row_whose_file_vanished_while_it_was_not_watched() {
    let dir = scratch_dir("live-sweep-gone");
    // A sibling keeps the directory watchable, so the ghost is dropped for
    // being missing rather than for its directory being missing.
    let sibling = dir.join("present.txt");
    std::fs::write(&sibling, "hello world").unwrap();
    let ghost = dir.join("vanished.txt").to_string_lossy().into_owned();
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(
        "hello",
        vec![Target {
            path: ghost.clone(),
            text: None,
            size: 11,
            mtime: 1,
        }],
        &Config::default(),
    );

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    assert!(
        updates
            .iter()
            .any(|u| matches!(u, LiveUpdate::Gone { path } if *path == ghost)),
        "expected a Gone for {ghost}, got {updates:?}"
    );
}

#[test]
fn arming_says_nothing_about_a_row_that_already_agrees_with_the_disk() {
    let dir = scratch_dir("live-sweep-quiet");
    let (watcher, rx, _path) = watch_one_matching(
        &dir,
        "steady.txt",
        "hello world",
        Some(ContentTier::Exact),
        "hello",
        &Config::default(),
    );

    // `watch_one_matching` returns once the sweep is sent.
    let updates: Vec<LiveUpdate> = rx.try_iter().collect();
    stop_and_clean(watcher, &dir);

    assert!(
        updates.is_empty(),
        "the sweep invented an update: {updates:?}"
    );
}

/// A pre-1970 time reads as 0 here as it does in the index, so the row the
/// indexer stored for such a file agrees with the disk.
#[test]
fn arming_says_nothing_about_a_pre_1970_file_whose_row_reads_0() {
    let dir = scratch_dir("live-sweep-pre-1970");
    let path = dir.join("old.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello from the past").unwrap();
    if !crate::testutil::backdate_before_1970(Path::new(&path)) {
        std::fs::remove_dir_all(&dir).ok();
        return; // The filesystem keeps no pre-1970 time; nothing to test.
    }
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(
        "hello",
        vec![Target {
            path: path.clone(),
            text: Some(ContentTier::Exact),
            size: "hello from the past".len() as u64,
            mtime: 0,
        }],
        &Config::default(),
    );
    watcher.sync();

    let updates: Vec<LiveUpdate> = rx.try_iter().collect();
    stop_and_clean(watcher, &dir);

    assert!(
        updates.is_empty(),
        "the sweep reported an unchanged row: {updates:?}"
    );
}

/// The sweep corrects a stale offline row's metadata but, like a change,
/// never reads it: a read would re-cut the window. Windows-only, and not run
/// in CI.
#[cfg(windows)]
#[test]
fn arming_corrects_an_offline_row_without_reading_it() {
    let dir = scratch_dir("live-sweep-offline");
    let path = dir.join("drifted.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello, a body the index never saw").unwrap();
    crate::testutil::mark_offline(Path::new(&path)).unwrap();
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(
        "hello",
        vec![Target {
            path: path.clone(),
            text: Some(ContentTier::Exact),
            size: 5,
            mtime: 1,
        }],
        &Config::default(),
    );

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(size, "hello, a body the index never saw".len() as u64);
    assert_eq!(window, WindowUpdate::Unchanged);
}

/// A destination pairs only with the `From` right before it, and a deletion
/// with nothing, so the two in one window stay apart. A move-in from an
/// unwatched directory is the reliable lone destination on every platform: no
/// `From` half, so no `Both` follows.
#[test]
fn e2e_an_unpaired_move_in_does_not_capture_a_later_deletion() {
    let dir = scratch_dir("live-orphan-drain");
    let (watcher, rx, path) = watch_one(&dir, "before.txt");

    let elsewhere = scratch_dir("live-orphan-source");
    let outside = elsewhere.join("moved-in.txt");
    std::fs::write(&outside, "unrelated").unwrap();
    std::fs::rename(&outside, dir.join("moved-in.txt")).unwrap();
    std::fs::remove_file(&path).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);
    std::fs::remove_dir_all(&elsewhere).ok();

    match updates.first() {
        Some(LiveUpdate::Gone { path: gone, .. }) => assert_eq!(gone, &path),
        other => panic!("a deletion must not pair with unrelated churn: {other:?}"),
    }
}

/// A row moved out of every watched directory and an unrelated file moved
/// in, back to back: adjacent, but their cookies differ.
#[cfg(target_os = "linux")]
#[test]
fn e2e_a_move_out_is_not_paired_with_a_later_move_in() {
    let dir = scratch_dir("live-move-out");
    let (watcher, rx, path) = watch_one(&dir, "leaving.txt");

    let elsewhere = scratch_dir("live-move-out-away");
    let arriving = elsewhere.join("arriving.txt");
    std::fs::write(&arriving, "unrelated").unwrap();
    std::fs::rename(&path, elsewhere.join("leaving.txt")).unwrap();
    std::fs::rename(&arriving, dir.join("arriving.txt")).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);
    std::fs::remove_dir_all(&elsewhere).ok();

    match updates.first() {
        Some(LiveUpdate::Gone { path: gone }) => assert_eq!(gone, &path),
        other => panic!("a move out paired with an unrelated move in: {other:?}"),
    }
}

/// A row deleted, then another file in its directory renamed, in one window:
/// the row did not become that file.
#[cfg(target_os = "linux")]
#[test]
fn e2e_a_delete_beside_a_rename_in_one_directory_is_gone() {
    let dir = scratch_dir("live-delete-beside");
    let other = dir.join("other.txt");
    std::fs::write(&other, "unrelated").unwrap();
    let (watcher, rx, path) = watch_one(&dir, "doomed.txt");
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&other, dir.join("renamed.txt")).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    match updates.first() {
        Some(LiveUpdate::Gone { path: gone }) => assert_eq!(gone, &path),
        other => panic!("a deletion took an unrelated rename's name: {other:?}"),
    }
}

// --- a `stat` that cannot look ---------------------------------------------

/// Make `dir` readable but not searchable: a watch on it still registers,
/// while a `stat` of anything in it fails with EACCES. `false`, with the mode
/// restored, when `probe` can still be looked at (rights that ignore the
/// mode, as root has).
#[cfg(unix)]
fn make_unsearchable(dir: &Path, probe: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::metadata(probe).is_ok() {
        crate::platform::restore_read(dir).unwrap();
        return false;
    }
    true
}

/// A `stat` refused for want of permission (as a dropped share's fails with
/// EIO or ESTALE) is no evidence the file left.
#[cfg(unix)]
#[test]
fn arming_leaves_a_row_it_cannot_stat_alone() {
    let dir = scratch_dir("live-sweep-unknown");
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    let hidden = locked.join("hidden.txt").to_string_lossy().into_owned();
    std::fs::write(&hidden, "hello world").unwrap();
    // A stale row beside it, whose correction shows the sweep ran.
    let drifted = dir.join("drifted.txt").to_string_lossy().into_owned();
    std::fs::write(&drifted, "hello, a body the index never saw").unwrap();
    if !make_unsearchable(&locked, Path::new(&hidden)) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    let stale = |path: &str| Target {
        path: path.to_string(),
        text: None,
        size: 5,
        mtime: 1,
    };
    let watcher = LiveWatcher::start(Arc::new(|| {}));
    let rx = watcher.watch(
        "hello",
        vec![stale(&hidden), stale(&drifted)],
        &Config::default(),
    );

    // Every update the sweep decided is sent once the sync returns.
    watcher.sync();
    let updates: Vec<LiveUpdate> = rx.try_iter().collect();
    crate::platform::restore_read(&locked).unwrap();
    stop_and_clean(watcher, &dir);

    one_change(&updates, &drifted);
    assert!(
        updates.iter().all(|u| u.path() != hidden),
        "a row that could not be looked at was updated: {updates:?}"
    );
}

/// The change's own `stat` is refused: the row gets no update, and the same
/// change goes through once the file can be looked at again. Applied by
/// hand, so the refusal cannot land after the `stat` it is meant for.
#[cfg(unix)]
#[test]
fn a_change_on_a_row_that_turns_unstatable_is_not_gone() {
    let dir = scratch_dir("live-change-unknown");
    let path = dir.join("notes.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello, written as access goes").unwrap();
    let (mut live, updates) = bare_loop(&[path.as_str()]);
    if !make_unsearchable(&dir, Path::new(&path)) {
        std::fs::remove_dir_all(&dir).ok();
        return;
    }
    live.apply(path.clone(), Op::Changed);
    let refused: Vec<LiveUpdate> = updates.try_iter().collect();
    crate::platform::restore_read(&dir).unwrap();
    live.apply(path.clone(), Op::Changed);
    let readable: Vec<LiveUpdate> = updates.try_iter().collect();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        refused.is_empty(),
        "a row that could not be looked at was updated: {refused:?}"
    );
    one_change(&readable, &path);
}

// --- the loop, driven by hand -----------------------------------------------

/// A loop with no thread or watcher behind it, showing `paths` and reporting
/// on the receiver returned: a test hands it each wait's outcome
/// ([`Loop::handle`]).
fn bare_loop(paths: &[&str]) -> (Loop, mpsc::Receiver<LiveUpdate>) {
    let (event_tx, rx) = mpsc::channel();
    let (updates, received) = mpsc::channel();
    let mut live = Loop::new(rx, event_tx, Arc::new(|| {}));
    live.update_tx = Some(updates);
    live.targets = targets(paths);
    (live, received)
}

fn hand(live: &mut Loop, event: NotifyEvent) {
    assert!(live.handle(Ok(Msg::Event(event))), "the loop stopped");
}

/// A wait that brought nothing, after which the window is due.
fn quiet_wait(live: &mut Loop) {
    live.settle_at = Some(Instant::now());
    assert!(live.handle(Err(mpsc::RecvTimeoutError::Timeout)));
}

/// A rename in the middle of a burst: its `From` arrives once the window is
/// due, and the flush on that same turn holds the row for the `To` behind
/// it. Reported gone there, the row would stay gone on Windows, which sends
/// no `Both` to correct it.
#[test]
fn a_rename_whose_from_lands_on_a_due_window_still_pairs() {
    let dir = scratch_dir("live-burst-rename");
    let [old, other, new] =
        ["old.txt", "other.txt", "new.txt"].map(|n| dir.join(n).to_string_lossy().into_owned());
    let (mut live, updates) = bare_loop(&[old.as_str(), other.as_str()]);
    // A change opened the window, which is now due.
    let change = event(EventKind::Modify(ModifyKind::Any), &[other.as_str()]);
    hand(&mut live, change);
    live.settle_at = Some(Instant::now());
    for half in windows_rename(&old, &new) {
        hand(&mut live, half);
    }
    quiet_wait(&mut live);
    std::fs::remove_dir_all(&dir).ok();

    let for_old: Vec<LiveUpdate> = updates.try_iter().filter(|u| u.path() == old).collect();
    assert!(
        matches!(for_old.as_slice(), [LiveUpdate::Renamed { to, .. }] if *to == new),
        "{for_old:?}"
    );
}

/// A `From` that nothing follows (a move out of every watched directory) is
/// held only until a wait brings no event, and then the row is gone.
#[test]
fn a_from_that_nothing_follows_is_gone_after_a_quiet_wait() {
    let (mut live, updates) = bare_loop(&["/docs/a.txt"]);
    live.settle_at = Some(Instant::now());
    hand(
        &mut live,
        event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/docs/a.txt"],
        ),
    );
    let held: Vec<LiveUpdate> = updates.try_iter().collect();
    quiet_wait(&mut live);
    let after: Vec<LiveUpdate> = updates.try_iter().collect();

    assert!(held.is_empty(), "reported before a `To` could: {held:?}");
    assert_eq!(
        after,
        vec![LiveUpdate::Gone {
            path: "/docs/a.txt".to_string()
        }]
    );
}

/// A message runs under a kill mark read once it arrived: a stop's kill that
/// landed while the thread was parked must not reach the sandbox children
/// the next message spawns.
#[test]
fn a_message_runs_under_a_kill_mark_read_after_it_arrives() {
    let mut watcher = LiveWatcher::start(Arc::new(|| {}));
    // Parked again, its mark for the next message read if it is read early.
    watcher.sync();
    crate::extract::sandbox::kill_sandbox_children();
    let after_the_kill = KillMark::now();
    let mark = watcher.sync();
    watcher.stop();
    assert!(
        mark >= after_the_kill,
        "{mark:?} was read before the kill at {after_the_kill:?}"
    );
}

// --- one channel per arm ----------------------------------------------------

/// A re-arm or a clear disconnects the last arm's receiver, so nothing meant
/// for the old rows can land on the new ones.
#[test]
fn a_superseded_arm_delivers_nothing() {
    let dir = scratch_dir("live-superseded");
    // Written before any watch, so no event for it can be in flight.
    let later = dir.join("later.txt").to_string_lossy().into_owned();
    std::fs::write(&later, "hello world").unwrap();
    let (watcher, first, path) = watch_one(&dir, "notes.txt");

    // Re-armed over the same row, so the change has an arm to reach.
    let second = watcher.watch(
        "hello",
        vec![current_target(&path, None)],
        &Config::default(),
    );
    watcher.sync();
    std::fs::write(&path, "hello, changed after the re-arm").unwrap();
    let delivered = collect(&second, 1, Duration::from_secs(5));
    let superseded = first.recv_timeout(Duration::from_secs(1));

    let third = watcher.watch(
        "hello",
        vec![current_target(&later, None)],
        &Config::default(),
    );
    watcher.sync();
    watcher.clear();
    std::fs::write(&later, "hello, changed after the clear").unwrap();
    let cleared = third.recv_timeout(Duration::from_secs(2));
    stop_and_clean(watcher, &dir);

    one_change(&delivered, &path);
    assert!(
        matches!(superseded, Err(mpsc::RecvTimeoutError::Disconnected)),
        "the re-armed-over receiver: {superseded:?}"
    );
    assert!(
        matches!(cleared, Err(mpsc::RecvTimeoutError::Disconnected)),
        "the cleared receiver: {cleared:?}"
    );
}
