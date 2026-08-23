use super::*;

use crate::test_ui::{painted_text, painted_text_center, raw_input};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

fn group(paths: &[&str]) -> DuplicateGroup {
    DuplicateGroup {
        hash: vec![0xab; 32],
        count: paths.len() as i64,
        total_size: 100 * paths.len() as i64,
        redundant_size: 100 * (paths.len() as i64 - 1),
        members: paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let name = p.rsplit('/').next().unwrap_or(p).to_string();
                (i as i64, name, p.to_string(), 100u64, 1_700_000_000i64)
            })
            .collect(),
    }
}

fn loaded(paths: &[&str]) -> DuplicatesTab {
    DuplicatesTab {
        state: DupState::Loaded(LoadedGroups::new(vec![group(paths)])),
        sort: DupSort::default(),
    }
}

fn frame(
    ctx: &egui::Context,
    tab: &mut DuplicatesTab,
    busy: bool,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, DuplicatesActions) {
    let mut actions = DuplicatesActions::default();
    let out = ctx.run(raw_input(SCREEN, events), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            actions = tab.ui(ui, busy);
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &out);
    (out, actions)
}

fn click(pos: egui::Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
    let mut events = vec![egui::Event::PointerMoved(pos)];
    events.extend(
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::default(),
            }),
    );
    events
}

/// Right-click `needle` and return what the menu it opened painted, plus the
/// actions from that frame.
fn context_menu_on(
    ctx: &egui::Context,
    tab: &mut DuplicatesTab,
    busy: bool,
    needle: &str,
) -> (Vec<String>, egui::Pos2) {
    let (out, _) = frame(ctx, tab, busy, Vec::new());
    let target = painted_text_center(&out, needle)
        .unwrap_or_else(|| panic!("nothing painted for {needle:?}"));
    frame(
        ctx,
        tab,
        busy,
        click(target, egui::PointerButton::Secondary),
    );
    // The menu is its own area, painted on the frame after the click.
    let (out, _) = frame(ctx, tab, busy, Vec::new());
    (painted_text(&out), target)
}

/// The title line carries the group; find it without rebuilding its wording.
fn header_of(ctx: &egui::Context, tab: &mut DuplicatesTab) -> String {
    let (out, _) = frame(ctx, tab, false, Vec::new());
    painted_text(&out)
        .into_iter()
        .find(|t| t.contains("reclaimable"))
        .expect("no group header painted")
}

const PATHS: [&str; 3] = ["/a/img.raw", "/b/img.raw", "/c/img.raw"];

#[test]
fn a_group_header_offers_the_verification() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let header = header_of(&ctx, &mut tab);
    let (menu, _) = context_menu_on(&ctx, &mut tab, false, &header);
    assert!(
        menu.contains(&VERIFY_LABEL.to_string()),
        "the group's own row does not offer it: {menu:?}"
    );
}

/// Clicking it asks for the whole group, not the one row it was asked from.
#[test]
fn verifying_asks_for_every_member_of_the_group() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let header = header_of(&ctx, &mut tab);
    context_menu_on(&ctx, &mut tab, false, &header);

    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let entry = painted_text_center(&out, VERIFY_LABEL).expect("no verify entry painted");
    let (_, actions) = frame(
        &ctx,
        &mut tab,
        false,
        click(entry, egui::PointerButton::Primary),
    );
    assert_eq!(
        actions.verify,
        Some(PATHS.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    );
}

/// There is one verification window, so a second run is refused where it is
/// asked for rather than replacing what someone is reading.
#[test]
fn a_second_verification_is_refused_while_the_window_is_open() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let header = header_of(&ctx, &mut tab);
    context_menu_on(&ctx, &mut tab, true, &header);

    let (out, _) = frame(&ctx, &mut tab, true, Vec::new());
    let entry = painted_text_center(&out, VERIFY_LABEL).expect("the entry should still be listed");
    let (_, actions) = frame(
        &ctx,
        &mut tab,
        true,
        click(entry, egui::PointerButton::Primary),
    );
    assert_eq!(actions.verify, None, "a disabled entry still fired");
}

/// Expanding a group and right-clicking one of its files offers the same
/// thing: the rows are what someone is looking at when the question occurs.
#[test]
fn a_member_row_offers_the_verification_too() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let header = header_of(&ctx, &mut tab);

    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let pos = painted_text_center(&out, &header).expect("no header painted");
    frame(
        &ctx,
        &mut tab,
        false,
        click(pos, egui::PointerButton::Primary),
    );

    let (menu, _) = context_menu_on(&ctx, &mut tab, false, PATHS[1]);
    assert!(
        menu.contains(&VERIFY_LABEL.to_string()),
        "an expanded member row does not offer it: {menu:?}"
    );
    assert!(
        menu.contains(&"Open File".to_string()),
        "the existing entries went missing: {menu:?}"
    );
}

