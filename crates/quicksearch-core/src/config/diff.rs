//! Config diffing: [`diff_actions`] turns two [`Config`]s into the
//! [`IndexWork`] reconciliation plan and its restart/rebuild flags.

use std::collections::BTreeSet;

use super::*;

/// What must happen to the *stored index* to bring it back in line with the
/// configuration, short of deleting and rebuilding it.
///
/// Every field is independently satisfiable and applying the whole is
/// idempotent — which is what lets the same plan come from a live edit and
/// from the `config_validation` fingerprint of a config hand-edited while
/// the app was closed. [`crate::scope`] applies it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexWork {
    /// Deconfigured roots, in stored spelling. Purely a row delete: a root
    /// whose folder is gone is handled the same as one that still exists.
    pub drop_roots: Vec<String>,
    /// Ignore/hidden rules narrowed; surviving rows are re-tested.
    pub prune_scope: bool,
    /// Symlink following turned off. A followed target is stored under its
    /// own canonical path, which can be outside every root — with links off
    /// no walk and no root range would ever reach it again, so every row
    /// outside the roots goes.
    pub drop_aliases: bool,
    /// `content_extensions` changed. Re-tested both ways: newly-included
    /// files go back to pending, newly-excluded ones give up text and FTS
    /// but keep the name/path row filename search needs.
    pub reconcile_content: bool,
    /// `store_text_for_snippets` turned on; rows extracted under the old
    /// setting kept no text, so they must run again.
    pub restore_text: bool,
    /// `store_text_for_snippets` turned off.
    pub drop_text: bool,
    /// Newly in-scope files exist only on disk, so only a walk can find them.
    pub reindex: bool,
}

impl IndexWork {
    pub fn is_empty(&self) -> bool {
        *self == IndexWork::default()
    }

    /// Fold `other` in so one pass satisfies both — for an edit arriving
    /// while the previous one is still being applied. The union is the only
    /// thing certainly enough, and every part is idempotent.
    pub fn merge_from(&mut self, other: &IndexWork) {
        for root in &other.drop_roots {
            if !self.drop_roots.contains(root) {
                self.drop_roots.push(root.clone());
            }
        }
        self.drop_aliases |= other.drop_aliases;
        self.prune_scope |= other.prune_scope;
        self.reconcile_content |= other.reconcile_content;
        self.restore_text |= other.restore_text;
        self.drop_text |= other.drop_text;
        self.reindex |= other.reindex;
    }

    /// Whether this touches stored rows, as opposed to only asking for a walk.
    pub fn touches_index(&self) -> bool {
        !self.drop_roots.is_empty()
            || self.drop_aliases
            || self.prune_scope
            || self.reconcile_content
            || self.restore_text
            || self.drop_text
    }

    /// Whether applying this scans rows under each surviving root, instead
    /// of only deleting whole ranges.
    pub fn scans_rows(&self) -> bool {
        self.prune_scope || self.reconcile_content || self.restore_text || self.drop_text
    }

    /// The plan in one line for the log, naming what changed.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.drop_roots.is_empty() {
            parts.push(format!(
                "{} root(s) no longer indexed",
                self.drop_roots.len()
            ));
        }
        if self.prune_scope {
            parts.push("narrowed ignore or hidden-file rules".into());
        }
        if self.drop_aliases {
            parts.push("symlinks no longer followed".into());
        }
        if self.reconcile_content {
            parts.push("changed content extensions".into());
        }
        if self.restore_text {
            parts.push("snippet text turned on".into());
        }
        if self.drop_text {
            parts.push("snippet text turned off".into());
        }
        if parts.is_empty() {
            // `touches_index` is false here, so no caller logs it today.
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
/// paths — an unresolvable root is compared as spelled.
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

/// Whether `new` accepts everything `old` did and more.
///
/// Trap: an empty list means "no filter, everything allowed" — a superset of
/// every other list, which plain set arithmetic gets backwards.
fn filter_widened(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool {
    match (old.is_empty(), new.is_empty()) {
        (_, true) => !old.is_empty(),
        (true, false) => false,
        (false, false) => new.difference(old).next().is_some(),
    }
}

/// What an edit means for the running services and the stored index.
///
/// The rule: a wipe is only for data that can no longer be read or compared;
/// everything else is a difference, and differences reconcile. Roots, ignore
/// patterns and content extensions compare as **sets** after normalization,
/// so reordering or re-spelling is not a change.
pub fn diff_actions(old: &Config, new: &Config) -> ConfigActions {
    let old_roots = old.normalized_indexing_paths();
    let new_roots = new.normalized_indexing_paths();

    // The only three: a different key makes the file unreadable, the
    // tokenizer is baked into the FTS table definition, and `hash_length`
    // decides what bytes a stored hash covers. (`use_keychain` changes only
    // where the key is remembered.)
    let requires_rebuild = old.processing.hash_length != new.processing.hash_length
        || old.processing.tokenize != new.processing.tokenize
        || old.security.password_protected != new.security.password_protected
        || old.security.salt != new.security.salt;

    let mut work = IndexWork::default();
    if !requires_rebuild {
        work.drop_roots = old_roots.difference(&new_roots).cloned().collect();

        let old_ignores: BTreeSet<&str> = old
            .indexing
            .ignore_patterns
            .iter()
            .map(|s| s.trim())
            .collect();
        let new_ignores: BTreeSet<&str> = new
            .indexing
            .ignore_patterns
            .iter()
            .map(|s| s.trim())
            .collect();

        // Hidden files narrow the walk exactly as an added ignore pattern does.
        work.prune_scope = new_ignores.difference(&old_ignores).next().is_some()
            || (old.indexing.include_hidden && !new.indexing.include_hidden);

        // Symlinks do not: a followed target inside a root is a row a direct
        // walk produces anyway, but one outside every root is stranded where
        // no walk and no per-root scan will look again.
        work.drop_aliases = old.indexing.follow_symlinks && !new.indexing.follow_symlinks;

        let old_content = content_filter_set(&old.indexing.content_extensions);
        let new_content = content_filter_set(&new.indexing.content_extensions);
        let content_widened = filter_widened(&old_content, &new_content);
        work.reconcile_content = old_content != new_content;

        let old_store = old.processing.store_text_for_snippets;
        let new_store = new.processing.store_text_for_snippets;
        work.restore_text = !old_store && new_store;
        work.drop_text = old_store && !new_store;

        // Widening only adds files, and a file absent from the index is not
        // findable from it — only a walk produces those rows. Re-extraction
        // needs a run too: the content pass runs as part of one.
        work.reindex = new_roots.difference(&old_roots).next().is_some()
            || old_ignores.difference(&new_ignores).next().is_some()
            || (!old.indexing.include_hidden && new.indexing.include_hidden)
            || (!old.indexing.follow_symlinks && new.indexing.follow_symlinks)
            || content_widened
            || work.restore_text;
    }

    ConfigActions {
        requires_rebuild,
        work,
        search_db_changed: old.paths.database_path != new.paths.database_path,
    }
}
