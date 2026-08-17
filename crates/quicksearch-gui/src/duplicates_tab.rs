//! The Duplicates tab: groups of files sharing a content hash.

use quicksearch_core::search::DuplicateGroup;

use crate::format::{group_thousands, human_size};
use crate::platform;
use crate::ui_util::hint;

pub enum DupState {
    NotLoaded,
    Loading,
    Loaded(Vec<DuplicateGroup>),
    Error(String),
}

pub struct DuplicatesTab {
    pub state: DupState,
}

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct DuplicatesActions {
    pub refresh: bool,
    /// Every member of one group, to be read through and compared byte for
    /// byte. Group-scoped whichever row it was asked for from: the question
    /// "is this row really a duplicate" is a question about the group.
    pub verify: Option<Vec<String>>,
}

/// The entry both context menus carry. Named for what it settles, since the
/// grouping itself never claimed more than a shared size and head.
const VERIFY_LABEL: &str = "Verify copies are identical…";
const VERIFY_TIP: &str = "Reads every file in the group in full and compares them byte for \
                          byte. Grouping only reads each file's size and how it begins.";

impl DuplicatesTab {
    pub fn new() -> DuplicatesTab {
        DuplicatesTab {
            state: DupState::NotLoaded,
        }
    }

    /// `verify_open` is the verification window being up — running or showing
    /// a result. There is one of it, so the entry greys out rather than
    /// replacing what someone is reading.
    pub fn ui(&mut self, ui: &mut egui::Ui, verify_open: bool) -> DuplicatesActions {
        let mut actions = DuplicatesActions::default();

        ui.horizontal(|ui| {
            let loading = matches!(self.state, DupState::Loading);
            if ui
                .add_enabled(!loading, egui::Button::new("Refresh"))
                .clicked()
            {
                actions.refresh = true;
            }
            if loading {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label("Scanning for duplicates…");
            }
        });
        ui.separator();

        match &self.state {
            DupState::NotLoaded => {
                ui.label(
                    egui::RichText::new("Press Refresh to scan the index for duplicate files.")
                        .weak(),
                );
            }
            // The header row above already shows the spinner and its label.
            DupState::Loading => {}
            DupState::Error(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            DupState::Loaded(groups) => {
                if groups.is_empty() {
                    ui.label("No duplicate files found.");
                    return actions;
                }
                if groups.len() == 500 {
                    ui.label(hint("Showing the 500 largest groups."));
                }
                let scroll = egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for (i, group) in groups.iter().enumerate() {
                            let name = group
                                .members
                                .first()
                                .map(|m| m.1.as_str())
                                .unwrap_or("(unknown)");
                            let title = format!(
                                "{} × {}: {} reclaimable ({} total)",
                                group_thousands(group.count as u64),
                                name,
                                human_size(group.redundant_size.max(0) as u64),
                                human_size(group.total_size.max(0) as u64),
                            );
                            let header =
                                egui::CollapsingHeader::new(title)
                                    .id_salt(i)
                                    .show(ui, |ui| {
                                        for (_, _, path, size, _) in &group.members {
                                            ui.horizontal(|ui| {
                                                ui.label(human_size(*size));
                                                let response = ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(path).monospace(),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                );
                                                if response.double_clicked() {
                                                    platform::open_file(path);
                                                }
                                                response.context_menu(|ui| {
                                                    if ui.button("Open File").clicked() {
                                                        platform::open_file(path);
                                                        ui.close();
                                                    }
                                                    if ui.button("Open containing folder").clicked()
                                                    {
                                                        platform::reveal_in_folder(path);
                                                        ui.close();
                                                    }
                                                    ui.separator();
                                                    if verify_entry(ui, verify_open) {
                                                        actions.verify = Some(member_paths(group));
                                                    }
                                                });
                                            });
                                        }
                                    });
                            // Also on the group's own row: the question is
                            // about the group, and the rows it is about are
                            // behind a collapsed header until they are not.
                            header.header_response.context_menu(|ui| {
                                if verify_entry(ui, verify_open) {
                                    actions.verify = Some(member_paths(group));
                                }
                            });
                        }
                    });
                crate::ui_util::more_below_hint(ui, &scroll);
            }
        }
        actions
    }
}

/// The shared menu entry. Returns whether it was clicked, and closes the menu
/// when it was.
fn verify_entry(ui: &mut egui::Ui, open: bool) -> bool {
    let clicked = ui
        .add_enabled(!open, egui::Button::new(VERIFY_LABEL))
        .on_hover_text(VERIFY_TIP)
        .on_disabled_hover_text("Close the verification window first.")
        .clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn member_paths(group: &DuplicateGroup) -> Vec<String> {
    group.members.iter().map(|m| m.2.clone()).collect()
}

#[cfg(test)]
mod tests;
