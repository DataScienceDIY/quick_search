use super::*;

use crate::test_ui::{click_at, painted, painted_rects, painted_spans, painted_text, raw_input_at};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);
/// Deliberately not the real home directory: the folders page's fallback
/// wording is the one every other test should see, whatever machine it runs on.
const ROOT: &str = "/srv/projects";

fn roots() -> Vec<String> {
    vec![ROOT.to_string()]
}

/// A tour parked on `page` with that page's arrival already dealt with —
/// what every test about the *rendering* of a page wants.
fn at(page: usize) -> Tutorial {
    Tutorial {
        page,
        shown: Some(page),
        typing: None,
        moved: false,
    }
}

/// A tour about to show `page` for the first time, so entering it still has
/// its tab to ask for and its typing to start.
fn entering(page: usize) -> Tutorial {
    Tutorial {
        page,
        shown: None,
        typing: None,
        moved: false,
    }
}

/// One pass, and what the tour asked for during it.
fn pass(
    ctx: &egui::Context,
    tour: &mut Tutorial,
    events: Vec<egui::Event>,
    time: f64,
) -> (egui::FullOutput, TourActions) {
    let roots = roots();
    let mut actions = TourActions::default();
    let out = ctx.run(raw_input_at(SCREEN, events, time), |ctx| {
        actions = tour.ui(ctx, &roots);
    });
    (out, actions)
}

fn merge(a: TourActions, b: TourActions) -> TourActions {
    TourActions {
        dismissed: a.dismissed || b.dismissed,
        goto_tab: b.goto_tab.or(a.goto_tab),
        set_query: b.set_query.or(a.set_query),
        focus_search: a.focus_search || b.focus_search,
    }
}

/// Two passes: an `egui::Window` is measured on its first frame and only
/// placed on the next. The app acts on what every pass returns, so the
/// caller sees both merged.
fn frame(
    ctx: &egui::Context,
    tour: &mut Tutorial,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, TourActions) {
    let (_, first) = pass(ctx, tour, Vec::new(), 0.0);
    let (out, second) = pass(ctx, tour, events, 0.0);
    (out, merge(first, second))
}

#[test]
fn every_page_paints_its_own_title_and_body() {
    for (page, spec) in PAGES.iter().enumerate() {
        let ctx = crate::test_ui::ctx();
        let mut tour = at(page);
        let (out, _) = frame(&ctx, &mut tour, Vec::new());
        let painted = painted_text(&out);
        assert!(
            painted.iter().any(|t| t == spec.title),
            "page {page} painted no title: {painted:?}"
        );
        assert!(
            !paragraphs(spec, &roots()).is_empty(),
            "page {page} has nothing to say"
        );
        assert!(
            painted
                .iter()
                .any(|t| t == &format!("{} of {}", page + 1, PAGES.len())),
            "page {page} did not say where it is: {painted:?}"
        );
        // The brackets are markup. Painting one means a keyword was not
        // recognised, and it is the reader who finds out.
        for text in &painted {
            assert!(
                !text.contains('[') && !text.contains(']'),
                "page {page} painted its markup: {text:?}"
            );
        }
        crate::test_ui::assert_no_tofu(&ctx, &out);
    }
}

/// The keyword-to-spot pairing is positional, so a page that adds a keyword
/// without a spot (or the reverse) silently starts colouring the wrong things.
#[test]
fn every_keyword_has_a_spot_to_point_at() {
    for (page, spec) in PAGES.iter().enumerate() {
        let keywords: Vec<&str> = paragraphs(spec, &roots())
            .iter()
            .flat_map(|p| {
                runs(p)
                    .into_iter()
                    .filter(|(_, keyword)| *keyword)
                    .map(|(run, _)| run.to_string())
                    .collect::<Vec<_>>()
            })
            .map(|s| Box::leak(s.into_boxed_str()) as &str)
            .collect();
        assert_eq!(
            keywords.len(),
            spec.spots.len(),
            "page {page} ({}) highlights {keywords:?} but points at {:?}",
            spec.title,
            spec.spots
        );
    }
}

