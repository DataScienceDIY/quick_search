//! Path ↔ `files.parent`/`files.name` string normalization and the filtered
//! walkdir wrappers the reconcile passes use.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use walkdir::{DirEntry, WalkDir};

use crate::config::IgnoreSet;

/// Render a path in the spelling the index stores it in.
///
/// `Path::canonicalize` on Windows hands back extended-length paths; the
/// index stores plain ones. A UNC share canonicalizes to
/// `\\?\UNC\server\share`, so the two prefixes have to be stripped
/// differently — taking four characters off both leaves `UNC\server\share`,
/// which is not a path that exists.
///
/// A volume mounted at a folder has no DOS name and canonicalizes to
/// `\\?\Volume{GUID}\…`; stripping the prefix there yields a path that cannot
/// be opened — every file under such a mount would fail to hash. Only a
/// genuine drive letter is safe to un-prefix.
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

/// What SQLite and we hang off the index's own filename; `.lock` is
/// [`crate::platform::IndexLock`]'s.
pub(crate) const INDEX_SIDECAR_SUFFIXES: [&str; 4] = ["-wal", "-shm", "-journal", ".lock"];

/// Every file belonging to the index at `db_path`, spelled the way the walk
/// spells the files it visits.
///
/// **Nothing may ever open one of these.** On POSIX, closing *any* descriptor
/// on an inode cancels every advisory lock the whole process holds on it, so a
/// walk worker that opens `index.sqlite-shm` to hash it destroys the DMS lock
/// SQLite took on that file — after which the next connection to attach, from
/// any process, truncates the wal-index to 3 bytes under our live mapping and
/// the next commit dies with SIGBUS. The same close cancels the main
/// database's own locks, which is SQLite's documented corruption hazard
/// (howtocorrupt.html §2.2). See [`crate::walk`], which prunes these before an
/// entry can become a candidate.
///
/// The *directory* is canonicalized rather than the files: the sidecars come
/// and go across a run, and `canonicalize` fails on a path that is not there
/// at the instant it is called.
pub(crate) fn index_file_set(db_path: &Path) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    // An empty filename means `database_path` names a directory; joining ""
    // onto the parent would prune the entire tree below it.
    let Some(name) = db_path.file_name().and_then(|s| s.to_str()) else {
        return set;
    };
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let dir = PathBuf::from(path_to_db_string(
        &dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
    ));
    set.insert(dir.join(name));
    for suffix in INDEX_SIDECAR_SUFFIXES {
        set.insert(dir.join(format!("{}{}", name, suffix)));
    }
    set
}

/// Canonicalize a root string for storage/comparison — the spelling stored
/// parents are prefixed with, and so the form roots must be compared in:
/// `~/docs` and `/home/me/docs` name one root and must not read as a change.
pub(crate) fn normalize_root_string(indexing_path: &str) -> String {
    let path = Path::new(indexing_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(indexing_path));
    path_to_db_string(&path)
}

/// Warn and report `true` for a path that cannot round-trip through the
/// index. [`path_to_db_string`] is lossy, and a lossy path must never become
/// a DB key: U+FFFD is an ordinary filename character, so the lossy spelling
/// is somebody's real name. Such a file is skipped whole.
///
/// The check for callers that arrive with a single path; the full walk
/// screens earlier, on the directory entry itself (`crate::walk`).
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

/// The stored spelling of a path that may no longer exist: canonicalizes the
/// deepest ancestor that still resolves and re-joins the missing tail. On
/// Linux a root reached through a symlinked parent (`/home` → `/mnt/home`)
/// makes every removal a no-op without this.
///
/// **The caller must have screened `path` for representability.** This ends
/// in lossy [`path_to_db_string`] and the answer is used to *delete*: for a
/// non-UTF-8 path the key names some other, real file, and deleting by it
/// takes that file's row and everything under it.
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
            // Nothing above resolves — the raw spelling is the best key left.
            _ => return path_to_db_string(path),
        }
    }
}

