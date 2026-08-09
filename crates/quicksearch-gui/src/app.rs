//! Application shell: tab strip, per-frame event drains, debounce,
//! status bar, and config-change routing.

use std::sync::mpsc;
use std::time::Duration;

use quicksearch_core::config::{diff_actions, nested_roots, Config, SecurityConfig};
use quicksearch_core::coordinator::{IndexMode, IndexerState, ReconcileState, WatcherStatus};
use quicksearch_core::db;
use quicksearch_core::indexing::{
    overall_progress, ConfigChange, IndexingStatus, PrepStep, RootPhase, RootProgress,
};
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
use crate::options::{OptionsWindow, SecurityAction};
use crate::search_tab::SearchTab;
use crate::unlock::KeySource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Search,
    Manage,
    Duplicates,
    Logs,
    Help,
}

/// A navigation the unsaved-changes guard put on hold. The intent survives
/// while the guard walks the dirty editors (on Quit there can be two); once
/// nothing relevant is dirty, [`QuickSearchApp::complete_nav`] performs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavIntent {
    /// Leave the Manage tab for this one.
    SwitchTab(Tab),
    /// Close the Options window.
    CloseOptions,
    /// Close the application window.
    Quit,
}

/// Which editor the guard is currently asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsavedSource {
    Manage,
    Options,
}

/// A button (or Esc/backdrop click) in the unsaved-changes modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UnsavedChoice {
    Apply,
    Discard,
    Cancel,
}

/// Which editor the guard must ask about for `intent`, if any. Quit asks
/// about Options before Manage — sequential prompts, one decision each; a
/// combined prompt could not Apply both drafts safely, since each is a full
/// `Config` snapshot and the second apply would revert the first.
fn guard_source(
    intent: NavIntent,
    manage_dirty: bool,
    options_dirty: bool,
) -> Option<UnsavedSource> {
    match intent {
        NavIntent::SwitchTab(_) => manage_dirty.then_some(UnsavedSource::Manage),
        NavIntent::CloseOptions => options_dirty.then_some(UnsavedSource::Options),
        NavIntent::Quit if options_dirty => Some(UnsavedSource::Options),
        NavIntent::Quit if manage_dirty => Some(UnsavedSource::Manage),
        NavIntent::Quit => None,
    }
}

/// Whether quitting now needs the "settings are still being applied" warning.
///
/// Only a Quit, and only while a reconciliation is actually running: leaving
/// mid-pass leaves entries the user excluded still in the index until an
/// indexing run redoes the work, which in manual mode means until they ask for
/// one. Its own function because it is a rule, not a rendering decision, and
/// because the guard it belongs to has two entrances — the close request, and
/// the unsaved-changes prompt resolving to Quit.
fn quit_needs_reconcile_warning(intent: NavIntent, reconciling: bool) -> bool {
    intent == NavIntent::Quit && reconciling
}

/// Whether leaving the current tab has to go through the unsaved-changes
/// guard instead of happening directly.
///
/// Its own function because the tab strip is no longer the only way to leave
/// a tab: the system-wide search shortcut does it too, from outside the
/// window, and a second copy of this rule is how one of them would quietly
/// start discarding a page of staged index settings.
///
/// A navigation already on hold wins: the guard is walking one decision at a
/// time and a second intent would replace the answer it is waiting for.
fn switch_needs_guard(from: Tab, manage_dirty: bool, nav_pending: bool) -> bool {
    from == Tab::Manage && manage_dirty && !nav_pending
}

pub struct QuickSearchApp {
    cfg: Config,
    backend: Backend,
    tab: Tab,
    search: SearchTab,
    manage: ManageTab,
    dups: DuplicatesTab,
    logs: LogsTab,
    options: OptionsWindow,
    /// Set when applying a config that invalidates the stored index.
    rebuild_prompt: Option<Vec<ConfigChange>>,
    /// Set while the "delete the index?" confirmation is open.
    clear_prompt: bool,
    /// Nested roots found in the loaded config (startup validation); shown
    /// as a modal over the Manage tab until dismissed.
    nested_prompt: Option<Vec<(String, String)>>,
    /// How this session's key was obtained, for wording that refers to it.
    key_source: KeySource,
    /// Set when the index on disk was written by a different schema version
    /// and the next run will replace it; see
    /// [`QuickSearchApp::stale_index_prompt_ui`].
    stale_index_prompt: bool,
    /// Set at startup when the index has not caught up with the settings —
    /// a reconciliation cut short by a previous quit, or a config edited
    /// while the app was closed. See [`QuickSearchApp::reconcile_owed_ui`].
    reconcile_owed: bool,
    /// `last_full_index` as it read at startup; the run that moves it past
    /// this is the run that clears `reconcile_owed`.
    reconcile_owed_since: Option<u64>,
    /// Set when the watcher gave up on the directory budget and live
    /// updates are off; see [`QuickSearchApp::check_watch_cap_warning`].
    watch_cap_prompt: Option<WatchError>,
    /// In-flight security flow (enable/disable/change password), driven by
    /// the Options window's Security block.
    security_prompt: Option<SecurityPrompt>,
    /// A navigation held by the unsaved-changes guard; see [`NavIntent`].
    pending_nav: Option<NavIntent>,
    /// The guard resolved a Quit: let the next close request through.
    quit_confirmed: bool,
    config_error: Option<String>,
    /// Scripted self-capture driver; `None` unless a `capture` build has
    /// `QS_CAPTURE_SCRIPT` set. See [`crate::capture`].
    #[cfg(feature = "capture")]
    pub(crate) capture: Option<Box<crate::capture::CaptureDriver>>,
}

/// The two-step security flow: collect a password (enable/change), derive
/// its key off the UI thread, then confirm the mandatory index rebuild.
/// Disabling skips straight to the confirmation.
enum SecurityPrompt {
    SetPassword {
        pw1: String,
        pw2: String,
        remember: bool,
        change: bool,
    },
    Deriving {
        rx: mpsc::Receiver<(SecurityConfig, IndexKey)>,
    },
    ConfirmRebuild {
        new_security: SecurityConfig,
        new_key: Option<IndexKey>,
    },
}

impl Drop for SecurityPrompt {
    fn drop(&mut self) {
        if let SecurityPrompt::SetPassword { pw1, pw2, .. } = self {
            pw1.zeroize();
            pw2.zeroize();
        }
    }
}

