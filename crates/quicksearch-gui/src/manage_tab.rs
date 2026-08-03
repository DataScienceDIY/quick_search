//! The Manage Index tab: detailed status, mode controls, indexed roots,
//! and the content/ignore filter editors.

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::{IndexMode, IndexerState, WatcherStatus};
use quicksearch_core::indexing::{IndexingStatus, RootPhase, RootProgress};

use crate::format::{fmt_interval, fmt_rate, group_thousands, middle_truncate};
use crate::options::{config_editor_ui, Section};
use crate::tracker::SpeedTracker;

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct ManageActions {
    pub start_now: bool,
    pub stop: bool,
    pub auto: bool,
    /// Ask the app to confirm and delete the index.
    pub clear_index: bool,
    /// A full edited config to apply (roots / filters / indexing knobs).
    pub apply_config: Option<Config>,
}

pub struct ManageTab {
    pub speed: SpeedTracker,
    /// Multiline editor, one extension per line; synced from config and
    /// parsed back on Apply.
    ext_filter_text: String,
    new_root: String,
    /// Text of the inline "add ignore pattern" box.
    new_ignore: String,
    /// Inline error from a rejected root add (nested/duplicate).
    root_error: Option<String>,
    /// The config the draft was last synced from. `None` forces a full
    /// resync (first frame, and right after our own Apply).
    baseline: Option<Config>,
    /// Draft of the roots/filters/indexing knobs edited in-place.
    draft: Option<Config>,
}

impl ManageTab {
    pub fn new() -> ManageTab {
        ManageTab {
            speed: SpeedTracker::new(),
            ext_filter_text: String::new(),
            new_root: String::new(),
            new_ignore: String::new(),
            root_error: None,
            baseline: None,
            draft: None,
        }
    }

    /// Feed the tracker from the polled status (called every frame, on
    /// every tab, so the status bar rate stays live).
    pub fn observe(&mut self, status: &IndexingStatus) {
        match status {
            IndexingStatus::Running { roots, .. } => {
                // Monotonic within a run: walks and extractions only grow.
                let total: usize = roots.iter().map(|r| r.walked + r.extracted).sum();
                self.speed.record(total);
            }
            IndexingStatus::Idle | IndexingStatus::Error(_) => self.speed.reset(),
            _ => {}
        }
    }

    /// Reconcile the draft with the live config, every frame. This is what
    /// keeps the ignore-pattern list realtime: a filter persisted from the
    /// Search tab shows up here on the next frame, staged edits or not.
    fn sync_editors(&mut self, config: &Config) {
        let Some(baseline) = &self.baseline else {
            // First frame, or right after our own Apply.
            return self.resync(config);
        };
        if baseline == config {
            return;
        }
        // The config changed elsewhere (Options apply, a filter persisted
        // from the Search tab, the fuzzy toggle's direct save…).
        let dirty = self.draft.as_ref() != Some(baseline)
            || self.ext_filter_text != baseline.indexing.content_extensions.join("\n");
        if !dirty {
            // Nothing staged, nothing to lose.
            return self.resync(config);
        }
        // Staged edits exist: keep the sections this tab edits, adopt
        // everything else so a later Apply cannot revert changes made
        // elsewhere, and fold in ignore patterns added externally.
        let draft = self.draft.take().expect("synced");
        let mut merged = config.clone();
        merged.paths.indexing_paths = draft.paths.indexing_paths;
        merged.indexing = draft.indexing;
        merged.processing = draft.processing;
        for pat in &config.indexing.ignore_patterns {
            if !baseline.indexing.ignore_patterns.contains(pat)
                && !merged.indexing.ignore_patterns.contains(pat)
            {
                merged.indexing.ignore_patterns.push(pat.clone());
            }
        }
        self.draft = Some(merged);
        self.baseline = Some(config.clone());
    }

