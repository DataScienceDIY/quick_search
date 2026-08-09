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
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[derive(Default)]
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
    /// The indexing mode, written down: `true` is automatic — filesystem
    /// watchers apply changes as they happen and a full reindex runs every
    /// `reindex_interval_minutes` — and `false` is manual, where nothing
    /// runs until the user asks. The GUI's Stop / Return to Automatic
    /// controls save it as they switch, so the mode survives a restart.
    pub auto_index: bool,
    pub reindex_interval_minutes: u64,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
    /// Empty = extract content from everything the extractor registry
    /// supports. Non-empty = only files with these extensions get content
    /// extraction/FTS; everything else is still listed for filename search
    /// (`content_state = NA`). Entries are case-insensitive, with or
    /// without a leading dot. The reserved entry [`EXTENSIONLESS`] whitelists
    /// files that have no extension at all (`Makefile`, `README`, `.bashrc`),
    /// which are otherwise excluded by any non-empty filter. `#` starts a
    /// comment — whole-entry or trailing — see [`content_filter_entries`].
    /// Applied at walk time, when the row is written — which is why changing
    /// what it matches forces a rebuild (see [`diff_actions`]).
    pub content_extensions: Vec<String>,
    /// Excluded from the index entirely — never even listed. A pattern
    /// without `/` matches any single path component (so `.git` prunes
    /// whole subtrees); a pattern containing `/` or resembling a path is
    /// matched against the full path. Glob syntax (`*`, `?`, `[..]`).
    pub ignore_patterns: Vec<String>,
    /// Per-root walker thread override, keyed by the root string exactly
    /// as it appears in `indexing_paths` — the indexer resolves both sides
    /// to the same canonical path before matching, so a key that spells its
    /// root as `~/docs`, `/docs/` or a symlink still applies. Absent or
    /// 0 = auto-detect (4 for local storage, 16 for network mounts). Read
    /// at run start; a change applies to the next run.
    pub root_workers: std::collections::HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProcessingConfig {
    /// Bytes read from the head of each new or changed file. Those bytes do
    /// four jobs, so this one number sets more than the hash:
    ///
    /// 1. with the size, they identify the file (see `get_file_hash`);
    /// 2. they are the magic-byte window for MIME detection — `infer` reads
    ///    8 KiB from a path and its longest matcher needs 262 bytes, so the
    ///    default is exactly as good as opening the file, and a value under
    ///    262 makes some formats undetectable except by extension;
    /// 3. they are the text-sniff window (`textenc::looks_like_text`) that
    ///    decides whether an unknown-extension or extensionless file is
    ///    valid UTF-8 and so worth extracting — small values judge files on
    ///    less evidence, and a binary whose first bytes happen to be valid
    ///    UTF-8 is likelier to slip through a short window than a long one;
    /// 4. any plaintext file no larger than this is extracted during the
    ///    walk, sparing the content pass an open/read/close.
    ///
    /// Changing it invalidates stored hashes and forces a rebuild.
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_text_file_size: u64,
    pub batch_size: usize,
    pub fts_update_batch_size: usize,
    /// How large the write-ahead log may grow during a run before the indexer
    /// forces a checkpoint, in bytes. `0` disables forced checkpoints;
    /// anything else is raised to [`MINIMUM_WAL_SIZE`] at the use site.
    ///
    /// SQLite's own autocheckpoint copies the log into the index continuously
    /// but can only *reset* it when no reader is mid-query, and a run keeps a
    /// reader per root busy from start to finish — so left alone the log grows
    /// for the whole run and can end up larger than the index itself. This is
    /// a stall-frequency knob, not a throughput one: by the time a forced
    /// checkpoint fires there is almost nothing left to copy, so it costs lock
    /// acquisition and little else.
    pub maximum_wal_size: u64,
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

/// Floor on a non-zero [`ProcessingConfig::maximum_wal_size`]. A checkpoint
/// costs a lock acquisition that can wait on a running search, so a cap set
/// low enough to fire every round would trade a large log for a stalled
/// writer. Zero still means "never force one".
pub const MINIMUM_WAL_SIZE: u64 = 1024 * 1024 * 16;

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
            reindex_interval_minutes: 60,
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
            batch_size: 500,
            fts_update_batch_size: 1000,
            maximum_wal_size: 1024 * 1024 * 512,
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
            None => Err(
                "password protection is enabled but the config has no salt; \
                         disable protection or set the password again"
                    .to_string(),
            ),
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
    /// System-wide shortcut that raises the window and puts the caret in the
    /// search box, as `Ctrl+Shift+F`: modifiers from `Ctrl`, `Alt` and
    /// `Shift`, then one key, joined by `+`. Empty disables it.
    ///
    /// A plain string rather than a structured key so that a hand-edited
    /// config reads the way the Options window prints it, and so an
    /// unparseable value degrades to "no shortcut" with a message instead of
    /// refusing to load. On Wayland the desktop, not this value, has the
    /// final say — see the GUI's `hotkey` module.
    pub search_hotkey: String,
    /// `dark` or `light`. Applied live; the desktop's own light/dark setting
    /// is deliberately not consulted, since reading it means opening a D-Bus
    /// session and subscribing to the user's settings.
    ///
    /// A plain string for the same reason as `search_hotkey`: a value nobody
    /// recognises falls back to dark, where a typed-out enum would fail to
    /// deserialize and take the whole config file down with it.
    pub color_scheme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            scale: 1.1,
            watch_cap_warned_roots: Vec::new(),
            search_hotkey: "Ctrl+Shift+F".to_string(),
            color_scheme: "dark".to_string(),
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
    let mut patterns = vec![".git", "node_modules", "*.tmp", ".venv", "venv"];
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
            let cfg = Config {
                source: Some(path.to_path_buf()),
                ..Config::default()
            };
            cfg.save()?;
            Ok(cfg)
        }
    }

    /// Write back to the file this config was loaded from (or the default
    /// location), creating parent directories as needed. Raw values are
    /// written verbatim — relative paths in a portable config stay relative.
    pub fn save(&self) -> Result<(), String> {
        let path = self.source.clone().unwrap_or_else(Self::config_path);
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

    /// `resolved_indexing_paths` canonicalized and spelled the way
    /// `files.path` prefixes them.
    ///
    /// The form roots must be compared in: `~/docs`, `docs` in a portable
    /// config and `/home/me/docs` are one root under three spellings, and a
    /// re-spelling is not a configuration change. Duplicates collapse, order
    /// is not preserved — a caller that needs one uses
    /// [`Config::resolved_indexing_paths`].
    pub fn normalized_indexing_paths(&self) -> BTreeSet<String> {
        self.resolved_indexing_paths()
            .iter()
            .map(|p| crate::file_handling::normalize_root_string(&p.to_string_lossy()))
            .collect()
    }

    /// `indexing_paths` with the same resolution rules as
    /// [`Config::resolved_database_path`].
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

/// Reserved `content_extensions` entry standing for "files with no
/// extension". Matched case-insensitively, and it cannot collide with a real
/// extension because the parentheses are not part of one.
pub const EXTENSIONLESS: &str = "(none)";

/// The `content_extensions` entries that actually filter: `#` starts a
/// comment and runs to the end of the entry, so a whole-line comment drops
/// out entirely and `md  # notes` filters on `md`. Surrounding space is
/// trimmed; what is left of a `#` never is.
pub fn content_filter_entries(list: &[String]) -> impl Iterator<Item = &str> {
    list.iter().filter_map(|raw| {
        let entry = raw.split('#').next().unwrap_or_default().trim();
        (!entry.is_empty()).then_some(entry)
    })
}

/// Whether a file's content (text extraction + FTS) should be indexed under
/// the `content_extensions` filter. Files that fail this are still listed
/// for filename search. No entries (empty, or nothing but comments) =
/// everything allowed.
pub fn content_allowed(path: &Path, cfg: &Config) -> bool {
    let list = &cfg.indexing.content_extensions;
    if content_filter_entries(list).next().is_none() {
        return true;
    }
    // `Path::extension` is None for `Makefile` and for dot-only names like
    // `.bashrc`, so without the sentinel a non-empty filter always skips them.
    match path.extension().and_then(|e| e.to_str()) {
        // The sentinel is reserved: it never doubles as an extension, so a
        // file named `x.(none)` is not whitelisted by it.
        Some(ext) => content_filter_entries(list)
            .filter(|allowed| !allowed.eq_ignore_ascii_case(EXTENSIONLESS))
            .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(ext)),
        None => {
            content_filter_entries(list).any(|allowed| allowed.eq_ignore_ascii_case(EXTENSIONLESS))
        }
    }
}

