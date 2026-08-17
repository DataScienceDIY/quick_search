//! The Manage Index tab: detailed status, mode controls, indexed roots,
//! and the content/ignore filter editors.

use std::path::Path;
use std::time::{Duration, Instant};

use quicksearch_core::config::Config;
use quicksearch_core::coordinator::{IndexMode, IndexerState, ReconcileState, WatcherStatus};
use quicksearch_core::indexing::{
    IndexingStatus, PrepStep, ReconcileProgress, RootPhase, RootProgress,
};

use crate::format::{
    fmt_duration_clock, fmt_interval, fmt_rate, fmt_reconcile_summary, group_thousands, human_size,
    middle_truncate,
};
use crate::tips::{self, Tipped};
use crate::tracker::SpeedTracker;
use crate::ui_util::hint;
use crate::ui_util::middle_elide;

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
    /// Multiline editor, one extension per line; parsed back on Apply.
    ext_filter_text: String,
    new_root: String,
    /// Text of the inline "add ignore pattern" box.
    new_ignore: String,
    /// Inline error from a rejected root add (nested/duplicate).
    root_error: Option<String>,
    /// The config the draft was last synced from; `None` forces a full resync.
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
    /// every tab).
    pub fn observe(&mut self, status: &IndexingStatus) {
        match status {
            IndexingStatus::Running { roots, .. } => {
                // Monotonic within a run: walks and extractions only grow.
                let total: usize = roots.iter().map(|r| r.walked + r.extracted).sum();
                self.speed.record(total);
            }
            // Preparing included: a stale files/sec left over from the last
            // run would read as progress that is not happening.
            IndexingStatus::Idle
            | IndexingStatus::Error(_)
            | IndexingStatus::Optimizing
            | IndexingStatus::Preparing { .. } => self.speed.reset(),
            _ => {}
        }
    }

    /// Reconcile the draft with the live config, every frame, so a filter
    /// persisted elsewhere shows up here on the next frame.
    fn sync_editors(&mut self, config: &Config) {
        let Some(baseline) = &self.baseline else {
            // First frame, or right after our own Apply.
            return self.resync(config);
        };
        if baseline == config {
            return;
        }
        // The config changed elsewhere.
        if !self.is_dirty() {
            // Nothing staged, nothing to lose.
            return self.resync(config);
        }
        // Staged edits exist: keep the sections this tab edits, adopt the
        // rest so a later Apply cannot revert changes made elsewhere.
        let draft = self.draft.take().expect("synced");
        let mut merged = config.clone();
        merged.paths.indexing_paths = draft.paths.indexing_paths;
        merged.indexing = draft.indexing;
        // Live state, not a user edit: a stale `auto_index` frozen into the
        // draft would read as permanently dirty.
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

    /// Whether the editors hold changes not yet applied. Live fields are
    /// pinned before comparing (`pin_live_fields`) and the extension text is
    /// compared parsed, so a trailing newline never reads as dirty. False
    /// before the first sync.
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

    /// The Apply & Save action. Syncs against `live` first so a config
    /// applied elsewhere moments ago is not reverted. Does NOT clear
    /// `baseline`: the app calls [`ManageTab::mark_applied`] only after the
    /// apply succeeds, so a rejected apply keeps the staged edits.
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

    /// Drop every staged edit; the next frame resyncs from the live config.
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
                        .tip(&tips::START_NOW)
                        .clicked()
                    {
                        actions.start_now = true;
                    }
                    if ui
                        .add_enabled(
                            running || state.mode == IndexMode::Auto,
                            egui::Button::new("Stop"),
                        )
                        .tip(&tips::STOP_INDEXING)
                        .clicked()
                    {
                        actions.stop = true;
                    }
                    if ui
                        .add_enabled(
                            state.mode != IndexMode::Auto,
                            egui::Button::new("Return to Automatic"),
                        )
                        .tip(&tips::RETURN_TO_AUTO)
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
                        .tip(&tips::CLEAR_INDEX)
                        .clicked()
                    {
                        actions.clear_index = true;
                    }
                    if state.queued_events > 0 {
                        ui.label(hint(format!("{} changes queued", state.queued_events)));
                    }
                });
                ui.separator();

                // --- Indexed roots ---------------------------------------------
                ui.heading(egui::RichText::new("Indexed folders").strong());
                let draft = self.draft.as_mut().expect("synced");
                let mut remove: Option<usize> = None;
                let (paths, indexing) = (&draft.paths, &mut draft.indexing);
                for (i, root) in paths.indexing_paths.iter().enumerate() {
                    ui.horizontal(|ui| {
                        // Controls claim the right edge first so a long path
                        // can never push them out of view.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").tip(&tips::REMOVE_ROOT).clicked() {
                                remove = Some(i);
                            }
                            // Per-root walker override; 0 = auto (4 local / 16
                            // network, detected per root). Applies on the next run.
                            let mut workers = indexing.root_workers.get(root).copied().unwrap_or(0);
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
                                .tip(&tips::ROOT_WORKERS);
                            #[cfg(test)]
                            tests::record_widget("workers", &response);
                            if response.changed() {
                                if workers == 0 {
                                    indexing.root_workers.remove(root);
                                } else {
                                    indexing.root_workers.insert(root.clone(), workers);
                                }
                            }
                            ui.label(hint("workers:"));

                            // Unconditional, placeholder and all: egui names a
                            // widget by how many precede it, so a label that
                            // came and went would rename the field above and
                            // cost it any edit in progress. After that field
                            // for the same reason — in this right-to-left
                            // layout "after" is to its left.
                            ui.label(hint(root_counts_text(state, root)))
                                .tip(&tips::ROOT_COUNTS);

                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                                    let shown =
                                        middle_elide(ui, root, ui.available_width(), &font_id);
                                    ui.monospace(shown.as_ref()).on_hover_text(root);
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
                    if ui.button("Add folder…").tip(&tips::ADD_ROOT).clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            let path = dir.to_string_lossy().into_owned();
                            try_add_root(draft, path, &mut self.root_error);
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_root)
                            .desired_width(240.0)
                            .hint_text("or type a path"),
                    )
                    .tip(&tips::ADD_ROOT);
                    if ui.button("Add").tip(&tips::ADD_ROOT).clicked()
                        && !self.new_root.trim().is_empty()
                    {
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
                ui.label(hint(
                    "Removing a folder removes its entries and leaves the rest of \
                         the index untouched; adding one reindexes to pick it up. \
                         Neither rebuilds.",
                ));
                ui.separator();

                // --- Filters ---------------------------------------------------
                ui.heading(egui::RichText::new("Content filters").strong());
                ui.columns(2, |cols| {
                    cols[0]
                        .label("Full-text extensions whitelist (empty = all supported):")
                        .tip(&tips::EXT_WHITELIST);
                    cols[0]
                        .add(
                            egui::TextEdit::multiline(&mut self.ext_filter_text)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .hint_text("txt\nmd\npdf  # comments allowed\n(none)"),
                        )
                        .tip(&tips::EXT_WHITELIST);
                    cols[1]
                        .label("Ignore patterns (excluded entirely):")
                        .tip(&tips::IGNORE_PATTERNS);
                    let mut remove_pat: Option<usize> = None;
                    // The list grows and shrinks, so it is kept off the id of
                    // the editor below it (see `ui_util::stable_section`).
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
                            ui.label(hint("No ignore patterns."));
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
                        response.tip(&tips::IGNORE_PATTERNS);
                        if ui
                            .add_enabled(valid, egui::Button::new("Add"))
                            .tip(&tips::IGNORE_PATTERNS)
                            .clicked()
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
                    cols[1].label(hint(
                        "Changes apply on Apply & Save. A new pattern removes the \
                             entries it matches; removing one reindexes to bring them \
                             back.",
                    ));
                });
                ui.separator();

                ui.label(hint(
                    "Reindex interval, symlinks, hidden files, tokenizer, and size \
                     limits are on the Settings tab.",
                ));
                ui.add_space(8.0);

                let dirty = self.is_dirty();
                let p = crate::color::palette(ui.visuals().dark_mode);
                ui.horizontal(|ui| {
                    let apply = ui
                        .add(crate::ui_util::bordered_button(
                            "Apply & Save",
                            if dirty { p.orange } else { p.blue },
                        ))
                        .tip(&tips::APPLY_SAVE);
                    #[cfg(test)]
                    tests::record_widget("apply", &apply);
                    // The label comes and goes with the dirty state; keep it
                    // off the ids of whatever sits after it.
                    crate::ui_util::stable_section(ui, |ui| {
                        if dirty {
                            ui.label(
                                egui::RichText::new("Unsaved changes")
                                    .small()
                                    .color(p.orange),
                            );
                        }
                    });
                    if apply.clicked() {
                        actions.apply_config = self.take_apply_config(config);
                    }
                });
            });
        crate::ui_util::more_below_hint(ui, &scroll);

        // Nothing else asks for repaints while the app sits idle; without
        // this the size would freeze until the pointer next moved.
        ui.ctx().request_repaint_after(DB_SIZE_REFRESH);

        actions
    }
}

