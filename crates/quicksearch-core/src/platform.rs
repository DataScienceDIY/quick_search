//! The one place `#[cfg]` lives. Two rules: every function is defined for
//! every target, so callers never wrap a call site in `#[cfg]`; and anything
//! decidable from a string is split out and testable on Linux ([`is_unc_string`]).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The user's home directory. On Windows `%USERPROFILE%` is checked first:
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

/// The file the user launched: the `.AppImage` when this process runs from
/// inside one, else the running executable. An AppImage's runtime mounts the
/// image (`/tmp/.mount_…`, or unpacks it under `APPIMAGE_EXTRACT_AND_RUN`) and
/// runs the binary from there, a path that vanishes with the process; it
/// exports the image's own path as `$APPIMAGE` and the mount as `$APPDIR`.
pub fn launched_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let image = appimage_of(
        &exe,
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
    );
    Some(image.unwrap_or(exe))
}

/// `appimage` when both paths are absolute and `exe` runs from inside
/// `appdir`, else `None`. That includes an inherited pair: a program started
/// from another AppImage (a file opened from it) carries that image's pair,
/// whose mount this binary is not in.
fn appimage_of(
    exe: &Path,
    appimage: Option<OsString>,
    appdir: Option<OsString>,
) -> Option<PathBuf> {
    let (appimage, appdir) = (PathBuf::from(appimage?), PathBuf::from(appdir?));
    if !appimage.is_absolute() || !appdir.is_absolute() {
        return None;
    }
    // The runtime mounts or unpacks under `$TMPDIR` and spells `APPDIR`
    // through any link in it; `current_exe`, read from `/proc/self/exe`,
    // never does.
    let appdir = appdir.canonicalize().unwrap_or(appdir);
    exe.starts_with(&appdir).then_some(appimage)
}

/// Spelled out so this compiles and tests on Linux; checked against the real header below.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_HIDDEN == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
);

/// Only `FILE_ATTRIBUTE_HIDDEN` counts, never System: cloud-sync clients set
/// System on visible sync roots (Windows honours `desktop.ini` icons only
/// then), and the system files worth skipping (`$RECYCLE.BIN`,
/// `pagefile.sys`) are Hidden too.
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
/// Windows: a leading dot or `FILE_ATTRIBUTE_HIDDEN` (never System; see
/// [`attributes_are_hidden`]). `meta` must report the entry itself, never a
/// link target (call sites disagreeing churns the index every cycle), and is
/// a closure because on Unix it is never called (an extra `lstat` per entry).
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
///
/// NTFS may lag it for a file open for writing or written through another
/// hard link. The walk trusts it only to say "unchanged" (such a change waits
/// for the next run) and takes a changed stamp from the opened handle.
#[cfg(windows)]
pub(crate) type CachedMetadata = std::fs::Metadata;
#[cfg(not(windows))]
pub(crate) type CachedMetadata = std::convert::Infallible;

#[cfg(not(windows))]
const _: () = assert!(std::mem::size_of::<Option<CachedMetadata>>() == 0);

/// Cached entry metadata, or `None` for any reparse point: `fs::metadata`
/// *follows* a reparse point while the cached buffer describes the link
/// itself, and the tags std does not classify as symlinks
/// (`IO_REPARSE_TAG_APPEXECLINK`, OneDrive's `IO_REPARSE_TAG_CLOUD_*`) reach
/// the walk's ordinary file arm where the two answers differ. Hence the raw
/// `FILE_ATTRIBUTE_REPARSE_POINT` test, not `is_symlink`, which also wants
/// the name-surrogate bit `0x20000000`: the test that lets those tags through.
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

