use super::*;

fn tab_with_results(n: usize) -> SearchTab {
    let mut tab = SearchTab::new(false);
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

fn run_frame(
    ctx: &egui::Context,
    tab: &mut SearchTab,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = crate::test_ui::raw_input(egui::vec2(1000.0, 700.0), events);
    ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            tab.ui(ui);
        });
    })
}

/// Walk the pointer down the name column until it sits on `row`'s label
/// glyphs: the text cursor proves a selectable label won the hit-test,
/// `hovered_row` proves the row still tracks hover.
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
        "deeply/nested/under/several/more/directories/alpha_widget_0.txt"
    )
    .to_string()
}

/// The Path column drops out of the *middle*, so the volume the file
/// sits on and the directories right above it both stay on screen.
#[test]
fn long_paths_elide_from_the_middle_of_the_path_column() {
    let ctx = egui::Context::default();
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

/// The whole path stays reachable on hover: egui's free tooltip only
/// appears while *it* did the eliding, so pre-shortened text brings its own.
#[test]
fn an_elided_path_still_shows_the_whole_thing_on_hover() {
    let ctx = egui::Context::default();
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
    let mut tab = SearchTab::new(false);
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

/// The cascade streams in scan order; the table must stay ordered by its
/// keyed column as batches land, not merely append them.
#[test]
fn later_batches_land_in_the_tables_sort_order() {
    let mut tab = streaming_tab();
    batch(&mut tab, vec![hit(1, "middling.txt", 4.0, 50)]);
    assert_eq!(displayed(&tab), vec!["middling.txt"]);

    // A rank-1 hit found later in the scan belongs on top.
    batch(&mut tab, vec![hit(2, "best.txt", 1.0, 10)]);
    assert_eq!(displayed(&tab), vec!["best.txt", "middling.txt"]);

    // And a rank-10 one belongs at the bottom, not wherever it arrived.
    batch(&mut tab, vec![hit(3, "worst.txt", 10.0, 90)]);
    assert_eq!(
        displayed(&tab),
        vec!["best.txt", "middling.txt", "worst.txt"]
    );
}

/// Rank is only the default. Under any other key, arrivals slot into that
/// key's order.
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

    tab.sort = (SortKey::Size, false);
    tab.sort_dirty = true;
    tab.resort();
    assert_eq!(
        displayed(&tab),
        vec!["zucchini.txt", "mango.txt", "apple.txt"]
    );
}

/// Re-keying the sort mid-stream re-orders rows already shown, and later
/// batches land under the new key.
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

/// At the display cap, retention stays keyed on rank even when the table
/// is shown in another order.
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

/// Batches arriving while the old table fades out are ordered at the swap.
#[test]
fn staged_batches_are_ordered_once_the_fade_swaps() {
    let mut tab = SearchTab::new(false);
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

/// Step the transition at a steady 60 fps until `done`, and report how
/// long it took.
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
    let mut tab = SearchTab::new(false);
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
    let mut tab = SearchTab::new(false);
    tab.swap_pending = true;
    tab.advance_fade(10.0);
    assert_eq!(tab.fade, 0.0, "a whole ten seconds lands, not passes");

    tab.swap_pending = false;
    tab.wipe = 1.0;
    tab.advance_fade(10.0);
    assert_eq!(tab.wipe, 0.0);
    assert_eq!(tab.fade, 1.0);
}

/// Clearing the old results holds the reveal where it stands — run
/// backwards it would flash just-covered rows back on screen.
#[test]
fn clearing_results_holds_the_reveal_where_it_stands() {
    let mut tab = SearchTab::new(false);
    tab.wipe = 1.0;
    tab.advance_fade(FADE_IN_SECS / 5.0);
    let standing = tab.wipe;
    assert!((standing - 0.8).abs() < 1e-4, "one fifth in: {standing}");
    let carried = tab.fade;

    tab.swap_pending = true;
    tab.advance_fade(0.0);
    assert_eq!(tab.wipe, standing, "the reveal is frozen, not rewound");
    assert_eq!(tab.fade, carried, "and the opacity carries on from here");

    // The scrim holds its position for the whole of the fade-out, and
    // the opacity takes the full FADE_OUT_SECS to get from here to zero.
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
    let mut tab = SearchTab::new(false);
    // Nothing pending, nothing covered, nothing dimmed: the steady state
    // must not repaint forever.
    assert!(tab.fade_settled());
    tab.advance_fade(1.0 / 60.0);
    assert!(tab.fade_settled());

    // Whereas each stage of a transition keeps the frames coming, the
    // swap frame — cleared out but not yet revealing — included.
    tab.swap_pending = true;
    assert!(!tab.fade_settled());
    tab.advance_fade(FADE_OUT_SECS);
    tab.swap_pending = false;
    tab.wipe = 1.0;
    assert!(!tab.fade_settled());
}

/// Drive frames until the reveal has uncovered all but `to` of the
/// section, and hand back the frame it got there on. `run_frame` leaves
/// `RawInput::time` unset, so egui advances its own clock a predicted
/// frame at a time and the tab sees a steady `stable_dt`.
fn reveal_to(ctx: &egui::Context, tab: &mut SearchTab, to: f32) -> egui::FullOutput {
    for _ in 0..200 {
        let out = run_frame(ctx, tab, vec![]);
        if tab.wipe <= to {
            return out;
        }
    }
    panic!("the reveal never got down to {to}");
}

/// Drive frames until the staged results swap in, and hand back the
/// frame it happened on.
fn swap_in(ctx: &egui::Context, tab: &mut SearchTab) -> egui::FullOutput {
    for _ in 0..200 {
        let out = run_frame(ctx, tab, vec![]);
        if !tab.swap_pending {
            return out;
        }
    }
    panic!("the staged results never swapped in");
}

/// Twenty staged hits under whatever generation is in flight.
fn stage_results(tab: &mut SearchTab) {
    batch(
        tab,
        (0..20)
            .map(|i| hit(i, &format!("alpha_widget_{i}.txt"), 3.0, 116))
            .collect(),
    );
}

/// Where the first and last result rows were painted.
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

/// The y where the scrim first turns fully solid — the edge of what is
/// still hidden.
fn solid_from(ramp: &[(f32, u8)]) -> f32 {
    ramp.iter()
        .find(|&&(_, a)| a == 255)
        .expect("a solid stretch")
        .0
}

/// Clearing the old results paints no scrim: it is a plain dip to nothing.
#[test]
fn clearing_results_paints_no_scrim() {
    let ctx = egui::Context::default();
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
            // The swap frame belongs to the new set, which starts
            // covered — checked separately below.
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

/// The new set arrives fully covered, headers included. The scrim carries
/// its own alpha — painted through the `Ui` it would be scaled by the
/// section opacity, which is exactly zero on this frame.
#[test]
fn new_results_start_completely_covered() {
    let ctx = egui::Context::default();
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

    // Which is also why the rows have to be measured from a frame the
    // reveal has let some light through.
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

/// The reveal uncovers the table from the top down, first rows early.
#[test]
fn new_results_are_uncovered_from_the_top_down() {
    let ctx = egui::Context::default();
    let mut tab = tab_with_results(20);
    run_frame(&ctx, &mut tab, vec![]);

    tab.on_search_started(1);
    stage_results(&mut tab);
    swap_in(&ctx, &mut tab);

    // A fifth of the way in: the head of the table is out from behind
    // the scrim while the foot is still under it.
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

    // It ends with the scrim gone entirely rather than lingering.
    let done = reveal_to(&ctx, &mut tab, 0.0);
    assert!(
        crate::test_ui::painted_meshes(&done).is_empty(),
        "the scrim clears away at the end of the reveal"
    );
    assert!(tab.fade_settled(), "and the section stops animating");
}

/// A selected row is identified by file id, so it survives both the
/// re-ordering and the eviction that a new batch can cause.
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
    let ctx = egui::Context::default();
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

/// Patterns are spelled natively and a drive root does not become the
/// never-matching `C:\/*`.
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

/// The freshness fade ends where the theme's own weak text does.
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

/// The rows `job` actually lays out. `Galley::text` hands back the whole
/// job, including every row epaint dropped at `wrap.max_rows`, so it
/// cannot see a truncation at all.
fn laid_out_rows(ui: &egui::Ui, job: LayoutJob) -> Vec<String> {
    ui.fonts(|f| f.layout_job(job))
        .rows
        .iter()
        .map(|r| r.text())
        .collect()
}

/// A content snippet whose lead-in is `lines` short lines. The lead-in is
/// multi-byte: snippet ranges are byte offsets while row arithmetic counts
/// characters.
fn ragged_snippet(lines: usize) -> Snippet {
    let lead = "café\n".repeat(lines);
    Snippet {
        ranges: vec![(lead.len(), lead.len() + 6)],
        window: format!("{lead}NEEDLE and trailing context"),
        truncated_start: true,
        truncated_end: true,
    }
}

/// A window whose lead-in is dozens of short lines must still get the hit
/// on screen within the row budget.
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
        // Trimmed at a line boundary and said so. A start landing mid
        // character would read "…afé" — or panic on a byte offset that
        // is not a char boundary.
        assert_eq!(rows[0], "… café", "{rows:#?}");
    });
}

