//! Config diffing: [`diff_actions`] turns two [`Config`]s into the
//! [`IndexWork`] reconciliation plan and its restart/rebuild flags.

use std::collections::BTreeSet;

use super::*;

/// What must happen to the *stored index* to bring it back in line with the
/// configuration, short of deleting and rebuilding it.
///
/// Every field is independently satisfiable and applying the whole is
/// idempotent, so the same plan can come from a live edit or from the
/// `config_validation` fingerprint of a config hand-edited while the app was
/// closed. [`crate::scope`] applies it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexWork {
    /// Deconfigured roots, in stored spelling. Purely a row delete: a root
    /// whose folder is gone is handled the same as one that still exists.
    pub drop_roots: Vec<String>,
    /// Ignore/hidden rules narrowed; surviving rows are re-tested.
    pub prune_scope: bool,
    /// Symlink following turned off. A followed target is stored under its
    /// own canonical path, which can be outside every root; with links off no
    /// walk and no root range would ever reach it again, so every row outside
    /// the roots goes.
    pub drop_aliases: bool,
    /// `content_extensions` or `maximum_text_file_size` changed. Both decide
    /// whether a file's text is read, and [`crate::scope::advance`] re-runs
    /// the whole predicate, so one flag covers them. Newly included rows go
    /// back to pending; newly excluded ones lose text and FTS but keep their
    /// row.
    pub reconcile_content: bool,
    /// Newly in-scope files exist only on disk, so only a walk can find them.
    pub reindex: bool,
}

impl IndexWork {
    pub fn is_empty(&self) -> bool {
        *self == IndexWork::default()
    }

    /// Fold `other` in so one pass satisfies both, for an edit arriving while
    /// the previous one is still being applied. The union is the only thing
    /// certainly enough, and every part is idempotent.
    pub fn merge_from(&mut self, other: &IndexWork) {
        for root in &other.drop_roots {
            if !self.drop_roots.contains(root) {
                self.drop_roots.push(root.clone());
            }
        }
        self.drop_aliases |= other.drop_aliases;
        self.prune_scope |= other.prune_scope;
        self.reconcile_content |= other.reconcile_content;
        self.reindex |= other.reindex;
    }

    /// Whether this touches stored rows, as opposed to only asking for a walk.
    pub fn touches_index(&self) -> bool {
        !self.drop_roots.is_empty()
            || self.drop_aliases
            || self.prune_scope
            || self.reconcile_content
    }

    /// Whether applying this scans rows under each surviving root, instead
    /// of only deleting whole ranges.
    pub fn scans_rows(&self) -> bool {
        self.prune_scope || self.reconcile_content
    }

    /// The plan in one line for the log, naming what changed.
    pub fn summary(&self) -> String {
        let roots = format!("{} root(s) no longer indexed", self.drop_roots.len());
        let parts: Vec<&str> = [
            (!self.drop_roots.is_empty(), roots.as_str()),
            (self.prune_scope, "narrowed ignore or hidden-file rules"),
            (self.drop_aliases, "symlinks no longer followed"),
            (
                self.reconcile_content,
                "changed which files have their text read",
            ),
        ]
        .into_iter()
        .filter_map(|(changed, part)| changed.then_some(part))
        .collect();
        if parts.is_empty() {
            // `touches_index` is false here, so no caller logs it.
            return "no stored rows affected".into();
        }
        parts.join("; ")
    }
}

/// What running services must do after a config edit.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigActions {
    /// The stored file cannot be read or compared under the new config.
    /// Only three settings do this: the FTS tokenizer, `hash_length`, and
    /// the encryption key (see [`diff_actions`]).
    pub requires_rebuild: bool,
    /// In-place reconciliation; empty when `requires_rebuild` is set.
    pub work: IndexWork,
    pub search_db_changed: bool,
}

/// Roots inside other roots, as `(child, parent)` pairs (exact duplicates
/// reported once). Disallowed because one walker per root would race for the
/// same files and split progress. Compared on best-effort canonicalized
/// paths; an unresolvable root is compared as spelled.
pub fn nested_roots(roots: &[String]) -> Vec<(String, String)> {
    let resolved: Vec<PathBuf> = roots
        .iter()
        .map(|r| {
            let p = expand_tilde(r);
            fs::canonicalize(&p).unwrap_or(p)
        })
        .collect();
    let mut out = Vec::new();
    for (i, child) in resolved.iter().enumerate() {
        for (j, parent) in resolved.iter().enumerate() {
            let duplicate = child == parent && i > j;
            let properly_nested = child != parent && child.starts_with(parent);
            if duplicate || properly_nested {
                out.push((roots[i].clone(), roots[j].clone()));
            }
        }
    }
    out
}

