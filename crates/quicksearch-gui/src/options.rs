//! The Options window and the shared config editor used by both the
//! window and the Manage Index tab. Edits happen on a draft; Apply
//! validates, saves, and hands the new config to the app.

use crate::keychain;
use quicksearch_core::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Indexing,
    Processing,
    Search,
}

/// A click in the Security block. Unlike the rest of the Options window
/// these are not draft edits: passwords are not config fields, and every
/// action here runs its own explicit flow (with a rebuild warning where
/// one is required) in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityAction {
    Enable,
    Disable,
    ChangePassword,
    SetKeychain(bool),
}

/// What one frame of the Options window produced.
#[derive(Default)]
pub struct OptionsOutput {
    /// "Apply & Save" was clicked with this draft.
    pub applied: Option<Config>,
    /// A Security block action was clicked.
    pub security: Option<SecurityAction>,
    /// The title-bar close was clicked while the draft holds unapplied
    /// edits. The window is held open; the app raises the unsaved-changes
    /// guard.
    pub close_requested: bool,
}

pub struct OptionsWindow {
    pub open: bool,
    draft: Option<Config>,
    /// Cached answer from [`OptionsWindow::keychain_active`], with the
    /// `use_keychain` preference it was probed under.
    keychain_probed_for: Option<bool>,
    keychain_active: bool,
}

impl OptionsWindow {
    pub fn new() -> OptionsWindow {
        OptionsWindow {
            open: false,
            draft: None,
            keychain_probed_for: None,
            keychain_active: false,
        }
    }

    pub fn open_with(&mut self, current: &Config) {
        self.open = true;
        self.draft = Some(current.clone());
        self.keychain_probed_for = None;
    }

    /// Whether the draft differs from the live config. The fields the app
    /// pins on apply (`security`, `indexing.auto_index`) are neutralized
    /// first — the Security block acts on the live config directly and must
    /// not make the window read as dirty.
    pub fn is_dirty(&self, current: &Config) -> bool {
        let Some(draft) = &self.draft else {
            return false;
        };
        let mut d = draft.clone();
        crate::app::pin_live_fields(&mut d, current);
        d != *current
    }

    /// The draft as it stands, for the app's unsaved-changes guard.
    pub fn draft_config(&self) -> Option<Config> {
        self.draft.clone()
    }

    /// Close and drop the draft (Discard, or a clean close).
    pub fn close_discard(&mut self) {
        self.open = false;
        self.draft = None;
    }

    /// Adopt the window's open flag for this frame. A dirty close is
    /// intercepted: the window is held open and the caller is told to raise
    /// the unsaved-changes guard instead.
    fn intercept_close(&mut self, still_open: bool, current: &Config) -> bool {
        self.open = still_open;
        if self.open {
            return false;
        }
        if self.is_dirty(current) {
            self.open = true;
            true
        } else {
            self.draft = None;
            false
        }
    }

    /// True when this index's key really is in the OS keychain: the
    /// preference is on *and* the keychain answers with an entry (a dead
    /// daemon, a locked keyring or a denied prompt all read as "no", which
    /// is exactly when the startup prompt still appears). Probed when the
    /// window opens and whenever the preference changes — a keychain read
    /// is an IPC round trip, far too costly to repeat every frame.
    fn keychain_active(&mut self, current: &Config) -> bool {
        if self.keychain_probed_for != Some(current.security.use_keychain) {
            let db_path = current.resolved_database_path();
            self.keychain_active = current.security.use_keychain
                && matches!(keychain::load_key(&db_path.to_string_lossy()), Ok(Some(_)));
            self.keychain_probed_for = Some(current.security.use_keychain);
        }
        self.keychain_active
    }

