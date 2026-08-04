//! The Manage Index tab: detailed status, mode controls, indexed roots,
//! and the content/ignore filter editors.

use std::path::Path;
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::{IndexMode, IndexerState, WatcherStatus};
use quicksearch_core::indexing::{IndexingStatus, RootPhase, RootProgress};

use crate::format::{fmt_interval, fmt_rate, group_thousands, human_size, middle_truncate};
use crate::tracker::SpeedTracker;

/// What the tab asks the app to do after this frame.
#[derive(Default)]
pub struct ManageActions {
    pub start_now: bool,
    pub stop: bool,
    pub auto: bool,
    /// Ask the app to confirm and delete the index.
    pub clear_index: bool,
    /// A full edited config to apply (roots / filters).
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
    /// Draft of the roots/filters edited in-place.
    draft: Option<Config>,
    /// Cached on-disk footprint of the index, restatted on a timer.
    db_size: DbSizeProbe,
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
            db_size: DbSizeProbe::default(),
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
            IndexingStatus::Idle | IndexingStatus::Error(_) | IndexingStatus::Optimizing => {
                self.speed.reset()
            }
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
        if !self.is_dirty() {
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
        // Live state, not a user edit: the mode buttons write `auto_index`
        // straight to the config, and a stale copy frozen into the draft
        // here would read as permanently dirty.
        merged.indexing.auto_index = config.indexing.auto_index;
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

    /// Whether the editors hold changes not yet applied.
    ///
    /// `security` and `indexing.auto_index` are neutralized before comparing
    /// — they are live state the app pins on apply (`pin_live_fields`), not
    /// user edits — and the extension text is compared parsed, so a trailing
    /// newline never reads as dirty. False before the first sync.
    pub fn is_dirty(&self) -> bool {
        let (Some(draft), Some(baseline)) = (&self.draft, &self.baseline) else {
            return false;
        };
        let mut d = draft.clone();
        crate::app::pin_live_fields(&mut d, baseline);
        d != *baseline
            || parse_lines(&self.ext_filter_text)
                != parse_lines(&baseline.indexing.content_extensions.join("\n"))
    }

    /// The Apply & Save action, callable from the app's unsaved-changes
    /// modal as well as the button. Syncs against `live` first — the same
    /// merge the per-frame sync does — so a config applied elsewhere moments
    /// ago is not reverted. Does NOT clear `baseline`: the app calls
    /// [`ManageTab::mark_applied`] only after the apply succeeds, so a
    /// rejected apply (nested roots) keeps the staged edits.
    pub fn take_apply_config(&mut self, live: &Config) -> Option<Config> {
        self.sync_editors(live);
        let draft = self.draft.as_ref()?;
        let mut new_config = draft.clone();
        new_config.indexing.content_extensions = parse_lines(&self.ext_filter_text);
        let roots = new_config.paths.indexing_paths.clone();
        new_config
            .indexing
            .root_workers
            .retain(|root, _| roots.contains(root));
        Some(new_config)
    }

    /// The last apply landed: resync from the applied config next frame.
    pub fn mark_applied(&mut self) {
        self.baseline = None;
    }

    /// Drop every staged edit and editor box; the next frame resyncs from
    /// the live config.
    pub fn discard(&mut self) {
        self.draft = None;
        self.baseline = None;
        self.new_root.clear();
        self.new_ignore.clear();
        self.root_error = None;
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        state: &IndexerState,
        config: &Config,
    ) -> ManageActions {
        let mut actions = ManageActions::default();
        self.sync_editors(config);

        let scroll = egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                // --- Status ---------------------------------------------------
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Status").strong());
                    // Right-aligned, clear of the progress text below it —
                    // that text changes width every frame during a run.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        db_size_label(ui, self.db_size.size(config, Instant::now()));
                    });
                });
                status_panel(ui, state, &self.speed);
                watch_panel(ui, state, config);
                ui.add_space(8.0);

                // --- Controls -------------------------------------------------
                ui.horizontal(|ui| {
                    let running = !matches!(
                        state.activity,
                        IndexingStatus::Idle | IndexingStatus::Error(_)
                    );
                    if ui
                        .add_enabled(!running, egui::Button::new("Start indexing now"))
                        .clicked()
                    {
                        actions.start_now = true;
                    }
                    if ui
                        .add_enabled(
                            running || state.mode == IndexMode::Auto,
                            egui::Button::new("Stop"),
                        )
                        .on_hover_text(
                            "Stop indexing and switch to manual. Saved right away: it \
                         stays manual on the next launch too.",
                        )
                        .clicked()
                    {
                        actions.stop = true;
                    }
                    if ui
                        .add_enabled(
                            state.mode != IndexMode::Auto,
                            egui::Button::new("Return to Automatic"),
                        )
                        .on_hover_text(
                            "Watch for changes and reindex periodically again. Also \
                         saved, so this is how the app starts from now on.",
                        )
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
                        .button(
                            egui::RichText::new("Clear index…").color(ui.visuals().error_fg_color),
                        )
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
                ui.heading(egui::RichText::new("Indexed folders").strong());
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
                            #[cfg(test)]
                            tests::record_widget("workers", &response);
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
                crate::ui_util::stable_section(ui, |ui| {
                    if let Some(err) = &self.root_error {
                        ui.colored_label(ui.visuals().error_fg_color, err);
                    }
                });
                ui.separator();

                // --- Filters ---------------------------------------------------
                ui.heading(egui::RichText::new("Content filters").strong());
                ui.columns(2, |cols| {
                    cols[0].label("Full-text extensions whitelist (empty = all supported):");
                    cols[0]
                        .add(
                            egui::TextEdit::multiline(&mut self.ext_filter_text)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .hint_text("txt\nmd\npdf  # comments allowed\n(none)"),
                        )
                        .on_hover_text(
                            "One extension per line, leading dot optional. A non-empty \
                             list also excludes files that have no extension at all \
                             (Makefile, README, .bashrc) — add the line \"(none)\" to \
                             keep extracting text from those.\n\n\
                             \"#\" starts a comment, either on its own line or after an \
                             entry, so a type can be commented out without losing it.",
                        );
                    cols[1].label("Ignore patterns (excluded entirely):");
                    let mut remove_pat: Option<usize> = None;
                    // The list grows and shrinks — including from outside
                    // this tab — so it is kept off the id of the editor
                    // below it (see `ui_util::stable_section`).
                    crate::ui_util::stable_section(&mut cols[1], |ui| {
                        for (i, pat) in draft.indexing.ignore_patterns.iter().enumerate() {
                            ui.horizontal(|ui| {
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
                            ui.label(egui::RichText::new("No ignore patterns.").small().weak());
                        }
                    });
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
                    crate::ui_util::pattern_hint_label(&mut cols[1], &self.new_ignore);
                    cols[1].label(
                        egui::RichText::new(
                            "Changes apply on Apply & Save (may trigger index rebuild).",
                        )
                        .small()
                        .weak(),
                    );
                });
                ui.separator();

                // The indexing/processing knobs themselves live only in the
                // Options window; this points at them so the tab does not look
                // like the whole story.
                ui.label(
                    egui::RichText::new(
                        "Reindex interval, symlinks, hidden files, tokenizer, and size \
                     limits are in Options (⚙ in the toolbar).",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(8.0);

                let dirty = self.is_dirty();
                ui.horizontal(|ui| {
                    let apply = ui.add(crate::ui_util::bordered_button(
                        "Apply & Save",
                        if dirty {
                            crate::ui_util::ORANGE
                        } else {
                            crate::ui_util::BLUE
                        },
                    ));
                    #[cfg(test)]
                    tests::record_widget("apply", &apply);
                    // The label comes and goes with the dirty state; keep it
                    // off the ids of whatever sits after it.
                    crate::ui_util::stable_section(ui, |ui| {
                        if dirty {
                            ui.label(
                                egui::RichText::new("Unsaved changes")
                                    .small()
                                    .color(crate::ui_util::ORANGE),
                            );
                        }
                    });
                    if apply.clicked() {
                        actions.apply_config = self.take_apply_config(config);
                    }
                });
            });
        crate::ui_util::more_below_hint(ui, &scroll);

        // Nothing else asks for repaints while the app sits idle, so without
        // this the size would freeze at whatever it read when the pointer
        // last moved. One frame per interval, and only while this tab is the
        // one on screen.
        ui.ctx().request_repaint_after(DB_SIZE_REFRESH);

        actions
    }
}

/// How often the index files are re-statted. The number moves slowly even
/// during a run, so this is deliberately lazy: it costs three stats and one
/// repaint, and asking any more often would buy nothing.
const DB_SIZE_REFRESH: Duration = Duration::from_secs(10);

/// Total on-disk footprint of the index: the database plus its `-wal` and
/// `-shm` sidecars. The `-wal` file is why the database alone will not do —
/// mid-run it can hold hundreds of megabytes the database does not show yet.
///
/// A file that is not there counts as zero: there is no database at all
/// before the first run, and the sidecars exist only while a connection is
/// open.
fn measure_db_size(db: &Path) -> u64 {
    // Only regular files: a misconfigured path pointing at a directory
    // would otherwise report that directory's own inode size as an index.
    let len = |path: &Path| {
        std::fs::metadata(path)
            .map(|m| if m.is_file() { m.len() } else { 0 })
            .unwrap_or(0)
    };
    let name = db.file_name().and_then(|s| s.to_str()).unwrap_or("");
    // No `-journal`: the index runs in WAL mode, so a rollback journal is
    // not part of a live database.
    len(db)
        + ["-wal", "-shm"]
            .iter()
            .map(|suffix| len(&db.with_file_name(format!("{}{}", name, suffix))))
            .sum::<u64>()
}

/// Caches the last measurement so the tab can ask for it every frame.
#[derive(Default)]
struct DbSizeProbe {
    /// Configured (unresolved) path the cached size belongs to.
    path: String,
    bytes: u64,
    measured_at: Option<Instant>,
}

impl DbSizeProbe {
    /// The cached size, restatted when it has gone stale or the configured
    /// database path changed under it. `now` is a parameter so the refresh
    /// cadence can be tested without sleeping.
    fn size(&mut self, config: &Config, now: Instant) -> u64 {
        let expired = self
            .measured_at
            .is_none_or(|at| now.duration_since(at) >= DB_SIZE_REFRESH);
        if expired || self.path != config.paths.database_path {
            // Resolving the path is only worth doing on a real refresh.
            self.bytes = measure_db_size(&config.resolved_database_path());
            self.path = config.paths.database_path.clone();
            self.measured_at = Some(now);
        }
        self.bytes
    }
}

/// The index's footprint, with the levers for shrinking it on hover: a user
/// who finds the number too large looks here first, and every lever named
/// is either on this tab or in Options.
fn db_size_label(ui: &mut egui::Ui, bytes: u64) {
    let response = ui.label(format!("Index size: {}", human_size(bytes)));
    #[cfg(test)]
    tests::record_widget("db-size", &response);
    response.on_hover_ui(db_size_tooltip);
}

fn db_size_tooltip(ui: &mut egui::Ui) {
    ui.set_max_width(440.0);
    ui.strong("To reduce the index size");
    for lever in [
        "Add ignore filters for files and folders you never search — the ignore \
         pattern list further down this tab.",
        "Remove indexed folders you do not need, in Indexed folders above.",
        "Narrow the full-text extension whitelist, so text is only extracted \
         from the file types you actually search.",
        "Turn off \"Store text for snippets\" in Options: full-text search keeps \
         working, but without previews, occurrence ranking or fuzzy matching \
         inside file contents.",
        "Lower \"Max text file size\" and \"Max stored text\" in Options.",
    ] {
        ui.label(format!("•  {}", lever));
    }
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(
            "The file does not shrink on its own: freed space is reused by the \
             index rather than returned to the disk. To hand it back after \
             narrowing the filters, use Clear index… and reindex.",
        )
        .small()
        .weak(),
    );
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
///
/// Wrapped for the same reason as [`status_panel`]: this panel is empty in
/// manual mode and a line long otherwise.
fn watch_panel(ui: &mut egui::Ui, state: &IndexerState, config: &Config) {
    crate::ui_util::stable_section(ui, |ui| watch_contents(ui, state, config));
}

fn watch_contents(ui: &mut egui::Ui, state: &IndexerState, config: &Config) {
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

/// Live progress. Its widget count tracks the run (roots come and go, the
/// rate line and the current file appear and vanish), so it renders inside
/// a [`crate::ui_util::stable_section`]: without one, every id below it —
/// including the per-root worker fields — would move mid-run.
fn status_panel(ui: &mut egui::Ui, state: &IndexerState, speed: &SpeedTracker) {
    crate::ui_util::stable_section(ui, |ui| status_contents(ui, state, speed));
}

fn status_contents(ui: &mut egui::Ui, state: &IndexerState, speed: &SpeedTracker) {
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
        IndexingStatus::Optimizing => {
            // No per-file progress exists to show: this is one bulk rewrite
            // of the whole file, and on a large index it runs for minutes.
            ui.label("Optimizing index; reclaiming unused space…");
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
                match r.walk_denominator() {
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
                        ui.add(
                            egui::ProgressBar::new(0.0)
                                .animate(true)
                                .desired_width(160.0),
                        );
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
                    "{} / {} ({:.0}%) · {}/{} workers",
                    group_thousands(r.extracted as u64),
                    group_thousands(r.extract_total as u64),
                    frac * 100.0,
                    r.active_workers,
                    r.total_workers
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

    use std::cell::RefCell;

    // Driving the real tab through a headless egui context is the only way
    // to test widget identity, and identity is exactly what these tests are
    // about — so the widgets under test report themselves here.
    thread_local! {
        static WIDGETS: RefCell<Vec<(&'static str, egui::Id, egui::Rect)>> =
            const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_widget(tag: &'static str, response: &egui::Response) {
        WIDGETS.with(|w| w.borrow_mut().push((tag, response.id, response.rect)));
    }

    fn widget(tag: &str) -> (egui::Id, egui::Rect) {
        WIDGETS.with(|w| {
            w.borrow()
                .iter()
                .find(|(t, _, _)| *t == tag)
                .map(|(_, id, rect)| (*id, *rect))
                .unwrap_or_else(|| panic!("{} widget was not drawn", tag))
        })
    }

    fn idle_state() -> IndexerState {
        IndexerState {
            mode: IndexMode::Auto,
            activity: IndexingStatus::Idle,
            last_full_index: Some(0),
            queued_events: 0,
            watcher: WatcherStatus::Active { dirs: 10 },
        }
    }

    /// A run in progress. `current_file` and the number of roots are the
    /// parts that come and go from frame to frame in a real run.
    fn running_state(roots: &[&str], current_file: Option<&str>) -> IndexerState {
        state_with(
            roots
                .iter()
                .map(|root| RootProgress {
                    root: (*root).to_string(),
                    phase: RootPhase::Walking,
                    walked: 100,
                    walk_total: Some(1000),
                    extracted: 0,
                    extract_total: 0,
                    current_file: current_file.map(str::to_string),
                    active_workers: 4,
                    total_workers: 4,
                })
                .collect(),
        )
    }

    /// A run whose roots are described one by one, for the rows whose
    /// contents — not just their widget ids — are under test.
    fn state_with(roots: Vec<RootProgress>) -> IndexerState {
        IndexerState {
            mode: IndexMode::Auto,
            activity: IndexingStatus::Running {
                start_time: std::time::Instant::now(),
                roots,
            },
            last_full_index: Some(0),
            queued_events: 0,
            watcher: WatcherStatus::Active { dirs: 10 },
        }
    }

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 900.0),
            )),
            events,
            ..Default::default()
        }
    }

    /// One frame of the real tab, with `events` delivered to it.
    fn frame(
        ctx: &egui::Context,
        tab: &mut ManageTab,
        cfg: &Config,
        state: &IndexerState,
        events: Vec<egui::Event>,
    ) -> ManageActions {
        WIDGETS.with(|w| w.borrow_mut().clear());
        let mut actions = ManageActions::default();
        let _ = ctx.run(raw_input(events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                actions = tab.ui(ui, state, cfg);
            });
        });
        actions
    }

    /// Every string the tab actually painted this frame. Labels carry no
    /// widget id worth recording, so the rendered text is read back off the
    /// shapes — the only place a number the user sees can be checked.
    fn frame_text(ctx: &egui::Context, tab: &mut ManageTab, state: &IndexerState) -> Vec<String> {
        frame_text_with(ctx, tab, &cfg_with_root(), state)
    }

    fn frame_text_with(
        ctx: &egui::Context,
        tab: &mut ManageTab,
        cfg: &Config,
        state: &IndexerState,
    ) -> Vec<String> {
        WIDGETS.with(|w| w.borrow_mut().clear());
        let out = ctx.run(raw_input(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                tab.ui(ui, state, cfg);
            });
        });
        let mut text = Vec::new();
        for clipped in &out.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        text
    }

    fn collect_text(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::epaint::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_text(s, out);
                }
            }
            _ => {}
        }
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            pointer(pos, true),
            pointer(pos, false),
        ]
    }

    fn cfg_with_root() -> Config {
        let mut cfg = Config::default();
        cfg.paths.indexing_paths = vec!["/data".into()];
        cfg
    }

    fn staged_workers(tab: &ManageTab) -> Option<usize> {
        tab.draft
            .as_ref()
            .unwrap()
            .indexing
            .root_workers
            .get("/data")
            .copied()
    }

    /// The per-root worker field must keep the same widget id however the
    /// status above it changes: egui hangs focus and in-progress text off
    /// that id, so a field that is renamed mid-run silently drops the edit.
    #[test]
    fn the_worker_field_keeps_its_identity_as_the_status_changes() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let cfg = cfg_with_root();

        frame(
            &ctx,
            &mut tab,
            &cfg,
            &running_state(&["/data"], None),
            vec![],
        );
        let (baseline, _) = widget("workers");

        for state in [
            running_state(&["/data"], Some("/data/file")),
            running_state(&["/data", "/other"], None),
            idle_state(),
            IndexerState {
                watcher: WatcherStatus::Off,
                ..idle_state()
            },
            IndexerState {
                activity: IndexingStatus::Error("boom".into()),
                ..idle_state()
            },
        ] {
            frame(&ctx, &mut tab, &cfg, &state, vec![]);
            assert_eq!(
                widget("workers").0,
                baseline,
                "status change moved the worker field"
            );
        }
    }

    /// Click the field, type a count, click Apply & Save — while a run is
    /// reporting progress the whole time.
    #[test]
    fn a_typed_worker_count_reaches_the_applied_config() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let cfg = cfg_with_root();

        frame(
            &ctx,
            &mut tab,
            &cfg,
            &running_state(&["/data"], None),
            vec![],
        );
        let field = widget("workers").1.center();
        frame(
            &ctx,
            &mut tab,
            &cfg,
            &running_state(&["/data"], None),
            click_at(field),
        );
        // The run starts reporting a file: one more label above the field.
        let busy = running_state(&["/data"], Some("/data/file"));
        frame(&ctx, &mut tab, &cfg, &busy, vec![]);
        frame(
            &ctx,
            &mut tab,
            &cfg,
            &busy,
            vec![egui::Event::Text("8".into())],
        );
        assert_eq!(staged_workers(&tab), Some(8), "typed count was not staged");

        let apply = widget("apply").1.center();
        let mut actions = frame(&ctx, &mut tab, &cfg, &busy, click_at(apply));
        if actions.apply_config.is_none() {
            // egui fires a click on release; give it the follow-up frame.
            actions = frame(&ctx, &mut tab, &cfg, &busy, vec![]);
        }
        let applied = actions
            .apply_config
            .expect("Apply & Save produced a config");
        assert_eq!(applied.indexing.root_workers.get("/data"), Some(&8));
    }

    /// The other way to set the field: drag it.
    #[test]
    fn a_dragged_worker_count_is_staged() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let cfg = cfg_with_root();
        let busy = running_state(&["/data"], None);

        frame(&ctx, &mut tab, &cfg, &busy, vec![]);
        let field = widget("workers").1.center();
        frame(
            &ctx,
            &mut tab,
            &cfg,
            &busy,
            vec![egui::Event::PointerMoved(field), pointer(field, true)],
        );
        // Drag right across frames, with the status changing underneath.
        let mut pos = field;
        for state in [
            running_state(&["/data"], Some("/data/a")),
            running_state(&["/data"], None),
            running_state(&["/data"], Some("/data/b")),
        ] {
            pos.x += 4.0;
            frame(
                &ctx,
                &mut tab,
                &cfg,
                &state,
                vec![egui::Event::PointerMoved(pos)],
            );
        }
        frame(&ctx, &mut tab, &cfg, &busy, vec![pointer(pos, false)]);
        assert!(
            staged_workers(&tab).is_some_and(|w| w > 0),
            "dragging staged nothing: {:?}",
            staged_workers(&tab)
        );
    }

    fn root_progress(phase: RootPhase, walked: usize, walk_total: Option<usize>) -> RootProgress {
        RootProgress {
            root: "/data".to_string(),
            phase,
            walked,
            walk_total,
            extracted: 0,
            extract_total: 0,
            current_file: None,
            active_workers: 4,
            total_workers: 4,
        }
    }

    /// The `find` count is an estimate of *tree entries*, so it runs far
    /// ahead of the files a walk actually emits. Once the walk ends the exact
    /// number is in hand, and the row must show that instead — the estimate
    /// is what kept the bar short of full for the rest of the run.
    #[test]
    fn a_finished_root_reports_its_exact_count_not_the_estimate() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let mut done = root_progress(RootPhase::Done, 261_088, Some(6_677_062));
        done.extracted = 238_929;
        done.active_workers = 0;
        done.total_workers = 0;

        let text = frame_text(&ctx, &mut tab, &state_with(vec![done])).join(" | ");
        assert!(
            text.contains("indexed 261,088, extracted 238,929"),
            "finished row: {}",
            text
        );
        assert!(
            !text.contains("6,677,062"),
            "the stale estimate is still on screen: {}",
            text
        );
    }

    /// While the walk runs the estimate is all there is, so it is shown —
    /// but never below what has already been walked, or the row would sit at
    /// 100% with the walk still going and read as a hang.
    #[test]
    fn a_walking_root_shows_the_estimate_raised_to_what_it_has_walked() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();

        let honest = frame_text(
            &ctx,
            &mut tab,
            &state_with(vec![root_progress(RootPhase::Walking, 100, Some(1000))]),
        )
        .join(" | ");
        assert!(honest.contains("100 / 1,000 (10%)"), "{}", honest);

        let overtaken = frame_text(
            &ctx,
            &mut tab,
            &state_with(vec![root_progress(RootPhase::Walking, 1500, Some(1000))]),
        )
        .join(" | ");
        assert!(
            overtaken.contains("1,500 / 1,500 (100%)"),
            "an overtaken estimate must be raised, not shown: {}",
            overtaken
        );
    }

    /// No count has landed yet: an indeterminate row, not a fabricated one.
    #[test]
    fn a_walking_root_without_a_count_shows_no_denominator() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let text = frame_text(
            &ctx,
            &mut tab,
            &state_with(vec![root_progress(RootPhase::Walking, 100, None)]),
        )
        .join(" | ");
        assert!(text.contains("100 files"), "{}", text);
        assert!(!text.contains(" / "), "invented a denominator: {}", text);
    }

    /// A scratch directory of its own for each size test, so two of them
    /// running in parallel cannot see each other's files.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qs-dbsize-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_bytes(path: &Path, len: usize) {
        std::fs::write(path, vec![b'x'; len]).expect("write");
    }

    /// A config whose database is `path`, for the probe and the rendered row.
    fn cfg_with_db(path: &Path) -> Config {
        let mut cfg = cfg_with_root();
        cfg.paths.database_path = path.to_string_lossy().into_owned();
        cfg
    }

    /// Each of the three files SQLite keeps for one database is added, and
    /// nothing else that happens to sit beside them is.
    #[test]
    fn db_size_counts_the_database_and_both_sidecars() {
        let dir = scratch_dir("parts");
        let db = dir.join("index.sqlite");
        write_bytes(&db, 4096);
        assert_eq!(measure_db_size(&db), 4096, "the database itself");
        write_bytes(&dir.join("index.sqlite-wal"), 1024);
        assert_eq!(measure_db_size(&db), 5120, "-wal was not added");
        write_bytes(&dir.join("index.sqlite-shm"), 32);
        assert_eq!(measure_db_size(&db), 5152, "-shm was not added");

        // Decoys: a rollback journal (never present in WAL mode) and an
        // unrelated neighbour.
        write_bytes(&dir.join("index.sqlite-journal"), 999);
        write_bytes(&dir.join("index.sqlite.bak"), 777);
        assert_eq!(measure_db_size(&db), 5152, "a decoy was counted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Before the first indexing run there is no database at all, and a
    /// closed database has no sidecars — neither is an error.
    #[test]
    fn a_missing_database_measures_zero() {
        let dir = scratch_dir("missing");
        let db = dir.join("index.sqlite");
        assert_eq!(measure_db_size(&db), 0);
        assert_eq!(measure_db_size(&dir), 0, "a directory");

        write_bytes(&db, 100);
        assert_eq!(measure_db_size(&db), 100, "sidecars are optional");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe answers from cache until the interval is up: `ui()` asks it
    /// every frame, and three stats per frame is churn nobody needs.
    #[test]
    fn the_probe_caches_until_the_refresh_interval_is_up() {
        let dir = scratch_dir("cache");
        let db = dir.join("index.sqlite");
        write_bytes(&db, 100);
        let cfg = cfg_with_db(&db);
        let mut probe = DbSizeProbe::default();
        let t0 = Instant::now();

        assert_eq!(probe.size(&cfg, t0), 100);
        write_bytes(&db, 5000);
        assert_eq!(
            probe.size(&cfg, t0 + DB_SIZE_REFRESH / 2),
            100,
            "restatted before the interval was up"
        );
        assert_eq!(
            probe.size(&cfg, t0 + DB_SIZE_REFRESH),
            5000,
            "the refresh never happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A database path edited in Options must not keep showing the old
    /// database's size for the rest of the interval.
    #[test]
    fn the_probe_follows_a_changed_database_path() {
        let dir = scratch_dir("moved");
        let first = dir.join("index.sqlite");
        let second = dir.join("other.sqlite");
        write_bytes(&first, 100);
        write_bytes(&second, 7000);
        let mut probe = DbSizeProbe::default();
        let t0 = Instant::now();

        assert_eq!(probe.size(&cfg_with_db(&first), t0), 100);
        assert_eq!(
            probe.size(&cfg_with_db(&second), t0),
            7000,
            "a new path must restat at once, not wait out the interval"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The number reaches the screen, and it is the total of all three files
    /// rather than the database on its own.
    #[test]
    fn the_status_row_shows_the_total_index_size() {
        let dir = scratch_dir("render");
        let db = dir.join("index.sqlite");
        write_bytes(&db, 3_000_000);
        write_bytes(&dir.join("index.sqlite-wal"), 200_000);
        write_bytes(&dir.join("index.sqlite-shm"), 32_768);

        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let text = frame_text_with(&ctx, &mut tab, &cfg_with_db(&db), &idle_state()).join(" | ");
        assert!(
            text.contains("Index size: 3.2 MB"),
            "the size is not on screen: {}",
            text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hovering is the whole point of the readout: the number tells the user
    /// the index is big, the tooltip tells them what to do about it. Each
    /// lever named here has to keep matching a control that exists.
    #[test]
    fn hovering_the_size_explains_how_to_shrink_the_index() {
        let dir = scratch_dir("hover");
        let db = dir.join("index.sqlite");
        write_bytes(&db, 2048);

        let ctx = egui::Context::default();
        // egui holds tooltips back for a third of a second, and frames here
        // are only 1/60 s of simulated time apart.
        ctx.style_mut(|s| s.interaction.tooltip_delay = 0.0);
        let mut tab = ManageTab::new();
        let cfg = cfg_with_db(&db);

        frame_text_with(&ctx, &mut tab, &cfg, &idle_state());
        let at = widget("db-size").1.center();
        frame(
            &ctx,
            &mut tab,
            &cfg,
            &idle_state(),
            vec![egui::Event::PointerMoved(at)],
        );
        // The tooltip is painted in the frame after the pointer lands.
        let text = frame_text_with(&ctx, &mut tab, &cfg, &idle_state()).join(" | ");

        assert!(
            text.contains("To reduce the index size"),
            "no tooltip: {}",
            text
        );
        for lever in [
            "ignore filters",
            "Indexed folders",
            "whitelist",
            "Store text for snippets",
            "Options",
        ] {
            assert!(text.contains(lever), "tooltip never mentions {}", lever);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.log".into());
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
        assert!(draft
            .indexing
            .ignore_patterns
            .contains(&"*.log".to_string()));
        assert_eq!(tab.baseline.as_ref().unwrap(), &external);
    }

    #[test]
    fn dirty_draft_adopts_sections_owned_elsewhere() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.bak".into());
        // The fuzzy toggle saves the config directly, outside this tab.
        let mut external = cfg.clone();
        external.search.fuzzy_default = !cfg.search.fuzzy_default;
        tab.sync_editors(&external);
        let draft = tab.draft.as_ref().unwrap();
        assert_eq!(draft.search.fuzzy_default, external.search.fuzzy_default);
        assert!(draft
            .indexing
            .ignore_patterns
            .contains(&"*.bak".to_string()));
    }

    #[test]
    fn external_pattern_is_not_duplicated_into_a_draft_that_has_it() {
        let cfg = Config::default();
        let mut tab = synced_tab(&cfg);
        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.log".into());
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

    #[test]
    fn a_fresh_tab_is_not_dirty_before_or_after_its_first_sync() {
        let mut tab = ManageTab::new();
        assert!(!tab.is_dirty(), "no draft yet");
        tab.sync_editors(&cfg_with_root());
        assert!(!tab.is_dirty(), "a fresh sync stages nothing");
    }

    #[test]
    fn a_staged_edit_reads_dirty_and_discard_reverts_it() {
        let cfg = cfg_with_root();
        let mut tab = synced_tab(&cfg);
        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.jpg".into());
        tab.new_ignore = "half-typ".into();
        tab.root_error = Some("bad".into());
        assert!(tab.is_dirty());

        tab.discard();
        assert!(!tab.is_dirty());
        assert!(tab.new_ignore.is_empty(), "scratch boxes are cleared too");
        assert!(tab.root_error.is_none());
        tab.sync_editors(&cfg);
        assert_eq!(tab.draft.as_ref(), Some(&cfg), "resynced from live");
        assert!(!tab.is_dirty());
    }

    /// `parse_lines` drops blank lines and trims entries, so cosmetic
    /// whitespace in the extension editor must not read as an edit.
    #[test]
    fn a_trailing_newline_in_the_extension_editor_is_not_dirty() {
        let mut cfg = cfg_with_root();
        cfg.indexing.content_extensions = vec!["txt".into(), "md".into()];
        let mut tab = synced_tab(&cfg);
        tab.ext_filter_text.push('\n');
        assert!(!tab.is_dirty(), "a blank line is not an edit");
        tab.ext_filter_text.push_str("pdf");
        assert!(tab.is_dirty(), "a real entry is");
    }

    /// The mode buttons write `auto_index` straight to the live config. A
    /// stale copy frozen into a staged draft used to read as permanently
    /// dirty — and applying it would have reverted the stop.
    #[test]
    fn a_live_mode_flip_does_not_read_as_dirty() {
        let mut cfg = cfg_with_root();
        cfg.indexing.auto_index = true;
        let mut tab = synced_tab(&cfg);

        // Stop clicked, nothing staged: the tab resyncs and stays clean.
        let mut stopped = cfg.clone();
        stopped.indexing.auto_index = false;
        tab.sync_editors(&stopped);
        assert!(!tab.is_dirty());

        // Staged edit, then Return to Automatic: dirty because of the edit
        // only, and the draft adopts the live mode.
        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.jpg".into());
        let mut auto_again = stopped.clone();
        auto_again.indexing.auto_index = true;
        tab.sync_editors(&auto_again);
        assert!(tab.is_dirty(), "the staged pattern is still pending");
        let applied = tab.take_apply_config(&auto_again).expect("a config");
        assert!(
            applied.indexing.auto_index,
            "applying must not revert the live mode"
        );

        // Un-staging the edit reads clean again — not permanently dirty on
        // a stale mode copy.
        tab.draft.as_mut().unwrap().indexing.ignore_patterns.pop();
        assert!(!tab.is_dirty());
    }

    /// `take_apply_config` must leave the editors intact: the app reports
    /// back via `mark_applied` only when the apply landed, so a rejected
    /// config (nested roots) keeps the user's staged edits on screen.
    #[test]
    fn a_rejected_apply_keeps_the_draft() {
        let cfg = cfg_with_root();
        let mut tab = synced_tab(&cfg);
        tab.draft
            .as_mut()
            .unwrap()
            .paths
            .indexing_paths
            .push("/data/nested".into());

        let staged = tab.take_apply_config(&cfg).expect("a config to apply");
        assert!(staged
            .paths
            .indexing_paths
            .contains(&"/data/nested".to_string()));
        assert!(tab.baseline.is_some(), "baseline survives the attempt");
        assert!(tab.is_dirty(), "the staged root is still pending");

        tab.mark_applied();
        assert!(tab.baseline.is_none(), "a landed apply forces a resync");
        assert!(!tab.is_dirty());
    }

    /// The dirty label sits after the Apply button inside a stable section:
    /// its coming and going must never rename the button, which egui hangs
    /// interaction state off.
    #[test]
    fn the_unsaved_label_appears_without_renaming_the_apply_button() {
        let ctx = egui::Context::default();
        let mut tab = ManageTab::new();
        let cfg = cfg_with_root();

        let clean = frame_text_with(&ctx, &mut tab, &cfg, &idle_state());
        assert!(
            !clean.iter().any(|t| t.contains("Unsaved changes")),
            "a clean tab must not claim unsaved changes"
        );
        let (clean_id, _) = widget("apply");

        tab.draft
            .as_mut()
            .unwrap()
            .indexing
            .ignore_patterns
            .push("*.jpg".into());
        let dirty = frame_text_with(&ctx, &mut tab, &cfg, &idle_state());
        assert!(
            dirty.iter().any(|t| t.contains("Unsaved changes")),
            "painted: {:?}",
            dirty
        );
        assert_eq!(widget("apply").0, clean_id, "the label renamed the button");
    }
}
