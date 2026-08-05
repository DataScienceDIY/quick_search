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
//!
//! Beyond filesystem and path semantics, this is also where the process's
//! dealings with its own allocator and threads live — [`release_free_heap`]
//! and [`heap_stats`], which are glibc-only, and [`spawn_worker`], which puts
//! the run's worker stack size in one place. They are here for the same reason
//! everything else is: they are `#[cfg]`, and `#[cfg]` lives here.

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

/// `FILE_ATTRIBUTE_HIDDEN`.
///
/// Spelled out rather than imported, for the reason in the module header:
/// `windows-sys` is a `cfg(windows)`-only dependency, and
/// [`attributes_are_hidden`] has to compile — and be tested — on Linux, where
/// the suite runs.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

/// The cross-compiled Windows build is where the spelling above is checked
/// against the real header value, so a typo fails that job rather than quietly
/// indexing `$RECYCLE.BIN`.
#[cfg(windows)]
const _: () = assert!(
    FILE_ATTRIBUTE_HIDDEN == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
);

/// Whether a Windows attribute word marks an entry hidden.
///
/// `FILE_ATTRIBUTE_HIDDEN` and nothing else. `FILE_ATTRIBUTE_SYSTEM` was part
/// of this test and was removed, because it pruned users' cloud folders:
/// Windows honours the `desktop.ini` inside a folder only if the folder itself
/// carries Read-only or System, so the ownCloud, Nextcloud, OneDrive and Google
/// Drive clients set System on their sync root purely to get a branded icon.
/// Such a folder has no Hidden bit and is plainly visible in Explorer, yet the
/// whole subtree vanished from the index with nothing to explain it — only
/// "Index hidden files" brought it back, which is the opposite of what that
/// setting is for. Any folder given a custom icon is the same case.
///
/// Nothing the System term existed for is lost. `$RECYCLE.BIN`, `System Volume
/// Information`, `pagefile.sys` and the legacy per-user junctions (`My
/// Documents`, `Local Settings`, `Application Data`) are Hidden **and** System —
/// that pairing is Windows' own definition of a protected operating system file
/// — and `AppData` is Hidden alone. Hidden catches every one. The drive-root
/// names are excluded by [`crate::config`]'s default ignore patterns as well, so
/// they have two independent reasons to stay out.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_hidden(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
}

/// Why an entry counted as hidden.
///
/// The distinction exists for the walk's log line: a dot prefix explains itself
/// and an ignore pattern is something the user typed, but "this visible folder
/// was skipped over an attribute you cannot see" has no discoverability at all,
/// so only that case is worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenReason {
    DotPrefix,
    Attribute,
}

/// Whether a directory entry counts as hidden, and why.
///
/// Unix: a leading dot. Windows: a leading dot **or** `FILE_ATTRIBUTE_HIDDEN` —
/// without which `include_hidden = false` hides nothing on Windows, and
/// `$RECYCLE.BIN`, `System Volume Information`, `pagefile.sys` and `AppData` all
/// get indexed. `FILE_ATTRIBUTE_SYSTEM` is deliberately not part of it; see
/// [`attributes_are_hidden`].
///
/// `meta` must report the attributes of the entry **itself**, never of a link
/// target — callers pass `symlink_metadata` or an already-cached directory
/// entry. A hidden symlink is a hidden alias; a visible symlink to a hidden
/// target is a visible alias. All four call sites have to agree on that, or a
/// full run indexes a file the watcher then refuses to update and the index
/// churns on every cycle.
///
/// `meta` is a closure because on Unix it is never called: the walkers
/// deliberately avoid `metadata()`, which would cost an extra `lstat` per
/// entry and a full round trip on a network share. On Windows the cost is
/// zero anyway — both `std::fs::DirEntry::metadata` and
/// `walkdir::DirEntry::metadata` hand back data already cached from
/// `FindNextFileW`.
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

