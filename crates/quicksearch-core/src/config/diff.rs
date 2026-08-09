//! Config diffing: [`diff_actions`] turns two [`Config`]s into the
//! [`IndexWork`] reconciliation plan and its restart/rebuild flags.

use std::collections::BTreeSet;

use super::*;

/// What must happen to the *stored index* to bring it back in line with the
/// configuration, short of deleting and rebuilding it.
///
/// Every field is independently satisfiable and the whole thing is
/// idempotent: applying it twice does nothing the second time, which is what
/// lets the same plan be produced from a live config edit and from the
/// `config_validation` fingerprint of a config that was hand-edited while the
/// app was closed. See [`crate::scope`] for the pass that applies it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexWork {
    /// Roots that are no longer configured, in `files.path` spelling. Every
    /// row beneath one is deleted; no filesystem access is involved, so a
    /// root whose folder is gone is handled the same as one that still
    /// exists.
    pub drop_roots: Vec<String>,
    /// The ignore/hidden rules narrowed. Stored rows under the surviving
    /// roots are re-tested against them and the ones the walker would no
    /// longer emit are deleted.
    pub prune_scope: bool,
    /// Symlink following was turned off. A followed target is stored under
    /// its own canonical path, which can be outside every root; with links
    /// off no walk can produce such a row, and no root's range would ever
    /// visit it again, so every row outside the roots goes.
    pub drop_aliases: bool,
    /// The `content_extensions` filter changed. Kept rows are re-tested
    /// against it in both directions: newly-included files go back to
    /// pending, newly-excluded ones give up their text, properties and FTS
    /// row but keep the name/path row that filename search needs.
    pub reconcile_content: bool,
    /// `store_text_for_snippets` turned on. Rows that finished extraction
    /// under the old setting kept no text, so they must run again.
    pub restore_text: bool,
    /// `store_text_for_snippets` turned off. The stored text is dead weight
    /// now; dropping it leaves full-text search working and only costs
    /// snippets.
    pub drop_text: bool,
    /// Files that are newly in scope exist only on disk — nothing in the
    /// index points at them, so a full walk has to go and find them.
    pub reindex: bool,
}

impl IndexWork {
    /// Whether there is nothing to do at all.
    pub fn is_empty(&self) -> bool {
        *self == IndexWork::default()
    }

    /// Fold `other` in, so one pass satisfies both. For a second config edit
    /// arriving while the first is still being applied: the union is the
    /// only thing that is certainly enough, and every part is idempotent.
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

    /// Whether any part of this touches stored rows, as opposed to only
    /// asking for another walk.
    pub fn touches_index(&self) -> bool {
        !self.drop_roots.is_empty()
            || self.drop_aliases
            || self.prune_scope
            || self.reconcile_content
            || self.restore_text
            || self.drop_text
    }

    /// Whether applying this means scanning the rows under each surviving
    /// root, rather than just deleting whole ranges.
    pub fn scans_rows(&self) -> bool {
        self.prune_scope || self.reconcile_content || self.restore_text || self.drop_text
    }

    /// The plan in one line, for the log entry that announces the scan.
    /// Names what changed rather than what will happen to the rows.
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
            // `touches_index` is false here, so no caller logs this; a
            // placeholder beats an empty pair of parentheses if one ever does.
            return "no stored rows affected".into();
        }
        parts.join("; ")
    }
}

/// What running services must do after a config edit. Computed by the GUI
/// (the only runtime editor) after saving, and by the coordinator from the
/// config it was already holding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigActions {
    /// The stored file cannot be read or compared under the new
    /// configuration and must be deleted and rebuilt from scratch. Reserved
    /// for the three settings that leave no other option: the FTS tokenizer
    /// (baked into the table definition), the hash length (stored hashes
    /// become incomparable) and the encryption key.
    pub requires_rebuild: bool,
    /// Reconciliation the index can do in place. Empty when
    /// `requires_rebuild` is set — a wipe subsumes all of it.
    pub work: IndexWork,
    /// Searches must reopen against a different database file.
    pub search_db_changed: bool,
}