/// How often the index files are re-statted.
const DB_SIZE_REFRESH: Duration = Duration::from_secs(10);

/// Total on-disk footprint of the index: the database plus its `-wal` and
/// `-shm` sidecars — mid-run the `-wal` can hold hundreds of megabytes the
/// database does not show yet. A file that is not there counts as zero.
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
            self.bytes = measure_db_size(&config.resolved_database_path());
            self.path = config.paths.database_path.clone();
            self.measured_at = Some(now);
        }
        self.bytes
    }
}

/// The index's footprint, with the levers for shrinking it on hover.
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
        "Add ignore filters for files and folders you never search, in the ignore \
         pattern list further down this tab.",
        "Remove indexed folders you do not need, in Indexed folders above.",
        "Narrow the full-text extension whitelist, so text is only extracted \
         from the file types you actually search.",
        "Turn off \"Store text for snippets\" on the Settings tab: full-text search \
         keeps working, but without previews, occurrence ranking or fuzzy matching \
         inside file contents.",
        "Lower \"Max text file size\" and \"Max stored text\", both on the Settings tab.",
    ] {
        ui.label(format!("•  {}", lever));
    }
    ui.add_space(6.0);
    ui.label(hint(
        "Narrowing any of these removes the entries it excludes straight away, \
             but the file does not shrink on its own. The freed space is reused by \
             the index rather than returned to the disk, until an indexing run's \
             optimize pass compacts it.",
    ));
}

