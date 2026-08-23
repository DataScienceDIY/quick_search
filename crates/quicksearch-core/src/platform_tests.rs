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

/// `unsafe` on both real targets, and run at the top of every walker thread.
#[test]
fn background_priority_is_best_effort_and_repeatable() {
    set_background_priority();
    set_background_priority();
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

/// Providers do not agree on which recall attribute they set, and getting
/// this wrong means silently downloading someone's entire cloud drive.
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

#[test]
#[cfg(not(windows))]
fn nothing_is_a_cloud_placeholder_off_windows() {
    let meta = std::fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap();
    assert!(!is_cloud_placeholder(&meta));
}

/// A junction, an AppExecLink stub and a OneDrive placeholder all carry this bit.
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

/// `fs::metadata` follows a reparse point and the cached buffer does not, so
/// the fast path must decline them all — including the tags std does not call
/// symlinks, which are the ones that reach the walk's ordinary file arm.
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

/// The real `FILE_ATTRIBUTE_*` bits.
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

#[test]
fn only_the_hidden_bit_hides_an_entry() {
    // AppData: Hidden alone, and `std::env::temp_dir()` lives under it.
    assert!(attributes_are_hidden(HIDDEN | DIRECTORY));
    // $RECYCLE.BIN and System Volume Information: Hidden+System, Windows' own
    // definition of a protected operating system file.
    assert!(attributes_are_hidden(HIDDEN | SYSTEM | DIRECTORY));
    // pagefile.sys.
    assert!(attributes_are_hidden(HIDDEN | SYSTEM | ARCHIVE));

    assert!(!attributes_are_hidden(0));
    assert!(!attributes_are_hidden(NORMAL));
    assert!(!attributes_are_hidden(DIRECTORY));
    assert!(!attributes_are_hidden(READONLY | DIRECTORY));
}

/// A cloud sync root carries System and *not* Hidden — Windows will not
/// honour the `desktop.ini` supplying its branded icon otherwise.
#[test]
fn a_sync_root_marked_system_but_not_hidden_is_not_hidden() {
    assert!(!attributes_are_hidden(SYSTEM | DIRECTORY));
    // Read-only also enables desktop.ini; Explorer sets it for custom icons.
    assert!(!attributes_are_hidden(READONLY | SYSTEM | DIRECTORY));
    assert!(!attributes_are_hidden(SYSTEM));
}

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

    // A hidden root was chosen explicitly: the walk keeps it, so must the watcher.
    assert!(!path_has_hidden_component_under(&root, &roots));
    assert!(!path_has_hidden_component_under(
        &root.join("app.conf"),
        &roots
    ));

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
    let roots = vec![PathBuf::from(format!("{}a{}b", sep_prefix(), SEP))];
    let other = PathBuf::from(format!("{}a{}bc{}.x", sep_prefix(), SEP, SEP));
    assert!(path_has_hidden_component_under(&other, &roots));
}

const SEP: char = std::path::MAIN_SEPARATOR;

fn sep_prefix() -> String {
    if cfg!(windows) {
        r"C:\".to_string()
    } else {
        "/".to_string()
    }
}

#[test]
fn the_index_lock_is_exclusive_while_held() {
    let db = crate::testutil::scratch_dir("lock-excl").join("index.sqlite");
    let first = IndexLock::acquire(&db).expect("first acquire");
    match IndexLock::acquire(&db) {
        Err(LockError::Held { pid }) => {
            assert_eq!(pid, Some(std::process::id()));
        }
        Err(LockError::Unsupported(why)) => {
            eprintln!("skipping: {}", why);
        }
        Ok(_) => panic!("the lock was handed out twice"),
    }
    drop(first);
    // `flock` belongs to the *open file description* and `fork` duplicates the
    // descriptor table: between another thread's `fork` and its `exec` a child
    // shares the description just released (`O_CLOEXEC` closes it only at
    // `exec`). Other tests here spawn processes, so under `cargo test` the
    // window is hit — hence the retry. It cannot reach the product, which
    // never drops and immediately retakes the lock.
    acquire_within(&db, Duration::from_secs(5));
}

/// [`IndexLock::acquire`], retried past the `fork`/`exec` window above.
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

/// Nothing unlinks the lock file, so it outlives an unclean exit; the guard is
/// the kernel's `flock`/`LockFileEx`, released however the holder dies, so the
/// leftover file is inert. Only a really-killed child process reproduces "died
/// without running a destructor" while leaving a test alive to check.
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

    child.kill().expect("kill the holder");
    child.wait().expect("reap the holder");

    assert!(
        lock_path.exists(),
        "the crash should leave the lock file at {}",
        lock_path.display()
    );
    // Retried only for the `fork`/`exec` window `acquire_within` documents.
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
    // it for an instant just as we ask; retry.
    let deadline = Instant::now() + Duration::from_secs(30);
    let _lock = loop {
        match IndexLock::acquire(&db) {
            Ok(lock) => break lock,
            Err(e) if Instant::now() >= deadline => panic!("child never acquired: {:?}", e),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Backstop: a parent that dies first must not strand this process.
    std::thread::sleep(Duration::from_secs(120));
}

/// The new lock is taken before the old is dropped: a refused move must leave
/// the app still guarding the index it keeps writing to.
/// The one test that touches the process-wide [`HELD_LOCK`]; its paths are
/// per-test scratch directories, so it does not race the sibling tests that
/// call [`IndexLock::acquire`] directly.
#[test]
fn the_held_lock_follows_the_database_path() {
    let dir = crate::testutil::scratch_dir("lock-move");
    let first = dir.join("first.sqlite");
    let second = dir.join("second.sqlite");

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

    let rival = acquire_within(&second, Duration::from_secs(5));
    assert!(
        matches!(IndexLock::move_to(&second), Err(LockError::Held { .. })),
        "a held destination must refuse the move"
    );
    assert!(
        matches!(IndexLock::acquire(&first), Err(LockError::Held { .. })),
        "the original lock must survive a refused move"
    );

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

/// Never the database or a SQLite sidecar: their inodes must not be touched.
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

/// The check runs at the start of the first run, before the file exists.
#[test]
fn available_space_walks_up_to_an_existing_ancestor() {
    let missing = crate::testutil::scratch_dir("space-missing")
        .join("not")
        .join("created")
        .join("index.sqlite");
    assert!(!missing.exists());
    assert!(available_space(&missing).is_some());
}