/// The innermost of `roots` containing `path`, and what lies below it.
/// Containment is by whole components (`/a/bc` is not under `/a/b`), and a
/// root contains itself. Innermost wins: with both `/data` and `/data/.cache`
/// configured, a file under the latter is judged only below `.cache`.
///
/// Compared as spelled, so `path` and `roots` must share a spelling: both
/// canonical, or both as configured, the spelling watcher events arrive in
/// (through the link, for a root that is one).
pub(crate) fn split_at_root<'r, 'p>(
    path: &'p Path,
    roots: impl IntoIterator<Item = &'r Path>,
) -> Option<(&'r Path, &'p Path)> {
    roots
        .into_iter()
        .filter_map(|root| Some((root, path.strip_prefix(root).ok()?)))
        .max_by_key(|(root, _)| root.components().count())
}

/// Whether `path` has a hidden component *below* the root that contains it.
/// Components at or above a root are exempt because the walkers exempt their
/// root too; the two must agree or the index churns, a certainty on Windows,
/// where `AppData` is Hidden and roots under it are routine. The root is
/// found by [`split_at_root`]; a path under no known root is checked in full.
pub fn path_has_hidden_component_under(path: &Path, roots: &[PathBuf]) -> bool {
    let (root, tail) =
        split_at_root(path, roots.iter().map(PathBuf::as_path)).unwrap_or((Path::new(""), path));
    let mut current = root.to_path_buf();

    tail.components().any(|component| {
        current.push(component);
        let Component::Normal(name) = component else {
            return false;
        };
        // `symlink_metadata`, not `metadata`: each component is judged as
        // itself, as the walkers do.
        entry_is_hidden(&name.to_string_lossy(), || {
            std::fs::symlink_metadata(&current).ok()
        })
    })
}

/// Whether `s` names a UNC path, in either spelling. Only *called* on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_unc_string(s: &str) -> bool {
    s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\"))
}

/// `/proc/mounts` types whose every read is a round trip. A FUSE mount is
/// matched by its subtype (`fuse.sshfs`): bare `fuse` and `fuseblk` are any
/// userspace filesystem, local ones included. davfs2 mounts as bare `fuse`,
/// so a WebDAV share is undetectable; `indexing.root_workers` covers it.
///
/// Not listed: `fuse.rclone`, `fuse.gvfsd-fuse`, `ceph` and
/// `fuse.glusterfs`. The worker count is only a speed choice, so a type joins
/// once `walkprobe` shows 16 workers beating 4 on it, and none has been
/// measured (2026-10-11: no such mount on the dev box). gvfs also serves
/// local phones and cameras from the same mount.
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

#[cfg(target_os = "linux")]
pub(crate) fn is_network_path(path: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    mounts_say_network(&mounts, &target)
}

/// Whether the mount serving `target` is a network one, by the
/// `/proc/mounts` text `mounts`. That mount is at the longest mount point
/// that is a prefix of `target` and, of the mounts there, the last one
/// listed, which is the mount on top: an automount lists its `autofs`
/// trigger before the share stacked on it. A root that is itself an
/// untriggered automount point reads as `autofs`, so local; triggering it
/// here would block on a hung share.
#[cfg(target_os = "linux")]
fn mounts_say_network(mounts: &str, target: &Path) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_dev), Some(point), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let point = unescape_mount_field(point);
        let point = Path::new(&point);
        if !target.starts_with(point) {
            continue;
        }
        let depth = point.components().count();
        let is_network = NETWORK_FS_TYPES.contains(&fstype);
        if best.is_none_or(|(d, _)| depth >= d) {
            best = Some((depth, is_network));
        }
    }
    best.is_some_and(|(_, is_network)| is_network)
}

/// A `/proc/mounts` field with every `\ooo` escape decoded to its byte. The
/// kernel writes space, tab, newline and backslash that way (`\040`, `\011`,
/// `\012`, `\134`), three octal digits for one byte, so `\4xx` and up are no
/// escape and stay as written.
#[cfg(target_os = "linux")]
fn unescape_mount_field(field: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let src = field.as_bytes();
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let escape = src.get(i + 1..i + 4).filter(|d| {
            src[i] == b'\\' && d[0] <= b'3' && d.iter().all(|b| (b'0'..=b'7').contains(b))
        });
        match escape {
            Some(d) => {
                out.push(d.iter().fold(0u8, |n, b| n * 8 + (b - b'0')));
                i += 4;
            }
            None => {
                out.push(src[i]);
                i += 1;
            }
        }
    }
    std::ffi::OsString::from_vec(out)
}