/// The two-row preview strip keeps the match too.
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

/// A window that already fits is rendered exactly as it arrived: no
/// trimming, and no ellipsis for a trim that did not happen.
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

/// The Match cell is laid out in Extend mode (infinite wrap width), so
/// only its own budget keeps it inside the column; an overshoot is
/// clipped on *both* sides with no ellipsis.
#[test]
fn the_match_cell_stays_inside_its_column() {
    with_ui(|ui| {
        let snip = Snippet {
            window: "a long stretch of leading context NEEDLE and a long tail after it".into(),
            ranges: vec![(34, 40)],
            truncated_start: true,
            truncated_end: true,
        };
        // Down to widths the column itself cannot reach, so the budget
        // degrades rather than overflowing.
        for width in [20.0, 60.0, 90.0, 120.0, 150.0, 240.0, 400.0, 4000.0] {
            for whole_field in [false, true] {
                let job = centered_match_job(ui, &snip, width, whole_field);
                let galley = ui.fonts(|f| f.layout_job(job));
                assert!(
                    galley.size().x <= width,
                    "{}pt of text in a {width}pt column (whole_field={whole_field}): {:?}",
                    galley.size().x,
                    galley.text()
                );
                // At the column's 120pt floor the whole hit must survive;
                // below that, its head still gets the room over context.
                let kept = if width >= 120.0 { "NEEDLE" } else { "N" };
                assert!(
                    galley.text().contains(kept),
                    "the match was budgeted away at {width}pt: {:?}",
                    galley.text()
                );
            }
        }
    });
}

/// Hovering the Match cell puts the hit on screen. The cell itself paints
/// the match once, so the tooltip is the *second* appearance.
#[test]
fn hovering_the_match_cell_shows_the_match_in_the_tooltip() {
    let ctx = egui::Context::default();
    // Testing that the tooltip carries the match, not egui's hover timing.
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
    let mut tab = tab_with_results(1);
    tab.has_snippets = true;
    tab.results[0].stage = 6; // a full-text stage: no [brackets]
    tab.results[0].snippet = Some(ragged_snippet(40));

    run_frame(&ctx, &mut tab, vec![]); // settle the table's layout
    for y in 40..250 {
        // x lands in the Match column, past Name and Path.
        let pos = egui::pos2(600.0, y as f32);
        let mut out = run_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(pos)]);
        if tab.hovered_row != Some(0) {
            continue;
        }
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
