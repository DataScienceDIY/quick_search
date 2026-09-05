//! The Duplicates tab: groups of files sharing a content hash.
//!
//! Grouping is a *suspicion*, not a verdict — the hash covers each file's size
//! and its first `processing.hash_length` bytes and nothing else. The tab says
//! so where someone about to delete something will read it, and offers the
//! byte-for-byte verification that settles it.
//!
//! Two ways to stop seeing a group again: exclude paths by glob (a folder that
//! is *meant* to hold copies), or hide one group by its hash (a head-hash
//! false positive). Both are persisted, and both are applied to the listing
//! already in hand rather than by going back to the database.

use std::collections::HashSet;

use quicksearch_core::config::{DuplicatesConfig, IgnoreSet};
use quicksearch_core::search::DuplicateGroup;

use crate::format::{group_thousands, human_size};
use crate::platform;
use crate::ui_util::{hint, stable_section};

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

/// A name's extension, lowercased; empty when it has none.
fn extension_of(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// What [`LoadedGroups::rebuild`] orders an extension listing by: no extension
/// last, then the extension, then the biggest waste, then the hash to settle
/// whatever is left.
type ExtensionKey<'a> = (bool, &'a str, std::cmp::Reverse<i64>, &'a [u8]);

fn extension_key<'a>(row: &'a Row, groups: &'a [DuplicateGroup]) -> ExtensionKey<'a> {
    (
        // Extensionless groups last: a category of their own, and not an
        // interesting one.
        row.extension.is_empty(),
        row.extension.as_str(),
        std::cmp::Reverse(row.redundant),
        groups[row.group].hash.as_slice(),
    )
}

// --- Filters ---------------------------------------------------------------

/// The tab's copy of `[duplicates]`, with the matcher it compiles to. The tab
/// owns this outright once the app has started: `app::pin_live_fields` keeps a
/// Settings draft from writing an older copy back over it.
pub struct DupFilters {
    config: DuplicatesConfig,
    exclude: IgnoreSet,
    /// Why `exclude` is empty when it should not be. Only a hand-edited config
    /// can get here — the editor refuses an invalid glob — and it is painted
    /// rather than swallowed: silently dropping the pattern would list files
    /// the user asked never to see again.
    error: Option<String>,
    hidden: HashSet<String>,
    /// Bumped by every edit; [`LoadedGroups`] rebuilds when it moves.
    revision: u64,
}

impl DupFilters {
    pub fn new(config: &DuplicatesConfig) -> DupFilters {
        let mut filters = DupFilters {
            config: config.clone(),
            exclude: IgnoreSet::compile(&[]).expect("an empty pattern set compiles"),
            error: None,
            hidden: HashSet::new(),
            revision: 0,
        };
        filters.recompile();
        filters
    }

    fn recompile(&mut self) {
        self.revision += 1;
        self.hidden = self
            .config
            .hidden_groups
            .iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect();
        match IgnoreSet::compile(&self.config.exclude_patterns) {
            Ok(set) => {
                self.exclude = set;
                self.error = None;
            }
            Err(e) => {
                self.exclude = IgnoreSet::compile(&[]).expect("an empty pattern set compiles");
                self.error = Some(e);
            }
        }
    }

    fn add_pattern(&mut self, pattern: &str) {
        let pattern = pattern.trim().to_string();
        if pattern.is_empty() || self.config.exclude_patterns.contains(&pattern) {
            return;
        }
        self.config.exclude_patterns.push(pattern);
        self.recompile();
    }

    fn remove_pattern(&mut self, i: usize) {
        if i < self.config.exclude_patterns.len() {
            self.config.exclude_patterns.remove(i);
            self.recompile();
        }
    }

    fn hide(&mut self, hash_hex: String) {
        if self.hidden.contains(&hash_hex) {
            return;
        }
        self.config.hidden_groups.push(hash_hex);
        self.recompile();
    }

    fn unhide(&mut self, hash_hex: &str) {
        self.config
            .hidden_groups
            .retain(|h| !h.trim().eq_ignore_ascii_case(hash_hex));
        self.recompile();
    }

    fn clear_hidden(&mut self) {
        self.config.hidden_groups.clear();
        self.recompile();
    }

    fn excluded(&self, path: &str) -> bool {
        !self.exclude.is_empty() && self.exclude.matches_path(std::path::Path::new(path))
    }

    fn is_hidden(&self, group: &DuplicateGroup) -> bool {
        !self.hidden.is_empty() && self.hidden.contains(&group.hash_hex())
    }
}

// --- The loaded listing ----------------------------------------------------

/// One group as listed: which of its members survived the exclusions, and the
/// totals recomputed from them.
struct Row {
    group: usize,
    /// Indices into the group's members, path-ordered.
    members: Vec<usize>,
    hidden: bool,
    title: String,
    redundant: i64,
    extension: String,
}

/// The group's totals from the members that survived, as the scan prices them:
/// every member of a hash group shares a size — the hash covers it (see
/// [`quicksearch_core::search::find_duplicate_groups`]) — so the first one
/// speaks for all of them. An unfiltered group therefore reads exactly as the
/// scan ranked it, and saturates on absurd sizes the same way.
fn totals(group: &DuplicateGroup, members: &[usize]) -> (i64, i64, i64) {
    let count = members.len() as i64;
    let size = members
        .first()
        .map(|&j| group.members[j].3.min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    (
        count,
        size.saturating_mul(count),
        size.saturating_mul(count - 1),
    )
}

/// The scan's result, with each visible group's header line already built.
/// Measured: building the titles in the render loop cost ~2,000 allocations a
/// frame, on a list that repaints at 20 Hz for as long as the tab is open.
pub struct LoadedGroups {
    groups: Vec<DuplicateGroup>,
    /// The visible groups, in display order.
    rows: Vec<Row>,
    /// Groups the filters kept off screen, for the line that says so.
    filtered_out: usize,
    /// The `(sort, filter revision, show hidden)` `rows` was built for.
    built_for: Option<(DupSort, u64, bool)>,
}

impl LoadedGroups {
    pub fn new(groups: Vec<DuplicateGroup>) -> LoadedGroups {
        LoadedGroups {
            groups,
            rows: Vec::new(),
            filtered_out: 0,
            built_for: None,
        }
    }

    /// Rebuild the visible rows. A no-op when nothing that decides them has
    /// moved, so the render loop can call it unconditionally.
    fn rebuild(&mut self, sort: DupSort, filters: &DupFilters, show_hidden: bool) {
        let key = (sort, filters.revision, show_hidden);
        if self.built_for == Some(key) {
            return;
        }
        self.built_for = Some(key);
        self.rows.clear();
        self.filtered_out = 0;

        for (i, group) in self.groups.iter().enumerate() {
            let hidden = filters.is_hidden(group);
            if hidden && !show_hidden {
                self.filtered_out += 1;
                continue;
            }
            let members: Vec<usize> = group
                .members
                .iter()
                .enumerate()
                .filter(|(_, m)| !filters.excluded(&m.2))
                .map(|(j, _)| j)
                .collect();
            // One surviving copy is not a duplicate of anything.
            if members.len() < 2 {
                self.filtered_out += 1;
                continue;
            }
            let (count, total, redundant) = totals(group, &members);
            // Copies of one file can be filed under different names, so the
            // group is named — and filed under the extension of — the member
            // its title names.
            let name = group.members[members[0]].1.as_str();
            let title = format!(
                "{}{} × {}: {} reclaimable ({} total)",
                if hidden { "Hidden — " } else { "" },
                group_thousands(count as u64),
                name,
                human_size(redundant.max(0) as u64),
                human_size(total.max(0) as u64),
            );
            self.rows.push(Row {
                group: i,
                extension: extension_of(name),
                members,
                hidden,
                title,
                redundant,
            });
        }

        // `rows` came out in the order the scan returned, which is already the
        // reclaimable one; only the extension listing has work to do.
        if sort == DupSort::Extension {
            let groups = &self.groups;
            self.rows
                .sort_by(|a, b| extension_key(a, groups).cmp(&extension_key(b, groups)));
        }
    }
}

// --- The tab ---------------------------------------------------------------

pub struct DuplicatesTab {
    pub state: DupState,
    pub sort: DupSort,
    pub filters: DupFilters,
    /// List the groups the filters hid, so hiding one is reversible.
    show_hidden: bool,
    /// What the last scan was asked for; a result of exactly this many groups
    /// is a truncated one. See [`crate::backend::Backend::start_duplicates`].
    pub scan_limit: u32,
    /// The exclusion being typed.
    draft: String,
}

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct DuplicatesActions {
    pub refresh: bool,
    /// Every surviving member of one group, whichever row it was asked for
    /// from — excluded members are not what anyone is looking at.
    pub verify: Option<Vec<String>>,
    /// The exclusions or the hidden groups changed; write them to the config.
    pub save_filters: Option<DuplicatesConfig>,
}

const VERIFY_LABEL: &str = "Verify copies are identical…";
const VERIFY_TIP: &str = "Reads every file in the group in full and compares them byte for \
                          byte. Grouping only reads each file's size and how it begins.";
const HIDE_LABEL: &str = "Hide this group";
const UNHIDE_LABEL: &str = "Unhide this group";
const NO_GROUPS: &str = "No duplicate files found.";
const ALL_FILTERED: &str = "Every duplicate group is hidden by your filters.";
/// Prose wraps to this, rather than to a maximised window's full width.
const PROSE_WIDTH: f32 = 720.0;

impl DuplicatesTab {
    pub fn new(filters: &DuplicatesConfig) -> DuplicatesTab {
        DuplicatesTab {
            state: DupState::NotLoaded,
            sort: DupSort::default(),
            filters: DupFilters::new(filters),
            show_hidden: false,
            scan_limit: 0,
            draft: String::new(),
        }
    }

    /// `verify_open` greys the verify entry out: there is only one verify
    /// window. `hash_length` is what the banner quotes as the amount of each
    /// file the grouping actually read.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        verify_open: bool,
        hash_length: usize,
    ) -> DuplicatesActions {
        let mut actions = DuplicatesActions::default();
        let mut edited = false;

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

        // Everything from here to the exclusion editor appears and disappears
        // with the state, and egui names a widget by how many precede it in
        // the same `Ui` — an unstable count would rename the editor below and
        // drop its focus mid-word. One id each, whatever they hold.
        let listing = matches!(&self.state, DupState::Loaded(l) if !l.groups.is_empty());
        stable_section(ui, |ui| {
            if listing {
                caution_banner(ui, hash_length);
            }
        });
        edited |= self.filter_ui(ui);
        ui.separator();

        // Applied here rather than at the click, so a Refresh landing under a
        // non-default choice comes out in that order too.
        if let DupState::Loaded(loaded) = &mut self.state {
            loaded.rebuild(self.sort, &self.filters, self.show_hidden);
        }

        // The menus mutate the filters, which the listing below is borrowed
        // from; they report what they were asked for and it is applied after.
        let mut hide: Option<String> = None;
        let mut unhide: Option<String> = None;
        let mut exclude: Option<String> = None;

        match &self.state {
            // `NotLoaded` survives at most the one frame before the app starts
            // the scan (`switch_tab`), and `Loading` has its spinner and label
            // in the header row above.
            DupState::NotLoaded | DupState::Loading => {}
            DupState::Error(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            // Nothing here may return early: the filter edits collected above
            // are applied at the bottom of this function, and skipping that is
            // how an exclusion that empties the list loses itself.
            DupState::Loaded(loaded) if loaded.groups.is_empty() => {
                ui.label(NO_GROUPS);
            }
            DupState::Loaded(loaded) if loaded.rows.is_empty() => {
                ui.label(ALL_FILTERED);
            }
            DupState::Loaded(loaded) => {
                // "Limited to", not "showing": the scan asks for extra to
                // cover the hidden set, so the number it found is not always
                // the number on screen. The filtered-out line below accounts
                // for the difference.
                if loaded.groups.len() as u32 >= self.scan_limit && self.scan_limit > 0 {
                    ui.label(hint(match self.sort {
                        DupSort::Reclaimable => {
                            format!("Limited to the {} largest groups.", self.scan_limit)
                        }
                        // Said plainly: this is not every .raw file you own,
                        // it is the biggest groups put in that order.
                        DupSort::Extension => format!(
                            "Limited to the {} largest groups, then ordered by extension.",
                            self.scan_limit
                        ),
                    }));
                }
                if loaded.filtered_out > 0 {
                    ui.label(hint(format!(
                        "{} more hidden by your filters.",
                        group_thousands(loaded.filtered_out as u64)
                    )));
                }

                let scroll = egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for row in &loaded.rows {
                            let group = &loaded.groups[row.group];
                            let header = egui::CollapsingHeader::new(&row.title)
                                .id_salt(row.group)
                                .show(ui, |ui| {
                                    for &j in &row.members {
                                        let (_, _, path, size, _) = &group.members[j];
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
                                                if ui.button("Open containing folder").clicked() {
                                                    platform::reveal_in_folder(path);
                                                    ui.close();
                                                }
                                                ui.separator();
                                                if verify_entry(ui, verify_open) {
                                                    actions.verify = Some(member_paths(group, row));
                                                }
                                                ui.separator();
                                                if hide_entry(ui, row.hidden) {
                                                    let hex = group.hash_hex();
                                                    if row.hidden {
                                                        unhide = Some(hex);
                                                    } else {
                                                        hide = Some(hex);
                                                    }
                                                }
                                                if let Some(p) = exclude_entries(ui, path) {
                                                    exclude = Some(p);
                                                }
                                            });
                                        });
                                    }
                                });
                            // Also on the group's own row, whose members are
                            // behind a collapsed header until they are not.
                            header.header_response.context_menu(|ui| {
                                if verify_entry(ui, verify_open) {
                                    actions.verify = Some(member_paths(group, row));
                                }
                                ui.separator();
                                if hide_entry(ui, row.hidden) {
                                    let hex = group.hash_hex();
                                    if row.hidden {
                                        unhide = Some(hex);
                                    } else {
                                        hide = Some(hex);
                                    }
                                }
                            });
                        }
                    });
                crate::ui_util::more_below_hint(ui, &scroll);
            }
        }

        if let Some(hex) = hide {
            self.filters.hide(hex);
            edited = true;
        }
        if let Some(hex) = unhide {
            self.filters.unhide(&hex);
            edited = true;
        }
        if let Some(pattern) = exclude {
            self.filters.add_pattern(&pattern);
            edited = true;
        }
        if edited {
            actions.save_filters = Some(self.filters.config.clone());
        }
        actions
    }

    /// The exclusion editor, its chips, and the hidden-group controls.
    /// `true` when something here changed the filters.
    fn filter_ui(&mut self, ui: &mut egui::Ui) -> bool {
        use crate::ui_util::{pattern_edit, pattern_hint_label};
        let mut edited = false;
        let mut add: Option<String> = None;
        let mut remove: Option<usize> = None;

        ui.horizontal_wrapped(|ui| {
            ui.label("Exclude:")
                .on_hover_text("Files whose path matches are left out of the listing entirely.");
            let (response, valid) = pattern_edit(ui, &mut self.draft, 200.0, "name, path or glob");
            let entered =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && valid;
            if ui
                .add_enabled(valid, egui::Button::new("Add"))
                .on_disabled_hover_text(if self.draft.trim().is_empty() {
                    "Type a name, path or glob first."
                } else {
                    "That is not a valid pattern."
                })
                .clicked()
                || entered
            {
                add = Some(std::mem::take(&mut self.draft));
            }
            for (i, pattern) in self.filters.config.exclude_patterns.iter().enumerate() {
                if ui
                    .small_button(format!("{} ×", pattern))
                    .on_hover_text("Stop excluding this")
                    .clicked()
                {
                    remove = Some(i);
                }
            }
        });
        pattern_hint_label(ui, &self.draft);

        let error = self.filters.error.clone();
        stable_section(ui, |ui| {
            if let Some(e) = &error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Exclusions are not being applied: {e}"),
                );
            }
        });

        let hidden = self.filters.config.hidden_groups.len();
        let show_hidden = &mut self.show_hidden;
        let filters = &mut self.filters;
        stable_section(ui, |ui| {
            if hidden == 0 {
                return;
            }
            ui.horizontal(|ui| {
                ui.label(hint(format!(
                    "Hidden: {} group{}",
                    group_thousands(hidden as u64),
                    if hidden == 1 { "" } else { "s" }
                )));
                ui.checkbox(show_hidden, "Show hidden");
                if ui
                    .small_button("Clear")
                    .on_hover_text("List every hidden group again")
                    .clicked()
                {
                    filters.clear_hidden();
                    edited = true;
                }
            });
        });

        if let Some(pattern) = add {
            self.filters.add_pattern(&pattern);
            edited = true;
        }
        if let Some(i) = remove {
            self.filters.remove_pattern(i);
            edited = true;
        }
        edited
    }
}