/// UNC needs no syscall; a *mapped drive letter* is indistinguishable from a
/// local disk by string inspection, and `GetDriveTypeW` is the only way to
/// tell.
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
/// caller registers every directory itself, which lets it skip ignored and
/// hidden subtrees. `true` (`ReadDirectoryChangesW`): one handle covers the
/// subtree, and notify allocates a 16 KiB buffer *inline per watch*, so
/// per-directory registration would ask for gigabytes; pruning moves to the
/// event path. All churn under a root then shares that one buffer, and a
/// burst that outgrows it arrives as a rescan (patched in `vendor/notify`),
/// which the coordinator answers with a full run. macOS FSEvents is also
/// recursive but stays per-directory.
pub const WATCH_ROOTS_RECURSIVELY: bool = cfg!(windows);

/// What ignore patterns compile against: on Windows and macOS `node_modules`
/// has to exclude `Node_Modules`, and on Linux it must not.
pub const PATHS_ARE_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Drop the calling thread to background scheduling priority (the GUI shares
/// this process). Best-effort and idempotent. CPU only, on every platform:
/// Windows' `THREAD_MODE_BACKGROUND_BEGIN` drops I/O to a tier the kernel
/// actively rate-limits, and Linux's `nice` has no I/O half; don't
/// reintroduce it.
pub fn set_background_priority() {
    #[cfg(target_os = "linux")]
    {
        // Per thread on Linux, and absolute: spawned threads inherit their
        // spawner's niceness, so a relative `nice(10)` would stack to 19.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    // Elsewhere (macOS, BSD): `nice` applies to the whole process, so skip.
}

/// Run `f`, then give SIGHUP back the disposition it had before, whatever `f`
/// installed: exit by default, nothing under `nohup`.
pub fn preserving_sighup<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(unix)]
    {
        let mut before: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut before) };
        let out = f();
        unsafe { libc::sigaction(libc::SIGHUP, &before, std::ptr::null_mut()) };
        out
    }
    #[cfg(not(unix))]
    {
        f()
    }
}

/// Stack size for [`spawn_worker`] threads. Kept small because glibc caches
/// freed thread stacks (`stack_cache_maxsize`, 40 MiB default) with their
/// dirty pages for the life of the process, so every worker pays for a
/// larger one. The PDF interpreter's deepest bounded recursion fits it:
/// `extract::pdf`'s `pdf_recursion_bounds_fit_the_worker_stack`.
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

/// What a [`spawn_limited`] child maps beyond the heap it asked for, which
/// `RLIMIT_AS` counts too: the binary, the libraries, the thread stacks and
/// mimalloc's segment reservations around its live heap. Measured with
/// `prlimit --as` on a release sandbox child parsing a PDF
/// (`extract::sandbox`): a small file runs in 48 MiB, and a 16 MB PDF
/// holding 150 MB of heap needs between 192 and 256 MiB, so the overhead
/// above the heap is under this. mimalloc wants a 1 GiB arena but falls
/// back to smaller mappings when the limit refuses one, which is why this is
/// not a gigabyte.
#[cfg(unix)]
const RESERVED_ADDRESS_SPACE: u64 = 256 << 20;

/// What keeps a [`spawn_limited`] child's limits in force: on Windows its job
/// object, which kills the child once closed. Hold it until the child has
/// been waited for.
pub struct ChildLimits {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for ChildLimits {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
    }
}

