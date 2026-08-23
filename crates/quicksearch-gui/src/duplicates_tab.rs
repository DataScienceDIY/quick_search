//! The Duplicates tab: groups of files sharing a content hash.

use quicksearch_core::search::DuplicateGroup;

use crate::format::{group_thousands, human_size};
use crate::platform;
use crate::ui_util::hint;

pub enum DupState {
    NotLoaded,
    Loading,
    Loaded(LoadedGroups),
    Error(String),
}

/// What order the groups are listed in. The scan itself always returns the
/// groups with the most reclaimable bytes (see `backend.rs`); this reorders
/// that set in place, so switching costs nothing and never changes *which*
/// groups are on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DupSort {
    #[default]
    Reclaimable,
    Extension,
}

impl DupSort {
    fn label(self) -> &'static str {
        match self {
            DupSort::Reclaimable => "Reclaimable space",
            DupSort::Extension => "File extension",
        }
    }
}

/// A group's extension, lowercased: the one from the member the title names,
/// since copies of one file can be filed under different names. Empty for a
/// group whose representative has no extension at all.
fn group_extension(group: &DuplicateGroup) -> String {
    group
        .members
        .first()
        .map(|m| m.1.as_str())
        .and_then(|name| std::path::Path::new(name).extension())
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// What [`LoadedGroups::sort`] orders an extension listing by, ending in the
/// group's own index: no extension last, then the extension, then the
/// biggest waste, then the hash to settle whatever is left.
type ExtensionKey<'a> = (bool, String, std::cmp::Reverse<i64>, &'a [u8], usize);

/// The scan's result, with each group's header line already built. Measured:
/// building the titles in the render loop cost ~2,000 allocations a frame, on
/// a list that repaints at 20 Hz for as long as the tab is open.
pub struct LoadedGroups {
    pub groups: Vec<DuplicateGroup>,
    titles: Vec<String>,
    /// Indices into `groups`, in display order. Reordering this leaves
    /// `groups` and `titles` parallel, which the render loop relies on.
    order: Vec<usize>,
    sorted_by: DupSort,
}

impl LoadedGroups {
    pub fn new(groups: Vec<DuplicateGroup>) -> LoadedGroups {
        let titles = groups
            .iter()
            .map(|group| {
                let name = group
                    .members
                    .first()
                    .map(|m| m.1.as_str())
                    .unwrap_or("(unknown)");
                format!(
                    "{} × {}: {} reclaimable ({} total)",
                    group_thousands(group.count as u64),
                    name,
                    human_size(group.redundant_size.max(0) as u64),
                    human_size(group.total_size.max(0) as u64),
                )
            })
            .collect();
        let order = (0..groups.len()).collect();
        LoadedGroups {
            groups,
            titles,
            order,
            sorted_by: DupSort::Reclaimable,
        }
    }

    /// Reorder to `key`. A no-op when it is already the order in force, so
    /// the render loop can call it unconditionally.
    fn sort(&mut self, key: DupSort) {
        if self.sorted_by == key {
            return;
        }
        self.sorted_by = key;
        match key {
            // What the query already returned, so the indices go back as they came.
            DupSort::Reclaimable => self.order.sort_unstable(),
            // Extensionless groups last, biggest waste first within an
            // extension, hash to break the remaining ties for good.
            DupSort::Extension => {
                let mut keyed: Vec<ExtensionKey<'_>> = self
                    .order
                    .iter()
                    .map(|&i| {
                        let group = &self.groups[i];
                        let ext = group_extension(group);
                        (
                            ext.is_empty(),
                            ext,
                            std::cmp::Reverse(group.redundant_size),
                            group.hash.as_slice(),
                            i,
                        )
                    })
                    .collect();
                keyed.sort_unstable();
                self.order = keyed.into_iter().map(|k| k.4).collect();
            }
        }
    }
}

pub struct DuplicatesTab {
    pub state: DupState,
    pub sort: DupSort,
}

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct DuplicatesActions {
    pub refresh: bool,
    /// Every member of one group, whichever row it was asked for from.
    pub verify: Option<Vec<String>>,
}

const VERIFY_LABEL: &str = "Verify copies are identical…";
const VERIFY_TIP: &str = "Reads every file in the group in full and compares them byte for \
                          byte. Grouping only reads each file's size and how it begins.";

impl DuplicatesTab {
    pub fn new() -> DuplicatesTab {
        DuplicatesTab {
            state: DupState::NotLoaded,
            sort: DupSort::default(),
        }
    }

    /// `verify_open` greys the entry out: there is only one verify window.
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
            // Label first, by hand: `from_label` puts it after the box, which
            // reads as "Reclaimable space  Sort by".
            ui.label("Sort by:");
            egui::ComboBox::from_id_salt("dup-sort")
                .selected_text(self.sort.label())
                .show_ui(ui, |ui| {
                    for key in [DupSort::Reclaimable, DupSort::Extension] {
                        ui.selectable_value(&mut self.sort, key, key.label());
                    }
                })
                .response
                .on_hover_text(
                    "Reorders the groups already found. Which groups those are \
                     does not change: the scan always returns the ones wasting \
                     the most space.",
                );
            if loading {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label("Scanning for duplicates…");
            }
        });
        ui.separator();

        // Applied here rather than at the click, so a Refresh landing under a
        // non-default choice comes out in that order too.
        if let DupState::Loaded(loaded) = &mut self.state {
            loaded.sort(self.sort);
        }

        match &self.state {
            // `NotLoaded` survives at most the one frame before the app starts
            // the scan (`switch_tab`), and `Loading` has its spinner and label
            // in the header row above.
            DupState::NotLoaded | DupState::Loading => {}
            DupState::Error(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            DupState::Loaded(loaded) => {
                let groups = &loaded.groups;
                if groups.is_empty() {
                    ui.label("No duplicate files found.");
                    return actions;
                }
                if groups.len() == 500 {
                    ui.label(hint(match self.sort {
                        DupSort::Reclaimable => "Showing the 500 largest groups.",
                        // Said plainly: this is not every .raw file you own,
                        // it is the 500 biggest groups put in that order.
                        DupSort::Extension => "Showing the 500 largest groups, by extension.",
                    }));
                }
                let scroll = egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for &i in &loaded.order {
                            let group = &groups[i];
                            let title = loaded.titles[i].as_str();
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
                            // Also on the group's own row, whose members are
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

/// Whether the shared entry was clicked; closes the menu when it was.
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
