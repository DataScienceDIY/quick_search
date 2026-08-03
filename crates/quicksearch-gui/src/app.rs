//! Application shell: tab strip, per-frame event drains, debounce,
//! status bar, and config-change routing.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use quicksearch_core::cli::{index_counts, IndexCounts};
use quicksearch_core::config::{diff_actions, nested_roots, Config, SecurityConfig};
use quicksearch_core::coordinator::{IndexMode, IndexerState, WatcherStatus};
use quicksearch_core::db;
use quicksearch_core::indexing::{ConfigChange, IndexingStatus, RootPhase};
use quicksearch_core::search::SearchOptions;
use quicksearch_core::security::{derive_key, generate_salt, salt_to_hex, IndexKey};
use quicksearch_core::watcher::WatchError;
use zeroize::{Zeroize, Zeroizing};

use crate::backend::Backend;
use crate::duplicates_tab::{DupState, DuplicatesTab};
use crate::format::{fmt_interval, group_thousands};
use crate::keychain;
use crate::logs_tab::LogsTab;
use crate::manage_tab::ManageTab;
use crate::options::{OptionsWindow, SecurityAction};
use crate::search_tab::SearchTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Search,
    Manage,
    Duplicates,
    Logs,
    Help,
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
    /// Cached idle counts for the status bar, refreshed at most every 5 s.
    counts: Option<(Instant, IndexCounts)>,
    /// Set when applying a config that invalidates the stored index.
    rebuild_prompt: Option<Vec<ConfigChange>>,
    /// Set while the "delete the index?" confirmation is open.
    clear_prompt: bool,
    /// Nested roots found in the loaded config (startup validation); shown
    /// as a modal over the Manage tab until dismissed.
    nested_prompt: Option<Vec<(String, String)>>,
    /// Set when the watcher gave up on the directory budget and live
    /// updates are off; see [`QuickSearchApp::check_watch_cap_warning`].
    watch_cap_prompt: Option<WatchError>,
    /// In-flight security flow (enable/disable/change password), driven by
    /// the Options window's Security block.
    security_prompt: Option<SecurityPrompt>,
    config_error: Option<String>,
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
    ) -> Result<QuickSearchApp, String> {
        // Compact styling: results density is the whole point.
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(6.0, 3.0);
            style.spacing.button_padding = egui::vec2(6.0, 2.0);
        });
        ctx.set_zoom_factor(clamp_scale(cfg.ui.scale));

        let backend = Backend::start(&cfg, ctx.clone())?;
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
            counts: None,
            rebuild_prompt: None,
            clear_prompt: false,
            nested_prompt,
            watch_cap_prompt: None,
            security_prompt: None,
            config_error,
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

    /// Save + route an edited config to the running services.
    fn apply_new_config(&mut self, ctx: &egui::Context, mut new: Config) {
        pin_live_fields(&mut new, &self.cfg);
        if let Some((child, parent)) = nested_roots(&new.paths.indexing_paths).first() {
            self.config_error = Some(format!(
                "Not applied: indexed folder {} is nested under {}",
                child, parent
            ));
            return;
        }
        // Warned-root memory only means anything for folders still indexed.
        // Pruning here is what makes removing and re-adding a folder warn
        // again rather than staying silently suppressed forever.
        new.ui
            .watch_cap_warned_roots
            .retain(|root| new.paths.indexing_paths.contains(root));
        let actions = diff_actions(&self.cfg, &new);
        if let Err(e) = new.save() {
            self.config_error = Some(e);
        }
        if (new.ui.scale - self.cfg.ui.scale).abs() > f32::EPSILON {
            ctx.set_zoom_factor(clamp_scale(new.ui.scale));
        }
        if actions.search_db_changed {
            self.backend
                .search()
                .set_db_path(new.resolved_database_path());
            self.counts = None;
        }
        self.backend.coordinator.apply_config(new.clone());
        if actions.requires_rebuild {
            if self.backend.coordinator.state().mode == IndexMode::Auto {
                // Automatic mode is hands-off: reconcile immediately, no
                // prompt. Root-only changes need just a full run — the
                // walk indexes new roots and the stale sweep drops removed
                // ones. Anything else (tokenizer, hashing, filters, hidden
                // files) invalidates stored data and gets the real wipe.
                let roots_only = {
                    let mut probe = new.clone();
                    probe.paths.indexing_paths = self.cfg.paths.indexing_paths.clone();
                    !diff_actions(&self.cfg, &probe).requires_rebuild
                };
                if roots_only {
                    self.backend.coordinator.reindex_now();
                } else {
                    self.backend.coordinator.rebuild_index();
                }
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
        loop {
            match self.backend.search_rx.try_recv() {
                Ok(update) => self
                    .search
                    .apply_update(update, self.cfg.search.display_limit),
                Err(_) => break,
            }
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
                    IndexingStatus::Idle => {
                        let mode = match state.mode {
                            IndexMode::Auto => "Auto",
                            IndexMode::ManualStopped => "Manual",
                            IndexMode::ManualRunning => "Manual",
                        };
                        let stale = self
                            .counts
                            .map(|(at, _)| at.elapsed() > Duration::from_secs(5))
                            .unwrap_or(true);
                        if stale {
                            let db = self.cfg.resolved_database_path();
                            let counts =
                                index_counts(&db.to_string_lossy()).unwrap_or(IndexCounts {
                                    files: 0,
                                    content_done: 0,
                                    content_pending: 0,
                                });
                            self.counts = Some((Instant::now(), counts));
                        }
                        let files = self.counts.map(|(_, c)| c.files).unwrap_or(0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Idle · {} · {} files indexed",
                                mode,
                                group_thousands(files.max(0) as u64)
                            ))
                            .small(),
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
                    IndexingStatus::Running { roots, .. } => {
                        let done = roots.iter().filter(|r| r.phase == RootPhase::Done).count();
                        let processed: usize = roots.iter().map(|r| r.walked + r.extracted).sum();
                        let totals_known = roots.iter().all(|r| r.walk_total.is_some());
                        let denominator: usize = roots
                            .iter()
                            .map(|r| r.walk_total.unwrap_or(0) + r.extract_total)
                            .sum();

                        let mut text = if totals_known && denominator > 0 {
                            let frac = (processed as f64 / denominator as f64).min(1.0);
                            format!(
                                "Indexing {} / {} ({:.0}%)",
                                group_thousands(processed as u64),
                                group_thousands(denominator as u64),
                                frac * 100.0
                            )
                        } else {
                            format!("Indexing · {} files", group_thousands(processed as u64))
                        };
                        if roots.len() > 1 {
                            text.push_str(&format!(" · {}/{} roots done", done, roots.len()));
                        }
                        if let Some(rate) = self.manage.speed.files_per_sec() {
                            text.push_str(&format!(" · {}", crate::format::fmt_rate(rate)));
                        }
                        let active: usize = roots.iter().map(|r| r.active_workers).sum();
                        let total_workers: usize = roots.iter().map(|r| r.total_workers).sum();
                        if total_workers > 0 {
                            text.push_str(&format!(" · {}/{} workers", active, total_workers));
                        }
                        ui.label(egui::RichText::new(text).small());
                        if totals_known && denominator > 0 {
                            let frac = (processed as f32 / denominator as f32).clamp(0.0, 1.0);
                            ui.add(egui::ProgressBar::new(frac).desired_width(120.0));
                        } else {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                    }
                }

                // Right corner: search result count.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.tab == Tab::Search {
                        if let Some(label) = self.search.result_count_label() {
                            ui.label(egui::RichText::new(label).small().weak());
                        }
                    }
                });
            });
        });

        // Keep painting while anything is moving.
        if !matches!(
            state.activity,
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) {
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
                    ui.monospace("indexing settings changed");
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
                        "A full rebuild applies them everywhere. Until then, existing \
                         entries keep the old settings.",
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
                } else {
                    keychain::delete_key(&db_path.to_string_lossy());
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
            // survive pointing at the previous encryption state.
            _ => keychain::delete_key(&db_path),
        }
        db::set_process_key(new_key);
        self.backend.coordinator.rebuild_index();
        self.counts = None;
        self.dups.state = DupState::NotLoaded;
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
                        self.counts = None;
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
}