impl QuickSearchApp {
    /// `initial_query` pre-fills the search box and fires a search on the
    /// first frame. It carries the positional arguments the binary was given,
    /// which on Windows is the only thing the GUI can do with them — terminal
    /// output belongs to `quicksearch-cli` there.
    /// Takes a plain [`egui::Context`] rather than eframe's
    /// `CreationContext` because construction can happen mid-session: the
    /// unlock gate builds the app only after the password verifies.
    pub fn new(
        ctx: &egui::Context,
        cfg: Config,
        config_error: Option<String>,
        initial_query: Option<String>,
        key_source: KeySource,
    ) -> Result<QuickSearchApp, String> {
        // Compact styling: results density is the whole point. Both themes,
        // because `style_mut` reaches only the one in use — styling just the
        // live theme means the spacing reverts to egui's defaults the moment
        // the color scheme is switched.
        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(6.0, 3.0);
            style.spacing.button_padding = egui::vec2(6.0, 2.0);
        });
        ctx.set_zoom_factor(clamp_scale(cfg.ui.scale));

        // Probed *before* the backend exists. The coordinator can begin a full
        // run — and with it the wipe — the moment it starts, and afterwards
        // there is nothing left on disk to tell an upgrade apart from a fresh
        // install.
        let stale_index_prompt =
            db::index_needs_rebuild(&cfg.resolved_database_path().to_string_lossy());

        // Also before the backend, and for the same reason: in automatic mode
        // the coordinator's first run can reconcile — and clear the answer —
        // before the first frame is drawn. A missing or unreadable index owes
        // nothing; it has never been reconciled against anything.
        let db_path = cfg.resolved_database_path().to_string_lossy().into_owned();
        let reconcile_owed = quicksearch_core::scope::outstanding_work(&db_path, &cfg)
            .map(|work| work.touches_index())
            .unwrap_or(false);

        let backend = Backend::start(&cfg, ctx.clone())?;
        // Read from the coordinator rather than the file: it stamps this at
        // startup, before its thread can run anything, so it is the same
        // number the frames below compare against.
        let reconcile_owed_since = backend.coordinator.state().last_full_index;
        let fuzzy = cfg.search.fuzzy_default;
        // Startup validation: a hand-edited config can nest roots, which
        // per-root pipelines can't accept. Redirect straight to the folder
        // list with an explanatory modal; the coordinator refuses runs
        // until it's fixed.
        let nested = nested_roots(&cfg.paths.indexing_paths);
        let (tab, nested_prompt) = if nested.is_empty() {
            (Tab::Search, None)
        } else {
            (Tab::Manage, Some(nested))
        };
        let mut search = SearchTab::new(fuzzy);
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
            options: OptionsWindow::new(),
            rebuild_prompt: None,
            clear_prompt: false,
            nested_prompt,
            key_source,
            stale_index_prompt,
            reconcile_owed,
            reconcile_owed_since,
            watch_cap_prompt: None,
            security_prompt: None,
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
        let generation = self
            .backend
            .search()
            .search(&self.search.query, self.search_options());
        self.search.on_search_started(generation);
    }

    fn start_duplicates_scan(&mut self, ctx: &egui::Context) {
        self.dups.state = DupState::Loading;
        let cfg = self.cfg.clone();
        self.backend.start_duplicates(&cfg, ctx.clone());
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
        // Warned-root memory only means anything for folders still indexed.
        // Pruning here is what makes removing and re-adding a folder warn
        // again rather than staying silently suppressed forever.
        new.ui
            .watch_cap_warned_roots
            .retain(|root| new.paths.indexing_paths.contains(root));
        let actions = diff_actions(&self.cfg, &new);
        // A config that could not be written must not take effect either: a
        // read-only config directory would otherwise apply the settings to
        // this process, revert them on restart, and — because `is_dirty`
        // compares against `self.cfg` — show nothing unsaved in between.
        if let Err(e) = new.save() {
            self.config_error = Some(e);
            return false;
        }
        if (new.ui.scale - self.cfg.ui.scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(clamp_scale(new.ui.scale));
        }
        if new.ui.search_hotkey != self.cfg.ui.search_hotkey {
            // Re-registering is cheap but not free — on Wayland it opens a
            // new portal session, which some desktops confirm with the user —
            // so it happens only when the setting actually moved.
            crate::hotkey::apply(&new.ui.search_hotkey);
        }
        if new.ui.color_scheme != self.cfg.ui.color_scheme {
            apply_theme(ctx, &new.ui.color_scheme);
        }
        if actions.search_db_changed {
            self.backend
                .search()
                .set_db_path(new.resolved_database_path());
        }
        // Everything the index can reconcile in place — pruning rows a
        // narrowed filter put out of scope, re-deciding extracted text,
        // walking for files a widened one brought in — the coordinator does
        // on its own, in either mode and without asking. Only the three
        // settings that leave the stored file unreadable get this far.
        self.backend.coordinator.apply_config(new.clone());
        if actions.requires_rebuild {
            if self.backend.coordinator.state().mode == IndexMode::Auto {
                // Automatic mode is hands-off: wipe and start over.
                self.backend.coordinator.rebuild_index();
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
        self.cfg = new;
        true
    }

    /// What the system-wide search shortcut does once the window is up:
    /// show the Search tab with the caret in the query box and whatever was
    /// there already selected, so the next keystroke starts a new search
    /// instead of extending the last one.
    ///
    /// The tab switch goes through the same guard as a click on the tab strip
    /// rather than around it. Someone who pressed the shortcut wants to
    /// search, not to silently lose a page of unapplied index settings.
    pub(crate) fn activate_search(&mut self) {
        if switch_needs_guard(self.tab, self.manage.is_dirty(), self.pending_nav.is_some()) {
            self.pending_nav = Some(NavIntent::SwitchTab(Tab::Search));
        } else {
            self.tab = Tab::Search;
        }
        self.search.request_focus();
    }

    /// Whether the Options window is currently reading a key press to bind.
    pub(crate) fn capturing_hotkey(&self) -> bool {
        self.options.capturing_hotkey()
    }

    /// Switch the indexing mode and write it to the config immediately.
    ///
    /// The mode is a persisted setting (`indexing.auto_index`), not a
    /// per-session one: a manual stop must still be manual after a
    /// restart, or the next launch quietly resumes the indexing the user
    /// just stopped.
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
        // Streamed search results.
        while let Ok(update) = self.backend.search_rx.try_recv() {
            self.search
                .apply_update(update, self.cfg.search.display_limit);
        }
        // Duplicates worker.
        if let Some(rx) = &self.backend.dup_job {
            match rx.try_recv() {
                Ok(Ok(groups)) => {
                    self.dups.state = DupState::Loaded(groups);
                    self.backend.dup_job = None;
                }
                Ok(Err(e)) => {
                    self.dups.state = DupState::Error(e);
                    self.backend.dup_job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.dups.state = DupState::Error("duplicates scan aborted".into());
                    self.backend.dup_job = None;
                }
            }
        }
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

    /// Raise the "live updates are disabled" modal when the watcher has
    /// given up on the directory budget and at least one indexed folder has
    /// not been warned about yet.
    ///
    /// Keyed on roots rather than a single dismissed flag: a restart should
    /// stay quiet, but adding a folder changes the trade-off and deserves
    /// the warning again.
    fn check_watch_cap_warning(&mut self, state: &IndexerState) {
        let WatcherStatus::Disabled { reason } = &state.watcher else {
            // Recovered (e.g. the user trimmed the folder list) — retract a
            // modal that is no longer true.
            self.watch_cap_prompt = None;
            return;
        };
        // Only the budget limits warrant a modal. Other failures are
        // transient and not the user's to act on; they are named in the
        // status line's tooltip and logged to the Logs tab.
        if !matches!(
            reason,
            WatchError::TooManyDirectories { .. } | WatchError::KernelLimit { .. }
        ) {
            return;
        }
        if self.watch_cap_prompt.is_some() {
            return;
        }
        let unwarned = self
            .cfg
            .paths
            .indexing_paths
            .iter()
            .any(|root| !self.cfg.ui.watch_cap_warned_roots.contains(root));
        if unwarned {
            self.watch_cap_prompt = Some(reason.clone());
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let state = self.backend.coordinator.state();
        self.manage.observe(&state.activity);
        self.check_watch_cap_warning(&state);

        egui::TopBottomPanel::bottom("status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match &state.activity {
                    // A reconcile the coordinator applies between runs: no run
                    // holds the index, but it is scanning every row of it —
                    // and for a moment after, so a pass shorter than a frame
                    // still leaves a trace.
                    IndexingStatus::Idle if state.reconcile.is_some() => {
                        match state.reconcile.expect("matched Some") {
                            ReconcileState::Running(r) => {
                                ui.label(
                                    egui::RichText::new(match (r.total, r.fraction()) {
                                        (Some(total), Some(frac)) => format!(
                                            "Applying configuration change · {} / {} ({:.0}%)",
                                            group_thousands(r.examined as u64),
                                            group_thousands(total as u64),
                                            frac * 100.0
                                        ),
                                        _ => format!(
                                            "Applying configuration change · {} entries",
                                            group_thousands(r.examined as u64)
                                        ),
                                    })
                                    .small(),
                                );
                                progress_widget(ui, r.fraction());
                            }
                            ReconcileState::Finished(r) => {
                                ui.label(
                                    egui::RichText::new(crate::format::fmt_reconcile_summary(
                                        r.deleted,
                                        r.recontented,
                                    ))
                                    .small(),
                                );
                            }
                        }
                    }
                    IndexingStatus::Preparing { start_time, step } => {
                        let (label, frac) = match step {
                            PrepStep::PreviousRun => {
                                ("Finishing the previous run…".to_string(), None)
                            }
                            PrepStep::OpeningIndex => ("Opening the index…".to_string(), None),
                            PrepStep::Reconciling(r) => (
                                format!(
                                    "Applying configuration change · {} entries",
                                    group_thousands(r.examined as u64)
                                ),
                                r.fraction(),
                            ),
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                label,
                                crate::format::fmt_duration_clock(start_time.elapsed())
                            ))
                            .small(),
                        );
                        progress_widget(ui, frac);
                    }
                    IndexingStatus::Idle => {
                        let colors = palette(ui.visuals().dark_mode);
                        status_line(
                            ui,
                            &idle_line(state.mode, state.files.unwrap_or(0), &colors),
                        );
                    }
                    IndexingStatus::Error(e) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            egui::RichText::new(format!("Indexing error: {}", e)).small(),
                        );
                    }
                    IndexingStatus::Stopping => {
                        ui.label(egui::RichText::new("Stopping indexing…").small());
                    }
                    IndexingStatus::Optimizing => {
                        ui.label(egui::RichText::new("Optimizing index…").small());
                    }
                    IndexingStatus::Running { roots, .. } => {
                        let colors = palette(ui.visuals().dark_mode);
                        let rate = self.manage.speed.files_per_sec();
                        status_line(ui, &running_line(roots, rate, &colors));
                        progress_widget(ui, overall_progress(roots).fraction());
                    }
                }

                // Right corner: build id, then the search result count. In a
                // right-to-left layout the first widget added is the rightmost,
                // so the version is the fixed anchor and the count grows away
                // from it rather than shoving it around.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(crate::version::BUILD_ID).small().weak())
                        .on_hover_text(crate::version::BUILD_ID_HINT);
                    if self.tab == Tab::Search {
                        if let Some(label) = self.search.result_count_label() {
                            ui.label(egui::RichText::new("·").small().weak());
                            ui.label(egui::RichText::new(label).small().weak());
                        }
                    }
                });
            });
        });

        // Keep painting while anything is moving. The reconcile clause is not
        // redundant: the coordinator's own pass runs with the activity `Idle`,
        // so without it the counters would freeze mid-scan until the pointer
        // moved — and the summary that follows would never age off screen.
        if !matches!(
            state.activity,
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) || state.reconcile.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        // Watcher registration walks every root, so its verdict can land
        // minutes after startup. Without this the warning would wait for
        // the user to happen to move the mouse.
        if matches!(state.watcher, WatcherStatus::Starting) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn rebuild_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(changes) = &self.rebuild_prompt else {
            return;
        };
        let changes = changes.clone();
        let mut close = false;
        egui::Window::new("Rebuild index?")
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("These settings differ from what the index was built with:");
                ui.add_space(4.0);
                if changes.is_empty() {
                    ui.monospace("the index cannot be read with the new settings");
                }
                for change in &changes {
                    ui.strong(format!("{}:", change.key));
                    // Multi-line values (roots, patterns, extensions) are
                    // newline-joined — side-by-side columns keep before and
                    // after readable instead of one run-on arrow line.
                    ui.columns(2, |cols| {
                        cols[0].label(egui::RichText::new("index was built with").small().weak());
                        cols[0].monospace(display_value(&change.stored));
                        cols[1].label(egui::RichText::new("config now says").small().weak());
                        cols[1].monospace(display_value(&change.current));
                    });
                    ui.add_space(6.0);
                }
                ui.label(
                    egui::RichText::new(
                        "Unlike folders, filters and hidden files — which are applied \
                         to the existing index in place — these cannot be, so the \
                         index has to be built again. Until it is, existing entries \
                         keep the old settings.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Rebuild now").clicked() {
                        self.backend.coordinator.rebuild_index();
                        close = true;
                    }
                    if ui.button("Later").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.rebuild_prompt = None;
        }
    }

    /// Route a click in the Options window's Security block. Keychain
    /// toggles act immediately; everything else opens the two-step flow.
    fn handle_security_action(&mut self, action: SecurityAction) {
        match action {
            SecurityAction::Enable | SecurityAction::ChangePassword => {
                self.security_prompt = Some(SecurityPrompt::SetPassword {
                    pw1: String::new(),
                    pw2: String::new(),
                    remember: self.cfg.security.use_keychain,
                    change: matches!(action, SecurityAction::ChangePassword),
                });
            }
            SecurityAction::Disable => {
                self.security_prompt = Some(SecurityPrompt::ConfirmRebuild {
                    new_security: SecurityConfig::default(),
                    new_key: None,
                });
            }
            SecurityAction::SetKeychain(remember) => {
                let db_path = self.cfg.resolved_database_path();
                if remember {
                    match db::process_key_hex() {
                        Some(hex) => {
                            if let Err(e) = keychain::store_key(&db_path.to_string_lossy(), &hex) {
                                self.config_error = Some(e);
                                return; // preference not saved either
                            }
                        }
                        None => {
                            // Unreachable while protected — the gate always
                            // installs a key before the app starts.
                            self.config_error =
                                Some("no key to remember; restart and unlock first".to_string());
                            return;
                        }
                    }
                } else if let Err(e) = keychain::delete_key(&db_path.to_string_lossy()) {
                    // Mirrors the store half: the preference describes what
                    // is on the keychain, so it must not claim the key is
                    // gone while it is still there.
                    self.config_error = Some(e);
                    return;
                }
                self.cfg.security.use_keychain = remember;
                if let Err(e) = self.cfg.save() {
                    self.config_error = Some(e);
                }
            }
        }
    }

    /// Render the active security flow (drawn with the other modals).
    fn security_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.security_prompt else {
            return;
        };
        match prompt {
            SecurityPrompt::SetPassword {
                pw1,
                pw2,
                remember,
                change,
            } => {
                let title = if *change {
                    "Change password"
                } else {
                    "Enable password protection"
                };
                let mut submit = false;
                let mut cancel = false;
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_max_width(360.0);
                        ui.add(
                            egui::TextEdit::singleline(pw1)
                                .id(egui::Id::new("security-pw1"))
                                .password(true)
                                .hint_text("Password")
                                .desired_width(240.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(pw2)
                                .id(egui::Id::new("security-pw2"))
                                .password(true)
                                .hint_text("Confirm password")
                                .desired_width(240.0),
                        );
                        ui.checkbox(remember, "Remember on this device")
                            .on_hover_text(
                                "Stores the derived key (not the password) in the OS \
                             keychain and skips the startup prompt.",
                            );
                        if !pw1.is_empty() && !pw2.is_empty() && pw1 != pw2 {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                "Passwords do not match.",
                            );
                        }
                        ui.horizontal(|ui| {
                            let ok = !pw1.is_empty() && pw1 == pw2;
                            if ui.add_enabled(ok, egui::Button::new("Continue")).clicked() {
                                submit = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if cancel {
                    self.security_prompt = None; // Drop impl zeroizes
                    purge_security_field_state(ctx);
                } else if submit {
                    let password = Zeroizing::new(std::mem::take(pw1));
                    pw2.zeroize();
                    let remember = *remember;
                    purge_security_field_state(ctx);
                    let (tx, rx) = mpsc::channel();
                    let repaint = ctx.clone();
                    std::thread::spawn(move || {
                        let salt = generate_salt();
                        let key = derive_key(&password, &salt);
                        drop(password);
                        let new_security = SecurityConfig {
                            password_protected: true,
                            salt: Some(salt_to_hex(&salt)),
                            use_keychain: remember,
                        };
                        let _ = tx.send((new_security, key));
                        repaint.request_repaint();
                    });
                    self.security_prompt = Some(SecurityPrompt::Deriving { rx });
                }
            }
            SecurityPrompt::Deriving { rx } => match rx.try_recv() {
                Ok((new_security, key)) => {
                    self.security_prompt = Some(SecurityPrompt::ConfirmRebuild {
                        new_security,
                        new_key: Some(key),
                    });
                }
                Err(mpsc::TryRecvError::Empty) => {
                    egui::Window::new("Deriving key")
                        .collapsible(false)
                        .resizable(false)
                        .title_bar(false)
                        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                        .show(ctx, |ui| {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Deriving key…");
                            });
                        });
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.config_error = Some("key derivation thread died".to_string());
                    self.security_prompt = None;
                }
            },
            SecurityPrompt::ConfirmRebuild {
                new_security,
                new_key,
            } => {
                let title = match (new_key.is_some(), self.cfg.security.password_protected) {
                    (false, _) => "Disable password protection?",
                    (true, false) => "Enable password protection?",
                    (true, true) => "Change password?",
                };
                let mut confirm = false;
                let mut cancel = false;
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_max_width(420.0);
                        ui.label(
                            "Changing index encryption deletes the index and \
                             re-indexes everything. Searches return incomplete \
                             results until the rebuild finishes. Your files are \
                             not touched.",
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .button(
                                    egui::RichText::new("Delete & rebuild index")
                                        .color(ui.visuals().error_fg_color),
                                )
                                .clicked()
                            {
                                confirm = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if cancel {
                    self.security_prompt = None;
                } else if confirm {
                    let new_security = new_security.clone();
                    let new_key = new_key.clone();
                    self.security_prompt = None;
                    self.apply_security_change(new_security, new_key);
                }
            }
        }
    }

    /// Commit a confirmed security change: config, keychain, process key —
    /// in that order, before the rebuild so the fresh index is created
    /// under the new key (or none).
    fn apply_security_change(&mut self, new_security: SecurityConfig, new_key: Option<IndexKey>) {
        let db_path = self
            .cfg
            .resolved_database_path()
            .to_string_lossy()
            .into_owned();
        self.cfg.security = new_security;
        if let Err(e) = self.cfg.save() {
            self.config_error = Some(e);
        }
        match (&new_key, self.cfg.security.use_keychain) {
            (Some(key), true) => {
                if let Err(e) = keychain::store_key(&db_path, &key.to_hex()) {
                    self.config_error = Some(e);
                }
            }
            // Disabling protection, or "remember" off: no stored key may
            // survive pointing at the previous encryption state. A failure
            // here leaves one that does, which is worth saying out loud.
            _ => {
                if let Err(e) = keychain::delete_key(&db_path) {
                    self.config_error = Some(e);
                }
            }
        }
        db::set_process_key(new_key);
        self.backend.coordinator.rebuild_index();
        self.dups.state = DupState::NotLoaded;
    }
}

/// One run of status text and the color hint it carries, if any. `None` is
/// the theme's own text color, not an absence of paint.
type Span = (String, Option<egui::Color32>);

/// A status line assembled from colored spans, painted as one small widget so
/// the segments keep the exact spacing they would have had inside a single
/// label — and so the bar's widget count does not depend on how many spans a
/// state happens to need.
fn status_line(ui: &mut egui::Ui, spans: &[Span]) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let default = ui.visuals().text_color();
    let mut job = egui::text::LayoutJob::default();
    for (text, color) in spans {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: color.unwrap_or(default),
                ..Default::default()
            },
        );
    }
    ui.label(job);
}

