use super::*;

use std::cell::RefCell;
use std::sync::Arc;

use quicksearch_core::coordinator::RootCount;
use quicksearch_core::db::repo::RootCounts;

// The widgets under test report themselves here so their identity can be
// checked across frames.
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
        files: Some(0),
        queued_events: 0,
        watcher: WatcherStatus::Active { dirs: 10 },
        reconcile: None,
        root_counts: Arc::new(Vec::new()),
    }
}

/// An idle state carrying figures for `/data`, the root `cfg_with_root`
/// configures.
fn counted_state(files: i64, fts: i64) -> IndexerState {
    IndexerState {
        root_counts: Arc::new(vec![RootCount {
            root: "/data".into(),
            counts: RootCounts { files, fts },
        }]),
        ..idle_state()
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
        activity: IndexingStatus::Running {
            start_time: std::time::Instant::now(),
            roots,
        },
        ..idle_state()
    }
}

/// A run still in its prologue, before the walk has produced anything.
fn preparing_state(step: PrepStep) -> IndexerState {
    IndexerState {
        activity: IndexingStatus::Preparing {
            start_time: std::time::Instant::now(),
            step,
        },
        ..idle_state()
    }
}

use crate::test_ui::{click_at, painted_text};

fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
    crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events)
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

/// Every string the tab actually painted this frame, read back off the
/// shapes.
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
    painted_text(&out)
}

fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
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
        // The per-root figures appear and disappear on the same row as the
        // field, which is the case a conditionally-drawn label would break.
        counted_state(1_234_567, 456_789),
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

/// The walk-total estimate counts *tree entries* and runs far ahead of
/// the files a walk emits; a finished root must show the exact count.
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

/// The folder list carries the last completed run's figures, so what a root
/// holds survives the run that counted it.
#[test]
fn a_configured_root_shows_what_the_last_run_counted() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();

    let text = frame_text(&ctx, &mut tab, &counted_state(1_234_567, 456_789)).join(" | ");
    assert!(
        text.contains("indexed 1,234,567 · extracted 456,789"),
        "folder row: {}",
        text
    );
}

/// A root with no stored figures says so. Zero would be a claim — that the
/// folder is empty — where the truth is that nothing has counted it yet.
#[test]
fn a_root_the_index_has_never_counted_says_so() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();

    let text = frame_text(&ctx, &mut tab, &idle_state()).join(" | ");
    assert!(text.contains("not yet indexed"), "folder row: {}", text);
    assert!(
        !text.contains("indexed 0 · extracted 0"),
        "an uncounted root must not read as an empty one: {}",
        text
    );
}

/// Figures are matched to the root by the spelling the config uses, so a
/// root the coordinator has not published anything for keeps the placeholder
/// rather than borrowing another root's numbers.
#[test]
fn figures_belong_to_the_root_they_were_counted_for() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let state = IndexerState {
        root_counts: Arc::new(vec![RootCount {
            root: "/somewhere-else".into(),
            counts: RootCounts { files: 99, fts: 9 },
        }]),
        ..idle_state()
    };

    let text = frame_text(&ctx, &mut tab, &state).join(" | ");
    assert!(text.contains("not yet indexed"), "folder row: {}", text);
    assert!(
        !text.contains("99"),
        "borrowed another root's count: {}",
        text
    );
}

/// The estimate is shown while the walk runs — but never below what has
/// already been walked, or the row would read as a hang at 100%.
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

/// Like [`frame_text`], but keeping the color each run of text was
/// painted in — the only way to check a hint.
fn frame_spans(
    ctx: &egui::Context,
    tab: &mut ManageTab,
    state: &IndexerState,
) -> Vec<(String, egui::Color32)> {
    WIDGETS.with(|w| w.borrow_mut().clear());
    let cfg = cfg_with_root();
    let out = ctx.run(raw_input(vec![]), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            tab.ui(ui, state, &cfg);
        });
    });
    crate::test_ui::painted_spans(&out)
}

/// The phase word carries a color hint, and it has to hold up in both
/// themes.
#[test]
fn every_phase_word_is_painted_in_its_hint_color() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let ctx = egui::Context::default();
        ctx.set_theme(theme);
        let mut tab = ManageTab::new();
        let colors = crate::color::palette(theme == egui::Theme::Dark);

        for (phase, word, want) in [
            (RootPhase::Walking, "indexing", colors.yellow),
            (RootPhase::Extracting, "extracting text", colors.green),
            (RootPhase::Done, "done", colors.blue),
        ] {
            let state = state_with(vec![root_progress(phase, 100, Some(1000))]);
            let spans = frame_spans(&ctx, &mut tab, &state);
            let hint = spans.iter().find(|(text, _)| text == word).map(|(_, c)| *c);
            assert_eq!(hint, Some(want), "{:?}: {:?} in {:?}", theme, word, spans);
        }
    }
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

/// Every step of the prologue names itself and carries a clock.
#[test]
fn each_prologue_step_says_what_it_is_waiting_on() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();

    for (step, expected) in [
        (PrepStep::PreviousRun, "Finishing the previous run…"),
        (PrepStep::OpeningIndex, "Opening the index…"),
    ] {
        let text = frame_text(&ctx, &mut tab, &preparing_state(step)).join(" | ");
        assert!(text.contains(expected), "{}", text);
        // The clock is the point of the row.
        assert!(text.contains("0:00"), "no elapsed time shown: {}", text);
    }
}

