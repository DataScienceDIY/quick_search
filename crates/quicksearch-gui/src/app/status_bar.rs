//! The bottom status bar and the watch-cap warning it feeds.

use super::*;

use crate::ui_util::hint;

impl QuickSearchApp {
    /// Raise the "live updates are disabled" modal. Keyed on roots: a
    /// restart stays quiet, but adding a folder warns again.
    fn check_watch_cap_warning(&mut self, state: &IndexerState) {
        let WatcherStatus::Disabled { reason } = &state.watcher else {
            self.watch_cap_prompt = None;
            return;
        };
        // Only the budget limits warrant a modal; other failures are transient.
        if !matches!(
            reason,
            WatchError::TooManyDirectories { .. } | WatchError::KernelLimit { .. }
        ) {
            return;
        }
        if self.watch_cap_prompt.is_some() {
            return;
        }
        let unwarned = self
            .cfg
            .paths
            .indexing_paths
            .iter()
            .any(|root| !self.cfg.ui.watch_cap_warned_roots.contains(root));
        if unwarned {
            self.watch_cap_prompt = Some(reason.clone());
        }
    }

    pub(super) fn status_bar(&mut self, ctx: &egui::Context) {
        let state = self.backend.coordinator.state();
        self.manage.observe(&state.activity);
        self.check_watch_cap_warning(&state);

        let panel = egui::TopBottomPanel::bottom("status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match &state.activity {
                    // A coordinator reconcile runs with activity `Idle`.
                    IndexingStatus::Idle if state.reconcile.is_some() => {
                        match state.reconcile.expect("matched Some") {
                            ReconcileState::Running(r) => {
                                ui.label(
                                    egui::RichText::new(match (r.total, r.fraction()) {
                                        (Some(total), Some(frac)) => format!(
                                            "Applying configuration change · {} / {} ({:.0}%)",
                                            group_thousands(r.examined as u64),
                                            group_thousands(total as u64),
                                            frac * 100.0
                                        ),
                                        _ => format!(
                                            "Applying configuration change · {} entries",
                                            group_thousands(r.examined as u64)
                                        ),
                                    })
                                    .small(),
                                );
                                progress_widget(ui, r.fraction());
                            }
                            ReconcileState::Finished(r) => {
                                ui.label(
                                    egui::RichText::new(crate::format::fmt_reconcile_summary(
                                        r.deleted,
                                        r.recontented,
                                    ))
                                    .small(),
                                );
                            }
                        }
                    }
                    IndexingStatus::Preparing { start_time, step } => {
                        let (label, frac) = match step {
                            PrepStep::PreviousRun => {
                                ("Finishing the previous run…".to_string(), None)
                            }
                            PrepStep::OpeningIndex => ("Opening the index…".to_string(), None),
                            PrepStep::Starting => ("Getting the index ready…".to_string(), None),
                            PrepStep::Reconciling(r) => (
                                format!(
                                    "Applying configuration change · {} entries",
                                    group_thousands(r.examined as u64)
                                ),
                                r.fraction(),
                            ),
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                label,
                                crate::format::fmt_duration_clock(start_time.elapsed())
                            ))
                            .small(),
                        );
                        progress_widget(ui, frac);
                    }
                    IndexingStatus::Idle => {
                        let colors = palette(ui.visuals().dark_mode);
                        status_line(
                            ui,
                            &idle_line(state.mode, state.files.unwrap_or(0), &colors),
                        );
                    }
                    IndexingStatus::Error(e) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            egui::RichText::new(format!("Indexing error: {}", e)).small(),
                        );
                    }
                    IndexingStatus::Stopping => {
                        ui.label(egui::RichText::new("Stopping indexing…").small());
                    }
                    IndexingStatus::Optimizing => {
                        ui.label(egui::RichText::new("Optimizing index…").small());
                    }
                    IndexingStatus::Running {
                        roots, maintenance, ..
                    } => {
                        let colors = palette(ui.visuals().dark_mode);
                        let rate = self.manage.speed.files_per_sec();
                        status_line(ui, &running_line(roots, *maintenance, rate, &colors));
                        progress_widget(ui, overall_progress(roots).fraction());
                    }
                }

                // In a right-to-left layout the first widget added is rightmost.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(hint(crate::version::BUILD_ID))
                        .on_hover_text(crate::version::BUILD_ID_HINT);
                    if self.tab == Tab::Search {
                        if let Some(label) = self.search.result_count_label() {
                            ui.label(hint("·"));
                            ui.label(hint(label));
                        }
                    }
                });
            });
        });
        // The whole bar, not its contents: the tour's page is about the line
        // along the bottom of the window.
        crate::spotlight::mark(ctx, crate::spotlight::Spot::StatusBar, panel.response.rect);

        // The reconcile clause is not redundant: without it the counters
        // freeze mid-scan, since that pass runs with activity `Idle`.
        if !matches!(
            state.activity,
            IndexingStatus::Idle | IndexingStatus::Error(_)
        ) || state.reconcile.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        // Watcher registration walks every root, so its verdict can land
        // minutes after startup, with no input to repaint on.
        if matches!(state.watcher, WatcherStatus::Starting) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// One run of status text and its hint; `None` is the theme's text color.
