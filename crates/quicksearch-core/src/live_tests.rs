//! Tests for live result watching.
//!
//! The [`classify`] tests are pure: they feed the exact event shapes `notify`
//! 6.1 emits on each platform and check what one settled window decides. They
//! are the ones that pin the design — in particular that a watch on a
//! *directory* sees an atomic save, which a watch on the file would not.
//!
//! The end-to-end tests drive a real filesystem through a real watcher.

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

/// Feed a window of events and report what it decided, after the same
/// orphan-pairing `flush_settled` applies.
fn window(targets: &HashMap<String, Target>, events: Vec<NotifyEvent>) -> HashMap<String, Op> {
    let mut pending = HashMap::new();
    let mut orphan_to = Vec::new();
    for event in &events {
        classify(event, targets, &mut pending, &mut orphan_to);
    }
    let gone: Vec<String> = pending
        .iter()
        .filter(|(_, op)| **op == Op::Gone)
        .map(|(p, _)| p.clone())
        .collect();
    if gone.len() == 1 && orphan_to.len() == 1 {
        pending.insert(gone[0].clone(), Op::Renamed(orphan_to[0].clone()));
    }
    pending
}

/// An editor saving a file writes a temporary and renames it over the target.
/// The row did not move — its contents changed — and a watch on the file
/// itself would have seen none of this, because the inode it was attached to
/// is the one that got orphaned. This test is why the watches are on
/// directories.
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

/// Linux emits From, To and then Both for one in-directory rename. The
/// provisional Gone recorded for the From half must not escape the window.
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

/// Windows never emits `Both` and gives no cookie to pair the halves by, so
/// one unclaimed destination beside one departed target is paired by position.
#[test]
fn a_windows_rename_pairs_by_elimination() {
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
        ],
    );
    assert_eq!(
        decided.get("/docs/old.txt"),
        Some(&Op::Renamed(PathBuf::from("/docs/new.txt")))
    );
}

/// Two departures and two arrivals in one window cannot be paired without
/// guessing. Guessing wrong renames a row to someone else's file, so the
/// ambiguous case resolves to the truthful answer instead.
#[test]
fn an_ambiguous_windows_window_reports_gone_rather_than_guessing() {
    let t = targets(&["/docs/a.txt", "/docs/b.txt"]);
    let decided = window(
        &t,
        vec![
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/a.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/docs/b.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/x.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/docs/y.txt"],
            ),
        ],
    );
    assert_eq!(decided.get("/docs/a.txt"), Some(&Op::Gone));
    assert_eq!(decided.get("/docs/b.txt"), Some(&Op::Gone));
}

/// [`event`], for a path that cannot be spelled as a `&str`.
fn event_os(kind: EventKind, paths: &[PathBuf]) -> NotifyEvent {
    NotifyEvent {
        kind,
        paths: paths.to_vec(),
        attrs: Default::default(),
    }
}

/// A path in the displayed set, and its unrepresentable neighbour that
/// `to_string_lossy` collapses onto exactly that spelling.
fn twin_pair() -> (String, PathBuf) {
    let shown = format!("/docs/{}", crate::testutil::lossy_twin("report", ".txt"));
    let bad = PathBuf::from("/docs").join(crate::testutil::unrepresentable_name("report", ".txt"));
    assert_eq!(bad.to_string_lossy(), shown, "the two must collide");
    (shown, bad)
}

/// A file the index could never hold still generates events, and its lossy
/// spelling is a displayed row's real path. Keyed lossily, every one of those
/// events lands on that row: a `Remove` of the file we cannot index would mark
/// a completely different file gone, on screen, while it sits there on disk.
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

/// Renamed *to* a name the index cannot spell, the row cannot keep a usable
/// path — `Op::Renamed` carries the destination, and the GUI opens rows by it,
/// so a lossy one would open some other file. It left the searchable world, so
/// the honest answer is `Gone`; dropping the event instead would leave a stale
/// row on screen until something else disturbed it.
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

/// The Windows shape of the same thing: the halves arrive separately, and a
/// lone `Gone` plus a lone arrival are paired into a rename. The arrival must
/// not be a path we cannot spell, or the pairing invents the same bad
/// destination the test above rejects.
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

/// A watched directory is full of files that are not on screen. None of them
/// may produce an update — live results never add rows.
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

