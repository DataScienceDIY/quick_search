use super::*;

use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::walk::{LOCAL_THREADS, NETWORK_THREADS};

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

/// The walk's pool size for a root at `target` when the mount table reads
/// `mounts`, as `walk::thread_count_for` picks it.
#[cfg(target_os = "linux")]
fn walkers(mounts: &str, target: &str) -> usize {
    if mounts_say_network(mounts, Path::new(target)) {
        NETWORK_THREADS
    } else {
        LOCAL_THREADS
    }
}

/// systemd and autofs list the automount trigger first and the share
/// stacked on it after, at the same mount point.
#[cfg(target_os = "linux")]
#[test]
fn a_share_stacked_on_its_automount_point_is_network() {
    for share in [
        "//nas/share /mnt/nas cifs rw,relatime,vers=3.1.1,cache=strict 0 0",
        "nas:/export /mnt/nas nfs4 rw,relatime,vers=4.2,proto=tcp 0 0",
    ] {
        let mounts = format!(
            "/dev/sda2 / ext4 rw,relatime 0 0\n\
             systemd-1 /mnt/nas autofs rw,relatime,fd=52,pgrp=1,timeout=0,direct 0 0\n\
             {share}\n"
        );
        assert_eq!(
            walkers(&mounts, "/mnt/nas/photos"),
            NETWORK_THREADS,
            "{share}"
        );
        assert_eq!(walkers(&mounts, "/mnt/nas"), NETWORK_THREADS, "{share}");
        assert_eq!(walkers(&mounts, "/home/me"), LOCAL_THREADS, "{share}");
    }
}

/// Before the first access only the trigger is listed. That reads as local
/// rather than touching a share that may hang.
#[cfg(target_os = "linux")]
#[test]
fn an_untriggered_automount_point_reads_as_local() {
    let mounts = "/dev/sda2 / ext4 rw,relatime 0 0\n\
                  systemd-1 /mnt/nas autofs rw,relatime,fd=52,direct 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/nas"), LOCAL_THREADS);
}

#[cfg(target_os = "linux")]
#[test]
fn the_deepest_mountpoint_beats_a_later_shallower_one() {
    let mounts = "//nas/share /mnt/nas cifs rw 0 0\n\
                  /dev/sdb1 /mnt ext4 rw 0 0\n\
                  /dev/sda2 / ext4 rw 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/nas/docs"), NETWORK_THREADS);
    assert_eq!(walkers(mounts, "/mnt/usb"), LOCAL_THREADS);

    // A local disk mounted inside a share serves its own subtree.
    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  nas:/home /home nfs4 rw 0 0\n\
                  /dev/sdc1 /home/me/scratch ext4 rw 0 0\n";
    assert_eq!(walkers(mounts, "/home/me/scratch/build"), LOCAL_THREADS);
    assert_eq!(walkers(mounts, "/home/me/docs"), NETWORK_THREADS);
}

#[cfg(target_os = "linux")]
#[test]
fn a_sibling_sharing_a_prefix_does_not_match() {
    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  //nas/share /mnt/nas cifs rw 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/nas2"), LOCAL_THREADS);
    assert_eq!(walkers(mounts, "/mnt/nas2/docs"), LOCAL_THREADS);
    assert_eq!(walkers(mounts, "/mnt/nas/docs"), NETWORK_THREADS);

    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  //nas/share /mnt/nas2 cifs rw 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/nas"), LOCAL_THREADS);
}

#[cfg(target_os = "linux")]
#[test]
fn an_escaped_space_matches() {
    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  //nas/my\\040share /mnt/my\\040share cifs rw 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/my share/docs"), NETWORK_THREADS);
    assert_eq!(walkers(mounts, "/mnt/my share"), NETWORK_THREADS);
    assert_eq!(walkers(mounts, "/mnt/my"), LOCAL_THREADS);
    // A name really spelled with a backslash is listed as `\134`, so the
    // escaped text itself names no mount.
    assert_eq!(walkers(mounts, "/mnt/my\\040share/docs"), LOCAL_THREADS);
}