type Span = (String, Option<egui::Color32>);

/// Colored spans painted as one widget, so the segments keep the spacing of
/// a single label.
fn status_line(ui: &mut egui::Ui, spans: &[Span]) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let default = ui.visuals().text_color();
    let mut job = egui::text::LayoutJob::default();
    for (text, color) in spans {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: color.unwrap_or(default),
                ..Default::default()
            },
        );
    }
    ui.label(job);
}

/// The bottom bar's line for a run in progress. An upkeep step outranks the
/// per-root phases: while one runs it is the only thing moving.
fn running_line(
    roots: &[RootProgress],
    maintenance: Option<MaintenanceStep>,
    rate: Option<f64>,
    colors: &Palette,
) -> Vec<Span> {
    let (word, phase) = match maintenance {
        Some(_) => ("Maintenance", colors.orange),
        None if roots.iter().any(|r| r.phase == RootPhase::Walking) => ("Indexing", colors.yellow),
        None => ("Indexing", colors.green),
    };
    let done = roots.iter().filter(|r| r.phase == RootPhase::Done).count();
    let progress = overall_progress(roots);
    let mut rest = match (progress.total, progress.fraction()) {
        (Some(total), Some(frac)) => format!(
            " {} / {} ({:.0}%)",
            group_thousands(progress.processed as u64),
            group_thousands(total as u64),
            frac * 100.0
        ),
        _ => format!(" · {} files", group_thousands(progress.processed as u64)),
    };
    if roots.len() > 1 {
        rest.push_str(&format!(" · {}/{} roots done", done, roots.len()));
    }
    if let Some(rate) = rate {
        rest.push_str(&format!(" · {}", crate::format::fmt_rate(rate)));
    }
    let active: usize = roots.iter().map(|r| r.active_workers).sum();
    let total_workers: usize = roots.iter().map(|r| r.total_workers).sum();
    if total_workers > 0 {
        rest.push_str(&format!(" · {}/{} workers", active, total_workers));
    }
    if let Some(step) = maintenance {
        // Last, and without the ellipsis the tab's own line carries: the bar
        // is one sentence, not a heading.
        rest.push_str(&format!(
            " · {}",
            crate::format::fmt_maintenance(step).trim_end_matches('…')
        ));
    }
    vec![(word.to_string(), Some(phase)), (rest, None)]
}

fn idle_line(mode: IndexMode, files: i64, colors: &Palette) -> Vec<Span> {
    let (mode_text, mode_color) = match mode {
        IndexMode::Auto => ("Auto", None),
        IndexMode::ManualStopped | IndexMode::ManualRunning => ("Manual", Some(colors.orange)),
    };
    vec![
        ("Idle · ".to_string(), None),
        (mode_text.to_string(), mode_color),
        (
            format!(" · {} files indexed", group_thousands(files.max(0) as u64)),
            None,
        ),
    ]
}

