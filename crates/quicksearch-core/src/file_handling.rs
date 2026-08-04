use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
// Only the Unix entry-count path shells out; Windows walks the tree directly.
use std::collections::HashMap;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::config::{Config, IgnoreSet};
use crate::db::repo::{self, NewFile};
use crate::extract::Registry;
use crate::indexing::should_abort;
use crate::mime::{guess_mime_from_head, mime_to_type, FileType};

/// One directory's indexed files, as `name -> mtime`.
///
/// The unit classification works in. Names, not full paths: the parent is
/// implied by which directory is being walked, and at millions of files the
/// repeated directory prefix is the whole cost. Produced by
/// [`crate::db::repo::dir_rows`].
pub type DirRows = HashMap<String, u64>;

/// Derive (inode, device_id) from a `std::fs::Metadata` on platforms that
/// expose them. Returns `(None, None)` on Windows and other non-Unix targets.
fn inode_and_device(_meta: &std::fs::Metadata) -> (Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (Some(_meta.ino()), Some(_meta.dev()))
    }
    #[cfg(not(unix))]
    {
        (None, None)
    }
}

/// Render a path as the string stored in `files.path`.
///
/// `Path::canonicalize` on Windows hands back extended-length paths; the
/// index stores plain ones. A UNC share canonicalizes to
/// `\\?\UNC\server\share`, so the two prefixes have to be stripped
/// differently — taking four characters off both leaves `UNC\server\share`,
/// which is not a path that exists.
///
/// A volume mounted at a folder rather than a drive letter has no DOS name, so
/// it canonicalizes to `\\?\Volume{GUID}\…`. Stripping the prefix there yields
/// `Volume{GUID}\…`, which cannot be opened — every file under such a mount
/// would fail to hash. Only a genuine drive letter is safe to un-prefix.
pub(crate) fn path_to_db_string(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = s
        .strip_prefix(r"\\?\")
        .filter(|r| starts_with_drive_letter(r))
    {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// Warn and report `true` for a path that cannot round-trip through
/// `files.path`.
///
/// `files.path` is a TEXT column, and everything downstream of the walk
/// reopens the file by that string: hashing, MIME sniffing, text extraction,
/// and opening a result from the GUI. [`path_to_db_string`] goes through
/// `to_string_lossy`, so a name that is not valid UTF-8 arrives with its bad
/// bytes replaced by U+FFFD and then names a file that does not exist. Such a
/// file is skipped whole rather than stored under a path nothing can reopen —
/// otherwise the first symptom is the hasher reporting "No such file or
/// directory" for a file the walk just stat'ed successfully, which reads like
/// a race or a broken share rather than a name that cannot be represented.
///
/// The message spells the offending bytes out as `\xNN` (via `Debug`, which is
/// portable — no `#[cfg]` and no `OsStrExt`): a replacement character is easy
/// to miss in a terminal and vanishes entirely once the line has been copied
/// and pasted somewhere else.
pub(crate) fn warn_if_unrepresentable(path: &Path) -> bool {
    if path.to_str().is_some() {
        return false;
    }
    crate::log_warn!(
        "Skipping file (name is not valid UTF-8, so it cannot be hashed \
         or text-indexed): {:?}",
        path
    );
    true
}

/// Whether `s` begins `X:` for some ASCII letter — the only `\\?\` payload
/// that is still a usable path once the prefix is gone.
fn starts_with_drive_letter(s: &str) -> bool {
    let mut it = s.chars();
    matches!((it.next(), it.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
}

/// The `files.path` key for a path that may no longer exist.
///
/// The insert side canonicalizes before storing, so a lookup that skips that
/// step compares two different spellings of the same file and silently matches
/// nothing. A `Remove` event names something already gone, though, so plain
/// `canonicalize` fails on it unconditionally — instead this canonicalizes the
/// deepest ancestor that still resolves and re-joins the missing tail.
///
/// Cross-platform, not just Windows: on Linux a root reached through a
/// symlinked parent (`/home` → `/mnt/home`) makes every removal a no-op
/// without this.
pub fn db_key_for_missing_path(path: &Path) -> String {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;

    loop {
        if let Ok(resolved) = cursor.canonicalize() {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return path_to_db_string(&out);
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cursor = parent;
            }
            // Nothing above resolves (a bare relative name, or a root that is
            // itself gone) — the raw spelling is the best key available.
            _ => return path_to_db_string(path),
        }
    }
}

/// Parent directory of a path as a UTF-8 string, empty if root.
fn parent_str(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Paths a walk could not read, collected as it runs.
///
/// A full run deletes index rows for everything it did not see, so "I could
/// not read this directory" and "this directory's files are gone" must not
/// look alike — an unplugged drive or a network share that blips would
/// otherwise silently delete that whole subtree. See
/// [`UnreadableDirs::covers`] and the stale-entry guard in `run_indexing`.
///
/// Shared behind a mutex because the walk that fills it is threaded.
#[derive(Debug, Default)]
pub struct UnreadableDirs {
    dirs: Mutex<Vec<std::path::PathBuf>>,
}

impl UnreadableDirs {
    pub fn record(&self, path: std::path::PathBuf) {
        self.dirs.lock().unwrap().push(path);
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.lock().unwrap().is_empty()
    }

    pub fn paths(&self) -> Vec<std::path::PathBuf> {
        self.dirs.lock().unwrap().clone()
    }

    /// Whether `path` lies under a directory the walk failed to read, and so
    /// must not be treated as deleted.
    ///
    /// Compares by path component, not by string prefix: `/a/bc` does not
    /// live under `/a/b`.
    pub fn covers(&self, path: &str) -> bool {
        let dirs = self.dirs.lock().unwrap();
        if dirs.is_empty() {
            return false;
        }
        let path = Path::new(path);
        dirs.iter().any(|d| path.starts_with(d))
    }
}

/// Whether a walked entry survives the hidden/ignore filters.
///
/// The single definition of "would we index this", shared by
/// [`filtered_walk`], [`filtered_dirs`], and the watcher's decision to
/// register a newly created directory. Keeping one predicate is what stops
/// the walker and the watcher from disagreeing about which subtrees exist.
///
/// Used as a `walkdir` `filter_entry` predicate, so returning `false` for a
/// directory prunes the whole subtree instead of merely skipping the entry.
fn walk_filter(e: &DirEntry, include_hidden: bool, ignore: &IgnoreSet) -> bool {
    // Depth 0 is the root itself — a `false` here would silence the
    // entire walk, and users explicitly chose their roots.
    if e.depth() == 0 {
        return true;
    }
    let name = e.file_name().to_string_lossy();
    // Free on Windows (walkdir hands back the attributes `FindNextFileW`
    // already returned) and never called on Unix, so the "no extra lstat"
    // property below still holds.
    if !include_hidden && crate::platform::entry_is_hidden(&name, || e.metadata().ok()) {
        return false;
    }
    !ignore.matches_component(&name) && !ignore.matches_path_pattern(e.path())
}

/// The shared walk behind [`filtered_walk`] and [`filtered_dirs`]: prunes
/// hidden and ignored subtrees *before* descending, and records what it
/// could not read.
///
/// Directories that cannot be read are recorded in `failures` rather than
/// silently skipped, so the caller can tell an unreadable subtree apart
/// from a deleted one.
///
/// Unlike the parallel walker's symlink resolution, entries here never grow
/// a Windows `\\?\` prefix: walkdir does not canonicalize — every yielded
/// path is `root` plus name components — and the callers pass plainly-spelled
/// roots, so `walk_filter`'s full-path ignore matching sees the same spelling
/// the patterns use.
fn walk_entries<'a>(
    root: &str,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &'a IgnoreSet,
    failures: &'a UnreadableDirs,
) -> impl Iterator<Item = DirEntry> + 'a {
    WalkDir::new(root)
        .follow_links(follow_symlinks)
        .into_iter()
        .filter_entry(move |e| walk_filter(e, include_hidden, ignore))
        .filter_map(move |res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                // Record what we could not read. Dropping this on the floor
                // is what turns a transient mount failure into a deletion.
                if let Some(p) = err.path() {
                    crate::log_warn!("cannot read {}: {}", p.display(), err);
                    failures.record(p.to_path_buf());
                } else {
                    crate::log_warn!("walk error: {}", err);
                }
                None
            }
        })
}

/// Walk `root` yielding only files, pruning hidden and ignored subtrees
/// *before* descending into them. This is the single choke point for "what
/// exists" during full indexing; watcher events apply the same rules via
/// [`IgnoreSet::matches_path`] + [`crate::platform::path_has_hidden_component_under`].
pub fn filtered_walk<'a>(
    root: &str,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &'a IgnoreSet,
    failures: &'a UnreadableDirs,
) -> impl Iterator<Item = DirEntry> + 'a {
    // `file_type` is the cached `d_type` from the directory read, so
    // this costs nothing; `metadata()` would be an extra `lstat` per
    // entry, which on a network share is a full round trip.
    walk_entries(root, follow_symlinks, include_hidden, ignore, failures)
        .filter(|entry| !entry.file_type().is_dir())
}

