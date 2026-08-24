use super::*;

fn new_tab() -> SearchTab {
    SearchTab::new(false, ColumnsConfig::default(), true)
}

/// Rows all carry a content snippet — the expensive cell, which measures
/// glyph advances across the window to centre the match.
fn tab_with_content_snippets(n: usize) -> SearchTab {
    let mut tab = new_tab();
    tab.query = "quartzite".into();
    tab.focus_query = false;
    // A full-width window with the match in the middle, as the cascade cuts them.
    let filler = "lorem ipsum dolor sit amet consectetur ";
    let head = filler.repeat(8);
    let tail = filler.repeat(8);
    let window = format!("{head}quartzite{tail}");
    let at = head.len();
    tab.results = (0..n)
        .map(|i| SearchHit {
            file_id: i as i64,
            name: format!("alpha_widget_{i}.txt"),
            path: format!("/qs-test/deeply/nested/directory/tree/alpha_widget_{i}.txt"),
            size: 116,
            mtime: 1_700_000_000,
            rank: 6.0,
            stage: 6,
            snippet: Some(Snippet {
                window: window.clone(),
                ranges: vec![(at, at + "quartzite".len())],
                truncated_start: true,
                truncated_end: true,
            }),
        })
        .collect();
    tab.order = (0..n as u32).collect();
    tab
}

/// One frame with no assertions; `run_frame`'s glyph check is wrong for timing.
fn timed_frame(ctx: &egui::Context, tab: &mut SearchTab) {
    let input = crate::test_ui::raw_input(egui::vec2(1400.0, 900.0), Vec::new());
    let _ = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            tab.ui(ui);
        });
    });
}

/// Per-visible-row frame cost, printed rather than asserted. Measured: 1000
/// rows with content snippets cost 615 µs/frame (<4% of a 60 fps budget),
/// which argued *against* memoizing the Content Match cut — re-measure here
/// before optimizing the row renderer.
///
/// ```text
/// QSB_RENDER_PERF=1 cargo test --release -p quicksearch-gui -- render_perf --nocapture
/// ```
#[test]
fn render_perf() {
    if std::env::var("QSB_RENDER_PERF").is_err() {
        eprintln!("skipping: set QSB_RENDER_PERF=1 to run");
        return;
    }
    const FRAMES: u32 = 300;
    let ctx = crate::test_ui::ctx();

    // Subtracting the empty floor turns the figure into a statement about the rows.
    let mut cases: Vec<(&str, std::time::Duration)> = Vec::new();
    for (label, mut tab) in [
        ("empty (floor)", new_tab()),
        ("1000 rows, name only", tab_with_results(1000)),
        (
            "1000 rows, content snippets",
            tab_with_content_snippets(1000),
        ),
    ] {
        for _ in 0..10 {
            timed_frame(&ctx, &mut tab);
        }
        let start = std::time::Instant::now();
        for _ in 0..FRAMES {
            timed_frame(&ctx, &mut tab);
        }
        cases.push((label, start.elapsed() / FRAMES));
    }

    let floor = cases[0].1;
    for (label, each) in &cases {
        println!(
            "{:<30} {:>9.1?}/frame   rows cost {:>9.1?}   ({:.0} fps ceiling)",
            label,
            each,
            each.saturating_sub(floor),
            1.0 / each.as_secs_f64(),
        );
    }
    println!("\n(a 60 fps budget is 16.6 ms; the table virtualizes, so this is per *visible* row)");
}

fn tab_with_results(n: usize) -> SearchTab {
    let mut tab = new_tab();
    tab.query = "alpha".into();
    tab.focus_query = false;
    tab.results = (0..n)
        .map(|i| SearchHit {
            file_id: i as i64,
            name: format!("alpha_widget_{i}.txt"),
            path: format!("/qs-test/alpha_widget_{i}.txt"),
            size: 116,
            mtime: 1_700_000_000,
            rank: 3.0,
            stage: 1,
            snippet: None,
        })
        .collect();
    tab.order = (0..n as u32).collect();
    tab
}

/// One frame at the given width; the no-tofu glyph check rides along on
/// every test in this file.
fn frame_at(
    ctx: &egui::Context,
    tab: &mut SearchTab,
    width: f32,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, SearchActions) {
    let input = crate::test_ui::raw_input(egui::vec2(width, 700.0), events);
    let mut actions = SearchActions::default();
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            actions = tab.ui(ui);
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &out);
    (out, actions)
}

fn run_frame(
    ctx: &egui::Context,
    tab: &mut SearchTab,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    frame_at(ctx, tab, 1000.0, events).0
}

/// Walk the pointer down the name column until it sits on `row`'s label
/// glyphs (text cursor: a selectable label won the hit-test).
fn hover_row_text(ctx: &egui::Context, tab: &mut SearchTab, row: usize) -> egui::Pos2 {
    for y in 40..250 {
        let pos = egui::pos2(60.0, y as f32);
        let out = run_frame(ctx, tab, vec![egui::Event::PointerMoved(pos)]);
        let over_text = out.platform_output.cursor_icon == egui::CursorIcon::Text;
        if over_text && tab.hovered_row == Some(row) {
            return pos;
        }
    }
    panic!("never landed on row {row}'s label text");
}

use crate::test_ui::painted_text;

/// Far too long for the Path column at the test's 1000pt screen width.
fn deep_path() -> String {
    concat!(
        "/media/shared/QuickSearch/crates/quicksearch-gui/src/",
        "deeply/nested/under/several/more/directories/that/keep/going/",
        "well/past/anything/a/column/could/show/alpha_widget_0.txt"
    )
    .to_string()
}

#[test]
fn long_paths_elide_from_the_middle_of_the_path_column() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = deep_path();
    tab.results[0].path = path.clone();

    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    let cell = painted
        .iter()
        .find(|t| t.starts_with("/media") && t.contains('…'))
        .unwrap_or_else(|| panic!("no elided path cell among {painted:?}"));

    assert!(!painted.contains(&path), "painted in full");
    assert_eq!(cell.matches('…').count(), 1, "elided twice: {cell}");
    let (head, tail) = cell.split_once('…').expect("one ellipsis");
    assert!(path.starts_with(head), "{cell}");
    assert!(path.ends_with(tail), "{cell}");
    assert!(
        tail.ends_with("alpha_widget_0.txt"),
        "the deep end survives: {cell}"
    );
}

#[test]
fn an_elided_path_still_shows_the_whole_thing_on_hover() {
    let ctx = crate::test_ui::ctx();
    // Testing that the tooltip is wired up, not egui's hover timing.
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
    let mut tab = tab_with_results(1);
    let path = deep_path();
    tab.results[0].path = path.clone();

    run_frame(&ctx, &mut tab, vec![]); // settle the table's layout
    for y in 40..250 {
        // x lands in the Path column, past the 220pt Name column.
        let pos = egui::pos2(300.0, y as f32);
        let mut out = run_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(pos)]);
        if tab.hovered_row != Some(0) {
            continue;
        }
        // The tooltip is its own area, so it may land a frame behind.
        for _ in 0..3 {
            if painted_text(&out).contains(&path) {
                return;
            }
            out = run_frame(&ctx, &mut tab, vec![]);
        }
    }
    panic!("the full path never appeared on hover");
}

fn click(pos: egui::Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
    [true, false]
        .into_iter()
        .map(|pressed| egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        })
        .collect()
}

fn hit(id: i64, name: &str, rank: f64, size: u64) -> SearchHit {
    SearchHit {
        file_id: id,
        name: name.to_string(),
        path: format!("/d/{name}"),
        size,
        mtime: 1_700_000_000,
        rank,
        stage: rank as u8,
        snippet: None,
    }
}

fn batch(tab: &mut SearchTab, hits: Vec<SearchHit>) {
    tab.apply_update(
        SearchUpdate::Hits {
            generation: tab.generation,
            hits,
        },
        1000,
    );
    if tab.sort_dirty {
        tab.resort();
    }
}

/// A tab already past the fade, so batches land straight in `results`.
fn streaming_tab() -> SearchTab {
    let mut tab = new_tab();
    tab.focus_query = false;
    tab.query = "zebra".into();
    tab.swap_pending = false;
    tab
}

fn displayed(tab: &SearchTab) -> Vec<&str> {
    tab.order
        .iter()
        .map(|&i| tab.results[i as usize].name.as_str())
        .collect()
}

#[test]
fn later_batches_land_in_the_tables_sort_order() {
    let mut tab = streaming_tab();
    batch(&mut tab, vec![hit(1, "middling.txt", 4.0, 50)]);
    assert_eq!(displayed(&tab), vec!["middling.txt"]);

    batch(&mut tab, vec![hit(2, "best.txt", 1.0, 10)]);
    assert_eq!(displayed(&tab), vec!["best.txt", "middling.txt"]);

    batch(&mut tab, vec![hit(3, "worst.txt", 10.0, 90)]);
    assert_eq!(
        displayed(&tab),
        vec!["best.txt", "middling.txt", "worst.txt"]
    );
}

#[test]
fn batches_respect_a_non_rank_sort_key() {
    let mut tab = streaming_tab();
    tab.sort = (SortKey::Name, true);
    batch(&mut tab, vec![hit(1, "mango.txt", 1.0, 50)]);
    batch(&mut tab, vec![hit(2, "apple.txt", 9.0, 10)]);
    batch(&mut tab, vec![hit(3, "zucchini.txt", 2.0, 90)]);
    assert_eq!(
        displayed(&tab),
        vec!["apple.txt", "mango.txt", "zucchini.txt"],
        "name order, not arrival or rank order"
    );

    tab.columns.size = true;
    tab.sort = (SortKey::Size, false);
    tab.sort_dirty = true;
    tab.resort();
    assert_eq!(
        displayed(&tab),
        vec!["zucchini.txt", "mango.txt", "apple.txt"]
    );
}