/// What `root` held when indexing last completed, worded as the live
/// per-root rows word it (see [`root_row`]) so the list does not rename the
/// same two figures once the run that produced them is over.
///
/// A root the coordinator has no figures for — never indexed to completion,
/// staged in the draft but not yet applied, or an index that was cleared —
/// says so rather than claiming zero.
fn root_counts_text(state: &IndexerState, root: &str) -> String {
    match state.root_counts.iter().find(|c| c.root == root) {
        Some(c) => format!(
            "indexed {} · extracted {}",
            group_thousands(c.counts.files.max(0) as u64),
            group_thousands(c.counts.fts.max(0) as u64)
        ),
        None => "not yet indexed".to_string(),
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

/// Live-update health. Empty in manual mode and a line long otherwise,
/// hence the stable section (see [`status_panel`]).
fn watch_panel(ui: &mut egui::Ui, state: &IndexerState, config: &Config) {
    crate::ui_util::stable_section(ui, |ui| watch_contents(ui, state, config));
}

fn watch_contents(ui: &mut egui::Ui, state: &IndexerState, config: &Config) {
    match &state.watcher {
        WatcherStatus::Off => {}
        WatcherStatus::Starting => {
            ui.label(hint("Setting up live updates…"));
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

/// Live progress. Its widget count tracks the run, so it renders inside a
/// [`crate::ui_util::stable_section`]: without one, every id below it would
/// move mid-run.
fn status_panel(ui: &mut egui::Ui, state: &IndexerState, speed: &SpeedTracker) {
    crate::ui_util::stable_section(ui, |ui| status_contents(ui, state, speed));
}

fn status_contents(ui: &mut egui::Ui, state: &IndexerState, speed: &SpeedTracker) {
    match &state.activity {
        IndexingStatus::Idle => {
            // A between-runs reconcile: the activity really is Idle, but the
            // thread may be scanning every row for minutes.
            match &state.reconcile {
                Some(ReconcileState::Running(r)) => return reconcile_row(ui, r, None),
                // Kept on screen a few seconds after the work ends: a small
                // index applies a filter faster than the display could show.
                Some(ReconcileState::Finished(r)) => {
                    ui.label(fmt_reconcile_summary(r.deleted, r.recontented));
                    return;
                }
                None => {}
            }
            let last = state
                .last_full_index
                .map(crate::format::fmt_ago)
                .unwrap_or_else(|| "never".to_string());
            ui.label(format!("Idle; last full index: {}", last));
        }
        IndexingStatus::Preparing { start_time, step } => {
            prep_row(ui, step, start_time.elapsed());
        }
        IndexingStatus::Error(e) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("Error: {}", e));
        }
        IndexingStatus::Stopping => {
            ui.label("Stopping…");
        }
        IndexingStatus::Optimizing => {
            // One bulk rewrite of the whole file; no per-file progress exists.
            ui.label("Optimizing index; reclaiming unused space…");
        }
        IndexingStatus::Running { roots, .. } => {
            for root in roots {
                root_row(ui, root);
            }
            if let Some(rate) = speed.files_per_sec() {
                ui.label(
                    egui::RichText::new(format!(
                        "last {}s: {}",
                        crate::tracker::WINDOW.as_secs(),
                        fmt_rate(rate)
                    ))
                    .small()
                    .weak(),
                );
            }
        }
    }
}

/// What a run is doing before it walks its first file. Each step can
/// outlast the walk itself on a large index; the elapsed clock is what
/// distinguishes slow work from a hang.
fn prep_row(ui: &mut egui::Ui, step: &PrepStep, elapsed: Duration) {
    match step {
        PrepStep::PreviousRun => waiting_row(ui, "Finishing the previous run…", elapsed),
        PrepStep::OpeningIndex => waiting_row(ui, "Opening the index…", elapsed),
        PrepStep::Reconciling(r) => reconcile_row(ui, r, Some(elapsed)),
    }
}

/// A prologue step with no counters: label, clock, indeterminate bar.
fn waiting_row(ui: &mut egui::Ui, label: &str, elapsed: Duration) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.label(hint(fmt_duration_clock(elapsed)));
        crate::ui_util::progress_bar(ui, None, 160.0);
    });
}

