//! Configuration: TOML file, resolution rules, filter sets, and live-update
//! classification. A portable `config.toml` beside the executable, if it is
//! a QuickSearch config, wins over the XDG location; relative paths inside it
//! resolve against its directory.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

mod diff;
mod ignore;
#[cfg(test)]
mod tests;

pub use diff::{diff_actions, nested_roots, ConfigActions, IndexWork};
pub use ignore::{glob_literal, IgnoreSet};

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
    /// File this config was loaded from; `save()` writes back to it, and
    /// refuses when this is `None` ([`Config::fallback`]).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PathConfig {
    /// Directory roots to index. A run is refused if two resolve to the same
    /// or nested folders ([`nested_roots`]). A root may itself be a link;
    /// links inside one are never followed.
    pub indexing_paths: Vec<String>,
    /// SQLite index location. Relative values resolve against the config
    /// file's directory; `~` expands to the home directory.
    pub database_path: String,
}

/// What to index and when: the knobs the coordinator and walker consume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IndexingConfig {
    /// `true`: watchers apply changes live and a full reindex runs every
    /// `reindex_interval_minutes`. `false`: nothing runs until the user asks.
    pub auto_index: bool,
    pub reindex_interval_minutes: u64,
    pub include_hidden: bool,
    /// Empty = extract text from every supported format; otherwise only these
    /// extensions (case-insensitive, leading dot optional, `#` comments,
    /// [`EXTENSIONLESS`] for none). Other files stay listed for filename
    /// search. A change reconciles stored text in place
    /// ([`IndexWork::reconcile_content`]).
    pub content_extensions: Vec<String>,
    /// Excluded from the index entirely, never even listed. A pattern
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
    /// hash window, the MIME sniff window, and the inline-extract cap. Also
    /// the tail the Duplicates tab reads from each listed file
    /// ([`crate::search::split_by_tail`]): at the default 8 KiB, head alone
    /// misgroups 3.44% of 86k real files and head and tail 0.10%.
    /// Changing it invalidates stored hashes and rebuilds the index: at once
    /// in automatic mode, otherwise when confirmed or at the next run.
    /// `examples/hashprobe.rs` measures what a different value would trade.
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_text_file_size: u64,
    /// Rows per write batch in the indexer's writer and the reconciliation
    /// scans. Read through [`ProcessingConfig::batch`], which holds it to
    /// `1..=`[`MAX_BATCH_SIZE`].
    pub batch_size: usize,
    /// Writer time one root's turn may take before the round moves on, in
    /// milliseconds: the bound on how long any one root can hold up the
    /// others. `0` gives each turn one `batch_size` batch and no more.
    pub writer_turn_slice_ms: u64,
    /// How large the WAL may grow during a run before the indexer forces a
    /// checkpoint, in bytes; `0` disables, else raised to [`MINIMUM_WAL_SIZE`].
    /// Autocheckpoint alone cannot reset the log while a run keeps a reader
    /// per root busy. A low value stalls the writer on checkpoints; a high one
    /// slows every reader, which searches the log (one hash block per 4096
    /// frames) before each page it fetches, and lengthens crash recovery.
    /// User-facing guidance is in `config_example.toml`.
    pub maximum_wal_size: u64,
    /// When `true` (default), the full-text index keeps no word positions
    /// (FTS5 `detail=none`): a term is looked up as the AND of its trigrams
    /// and the full-text pass drops the documents where they are not
    /// adjacent, against the stored text. Several times smaller and faster
    /// to build, at the cost of checking more candidates per search; see
    /// [`crate::db::schema::fts_create_sql`] for the measurements.
    pub compact_text_index: bool,
}

impl ProcessingConfig {
    /// [`ProcessingConfig::batch_size`] held to `1..=`[`MAX_BATCH_SIZE`]:
    /// zero would panic `chunks`, and each user checks its deadline only once
    /// per batch, so a hand-edited huge value would be a stall as long.
    pub fn batch(&self) -> usize {
        self.batch_size.clamp(1, MAX_BATCH_SIZE)
    }
}