#[test]
fn a_sort_key_whose_column_is_hidden_falls_back_to_rank() {
    let cols = ColumnsConfig {
        size: true,
        modified: true,
        ..ColumnsConfig::default()
    };

    for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
        assert_eq!(effective_sort((key, false), &cols), (key, false));
    }
    // The path column cannot be switched off; rank needs no column.
    for key in [SortKey::Path, SortKey::Rank] {
        assert_eq!(
            effective_sort((key, false), &ColumnsConfig::default()),
            (key, false)
        );
    }

    let off = ColumnsConfig {
        name: false,
        size: false,
        modified: false,
        ..ColumnsConfig::default()
    };
    for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
        assert_eq!(
            effective_sort((key, false), &off),
            (SortKey::Rank, true),
            "{key:?} survived its column being hidden"
        );
    }
}

#[test]
fn re_keying_the_sort_mid_stream_reorders_everything() {
    let mut tab = streaming_tab();
    batch(&mut tab, vec![hit(1, "delta.txt", 1.0, 30)]);
    batch(&mut tab, vec![hit(2, "alpha.txt", 5.0, 10)]);
    assert_eq!(
        displayed(&tab),
        vec!["delta.txt", "alpha.txt"],
        "rank order"
    );

    // Header click, mid-search.
    tab.sort = (SortKey::Name, true);
    tab.sort_dirty = true;
    tab.resort();
    assert_eq!(
        displayed(&tab),
        vec!["alpha.txt", "delta.txt"],
        "rows that already arrived re-order under the new key"
    );

    batch(&mut tab, vec![hit(3, "bravo.txt", 2.0, 20)]);
    assert_eq!(
        displayed(&tab),
        vec!["alpha.txt", "bravo.txt", "delta.txt"],
        "and the next batch lands under it too"
    );
}

#[test]
fn a_late_better_hit_displaces_the_worst_at_the_cap() {
    let mut tab = streaming_tab();
    tab.sort = (SortKey::Name, true);
    let limit = 3;

    let send = |tab: &mut SearchTab, hits: Vec<SearchHit>| {
        tab.apply_update(
            SearchUpdate::Hits {
                generation: tab.generation,
                hits,
            },
            limit,
        );
        if tab.sort_dirty {
            tab.resort();
        }
    };

    send(
        &mut tab,
        vec![
            hit(1, "aaa.txt", 9.0, 10),
            hit(2, "bbb.txt", 8.0, 20),
            hit(3, "ccc.txt", 7.0, 30),
        ],
    );
    assert_eq!(displayed(&tab), vec!["aaa.txt", "bbb.txt", "ccc.txt"]);
    assert!(!tab.limited);

    // Full. A rank-1 arrival must still get in, evicting rank 9.
    send(&mut tab, vec![hit(4, "zzz.txt", 1.0, 40)]);
    assert!(tab.limited, "the cap was hit");
    assert_eq!(tab.results.len(), limit);
    assert_eq!(
        displayed(&tab),
        vec!["bbb.txt", "ccc.txt", "zzz.txt"],
        "worst rank dropped, display still in name order"
    );
}

#[test]
fn staged_batches_are_ordered_once_the_fade_swaps() {
    let mut tab = new_tab();
    tab.focus_query = false;
    tab.query = "zebra".into();
    tab.on_search_started(1);
    assert!(tab.swap_pending);

    for h in [hit(1, "worst.txt", 9.0, 10), hit(2, "best.txt", 1.0, 20)] {
        tab.apply_update(
            SearchUpdate::Hits {
                generation: 1,
                hits: vec![h],
            },
            1000,
        );
    }
    assert!(tab.results.is_empty(), "still staged behind the fade");

    // What the fade does when it reaches zero.
    tab.results = std::mem::take(&mut tab.staging);
    tab.swap_pending = false;
    tab.sort_dirty = true;
    tab.resort();
    assert_eq!(displayed(&tab), vec!["best.txt", "worst.txt"]);
}

/// Step the transition at 60 fps until `done`; report how long it took.
fn run_fade(tab: &mut SearchTab, done: impl Fn(&SearchTab) -> bool) -> f32 {
    let dt = 1.0 / 60.0;
    for frame in 0..1000 {
        if done(tab) {
            return frame as f32 * dt;
        }
        tab.advance_fade(dt);
    }
    panic!("the transition never finished");
}

#[test]
fn each_half_of_the_transition_takes_its_own_duration() {
    let mut tab = new_tab();
    tab.swap_pending = true;
    let out = run_fade(&mut tab, |t| t.fade <= 0.0);
    assert_eq!(tab.fade, 0.0, "settles exactly on invisible");
    assert!(
        (out - FADE_OUT_SECS).abs() <= 1.0 / 60.0,
        "clearing runs for FADE_OUT_SECS, took {out}"
    );

    // What `ui` does at the swap.
    tab.swap_pending = false;
    tab.wipe = 1.0;
    let into = run_fade(&mut tab, |t| t.wipe <= 0.0);
    assert_eq!(tab.fade, 1.0, "and the reveal settles on fully opaque");
    assert!(
        (into - FADE_IN_SECS).abs() <= 1.0 / 60.0,
        "the reveal runs for FADE_IN_SECS, took {into}"
    );
}

#[test]
fn a_stalled_frame_does_not_overshoot() {
    let mut tab = new_tab();
    tab.swap_pending = true;
    tab.advance_fade(10.0);
    assert_eq!(tab.fade, 0.0, "a whole ten seconds lands, not passes");

    tab.swap_pending = false;
    tab.wipe = 1.0;
    tab.advance_fade(10.0);
    assert_eq!(tab.wipe, 0.0);
    assert_eq!(tab.fade, 1.0);
}

/// Run backwards, it would flash just-covered rows back on screen.
#[test]
fn clearing_results_holds_the_reveal_where_it_stands() {
    let mut tab = new_tab();
    tab.wipe = 1.0;
    tab.advance_fade(FADE_IN_SECS / 5.0);
    let standing = tab.wipe;
    assert!((standing - 0.8).abs() < 1e-4, "one fifth in: {standing}");
    let carried = tab.fade;

    tab.swap_pending = true;
    tab.advance_fade(0.0);
    assert_eq!(tab.wipe, standing, "the reveal is frozen, not rewound");
    assert_eq!(tab.fade, carried, "and the opacity carries on from here");

    // The scrim holds its position for the whole of the fade-out.
    let out = run_fade(&mut tab, |t| t.fade <= 0.0);
    assert_eq!(tab.wipe, standing, "still frozen at the end of it");
    let expected = carried * FADE_OUT_SECS;
    assert!(
        (out - expected).abs() <= 1.0 / 60.0,
        "a partly faded section clears proportionally: {out} vs {expected}"
    );
}

#[test]
fn a_settled_section_stops_asking_for_frames() {
    let mut tab = new_tab();
    // The steady state must not repaint forever.
    assert!(tab.fade_settled());
    tab.advance_fade(1.0 / 60.0);
    assert!(tab.fade_settled());

    // Each stage of a transition keeps the frames coming, the swap frame included.
    tab.swap_pending = true;
    assert!(!tab.fade_settled());
    tab.advance_fade(FADE_OUT_SECS);
    tab.swap_pending = false;
    tab.wipe = 1.0;
    assert!(!tab.fade_settled());
}

/// Drive frames until the reveal has uncovered all but `to` of the section.
/// `run_frame` leaves `RawInput::time` unset, so the tab sees a steady `stable_dt`.
fn reveal_to(ctx: &egui::Context, tab: &mut SearchTab, to: f32) -> egui::FullOutput {
    for _ in 0..200 {
        let out = run_frame(ctx, tab, vec![]);
        if tab.wipe <= to {
            return out;
        }
    }
    panic!("the reveal never got down to {to}");
}

/// Drive frames until the staged results swap in.
fn swap_in(ctx: &egui::Context, tab: &mut SearchTab) -> egui::FullOutput {
    for _ in 0..200 {
        let out = run_frame(ctx, tab, vec![]);
        if !tab.swap_pending {
            return out;
        }
    }
    panic!("the staged results never swapped in");
}

fn stage_results(tab: &mut SearchTab) {
    batch(
        tab,
        (0..20)
            .map(|i| hit(i, &format!("alpha_widget_{i}.txt"), 3.0, 116))
            .collect(),
    );
}

fn row_bounds(out: &egui::FullOutput) -> (egui::Rect, egui::Rect) {
    let rows: Vec<egui::Rect> = crate::test_ui::painted(out)
        .into_iter()
        .filter(|(text, _)| text.starts_with("alpha_widget_"))
        .map(|(_, rect)| rect)
        .collect();
    (
        *rows.first().expect("rows painted"),
        *rows.last().expect("rows painted"),
    )
}

/// Vertices down the scrim as (y, alpha), in paint order.
fn scrim(out: &egui::FullOutput) -> Vec<(f32, u8)> {
    let meshes = crate::test_ui::painted_meshes(out);
    assert_eq!(meshes.len(), 1, "one scrim over the section, no more");
    meshes[0]
        .vertices
        .iter()
        .map(|v| (v.pos.y, v.color.a()))
        .collect()
}

/// The y where the scrim first turns fully solid.
fn solid_from(ramp: &[(f32, u8)]) -> f32 {
    ramp.iter()
        .find(|&&(_, a)| a == 255)
        .expect("a solid stretch")
        .0
}

