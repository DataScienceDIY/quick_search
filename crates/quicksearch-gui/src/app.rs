//! Application shell: tab strip, per-frame event drains, debounce,
//! status bar, and config-change routing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use quicksearch_core::config::{diff_actions, nested_roots, Config, SecurityConfig};
use quicksearch_core::coordinator::{IndexMode, IndexerState, ReconcileState, WatcherStatus};
use quicksearch_core::db;
use quicksearch_core::indexing::{
    overall_progress, ConfigChange, IndexingStatus, PrepStep, RootPhase, RootProgress,
};
use quicksearch_core::platform::{IndexLock, LockError};
use quicksearch_core::search::SearchOptions;
use quicksearch_core::security::{derive_key, generate_salt, salt_to_hex, IndexKey};
use quicksearch_core::watcher::WatchError;
use zeroize::{Zeroize, Zeroizing};

use crate::backend::Backend;
use crate::color::{palette, Palette};
use crate::duplicates_tab::{DupState, DuplicatesTab};
use crate::format::{fmt_interval, group_thousands};
use crate::keychain;
use crate::logs_tab::LogsTab;
use crate::manage_tab::ManageTab;
use crate::search_tab::SearchTab;
use crate::settings_tab::{SecurityAction, SettingsTab};
use crate::unlock::KeySource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Search,
    Manage,
    Duplicates,
    Logs,
    Help,
    Settings,
}

/// The editor a tab holds, if it stages its edits on a draft rather than
/// saving them the moment they change.
fn tab_editor(tab: Tab) -> Option<UnsavedSource> {
    match tab {
        Tab::Manage => Some(UnsavedSource::Manage),
        Tab::Settings => Some(UnsavedSource::Settings),
        Tab::Search | Tab::Duplicates | Tab::Logs | Tab::Help => None,
    }
}

/// A navigation the unsaved-changes guard put on hold; once nothing relevant
/// is dirty, [`QuickSearchApp::complete_nav`] performs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavIntent {
    SwitchTab(Tab),
    Quit,
}

/// Which editor the guard is currently asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsavedSource {
    Manage,
    Settings,
}

/// The editor `from` holds, if it has one and it is holding unapplied edits.
fn dirty_editor(from: Tab, manage_dirty: bool, settings_dirty: bool) -> Option<UnsavedSource> {
    match tab_editor(from)? {
        UnsavedSource::Manage => manage_dirty.then_some(UnsavedSource::Manage),
        UnsavedSource::Settings => settings_dirty.then_some(UnsavedSource::Settings),
    }
}

/// Which editor the guard must ask about for `intent` while sitting on
/// `from`, if any. A tab switch asks only about the tab being left; Quit asks
/// about Settings before Manage, one prompt at a time, because each draft is
/// a full `Config` snapshot and applying both at once would revert the first.
fn guard_source(
    intent: NavIntent,
    from: Tab,
    manage_dirty: bool,
    settings_dirty: bool,
) -> Option<UnsavedSource> {
    match intent {
        NavIntent::SwitchTab(_) => dirty_editor(from, manage_dirty, settings_dirty),
        NavIntent::Quit if settings_dirty => Some(UnsavedSource::Settings),
        NavIntent::Quit if manage_dirty => Some(UnsavedSource::Manage),
        NavIntent::Quit => None,
    }
}

/// Whether quitting now needs the "settings are still being applied" warning:
/// leaving mid-reconcile leaves entries the user excluded still in the index
/// until a later indexing run redoes the work.
fn quit_needs_reconcile_warning(intent: NavIntent, reconciling: bool) -> bool {
    intent == NavIntent::Quit && reconciling
}

/// Whether leaving the current tab has to go through the unsaved-changes
/// guard. A navigation already on hold wins: a second intent would replace
/// the answer the guard is waiting for.
fn switch_needs_guard(
    from: Tab,
    manage_dirty: bool,
    settings_dirty: bool,
    nav_pending: bool,
) -> bool {
    !nav_pending && dirty_editor(from, manage_dirty, settings_dirty).is_some()
}

