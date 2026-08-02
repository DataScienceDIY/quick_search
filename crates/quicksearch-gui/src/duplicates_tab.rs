//! The Duplicates tab: groups of files sharing a content hash.

use quicksearch_core::search::DuplicateGroup;

use crate::format::{group_thousands, human_size};
use crate::platform;

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
}

impl DuplicatesTab {
    pub fn new() -> DuplicatesTab {
        DuplicatesTab {
            state: DupState::NotLoaded,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> DuplicatesActions {
        let mut actions = DuplicatesActions::default();

        ui.horizontal(|ui| {
            let loading = matches!(self.state, DupState::Loading);
            if ui.add_enabled(!loading, egui::Button::new("Refresh")).clicked() {
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
                    ui.label(
                        egui::RichText::new("Showing the 500 largest groups.").small().weak(),
                    );
                }
                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
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
                        egui::CollapsingHeader::new(title).id_salt(i).show(ui, |ui| {
                            for (_, _, path, size, _) in &group.members {
                                ui.horizontal(|ui| {
                                    ui.label(human_size(*size));
                                    let response = ui
                                        .add(egui::Label::new(egui::RichText::new(path).monospace())
                                            .sense(egui::Sense::click()));
                                    if response.double_clicked() {
                                        platform::open_file(path);
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button("Open").clicked() {
                                            platform::open_file(path);
                                            ui.close();
                                        }
                                        if ui.button("Open containing folder").clicked() {
                                            platform::reveal_in_folder(path);
                                            ui.close();
                                        }
                                    });
                                });
                            }
                        });
                    }
                });
            }
        }
        actions
    }
}