/// A configuration reconciliation, from either place one runs. `elapsed`
/// is `Some` for a run's prologue; the between-runs pass has no start time.
fn reconcile_row(ui: &mut egui::Ui, r: &ReconcileProgress, elapsed: Option<Duration>) {
    ui.horizontal(|ui| {
        ui.label("Applying configuration change");
        ui.label(egui::RichText::new("|").weak());
        match (r.total, r.fraction()) {
            (Some(total), Some(frac)) => {
                ui.label(format!(
                    "{} / {} ({:.0}%) entries checked",
                    group_thousands(r.examined as u64),
                    group_thousands(total as u64),
                    frac * 100.0
                ));
            }
            _ => {
                ui.label(format!(
                    "{} entries checked",
                    group_thousands(r.examined as u64)
                ));
            }
        }
        if let Some(elapsed) = elapsed {
            ui.label(hint(fmt_duration_clock(elapsed)));
        }
        match r.fraction() {
            Some(frac) => {
                crate::ui_util::progress_bar(ui, Some(frac as f32), 160.0);
            }
            // Whole-range deletions read no rows, so they reach the bar
            // with no denominator.
            None => {
                crate::ui_util::progress_bar(ui, None, 160.0);
            }
        }
    });
    if r.deleted > 0 || r.recontented > 0 {
        ui.label(
            egui::RichText::new(format!(
                "{} entries removed, {} re-examined for text extraction",
                group_thousands(r.deleted as u64),
                group_thousands(r.recontented as u64)
            ))
            .small()
            .weak(),
        );
    }
}

/// One root's progress: path, phase, bar, counters, current file.
fn root_row(ui: &mut egui::Ui, r: &RootProgress) {
    // Weak "|" separators split the row into folder | status | numbers.
    let divider = |ui: &mut egui::Ui| {
        ui.label(egui::RichText::new("|").weak());
    };
    let phase = crate::color::palette(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        ui.monospace(middle_truncate(&r.root, 48));
        divider(ui);
        match r.phase {
            RootPhase::Walking => {
                ui.label(egui::RichText::new("indexing").color(phase.yellow));
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
                        crate::ui_util::progress_bar(ui, Some(frac), 160.0);
                    }
                    _ => {
                        ui.label(format!(
                            "{} files · {}",
                            group_thousands(r.walked as u64),
                            workers
                        ));
                        crate::ui_util::progress_bar(ui, None, 160.0);
                    }
                }
            }
            RootPhase::Extracting => {
                ui.label(egui::RichText::new("extracting text").color(phase.green));
                divider(ui);
                let workers = format!("{}/{} workers", r.active_workers, r.total_workers);
                match r.extract_total {
                    Some(total) => {
                        let frac = if total > 0 {
                            (r.extracted as f32 / total as f32).clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        ui.label(format!(
                            "{} / {} ({:.0}%) · {}",
                            group_thousands(r.extracted as u64),
                            group_thousands(total as u64),
                            frac * 100.0,
                            workers
                        ));
                        crate::ui_util::progress_bar(ui, Some(frac), 160.0);
                    }
                    // The pass is still counting its range — the same shape
                    // as a walk without a denominator yet.
                    None => {
                        ui.label(format!(
                            "{} files · {}",
                            group_thousands(r.extracted as u64),
                            workers
                        ));
                        crate::ui_util::progress_bar(ui, None, 160.0);
                    }
                }
            }
            RootPhase::Done => {
                // Whole-root totals: `walked` counts every file the walk saw
                // and `extracted` all rows with searchable text, not just
                // this run's new work.
                ui.label(egui::RichText::new("done").color(phase.blue));
                divider(ui);
                ui.label(format!(
                    "indexed {}, extracted {}",
                    group_thousands(r.walked as u64),
                    group_thousands(r.extracted as u64)
                ));
                crate::ui_util::progress_bar(ui, Some(1.0), 160.0);
            }
        }
    });
    if let Some(f) = &r.current_file {
        ui.label(hint(middle_truncate(f, 90)));
    }
}

#[cfg(test)]
mod tests;