#[test]
fn clearing_results_paints_no_scrim() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(20);

    let settled = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        crate::test_ui::painted_meshes(&settled).is_empty(),
        "a settled table pays nothing for the effect"
    );

    tab.on_search_started(1);
    stage_results(&mut tab);
    let mut dimmest: f32 = 1.0;
    for frame in 0..200 {
        let out = run_frame(&ctx, &mut tab, vec![]);
        if !tab.swap_pending {
            // The swap frame belongs to the new set, which starts covered.
            assert!(frame > 0, "the fade-out was over before it began");
            break;
        }
        assert!(
            crate::test_ui::painted_meshes(&out).is_empty(),
            "no scrim while the old results clear, at fade {}",
            tab.fade
        );
        dimmest = dimmest.min(tab.fade);
    }
    assert!(
        dimmest < 0.35,
        "the section really does dim on the way out: got no lower than {dimmest}"
    );
}

/// The scrim carries its own alpha — painted through the `Ui` it would be
/// scaled by the section opacity, which is exactly zero on this frame.
#[test]
fn new_results_start_completely_covered() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(20);
    run_frame(&ctx, &mut tab, vec![]);

    tab.on_search_started(1);
    stage_results(&mut tab);
    let swapped = swap_in(&ctx, &mut tab);
    assert_eq!(tab.wipe, 1.0, "the reveal starts from the top");

    let ramp = scrim(&swapped);
    assert!(
        ramp.iter().all(|&(_, a)| a == 255),
        "nothing shows through: {ramp:?}"
    );
    assert!(
        !crate::test_ui::painted_text(&swapped)
            .iter()
            .any(|t| t.starts_with("alpha_widget_")),
        "at zero opacity egui drops the section's shapes outright, so \
         the scrim is belt to that braces"
    );

    // The rows must be measured from a frame with some light let through.
    let (first, last) = row_bounds(&reveal_to(&ctx, &mut tab, 0.8));
    let (top, bottom) = (
        ramp.first().expect("vertices").0,
        ramp.last().expect("vertices").0,
    );
    assert!(
        top < first.top(),
        "the scrim starts above the first row, so the column headers \
         are covered too: {top} vs {}",
        first.top()
    );
    assert!(
        bottom >= last.bottom(),
        "and runs past the last one: {bottom} vs {}",
        last.bottom()
    );
}

#[test]
fn new_results_are_uncovered_from_the_top_down() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(20);
    run_frame(&ctx, &mut tab, vec![]);

    tab.on_search_started(1);
    stage_results(&mut tab);
    swap_in(&ctx, &mut tab);

    // A fifth of the way in: head uncovered, foot still under.
    let early_frame = reveal_to(&ctx, &mut tab, 0.8);
    let (first, last) = row_bounds(&early_frame);
    let early = solid_from(&scrim(&early_frame));
    assert!(
        early > first.bottom(),
        "the first row is readable a fifth of the way in: {early} vs {}",
        first.bottom()
    );
    assert!(
        early < last.top(),
        "while the last is still covered: {early} vs {}",
        last.top()
    );

    // …and the edge keeps going down, not back up.
    let later = solid_from(&scrim(&reveal_to(&ctx, &mut tab, 0.4)));
    assert!(
        later > early,
        "the edge travels downward: {early} then {later}"
    );
    assert!(
        later > last.top(),
        "and has uncovered the last row by then: {later} vs {}",
        last.top()
    );

    let done = reveal_to(&ctx, &mut tab, 0.0);
    assert!(
        crate::test_ui::painted_meshes(&done).is_empty(),
        "the scrim clears away at the end of the reveal"
    );
    assert!(tab.fade_settled(), "and the section stops animating");
}

#[test]
fn the_selection_follows_its_file_across_batches() {
    let mut tab = streaming_tab();
    batch(&mut tab, vec![hit(1, "chosen.txt", 5.0, 10)]);
    tab.selected = Some(0);

    batch(&mut tab, vec![hit(2, "better.txt", 1.0, 20)]);
    let sel = tab.selected.expect("still selected");
    assert_eq!(
        tab.results[sel as usize].file_id, 1,
        "selection follows the file, not the slot"
    );
}

#[test]
fn rows_respond_over_selectable_label_text() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(3);
    run_frame(&ctx, &mut tab, Vec::new());

    // Hovering glyphs still marks the row hovered (drives the hover fill).
    let pos = hover_row_text(&ctx, &mut tab, 0);

    // Left click on glyphs selects the row.
    run_frame(&ctx, &mut tab, click(pos, egui::PointerButton::Primary));
    assert_eq!(tab.selected, Some(0));

    // Right click on glyphs selects the row and opens the context menu.
    let pos = hover_row_text(&ctx, &mut tab, 1);
    run_frame(&ctx, &mut tab, click(pos, egui::PointerButton::Secondary));
    assert_eq!(tab.selected, Some(1));
    assert!(egui::Popup::is_any_open(&ctx));
}

/// A drive root must not become the never-matching `C:\/*`.
#[test]
fn dir_ignore_patterns_use_the_platform_separator() {
    use std::path::Path;
    #[cfg(unix)]
    {
        assert_eq!(dir_ignore_pattern(Path::new("/home/x")), "/home/x/*");
        assert_eq!(dir_ignore_pattern(Path::new("/")), "/*");
    }
    #[cfg(windows)]
    {
        assert_eq!(
            dir_ignore_pattern(Path::new(r"C:\Users\x")),
            r"C:\Users\x\*"
        );
        assert_eq!(dir_ignore_pattern(Path::new(r"C:\")), r"C:\*");
    }
}

#[test]
fn the_recency_fade_ends_at_the_theme_color() {
    with_ui(|ui| {
        let now = quicksearch_core::log::now_unix() as i64;
        let ancient = recency_color(ui, now - 60 * 60 * 24 * 365 * 20);
        assert_eq!(ancient, ui.visuals().weak_text_color());
        let fresh = recency_color(ui, now);
        assert_eq!(fresh, crate::color::palette(ui.visuals().dark_mode).green);
    });
}

// --- snippet rendering ----------------------------------------------

use crate::test_ui::{painted_rows, with_ui};

/// The rows `job` actually lays out — `Galley::text` includes every row
/// epaint dropped at `wrap.max_rows`, so it cannot see a truncation.
fn laid_out_rows(ui: &egui::Ui, job: LayoutJob) -> Vec<String> {
    ui.fonts(|f| f.layout_job(job))
        .rows
        .iter()
        .map(|r| r.text())
        .collect()
}

/// A content snippet whose multi-byte lead-in is `lines` short lines:
/// snippet ranges are byte offsets while row arithmetic counts characters.
fn ragged_snippet(lines: usize) -> Snippet {
    let lead = "café\n".repeat(lines);
    Snippet {
        ranges: vec![(lead.len(), lead.len() + 6)],
        window: format!("{lead}NEEDLE and trailing context"),
        truncated_start: true,
        truncated_end: true,
    }
}

#[test]
fn the_hover_snippet_keeps_the_match_when_the_lead_in_is_all_newlines() {
    with_ui(|ui| {
        ui.set_max_width(520.0); // what the Match cell's tooltip sets
        let snip = ragged_snippet(40);
        let rows = laid_out_rows(ui, snippet_job(ui, &snip, 10));
        assert!(rows.len() <= 10, "over the row budget: {rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("NEEDLE")),
            "the match never made it on screen: {rows:#?}"
        );
        // Trimmed at a line boundary and said so: a start landing mid
        // character would read "…afé" — or panic off a char boundary.
        assert_eq!(rows[0], "… café", "{rows:#?}");
    });
}

#[test]
fn the_preview_strip_keeps_the_match_too() {
    with_ui(|ui| {
        let snip = ragged_snippet(40);
        let rows = laid_out_rows(ui, snippet_job(ui, &snip, 2));
        assert!(rows.len() <= 2, "over the row budget: {rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("NEEDLE")),
            "the match never made it on screen: {rows:#?}"
        );
    });
}

#[test]
fn a_snippet_that_fits_is_left_alone() {
    with_ui(|ui| {
        let snip = Snippet {
            window: "alpha beta NEEDLE gamma".into(),
            ranges: vec![(11, 17)],
            truncated_start: false,
            truncated_end: false,
        };
        assert_eq!(
            laid_out_rows(ui, snippet_job(ui, &snip, 10)),
            vec!["alpha beta NEEDLE gamma".to_string()]
        );
    });
}

/// The cell is laid out in Extend mode (infinite wrap width), so only its
/// own budget keeps it inside the column.
#[test]
fn the_content_match_cell_stays_inside_its_column() {
    with_ui(|ui| {
        let snip = Snippet {
            window: "a long stretch of leading context NEEDLE and a long tail after it".into(),
            ranges: vec![(34, 40)],
            truncated_start: true,
            truncated_end: true,
        };
        // Down to widths the column itself cannot reach.
        for width in [20.0, 60.0, 90.0, 120.0, 150.0, 240.0, 400.0, 4000.0] {
            let job = centered_match_job(ui, &snip, width);
            let galley = ui.fonts(|f| f.layout_job(job));
            assert!(
                galley.size().x <= width,
                "{}pt of text in a {width}pt column: {:?}",
                galley.size().x,
                galley.text()
            );
            // At the column's 120pt floor the whole hit must survive.
            let kept = if width >= 120.0 { "NEEDLE" } else { "N" };
            assert!(
                galley.text().contains(kept),
                "the match was budgeted away at {width}pt: {:?}",
                galley.text()
            );
        }
    });
}

/// The cell paints the match once, so the tooltip is the *second* appearance.
#[test]
fn hovering_the_content_match_cell_shows_the_match_in_the_tooltip() {
    let ctx = crate::test_ui::ctx();
    // Testing that the tooltip carries the match, not egui's hover timing.
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
    let mut tab = tab_with_results(1);
    tab.results[0].stage = 6; // a full-text stage
    tab.results[0].snippet = Some(ragged_snippet(40));

    run_frame(&ctx, &mut tab, vec![]); // settle the table's layout

    // Find the row over the Name column, then sweep for the Match cell:
    // where it sits depends on the window and which columns are showing.
    let mut row_y = None;
    for y in 40..250 {
        run_frame(
            &ctx,
            &mut tab,
            vec![egui::Event::PointerMoved(egui::pos2(60.0, y as f32))],
        );
        if tab.hovered_row == Some(0) {
            row_y = Some(y as f32);
            break;
        }
    }
    let row_y = row_y.expect("no row under the pointer anywhere down the name column");

    for x in (80..960).step_by(20) {
        let pos = egui::pos2(x as f32, row_y);
        let mut out = run_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(pos)]);
        // The tooltip is its own area, so it may land a frame behind.
        for _ in 0..3 {
            let showing = painted_rows(&out)
                .iter()
                .filter(|r| r.contains("NEEDLE"))
                .count();
            if showing >= 2 {
                return;
            }
            out = run_frame(&ctx, &mut tab, vec![]);
        }
    }
    panic!("the match never appeared in the hover tooltip");
}

// --- Columns, highlighting, and the query strip ---------------------------

use crate::test_ui::{click_at, painted, painted_backgrounds, painted_text_center};

fn run_frame_actions(
    ctx: &egui::Context,
    tab: &mut SearchTab,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, SearchActions) {
    frame_at(ctx, tab, 1000.0, events)
}

/// A filename match, carrying the whole-field snippet core promises.
fn name_hit(name: &str, mark: (usize, usize)) -> SearchHit {
    SearchHit {
        file_id: 1,
        name: name.to_string(),
        path: format!("/qs-test/{name}"),
        size: 116,
        mtime: 1_700_000_000,
        rank: 3.0,
        stage: 3,
        snippet: Some(Snippet {
            window: name.to_string(),
            ranges: vec![mark],
            truncated_start: false,
            truncated_end: false,
        }),
    }
}

/// The runs painted as a *matched* span, keyed on the highlight's background
/// (headers are painted strong too, and the rank chip has its own background).
fn highlight_runs(out: &egui::FullOutput, ctx: &egui::Context) -> Vec<String> {
    let marked = ctx.style().visuals.selection.bg_fill.gamma_multiply(0.4);
    painted_backgrounds(out)
        .into_iter()
        .filter(|(_, bg)| *bg == marked)
        .map(|(text, _)| text)
        .collect()
}

#[test]
fn size_and_modified_are_off_by_default_and_can_be_switched_on() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);

    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(!painted.contains(&"Size".to_string()), "{painted:?}");
    assert!(!painted.contains(&"Modified".to_string()), "{painted:?}");
    assert!(!painted.contains(&"116 B".to_string()), "{painted:?}");
    assert!(painted.contains(&"Path".to_string()), "{painted:?}");

    tab.columns.size = true;
    tab.columns.modified = true;
    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(painted.contains(&"Size".to_string()), "{painted:?}");
    assert!(painted.contains(&"Modified".to_string()), "{painted:?}");
    assert!(painted.contains(&"116 B".to_string()), "{painted:?}");
}