/// Roots that live inside other roots, as `(child, parent)` pairs (exact
/// duplicates are reported once). Nested roots are disallowed: with one
/// walker per root they would race for the same files and split progress
/// attribution. Comparison is on best-effort canonicalized paths (an
/// unresolvable root is compared as spelled), component-wise per
/// [`crate::file_handling::UnreadableDirs::covers`].
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

/// The `content_extensions` entries that decide what a file is matched
/// against, normalized the way [`content_allowed`] compares them: comments
/// stripped, a leading dot optional, case-insensitive. Two lists with the
/// same set here filter identically, however they are spelled or ordered.
fn content_filter_set(list: &[String]) -> BTreeSet<String> {
    content_filter_entries(list)
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect()
}

/// Whether `new` accepts everything `old` did and more.
///
/// An empty list means "no filter, everything allowed", so it is a superset
/// of every other list rather than the empty set — the one case plain set
/// arithmetic gets backwards.
fn filter_widened(old: &BTreeSet<String>, new: &BTreeSet<String>) -> bool {
    match (old.is_empty(), new.is_empty()) {
        (_, true) => !old.is_empty(),
        (true, false) => false,
        (false, false) => new.difference(old).next().is_some(),
    }
}

/// What an edit means for the running services and the stored index.
///
/// The guiding rule: a wipe is only for data that cannot be read or compared
/// any more. Everything else is a difference between what the index holds and
/// what the configuration would produce, and a difference can be reconciled —
/// rows that fell out of scope are deleted ([`IndexWork::prune_scope`],
/// [`IndexWork::drop_roots`]), rows whose content scope moved are re-tested
/// ([`IndexWork::reconcile_content`]), and files that came *into* scope are
/// found by another walk ([`IndexWork::reindex`]).
///
/// Roots, ignore patterns and content extensions are compared as **sets**
/// after normalization, so reordering a list or re-spelling a root is not a
/// change at all.
pub fn diff_actions(old: &Config, new: &Config) -> ConfigActions {
    let old_roots = old.normalized_indexing_paths();
    let new_roots = new.normalized_indexing_paths();

    // Encryption on↔off or a different salt (⇒ a different key) makes the
    // on-disk file unreadable to the new configuration. The tokenizer is part
    // of the FTS table's definition, and `hash_length` decides what bytes a
    // stored hash covers, so old and new hashes cannot be compared. Those
    // three are the whole of it; the GUI's security flows drive their own
    // explicit dialog, and this covers hand-edited configs applied through
    // the generic path. `use_keychain` only changes where the key is
    // remembered, not the file.
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

        // Hidden files narrow the walk exactly the way an added ignore pattern
        // does, so they take the same route.
        work.prune_scope = new_ignores.difference(&old_ignores).next().is_some()
            || (old.indexing.include_hidden && !new.indexing.include_hidden);

        // Symlinks do not: a followed target is stored under its own canonical
        // path, which is either inside a root — where a direct walk produces
        // exactly the same row, so nothing changes — or outside every root,
        // where turning links off strands it somewhere no walk and no
        // per-root scan will ever look again.
        work.drop_aliases = old.indexing.follow_symlinks && !new.indexing.follow_symlinks;

        let old_content = content_filter_set(&old.indexing.content_extensions);
        let new_content = content_filter_set(&new.indexing.content_extensions);
        let content_widened = filter_widened(&old_content, &new_content);
        work.reconcile_content = old_content != new_content;

        let old_store = old.processing.store_text_for_snippets;
        let new_store = new.processing.store_text_for_snippets;
        work.restore_text = !old_store && new_store;
        work.drop_text = old_store && !new_store;

        // Widening only ever *adds* files, and a file that is not in the index
        // is not findable from it: only a walk can produce those rows. Text
        // that has to be extracted again needs a run for the same reason —
        // the content pass runs as part of one.
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