/// Spawn `command` with its CPU time capped at `cpu` and the memory it may
/// allocate at `memory`, writing no core file: for a child that parses
/// untrusted input ([`crate::extract::sandbox`]). Past the CPU limit
/// the OS kills it; past the memory limit its allocations fail, which aborts
/// it.
///
/// Unix: `setrlimit` between fork and exec, so the limits hold from the
/// child's first instruction, each lowered to the hard limit already in
/// force. Memory is `RLIMIT_AS`: `RLIMIT_DATA` would count mimalloc's arena
/// reservation the same way (it is mapped writable up front) and miss
/// nothing more. On Linux the child also dies with the thread that spawned
/// it (`PR_SET_PDEATHSIG`), so no parse outlives the process.
///
/// Windows: a job object of the child's own, with per-process memory
/// (commit) and user-time limits, killing the child when [`ChildLimits`]
/// closes it or this process exits. The child runs before it can be put in
/// the job, so its stdin is replaced by a pipe closed only once it is in:
/// the child must read stdin to EOF before anything else. It runs at below
/// normal priority.
pub fn spawn_limited(
    command: &mut std::process::Command,
    cpu: std::time::Duration,
    memory: u64,
) -> std::io::Result<(std::process::Child, ChildLimits)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let limits = [
            (libc::RLIMIT_CPU, cpu.as_secs().max(1)),
            (libc::RLIMIT_CORE, 0),
            (
                libc::RLIMIT_AS,
                memory.saturating_add(RESERVED_ADDRESS_SPACE),
            ),
        ]
        .map(|(resource, value)| {
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let hard = if unsafe { libc::getrlimit(resource, &mut current) } == 0 {
                current.rlim_max
            } else {
                libc::RLIM_INFINITY
            };
            let value = (value as libc::rlim_t).min(hard);
            (
                resource,
                libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                },
            )
        });
        #[cfg(target_os = "linux")]
        let parent = std::process::id();
        // SAFETY: only `setrlimit`, `prctl` and `getppid` run between fork
        // and exec, and each is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                for (resource, limit) in &limits {
                    if libc::setrlimit(*resource, limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // Killed with the thread that spawned it, which waits on it
                // until it exits, so an indexer that dies or is killed leaves
                // no parse running. A parent gone before this call is caught
                // by the check after it.
                #[cfg(target_os = "linux")]
                {
                    let signal = libc::SIGKILL as libc::c_ulong;
                    if libc::prctl(libc::PR_SET_PDEATHSIG, signal) != 0
                        || libc::getppid() as u32 != parent
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok((command.spawn()?, ChildLimits {}))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_PROCESS_TIME,
        };
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let limits = ChildLimits { job };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // In 100 ns units.
        info.BasicLimitInformation.PerProcessUserTimeLimit =
            i64::try_from(cpu.as_nanos() / 100).unwrap_or(i64::MAX);
        info.ProcessMemoryLimit = usize::try_from(memory).unwrap_or(usize::MAX);
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Below normal, as the content workers parsed in-process
        // ([`set_background_priority`]); on Linux the child inherits the
        // spawning thread's `nice`.
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as isize) } == 0 {
            let e = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        // In the job: the child may start.
        drop(child.stdin.take());
        Ok((child, limits))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (cpu, memory);
        Ok((command.spawn()?, ChildLimits {}))
    }
}

/// The allocator every shipped binary, bench and timing probe installs.
///
/// Re-exported because `#[global_allocator]` only takes effect in the crate
/// that *declares* it: a library cannot choose one for its dependents. So
/// each of them names this type (the counting harnesses wrap it), and they
/// must all name the same one: a measurement harness left on the system
/// allocator would report numbers for a build nobody ships.
///
/// ```ignore
/// #[global_allocator]
/// static GLOBAL: quicksearch_core::platform::Allocator =
///     quicksearch_core::platform::Allocator;
/// ```
///
/// Why not glibc: it gives each thread a 64 MiB arena and never shrinks one
/// below its high-water mark. A multi-million-file run settled at 985 MB RSS,
/// 871 MB of it anonymous slack that `malloc_trim` could not coalesce (it
/// returns only pages that are wholly free, and one live chunk pins 4 KiB).
/// Capping arenas at 2 cut the floor to 146 MB but made indexing dramatically
/// slower, because two arenas serialise every worker. mimalloc has
/// per-thread heaps with no lock on the fast path and decommits freed
/// segments.
///
/// It serves Rust allocations only. It is built without `override`, so the C
/// libraries (SQLite's page cache, OpenSSL, zstd) still allocate from the
/// system `malloc`.
pub use mimalloc::MiMalloc as Allocator;