/// The largest [`ProcessingConfig::batch_size`] in effect, and the top of the
/// settings form's range.
pub const MAX_BATCH_SIZE: usize = 100_000;

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
    /// term (one edit per three bytes) up to this cap, and never past the hard
    /// cap of [`crate::search::fuzzy::MAX_FUZZY_EDITS`]. 0 disables the fuzzy
    /// stages; above [`FUZZY_EDITS_WARN_ABOVE`] the GUI and CLI warn.
    pub fuzzy_max_edits: usize,
    /// Hard cap on buffered/displayed results per search.
    pub display_limit: usize,
    /// Streaming batch size: how many hits per update event.
    pub results_per_page: usize,
    pub debounce_ms: u64,
    /// Watch the visible search results and show renames, deletions and
    /// content changes as they happen. See [`crate::live`].
    pub live_results: bool,
    /// Page cache held by the search connection, in MiB. `0` derives it from
    /// the index ([`crate::db::schema::recommended_search_cache_mib`] sizes it
    /// to hold the `files` table every keystroke rescans). Set it only when
    /// the derived value is wrong for your tree; the GUI shows the
    /// recommendation next to the field.
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
/// what a search finds: a file excluded from the duplicate listing is still
/// in the index and still turns up in results.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DuplicatesConfig {
    /// Globs, the same syntax and the same matcher as
    /// `indexing.ignore_patterns` ([`IgnoreSet`]). A member whose path matches
    /// is not counted, and a group left with fewer than two members is not
    /// listed at all.
    pub exclude_patterns: Vec<String>,
    /// Identities of groups dismissed with "Hide this group": head hash then
    /// tail digest, lowercase hex (see
    /// [`crate::search::DuplicateGroup::identity`]). Keyed by content rather
    /// than by path so a hidden group stays hidden when its files are renamed
    /// or moved, and comes back when their contents change. Anything else,
    /// such as the head hash alone that earlier versions saved, is dropped
    /// when the config loads.
    ///
    /// Both cover `processing.hash_length` bytes, so changing that setting
    /// (which rebuilds the index anyway) strands every entry here.
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
            indexing_paths: default_roots(crate::platform::home_dir()),
            database_path: database_path_setting(default_db_path()),
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        IndexingConfig {
            auto_index: true,
            reindex_interval_minutes: 60,
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
            maximum_text_file_size: 1024 * 1024 * 50,
            batch_size: 500,
            writer_turn_slice_ms: 100,
            maximum_wal_size: 1024 * 1024 * 1024 * 2,
            compact_text_index: true,
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
            // Derived from the index; see the field's doc comment.
            cache_size_mib: 0,
            columns: ColumnsConfig::default(),
        }
    }
}

/// Index encryption. The password itself is never stored anywhere: only the
/// KDF salt lives here, and it is not a secret (it makes the derivation
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
    /// The decoded salt. An error means the config is unusable for unlocking,
    /// and is surfaced to the user, never guessed around.
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
    /// everyday ones; off by default. A view preference, written the moment
    /// the box is ticked, without an Apply, like the column picker.
    pub show_advanced_settings: bool,
    /// Whether the first-start tour has been dismissed. `None` means the key
    /// predates the tour, so only a config this version *created* is offered
    /// it.
    ///
    /// The field-level `default` is load-bearing: the struct-level
    /// `#[serde(default)]` would fill a missing field from
    /// `UiConfig::default()`, i.e. `Some(false)`, handing every upgrading
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

