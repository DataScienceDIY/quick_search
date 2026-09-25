//! The app's confirmation prompts, as free render functions plus the
//! `*_ui` drivers `update()` calls in its documented order.

use super::*;

use crate::ui_util::{centered_modal, hint};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UnsavedChoice {
    Apply,
    Discard,
    Cancel,
}

impl QuickSearchApp {
    pub(super) fn rebuild_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(changes) = &self.rebuild_prompt else {
            return;
        };
        let changes = changes.clone();
        let mut close = false;
        // Not `centered_modal`: the columns below need `default_width`.
        egui::Window::new("Rebuild index?")
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("These settings differ from what the index was built with:");
                ui.add_space(4.0);
                if changes.is_empty() {
                    ui.monospace("the index cannot be read with the new settings");
                }
                for change in &changes {
                    ui.strong(format!("{}:", change.key));
                    ui.columns(2, |cols| {
                        cols[0].label(hint("index was built with"));
                        cols[0].monospace(display_value(&change.stored));
                        cols[1].label(hint("config now says"));
                        cols[1].monospace(display_value(&change.current));
                    });
                    ui.add_space(6.0);
                }
                ui.label(hint(
                    "These settings cannot be applied to the existing index, so it \
                     must be rebuilt. Until then, existing entries keep the old \
                     settings.",
                ));
                ui.horizontal(|ui| {
                    if ui.button("Rebuild now").clicked() {
                        self.backend.rebuild_index();
                        close = true;
                    }
                    if ui.button("Later").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.rebuild_prompt = None;
        }
    }