/// `content_extensions` normalized the way [`content_allowed`] compares:
/// comments stripped, leading dot optional, case-insensitive. Equal sets
/// filter identically however they are spelled or ordered.
fn content_filter_set(list: &[String]) -> BTreeSet<String> {
    content_filter_entries(list)
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect()
}

/// Whether `new` accepts something `old` did not.
///
/// Trap: an empty list means "no filter, everything allowed", a superset of
/// every other list, which plain set arithmetic gets backwards.
fn filter_widened(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool {
    !old.is_empty() && (new.is_empty() || !new.is_subset(old))
}

/// The ignore patterns as a set, whitespace-trimmed as they compile.
fn ignore_set(c: &Config) -> BTreeSet<&str> {
    c.indexing
        .ignore_patterns
        .iter()
        .map(|s| s.trim())
        .collect()
}

/// What an edit means for the running services and the stored index.
///
/// The rule: a wipe is only for data that can no longer be read or compared;
/// everything else is a difference, and differences reconcile. Roots, ignore
/// patterns and content extensions compare as sets after normalization,
/// so reordering or re-spelling is not a change.
pub fn diff_actions(old: &Config, new: &Config) -> ConfigActions {
    let search_db_changed = old.paths.database_path != new.paths.database_path;

    // The only four: a different key makes the file unreadable, the
    // tokenizer and the position setting are baked into the FTS table
    // definition, and `hash_length` decides what bytes a stored hash covers.
    // (`use_keychain` changes only where the key is remembered.)
    let requires_rebuild = old.processing.hash_length != new.processing.hash_length
        || old.processing.tokenize != new.processing.tokenize
        || old.processing.fts_positions() != new.processing.fts_positions()
        || old.security.password_protected != new.security.password_protected
        || old.security.salt != new.security.salt;
    if requires_rebuild {
        return ConfigActions {
            requires_rebuild,
            work: IndexWork::default(),
            search_db_changed,
        };
    }

    let (old_roots, new_roots) = (
        old.normalized_indexing_paths(),
        new.normalized_indexing_paths(),
    );
    let (old_ignores, new_ignores) = (ignore_set(old), ignore_set(new));
    let (old_idx, new_idx) = (&old.indexing, &new.indexing);
    let old_content = content_filter_set(&old_idx.content_extensions);
    let new_content = content_filter_set(&new_idx.content_extensions);
    let old_cap = old.processing.maximum_text_file_size;
    let new_cap = new.processing.maximum_text_file_size;

    let work = IndexWork {
        drop_roots: old_roots.difference(&new_roots).cloned().collect(),
        // Hidden files narrow the walk exactly as an added ignore pattern does.
        prune_scope: !new_ignores.is_subset(&old_ignores)
            || (old_idx.include_hidden && !new_idx.include_hidden),
        // Symlinks do not: a followed target inside a root is a row a direct
        // walk produces anyway, but one outside every root is stranded where
        // no walk and no per-root scan will look again.
        drop_aliases: old_idx.follow_symlinks && !new_idx.follow_symlinks,
        // See the field: the size cap decides what the extension filter does.
        reconcile_content: old_content != new_content || old_cap != new_cap,
        // Widening only adds files, and a file absent from the index is not
        // findable from it: only a walk produces those rows. Re-extraction
        // needs a run too, since the content pass runs as part of one; a
        // raised cap widens like an added extension.
        reindex: !new_roots.is_subset(&old_roots)
            || !old_ignores.is_subset(&new_ignores)
            || (!old_idx.include_hidden && new_idx.include_hidden)
            || (!old_idx.follow_symlinks && new_idx.follow_symlinks)
            || filter_widened(&old_content, &new_content)
            || new_cap > old_cap,
    };

    ConfigActions {
        requires_rebuild,
        work,
        search_db_changed,
    }
}
