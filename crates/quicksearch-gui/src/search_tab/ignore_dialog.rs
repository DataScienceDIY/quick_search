//! The session-ignore dialog: build an ignore chip from a hit's path.

use super::*;

pub struct IgnoreDialog {
    pub source_path: String,
    /// `*.{ext}`, when the file has an extension.
    pub ext_pattern: Option<String>,
    pub name_pattern: String,
    pub dir_pattern: String,
    pub persist: bool,
}

/// Glob ignoring everything under `dir`, spelled with the platform
/// separator. `Path::join` inserts a separator only where one is needed, so
/// a drive root yields `C:\*` rather than the never-matching `C:\/*` a
/// `format!("{}/*")` would produce.
pub(super) fn dir_ignore_pattern(dir: &std::path::Path) -> String {
    dir.join("*").to_string_lossy().into_owned()
}

impl SearchTab {
    pub(super) fn ignore_dialog_ui(&mut self, ctx: &egui::Context, actions: &mut SearchActions) {
        use crate::ui_util::{bordered_button, pattern_edit};
        let p = crate::color::palette(ctx.style().visuals.dark_mode);
        let Some(dialog) = &mut self.ignore_dialog else {
            return;
        };
        let buttons = crate::ui_util::centered_modal(ctx, "Ignore filter", |ui| {
            let mut chosen: Option<String> = None;
            {
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
                ui.horizontal(|ui| {
                    let (_, valid) =
                        pattern_edit(ui, &mut dialog.name_pattern, 240.0, "filename or glob");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(valid, bordered_button("Ignore this filename", p.orange))
                            .clicked()
                        {
                            chosen = Some(dialog.name_pattern.trim().to_string());
                        }
                    });
                });
                // Inside a stable section, or the hint's appearance would
                // rename the directory editor below and drop its focus.
                crate::ui_util::pattern_hint_label(ui, &dialog.name_pattern);
                ui.separator();

                // --- Directory ---------------------------------------------
                ui.horizontal(|ui| {
                    let (_, valid) =
                        pattern_edit(ui, &mut dialog.dir_pattern, 240.0, "directory glob");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(valid, bordered_button("Ignore this directory", p.orange))
                            .clicked()
                        {
                            chosen = Some(dialog.dir_pattern.trim().to_string());
                        }
                    });
                });
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
            }
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
