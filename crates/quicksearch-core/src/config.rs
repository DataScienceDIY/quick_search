//! Configuration: TOML file, resolution rules, filter sets, and live-update
//! classification.
//!
//! File resolution (see [`Config::config_path`]): a `config.toml` sitting
//! next to the executable wins ("portable mode"); otherwise the per-user
//! XDG location is used and auto-created on first run. Relative paths
//! *inside* a config resolve against the config file's own directory, so a
//! portable folder can be moved wholesale.
//!
//! The GUI is the only writer of the file at runtime; external edits take
//! effect on next start. After editing, callers run [`diff_actions`] to
//! learn which running services must react (rebuild the index, restart the
//! watcher, repoint search).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub paths: PathConfig,
    pub indexing: IndexingConfig,
    pub processing: ProcessingConfig,
    pub search: SearchConfig,
    pub ui: UiConfig,
    pub security: SecurityConfig,
    /// File this config was loaded from; `save()` writes back to it.
    /// `None` for hand-built configs (tests), which save to the default
    /// location.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PathConfig {
    /// One or more directory roots to index. Indexing walks each root
    /// independently; duplicates and nested roots are de-duplicated by the
    /// indexer at run time.
    pub indexing_paths: Vec<String>,
    /// SQLite index location. Relative values resolve against the config
    /// file's directory; `~` expands to the home directory.
    pub database_path: String,
}

/// What to index and when — the knobs the coordinator and walker consume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IndexingConfig {
    /// Start in automatic mode: filesystem watchers apply changes as they
    /// happen and a full reindex runs every `reindex_interval_minutes`.
    pub auto_index: bool,
    pub reindex_interval_minutes: u64,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
    /// Empty = extract content from everything the extractor registry
    /// supports. Non-empty = only files with these extensions get content
    /// extraction/FTS; everything else is still listed for filename search
    /// (`content_state = NA`). Entries are case-insensitive, with or
    /// without a leading dot.
    pub content_extensions: Vec<String>,
    /// Excluded from the index entirely — never even listed. A pattern
    /// without `/` matches any single path component (so `.git` prunes
    /// whole subtrees); a pattern containing `/` or resembling a path is
    /// matched against the full path. Glob syntax (`*`, `?`, `[..]`).
    pub ignore_patterns: Vec<String>,
    /// Per-root walker thread override, keyed by the root string exactly
    /// as it appears in `indexing_paths`. Absent or 0 = auto-detect
    /// (4 for local storage, 16 for network mounts). Read at run start;
    /// a change applies to the next run.
    pub root_workers: std::collections::HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProcessingConfig {
    /// Bytes read from the head of each new or changed file. Those bytes do
    /// three jobs, so this one number sets more than the hash:
    ///
    /// 1. with the size, they identify the file (see `get_file_hash`);
    /// 2. they are the magic-byte window for MIME detection — `infer` reads
    ///    8 KiB from a path and its longest matcher needs 262 bytes, so the
    ///    default is exactly as good as opening the file, and a value under
    ///    262 makes some formats undetectable except by extension;
    /// 3. any plaintext file no larger than this is extracted during the
    ///    walk, sparing the content pass an open/read/close.
    ///
    /// Changing it invalidates stored hashes and forces a rebuild.
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_text_file_size: u64,
    pub batch_size: usize,
    pub fts_update_batch_size: usize,
    pub tokenize: String,
    /// When `true` (default), extracted text is stored zstd-compressed in
    /// `documents_text` so search results can render snippet/highlight
    /// previews without re-reading the source file. When `false` the
    /// inverted FTS5 index still gets the tokens (so queries return the
    /// same hits) but nothing is stored alongside; full-text results carry
    /// no snippets, can't be case-verified or occurrence-ranked, and fuzzy
    /// full-text search is unavailable. This mode drops the on-disk
    /// footprint to roughly what stock Baloo uses.
    pub store_text_for_snippets: bool,
}

/// Fuzzy edit distances above this are allowed but warned about: matches
/// become dominated by coincidence and every fuzzy pass slows down.
pub const FUZZY_EDITS_WARN_ABOVE: usize = 3;

