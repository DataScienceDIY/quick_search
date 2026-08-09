//! The Options window and the shared config editor used by both the
//! window and the Manage Index tab. Edits happen on a draft; Apply
//! validates, saves, and hands the new config to the app.

use crate::keychain;
use crate::tips::{self, tip_row, Tipped};
use crate::ui_util::hint;
use quicksearch_core::config::Config;

/// A [`tip_row`] holding one numeric [`egui::DragValue`] — the shape of most
/// rows in the config editor.
fn drag_row<N: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    tip: &'static tips::Tip,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
) {
    tip_row(ui, label, tip, |ui| {
        ui.add(egui::DragValue::new(value).range(range))
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Indexing,
    Processing,
    Search,
}

/// A click in the Security block. These are not draft edits: every action
/// runs its own explicit flow in the app.
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
    /// edits; the window is held open and the app raises the guard.
    pub close_requested: bool,
}

pub struct OptionsWindow {
    pub open: bool,
    draft: Option<Config>,
    /// Cached answer from [`OptionsWindow::keychain_active`], with the
    /// `use_keychain` preference it was probed under.
    keychain_probed_for: Option<bool>,
    keychain_active: bool,
    /// The search-shortcut button is waiting for a key press to bind.
    capturing_hotkey: bool,
}

impl OptionsWindow {
    pub fn new() -> OptionsWindow {
        OptionsWindow {
            open: false,
            draft: None,
            keychain_probed_for: None,
            keychain_active: false,
            capturing_hotkey: false,
        }
    }

    /// Whether the shortcut button is reading a key press right now, so the
    /// app can hold the shortcut it is about to replace. See
    /// [`crate::unlock::Gate::handle_hotkey`].
    pub fn capturing_hotkey(&self) -> bool {
        self.open && self.capturing_hotkey
    }

    pub fn open_with(&mut self, current: &Config) {
        self.open = true;
        self.draft = Some(current.clone());
        self.keychain_probed_for = None;
    }

    /// Whether the draft differs from the live config. The fields the app
    /// pins on apply are neutralized first, so the Security block never
    /// makes the window read as dirty.
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
        self.capturing_hotkey = false;
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
    /// daemon, a locked keyring or a denied prompt all read as "no").
    /// Probed when the window opens and when the preference changes — a
    /// keychain read is an IPC round trip.
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
        let capturing = &mut self.capturing_hotkey;
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
                            tip_row(ui, "Database file", &tips::DATABASE_PATH, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.paths.database_path)
                                        .desired_width(260.0),
                                )
                            });
                        });
                        ui.label(hint("Indexed folders are managed on the Manage Index tab."));
                        ui.separator();

                        ui.heading(egui::RichText::new("Indexing").strong());
                        config_editor_ui(ui, draft, Section::Indexing);
                        ui.label(hint(
                            "Automatic and manual indexing are switched on the \
                             Manage Index tab.",
                        ));
                        ui.separator();

                        ui.heading(egui::RichText::new("Processing").strong());
                        config_editor_ui(ui, draft, Section::Processing);
                        ui.separator();

                        ui.heading(egui::RichText::new("Search").strong());
                        config_editor_ui(ui, draft, Section::Search);
                        ui.separator();

                        ui.heading(egui::RichText::new("Interface").strong());
                        egui::Grid::new("opt-ui").num_columns(2).show(ui, |ui| {
                            tip_row(ui, "UI scale", &tips::UI_SCALE, |ui| {
                                ui.add(
                                    egui::Slider::new(&mut draft.ui.scale, 0.5..=2.5)
                                        .step_by(0.05)
                                        .fixed_decimals(2),
                                )
                            });
                            tip_row(ui, "Search shortcut", &tips::SEARCH_HOTKEY, |ui| {
                                hotkey_edit(ui, &mut draft.ui.search_hotkey, capturing)
                            });
                            tip_row(ui, "Color scheme", &tips::COLOR_SCHEME, |ui| {
                                color_scheme_edit(ui, &mut draft.ui.color_scheme)
                            });
                        });
                        hotkey_note(ui, &draft.ui.search_hotkey, &current.ui.search_hotkey);
                        ui.separator();

                        // Security acts on the live config, not the draft.
                        // The KDF salt is never shown here or anywhere else
                        // in the GUI.
                        ui.heading(egui::RichText::new("Security").strong());
                        out.security = security_ui(ui, current, keychain_active);
                    });
                crate::ui_util::more_below_hint(ui, &scroll);

                ui.separator();
                let p = crate::color::palette(ui.visuals().dark_mode);
                ui.horizontal(|ui| {
                    let apply = ui
                        .add(crate::ui_util::bordered_button(
                            "Apply & Save",
                            if dirty { p.orange } else { p.blue },
                        ))
                        .tip(&tips::APPLY_SAVE);
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
                                    .color(p.orange),
                            );
                        }
                    });
                });
                ui.label(hint(
                    "Narrowing a filter removes the entries it excludes; widening \
                         one reindexes to find what it now allows. Only the tokenizer \
                         and hash length require a full rebuild.",
                ));
            });

        out.close_requested = self.intercept_close(open, current);
        out
    }
}

