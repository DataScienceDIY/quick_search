//! The one place `#[cfg]` lives.
//!
//! Everything here answers "what does this platform do differently", so no
//! other module has to ask. Two rules keep it honest:
//!
//! - Every function is defined for every target. Callers never wrap a call
//!   site in `#[cfg]`; if a platform has nothing to do, its arm is the
//!   trivial one.
//! - Anything that can be decided from a string rather than a syscall is
//!   split out and made testable everywhere ([`is_unc_string`],
//!   [`PATH_COLLATION`]), because the test suite runs on Linux.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The user's home directory.
///
/// On Windows `%USERPROFILE%` is checked **first**. Git Bash and MSYS2 export
/// `HOME` as a POSIX path (`/c/Users/me`) that no Win32 API can open, and
/// preferring it would point the config file, the index, and the default
/// indexing root at a directory that does not exist.
pub fn home_dir() -> Option<OsString> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(profile);
        }
    }
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
}

/// Whether a directory entry counts as hidden.
///
/// Unix: a leading dot. Windows: a leading dot **or** `FILE_ATTRIBUTE_HIDDEN`
/// / `FILE_ATTRIBUTE_SYSTEM` — without which `include_hidden = false` hides
/// nothing on Windows, and `$RECYCLE.BIN`, `System Volume Information`,
/// `pagefile.sys` and `AppData` all get indexed.
///
/// `meta` is a closure because on Unix it is never called: the walkers
/// deliberately avoid `metadata()`, which would cost an extra `lstat` per
/// entry and a full round trip on a network share. On Windows the cost is
/// zero anyway — both `std::fs::DirEntry::metadata` and
/// `walkdir::DirEntry::metadata` hand back data already cached from
/// `FindNextFileW`.
pub fn entry_is_hidden<F>(name: &str, meta: F) -> bool
where
    F: FnOnce() -> Option<std::fs::Metadata>,
{
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM,
        };
        if let Some(m) = meta() {
            return m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
    }
    false
}

/// Whether `path` has a hidden component *below* the root that contains it.
///
/// Components at or above a root are exempt, because the walkers exempt their
/// root too (depth 0 is always kept — users explicitly chose their roots).
/// The two must agree: if they disagree, a full run indexes a file that the
/// watcher then refuses to update, and the index churns on every cycle.
///
/// That is a latent bug on Unix (`~/.config/app` as a root) and a certainty on
/// Windows, where `AppData` carries `FILE_ATTRIBUTE_HIDDEN` and
/// `std::env::temp_dir()` lives underneath it.
///
/// `roots` are matched by whole path components, so `/a/bc` is not treated as
/// living under `/a/b`. A path under no known root is checked in full.
pub fn path_has_hidden_component_under(path: &Path, roots: &[PathBuf]) -> bool {
    // Innermost containing root wins: with both `/data` and `/data/.cache`
    // configured, a file under the latter is only judged below `.cache`.
    let base = roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count());

    let (mut current, tail) = match base {
        Some(root) => match path.strip_prefix(root) {
            Ok(tail) => (root.clone(), tail),
            Err(_) => (PathBuf::new(), path),
        },
        None => (PathBuf::new(), path),
    };

    // Rebuild the absolute path as we descend: a bare tail component cannot
    // be stat'd on its own, and the attribute check needs a real path.
    for component in tail.components() {
        current.push(component);
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            if entry_is_hidden(&name, || std::fs::metadata(&current).ok()) {
                return true;
            }
        }
    }
    false
}

/// Whether `s` names a UNC path, in either spelling.
///
/// Split out from [`is_network_path`] so the string half is testable on every
/// platform, and written with explicit parentheses — the precedence of `&&`
/// against `||` is exactly the kind of thing that silently disables the
/// network thread pool.
///
/// Only *called* on Windows; compiled everywhere so its tests run everywhere,
/// which is the point of splitting it out.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_unc_string(s: &str) -> bool {
    s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\"))
}

/// Filesystem types whose operations are network round trips.
#[cfg(target_os = "linux")]
const NETWORK_FS_TYPES: [&str; 8] = [
    "cifs", "smb3", "smbfs", "nfs", "nfs4", "afs", "fuse.sshfs", "9p",
];