#[test]
fn brackets_mark_keywords_and_survive_being_unbalanced() {
    assert_eq!(runs("plain text"), vec![("plain text", false)]);
    assert_eq!(
        runs("add them [here]."),
        vec![("add them ", false), ("here", true), (".", false)]
    );
    assert_eq!(
        runs("the [a] and the [b]"),
        vec![
            ("the ", false),
            ("a", true),
            (" and the ", false),
            ("b", true),
        ]
    );
    // Nothing to close it: prose, not markup, and certainly not a panic.
    assert_eq!(runs("a [ b"), vec![("a [ b", false)]);
    assert_eq!(runs("a ] b"), vec![("a ] b", false)]);
    assert_eq!(runs(""), Vec::new());
}

/// The point of the whole exercise: the word and the widget it names are the
/// same colour, and different words are different colours.
#[test]
fn each_keyword_is_painted_in_the_colour_of_the_spot_it_names() {
    let input = PAGES
        .iter()
        .position(|p| p.title == "Search: Input")
        .expect("the input page");
    let ctx = crate::test_ui::ctx();
    let mut tour = at(input);
    let (out, _) = frame(&ctx, &mut tour, Vec::new());
    let dark_mode = ctx.style().visuals.dark_mode;
    let spans = painted_spans(&out);

    for (n, keyword) in ["search bar", "?"].into_iter().enumerate() {
        let want = accent(n, dark_mode);
        assert!(
            spans.iter().any(|(t, c)| t == keyword && *c == want),
            "{keyword:?} is not painted in {want:?}: {spans:?}"
        );
    }
    assert_ne!(
        accent(0, dark_mode),
        accent(1, dark_mode),
        "two keywords on one page must not share a colour"
    );
}

/// Both halves of the effect move together, and they move at all.
#[test]
fn the_keyword_and_its_ring_pulse_in_step() {
    let status = PAGES
        .iter()
        .position(|p| p.title == "The status bar")
        .expect("the status bar page");
    let target = egui::Rect::from_min_size(egui::pos2(0.0, 660.0), egui::vec2(1000.0, 24.0));

    // The tint behind a keyword and the rings around its widget, at one time.
    let sample = |time: f64| {
        let ctx = crate::test_ui::ctx();
        let mut tour = at(status);
        let roots = roots();
        let mut run = || {
            ctx.run(raw_input_at(SCREEN, Vec::new(), time), |ctx| {
                crate::spotlight::set_active(ctx, true);
                crate::spotlight::mark(ctx, Spot::StatusBar, target);
                tour.ui(ctx, &roots);
            })
        };
        run();
        let out = run();
        let tint = crate::test_ui::painted_backgrounds(&out)
            .into_iter()
            .find(|(text, _)| text == "status bar")
            .map(|(_, color)| color.a())
            .expect("the keyword is tinted");
        let rings: Vec<u8> = painted_rects(&out)
            .into_iter()
            .filter(|r| r.stroke.width > 0.0 && r.rect.contains_rect(target))
            .map(|r| r.stroke.color.a())
            .collect();
        (tint, rings)
    };

    // A quarter period apart: the dimmest sample against the brightest.
    let (dim_tint, dim_rings) = sample(PULSE_PERIOD * 0.75);
    let (bright_tint, bright_rings) = sample(PULSE_PERIOD * 0.25);

    assert!(
        !dim_rings.is_empty(),
        "the marked widget was not ringed at all"
    );
    assert_eq!(
        dim_rings.len(),
        bright_rings.len(),
        "the ring count must not depend on the phase"
    );
    assert!(
        bright_tint > dim_tint,
        "the keyword's tint does not pulse ({dim_tint} → {bright_tint})"
    );
    for (dim, bright) in dim_rings.iter().zip(&bright_rings) {
        assert!(
            bright > dim,
            "a ring does not pulse with the keyword ({dim} → {bright})"
        );
    }
}

