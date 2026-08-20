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

#[test]
fn an_empty_result_says_so_rather_than_showing_an_empty_list() {
    let ctx = crate::test_ui::ctx();
    let mut tab = DuplicatesTab {
        state: DupState::Loaded(LoadedGroups::new(Vec::new())),
    };
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(
        painted.contains(&"No duplicate files found.".to_string()),
        "{painted:?}"
    );
}