/// Whether `path` lives on a network filesystem.
///
/// Reads `/proc/mounts` and takes the longest mount point that is a prefix of
/// `path` — the innermost mount is the one that actually serves it.
#[cfg(target_os = "linux")]
pub(crate) fn is_network_path(path: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_dev), Some(point), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // `/proc/mounts` octal-escapes spaces and a few other characters.
        let point = point.replace("\\040", " ");
        let point = Path::new(&point);
        if !target.starts_with(point) {
            continue;
        }
        let depth = point.components().count();
        let is_network = NETWORK_FS_TYPES.contains(&fstype);
        if best.is_none_or(|(d, _)| depth > d) {
            best = Some((depth, is_network));
        }
    }
    best.is_some_and(|(_, is_network)| is_network)
}

/// Whether `path` is served by a network redirector.
///
/// UNC needs no syscall. A *mapped drive letter* does: `Z:\` backed by an SMB
/// share is indistinguishable from a local disk by string inspection, and it
/// is the common case — asking `GetDriveTypeW` is the only way to tell. Left
/// undetected it walks with `LOCAL_THREADS` instead of `NETWORK_THREADS`,
/// which is the exact failure the threading design exists to prevent.
#[cfg(windows)]
pub(crate) fn is_network_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    let s = path.to_string_lossy();
    if is_unc_string(&s) {
        return true;
    }

    // GetDriveTypeW wants a root ("Z:\"), not an arbitrary path.
    let Some(root) = path.components().next() else {
        return false;
    };
    let Component::Prefix(prefix) = root else {
        return false;
    };
    let mut wide: Vec<u16> = prefix.as_os_str().encode_wide().collect();
    wide.push(b'\\' as u16);
    wide.push(0);
    unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_REMOTE }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn is_network_path(_path: &Path) -> bool {
    false
}

/// Whether the filesystem-notification backend covers a whole tree from one
/// watch on its root.
///
/// `false` (inotify): one watch descriptor covers exactly one directory's
/// entries, so the caller must walk the tree and register every directory
/// itself — which is what lets it skip `.git`, `node_modules` and hidden
/// subtrees instead of spending a scarce descriptor on each.
///
/// `true` (`ReadDirectoryChangesW`): one handle covers the subtree, and
/// directories created later are included automatically. Registering
/// per-directory here would be actively harmful rather than merely wasteful —
/// notify allocates a 16 KiB buffer *inline per watch* plus a directory
/// handle, so a large tree would ask for gigabytes of buffers and tens of
/// thousands of handles. The pruning moves to the event path instead.
///
/// macOS FSEvents is also natively recursive, but it is left on the
/// per-directory path here because that path works there and is the one under
/// test.
pub const WATCH_ROOTS_RECURSIVELY: bool = cfg!(windows);

/// SQLite collation for comparing stored path strings.
///
/// Windows filesystems are case-insensitive, and SQLite's `LIKE` already folds
/// ASCII case by default. A path filter that compares one half with `=` and the
/// other with `LIKE` would otherwise disagree with itself. `NOCASE` folds ASCII
/// only, which matches what `LIKE` does — non-ASCII paths stay case-sensitive
/// on both sides, consistently.
pub const PATH_COLLATION: &str = if cfg!(windows) { "NOCASE" } else { "BINARY" };

