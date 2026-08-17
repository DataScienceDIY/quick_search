//! The app's confirmation prompts, as free render functions plus the
//! `*_ui` drivers `update()` calls in its documented order.

use super::*;

use crate::ui_util::{centered_modal, hint};

/// A button (or Esc/backdrop click) in the unsaved-changes modal.
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
                    "Unlike folders, filters and hidden files — which are applied \
                     to the existing index in place — these cannot be, so the \
                     index has to be built again. Until it is, existing entries \
                     keep the old settings.",
                ));
                ui.horizontal(|ui| {
                    if ui.button("Rebuild now").clicked() {
                        self.backend.coordinator.rebuild_index();
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
                "Each root is indexed by its own worker pool, so one root \
                 may not contain another. Fix the folder list below:",
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

    /// Tell the user their index is being replaced because it belongs to an
    /// older version. The button starts the rebuild rather than merely
    /// dismissing — in manual mode nothing else would.
    pub(super) fn stale_index_prompt_ui(&mut self, ctx: &egui::Context) {
        if !self.stale_index_prompt {
            return;
        }
        if stale_index_window(ctx, self.key_source) {
            self.stale_index_prompt = false;
            self.backend.coordinator.rebuild_index();
            self.dups.state = DupState::NotLoaded;
        }
    }

    /// Tell the user their settings have not reached the index. Work still
    /// owed means a pass was abandoned or the config was edited while the
    /// app was closed. Hidden while a run or reconcile is in progress.
    pub(super) fn reconcile_owed_ui(&mut self, ctx: &egui::Context) {
        if !self.reconcile_owed {
            return;
        }
        let state = self.backend.coordinator.state();
        // A completed run reconciles from the same record and stamps it; a
        // run the user stops does not move this.
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
                // Not cleared here: the run that finishes clears it, and one
                // that is stopped half-way leaves the reminder standing.
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
                        "Your indexed folders contain more than {} directories. The \
                         system limits how many folders can be watched for changes at \
                         once, so QuickSearch cannot update the index as files change.",
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
                WatchError::Other(msg) => {
                    ui.label(format!("Live updates are unavailable: {}", msg));
                }
            }
            ui.add_space(4.0);
            ui.label(format!(
                "The index is rebuilt every {} instead. Searches keep working; \
                 recent changes may take that long to appear.",
                fmt_interval(self.cfg.indexing.reindex_interval_minutes),
            ));
            ui.label(hint(
                "To restore live updates, index fewer folders or exclude large \
                 subfolders under Filters on the Manage Index tab.",
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
            if let Err(e) = self.cfg.save() {
                self.config_error = Some(e);
            }
        }
    }

    /// The first-start tour. Dismissal is written straight to the config, the
    /// way the fuzzy default is — not through the Settings draft, which this
    /// has nothing to do with.
    pub(super) fn tutorial_ui(&mut self, ctx: &egui::Context) {
        let Some(tour) = &mut self.tutorial else {
            return;
        };
        let roots = self.cfg.paths.indexing_paths.clone();
        if tour.ui(ctx, &roots) {
            self.tutorial = None;
            self.cfg.ui.tutorial_seen = Some(true);
            if let Err(e) = self.cfg.save() {
                self.config_error = Some(e);
            }
        }
    }

    /// Re-open the tour from the Help tab. Nothing is written until it is
    /// dismissed again, so a re-read costs the config nothing.
    pub(crate) fn show_tutorial(&mut self) {
        self.tutorial = Some(crate::tutorial::Tutorial::new());
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
                    // resurrect what was just deleted, nor the next launch
                    // undo the stop.
                    self.set_index_mode(false);
                    self.backend.coordinator.clear_index();
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

    /// Drive the unsaved-changes guard. Each frame the pending intent picks
    /// the editor to ask about; Apply and Discard clean one editor and the
    /// next frame moves on or falls through to the navigation. Cancel —
    /// button, Esc, or a backdrop click — drops the intent.
    pub(super) fn unsaved_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.pending_nav else {
            return;
        };
        let dirty = (self.manage.is_dirty(), self.settings.is_dirty(&self.cfg));
        // `self.tab` is the tab being left: the switch itself is what the
        // intent is holding back.
        let Some(source) = guard_source(intent, self.tab, dirty.0, dirty.1) else {
            // Inside the guard: the Discard-then-quit path sets
            // `quit_confirmed` and never returns to the close-request check,
            // so a warning living only there would be skipped.
            if quit_needs_reconcile_warning(intent, self.backend.coordinator.reconciling()) {
                // Repaint so a reconcile that ends takes the modal with it.
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
                let ok = match source {
                    UnsavedSource::Manage => match self.manage.take_apply_config(&self.cfg) {
                        Some(cfg) => {
                            let ok = self.apply_new_config(ctx, cfg);
                            if ok {
                                self.manage.mark_applied();
                            }
                            ok
                        }
                        None => true,
                    },
                    UnsavedSource::Settings => match self.settings.draft_config() {
                        Some(cfg) => {
                            let ok = self.apply_new_config(ctx, cfg);
                            if ok {
                                self.settings.discard();
                            }
                            ok
                        }
                        None => true,
                    },
                };
                if !ok {
                    // Rejected (nested roots): stay put, keep the staged
                    // edits; the error banner explains what to fix.
                    self.pending_nav = None;
                }
            }
        }
    }

    /// Perform a navigation the guard has cleared.
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

/// Body of the unsaved-changes guard; `Some(choice)` when the user decided
/// this frame. Esc and a click on the backdrop count as Cancel.
///
/// Unlike the centered `egui::Window` the other prompts use, `egui::Modal`'s
/// backdrop blocks input to everything behind it — a click landing on the
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

/// What the user chose in the "settings not applied yet" banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReconcileOwedChoice {
    StartIndexing,
    Dismiss,
}

/// The banner's body, as a top panel under the config-error one.
fn reconcile_owed_banner(ctx: &egui::Context) -> Option<ReconcileOwedChoice> {
    let mut choice = None;
    egui::TopBottomPanel::top("reconcile-owed").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "⚠ Your indexing settings have not been applied to the index yet.",
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

/// Body of the quit-during-a-reconcile guard; `Some(true)` to quit anyway,
/// `Some(false)` to stay. Esc and a backdrop click count as staying.
fn reconcile_quit_modal(ctx: &egui::Context) -> Option<bool> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("reconcile-quit-guard")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Settings are still being applied");
        ui.label(
            "QuickSearch is still applying your indexing settings to the index. If you \
             quit now it stops part-way, and entries you excluded can still turn up in \
             search results.",
        );
        ui.add_space(4.0);
        ui.label(
            "Nothing is lost: the next indexing run picks the work up again. In manual \
             mode, choose \"Start indexing now\" on the Manage Index tab after the next \
             launch.",
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

/// The stale-index window's body. Returns whether the user asked for the
/// rebuild.
fn stale_index_window(ctx: &egui::Context, key_source: KeySource) -> bool {
    centered_modal(ctx, "Index reset for this version", |ui| {
        ui.set_max_width(440.0);
        ui.label(
            "Your search index was created by an older version of QuickSearch, \
             which this version cannot read. It is being reset and rebuilt from \
             scratch. Don't worry, this is very Quick!",
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
        ui.button("Rebuild now").clicked()
    })
    .unwrap_or(false)
}

/// A stored/current config value for the rebuild prompt; list values are
/// already newline-joined and render as-is, empty means unset.
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

    fn frame(ctx: &egui::Context, source: KeySource, events: Vec<egui::Event>) -> bool {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut clicked = false;
        let _ = ctx.run(input, |ctx| clicked = stale_index_window(ctx, source));
        clicked
    }

    use crate::test_ui::click_at;

    /// The viewport every modal in this module is centred in.
    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    /// The stale-index modal renders under every key source and its one
    /// button reports the click that starts the rebuild.
    #[test]
    fn the_stale_index_modal_renders_and_its_button_fires() {
        for source in [
            KeySource::Unprotected,
            KeySource::Prompt,
            KeySource::Keychain,
        ] {
            let ctx = egui::Context::default();
            assert!(
                !frame(&ctx, source, Vec::new()),
                "an untouched frame must not request a rebuild"
            );

            // Sweep for the button: the window's height depends on which
            // sentence is shown.
            let mut fired = None;
            'sweep: for y in (230..480).step_by(3) {
                for x in (250..760).step_by(6) {
                    let pos = egui::pos2(x as f32, y as f32);
                    if frame(&ctx, source, click_at(pos)) {
                        fired = Some(pos);
                        break 'sweep;
                    }
                }
            }
            assert!(
                fired.is_some(),
                "no clickable Rebuild button for {source:?}"
            );
        }
    }

    fn modal_frame(
        ctx: &egui::Context,
        source: UnsavedSource,
        events: Vec<egui::Event>,
    ) -> Option<UnsavedChoice> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = unsaved_changes_modal(ctx, source));
        choice
    }

    /// Every way out of the guard reports the right choice: all three
    /// buttons fire, Esc cancels, and an untouched frame decides nothing.
    #[test]
    fn the_unsaved_modal_reports_each_choice() {
        for source in [UnsavedSource::Manage, UnsavedSource::Settings] {
            let ctx = egui::Context::default();
            assert_eq!(
                modal_frame(&ctx, source, Vec::new()),
                None,
                "an untouched frame must not decide"
            );

            let mut seen = std::collections::HashSet::new();
            for y in (250..450).step_by(3) {
                for x in (250..760).step_by(6) {
                    let pos = egui::pos2(x as f32, y as f32);
                    if let Some(choice) = modal_frame(&ctx, source, click_at(pos)) {
                        seen.insert(choice);
                    }
                }
            }
            for expected in [
                UnsavedChoice::Apply,
                UnsavedChoice::Discard,
                UnsavedChoice::Cancel,
            ] {
                assert!(
                    seen.contains(&expected),
                    "{expected:?} never fired ({source:?})"
                );
            }

            let esc = modal_frame(
                &ctx,
                source,
                vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(esc, Some(UnsavedChoice::Cancel), "Esc must cancel");
        }
    }

    fn reconcile_modal_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Option<bool> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = reconcile_quit_modal(ctx));
        choice
    }

    /// Both ways out of the quit warning work, and neither is the default.
    #[test]
    fn the_quit_warning_reports_both_answers() {
        let ctx = egui::Context::default();
        assert_eq!(
            reconcile_modal_frame(&ctx, Vec::new()),
            None,
            "an untouched frame must not decide"
        );

        let mut seen = std::collections::HashSet::new();
        for y in (250..450).step_by(3) {
            for x in (250..760).step_by(6) {
                if let Some(choice) =
                    reconcile_modal_frame(&ctx, click_at(egui::pos2(x as f32, y as f32)))
                {
                    seen.insert(choice);
                }
            }
        }
        assert!(seen.contains(&true), "\"Quit anyway\" never fired");
        assert!(seen.contains(&false), "Cancel never fired");

        let esc = reconcile_modal_frame(
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(esc, Some(false), "Esc must keep the app open");
    }

    fn banner_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Option<ReconcileOwedChoice> {
        let input = crate::test_ui::raw_input(SCREEN, events);
        let mut choice = None;
        let _ = ctx.run(input, |ctx| choice = reconcile_owed_banner(ctx));
        choice
    }

    /// Both banner buttons report their clicks.
    #[test]
    fn the_reconcile_banner_reports_both_buttons() {
        let ctx = egui::Context::default();
        assert_eq!(banner_frame(&ctx, Vec::new()), None);

        let mut seen = std::collections::HashSet::new();
        // A top panel, so it sits in the first rows of the window.
        for y in (0..60).step_by(2) {
            for x in (0..1000).step_by(4) {
                if let Some(choice) = banner_frame(&ctx, click_at(egui::pos2(x as f32, y as f32))) {
                    seen.insert(choice);
                }
            }
        }
        assert!(
            seen.contains(&ReconcileOwedChoice::StartIndexing),
            "\"Start indexing now\" never fired"
        );
        assert!(
            seen.contains(&ReconcileOwedChoice::Dismiss),
            "Dismiss never fired"
        );
    }
}