/// A spot nothing marked this pass — a hidden column, another tab — is not
/// worth painting a ring in the middle of nowhere for.
#[test]
fn an_unmarked_spot_is_not_ringed() {
    let status = PAGES
        .iter()
        .position(|p| p.title == "The status bar")
        .expect("the status bar page");
    let ctx = crate::test_ui::ctx();
    let mut tour = at(status);
    let (out, _) = frame(&ctx, &mut tour, Vec::new());
    let dark_mode = ctx.style().visuals.dark_mode;
    let ring = accent(0, dark_mode);
    assert!(
        !painted_rects(&out)
            .into_iter()
            .any(|r| r.stroke.color.to_opaque() == ring.to_opaque() && r.stroke.width > 0.0),
        "something was ringed with nothing to ring"
    );
}

#[test]
fn the_folders_page_says_which_folders_those_are() {
    // The machine's real home, which is what a default install indexes.
    if let Some(home) = quicksearch_core::platform::home_dir() {
        let home = home.to_string_lossy().into_owned();
        let line = folders_line(std::slice::from_ref(&home));
        assert!(
            line.contains(&home),
            "{line:?} does not name the home folder"
        );
        assert!(line.contains("[here]"), "{line:?} points nowhere");
        // A trailing separator in the config is the same folder.
        assert_eq!(folders_line(&[format!("{home}/")]), line);
    }

    // Anything else: say what is actually indexed rather than claim home is.
    let line = folders_line(&[ROOT.to_string()]);
    assert!(line.contains(ROOT), "{line:?} does not name the root");
    assert!(line.contains("[here]"), "{line:?} points nowhere");

    // And with nothing indexed at all, which a reopened tour can find.
    let line = folders_line(&[]);
    assert!(line.contains("[here]"), "{line:?} points nowhere");
}

#[test]
fn the_folders_page_paints_the_configured_folder() {
    let page = PAGES
        .iter()
        .position(|p| p.lead.is_some())
        .expect("the folders page");
    let ctx = crate::test_ui::ctx();
    let mut tour = at(page);
    let (out, _) = frame(&ctx, &mut tour, Vec::new());
    let painted = painted_text(&out);
    assert!(
        painted.iter().any(|t| t.contains(ROOT)),
        "the page does not name the folder that is indexed: {painted:?}"
    );
}

/// The last page sends the reader to a button; that button has to still be
/// called that.
#[test]
fn the_last_page_names_the_help_tab_button() {
    let ctx = crate::test_ui::ctx();
    let out = ctx.run(raw_input_at(SCREEN, Vec::new(), 0.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            crate::help_tab::ui(ui);
        });
    });
    let buttons = painted_text(&out);
    let last = PAGES.last().expect("pages");
    let prose = paragraphs(last, &roots()).join(" ");
    assert!(
        buttons
            .iter()
            .any(|label| prose.contains(label.as_str())
                && label.to_lowercase().contains("introduction")),
        "the last page does not quote the Help tab's button: {prose:?} vs {buttons:?}"
    );
}

/// Entering a page moves to the tab it is about — once. Repeating it every
/// frame would drag back a user who clicked somewhere else mid-page.
#[test]
fn a_page_asks_for_its_tab_once_on_arrival() {
    let ctx = crate::test_ui::ctx();
    let duplicates = PAGES
        .iter()
        .position(|p| p.tab == Some(Tab::Duplicates))
        .expect("the duplicates page");
    let mut tour = entering(duplicates);

    let (_, actions) = pass(&ctx, &mut tour, Vec::new(), 0.0);
    assert_eq!(actions.goto_tab, Some(Tab::Duplicates));
    // The second pass is also where the window, measured on the first, is
    // finally placed and its buttons painted.
    let (out, actions) = pass(&ctx, &mut tour, Vec::new(), 0.1);
    assert_eq!(actions.goto_tab, None, "asked twice for one page");

    // Next, then Back: arriving again asks again, or the tour walks forward
    // into a page whose subject is not on screen.
    let next = crate::test_ui::painted_text_center(&out, "Next").expect("a Next button");
    let (_, actions) = pass(&ctx, &mut tour, click_at(next), 0.2);
    assert_eq!(tour.page, duplicates + 1);
    assert_eq!(actions.goto_tab, None, "the click frame is still this page");
    let (out, actions) = pass(&ctx, &mut tour, Vec::new(), 0.3);
    assert_eq!(actions.goto_tab, PAGES[duplicates + 1].tab);

    let back = crate::test_ui::painted_text_center(&out, "Back").expect("a Back button");
    pass(&ctx, &mut tour, click_at(back), 0.4);
    let (_, actions) = pass(&ctx, &mut tour, Vec::new(), 0.5);
    assert_eq!(tour.page, duplicates);
    assert_eq!(
        actions.goto_tab,
        Some(Tab::Duplicates),
        "going back re-asks"
    );
}