/// The tab's standing caution, painted above the list rather than left to a
/// right-click nobody has performed yet: the group is a suspicion, and the way
/// to settle it is right here.
fn caution_banner(ui: &mut egui::Ui, hash_length: usize) {
    let p = crate::color::palette(ui.visuals().dark_mode);
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, p.orange))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            // Only ever a cap, never a floor: `set_max_width` widens a `Ui` as
            // readily as it narrows one, and a column set wider than the panel
            // lays its text out past the clip rect, where the ends of the
            // lines are simply cut off.
            ui.set_max_width(ui.available_width().min(PROSE_WIDTH));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(egui::RichText::new("These are suspected duplicates. ").color(p.orange));
                ui.label(format!(
                    "Files are grouped on their size and their first {}, which is all \
                     indexing reads, and why it can be so fast. This is not proof that they are identical! \
                     Right-click any group and choose \"{}\" to compare every byte before \
                     you delete anything.",
                    human_size(hash_length as u64),
                    VERIFY_LABEL.trim_end_matches('…'),
                ));
            });
        });
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

/// The hide/unhide entry, in whichever direction this group is in. `true` when
/// it was clicked; which way round is the caller's to read from `hidden`.
fn hide_entry(ui: &mut egui::Ui, hidden: bool) -> bool {
    let (label, tip) = if hidden {
        (UNHIDE_LABEL, "List this group again.")
    } else {
        (
            HIDE_LABEL,
            "Keep this group out of the listing from now on, including after a \
             restart. Nothing is deleted, and \"Show hidden\" brings it back.",
        )
    };
    let clicked = ui.button(label).on_hover_text(tip).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

/// The two exclusions a member row can offer, and the pattern the one that was
/// clicked stands for.
fn exclude_entries(ui: &mut egui::Ui, path: &str) -> Option<String> {
    let path = std::path::Path::new(path);
    let mut chosen: Option<String> = None;
    if let Some(ext) = path.extension() {
        let pattern = format!("*.{}", ext.to_string_lossy());
        if ui
            .button(format!("Exclude {pattern} from duplicates"))
            .on_hover_text("Files of this type are left out of the listing; they stay indexed.")
            .clicked()
        {
            chosen = Some(pattern);
        }
    }
    if let Some(dir) = path.parent() {
        let pattern = crate::ui_util::dir_ignore_pattern(dir);
        if ui
            .button("Exclude this folder from duplicates")
            .on_hover_text(&pattern)
            .clicked()
        {
            chosen = Some(pattern);
        }
    }
    if chosen.is_some() {
        ui.close();
    }
    chosen
}

/// The members of `group` that survived the exclusions — verifying files
/// nobody is being shown would answer a question nobody asked.
fn member_paths(group: &DuplicateGroup, row: &Row) -> Vec<String> {
    row.members
        .iter()
        .map(|&j| group.members[j].2.clone())
        .collect()
}

#[cfg(test)]
mod tests;