/// The color schemes, as stored (lowercase) and as shown.
const COLOR_SCHEMES: [(&str, &str); 2] = [("dark", "Dark"), ("light", "Light")];

/// What the dropdown shows for a stored value, resolved through
/// [`crate::app::theme_for`] so the box says what the app will actually do
/// — including with a value it does not recognise.
fn scheme_label(value: &str) -> &'static str {
    match crate::app::theme_for(value) {
        egui::Theme::Dark => "Dark",
        egui::Theme::Light => "Light",
    }
}

/// The color scheme dropdown. Returns the box, which is what the row's
/// tooltip hangs off.
fn color_scheme_edit(ui: &mut egui::Ui, setting: &mut String) -> egui::Response {
    egui::ComboBox::from_id_salt("cfg-color-scheme")
        .selected_text(scheme_label(setting))
        .show_ui(ui, |ui| {
            for (stored, label) in COLOR_SCHEMES {
                ui.selectable_value(setting, stored.to_string(), label);
            }
        })
        .response
}

/// The search shortcut's control: a button showing the current binding that
/// turns into a key-press reader when clicked, and a Clear beside it.
/// Returns the button, which is what the row's tooltip hangs off.
fn hotkey_edit(ui: &mut egui::Ui, setting: &mut String, capturing: &mut bool) -> egui::Response {
    let p = crate::color::palette(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        let label = if *capturing {
            "Press a key combination...".to_string()
        } else if setting.trim().is_empty() {
            "None".to_string()
        } else {
            setting.clone()
        };
        let button = ui.add(crate::ui_util::bordered_button(
            label,
            if *capturing { p.orange } else { p.blue },
        ));
        // A second click backs out.
        if button.clicked() {
            *capturing = !*capturing;
        } else if *capturing {
            match read_capture(ui) {
                Some(Some(binding)) => {
                    *setting = binding.to_string();
                    *capturing = false;
                }
                Some(None) => *capturing = false,
                None => {}
            }
        }
        if ui
            .add_enabled(
                !setting.trim().is_empty(),
                egui::Button::new("Clear").small(),
            )
            .clicked()
        {
            setting.clear();
            *capturing = false;
        }
        button
    })
    .inner
}

/// One frame of shortcut capture: `Some(Some(binding))` for a press worth
/// binding, `Some(None)` for a cancel, `None` while nothing usable has
/// arrived. Raw events, because egui's shortcut matching cannot report an
/// arbitrary combination; invalid presses (a bare letter) are ignored, not
/// treated as a cancel.
fn read_capture(ui: &egui::Ui) -> Option<Option<crate::hotkey::Binding>> {
    ui.input(|i| {
        for event in &i.events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if *key == egui::Key::Escape {
                return Some(None);
            }
            if let Some(binding) = crate::hotkey::Binding::from_egui(*key, modifiers) {
                return Some(Some(binding));
            }
        }
        None
    })
}

/// What the shortcut is really doing, under the Interface grid. Silent
/// while registered and working; a line appears only when what is on the
/// button is not what is in force.
fn hotkey_note(ui: &mut egui::Ui, draft: &str, live: &str) {
    use crate::hotkey::Status;
    let (text, color) = if draft.trim() != live.trim() {
        ("Not registered until Apply and Save.".to_string(), None)
    } else {
        match crate::hotkey::status() {
            Status::Disabled | Status::Active => (String::new(), None),
            Status::Pending => (
                "Waiting for your desktop to accept the shortcut.".to_string(),
                None,
            ),
            Status::PortalBound(trigger) => (
                format!(
                    "Your desktop registered this as {}. It has the final say; \
                     change it in its own keyboard settings. On Wayland it also \
                     decides whether the window comes forward, so a minimised \
                     window may stay minimised.",
                    trigger
                ),
                None,
            ),
            Status::Error(why) => (
                format!("The shortcut is not active: {}.", why),
                Some(crate::color::palette(ui.visuals().dark_mode).orange),
            ),
        }
    };
    // Comes and goes with the state; keep it off the ids of what follows.
    crate::ui_util::stable_section(ui, |ui| {
        if text.is_empty() {
            return;
        }
        let rich = egui::RichText::new(text).small();
        ui.label(match color {
            Some(color) => rich.color(color),
            None => rich.weak(),
        });
    });
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
            if ui
                .button("Change password…")
                .tip(&tips::CHANGE_PASSWORD)
                .clicked()
            {
                action = Some(SecurityAction::ChangePassword);
            }
            if ui
                .button("Disable protection…")
                .tip(&tips::DISABLE_PASSWORD)
                .clicked()
            {
                action = Some(SecurityAction::Disable);
            }
        });
        let mut remember = current.security.use_keychain;
        if ui
            .checkbox(&mut remember, "Remember on this device")
            .tip(&tips::REMEMBER_KEYCHAIN)
            .changed()
        {
            action = Some(SecurityAction::SetKeychain(remember));
        }
    } else {
        ui.label("The index is not encrypted.");
        if ui
            .button("Enable password protection…")
            .tip(&tips::ENABLE_PASSWORD)
            .clicked()
        {
            action = Some(SecurityAction::Enable);
        }
        ui.label(hint(
            "The index stores the names and text of your files. A password \
                 encrypts it on disk; enabling one rebuilds the index.",
        ));
    }
    action
}