/// Walk `root` yielding only directories, using the same pruning as
/// [`filtered_walk`]. The watcher registers one inotify watch per yielded
/// directory — inotify has no recursive mode, so the set this returns is
/// exactly the set of watch descriptors a root costs.
pub fn filtered_dirs<'a>(
    root: &str,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &'a IgnoreSet,
    failures: &'a UnreadableDirs,
) -> impl Iterator<Item = DirEntry> + 'a {
    walk_entries(root, follow_symlinks, include_hidden, ignore, failures)
        .filter(|entry| entry.file_type().is_dir())
}

#[cfg(unix)]
fn parse_wc_l_stdout(bytes: &[u8]) -> Result<usize, String> {
    let s = String::from_utf8_lossy(bytes);
    let token = s
        .trim()
        .split_whitespace()
        .next()
        .ok_or_else(|| "wc: empty output".to_string())?;
    token
        .parse()
        .map_err(|e| format!("wc: invalid count {:?}: {}", token, e))
}

/// Poll interval while waiting on count subprocesses; the granularity of
/// cancellation.
#[cfg(unix)]
const COUNT_POLL_MS: u64 = 50;

/// Wait for `terminal` (the last process in the pipeline) while honouring
/// `cancel`: on cancellation every process in `children` is killed and a
/// recognizable error is returned. On normal exit, returns the terminal
/// child's stdout.
#[cfg(unix)]
fn wait_pipeline_cancellable(
    children: &mut [&mut std::process::Child],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>, String> {
    use std::sync::atomic::Ordering;
    loop {
        if cancel.load(Ordering::Relaxed) {
            for child in children.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err("count cancelled".to_string());
        }
        let terminal = children.last_mut().expect("pipeline has processes");
        match terminal.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(format!("count pipeline exited with {}", status));
                }
                // The terminal child's output is a couple dozen bytes
                // (a `wc -l` figure), so reading after exit can't deadlock.
                let mut out = Vec::new();
                if let Some(stdout) = terminal.stdout.take() {
                    use std::io::Read;
                    let mut stdout = stdout;
                    let _ = stdout.read_to_end(&mut out);
                }
                // Reap the rest of the pipeline.
                for child in children.iter_mut() {
                    let _ = child.wait();
                }
                return Ok(out);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(COUNT_POLL_MS)),
            Err(e) => return Err(format!("count wait: {}", e)),
        }
    }
}

#[cfg(unix)]
fn count_find_pipe_wc(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
    printf_newlines: bool,
) -> Result<usize, String> {
    let mut find_cmd = Command::new("find");
    find_cmd.arg(path);
    if printf_newlines {
        // GNU find: emit one newline per entry without formatting paths.
        find_cmd.arg("-printf").arg("\n");
    }
    let mut find = find_cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("find: {}", e))?;
    let find_stdout = find.stdout.take().ok_or("find: stdout")?;
    let mut wc = match Command::new("wc")
        .arg("-l")
        .stdin(find_stdout)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(wc) => wc,
        Err(e) => {
            let _ = find.kill();
            let _ = find.wait();
            return Err(format!("wc: {}", e));
        }
    };
    let out = wait_pipeline_cancellable(&mut [&mut find, &mut wc], cancel)?;
    parse_wc_l_stdout(&out)
}

