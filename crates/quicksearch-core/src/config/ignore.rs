//! [`IgnoreSet`]: the compiled ignore-pattern matcher, split by scope
//! (single path components vs full paths).

use std::path::Path;

/// Longest name folded without allocating. Comfortably above `NAME_MAX` on the
/// filesystems that matter (255 bytes), so the heap path is effectively dead
/// code kept for correctness rather than for use.
pub(super) const FOLD_BUF: usize = 256;

/// Whether `pat` is a plain name with no glob syntax in it.
///
/// `\` is included even though it is not glob syntax everywhere: a pattern
/// containing one is routed to the path set rather than the component set, so
/// treating it as literal here would put it in the wrong place.
fn is_literal_name(pat: &str) -> bool {
    !pat.contains(['*', '?', '[', ']', '{', '}', '/', '\\'])
}

/// Compiled ignore patterns, split by matching scope: patterns without a
/// path separator match any single path component; the rest match the full
/// path. Both use glob syntax.
#[derive(Debug)]
pub struct IgnoreSet {
    /// Component patterns that are plain ASCII names — `.git`, `node_modules`.
    /// Held apart from `component` for speed: globset gives up every fast
    /// path (`literal`, `ext`, `prefix`, `suffix`, `basename_tokens`) the
    /// moment `case_insensitive` is set, which left Windows running a
    /// 12-pattern regex DFA over every directory entry where Linux did five
    /// hash lookups.
    ///
    /// Folded and compared as **ASCII**, matching [`PATH_COLLATION`]:
    /// SQLite's `NOCASE` and `LIKE` fold ASCII only. Non-ASCII patterns stay
    /// in `component` with globset's Unicode folding.
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
            // Trailing separators are how people naturally write directory
            // patterns ("/tmp/"); paths compare without them, so strip —
            // except a drive root ("D:\" or "D:/"), where the separator is
            // the whole point: trimmed to "D:" it would become a component
            // pattern that can never match. Drive roots keep a normalized
            // "D:/" spelling, which the ancestor walk in
            // `matches_path_pattern` does reach.
            let raw = pat.trim();
            let trimmed = raw.trim_end_matches(['/', '\\']);
            let is_drive_root = raw.len() > trimmed.len()
                && trimmed.len() == 2
                && trimmed.as_bytes()[0].is_ascii_alphabetic()
                && trimmed.as_bytes()[1] == b':';
            let drive_root;
            let pat: &str = if is_drive_root {
                drive_root = format!("{}/", trimmed);
                &drive_root
            } else {
                trimmed
            };
            if pat.is_empty() {
                continue;
            }
            // A plain ASCII name needs no glob machinery — see
            // `literal_components`.
            if pat.is_ascii() && is_literal_name(pat) {
                literal_components.insert(if crate::platform::PATHS_ARE_CASE_INSENSITIVE {
                    pat.to_ascii_lowercase()
                } else {
                    pat.to_string()
                });
                continue;
            }
            let glob = globset::GlobBuilder::new(pat)
                .literal_separator(false)
                // Match the filesystem's own rules, or `node_modules` fails to
                // exclude `Node_Modules`. globset already handles the other
                // half of Windows compatibility on its own: `Candidate` folds
                // `\` to `/` when matching, and backslash-as-escape is off
                // wherever `\` is a separator.
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

    /// Whether `name` is one of the plain-name patterns, folded per platform.
    /// Allocation-free for any name that fits [`FOLD_BUF`]; this runs on
    /// every directory entry the walker sees.
    fn matches_literal(&self, name: &str) -> bool {
        if self.literal_components.is_empty() {
            return false;
        }
        if !crate::platform::PATHS_ARE_CASE_INSENSITIVE {
            return self.literal_components.contains(name);
        }
        // Every stored literal is ASCII, and ASCII case folding maps ASCII to
        // ASCII, so a name containing any non-ASCII byte cannot equal one.
        if !name.is_ascii() {
            return false;
        }
        if name.len() <= FOLD_BUF {
            let mut buf = [0u8; FOLD_BUF];
            let buf = &mut buf[..name.len()];
            buf.copy_from_slice(name.as_bytes());
            buf.make_ascii_lowercase();
            // Lowercasing ASCII yields ASCII, which is always valid UTF-8.
            let folded = std::str::from_utf8(buf).expect("ascii stays utf-8");
            return self.literal_components.contains(folded);
        }
        self.literal_components.contains(&name.to_ascii_lowercase())
    }

    /// Match a single file/directory name. Used by the walker to prune
    /// subtrees before descending.
    pub fn matches_component(&self, name: &str) -> bool {
        if self.empty {
            return false;
        }
        self.matches_literal(name) || self.component.is_match(name)
    }

    /// Match a path against the full-path patterns only. The path *and its
    /// ancestors* are tested, so a pattern matching a directory ignores
    /// everything beneath it — the same semantics the walker gets by
    /// pruning that directory before descending.
    pub fn matches_path_pattern(&self, path: &Path) -> bool {
        if self.path.is_empty() {
            return false;
        }
        let mut cur = Some(path);
        while let Some(p) = cur {
            if self.path.is_match(p) {
                return true;
            }
            cur = p.parent();
        }
        false
    }

    /// Match a full path: either a full-path pattern hits it (or an
    /// ancestor), or any single component matches a component pattern.
    /// Used for watcher events, where walk-time pruning never saw the path.
    pub fn matches_path(&self, path: &Path) -> bool {
        if self.empty {
            return false;
        }
        if self.matches_path_pattern(path) {
            return true;
        }
        path.components().any(|c| match c {
            // Routed through `matches_component` rather than `self.component`
            // directly, or the literal patterns would be invisible here and the
            // watcher would index what the walker prunes.
            std::path::Component::Normal(name) => self.matches_component(&name.to_string_lossy()),
            _ => false,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}
