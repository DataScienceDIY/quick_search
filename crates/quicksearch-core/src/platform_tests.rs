use super::*;

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