/// The bottom bar's line for a run in progress, where only the phase word
/// carries the hint: the counters beside it are read, not glanced at.
///
/// One line covers every root, so mixed phases need a rule — and a run with
/// any root still walking is still walking, since the walk is what decides how
/// much extraction there will be. Once none is left the only work remaining is
/// extraction; a root that reached `Done` early has nothing left to contribute.
fn running_line(roots: &[RootProgress], rate: Option<f64>, colors: &Palette) -> Vec<Span> {
    let phase = if roots.iter().any(|r| r.phase == RootPhase::Walking) {
        colors.yellow
    } else {
        colors.green
    };
    let done = roots.iter().filter(|r| r.phase == RootPhase::Done).count();
    let progress = overall_progress(roots);
    let mut rest = match (progress.total, progress.fraction()) {
        (Some(total), Some(frac)) => format!(
            " {} / {} ({:.0}%)",
            group_thousands(progress.processed as u64),
            group_thousands(total as u64),
            frac * 100.0
        ),
        _ => format!(" · {} files", group_thousands(progress.processed as u64)),
    };
    if roots.len() > 1 {
        rest.push_str(&format!(" · {}/{} roots done", done, roots.len()));
    }
    if let Some(rate) = rate {
        rest.push_str(&format!(" · {}", crate::format::fmt_rate(rate)));
    }
    let active: usize = roots.iter().map(|r| r.active_workers).sum();
    let total_workers: usize = roots.iter().map(|r| r.total_workers).sum();
    if total_workers > 0 {
        rest.push_str(&format!(" · {}/{} workers", active, total_workers));
    }
    vec![("Indexing".to_string(), Some(phase)), (rest, None)]
}