fn progress_widget(ui: &mut egui::Ui, fraction: Option<f64>) {
    match fraction {
        Some(frac) => {
            ui.add(egui::ProgressBar::new(frac as f32).desired_width(120.0));
        }
        None => {
            ui.add(egui::Spinner::new().size(12.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(phase: RootPhase, walked: usize, walk_total: Option<usize>) -> RootProgress {
        RootProgress {
            root: "/data".to_string(),
            phase,
            walked,
            walk_total,
            extracted: 0,
            extract_total: None,
            current_file: None,
            active_workers: 2,
            total_workers: 4,
        }
    }

    fn line(spans: &[Span]) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    /// The spacing comes from the text, not from egui's item spacing.
    #[test]
    fn the_running_line_reads_as_one_sentence() {
        let colors = palette(true);

        assert_eq!(
            line(&running_line(
                &[root(RootPhase::Walking, 100, Some(1000))],
                None,
                None,
                &colors
            )),
            "Indexing 100 / 1,000 (10%) · 2/4 workers"
        );

        assert_eq!(
            line(&running_line(
                &[root(RootPhase::Walking, 100, None)],
                None,
                None,
                &colors
            )),
            "Indexing · 100 files · 2/4 workers"
        );

        let mut extracting = root(RootPhase::Extracting, 1_000, None);
        extracting.extracted = 200;
        extracting.extract_total = Some(800);
        extracting.active_workers = 3;
        let mut done = root(RootPhase::Done, 500, None);
        done.extracted = 500;
        done.extract_total = Some(500);
        done.active_workers = 0;
        done.total_workers = 0;
        assert_eq!(
            line(&running_line(
                &[extracting, done],
                None,
                Some(120.0),
                &colors
            )),
            "Indexing 2,200 / 2,800 (79%) · 1/2 roots done · 120 files/s · 3/4 workers"
        );
    }

    /// The counters are still the run's last true position; only the phase
    /// word and the trailing clause say that nothing is moving.
    #[test]
    fn an_upkeep_step_renames_the_phase_and_names_itself() {
        let colors = palette(true);
        let spans = running_line(
            &[root(RootPhase::Extracting, 100, Some(1000))],
            Some(MaintenanceStep::Checkpoint),
            None,
            &colors,
        );
        assert_eq!(
            line(&spans),
            "Maintenance 100 / 100 (100%) · 2/4 workers · Compacting the write-ahead log"
        );
        assert_eq!(spans[0].1, Some(colors.orange));
    }

    #[test]
    fn only_the_phase_word_of_the_running_line_is_hinted() {
        for dark in [true, false] {
            let colors = palette(dark);
            let spans = running_line(
                &[root(RootPhase::Walking, 100, Some(1000))],
                None,
                None,
                &colors,
            );
            assert_eq!(spans[0].0, "Indexing");
            assert_eq!(spans[0].1, Some(colors.yellow), "dark_mode={}", dark);
            assert!(
                spans[1..].iter().all(|(_, color)| color.is_none()),
                "the counters carry a hint: {:?}",
                spans
            );
        }
    }

    #[test]
    fn the_running_hint_follows_the_least_advanced_root() {
        let colors = palette(true);
        let hint = |roots: &[RootProgress]| running_line(roots, None, None, &colors)[0].1;

        assert_eq!(
            hint(&[
                root(RootPhase::Extracting, 100, None),
                root(RootPhase::Walking, 100, Some(1000)),
                root(RootPhase::Done, 100, None),
            ]),
            Some(colors.yellow)
        );
        assert_eq!(
            hint(&[
                root(RootPhase::Extracting, 100, None),
                root(RootPhase::Done, 100, None),
            ]),
            Some(colors.green)
        );
        assert_eq!(
            hint(&[root(RootPhase::Done, 100, None)]),
            Some(colors.green)
        );
        // Upkeep outranks every phase: it is the only thing running.
        for phase in [RootPhase::Walking, RootPhase::Extracting, RootPhase::Done] {
            assert_eq!(
                running_line(
                    &[root(phase, 100, Some(1000))],
                    Some(MaintenanceStep::MergingText),
                    None,
                    &colors
                )[0]
                .1,
                Some(colors.orange),
                "{:?}",
                phase
            );
        }
    }

    #[test]
    fn only_manual_idle_is_hinted() {
        for dark in [true, false] {
            let colors = palette(dark);

            let auto = idle_line(IndexMode::Auto, 12_000, &colors);
            assert_eq!(line(&auto), "Idle · Auto · 12,000 files indexed");
            assert!(
                auto.iter().all(|(_, color)| color.is_none()),
                "automatic mode is not a warning: {:?}",
                auto
            );

            for mode in [IndexMode::ManualStopped, IndexMode::ManualRunning] {
                let spans = idle_line(mode, 12_000, &colors);
                assert_eq!(line(&spans), "Idle · Manual · 12,000 files indexed");
                let hinted: Vec<_> = spans.iter().filter(|(_, c)| c.is_some()).collect();
                assert_eq!(
                    hinted,
                    vec![&("Manual".to_string(), Some(colors.orange))],
                    "{:?} in dark_mode={}",
                    mode,
                    dark
                );
            }
        }
    }

    #[test]
    fn a_negative_file_count_reads_as_zero() {
        let colors = palette(true);
        assert_eq!(
            line(&idle_line(IndexMode::Auto, -1, &colors)),
            "Idle · Auto · 0 files indexed"
        );
    }
}
