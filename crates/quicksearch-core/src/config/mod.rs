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

mod diff;
mod ignore;
#[cfg(test)]
mod tests;

pub use diff::{diff_actions, nested_roots, ConfigActions, IndexWork};
pub use ignore::IgnoreSet;

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
    /// Writer time one root's turn may take before the round moves on, in
    /// milliseconds. The time half of the round-robin whose row half is
    /// `batch_size`, and so the bound on how long any one root can hold up
    /// the others.
    ///
    /// Before there was one, an extraction turn ran to the end of whatever
    /// was ready — half a second to two seconds of FTS5 trigram tokenization
    /// for a batch of large documents — while a walking root's rows sat in
    /// its channel and its walkers parked behind them. Reads as "4/4 workers
    /// busy, no progress".
    ///
    /// `0` gives each turn one `batch_size` quantum and no more, which is
    /// the finest the round-robin goes; the tests that count work per round
    /// use small values here so a phase cannot begin and end between two
    /// status snapshots.
    pub writer_turn_slice_ms: u64,
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
    /// Watch the search results currently on screen and show renames,
    /// deletions and content changes as they happen. Only the rows actually
    /// visible are watched, and any edit to the query drops the watches.
    /// What a row shows is read from the file itself, so this holds whether
    /// or not indexing is running; the files it reads are then brought up to
    /// date in the index, so what is stored cannot drift from what is on
    /// screen. See [`crate::live`].
    pub live_results: bool,
    /// Which columns the Search tab shows.
    pub columns: ColumnsConfig,
}

/// Which columns the Search tab shows, as picked from the right-click menu on
/// any column header or from the Settings tab.
///
/// The path column is deliberately not represented: it is always shown, so
/// "no columns at all" is not a state this can hold. Size and modified are off
/// by default — the width they cost is better spent on the path and the match,
/// and both are one click away.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColumnsConfig {
    pub name: bool,
    /// The matched excerpt from a file's contents. Rows that matched on their
    /// name or path instead show a dash there.
    pub content_match: bool,
    pub size: bool,
    pub modified: bool,
    pub rank: bool,
}

impl Default for ColumnsConfig {
    fn default() -> Self {
        ColumnsConfig {
            name: true,
            content_match: true,
            size: false,
            modified: false,
            rank: true,
        }
    }
}

impl SearchConfig {
    /// The caution to show next to `fuzzy_max_edits`, or `None` when the
    /// value is sane.
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
            writer_turn_slice_ms: 100,
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
            live_results: true,
            columns: ColumnsConfig::default(),
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
    /// `Shift`, then one key, joined by `+`. Empty disables it. An
    /// unparseable value degrades to "no shortcut" with a message instead of
    /// refusing to load. On Wayland the desktop, not this value, has the
    /// final say — see the GUI's `hotkey` module.
    pub search_hotkey: String,
    /// `dark` or `light`. Applied live; the desktop's own light/dark setting
    /// is not consulted (reading it means a D-Bus session). A value nobody
    /// recognises falls back to dark, where a typed-out enum would fail to
    /// deserialize and take the whole config file down with it.
    pub color_scheme: String,
    /// Whether the first-start tour has been dismissed.
    ///
    /// Three-valued on purpose. `None` means the key predates the tour — an
    /// installation that upgraded into this version, which has already found
    /// its way around — so only a config file this version *created* (which
    /// gets `Some(false)` from [`UiConfig::default`]) is ever offered the tour.
    /// A plain `bool` could not tell those apart.
    ///
    /// The field-level `default` is load-bearing and not redundant with the
    /// `#[serde(default)]` on the struct: that one fills a missing field from
    /// `UiConfig::default()`, which says `Some(false)` — and would hand every
    /// upgrading installation the tour. This one fills it from
    /// `Option::default()`, which is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tutorial_seen: Option<bool>,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            scale: 1.1,
            watch_cap_warned_roots: Vec::new(),
            search_hotkey: "Ctrl+Shift+F".to_string(),
            color_scheme: "dark".to_string(),
            // Not `None`: a config built from these defaults is a config being
            // written for the first time, and that is exactly who the tour is
            // for. `None` is reserved for a file that predates the key.
            tutorial_seen: Some(false),
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
/// No exclusion for `C:\Windows`: a pattern general enough to catch it would
/// also catch a user folder named `Windows`; `config_example.toml` documents
/// it for people who add a drive root.
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
            // Before anything reads a value — and in particular before
            // `config_check` compares `hash_length` against what the index was
            // built with, or a clamp applied later would read as a changed
            // setting and force a rebuild.
            for warning in cfg.clamp_out_of_range() {
                crate::log_warn!("config: {}", warning);
            }
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