/// The reconcile is the long one, and the only prologue step with
/// something to count. It reports its position in the scan.
#[test]
fn a_reconcile_reports_how_far_through_the_index_it_is() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let text = frame_text(
        &ctx,
        &mut tab,
        &preparing_state(PrepStep::Reconciling(ReconcileProgress {
            examined: 2_500_000,
            total: Some(8_000_000),
            deleted: 1_204,
            recontented: 0,
        })),
    )
    .join(" | ");

    assert!(text.contains("Applying configuration change"), "{}", text);
    assert!(
        text.contains("2,500,000 / 8,000,000 (31%) entries checked"),
        "{}",
        text
    );
    assert!(text.contains("1,204 entries removed"), "{}", text);
}

/// Whole-range deletions read no rows, so the scan can reach the display
/// with nothing to divide by; it must not invent a denominator.
#[test]
fn a_reconcile_without_a_row_count_shows_no_denominator() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let text = frame_text(
        &ctx,
        &mut tab,
        &preparing_state(PrepStep::Reconciling(ReconcileProgress::default())),
    )
    .join(" | ");
    assert!(text.contains("0 entries checked"), "{}", text);
    assert!(!text.contains(" / "), "invented a denominator: {}", text);
}

/// A prune between runs is reported, even though the activity really is
/// `Idle` while the thread scans every row.
#[test]
fn a_prune_between_runs_is_reported_instead_of_idle() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let state = IndexerState {
        reconcile: Some(ReconcileState::Running(ReconcileProgress {
            examined: 40_000,
            total: Some(80_000),
            deleted: 0,
            recontented: 0,
        })),
        ..idle_state()
    };

    let text = frame_text(&ctx, &mut tab, &state).join(" | ");
    assert!(
        text.contains("Applying configuration change"),
        "the scan is invisible: {}",
        text
    );
    assert!(text.contains("40,000 / 80,000 (50%)"), "{}", text);
    assert!(
        !text.contains("Idle; last full index"),
        "reported idle while scanning: {}",
        text
    );
}

/// The pass itself can be over between two frames; the lingering summary
/// is the only evidence it happened.
#[test]
fn a_finished_prune_reports_what_it_did() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let state = IndexerState {
        reconcile: Some(ReconcileState::Finished(ReconcileProgress {
            examined: 80_000,
            total: Some(80_000),
            deleted: 1_204,
            recontented: 7,
        })),
        ..idle_state()
    };

    let text = frame_text(&ctx, &mut tab, &state).join(" | ");
    assert!(
        text.contains("Configuration change applied"),
        "the finished pass left no trace: {}",
        text
    );
    assert!(text.contains("1,204 entries removed"), "{}", text);
    assert!(text.contains("7 entries re-examined"), "{}", text);
    assert!(
        !text.contains("Idle; last full index"),
        "the summary was replaced by the idle line: {}",
        text
    );
}

/// The static "Starting…" placeholder must not reappear.
#[test]
fn the_starting_placeholder_is_gone() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    for state in [
        preparing_state(PrepStep::PreviousRun),
        preparing_state(PrepStep::OpeningIndex),
        preparing_state(PrepStep::Reconciling(ReconcileProgress::default())),
        idle_state(),
    ] {
        let text = frame_text(&ctx, &mut tab, &state).join(" | ");
        assert!(!text.contains("Starting"), "{}", text);
    }
}

use quicksearch_core::testutil::scratch_dir;

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

/// The probe answers from cache until the interval is up.
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

/// The size tooltip's levers each have to keep matching a control that
/// exists.
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

/// The mode buttons write `auto_index` straight to the live config; a
/// stale copy frozen into the draft must not read dirty or revert the
/// mode on apply.
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

/// `take_apply_config` must leave the editors intact, so a rejected
/// config keeps the user's staged edits on screen.
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

/// The dirty label's coming and going must never rename the Apply button,
/// which egui hangs interaction state off.
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

/// The status area gains and loses rows as a prune runs; every frame must
/// leave the widgets below it with the ids they had — an editor whose id
/// changes mid-edit loses its buffer.
#[test]
fn the_prune_rows_come_and_go_without_renaming_anything_below() {
    let ctx = egui::Context::default();
    let mut tab = ManageTab::new();
    let cfg = cfg_with_root();
    let progress = ReconcileProgress {
        examined: 40_000,
        total: Some(80_000),
        deleted: 12,
        recontented: 0,
    };

    frame_text_with(&ctx, &mut tab, &cfg, &idle_state());
    let (apply, _) = widget("apply");
    let (workers, _) = widget("workers");

    for reconcile in [
        Some(ReconcileState::Running(progress)),
        Some(ReconcileState::Finished(progress)),
        None,
    ] {
        let state = IndexerState {
            reconcile,
            ..idle_state()
        };
        frame_text_with(&ctx, &mut tab, &cfg, &state);
        assert_eq!(widget("apply").0, apply, "renamed by {reconcile:?}");
        assert_eq!(widget("workers").0, workers, "renamed by {reconcile:?}");
    }
}