/// A group named by its first member, with the waste that decides the
/// default order set by hand.
fn sized_group(name: &str, redundant: i64) -> DuplicateGroup {
    let mut group = group(&[&format!("/a/{name}"), &format!("/b/{name}")]);
    group.hash = name.as_bytes().to_vec();
    group.redundant_size = redundant;
    group
}

/// The names of the groups, in the order the tab would list them.
fn listed(groups: Vec<DuplicateGroup>, sort: DupSort) -> Vec<String> {
    let mut loaded = LoadedGroups::new(groups);
    loaded.sort(sort);
    loaded
        .order
        .iter()
        .map(|&i| loaded.groups[i].members[0].1.clone())
        .collect()
}

#[test]
fn the_default_order_is_the_one_the_scan_returned() {
    let groups = vec![
        sized_group("big.txt", 900),
        sized_group("small.jpg", 10),
        sized_group("middling.txt", 100),
    ];
    assert_eq!(
        listed(groups, DupSort::Reclaimable),
        ["big.txt", "small.jpg", "middling.txt"],
        "the query already ordered these; the tab must not reshuffle them"
    );
}

#[test]
fn sorting_by_extension_gathers_the_extensions_together() {
    let groups = vec![
        sized_group("b.txt", 10),
        sized_group("a.jpg", 20),
        sized_group("a.txt", 30),
        sized_group("b.jpg", 40),
    ];
    assert_eq!(
        listed(groups, DupSort::Extension),
        // .jpg before .txt, and within each the bigger waste first.
        ["b.jpg", "a.jpg", "a.txt", "b.txt"]
    );
}

/// Files without one are a category of their own, and not an interesting
/// one; they go after every extension rather than in front of "a".
#[test]
fn groups_without_an_extension_sort_last() {
    let groups = vec![
        sized_group("README", 900),
        sized_group("notes.zzz", 10),
        sized_group("Makefile", 800),
    ];
    assert_eq!(
        listed(groups, DupSort::Extension),
        ["notes.zzz", "README", "Makefile"]
    );
}

#[test]
fn extension_matching_ignores_case() {
    let groups = vec![
        sized_group("shot.JPG", 10),
        sized_group("scan.jpg", 20),
        sized_group("note.txt", 30),
    ];
    assert_eq!(
        listed(groups, DupSort::Extension),
        ["scan.jpg", "shot.JPG", "note.txt"],
        "one extension, spelled two ways, is still one extension"
    );
}

/// Switching back is switching back, not a second arbitrary order.
#[test]
fn the_order_returns_when_the_choice_does() {
    let groups = vec![
        sized_group("big.txt", 900),
        sized_group("small.jpg", 10),
        sized_group("middling.txt", 100),
    ];
    let mut loaded = LoadedGroups::new(groups);
    loaded.sort(DupSort::Extension);
    loaded.sort(DupSort::Reclaimable);
    let names: Vec<&str> = loaded
        .order
        .iter()
        .map(|&i| loaded.groups[i].members[0].1.as_str())
        .collect();
    assert_eq!(names, ["big.txt", "small.jpg", "middling.txt"]);
}

/// End to end: the choice reaches the list, and reordering what is already
/// loaded is not a reason to go back to the database.
#[test]
fn choosing_an_order_relists_without_rescanning() {
    let ctx = crate::test_ui::ctx();
    let mut tab = DuplicatesTab {
        state: DupState::Loaded(LoadedGroups::new(vec![
            sized_group("b.txt", 900),
            sized_group("a.jpg", 10),
        ])),
        sort: DupSort::default(),
    };

    let order_on_screen = |out: &egui::FullOutput| -> Vec<String> {
        painted_text(out)
            .into_iter()
            .filter(|t| t.contains("reclaimable"))
            .collect()
    };
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let before = order_on_screen(&out);
    assert!(before[0].contains("b.txt"), "{before:?}");

    tab.sort = DupSort::Extension;
    let (out, actions) = frame(&ctx, &mut tab, false, Vec::new());
    let after = order_on_screen(&out);
    assert!(after[0].contains("a.jpg"), "{after:?}");
    assert!(
        !actions.refresh,
        "reordering what is loaded must not ask for another scan"
    );
    assert_eq!(before.len(), after.len(), "a group went missing");
}

#[test]
fn an_empty_result_says_so_rather_than_showing_an_empty_list() {
    let ctx = crate::test_ui::ctx();
    let mut tab = DuplicatesTab {
        state: DupState::Loaded(LoadedGroups::new(Vec::new())),
        sort: DupSort::default(),
    };
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(
        painted.contains(&"No duplicate files found.".to_string()),
        "{painted:?}"
    );
}