    fn resync(&mut self, config: &Config) {
        self.ext_filter_text = config.indexing.content_extensions.join("\n");
        self.draft = Some(config.clone());
        self.baseline = Some(config.clone());
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        state: &IndexerState,
        config: &Config,
    ) -> ManageActions {
        let mut actions = ManageActions::default();
        self.sync_editors(config);

        let scroll = egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            // --- Status ---------------------------------------------------
            ui.heading("Status");
            status_panel(ui, state, &self.speed);
            watch_panel(ui, state, config);
            ui.add_space(8.0);

            // --- Controls -------------------------------------------------
            ui.horizontal(|ui| {
                let running = !matches!(
                    state.activity,
                    IndexingStatus::Idle | IndexingStatus::Error(_)
                );
                if ui.add_enabled(!running, egui::Button::new("Start indexing now")).clicked() {
                    actions.start_now = true;
                }
                if ui.add_enabled(running || state.mode == IndexMode::Auto, egui::Button::new("Stop")).clicked() {
                    actions.stop = true;
                }
                if ui
                    .add_enabled(state.mode != IndexMode::Auto, egui::Button::new("Return to Automatic"))
                    .clicked()
                {
                    actions.auto = true;
                }
                let mode = match state.mode {
                    IndexMode::Auto => "Automatic",
                    IndexMode::ManualStopped => "Manual (stopped)",
                    IndexMode::ManualRunning => "Manual (running)",
                };
                ui.label(egui::RichText::new(format!("Mode: {}", mode)).weak());
                ui.separator();
                if ui
                    .button(egui::RichText::new("Clear index…").color(ui.visuals().error_fg_color))
                    .on_hover_text("Delete the index database (asks for confirmation)")
                    .clicked()
                {
                    actions.clear_index = true;
                }
                if state.queued_events > 0 {
                    ui.label(
                        egui::RichText::new(format!("{} changes queued", state.queued_events))
                            .small()
                            .weak(),
                    );
                }
            });
            ui.separator();

            // --- Indexed roots ---------------------------------------------
            ui.heading("Indexed folders");
            let draft = self.draft.as_mut().expect("synced");
            let mut remove: Option<usize> = None;
            for (i, root) in draft.paths.indexing_paths.clone().iter().enumerate() {
                ui.horizontal(|ui| {
                    // Controls claim the right edge first so a long path can
                    // never push them out of view; the path truncates into
                    // whatever width remains (full path on hover).
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(i);
                        }
                        // Per-root walker override; 0 = auto (4 local / 16
                        // network, detected per root). Applies on the next run.
                        let mut workers =
                            draft.indexing.root_workers.get(root).copied().unwrap_or(0);
                        let response = ui
                            .add(
                                egui::DragValue::new(&mut workers)
                                    .range(0..=64)
                                    .custom_formatter(|n, _| {
                                        if n == 0.0 {
                                            "auto".to_string()
                                        } else {
                                            format!("{:.0}", n)
                                        }
                                    })
                                    .custom_parser(|s| {
                                        let s = s.trim();
                                        if s.is_empty() || s.eq_ignore_ascii_case("auto") {
                                            Some(0.0)
                                        } else {
                                            s.parse().ok()
                                        }
                                    }),
                            )
                            .on_hover_text(
                                "Walker threads for this folder. auto = 4 on local \
                                 storage, 16 on network mounts. Takes effect on \
                                 the next indexing run.",
                            );
                        if response.changed() {
                            if workers == 0 {
                                draft.indexing.root_workers.remove(root);
                            } else {
                                draft.indexing.root_workers.insert(root.clone(), workers);
                            }
                        }
                        ui.label(egui::RichText::new("workers:").small().weak());

                        // Path label takes the leftover width, middle-truncated.
                        ui.with_layout(
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                                let char_width =
                                    ui.fonts(|f| f.glyph_width(&font_id, '0')).max(1.0);
                                let budget =
                                    ((ui.available_width() / char_width) as usize).max(16);
                                ui.monospace(middle_truncate(root, budget))
                                    .on_hover_text(root);
                            },
                        );
                    });
                });
            }
            if let Some(i) = remove {
                let removed = draft.paths.indexing_paths.remove(i);
                draft.indexing.root_workers.remove(&removed);
            }
            ui.horizontal(|ui| {
                if ui.button("Add folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let path = dir.to_string_lossy().into_owned();
                        try_add_root(draft, path, &mut self.root_error);
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_root)
                        .desired_width(240.0)
                        .hint_text("or type a path"),
                );
                if ui.button("Add").clicked() && !self.new_root.trim().is_empty() {
                    let path = self.new_root.trim().to_string();
                    if try_add_root(draft, path, &mut self.root_error) {
                        self.new_root.clear();
                    }
                }
            });
            if let Some(err) = &self.root_error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            ui.separator();

            // --- Filters ---------------------------------------------------
            ui.heading("Content filters");
            ui.columns(2, |cols| {
                cols[0].label("Full-text extensions (empty = all supported):");
                cols[0].add(
                    egui::TextEdit::multiline(&mut self.ext_filter_text)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .hint_text("txt\nmd\npdf"),
                );
                cols[1].label("Ignore patterns (excluded entirely):");
                let mut remove_pat: Option<usize> = None;
                for (i, pat) in draft.indexing.ignore_patterns.iter().enumerate() {
                    cols[1].horizontal(|ui| {
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("Remove").clicked() {
                                    remove_pat = Some(i);
                                }
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.monospace(pat);
                                    },
                                );
                            },
                        );
                    });
                }
                if draft.indexing.ignore_patterns.is_empty() {
                    cols[1].label(egui::RichText::new("No ignore patterns.").small().weak());
                }
                if let Some(i) = remove_pat {
                    draft.indexing.ignore_patterns.remove(i);
                }
                cols[1].horizontal(|ui| {
                    let (response, valid) = crate::ui_util::pattern_edit(
                        ui,
                        &mut self.new_ignore,
                        180.0,
                        "*.tmp or node_modules",
                    );
                    let submitted =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add_enabled(valid, egui::Button::new("Add")).clicked()
                        || (submitted && valid)
                    {
                        let pat = self.new_ignore.trim().to_string();
                        if !draft.indexing.ignore_patterns.contains(&pat) {
                            draft.indexing.ignore_patterns.push(pat);
                        }
                        self.new_ignore.clear();
                    }
                });
                cols[1].label(
                    egui::RichText::new(
                        "Changes apply on Apply & Save (may trigger a rebuild). \
                         Session-only filters are shown and removed on the Search tab.",
                    )
                    .small()
                    .weak(),
                );
            });
            ui.separator();

            // --- Indexing options -------------------------------------------
            ui.heading("Indexing options");
            config_editor_ui(ui, draft, Section::Indexing);
            ui.add_space(4.0);
            config_editor_ui(ui, draft, Section::Processing);
            ui.add_space(8.0);

            if ui
                .add(crate::ui_util::bordered_button(
                    "Apply & Save",
                    crate::ui_util::BLUE,
                ))
                .clicked()
            {
                let mut new_config = draft.clone();
                new_config.indexing.content_extensions = parse_lines(&self.ext_filter_text);
                let roots = new_config.paths.indexing_paths.clone();
                new_config
                    .indexing
                    .root_workers
                    .retain(|root, _| roots.contains(root));
                actions.apply_config = Some(new_config);
                self.baseline = None;
            }
        });
        crate::ui_util::more_below_hint(ui, &scroll);

        actions
    }
}

