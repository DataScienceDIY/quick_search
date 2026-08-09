//! Path ↔ `files.path` string normalization and the filtered walkdir
//! wrappers the reconcile passes use.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use walkdir::{DirEntry, WalkDir};

use crate::config::IgnoreSet;

/// Derive (inode, device_id) from a `std::fs::Metadata` on platforms that
/// expose them. Returns `(None, None)` on Windows and other non-Unix targets.
pub(super) fn inode_and_device(_meta: &std::fs::Metadata) -> (Option<u64>, Option<u64>) {
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

/// Canonicalize a root string for storage/comparison. Multi-root strings
/// (newline-joined) fail canonicalize and pass through verbatim, which still
/// compares consistently.
///
/// This is the spelling `files.path` rows are prefixed with, so it is also the
/// form roots must be compared in: `~/docs` and `/home/me/docs` name one root
/// and must not read as a change.
pub(crate) fn normalize_root_string(indexing_path: &str) -> String {
    let path = Path::new(indexing_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(indexing_path));
    path_to_db_string(&path)
}

/// Warn and report `true` for a path that cannot round-trip through
/// `files.path`.
///
/// Everything downstream reopens the file by that TEXT column, and
/// [`path_to_db_string`] is lossy: a non-UTF-8 name would be stored as a
/// path naming a file that does not exist. Such a file is skipped whole.
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
/// The insert side canonicalizes before storing, and plain `canonicalize`
/// fails on a path already gone — so this canonicalizes the deepest ancestor
/// that still resolves and re-joins the missing tail. On Linux a root reached
/// through a symlinked parent (`/home` → `/mnt/home`) makes every removal a
/// no-op without this.
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
pub(super) fn parent_str(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Paths a walk could not read, collected as it runs.
///
/// A full run deletes index rows for everything it did not see, so "I could
/// not read this directory" and "this directory's files are gone" must not
/// look alike — an unplugged drive or a network blip would otherwise silently
/// delete that whole subtree.
#[derive(Debug, Default)]
pub struct UnreadableDirs {
    dirs: Mutex<Vec<std::path::PathBuf>>,
}

impl UnreadableDirs {
    pub fn record(&self, path: std::path::PathBuf) {
        crate::lock_ok(&self.dirs).push(path);
    }

    pub fn is_empty(&self) -> bool {
        crate::lock_ok(&self.dirs).is_empty()
    }

    pub fn paths(&self) -> Vec<std::path::PathBuf> {
        crate::lock_ok(&self.dirs).clone()
    }

    /// Whether `path` lies under a directory the walk failed to read, and so
    /// must not be treated as deleted.
    ///
    /// Containment is by path component, never by string prefix:
    /// `Path::starts_with` compares whole components, so `/a/bc` does not
    /// live under `/a/b`. A `str::starts_with` would match a sibling by
    /// accident — a neighbouring folder's index disappearing.
    pub fn covers(&self, path: &str) -> bool {
        let dirs = crate::lock_ok(&self.dirs);
        if dirs.is_empty() {
            return false;
        }
        let path = Path::new(path);
        dirs.iter().any(|d| path.starts_with(d))
    }
}

/// Whether a walked entry survives the hidden/ignore filters — the single
/// definition of "would we index this", shared by [`filtered_walk`],
/// [`filtered_dirs`], and the watcher.
///
/// Used as a `walkdir` `filter_entry` predicate, so returning `false` for a
/// directory prunes the whole subtree instead of merely skipping the entry.
fn walk_filter(
    e: &DirEntry,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &IgnoreSet,
) -> bool {
    // Depth 0 is the root itself: users explicitly chose their roots.
    if e.depth() == 0 {
        return true;
    }
    let name = e.file_name().to_string_lossy();
    // The closure runs only on Windows, where walkdir already holds the
    // attributes. A followed symlink is the trap: `DirEntry::metadata`
    // switches to `fs::metadata` there and would report the *target's*
    // attributes, so ask for the link's own explicitly.
    if !include_hidden
        && crate::platform::entry_is_hidden(&name, || {
            if follow_symlinks && e.path_is_symlink() {
                std::fs::symlink_metadata(e.path()).ok()
            } else {
                e.metadata().ok()
            }
        })
    {
        return false;
    }
    !ignore.matches_component(&name) && !ignore.matches_path_pattern(e.path())
}

/// The shared walk behind [`filtered_walk`] and [`filtered_dirs`]: prunes
/// hidden and ignored subtrees *before* descending, and records directories
/// it could not read in `failures`, so the caller can tell an unreadable
/// subtree apart from a deleted one.
///
/// Entries here never grow a Windows `\\?\` prefix: walkdir does not
/// canonicalize, so every yielded path is `root` plus name components.
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
        .filter_entry(move |e| walk_filter(e, follow_symlinks, include_hidden, ignore))
        .filter_map(move |res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                // Dropping this on the floor turns a transient mount failure
                // into a deletion.
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
/// *before* descending into them.
pub fn filtered_walk<'a>(
    root: &str,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &'a IgnoreSet,
    failures: &'a UnreadableDirs,
) -> impl Iterator<Item = DirEntry> + 'a {
    // `file_type` is the cached `d_type` from the directory read; `metadata()`
    // would be an extra `lstat` per entry.
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
