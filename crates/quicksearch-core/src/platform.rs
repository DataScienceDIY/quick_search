//! The one place `#[cfg]` lives. Two rules: every function is defined for
//! every target, so callers never wrap a call site in `#[cfg]`; and anything
//! decidable from a string is split out and testable on Linux ([`is_unc_string`]).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The user's home directory. On Windows `%USERPROFILE%` is checked **first**:
/// Git Bash and MSYS2 export `HOME` as a POSIX path (`/c/Users/me`) that no
/// Win32 API can open.
pub fn home_dir() -> Option<OsString> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(profile);
        }
    }
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
}

/// Spelled out so this compiles and tests on Linux; checked against the real header below.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_HIDDEN == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
);

/// `FILE_ATTRIBUTE_HIDDEN` and nothing else. `FILE_ATTRIBUTE_SYSTEM` must not
/// be part of this test: Windows honours a folder's `desktop.ini` only if it
/// carries Read-only or System, so cloud-sync clients set System on their
/// plainly visible sync roots purely for a branded icon — including it pruned
/// whole cloud folders from the index. Everything System was meant to catch
/// (`$RECYCLE.BIN`, `pagefile.sys`, …) is Hidden **and** System anyway.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_hidden(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
}

/// Only the attribute case is worth the walk's log line: an attribute the
/// user cannot see has no discoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenReason {
    DotPrefix,
    Attribute,
}

/// Whether a directory entry counts as hidden, and why. Unix: a leading dot.
/// Windows: a leading dot **or** `FILE_ATTRIBUTE_HIDDEN` (never System; see
/// [`attributes_are_hidden`]). `meta` must report the entry **itself**, never
/// a link target — call sites disagreeing churns the index every cycle — and
/// is a closure because on Unix it is never called (an extra `lstat` per entry).
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

pub fn entry_is_hidden<F>(name: &str, meta: F) -> bool
where
    F: FnOnce() -> Option<std::fs::Metadata>,
{
    entry_hidden_reason(name, meta).is_some()
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_REPARSE_POINT
        == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
);

/// The attributes a dehydrated cloud file carries: `OFFLINE` (0x1000) is the
/// old tape-archive bit OneDrive reused; `RECALL_ON_OPEN` (0x40000) marks a
/// file whose *metadata* is local but whose data is not;
/// `RECALL_ON_DATA_ACCESS` (0x400000) is the modern Files-On-Demand
/// placeholder. Any one of them means a read pulls the file over the network.
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
/// A placeholder has real metadata and no data; the walk hashes the head of
/// every new or changed file, so getting this wrong quietly downloads the
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

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_dehydrated(attributes: u32) -> bool {
    attributes
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

/// *Not* the same question as `FileType::is_symlink`, which additionally
/// requires the name-surrogate bit: see [`entry_cached_metadata`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_reparse_point(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Metadata a directory read already handed back. On Windows `FindNextFileW`
/// returns size, timestamps and attributes with every name and
/// `DirEntry::metadata` is a copy of that buffer; `fs::metadata(path)` is a
/// whole extra open/stat/close through every antivirus minifilter.
/// Uninhabited elsewhere, so `Option<CachedMetadata>` is zero-sized off Windows.
#[cfg(windows)]
pub(crate) type CachedMetadata = std::fs::Metadata;
#[cfg(not(windows))]
pub(crate) type CachedMetadata = std::convert::Infallible;

#[cfg(not(windows))]
const _: () = assert!(std::mem::size_of::<Option<CachedMetadata>>() == 0);

/// Cached entry metadata, but only where trusting it is indistinguishable
/// from a fresh `stat`. `None` for any reparse point, and that is the whole
/// subtlety: `fs::metadata` *follows* a reparse point while the cached buffer
/// describes the link itself, and the tags std does not classify as symlinks
/// (`IO_REPARSE_TAG_APPEXECLINK`, OneDrive's `IO_REPARSE_TAG_CLOUD_*`) reach
/// the walk's ordinary file arm where the two answers differ. Hence the raw
/// `FILE_ATTRIBUTE_REPARSE_POINT` test, not `is_symlink`, which also wants
/// the name-surrogate bit `0x20000000` — the test that lets those tags through.
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
/// Components at or above a root are exempt because the walkers exempt their
/// root too; the two must agree or the index churns — a certainty on Windows,
/// where `AppData` is Hidden and roots under it are routine. `roots` match by
/// whole components; a path under no known root is checked in full.
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

    for component in tail.components() {
        current.push(component);
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            // `symlink_metadata`, not `metadata`: each component is judged as
            // itself, which is what the walkers do.
            if entry_is_hidden(&name, || std::fs::symlink_metadata(&current).ok()) {
                return true;
            }
        }
    }
    false
}