/// `FILE_ATTRIBUTE_REPARSE_POINT`. Spelled out for [`FILE_ATTRIBUTE_HIDDEN`]'s
/// reason, and checked against the real header value the same way.
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
/// OneDrive, and every other Files-On-Demand provider, leaves a placeholder on
/// disk with real metadata and no data. Opening one for read is not a local
/// operation: it blocks on a network download of the entire file. The default
/// indexing root is `%USERPROFILE%`, the OneDrive folder beneath it is not
/// hidden, and the walk hashes the first 8 KiB of every new or changed file — so
/// without this test a first index quietly downloads the user's whole cloud
/// drive, filling their disk with the files they had deliberately offloaded.
///
/// Named for the question the caller is actually asking ("will reading this
/// cost a download?") rather than for the bits, because the answer is what the
/// indexing path branches on. Always `false` off Windows: no other platform this
/// runs on has an equivalent, and a `stat` there tells the truth about the data.
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
/// exercises it. See this module's second rule.
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
/// Split out from [`entry_cached_metadata`] so the bit test is exercised by the
/// Linux suite, per this module's second rule. Deliberately *not* the same
/// question as `FileType::is_symlink`, which additionally requires the
/// name-surrogate bit: see [`entry_cached_metadata`] for why that distinction is
/// the whole point.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attributes_are_reparse_point(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Metadata a directory read already handed back, on the platforms that hand
/// any back at all.
///
/// `std::fs::Metadata` on Windows: `FindFirstFileW`/`FindNextFileW` return size,
/// timestamps and attributes with every name, and `std::fs::DirEntry::metadata`
/// is a copy of that buffer rather than a syscall. A `fs::metadata(path)` on the
/// same entry is therefore a whole extra `CreateFileW` +
/// `GetFileInformationByHandle` + `CloseHandle` — an `IRP_MJ_CREATE` through
/// every antivirus and EDR minifilter on the machine — for data already in hand.
///
/// Uninhabited everywhere else, because `getdents64` returns only `d_type`: size
/// and mtime genuinely need the `statx`, and `DirEntry::metadata` there is a
/// *second* syscall rather than a saved one. Uninhabited rather than a unit
/// struct so `Option<CachedMetadata>` is zero-sized off Windows — the walker
/// queues one per pending file and nothing throttles file chunks (see
/// `walk::Job::Files`), so carrying `Option<std::fs::Metadata>` would cost Linux
/// 176 bytes per queued file to hold nothing at all.
#[cfg(windows)]
pub(crate) type CachedMetadata = std::fs::Metadata;
#[cfg(not(windows))]
pub(crate) type CachedMetadata = std::convert::Infallible;

/// The zero-cost half of the claim above, enforced rather than asserted in
/// prose: a layout change in a future toolchain becomes a compile error instead
/// of a silent regression in the walker's memory profile.
#[cfg(not(windows))]
const _: () = assert!(std::mem::size_of::<Option<CachedMetadata>>() == 0);

/// The metadata a directory read already produced for one entry — but only
/// where trusting it is both free *and* indistinguishable from a fresh `stat`.
///
/// `meta` is a closure for exactly [`entry_hidden_reason`]'s reason: on Unix it
/// is never called, because `std::fs::DirEntry::metadata` there is a real
/// `lstat` and the walk spends exactly one `statx` per file by design.
///
/// `None` for a reparse point even on Windows, and that is the whole subtlety.
/// `fs::metadata` *follows* a reparse point; the cached buffer describes the
/// link itself. The tags std does not classify as symlinks reach the walk's
/// ordinary file arm — `IO_REPARSE_TAG_APPEXECLINK`, the zero-byte Store app
/// stubs under `WindowsApps`, and the OneDrive `IO_REPARSE_TAG_CLOUD_*` family —
/// and for those the two answers differ: today an AppExecLink fails
/// `CreateFileW` with `ERROR_CANT_ACCESS_FILE` and is skipped, while its
/// directory entry looks like an ordinary empty file and would get indexed.
/// Sending every reparse point back to the path-based `fs::metadata` keeps the
/// semantics identical and leaves only the common case on the fast path.
///
/// Tested against the raw `FILE_ATTRIBUTE_REPARSE_POINT` bit, deliberately not
/// against `file_type().is_symlink()`: std's `is_symlink` additionally requires
/// the name-surrogate bit `0x20000000`, which is precisely the test that lets
/// AppExecLink and the cloud tags through.
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
/// supplied it and from a `stat` where it did not.
///
/// The `#[cfg]` lives here rather than at the call site, per this module's first
/// rule. Off Windows `cached` is uninhabited and therefore provably `None`,
/// which is why the walk still costs exactly one `statx` per file there.
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

/// Whether this platform's filesystem matches names without regard to case.
///
/// What ignore patterns compile against: on Windows and macOS `node_modules`
/// has to exclude `Node_Modules`, and on Linux it must not. Named here rather
/// than spelled `cfg!(any(windows, target_os = "macos"))` at each use, so the
/// two places that must agree — [`crate::config::IgnoreSet`]'s literal set and
/// its glob set — cannot drift apart.
pub const PATHS_ARE_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

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
///
/// **CPU only, on every platform.** This used to be
/// `THREAD_MODE_BACKGROUND_BEGIN` on Windows, which is background *mode*: it
/// lowers the thread to base priority 4 and drops it to `IoPriorityVeryLow`, a
/// tier the kernel does not merely deprioritise but actively rate-limits — the
/// same one SuperFetch and defrag run in. Linux's `nice` has no I/O half at all
/// under the usual schedulers, so the two platforms were not doing remotely the
/// same thing: every walker, prefetcher, extractor *and* the single SQLite
/// writer thread were throttled on Windows and full-speed on Linux, which is
/// most of why Windows indexing was so much slower. Matching `nice`'s CPU-only
/// semantics is the deliberate choice; a machine that genuinely needs I/O
/// throttling needs it as a user-visible setting, not as a silent per-platform
/// difference.
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
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // Yields the CPU to the foreground without touching I/O priority — the
        // closest Windows equivalent of `nice(10)`. See the note above for why
        // this is not `THREAD_MODE_BACKGROUND_BEGIN`.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    // Elsewhere (macOS, BSD): deliberately nothing. `nice` there applies to the
    // whole process, so it would hit the GUI. The right call is
    // `pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0)`, which is worth
    // adding on its own terms rather than approximating here.
}