    pub(super) fn nested_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(pairs) = &self.nested_prompt else {
            return;
        };
        let pairs = pairs.clone();
        let close = centered_modal(ctx, "Indexed folders may not be nested", |ui| {
            ui.label(
                "Each indexed folder is indexed on its own, so one may not \
                 contain another. These overlap:",
            );
            for (child, parent) in &pairs {
                ui.monospace(format!("{}  ⊂  {}", child, parent));
            }
            ui.label(hint(
                "Indexing stays paused until the overlap is removed and \
                 the list is applied.",
            ));
            ui.button("Fix folders").clicked()
        });
        if close == Some(true) {
            self.nested_prompt = None;
            self.switch_tab(ctx, Tab::Manage);
        }
    }

    /// The button also starts the rebuild when nothing else will (see
    /// [`stale_prompt_should_command`]).
    pub(super) fn stale_index_prompt_ui(&mut self, ctx: &egui::Context) {
        if !self.stale_index_prompt {
            return;
        }
        let command = stale_prompt_should_command(
            self.backend.coordinator.state().mode,
            self.backend.coordinator.is_indexing(),
        );
        if stale_index_window(ctx, self.key_source, command) {
            self.stale_index_prompt = false;
            if command {
                self.backend.rebuild_index();
            }
            // Either way the index is being replaced under the tab.
            self.dups.state = DupState::NotLoaded;
        }
    }

    /// Work still owed means a pass was abandoned or the config was edited
    /// while the app was closed. Hidden while a run or reconcile is going.
    pub(super) fn reconcile_owed_ui(&mut self, ctx: &egui::Context) {
        if !self.reconcile_owed {
            return;
        }
        let state = self.backend.coordinator.state();
        // A completed run stamps this; a run the user stops does not.
        if state.last_full_index > self.reconcile_owed_since {
            self.reconcile_owed = false;
            return;
        }
        if !matches!(
            state.activity,
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) || state.reconcile.is_some()
        {
            return;
        }
        match reconcile_owed_banner(ctx) {
            None => {}
            Some(ReconcileOwedChoice::StartIndexing) => {
                self.backend.coordinator.reindex_now();
                // The run that finishes clears it; one stopped half-way
                // leaves the reminder standing.
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Some(ReconcileOwedChoice::Dismiss) => self.reconcile_owed = false,
        }
    }

    pub(super) fn watch_cap_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(reason) = &self.watch_cap_prompt else {
            return;
        };
        let reason = reason.clone();
        let close = centered_modal(ctx, "Live index updating is disabled", |ui| {
            ui.set_max_width(420.0);
            match &reason {
                WatchError::TooManyDirectories { cap, .. } => {
                    ui.label(format!(
                        "Your indexed folders contain more than {} directories, more \
                         than can be watched for changes at once, so QuickSearch \
                         cannot update the index as files change.",
                        group_thousands(*cap as u64),
                    ));
                }
                WatchError::KernelLimit { registered } => {
                    ui.label(format!(
                        "The system ran out of folder watches after {} directories, so \
                         QuickSearch cannot update the index as files change.",
                        group_thousands(*registered as u64),
                    ));
                }
                // Unreachable: an overflow schedules a full run instead of
                // disabling live updates. Matched so a new variant is a
                // compile error here.
                WatchError::Overflowed => {
                    ui.label(
                        "The system dropped some change notifications, so the index \
                         is being rebuilt to catch up.",
                    );
                }
                WatchError::Other(msg) => {
                    ui.label(format!("Live updates are unavailable: {}", msg));
                }
            }
            ui.add_space(4.0);
            ui.label(format!(
                "Instead, a full reindex runs every {}. Searches keep working, \
                 but recent changes can take that long to appear.",
                fmt_interval(self.cfg.indexing.reindex_interval_minutes),
            ));
            ui.label(hint(
                "To restore live updates, index fewer folders or exclude large \
                 subfolders under Content filters on the Manage Index tab.",
            ));
            ui.button("OK").clicked()
        });
        if close == Some(true) {
            self.watch_cap_prompt = None;
            for root in &self.cfg.paths.indexing_paths {
                if !self.cfg.ui.watch_cap_warned_roots.contains(root) {
                    self.cfg.ui.watch_cap_warned_roots.push(root.clone());
                }
            }
            self.save_cfg();
        }
    }

    /// Dismissal is written straight to the config, not the Settings draft.
    pub(super) fn tutorial_ui(&mut self, ctx: &egui::Context) {
        let Some(tour) = &mut self.tutorial else {
            return;
        };
        let roots = self.cfg.paths.indexing_paths.clone();
        let hotkey = self.cfg.ui.search_hotkey.clone();
        let actions = tour.ui(ctx, &roots, &hotkey);
        // Through the same path a keystroke takes: `seed` arms the debounce,
        // so the demonstration search runs once the typing stops.
        if let Some(query) = actions.set_query {
            self.search.seed(query);
        }
        if actions.focus_search {
            self.search.request_focus();
        }
        // Sent only by the page's Apply button, so it applies and saves
        // together, as the Settings tab's Apply does.
        if let Some(scale) = actions.set_scale {
            self.cfg.ui.scale = scale;
            ctx.set_zoom_factor(super::clamp_scale(scale));
            self.save_cfg();
        }
        // Registered as it is captured: the tour has no Apply, and the page
        // says it takes effect at once.
        if let Some(hotkey) = actions.set_hotkey {
            crate::hotkey::apply(&hotkey);
            // The system-wide binding follows, where one is written.
            crate::shortcut_setup::hotkey_changed(&hotkey);
            self.cfg.ui.search_hotkey = hotkey;
            self.save_cfg();
        }
        // Through the guard, so a dirty draft still gets its say.
        if let Some(tab) = actions.goto_tab {
            self.request_tab(ctx, tab);
        }
        if actions.dismissed {
            self.tutorial = None;
            self.cfg.ui.tutorial_seen = Some(true);
            self.save_cfg();
        }
    }

    /// Re-open the tour from the Help tab.
    pub(crate) fn show_tutorial(&mut self) {
        self.tutorial = Some(crate::tutorial::Tutorial::default());
    }

    pub(super) fn clear_prompt_ui(&mut self, ctx: &egui::Context) {
        if !self.clear_prompt {
            return;
        }
        let close = centered_modal(ctx, "Clear index?", |ui| {
            ui.label("This deletes the search index database. Your files are not touched.");
            ui.label(hint(
                "Indexing switches to manual until you start it again or return \
                 to automatic mode.",
            ));
            ui.horizontal(|ui| {
                let delete = egui::RichText::new("Delete index").color(ui.visuals().error_fg_color);
                if ui.button(delete).clicked() {
                    // Manual first, and persisted: automatic mode must not
                    // resurrect what was just deleted.
                    self.set_index_mode(false);
                    self.backend.clear_index();
                    self.dups.state = DupState::NotLoaded;
                    return true;
                }
                ui.button("Cancel").clicked()
            })
            .inner
        });
        if close == Some(true) {
            self.clear_prompt = false;
        }
    }

    /// Each frame the pending intent picks the editor to ask about; Apply
    /// and Discard clean one editor and the next frame moves on. Cancel
    /// (button, Esc, or backdrop click) drops the intent.
    pub(super) fn unsaved_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.pending_nav else {
            return;
        };
        let dirty = (self.manage.is_dirty(), self.settings.is_dirty(&self.cfg));
        let Some(source) = guard_source(intent, self.tab, dirty.0, dirty.1) else {
            // Inside the guard: the Discard-then-quit path never returns to
            // the close-request check, so a warning only there is skipped.
            // Quitting mid-reconcile leaves excluded entries in the index; a
            // tab switch does not end the pass.
            if intent == NavIntent::Quit && self.backend.coordinator.reconciling() {
                ctx.request_repaint_after(Duration::from_millis(250));
                match reconcile_quit_modal(ctx) {
                    None => return,
                    Some(false) => {
                        self.pending_nav = None;
                        return;
                    }
                    Some(true) => {}
                }
            }
            return self.complete_nav(ctx, intent);
        };
        match unsaved_changes_modal(ctx, source) {
            None => {}
            Some(UnsavedChoice::Cancel) => self.pending_nav = None,
            Some(UnsavedChoice::Discard) => match source {
                UnsavedSource::Manage => self.manage.discard(),
                UnsavedSource::Settings => self.settings.discard(),
            },
            Some(UnsavedChoice::Apply) => {
                let draft = match source {
                    UnsavedSource::Manage => self.manage.take_apply_config(&self.cfg),
                    UnsavedSource::Settings => self.settings.draft_config(),
                };
                match draft.map(|cfg| self.apply_new_config(ctx, cfg)) {
                    None => {}
                    Some(true) => match source {
                        UnsavedSource::Manage => self.manage.mark_applied(),
                        UnsavedSource::Settings => self.settings.discard(),
                    },
                    // Rejected: stay put, keep the staged edits.
                    Some(false) => self.pending_nav = None,
                }
            }
        }
    }

    pub(super) fn complete_nav(&mut self, ctx: &egui::Context, intent: NavIntent) {
        self.pending_nav = None;
        match intent {
            NavIntent::SwitchTab(tab) => self.switch_tab(ctx, tab),
            NavIntent::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// Esc and a backdrop click count as Cancel. `egui::Modal`, not the usual
/// centered window: its backdrop blocks input, and a click landing on the
/// tab strip would re-trigger or bypass the guard.
fn unsaved_changes_modal(ctx: &egui::Context, source: UnsavedSource) -> Option<UnsavedChoice> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("unsaved-guard")).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.heading("Unsaved changes");
        ui.label(match source {
            UnsavedSource::Manage => "The Manage Index tab has edits that have not been applied.",
            UnsavedSource::Settings => "The Settings tab has edits that have not been applied.",
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add(crate::ui_util::bordered_button(
                    "Apply & Save",
                    palette(ui.visuals().dark_mode).blue,
                ))
                .clicked()
            {
                choice = Some(UnsavedChoice::Apply);
            }
            if ui
                .button(egui::RichText::new("Discard changes").color(ui.visuals().error_fg_color))
                .clicked()
            {
                choice = Some(UnsavedChoice::Discard);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(UnsavedChoice::Cancel);
            }
        });
    });
    if choice.is_none() && modal.should_close() {
        choice = Some(UnsavedChoice::Cancel);
    }
    choice
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReconcileOwedChoice {
    StartIndexing,
    Dismiss,
}