/// The bottom bar's idle line. Manual mode is the one worth flagging — it
/// means the index will not refresh itself — so Auto stays unpainted: a hint
/// that is always on says nothing.
fn idle_line(mode: IndexMode, files: i64, colors: &Palette) -> Vec<Span> {
    let (mode_text, mode_color) = match mode {
        IndexMode::Auto => ("Auto", None),
        IndexMode::ManualStopped | IndexMode::ManualRunning => ("Manual", Some(colors.orange)),
    };
    vec![
        ("Idle · ".to_string(), None),
        (mode_text.to_string(), mode_color),
        (
            format!(" · {} files indexed", group_thousands(files.max(0) as u64)),
            None,
        ),
    ]
}

/// The status bar's trailing progress indicator: a bar when the work has a
/// denominator, a spinner when it does not. One helper so every kind of
/// activity the bar reports ends the same way.
fn progress_widget(ui: &mut egui::Ui, fraction: Option<f64>) {
    match fraction {
        Some(frac) => {
            ui.add(egui::ProgressBar::new(frac as f32).desired_width(120.0));
        }
        None => {
            ui.add(egui::Spinner::new().size(12.0));
        }
    }
}

/// Drop egui's retained text-field state (buffer + undo history) for the
/// password dialog fields.
fn purge_security_field_state(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw1"));
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw2"));
    });
}