// --- Column widths -------------------------------------------------------

fn flex(specs: &[(f32, f32)]) -> Vec<ColumnPlan> {
    specs
        .iter()
        .map(|&(floor, initial)| ColumnPlan {
            kind: ColumnKind::Flex,
            floor,
            initial,
        })
        .collect()
}

/// The results table's own shape: three flexing text columns then Rank fixed.
fn text_and_rank() -> Vec<ColumnPlan> {
    let mut p = flex(&[(80.0, 220.0), (120.0, 320.0), (120.0, 320.0)]);
    p.push(ColumnPlan {
        kind: ColumnKind::Fixed,
        floor: 40.0,
        initial: 52.0,
    });
    p
}

/// AG Grid's own worked example, transcribed: a 450px grid holding one 150px
/// fixed column, one `flex: 1` and one `flex: 2` lays out 150 / 100 / 200.
/// Weights here are the columns' current widths (100 and 200).
#[test]
fn flex_columns_divide_what_the_fixed_ones_leave() {
    let plans = vec![
        ColumnPlan {
            kind: ColumnKind::Fixed,
            floor: 50.0,
            initial: 150.0,
        },
        ColumnPlan {
            kind: ColumnKind::Flex,
            floor: 50.0,
            initial: 100.0,
        },
        ColumnPlan {
            kind: ColumnKind::Flex,
            floor: 50.0,
            initial: 200.0,
        },
    ];
    assert_eq!(
        fit_widths(&[150.0, 100.0, 200.0], &plans, 450.0),
        vec![150.0, 100.0, 200.0]
    );
    // The fixed column keeps its 150; the flex pair shares the rest, still 1:2.
    assert_eq!(
        fit_widths(&[150.0, 100.0, 200.0], &plans, 750.0),
        vec![150.0, 200.0, 400.0]
    );
}

#[test]
fn a_fixed_column_keeps_its_width_when_the_window_grows() {
    let plans = text_and_rank();
    let narrow = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, 912.0);
    let wide = fit_widths(&narrow, &plans, 1512.0);

    assert_eq!(narrow[3], 52.0);
    assert_eq!(wide[3], 52.0, "Rank grew with the window");
    // The 600 went to the three text columns, in proportion.
    for i in 0..3 {
        assert!(
            wide[i] > narrow[i],
            "flex column {i} did not take its share"
        );
    }
    assert!((wide.iter().sum::<f32>() - 1512.0).abs() < 0.01, "{wide:?}");
}

#[test]
fn fixed_columns_shrink_only_once_the_flex_ones_have_bottomed_out() {
    let plans = text_and_rank();
    // The flex floors come to 320 and Rank sits at 52: 372 wanted, 365 there.
    let widths = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, 365.0);
    assert_eq!(&widths[..3], &[80.0, 120.0, 120.0], "flex floors first");
    assert!(
        widths[3] < 52.0,
        "Rank should have given up the difference: {widths:?}"
    );
    assert!(
        (widths.iter().sum::<f32>() - 365.0).abs() < 0.01,
        "{widths:?}"
    );
    assert!(
        widths[3] >= 40.0,
        "Rank went under its own floor: {widths:?}"
    );
}

/// Including the space freed by narrowing, the case that used to leave a
/// blank strip down the right of the table.
#[test]
fn a_dragged_column_is_taken_as_given_and_the_rest_absorb() {
    let plans = text_and_rank();
    let budget = 912.0;
    let before = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, budget);

    // Name dragged down to 100.
    let mut dragged = before.clone();
    dragged[0] = 100.0;
    let after = fit_around(&dragged, &plans, budget, Some(0));

    assert_eq!(after[0], 100.0, "the drag was overruled");
    assert!(
        (after.iter().sum::<f32>() - budget).abs() < 0.01,
        "{after:?}"
    );
    assert!(after[1] > before[1] && after[2] > before[2], "{after:?}");
    assert_eq!(after[3], 52.0, "a fixed column absorbed the drag");

    // Held no wider than leaves everyone else their floor.
    let hogged = fit_around(&[5000.0, 320.0, 320.0, 52.0], &plans, budget, Some(0));
    assert!(
        (hogged.iter().sum::<f32>() - budget).abs() < 0.01,
        "a drag overflowed the table: {hogged:?}"
    );
}

/// The rule chosen over AG Grid's: a dragged column rejoins the pool, so a
/// resize keeps the *share* it was given rather than those pixels.
#[test]
fn a_dragged_flex_column_keeps_its_share_across_a_resize() {
    let plans = text_and_rank();
    let dragged = fit_around(&[100.0, 320.0, 320.0, 52.0], &plans, 912.0, Some(0));
    let flex_total: f32 = dragged[..3].iter().sum();
    let share = dragged[0] / flex_total;

    let resized = fit_widths(&dragged, &plans, 1512.0);
    let resized_share = resized[0] / resized[..3].iter().sum::<f32>();
    assert!(
        (resized_share - share).abs() < 0.001,
        "the share moved from {share} to {resized_share}"
    );
    assert!(resized[0] > dragged[0], "it did not scale up with the rest");
}

#[test]
fn refitting_fills_the_budget_and_keeps_the_proportions() {
    let p = flex(&[(80.0, 220.0), (120.0, 260.0), (120.0, 260.0)]);
    // No floor binds at this budget, so this is the proportions alone.
    let widths = fit_widths(&[100.0, 200.0, 100.0], &p, 800.0);

    assert!(
        (widths.iter().sum::<f32>() - 800.0).abs() < 0.01,
        "the columns must fill the budget exactly: {widths:?}"
    );
    assert_eq!(widths, vec![200.0, 400.0, 200.0]);
}

#[test]
fn a_column_pinned_to_its_floor_does_not_starve_the_others() {
    let p = flex(&[(80.0, 220.0), (120.0, 260.0), (120.0, 260.0)]);
    // Scaling 1:8:1 into 400 would give the outer two 40, under both floors.
    let widths = fit_widths(&[100.0, 800.0, 100.0], &p, 400.0);

    assert_eq!(widths[0], 80.0, "the name column is at its floor");
    assert_eq!(widths[2], 120.0, "the content column is at its floor");
    assert_eq!(widths[1], 200.0, "the rest went to the free column");
    assert!(
        (widths.iter().sum::<f32>() - 400.0).abs() < 0.01,
        "{widths:?}"
    );

    // Narrower than the floors add up to, the floors win and the table overflows.
    assert_eq!(
        fit_widths(&[200.0, 200.0, 200.0], &p, 100.0),
        vec![80.0, 120.0, 120.0]
    );
}