pub struct QuickSearchApp {
    cfg: Config,
    backend: Backend,
    tab: Tab,
    search: SearchTab,
    manage: ManageTab,
    dups: DuplicatesTab,
    logs: LogsTab,
    settings: SettingsTab,
    /// Set when applying a config that invalidates the stored index.
    rebuild_prompt: Option<Vec<ConfigChange>>,
    /// The first-start tour, while it is open. Only ever `Some` for a config
    /// file this version created — see [`crate::tutorial`].
    tutorial: Option<crate::tutorial::Tutorial>,
    /// Set while the "delete the index?" confirmation is open.
    clear_prompt: bool,
    /// Nested roots found in the loaded config; shown as a modal over the
    /// Manage tab until dismissed.
    nested_prompt: Option<Vec<(String, String)>>,
    /// How this session's key was obtained, for wording that refers to it.
    key_source: KeySource,
    /// Set when the index on disk was written by a different schema version
    /// and the next run will replace it.
    stale_index_prompt: bool,
    /// Set at startup when the index has not caught up with the settings;
    /// see [`QuickSearchApp::reconcile_owed_ui`].
    reconcile_owed: bool,
    /// `last_full_index` as it read at startup; the run that moves it past
    /// this is the run that clears `reconcile_owed`.
    reconcile_owed_since: Option<u64>,
    /// Set when the watcher gave up on the directory budget and live
    /// updates are off.
    watch_cap_prompt: Option<WatchError>,
    /// The byte-for-byte check of one duplicate group, while its modal is up.
    verify: Option<VerifyModal>,
    /// In-flight security flow (enable/disable/change password).
    security_prompt: Option<SecurityPrompt>,
    /// In-flight show-key flow (confirm password, then reveal).
    key_prompt: Option<KeyPrompt>,
    /// A navigation held by the unsaved-changes guard; see [`NavIntent`].
    pending_nav: Option<NavIntent>,
    /// The guard resolved a Quit: let the next close request through.
    quit_confirmed: bool,
    config_error: Option<String>,
    /// Scripted self-capture driver; `None` unless `QS_CAPTURE_SCRIPT` is set.
    #[cfg(feature = "capture")]
    pub(crate) capture: Option<Box<crate::capture::CaptureDriver>>,
}

mod modals;
mod security;
mod status_bar;
#[cfg(test)]
mod tests;
mod verify;

use security::{KeyPrompt, SecurityPrompt};
use verify::VerifyModal;

