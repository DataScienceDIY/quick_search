//! The session-ignore dialog: build an ignore chip from a hit's path.

use super::*;

use quicksearch_core::config::glob_literal;

pub struct IgnoreDialog {
    pub source_path: String,
    /// `*.{ext}`, the extension escaped, when the file has one.
    pub ext_pattern: Option<String>,
    pub name_pattern: String,
    pub dir_pattern: String,
    pub persist: bool,
}

impl IgnoreDialog {
    /// The dialog's starting patterns for one hit: its extension, its name
    /// and its folder, each escaped so it matches only what it was built from.
    pub(super) fn for_hit(hit: &SearchHit) -> IgnoreDialog {
        IgnoreDialog {
            source_path: hit.path.clone(),
            ext_pattern: std::path::Path::new(&hit.name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!("*.{}", glob_literal(e))),
            name_pattern: glob_literal(&hit.name),
            dir_pattern: std::path::Path::new(&hit.path)
                .parent()
                .map(crate::ui_util::dir_ignore_pattern)
                .unwrap_or_default(),
            persist: false,
        }
    }
}

impl SearchTab {
    pub(super) fn ignore_dialog_ui(&mut self, ctx: &egui::Context, actions: &mut SearchActions) {
        use crate::ui_util::{bordered_button, pattern_edit};
        let p = crate::color::palette(ctx.style().visuals.dark_mode);
        let Some(dialog) = &mut self.ignore_dialog else {
            return;
        };
        // A pattern box with its button right-aligned; the pattern when clicked.
        let pattern_row = |ui: &mut egui::Ui, pattern: &mut String, hint: &str, label: &str| {
            ui.horizontal(|ui| {
                let (_, valid) = pattern_edit(ui, pattern, 240.0, hint);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled(valid, bordered_button(label, p.orange))
                        .clicked()
                        .then(|| pattern.trim().to_string())
                })
                .inner
            })
            .inner
        };
        let buttons = crate::ui_util::centered_modal(ctx, "Ignore filter", |ui| {
            let mut chosen: Option<String> = None;
            ui.set_min_width(430.0);
            ui.label(format!("From: {}", dialog.source_path));
            ui.separator();

            // --- Extension ---------------------------------------------
            ui.horizontal(|ui| match &dialog.ext_pattern {
                Some(ext) => {
                    ui.monospace(ext);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(bordered_button("Ignore this extension", p.orange))
                            .clicked()
                        {
                            chosen = Some(ext.clone());
                        }
                    });
                }
                None => {
                    ui.label(egui::RichText::new("(no file extension)").weak());
                }
            });
            ui.separator();

            // --- Filename ----------------------------------------------
            chosen = pattern_row(
                ui,
                &mut dialog.name_pattern,
                "filename or glob",
                "Ignore this filename",
            )
            .or(chosen);
            // In a stable section, so the hint coming and going keeps the
            // directory editor's id (see `ui_util::stable_section`).
            crate::ui_util::pattern_hint_label(ui, &dialog.name_pattern);
            ui.separator();

            // --- Directory ---------------------------------------------
            chosen = pattern_row(
                ui,
                &mut dialog.dir_pattern,
                "directory glob",
                "Ignore this directory",
            )
            .or(chosen);
            ui.separator();

            // --- Persist + close ---------------------------------------
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, p.blue))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(6, 3))
                .show(ui, |ui| {
                    ui.checkbox(&mut dialog.persist, "Persist to config");
                });
            ui.label(hint(
                "Session filters hide results immediately. Persisted filters also \
                 exclude files from the index at the next reindex.",
            ));
            let cancel = ui.button("Cancel").clicked();
            (chosen, cancel)
        });
        let (chosen, cancel) = buttons.unwrap_or((None, false));
        if let Some(pattern) = chosen {
            let dialog = self.ignore_dialog.take().unwrap();
            if !self.session_ignores.contains(&pattern) {
                self.session_ignores.push(pattern.clone());
            }
            if dialog.persist {
                actions.persist_ignore = Some(pattern);
            }
            actions.rerun = true;
        } else if cancel {
            self.ignore_dialog = None;
        }
    }
}