fn reconcile_owed_banner(ctx: &egui::Context) -> Option<ReconcileOwedChoice> {
    let mut choice = None;
    egui::TopBottomPanel::top("reconcile-owed").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Your indexing settings have not been applied to the index yet.",
            );
            if ui.small_button("Start indexing now").clicked() {
                choice = Some(ReconcileOwedChoice::StartIndexing);
            }
            if ui.small_button("Dismiss").clicked() {
                choice = Some(ReconcileOwedChoice::Dismiss);
            }
        });
    });
    choice
}

/// `Some(true)` to quit anyway; Esc and a backdrop click count as staying.
fn reconcile_quit_modal(ctx: &egui::Context) -> Option<bool> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("reconcile-quit-guard")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Settings are still being applied");
        ui.label(
            "QuickSearch is still applying your indexing settings. If you quit now \
             it stops part-way, and entries you excluded can still turn up in \
             results.",
        );
        ui.add_space(4.0);
        ui.label(
            "Nothing is lost: the next indexing run finishes the job. In manual \
             mode, click \"Start indexing now\" on the Manage Index tab after you \
             reopen QuickSearch.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button(egui::RichText::new("Quit anyway").color(ui.visuals().error_fg_color))
                .clicked()
            {
                choice = Some(true);
            }
            if ui
                .add(crate::ui_util::bordered_button(
                    "Cancel",
                    palette(ui.visuals().dark_mode).blue,
                ))
                .clicked()
            {
                choice = Some(false);
            }
        });
    });
    if choice.is_none() && modal.should_close() {
        choice = Some(false);
    }
    choice
}

/// Whether the stale-index button has to command the rebuild itself.
///
/// In Auto the coordinator already does: the recreated index has no
/// `last_full_index`, so its first tick schedules a full run. Sending
/// `RebuildIndex` on top of that deletes the rebuild in progress and starts
/// it over from zero. Only manual mode needs the button to do anything.
pub(super) fn stale_prompt_should_command(mode: IndexMode, indexing: bool) -> bool {
    mode != IndexMode::Auto && !indexing
}