/// Render a directory as the string stored in `files.parent`.
///
/// **`files.parent` always ends in the platform separator**, and that is the
/// invariant the whole schema rests on:
///
/// * A file's path is `parent` concatenated with `name` — no separator logic
///   at the join, and so no special case for `/` or `C:\`, whose children
///   would otherwise be spelled `//x` and `C:\\x`.
/// * A root's subtree is the single range `[root + SEP, root + succ(SEP))`,
///   because the root's *own* parent is `root + SEP` rather than `root`. With
///   a bare `root` there is no such range: strings between `root` and
///   `root + SEP` are siblings (`/a-b` sorts inside `["/a", "/a0")`).
pub(crate) fn dir_to_db_parent(dir: &Path) -> String {
    let mut s = path_to_db_string(dir);
    // The platform's own separator only. On Unix `\` is an ordinary filename
    // character, so a directory genuinely named `weird\` must still get its
    // `/` — testing both separators would leave that row unjoinable.
    if !s.ends_with(std::path::MAIN_SEPARATOR) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    s
}

/// Split a stored path into the `(parent, name)` pair the index keys on. The
/// separator stays with the parent, so `parent` + `name` is the original
/// string back. `None` for anything that cannot be a file's path.
pub fn split_db_path(path: &str) -> Option<(&str, &str)> {
    let cut = path.rfind(std::path::MAIN_SEPARATOR)?;
    let (parent, name) = path.split_at(cut + 1);
    (!name.is_empty()).then_some((parent, name))
}

/// Paths a walk could not read. A full run deletes rows for everything it did
/// not see, so "could not read" and "gone" must not look alike — an unplugged
/// drive would otherwise silently delete its whole subtree from the index.
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

    /// Whether `path` lies under a directory the walk failed to read.
    /// Containment is by path component, never by string prefix: a
    /// `str::starts_with` would match a sibling (`/a/bc` under `/a/b`).
    pub fn covers(&self, path: &str) -> bool {
        let dirs = crate::lock_ok(&self.dirs);
        if dirs.is_empty() {
            return false;
        }
        let path = Path::new(path);
        dirs.iter().any(|d| path.starts_with(d))
    }
}

/// The single definition of "would we index this", shared by the walks and
/// the watcher. As a `filter_entry` predicate, returning `false` for a
/// directory prunes the whole subtree.
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
    // The closure runs only on Windows. A followed symlink is the trap:
    // `DirEntry::metadata` would report the *target's* attributes there, so
    // ask for the link's own explicitly.
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
/// hidden and ignored subtrees *before* descending, and records unreadable
/// directories in `failures`. Entries never grow a Windows `\\?\` prefix:
/// walkdir does not canonicalize.
fn walk_entries<'a>(
    root: &str,
    follow_symlinks: bool,
    include_hidden: bool,
    ignore: &'a IgnoreSet,
    failures: &'a UnreadableDirs,
) -> impl Iterator<Item = DirEntry> + 'a {
    WalkDir::new(root)
        .follow_links(follow_symlinks)
        // walkdir defaults this to *true*, independently of `follow_links`:
        // without it a root that is itself a symlink gets descended even when
        // following is off, and rows land under the target's real path —
        // possibly outside every sweep range, orphans until a rebuild.
        .follow_root_links(follow_symlinks)
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
        // A symlink is an entry in its own right when following is off, and
        // `walk_filter` cannot drop it (it passes depth 0 unconditionally).
        // Yielding it would index the link as a file under the target's real
        // path — outside every root, a row no sweep range covers. No cost
        // when following is on: walkdir resolves links then.
        .filter(move |e| follow_symlinks || !e.file_type().is_symlink())
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
    walk_entries(root, follow_symlinks, include_hidden, ignore, failures)
        .filter(|entry| !entry.file_type().is_dir())
}

/// Walk `root` yielding only directories, with [`filtered_walk`]'s pruning.
/// inotify has no recursive mode, so this is exactly the set of watch
/// descriptors a root costs.
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