/// A delete marks the row; a file recreated at the same path un-marks it,
/// which is what makes the mark reversible without re-registering anything.
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

/// Several writes to one file inside a window are one decision, not several.
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

/// Collect updates until `want` of them arrive or the timeout expires.
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

/// A target describing `path` exactly as it is on disk right now, so the
/// arm-time sweep finds nothing to report and the test sees only what it
/// provokes afterwards.
fn current_target(path: &str, text: Option<ContentTier>) -> Target {
    let meta = std::fs::metadata(path).expect("target file exists");
    Target {
        path: path.to_string(),
        text,
        size: meta.len(),
        mtime: mtime_of(&meta),
    }
}

fn watch_one(dir: &Path, name: &str) -> (LiveWatcher, mpsc::Receiver<LiveUpdate>, String) {
    watch_one_matching(dir, name, "hello world", None)
}

/// Write `body` at `dir/name` and watch it for the query `hello`.
fn watch_one_matching(
    dir: &Path,
    name: &str,
    body: &str,
    text: Option<ContentTier>,
) -> (LiveWatcher, mpsc::Receiver<LiveUpdate>, String) {
    let path = dir.join(name).to_string_lossy().into_owned();
    std::fs::write(&path, body).unwrap();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    watcher.watch(
        "hello",
        vec![current_target(&path, text)],
        &Config::default(),
    );
    // Registration happens on the watcher thread.
    std::thread::sleep(Duration::from_millis(300));
    (watcher, rx, path)
}

/// Stop the watcher, then take the tree with it.
///
/// The stop is not optional: the watcher holds an inotify registration on the
/// directory, and pulling the directory out from under a live one is a race
/// worth not having. Unlike most of this crate's tests these do clean up on
/// the way out — the trees are a file or two apiece, generated identically
/// every run, so they hold no evidence the assertion message does not already
/// carry.
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

/// The design test, on a real filesystem: write a temporary and rename it over
/// the target, the way an editor saves. It must read as a change to the row,
/// not as the row disappearing — which is exactly what a watch on the file
/// itself would have reported.
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
}