impl QuickSearchApp {
    /// `initial_query` pre-fills the search box and fires a search on the
    /// first frame. Takes a plain [`egui::Context`] because construction can
    /// happen mid-session: the unlock gate builds the app only after the
    /// password verifies.
    pub fn new(
        ctx: &egui::Context,
        cfg: Config,
        config_error: Option<String>,
        initial_query: Option<String>,
        key_source: KeySource,
    ) -> Result<QuickSearchApp, String> {
        // Both themes: `style_mut` reaches only the theme in use, and spacing
        // styled on just the live theme reverts to egui's defaults the moment
        // the color scheme is switched.
        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(6.0, 3.0);
            style.spacing.button_padding = egui::vec2(6.0, 2.0);
        });
        ctx.set_zoom_factor(clamp_scale(cfg.ui.scale));

        // Probed before the backend exists: the coordinator's first run can
        // wipe the index, leaving nothing on disk to tell an upgrade apart
        // from a fresh install.
        let stale_index_prompt =
            db::index_needs_rebuild(&cfg.resolved_database_path().to_string_lossy());

        // Also before the backend: in automatic mode the coordinator's first
        // run can reconcile — and clear the answer — before the first frame.
        let db_path = cfg.resolved_database_path().to_string_lossy().into_owned();
        let reconcile_owed = quicksearch_core::scope::outstanding_work(&db_path, &cfg)
            .map(|work| work.touches_index())
            .unwrap_or(false);

        let backend = Backend::start(&cfg, ctx.clone())?;
        // The coordinator stamps this at startup, before its thread can run.
        let reconcile_owed_since = backend.coordinator.state().last_full_index;
        let fuzzy = cfg.search.fuzzy_default;
        // A hand-edited config can nest roots; the coordinator refuses runs
        // until it's fixed.
        let nested = nested_roots(&cfg.paths.indexing_paths);
        let (tab, nested_prompt) = if nested.is_empty() {
            (Tab::Search, None)
        } else {
            (Tab::Manage, Some(nested))
        };
        // `Some(false)` means a config file *this version wrote*, which is
        // the only thing that counts as a first start. A key that is absent
        // (`None`) belongs to an installation that upgraded into this version
        // and has already found its way around.
        let tutorial = (cfg.ui.tutorial_seen == Some(false)).then(crate::tutorial::Tutorial::new);
        let mut search = SearchTab::new(fuzzy, cfg.search.columns.clone(), cfg.search.live_results);
        if let Some(query) = initial_query {
            search.seed(query);
        }
        Ok(QuickSearchApp {
            cfg,
            backend,
            tab,
            search,
            manage: ManageTab::new(),
            dups: DuplicatesTab::new(),
            logs: LogsTab::new(),
            settings: SettingsTab::new(),
            rebuild_prompt: None,
            tutorial,
            clear_prompt: false,
            nested_prompt,
            key_source,
            stale_index_prompt,
            reconcile_owed,
            reconcile_owed_since,
            watch_cap_prompt: None,
            verify: None,
            security_prompt: None,
            key_prompt: None,
            pending_nav: None,
            quit_confirmed: false,
            config_error,
            #[cfg(feature = "capture")]
            capture: crate::capture::CaptureDriver::from_env(),
        })
    }

    fn search_options(&self) -> SearchOptions {
        SearchOptions {
            fuzzy: self.search.fuzzy,
            fuzzy_max_edits: self.cfg.search.fuzzy_max_edits,
            limit: self.cfg.search.display_limit,
            batch: self.cfg.search.results_per_page.max(1),
            session_ignores: self.search.session_ignores.clone(),
        }
    }

    fn start_search(&mut self) {
        let Some(search) = self.backend.search() else {
            return;
        };
        // The single funnel every search goes through — the debounce, `seed`,
        // and every `actions.rerun` producer — so it is the one place the old
        // results' watches have to be dropped.
        self.backend.clear_live();
        let generation = search.search(&self.search.query, self.search_options());
        self.search.on_search_started(generation);
    }

    fn start_duplicates_scan(&mut self, ctx: &egui::Context) {
        self.dups.state = DupState::Loading;
        let cfg = self.cfg.clone();
        self.backend.start_duplicates(&cfg, ctx.clone());
    }

    /// Move to another tab, running what leaving one tab and arriving at the
    /// other owe. Every switch goes through here — including the ones the
    /// unsaved-changes guard completes a frame later, which is why this is a
    /// funnel rather than a comparison against the previous frame's tab.
    fn switch_tab(&mut self, ctx: &egui::Context, to: Tab) {
        if self.tab == to {
            return;
        }
        match self.tab {
            // Watching rows nobody is looking at costs descriptors for
            // nothing.
            Tab::Search => {
                self.backend.clear_live();
                self.search.reset_live();
            }
            // A draft kept while the config is edited elsewhere would go
            // stale, and applying it later would revert those edits.
            Tab::Settings => self.settings.discard(),
            _ => {}
        }
        self.tab = to;
        if to == Tab::Duplicates {
            self.start_duplicates_scan(ctx);
        }
    }

    /// Save + route an edited config to the running services. Reports
    /// whether the config was accepted — a `false` means nothing was saved
    /// and the caller must keep any staged edits alive.
    fn apply_new_config(&mut self, ctx: &egui::Context, mut new: Config) -> bool {
        pin_live_fields(&mut new, &self.cfg);
        if let Some((child, parent)) = nested_roots(&new.paths.indexing_paths).first() {
            self.config_error = Some(format!(
                "Not applied: indexed folder {} is nested under {}",
                child, parent
            ));
            return false;
        }
        // Pruning here makes removing and re-adding a folder warn again
        // rather than staying silently suppressed forever.
        new.ui
            .watch_cap_warned_roots
            .retain(|root| new.paths.indexing_paths.contains(root));
        let actions = diff_actions(&self.cfg, &new);
        // The instance lock follows the database path, and it has to move
        // before the change is written: a `database_path` that another
        // instance already holds must never reach the config file, or the next
        // launch reads it and refuses to start at all. `move_to` takes the new
        // lock before dropping the old, so this rejection leaves us holding
        // what we already had.
        if actions.search_db_changed {
            match IndexLock::move_to(&new.resolved_database_path()) {
                Ok(()) => {}
                Err(LockError::Held { pid }) => {
                    let who = match pid {
                        Some(pid) => format!(" (process {})", pid),
                        None => String::new(),
                    };
                    self.config_error = Some(format!(
                        "Not applied: another QuickSearch{} is using that index.",
                        who
                    ));
                    return false;
                }
                // The move happened and the new path simply cannot be locked.
                // Same rule as at startup: a convenience guard is never a good
                // enough reason to refuse.
                Err(LockError::Unsupported(why)) => {
                    quicksearch_core::log_warn!("cannot lock the index ({}); continuing", why);
                }
            }
        }
        // A config that could not be written must not take effect either: it
        // would apply to this process, revert on restart, and show nothing
        // unsaved in between.
        if let Err(e) = new.save() {
            // Put the lock back on the path the config still names, or this
            // process would go on using the old index while guarding the new
            // one — leaving the index it is actually writing open to a second
            // instance.
            if actions.search_db_changed {
                let _ = IndexLock::move_to(&self.cfg.resolved_database_path());
            }
            self.config_error = Some(e);
            return false;
        }
        if (new.ui.scale - self.cfg.ui.scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(clamp_scale(new.ui.scale));
        }
        if new.ui.search_hotkey != self.cfg.ui.search_hotkey {
            // Only when the setting moved: on Wayland re-registering opens a
            // new portal session, which some desktops confirm with the user.
            crate::hotkey::apply(&new.ui.search_hotkey);
        }
        if new.ui.color_scheme != self.cfg.ui.color_scheme {
            apply_theme(ctx, &new.ui.color_scheme);
        }
        if actions.search_db_changed {
            if let Some(search) = self.backend.search() {
                search.set_db_path(new.resolved_database_path());
            }
        }
        // The coordinator reconciles most changes in place on its own; only
        // settings that leave the stored file unreadable require a rebuild.
        self.backend.coordinator.apply_config(new.clone());
        if actions.requires_rebuild {
            if self.backend.coordinator.state().mode == IndexMode::Auto {
                self.backend.rebuild_index();
            } else {
                let changes = self
                    .backend
                    .coordinator
                    .check_config_validation(&new)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                self.rebuild_prompt = Some(changes);
            }
        }
        self.search.live_enabled = new.search.live_results;
        if new.search.live_results {
            // The watcher holds a copy of the config for its extraction
            // limits and filters, so a config edit has to re-arm; dropping
            // the tab-side state is what makes the next frame do it.
            self.search.reset_live();
        } else {
            self.backend.clear_live();
            self.search.reset_live();
        }
        self.cfg = new;
        true
    }

    /// What the system-wide search shortcut does once the window is up:
    /// show the Search tab with the caret in the query box and any existing
    /// text selected.
    pub(crate) fn activate_search(&mut self, ctx: &egui::Context) {
        if switch_needs_guard(
            self.tab,
            self.manage.is_dirty(),
            self.settings.is_dirty(&self.cfg),
            self.pending_nav.is_some(),
        ) {
            self.pending_nav = Some(NavIntent::SwitchTab(Tab::Search));
        } else {
            self.switch_tab(ctx, Tab::Search);
        }
        self.search.request_focus();
    }

    /// Whether the Settings tab is currently reading a key press to bind.
    pub(crate) fn capturing_hotkey(&self) -> bool {
        self.tab == Tab::Settings && self.settings.capturing_hotkey()
    }

    /// Switch the indexing mode and write it to the config immediately: a
    /// manual stop must survive a restart, or the next launch quietly
    /// resumes the indexing the user just stopped.
    fn set_index_mode(&mut self, auto: bool) {
        self.backend.coordinator.set_mode(if auto {
            IndexMode::Auto
        } else {
            IndexMode::ManualStopped
        });
        if self.cfg.indexing.auto_index == auto {
            return;
        }
        self.cfg.indexing.auto_index = auto;
        if let Err(e) = self.cfg.save() {
            self.config_error = Some(e);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(update) = self.backend.search_rx.try_recv() {
            self.search
                .apply_update(update, self.cfg.search.display_limit);
        }
        // Every live update is something the watcher read off the disk that
        // the index has not been told about. Handing the paths back keeps the
        // index from drifting away from the rows on screen — and it is the
        // only thing that does so while indexing is stopped.
        let mut touched: Vec<PathBuf> = Vec::new();
        while let Ok(update) = self.backend.live_rx.try_recv() {
            touched.push(PathBuf::from(update.path()));
            // A rename has two sides: the old path leaves the index and the
            // new one enters it.
            if let quicksearch_core::live::LiveUpdate::Renamed { to, .. } = &update {
                touched.push(PathBuf::from(to));
            }
            self.search.apply_live(update);
        }
        self.backend.reindex_live_paths(touched);
        // Duplicates worker.
        if let Some(rx) = &self.backend.dup_job {
            use std::sync::mpsc::TryRecvError;
            let done = match rx.try_recv() {
                Ok(Ok(groups)) => Some(DupState::Loaded(crate::duplicates_tab::LoadedGroups::new(
                    groups,
                ))),
                Ok(Err(e)) => Some(DupState::Error(e)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(DupState::Error("duplicates scan aborted".into()))
                }
            };
            if let Some(state) = done {
                self.dups.state = state;
                self.backend.dup_job = None;
            }
        }
        self.drain_verify();
    }

    fn tick_debounce(&mut self, ctx: &egui::Context) {
        let Some(edited_at) = self.search.pending_edit else {
            return;
        };
        let debounce = Duration::from_millis(self.cfg.search.debounce_ms);
        let elapsed = edited_at.elapsed();
        if elapsed >= debounce {
            self.search.pending_edit = None;
            self.start_search();
        } else {
            ctx.request_repaint_after(debounce - elapsed);
        }
    }
}