/// Search-side preferences, shared by the GUI and the CLI mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SearchConfig {
    /// Whether the fuzzy stages start enabled.
    pub fuzzy_default: bool,
    /// Ceiling on the fuzzy stages' Levenshtein budget. The budget grows
    /// with the term (one edit per three characters) up to this cap, so 2
    /// means "1 edit for 3–5 character terms, 2 for anything longer".
    /// 0 disables the fuzzy stages entirely; above
    /// [`FUZZY_EDITS_WARN_ABOVE`] the GUI and CLI warn.
    pub fuzzy_max_edits: usize,
    /// Hard cap on buffered/displayed results per search.
    pub display_limit: usize,
    /// Streaming batch size — how many hits per update event.
    pub results_per_page: usize,
    /// How long the GUI waits after the last keystroke before searching.
    pub debounce_ms: u64,
}

impl SearchConfig {
    /// The caution to show next to `fuzzy_max_edits`, or `None` when the
    /// value is sane. One wording, shared by the Options window and the
    /// terminal mode.
    pub fn fuzzy_edits_warning(&self) -> Option<String> {
        if self.fuzzy_max_edits <= FUZZY_EDITS_WARN_ABOVE {
            return None;
        }
        Some(format!(
            "Fuzzy edit distance {} is above the recommended maximum of {}; \
             results will be dominated by false matches and every fuzzy pass \
             gets slower.",
            self.fuzzy_max_edits, FUZZY_EDITS_WARN_ABOVE
        ))
    }
}

impl Default for PathConfig {
    fn default() -> Self {
        PathConfig {
            indexing_paths: vec![default_home_path()],
            database_path: default_db_path().to_string_lossy().into_owned(),
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        IndexingConfig {
            auto_index: true,
            reindex_interval_minutes: 24 * 60,
            follow_symlinks: false,
            include_hidden: false,
            content_extensions: Vec::new(),
            ignore_patterns: default_ignore_patterns(),
            root_workers: std::collections::HashMap::new(),
        }
    }
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        ProcessingConfig {
            hash_length: 1024 * 8,
            maximum_text_size: 1024 * 256,
            maximum_text_file_size: 1024 * 1024 * 2,
            batch_size: 200,
            fts_update_batch_size: 1000,
            tokenize: "trigram".to_string(),
            store_text_for_snippets: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            fuzzy_default: false,
            fuzzy_max_edits: 2,
            display_limit: 1000,
            results_per_page: 100,
            debounce_ms: 150,
        }
    }
}

/// Index encryption. The password itself is never stored anywhere — only
/// the KDF salt lives here, and it is not a secret (it makes the derivation
/// unique per install, nothing more).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SecurityConfig {
    /// Encrypt the index (SQLCipher) with a password asked for at startup.
    /// Turning this on or off requires deleting and rebuilding the index.
    pub password_protected: bool,
    /// KDF salt, exactly 32 lowercase hex digits (16 bytes). Written by the
    /// app at the moment a password is set — never generated as a default,
    /// never edited by hand, and never shown in the GUI. Absent until a
    /// password exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Store the derived key in the OS keychain (Secret Service / Windows
    /// Credential Manager) and skip the startup prompt on this machine.
    pub use_keychain: bool,
}

impl SecurityConfig {
    /// The decoded salt. An error means the config is unusable for
    /// unlocking: protection is on but no salt was ever written, or the
    /// value was tampered with — both surfaced to the user, never guessed
    /// around.
    pub fn salt_bytes(&self) -> Result<[u8; crate::security::SALT_LEN], String> {
        match &self.salt {
            None => Err("password protection is enabled but the config has no salt; \
                         disable protection or set the password again"
                .to_string()),
            Some(hex) => crate::security::salt_from_hex(hex)
                .map_err(|e| format!("invalid salt in config: {}", e)),
        }
    }
}

/// Interface preferences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Zoom factor for the whole GUI — fonts, spacing, and widgets scale
    /// together. Applied live; also adjustable with Ctrl +/-/0 at runtime
    /// (keyboard zoom isn't persisted).
    pub scale: f32,
    /// Roots the "live updates are disabled" warning has already been shown
    /// for, as they appear in `indexing_paths`.
    ///
    /// Keyed by root rather than a single flag so that adding a folder warns
    /// again — the trade-off changed — while restarting the app does not.
    /// Pruned to the current root set whenever the folder list is applied.
    pub watch_cap_warned_roots: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            scale: 1.1,
            watch_cap_warned_roots: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            paths: PathConfig::default(),
            indexing: IndexingConfig::default(),
            processing: ProcessingConfig::default(),
            search: SearchConfig::default(),
            ui: UiConfig::default(),
            security: SecurityConfig::default(),
            source: None,
        }
    }
}

