//! The Options window and the shared config editor used by both the
//! window and the Manage Index tab. Edits happen on a draft; Apply
//! validates, saves, and hands the new config to the app.

use crate::keychain;
use crate::tips::{self, tip_row, Tipped};
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
                            tip_row(ui, "Database file", &tips::DATABASE_PATH, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.paths.database_path)
                                        .desired_width(260.0),
                                )
                            });
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
                            tip_row(ui, "UI scale", &tips::UI_SCALE, |ui| {
                                ui.add(
                                    egui::Slider::new(&mut draft.ui.scale, 0.5..=2.5)
                                        .step_by(0.05)
                                        .fixed_decimals(2),
                                )
                            });
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
                    let apply = ui
                        .add(crate::ui_util::bordered_button(
                            "Apply & Save",
                            if dirty {
                                crate::ui_util::ORANGE
                            } else {
                                crate::ui_util::BLUE
                            },
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
                                    .color(crate::ui_util::ORANGE),
                            );
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "Narrowing a filter removes the entries it excludes; widening \
                         one reindexes to find what it now allows. Only the tokenizer \
                         and hash length require a full rebuild.",
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

/// The per-section config controls of the Options window.
///
/// Every row goes through [`crate::tips::tip_row`], which takes the tooltip
/// that explains it: a setting cannot arrive here without one, and hovering
/// the name works as well as hovering the control.
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

                    tip_row(ui, "Hash sample size (bytes)", &tips::HASH_LENGTH, |ui| {
                        ui.add(
                            egui::DragValue::new(&mut config.processing.hash_length)
                                .range(512..=1_048_576),
                        )
                    });

                    tip_row(
                        ui,
                        "Max stored text (bytes)",
                        &tips::MAX_STORED_TEXT,
                        |ui| {
                            ui.add(
                                egui::DragValue::new(&mut config.processing.maximum_text_size)
                                    .range(1024..=16_777_216),
                            )
                        },
                    );

                    tip_row(
                        ui,
                        "Max text file size (bytes)",
                        &tips::MAX_TEXT_FILE_SIZE,
                        |ui| {
                            ui.add(
                                egui::DragValue::new(&mut config.processing.maximum_text_file_size)
                                    .range(1024..=1_073_741_824),
                            )
                        },
                    );

                    tip_row(ui, "Batch size", &tips::BATCH_SIZE, |ui| {
                        ui.add(
                            egui::DragValue::new(&mut config.processing.batch_size)
                                .range(10..=100_000),
                        )
                    });

                    tip_row(ui, "Max WAL size (bytes)", &tips::MAX_WAL_SIZE, |ui| {
                        ui.add(
                            egui::DragValue::new(&mut config.processing.maximum_wal_size)
                                .range(0u64..=8_589_934_592u64),
                        )
                    });

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

                tip_row(ui, "Fuzzy edit distance", &tips::FUZZY_EDITS, |ui| {
                    ui.add(egui::DragValue::new(&mut config.search.fuzzy_max_edits).range(0..=8))
                });

                tip_row(ui, "Display limit", &tips::DISPLAY_LIMIT, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut config.search.display_limit).range(50..=100_000),
                    )
                });

                tip_row(ui, "Stream batch size", &tips::RESULTS_PER_PAGE, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut config.search.results_per_page)
                            .range(10..=10_000),
                    )
                });

                tip_row(ui, "Debounce (ms)", &tips::DEBOUNCE, |ui| {
                    ui.add(egui::DragValue::new(&mut config.search.debounce_ms).range(0..=2000))
                });
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

    use crate::test_ui::{painted_text, painted_text_center};

    /// Every row of every section, with the tip it must show. `tip_row`
    /// makes a row without *a* tooltip impossible; this table is what makes
    /// a row with the *wrong* one impossible.
    const ROWS: &[(Section, &str, &tips::Tip)] = &[
        (
            Section::Indexing,
            "Full reindex every",
            &tips::REINDEX_INTERVAL,
        ),
        (Section::Indexing, "Follow symlinks", &tips::FOLLOW_SYMLINKS),
        (
            Section::Indexing,
            "Include hidden files",
            &tips::INCLUDE_HIDDEN,
        ),
        (Section::Processing, "Tokenizer", &tips::TOKENIZER),
        (
            Section::Processing,
            "Hash sample size (bytes)",
            &tips::HASH_LENGTH,
        ),
        (
            Section::Processing,
            "Max stored text (bytes)",
            &tips::MAX_STORED_TEXT,
        ),
        (
            Section::Processing,
            "Max text file size (bytes)",
            &tips::MAX_TEXT_FILE_SIZE,
        ),
        (Section::Processing, "Batch size", &tips::BATCH_SIZE),
        (
            Section::Processing,
            "Max WAL size (bytes)",
            &tips::MAX_WAL_SIZE,
        ),
        (
            Section::Processing,
            "Store text for snippets",
            &tips::STORE_TEXT,
        ),
        (
            Section::Search,
            "Fuzzy search ON by default",
            &tips::FUZZY_DEFAULT,
        ),
        (Section::Search, "Fuzzy edit distance", &tips::FUZZY_EDITS),
        (Section::Search, "Display limit", &tips::DISPLAY_LIMIT),
        (
            Section::Search,
            "Stream batch size",
            &tips::RESULTS_PER_PAGE,
        ),
        (Section::Search, "Debounce (ms)", &tips::DEBOUNCE),
    ];

    /// Hovering a row's name paints that row's own explanation. Rendered
    /// without the window's scroll area so nothing sits below the fold.
    #[test]
    fn every_row_shows_its_own_tip() {
        for (section, label, tip) in ROWS {
            let ctx = egui::Context::default();
            ctx.style_mut(|s| {
                s.interaction.tooltip_delay = 0.0;
                s.interaction.show_tooltips_only_when_still = false;
            });
            let mut cfg = Config::default();
            let mut run = |events: Vec<egui::Event>| {
                let input = crate::test_ui::raw_input(egui::vec2(600.0, 800.0), events);
                ctx.run(input, |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| config_editor_ui(ui, &mut cfg, *section));
                })
            };

            let first = run(vec![]);
            let pos = painted_text_center(&first, label)
                .unwrap_or_else(|| panic!("{label} was not painted"));

            // Enough of the body to be unique, and short enough to survive
            // an edit to the sentence it starts.
            let opening: String = tip.body.chars().take(40).collect();
            let mut out = run(vec![egui::Event::PointerMoved(pos)]);
            let mut found = false;
            for _ in 0..3 {
                // The tooltip is an area of its own, so it can land a frame
                // late.
                if painted_text(&out).join("\n").contains(&opening) {
                    found = true;
                    break;
                }
                out = run(vec![]);
            }
            assert!(found, "hovering {label:?} did not show {:?}", tip.title);
        }
    }

    /// Hovering a setting's *name* explains it, not just its control: the
    /// label is the larger target and the one a reader's eye is already on.
    /// Checks the wiring, so the tooltip timing is turned off.
    #[test]
    fn hovering_a_setting_label_explains_it() {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let cfg = Config::default();
        let mut w = OptionsWindow::new();
        w.open_with(&cfg);

        let run = |w: &mut OptionsWindow, events: Vec<egui::Event>| {
            let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
            ctx.run(input, |ctx| {
                w.ui(ctx, &cfg);
            })
        };

        // The window spends its first frames sizing itself and painting
        // nothing; run until the label is on screen.
        let mut target = None;
        for _ in 0..5 {
            let full = run(&mut w, vec![]);
            target = painted_text_center(&full, "Tokenizer");
            if target.is_some() {
                break;
            }
        }
        let target = target.expect("the Tokenizer label was not painted");

        // The tooltip is an area of its own, so it can land a frame late.
        let mut out = run(&mut w, vec![egui::Event::PointerMoved(target)]);
        for _ in 0..3 {
            let painted = painted_text(&out).join("\n");
            if painted.contains(crate::tips::TOKENIZER.title)
                && painted.contains("cut up so that it can be")
            {
                return;
            }
            out = run(&mut w, vec![]);
        }
        panic!("no tooltip appeared over the Tokenizer label");
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
            let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
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
