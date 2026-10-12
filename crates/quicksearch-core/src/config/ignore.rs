//! [`IgnoreSet`]: the compiled ignore-pattern matcher, split by scope
//! (single path components vs full paths), and [`glob_literal`] for patterns
//! built from a name.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// Longest name folded without allocating; comfortably above `NAME_MAX`.
pub(super) const FOLD_BUF: usize = 256;

/// Whether `pat` is a plain name with no glob syntax. `\` is included: on
/// Unix it is the glob escape, which only globset reads, and on Windows it is
/// a separator.
fn is_literal_name(pat: &str) -> bool {
    !pat.contains(['*', '?', '[', ']', '{', '}', '/', '\\'])
}

/// A glob matching exactly `s`. Each glob metacharacter is wrapped in a
/// bracket class (`[*]`), as `globset::escape` does. So is a backslash off
/// Windows, where it is a name character and the glob escape (a class keeps
/// it literal, and a trailing one is not trimmed as a dangling escape), and so
/// is leading or trailing whitespace, which [`normalized_pattern`] would trim.
/// On Windows a backslash is a separator and stays as it is, so a full path
/// keeps its spelling.
pub fn glob_literal(s: &str) -> String {
    let (start, end) = (s.len() - s.trim_start().len(), s.trim_end().len());
    let mut out = String::with_capacity(s.len());
    for (at, c) in s.char_indices() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}')
            || (c == '\\' && !std::path::is_separator(c))
            || !(start..end).contains(&at)
        {
            out.extend(['[', c, ']']);
        } else {
            out.push(c);
        }
    }
    out
}

/// `pat` as it compiles, or `None` for one that trims to nothing (skipped).
///
/// Trailing separators are stripped ("/tmp/" compares as "/tmp"), both of
/// them on every platform on purpose: this normalizes glob syntax, not a
/// stored key, and on Unix a trailing `\` is a dangling glob escape. A drive
/// root is the exception, where the separator is the whole point: trimmed to
/// "D:" it becomes a component pattern that can never match. Drive roots keep
/// a normalized "D:/" spelling.
pub(super) fn normalized_pattern(pat: &str) -> Option<Cow<'_, str>> {
    let raw = pat.trim();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    let is_drive_root = raw.len() > trimmed.len()
        && matches!(trimmed.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic());
    if is_drive_root {
        Some(Cow::Owned(format!("{}/", trimmed)))
    } else {
        (!trimmed.is_empty()).then_some(Cow::Borrowed(trimmed))
    }
}

/// Compiled ignore patterns, split by matching scope: patterns without one of
/// the platform's path separators match any single path component; the rest
/// match the full path. Both use glob syntax.
#[derive(Debug)]
pub struct IgnoreSet {
    /// Plain-ASCII component names, held apart for speed: globset gives up
    /// every fast path the moment `case_insensitive` is set (measured: a
    /// regex DFA per directory entry on Windows vs five hash lookups).
    /// Folded as ASCII, matching SQLite's `NOCASE`/`LIKE`; non-ASCII patterns
    /// stay in `component` with globset's Unicode folding.
    pub(super) literal_components: std::collections::HashSet<String>,
    component: globset::GlobSet,
    path: globset::GlobSet,
    empty: bool,
}

impl IgnoreSet {
    pub fn compile(patterns: &[String]) -> Result<IgnoreSet, String> {
        let mut literal_components = std::collections::HashSet::new();
        let mut component = globset::GlobSetBuilder::new();
        let mut path = globset::GlobSetBuilder::new();
        for pat in patterns {
            let Some(pat) = normalized_pattern(pat) else {
                continue;
            };
            if pat.is_ascii() && is_literal_name(&pat) {
                literal_components.insert(if crate::platform::PATHS_ARE_CASE_INSENSITIVE {
                    pat.to_ascii_lowercase()
                } else {
                    pat.to_string()
                });
                continue;
            }
            let glob = globset::GlobBuilder::new(&pat)
                .literal_separator(false)
                // Match the filesystem's own rules, or `node_modules` fails
                // to exclude `Node_Modules`.
                .case_insensitive(crate::platform::PATHS_ARE_CASE_INSENSITIVE)
                .build()
                .map_err(|e| format!("invalid ignore pattern {:?}: {}", pat, e))?;
            // The platform's separators decide: on Unix a backslash is the
            // glob escape, so an escaped name stays a component pattern.
            if pat.contains(std::path::is_separator) {
                path.add(glob);
            } else {
                component.add(glob);
            }
        }
        let component = component
            .build()
            .map_err(|e| format!("compile ignore patterns: {}", e))?;
        let path = path
            .build()
            .map_err(|e| format!("compile ignore patterns: {}", e))?;
        let empty = literal_components.is_empty() && component.is_empty() && path.is_empty();
        Ok(IgnoreSet {
            literal_components,
            component,
            path,
            empty,
        })
    }

    /// Allocation-free for any name that fits [`FOLD_BUF`]; this runs on
    /// every directory entry the walker sees.
    fn matches_literal(&self, name: &str) -> bool {
        if self.literal_components.is_empty() {
            return false;
        }
        if !crate::platform::PATHS_ARE_CASE_INSENSITIVE {
            return self.literal_components.contains(name);
        }
        // A name containing any non-ASCII byte cannot equal a stored ASCII
        // literal.
        if !name.is_ascii() {
            return false;
        }
        if name.len() <= FOLD_BUF {
            let mut buf = [0u8; FOLD_BUF];
            let buf = &mut buf[..name.len()];
            buf.copy_from_slice(name.as_bytes());
            buf.make_ascii_lowercase();
            let folded = std::str::from_utf8(buf).expect("ascii stays utf-8");
            return self.literal_components.contains(folded);
        }
        self.literal_components.contains(&name.to_ascii_lowercase())
    }

    /// Match a single file/directory name.
    pub fn matches_component(&self, name: &str) -> bool {
        if self.empty {
            return false;
        }
        self.matches_literal(name) || self.component.is_match(name)
    }

    /// Match a path against the full-path patterns only. The path *and its
    /// ancestors* are tested, so a pattern matching a directory ignores
    /// everything beneath it.
    pub fn matches_path_pattern(&self, path: &Path) -> bool {
        !self.path.is_empty() && path.ancestors().any(|p| self.path.is_match(p))
    }

    /// Match a full path, every component tested: for whole-path filters (a
    /// search session's, the duplicates tab's), which have no root to measure
    /// from.
    pub fn matches_path(&self, path: &Path) -> bool {
        self.matches_below(path, &[])
    }

    /// Whether the walk would prune `path`: a full-path pattern matches it or
    /// an ancestor, or a component pattern matches a name *below* the
    /// innermost of `roots` containing it
    /// ([`crate::platform::split_at_root`]). The walk never tests a root's own
    /// name or anything above it, so `~/venv/proj` keeps its files. A path
    /// under no root has every component tested.
    pub fn matches_below(&self, path: &Path, roots: &[PathBuf]) -> bool {
        if self.empty {
            return false;
        }
        let below = crate::platform::split_at_root(path, roots.iter().map(PathBuf::as_path))
            .map_or(path, |(_, below)| below);
        // Through `matches_component`, or the literal patterns would be
        // invisible here and the watcher would index what the walker prunes.
        self.matches_path_pattern(path)
            || below.components().any(|c| {
                matches!(c, Component::Normal(n) if self.matches_component(&n.to_string_lossy()))
            })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}