/// Every page is explicit about the tab it wants, so no page can inherit
/// whatever the last one happened to leave on screen.
#[test]
fn every_page_names_the_tab_it_belongs_on() {
    for (page, spec) in PAGES.iter().enumerate() {
        assert!(
            spec.tab.is_some(),
            "page {page} ({}) does not say where it should be read",
            spec.title
        );
    }
}

#[test]
fn the_input_page_types_the_query_one_character_at_a_time() {
    let input = PAGES
        .iter()
        .position(|p| p.type_query.is_some())
        .expect("a page that types");
    let want = PAGES[input].type_query.expect("checked above");
    let ctx = crate::test_ui::ctx();
    let mut tour = entering(input);

    let (_, actions) = pass(&ctx, &mut tour, Vec::new(), 0.0);
    assert_eq!(
        actions.set_query.as_deref(),
        Some(""),
        "typing starts from an empty box"
    );
    assert!(actions.focus_search, "the caret belongs in the box");

    // Sampled far finer than one character per step, so a burst that skipped
    // to the end would show up as a jump.
    let mut seen: Vec<String> = Vec::new();
    let mut time = 0.0;
    while time < TYPE_INTERVAL * (want.chars().count() as f64 + 2.0) {
        time += TYPE_INTERVAL / 3.0;
        let (_, actions) = pass(&ctx, &mut tour, Vec::new(), time);
        if let Some(query) = actions.set_query {
            seen.push(query);
        }
    }

    let expected: Vec<String> = (1..=want.chars().count())
        .map(|n| want.chars().take(n).collect())
        .collect();
    assert_eq!(seen, expected, "not typed one character at a time");

    // And then it stops: `seed` re-arms the debounce, so a page that kept
    // re-sending the finished query would hold the search off forever.
    let (_, actions) = pass(&ctx, &mut tour, Vec::new(), time + 5.0);
    assert_eq!(
        actions.set_query, None,
        "still typing after the last letter"
    );
}

#[test]
fn leaving_the_typing_page_and_returning_types_it_again() {
    let input = PAGES
        .iter()
        .position(|p| p.type_query.is_some())
        .expect("a page that types");
    let ctx = crate::test_ui::ctx();
    let mut tour = entering(input);
    pass(&ctx, &mut tour, Vec::new(), 0.0);
    pass(&ctx, &mut tour, Vec::new(), 10.0);
    assert!(tour.typing.is_some());

    // Off the page and back on.
    tour.page = input + 1;
    pass(&ctx, &mut tour, Vec::new(), 10.1);
    assert!(tour.typing.is_none(), "the typing outlived its page");
    tour.page = input;
    let (_, actions) = pass(&ctx, &mut tour, Vec::new(), 10.2);
    assert_eq!(actions.set_query.as_deref(), Some(""), "it starts over");
}

#[test]
fn dismissing_returns_to_search_with_an_empty_box() {
    for page in [0, PAGES.len() - 1] {
        let ctx = crate::test_ui::ctx();
        let mut tour = at(page);
        let (out, _) = frame(&ctx, &mut tour, Vec::new());
        let skip = crate::test_ui::painted_text_center(&out, "Skip").expect("a Skip button");
        let (_, actions) = pass(&ctx, &mut tour, click_at(skip), 1.0);
        assert!(actions.dismissed, "page {page} would not skip");
        assert_eq!(actions.goto_tab, Some(Tab::Search));
        assert_eq!(
            actions.set_query.as_deref(),
            Some(""),
            "the demonstration query is not the user's"
        );
    }
}