/// State the scripted capture driver steers and waits on; see
/// [`crate::capture`].
#[cfg(feature = "capture")]
impl QuickSearchApp {
    /// Route through the same pending-nav path a click takes, so the
    /// Duplicates auto-scan still fires and the guard keeps its invariants.
    pub(crate) fn capture_request_tab(&mut self, tab: Tab) {
        if self.pending_nav.is_none() {
            self.pending_nav = Some(NavIntent::SwitchTab(tab));
        }
    }

    pub(crate) fn capture_indexing_status(&self) -> IndexingStatus {
        self.backend.coordinator.state().activity
    }

    /// Let the close request a scripted quit sends through both guards.
    pub(crate) fn capture_confirm_quit(&mut self) {
        self.quit_confirmed = true;
    }

    pub(crate) fn capture_search_settled(&self) -> bool {
        self.search.settled()
    }

    pub(crate) fn capture_dups_done(&self) -> bool {
        matches!(self.dups.state, DupState::Loaded(_) | DupState::Error(_))
    }

    /// Empty the query through the same edit path typing uses, so the empty
    /// search runs and the results table clears.
    pub(crate) fn capture_clear_query(&mut self) {
        self.search.query.clear();
        // Qualified: a plain `use` would warn in every default build.
        self.search.pending_edit = Some(std::time::Instant::now());
    }