impl QuickSearchApp {
    fn nested_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(pairs) = &self.nested_prompt else {
            return;
        };
        let pairs = pairs.clone();
        let mut close = false;
        egui::Window::new("Indexed folders may not be nested")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    "Each root is indexed by its own worker pool, so one root \
                     may not contain another. Fix the folder list below:",
                );
                for (child, parent) in &pairs {
                    ui.monospace(format!("{}  ⊂  {}", child, parent));
                }
                ui.label(
                    egui::RichText::new(
                        "Indexing stays paused until the overlap is removed and \
                         the list is applied.",
                    )
                    .small()
                    .weak(),
                );
                if ui.button("Fix folders").clicked() {
                    close = true;
                }
            });
        if close {
            self.nested_prompt = None;
            self.tab = Tab::Manage;
        }
    }

    /// Tell the user their index is being replaced because it belongs to an
    /// older version — rather than letting them discover it as a run of failed
    /// searches.
    ///
    /// This is the one modal that is not a question. The old index genuinely
    /// cannot be read by this build, so there is no "keep it" branch to offer;
    /// the button starts the rebuild rather than merely dismissing, which is
    /// what makes the promise true in manual mode as well as automatic.
    fn stale_index_prompt_ui(&mut self, ctx: &egui::Context) {
        if !self.stale_index_prompt {
            return;
        }
        if stale_index_window(ctx, self.key_source) {
            self.stale_index_prompt = false;
            self.backend.coordinator.rebuild_index();
            self.dups.state = DupState::NotLoaded;
        }
    }

    /// Tell the user their settings have not reached the index, and offer the
    /// one thing that fixes it.
    ///
    /// The condition is the index's own record: a reconciliation that finishes
    /// stamps it, so work still owed means a pass was abandoned — quitting
    /// during one is the ordinary way — or the config was edited while the app
    /// was closed. In automatic mode the periodic run clears it without the
    /// user doing anything, which is why the banner is a line and a button
    /// rather than a modal; in manual mode nothing happens until they ask.
    ///
    /// Held out of the way while a run or a reconcile is in progress: that is
    /// the work itself, and it can only be answered by waiting.
    fn reconcile_owed_ui(&mut self, ctx: &egui::Context) {
        if !self.reconcile_owed {
            return;
        }
        let state = self.backend.coordinator.state();
        // A completed run is the proof: it reconciles from the same record
        // and stamps it. A run the user stops does not move this, and the
        // banner correctly comes back.
        if state.last_full_index > self.reconcile_owed_since {
            self.reconcile_owed = false;
            return;
        }
        if !matches!(
            state.activity,
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) || state.reconcile.is_some()
        {
            return;
        }
        match reconcile_owed_banner(ctx) {
            None => {}
            Some(ReconcileOwedChoice::StartIndexing) => {
                self.backend.coordinator.reindex_now();
                // Not cleared here: the run that finishes clears it, and one
                // that is stopped half-way leaves the reminder standing.
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Some(ReconcileOwedChoice::Dismiss) => self.reconcile_owed = false,
        }
    }

    fn watch_cap_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(reason) = &self.watch_cap_prompt else {
            return;
        };
        let reason = reason.clone();
        let mut close = false;
        egui::Window::new("Live index updating is disabled")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                match &reason {
                    WatchError::TooManyDirectories { cap, .. } => {
                        ui.label(format!(
                            "Your indexed folders contain more than {} directories. The \
                             system limits how many folders can be watched for changes at \
                             once, so QuickSearch cannot update the index as files change.",
                            group_thousands(*cap as u64),
                        ));
                    }
                    WatchError::KernelLimit { registered } => {
                        ui.label(format!(
                            "The system ran out of folder watches after {} directories, so \
                             QuickSearch cannot update the index as files change.",
                            group_thousands(*registered as u64),
                        ));
                    }
                    WatchError::Other(msg) => {
                        ui.label(format!("Live updates are unavailable: {}", msg));
                    }
                }
                ui.add_space(4.0);
                ui.label(format!(
                    "The index is rebuilt every {} instead. Searches keep working; \
                     recent changes may take that long to appear.",
                    fmt_interval(self.cfg.indexing.reindex_interval_minutes),
                ));
                ui.label(
                    egui::RichText::new(
                        "To restore live updates, index fewer folders or exclude large \
                         subfolders under Filters on the Manage Index tab.",
                    )
                    .small()
                    .weak(),
                );
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
        if close {
            self.watch_cap_prompt = None;
            for root in &self.cfg.paths.indexing_paths {
                if !self.cfg.ui.watch_cap_warned_roots.contains(root) {
                    self.cfg.ui.watch_cap_warned_roots.push(root.clone());
                }
            }
            if let Err(e) = self.cfg.save() {
                self.config_error = Some(e);
            }
        }
    }

    fn clear_prompt_ui(&mut self, ctx: &egui::Context) {
        if !self.clear_prompt {
            return;
        }
        let mut close = false;
        egui::Window::new("Clear index?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("This deletes the search index database. Your files are not touched.");
                ui.label(
                    egui::RichText::new(
                        "Indexing switches to manual until you start it again or return                          to automatic mode.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button(
                            egui::RichText::new("Delete index")
                                .color(ui.visuals().error_fg_color),
                        )
                        .clicked()
                    {
                        // Manual first, and persisted: clearing drops the
                        // coordinator to manual so automatic mode cannot
                        // resurrect what was just deleted, and the next
                        // launch must not undo that either.
                        self.set_index_mode(false);
                        self.backend.coordinator.clear_index();
                        self.dups.state = DupState::NotLoaded;
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.clear_prompt = false;
        }
    }

    /// Drive the unsaved-changes guard. Each frame the pending intent picks
    /// the editor to ask about; Apply and Discard clean one editor and let
    /// the next frame either move to the second (Quit with both dirty asks
    /// about Options, then Manage) or fall through to the navigation
    /// itself. Cancel — button, Esc, or a backdrop click — drops the intent
    /// and stays put.
    fn unsaved_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.pending_nav else {
            return;
        };
        let dirty = (self.manage.is_dirty(), self.options.is_dirty(&self.cfg));
        let Some(source) = guard_source(intent, dirty.0, dirty.1) else {
            // Second in line, and inside the guard rather than beside it: the
            // Discard-then-quit path sets `quit_confirmed` and never returns
            // to the close-request check, so a warning that lived only there
            // would be skipped by exactly the user who dirtied an editor.
            if quit_needs_reconcile_warning(intent, self.backend.coordinator.reconciling()) {
                // Repaint on its own: a reconcile that ends while the modal is
                // up should take the modal with it.
                ctx.request_repaint_after(Duration::from_millis(250));
                match reconcile_quit_modal(ctx) {
                    None => return,
                    Some(false) => {
                        self.pending_nav = None;
                        return;
                    }
                    Some(true) => {}
                }
            }
            return self.complete_nav(ctx, intent);
        };
        match unsaved_changes_modal(ctx, source) {
            None => {}
            Some(UnsavedChoice::Cancel) => self.pending_nav = None,
            Some(UnsavedChoice::Discard) => match source {
                UnsavedSource::Manage => self.manage.discard(),
                UnsavedSource::Options => self.options.close_discard(),
            },
            Some(UnsavedChoice::Apply) => {
                let ok = match source {
                    UnsavedSource::Manage => match self.manage.take_apply_config(&self.cfg) {
                        Some(cfg) => {
                            let ok = self.apply_new_config(ctx, cfg);
                            if ok {
                                self.manage.mark_applied();
                            }
                            ok
                        }
                        None => true,
                    },
                    UnsavedSource::Options => match self.options.draft_config() {
                        Some(cfg) => {
                            let ok = self.apply_new_config(ctx, cfg);
                            if ok {
                                self.options.close_discard();
                            }
                            ok
                        }
                        None => true,
                    },
                };
                if !ok {
                    // Rejected (nested roots): stay put, keep the staged
                    // edits; the error banner explains what to fix.
                    self.pending_nav = None;
                }
            }
        }
    }

    /// Perform a navigation the guard has cleared.
    fn complete_nav(&mut self, ctx: &egui::Context, intent: NavIntent) {
        self.pending_nav = None;
        match intent {
            NavIntent::SwitchTab(tab) => {
                let was = self.tab;
                self.tab = tab;
                // Same trigger the direct switch in `update` runs; a guarded
                // switch lands here instead.
                if tab == Tab::Duplicates && was != Tab::Duplicates {
                    self.start_duplicates_scan(ctx);
                }
            }
            NavIntent::CloseOptions => self.options.close_discard(),
            NavIntent::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// State the scripted capture driver steers and waits on; see
/// [`crate::capture`].
#[cfg(feature = "capture")]
impl QuickSearchApp {
    /// Route through the same pending-nav path a click takes: `complete_nav`
    /// resolves it at the end of this frame, so the Duplicates auto-scan
    /// still fires and the unsaved-changes guard keeps its invariants
    /// (capture scenarios never dirty an editor, so the guard never prompts).
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
        self.search.capture_settled()
    }

    pub(crate) fn capture_dups_done(&self) -> bool {
        matches!(self.dups.state, DupState::Loaded(_) | DupState::Error(_))
    }

    /// Empty the query through the same edit path typing uses, so the empty
    /// search runs and the results table clears.
    pub(crate) fn capture_clear_query(&mut self) {
        self.search.query.clear();
        // Qualified rather than imported: this is the only `Instant` left in
        // this module, and it is behind `feature = "capture"`, so a plain
        // `use` would warn in every default build.
        self.search.pending_edit = Some(std::time::Instant::now());
    }

    pub(crate) fn capture_focus_search(&mut self) {
        self.search.request_focus();
    }

    pub(crate) fn capture_match_cell(&self, n: usize) -> Option<egui::Rect> {
        self.search.capture_match_cell(n)
    }
}

/// Overwrite the fields a config draft must never carry back.
///
/// Both are live state the GUI changes through their own controls — the
/// security flows in `handle_security_action`, the mode buttons in
/// [`QuickSearchApp::set_index_mode`] — and both are saved the moment they
/// change. A draft taken before one of those clicks still holds the old
/// value, so applying it would silently revert protection, the salt, or
/// the indexing mode.
pub(crate) fn pin_live_fields(new: &mut Config, live: &Config) {
    new.security = live.security.clone();
    new.indexing.auto_index = live.indexing.auto_index;
}

/// Body of the unsaved-changes guard; `Some(choice)` when the user decided
/// this frame. Esc and a click on the backdrop count as Cancel.
///
/// The one `egui::Modal` in the app, deliberately: unlike the centered
/// `egui::Window` the other prompts use, its backdrop blocks input to
/// everything behind it — this guard exists to force a decision, and a
/// click that lands on the tab strip or the Options ✕ behind the prompt
/// would re-trigger or bypass it.
///
/// A free function (not a method) so tests can render it, and click its
/// buttons, in a headless egui context.
fn unsaved_changes_modal(ctx: &egui::Context, source: UnsavedSource) -> Option<UnsavedChoice> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("unsaved-guard")).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.heading("Unsaved changes");
        ui.label(match source {
            UnsavedSource::Manage => "The Manage Index tab has edits that have not been applied.",
            UnsavedSource::Options => "The Options window has edits that have not been applied.",
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add(crate::ui_util::bordered_button(
                    "Apply & Save",
                    palette(ui.visuals().dark_mode).blue,
                ))
                .clicked()
            {
                choice = Some(UnsavedChoice::Apply);
            }
            if ui
                .button(egui::RichText::new("Discard changes").color(ui.visuals().error_fg_color))
                .clicked()
            {
                choice = Some(UnsavedChoice::Discard);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(UnsavedChoice::Cancel);
            }
        });
    });
    if choice.is_none() && modal.should_close() {
        choice = Some(UnsavedChoice::Cancel);
    }
    choice
}

