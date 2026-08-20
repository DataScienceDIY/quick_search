use super::*;

use std::time::{Duration, Instant};

#[test]
fn unc_spellings() {
    assert!(is_unc_string(r"\\server\share"));
    assert!(is_unc_string(r"\\server\share\dir\file.txt"));
    assert!(is_unc_string(r"\\?\UNC\server\share"));
    // A verbatim *drive* path is local, not a share.
    assert!(!is_unc_string(r"\\?\C:\Users\me"));
    assert!(!is_unc_string(r"C:\Users\me"));
    assert!(!is_unc_string("/home/me"));
    assert!(!is_unc_string(""));
}

/// The call is `unsafe` on both real targets and runs at the top of every
/// walker thread, so it must be safe to repeat.
#[test]
fn background_priority_is_best_effort_and_repeatable() {
    set_background_priority();
    set_background_priority();
}

#[test]
fn collation_matches_like_case_folding() {
    // LIKE folds ASCII case on every platform; the `=` half of a path
    // filter has to agree with it, which is what this constant is for.
    assert_eq!(
        PATH_COLLATION,
        if cfg!(windows) { "NOCASE" } else { "BINARY" }
    );
}

#[test]
fn dotfiles_are_hidden_without_consulting_metadata() {
    let mut called = false;
    assert!(entry_is_hidden(".git", || {
        called = true;
        None
    }));
    assert!(!called, "a dot prefix must short-circuit before any stat");
}

/// Each of the three attributes on its own must be enough: providers do not
/// agree on which they set, and getting this wrong means silently
/// downloading someone's entire cloud drive.
#[test]
fn any_recall_attribute_marks_a_file_dehydrated() {
    for bit in [OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS] {
        assert!(attributes_are_dehydrated(bit));
        assert!(attributes_are_dehydrated(ARCHIVE | REPARSE_POINT | bit));
    }
    // A synced-down file keeps the reparse point but drops the recall bits.
    assert!(!attributes_are_dehydrated(ARCHIVE | REPARSE_POINT));
    assert!(!attributes_are_dehydrated(ARCHIVE));
    assert!(!attributes_are_dehydrated(NORMAL));
    assert!(!attributes_are_dehydrated(0));
}

/// Off Windows there is no such thing, and a local `stat` tells the truth.
#[test]
#[cfg(not(windows))]
fn nothing_is_a_cloud_placeholder_off_windows() {
    let meta = std::fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap();
    assert!(!is_cloud_placeholder(&meta));
}

/// The bit test behind the walk's fast path, exercised where the suite
/// actually runs. A junction, an AppExecLink stub and a OneDrive
/// placeholder all carry this bit; an ordinary file does not.
#[test]
fn reparse_points_are_recognised_by_attribute() {
    assert!(attributes_are_reparse_point(REPARSE_POINT));
    assert!(attributes_are_reparse_point(DIRECTORY | REPARSE_POINT));
    // A dehydrated cloud file: reparse point plus the recall attributes.
    assert!(attributes_are_reparse_point(
        ARCHIVE | REPARSE_POINT | 0x40_0000
    ));
    assert!(!attributes_are_reparse_point(ARCHIVE));
    assert!(!attributes_are_reparse_point(NORMAL));
    assert!(!attributes_are_reparse_point(DIRECTORY));
    assert!(!attributes_are_reparse_point(0));
}

/// Off Windows the directory read supplies nothing, and asking it for
/// anything would be the `lstat` per entry the walker exists to avoid.
#[test]
#[cfg(not(windows))]
fn nothing_is_served_from_a_directory_read_off_windows() {
    let mut called = false;
    let got = entry_cached_metadata(|| {
        called = true;
        None
    });
    assert!(got.is_none());
    assert!(
        !called,
        "DirEntry::metadata here is an lstat, which is the syscall the walk exists to avoid"
    );
}