/// Append a root to the draft unless it would duplicate or nest with an
/// existing one; the rejection reason lands in `error`.
fn try_add_root(draft: &mut Config, candidate: String, error: &mut Option<String>) -> bool {
    if draft.paths.indexing_paths.contains(&candidate) {
        *error = Some(format!("{} is already in the list", candidate));
        return false;
    }
    let mut probe = draft.paths.indexing_paths.clone();
    probe.push(candidate.clone());
    if let Some((child, parent)) = quicksearch_core::config::nested_roots(&probe).first() {
        *error = Some(format!(
            "Not added: {} is nested under {}; indexed folders may not overlap",
            child, parent
        ));
        return false;
    }
    draft.paths.indexing_paths.push(candidate);
    *error = None;
    true
}

fn parse_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Live-update health. Permanent counterpart to the one-time modal: the
/// modal is dismissed and remembered per root, but "live updates are off"
/// stays true and must remain discoverable.
fn watch_panel(ui: &mut egui::Ui, state: &IndexerState, config: &Config) {
    match &state.watcher {
        // Manual mode already says "stopped" in the controls row; repeating
        // it here would be noise.
        WatcherStatus::Off => {}
        WatcherStatus::Starting => {
            ui.label(
                egui::RichText::new("Setting up live updates…")
                    .small()
                    .weak(),
            );
        }
        WatcherStatus::Active { dirs } => {
            ui.label(
                egui::RichText::new(format!(
                    "Live updates on, watching {} folders",
                    group_thousands(*dirs as u64)
                ))
                .small()
                .weak(),
            );
        }
        WatcherStatus::Disabled { reason } => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "⚠ Live updates off; reindexing every {}",
                    fmt_interval(config.indexing.reindex_interval_minutes)
                ),
            )
            .on_hover_text(reason.to_string());
        }
    }
}