#[test]
fn refitting_without_a_measurement_splits_the_budget_evenly() {
    let p = flex(&[(80.0, 220.0), (120.0, 260.0)]);
    let widths = fit_widths(&[0.0, 0.0], &p, 400.0);
    assert_eq!(widths, vec![200.0, 200.0]);
    assert!(fit_widths(&[], &[], 400.0).is_empty());
}

/// The bug this exists for: `egui_extras` reloads a resizable column as
/// `Size::exact(stored_width)`, so nothing re-fits on its own.
#[test]
fn the_columns_follow_the_window_when_it_is_resized() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(3);
    tab.columns = ColumnsConfig {
        name: true,
        content_match: true,
        size: false,
        modified: false,
        rank: true,
    };

    let width_of = |ctx: &egui::Context, tab: &mut SearchTab, w: f32| -> Vec<f32> {
        // The first frame lays out at the new width, the second measures it.
        for _ in 0..2 {
            let input = crate::test_ui::raw_input(egui::vec2(w, 700.0), Vec::new());
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    tab.ui(ui);
                });
            });
        }
        tab.col_widths.clone()
    };

    let wide = width_of(&ctx, &mut tab, 1200.0);
    assert_eq!(wide.len(), 4, "name, path, content match, rank");
    let wide_total: f32 = wide.iter().sum();

    let narrow = width_of(&ctx, &mut tab, 700.0);
    let narrow_total: f32 = narrow.iter().sum();
    assert!(
        narrow_total < wide_total - 400.0,
        "the columns did not follow the window down: {wide_total} then {narrow_total}"
    );
    assert!(
        narrow_total <= 700.0,
        "the table is wider than its window: {narrow_total} in 700"
    );

    // And back out: the columns have to grow with the window too.
    let regrown: f32 = width_of(&ctx, &mut tab, 1200.0).iter().sum();
    assert!(
        regrown > narrow_total + 400.0,
        "the columns did not follow the window back up: {narrow_total} then {regrown}"
    );
}

/// Once the table fills its window, a column can only be widened by
/// narrowing another first.
#[test]
fn a_column_cannot_be_dragged_wider_than_the_slack_that_is_left() {
    let p = flex(&[(80.0, 220.0), (120.0, 260.0)]);
    let budget = 400.0;
    let current = fit_widths(&[200.0, 200.0], &p, budget);
    let slack = budget - current.iter().sum::<f32>();
    assert!(slack.abs() < 0.01, "the fixture must start full: {slack}");

    // This is the bound the table hands egui_extras as `at_most`.
    for (&w, plan) in current.iter().zip(&p) {
        let at_most = (w + slack).max(plan.floor);
        assert!(
            at_most <= w + 0.01,
            "a full table still offered {at_most} of room for a {w} column"
        );
    }
}

/// End to end through egui_extras' own drag handling: haul the first
/// column's handle far past the window's edge. The contract is where the
/// table *settles*, not every frame between — egui_extras lays out from
/// widths stored a frame earlier, so each 400pt step is settled before
/// measuring.
#[test]
fn dragging_a_column_cannot_push_the_table_off_the_edge() {
    const W: f32 = 900.0;
    const STEP: f32 = 400.0;
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(3);
    tab.columns = ColumnsConfig {
        name: true,
        content_match: true,
        size: false,
        modified: false,
        rank: true,
    };

    fn frame(ctx: &egui::Context, tab: &mut SearchTab, events: Vec<egui::Event>) {
        frame_at(ctx, tab, W, events);
    }
    fn total(tab: &SearchTab) -> f32 {
        tab.col_widths.iter().sum()
    }
    /// A fixed count, not "stop when two frames agree": the total sits
    /// unchanged for the frames the measurement and reflow spend in the
    /// pipeline, so stopping on the first repeat is premature.
    fn settle(ctx: &egui::Context, tab: &mut SearchTab) -> f32 {
        for _ in 0..8 {
            frame(ctx, tab, Vec::new());
        }
        total(tab)
    }
    let settled = settle(&ctx, &mut tab);
    assert!(settled <= W, "the fixture starts overflowing: {settled}");

    // The Name column's right edge, then a drag far beyond the window.
    let handle_x = tab.col_widths[0] + 10.0;
    let y = 60.0;
    frame(
        &ctx,
        &mut tab,
        vec![
            egui::Event::PointerMoved(egui::pos2(handle_x, y)),
            egui::Event::PointerButton {
                pos: egui::pos2(handle_x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ],
    );
    for step in 1..=8 {
        let x = handle_x + step as f32 * STEP;
        frame(
            &ctx,
            &mut tab,
            vec![egui::Event::PointerMoved(egui::pos2(x, y))],
        );
        let total = settle(&ctx, &mut tab);
        assert!(
            total <= W + 1.0,
            "the drag settled at {total} inside a {W} window"
        );
    }
    frame(
        &ctx,
        &mut tab,
        vec![egui::Event::PointerButton {
            pos: egui::pos2(handle_x + 8.0 * STEP, y),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }],
    );
    let total = settle(&ctx, &mut tab);
    assert!(total <= W + 1.0, "released at {total} inside a {W} window");
    assert!(
        total >= settled - 1.0,
        "the table gave up {} points of its window",
        settled - total
    );
    assert_eq!(tab.col_widths.len(), 4, "a column was dropped entirely");
    for (i, &w) in tab.col_widths.iter().enumerate() {
        assert!(w > 0.0, "column {i} was squeezed out of existence: {w}");
    }
    // The other columns' floors are the bound that stopped it, not the edge.
    assert!(
        tab.col_widths[0] > settled / 2.0,
        "the drag barely moved: {:?}",
        tab.col_widths
    );
}

/// The property the whole drag rests on: a divider drag must not move any
/// column to its left, or the divider walks away from the cursor (see
/// `fit_around`) — which is what made dragging uncontrollable.
#[test]
fn a_drag_leaves_every_column_to_its_left_alone() {
    let plans = text_and_rank();
    let budget = 912.0;
    let before = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, budget);

    // Hauled right by less than the columns to its right can pay for.
    let mut dragged = before.clone();
    dragged[1] = before[1] + 150.0;
    let after = fit_around(&dragged, &plans, budget, Some(1));

    assert_eq!(after[0], before[0], "the name column moved under the drag");
    assert_eq!(after[1], dragged[1], "the drag was overruled");
    assert_eq!(
        after[..2].iter().sum::<f32>(),
        dragged[..2].iter().sum::<f32>(),
        "the divider did not land where the drag put it"
    );
    assert!(
        after[2] < before[2],
        "nothing to the right absorbed: {after:?}"
    );
    assert!(
        (after.iter().sum::<f32>() - budget).abs() < 0.01,
        "{after:?}"
    );
}

#[test]
fn a_drag_leftwards_hands_the_space_to_the_right() {
    let plans = text_and_rank();
    let budget = 912.0;
    let before = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, budget);

    let mut dragged = before.clone();
    dragged[1] = before[1] - 120.0;
    let after = fit_around(&dragged, &plans, budget, Some(1));

    assert_eq!(after[0], before[0], "the column left of the divider moved");
    assert_eq!(after[1], dragged[1], "the drag was overruled");
    assert!(
        after[2] > before[2],
        "the space was not handed on: {after:?}"
    );
    assert!(
        (after.iter().sum::<f32>() - budget).abs() < 0.01,
        "{after:?}"
    );
}

#[test]
fn the_ceiling_is_what_the_columns_to_the_right_can_give() {
    let plans = text_and_rank();
    let budget = 912.0;
    let widths = fit_widths(&[220.0, 320.0, 320.0, 52.0], &plans, budget);

    // Name may take everything the others hold above their floors, no more.
    let ceiling = grow_ceiling(&widths, &plans, budget, 0);
    assert!(
        (ceiling - (budget - 120.0 - 120.0 - 40.0)).abs() < 0.01,
        "{ceiling}"
    );

    // Rank has nothing to its right, so its divider is inert.
    let last = plans.len() - 1;
    assert!(
        (grow_ceiling(&widths, &plans, budget, last) - widths[last]).abs() < 1.0,
        "the rightmost divider offered room it does not have"
    );

    // Dragging to the ceiling still fits, with the right-hand columns floored.
    let mut hauled = widths.clone();
    hauled[0] = ceiling + 500.0;
    let after = fit_around(&hauled, &plans, budget, Some(0));
    assert!(
        (after.iter().sum::<f32>() - budget).abs() < 0.01,
        "{after:?}"
    );
    assert_eq!(&after[1..], &[120.0, 120.0, 40.0]);
}