/// `fs::metadata` follows a reparse point and the cached buffer does not,
/// so the fast path must decline every one of them — including the tags std
/// does not call symlinks, which are precisely the ones that reach the
/// walk's ordinary file arm.
#[test]
#[cfg(windows)]
fn a_reparse_point_is_never_served_from_the_directory_read() {
    let dir = std::env::temp_dir().join(format!("qs-reparse-{}", std::process::id()));
    let target = dir.join("target");
    let link = dir.join("link");
    let plain = dir.join("plain.txt");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(&plain, b"x").unwrap();

    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if made {
        let m = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            entry_cached_metadata(|| Some(m)).is_none(),
            "a junction must fall back to the path-based stat"
        );
    }

    let m = std::fs::metadata(&plain).unwrap();
    assert!(
        entry_cached_metadata(|| Some(m)).is_some(),
        "an ordinary file must take the fast path"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ordinary_names_are_not_hidden() {
    assert!(!entry_is_hidden("Documents", || None));
    assert!(!entry_is_hidden("report.txt", || None));
}

/// The real `FILE_ATTRIBUTE_*` bits, so the cases below read as the files
/// they stand for.
const READONLY: u32 = 0x1;
const HIDDEN: u32 = 0x2;
const SYSTEM: u32 = 0x4;
const DIRECTORY: u32 = 0x10;
const ARCHIVE: u32 = 0x20;
const NORMAL: u32 = 0x80;
const REPARSE_POINT: u32 = 0x400;
const OFFLINE: u32 = 0x1000;
const RECALL_ON_OPEN: u32 = 0x4_0000;
const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// The attribute half of `entry_is_hidden`, testable from Linux.
#[test]
fn only_the_hidden_bit_hides_an_entry() {
    // AppData: Hidden alone, and `std::env::temp_dir()` lives under it.
    assert!(attributes_are_hidden(HIDDEN | DIRECTORY));
    // $RECYCLE.BIN, System Volume Information, and the legacy per-user
    // junctions: Hidden+System, Windows' own definition of a protected
    // operating system file.
    assert!(attributes_are_hidden(HIDDEN | SYSTEM | DIRECTORY));
    // pagefile.sys.
    assert!(attributes_are_hidden(HIDDEN | SYSTEM | ARCHIVE));

    assert!(!attributes_are_hidden(0));
    assert!(!attributes_are_hidden(NORMAL));
    assert!(!attributes_are_hidden(DIRECTORY));
    assert!(!attributes_are_hidden(READONLY | DIRECTORY));
}

/// A cloud sync root carries System and *not* Hidden — Windows will not
/// honour the `desktop.ini` supplying its branded icon otherwise — and is
/// fully visible in Explorer, so it must not read as hidden.
#[test]
fn a_sync_root_marked_system_but_not_hidden_is_not_hidden() {
    assert!(!attributes_are_hidden(SYSTEM | DIRECTORY));
    // Read-only is the other attribute that enables desktop.ini, and
    // Explorer sets it when a user picks a custom folder icon.
    assert!(!attributes_are_hidden(READONLY | SYSTEM | DIRECTORY));
    assert!(!attributes_are_hidden(SYSTEM));
}

/// The walk announces an attribute prune and stays quiet about a dot
/// prefix, so the two must stay distinguishable.
#[test]
fn a_dot_prefix_reports_itself_as_the_reason() {
    assert_eq!(
        entry_hidden_reason(".git", || None),
        Some(HiddenReason::DotPrefix)
    );
    assert_eq!(entry_hidden_reason("Documents", || None), None);
}

#[test]
fn hidden_components_are_measured_from_the_innermost_root() {
    let root = PathBuf::from(format!("{}.config", sep_prefix()));
    let roots = vec![root.clone()];

    // The root itself is hidden, but it was chosen explicitly — the walk
    // keeps it, so the watcher must too.
    assert!(!path_has_hidden_component_under(&root, &roots));
    assert!(!path_has_hidden_component_under(
        &root.join("app.conf"),
        &roots
    ));

    // A dot *below* the root still counts.
    assert!(path_has_hidden_component_under(
        &root.join(".secret").join("x"),
        &roots
    ));
}

#[test]
fn a_path_under_no_root_is_checked_in_full() {
    let roots = vec![PathBuf::from(format!("{}srv", sep_prefix()))];
    let stray = PathBuf::from(format!("{}home{}me{}.ssh", sep_prefix(), SEP, SEP));
    assert!(path_has_hidden_component_under(&stray, &roots));
}

#[test]
fn sibling_roots_do_not_capture_each_other() {
    // `/a/bc` does not live under `/a/b`, so the `.x` below it is judged,
    // not exempted.
    let roots = vec![PathBuf::from(format!("{}a{}b", sep_prefix(), SEP))];
    let other = PathBuf::from(format!("{}a{}bc{}.x", sep_prefix(), SEP, SEP));
    assert!(path_has_hidden_component_under(&other, &roots));
}

const SEP: char = std::path::MAIN_SEPARATOR;

/// An absolute-path prefix for the running platform, so these tests read
/// the same on both.
fn sep_prefix() -> String {
    if cfg!(windows) {
        r"C:\".to_string()
    } else {
        "/".to_string()
    }
}

/// Two live instances cannot both hold the index.
#[test]
fn the_index_lock_is_exclusive_while_held() {
    let db = crate::testutil::scratch_dir("lock-excl").join("index.sqlite");
    let first = IndexLock::acquire(&db).expect("first acquire");
    match IndexLock::acquire(&db) {
        Err(LockError::Held { pid }) => {
            // Recorded for the message only, but it should name us.
            assert_eq!(pid, Some(std::process::id()));
        }
        Err(LockError::Unsupported(why)) => {
            // A filesystem with no locks cannot answer; nothing to assert.
            eprintln!("skipping: {}", why);
        }
        Ok(_) => panic!("the lock was handed out twice"),
    }
    drop(first);
    // And it comes back once the holder lets go — but not necessarily in the
    // same instant, which is why this retries instead of asserting outright.
    //
    // `flock` belongs to the *open file description*, and `fork` duplicates
    // the descriptor table: between another thread's `fork` and its `exec`,
    // the child shares every description this process has open, including the
    // one we just released. `O_CLOEXEC` closes it at `exec` — verified, no
    // descriptor survives into a spawned child — but until then the lock
    // stays held. Several tests in this suite spawn processes (the sibling
    // test below, and `file_handling::counting`'s `find`/`wc`), so under
    // `cargo test` this window is reached often enough to be seen.
    //
    // It cannot reach the product: the lock is taken once at startup and held
    // for the life of the process, never dropped and immediately retaken.
    acquire_within(&db, Duration::from_secs(5));
}

/// [`IndexLock::acquire`], retried past the `fork`/`exec` window described in
/// [`the_index_lock_is_exclusive_while_held`].
fn acquire_within(db: &std::path::Path, budget: Duration) -> IndexLock {
    let deadline = Instant::now() + budget;
    loop {
        match IndexLock::acquire(db) {
            Ok(lock) => return lock,
            Err(e) if Instant::now() >= deadline => panic!("never acquired: {:?}", e),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Env var naming the database whose lock [`lock_holder_child`] should take.
/// Absent in an ordinary run, which is what makes that test a no-op.
const LOCK_CHILD_DB: &str = "QS_LOCK_CHILD_DB";

/// **A crash must not lock the user out.**
///
/// Nothing unlinks the lock file, so it outlives an unclean exit. If startup
/// keyed on the file *existing*, one SIGKILL — or the SIGBUS this whole change
/// is about — would leave QuickSearch permanently unopenable. The guard is the
/// kernel's `flock`/`LockFileEx`, released when the holder's handle goes away
/// however it goes away, so the leftover file is inert.
///
/// Only a real killed process proves that, so this spawns one: nothing a
/// single process can do to itself reproduces "died without running a
/// destructor" while leaving a test alive to check the result.
#[test]
fn a_killed_holder_does_not_block_the_next_start() {
    let db = crate::testutil::scratch_dir("lock-crash").join("index.sqlite");
    let lock_path = IndexLock::path_for(&db);

    // Probe first: on a filesystem without locks there is nothing to test.
    match IndexLock::acquire(&db) {
        Ok(lock) => drop(lock),
        Err(LockError::Unsupported(why)) => {
            eprintln!("skipping: {}", why);
            return;
        }
        Err(LockError::Held { .. }) => panic!("a fresh path cannot be held"),
    }

    let exe = std::env::current_exe().expect("test binary path");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "platform::tests::lock_holder_child",
            "--nocapture",
        ])
        .env(LOCK_CHILD_DB, &db)
        .spawn()
        .expect("spawn the lock holder");

    // Poll rather than read a pipe: a child that dies early then fails this
    // test at the deadline instead of hanging it forever.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(IndexLock::acquire(&db), Err(LockError::Held { .. })) {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the child never took the lock");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // SIGKILL / TerminateProcess: no unwinding, no destructors, no cleanup —
    // exactly what a SIGBUS leaves behind.
    child.kill().expect("kill the holder");
    child.wait().expect("reap the holder");

    assert!(
        lock_path.exists(),
        "the crash should leave the lock file at {}",
        lock_path.display()
    );
    // The point of the whole test: file present, holder dead, start succeeds.
    // Retried for the reason `acquire_within` documents, not because a dead
    // holder could still be holding anything.
    acquire_within(&db, Duration::from_secs(5));
}

/// The child half of [`a_killed_holder_does_not_block_the_next_start`]: take
/// the lock, then wait to be killed. A no-op in an ordinary run.
#[test]
fn lock_holder_child() {
    let Some(db) = std::env::var_os(LOCK_CHILD_DB) else {
        return;
    };
    let db = std::path::PathBuf::from(db);
    // The parent probes the lock to find out when we have it, so it may hold
    // it for an instant just as we ask. Retry rather than lose the race.
    let deadline = Instant::now() + Duration::from_secs(30);
    let _lock = loop {
        match IndexLock::acquire(&db) {
            Ok(lock) => break lock,
            Err(e) if Instant::now() >= deadline => panic!("child never acquired: {:?}", e),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Killed long before this returns. The sleep is a backstop so a parent
    // that dies first cannot strand this process.
    std::thread::sleep(Duration::from_secs(120));
}

/// A `database_path` changed in Settings must carry the lock with it — and a
/// refused move must leave this process holding exactly what it held.
///
/// The whole point of taking the new lock before dropping the old: if the
/// destination is already somebody else's, the settings change is rejected and
/// the app goes on using the index it was using, still guarded. Releasing
/// first would open a window on the index we are about to keep writing to.
///
/// Uses the process-wide slot, so it is the one test that touches
/// [`HELD_LOCK`]; the paths are per-test scratch directories, so it does not
/// race the sibling tests that call [`IndexLock::acquire`] directly.
#[test]
fn the_held_lock_follows_the_database_path() {
    let dir = crate::testutil::scratch_dir("lock-move");
    let first = dir.join("first.sqlite");
    let second = dir.join("second.sqlite");

    // A filesystem with no locks cannot answer any of this.
    match IndexLock::hold(&first) {
        Ok(()) => {}
        Err(LockError::Unsupported(why)) => {
            eprintln!("skipping: {}", why);
            return;
        }
        Err(LockError::Held { .. }) => panic!("a fresh path cannot be held"),
    }

    // Naming the index we already hold is a no-op, not a self-collision:
    // `flock` conflicts with itself across two descriptions in one process.
    IndexLock::move_to(&first).expect("re-holding the same path");

    // Somebody else owns the destination, so the move is refused...
    let rival = acquire_within(&second, Duration::from_secs(5));
    assert!(
        matches!(IndexLock::move_to(&second), Err(LockError::Held { .. })),
        "a held destination must refuse the move"
    );
    // ...and the old path is still ours, which is what lets the caller reject
    // the settings change and stay correct.
    assert!(
        matches!(IndexLock::acquire(&first), Err(LockError::Held { .. })),
        "the original lock must survive a refused move"
    );

    // Once the destination frees up the move goes through, and the path we
    // came from is released.
    drop(rival);
    let deadline = Instant::now() + Duration::from_secs(5);
    while IndexLock::move_to(&second).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(IndexLock::acquire(&second), Err(LockError::Held { .. })),
        "the new path must be held after the move"
    );
    acquire_within(&first, Duration::from_secs(5));
}

/// `database_path` in hand, the lock is a sibling with its own name — never
/// the database or one of SQLite's sidecars, whose inodes we must not touch.
#[test]
fn the_lock_file_is_not_the_database_or_a_sidecar() {
    let db = std::path::Path::new("/var/lib/qs/index.sqlite");
    let lock = IndexLock::path_for(db);
    assert_eq!(lock, std::path::Path::new("/var/lib/qs/index.sqlite.lock"));
    for suffix in crate::file_handling::INDEX_SIDECAR_SUFFIXES {
        if suffix == ".lock" {
            continue;
        }
        assert_ne!(
            lock,
            std::path::PathBuf::from(format!("{}{}", db.display(), suffix))
        );
    }
    assert_ne!(lock, db);
}

#[test]
fn available_space_answers_for_a_real_directory() {
    let dir = crate::testutil::scratch_dir("space");
    let free = available_space(&dir).expect("temp dir has a filesystem");
    assert!(free > 0, "a writable scratch dir should have free space");
}

/// The database is asked about before it exists — the check runs at the start
/// of the first run, when nothing has created the file yet.
#[test]
fn available_space_walks_up_to_an_existing_ancestor() {
    let missing = crate::testutil::scratch_dir("space-missing")
        .join("not")
        .join("created")
        .join("index.sqlite");
    assert!(!missing.exists());
    assert!(available_space(&missing).is_some());
}