/// Drop the **calling thread** to background scheduling priority.
///
/// Per-thread, not per-process. The GUI shares this process, so lowering the
/// process would slow the very window the user is watching progress in — the
/// point is to yield to the foreground, not to throttle ourselves. Called by
/// the threads that do indexing work and by nobody else; the search worker,
/// the coordinator and the watcher exist to answer promptly and keep normal
/// priority.
///
/// Best-effort and idempotent: a refusal is not worth reporting, since the
/// only consequence is that indexing competes on equal terms.
pub fn set_background_priority() {
    #[cfg(target_os = "linux")]
    {
        // Linux schedules per task, so `nice` moves this thread alone.
        // Deliberately not `setpriority(PRIO_PROCESS, 0, …)`, which is
        // process-wide on the BSDs and would take the GUI with it.
        unsafe { libc::nice(10) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        // Background *mode*, not merely a lower priority number: it drops I/O
        // priority as well, which is what actually keeps a walk from starving
        // the foreground on a spinning disk.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    }
    // Elsewhere (macOS, BSD): deliberately nothing. `nice` there applies to the
    // whole process, so it would hit the GUI. The right call is
    // `pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0)`, which is worth
    // adding on its own terms rather than approximating here.
}

/// How long to keep retrying a delete that fails because something else holds
/// the file open.
#[cfg(windows)]
const REMOVE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// `fs::remove_file`, retried briefly on Windows.
///
/// Unix `unlink` succeeds even with the file open, so this is a single call
/// there. Windows returns a sharing violation while *any* handle is open —
/// most often an antivirus scanner reading the file microseconds after we
/// closed it. The retry turns a spurious hard failure into a short pause.
pub fn remove_file_retrying(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::remove_file(path)
    }
    #[cfg(windows)]
    {
        let deadline = std::time::Instant::now() + REMOVE_RETRY_BUDGET;
        loop {
            match std::fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(e),
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }
}

/// Deny read access to `dir`, for tests that exercise the unreadable-directory
/// guards.
///
/// Exposed (hidden) rather than duplicated per test module because
/// `tests/full_index.rs` is a separate crate and needs it too. Windows uses
/// `icacls`: a deny ACE binds even the owner until the paired
/// [`restore_read`] rewrites it, and neither call needs elevation.
#[doc(hidden)]
pub fn deny_read(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000))
    }
    #[cfg(windows)]
    {
        icacls(dir, &["/deny", &format!("{}:(OI)(CI)(RD)", current_user()?)])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dir;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "deny_read is not supported on this target",
        ))
    }
}

/// Undo [`deny_read`] so the directory can be cleaned up.
#[doc(hidden)]
pub fn restore_read(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
    }
    #[cfg(windows)]
    {
        icacls(dir, &["/remove:d", &current_user()?])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(windows)]
fn current_user() -> std::io::Result<String> {
    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) => Ok(format!("{}\\{}", domain, user)),
        (_, Ok(user)) => Ok(user),
        _ => Err(std::io::Error::other("USERNAME is not set")),
    }
}

#[cfg(windows)]
fn icacls(dir: &Path, args: &[&str]) -> std::io::Result<()> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args(args)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_spellings() {
        assert!(is_unc_string(r"\\server\share"));
        assert!(is_unc_string(r"\\server\share\dir\file.txt"));
        assert!(is_unc_string(r"\\?\UNC\server\share"));
        // A verbatim *drive* path is local, not a share. This is the case the
        // original `&&`/`||` precedence got wrong.
        assert!(!is_unc_string(r"\\?\C:\Users\me"));
        assert!(!is_unc_string(r"C:\Users\me"));
        assert!(!is_unc_string("/home/me"));
        assert!(!is_unc_string(""));
    }

    /// Not observable cross-platform beyond "it did not blow up", which is
    /// still worth pinning: the call is `unsafe` on both real targets, and it
    /// runs at the top of every walker thread, so it must also be safe to
    /// repeat.
    #[test]
    fn background_priority_is_best_effort_and_repeatable() {
        set_background_priority();
        set_background_priority();
    }

    #[test]
    fn collation_matches_like_case_folding() {
        // LIKE folds ASCII case on every platform; the `=` half of a path
        // filter has to agree with it, which is what this constant is for.
        assert_eq!(PATH_COLLATION, if cfg!(windows) { "NOCASE" } else { "BINARY" });
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

    #[test]
    fn ordinary_names_are_not_hidden() {
        assert!(!entry_is_hidden("Documents", || None));
        assert!(!entry_is_hidden("report.txt", || None));
    }

    #[test]
    fn hidden_components_are_measured_from_the_innermost_root() {
        let root = PathBuf::from(format!("{}.config", sep_prefix()));
        let roots = vec![root.clone()];

        // The root itself is hidden, but it was chosen explicitly — the walk
        // keeps it, so the watcher must too.
        assert!(!path_has_hidden_component_under(&root, &roots));
        assert!(!path_has_hidden_component_under(&root.join("app.conf"), &roots));

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
}