/// Whether `s` names a UNC path, in either spelling. Only *called* on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_unc_string(s: &str) -> bool {
    s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\"))
}

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

/// The longest `/proc/mounts` mount point that is a prefix of `path` is the
/// one that actually serves it.
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

/// UNC needs no syscall; a *mapped drive letter* is indistinguishable from a
/// local disk by string inspection — `GetDriveTypeW` is the only way to tell.
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

/// Whether the notification backend covers a whole tree from one watch on its
/// root. `false` (inotify): one descriptor covers one directory, so the
/// caller registers every directory itself — which lets it skip ignored and
/// hidden subtrees. `true` (`ReadDirectoryChangesW`): one handle covers the
/// subtree, and notify allocates a 16 KiB buffer *inline per watch*, so
/// per-directory registration would ask for gigabytes; pruning moves to the
/// event path. macOS FSEvents is also recursive but stays per-directory.
pub const WATCH_ROOTS_RECURSIVELY: bool = cfg!(windows);

/// What ignore patterns compile against: on Windows and macOS `node_modules`
/// has to exclude `Node_Modules`, and on Linux it must not.
pub const PATHS_ARE_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Drop the **calling thread** to background scheduling priority (the GUI
/// shares this process). Best-effort and idempotent. **CPU only, on every
/// platform**: Windows' `THREAD_MODE_BACKGROUND_BEGIN` drops I/O to a tier
/// the kernel actively rate-limits, and Linux's `nice` has no I/O half —
/// don't reintroduce it.
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
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    // Elsewhere (macOS, BSD): `nice` applies to the whole process — skip.
}

/// glibc **caches freed thread stacks** (`stack_cache_maxsize`, 40 MiB
/// default) and a cached stack keeps its dirty pages for the life of the
/// process; 512 KiB is ample. Document parsing recurses on untrusted input —
/// if a malformed document ever overflows this, raise it and add a depth limit.
const WORKER_STACK_SIZE: usize = 512 * 1024;

/// Spawn a named worker thread with [`WORKER_STACK_SIZE`]; panics like
/// `thread::spawn` if it cannot.
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

/// Return free heap pages to the kernel. glibc's `free` keeps chunks on arena
/// free lists, so a transient peak stays in RSS for the life of the process;
/// `malloc_trim(0)` walks *every* arena, reclaiming other threads' leavings
/// too, and costs milliseconds — it must not go anywhere hot. Idempotent.
pub fn release_free_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: callable from any thread; glibc takes the arena locks itself.
        unsafe { libc::malloc_trim(0) };
    }
    // Elsewhere: `malloc_trim` is a glibc extension; musl frees to the kernel.
}

/// Create `dir` and its parents, readable only by their owner:
/// `create_dir_all` leaves the mode to the umask (022 ⇒ world `+rx`) and
/// these directories hold the index and config. Only directories *created
/// here* are narrowed — a user who chose `~/Documents` as their data
/// directory did not ask for it to be locked down.
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
        // Windows: the user-profile ACL already excludes other users.
        std::fs::create_dir_all(dir)
    }
}

/// Narrow `path` to owner-only access, best effort. SQLite creates its
/// database 0644 and copies that mode to the `-wal`/`-shm` it derives, so
/// narrowing the main file before anything else opens covers all three; the
/// index is a strictly larger secret than any single file it was read from.
/// Failure is ignored: a FAT stick or share has no permissions to set.
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

