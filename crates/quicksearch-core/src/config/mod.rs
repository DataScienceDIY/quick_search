//! Configuration: TOML file, resolution rules, filter sets, and live-update
//! classification. A portable `config.toml` beside the executable wins over
//! the XDG location; relative paths inside it resolve against its directory.

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
    pub duplicates: DuplicatesConfig,
    pub ui: UiConfig,
    pub security: SecurityConfig,
    /// File this config was loaded from; `save()` writes back to it.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PathConfig {
    /// Directory roots to index; duplicates and nested roots are de-duplicated
    /// at run time.
    pub indexing_paths: Vec<String>,
    /// SQLite index location. Relative values resolve against the config
    /// file's directory; `~` expands to the home directory.
    pub database_path: String,
}

/// What to index and when — the knobs the coordinator and walker consume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IndexingConfig {
    /// `true`: watchers apply changes live and a full reindex runs every
    /// `reindex_interval_minutes`. `false`: nothing runs until the user asks.
    pub auto_index: bool,
    pub reindex_interval_minutes: u64,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
    /// Empty = extract content from everything supported. Non-empty = only
    /// these extensions get content extraction/FTS; everything else is still
    /// listed for filename search. Case-insensitive, leading dot optional;
    /// [`EXTENSIONLESS`] whitelists extension-less files; `#` starts a
    /// comment. Applied at walk time, so a change forces a rebuild.
    pub content_extensions: Vec<String>,
    /// Excluded from the index entirely — never even listed. A pattern
    /// without `/` matches any single path component (so `.git` prunes
    /// whole subtrees); one containing `/` matches the full path. Glob
    /// syntax (`*`, `?`, `[..]`).
    pub ignore_patterns: Vec<String>,
    /// Per-root walker thread override, keyed by the root as spelled in
    /// `indexing_paths` (both sides are canonicalized before matching).
    /// Absent or 0 = auto-detect. Read at run start.
    pub root_workers: std::collections::HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProcessingConfig {
    /// Bytes read from the head of each new or changed file: the identity
    /// hash window, the MIME sniff window, and the inline-extract cap.
    /// Changing it invalidates stored hashes and forces a rebuild.
    /// 8192 is measured (`examples/hashprobe.rs`); don't retune casually.
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_text_file_size: u64,
    pub batch_size: usize,
    /// Writer time one root's turn may take before the round moves on, in
    /// milliseconds — the bound on how long any one root can hold up the
    /// others. `0` gives each turn one `batch_size` quantum and no more.
    pub writer_turn_slice_ms: u64,
    pub fts_update_batch_size: usize,
    /// How large the WAL may grow during a run before the indexer forces a
    /// checkpoint, in bytes. `0` disables; else raised to [`MINIMUM_WAL_SIZE`].
    /// Needed because autocheckpoint can only *reset* the log when no reader
    /// is mid-query, and a run keeps a reader per root busy throughout.
    ///
    /// **Both directions cost something, which is why the default is neither
    /// end of the range.** A checkpoint blocks the writer for its whole
    /// copy-back and has to evict every per-root reader first, so a low value
    /// stalls indexing often. A high one is paid by *readers*: SQLite searches
    /// the log before every page it fetches from the database file, one hash
    /// block per 4096 frames, and a page that is not in the log is charged for
    /// all of them — so a larger log slows the walk prefetchers and every
    /// search run alongside indexing. It also lengthens WAL recovery after an
    /// unclean exit, which is read and checksummed frame by frame.
    ///
    /// The default trades toward fewer stalls; lower it if searching during a
    /// run matters more than the run finishing quickly.
    pub maximum_wal_size: u64,
    pub tokenize: String,
    /// When `true` (default), extracted text is stored zstd-compressed in
    /// `documents_text` for snippet previews. When `false` queries return the
    /// same hits, but results carry no snippets, can't be case-verified or
    /// occurrence-ranked, and fuzzy full-text search is unavailable.
    pub store_text_for_snippets: bool,
}