/// The two symptoms as reported: widening did nothing at all, and the split
/// would not stay under the cursor.
#[test]
fn a_dragged_divider_widens_its_column_and_stays_under_the_cursor() {
    const W: f32 = 1000.0;
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(3);
    tab.columns = ColumnsConfig {
        name: true,
        content_match: true,
        size: false,
        modified: false,
        rank: true,
    };

    fn frame(ctx: &egui::Context, tab: &mut SearchTab, events: Vec<egui::Event>) {
        frame_at(ctx, tab, W, events);
    }
    for _ in 0..4 {
        frame(&ctx, &mut tab, Vec::new());
    }
    let started_at = tab.col_widths[0];

    // The Name|Path divider; egui's grab radius covers the panel margin.
    let y = 60.0;
    let grab = tab.col_widths[0] + 10.0;
    frame(
        &ctx,
        &mut tab,
        vec![
            egui::Event::PointerMoved(egui::pos2(grab, y)),
            egui::Event::PointerButton {
                pos: egui::pos2(grab, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ],
    );

    // Comparing *deltas* is independent of the panel margin, and is the
    // exact statement of "the split stays under the cursor".
    let mut x = grab;
    for step in 1..=10 {
        let before = tab.col_widths[0];
        x += 20.0;
        frame(
            &ctx,
            &mut tab,
            vec![egui::Event::PointerMoved(egui::pos2(x, y))],
        );
        frame(&ctx, &mut tab, Vec::new());
        let moved = tab.col_widths[0] - before;
        // The first step also takes up the grab-radius slack; every step
        // after that is pure tracking.
        if step > 1 {
            assert!(
                (moved - 20.0).abs() < 0.5,
                "step {step}: the pointer moved 20 and the divider moved {moved}"
            );
        }
    }

    // Before the fix the ceiling was the column's own width, so widening
    // was clamped away on every frame.
    assert!(
        tab.col_widths[0] > started_at + 150.0,
        "the drag barely widened the column: {started_at} to {}",
        tab.col_widths[0]
    );
    assert!(
        tab.col_widths.iter().sum::<f32>() <= W,
        "the table overflowed its window: {:?}",
        tab.col_widths
    );
}

/// The path identifies a result on its own, so it is painted under every
/// combination the picker can produce.
#[test]
fn the_path_column_survives_every_column_combination() {
    let ctx = crate::test_ui::ctx();
    for bits in 0..32u8 {
        let mut tab = tab_with_results(1);
        tab.columns = ColumnsConfig {
            name: bits & 1 != 0,
            content_match: bits & 2 != 0,
            size: bits & 4 != 0,
            modified: bits & 8 != 0,
            rank: bits & 16 != 0,
        };
        let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
        assert!(
            painted.contains(&"Path".to_string()),
            "columns {:?} lost the path header",
            tab.columns
        );
        assert!(
            painted.iter().any(|t| t.contains("alpha_widget_0.txt")),
            "columns {:?} painted no path",
            tab.columns
        );
    }
}

/// A checked box that paints nothing is indistinguishable from a bug, which
/// is what the last one was taken for.
#[test]
fn the_content_match_column_follows_only_its_checkbox() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(2);
    assert!(
        tab.results
            .iter()
            .all(|h| h.match_field() == MatchField::Name),
        "the fixture stopped being a filename-only result set"
    );

    tab.columns.content_match = true;
    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        painted.contains(&"Content Match".to_string()),
        "{painted:?}"
    );
    assert_eq!(
        painted.iter().filter(|t| *t == NO_CONTENT_MATCH).count(),
        2,
        "expected one dash per row: {painted:?}"
    );

    tab.columns.content_match = false;
    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        !painted.contains(&"Content Match".to_string()),
        "{painted:?}"
    );
    assert!(
        !painted.contains(&NO_CONTENT_MATCH.to_string()),
        "{painted:?}"
    );
}

#[test]
fn right_clicking_any_header_opens_the_column_picker() {
    for header in ["Name", "Path", "Rank"] {
        let ctx = crate::test_ui::ctx();
        let mut tab = tab_with_results(1);
        let out = run_frame(&ctx, &mut tab, vec![]);
        let pos = painted_text_center(&out, header)
            .unwrap_or_else(|| panic!("no {header} header painted"));
        run_frame(&ctx, &mut tab, click(pos, egui::PointerButton::Secondary));
        assert!(
            egui::Popup::is_any_open(&ctx),
            "right-clicking {header} opened no menu"
        );
    }
}

#[test]
fn a_filename_match_is_highlighted_in_the_name_column() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.results[0] = name_hit("quarterly_budget.txt", (10, 16));

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert_eq!(highlight_runs(&out, &ctx), vec!["budget".to_string()]);

    let dash = painted_text_center(&out, NO_CONTENT_MATCH).expect("no dash painted");
    let header = painted_text_center(&out, "Content Match").expect("no Content Match header");
    assert!(
        (dash.x - header.x).abs() < 30.0,
        "the dash is not in the Content Match column: {dash:?} vs {header:?}"
    );
}

/// The dash stays: it describes the *content* column, still telling the truth.
#[test]
fn hiding_the_name_column_skips_its_highlight() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.results[0] = name_hit("quarterly_budget.txt", (10, 16));
    tab.columns.name = false;

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        highlight_runs(&out, &ctx).is_empty(),
        "a highlight was painted with the Name column hidden"
    );
    assert!(painted_text_center(&out, NO_CONTENT_MATCH).is_some());
}

/// A snippet that is *not* the field verbatim indexes a window, so its
/// ranges would mark the wrong glyphs; painting nothing is the only safe answer.
#[test]
fn a_name_snippet_that_is_not_the_name_paints_no_highlight() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let mut hit = name_hit("a_very_long_quarterly_budget_report.txt", (0, 6));
    // What `window_around` used to hand back: a suffix, with rebased ranges.
    hit.snippet = Some(Snippet {
        window: "budget_report.txt".to_string(),
        ranges: vec![(0, 6)],
        truncated_start: true,
        truncated_end: false,
    });
    tab.results[0] = hit;

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        highlight_runs(&out, &ctx).is_empty(),
        "a windowed snippet was trusted to index the name"
    );
    assert!(painted_text(&out)
        .iter()
        .any(|t| t == "a_very_long_quarterly_budget_report.txt"));
}

#[test]
fn a_path_match_is_highlighted_in_the_path_column() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = "/qs-test/reports/alpha_widget_0.txt";
    tab.results[0].path = path.to_string();
    tab.results[0].stage = 10;
    tab.results[0].snippet = Some(Snippet {
        window: path.to_string(),
        ranges: vec![(9, 16)],
        truncated_start: false,
        truncated_end: false,
    });

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert_eq!(highlight_runs(&out, &ctx), vec!["reports".to_string()]);
}

/// A match lost to elision must not be re-based onto whatever glyphs sit at
/// those offsets in the shortened string.
#[test]
fn a_path_match_lost_to_elision_paints_no_stray_highlight() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = deep_path();
    tab.results[0].path = path.clone();
    tab.results[0].stage = 10;
    // "several" sits deep in the middle, which is what elision drops.
    let at = path.find("several").expect("fixture contains it");
    tab.results[0].snippet = Some(Snippet {
        window: path.clone(),
        ranges: vec![(at, at + "several".len())],
        truncated_start: false,
        truncated_end: false,
    });

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        !highlight_runs(&out, &ctx).contains(&"several".to_string()),
        "an elided-away match was painted anyway"
    );
}

/// The elision mark is the one weak part: punctuation the renderer added,
/// not part of the path.
#[test]
fn the_path_column_paints_at_full_strength_but_marks_its_elision_weak() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = deep_path();
    tab.results[0].path = path.clone();

    let out = run_frame(&ctx, &mut tab, vec![]);
    let (normal, weak) = {
        let visuals = &ctx.style().visuals;
        (visuals.text_color(), visuals.weak_text_color())
    };

    // The cell paints as three spans, so the mark locates the other two.
    let spans = crate::test_ui::painted_spans(&out);
    let mark = spans
        .iter()
        .position(|(text, _)| text == "…")
        .unwrap_or_else(|| panic!("no elision mark among {spans:?}"));
    let (head, head_color) = &spans[mark - 1];
    let (tail, tail_color) = &spans[mark + 1];

    assert_eq!(spans[mark].1, weak, "the elision mark is not weak");
    assert_eq!(*head_color, normal, "the path head is not full strength");
    assert_eq!(*tail_color, normal, "the path tail is not full strength");
    assert!(path.starts_with(head), "{head:?} does not open the path");
    assert!(tail.ends_with("alpha_widget_0.txt"), "{tail:?}");
    assert!(path.ends_with(tail.as_str()), "{tail:?} does not end it");
}

#[test]
fn a_path_that_fits_is_painted_whole_at_full_strength() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = tab.results[0].path.clone();

    let out = run_frame(&ctx, &mut tab, vec![]);
    let normal = ctx.style().visuals.text_color();
    let spans = crate::test_ui::painted_spans(&out);
    assert!(
        spans.contains(&(path.clone(), normal)),
        "{path} was not painted whole in the normal text color: {spans:?}"
    );
}

#[test]
fn the_repeat_button_tracks_the_search_it_would_repeat() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let has_button = |out: &egui::FullOutput| painted_text(out).contains(&"↻".to_string());

    assert!(
        !has_button(&run_frame(&ctx, &mut tab, vec![])),
        "before any search"
    );

    tab.on_search_started(1);
    assert!(
        !has_button(&run_frame(&ctx, &mut tab, vec![])),
        "while running"
    );

    tab.apply_update(
        SearchUpdate::Completed {
            generation: 1,
            total: 1,
            limited: false,
        },
        1000,
    );
    assert!(
        has_button(&run_frame(&ctx, &mut tab, vec![])),
        "after completion"
    );

    tab.pending_edit = Some(Instant::now());
    assert!(
        !has_button(&run_frame(&ctx, &mut tab, vec![])),
        "after an edit"
    );
}

fn completed_tab(ctx: &egui::Context) -> SearchTab {
    let mut tab = tab_with_results(1);
    tab.on_search_started(1);
    tab.apply_update(
        SearchUpdate::Completed {
            generation: 1,
            total: 1,
            limited: false,
        },
        1000,
    );
    run_frame(ctx, &mut tab, vec![]);
    tab
}

#[test]
fn clicking_the_repeat_button_asks_for_a_rerun() {
    let ctx = crate::test_ui::ctx();
    let mut tab = completed_tab(&ctx);
    let out = run_frame(&ctx, &mut tab, vec![]);
    let pos = painted_text_center(&out, "↻").expect("no repeat button painted");
    let (_, actions) = run_frame_actions(&ctx, &mut tab, click_at(pos));
    assert!(actions.rerun, "the repeat button reported nothing");
}