/// Open `path` for reading, refusing anything that is not a regular file. A
/// rename since the caller's `stat` can put a FIFO or device there, and
/// `open` on one blocks in the kernel until a writer appears —
/// uninterruptibly, past any stop flag — stranding a walk worker and the
/// pool behind it. `O_NONBLOCK` makes the open return; `fstat` on the
/// descriptor then answers un-swappably. Both are free on a regular file.
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

/// Why [`IndexLock::acquire`] did not hand back a lock.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it. `pid` is for the message only — never decides.
    Held { pid: Option<u32> },
    /// The filesystem does not do locks. The caller must carry on regardless.
    Unsupported(String),
}

/// Proof that this process, and no other, owns the index at a given path.
/// Dropping it, or exiting, releases the lock.
#[derive(Debug)]
pub struct IndexLock {
    /// The lock lives on this open file description; keeping it alive is the mechanism.
    _file: std::fs::File,
    path: PathBuf,
}

/// Never dropped: the kernel releases the lock however the process goes away.
static HELD_LOCK: std::sync::Mutex<Option<IndexLock>> = std::sync::Mutex::new(None);

impl IndexLock {
    /// The lock file for the index at `db_path`. Its own name, never the
    /// database or a SQLite sidecar: a `flock` of ours on an inode SQLite
    /// also locks would be a second protocol on one file, and on Unix our
    /// `close` of it would cancel SQLite's locks.
    pub fn path_for(db_path: &Path) -> PathBuf {
        let name = db_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("index.sqlite");
        db_path.with_file_name(format!("{}.lock", name))
    }

    /// Take the index lock, or report who has it.
    ///
    /// **The guard is the kernel's lock, never the file's existence**: the
    /// kernel drops it however the process dies, so a leftover `.lock` file is
    /// inert and nothing here may ever branch on its presence — a stale-PID
    /// scheme would strand the user behind a crash. [`LockError::Unsupported`]
    /// means the filesystem could not answer, not that the lock is taken;
    /// callers **must** start anyway.
    pub fn acquire(db_path: &Path) -> Result<IndexLock, LockError> {
        let path = IndexLock::path_for(db_path);
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = create_dir_private(dir);
            }
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            // An unwritable index directory is not a second instance: carry on.
            Err(e) => return Err(LockError::Unsupported(format!("{}: {}", path.display(), e))),
        };
        lock_exclusive_nonblocking(&file).map_err(|e| match e {
            LockAttempt::Held => LockError::Held {
                pid: read_recorded_pid(&path),
            },
            LockAttempt::Unsupported(msg) => LockError::Unsupported(msg),
        })?;
        let lock = IndexLock {
            _file: file,
            path: path.clone(),
        };
        lock.record_holder();
        Ok(lock)
    }

    /// [`IndexLock::acquire`] into [`HELD_LOCK`]; what a frontend calls at startup.
    pub fn hold(db_path: &Path) -> Result<(), LockError> {
        let lock = IndexLock::acquire(db_path)?;
        *crate::lock_ok(&HELD_LOCK) = Some(lock);
        Ok(())
    }

    /// Move the held lock onto the index at `db_path`. **The new lock is
    /// taken before the old one is let go**, so a refusal leaves this process
    /// holding what it held. [`LockError::Unsupported`] means the move
    /// *happened* and the new path simply cannot be locked; carry on.
    pub fn move_to(db_path: &Path) -> Result<(), LockError> {
        let mut slot = crate::lock_ok(&HELD_LOCK);
        // Short-circuit before acquiring: `flock` conflicts with itself across
        // two open file descriptions even inside one process, so re-taking the
        // path we already hold would report itself as `Held`.
        if slot
            .as_ref()
            .is_some_and(|held| held.path == IndexLock::path_for(db_path))
        {
            return Ok(());
        }
        match IndexLock::acquire(db_path) {
            Ok(lock) => {
                *slot = Some(lock);
                Ok(())
            }
            Err(LockError::Unsupported(why)) => {
                *slot = None;
                Err(LockError::Unsupported(why))
            }
            Err(e) => Err(e),
        }
    }

    fn record_holder(&self) {
        use std::io::Write;
        // A fresh handle: writing through `_file` would move the shared file
        // offset. That a *second* handle can write here at all is why the
        // Windows lock byte sits at [`LOCK_BYTE_OFFSET`] rather than offset 0
        // — its byte-range locks are mandatory and per-handle, so a lock over
        // this byte would block our own write.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .and_then(|mut f| write!(f, "{}", std::process::id()));
        if let Err(e) = written {
            crate::log_warn!(
                "could not record the lock holder in {}: {}",
                self.path.display(),
                e
            );
        }
    }
}