/// Directories and files excluded from a fresh index.
///
/// Build artefacts everywhere, plus the things a Windows home directory or
/// drive root contains that are actively harmful to index:
///
/// - `$RECYCLE.BIN` holds *deleted* files. Indexing it puts their contents
///   back into search results, which is a privacy problem rather than mere
///   noise. It is Hidden+System, so the walk already skips it by default —
///   this covers the user who legitimately turns `include_hidden` on. (`$` is
///   not a glob metacharacter, so the name is matched literally.)
/// - `System Volume Information` is ACL-denied even to Administrators, so
///   without it every run logs an unreadable-directory warning per drive.
/// - The kernel's paging and hibernation files are multi-gigabyte and
///   permanently locked.
/// - `Thumbs.db` and `desktop.ini` occur in thousands of folders and carry no
///   searchable content.
///
/// Deliberately absent: any exclusion for `C:\Windows`. The default root is
/// the user's profile, so it would never apply, and a pattern general enough
/// to catch it would also catch a user folder named `Windows`.
/// `config_example.toml` documents it for people who add a drive root.
fn default_ignore_patterns() -> Vec<String> {
    let mut patterns = vec![".git", "node_modules", "*.tmp", ".venv", "venv", "*.pdf"];
    if cfg!(windows) {
        patterns.extend([
            "$RECYCLE.BIN",
            "System Volume Information",
            "pagefile.sys",
            "hiberfil.sys",
            "swapfile.sys",
            "Thumbs.db",
            "desktop.ini",
        ]);
    }
    patterns.into_iter().map(str::to_string).collect()
}

/// Platform-sensible default for the first indexing root when no config
/// exists. `$HOME` on Unix, `%USERPROFILE%` on Windows; falls back to the
/// current directory.
fn default_home_path() -> String {
    if let Some(home) = crate::platform::home_dir() {
        return home.to_string_lossy().into_owned();
    }
    ".".to_string()
}

/// `config.toml` beside the running executable, if the executable's
/// location is known. Existence is checked by the caller.
fn portable_config_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config.toml"))
}

/// Per-user config directory: `$XDG_CONFIG_HOME`/`~/.config` on Unix,
/// `%APPDATA%` on Windows.
fn config_base_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("APPDATA") {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(d);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(home) = crate::platform::home_dir() {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".")
}

/// Per-user data directory: `$XDG_DATA_HOME`/`~/.local/share` on Unix,
/// `%LOCALAPPDATA%` on Windows.
fn data_base_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(d);
        if p.is_absolute() {
            return p;
        }
    }
    if let Some(home) = crate::platform::home_dir() {
        return PathBuf::from(home).join(".local").join("share");
    }
    PathBuf::from(".")
}

/// Default index location when the config doesn't name one.
pub fn default_db_path() -> PathBuf {
    data_base_dir().join("quicksearch").join("index.sqlite")
}

/// Expand a leading `~`/`~/` to the home directory. Other `~user` forms are
/// left untouched.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        if let Some(home) = crate::platform::home_dir() {
            let mut p = PathBuf::from(home);
            if path.len() > 2 {
                p.push(&path[2..]);
            }
            return p;
        }
    }
    PathBuf::from(path)
}