/// egui derives widget ids from how many widgets precede them; a renamed
/// `TextEdit` silently loses focus. Typing is the assertion that matters —
/// an id comparison would pass even if focus was dropped and handed back.
#[test]
fn the_repeat_button_does_not_steal_the_query_box() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.query = "alpha".into();
    tab.focus_query = true;
    run_frame(&ctx, &mut tab, vec![]);

    // Finish a search, so the button appears between two frames of typing.
    tab.on_search_started(1);
    tab.apply_update(
        SearchUpdate::Completed {
            generation: 1,
            total: 1,
            limited: false,
        },
        1000,
    );
    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(painted_text(&out).contains(&"↻".to_string()));

    run_frame(&ctx, &mut tab, vec![egui::Event::Text("x".into())]);
    assert!(
        tab.query.ends_with('x'),
        "typing did not reach the query box: {:?}",
        tab.query
    );
}

/// The unconditional gutter keeps the query text from jumping sideways.
#[test]
fn the_query_text_does_not_shift_when_the_repeat_button_appears() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.query = "alpha".into();

    let rect_of = |out: &egui::FullOutput| {
        painted(out)
            .into_iter()
            .find(|(text, _)| text == "alpha")
            .map(|(_, rect)| rect)
            .expect("the query text was not painted")
    };
    let without = rect_of(&run_frame(&ctx, &mut tab, vec![]));

    tab.on_search_started(1);
    tab.apply_update(
        SearchUpdate::Completed {
            generation: 1,
            total: 1,
            limited: false,
        },
        1000,
    );
    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(painted_text(&out).contains(&"↻".to_string()));
    assert_eq!(without, rect_of(&out), "the query text moved");
}

/// Left to right: syntax help, the box, how long the search took, then Fuzzy.
#[test]
fn the_query_strip_reads_help_box_duration_fuzzy() {
    let ctx = crate::test_ui::ctx();
    let mut tab = completed_tab(&ctx);
    let out = run_frame(&ctx, &mut tab, vec![]);

    let x = |needle: &str| {
        painted_text_center(&out, needle)
            .unwrap_or_else(|| panic!("{needle} was not painted: {:?}", painted_text(&out)))
            .x
    };
    let help = x("?");
    let fuzzy = x("Fuzzy");
    let elapsed = painted(&out)
        .into_iter()
        .find(|(t, _)| t.ends_with("ms") || t.ends_with('s'))
        .map(|(_, r)| r.center().x)
        .expect("no elapsed label");

    assert!(help < elapsed, "the ? is not left of the duration");
    assert!(elapsed < fuzzy, "the duration is not left of Fuzzy");
}

/// Splitting the widget must not silently lose a click target the combined
/// `ui.checkbox` had.
#[test]
fn the_fuzzy_label_is_left_of_its_box_and_still_toggles() {
    let ctx = crate::test_ui::ctx();
    let mut tab = completed_tab(&ctx);
    let out = run_frame(&ctx, &mut tab, vec![]);
    let label = painted(&out)
        .into_iter()
        .find(|(t, _)| t == "Fuzzy")
        .map(|(_, r)| r)
        .expect("no Fuzzy label");

    // Sweep for the box rather than assume how wide egui draws it.
    let before = tab.fuzzy;
    let mut hit = None;
    for dx in 1..40 {
        let (_, actions) = run_frame_actions(
            &ctx,
            &mut tab,
            click_at(egui::pos2(label.right() + dx as f32, label.center().y)),
        );
        if tab.fuzzy != before {
            hit = Some(actions);
            break;
        }
    }
    let actions = hit.expect("no checkbox to the right of the Fuzzy label");
    assert_eq!(actions.save_fuzzy_default, Some(tab.fuzzy));
    assert!(actions.rerun);

    // And the label itself is still a target.
    let (_, actions) = run_frame_actions(&ctx, &mut tab, click_at(label.center()));
    assert_eq!(tab.fuzzy, before, "the label lost its click target");
    assert_eq!(actions.save_fuzzy_default, Some(tab.fuzzy));
}

#[test]
fn a_capped_count_says_so_without_the_word_truncated() {
    let mut tab = tab_with_results(3);
    tab.query.clear();
    tab.results.clear();
    assert_eq!(tab.result_count_label(), None, "nothing searched yet");

    let mut tab = tab_with_results(3);
    assert_eq!(tab.result_count_label().as_deref(), Some("3 results"));

    tab.limited = true;
    let label = tab.result_count_label().expect("a label");
    assert_eq!(label, "3+ results");
    assert!(!label.contains("truncated"), "{label}");
}

/// The in-tab notice is the one with room to explain the cap.
#[test]
fn the_in_tab_notice_still_explains_the_cap() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(3);
    tab.limited = true;
    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        painted.iter().any(|t| t.starts_with("Showing first")),
        "{painted:?}"
    );
}

// --- Live results ---------------------------------------------------------

/// Only the rows actually rendered are watched — the request is explicit that
/// it is the visible set, not everything the search returned.
#[test]
fn only_the_rendered_rows_are_offered_for_watching() {
    let ctx = crate::test_ui::ctx();
    // A settled tab: no search running, nothing pending, no reveal underway.
    let mut tab = tab_with_results(500);
    run_frame(&ctx, &mut tab, vec![]);
    std::thread::sleep(LIVE_ARM_DELAY);
    let (_, actions) = run_frame_actions(&ctx, &mut tab, vec![]);

    let targets = actions
        .live_targets
        .expect("nothing was offered for watching");
    assert!(!targets.is_empty());
    assert!(
        targets.len() < 100,
        "{} of 500 rows were watched — that is not the visible set",
        targets.len()
    );
}

/// Editing the query drops the watches straight away, without waiting for the
/// debounce to fire the next search.
#[test]
fn editing_the_query_drops_the_watches() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.live_armed = vec![live_target("/qs-test/alpha_widget_0.txt")];
    tab.focus_query = true;
    run_frame(&ctx, &mut tab, vec![]);

    let (_, actions) = run_frame_actions(&ctx, &mut tab, vec![egui::Event::Text("z".into())]);
    assert_eq!(actions.live_targets, Some(Vec::new()));
    assert!(tab.live_armed.is_empty());
}

/// A target for `path` as the tab would build one, with a baseline nothing in
/// these tests reads.
fn live_target(path: &str) -> Target {
    Target {
        path: path.to_string(),
        text: None,
        size: 116,
        mtime: 1_700_000_000,
    }
}

#[test]
fn should_arm_waits_for_the_results_to_settle_and_hold_still() {
    let now = Instant::now();
    let long_ago = now - LIVE_ARM_DELAY * 2;

    assert!(should_arm(true, false, Some(long_ago), true, now));
    assert!(
        !should_arm(false, false, Some(long_ago), true, now),
        "the feature is switched off"
    );
    assert!(
        !should_arm(true, false, Some(long_ago), false, now),
        "the results do not match the query box yet"
    );
    assert!(
        !should_arm(true, false, Some(now), true, now),
        "the rows are still moving"
    );
    assert!(
        !should_arm(true, false, None, true, now),
        "nothing has changed to arm for"
    );
    assert!(
        !should_arm(true, true, Some(long_ago), true, now),
        "already watching exactly these rows"
    );
}

/// What counts as "already watching these rows": the paths and whether each
/// needs its body re-read, and nothing else.
#[test]
fn the_watch_set_is_the_paths_and_the_tier_and_not_the_baseline() {
    let wanted = vec![live_target("/a.txt"), live_target("/b.txt")];
    assert!(same_watch_set(&wanted, &wanted));

    // The baseline rides along on `Target` but says nothing about *what* is
    // watched: a file whose size moved must not tear down and rebuild every
    // registration on screen.
    let mut moved = wanted.clone();
    moved[0].size += 1;
    moved[0].mtime += 1;
    assert!(
        same_watch_set(&moved, &wanted),
        "a changed baseline read as a different watch set"
    );

    let mut renamed = wanted.clone();
    renamed[0].path = "/c.txt".into();
    assert!(
        !same_watch_set(&renamed, &wanted),
        "a renamed row read as the same watch set"
    );

    let mut retiered = wanted.clone();
    retiered[0].text = Some(quicksearch_core::search::ContentTier::Exact);
    assert!(
        !same_watch_set(&retiered, &wanted),
        "a row that started showing body text read as the same watch set"
    );
}

/// The bug a rename used to hit: the row's path changes but its position does
/// not, so anything keyed on row indices would decide nothing had moved and
/// leave the watcher pointed at a file that is no longer there.
#[test]
fn a_renamed_row_is_re_armed_at_its_new_path() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(2);
    run_frame(&ctx, &mut tab, vec![]);
    std::thread::sleep(LIVE_ARM_DELAY);
    let (_, actions) = run_frame_actions(&ctx, &mut tab, vec![]);
    assert!(actions.live_targets.is_some(), "never armed to begin with");

    tab.apply_live(LiveUpdate::Renamed {
        path: "/qs-test/alpha_widget_0.txt".into(),
        to: "/elsewhere/renamed.txt".into(),
        name: "renamed.txt".into(),
    });
    // The rename restarts the arm delay, exactly as a scroll would.
    run_frame(&ctx, &mut tab, vec![]);
    std::thread::sleep(LIVE_ARM_DELAY);
    let (_, actions) = run_frame_actions(&ctx, &mut tab, vec![]);

    let targets = actions.live_targets.expect("the rename did not re-arm");
    assert!(
        targets.iter().any(|t| t.path == "/elsewhere/renamed.txt"),
        "the watcher is still keyed on the old path: {targets:?}"
    );
}