/// The window is the one thing on screen that can be in the way of what the
/// tour is pointing at, so it has to be movable.
#[test]
fn the_window_can_be_dragged_out_of_the_way() {
    let ctx = crate::test_ui::ctx();
    let mut tour = at(0);
    let title = PAGES[0].title;

    let (out, _) = frame(&ctx, &mut tour, Vec::new());
    let where_is = |out: &egui::FullOutput| {
        painted(out)
            .into_iter()
            .find(|(text, _)| text == title)
            .map(|(_, rect)| rect.min)
            .expect("the title is painted")
    };
    let before = where_is(&out);

    let grab = crate::test_ui::painted_text_center(&out, title).expect("the title bar");
    let delta = egui::vec2(-180.0, -120.0);
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    pass(
        &ctx,
        &mut tour,
        vec![egui::Event::PointerMoved(grab), button(grab, true)],
        0.0,
    );
    pass(
        &ctx,
        &mut tour,
        vec![egui::Event::PointerMoved(grab + delta)],
        0.1,
    );
    let (out, _) = pass(&ctx, &mut tour, vec![button(grab + delta, false)], 0.2);

    let after = where_is(&out);
    assert!(
        (after - before - delta).length() < 2.0,
        "dragged by {delta:?} but the window went from {before:?} to {after:?}"
    );

    // And it stays there when the page changes, rather than snapping back.
    // Measured on the window itself: the next page is a different width, so
    // where its title starts says nothing about where the window is.
    let centre = |ctx: &egui::Context| {
        ctx.memory(|m| m.area_rect(egui::Id::new(WINDOW_ID)))
            .expect("the tour's window")
            .center()
    };
    let dragged_to = centre(&ctx);
    let next = crate::test_ui::painted_text_center(&out, "Next").expect("a Next button");
    pass(&ctx, &mut tour, click_at(next), 0.3);
    pass(&ctx, &mut tour, Vec::new(), 0.4);
    assert!(
        (centre(&ctx) - dragged_to).length() < 2.0,
        "the next page put the window back at {:?}, not {dragged_to:?}",
        centre(&ctx)
    );
}

/// The y of the footer row, probed down the middle column (only Skip
/// occupies it): wrapped text height moves the row from page to page.
fn footer_y(ctx: &egui::Context, page: usize) -> f32 {
    for y in (100..650).step_by(2) {
        let mut t = at(page);
        let (_, actions) = frame(ctx, &mut t, click_at(egui::pos2(500.0, y as f32)));
        if actions.dismissed {
            return y as f32;
        }
    }
    panic!("no Skip button down the middle of page {page}");
}

/// The stretch of x one button answers a click on.
#[derive(Debug, Clone, Copy)]
struct Span {
    lo: f32,
    hi: f32,
}

impl Span {
    fn width(&self) -> f32 {
        self.hi - self.lo
    }
}

/// The three footer buttons, found by what clicking each one does. Must
/// run on a middle page: on the last, Finish and Skip both dismiss
/// without moving; on the first, Back is disabled.
fn footer_spans(ctx: &egui::Context, page: usize) -> [Option<Span>; 3] {
    assert!(page > 0 && page + 1 < PAGES.len(), "probe a middle page");
    let y = footer_y(ctx, page);
    let mut spans: [Option<Span>; 3] = [None; 3];
    for x in 150..850 {
        let mut t = at(page);
        let (_, actions) = frame(ctx, &mut t, click_at(egui::pos2(x as f32, y)));
        let x = x as f32;
        let which = if actions.dismissed {
            1 // Skip
        } else if t.page + 1 == page {
            0 // Back
        } else if t.page == page + 1 {
            2 // Next
        } else {
            continue;
        };
        match &mut spans[which] {
            Some(span) => span.hi = x,
            slot => *slot = Some(Span { lo: x, hi: x }),
        }
    }
    spans
}