impl Config {
    /// The config file this process should use: the portable override next
    /// to the binary when present, else the XDG location.
    pub fn config_path() -> PathBuf {
        if let Some(p) = portable_config_path() {
            if p.exists() {
                return p;
            }
        }
        config_base_dir().join("quicksearch").join("config.toml")
    }

    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::config_path())
    }

    /// Load from an explicit path. A missing file is created with defaults
    /// (directories included). The loaded config remembers `path` and
    /// `save()` writes back to it.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        if path.exists() {
            let content = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read config file {}: {}", path.display(), e))?;
            let mut cfg: Config = toml::from_str(&content)
                .map_err(|e| format!("Failed to parse config file {}: {}", path.display(), e))?;
            cfg.source = Some(path.to_path_buf());
            Ok(cfg)
        } else {
            let mut cfg = Config::default();
            cfg.source = Some(path.to_path_buf());
            cfg.save()?;
            Ok(cfg)
        }
    }

    /// Write back to the file this config was loaded from (or the default
    /// location), creating parent directories as needed. Raw values are
    /// written verbatim — relative paths in a portable config stay relative.
    pub fn save(&self) -> Result<(), String> {
        let path = self
            .source
            .clone()
            .unwrap_or_else(Self::config_path);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| format!("Failed to create config dir {}: {}", dir.display(), e))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        fs::write(&path, content)
            .map_err(|e| format!("Failed to write config file {}: {}", path.display(), e))?;
        Ok(())
    }

    /// Directory that relative in-config paths resolve against: the config
    /// file's own directory, falling back to the CWD for sourceless configs.
    fn base_dir(&self) -> PathBuf {
        self.source
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `database_path` with `~` expanded and relative values resolved
    /// against the config file's directory.
    pub fn resolved_database_path(&self) -> PathBuf {
        let p = expand_tilde(&self.paths.database_path);
        if p.is_absolute() {
            p
        } else {
            self.base_dir().join(p)
        }
    }

    /// `indexing_paths` with the same resolution rules as
    /// [`resolved_database_path`].
    pub fn resolved_indexing_paths(&self) -> Vec<PathBuf> {
        self.paths
            .indexing_paths
            .iter()
            .map(|raw| {
                let p = expand_tilde(raw);
                if p.is_absolute() {
                    p
                } else {
                    self.base_dir().join(p)
                }
            })
            .collect()
    }
}

/// Whether a file's content (text extraction + FTS) should be indexed under
/// the `content_extensions` filter. Files that fail this are still listed
/// for filename search. Empty filter = everything allowed.
pub fn content_allowed(path: &Path, cfg: &Config) -> bool {
    if cfg.indexing.content_extensions.is_empty() {
        return true;
    }
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    cfg.indexing
        .content_extensions
        .iter()
        .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(&ext))
}

/// Compiled ignore patterns, split by matching scope: patterns without a
/// path separator match any single path component; the rest match the full
/// path. Both use glob syntax.
#[derive(Debug)]
pub struct IgnoreSet {
    component: globset::GlobSet,
    path: globset::GlobSet,
    empty: bool,
}