    /// Bring values that would break the program back into range, returning a
    /// line about each one changed.
    ///
    /// Clamps, never rejects: this file is hand-editable and a typo in it must
    /// not stop the app starting, the same position `main.rs` takes on a file
    /// that will not parse at all. Only the fields that are *not* already
    /// defended where they are used appear here — `results_per_page`,
    /// `root_workers`, `ui.scale`, `maximum_wal_size`, `batch_size` and
    /// `reindex_interval_minutes` all clamp at their call sites, and doing it
    /// twice would just be two places to disagree.
    fn clamp_out_of_range(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut clamp = |name: &str, value: &mut u64, lo: u64, hi: u64| {
            let bounded = (*value).clamp(lo, hi);
            if bounded != *value {
                warnings.push(format!(
                    "{} is {}, which is out of range; using {}",
                    name, *value, bounded
                ));
                *value = bounded;
            }
        };

        // Zero means every search returns nothing at all: `cascade::run`
        // computes `remaining()` as zero and stops before its first pass, and
        // reports the empty result as truncated.
        let mut display_limit = self.search.display_limit as u64;
        clamp("[search] display_limit", &mut display_limit, 1, 1_000_000);
        self.search.display_limit = display_limit as usize;

        // The last ceiling on how much an extractor reads: the extractors cap
        // a single read, but this is what decides which files they open at all.
        clamp(
            "[processing] maximum_text_file_size",
            &mut self.processing.maximum_text_file_size,
            1,
            4 * 1024 * 1024 * 1024,
        );

        // Below 262 bytes `infer`'s longest magic-number matcher cannot run,
        // so file types stop being detectable by content; above a megabyte the
        // walk's per-file head buffer stops being a head.
        let mut hash_length = self.processing.hash_length as u64;
        clamp(
            "[processing] hash_length",
            &mut hash_length,
            262,
            1024 * 1024,
        );
        self.processing.hash_length = hash_length as usize;

        // The writer's round-robin turn. Unbounded, one root holds the writer
        // for as long as it likes and every other root's walk waits behind it;
        // zero is meaningful (one quantum per turn) and stays legal.
        clamp(
            "[processing] writer_turn_slice_ms",
            &mut self.processing.writer_turn_slice_ms,
            0,
            10_000,
        );

        warnings
    }

    /// Write back to the file this config was loaded from (or the default
    /// location), creating parent directories as needed. Raw values are
    /// written verbatim — relative paths in a portable config stay relative.
    ///
    /// Atomic: see the comment on the rename below.
    pub fn save(&self) -> Result<(), String> {
        let path = self.source.clone().unwrap_or_else(Self::config_path);
        if let Some(dir) = path.parent() {
            crate::platform::create_dir_private(dir)
                .map_err(|e| format!("Failed to create config dir {}: {}", dir.display(), e))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        // Written beside the target and renamed over it, rather than
        // truncate-then-write. `[security].salt` exists *only* in this file:
        // it is not derivable from the index and not stored anywhere else, so
        // a config truncated by a crash, a full disk or a power cut in the
        // middle of `write` is an encrypted index that no password can ever
        // open again. `rename` is atomic on both platforms, and the `sync_all`
        // before it means the bytes are on the disk before the name points at
        // them.
        let tmp = write_private_temp(&path, content.as_bytes()).map_err(|e| {
            format!(
                "Failed to write config file beside {}: {}",
                path.display(),
                e
            )
        })?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Failed to replace config file {}: {}", path.display(), e)
        })?;
        // The rename is atomic but not durable: it is a directory entry, and
        // the directory has its own dirty state. Without this a power cut just
        // after can leave *neither* name, which is the salt loss the whole
        // dance exists to prevent. Best-effort, and on Windows it is a no-op
        // every time rather than only on an exotic filesystem: opening a
        // directory as a file needs `FILE_FLAG_BACKUP_SEMANTICS`, which
        // `fs::File::open` does not pass, so this fails and is skipped. NTFS
        // journals the rename itself, which is the guarantee this is reaching
        // for; on Unix it has to be asked for.
        if let Some(dir) = path.parent() {
            if let Ok(handle) = fs::File::open(dir) {
                let _ = handle.sync_all();
            }
            // Leftovers from a save that died between `create_new` and the
            // rename. The old fixed `config.toml.tmp` overwrote itself, so
            // there was never more than one; a unique name per attempt is what
            // makes the write safe (see `write_private_temp`) and what makes
            // them accumulate, so they are swept here instead.
            sweep_stale_temps(dir, &path);
        }
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