/// The baseline the watcher sweeps against is what the row is *displaying* —
/// which on a fresh result is what the index said. That is what turns arming
/// into a check of the index against the disk.
#[test]
fn a_target_carries_what_the_row_is_displaying() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(2);
    tab.results[0].size = 4242;
    tab.results[0].mtime = 1_710_000_000;
    run_frame(&ctx, &mut tab, vec![]);
    std::thread::sleep(LIVE_ARM_DELAY);
    let (_, actions) = run_frame_actions(&ctx, &mut tab, vec![]);

    let targets = actions.live_targets.expect("nothing was armed");
    let first = targets
        .iter()
        .find(|t| t.path == "/qs-test/alpha_widget_0.txt")
        .expect("the row was not offered");
    assert_eq!((first.size, first.mtime), (4242, 1_710_000_000));
}

/// A rename lands from the filesystem event itself. The row keeps its place,
/// its identity and its rank — only what it says about the file changes.
#[test]
fn a_rename_updates_the_row_in_place() {
    let mut tab = tab_with_results(2);
    tab.results[0] = name_hit("before.txt", (0, 6));
    tab.selected = Some(0);
    let (rank, stage, file_id) = (
        tab.results[0].rank,
        tab.results[0].stage,
        tab.results[0].file_id,
    );

    tab.apply_live(LiveUpdate::Renamed {
        path: "/qs-test/before.txt".into(),
        to: "/qs-test/after.txt".into(),
        name: "after.txt".into(),
    });

    let hit = &tab.results[0];
    assert_eq!(hit.name, "after.txt");
    assert_eq!(hit.path, "/qs-test/after.txt");
    assert_eq!((hit.rank, hit.stage, hit.file_id), (rank, stage, file_id));
    assert_eq!(tab.selected, Some(0), "the selection moved");
    // The old name's marks cannot describe the new one.
    let snip = hit.snippet.as_ref().expect("a name hit carries one");
    assert_eq!(snip.window, "after.txt");
    assert!(snip.ranges.is_empty());
}

/// A file that disappears leaves its row where it is, struck through: dropping
/// it would shift everything below while someone is reading.
#[test]
fn a_vanished_file_is_struck_through_and_comes_back() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    let path = tab.results[0].path.clone();

    // The *name* run, exactly — the path column paints the filename as well,
    // as part of a longer run and never weak.
    let name_is_weak = |out: &egui::FullOutput| {
        let weak = ctx.style().visuals.weak_text_color();
        crate::test_ui::painted_spans(out)
            .iter()
            .any(|(text, color)| text == "alpha_widget_0.txt" && *color == weak)
    };

    tab.apply_live(LiveUpdate::Gone { path: path.clone() });
    let out = run_frame(&ctx, &mut tab, vec![]);
    assert_eq!(tab.results.len(), 1, "the row was removed");
    assert!(name_is_weak(&out), "the vanished row was not de-emphasised");

    tab.apply_live(LiveUpdate::Changed {
        path,
        size: 200,
        mtime: 1_800_000_000,
        window: WindowUpdate::Unchanged,
    });
    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        !name_is_weak(&out),
        "a recreated file stayed marked as gone"
    );
    assert_eq!(tab.results[0].size, 200);
}

/// A refreshed content snippet is painted with its match marked, and turns the
/// Content Match column on if it was not already.
#[test]
fn a_content_change_repaints_the_highlight() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.results[0].stage = 6;
    let path = tab.results[0].path.clone();

    tab.apply_live(LiveUpdate::Changed {
        path,
        size: 300,
        mtime: 1_800_000_000,
        window: WindowUpdate::Cut(Snippet {
            window: "the quarterly budget was revised".into(),
            ranges: vec![(14, 20)],
            truncated_start: false,
            truncated_end: false,
        }),
    });

    let out = run_frame(&ctx, &mut tab, vec![]);
    assert!(
        highlight_runs(&out, &ctx).contains(&"budget".to_string()),
        "the refreshed match was not highlighted: {:?}",
        painted_text(&out)
    );
}

/// The watcher re-cuts a content row's window from the file, so `None` is not
/// "nothing to say" — it is "the body stopped matching", and the cell has to
/// fall back to its dash rather than keep showing text that is no longer a hit.
#[test]
fn an_edit_that_removes_the_match_clears_the_content_cell() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.results[0].stage = 6;
    tab.results[0].snippet = Some(Snippet {
        window: "the quarterly budget was revised".into(),
        ranges: vec![(14, 20)],
        truncated_start: false,
        truncated_end: false,
    });
    let path = tab.results[0].path.clone();

    tab.apply_live(LiveUpdate::Changed {
        path,
        size: 300,
        mtime: 1_800_000_000,
        window: WindowUpdate::NoMatch,
    });

    assert!(tab.results[0].snippet.is_none());
    let painted = painted_text(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        !painted.iter().any(|t| t.contains("quarterly")),
        "the stale window is still on screen: {painted:?}"
    );
}

/// A name-tier row's snippet *is* its filename, not its body, so a write must
/// not take it away — only the content tiers hand over an authoritative window.
#[test]
fn a_content_change_leaves_a_name_hit_s_snippet_alone() {
    let mut tab = tab_with_results(1);
    tab.results[0] = name_hit("before.txt", (0, 6));
    let path = tab.results[0].path.clone();

    tab.apply_live(LiveUpdate::Changed {
        path,
        size: 300,
        mtime: 1_800_000_000,
        window: WindowUpdate::Unchanged,
    });

    let snip = tab.results[0].snippet.as_ref().expect("the name hit's own");
    assert_eq!(snip.window, "before.txt");
    assert_eq!(tab.results[0].size, 300, "the metadata still landed");
}

/// The Name column is optional, so it cannot be the only place a vanished file
/// says so. With it hidden, the path — which is always shown — carries it.
#[test]
fn a_vanished_file_is_legible_with_the_name_column_hidden() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_results(1);
    tab.columns.name = false;
    let path = tab.results[0].path.clone();
    let weak = ctx.style().visuals.weak_text_color();

    let before = crate::test_ui::painted_spans(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        !before
            .iter()
            .any(|(t, c)| t.contains("alpha_widget_0") && *c == weak),
        "the path was already weak before anything vanished: {before:?}"
    );

    tab.apply_live(LiveUpdate::Gone { path });
    let after = crate::test_ui::painted_spans(&run_frame(&ctx, &mut tab, vec![]));
    assert!(
        after
            .iter()
            .any(|(t, c)| t.contains("alpha_widget_0") && *c == weak),
        "nothing on the row says the file is gone: {after:?}"
    );
}

/// One frame with the tour on screen, and where each of `wanted` ended up.
/// Read inside the pass, which is the only place a mark is live — and the
/// only place the tour itself ever reads one.
fn frame_spotlit(
    ctx: &egui::Context,
    tab: &mut SearchTab,
    wanted: &[crate::spotlight::Spot],
) -> (egui::FullOutput, Vec<Option<egui::Rect>>) {
    let input = crate::test_ui::raw_input(egui::vec2(1000.0, 700.0), Vec::new());
    let mut found = Vec::new();
    let out = ctx.run(input, |ctx| {
        crate::spotlight::set_active(ctx, true);
        egui::CentralPanel::default().show(ctx, |ui| {
            tab.ui(ui);
        });
        found = wanted
            .iter()
            .map(|spot| crate::spotlight::rect(ctx, *spot))
            .collect();
    });
    (out, found)
}

/// Where a string was painted; the tour's glow has to land on the widget the
/// user is being told to look at, not near it.
fn painted_rect(out: &egui::FullOutput, needle: &str) -> egui::Rect {
    crate::test_ui::painted(out)
        .into_iter()
        .find(|(text, _)| text == needle)
        .map(|(_, rect)| rect)
        .unwrap_or_else(|| panic!("nothing painted for {needle:?}"))
}

#[test]
fn the_tour_can_find_the_query_strip_and_the_rank_column() {
    use crate::spotlight::Spot;
    const WANTED: [Spot; 4] = [
        Spot::QueryHelp,
        Spot::SearchBar,
        Spot::FuzzyToggle,
        Spot::RankColumn,
    ];
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_content_snippets(3);
    let (out, found) = frame_spotlit(&ctx, &mut tab, &WANTED);
    let spot = |want: Spot| {
        let i = WANTED.iter().position(|s| *s == want).expect("asked for");
        found[i].unwrap_or_else(|| panic!("{want:?} was never marked"))
    };

    // Reported *after* the pass, so the marks have to have survived it.
    assert!(
        spot(Spot::QueryHelp).contains_rect(painted_rect(&out, "?")),
        "the ? button's mark misses the glyph"
    );
    assert!(
        spot(Spot::SearchBar).contains_rect(painted_rect(&out, &tab.query)),
        "the query box's mark misses the text in it"
    );

    // The label and the box: the page tells the reader to tick one and reads
    // the other, and a ring around half of that is a ring around the wrong thing.
    let fuzzy_word = painted_rect(&out, "Fuzzy");
    let fuzzy = spot(Spot::FuzzyToggle);
    assert!(fuzzy.contains_rect(fuzzy_word), "the mark misses the label");
    assert!(
        fuzzy.width() > fuzzy_word.width() + 8.0,
        "the mark is the label alone, without the checkbox beside it"
    );

    // Header plus cells: the column, not its heading.
    let rank = spot(Spot::RankColumn);
    assert!(rank.contains_rect(painted_rect(&out, "Rank")));
    assert!(
        rank.contains_rect(painted_rect(&out, " 6.00 ")),
        "the mark stops at the header instead of covering the rows"
    );
}

/// Nothing is published while the tour is closed, which is nearly always.
#[test]
fn the_query_strip_publishes_nothing_with_no_tour_open() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab_with_content_snippets(3);
    let input = crate::test_ui::raw_input(egui::vec2(1000.0, 700.0), Vec::new());
    let mut marked = None;
    let _ = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            tab.ui(ui);
        });
        marked = crate::spotlight::rect(ctx, crate::spotlight::Spot::SearchBar);
    });
    assert_eq!(marked, None);
}