/// Longest name folded without allocating. Comfortably above `NAME_MAX` on the
/// filesystems that matter (255 bytes), so the heap path is effectively dead
/// code kept for correctness rather than for use.
const FOLD_BUF: usize = 256;

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
    /// Component patterns that are plain ASCII names — `.git`, `node_modules`,
    /// `System Volume Information`. Held apart from `component` purely for
    /// speed, and it is a large difference on Windows.
    ///
    /// globset compiles a case-insensitive glob by giving up every fast path it
    /// has: `Glob::literal`, `ext`, `prefix`, `suffix` and `basename_tokens` all
    /// return `None` the moment `case_insensitive` is set, so every pattern
    /// falls through to a `RegexSet` scan. Case-sensitively, `.git` and
    /// `node_modules` compile to a hash lookup and `*.tmp` to an extension
    /// lookup. That left Windows — which is case-insensitive *and* carries seven
    /// extra default patterns — running a 12-pattern regex DFA over every single
    /// directory entry, where Linux did five hash lookups.
    ///
    /// Folded and compared as **ASCII**, matching [`PATH_COLLATION`]'s reasoning
    /// exactly: SQLite's `NOCASE` and `LIKE` fold ASCII only, so the glob layer
    /// agreeing with them is what keeps a path filter from disagreeing with the
    /// ignore rules. Patterns that are not ASCII stay in `component` and keep
    /// globset's Unicode folding, so nothing that matched before stops matching.
    literal_components: std::collections::HashSet<String>,
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
            // A plain ASCII name needs no glob machinery at all — see
            // `literal_components`. Everything else, including every non-ASCII
            // pattern, goes on to globset unchanged.
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
    ///
    /// Allocation-free for any name that fits [`FOLD_BUF`], which is every name
    /// a real filesystem can produce. This runs on every directory entry the
    /// walker sees, so it is the one place in the ignore path worth keeping off
    /// the heap.
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
        // The literal set answers almost every call — the defaults are all
        // plain names — and answers it without touching the regex engine.
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

    /// Fold `other` in, so one pass satisfies both.
    ///
    /// For a second config edit arriving while the first is still being
    /// applied: the plans are computed against different configurations and
    /// neither knows what the other left undone, so the union of the two is
    /// the only thing that is certainly enough. Every part is idempotent, so
    /// re-doing the finished half of the first costs time and nothing else.
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
    ///
    /// Names what changed rather than what will happen to the rows: the
    /// reader is someone asking why a run has not started walking yet, and
    /// the answer they need is which edit of theirs caused it.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        crate::testutil::scratch_dir("config")
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
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.paths.indexing_paths, vec!["/x".to_string()]);
        assert_eq!(cfg.processing.batch_size, 500, "missing sections default");
        assert_eq!(cfg.search.debounce_ms, 150);
        assert!((cfg.ui.scale - 1.1).abs() < f32::EPSILON);
        // A config written before the shortcut existed must come back with
        // one, not with no shortcut at all.
        assert_eq!(cfg.ui.search_hotkey, "Ctrl+Shift+F");
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
        assert!(
            content_allowed(Path::new("/a/readme.md"), &cfg),
            "leading dot + case in filter"
        );
        assert!(!content_allowed(Path::new("/a/b.pdf"), &cfg));
        assert!(!content_allowed(Path::new("/a/noext"), &cfg));
        assert!(
            !content_allowed(Path::new("/a/.bashrc"), &cfg),
            "dot-only name has no ext"
        );
    }

    #[test]
    fn content_allowed_extensionless_sentinel() {
        let mut cfg = Config::default();
        cfg.indexing.content_extensions = vec!["txt".into(), "  (NonE)  ".into()];
        assert!(content_allowed(Path::new("/a/Makefile"), &cfg));
        assert!(
            content_allowed(Path::new("/a/.bashrc"), &cfg),
            "dot-only name"
        );
        assert!(
            content_allowed(Path::new("/a/b.txt"), &cfg),
            "real extensions still work"
        );
        assert!(
            !content_allowed(Path::new("/a/b.pdf"), &cfg),
            "sentinel is not a wildcard"
        );
        // The sentinel is not itself an extension: a file literally named
        // `x.none` is not whitelisted by it.
        assert!(!content_allowed(Path::new("/a/x.none"), &cfg));
        assert!(!content_allowed(Path::new("/a/x.(none)"), &cfg));

        // Every capitalisation of the word means the same thing.
        for spelling in ["(none)", "(NONE)", "(NonE)", "(nOnE)"] {
            let mut c = Config::default();
            c.indexing.content_extensions = vec![spelling.to_string()];
            assert!(content_allowed(Path::new("/a/README"), &c), "{spelling}");
        }

        // A leading dot is stripped for extensions but must not turn some
        // other entry into the sentinel.
        let mut only_txt = Config::default();
        only_txt.indexing.content_extensions = vec!["txt".into()];
        assert!(!content_allowed(Path::new("/a/Makefile"), &only_txt));
    }

    #[test]
    fn content_allowed_comments() {
        let mut cfg = Config::default();
        cfg.indexing.content_extensions = vec![
            "# source files only".into(),
            "rs # rust".into(),
            "  .MD\t# docs  ".into(),
            "   # indented whole-line comment".into(),
            "(none) # Makefile, LICENSE, ...".into(),
        ];
        assert!(content_allowed(Path::new("/a/b.rs"), &cfg));
        assert!(
            content_allowed(Path::new("/a/b.md"), &cfg),
            "dot + trailing comment"
        );
        assert!(
            content_allowed(Path::new("/a/Makefile"), &cfg),
            "sentinel + comment"
        );
        assert!(!content_allowed(Path::new("/a/b.pdf"), &cfg));
        // Comment text is not itself a filter entry.
        assert!(!content_allowed(Path::new("/a/b.rust"), &cfg));
        assert!(!content_allowed(Path::new("/a/b.only"), &cfg));
        assert!(!content_allowed(Path::new("/a/b.docs"), &cfg));

        // Nothing but comments filters nothing — same as an empty list.
        let mut all_comments = Config::default();
        all_comments.indexing.content_extensions =
            vec!["# nothing enabled yet".into(), "  ".into(), "#".into()];
        assert!(content_allowed(Path::new("/a/b.pdf"), &all_comments));
        assert!(content_allowed(Path::new("/a/Makefile"), &all_comments));
    }

    /// Comments, spelling and order are not part of the content filter, so
    /// editing them is no work at all — and a real change to it is never a
    /// rebuild, only a re-decision of the text already stored.
    #[test]
    fn comment_only_edit_is_no_work_at_all() {
        let mut old = Config::default();
        old.indexing.content_extensions = vec!["txt".into(), "md".into()];

        for cosmetic in [
            vec!["# my notes".into(), "txt".into(), "md  # markdown".into()],
            vec!["md".into(), "txt".into()],
            vec![".TXT".into(), ".Md".into()],
        ] {
            let mut new = old.clone();
            new.indexing.content_extensions = cosmetic;
            let a = diff_actions(&old, &new);
            assert_eq!(a, ConfigActions::default(), "cosmetic edit is not a change");
        }

        // Adding an extension widens the filter: files already indexed by
        // name need their text extracted, which takes a run.
        let mut widened = old.clone();
        widened.indexing.content_extensions = vec!["txt".into(), "md".into(), "(none)".into()];
        let a = diff_actions(&old, &widened);
        assert!(!a.requires_rebuild);
        assert!(a.work.reconcile_content && a.work.reindex);

        // Commenting one out narrows it: the stored text goes, and nothing
        // needs walking to make that true.
        let mut narrowed = old.clone();
        narrowed.indexing.content_extensions = vec!["txt".into(), "# md".into()];
        let a = diff_actions(&old, &narrowed);
        assert!(!a.requires_rebuild);
        assert!(a.work.reconcile_content && !a.work.reindex);
    }

    /// An empty list means "everything allowed", so it is a superset of every
    /// other list — the case plain set arithmetic reads backwards.
    #[test]
    fn an_empty_content_filter_is_the_widest_one() {
        let mut listed = Config::default();
        listed.indexing.content_extensions = vec!["txt".into()];
        let mut unfiltered = listed.clone();
        unfiltered.indexing.content_extensions = vec![];

        let widening = diff_actions(&listed, &unfiltered).work;
        assert!(widening.reconcile_content && widening.reindex);

        let narrowing = diff_actions(&unfiltered, &listed).work;
        assert!(narrowing.reconcile_content && !narrowing.reindex);
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
            "/tmp/".to_string(),     // absolute dir, natural spelling
            "cache/".to_string(),    // becomes a component pattern
            "*/target/".to_string(), // dir anywhere by suffix
            "/".to_string(),         // degenerate: trims to nothing, skipped
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

    /// A drive-root pattern must survive the trailing-separator trim as a
    /// path pattern — trimmed to "D:" it would land in the component set,
    /// where nothing is ever named "D:".
    #[test]
    fn drive_root_patterns_are_not_component_patterns() {
        let set = IgnoreSet::compile(&[r"D:\".to_string(), "E:/".to_string()]).unwrap();
        assert!(!set.matches_component("D:"));
        assert!(!set.matches_component(r"D:\"));
        assert!(!set.matches_component("E:"));
    }

    /// The full drive-root behavior needs Windows path semantics:
    /// `Path::parent` only walks up to `D:\` there, and globset only folds
    /// `\` to `/` where `\` is a separator.
    #[cfg(windows)]
    #[test]
    fn drive_root_pattern_ignores_the_whole_drive() {
        let set = IgnoreSet::compile(&[r"D:\".to_string()]).unwrap();
        assert!(set.matches_path(Path::new(r"D:\")));
        assert!(set.matches_path(Path::new(r"D:\Users\x\file.txt")));
        assert!(set.matches_path(Path::new(r"d:\case\folded.txt")));
        assert!(!set.matches_path(Path::new(r"E:\file.txt")));
    }

    /// A bare "D:" (no separator) compiles but can only match a component
    /// literally named "D:", which no file ever is. The GUI warns about
    /// this shape; the compiler intentionally leaves it alone.
    #[test]
    fn bare_drive_letter_stays_a_component_pattern() {
        let set = IgnoreSet::compile(&["D:".to_string()]).unwrap();
        assert!(set.matches_component("D:"));
        #[cfg(windows)]
        assert!(!set.matches_path(Path::new(r"D:\file.txt")));
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
        assert!(
            set.matches_component("node_modules"),
            "exact always matches"
        );

        let folded = cfg!(any(windows, target_os = "macos"));
        assert_eq!(
            set.matches_component("Node_Modules"),
            folded,
            "case folding must track the platform's filesystem semantics"
        );
    }

    /// Which patterns take the fast path, and that taking it changes nothing
    /// observable. A plain name is matched whole — never as a prefix, a
    /// substring or a wildcard — and only its case is allowed to vary.
    #[test]
    fn the_literal_fast_path_matches_whole_names_only() {
        let literal = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
        assert!(
            !literal.literal_components.is_empty(),
            "a plain name belongs on the fast path"
        );
        for globby in ["node_module?", "node_*", "*.tmp", "a[bc]d", "x{1,2}"] {
            assert!(
                IgnoreSet::compile(&[globby.to_string()])
                    .unwrap()
                    .literal_components
                    .is_empty(),
                "{} has glob syntax and must stay with globset",
                globby
            );
        }

        assert!(literal.matches_component("node_modules"));
        for cased in ["Node_Modules", "NODE_MODULES", "node_moduleS"] {
            assert_eq!(
                literal.matches_component(cased),
                cfg!(any(windows, target_os = "macos")),
                "only case may vary, and only where the filesystem says so: {}",
                cased
            );
        }
        for name in [
            "node_modules_",
            "_node_modules",
            "nodemodules",
            "node_module",
            "src",
            "",
        ] {
            assert!(
                !literal.matches_component(name),
                "{} is not the ignored name",
                name
            );
        }
    }

    /// A non-ASCII pattern keeps globset's Unicode folding rather than being
    /// silently downgraded to the ASCII fast path.
    #[test]
    fn non_ascii_patterns_stay_on_the_glob_path() {
        let set = IgnoreSet::compile(&["café".to_string()]).unwrap();
        assert!(
            set.literal_components.is_empty(),
            "a non-ASCII name must not join the ASCII-folded set"
        );
        assert!(set.matches_component("café"));
        assert!(!set.matches_component("cafe"));
    }

    /// Names longer than the stack fold buffer take the heap path, and must
    /// come back with the same answer.
    #[test]
    fn overlong_names_still_fold_correctly() {
        let long = "a".repeat(FOLD_BUF + 10);
        let set = IgnoreSet::compile(std::slice::from_ref(&long)).unwrap();
        assert!(set.matches_component(&long));
        assert_eq!(
            set.matches_component(&long.to_uppercase()),
            cfg!(any(windows, target_os = "macos"))
        );
        assert!(!set.matches_component(&"a".repeat(FOLD_BUF + 9)));
    }

    /// Watcher events are matched by whole path, and the literal patterns have
    /// to be visible on that route too — otherwise the watcher indexes exactly
    /// what the walker prunes and the index churns every cycle.
    #[test]
    fn full_path_matching_sees_literal_component_patterns() {
        let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
        assert!(set.matches_path(Path::new("/home/me/proj/node_modules/pkg/index.js")));
        assert!(!set.matches_path(Path::new("/home/me/proj/src/index.js")));
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
        let mut cfg = Config {
            source: Some(path.clone()),
            ..Config::default()
        };
        cfg.paths.indexing_paths = vec!["/data".into(), "/share".into()];
        cfg.indexing.root_workers.insert("/share".into(), 24);
        cfg.save().unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.indexing.root_workers.get("/share"), Some(&24));
        assert_eq!(
            loaded.indexing.root_workers.get("/data"),
            None,
            "absent = auto"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Only three settings may wipe the index: the FTS tokenizer, the hash
    /// length and the encryption key. Anything else that reaches
    /// `requires_rebuild` is a bug — it costs the user everything the index
    /// took hours to learn.
    #[test]
    fn only_unreadable_data_forces_a_rebuild() {
        let base = Config::default();

        let mut tokenizer = base.clone();
        tokenizer.processing.tokenize = "unicode61".into();
        let mut hash = base.clone();
        hash.processing.hash_length = base.processing.hash_length + 1;
        let mut protect = base.clone();
        protect.security.password_protected = true;
        let mut salt = base.clone();
        salt.security.salt = Some("00".repeat(16));

        for c in [&tokenizer, &hash, &protect, &salt] {
            let a = diff_actions(&base, c);
            assert!(a.requires_rebuild, "must wipe");
            assert!(
                a.work.is_empty(),
                "a wipe subsumes reconciliation; leaving work behind would run it \
                 against a file that is about to be deleted"
            );
        }

        // The keychain only decides where the key is remembered, not what the
        // file was written with.
        let mut keychain = base.clone();
        keychain.security.use_keychain = true;
        assert_eq!(diff_actions(&base, &keychain), ConfigActions::default());
    }

    /// Narrowing deletes; widening walks. Nothing here may wipe.
    #[test]
    fn diff_actions_matrix() {
        let dir = tmp_dir();
        let kept = dir.join("kept");
        let dropped = dir.join("dropped");
        fs::create_dir_all(&kept).unwrap();
        fs::create_dir_all(&dropped).unwrap();
        let (kept, dropped) = (
            kept.to_string_lossy().into_owned(),
            dropped.to_string_lossy().into_owned(),
        );

        let mut base = Config::default();
        base.paths.indexing_paths = vec![kept.clone(), dropped.clone()];
        base.indexing.ignore_patterns = vec!["node_modules".into()];
        base.indexing.include_hidden = true;
        base.indexing.follow_symlinks = true;

        assert_eq!(diff_actions(&base, &base.clone()), ConfigActions::default());

        // Removing a root: its rows are deleted by range, and no walk is
        // needed to establish that they should go.
        let mut c = base.clone();
        c.paths.indexing_paths = vec![kept.clone()];
        let a = diff_actions(&base, &c);
        assert!(!a.requires_rebuild);
        assert_eq!(a.work.drop_roots, vec![dropped.clone()]);
        assert!(!a.work.reindex && !a.work.prune_scope);

        // Adding one: nothing stored is wrong, there is just more to find.
        let a = diff_actions(&c, &base);
        assert!(!a.requires_rebuild);
        assert!(a.work.drop_roots.is_empty() && a.work.reindex && !a.work.prune_scope);

        for (narrow, widen, what) in [
            (
                {
                    let mut c = base.clone();
                    c.indexing.ignore_patterns.push("*.log".into());
                    c
                },
                {
                    let mut c = base.clone();
                    c.indexing.ignore_patterns.clear();
                    c
                },
                "ignore patterns",
            ),
            (
                {
                    let mut c = base.clone();
                    c.indexing.include_hidden = false;
                    c
                },
                base.clone(),
                "hidden files",
            ),
        ] {
            let a = diff_actions(&base, &narrow);
            assert!(!a.requires_rebuild, "{} must not wipe", what);
            assert!(
                a.work.prune_scope && !a.work.reindex,
                "narrowing {} prunes and needs no walk",
                what
            );
            let a = diff_actions(&narrow, &widen);
            assert!(!a.requires_rebuild, "{} must not wipe", what);
            assert!(
                a.work.reindex && !a.work.prune_scope,
                "widening {} walks and deletes nothing",
                what
            );
        }

        // Symlinks take their own route: with links on, a target inside a root
        // is stored under exactly the path a direct walk would produce, so
        // nothing in scope changes. What turning them off strands is the rows
        // *outside* every root, which no per-root scan would ever revisit.
        let mut no_links = base.clone();
        no_links.indexing.follow_symlinks = false;
        let a = diff_actions(&base, &no_links);
        assert!(!a.requires_rebuild);
        assert!(
            a.work.drop_aliases && !a.work.prune_scope && !a.work.reindex,
            "turning links off sweeps outside the roots and nothing else"
        );
        let a = diff_actions(&no_links, &base);
        assert!(
            a.work.reindex && !a.work.drop_aliases && !a.work.prune_scope,
            "turning links on only adds"
        );

        // Stored text: turning it on means re-extracting, turning it off means
        // throwing the blobs away — never a rebuild either way.
        let mut off = base.clone();
        off.processing.store_text_for_snippets = false;
        let mut on = base.clone();
        on.processing.store_text_for_snippets = true;
        let a = diff_actions(&on, &off);
        assert!(a.work.drop_text && !a.work.restore_text && !a.work.reindex);
        let a = diff_actions(&off, &on);
        assert!(a.work.restore_text && !a.work.drop_text && a.work.reindex);

        let mut c = base.clone();
        c.paths.database_path = "/elsewhere.sqlite".into();
        let a = diff_actions(&base, &c);
        assert!(a.search_db_changed && !a.requires_rebuild && a.work.is_empty());

        let mut c = base.clone();
        c.search.display_limit = 5000;
        c.processing.batch_size = 999;
        c.processing.maximum_wal_size = 0;
        c.indexing.auto_index = false;
        c.indexing.reindex_interval_minutes = 5;
        assert_eq!(
            diff_actions(&base, &c),
            ConfigActions::default(),
            "soft knobs are not index work"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// A second edit landing while the first is still being applied must not
    /// lose the first's work: the two plans are computed against different
    /// configurations, so neither knows what the other left undone.
    #[test]
    fn merging_two_plans_loses_nothing() {
        let first = IndexWork {
            drop_roots: vec!["/gone".into(), "/shared".into()],
            prune_scope: true,
            drop_text: true,
            ..IndexWork::default()
        };
        let second = IndexWork {
            drop_roots: vec!["/shared".into(), "/also-gone".into()],
            reconcile_content: true,
            reindex: true,
            ..IndexWork::default()
        };

        let mut merged = second.clone();
        merged.merge_from(&first);
        assert_eq!(
            merged.drop_roots,
            vec!["/shared", "/also-gone", "/gone"],
            "every root from both, each once"
        );
        assert!(merged.prune_scope && merged.drop_text);
        assert!(merged.reconcile_content && merged.reindex);

        // Merging an empty plan changes nothing, and merging a plan into
        // itself is the identity — both are what make a restart safe.
        let mut untouched = first.clone();
        untouched.merge_from(&IndexWork::default());
        assert_eq!(untouched, first);
        untouched.merge_from(&first);
        assert_eq!(untouched, first);
    }

    /// Order and spelling are not configuration. Reordering the folder list,
    /// reordering the ignore patterns, or writing a root a different way used
    /// to wipe a multi-million-file index for nothing.
    #[test]
    fn respelling_a_list_is_not_a_change() {
        let dir = tmp_dir();
        let a_dir = dir.join("alpha");
        let b_dir = dir.join("beta");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();

        let mut base = Config::default();
        base.paths.indexing_paths = vec![
            a_dir.to_string_lossy().into_owned(),
            b_dir.to_string_lossy().into_owned(),
        ];
        base.indexing.ignore_patterns = vec!["node_modules".into(), "*.tmp".into()];

        let mut reordered = base.clone();
        reordered.paths.indexing_paths.reverse();
        reordered.indexing.ignore_patterns.reverse();
        assert_eq!(diff_actions(&base, &reordered), ConfigActions::default());

        // A trailing separator, a `.` hop and a duplicate entry all name the
        // same two roots.
        let mut respelled = base.clone();
        respelled.paths.indexing_paths = vec![
            format!("{}{}", a_dir.to_string_lossy(), std::path::MAIN_SEPARATOR),
            b_dir.join(".").to_string_lossy().into_owned(),
            a_dir.to_string_lossy().into_owned(),
        ];
        assert_eq!(diff_actions(&base, &respelled), ConfigActions::default());

        // Whitespace around an ignore pattern is trimmed before it compiles,
        // so it cannot be a change either.
        let mut padded = base.clone();
        padded.indexing.ignore_patterns = vec!["  node_modules ".into(), "*.tmp".into()];
        assert_eq!(diff_actions(&base, &padded), ConfigActions::default());

        fs::remove_dir_all(&dir).ok();
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
        assert_eq!(diff_actions(&base, &c), ConfigActions::default());
    }

    #[test]
    fn watch_cap_warned_roots_round_trips() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let mut cfg = Config {
            source: Some(path.clone()),
            ..Config::default()
        };
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

    /// Which theme the window uses is nobody's business but the window's: it
    /// must never cost a reindex or a watcher restart.
    #[test]
    fn color_scheme_is_a_soft_knob() {
        let base = Config::default();
        let mut c = base.clone();
        c.ui.color_scheme = "light".to_string();
        assert_eq!(diff_actions(&base, &c), ConfigActions::default());
    }

    #[test]
    fn color_scheme_round_trips_and_defaults_to_dark() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        assert_eq!(Config::default().ui.color_scheme, "dark");

        let mut cfg = Config {
            source: Some(path.clone()),
            ..Config::default()
        };
        cfg.ui.color_scheme = "light".to_string();
        cfg.save().unwrap();
        assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "light");

        // A config written before the setting existed keeps the appearance it
        // had, which was dark.
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n[ui]\nscale=1.25\n",
        )
        .unwrap();
        assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "dark");

        // A value nobody recognises is not a broken config file: the whole
        // point of storing it as a string is that the app still starts.
        fs::write(
            &path,
            "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
             [ui]\ncolor_scheme=\"drak\"\n",
        )
        .unwrap();
        assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "drak");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_max_edits_round_trips() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        let mut cfg = Config {
            source: Some(path.clone()),
            ..Config::default()
        };
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
            assert!(
                cfg.fuzzy_edits_warning().is_none(),
                "{} should be quiet",
                quiet
            );
        }
        for loud in [FUZZY_EDITS_WARN_ABOVE + 1, 8, usize::MAX] {
            cfg.fuzzy_max_edits = loud;
            let msg = cfg
                .fuzzy_edits_warning()
                .expect("warns above the threshold");
            assert!(msg.contains(&loud.to_string()));
            assert!(msg.contains(&FUZZY_EDITS_WARN_ABOVE.to_string()));
        }
    }
}