/// Floor on a non-zero [`ProcessingConfig::maximum_wal_size`]: a checkpoint
/// waits on running searches, so a cap that fires every round stalls the
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
    /// Ceiling on the fuzzy stages' Levenshtein budget, which grows with the
    /// term (one edit per three characters) up to this cap. 0 disables the
    /// fuzzy stages; above [`FUZZY_EDITS_WARN_ABOVE`] the GUI and CLI warn.
    pub fuzzy_max_edits: usize,
    /// Hard cap on buffered/displayed results per search.
    pub display_limit: usize,
    /// Streaming batch size — how many hits per update event.
    pub results_per_page: usize,
    pub debounce_ms: u64,
    /// Watch the visible search results and show renames, deletions and
    /// content changes as they happen. See [`crate::live`].
    pub live_results: bool,
    /// Page cache held by the search connection, in MiB. **`0` derives it from
    /// the index** — see [`crate::db::schema::recommended_search_cache_mib`],
    /// which sizes it to hold the `files` table because that is what every
    /// keystroke rescans. Set it only when the derived value is wrong for your
    /// tree; the GUI shows the recommendation next to the field.
    pub cache_size_mib: usize,
    pub columns: ColumnsConfig,
}

/// Which columns the Search tab shows. The path column is deliberately not
/// represented: it is always shown, so "no columns at all" is not a state
/// this can hold.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColumnsConfig {
    pub name: bool,
    /// The matched excerpt from a file's contents.
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

/// What the Duplicates tab lists. Nothing here changes what is indexed or
/// what a search finds — a file excluded from the duplicate listing is still
/// in the index and still turns up in results.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DuplicatesConfig {
    /// Globs, the same syntax and the same matcher as
    /// `indexing.ignore_patterns` ([`IgnoreSet`]). A member whose path matches
    /// is not counted, and a group left with fewer than two members is not
    /// listed at all.
    pub exclude_patterns: Vec<String>,
    /// Content hashes of groups dismissed with "Hide this group", lowercase
    /// hex — see [`crate::search::DuplicateGroup::hash_hex`]. Keyed by hash
    /// rather than by path so a hidden group stays hidden when its files are
    /// renamed or moved, and comes back when their contents change.
    ///
    /// The hash covers `processing.hash_length` bytes, so changing that
    /// setting (which rebuilds the index anyway) strands every entry here.
    pub hidden_groups: Vec<String>,
}

impl SearchConfig {
    /// The caution to show next to `fuzzy_max_edits`, or `None` when sane.
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
            maximum_wal_size: 1024 * 1024 * 1024 * 2,
            tokenize: "trigram".to_string(),
            store_text_for_snippets: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            fuzzy_default: true,
            fuzzy_max_edits: 2,
            display_limit: 1000,
            results_per_page: 100,
            debounce_ms: 150,
            live_results: true,
            // Derived from the index; see the field's doc comment.
            cache_size_mib: 0,
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
    /// app when a password is set; absent until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    /// Store the derived key in the OS keychain (Secret Service / Windows
    /// Credential Manager) and skip the startup prompt on this machine.
    pub use_keychain: bool,
}

impl SecurityConfig {
    /// The decoded salt. An error means the config is unusable for unlocking
    /// — surfaced to the user, never guessed around.
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
    /// Zoom factor for the whole GUI. Applied live.
    pub scale: f32,
    /// Roots the "live updates are disabled" warning has already been shown
    /// for. Keyed by root so that adding a folder warns again while
    /// restarting the app does not.
    pub watch_cap_warned_roots: Vec<String>,
    /// The shortcut QuickSearch claims for itself while running, as
    /// `Ctrl+Shift+F`. Empty disables it; an unparseable value degrades to
    /// "no shortcut". On Wayland the desktop, not this value, has the final
    /// say. It cannot fire while QuickSearch is not running: that is what a
    /// desktop binding to `quicksearch --toggle` is for.
    pub search_hotkey: String,
    /// `dark` or `light`. An unrecognised value falls back to dark, where a
    /// typed-out enum would fail to deserialize and take the whole config
    /// file down with it.
    pub color_scheme: String,
    /// Whether the Settings tab shows the technical settings as well as the
    /// everyday ones. Off is the default: most of that tab is byte budgets and
    /// indexer internals that a person who indexed their home folder will
    /// never need, and cannot evaluate without already knowing how the indexer
    /// works.
    ///
    /// A view preference, not a setting the rest of the program reads — it is
    /// written the moment the box is ticked, without an Apply, the way the
    /// column picker is.
    pub show_advanced_settings: bool,
    /// Whether the first-start tour has been dismissed. `None` means the key
    /// predates the tour, so only a config this version *created* is offered
    /// it.
    ///
    /// The field-level `default` is load-bearing: the struct-level
    /// `#[serde(default)]` would fill a missing field from
    /// `UiConfig::default()` — `Some(false)`, handing every upgrading
    /// installation the tour. This one fills it with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tutorial_seen: Option<bool>,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            scale: 1.25,
            watch_cap_warned_roots: Vec::new(),
            search_hotkey: "Ctrl+Shift+F".to_string(),
            color_scheme: "dark".to_string(),
            show_advanced_settings: false,
            // `Some(false)`, not `None`: `None` is reserved for a file that
            // predates the key.
            tutorial_seen: Some(false),
        }
    }
}