/// What the user chose in the "settings not applied yet" banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReconcileOwedChoice {
    StartIndexing,
    Dismiss,
}

/// The banner's body, as a top panel under the config-error one.
///
/// A free function for the same reason as [`stale_index_window`]: a test can
/// render it and click its buttons without a coordinator behind it.
fn reconcile_owed_banner(ctx: &egui::Context) -> Option<ReconcileOwedChoice> {
    let mut choice = None;
    egui::TopBottomPanel::top("reconcile-owed").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "⚠ Your indexing settings have not been applied to the index yet.",
            );
            if ui.small_button("Start indexing now").clicked() {
                choice = Some(ReconcileOwedChoice::StartIndexing);
            }
            if ui.small_button("Dismiss").clicked() {
                choice = Some(ReconcileOwedChoice::Dismiss);
            }
        });
    });
    choice
}

/// Body of the quit-during-a-reconcile guard; `Some(true)` to quit anyway,
/// `Some(false)` to stay. Esc and a backdrop click count as staying.
///
/// The same blocking `egui::Modal` the unsaved guard uses, and for the same
/// reason: this is a decision, not a notice. Quitting is not refused — the
/// pass is cancellable and the index stays consistent either way — but the
/// consequence is invisible otherwise, since the entries the user excluded go
/// on appearing in search results until an indexing run finishes the job.
fn reconcile_quit_modal(ctx: &egui::Context) -> Option<bool> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("reconcile-quit-guard")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Settings are still being applied");
        ui.label(
            "QuickSearch is still applying your indexing settings to the index. If you \
             quit now it stops part-way, and entries you excluded can still turn up in \
             search results.",
        );
        ui.add_space(4.0);
        ui.label(
            "Nothing is lost: the next indexing run picks the work up again. In manual \
             mode, choose \"Start indexing now\" on the Manage Index tab after the next \
             launch.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button(egui::RichText::new("Quit anyway").color(ui.visuals().error_fg_color))
                .clicked()
            {
                choice = Some(true);
            }
            if ui
                .add(crate::ui_util::bordered_button(
                    "Cancel",
                    palette(ui.visuals().dark_mode).blue,
                ))
                .clicked()
            {
                choice = Some(false);
            }
        });
    });
    if choice.is_none() && modal.should_close() {
        choice = Some(false);
    }
    choice
}

/// The stale-index window's body. Returns whether the user asked for the
/// rebuild.
///
/// Split out from the method so it can be rendered, and its button clicked, in
/// a headless egui context — building a whole [`QuickSearchApp`] would start a
/// coordinator and a watcher.
fn stale_index_window(ctx: &egui::Context, key_source: KeySource) -> bool {
    let mut rebuild = false;
    egui::Window::new("Index reset for this version")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(
                "Your search index was created by an older version of QuickSearch, \
                 which this version cannot read. It is being reset and rebuilt from \
                 scratch. Don't worry, this is very Quick!",
            );
            // Only says "you just entered" when that actually happened: with
            // the key remembered on this device there was no prompt to type
            // into, and naming one the user never saw would just confuse.
            let reassurance = match key_source {
                KeySource::Unprotected => None,
                KeySource::Prompt => Some(
                    "The rebuilt index is encrypted with the password you just \
                     entered, so it stays password protected.",
                ),
                KeySource::Keychain => Some(
                    "The rebuilt index is encrypted with the key remembered on \
                     this device, so it stays password protected.",
                ),
            };
            if let Some(text) = reassurance {
                ui.add_space(4.0);
                ui.label(text);
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Your files are not touched. Searches return incomplete results \
                     until the rebuild finishes; progress is on the Manage Index tab.",
                )
                .small()
                .weak(),
            );
            ui.add_space(4.0);
            if ui.button("Rebuild now").clicked() {
                rebuild = true;
            }
        });
    rebuild
}

/// A stored/current config value for the rebuild prompt; list values are
/// already newline-joined and render as-is, empty means unset.
fn display_value(value: &str) -> String {
    if value.trim().is_empty() {
        "(none)".to_string()
    } else {
        value.to_string()
    }
}

/// Keep the configured UI scale within sane, recoverable bounds.
fn clamp_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.5, 2.5)
    } else {
        1.1
    }
}

/// What `[ui] color_scheme` means to egui.
///
/// Anything but `light` is dark, `dark` included: the setting is
/// hand-editable, and a typo should not leave the window in some third state
/// nobody chose.
///
/// The desktop's own light/dark setting is deliberately not consulted. On
/// Linux nothing in the window system reports it, so the only way to know is
/// to connect to the session message bus and subscribe to the user's settings
/// feed — more of someone's session than a search tool should be in, to decide
/// what color some text is.
pub(crate) fn theme_for(setting: &str) -> egui::Theme {
    match setting.trim().to_ascii_lowercase().as_str() {
        "light" => egui::Theme::Light,
        _ => egui::Theme::Dark,
    }
}

/// Apply the configured color scheme. Called once at startup, before the
/// unlock gate, and again whenever the setting changes; egui repaints with it
/// on the next frame, so neither needs a restart.
pub(crate) fn apply_theme(ctx: &egui::Context, setting: &str) {
    ctx.set_theme(theme_for(setting));
}