fn status_panel(ui: &mut egui::Ui, state: &IndexerState, speed: &SpeedTracker) {
    match &state.activity {
        IndexingStatus::Idle => {
            // Relative wording makes even a milliseconds-fast run visibly
            // register ("just now") instead of looking like a dead button.
            let last = state
                .last_full_index
                .map(crate::format::fmt_ago)
                .unwrap_or_else(|| "never".to_string());
            ui.label(format!("Idle; last full index: {}", last));
        }
        IndexingStatus::Error(e) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("Error: {}", e));
        }
        IndexingStatus::Stopping => {
            ui.label("Stopping…");
        }
        IndexingStatus::Running { roots, .. } => {
            for root in roots {
                root_row(ui, root);
            }
            if let Some(rate) = speed.files_per_sec() {
                ui.label(
                    egui::RichText::new(format!("overall: {}", fmt_rate(rate)))
                        .small()
                        .weak(),
                );
            }
        }
    }
}

/// One root's progress: path, phase, bar, counters, current file.
fn root_row(ui: &mut egui::Ui, r: &RootProgress) {
    // Weak "|" separators split the row into folder | status | numbers.
    let divider = |ui: &mut egui::Ui| {
        ui.label(egui::RichText::new("|").weak());
    };
    ui.horizontal(|ui| {
        ui.monospace(middle_truncate(&r.root, 48));
        divider(ui);
        match r.phase {
            RootPhase::Walking => {
                ui.label("indexing");
                divider(ui);
                let workers = format!("{}/{} workers", r.active_workers, r.total_workers);
                match r.walk_total {
                    Some(total) if total > 0 => {
                        let frac = (r.walked as f32 / total as f32).clamp(0.0, 1.0);
                        ui.label(format!(
                            "{} / {} ({:.0}%) · {}",
                            group_thousands(r.walked as u64),
                            group_thousands(total as u64),
                            frac * 100.0,
                            workers
                        ));
                        ui.add(egui::ProgressBar::new(frac).desired_width(160.0));
                    }
                    _ => {
                        ui.label(format!(
                            "{} files · {}",
                            group_thousands(r.walked as u64),
                            workers
                        ));
                        ui.add(egui::ProgressBar::new(0.0).animate(true).desired_width(160.0));
                    }
                }
            }
            RootPhase::Extracting => {
                ui.label("extracting text for search");
                divider(ui);
                let frac = if r.extract_total > 0 {
                    (r.extracted as f32 / r.extract_total as f32).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                ui.label(format!(
                    "{} / {} ({:.0}%)",
                    group_thousands(r.extracted as u64),
                    group_thousands(r.extract_total as u64),
                    frac * 100.0
                ));
                ui.add(egui::ProgressBar::new(frac).desired_width(160.0));
            }
            RootPhase::Done => {
                // Whole-root totals: `walked` covers every file the walk
                // saw (including unchanged, skipped ones) and `extracted`
                // covers all rows with searchable text, not just this
                // run's new work.
                ui.label("done");
                divider(ui);
                ui.label(format!(
                    "indexed {}, extracted {}",
                    group_thousands(r.walked as u64),
                    group_thousands(r.extracted as u64)
                ));
                ui.add(egui::ProgressBar::new(1.0).desired_width(160.0));
            }
        }
    });
    if let Some(f) = &r.current_file {
        ui.label(egui::RichText::new(middle_truncate(f, 90)).small().weak());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced_tab(config: &Config) -> ManageTab {
        let mut tab = ManageTab::new();
        tab.sync_editors(config);
        tab
    }

    #[test]
    fn identical_config_leaves_draft_untouched() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        // Stage an edit, then sync against the unchanged config.
        tab.draft.as_mut().unwrap().indexing.ignore_patterns.push("*.log".into());
        tab.sync_editors(&cfg);
        assert!(tab
            .draft
            .as_ref()
            .unwrap()
            .indexing
            .ignore_patterns
            .contains(&"*.log".to_string()));
    }

    #[test]
    fn clean_draft_adopts_external_changes_wholesale() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        let mut external = cfg.clone();
        external.indexing.ignore_patterns.push("*.log".into());
        external.search.fuzzy_default = !external.search.fuzzy_default;
        tab.sync_editors(&external);
        assert_eq!(tab.draft.as_ref().unwrap(), &external);
        assert_eq!(tab.baseline.as_ref().unwrap(), &external);
    }

    #[test]
    fn staged_edits_survive_an_external_persist() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        // Stage a removal of the first default pattern.
        let removed = tab
            .draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .remove(0);
        // Meanwhile the Search tab persists a new filter.
        let mut external = cfg.clone();
        external.indexing.ignore_patterns.push("*.log".into());
        tab.sync_editors(&external);
        let draft = tab.draft.as_ref().unwrap();
        assert!(!draft.indexing.ignore_patterns.contains(&removed));
        assert!(draft.indexing.ignore_patterns.contains(&"*.log".to_string()));
        assert_eq!(tab.baseline.as_ref().unwrap(), &external);
    }

    #[test]
    fn dirty_draft_adopts_sections_owned_elsewhere() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        tab.draft.as_mut().unwrap().indexing.ignore_patterns.push("*.bak".into());
        // The fuzzy toggle saves the config directly, outside this tab.
        let mut external = cfg.clone();
        external.search.fuzzy_default = !cfg.search.fuzzy_default;
        tab.sync_editors(&external);
        let draft = tab.draft.as_ref().unwrap();
        assert_eq!(draft.search.fuzzy_default, external.search.fuzzy_default);
        assert!(draft.indexing.ignore_patterns.contains(&"*.bak".to_string()));
    }

    #[test]
    fn external_pattern_is_not_duplicated_into_a_draft_that_has_it() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        tab.draft.as_mut().unwrap().indexing.ignore_patterns.push("*.log".into());
        let mut external = cfg.clone();
        external.indexing.ignore_patterns.push("*.log".into());
        tab.sync_editors(&external);
        let count = tab
            .draft
            .as_ref()
            .unwrap()
            .indexing
            .ignore_patterns
            .iter()
            .filter(|p| p.as_str() == "*.log")
            .count();
        assert_eq!(count, 1);
    }
}