/// Stack size for the per-run worker threads spawned by [`spawn_worker`].
///
/// Rust's default is 8 MiB of *reserved* address space, of which only touched
/// pages become resident — so on its own the default costs nothing much. What
/// costs is that glibc **caches freed thread stacks** rather than unmapping
/// them (`stack_cache_maxsize`, 40 MiB by default) and a cached stack keeps
/// its dirty pages. A run spawns walk and extraction workers per root, so the
/// pages those threads touched outlive them and sit in that cache for the life
/// of the process.
///
/// 512 KiB is ample for what these threads actually do: bounded loops over a
/// directory's entries and over a batch of rows. The one thing here that
/// recurses on untrusted input is document parsing — OLE2 compound files and
/// zip/XML containers — and that runs on extraction workers, so if a malformed
/// document ever overflows this it should be raised rather than reverted, and
/// the parser given a depth limit.
const WORKER_STACK_SIZE: usize = 512 * 1024;

/// Spawn one of a run's short-lived worker threads.
///
/// Exists to put [`WORKER_STACK_SIZE`] in one place rather than at each of the
/// four spawn sites, and to name the threads while it is at it — `qs-walk`,
/// `qs-extract` and friends show up in `top -H` and in a debugger, which the
/// anonymous `thread::spawn` versions did not.
///
/// Panics if the thread cannot be spawned, exactly as `thread::spawn` does: a
/// run that cannot start its workers has nothing to fall back to.
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
/// only the top of an arena is ever trimmed, and only past `M_TRIM_THRESHOLD`.
/// So a transient peak stays in RSS for the life of the process even though
/// nothing is using it. Indexing peaks around 200 MiB above baseline on a
/// large root (see `examples/memprobe.rs`), and a full-table scan fills a
/// connection's SQLite page cache with 4 KiB allocations that are individually
/// far below the mmap threshold — both land in an arena and stay there. This
/// is what gives them back.
///
/// Process-wide despite being one call: `malloc_trim(0)` walks *every* arena,
/// so calling it on the coordinator's thread also reclaims what the search
/// worker and the finished indexing threads left behind. That is why there are
/// only two call sites rather than one per subsystem — and why it must not go
/// anywhere hot, since the walk plus the `madvise` per free page costs
/// milliseconds on a large heap.
///
/// Best-effort and idempotent, like [`set_background_priority`]: a refusal
/// means only that the memory stays where it was.
pub fn release_free_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: no arguments, no pointers, and safe to call from any thread
        // at any time — glibc takes the arena locks itself.
        unsafe { libc::malloc_trim(0) };
    }
    // Elsewhere: deliberately nothing. `malloc_trim` is a glibc extension —
    // musl has no equivalent and does not need one (its allocator returns
    // spans to the kernel on free), and the Windows CRT heap has
    // `_heapmin`, which is worth adding on its own terms if Windows RSS ever
    // proves to be a problem rather than assuming it behaves like glibc.
}

/// Live and free-but-retained heap bytes, as `(in_use, free)`.
///
/// The gap between the two *is* the retention this module's
/// [`release_free_heap`] exists to close: `in_use` is memory something still
/// holds, `free` is memory the program has already given back to the allocator
/// and which glibc is nonetheless still charging the process for. A large
/// `free` immediately after a trim is the signal that `malloc_trim` cannot
/// reach the fragmentation and a different allocator is the answer.
///
/// `None` where the platform has no way to answer.
pub fn heap_stats() -> Option<(u64, u64)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // `mallinfo2`, not `mallinfo`: the older struct is `int`-typed and
        // silently wraps past 2 GiB, which is exactly the size where the
        // answer starts to matter.
        //
        // SAFETY: no arguments, returns a plain struct by value.
        let info = unsafe { libc::mallinfo2() };
        Some((info.uordblks as u64, info.fordblks as u64))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
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

    /// The attribute half of `entry_is_hidden`, which the Windows arm cannot
    /// be asked about from Linux. Split out precisely so this test runs
    /// everywhere.
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

    /// Regression: a cloud sync root carries System and *not* Hidden — Windows
    /// will not honour the `desktop.ini` supplying its branded icon otherwise —
    /// and is fully visible in Explorer. Keying on System pruned the folder and
    /// every file beneath it, silently, and only "include hidden files" brought
    /// it back.
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
}