    pub(crate) fn capture_focus_search(&mut self) {
        self.search.request_focus();
    }

    pub(crate) fn capture_match_cell(&self, n: usize) -> Option<egui::Rect> {
        self.search.capture_match_cell(n)
    }
}

/// Overwrite the fields a config draft must never carry back: both are live
/// state saved the moment they change, so a stale draft would silently
/// revert protection, the salt, or the indexing mode.
pub(crate) fn pin_live_fields(new: &mut Config, live: &Config) {
    new.security = live.security.clone();
    new.indexing.auto_index = live.indexing.auto_index;
    // The column picker writes straight to the live config the moment a
    // checkbox moves — from the table header *or* from the Settings tab,
    // which is why the Settings controls for it are not draft-backed. Pinning
    // here is what stops a draft taken before a header-menu change from
    // undoing it on Apply.
    new.search.columns = live.search.columns.clone();
}

/// Keep the configured UI scale within sane, recoverable bounds.
fn clamp_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.5, 2.5)
    } else {
        1.1
    }
}

/// What `[ui] color_scheme` means to egui. Anything but `light` is dark:
/// the setting is hand-editable. The desktop's own light/dark setting is
/// not consulted.
pub(crate) fn theme_for(setting: &str) -> egui::Theme {
    match setting.trim().to_ascii_lowercase().as_str() {
        "light" => egui::Theme::Light,
        _ => egui::Theme::Dark,
    }
}