/// Return freed heap to the OS. Costs milliseconds, so call it only at idle
/// points such as the end of a run, whose freed peak would otherwise stay
/// resident.
pub fn release_free_heap() {
    // `libmimalloc-sys` binds this only behind its `extended` feature, which
    // adds the `cty` crate for one declaration, so it is declared here. The
    // symbol is in the static library that crate links for `mimalloc`. It
    // reaches only what mimalloc holds, not the system `malloc` the C
    // libraries use (see `Allocator`).
    extern "C" {
        /// `void mi_collect(bool force)`. C `_Bool` and Rust `bool` are the
        /// same one byte.
        fn mi_collect(force: bool);
    }
    // SAFETY: no arguments of ours, no state of ours, callable from any
    // thread. `true` asks it to return memory to the OS rather than merely to
    // mimalloc's own free lists, the whole point of the call.
    unsafe { mi_collect(true) };
}

/// Create `dir` and its parents, readable only by their owner:
/// `create_dir_all` leaves the mode to the umask (022 ⇒ world `+rx`) and
/// these directories hold the index and config. Only directories *created
/// here* are narrowed: a user who chose `~/Documents` as their data
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

/// `parent/<prefix>-<euid>`, made `0700` when missing, and returned only
/// when it is a directory this user owns with no group or other access. For
/// a `parent` other users can write to, such as the temporary directory:
/// whoever takes the name first decides what is there, and another user's
/// directory, an open one or a symlink would let them answer in our place or
/// aim our writes at a file of their choosing. Each of those is
/// `PermissionDenied`. Named by the effective uid, so it needs no `$USER`.
#[cfg(unix)]
pub fn private_dir_under(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    // SAFETY: no arguments, and the call cannot fail.
    let uid = unsafe { libc::geteuid() };
    let dir = parent.join(format!("{}-{}", prefix, uid));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    // Not followed: a link is refused for being one, whatever it points at.
    let meta = std::fs::symlink_metadata(&dir)?;
    if meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0 {
        Ok(dir)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory of this user's",
                dir.display()
            ),
        ))
    }
}

/// Narrow `path` to owner-only access, best effort; the index is a strictly
/// larger secret than any single file it was read from. Only `path` itself:
/// SQLite copies the database's mode to a `-wal`/`-shm` it creates later, but
/// not to one that already exists, so `db::open::narrow_index_files` narrows
/// those explicitly. Failure is ignored: a FAT stick or share has no
/// permissions to set, and a sidecar may not exist.
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

/// Open `path` for reading, refusing anything that is not a regular file, and
/// return the handle with its `fstat`. A rename since the caller's `stat` can
/// put a FIFO or device there, and `open` on one blocks in the kernel until a
/// writer appears, uninterruptibly and past any stop flag, stranding a walk
/// worker and the pool behind it. `O_NONBLOCK` makes the open return; `fstat`
/// on the descriptor then answers un-swappably, for the file the handle
/// reads. Both are free on a regular file.
pub fn open_regular_file(path: &Path) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((file, meta))
}

/// What one `stat` of a shown row's path says ([`presence`]).
#[derive(Debug)]
pub(crate) enum Presence {
    /// A regular file, links followed.
    File(std::fs::Metadata),
    /// Nothing there, or no longer a regular file.
    Gone,
    /// The `stat` failed (permissions, I/O, a stale network handle), which
    /// is no evidence either way: a dropped share must not read as deleted.
    Unknown,
}

/// One `stat` of `path`; only [`says_gone`] errors say the file left.
pub(crate) fn presence(path: &Path) -> Presence {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Presence::File(meta),
        Ok(_) => Presence::Gone,
        Err(e) if says_gone(&e) => Presence::Gone,
        Err(_) => Presence::Unknown,
    }
}