fn stale_index_window(ctx: &egui::Context, key_source: KeySource, command: bool) -> bool {
    centered_modal(ctx, "Index reset for this version", |ui| {
        ui.set_max_width(440.0);
        ui.label(
            "This version of QuickSearch cannot read your existing index, so it \
             is being rebuilt from scratch.",
        );
        let reassurance = match key_source {
            KeySource::Unprotected => None,
            KeySource::Prompt => Some(
                "The rebuilt index is encrypted with the password you just \
                 entered, so it stays password protected.",
            ),
            KeySource::Keychain => Some(
                "The rebuilt index is encrypted with the key remembered on \
                 this device, so it stays password protected.",
            ),
        };
        if let Some(text) = reassurance {
            ui.add_space(4.0);
            ui.label(text);
        }
        ui.add_space(4.0);
        ui.label(hint(
            "Your files are not touched. Searches return incomplete results \
             until the rebuild finishes; progress is on the Manage Index tab.",
        ));
        ui.add_space(4.0);
        ui.button(if command { "Rebuild now" } else { "Continue" })
            .clicked()
    })
    .unwrap_or(false)
}

fn display_value(value: &str) -> String {
    if value.trim().is_empty() {
        "(none)".to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_ui::{click_at, key_press, painted_text_center, two_pass};

    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    /// One frame of `show`: what was painted, and what it decided.
    fn frame<T: Default>(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut show: impl FnMut(&egui::Context) -> T,
    ) -> (egui::FullOutput, T) {
        let mut decided = T::default();
        let out = ctx.run(crate::test_ui::raw_input(SCREEN, events), |ctx| {
            decided = show(ctx)
        });
        (out, decided)
    }

    /// Clicks `label` once the prompt has settled (a window is measured on
    /// its first frame and placed on the next) and returns what the click
    /// decided. Neither settling frame may decide anything.
    fn click<T: Default + PartialEq + std::fmt::Debug>(
        label: &str,
        mut show: impl FnMut(&egui::Context) -> T,
    ) -> T {
        let ctx = crate::test_ui::ctx();
        let out = two_pass(&ctx, SCREEN, Vec::new(), |ctx| {
            let decided = show(ctx);
            assert_eq!(decided, T::default(), "an untouched frame must not decide");
        });
        let pos = painted_text_center(&out, label).unwrap_or_else(|| panic!("no {label:?}"));
        frame(&ctx, click_at(pos), &mut show).1
    }

    /// What Esc decides once the prompt has settled.
    fn escape<T: Default>(mut show: impl FnMut(&egui::Context) -> T) -> T {
        let ctx = crate::test_ui::ctx();
        frame(&ctx, Vec::new(), &mut show);
        frame(&ctx, Vec::new(), &mut show);
        let esc = key_press(egui::Key::Escape, egui::Modifiers::NONE);
        frame(&ctx, esc, &mut show).1
    }

    #[test]
    fn the_stale_index_modal_renders_and_its_button_fires() {
        for source in [
            KeySource::Unprotected,
            KeySource::Prompt,
            KeySource::Keychain,
        ] {
            // "Rebuild now" when the click starts the rebuild, "Continue"
            // when one is already running.
            for (command, label) in [(true, "Rebuild now"), (false, "Continue")] {
                assert!(
                    click(label, |ctx| stale_index_window(ctx, source, command)),
                    "{label:?} did not fire for {source:?}"
                );
            }
        }
    }

    #[test]
    fn the_unsaved_modal_reports_each_choice() {
        for source in [UnsavedSource::Manage, UnsavedSource::Settings] {
            for (label, expected) in [
                ("Apply & Save", UnsavedChoice::Apply),
                ("Discard changes", UnsavedChoice::Discard),
                ("Cancel", UnsavedChoice::Cancel),
            ] {
                assert_eq!(
                    click(label, |ctx| unsaved_changes_modal(ctx, source)),
                    Some(expected),
                    "{label:?} ({source:?})"
                );
            }
            assert_eq!(
                escape(|ctx| unsaved_changes_modal(ctx, source)),
                Some(UnsavedChoice::Cancel),
                "Esc must cancel"
            );
        }
    }

    #[test]
    fn the_quit_warning_reports_both_answers() {
        assert_eq!(click("Quit anyway", reconcile_quit_modal), Some(true));
        assert_eq!(click("Cancel", reconcile_quit_modal), Some(false));
        assert_eq!(
            escape(reconcile_quit_modal),
            Some(false),
            "Esc must keep the app open"
        );
    }

    #[test]
    fn the_reconcile_banner_reports_both_buttons() {
        assert_eq!(
            click("Start indexing now", reconcile_owed_banner),
            Some(ReconcileOwedChoice::StartIndexing)
        );
        assert_eq!(
            click("Dismiss", reconcile_owed_banner),
            Some(ReconcileOwedChoice::Dismiss)
        );
    }
}