/// `Ui::columns` lays a column out justified, so a button dropped
/// straight into one comes out as wide as the whole third — which is
/// what Back was until it was given a layout of its own.
#[test]
fn the_footer_runs_back_then_skip_then_next_at_the_same_size() {
    let ctx = crate::test_ui::ctx();
    let [back, skip, next] = footer_spans(&ctx, 1);
    let back = back.expect("no Back button in the footer");
    let skip = skip.expect("no Skip button in the footer");
    let next = next.expect("no Next button in the footer");
    assert!(
        back.lo < skip.lo,
        "Back ({back:?}) is not left of Skip ({skip:?})"
    );
    assert!(
        skip.lo < next.lo,
        "Skip ({skip:?}) is not left of Next ({next:?})"
    );

    assert!(
        (back.width() - next.width()).abs() <= 2.0,
        "Back is {} wide against Next's {}",
        back.width(),
        next.width()
    );
    // A stretched button fills its third, which no four-letter label does.
    assert!(
        back.width() < 80.0,
        "Back is stretched to {} points",
        back.width()
    );
}

#[test]
fn the_first_page_cannot_go_back() {
    let ctx = crate::test_ui::ctx();
    let y = footer_y(&ctx, 0);
    for x in (150..850).step_by(4) {
        let mut t = at(0);
        let _ = frame(&ctx, &mut t, click_at(egui::pos2(x as f32, y)));
        assert!(
            t.page == 0 || t.page == 1,
            "clicking x={x} left page {}",
            t.page
        );
    }
}

fn sweep(ctx: &egui::Context, page: usize) -> Vec<usize> {
    let mut seen = Vec::new();
    for y in (150..560).step_by(4) {
        for x in (240..760).step_by(8) {
            let mut t = at(page);
            let (_, actions) = frame(ctx, &mut t, click_at(egui::pos2(x as f32, y as f32)));
            if actions.dismissed {
                seen.push(t.page);
            }
        }
    }
    seen
}

#[test]
fn skip_and_finish_both_dismiss() {
    let ctx = crate::test_ui::ctx();

    // Skip is on every page.
    assert!(
        !sweep(&ctx, 0).is_empty(),
        "Skip never fired on the first page"
    );

    // Finish only on the last, where it replaces Next.
    let last = PAGES.len() - 1;
    assert!(
        !sweep(&ctx, last).is_empty(),
        "Finish never fired on the last page"
    );
    let ctx = crate::test_ui::ctx();
    let mut tour = at(last);
    let (out, _) = frame(&ctx, &mut tour, Vec::new());
    assert!(
        painted_text(&out).contains(&"Finish".to_string()),
        "the last page still offers Next"
    );
}

/// The window is measured on its first frame, and on that frame the viewport
/// is not necessarily the size it settles at — the real app opened the tour
/// pinned to the bottom-right corner because a position picked once was
/// picked against the wrong screen. Until the user drags it, it belongs in
/// the middle of whatever the window currently is.
#[test]
fn the_window_centres_itself_on_the_window_it_is_in() {
    let ctx = crate::test_ui::ctx();
    let mut tour = at(0);
    let roots = roots();
    let mut run = |size: egui::Vec2| {
        let _ = ctx.run(raw_input_at(size, Vec::new(), 0.0), |ctx| {
            tour.ui(ctx, &roots);
        });
        let rect = ctx
            .memory(|m| m.area_rect(egui::Id::new(WINDOW_ID)))
            .expect("the tour's window");
        (
            rect.center(),
            egui::Rect::from_min_size(egui::Pos2::ZERO, size).center(),
        )
    };

    // Measured against a viewport it never sees again.
    run(egui::vec2(1920.0, 1080.0));
    for _ in 0..3 {
        let (window, screen) = run(SCREEN);
        assert!(
            (window - screen).length() < 2.0,
            "the window sits at {window:?} in a screen centred on {screen:?}"
        );
    }
}
