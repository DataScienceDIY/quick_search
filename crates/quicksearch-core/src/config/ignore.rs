//! [`IgnoreSet`]: the compiled ignore-pattern matcher, split by scope
//! (single path components vs full paths).

use std::borrow::Cow;
use std::path::{Component, Path};

/// Longest name folded without allocating; comfortably above `NAME_MAX`.
pub(super) const FOLD_BUF: usize = 256;

/// Whether `pat` is a plain name with no glob syntax. `\` is included even
/// though it is not glob syntax everywhere: a pattern containing one belongs
/// in the path set, not the component set.
fn is_literal_name(pat: &str) -> bool {
    !pat.contains(['*', '?', '[', ']', '{', '}', '/', '\\'])
}

/// Compiled ignore patterns, split by matching scope: patterns without a
/// path separator match any single path component; the rest match the full
/// path. Both use glob syntax.
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
            // Trailing separators are stripped ("/tmp/" compares as "/tmp"),
            // except from a drive root, where the separator is the whole
            // point: trimmed to "D:" it becomes a component pattern that can
            // never match. Drive roots keep a normalized "D:/" spelling.
            let raw = pat.trim();
            let trimmed = raw.trim_end_matches(['/', '\\']);
            let is_drive_root = raw.len() > trimmed.len()
                && matches!(trimmed.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic());
            let pat: Cow<str> = if is_drive_root {
                Cow::Owned(format!("{}/", trimmed))
            } else {
                Cow::Borrowed(trimmed)
            };
            if pat.is_empty() {
                continue;
            }
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
            if pat.contains('/') || pat.contains('\\') {
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

    /// Match a full path: for watcher events, where walk-time pruning never
    /// saw it.
    pub fn matches_path(&self, path: &Path) -> bool {
        if self.empty {
            return false;
        }
        // Through `matches_component`, or the literal patterns would be
        // invisible here and the watcher would index what the walker prunes.
        self.matches_path_pattern(path)
            || path.components().any(|c| {
                matches!(c, Component::Normal(n) if self.matches_component(&n.to_string_lossy()))
            })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}
