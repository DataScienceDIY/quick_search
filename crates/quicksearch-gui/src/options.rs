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
        let draft = self.draft.as_mut().unwrap();

        egui::Window::new("Options")
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                let scroll = egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
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
                    if ui.button("Apply & Save").clicked() {
                        out.applied = Some(draft.clone());
                    }
                    ui.label(
                        egui::RichText::new(
                            "Changes to tokenizer, filters, hidden files, or hashing \
                             prompt an index rebuild.",
                        )
                        .small()
                        .weak(),
                    );
                });
            });

        self.open = open;
        if !self.open {
            self.draft = None;
        }
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
            egui::Grid::new("cfg-indexing").num_columns(2).show(ui, |ui| {
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
            egui::Grid::new("cfg-processing").num_columns(2).show(ui, |ui| {
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
                ui.add(egui::DragValue::new(&mut config.processing.hash_length).range(512..=1_048_576));
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
                ui.add(egui::DragValue::new(&mut config.processing.batch_size).range(10..=100_000));
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
            if let Some(warning) = config.search.fuzzy_edits_warning() {
                ui.colored_label(ui.visuals().warn_fg_color, warning);
            }
        }
    }
}
