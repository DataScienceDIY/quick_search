//! The one place `#[cfg]` lives.
//!
//! Two rules: every function is defined for every target, so callers never
//! wrap a call site in `#[cfg]`; and anything decidable from a string rather
//! than a syscall is split out and made testable everywhere
//! ([`is_unc_string`], [`PATH_COLLATION`]), because the test suite runs on
//! Linux.

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

/// `FILE_ATTRIBUTE_HIDDEN`, spelled out so [`attributes_are_hidden`] compiles
/// and is tested on Linux; checked against the real header below.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_HIDDEN == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
);

/// Whether a Windows attribute word marks an entry hidden.
///
/// `FILE_ATTRIBUTE_HIDDEN` and nothing else. `FILE_ATTRIBUTE_SYSTEM` must not
/// be part of this test: Windows honours a folder's `desktop.ini` only if the
/// folder carries Read-only or System, so cloud-sync clients set System on
/// their plainly visible sync roots purely to get a branded icon — including
/// it pruned whole cloud folders from the index. Everything System was meant
/// to catch (`$RECYCLE.BIN`, `pagefile.sys`, the legacy per-user junctions)
/// is Hidden **and** System, so Hidden alone catches every one.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_hidden(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
}

/// Why an entry counted as hidden. Only the attribute case is worth the
/// walk's log line: an attribute the user cannot see has no discoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenReason {
    DotPrefix,
    Attribute,
}