/// Overwrite the fields a config draft must never carry back.
///
/// Both are live state the GUI changes through their own controls — the
/// security flows in `handle_security_action`, the mode buttons in
/// [`QuickSearchApp::set_index_mode`] — and both are saved the moment they
/// change. A draft taken before one of those clicks still holds the old
/// value, so applying it would silently revert protection, the salt, or
/// the indexing mode.
fn pin_live_fields(new: &mut Config, live: &Config) {
    new.security = live.security.clone();
    new.indexing.auto_index = live.indexing.auto_index;
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

impl eframe::App for QuickSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.tick_debounce(ctx);
        self.status_bar(ctx);

        let previous_tab = self.tab;
        egui::TopBottomPanel::top("tab-strip").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Search, "Search");
                ui.selectable_value(&mut self.tab, Tab::Manage, "Manage Index");
                ui.selectable_value(&mut self.tab, Tab::Duplicates, "Duplicates");
                ui.selectable_value(&mut self.tab, Tab::Logs, "Logs");
                ui.selectable_value(&mut self.tab, Tab::Help, "Help");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").on_hover_text("Options").clicked() {
                        if self.options.open {
                            self.options.open = false;
                        } else {
                            self.options.open_with(&self.cfg);
                        }
                    }
                });
            });
        });
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
                    self.apply_new_config(ctx, new_cfg);
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
        self.rebuild_prompt_ui(ctx);
        self.security_prompt_ui(ctx);
        self.clear_prompt_ui(ctx);
        self.nested_prompt_ui(ctx);
        self.watch_cap_prompt_ui(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.backend.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