/// Count tree entries with a plain directory walk.
///
/// Windows has no `find`, and the obvious substitute — `powershell.exe -Command
/// "(Get-ChildItem -Recurse | Measure-Object).Count"` — is a poor trade: 300+
/// ms of interpreter startup before any work, `Get-ChildItem -Recurse` is far
/// slower than a `FindNextFileW` loop, it pops a console window on a windowed
/// process, and the path has to be escaped into a script string. Walking
/// directly is faster, quieter, and cancels immediately instead of at the
/// 50 ms subprocess-poll granularity.
#[cfg(windows)]
fn count_tree_entries_native(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    use std::sync::atomic::Ordering;

    let mut n = 0usize;
    // Unreadable subtrees are skipped rather than fatal: this is a progress
    // estimate, and the walk proper reports what it could not read.
    for entry in WalkDir::new(path).min_depth(1) {
        // Checked every entry rather than every Nth: a relaxed load is far
        // cheaper than the directory read that produced the entry, and it
        // makes cancellation immediate instead of merely prompt.
        if cancel.load(Ordering::Relaxed) {
            return Err("count cancelled".to_string());
        }
        if entry.is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// Rough tree entry count for progress totals. Setting `cancel` stops it
/// rather than letting it scan an entire root after the run stopped: on Unix
/// that kills the `find`/`wc` subprocesses at ~50 ms granularity, on Windows
/// the native walk notices almost immediately. Runs concurrently with
/// indexing — its scope is not identical to the walker's classified file
/// count.
pub fn count_tree_entries_fast(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    #[cfg(windows)]
    {
        return count_tree_entries_native(path, cancel);
    }
    #[cfg(all(unix, target_os = "linux"))]
    {
        return count_find_pipe_wc(path, cancel, true).or_else(|e| {
            if e.contains("cancelled") {
                Err(e)
            } else {
                // Non-GNU find without -printf: plain listing.
                count_find_pipe_wc(path, cancel, false)
            }
        });
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        return count_find_pipe_wc(path, cancel, false);
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("tree entry count is not supported on this target".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIndexAction {
    Skip,
    Update,
    Insert,
}

/// Decide what Phase 1 should do with a file, given its name within the
/// directory being walked and the file's mtime.
///
/// Pure: the caller supplies both the `stat` result and the directory's rows
/// rather than this function going to disk or to SQLite for them, so the same
/// `stat` serves classification and the record build, and this runs on any
/// worker thread against data the prefetcher already fetched.
///
/// Keyed by name against one directory rather than by full path against the
/// whole index: see [`DirRows`]. A file the walk reaches under a spelling
/// whose parent is *not* this directory — a resolved symlink target — must
/// not be classified here; it would miss and read as [`FileIndexAction::Insert`],
/// and `insert_file`'s `INSERT OR IGNORE` would then silently not update it.
/// Use [`classify_by_mtime`] for those.
pub fn classify_for_indexing(name: &str, mtime: u64, rows: &DirRows) -> FileIndexAction {
    classify_by_mtime(rows.get(name).copied(), mtime)
}

/// The same decision from an already-resolved stored mtime.
///
/// The path [`classify_for_indexing`] funnels into, and the one the walk uses
/// directly for resolved symlink targets, whose stored row is found by exact
/// path instead of by name within a directory.
pub fn classify_by_mtime(stored: Option<u64>, mtime: u64) -> FileIndexAction {
    match stored {
        Some(known) if known == mtime => FileIndexAction::Skip,
        Some(_) => FileIndexAction::Update,
        None => FileIndexAction::Insert,
    }
}

/// Safely truncate a string to at most max_bytes bytes while respecting UTF-8 character boundaries
fn safe_truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    // Find the last valid UTF-8 character boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }

    s[..end].to_string()
}

/// Identify a file as `sha256(size || first hash_length bytes)`, returning
/// the head bytes alongside the digest so the caller can sniff a MIME type
/// without opening the file again.
///
/// Only the head is read. Reading a tail block too would mean a seek and a
/// second, non-contiguous read — on a network share that is an extra round
/// trip per file that readahead cannot hide, and files are the unit we
/// process millions of.
///
/// The cost is a collision class: two files of identical size whose first
/// `hash_length` bytes match hash identically. In practice that means
/// pre-allocated VM disk images — a fixed-size VHD keeps its unique footer
/// at the *end* of the file by design, and a freshly pre-allocated raw,
/// qcow2, or flat VMDK image is zeros at the head until it is partitioned.
/// Such files are reported as duplicates when they are not. Duplicate
/// listing is advisory (see `search::duplicates`), so this is a display
/// artifact rather than a correctness problem.
fn get_file_hash(
    size: u64,
    path: &Path,
    hash_length: usize,
) -> Result<(Vec<u8>, Vec<u8>), std::io::Error> {
    let mut f: File = File::open(path)?;
    // Files shorter than the window hash whole; `min` keeps the cast sound
    // for large files on 32-bit targets.
    let mut head = vec![0u8; size.min(hash_length as u64) as usize];
    f.read_exact(&mut head)?;

    let mut hasher = Sha256::new();
    hasher.update(&size.to_le_bytes());
    hasher.update(&head);
    Ok((hasher.finalize().to_vec(), head))
}

/// Nudge FTS5 to merge its index segments. Best-effort: the only failure
/// possible is logged and swallowed, so there is nothing for a caller to
/// handle — hence no `Result`.
pub fn fts_finalize_after_text_indexing(conn: &Connection) {
    if let Err(e) = conn.execute(
        "INSERT INTO searchabletext(searchabletext, rank) VALUES('automerge', 8)",
        [],
    ) {
        crate::log_warn!("FTS automerge failed (non-fatal): {}", e);
    }
}

/// An owned, fully-derived file record: everything needed to insert or
/// update a `files` row, produced by [`prepare_file_record`].
#[derive(Debug, Clone)]
pub struct OwnedNewFile {
    pub name: String,
    pub path: String,
    pub parent: String,
    pub size: u64,
    pub mtime: u64,
    pub inode: Option<u64>,
    pub device_id: Option<u64>,
    pub mime: Option<String>,
    pub ftype: FileType,
    pub hash: Vec<u8>,
    /// Text extracted from the head bytes during the walk, for files small
    /// enough that the head *was* the whole file. `Some` means the content
    /// pass never has to open this file; `None` leaves it pending as before.
    ///
    /// Only plaintext files at or below `hash_length` (8 KiB by default) carry
    /// one, and the walk's channel is bounded at `CHANNEL_CAP`, so this adds
    /// at most `CHANNEL_CAP * hash_length` of in-flight memory.
    pub inline_text: Option<String>,
    /// Whether the content pass has anything to do for this file — see
    /// [`content_extractable`], plus the `maximum_text_file_size` gate. `false`
    /// means the row is born `STATE_NA`, so "pending" counts only real work and
    /// the pass never queues a file just to write it off.
    pub needs_content: bool,
}

impl OwnedNewFile {
    pub fn as_new_file(&self) -> NewFile<'_> {
        NewFile {
            name: &self.name,
            path: &self.path,
            parent: &self.parent,
            size: self.size,
            mtime: self.mtime,
            inode: self.inode,
            device_id: self.device_id,
            mime: self.mime.as_deref(),
            ftype: self.ftype,
            hash: Some(&self.hash),
            needs_content: self.needs_content,
        }
    }
}

/// Build the `files` row for one on-disk file from a `stat` the caller
/// already holds. The single implementation behind both full-run batches
/// and incremental watcher updates.
///
/// `path` must already be canonical and in `files.path` spelling (see
/// [`path_to_db_string`]), and must still name the file once parsed back into
/// a [`Path`] — this opens it by that string. A path that only survived
/// `to_string_lossy` does not qualify; callers holding the original
/// [`Path`] screen it with [`warn_if_unrepresentable`] first. The full walk
/// gets the canonical spelling for free — every path it
/// produces descends from a canonicalized root — which saves a `realpath`
/// per file, and `realpath` costs roughly one `readlink` per path
/// component. Callers holding an unresolved path want
/// [`prepare_file_record_from_path`] instead.
///
/// Returns `None` for anything that isn't a readable regular file, with a
/// warning when hashing fails.
pub fn prepare_file_record(
    path: &str,
    meta: &std::fs::Metadata,
    config: &Config,
    registry: &Registry,
) -> Option<OwnedNewFile> {
    if !meta.is_file() {
        return None;
    }

    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())?;

    let (hash, head) = match get_file_hash(size, Path::new(path), config.processing.hash_length) {
        Ok(v) => v,
        Err(e) => {
            crate::log_warn!("Skipping file (cannot hash) {}: {}", path, e);
            return None;
        }
    };

    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())?;
    let parent = parent_str(path);
    let (inode, device_id) = inode_and_device(meta);
    // Sniff from the bytes hashing already read rather than reopening.
    let mime = guess_mime_from_head(Path::new(path), &head);
    let ftype = mime.as_deref().map(mime_to_type).unwrap_or(FileType::EMPTY);

    // Decided once, here, where the sniffed MIME, the size, the config and the
    // registry are all in hand — and stored as the row's `content_state`, so
    // "pending" downstream means a file that really needs reading rather than
    // one the content pass would only mark not-applicable.
    let needs_content = size <= config.processing.maximum_text_file_size
        && content_extractable(Path::new(path), mime.as_deref(), config, registry);

    // When the head is the whole file, an extractor that works from bytes can
    // finish the job now and spare the content pass an open/read/close. Any
    // condition that does not hold simply leaves this `None`, and the file
    // stays pending exactly as before — including invalid UTF-8, which the
    // content pass records as a failure with a reason.
    let inline_text = mime.as_deref().filter(|_| needs_content).and_then(|m| {
        // The head is the whole file only up to `hash_length`.
        //
        // Size 0 is excluded rather than treated as "trivially complete":
        // procfs, sysfs and some FUSE mounts report it for files that do have
        // content, and inlining would store empty text for them. The content
        // pass reads those correctly, and an actually-empty file costs the
        // same there as it ever did.
        if size == 0 || size > config.processing.hash_length as u64 {
            return None;
        }
        match registry.extract_complete_head(Path::new(path), m, &head) {
            Some(Ok(content)) => {
                let mut text = content.text;
                if text.len() > config.processing.maximum_text_size {
                    text = safe_truncate_string(&text, config.processing.maximum_text_size);
                }
                Some(text)
            }
            // A failure here is real, but recording it needs a file id the
            // walk does not have. Leaving it pending costs one reopen and
            // keeps failure reporting in one place.
            Some(Err(_)) | None => None,
        }
    });

    Some(OwnedNewFile {
        name,
        path: path.to_string(),
        parent,
        size,
        mtime,
        inode,
        device_id,
        mime,
        ftype,
        hash,
        inline_text,
        needs_content,
    })
}

/// [`prepare_file_record`] for a path that has not been resolved yet.
///
/// This is the watcher path: one file per event, so the extra `realpath`
/// and `stat` are irrelevant, and in exchange the caller doesn't have to
/// know about canonical spelling.
pub fn prepare_file_record_from_path(
    path: &Path,
    config: &Config,
    registry: &Registry,
) -> Option<OwnedNewFile> {
    let canonical = path.canonicalize().ok()?;
    if warn_if_unrepresentable(&canonical) {
        return None;
    }
    let db_path = path_to_db_string(&canonical);
    let meta = std::fs::metadata(&canonical).ok()?;
    prepare_file_record(&db_path, &meta, config, registry)
}

/// Extract content for one file and record the outcome on its row: text +
/// properties on success, `NA` when no extractor applies or the
/// `content_extensions` filter excludes it, `FAILED` with a reason on
/// extractor errors. The single implementation behind the full text-index
/// pass and incremental updates.
///
/// `mime` is authoritative, including when it is `None`. Every row reaches
/// here from [`prepare_file_record`], which has already sniffed the file's
/// head with [`guess_mime_from_head`] — and `infer` reads only the first few
/// hundred bytes, so sniffing the head and sniffing the path give the same
/// answer. Re-deriving it from disk therefore cost an open/fstat/read/close
/// per undetectable file to reproduce a `None` we were already handed.
pub fn extract_and_store(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    name: &str,
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> Result<(), String> {
    let outcome = decide_content(path, mime, registry, config);
    store_content_outcome(tx, file_id, name, &outcome, config)
}

/// What should be written for one file's content, decided without touching
/// the database.
///
/// Split from the write so the deciding — which opens the file, reads it and
/// runs an extractor over it — can happen on a worker thread while the single
/// writer holds nothing. See [`crate::content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentOutcome {
    /// Text (already truncated to `maximum_text_size`) and sorted properties.
    Done {
        text: String,
        properties: Vec<(String, String)>,
    },
    /// No extractor claims the MIME, or `content_extensions` excludes it.
    NotApplicable,
    /// The extractor ran and failed; the reason goes on the row.
    Failed(String),
}

/// Whether extraction will produce anything for this file: the
/// `content_extensions` filter allows it, and some extractor claims its MIME.
///
/// The single predicate behind both the `content_state` a row is born with
/// (see [`prepare_file_record`]) and [`decide_content`]'s not-applicable
/// early-out, so the two can never drift. That matters more than it looks: if
/// they disagreed, a file the walk wrote off as NA would silently never be
/// full-text indexed.
///
/// Deliberately size-free. The content pass's workers never see a size — the
/// `maximum_text_file_size` gate lives in the feeder's query
/// ([`crate::db::repo::pending_content_page`]) and in [`extract_scope_prepare`]
/// — so folding it in here would be a gate only one of the two callers could
/// honour.
pub fn content_extractable(
    path: &Path,
    mime: Option<&str>,
    config: &Config,
    registry: &Registry,
) -> bool {
    crate::config::content_allowed(path, config) && mime.is_some_and(|m| registry.supports(m))
}

/// Read `path` and decide what its content row should say. No database access,
/// no locks held — this is the expensive half.
///
/// `mime` is authoritative, including when it is `None`: see
/// [`extract_and_store`].
pub fn decide_content(
    path: &str,
    mime: Option<&str>,
    registry: &Registry,
    config: &Config,
) -> ContentOutcome {
    let p = Path::new(path);
    if !content_extractable(p, mime, config, registry) {
        return ContentOutcome::NotApplicable;
    }
    // `content_extractable` just established that some extractor claims this
    // MIME, so the `Ok(None)` arm below is unreachable; it stays as the honest
    // answer should that ever stop holding.
    let result = match mime {
        Some(m) => registry.extract(p, m),
        None => Ok(None),
    };
    match result {
        Ok(Some(mut content)) => {
            if content.text.len() > config.processing.maximum_text_size {
                content.text =
                    safe_truncate_string(&content.text, config.processing.maximum_text_size);
            }
            ContentOutcome::Done {
                properties: content.properties_sorted(),
                text: content.text,
            }
        }
        Ok(None) => ContentOutcome::NotApplicable,
        Err(reason) => ContentOutcome::Failed(reason),
    }
}

/// Apply a decision from [`decide_content`]. The cheap half: pure database
/// writes, so this is all that runs with the connection held.
pub fn store_content_outcome(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    name: &str,
    outcome: &ContentOutcome,
    config: &Config,
) -> Result<(), String> {
    match outcome {
        ContentOutcome::Done { text, properties } => repo::set_content_done(
            tx,
            file_id,
            name,
            text,
            properties,
            config.processing.store_text_for_snippets,
        ),
        ContentOutcome::NotApplicable => repo::set_content_na(tx, file_id),
        ContentOutcome::Failed(reason) => repo::set_content_failed(tx, file_id, reason),
    }
}

/// Write already-prepared records for files whose content changed.
///
/// The records arrive fully built (see [`prepare_file_record`]), so this does
/// no filesystem I/O; it only chunks the rows so each transaction, and
/// therefore each hold of the connection lock, stays short. Records that
/// already carry their text ([`OwnedNewFile::inline_text`]) are stored
/// complete here, which is what keeps the content pass from reopening them;
/// the rest stay pending. Progress display is the writer loop's job — these
/// are silent DB writers.
pub fn process_batch_updates(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_update: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<(), String> {
    if files_to_update.is_empty() {
        return Ok(());
    }

    let fts_batch = config.processing.fts_update_batch_size.max(1);

    for batch in files_to_update.chunks(fts_batch) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for rec in batch.iter() {
            if stop_flag.load(Ordering::Relaxed) {
                drop(tx);
                drop(conn);
                return Ok(());
            }

            let updated = repo::update_file_basic(&tx, &rec.as_new_file()).map_err(|e| {
                format!(
                    "Failed to update file record + clear stale content for {}: {}",
                    rec.path, e
                )
            })?;

            // No row matched: the path spelling we're writing disagrees with
            // the one stored. Silently dropping the update would leave the
            // row's mtime stale, so it would be reclassified as changed and
            // re-hashed on every run forever.
            let id = match updated {
                Some(id) => Some(id),
                None => {
                    crate::log_warn!(
                        "no indexed row matched {} during update; inserting instead",
                        rec.path
                    );
                    repo::insert_file(&tx, &rec.as_new_file())
                        .map_err(|e| format!("Failed to insert file record: {}", e))?
                }
            };

            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                store_inline_text(&tx, id, rec, text, config)?;
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Store text the walk already extracted, so the content pass skips this row.
///
/// Deliberately the same [`repo::set_content_done`] the content pass calls,
/// with the same empty property set a plaintext extraction produces, so a row
/// finished here is indistinguishable from one finished there.
pub(crate) fn store_inline_text(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    rec: &OwnedNewFile,
    text: &str,
    config: &Config,
) -> Result<(), String> {
    repo::set_content_done(
        tx,
        file_id,
        &rec.name,
        text,
        &[],
        config.processing.store_text_for_snippets,
    )
}

/// Write already-prepared records for newly discovered files. Silent, like
/// [`process_batch_updates`], and likewise stores any text the walk already
/// extracted.
pub fn process_batch_inserts(
    conn_mutex: &Arc<Mutex<Connection>>,
    files_to_insert: &[OwnedNewFile],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<(), String> {
    if files_to_insert.is_empty() {
        return Ok(());
    }

    for batch in files_to_insert.chunks(config.processing.batch_size) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;

        for rec in batch.iter() {
            if stop_flag.load(Ordering::Relaxed) {
                drop(tx);
                drop(conn);
                return Ok(());
            }
            let id = repo::insert_file(&tx, &rec.as_new_file())
                .map_err(|e| format!("Failed to insert file record: {}", e))?;
            if let (Some(id), Some(text)) = (id, rec.inline_text.as_deref()) {
                store_inline_text(&tx, id, rec, text, config)?;
            }
        }

        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }

    Ok(())
}

/// Delete the rows a completed run found no file behind, in chunked
/// transactions. Returns how many went.
///
/// The chunking is not just about transaction size: the connection is shared
/// with every search and status query, and `should_abort` *blocks* while the
/// indexer is suspended. Checking it inside the transaction — as this used to —
/// pinned the connection for the whole suspension and froze the GUI behind it.
/// So the stop flag, which never blocks, is what guards the inner loop, and
/// suspension is only ever observed between chunks with nothing held.
pub fn cleanup_stale_index_entries(
    conn_mutex: &Arc<Mutex<Connection>>,
    stale_paths: &[String],
    stop_flag: &Arc<AtomicBool>,
    suspend_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<usize, String> {
    if stale_paths.is_empty() {
        return Ok(0);
    }
    let chunk = config.processing.batch_size.max(1);
    let mut deleted_count = 0usize;

    for batch in stale_paths.chunks(chunk) {
        // Outside the lock, so a suspend parks here rather than mid-transaction.
        if should_abort(stop_flag, suspend_flag) {
            return Ok(deleted_count);
        }
        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin stale cleanup transaction: {}", e))?;
        for path in batch {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            if repo::delete_file_by_path(&tx, path)
                .map_err(|e| format!("Failed to remove stale index entry for {}: {}", path, e))?
            {
                deleted_count += 1;
            }
        }
        tx.commit()
            .map_err(|e| format!("Failed to commit stale cleanup transaction: {}", e))?;
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(deleted_count);
        }
    }

    if deleted_count > 0 && !should_abort(stop_flag, suspend_flag) {
        let conn = conn_mutex.lock().unwrap();
        fts_finalize_after_text_indexing(&conn);
    }

    Ok(deleted_count)
}

/// Keyset cursor bounding everything stored beneath one directory.
///
/// `lo`/`hi` are the half-open path range `[dir + SEP, dir + (SEP + 1))`, so
/// the pair is a pure index range on `UNIQUE(files.path)` — `SEARCH … (path>?
/// AND path<?)` rather than a scan. Used by content extraction, by the
/// vanished-directory sweep, and by the watcher's subtree deletion, all of
/// which need the same answer to "which rows live under this directory".
///
/// The separator is load-bearing and must be the platform's own. `files.path`
/// stores native separators (see [`path_to_db_string`]), and the successor of
/// `/` (`0x2F`) is `'0'` while the successor of `\` (`0x5C`) is `']'` — using
/// the Unix pair on Windows yields `hi = "C:\Users\me0"`, which every stored
/// path sorts *above*. The range would be empty, silently disabling content
/// extraction and the vanished-directory sweep on that platform.
#[derive(Debug, Clone)]
pub struct ExtractCursor {
    pub last_id: i64,
    pub lo: String,
    pub hi: String,
}

impl ExtractCursor {
    /// Cursor covering everything under `root`.
    pub fn for_root(root: &str) -> ExtractCursor {
        const SEP: char = std::path::MAIN_SEPARATOR;
        // Both separators are trimmed, not just the platform's: a config or a
        // watcher event may spell a directory either way, and a trailing one
        // would otherwise be doubled into the bounds.
        let base = root.trim_end_matches(['/', '\\']);
        let next = char::from_u32(SEP as u32 + 1).expect("separator successor is a valid char");
        ExtractCursor {
            last_id: 0,
            lo: format!("{}{}", base, SEP),
            hi: format!("{}{}", base, next),
        }
    }
}

/// What a root's extraction scope holds: rows still to extract this run,
/// and rows whose text is already searchable from earlier runs. Progress
/// displays show their sum so an unchanged root reads as fully extracted
/// rather than "extracted 0".
///
/// Both halves count only files an extractor claims: rows nothing will extract
/// are written `NA` at walk time (see [`content_extractable`]) and so appear in
/// neither. That is what makes the sum a denominator for the *work*, rather
/// than for every file under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractScope {
    pub pending: usize,
    pub already_done: usize,
}

/// Prepare a root's extraction scope: flip oversize pending rows to NA
/// (idempotent) and count what is pending vs. already extracted in the range.
///
/// [`prepare_file_record`] already applies `maximum_text_file_size` when it
/// writes a row, so the sweep finds nothing on a database this build filled.
/// It stays for the two cases that decision cannot cover: a
/// `maximum_text_file_size` *lowered* between runs (which, unlike
/// `content_extensions`, does not force a rebuild — see
/// [`crate::config::diff_actions`]), and rows left pending by an older build
/// that marked every file pending regardless.
pub fn extract_scope_prepare(
    conn_mutex: &Arc<Mutex<Connection>>,
    cursor: &ExtractCursor,
    config: &Config,
) -> Result<ExtractScope, String> {
    let max_size = i64::try_from(config.processing.maximum_text_file_size).unwrap_or(i64::MAX);
    let conn = conn_mutex.lock().unwrap();
    conn.execute(
        "UPDATE files SET content_state = 3 \
         WHERE content_state = 0 AND size > ?1 AND path >= ?2 AND path < ?3",
        rusqlite::params![max_size, cursor.lo, cursor.hi],
    )
    .map_err(|e| format!("mark oversize files NA: {}", e))?;
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files \
             WHERE content_state = 0 AND size <= ?1 AND path >= ?2 AND path < ?3",
            rusqlite::params![max_size, cursor.lo, cursor.hi],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count pending text files: {}", e))?;
    let already_done: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files \
             WHERE content_state = 1 AND path >= ?1 AND path < ?2",
            rusqlite::params![cursor.lo, cursor.hi],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count extracted files: {}", e))?;
    Ok(ExtractScope {
        pending: pending.max(0) as usize,
        already_done: already_done.max(0) as usize,
    })
}

/// Write a batch of already-extracted rows.
///
/// The cheap half of the content pass: [`crate::content`] does the reading and
/// the extracting on its own worker threads, and this is all that runs with the
/// connection held. Chunked so each transaction — and so each hold of the
/// connection lock — stays short, exactly as [`process_batch_inserts`] does.
///
/// Returns how many rows were written. A row whose write fails is logged and
/// skipped rather than failing the run: its `content_state` stays pending, so
/// the next run retries it.
pub fn store_extracted(
    conn_mutex: &Arc<Mutex<Connection>>,
    rows: &[crate::content::ExtractedRow],
    stop_flag: &Arc<AtomicBool>,
    config: &Config,
) -> Result<usize, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    for batch in rows.chunks(config.processing.batch_size.max(1)) {
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(written);
        }
        let conn = conn_mutex.lock().unwrap();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        for row in batch {
            if let Err(e) = store_content_outcome(&tx, row.file_id, &row.name, &row.outcome, config)
            {
                crate::log_warn!("content indexing for {}: {}", row.name, e);
                continue;
            }
            written += 1;
        }
        tx.commit()
            .map_err(|e| format!("Failed to commit transaction: {}", e))?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::MAIN_SEPARATOR;

    fn tmp_tree() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-walk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn filtered_walk_prunes_hidden_and_ignored() {
        let root = tmp_tree();
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/keep2.txt"));
        touch(&root.join("sub/skip.tmp"));
        touch(&root.join(".hidden/inside.txt"));
        touch(&root.join(".dotfile"));
        touch(&root.join("node_modules/dep/index.js"));
        touch(&root.join("secret/deep/file.txt"));

        let ignore = IgnoreSet::compile(&[
            "*.tmp".to_string(),
            "node_modules".to_string(),
            format!("{}/secret", root.display()),
        ])
        .unwrap();

        let mut names: Vec<String> = filtered_walk(
            root.to_str().unwrap(),
            false,
            false,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
        names.sort();
        assert_eq!(names, vec!["keep.txt", "keep2.txt"]);

        // include_hidden brings back dotfiles but ignores still apply.
        let mut names: Vec<String> = filtered_walk(
            root.to_str().unwrap(),
            false,
            true,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
        names.sort();
        assert_eq!(
            names,
            vec![".dotfile", "inside.txt", "keep.txt", "keep2.txt"]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The watcher spends one inotify descriptor per directory this yields,
    /// so it must prune exactly like [`filtered_walk`] — the pruned
    /// subtrees are the whole saving.
    #[test]
    fn filtered_dirs_yields_only_kept_directories() {
        let root = tmp_tree();
        touch(&root.join("keep.txt"));
        touch(&root.join("sub/nested/keep2.txt"));
        touch(&root.join(".hidden/inside.txt"));
        touch(&root.join("node_modules/dep/index.js"));

        let ignore = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();

        let mut names: Vec<String> = filtered_dirs(
            root.to_str().unwrap(),
            false,
            false,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
        names.sort();
        // The root itself is included (depth 0 is always kept); `.hidden`,
        // `node_modules`, and `node_modules/dep` cost nothing.
        let root_name = root.file_name().unwrap().to_string_lossy().into_owned();
        let mut want = vec![root_name.clone(), "nested".to_string(), "sub".to_string()];
        want.sort();
        assert_eq!(names, want);

        // include_hidden brings the dotted directory back.
        let names: Vec<String> = filtered_dirs(
            root.to_str().unwrap(),
            false,
            true,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
        assert!(names.contains(&".hidden".to_string()));
        assert!(
            !names.contains(&"node_modules".to_string()),
            "ignores still apply with include_hidden"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Files and directories partition the walk: every entry lands in
    /// exactly one of the two iterators.
    #[test]
    fn filtered_dirs_and_filtered_walk_do_not_overlap() {
        let root = tmp_tree();
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));
        let ignore = IgnoreSet::compile(&[]).unwrap();

        let files: Vec<_> = filtered_walk(
            root.to_str().unwrap(),
            false,
            false,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.path().to_path_buf())
        .collect();
        let dirs: Vec<_> = filtered_dirs(
            root.to_str().unwrap(),
            false,
            false,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.path().to_path_buf())
        .collect();

        assert_eq!(files.len(), 2);
        assert_eq!(dirs.len(), 2, "root + sub");
        assert!(
            files.iter().all(|f| !dirs.contains(f)),
            "a path must not be both a file and a directory"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filtered_walk_hidden_root_still_walked() {
        // Users explicitly chose their roots — a hidden root dir must not
        // silence the whole walk.
        let base = tmp_tree();
        let root = base.join(".config");
        touch(&root.join("app.conf"));
        let ignore = IgnoreSet::compile(&[]).unwrap();
        let names: Vec<String> = filtered_walk(
            root.to_str().unwrap(),
            false,
            false,
            &ignore,
            &UnreadableDirs::default(),
        )
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
        assert_eq!(names, vec!["app.conf"]);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn classify_uses_mtime_against_the_existing_index() {
        let mut rows = DirRows::new();
        rows.insert("known.txt".to_string(), 100);

        assert_eq!(
            classify_for_indexing("new.txt", 100, &rows),
            FileIndexAction::Insert,
            "a name absent from the directory's rows is new"
        );
        assert_eq!(
            classify_for_indexing("known.txt", 100, &rows),
            FileIndexAction::Skip,
            "same mtime means nothing to do"
        );
        assert_eq!(
            classify_for_indexing("known.txt", 101, &rows),
            FileIndexAction::Update,
            "a changed mtime means re-read"
        );
    }

    #[test]
    fn classify_by_mtime_matches_the_name_keyed_path() {
        // Resolved symlink targets take this route because their row is
        // found by exact path, not by name within the directory walked.
        assert_eq!(classify_by_mtime(None, 100), FileIndexAction::Insert);
        assert_eq!(classify_by_mtime(Some(100), 100), FileIndexAction::Skip);
        assert_eq!(classify_by_mtime(Some(99), 100), FileIndexAction::Update);
    }

    #[test]
    fn db_path_strips_windows_prefixes() {
        assert_eq!(
            path_to_db_string(Path::new("/plain/unix/path")),
            "/plain/unix/path"
        );
        assert_eq!(
            path_to_db_string(Path::new(r"\\?\C:\docs\a.txt")),
            r"C:\docs\a.txt"
        );
        // A share must come back as \\server\share, not UNC\server\share —
        // stripping a fixed four characters produces a path that cannot be
        // opened, and every file beneath it would be misfiled.
        assert_eq!(
            path_to_db_string(Path::new(r"\\?\UNC\server\share\a.txt")),
            r"\\server\share\a.txt"
        );
        // A volume mounted at a folder has no drive letter, so the prefix is
        // load-bearing: `Volume{...}\a.txt` is not a path anything can open.
        assert_eq!(
            path_to_db_string(Path::new(r"\\?\Volume{9f8a}\data\a.txt")),
            r"\\?\Volume{9f8a}\data\a.txt"
        );
    }

    /// The range must bracket paths spelled with the *platform's* separator.
    ///
    /// The bug this pins: `hi` was hard-coded as `'/' + 1` = `'0'`, so on
    /// Windows every stored `C:\Users\me\…` path sorted above it and the range
    /// was empty — content extraction and the vanished-directory sweep both
    /// silently did nothing.
    #[test]
    fn extract_cursor_brackets_paths_under_its_root() {
        use std::path::MAIN_SEPARATOR as SEP;

        let root = format!("{}Users{}me", root_prefix(), SEP);
        let c = ExtractCursor::for_root(&root);
        let inside = format!("{}{}docs{}a.txt", root, SEP, SEP);
        assert!(
            inside.as_str() >= c.lo.as_str() && inside.as_str() < c.hi.as_str(),
            "{:?} must fall inside [{:?}, {:?})",
            inside,
            c.lo,
            c.hi
        );

        // The directory itself is *not* in the range (the range is what lives
        // beneath it), and a prefix sibling is outside it.
        assert!(root.as_str() < c.lo.as_str());
        let sibling = format!("{}Users{}mexico{}a.txt", root_prefix(), SEP, SEP);
        assert!(
            !(sibling.as_str() >= c.lo.as_str() && sibling.as_str() < c.hi.as_str()),
            "{:?} is a sibling of {:?}, not a child",
            sibling,
            root
        );

        // A trailing separator of either flavour must not double up.
        for spelled in [format!("{}/", root), format!("{}\\", root)] {
            let c2 = ExtractCursor::for_root(&spelled);
            assert_eq!((&c2.lo, &c2.hi), (&c.lo, &c.hi), "spelled {:?}", spelled);
        }
    }

    /// An absolute-path prefix for the running platform.
    fn root_prefix() -> String {
        if cfg!(windows) {
            r"C:\".to_string()
        } else {
            "/".to_string()
        }
    }

    /// A Remove event names a path that is already gone, so the key for it has
    /// to be built from the deepest ancestor that still resolves.
    #[test]
    fn db_key_for_a_vanished_path_canonicalizes_what_remains() {
        let root = tmp_tree();
        let real = root.join("sub");
        std::fs::create_dir_all(&real).unwrap();

        let missing = real.join("gone").join("deeper.txt");
        let key = db_key_for_missing_path(&missing);

        let expected =
            path_to_db_string(&real.canonicalize().unwrap().join("gone").join("deeper.txt"));
        assert_eq!(key, expected, "existing prefix resolved, missing tail kept");

        // A redundant component in the *existing* part is collapsed, which is
        // the whole point — the stored key never contains one.
        let odd = root.join("sub").join(".").join("gone.txt");
        let odd_key = db_key_for_missing_path(&odd);
        assert!(
            !odd_key.contains(&format!("{}.{}", MAIN_SEPARATOR, MAIN_SEPARATOR)),
            "unexpected `.` component in {}",
            odd_key
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn db_key_for_an_entirely_missing_path_falls_back_to_the_raw_spelling() {
        let nowhere = Path::new("relative-thing-that-does-not-exist.txt");
        assert_eq!(db_key_for_missing_path(nowhere), path_to_db_string(nowhere));
    }

    #[test]
    fn unreadable_dirs_match_by_component_not_string_prefix() {
        let u = UnreadableDirs::default();
        assert!(!u.covers("/a/b/c.txt"), "an empty set covers nothing");

        u.record(std::path::PathBuf::from("/a/b"));
        assert!(u.covers("/a/b/c.txt"));
        assert!(u.covers("/a/b"));
        // The bug a naive `str::starts_with` would introduce: /a/bc is a
        // sibling of /a/b, and its rows must stay deletable.
        assert!(!u.covers("/a/bc/d.txt"));
        assert!(!u.covers("/a/other.txt"));
    }

    #[test]
    fn unreadable_directory_is_reported_rather_than_yielded_as_empty() {
        // A directory the walk cannot read must be recorded, so the caller
        // can tell "could not look" apart from "the files are gone" — the
        // latter deletes index rows.
        let root = tmp_tree();
        touch(&root.join("readable/a.txt"));
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        touch(&locked.join("hidden-from-us.txt"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

            let ignore = IgnoreSet::compile(&[]).unwrap();
            let failures = UnreadableDirs::default();
            let names: Vec<String> =
                filtered_walk(root.to_str().unwrap(), false, false, &ignore, &failures)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();

            // Restore before asserting so a failure still cleans up.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();

            assert_eq!(
                names,
                vec!["a.txt"],
                "the unreadable subtree yields nothing"
            );
            assert!(!failures.is_empty(), "and that failure must be recorded");
            assert!(
                failures.covers(locked.join("hidden-from-us.txt").to_str().unwrap()),
                "rows beneath it are protected from stale cleanup"
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_covers_size_and_head_only() {
        let root = tmp_tree();
        let a = root.join("a.bin");
        let b = root.join("b.bin");
        let c = root.join("c.bin");

        // Same size, same head, differing only in the tail: the documented
        // collision (pre-allocated VM images are the real-world case).
        std::fs::write(&a, [b"HEAD".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();
        std::fs::write(&b, [b"HEAD".as_slice(), &[0u8; 64], b"BBBB"].concat()).unwrap();
        // Differs within the head window.
        std::fs::write(&c, [b"DIFF".as_slice(), &[0u8; 64], b"AAAA"].concat()).unwrap();

        let h = |p: &Path| {
            get_file_hash(std::fs::metadata(p).unwrap().len(), p, 8)
                .unwrap()
                .0
        };
        assert_eq!(h(&a), h(&b), "tail differences are invisible by design");
        assert_ne!(h(&a), h(&c), "head differences are caught");

        // Size participates, so a prefix does not collide with its extension.
        let short = root.join("short.bin");
        std::fs::write(&short, b"HEAD").unwrap();
        assert_ne!(h(&a), h(&short));

        // The head is returned for MIME sniffing rather than re-read.
        let (_, head) = get_file_hash(72, &a, 8).unwrap();
        assert_eq!(head, b"HEAD\0\0\0\0", "exactly hash_length bytes");
        let (_, head) = get_file_hash(4, &short, 8).unwrap();
        assert_eq!(head, b"HEAD", "a short file hashes whole");

        std::fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod count_and_extract_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-ce-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn count_normal_small_tree() {
        let root = tmp("count");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for name in ["a.txt", "b.txt", "sub/c.txt"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let n = count_tree_entries_fast(root.to_str().unwrap(), &cancel).unwrap();
        // find lists the root, the subdir, and the three files.
        assert_eq!(n, 5);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn count_cancelled_returns_promptly() {
        // A pre-set token must kill the subprocesses on the first poll —
        // "/" would otherwise take minutes to scan.
        let cancel = AtomicBool::new(true);
        let started = std::time::Instant::now();
        let result = count_tree_entries_fast("/", &cancel);
        let elapsed = started.elapsed();
        assert!(result.is_err(), "cancelled count must not succeed");
        assert!(
            result.unwrap_err().contains("cancelled"),
            "error must be recognizable as cancellation"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "cancellation took {:?}",
            elapsed
        );
        // The token is observational only — nothing resets it.
        assert!(cancel.load(Ordering::Relaxed));
    }

    /// The invariant the whole "decide at walk time" design rests on. If these
    /// two ever disagree, a file the walk wrote off as NA would silently never
    /// be full-text indexed — a far worse bug than the inflated denominator
    /// this replaced. Fails the moment someone edits one predicate and not the
    /// other.
    #[test]
    fn content_extractable_is_decide_contents_not_applicable() {
        let root = tmp("extractable");
        std::fs::create_dir_all(&root).unwrap();
        let mut cfg = Config::default();
        let registry = Registry::default_set();

        // Real files, because `decide_content` runs the extractor for anything
        // it claims and its answer must be a genuine one. A text body sniffs
        // as text for the extension-less names; the NUL body keeps the
        // unclaimed side of the invariant exercised too.
        let cases: [(&str, &[u8]); 10] = [
            ("notes.txt", b"plain bytes with no magic"),
            ("data.json", b"plain bytes with no magic"),
            ("schema.sql", b"plain bytes with no magic"),
            ("song.mp3", b"plain bytes with no magic"),
            ("photo.jpg", b"plain bytes with no magic"),
            ("movie.mp4", b"plain bytes with no magic"),
            ("archive.zip", b"plain bytes with no magic"),
            ("blob.bin", b"plain bytes with no magic"),
            ("noextension", b"plain bytes with no magic"),
            ("real.bin", b"\x00\x01\x02\x03"),
        ];
        for (name, body) in cases {
            let p = root.join(name);
            std::fs::write(&p, body).unwrap();
        }

        // Once with the filter off (the default: everything the registry
        // claims), once with it narrowed to `.txt`.
        for filter in [Vec::new(), vec!["txt".to_string()]] {
            cfg.indexing.content_extensions = filter.clone();
            for (name, body) in cases {
                let p = root.join(name);
                let path = p.to_str().unwrap();
                let mime = guess_mime_from_head(&p, body);
                let claimed = content_extractable(&p, mime.as_deref(), &cfg, &registry);
                let outcome = decide_content(path, mime.as_deref(), &registry, &cfg);
                assert_eq!(
                    claimed,
                    outcome != ContentOutcome::NotApplicable,
                    "{} (mime {:?}, filter {:?}): predicate says {}, decide_content says {:?}",
                    name,
                    mime,
                    filter,
                    claimed,
                    outcome
                );
            }
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn prepare_file_record_marks_only_claimable_files() {
        let root = tmp("claimable");
        std::fs::create_dir_all(&root).unwrap();
        let registry = Registry::default_set();
        let needs = |cfg: &Config, name: &str| -> bool {
            let p = root.join(name);
            let meta = std::fs::metadata(&p).unwrap();
            prepare_file_record(p.to_str().unwrap(), &meta, cfg, &registry)
                .expect("regular file")
                .needs_content
        };

        for name in ["notes.txt", "song.mp3", "movie.mp4", "README"] {
            std::fs::write(root.join(name), b"body").unwrap();
        }
        // NUL bytes: the text sniff must not rescue a genuinely binary blob.
        std::fs::write(root.join("blob.bin"), b"\x00\x01\x02\x03").unwrap();
        let big = root.join("huge.txt");
        std::fs::write(&big, vec![b'x'; 4096]).unwrap();

        let cfg = Config::default();
        assert!(needs(&cfg, "notes.txt"), "plaintext is claimed");
        assert!(needs(&cfg, "song.mp3"), "audio tags are content too");
        assert!(
            needs(&cfg, "README"),
            "an extensionless text head sniffs as text/plain"
        );
        assert!(!needs(&cfg, "movie.mp4"), "no extractor claims video");
        assert!(
            !needs(&cfg, "blob.bin"),
            "binary content: no MIME, no extractor"
        );

        // Over `maximum_text_file_size`, so the content pass would never read
        // it even though plaintext claims the MIME.
        let mut small_cap = Config::default();
        small_cap.processing.maximum_text_file_size = 1024;
        assert!(!needs(&small_cap, "huge.txt"));
        assert!(needs(&cfg, "huge.txt"), "and claimed under the default cap");

        // The `content_extensions` allowlist excludes it.
        let mut only_md = Config::default();
        only_md.indexing.content_extensions = vec!["md".to_string()];
        assert!(!needs(&only_md, "notes.txt"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The headline regression test: the number the manage-index tab shows as
    /// the extraction denominator, measured where `indexing.rs` measures it —
    /// after the walk's inserts, before any content pass runs.
    #[test]
    fn extract_scope_counts_only_files_an_extractor_claims() {
        let root = tmp("denominator");
        std::fs::create_dir_all(&root).unwrap();
        let mut db = root.clone();
        db.set_extension("sqlite");

        // 4 files an extractor claims (README via the extensionless text
        // sniff), 5 it never will — the unclaimed set gets NUL bodies so
        // neither the extension tables nor the sniff have anything to say.
        // Big enough that the old behaviour (every row pending) can't
        // coincide with the new one.
        let claimed = ["a.txt", "b.json", "c.mp3", "README"];
        let unclaimed = ["d.mp4", "e.zip", "f.bin", "g.exe", "h"];
        for name in claimed.iter() {
            std::fs::write(root.join(name), b"body bytes, no magic").unwrap();
        }
        for name in unclaimed.iter() {
            std::fs::write(root.join(name), b"\x00\x01body\x00").unwrap();
        }

        let config = Config::default();
        let registry = Registry::default_set();
        let records: Vec<OwnedNewFile> = claimed
            .iter()
            .chain(unclaimed.iter())
            .map(|name| {
                let p = root.join(name);
                let meta = std::fs::metadata(&p).unwrap();
                prepare_file_record(p.to_str().unwrap(), &meta, &config, &registry)
                    .expect("regular file")
            })
            .collect();

        let conn_mutex = Arc::new(Mutex::new(
            crate::db::open_or_recreate(db.to_str().unwrap(), "trigram").unwrap(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        process_batch_inserts(&conn_mutex, &records, &stop, &config).unwrap();

        let cursor = ExtractCursor::for_root(root.to_str().unwrap());
        let scope = extract_scope_prepare(&conn_mutex, &cursor, &config).unwrap();

        // `extract_total` in the GUI. The three small text-ish files were
        // finished inline by the walk so they land in `already_done`; the mp3
        // needs the disk pass. Either way the denominator is the claimed set —
        // before this was decided at walk time it read 9, every indexed file.
        assert_eq!(
            (scope.pending, scope.already_done),
            (1, 3),
            "denominator must be the files needing text, not every indexed file"
        );
        assert_eq!(scope.pending + scope.already_done, claimed.len());

        let total: i64 = conn_mutex
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total as usize, claimed.len() + unclaimed.len());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }
}