    /// Render; reports an applied draft config and/or a security action.
    pub fn ui(&mut self, ctx: &egui::Context, current: &Config) -> OptionsOutput {
        if !self.open {
            self.draft = None;
            return OptionsOutput::default();
        }
        if self.draft.is_none() {
            self.draft = Some(current.clone());
        }
        let mut out = OptionsOutput::default();
        let mut open = self.open;
        let keychain_active = self.keychain_active(current);
        let dirty = self.is_dirty(current);
        let draft = self.draft.as_mut().unwrap();

        egui::Window::new("Options")
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                let scroll = egui::ScrollArea::vertical()
                    .max_height(480.0)
                    .show(ui, |ui| {
                        ui.heading(egui::RichText::new("Paths").strong());
                        egui::Grid::new("opt-paths").num_columns(2).show(ui, |ui| {
                            ui.label("Database file");
                            ui.add(
                                egui::TextEdit::singleline(&mut draft.paths.database_path)
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                        });
                        ui.label(
                            egui::RichText::new(
                                "Indexed folders are managed on the Manage Index tab.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.separator();

                        ui.heading(egui::RichText::new("Indexing").strong());
                        config_editor_ui(ui, draft, Section::Indexing);
                        ui.label(
                            egui::RichText::new(
                                "Automatic and manual indexing are switched on the \
                             Manage Index tab.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.separator();

                        ui.heading(egui::RichText::new("Processing").strong());
                        config_editor_ui(ui, draft, Section::Processing);
                        ui.separator();

                        ui.heading(egui::RichText::new("Search").strong());
                        config_editor_ui(ui, draft, Section::Search);
                        ui.separator();

                        ui.heading(egui::RichText::new("Interface").strong());
                        egui::Grid::new("opt-ui").num_columns(2).show(ui, |ui| {
                            ui.label("UI scale");
                            ui.add(
                                egui::Slider::new(&mut draft.ui.scale, 0.5..=2.5)
                                    .step_by(0.05)
                                    .fixed_decimals(2),
                            )
                            .on_hover_text(
                                "Zooms the whole interface: fonts, spacing, and \
                             widgets. Ctrl +/- and Ctrl 0 adjust it temporarily \
                             at runtime.",
                            );
                            ui.end_row();
                        });
                        ui.separator();

                        // Security acts on the live config, not the draft: each
                        // action opens its own confirmation flow immediately.
                        // The KDF salt is deliberately never shown here (or
                        // anywhere else in the GUI).
                        ui.heading(egui::RichText::new("Security").strong());
                        out.security = security_ui(ui, current, keychain_active);
                    });
                crate::ui_util::more_below_hint(ui, &scroll);

                ui.separator();
                ui.horizontal(|ui| {
                    let apply = ui.add(crate::ui_util::bordered_button(
                        "Apply & Save",
                        if dirty {
                            crate::ui_util::ORANGE
                        } else {
                            crate::ui_util::BLUE
                        },
                    ));
                    if apply.clicked() {
                        out.applied = Some(draft.clone());
                    }
                    // Comes and goes with the dirty state; keep it off the
                    // ids of the hint that follows.
                    crate::ui_util::stable_section(ui, |ui| {
                        if dirty {
                            ui.label(
                                egui::RichText::new("Unsaved changes")
                                    .small()
                                    .color(crate::ui_util::ORANGE),
                            );
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "Changes to tokenizer, filters, hidden files, or hashing \
                         prompt an index rebuild.",
                    )
                    .small()
                    .weak(),
                );
            });

        out.close_requested = self.intercept_close(open, current);
        out
    }
}

/// The Security block: status plus action buttons. Never renders the salt.
fn security_ui(
    ui: &mut egui::Ui,
    current: &Config,
    keychain_active: bool,
) -> Option<SecurityAction> {
    let mut action = None;
    if current.security.password_protected {
        if keychain_active {
            ui.label(
                "The index is encrypted; its password is securely stored by \
                 your Operating System.",
            );
        } else {
            ui.label("The index is encrypted; a password is required at startup.");
        }
        ui.horizontal(|ui| {
            if ui.button("Change password…").clicked() {
                action = Some(SecurityAction::ChangePassword);
            }
            if ui.button("Disable protection…").clicked() {
                action = Some(SecurityAction::Disable);
            }
        });
        let mut remember = current.security.use_keychain;
        if ui
            .checkbox(&mut remember, "Remember on this device")
            .on_hover_text(
                "Stores the derived key (not the password) in the OS keychain \
                 and skips the startup prompt on this machine.",
            )
            .changed()
        {
            action = Some(SecurityAction::SetKeychain(remember));
        }
    } else {
        ui.label("The index is not encrypted.");
        if ui.button("Enable password protection…").clicked() {
            action = Some(SecurityAction::Enable);
        }
        ui.label(
            egui::RichText::new(
                "The index stores the names and text of your files. A password \
                 encrypts it on disk; enabling one rebuilds the index.",
            )
            .small()
            .weak(),
        );
    }
    action
}

/// One implementation of the per-section config controls, shared by the
/// Options window and the Manage tab.
pub fn config_editor_ui(ui: &mut egui::Ui, config: &mut Config, section: Section) {
    match section {
        Section::Indexing => {
            egui::Grid::new("cfg-indexing")
                .num_columns(2)
                .show(ui, |ui| {
                    // Automatic vs manual is deliberately absent: it is live
                    // state, switched (and saved) by the Stop / Return to
                    // Automatic buttons on the Manage Index tab. A staged copy
                    // of it here would fight those buttons.
                    ui.label("Full reindex every");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut config.indexing.reindex_interval_minutes)
                                .range(5..=60 * 24 * 30),
                        );
                        ui.label("minutes");
                    });
                    ui.end_row();

                    ui.label("Follow symlinks");
                    ui.checkbox(&mut config.indexing.follow_symlinks, "");
                    ui.end_row();

                    ui.label("Include hidden files");
                    ui.checkbox(&mut config.indexing.include_hidden, "");
                    ui.end_row();
                });
        }
        Section::Processing => {
            egui::Grid::new("cfg-processing")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Tokenizer");
                    egui::ComboBox::from_id_salt("cfg-tokenize")
                        .selected_text(&config.processing.tokenize)
                        .show_ui(ui, |ui| {
                            for opt in ["trigram", "unicode61", "porter"] {
                                ui.selectable_value(
                                    &mut config.processing.tokenize,
                                    opt.to_string(),
                                    opt,
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("");
                    ui.hyperlink_to(
                        "Tokenizer documentation",
                        "https://www.sqlite.org/fts5.html#tokenizers",
                    );
                    ui.end_row();

                    ui.label("Hash sample size (bytes)");
                    ui.add(
                        egui::DragValue::new(&mut config.processing.hash_length)
                            .range(512..=1_048_576),
                    );
                    ui.end_row();

                    ui.label("Max stored text (bytes)");
                    ui.add(
                        egui::DragValue::new(&mut config.processing.maximum_text_size)
                            .range(1024..=16_777_216),
                    );
                    ui.end_row();

                    ui.label("Max text file size (bytes)");
                    ui.add(
                        egui::DragValue::new(&mut config.processing.maximum_text_file_size)
                            .range(1024..=1_073_741_824),
                    );
                    ui.end_row();

                    ui.label("Batch size");
                    ui.add(
                        egui::DragValue::new(&mut config.processing.batch_size).range(10..=100_000),
                    );
                    ui.end_row();

                    ui.label("Max WAL size (bytes)");
                    ui.add(
                        egui::DragValue::new(&mut config.processing.maximum_wal_size)
                            .range(0u64..=8_589_934_592u64),
                    )
                    .on_hover_text(
                        "How large index.sqlite-wal may grow during a run before the \
                     indexer forces a checkpoint. 0 disables forced checkpoints; \
                     anything below 16 MiB is raised to it.",
                    );
                    ui.end_row();

                    ui.label("Store text for snippets");
                    ui.checkbox(&mut config.processing.store_text_for_snippets, "")
                        .on_hover_text(
                            "Off: smaller index, but no previews, occurrence ranking, \
                         case verification, or fuzzy full-text search",
                        );
                    ui.end_row();
                });
        }
        Section::Search => {
            egui::Grid::new("cfg-search").num_columns(2).show(ui, |ui| {
                ui.label("Fuzzy search ON by default");
                ui.checkbox(&mut config.search.fuzzy_default, "");
                ui.end_row();

                ui.label("Fuzzy edit distance");
                ui.vertical(|ui| {
                    ui.add(egui::DragValue::new(&mut config.search.fuzzy_max_edits).range(0..=8))
                        .on_hover_text(
                            "Ceiling on the typo budget. The allowance grows with the \
                             search term, one edit per three characters, up to this \
                             value, so 2 means \"1 edit for short terms, 2 for longer \
                             ones\". 0 turns the fuzzy stages off.",
                        );
                    if let Some(warning) = config.search.fuzzy_edits_warning() {
                        ui.label(
                            egui::RichText::new(warning)
                                .small()
                                .color(crate::ui_util::ORANGE),
                        );
                    }
                });
                ui.end_row();

                ui.label("Display limit");
                ui.add(egui::DragValue::new(&mut config.search.display_limit).range(50..=100_000));
                ui.end_row();

                ui.label("Stream batch size");
                ui.add(
                    egui::DragValue::new(&mut config.search.results_per_page).range(10..=10_000),
                );
                ui.end_row();

                ui.label("Debounce (ms)");
                ui.add(egui::DragValue::new(&mut config.search.debounce_ms).range(0..=2000));
                ui.end_row();

                ui.label("Fuzzy max edits");
                ui.add(egui::DragValue::new(&mut config.search.fuzzy_max_edits).range(0..=8))
                    .on_hover_text(
                        "Ceiling on fuzzy edit distance (the budget grows one \
                         edit per three characters of the term). 0 disables \
                         the fuzzy passes.",
                    );
                ui.end_row();
            });
            // A warning that comes and goes as the value is edited would
            // otherwise move every widget below it in the window; see
            // `ui_util::stable_section`.
            crate::ui_util::stable_section(ui, |ui| {
                if let Some(warning) = config.search.fuzzy_edits_warning() {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // All headless-safe: `keychain_active` only probes the OS keychain when
    // `use_keychain` is set, and no test here sets it.

    #[test]
    fn a_fresh_draft_is_not_dirty() {
        let mut w = OptionsWindow::new();
        let cfg = Config::default();
        assert!(!w.is_dirty(&cfg), "no draft at all");
        w.open_with(&cfg);
        assert!(!w.is_dirty(&cfg));
    }

    #[test]
    fn an_edited_draft_is_dirty_until_discarded() {
        let mut w = OptionsWindow::new();
        let cfg = Config::default();
        w.open_with(&cfg);
        w.draft.as_mut().unwrap().search.debounce_ms += 100;
        assert!(w.is_dirty(&cfg));
        w.close_discard();
        assert!(!w.open);
        assert!(!w.is_dirty(&cfg), "the draft is gone");
    }

    /// The Security block and the mode buttons act on the live config while
    /// the window sits open; the stale copies in the draft are not edits.
    #[test]
    fn live_security_and_mode_changes_are_not_dirty() {
        let mut w = OptionsWindow::new();
        let mut cfg = Config::default();
        w.open_with(&cfg);
        cfg.security.use_keychain = !cfg.security.use_keychain;
        cfg.indexing.auto_index = !cfg.indexing.auto_index;
        assert!(!w.is_dirty(&cfg));
    }

    #[test]
    fn a_dirty_close_is_held_and_a_clean_one_drops_the_draft() {
        let mut w = OptionsWindow::new();
        let cfg = Config::default();
        w.open_with(&cfg);
        w.draft.as_mut().unwrap().search.debounce_ms += 100;

        assert!(
            w.intercept_close(false, &cfg),
            "dirty close raises the guard"
        );
        assert!(w.open, "the window is held open until the user decides");
        assert!(w.draft.is_some(), "the draft survives");

        assert!(!w.intercept_close(true, &cfg), "still open: nothing to do");

        w.draft = Some(cfg.clone());
        assert!(!w.intercept_close(false, &cfg), "a clean close just closes");
        assert!(!w.open);
        assert!(w.draft.is_none());
    }

    /// Where `needle` was painted this frame, as the center of its galley —
    /// a click target that follows the layout instead of pinning it.
    fn painted_text_center(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::epaint::Shape, needle: &str, found: &mut Option<egui::Pos2>) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    if t.galley.text() == needle {
                        *found = Some(t.pos + t.galley.size() / 2.0);
                    }
                }
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, needle, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = None;
        for clipped in &out.shapes {
            walk(&clipped.shape, needle, &mut found);
        }
        found
    }

    /// One real frame of the window in a headless context: it renders, and
    /// the Apply & Save click comes back out as `applied`.
    #[test]
    fn the_window_renders_and_apply_reports_the_draft() {
        let ctx = egui::Context::default();
        let cfg = Config::default();
        let mut w = OptionsWindow::new();
        w.open_with(&cfg);
        w.draft.as_mut().unwrap().search.debounce_ms += 100;

        let run = |w: &mut OptionsWindow, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 900.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = OptionsOutput::default();
            let full = ctx.run(input, |ctx| out = w.ui(ctx, &cfg));
            (out, full)
        };

        // A new egui window spends its first frames in sizing passes that
        // suppress painting; run untouched frames until the settled button
        // is actually on screen.
        let mut target = None;
        for _ in 0..5 {
            let (untouched, full) = run(&mut w, vec![]);
            assert!(untouched.applied.is_none());
            assert!(!untouched.close_requested);
            target = painted_text_center(&full, "Apply & Save");
            if target.is_some() {
                break;
            }
        }
        let target = target.expect("the Apply & Save button was not painted");
        let clicks = [true, false]
            .into_iter()
            .map(|pressed| egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            })
            .collect();
        let (clicked, _) = run(&mut w, clicks);
        let applied = clicked.applied.expect("the click did not report a config");
        assert_eq!(
            applied.search.debounce_ms,
            Config::default().search.debounce_ms + 100,
            "the click reported the edited draft"
        );
    }
}