/// Apply the configured color scheme; egui repaints with it on the next
/// frame, so no restart is needed.
pub(crate) fn apply_theme(ctx: &egui::Context, setting: &str) {
    ctx.set_theme(theme_for(setting));
}

impl eframe::App for QuickSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First, so a scripted navigation is held before the tab strip reads
        // this frame's state.
        #[cfg(feature = "capture")]
        self.capture_tick(ctx);

        self.drain_events();
        self.tick_debounce(ctx);

        // The close must be cancelled *this* frame — once the window is gone
        // there is nothing left to ask — and re-sent from `complete_nav` if
        // the user chooses to leave.
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && (self.manage.is_dirty()
                || self.settings.is_dirty(&self.cfg)
                || self.backend.coordinator.reconciling())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // Quitting subsumes any narrower pending navigation.
            self.pending_nav = Some(NavIntent::Quit);
        }

        self.status_bar(ctx);

        // Tab clicks land on a local first so the unsaved-changes guard can
        // hold them.
        let mut requested = self.tab;
        egui::TopBottomPanel::top("tab-strip").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut requested, Tab::Search, "Search");
                ui.selectable_value(&mut requested, Tab::Manage, "Manage Index");
                ui.selectable_value(&mut requested, Tab::Duplicates, "Duplicates");
                ui.selectable_value(&mut requested, Tab::Logs, "Logs");
                ui.selectable_value(&mut requested, Tab::Help, "Help");
                ui.selectable_value(&mut requested, Tab::Settings, "Settings");
            });
        });
        if requested != self.tab {
            if switch_needs_guard(
                self.tab,
                self.manage.is_dirty(),
                self.settings.is_dirty(&self.cfg),
                self.pending_nav.is_some(),
            ) {
                self.pending_nav = Some(NavIntent::SwitchTab(requested));
            } else {
                self.switch_tab(ctx, requested);
            }
        }

        if let Some(err) = &self.config_error {
            let err = err.clone();
            egui::TopBottomPanel::top("config-error").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Config problem: {} (using defaults)", err),
                    );
                    if ui.small_button("Dismiss").clicked() {
                        self.config_error = None;
                    }
                });
            });
        }
        self.reconcile_owed_ui(ctx);

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Search => {
                let actions = self.search.ui(ui);
                if let Some(fuzzy) = actions.save_fuzzy_default {
                    self.cfg.search.fuzzy_default = fuzzy;
                    if let Err(e) = self.cfg.save() {
                        self.config_error = Some(e);
                    }
                }
                // Live state, saved the moment it changes — like the fuzzy
                // default above, and unlike anything edited through the
                // Settings draft. The Settings tab's own column controls take
                // this same path, so the two editors cannot disagree and a
                // stale draft cannot revert either of them.
                if let Some(columns) = actions.save_columns {
                    self.cfg.search.columns = columns;
                    if let Err(e) = self.cfg.save() {
                        self.config_error = Some(e);
                    }
                }
                if let Some(pattern) = actions.persist_ignore {
                    let mut new_cfg = self.cfg.clone();
                    if !new_cfg.indexing.ignore_patterns.contains(&pattern) {
                        new_cfg.indexing.ignore_patterns.push(pattern);
                        self.apply_new_config(ctx, new_cfg);
                    }
                }
                if let Some(targets) = actions.live_targets {
                    self.backend
                        .watch_live(&self.search.query, targets, &self.cfg);
                }
                if actions.rerun {
                    self.start_search();
                }
            }
            Tab::Manage => {
                let state = self.backend.coordinator.state();
                let actions = self.manage.ui(ui, &state, &self.cfg);
                if actions.start_now {
                    self.backend.coordinator.reindex_now();
                }
                if actions.stop {
                    self.set_index_mode(false);
                }
                if actions.auto {
                    self.set_index_mode(true);
                }
                if actions.clear_index {
                    self.clear_prompt = true;
                }
                if actions.start_now || actions.stop || actions.auto {
                    // Keep repainting while the command lands — fast runs
                    // otherwise flash by between frames.
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                if let Some(new_cfg) = actions.apply_config {
                    if self.apply_new_config(ctx, new_cfg) {
                        self.manage.mark_applied();
                    }
                }
            }
            Tab::Duplicates => {
                let actions = self.dups.ui(ui, self.verify.is_some());
                if actions.refresh {
                    self.start_duplicates_scan(ctx);
                }
                if let Some(paths) = actions.verify {
                    let paths: Vec<std::path::PathBuf> =
                        paths.into_iter().map(std::path::PathBuf::from).collect();
                    self.backend
                        .start_verify(paths.clone(), &self.cfg, ctx.clone());
                    self.verify = Some(VerifyModal::new(paths));
                }
            }
            Tab::Logs => self.logs.ui(ui),
            Tab::Help => {
                if crate::help_tab::ui(ui) {
                    self.show_tutorial();
                }
            }
            Tab::Settings => {
                let out = self.settings.ui(ui, &self.cfg);
                if let Some(new_cfg) = out.applied {
                    self.apply_new_config(ctx, new_cfg);
                }
                if let Some(action) = out.security {
                    self.handle_security_action(action);
                }
                // Same live path the table header's picker takes, so the two
                // controls stay in step and neither needs an Apply.
                if let Some(columns) = out.columns {
                    self.cfg.search.columns = columns.clone();
                    self.search.columns = columns;
                    self.search.mark_sort_dirty();
                    if let Err(e) = self.cfg.save() {
                        self.config_error = Some(e);
                    }
                }
            }
        });

        self.rebuild_prompt_ui(ctx);
        self.security_prompt_ui(ctx);
        self.key_prompt_ui(ctx);
        self.clear_prompt_ui(ctx);
        self.nested_prompt_ui(ctx);
        // Ahead of the watch-cap warning: on a fresh upgrade both can be true.
        self.stale_index_prompt_ui(ctx);
        self.watch_cap_prompt_ui(ctx);
        self.verify_modal_ui(ctx);
        self.tutorial_ui(ctx);
        // Last: the guard must sit above everything else on screen.
        self.unsaved_prompt_ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.backend.shutdown();
    }
}