impl IgnoreSet {
    pub fn compile(patterns: &[String]) -> Result<IgnoreSet, String> {
        let mut component = globset::GlobSetBuilder::new();
        let mut path = globset::GlobSetBuilder::new();
        for pat in patterns {
            // Trailing separators are how people naturally write directory
            // patterns ("/tmp/"); paths compare without them, so strip.
            let pat = pat.trim().trim_end_matches(['/', '\\']);
            if pat.is_empty() {
                continue;
            }
            let glob = globset::GlobBuilder::new(pat)
                .literal_separator(false)
                // Match the filesystem's own rules, or `node_modules` fails to
                // exclude `Node_Modules`. globset already handles the other
                // half of Windows compatibility on its own: `Candidate` folds
                // `\` to `/` when matching, and backslash-as-escape is off
                // wherever `\` is a separator.
                .case_insensitive(cfg!(any(windows, target_os = "macos")))
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
        let empty = component.is_empty() && path.is_empty();
        Ok(IgnoreSet {
            component,
            path,
            empty,
        })
    }

    /// Match a single file/directory name. Used by the walker to prune
    /// subtrees before descending.
    pub fn matches_component(&self, name: &str) -> bool {
        !self.empty && self.component.is_match(name)
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
        path.components().any(|c| {
            matches!(c, std::path::Component::Normal(name)
                if self.component.is_match(Path::new(name)))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

/// What running services must do after a config edit. Computed by the GUI
/// (the only runtime editor) after saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigActions {
    /// The stored index no longer matches how it would be built — offer the
    /// user a rebuild (mirrors the `config_validation` mechanism).
    pub requires_rebuild: bool,
    /// Watched roots or link semantics changed — restart the watcher.
    pub restart_watcher: bool,
    /// Searches must reopen against a different database file.
    pub search_db_changed: bool,
}

/// Roots that live inside other roots, as `(child, parent)` pairs (exact
/// duplicates are reported once). Nested roots are disallowed: with one
/// walker per root they would race for the same files and split progress
/// attribution. Comparison is on best-effort canonicalized paths (an
/// unresolvable root is compared as spelled) and is component-boundary
/// aware — `/a/bc` is not under `/a/b`.
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
            if i == j {
                continue;
            }
            if child == parent {
                if i > j {
                    out.push((roots[i].clone(), roots[j].clone()));
                }
            } else if child.starts_with(parent) {
                out.push((roots[i].clone(), roots[j].clone()));
            }
        }
    }
    out
}

pub fn diff_actions(old: &Config, new: &Config) -> ConfigActions {
    let roots_changed = old.paths.indexing_paths != new.paths.indexing_paths;
    // Encryption on↔off or a different salt (⇒ a different key) makes the
    // on-disk file unreadable to the new configuration: rebuild. The GUI's
    // security flows drive their own explicit rebuild dialog; this covers
    // hand-edited configs applied through the generic path. `use_keychain`
    // only changes where the key is remembered, not the file.
    let requires_rebuild = old.processing.hash_length != new.processing.hash_length
        || old.processing.tokenize != new.processing.tokenize
        || old.indexing.include_hidden != new.indexing.include_hidden
        || old.indexing.ignore_patterns != new.indexing.ignore_patterns
        || old.indexing.content_extensions != new.indexing.content_extensions
        || old.security.password_protected != new.security.password_protected
        || old.security.salt != new.security.salt
        || roots_changed;
    ConfigActions {
        requires_rebuild,
        restart_watcher: requires_rebuild
            || roots_changed
            || old.indexing.follow_symlinks != new.indexing.follow_symlinks,
        search_db_changed: old.paths.database_path != new.paths.database_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "quicksearch-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn fresh_install_defaults_to_home_as_only_root() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        assert!(!path.exists(), "fresh install: no config yet");
        let cfg = Config::load_from(&path).unwrap();
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("test environment has a home dir");
        assert_eq!(
            cfg.paths.indexing_paths,
            vec![home],
            "the user's home folder must be the only default index root"
        );
        // The auto-created file round-trips identically.
        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.paths.indexing_paths, cfg.paths.indexing_paths);
        // A [paths] section that omits indexing_paths also falls back to
        // home, not to an empty list.
        fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
        let partial = Config::load_from(&path).unwrap();
        assert_eq!(partial.paths.indexing_paths, cfg.paths.indexing_paths);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_created_with_defaults() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let cfg = Config::load_from(&path).unwrap();
        assert!(path.exists(), "default config file should be written");
        assert_eq!(cfg.search.display_limit, 1000);
        assert_eq!(cfg.search.results_per_page, 100);
        assert!(cfg.indexing.auto_index);
        assert_eq!(cfg.source.as_deref(), Some(path.as_path()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_file_gets_section_defaults() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(&path, "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.paths.indexing_paths, vec!["/x".to_string()]);
        assert_eq!(cfg.processing.batch_size, 200, "missing sections default");
        assert_eq!(cfg.search.debounce_ms, 150);
        assert!((cfg.ui.scale - 1.1).abs() < f32::EPSILON);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_paths_resolve_against_config_dir() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"data\"]\ndatabase_path=\"index.sqlite\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.resolved_database_path(), dir.join("index.sqlite"));
        assert_eq!(cfg.resolved_indexing_paths(), vec![dir.join("data")]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_keeps_relative_paths_portable() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"data\"]\ndatabase_path=\"index.sqlite\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path).unwrap();
        cfg.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("database_path = \"index.sqlite\""),
            "relative path must survive a save round-trip: {}",
            text
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilde_expansion() {
        if std::env::var_os("HOME").is_none() {
            return; // nothing to assert without a home dir
        }
        let mut cfg = Config::default();
        cfg.paths.database_path = "~/qs/index.sqlite".to_string();
        let resolved = cfg.resolved_database_path();
        assert!(resolved.is_absolute());
        assert!(!resolved.to_string_lossy().contains('~'));
    }

    #[test]
    fn content_allowed_semantics() {
        let mut cfg = Config::default();
        assert!(content_allowed(Path::new("/a/b.xyz"), &cfg), "empty = all");
        cfg.indexing.content_extensions = vec!["txt".into(), ".MD".into()];
        assert!(content_allowed(Path::new("/a/b.txt"), &cfg));
        assert!(content_allowed(Path::new("/a/B.TXT"), &cfg));
        assert!(content_allowed(Path::new("/a/readme.md"), &cfg), "leading dot + case in filter");
        assert!(!content_allowed(Path::new("/a/b.pdf"), &cfg));
        assert!(!content_allowed(Path::new("/a/noext"), &cfg));
    }

    #[test]
    fn ignore_set_component_vs_path() {
        let set = IgnoreSet::compile(&[
            ".git".to_string(),
            "*.tmp".to_string(),
            "/home/*/secret".to_string(),
            "".to_string(), // blank lines ignored
        ])
        .unwrap();
        assert!(set.matches_component(".git"));
        assert!(set.matches_component("junk.tmp"));
        assert!(!set.matches_component("git"));
        // Full-path checks catch both kinds.
        assert!(set.matches_path(Path::new("/repo/.git/config")));
        assert!(set.matches_path(Path::new("/x/y/file.tmp")));
        assert!(set.matches_path(Path::new("/home/bob/secret")));
        // A dir-matching path pattern ignores everything beneath it, same
        // as the walker pruning that directory.
        assert!(set.matches_path(Path::new("/home/bob/secret/inner/deep.txt")));
        assert!(!set.matches_path(Path::new("/home/bob/public")));
        assert!(!set.matches_path(Path::new("/repo/src/main.rs")));
    }

    #[test]
    fn directory_patterns_with_trailing_slash() {
        let set = IgnoreSet::compile(&[
            "/tmp/".to_string(),        // absolute dir, natural spelling
            "cache/".to_string(),       // becomes a component pattern
            "*/target/".to_string(),    // dir anywhere by suffix
            "/".to_string(),            // degenerate: trims to nothing, skipped
        ])
        .unwrap();
        // The directory itself and everything beneath it.
        assert!(set.matches_path(Path::new("/tmp")));
        assert!(set.matches_path(Path::new("/tmp/a/b/c.txt")));
        assert!(!set.matches_path(Path::new("/tmpfoo/file.txt")));
        // "cache/" behaves like the component pattern "cache".
        assert!(set.matches_path(Path::new("/home/x/cache/obj.bin")));
        // Suffix form matches the dir at any depth.
        assert!(set.matches_path(Path::new("/repo/sub/target/debug/app")));
        // A bare "/" must not ignore the universe.
        assert!(!set.matches_path(Path::new("/etc/passwd")));
    }

    #[test]
    fn ignore_set_invalid_pattern_errors() {
        let err = IgnoreSet::compile(&["[".to_string()]).unwrap_err();
        assert!(err.contains("invalid ignore pattern"), "{}", err);
    }

    #[test]
    fn empty_ignore_set_matches_nothing() {
        let set = IgnoreSet::compile(&[]).unwrap();
        assert!(set.is_empty());
        assert!(!set.matches_path(Path::new("/any/thing")));
        assert!(!set.matches_component("anything"));
    }

    /// Pattern matching must follow the filesystem's own case rules, or
    /// `node_modules` silently fails to exclude `Node_Modules` on Windows.
    #[test]
    fn ignore_matching_follows_platform_case_rules() {
        let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
        assert!(set.matches_component("node_modules"), "exact always matches");

        let folded = cfg!(any(windows, target_os = "macos"));
        assert_eq!(
            set.matches_component("Node_Modules"),
            folded,
            "case folding must track the platform's filesystem semantics"
        );
    }

    #[test]
    fn default_ignore_patterns_cover_the_platform() {
        let d = IndexingConfig::default().ignore_patterns;
        for shared in [".git", "node_modules", "*.tmp", ".venv", "venv"] {
            assert!(d.iter().any(|p| p == shared), "missing {}", shared);
        }

        // `$RECYCLE.BIN` holds deleted files; indexing it would surface their
        // contents in search results.
        let recycle = d.iter().any(|p| p == "$RECYCLE.BIN");
        assert_eq!(recycle, cfg!(windows), "Windows-only exclusions");

        // Whatever the platform, the defaults must actually compile — a
        // pattern like `$RECYCLE.BIN` going through globset is the risk.
        let set = IgnoreSet::compile(&d).expect("default patterns compile");
        assert!(!set.is_empty());
        if cfg!(windows) {
            assert!(
                set.matches_component("$RECYCLE.BIN"),
                "`$` must be matched literally, not as a metacharacter"
            );
        }
    }

    #[test]
    fn nested_roots_matrix() {
        // Straight nesting (paths don't exist → compared as spelled).
        assert_eq!(
            nested_roots(&["/qs-x/b".into(), "/qs-x/b/c".into()]),
            vec![("/qs-x/b/c".to_string(), "/qs-x/b".to_string())]
        );
        // Component boundary: /a/bc is NOT under /a/b.
        assert!(nested_roots(&["/qs-x/b".into(), "/qs-x/bc".into()]).is_empty());
        // Disjoint roots.
        assert!(nested_roots(&["/qs-x/b".into(), "/qs-x/c".into()]).is_empty());
        // Exact duplicates flag once.
        assert_eq!(nested_roots(&["/qs-x".into(), "/qs-x".into()]).len(), 1);
        // Empty and singleton lists are fine.
        assert!(nested_roots(&[]).is_empty());
        assert!(nested_roots(&["/qs-x".into()]).is_empty());
        // Symlinked spellings of the same real directory are caught via
        // canonicalization.
        #[cfg(unix)]
        {
            let dir = tmp_dir();
            let real = dir.join("real");
            fs::create_dir_all(&real).unwrap();
            let link = dir.join("alias");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let pairs = nested_roots(&[
                real.to_string_lossy().into_owned(),
                link.to_string_lossy().into_owned(),
            ]);
            assert_eq!(pairs.len(), 1, "alias of the same dir counts as duplicate");
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn removed_precount_key_still_parses() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[processing]\nprecount_files_for_progress = true\nbatch_size = 42\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.processing.batch_size, 42, "known keys still load");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_workers_round_trip() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.source = Some(path.clone());
        cfg.paths.indexing_paths = vec!["/data".into(), "/share".into()];
        cfg.indexing.root_workers.insert("/share".into(), 24);
        cfg.save().unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.indexing.root_workers.get("/share"), Some(&24));
        assert_eq!(loaded.indexing.root_workers.get("/data"), None, "absent = auto");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diff_actions_matrix() {
        let base = Config::default();

        let same = diff_actions(&base, &base.clone());
        assert_eq!(
            same,
            ConfigActions {
                requires_rebuild: false,
                restart_watcher: false,
                search_db_changed: false
            }
        );

        let mut c = base.clone();
        c.processing.tokenize = "unicode61".into();
        assert!(diff_actions(&base, &c).requires_rebuild);

        let mut c = base.clone();
        c.indexing.ignore_patterns.push("*.log".into());
        let a = diff_actions(&base, &c);
        assert!(a.requires_rebuild && a.restart_watcher);

        let mut c = base.clone();
        c.indexing.follow_symlinks = true;
        let a = diff_actions(&base, &c);
        assert!(!a.requires_rebuild && a.restart_watcher);

        let mut c = base.clone();
        c.paths.database_path = "/elsewhere.sqlite".into();
        let a = diff_actions(&base, &c);
        assert!(a.search_db_changed && !a.requires_rebuild);

        let mut c = base.clone();
        c.search.display_limit = 5000;
        c.processing.batch_size = 999;
        c.indexing.auto_index = false;
        c.indexing.reindex_interval_minutes = 5;
        let a = diff_actions(&base, &c);
        assert_eq!(
            a,
            ConfigActions {
                requires_rebuild: false,
                restart_watcher: false,
                search_db_changed: false
            },
            "soft knobs never force restarts"
        );

        // Security: protection on↔off and salt changes rebuild; the
        // keychain preference is a soft knob.
        let mut c = base.clone();
        c.security.password_protected = true;
        assert!(diff_actions(&base, &c).requires_rebuild);
        let mut c = base.clone();
        c.security.salt = Some("00".repeat(16));
        assert!(diff_actions(&base, &c).requires_rebuild);
        let mut c = base.clone();
        c.security.use_keychain = true;
        assert!(!diff_actions(&base, &c).requires_rebuild);
    }

    #[test]
    fn security_config_round_trips_and_salt_is_omitted_when_none() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");

        // Defaults: protection off, no salt — and crucially the file must
        // not contain an invented salt value.
        let cfg = Config::load_from(&path).unwrap();
        assert!(!cfg.security.password_protected);
        assert_eq!(cfg.security.salt, None);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("salt"), "no default salt may be written");

        // With a salt set, it round-trips exactly.
        let mut cfg = cfg;
        cfg.security.password_protected = true;
        cfg.security.salt = Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string());
        cfg.security.use_keychain = true;
        cfg.save().unwrap();
        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.security, cfg.security);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_security_section_is_default() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.security, SecurityConfig::default());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn salt_bytes_validates_hostile_configs() {
        // Protected but no salt: hard error, nothing invented.
        let mut sec = SecurityConfig {
            password_protected: true,
            salt: None,
            use_keychain: false,
        };
        assert!(sec.salt_bytes().is_err());

        // Hand-crafted hostile values: truncated, oversized, non-hex,
        // embedded whitespace/quotes. All rejected.
        for bad in [
            "",
            "abcd",
            &"ab".repeat(17),
            &"ab".repeat(4096),
            "0g1e2d3c4b5a69788796a5b4c3d2e1f0",
            "0f1e2d3c4b5a6978 796a5b4c3d2e1f0",
            "0f1e2d3c4b5a69788796a5b4c3d2e1f'",
        ] {
            sec.salt = Some(bad.to_string());
            assert!(sec.salt_bytes().is_err(), "must reject salt {:?}", bad);
        }

        // A valid salt decodes, upper- or lowercase.
        sec.salt = Some("0F1E2D3C4B5A69788796A5B4C3D2E1F0".to_string());
        assert!(sec.salt_bytes().is_ok());
    }

    /// Dismissing the watch-cap warning must not trigger a rebuild or a
    /// watcher restart — it is pure UI bookkeeping, and restarting the
    /// watcher would re-trip the very warning being dismissed.
    #[test]
    fn watch_cap_warned_roots_is_a_soft_knob() {
        let base = Config::default();
        let mut c = base.clone();
        c.ui.watch_cap_warned_roots = vec!["/media/ApolloStore".to_string()];
        let a = diff_actions(&base, &c);
        assert_eq!(
            a,
            ConfigActions {
                requires_rebuild: false,
                restart_watcher: false,
                search_db_changed: false
            }
        );
    }

    #[test]
    fn watch_cap_warned_roots_round_trips() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.source = Some(path.clone());
        cfg.ui.watch_cap_warned_roots =
            vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()];
        cfg.save().unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(
            loaded.ui.watch_cap_warned_roots,
            vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()]
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Configs written before this field existed must still load.
    #[test]
    fn config_without_watch_cap_warned_roots_parses() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n[ui]\nscale=1.25\n",
        )
        .unwrap();

        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.ui.watch_cap_warned_roots.is_empty());
        assert_eq!(cfg.ui.scale, 1.25, "existing ui keys still parse");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_max_edits_round_trips() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.source = Some(path.clone());
        cfg.search.fuzzy_max_edits = 4;
        cfg.save().unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.search.fuzzy_max_edits, 4);
        fs::remove_dir_all(&dir).ok();
    }

    /// Configs written before this field existed keep the historic budget.
    #[test]
    fn config_without_fuzzy_max_edits_defaults_to_two() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
             [search]\nfuzzy_default=true\ndisplay_limit=250\n",
        )
        .unwrap();

        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.search.fuzzy_max_edits, 2);
        assert!(cfg.search.fuzzy_default, "existing search keys still parse");
        assert_eq!(cfg.search.display_limit, 250);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_edits_warning_only_above_the_threshold() {
        let mut cfg = SearchConfig::default();
        for quiet in 0..=FUZZY_EDITS_WARN_ABOVE {
            cfg.fuzzy_max_edits = quiet;
            assert!(cfg.fuzzy_edits_warning().is_none(), "{} should be quiet", quiet);
        }
        for loud in [FUZZY_EDITS_WARN_ABOVE + 1, 8, usize::MAX] {
            cfg.fuzzy_max_edits = loud;
            let msg = cfg.fuzzy_edits_warning().expect("warns above the threshold");
            assert!(msg.contains(&loud.to_string()));
            assert!(msg.contains(&FUZZY_EDITS_WARN_ABOVE.to_string()));
        }
    }
}