/// Whether `id` is shaped like [`crate::search::DuplicateGroup::identity`]:
/// two SHA-256 digests, head then tail, as 128 lowercase hex digits.
fn is_group_identity(id: &str) -> bool {
    id.len() == 128 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The home directory, else `.`. A home whose name is not valid Unicode
/// gives no root: the walk skips such paths silently, and a lossy spelling
/// must never become a database key.
fn default_roots(home: Option<std::ffi::OsString>) -> Vec<String> {
    match home {
        Some(home) => home.into_string().into_iter().collect(),
        None => vec![".".to_string()],
    }
}

/// Beside the file the user launched, which for an AppImage is the
/// `.AppImage` and not the binary inside its mount.
fn portable_config_path() -> Option<PathBuf> {
    let exe = crate::platform::launched_exe()?;
    Some(exe.parent()?.join("config.toml"))
}

/// The top-level tables of a config file, one per section of [`Config`].
const CONFIG_TABLES: [&str; 7] = [
    "paths",
    "indexing",
    "processing",
    "search",
    "duplicates",
    "ui",
    "security",
];

/// Whether the file at `path` is a QuickSearch config. A portable config
/// sits wherever the binary does, for an AppImage often `~/Downloads`, where
/// another program's `config.toml` would otherwise be adopted and then
/// overwritten by the next save. So a file is passed over, with a warning,
/// only when it is TOML with a top-level key outside [`CONFIG_TABLES`]. One
/// that can't be read or doesn't parse is still ours: its load fails with
/// the usual error, so a typo never silently swaps a portable user's
/// settings for the per-user ones. A missing file, or a directory by the
/// name, is not one.
fn is_quicksearch_config(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(table) = fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    else {
        return true;
    };
    match table.keys().find(|k| !CONFIG_TABLES.contains(&k.as_str())) {
        None => true,
        Some(key) => {
            crate::log_warn!(
                "{} is another program's: QuickSearch has no `{}`; using the per-user config instead",
                path.display(),
                key
            );
            false
        }
    }
}

/// The per-user base directory: `%win_var%` on Windows, else an absolute
/// `$xdg_var`, else `home_rel` under the home directory, else `.`.
fn user_base_dir(win_var: &str, xdg_var: &str, home_rel: &[&str]) -> PathBuf {
    if cfg!(windows) {
        if let Some(d) = std::env::var_os(win_var) {
            return PathBuf::from(d);
        }
    }
    if let Some(p) = std::env::var_os(xdg_var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return p;
    }
    match crate::platform::home_dir() {
        Some(home) => home_rel
            .iter()
            .fold(PathBuf::from(home), |p, part| p.join(part)),
        None => PathBuf::from("."),
    }
}

/// Default index location when the config doesn't name one.
pub fn default_db_path() -> PathBuf {
    user_base_dir("LOCALAPPDATA", "XDG_DATA_HOME", &[".local", "share"])
        .join("quicksearch")
        .join("index.sqlite")
}

/// `path` as the `database_path` setting. A data folder whose name is not
/// valid Unicode gives the relative `index.sqlite`, which [`Config::resolve`]
/// puts beside the config file: a lossy spelling would name another file.
/// Logged once per process, as defaults are built often.
fn database_path_setting(path: PathBuf) -> String {
    path.into_os_string().into_string().unwrap_or_else(|raw| {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            crate::log_warn!(
                "The data folder in {:?} is not valid Unicode; the index goes beside the config file instead",
                raw
            );
        });
        "index.sqlite".to_string()
    })
}

/// Expand a leading `~` to the home directory; `~user` forms are untouched.
/// Extra separators after the tilde are dropped: pushed as they are, `~//x`
/// would be the absolute `/x` and replace the home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        if let Some(home) = crate::platform::home_dir() {
            let mut p = PathBuf::from(home);
            if path.len() > 2 {
                p.push(path[2..].trim_start_matches(std::path::is_separator));
            }
            return p;
        }
    }
    PathBuf::from(path)
}