/// The per-section config controls of the Options window. Every row goes
/// through [`crate::tips::tip_row`], so a setting cannot arrive here
/// without a tooltip.
pub fn config_editor_ui(ui: &mut egui::Ui, config: &mut Config, section: Section) {
    match section {
        Section::Indexing => {
            egui::Grid::new("cfg-indexing")
                .num_columns(2)
                .show(ui, |ui| {
                    // Automatic vs manual is absent: it is live state switched
                    // on the Manage Index tab, and a staged copy here would
                    // fight those buttons.
                    tip_row(ui, "Full reindex every", &tips::REINDEX_INTERVAL, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut config.indexing.reindex_interval_minutes)
                                    .range(5..=60 * 24 * 30),
                            );
                            ui.label("minutes");
                        })
                        .response
                    });

                    tip_row(ui, "Follow symlinks", &tips::FOLLOW_SYMLINKS, |ui| {
                        ui.checkbox(&mut config.indexing.follow_symlinks, "")
                    });

                    tip_row(ui, "Include hidden files", &tips::INCLUDE_HIDDEN, |ui| {
                        ui.checkbox(&mut config.indexing.include_hidden, "")
                    });
                });
        }
        Section::Processing => {
            egui::Grid::new("cfg-processing")
                .num_columns(2)
                .show(ui, |ui| {
                    tip_row(ui, "Tokenizer", &tips::TOKENIZER, |ui| {
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
                            })
                            .response
                    });

                    ui.label("");
                    ui.hyperlink_to(
                        "Tokenizer documentation",
                        "https://www.sqlite.org/fts5.html#tokenizers",
                    );
                    ui.end_row();

                    drag_row(
                        ui,
                        "Hash sample size (bytes)",
                        &tips::HASH_LENGTH,
                        &mut config.processing.hash_length,
                        512..=1_048_576,
                    );

                    drag_row(
                        ui,
                        "Max stored text (bytes)",
                        &tips::MAX_STORED_TEXT,
                        &mut config.processing.maximum_text_size,
                        1024..=16_777_216,
                    );

                    drag_row(
                        ui,
                        "Max text file size (bytes)",
                        &tips::MAX_TEXT_FILE_SIZE,
                        &mut config.processing.maximum_text_file_size,
                        1024..=1_073_741_824,
                    );

                    drag_row(
                        ui,
                        "Batch size",
                        &tips::BATCH_SIZE,
                        &mut config.processing.batch_size,
                        10..=100_000,
                    );

                    drag_row(
                        ui,
                        "Max WAL size (bytes)",
                        &tips::MAX_WAL_SIZE,
                        &mut config.processing.maximum_wal_size,
                        0u64..=8_589_934_592u64,
                    );

                    tip_row(ui, "Store text for snippets", &tips::STORE_TEXT, |ui| {
                        ui.checkbox(&mut config.processing.store_text_for_snippets, "")
                    });
                });
        }
        Section::Search => {
            egui::Grid::new("cfg-search").num_columns(2).show(ui, |ui| {
                tip_row(
                    ui,
                    "Fuzzy search ON by default",
                    &tips::FUZZY_DEFAULT,
                    |ui| ui.checkbox(&mut config.search.fuzzy_default, ""),
                );

                drag_row(
                    ui,
                    "Fuzzy edit distance",
                    &tips::FUZZY_EDITS,
                    &mut config.search.fuzzy_max_edits,
                    0..=8,
                );

                drag_row(
                    ui,
                    "Display limit",
                    &tips::DISPLAY_LIMIT,
                    &mut config.search.display_limit,
                    50..=100_000,
                );

                drag_row(
                    ui,
                    "Stream batch size",
                    &tips::RESULTS_PER_PAGE,
                    &mut config.search.results_per_page,
                    10..=10_000,
                );

                drag_row(
                    ui,
                    "Debounce (ms)",
                    &tips::DEBOUNCE,
                    &mut config.search.debounce_ms,
                    0..=2000,
                );
            });
            // The warning comes and goes as the value is edited; keep it off
            // the ids of what follows (`ui_util::stable_section`).
            crate::ui_util::stable_section(ui, |ui| {
                if let Some(warning) = config.search.fuzzy_edits_warning() {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