    /// Whether `path` is the index itself — the database, one of SQLite's
    /// `-wal`/`-shm`/`-journal` sidecars, or the instance lock.
    ///
    /// For the paths that arrive from a `files` row rather than from a walk:
    /// once the walk stops indexing the index
    /// ([`crate::file_handling::index_file_set`], which explains why opening
    /// one is fatal), such rows are swept away, but a row written by an older
    /// build survives until the sweep reaches it and can still be opened from
    /// a result list in the meantime.
    ///
    /// The name is compared first and the directory only on a hit, so the
    /// overwhelmingly common miss costs one string comparison rather than the
    /// `canonicalize` the directory check needs.
    ///
    /// Names are folded where the filesystem folds them
    /// ([`crate::platform::PATHS_ARE_CASE_INSENSITIVE`]): a stored row spelling
    /// the database `Index.sqlite` names the same file as a config spelling it
    /// `index.sqlite`, and opening it has the same consequence. ASCII only,
    /// matching `PATH_COLLATION` and [`IgnoreSet`].
    pub fn is_index_file(&self, path: &Path) -> bool {
        let db = self.resolved_database_path();
        let (Some(db_name), Some(name)) = (
            db.file_name().and_then(|s| s.to_str()),
            path.file_name().and_then(|s| s.to_str()),
        ) else {
            return false;
        };
        let same_name = |a: &str, b: &str| {
            if crate::platform::PATHS_ARE_CASE_INSENSITIVE {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        // `str::get`, not `name[..cut]`: the cut is `db_name`'s *byte* length
        // and the two names are unrelated strings, so it can land inside a
        // multi-byte character — `€xyz` against a two-byte `db_name` is six
        // bytes either way — and indexing there panics. A non-boundary is
        // simply not a match.
        let name_matches = same_name(name, db_name)
            || crate::file_handling::INDEX_SIDECAR_SUFFIXES
                .iter()
                .any(|s| {
                    let cut = db_name.len();
                    name.len() == cut + s.len()
                        && name.get(..cut).is_some_and(|head| same_name(head, db_name))
                        && name.get(cut..).is_some_and(|tail| same_name(tail, s))
                });
        if !name_matches {
            return false;
        }
        let same_dir = |a: &Path, b: &Path| {
            a == b
                || a.canonicalize().unwrap_or_else(|_| a.to_path_buf())
                    == b.canonicalize().unwrap_or_else(|_| b.to_path_buf())
        };
        match (path.parent(), db.parent()) {
            (Some(a), Some(b)) => same_dir(a, b),
            // Both at a filesystem root, or neither: the name match stands.
            (None, None) => true,
            _ => false,
        }
    }

    /// `resolved_indexing_paths` canonicalized and spelled the way
    /// stored parents are prefixed with them.
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

/// Write `bytes` to a fresh temporary file beside `target`, owner-only, with
/// its contents flushed to the disk, and return its path for the caller to
/// rename into place.
///
/// `create_new` and a unique name, not a fixed one: `O_NOFOLLOW` refuses a
/// symlink but says nothing about a *regular* file or a hardlink that is
/// already sitting at the name we were going to use. The config directory is
/// not always somewhere only this user can write — a portable install can sit
/// in a shared or removable directory — and an attacker who pre-creates the
/// temp file as a hardlink to a file they can read would otherwise be handed
/// `[security].salt`. `mode(0o600)` only applies to a file this call creates,
/// which is the same reason.
fn write_private_temp(target: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());

    // A handful of attempts, then give up rather than spin: if something is
    // racing us for every name we pick, failing the save is the honest
    // outcome — the caller treats a failed save as fatal to the change.
    let mut last_err = None;
    for _ in 0..8 {
        let tmp = dir.join(format!("{}.{}.tmp", stem, unique_suffix()));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
            opts.mode(0o600);
        }
        match opts.open(&tmp) {
            Ok(mut f) => {
                let wrote = f.write_all(bytes).and_then(|()| f.sync_all());
                return match wrote {
                    Ok(()) => Ok(tmp),
                    Err(e) => {
                        let _ = fs::remove_file(&tmp);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a temporary config file",
        )
    }))
}

/// How old an abandoned temp file must be before [`sweep_stale_temps`] takes
/// it. A save is a serialize, a write and a rename — milliseconds — so an hour
/// is far past any doubt, while still short enough that leftovers do not
/// accumulate across a run of crashes.
const TEMP_SWEEP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Delete abandoned `<config name>.*.tmp` files beside the config.
///
/// [`write_private_temp`] must use a *unique* name — a fixed one can be
/// pre-created by someone else as a hardlink — and a unique name is one that
/// nothing later overwrites, so a process that dies between `create_new` and
/// the rename leaves its temp file behind for good. The old fixed
/// `config.toml.tmp` was reused by the next save and so never accumulated;
/// this is what replaces that property.
///
/// Age is the discriminator, not the recorded PID: a temp file created seconds
/// ago may belong to a save running *right now* in another process, and
/// deleting that would destroy the very write this whole dance protects.
/// Entirely best-effort — a directory that cannot be listed is not a reason to
/// fail a save that already succeeded.
fn sweep_stale_temps(dir: &Path, target: &Path) {
    let Some(stem) = target.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    // The trailing dot matters: without it the config file itself would be a
    // prefix match.
    let prefix = format!("{}.", stem);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let abandoned = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= TEMP_SWEEP_AGE));
        if abandoned {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// A short, non-guessable-enough suffix for the temp name.
///
/// This does not need to be unpredictable to an attacker — `create_new` is
/// what makes the write safe — only unlikely to collide with a leftover from
/// an interrupted save, so the process id and a clock reading are plenty.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
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