/// A row whose directory does not exist must not panic, and must not stop the
/// watcher from covering the rows whose directories do.
#[test]
fn a_missing_directory_does_not_stop_the_others() {
    let dir = scratch_dir("live-missing-dir");
    let good = dir.join("present.txt");
    std::fs::write(&good, "hello world").unwrap();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    watcher.watch(
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
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_file(&good).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    assert!(
        updates.iter().any(|u| matches!(u, LiveUpdate::Gone { .. })),
        "the reachable row stopped working: {updates:?}"
    );
}

// --- content, read from the file rather than from the index ---------------
//
// None of these open a database. That is the assertion they all share: a row
// on screen tracks the disk with no indexer involved, which is what the
// feature is for.

/// Pull the one `Changed` out of a batch, failing loudly on anything else.
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

/// The test the old index-backed design could not have: edit a watched file
/// with no index anywhere, and the row's size, modified time and Content Match
/// window all follow it.
#[test]
fn e2e_a_content_change_re_cuts_the_snippet_with_no_index() {
    let dir = scratch_dir("live-content");
    let (watcher, rx, path) =
        watch_one_matching(&dir, "notes.txt", "hello world", Some(ContentTier::Exact));
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

/// Edited until it no longer matches, the row keeps its place and its metadata
/// but loses its window — the Content Match cell falls back to its dash rather
/// than showing text that is no longer a hit.
#[test]
fn e2e_an_edit_that_removes_the_match_clears_the_window() {
    let dir = scratch_dir("live-unmatch");
    let (watcher, rx, path) =
        watch_one_matching(&dir, "notes.txt", "hello world", Some(ContentTier::Exact));
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

/// A fuzzy content hit is re-cut with the fuzzy matcher, not the literal one.
/// The literal is absent from the body by construction — that is what made
/// it a fuzzy hit — so re-cutting it as an exact row would read as "no longer
/// matches" and blank a cell that still has a hit in it.
#[test]
fn e2e_a_fuzzy_content_hit_is_re_cut_with_the_fuzzy_matcher() {
    let dir = scratch_dir("live-fuzzy");
    let path = dir.join("notes.txt").to_string_lossy().into_owned();
    // "quartz" within one edit of "quarts": a stage-8 hit for that query.
    std::fs::write(&path, "sphinx of black quarts judge my vow").unwrap();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    watcher.watch(
        "quartz",
        vec![current_target(&path, Some(ContentTier::Fuzzy))],
        &Config::default(),
    );
    std::thread::sleep(Duration::from_millis(300));
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

/// A file no extractor claims still reports what `metadata` knows. Size and
/// Modified are not the text columns' to withhold.
#[test]
fn e2e_a_file_with_no_extractable_text_still_reports_its_metadata() {
    let dir = scratch_dir("live-binary");
    let path = dir.join("blob.bin").to_string_lossy().into_owned();
    std::fs::write(&path, [0x00u8, 0x01, 0x02, 0xFF]).unwrap();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    watcher.watch(
        "hello",
        vec![current_target(&path, Some(ContentTier::Exact))],
        &Config::default(),
    );
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&path, [0x00u8, 0x01, 0x02, 0xFF, 0xFE, 0xFD]).unwrap();

    let updates = collect(&rx, 1, Duration::from_secs(5));
    stop_and_clean(watcher, &dir);

    let (size, _, window) = one_change(&updates, &path);
    assert_eq!(size, 6);
    // `Unchanged`, not `NoMatch`: nothing readable came back, so there is no
    // evidence the row's window is wrong — only that it is unverifiable.
    assert_eq!(window, WindowUpdate::Unchanged);
}

/// Past `maximum_text_file_size` the indexer stores no text, so neither does
/// the row — but it still says how big the file got.
#[test]
fn e2e_a_file_over_the_text_size_limit_reports_size_but_no_window() {
    let dir = scratch_dir("live-oversize");
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    let path = dir.join("huge.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello world").unwrap();
    let mut config = Config::default();
    config.processing.maximum_text_file_size = 16;
    watcher.watch(
        "hello",
        vec![current_target(&path, Some(ContentTier::Exact))],
        &config,
    );
    std::thread::sleep(Duration::from_millis(300));
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

// --- the arm-time sweep ---------------------------------------------------

/// A row armed with what the *index* said about a file that has since moved on
/// is corrected the moment it is watched. This is the check of the index
/// against the disk, and it is also the only thing that reports anything at
/// all on a filesystem the platform sends no events for.
#[test]
fn arming_corrects_a_row_that_went_stale_while_it_was_not_watched() {
    let dir = scratch_dir("live-sweep-stale");
    let path = dir.join("drifted.txt").to_string_lossy().into_owned();
    std::fs::write(&path, "hello, a body the index never saw").unwrap();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    // What a stale index row would have claimed.
    watcher.watch(
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

/// Same sweep, for a row whose file is simply not there any more.
#[test]
fn arming_reports_a_row_whose_file_vanished_while_it_was_not_watched() {
    let dir = scratch_dir("live-sweep-gone");
    // A sibling keeps the directory watchable, so the ghost is dropped for
    // being missing rather than for its directory being missing.
    let sibling = dir.join("present.txt");
    std::fs::write(&sibling, "hello world").unwrap();
    let ghost = dir.join("vanished.txt").to_string_lossy().into_owned();
    let (watcher, rx) = LiveWatcher::start(Arc::new(|| {}));
    watcher.watch(
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

/// The other half of the sweep, and the one that keeps it quiet: a row that
/// already agrees with the disk is not touched. Without this the watcher would
/// repaint every visible row on every scroll.
#[test]
fn arming_says_nothing_about_a_row_that_already_agrees_with_the_disk() {
    let dir = scratch_dir("live-sweep-quiet");
    let (watcher, rx, _path) =
        watch_one_matching(&dir, "steady.txt", "hello world", Some(ContentTier::Exact));

    // `watch_one_matching` already waited out registration; anything the
    // sweep decided has been sent by now.
    let updates = collect(&rx, 1, Duration::from_millis(500));
    stop_and_clean(watcher, &dir);

    assert!(
        updates.is_empty(),
        "the sweep invented an update: {updates:?}"
    );
}

/// Re-arming replaces the set wholesale; an event for a path that is no longer
/// shown decides nothing.
#[test]
fn re_arming_drops_the_previous_targets() {
    let t = targets(&["/docs/new.txt"]);
    let decided = window(
        &t,
        vec![event(
            EventKind::Modify(ModifyKind::Any),
            &["/docs/old.txt"],
        )],
    );
    assert!(decided.is_empty(), "{decided:?}");
}