impl Config {
    /// The portable override next to the binary when that is a QuickSearch
    /// config ([`is_quicksearch_config`]), else the XDG location. Decided
    /// once per process, so every caller (the `--toggle` socket is keyed by
    /// this path) agrees, and a refusal is logged once.
    pub fn config_path() -> PathBuf {
        static PORTABLE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        let portable =
            PORTABLE.get_or_init(|| portable_config_path().filter(|p| is_quicksearch_config(p)));
        match portable {
            Some(p) => p.clone(),
            None => user_base_dir("APPDATA", "XDG_CONFIG_HOME", &[".config"])
                .join("quicksearch")
                .join("config.toml"),
        }
    }

    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::config_path())
    }

    /// What runs when the config file fails to load: the defaults, made
    /// inert. No `source`, so [`Self::save`] refuses and the file stays as
    /// the user left it. No roots, so every run is refused and nothing
    /// reconciles the stored index against a root the user never chose. No
    /// tour, whose dismissal would save. Search still answers from the index
    /// at the default location.
    pub fn fallback() -> Config {
        let mut cfg = Config::default();
        cfg.indexing.auto_index = false;
        cfg.paths.indexing_paths.clear();
        cfg.ui.tutorial_seen = None;
        cfg
    }

    /// Load from an explicit path; a missing file is created with defaults.
    /// Ignore patterns are compiled here, so a bad one fails the load like a
    /// TOML error does, instead of failing the indexer's start later.
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
            let hidden = cfg.duplicates.hidden_groups.len();
            cfg.duplicates.hidden_groups.retain(|id| is_group_identity(id));
            if cfg.duplicates.hidden_groups.len() < hidden {
                crate::log_warn!(
                    "config: dropped {} hidden duplicate group(s) that are not a head and tail \
                     identity (saved by an older version); hide them again if they come back",
                    hidden - cfg.duplicates.hidden_groups.len()
                );
            }
            IgnoreSet::compile(&cfg.indexing.ignore_patterns)
                .map_err(|e| format!("Invalid ignore pattern in {}: {}", path.display(), e))?;
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

    /// Bring numbers that would break the program back into range, returning
    /// a line about each one changed. Numeric fields are clamped, never
    /// rejected: a typo in one must not stop the app starting. Only fields
    /// *not* already defended at their use sites appear here.
    fn clamp_out_of_range(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let w = &mut warnings;

        // Zero would make every search return nothing: `cascade::run` stops
        // before its first pass.
        clamp_field(
            w,
            "[search] display_limit",
            &mut self.search.display_limit,
            1,
            1_000_000,
        );

        // 0 is the automatic setting and must survive the clamp; anything else
        // is held to the range the sweep found useful: under the floor is
        // slower than automatic would be, over the cap is resident memory for
        // nothing.
        if self.search.cache_size_mib != 0 {
            clamp_field(
                w,
                "[search] cache_size_mib",
                &mut self.search.cache_size_mib,
                crate::db::schema::SEARCH_CACHE_MIN_MIB as usize,
                crate::db::schema::SEARCH_CACHE_OVERRIDE_MAX_MIB as usize,
            );
        }

        clamp_field(
            w,
            "[processing] maximum_text_file_size",
            &mut self.processing.maximum_text_file_size,
            1,
            4 * 1024 * 1024 * 1024,
        );

        // Not just the stored text: the extractors size their buffers from it
        // (`extract::Limits`), and those are held per worker across pools.
        // 16 MiB is far above any document worth full-text indexing whole and
        // keeps the derived inflation budget sane.
        clamp_field(
            w,
            "[processing] maximum_text_size",
            &mut self.processing.maximum_text_size,
            1,
            16 * 1024 * 1024,
        );

        // Below 262 bytes `infer`'s longest magic-number matcher cannot run.
        clamp_field(
            w,
            "[processing] hash_length",
            &mut self.processing.hash_length,
            262,
            1024 * 1024,
        );

        // Zero is meaningful (one batch per turn) and stays legal.
        clamp_field(
            w,
            "[processing] writer_turn_slice_ms",
            &mut self.processing.writer_turn_slice_ms,
            0,
            10_000,
        );

        warnings
    }

    /// Where [`Self::save`] writes: the file this config was loaded from.
    /// Without one this is the [`Self::fallback`] for a file that failed to
    /// load, and writing would replace that file with defaults. The message
    /// names the file because the GUI shows one error at a time: the first
    /// refused save replaces the load error there.
    fn save_target(&self) -> Result<&Path, String> {
        self.source.as_deref().ok_or_else(|| {
            format!(
                "Not saved: {} did not load; fix it and restart",
                Self::config_path().display()
            )
        })
    }

    /// Whether [`Self::save`] can write at all: `false` for the
    /// [`Self::fallback`] of a file that failed to load. For callers that must
    /// not begin what only a save can finish.
    pub fn can_save(&self) -> bool {
        self.source.is_some()
    }

    /// Write back to the file this config was loaded from. Raw values are
    /// written verbatim, so relative paths in a portable config stay
    /// relative. Atomic: see the rename below.
    pub fn save(&self) -> Result<(), String> {
        let path = self.save_target()?;
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
        let tmp = write_private_temp(path, content.as_bytes()).map_err(|e| {
            format!(
                "Failed to write config file beside {}: {}",
                path.display(),
                e
            )
        })?;
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Failed to replace config file {}: {}", path.display(), e)
        })?;
        // The rename is atomic but not durable: without a directory fsync a
        // power cut just after can leave *neither* name, the salt loss the
        // whole dance exists to prevent. Best-effort; on Windows opening a
        // directory as a file fails (no `FILE_FLAG_BACKUP_SEMANTICS`) and is
        // skipped, since NTFS journals the rename itself.
        if let Some(dir) = path.parent() {
            if let Ok(handle) = fs::File::open(dir) {
                let _ = handle.sync_all();
            }
            sweep_stale_temps(dir, path);
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

    /// `raw` with `~` expanded and a relative value resolved against the
    /// config file's directory: where the walk looks for a root spelled
    /// `raw`.
    pub fn resolve(&self, raw: &str) -> PathBuf {
        let p = expand_tilde(raw);
        if p.is_absolute() {
            p
        } else {
            self.base_dir().join(p)
        }
    }

    /// `database_path` with `~` expanded and relative values resolved.
    pub fn resolved_database_path(&self) -> PathBuf {
        self.resolve(&self.paths.database_path)
    }

    /// Whether `path` is the index itself: the database, one of SQLite's
    /// `-wal`/`-shm`/`-journal` sidecars, or the instance lock. Guards paths
    /// that arrive from a stale `files` row or a watcher event, not from a
    /// walk (see [`crate::file_handling::index_file_set`] for why opening one
    /// is fatal): the coordinator drops such events before they queue, and
    /// `incremental::upsert_file` refuses one before opening it.
    ///
    /// Names are folded where the filesystem folds them
    /// ([`crate::platform::PATHS_ARE_CASE_INSENSITIVE`]), ASCII only, as
    /// SQLite's `LIKE` and [`IgnoreSet`] fold.
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
        // The empty suffix is the database itself. `str::get`, not
        // `name[..cut]`: the cut is `db_name`'s *byte* length in an unrelated
        // string, so it can land inside a multi-byte character and indexing
        // there panics. A non-boundary is simply not a match.
        let cut = db_name.len();
        let name_matches = std::iter::once("")
            .chain(crate::file_handling::INDEX_SIDECAR_SUFFIXES)
            .any(|s| {
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

    /// Each configured root as spelled, paired with its normalized form: the
    /// resolved path canonicalized and spelled the way stored parents are
    /// prefixed with it, the form roots must be compared in.
    pub fn indexing_roots(&self) -> impl Iterator<Item = (&String, String)> + '_ {
        self.paths.indexing_paths.iter().map(|raw| {
            let resolved = self.resolve(raw);
            let root = crate::file_handling::normalize_root_string(&resolved.to_string_lossy());
            (raw, root)
        })
    }

    /// The normalized roots of [`Self::indexing_roots`], so a re-spelling is
    /// not a configuration change. Duplicates collapse, order is not kept.
    pub fn normalized_indexing_paths(&self) -> BTreeSet<String> {
        self.indexing_roots().map(|(_, root)| root).collect()
    }

    /// `indexing_paths` with `~` expanded and relative values resolved.
    pub fn resolved_indexing_paths(&self) -> Vec<PathBuf> {
        self.paths
            .indexing_paths
            .iter()
            .map(|raw| self.resolve(raw))
            .collect()
    }
}

/// Write `bytes` to a fresh owner-only temporary file beside `target`, synced,
/// and return its path for the caller to rename into place.
///
/// `create_new` and a unique name, not a fixed one: `O_NOFOLLOW` refuses a
/// symlink but not a regular file or hardlink pre-created at the name. The
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

/// Temp-name suffix. Only needs to be unlikely to collide with a leftover,
/// not unpredictable: `create_new` makes the write safe.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Clamp `value` into `lo..=hi`, noting in `warnings` when it moved.
fn clamp_field<T: Ord + Copy + std::fmt::Display>(
    warnings: &mut Vec<String>,
    name: &str,
    value: &mut T,
    lo: T,
    hi: T,
) {
    let bounded = (*value).clamp(lo, hi);
    if bounded != *value {
        warnings.push(format!(
            "{} is {}, which is out of range; using {}",
            name, *value, bounded
        ));
        *value = bounded;
    }
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