/// Whether a failed `stat` says the file left: `NotFound`, or
/// `NotADirectory` (an ancestor is now a file). Windows reads a missing drive
/// and an unreachable share as `NotFound` too (ERROR_INVALID_DRIVE,
/// ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME), and those say only that the
/// volume is out of reach.
fn says_gone(e: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME, ERROR_INVALID_DRIVE,
        };
        let code = e.raw_os_error().map(|code| code as u32);
        if matches!(
            code,
            Some(ERROR_INVALID_DRIVE | ERROR_BAD_NETPATH | ERROR_BAD_NET_NAME)
        ) {
            return false;
        }
    }
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Why [`IndexLock::acquire`] did not hand back a lock.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it. `pid` is for the message only; it never decides.
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
    /// inert and nothing here may ever branch on its presence; a stale-PID
    /// scheme would strand the user behind a crash. [`LockError::Unsupported`]
    /// means the filesystem could not answer, not that the lock is taken;
    /// callers must start anyway.
    pub fn acquire(db_path: &Path) -> Result<IndexLock, LockError> {
        let path = IndexLock::path_for(db_path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = create_dir_private(dir);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            // An unwritable index directory is not a second instance: carry on.
            .map_err(|e| LockError::Unsupported(format!("{}: {}", path.display(), e)))?;
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

    /// Move the held lock onto the index at `db_path`. The new lock is taken
    /// before the old one is let go, so a refusal leaves this process holding
    /// what it held. [`LockError::Unsupported`] means the move *happened* and
    /// the new path simply cannot be locked; carry on.
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
            Err(e @ LockError::Unsupported(_)) => {
                *slot = None;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    fn record_holder(&self) {
        use std::io::Write;
        // A fresh handle: writing through `_file` would move the shared file
        // offset. That a *second* handle can write here at all is why the
        // Windows lock byte sits at [`LOCK_BYTE_OFFSET`] rather than offset 0:
        // its byte-range locks are mandatory and per-handle, so a lock over
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

/// `None` whenever the file is absent, empty or unparseable, all ordinary.
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
    // `close` this process makes on the same inode (see
    // `file_handling::index_file_set`). A `flock` belongs to the open file
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

    // The locked byte sits at [`LOCK_BYTE_OFFSET`], not at offset 0, where
    // the PID is written. Windows byte-range locks are *mandatory* and
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
/// "unknown", never "zero". The nearest existing directory is asked, since
/// `GetDiskFreeSpaceExW` refuses a file: a file answers for its directory.
pub fn available_space(path: &Path) -> Option<u64> {
    let mut probe = path;
    while !probe.is_dir() {
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
    // is the fragment size the block counts are in; `f_bsize` is the
    // preferred I/O size and the wrong multiplier.
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
/// violation while *any* handle is open, most often an antivirus scanner
/// reading the file microseconds after we closed it. Unix `unlink` needs no
/// retry.
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

/// Deny read access to `dir`, for tests (exposed: `tests/full_index.rs`, a
/// separate crate, calls it). Windows uses `icacls`: a deny ACE binds even the
/// owner until [`restore_read`] rewrites it; neither call needs elevation.
///
/// Fails with [`std::io::ErrorKind::Unsupported`], read access restored, when
/// `dir` still lists afterwards: rights that ignore permission bits (root with
/// its DAC capabilities) would hand the caller a readable tree to test an
/// unreadable one with.
#[doc(hidden)]
pub fn deny_read(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let denied = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000))
    };
    #[cfg(windows)]
    let denied = icacls(
        dir,
        &["/deny", &format!("{}:(OI)(CI)(RD)", current_user()?)],
    );
    #[cfg(not(any(unix, windows)))]
    let denied: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "deny_read is not supported on this target",
    ));
    denied?;
    if std::fs::read_dir(dir).is_ok() {
        restore_read(dir).ok();
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "still readable: this process ignores permission bits",
        ));
    }
    Ok(())
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