/// Every `\ooo` escape the kernel writes is decoded, not just the space.
#[cfg(target_os = "linux")]
#[test]
fn every_octal_escape_in_a_mount_point_is_decoded() {
    use std::os::unix::ffi::OsStrExt;

    for (listed, real) in [
        ("/mnt/a\\134b", "/mnt/a\\b"),
        ("/mnt/a\\011b", "/mnt/a\tb"),
        ("/mnt/a\\012b", "/mnt/a\nb"),
        ("/mnt/\\134\\040\\134", "/mnt/\\ \\"),
    ] {
        let mounts = format!("/dev/sda2 / ext4 rw 0 0\n//nas/s {listed} cifs rw 0 0\n");
        assert_eq!(walkers(&mounts, real), NETWORK_THREADS, "{listed}");
        assert_eq!(walkers(&mounts, &format!("{real}/docs")), NETWORK_THREADS);
        assert_eq!(walkers(&mounts, listed), LOCAL_THREADS, "{listed} as text");
    }
    assert_eq!(unescape_mount_field("/mnt/a\\134b"), "/mnt/a\\b");
    // Not escapes: too short, not octal, or past one byte.
    for raw in [
        "/mnt/a\\13",
        "/mnt/a\\",
        "/mnt/a\\138",
        "/mnt/a\\400",
        "/mnt/a\\x41",
    ] {
        assert_eq!(unescape_mount_field(raw), raw);
    }
    assert_eq!(
        unescape_mount_field("\\377"),
        std::ffi::OsStr::from_bytes(b"\xff")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn short_or_garbage_lines_are_skipped() {
    // Listed after the share at its mount point, a line without a type must
    // not take its place.
    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  //nas/share /mnt/nas cifs rw 0 0\n\
                  \n\
                  garbage\n\
                  /dev/sdb1 /mnt/nas\n\
                  \t \n\
                  relative mnt/nas ext4 rw 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/nas/docs"), NETWORK_THREADS);

    // Nor does one decide on its own.
    assert_eq!(
        walkers("garbage\n\n//nas/share /mnt/nas\n", "/mnt/nas"),
        LOCAL_THREADS
    );
    assert_eq!(walkers("", "/mnt/nas"), LOCAL_THREADS);
}

/// FUSE is judged by subtype: sshfs is a share, while fuseblk (ntfs-3g) and
/// the document portal are local. davfs2 lists bare `fuse`, so it reads as
/// local too; `indexing.root_workers` is its override.
#[cfg(target_os = "linux")]
#[test]
fn fuse_mounts_are_judged_by_subtype() {
    let mounts = "/dev/sda2 / ext4 rw 0 0\n\
                  me@host:/srv /mnt/ssh fuse.sshfs rw,nosuid,nodev,user_id=1000 0 0\n\
                  /dev/sdb1 /media/usb fuseblk rw,nosuid,nodev,allow_other 0 0\n\
                  portal /run/user/1000/doc fuse.portal rw,nosuid,nodev 0 0\n\
                  https://dav.example.com/ /mnt/dav fuse rw,nosuid,nodev 0 0\n";
    assert_eq!(walkers(mounts, "/mnt/ssh/docs"), NETWORK_THREADS);
    for local in [
        "/media/usb/docs",
        "/run/user/1000/doc/1a2b",
        "/mnt/dav/docs",
    ] {
        assert_eq!(walkers(mounts, local), LOCAL_THREADS, "{local}");
    }
}

/// The run thread calls it, then spawns pool threads that inherit its
/// niceness and call it again: they must land where it did, not 10 lower.
#[cfg(target_os = "linux")]
#[test]
fn background_priority_is_idempotent() {
    let niceness = || unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    // A thread of its own, so the harness thread keeps its priority.
    std::thread::spawn(move || {
        set_background_priority();
        let first = niceness();
        // A process started above 10 stays there: setting it back down to
        // 10 is EACCES.
        assert!(first >= 10, "niceness {first}");
        set_background_priority();
        assert_eq!(niceness(), first, "a second call moved it");
        let inherited = std::thread::spawn(move || {
            set_background_priority();
            niceness()
        })
        .join()
        .unwrap();
        assert_eq!(inherited, first, "a spawned thread stacked on its spawner");
    })
    .join()
    .unwrap();
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
/// the fast path must decline them all, including the tags std does not call
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

/// A cloud sync root carries System and *not* Hidden: Windows will not honour
/// the `desktop.ini` supplying its branded icon otherwise.
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
    // window is hit, hence the retry. It cannot reach the product, which
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

/// A missing path answers for its nearest existing ancestor. A run never
/// asks for one: prologue has opened the database before `WalGuard::new`.
#[test]
fn available_space_walks_up_to_an_existing_ancestor() {
    let missing = crate::testutil::scratch_dir("space-missing")
        .join("not")
        .join("created")
        .join("index.sqlite");
    assert!(!missing.exists());
    assert!(available_space(&missing).is_some());
}

/// The run asks for the database file itself, which `GetDiskFreeSpaceExW`
/// refuses; only Windows tells this from asking the file. Not compared with
/// the directory's answer: free space moves between two calls.
#[test]
fn available_space_answers_for_an_existing_file() {
    let file = crate::testutil::scratch_dir("space-file").join("index.sqlite");
    std::fs::write(&file, b"x").unwrap();
    assert!(available_space(&file).is_some_and(|free| free > 0));
    // A missing name below a file walks past the file, too.
    assert!(available_space(&file.join("missing")).is_some_and(|free| free > 0));
}

/// The metadata comes from the opened handle: it describes the file as read,
/// whatever the name held at an earlier `stat`. Anything but a regular file
/// is refused, a FIFO without blocking.
#[test]
fn open_regular_file_returns_the_handle_and_its_stat() {
    use std::io::Read;

    let dir = crate::testutil::scratch_dir("open-regular");
    let file = dir.join("a.txt");
    std::fs::write(&file, b"twelve bytes").unwrap();
    let (mut handle, meta) = open_regular_file(&file).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), 12);
    let mut body = String::new();
    handle.read_to_string(&mut body).unwrap();
    assert_eq!(body, "twelve bytes");

    // Refused by the type check on Unix, and by the open itself on Windows.
    assert!(open_regular_file(&dir).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let fifo = dir.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let refused = open_regular_file(&fifo).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A file that left reads as gone; a `stat` that could not look does not.
#[test]
fn presence_distinguishes_gone_from_unknown() {
    let dir = crate::testutil::scratch_dir("presence");
    let file = dir.join("a.txt");
    std::fs::write(&file, b"twelve bytes").unwrap();
    match presence(&file) {
        Presence::File(meta) => assert_eq!(meta.len(), 12),
        other => panic!("a regular file read as {other:?}"),
    }
    let gone = |path: &Path| matches!(presence(path), Presence::Gone);
    assert!(gone(&dir.join("missing.txt")), "a missing file");
    assert!(gone(&dir), "a directory is not a file");
    // ENOTDIR: an ancestor is now a file.
    assert!(gone(&file.join("below.txt")), "a path through a file");

    #[cfg(unix)]
    {
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let hidden = locked.join("b.txt");
        std::fs::write(&hidden, b"x").unwrap();
        match crate::platform::deny_read(&locked) {
            // Skipped with rights that ignore the mode (root).
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {}
            denied => {
                denied.unwrap();
                assert!(
                    matches!(presence(&hidden), Presence::Unknown),
                    "a file under a parent that cannot be searched read as {:?}",
                    presence(&hidden)
                );
                crate::platform::restore_read(&locked).unwrap();
            }
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// std reads a missing drive and an unreachable share as `NotFound`, which
/// would turn a dropped share's rows gone.
#[cfg(windows)]
#[test]
fn an_unreachable_volume_does_not_say_gone() {
    use windows_sys::Win32::Foundation::{
        ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME, ERROR_FILE_NOT_FOUND, ERROR_INVALID_DRIVE,
    };
    for code in [ERROR_INVALID_DRIVE, ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME] {
        let e = std::io::Error::from_raw_os_error(code as i32);
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{code}");
        assert!(!says_gone(&e), "{code} said the file left");
    }
    let missing = std::io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32);
    assert!(says_gone(&missing));
}

/// The env var naming what [`limited_child`] should do: a number of MiB to
/// allocate, or `spin`.
#[cfg(unix)]
const LIMITED_CHILD: &str = "QS_LIMITED_CHILD";

/// [`spawn_limited`]'s limits reach the child from its first instruction:
/// an allocation inside the cap goes through, one past it kills the child,
/// and a child that will not stop runs out of CPU time. Unix only: the
/// Windows job object is applied just after the child starts, so it needs
/// the stdin handshake that `spawn_limited` documents and the sandbox
/// child performs, which a test binary does not.
#[cfg(unix)]
#[test]
fn a_limited_child_cannot_outgrow_its_limits() {
    // Generous, because the limit counts the test binary's own mappings.
    const CAP: u64 = 512 << 20;
    for (ask, survives) in [("16", true), ("8192", false), ("spin", false)] {
        let cpu = if ask == "spin" {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(60)
        };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "platform::tests::limited_child"])
            .env(LIMITED_CHILD, ask)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut child, _limits) = spawn_limited(&mut command, cpu, CAP).expect("spawn the child");
        let status = child.wait().expect("reap the child");
        assert_eq!(
            status.success(),
            survives,
            "{} against a {} MiB cap",
            ask,
            CAP >> 20
        );
    }
}

/// The child half of [`a_limited_child_cannot_outgrow_its_limits`]: ask for
/// that many MiB and touch every page of it, or spin. A no-op in an
/// ordinary run.
#[cfg(unix)]
#[test]
fn limited_child() {
    let Ok(ask) = std::env::var(LIMITED_CHILD) else {
        return;
    };
    if ask == "spin" {
        // Bounded, so a parent that died first cannot strand this process.
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
        return;
    }
    let bytes = ask.parse::<usize>().expect("MiB") << 20;
    let held = vec![1u8; bytes];
    assert_eq!(held.len(), bytes);
}

/// [`private_dir_under`] hands out only a directory nobody else can use:
/// made 0700 when missing, kept when it is ours and 0700 already, and
/// refused when it is open to others, a link, or not a directory at all.
/// Another user's directory needs root to stage, so the owner check is not
/// exercised here.
#[cfg(unix)]
#[test]
fn a_private_dir_is_ours_and_closed_or_refused() {
    use std::os::unix::fs::PermissionsExt;

    let parent = crate::testutil::Scratch::dir("private-dir");
    // SAFETY: no arguments, and the call cannot fail.
    let uid = unsafe { libc::geteuid() };
    let at = |prefix: &str| parent.join(format!("{}-{}", prefix, uid));
    let refused = |prefix: &str| {
        let err = private_dir_under(&parent, prefix).expect_err(prefix);
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{prefix}: {err}"
        );
    };

    let made = private_dir_under(&parent, "fresh").expect("a fresh dir");
    assert_eq!(made, at("fresh"), "named by the effective uid");
    let mode = std::fs::metadata(&made).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // Ours and 0700 already: the same directory, contents untouched.
    std::fs::write(made.join("kept"), b"x").unwrap();
    let again = private_dir_under(&parent, "fresh").expect("our own dir");
    assert_eq!(again, made);
    assert!(made.join("kept").exists());

    // Any group or other bit is too open.
    for open in [0o777, 0o750, 0o701] {
        let prefix = format!("open{:o}", open);
        std::fs::create_dir(at(&prefix)).unwrap();
        std::fs::set_permissions(at(&prefix), std::fs::Permissions::from_mode(open)).unwrap();
        refused(&prefix);
    }
    // A link to a directory that would itself pass.
    std::os::unix::fs::symlink(&made, at("link")).unwrap();
    refused("link");
    std::fs::write(at("file"), b"x").unwrap();
    refused("file");

    // Only the last component is made: a missing parent is an error.
    let missing = parent.join("missing");
    let err = private_dir_under(&missing, "x").expect_err("a missing parent");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(!missing.exists());
}

/// The image the runtime exported counts only for a binary inside its
/// mount. A pair inherited from another AppImage (a .deb install started from
/// one), a relative or empty path, or either half unset gives `None`, and
/// [`launched_exe`] falls back to the binary.
#[cfg(unix)]
#[test]
fn the_appimage_counts_only_from_inside_its_own_mount() {
    use std::os::unix::ffi::OsStringExt;

    let exe = Path::new("/tmp/.mount_quicksjBMkDo/usr/bin/quicksearch");
    let image = "/home/u/Apps/QuickSearch.AppImage";
    let mount = "/tmp/.mount_quicksjBMkDo";
    let of = |exe: &Path, appimage: Option<&str>, appdir: Option<&str>| {
        appimage_of(
            exe,
            appimage.map(OsString::from),
            appdir.map(OsString::from),
        )
    };

    assert_eq!(
        of(exe, Some(image), Some(mount)),
        Some(PathBuf::from(image))
    );
    // A trailing separator names the same directory.
    let trailing = format!("{}/", mount);
    assert_eq!(
        of(exe, Some(image), Some(&trailing)),
        Some(PathBuf::from(image))
    );
    // The image's path comes back byte for byte, never through a lossy step.
    let odd = OsString::from_vec(b"/home/u/\xffApps/QuickSearch.AppImage".to_vec());
    assert_eq!(
        appimage_of(exe, Some(odd.clone()), Some(mount.into())),
        Some(PathBuf::from(odd))
    );

    let inherited = Path::new("/usr/bin/quicksearch");
    assert_eq!(
        of(
            inherited,
            Some("/home/u/Apps/Other.AppImage"),
            Some("/tmp/.mount_OtherXyZ12")
        ),
        None
    );
    // Whole components: a mount whose name only begins the same is another.
    assert_eq!(of(exe, Some(image), Some("/tmp/.mount_quicks")), None);
    for (appimage, appdir) in [
        ("QuickSearch.AppImage", mount),
        ("", mount),
        (image, "tmp/.mount_quicksjBMkDo"),
        (image, ""),
    ] {
        assert_eq!(
            of(exe, Some(appimage), Some(appdir)),
            None,
            "{:?} {:?}",
            appimage,
            appdir
        );
    }
    assert_eq!(of(exe, None, Some(mount)), None);
    assert_eq!(of(exe, Some(image), None), None);
    assert_eq!(of(exe, None, None), None);
}

/// The runtime mounts or unpacks the image under `$TMPDIR`, and one that runs
/// through a link gives an `APPDIR` spelled through it (seen with
/// `APPIMAGE_EXTRACT_AND_RUN`), while the binary's path is resolved.
#[cfg(unix)]
#[test]
fn an_appimage_mounted_through_a_link_still_counts() {
    let dir = crate::testutil::scratch_dir("appimage-link")
        .canonicalize()
        .unwrap();
    let mount = dir.join("real").join(".mount_quicksjBMkDo");
    std::fs::create_dir_all(&mount).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    let image = OsString::from("/home/u/Apps/QuickSearch.AppImage");

    let linked = dir
        .join("link")
        .join(".mount_quicksjBMkDo")
        .into_os_string();

    let exe = mount.join("usr/bin/quicksearch");
    assert_eq!(
        appimage_of(&exe, Some(image.clone()), Some(linked.clone())),
        Some(PathBuf::from(&image))
    );
    // Resolving the link does not widen the rule: still only from inside.
    let outside = dir.join("real").join("quicksearch");
    assert_eq!(appimage_of(&outside, Some(image), Some(linked)), None);
}

#[cfg(unix)]
const LAUNCHED_CHILD: &str = "QS_LAUNCHED_CHILD";

/// [`launched_exe`] reads the pair from the environment. Each case runs this
/// binary as a child with its own environment, never this process's, which
/// parallel tests share.
#[cfg(unix)]
#[test]
fn the_launched_exe_follows_the_environment_it_runs_in() {
    let exe = std::env::current_exe().expect("test binary path");
    let image = "/home/u/Apps/QuickSearch.AppImage";
    let launched = |appimage: Option<&str>, appdir: Option<&Path>| {
        let mut command = std::process::Command::new(&exe);
        command
            .args([
                "--exact",
                "platform::tests::launched_exe_child",
                "--nocapture",
            ])
            .env(LAUNCHED_CHILD, "1");
        match appimage {
            Some(path) => command.env("APPIMAGE", path),
            None => command.env_remove("APPIMAGE"),
        };
        match appdir {
            Some(dir) => command.env("APPDIR", dir),
            None => command.env_remove("APPDIR"),
        };
        let out = command.output().expect("run the child");
        assert!(out.status.success(), "the child failed: {:?}", out);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        stdout
            .split("launched_exe=")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap_or_else(|| panic!("the child never reported: {}", stdout))
            .to_string()
    };
    let binary = exe.display().to_string();
    let mount = exe.parent().unwrap();

    assert_eq!(launched(Some(image), Some(mount)), image);
    assert_eq!(
        launched(Some(image), Some(Path::new("/tmp/.mount_OtherXyZ12"))),
        binary
    );
    assert_eq!(launched(None, Some(mount)), binary);
    assert_eq!(launched(Some(image), None), binary);
}

/// The child half of [`the_launched_exe_follows_the_environment_it_runs_in`].
/// A no-op in an ordinary run.
#[cfg(unix)]
#[test]
fn launched_exe_child() {
    if std::env::var_os(LAUNCHED_CHILD).is_none() {
        return;
    }
    println!(
        "launched_exe={}",
        launched_exe().expect("an executable").display()
    );
}