impl eframe::App for QuickSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First, so a command's effect is fully rendered before the next one
        // starts, and `previous_tab` below sees pre-navigation state.
        #[cfg(feature = "capture")]
        self.capture_tick(ctx);

        self.drain_events();
        self.tick_debounce(ctx);

        // Quitting with unapplied edits gets the same guard as tab
        // navigation. The close must be cancelled *this* frame — once the
        // window is gone there is nothing left to ask — and re-sent from
        // `complete_nav` if the user chooses to leave.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_confirmed {
            // A reconciliation in flight gets the same treatment as an unsaved
            // editor: hold the close and say what leaving now costs.
            if self.manage.is_dirty()
                || self.options.is_dirty(&self.cfg)
                || self.backend.coordinator.reconciling()
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                // Quitting subsumes any narrower pending navigation.
                self.pending_nav = Some(NavIntent::Quit);
            }
        }

        self.status_bar(ctx);

        let previous_tab = self.tab;
        // Tab clicks land on a local first, so leaving a dirty Manage tab
        // can be held for the unsaved-changes guard instead of committed.
        let mut requested = self.tab;
        egui::TopBottomPanel::top("tab-strip").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut requested, Tab::Search, "Search");
                ui.selectable_value(&mut requested, Tab::Manage, "Manage Index");
                ui.selectable_value(&mut requested, Tab::Duplicates, "Duplicates");
                ui.selectable_value(&mut requested, Tab::Logs, "Logs");
                ui.selectable_value(&mut requested, Tab::Help, "Help");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").on_hover_text("Options").clicked() {
                        if !self.options.open {
                            self.options.open_with(&self.cfg);
                        } else if self.options.is_dirty(&self.cfg) {
                            if self.pending_nav.is_none() {
                                self.pending_nav = Some(NavIntent::CloseOptions);
                            }
                        } else {
                            self.options.close_discard();
                        }
                    }
                });
            });
        });
        if requested != self.tab {
            if switch_needs_guard(self.tab, self.manage.is_dirty(), self.pending_nav.is_some()) {
                self.pending_nav = Some(NavIntent::SwitchTab(requested));
            } else {
                self.tab = requested;
            }
        }
        // Entering the Duplicates tab kicks off a fresh scan.
        if self.tab == Tab::Duplicates && previous_tab != Tab::Duplicates {
            self.start_duplicates_scan(ctx);
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
                if let Some(pattern) = actions.persist_ignore {
                    let mut new_cfg = self.cfg.clone();
                    if !new_cfg.indexing.ignore_patterns.contains(&pattern) {
                        new_cfg.indexing.ignore_patterns.push(pattern);
                        self.apply_new_config(ctx, new_cfg);
                    }
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
                    // Keep repainting while the command lands so the state
                    // change is visible without wiggling the mouse — fast
                    // runs otherwise flash by between frames.
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                if let Some(new_cfg) = actions.apply_config {
                    if self.apply_new_config(ctx, new_cfg) {
                        self.manage.mark_applied();
                    }
                }
            }
            Tab::Duplicates => {
                let actions = self.dups.ui(ui);
                if actions.refresh {
                    self.start_duplicates_scan(ctx);
                }
            }
            Tab::Logs => self.logs.ui(ui),
            Tab::Help => crate::help_tab::ui(ui),
        });

        let options_out = self.options.ui(ctx, &self.cfg);
        if let Some(new_cfg) = options_out.applied {
            self.apply_new_config(ctx, new_cfg);
        }
        if let Some(action) = options_out.security {
            self.handle_security_action(action);
        }
        if options_out.close_requested && self.pending_nav.is_none() {
            self.pending_nav = Some(NavIntent::CloseOptions);
        }
        self.rebuild_prompt_ui(ctx);
        self.security_prompt_ui(ctx);
        self.clear_prompt_ui(ctx);
        self.nested_prompt_ui(ctx);
        // Ahead of the watch-cap warning: on a fresh upgrade both can be true,
        // and "your index is being rebuilt" is the one that explains what the
        // user is actually looking at.
        self.stale_index_prompt_ui(ctx);
        self.watch_cap_prompt_ui(ctx);
        // Last: the guard must sit above everything else on screen.
        self.unsaved_prompt_ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.backend.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, source: KeySource, events: Vec<egui::Event>) -> bool {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut clicked = false;
        let _ = ctx.run(input, |ctx| clicked = stale_index_window(ctx, source));
        clicked
    }

    use crate::test_ui::click_at;

    /// The viewport every modal in this module is centred in.
    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    /// The modal the user sees after unlocking onto an index from an older
    /// version. It must render under every key source — each produces a
    /// different height — and its one button must actually report the click,
    /// since that click is what starts the rebuild. A dead button would leave
    /// the promise ("it is being rebuilt") unkept in manual mode.
    #[test]
    fn the_stale_index_modal_renders_and_its_button_fires() {
        for source in [
            KeySource::Unprotected,
            KeySource::Prompt,
            KeySource::Keychain,
        ] {
            let ctx = egui::Context::default();
            assert!(
                !frame(&ctx, source, Vec::new()),
                "an untouched frame must not request a rebuild"
            );

            // Sweep the window for the button rather than hard-coding its
            // position: it is left-aligned at the bottom of a centre-anchored
            // window whose height depends on which sentence is shown, and
            // pinning coordinates would make this a layout test.
            let mut fired = None;
            'sweep: for y in (230..480).step_by(3) {
                for x in (250..760).step_by(6) {
                    let pos = egui::pos2(x as f32, y as f32);
                    if frame(&ctx, source, click_at(pos)) {
                        fired = Some(pos);
                        break 'sweep;
                    }
                }
            }
            assert!(
                fired.is_some(),
                "no clickable Rebuild button for {source:?}"
            );
        }
    }

    #[test]
    fn a_stale_draft_cannot_revert_the_indexing_mode_or_security() {
        // The draft as it was when the editor last synced: automatic
        // indexing, no password — plus one real edit the user staged.
        let mut draft = Config::default();
        draft.indexing.auto_index = true;
        draft.indexing.reindex_interval_minutes = 60;

        // Since then: Stop was clicked and protection was enabled.
        let mut live = Config::default();
        live.indexing.auto_index = false;
        live.security = SecurityConfig {
            password_protected: true,
            salt: Some("ab".repeat(16)),
            use_keychain: true,
        };

        pin_live_fields(&mut draft, &live);
        assert!(
            !draft.indexing.auto_index,
            "applying the draft must not restart automatic indexing"
        );
        assert_eq!(draft.security, live.security);
        assert_eq!(
            draft.indexing.reindex_interval_minutes, 60,
            "the staged edit itself still applies"
        );
    }

    /// The whole guard decision table. Quit walks Options before Manage —
    /// two sequential prompts, because each draft is a full `Config`
    /// snapshot and applying both in one step would let the second revert
    /// the first.
    /// The system-wide search shortcut leaves a tab the same way a click on
    /// the tab strip does. Someone who pressed it wants to search, which is
    /// not a reason to throw away a page of staged index settings.
    #[test]
    fn leaving_a_dirty_manage_tab_is_guarded_however_it_is_asked_for() {
        assert!(switch_needs_guard(Tab::Manage, true, false));
        assert!(
            !switch_needs_guard(Tab::Manage, false, false),
            "a clean editor has nothing to ask about"
        );
        assert!(
            !switch_needs_guard(Tab::Manage, true, true),
            "one held navigation at a time"
        );
        for tab in [Tab::Search, Tab::Duplicates, Tab::Logs, Tab::Help] {
            assert!(
                !switch_needs_guard(tab, true, false),
                "{tab:?} holds no unapplied edits of its own"
            );
        }
    }

    #[test]
    fn guard_source_orders_quit_prompts_options_first() {
        use NavIntent::*;
        let tab = SwitchTab(Tab::Search);

        assert_eq!(guard_source(tab, true, true), Some(UnsavedSource::Manage));
        assert_eq!(guard_source(tab, true, false), Some(UnsavedSource::Manage));
        assert_eq!(
            guard_source(tab, false, true),
            None,
            "options guard its own close"
        );
        assert_eq!(guard_source(tab, false, false), None);

        assert_eq!(
            guard_source(CloseOptions, true, true),
            Some(UnsavedSource::Options)
        );
        assert_eq!(
            guard_source(CloseOptions, false, true),
            Some(UnsavedSource::Options)
        );
        assert_eq!(
            guard_source(CloseOptions, true, false),
            None,
            "manage guards tab switches"
        );
        assert_eq!(guard_source(CloseOptions, false, false), None);

        assert_eq!(guard_source(Quit, true, true), Some(UnsavedSource::Options));
        assert_eq!(
            guard_source(Quit, false, true),
            Some(UnsavedSource::Options)
        );
        assert_eq!(guard_source(Quit, true, false), Some(UnsavedSource::Manage));
        assert_eq!(guard_source(Quit, false, false), None);
    }

    fn modal_frame(
        ctx: &egui::Context,
        source: UnsavedSource,
        events: Vec<egui::Event>,
    ) -> Option<UnsavedChoice> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = unsaved_changes_modal(ctx, source));
        choice
    }

    /// Every way out of the guard reports the right choice: all three
    /// buttons fire, Esc cancels, and an untouched frame decides nothing.
    /// A backdrop click also maps to Cancel — that is `should_close`'s
    /// contract — so the sweep counts button hits by their distinct values.
    #[test]
    fn the_unsaved_modal_reports_each_choice() {
        for source in [UnsavedSource::Manage, UnsavedSource::Options] {
            let ctx = egui::Context::default();
            assert_eq!(
                modal_frame(&ctx, source, Vec::new()),
                None,
                "an untouched frame must not decide"
            );

            let mut seen = std::collections::HashSet::new();
            for y in (250..450).step_by(3) {
                for x in (250..760).step_by(6) {
                    let pos = egui::pos2(x as f32, y as f32);
                    if let Some(choice) = modal_frame(&ctx, source, click_at(pos)) {
                        seen.insert(choice);
                    }
                }
            }
            for expected in [
                UnsavedChoice::Apply,
                UnsavedChoice::Discard,
                UnsavedChoice::Cancel,
            ] {
                assert!(
                    seen.contains(&expected),
                    "{expected:?} never fired ({source:?})"
                );
            }

            let esc = modal_frame(
                &ctx,
                source,
                vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(esc, Some(UnsavedChoice::Cancel), "Esc must cancel");
        }
    }

    /// Quitting mid-reconcile is the one case that needs saying out loud: the
    /// index is left describing settings the user has already changed, and in
    /// manual mode nothing fixes that until they ask for a run. Nothing else
    /// warrants the prompt — a tab switch does not end the pass, and a quit
    /// with no pass running has nothing to warn about.
    #[test]
    fn only_quitting_during_a_reconcile_warns() {
        use NavIntent::*;
        assert!(quit_needs_reconcile_warning(Quit, true));
        assert!(!quit_needs_reconcile_warning(Quit, false));
        assert!(!quit_needs_reconcile_warning(SwitchTab(Tab::Search), true));
        assert!(!quit_needs_reconcile_warning(CloseOptions, true));
    }

    fn reconcile_modal_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Option<bool> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = reconcile_quit_modal(ctx));
        choice
    }

    /// Both ways out of the quit warning work, and neither is the default: a
    /// modal whose "Quit anyway" did nothing would trap the user in an app
    /// they asked to close, and one that quit on Esc would make the warning
    /// pointless.
    #[test]
    fn the_quit_warning_reports_both_answers() {
        let ctx = egui::Context::default();
        assert_eq!(
            reconcile_modal_frame(&ctx, Vec::new()),
            None,
            "an untouched frame must not decide"
        );

        let mut seen = std::collections::HashSet::new();
        for y in (250..450).step_by(3) {
            for x in (250..760).step_by(6) {
                if let Some(choice) =
                    reconcile_modal_frame(&ctx, click_at(egui::pos2(x as f32, y as f32)))
                {
                    seen.insert(choice);
                }
            }
        }
        assert!(seen.contains(&true), "\"Quit anyway\" never fired");
        assert!(seen.contains(&false), "Cancel never fired");

        let esc = reconcile_modal_frame(
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(esc, Some(false), "Esc must keep the app open");
    }

    fn banner_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Option<ReconcileOwedChoice> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = reconcile_owed_banner(ctx));
        choice
    }

    /// The reminder a quit mid-prune leaves behind. Its Start button is the
    /// whole point — the banner exists because in manual mode nothing else
    /// will finish the work — and Dismiss must not be the only live control.
    #[test]
    fn the_reconcile_banner_reports_both_buttons() {
        let ctx = egui::Context::default();
        assert_eq!(banner_frame(&ctx, Vec::new()), None);

        let mut seen = std::collections::HashSet::new();
        // A top panel, so it sits in the first rows of the window.
        for y in (0..60).step_by(2) {
            for x in (0..1000).step_by(4) {
                if let Some(choice) = banner_frame(&ctx, click_at(egui::pos2(x as f32, y as f32))) {
                    seen.insert(choice);
                }
            }
        }
        assert!(
            seen.contains(&ReconcileOwedChoice::StartIndexing),
            "\"Start indexing now\" never fired"
        );
        assert!(
            seen.contains(&ReconcileOwedChoice::Dismiss),
            "Dismiss never fired"
        );
    }

    fn root(phase: RootPhase, walked: usize, walk_total: Option<usize>) -> RootProgress {
        RootProgress {
            root: "/data".to_string(),
            phase,
            walked,
            walk_total,
            extracted: 0,
            extract_total: 0,
            current_file: None,
            active_workers: 2,
            total_workers: 4,
        }
    }

    /// What the user reads: the spans are a presentation detail, the sentence
    /// they spell is not.
    fn line(spans: &[Span]) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    /// Splitting the line to color its first word must not move a character
    /// of it — the spacing around the phase word comes from the text itself,
    /// not from egui's item spacing.
    #[test]
    fn the_running_line_reads_as_one_sentence() {
        let colors = palette(true);

        assert_eq!(
            line(&running_line(
                &[root(RootPhase::Walking, 100, Some(1000))],
                None,
                &colors
            )),
            "Indexing 100 / 1,000 (10%) · 2/4 workers"
        );

        // No count has landed yet: no denominator is invented for it.
        assert_eq!(
            line(&running_line(
                &[root(RootPhase::Walking, 100, None)],
                None,
                &colors
            )),
            "Indexing · 100 files · 2/4 workers"
        );

        let mut extracting = root(RootPhase::Extracting, 1_000, None);
        extracting.extracted = 200;
        extracting.extract_total = 800;
        extracting.active_workers = 3;
        let mut done = root(RootPhase::Done, 500, None);
        done.extracted = 500;
        done.extract_total = 500;
        done.active_workers = 0;
        done.total_workers = 0;
        assert_eq!(
            line(&running_line(&[extracting, done], Some(120.0), &colors)),
            "Indexing 2,200 / 2,800 (79%) · 1/2 roots done · 120 files/s · 3/4 workers"
        );
    }

    /// The hint is on the phase word alone: coloring the counters too would
    /// make the moving numbers the loudest thing in the window.
    #[test]
    fn only_the_phase_word_of_the_running_line_is_hinted() {
        for dark in [true, false] {
            let colors = palette(dark);
            let spans = running_line(&[root(RootPhase::Walking, 100, Some(1000))], None, &colors);
            assert_eq!(spans[0].0, "Indexing");
            assert_eq!(spans[0].1, Some(colors.yellow), "dark_mode={}", dark);
            assert!(
                spans[1..].iter().all(|(_, color)| color.is_none()),
                "the counters carry a hint: {:?}",
                spans
            );
        }
    }

    /// A run with any root still walking is still walking: the walk is what
    /// decides how much extraction there will be, so it owns the hint until
    /// the last one ends.
    #[test]
    fn the_running_hint_follows_the_least_advanced_root() {
        let colors = palette(true);
        let hint = |roots: &[RootProgress]| running_line(roots, None, &colors)[0].1;

        assert_eq!(
            hint(&[
                root(RootPhase::Extracting, 100, None),
                root(RootPhase::Walking, 100, Some(1000)),
                root(RootPhase::Done, 100, None),
            ]),
            Some(colors.yellow)
        );
        assert_eq!(
            hint(&[
                root(RootPhase::Extracting, 100, None),
                root(RootPhase::Done, 100, None),
            ]),
            Some(colors.green)
        );
        // Every root finished, but the run has not torn itself down yet.
        assert_eq!(
            hint(&[root(RootPhase::Done, 100, None)]),
            Some(colors.green)
        );
    }

    /// Manual mode is the one idle state worth flagging: it means nothing
    /// will refresh the index until the user says so. Auto is the expected
    /// state, and a hint that is always on says nothing.
    #[test]
    fn only_manual_idle_is_hinted() {
        for dark in [true, false] {
            let colors = palette(dark);

            let auto = idle_line(IndexMode::Auto, 12_000, &colors);
            assert_eq!(line(&auto), "Idle · Auto · 12,000 files indexed");
            assert!(
                auto.iter().all(|(_, color)| color.is_none()),
                "automatic mode is not a warning: {:?}",
                auto
            );

            for mode in [IndexMode::ManualStopped, IndexMode::ManualRunning] {
                let spans = idle_line(mode, 12_000, &colors);
                assert_eq!(line(&spans), "Idle · Manual · 12,000 files indexed");
                let hinted: Vec<_> = spans.iter().filter(|(_, c)| c.is_some()).collect();
                assert_eq!(
                    hinted,
                    vec![&("Manual".to_string(), Some(colors.orange))],
                    "{:?} in dark_mode={}",
                    mode,
                    dark
                );
            }
        }
    }

    /// A count read back as negative is a bug, not something to print.
    #[test]
    fn a_negative_file_count_reads_as_zero() {
        let colors = palette(true);
        assert_eq!(
            line(&idle_line(IndexMode::Auto, -1, &colors)),
            "Idle · Auto · 0 files indexed"
        );
    }

    /// The two values the Options window writes, plus everything a
    /// hand-edited config might hold instead.
    #[test]
    fn only_light_is_light() {
        assert_eq!(theme_for("light"), egui::Theme::Light);
        assert_eq!(theme_for("dark"), egui::Theme::Dark);

        // Spelled the user's way, not the config's.
        assert_eq!(theme_for("  LIGHT  "), egui::Theme::Light);
        assert_eq!(theme_for("Dark"), egui::Theme::Dark);

        // A typo costs the preference, not the config file.
        for nonsense in ["", "   ", "lite", "system", "auto", "true"] {
            assert_eq!(
                theme_for(nonsense),
                egui::Theme::Dark,
                "{:?} should be dark",
                nonsense
            );
        }
    }
}