/// `None` whenever the file is absent, empty or unparseable — all ordinary.
fn read_recorded_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

enum LockAttempt {
    Held,
    Unsupported(String),
}

/// Nowhere near the PID at offset 0. The Unix arm has no equivalent: `flock`
/// locks the open file description, not a byte range.
#[cfg(windows)]
const LOCK_BYTE_OFFSET: u64 = 1 << 63;

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &std::fs::File) -> Result<(), LockAttempt> {
    use std::os::unix::io::AsRawFd;

    // `flock`, not `fcntl`: a POSIX record lock would be cancelled by any
    // `close` this process makes on the same inode — the hazard documented in
    // `file_handling::index_file_set`. A `flock` belongs to the open file
    // description and is immune to it.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Err(LockAttempt::Held),
        _ => Err(LockAttempt::Unsupported(err.to_string())),
    }
}

#[cfg(windows)]
fn lock_exclusive_nonblocking(file: &std::fs::File) -> Result<(), LockAttempt> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

    // The locked byte sits at [`LOCK_BYTE_OFFSET`] — **not** at offset 0,
    // where the PID is written. Windows byte-range locks are *mandatory* and
    // belong to the file object: a lock covering offset 0 would make our own
    // `record_holder` (a second handle) and `read_recorded_pid` fail with
    // `ERROR_LOCK_VIOLATION`. Locking beyond end-of-file is explicitly legal
    // and is the conventional way to use a file as a semaphore.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous = OVERLAPPED_0 {
        Anonymous: OVERLAPPED_0_0 {
            Offset: LOCK_BYTE_OFFSET as u32,
            OffsetHigh: (LOCK_BYTE_OFFSET >> 32) as u32,
        },
    };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == ERROR_LOCK_VIOLATION as i32 => Err(LockAttempt::Held),
        _ => Err(LockAttempt::Unsupported(err.to_string())),
    }
}

/// Bytes free to this user on the filesystem holding `path`. `None` is
/// "unknown", never "zero". The nearest existing ancestor is what gets asked.
pub fn available_space(path: &Path) -> Option<u64> {
    let mut probe = path;
    loop {
        if probe.exists() {
            break;
        }
        probe = probe.parent()?;
    }
    available_space_of_existing(probe)
}

#[cfg(unix)]
fn available_space_of_existing(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `f_bavail`, not `f_bfree`: the difference is root's reserve. `f_frsize`
    // is the fragment size the block counts are in — `f_bsize` is the
    // preferred I/O size and is the wrong multiplier.
    let frsize = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    (stat.f_bavail as u64).checked_mul(frsize as u64)
}

#[cfg(windows)]
fn available_space_of_existing(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_to_caller)
}

#[cfg(windows)]
const REMOVE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// `fs::remove_file`, retried briefly on Windows, which returns a sharing
/// violation while *any* handle is open — most often an antivirus scanner
/// reading the file microseconds after we closed it. Unix `unlink` needs no retry.
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

/// Deny read access to `dir`, for tests (exposed: `tests/full_index.rs` is a
/// separate crate). Windows uses `icacls`: a deny ACE binds even the owner
/// until [`restore_read`] rewrites it; neither call needs elevation.
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