/// Build artefacts, plus the Windows entries that are actively harmful to
/// index. No `C:\Windows` pattern: it would also catch a user folder named
/// `Windows`; `config_example.toml` documents it for drive-root users.
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

fn default_home_path() -> String {
    if let Some(home) = crate::platform::home_dir() {
        return home.to_string_lossy().into_owned();
    }
    ".".to_string()
}

fn portable_config_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config.toml"))
}

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

/// Expand a leading `~` to the home directory; `~user` forms are untouched.
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
    /// The portable override next to the binary when present, else the XDG
    /// location.
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

    /// Load from an explicit path; a missing file is created with defaults.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        if path.exists() {
            let content = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read config file {}: {}", path.display(), e))?;
            let mut cfg: Config = toml::from_str(&content)
                .map_err(|e| format!("Failed to parse config file {}: {}", path.display(), e))?;
            cfg.source = Some(path.to_path_buf());
            // Must run before `config_check` compares `hash_length` against
            // what the index was built with, or a clamp applied later would
            // read as a changed setting and force a rebuild.
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
    /// line about each one changed. Clamps, never rejects: a typo must not
    /// stop the app starting. Only fields *not* already defended at their use
    /// sites appear here.
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

        // Zero would make every search return nothing: `cascade::run` stops
        // before its first pass.
        let mut display_limit = self.search.display_limit as u64;
        clamp("[search] display_limit", &mut display_limit, 1, 1_000_000);
        self.search.display_limit = display_limit as usize;

        // 0 is the automatic setting and must survive the clamp; anything else
        // is held to the range the sweep found useful — under the floor is
        // slower than automatic would be, over the cap is resident memory for
        // nothing.
        if self.search.cache_size_mib != 0 {
            let mut cache = self.search.cache_size_mib as u64;
            clamp(
                "[search] cache_size_mib",
                &mut cache,
                crate::db::schema::SEARCH_CACHE_MIN_MIB as u64,
                crate::db::schema::SEARCH_CACHE_OVERRIDE_MAX_MIB as u64,
            );
            self.search.cache_size_mib = cache as usize;
        }

        clamp(
            "[processing] maximum_text_file_size",
            &mut self.processing.maximum_text_file_size,
            1,
            4 * 1024 * 1024 * 1024,
        );

        // Not just the stored text: it is what the extractors size their
        // buffers from (`extract::Limits`), and those are held per worker
        // across pools. 16 MiB is far above any document worth full-text
        // indexing whole and keeps the derived inflation budget sane.
        let mut text_size = self.processing.maximum_text_size as u64;
        clamp(
            "[processing] maximum_text_size",
            &mut text_size,
            1,
            16 * 1024 * 1024,
        );
        self.processing.maximum_text_size = text_size as usize;

        // Below 262 bytes `infer`'s longest magic-number matcher cannot run.
        let mut hash_length = self.processing.hash_length as u64;
        clamp(
            "[processing] hash_length",
            &mut hash_length,
            262,
            1024 * 1024,
        );
        self.processing.hash_length = hash_length as usize;

        // Zero is meaningful (one quantum per turn) and stays legal.
        clamp(
            "[processing] writer_turn_slice_ms",
            &mut self.processing.writer_turn_slice_ms,
            0,
            10_000,
        );

        warnings
    }

    /// Write back to the file this config was loaded from (or the default
    /// location). Raw values are written verbatim — relative paths in a
    /// portable config stay relative. Atomic: see the rename below.
    pub fn save(&self) -> Result<(), String> {
        let path = self.source.clone().unwrap_or_else(Self::config_path);
        if let Some(dir) = path.parent() {
            crate::platform::create_dir_private(dir)
                .map_err(|e| format!("Failed to create config dir {}: {}", dir.display(), e))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        // Write-beside-and-rename, never truncate-then-write: `[security].salt`
        // exists *only* in this file, so a config truncated mid-write is an
        // encrypted index that no password can ever open again. The `sync_all`
        // inside means the bytes are on disk before the name points at them.
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
        // The rename is atomic but not durable: without a directory fsync a
        // power cut just after can leave *neither* name — the salt loss the
        // whole dance exists to prevent. Best-effort; on Windows opening a
        // directory as a file fails (no `FILE_FLAG_BACKUP_SEMANTICS`) and is
        // skipped — NTFS journals the rename itself.
        if let Some(dir) = path.parent() {
            if let Ok(handle) = fs::File::open(dir) {
                let _ = handle.sync_all();
            }
            sweep_stale_temps(dir, &path);
        }
        Ok(())
    }

    /// Directory that relative in-config paths resolve against.
    fn base_dir(&self) -> PathBuf {
        self.source
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `database_path` with `~` expanded and relative values resolved.
    pub fn resolved_database_path(&self) -> PathBuf {
        let p = expand_tilde(&self.paths.database_path);
        if p.is_absolute() {
            p
        } else {
            self.base_dir().join(p)
        }
    }

    /// Whether `path` is the index itself — the database, one of SQLite's
    /// `-wal`/`-shm`/`-journal` sidecars, or the instance lock. Guards paths
    /// that arrive from a stale `files` row, not just from a walk (see
    /// [`crate::file_handling::index_file_set`] for why opening one is fatal).
    ///
    /// Names are folded where the filesystem folds them
    /// ([`crate::platform::PATHS_ARE_CASE_INSENSITIVE`]); ASCII only — SQLite's
    /// `LIKE` folds ASCII only — matching [`IgnoreSet`].
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
        // in an unrelated string, so it can land inside a multi-byte character
        // and indexing there panics. A non-boundary is simply not a match.
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

    /// `resolved_indexing_paths` canonicalized and spelled the way stored
    /// parents are prefixed with them — the form roots must be compared in,
    /// so a re-spelling is not a configuration change. Duplicates collapse,
    /// order is not preserved.
    pub fn normalized_indexing_paths(&self) -> BTreeSet<String> {
        self.resolved_indexing_paths()
            .iter()
            .map(|p| crate::file_handling::normalize_root_string(&p.to_string_lossy()))
            .collect()
    }

    /// `indexing_paths` with `~` expanded and relative values resolved.
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

/// Write `bytes` to a fresh owner-only temporary file beside `target`, synced,
/// and return its path for the caller to rename into place.
///
/// `create_new` and a unique name, not a fixed one: `O_NOFOLLOW` refuses a
/// symlink but not a regular file or hardlink pre-created at the name — the
/// config directory is not always private (portable installs), and an
/// attacker who pre-creates the temp file as a hardlink to a file they can
/// read would be handed `[security].salt`.
fn write_private_temp(target: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());

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
/// it.
const TEMP_SWEEP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Delete abandoned `<config name>.*.tmp` files beside the config. Age is the
/// discriminator, not the recorded PID: a temp file created seconds ago may
/// belong to a save running *right now* in another process, and deleting that
/// would destroy the very write this dance protects. Best-effort.
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

/// Temp-name suffix. Does not need to be unpredictable — `create_new` is what
/// makes the write safe — only unlikely to collide with a leftover.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Reserved `content_extensions` entry standing for "files with no
/// extension"; the parentheses keep it from colliding with a real one.
pub const EXTENSIONLESS: &str = "(none)";

/// The `content_extensions` entries that actually filter: `#` starts a
/// comment, so a whole-line comment drops out and `md  # notes` filters on
/// `md`.
pub fn content_filter_entries(list: &[String]) -> impl Iterator<Item = &str> {
    list.iter().filter_map(|raw| {
        let entry = raw.split('#').next().unwrap_or_default().trim();
        (!entry.is_empty()).then_some(entry)
    })
}

/// Whether a file's content should be indexed under the `content_extensions`
/// filter. Files that fail this are still listed for filename search.
pub fn content_allowed(path: &Path, cfg: &Config) -> bool {
    let list = &cfg.indexing.content_extensions;
    if content_filter_entries(list).next().is_none() {
        return true;
    }
    // `Path::extension` is None for `Makefile` and for dot-only names like
    // `.bashrc`, so without the sentinel a non-empty filter always skips them.
    match path.extension().and_then(|e| e.to_str()) {
        // The sentinel never doubles as an extension: `x.(none)` is not
        // whitelisted by it.
        Some(ext) => content_filter_entries(list)
            .filter(|allowed| !allowed.eq_ignore_ascii_case(EXTENSIONLESS))
            .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(ext)),
        None => {
            content_filter_entries(list).any(|allowed| allowed.eq_ignore_ascii_case(EXTENSIONLESS))
        }
    }
}