/// Whether a directory entry counts as hidden, and why.
///
/// Unix: a leading dot. Windows: a leading dot **or**
/// `FILE_ATTRIBUTE_HIDDEN`, without which `include_hidden = false` hides
/// nothing there. `FILE_ATTRIBUTE_SYSTEM` is not part of it; see
/// [`attributes_are_hidden`].
///
/// `meta` must report the attributes of the entry **itself**, never of a link
/// target — callers pass `symlink_metadata` or an already-cached directory
/// entry. If call sites disagree on that, a full run indexes a file the
/// watcher then refuses to update and the index churns on every cycle.
///
/// `meta` is a closure because on Unix it is never called — `metadata()`
/// would cost an extra `lstat` per entry — while on Windows the data is
/// already cached from `FindNextFileW`.
pub fn entry_hidden_reason<F>(name: &str, meta: F) -> Option<HiddenReason>
where
    F: FnOnce() -> Option<std::fs::Metadata>,
{
    if name.starts_with('.') {
        return Some(HiddenReason::DotPrefix);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Some(m) = meta() {
            if attributes_are_hidden(m.file_attributes()) {
                return Some(HiddenReason::Attribute);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
    }
    None
}

/// [`entry_hidden_reason`] for the callers that only need the verdict.
pub fn entry_is_hidden<F>(name: &str, meta: F) -> bool
where
    F: FnOnce() -> Option<std::fs::Metadata>,
{
    entry_hidden_reason(name, meta).is_some()
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`, spelled out for
/// [`FILE_ATTRIBUTE_HIDDEN`]'s reason and checked the same way.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_REPARSE_POINT
        == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
);

/// The attributes a dehydrated cloud file carries.
///
/// `FILE_ATTRIBUTE_OFFLINE` (0x1000) is the old tape-archive bit that OneDrive
/// reused; `RECALL_ON_OPEN` (0x40000) marks a file whose *metadata* is local but
/// whose data is not; `RECALL_ON_DATA_ACCESS` (0x400000) is the modern
/// Files-On-Demand placeholder. Any one of them means opening the file for read
/// pulls it over the network.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::Storage::FileSystem as fs_attrs;
    assert!(FILE_ATTRIBUTE_OFFLINE == fs_attrs::FILE_ATTRIBUTE_OFFLINE);
    assert!(FILE_ATTRIBUTE_RECALL_ON_OPEN == fs_attrs::FILE_ATTRIBUTE_RECALL_ON_OPEN);
    assert!(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS == fs_attrs::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS);
};

/// Whether reading this file's contents would pull it down from the cloud.
///
/// A Files-On-Demand placeholder has real metadata and no data; opening one
/// for read blocks on a network download of the entire file. The default
/// indexing root is `%USERPROFILE%` and the walk hashes the head of every new
/// or changed file, so without this test a first index quietly downloads the
/// user's whole cloud drive. Always `false` off Windows.
pub fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return attributes_are_dehydrated(meta.file_attributes());
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

/// The bit test behind [`is_cloud_placeholder`], split out so the Linux suite
/// exercises it.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_dehydrated(attributes: u32) -> bool {
    attributes
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

/// Whether a Windows attribute word marks an entry as a reparse point.
///
/// *Not* the same question as `FileType::is_symlink`, which additionally
/// requires the name-surrogate bit: see [`entry_cached_metadata`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_reparse_point(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Metadata a directory read already handed back, on the platforms that hand
/// any back at all.
///
/// `std::fs::Metadata` on Windows: `FindFirstFileW`/`FindNextFileW` return
/// size, timestamps and attributes with every name, and
/// `std::fs::DirEntry::metadata` is a copy of that buffer rather than a
/// syscall. A `fs::metadata(path)` on the same entry is a whole extra
/// `CreateFileW` + `GetFileInformationByHandle` + `CloseHandle` — through
/// every antivirus minifilter on the machine — for data already in hand.
///
/// Uninhabited everywhere else (`getdents64` returns only `d_type`), so
/// `Option<CachedMetadata>` is zero-sized off Windows.
#[cfg(windows)]
pub(crate) type CachedMetadata = std::fs::Metadata;
#[cfg(not(windows))]
pub(crate) type CachedMetadata = std::convert::Infallible;

/// Enforces the zero-size claim above.
#[cfg(not(windows))]
const _: () = assert!(std::mem::size_of::<Option<CachedMetadata>>() == 0);

/// The metadata a directory read already produced for one entry — but only
/// where trusting it is indistinguishable from a fresh `stat`.
///
/// `meta` is a closure for [`entry_hidden_reason`]'s reason: on Unix it is
/// never called.
///
/// `None` for a reparse point even on Windows, and that is the whole
/// subtlety: `fs::metadata` *follows* a reparse point, while the cached
/// buffer describes the link itself. The tags std does not classify as
/// symlinks — `IO_REPARSE_TAG_APPEXECLINK`, the OneDrive
/// `IO_REPARSE_TAG_CLOUD_*` family — reach the walk's ordinary file arm, and
/// for those the two answers differ: an AppExecLink fails `CreateFileW` with
/// `ERROR_CANT_ACCESS_FILE` and is skipped, while its directory entry looks
/// like an ordinary empty file. Tested against the raw
/// `FILE_ATTRIBUTE_REPARSE_POINT` bit, not `is_symlink`, which additionally
/// requires the name-surrogate bit `0x20000000` — precisely the test that
/// lets AppExecLink and the cloud tags through.
pub(crate) fn entry_cached_metadata<F>(meta: F) -> Option<CachedMetadata>
where
    F: FnOnce() -> Option<std::fs::Metadata>,
{
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Some(m) = meta().filter(|m| !attributes_are_reparse_point(m.file_attributes())) {
            return Some(m);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
    }
    None
}

/// A file's `std::fs::Metadata`, from the directory read where that read
/// supplied it and from a `stat` where it did not. Off Windows `cached` is
/// uninhabited and therefore provably `None`.
#[cfg(windows)]
pub(crate) fn metadata_or_stat(
    path: &Path,
    cached: Option<CachedMetadata>,
) -> std::io::Result<std::fs::Metadata> {
    match cached {
        Some(m) => Ok(m),
        None => std::fs::metadata(path),
    }
}

#[cfg(not(windows))]
pub(crate) fn metadata_or_stat(
    path: &Path,
    _cached: Option<CachedMetadata>,
) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

/// Whether `path` has a hidden component *below* the root that contains it.
///
/// Components at or above a root are exempt, because the walkers exempt their
/// root too. The two must agree, or a full run indexes a file the watcher
/// then refuses to update and the index churns — a certainty on Windows,
/// where `AppData` carries `FILE_ATTRIBUTE_HIDDEN` and roots under it are
/// routine.
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
            // `symlink_metadata`, not `metadata`: each component is judged as
            // itself, which is what the walkers do. Only the final component
            // stops being followed, so intermediate ones still resolve
            // normally. See `entry_hidden_reason`.
            if entry_is_hidden(&name, || std::fs::symlink_metadata(&current).ok()) {
                return true;
            }
        }
    }
    false
}

/// Whether `s` names a UNC path, in either spelling.
///
/// Split out from [`is_network_path`] so the string half is testable on every
/// platform; only *called* on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_unc_string(s: &str) -> bool {
    s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\"))
}

/// Filesystem types whose operations are network round trips.
#[cfg(target_os = "linux")]
const NETWORK_FS_TYPES: [&str; 8] = [
    "cifs",
    "smb3",
    "smbfs",
    "nfs",
    "nfs4",
    "afs",
    "fuse.sshfs",
    "9p",
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
/// UNC needs no syscall. A *mapped drive letter* is indistinguishable from a
/// local disk by string inspection — `GetDriveTypeW` is the only way to tell.
/// Left undetected it walks with `LOCAL_THREADS` instead of
/// `NETWORK_THREADS`.
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
/// itself — which is what lets it skip ignored and hidden subtrees.
///
/// `true` (`ReadDirectoryChangesW`): one handle covers the subtree,
/// directories created later included. notify allocates a 16 KiB buffer
/// *inline per watch* plus a directory handle, so per-directory registration
/// here would ask for gigabytes on a large tree; pruning moves to the event
/// path instead.
///
/// macOS FSEvents is also natively recursive, but stays on the per-directory
/// path, which works there and is the one under test.
pub const WATCH_ROOTS_RECURSIVELY: bool = cfg!(windows);

/// SQLite collation for comparing stored path strings.
///
/// Windows filesystems are case-insensitive, and SQLite's `LIKE` already folds
/// ASCII case by default. A path filter that compares one half with `=` and the
/// other with `LIKE` would otherwise disagree with itself. `NOCASE` folds ASCII
/// only, which matches what `LIKE` does — non-ASCII paths stay case-sensitive
/// on both sides, consistently.
pub const PATH_COLLATION: &str = if cfg!(windows) { "NOCASE" } else { "BINARY" };

/// Whether this platform's filesystem matches names without regard to case —
/// what ignore patterns compile against: on Windows and macOS `node_modules`
/// has to exclude `Node_Modules`, and on Linux it must not.
pub const PATHS_ARE_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Drop the **calling thread** to background scheduling priority.
///
/// Per-thread, not per-process: the GUI shares this process. Best-effort and
/// idempotent.
///
/// **CPU only, on every platform.** Windows' `THREAD_MODE_BACKGROUND_BEGIN`
/// is not used: it drops the thread to `IoPriorityVeryLow`, a tier the kernel
/// actively rate-limits — the same one SuperFetch and defrag run in — while
/// Linux's `nice` has no I/O half at all, so the two platforms would not be
/// doing remotely the same thing.
pub fn set_background_priority() {
    #[cfg(target_os = "linux")]
    {
        // Linux schedules per task, so `nice` moves this thread alone.
        unsafe { libc::nice(10) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // CPU only; see the note above for why this is not
        // `THREAD_MODE_BACKGROUND_BEGIN`.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    // Elsewhere (macOS, BSD): nothing — `nice` there applies to the whole
    // process and would hit the GUI.
}

/// Stack size for the per-run worker threads spawned by [`spawn_worker`].
///
/// glibc **caches freed thread stacks** rather than unmapping them
/// (`stack_cache_maxsize`, 40 MiB by default), and a cached stack keeps its
/// dirty pages — so pages a short-lived worker touched outlive it for the
/// life of the process. 512 KiB is ample for these threads' bounded loops.
/// Document parsing recurses on untrusted input; if a malformed document ever
/// overflows this, raise it and give the parser a depth limit.
const WORKER_STACK_SIZE: usize = 512 * 1024;

/// Spawn one of a run's short-lived worker threads, named and with
/// [`WORKER_STACK_SIZE`].
///
/// Panics if the thread cannot be spawned, exactly as `thread::spawn` does.
pub fn spawn_worker<F, T>(name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(f)
        .expect("spawn worker thread")
}

/// Return free heap pages to the kernel.
///
/// glibc's `free` returns a chunk to its arena's free list, not to the OS —
/// only the top of an arena is ever trimmed, and only past
/// `M_TRIM_THRESHOLD` — so a transient peak stays in RSS for the life of the
/// process.
///
/// `malloc_trim(0)` walks *every* arena, so one call from the coordinator
/// also reclaims what other threads left behind. It costs milliseconds on a
/// large heap, so it must not go anywhere hot. Best-effort and idempotent.
pub fn release_free_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: no arguments, no pointers, and safe to call from any thread
        // at any time — glibc takes the arena locks itself.
        unsafe { libc::malloc_trim(0) };
    }
    // Elsewhere: nothing. `malloc_trim` is a glibc extension; musl returns
    // spans to the kernel on free.
}

/// Create `dir` and its parents, readable only by their owner.
///
/// `create_dir_all` leaves the mode to the umask, which is 022 nearly
/// everywhere and so grants the world `+rx`. The two directories this is
/// called for hold the index and the config, i.e. the names and text of
/// everything under the configured roots, plus the salt.
///
/// Only directories *created here* are narrowed: an existing one keeps its
/// mode, because a user who chose `~/Documents` as their data directory did
/// not ask for it to be locked down.
pub fn create_dir_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        // Windows: a file created under the user's profile inherits an ACL
        // that already excludes other users.
        std::fs::create_dir_all(dir)
    }
}

/// Narrow `path` to owner-only access, best effort.
///
/// SQLite creates its database 0644 (`SQLITE_DEFAULT_FILE_PERMISSIONS`, which
/// the bundled build does not override) and then copies that mode to the
/// `-wal` and `-shm` it derives from it, so narrowing the main file before
/// anything else is opened covers all three. The index is a strictly larger
/// secret than any single file it was read from: it holds the full text of
/// documents whose own permissions may be far tighter.
///
/// Failure is ignored: on a filesystem with no Unix permissions (a FAT stick,
/// a network share) there is nothing to set and nothing to report.
pub fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Open `path` for reading, refusing anything that is not a regular file.
///
/// Every caller has already decided the path *was* a regular file, from a
/// `stat` taken earlier — during the walk, or when a result row was put on
/// screen. A rename can put a FIFO, a tty or a character device at that name
/// in between, and `open` on one of those blocks in the kernel until a writer
/// or a carrier appears: uninterruptibly, past any stop flag, and for as long
/// as the process lives. One such open strands a walk worker while it still
/// holds its job slot, which parks the whole pool behind it.
///
/// `O_NONBLOCK` makes the open itself return, and the handle is then asked
/// what it actually is — `fstat` on the descriptor, so nothing can swap the
/// name again underneath the answer. Both are free on a regular file, which
/// is the only case that matters for speed: the flag is ignored for ordinary
/// files and the `fstat` hits the inode already in hand.
pub fn open_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
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
/// guards. Exposed because `tests/full_index.rs` is a separate crate.
///
/// Windows uses `icacls`: a deny ACE binds even the owner until the paired
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
        icacls(
            dir,
            &["/deny", &format!("{}:(OI)(CI)(RD)", current_user()?)],
        )
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
#[path = "platform_tests.rs"]
mod tests;
