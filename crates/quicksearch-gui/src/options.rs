//! The Options window and the shared config editor used by both the
//! window and the Manage Index tab. Edits happen on a draft; Apply
//! validates, saves, and hands the new config to the app.

use quicksearch_core::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Indexing,
    Processing,
    Search,
}

pub struct OptionsWindow {
    pub open: bool,
    draft: Option<Config>,
}

impl OptionsWindow {
    pub fn new() -> OptionsWindow {
        OptionsWindow {
            open: false,
            draft: None,
        }
    }

    pub fn open_with(&mut self, current: &Config) {
        self.open = true;
        self.draft = Some(current.clone());
    }

    /// Render; returns a new config when the user applied changes.
    pub fn ui(&mut self, ctx: &egui::Context, current: &Config) -> Option<Config> {
        if !self.open {
            self.draft = None;
            return None;
        }
        if self.draft.is_none() {
            self.draft = Some(current.clone());
        }
        let mut applied = None;
        let mut open = self.open;
        let draft = self.draft.as_mut().unwrap();

        egui::Window::new("Options")
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                    ui.heading("Paths");
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

                    ui.heading("Indexing");
                    config_editor_ui(ui, draft, Section::Indexing);
                    ui.separator();

                    ui.heading("Processing");
                    config_editor_ui(ui, draft, Section::Processing);
                    ui.separator();

                    ui.heading("Search");
                    config_editor_ui(ui, draft, Section::Search);
                    ui.separator();

                    ui.heading("Interface");
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
                });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Apply & Save").clicked() {
                        applied = Some(draft.clone());
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
        applied
    }
}

/// One implementation of the per-section config controls, shared by the
/// Options window and the Manage tab.
pub fn config_editor_ui(ui: &mut egui::Ui, config: &mut Config, section: Section) {
    match section {
        Section::Indexing => {
            egui::Grid::new("cfg-indexing").num_columns(2).show(ui, |ui| {
                ui.label("Automatic indexing");
                ui.checkbox(&mut config.indexing.auto_index, "watchers + periodic reindex");
                ui.end_row();

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
                ui.label("Fuzzy stages on by default");
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
                                .color(egui::Color32::from_rgb(220, 150, 40)),
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
