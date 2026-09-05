use super::*;

use crate::test_ui::{painted_text, painted_text_center, raw_input};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);
/// What `app.rs` passes; the banner quotes it back.
const HASH_LENGTH: usize = 8192;

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

fn tab(groups: Vec<DuplicateGroup>, filters: DuplicatesConfig) -> DuplicatesTab {
    let mut tab = DuplicatesTab::new(&filters);
    tab.scan_limit = crate::backend::DUP_SCAN_LIMIT;
    tab.state = DupState::Loaded(LoadedGroups::new(groups));
    tab
}

fn loaded(paths: &[&str]) -> DuplicatesTab {
    tab(vec![group(paths)], DuplicatesConfig::default())
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
            actions = tab.ui(ui, busy, HASH_LENGTH);
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

/// Click the menu entry labelled `label`, which the caller has already opened.
fn click_menu_entry(
    ctx: &egui::Context,
    tab: &mut DuplicatesTab,
    label: &str,
) -> DuplicatesActions {
    let (out, _) = frame(ctx, tab, false, Vec::new());
    let entry =
        painted_text_center(&out, label).unwrap_or_else(|| panic!("no {label:?} entry painted"));
    frame(ctx, tab, false, click(entry, egui::PointerButton::Primary)).1
}

/// The title line carries the group; find it without rebuilding its wording.
fn header_of(ctx: &egui::Context, tab: &mut DuplicatesTab) -> String {
    let (out, _) = frame(ctx, tab, false, Vec::new());
    headers(&out)
        .into_iter()
        .next()
        .expect("no group header painted")
}

/// Every group header on screen, in listed order.
fn headers(out: &egui::FullOutput) -> Vec<String> {
    painted_text(out)
        .into_iter()
        .filter(|t| t.contains("reclaimable"))
        .collect()
}

const PATHS: [&str; 3] = ["/a/img.raw", "/b/img.raw", "/c/img.raw"];

// --- The verification, and how it is found ---------------------------------

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
    let actions = click_menu_entry(&ctx, &mut tab, VERIFY_LABEL);
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

/// The complaint this answers: the only word about verification used to be
/// behind a right-click nobody had performed yet.
#[test]
fn the_listing_says_what_a_group_is_without_being_asked() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0).join(" ");
    assert!(
        painted.contains("suspected duplicates"),
        "nothing warns that a group is a suspicion: {painted:?}"
    );
    assert!(
        painted.contains("not proof"),
        "the caution does not say what it is cautioning about: {painted:?}"
    );
    assert!(
        painted.contains(VERIFY_LABEL.trim_end_matches('…')),
        "the banner does not name the way to settle it: {painted:?}"
    );
}

/// It quotes the setting, not a number written twice.
#[test]
fn the_banner_quotes_the_configured_sample_size() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let mut actions = DuplicatesActions::default();
    let out = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            actions = tab.ui(ui, false, 64 * 1024);
        });
    });
    let _ = actions;
    let painted = painted_text(&out).join(" ");
    assert!(
        painted.contains("first 65.5 KB"),
        "the banner ignores hash_length: {painted:?}"
    );
}

/// A tab with nothing to warn about does not warn.
#[test]
fn an_empty_result_says_so_rather_than_showing_an_empty_list() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(Vec::new(), DuplicatesConfig::default());
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(painted.contains(&NO_GROUPS.to_string()), "{painted:?}");
    assert!(
        !painted.iter().any(|t| t.contains("suspected duplicates")),
        "an empty tab cautioned about nothing: {painted:?}"
    );
}

// --- Ordering ---------------------------------------------------------------

/// A group named by its first member, with the waste that decides the
/// default order set by hand.
fn sized_group(name: &str, redundant: i64) -> DuplicateGroup {
    let mut group = group(&[&format!("/a/{name}"), &format!("/b/{name}")]);
    group.hash = name.as_bytes().to_vec();
    group.redundant_size = redundant;
    // The title is priced from the members, so the size has to agree with the
    // waste the ordering is being tested on: two copies, one reclaimable.
    for member in group.members.iter_mut() {
        member.3 = redundant.max(0) as u64;
    }
    group
}

/// The names of the groups, in the order the tab would list them.
fn listed(groups: Vec<DuplicateGroup>, sort: DupSort) -> Vec<String> {
    let filters = DupFilters::new(&DuplicatesConfig::default());
    let mut loaded = LoadedGroups::new(groups);
    loaded.rebuild(sort, &filters, false);
    loaded
        .rows
        .iter()
        .map(|row| loaded.groups[row.group].members[row.members[0]].1.clone())
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
    let filters = DupFilters::new(&DuplicatesConfig::default());
    let mut loaded = LoadedGroups::new(groups);
    loaded.rebuild(DupSort::Extension, &filters, false);
    loaded.rebuild(DupSort::Reclaimable, &filters, false);
    let names: Vec<&str> = loaded
        .rows
        .iter()
        .map(|row| loaded.groups[row.group].members[row.members[0]].1.as_str())
        .collect();
    assert_eq!(names, ["big.txt", "small.jpg", "middling.txt"]);
}

/// End to end: the choice reaches the list, and reordering what is already
/// loaded is not a reason to go back to the database.
#[test]
fn choosing_an_order_relists_without_rescanning() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(
        vec![sized_group("b.txt", 900), sized_group("a.jpg", 10)],
        DuplicatesConfig::default(),
    );

    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let before = headers(&out);
    assert!(before[0].contains("b.txt"), "{before:?}");

    tab.sort = DupSort::Extension;
    let (out, actions) = frame(&ctx, &mut tab, false, Vec::new());
    let after = headers(&out);
    assert!(after[0].contains("a.jpg"), "{after:?}");
    assert!(
        !actions.refresh,
        "reordering what is loaded must not ask for another scan"
    );
    assert_eq!(before.len(), after.len(), "a group went missing");
}

// --- Exclusions -------------------------------------------------------------

fn excluding(patterns: &[&str]) -> DuplicatesConfig {
    DuplicatesConfig {
        exclude_patterns: patterns.iter().map(|p| p.to_string()).collect(),
        hidden_groups: Vec::new(),
    }
}

/// An excluded copy stops being counted, and the group is re-priced around
/// the ones that are left — listing "3 ×" beside two paths would be a lie
/// about how much space deleting them saves.
#[test]
fn an_excluded_member_leaves_the_group_and_its_totals() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], excluding(&["/c/*"]));
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let header = headers(&out)
        .into_iter()
        .next()
        .expect("group went missing");
    assert!(
        header.starts_with("2 × img.raw:"),
        "the count still includes the excluded copy: {header:?}"
    );
    assert!(
        header.contains("100 B reclaimable (200 B total)"),
        "the totals still include the excluded copy: {header:?}"
    );
    let painted = painted_text(&out);
    assert!(
        !painted.contains(&"/c/img.raw".to_string()),
        "the excluded path is still listed: {painted:?}"
    );
}

/// One surviving copy is not a duplicate of anything.
#[test]
fn a_group_down_to_one_survivor_is_not_listed() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], excluding(&["/b/*", "/c/*"]));
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(painted.contains(&ALL_FILTERED.to_string()), "{painted:?}");
    assert!(
        !painted.contains(&NO_GROUPS.to_string()),
        "a filtered listing must not read as an index with no duplicates in it"
    );
}

/// Verification is offered on what is on screen, not on what was excluded.
#[test]
fn verifying_a_filtered_group_reads_only_its_survivors() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], excluding(&["/c/*"]));
    let header = header_of(&ctx, &mut tab);
    context_menu_on(&ctx, &mut tab, false, &header);
    let actions = click_menu_entry(&ctx, &mut tab, VERIFY_LABEL);
    assert_eq!(
        actions.verify,
        Some(vec!["/a/img.raw".to_string(), "/b/img.raw".to_string()]),
        "an excluded file was read anyway"
    );
}

/// Adding one is an edit to the listing, not a reason to re-read the index.
#[test]
fn a_new_exclusion_relists_without_rescanning_and_is_persisted() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], DuplicatesConfig::default());
    frame(&ctx, &mut tab, false, Vec::new());

    tab.draft = "*.raw".to_string();
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let add = painted_text_center(&out, "Add").expect("no Add button painted");
    let (_, actions) = frame(
        &ctx,
        &mut tab,
        false,
        click(add, egui::PointerButton::Primary),
    );

    assert!(!actions.refresh, "an exclusion asked for another scan");
    assert_eq!(
        actions.save_filters.map(|f| f.exclude_patterns),
        Some(vec!["*.raw".to_string()]),
        "the exclusion was not handed back to be saved"
    );
    // The click lands after the frame it was laid out for, so what it changed
    // is on the next one.
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(
        painted.contains(&ALL_FILTERED.to_string()),
        "the group survived its own exclusion: {painted:?}"
    );
    assert!(
        painted.contains(&"*.raw ×".to_string()),
        "no chip to take it back off with: {painted:?}"
    );
    assert!(
        !painted.contains(&"*.raw".to_string()),
        "the editor kept the pattern it just added: {painted:?}"
    );
}

/// …and taking the chip off puts the group back.
#[test]
fn removing_the_chip_brings_the_groups_back() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], excluding(&["*.raw"]));
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let chip = painted_text_center(&out, "*.raw ×").expect("no chip painted");
    let (out, actions) = frame(
        &ctx,
        &mut tab,
        false,
        click(chip, egui::PointerButton::Primary),
    );
    assert_eq!(
        actions.save_filters.map(|f| f.exclude_patterns),
        Some(Vec::new())
    );
    assert_eq!(headers(&out).len(), 1, "the group did not come back");
}

/// Right-clicking a file is the shortest way to say "not this folder again".
#[test]
fn a_member_row_offers_the_two_exclusions_it_stands_for() {
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
        menu.contains(&"Exclude *.raw from duplicates".to_string()),
        "no extension exclusion: {menu:?}"
    );
    assert!(
        menu.iter().any(|t| t.contains("Exclude this folder")),
        "no folder exclusion: {menu:?}"
    );

    let actions = click_menu_entry(&ctx, &mut tab, "Exclude *.raw from duplicates");
    assert_eq!(
        actions.save_filters.map(|f| f.exclude_patterns),
        Some(vec!["*.raw".to_string()])
    );
}

/// Only a hand-edited config can get here — the editor refuses one — and the
/// answer is to say so, not to quietly list files someone asked to never see.
#[test]
fn an_unparseable_pattern_is_reported_rather_than_silently_dropped() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(vec![group(&PATHS)], excluding(&["a[b"]));
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(
        painted
            .iter()
            .any(|t| t.contains("Exclusions are not being applied")),
        "a broken pattern set said nothing: {painted:?}"
    );
    assert_eq!(
        headers(&frame(&ctx, &mut tab, false, Vec::new()).0).len(),
        1,
        "a broken pattern set hid the listing instead of listing it"
    );
}

// --- Hidden groups ----------------------------------------------------------

/// The group `group()` builds, as the config spells it.
fn hash_of(group: &DuplicateGroup) -> String {
    group.hash_hex()
}

#[test]
fn hiding_a_group_takes_it_off_the_list_and_is_persisted() {
    let ctx = crate::test_ui::ctx();
    let mut tab = loaded(&PATHS);
    let hash = hash_of(&group(&PATHS));
    let header = header_of(&ctx, &mut tab);
    context_menu_on(&ctx, &mut tab, false, &header);
    let actions = click_menu_entry(&ctx, &mut tab, HIDE_LABEL);

    assert_eq!(
        actions.save_filters.map(|f| f.hidden_groups),
        Some(vec![hash]),
        "hiding is not remembered by content hash"
    );
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(painted.contains(&ALL_FILTERED.to_string()), "{painted:?}");
    assert!(
        painted.iter().any(|t| t.contains("Hidden: 1 group")),
        "nothing says a group is being kept off screen: {painted:?}"
    );
}

/// A listing already on screen when the config named the group still drops it,
/// so the hiding survives a restart.
#[test]
fn a_group_hidden_in_the_config_is_not_listed() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(
        vec![group(&PATHS), sized_group("keep.txt", 40)],
        DuplicatesConfig {
            exclude_patterns: Vec::new(),
            hidden_groups: vec![hash_of(&group(&PATHS))],
        },
    );
    let out = frame(&ctx, &mut tab, false, Vec::new()).0;
    let listed = headers(&out);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(listed[0].contains("keep.txt"), "{listed:?}");
    assert!(
        painted_text(&out)
            .iter()
            .any(|t| t.contains("1 more hidden by your filters")),
        "the listing does not account for what it dropped"
    );
}

/// Hiding has to be reversible without editing a config file by hand.
#[test]
fn show_hidden_lists_them_again_and_offers_the_way_back() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(
        vec![group(&PATHS)],
        DuplicatesConfig {
            exclude_patterns: Vec::new(),
            hidden_groups: vec![hash_of(&group(&PATHS))],
        },
    );
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let toggle = painted_text_center(&out, "Show hidden").expect("no Show hidden control");
    let (out, _) = frame(
        &ctx,
        &mut tab,
        false,
        click(toggle, egui::PointerButton::Primary),
    );
    let listed = headers(&out);
    assert_eq!(listed.len(), 1, "showing hidden groups listed none");
    assert!(
        listed[0].starts_with("Hidden — "),
        "a revealed group is not marked as one: {listed:?}"
    );

    context_menu_on(&ctx, &mut tab, false, &listed[0]);
    let actions = click_menu_entry(&ctx, &mut tab, UNHIDE_LABEL);
    assert_eq!(
        actions.save_filters.map(|f| f.hidden_groups),
        Some(Vec::new()),
        "unhiding did not take it back out of the config"
    );
}

/// Clear is the escape hatch for a hidden list nobody wants to unpick.
#[test]
fn clear_empties_the_hidden_list() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(
        vec![group(&PATHS)],
        DuplicatesConfig {
            exclude_patterns: Vec::new(),
            hidden_groups: vec![hash_of(&group(&PATHS)), "00".repeat(32)],
        },
    );
    let (out, _) = frame(&ctx, &mut tab, false, Vec::new());
    let clear = painted_text_center(&out, "Clear").expect("no Clear button");
    let (out, actions) = frame(
        &ctx,
        &mut tab,
        false,
        click(clear, egui::PointerButton::Primary),
    );
    assert_eq!(
        actions.save_filters.map(|f| f.hidden_groups),
        Some(Vec::new())
    );
    assert_eq!(headers(&out).len(), 1, "the group did not come back");
}

/// A hash spelled in capitals by a hand-edited config still means that group.
#[test]
fn hidden_hashes_are_matched_case_insensitively() {
    let ctx = crate::test_ui::ctx();
    let mut tab = tab(
        vec![group(&PATHS)],
        DuplicatesConfig {
            exclude_patterns: Vec::new(),
            hidden_groups: vec![hash_of(&group(&PATHS)).to_uppercase()],
        },
    );
    let painted = painted_text(&frame(&ctx, &mut tab, false, Vec::new()).0);
    assert!(painted.contains(&ALL_FILTERED.to_string()), "{painted:?}");
}

// --- The two together -------------------------------------------------------

/// The extension listing orders on what the titles say, which after an
/// exclusion is not what the scan ranked.
#[test]
fn the_extension_order_follows_the_recomputed_waste() {
    // Three copies of one .txt (200 B reclaimable) against two of another
    // (100 B) — until an exclusion takes the third copy off the first.
    let mut big = group(&["/a/big.txt", "/b/big.txt", "/keep-out/big.txt"]);
    big.hash = b"big".to_vec();
    let mut small = group(&["/a/small.txt", "/b/small.txt"]);
    small.hash = b"small".to_vec();

    assert_eq!(
        listed(vec![big.clone(), small.clone()], DupSort::Extension),
        ["big.txt", "small.txt"],
        "unfiltered, the three-copy group wastes more"
    );

    let filters = DupFilters::new(&excluding(&["/keep-out/*"]));
    let mut loaded = LoadedGroups::new(vec![big, small]);
    loaded.rebuild(DupSort::Extension, &filters, false);
    let names: Vec<&str> = loaded
        .rows
        .iter()
        .map(|row| loaded.groups[row.group].members[row.members[0]].1.as_str())
        .collect();
    assert_eq!(
        names,
        ["big.txt", "small.txt"],
        "both waste 100 B now, so the hash settles it — but stably"
    );
    assert!(
        loaded.rows.iter().all(|row| row.redundant == 100),
        "the order was taken from the scan's price, not the listed one"
    );
}

/// The rebuild is what every filter edit goes through, so it must not run for
/// a frame that changed nothing.
#[test]
fn an_unchanged_listing_is_not_rebuilt() {
    let filters = DupFilters::new(&DuplicatesConfig::default());
    let mut loaded = LoadedGroups::new(vec![group(&PATHS)]);
    loaded.rebuild(DupSort::Reclaimable, &filters, false);
    let built = loaded.built_for;
    loaded.rows.clear();
    loaded.rebuild(DupSort::Reclaimable, &filters, false);
    assert_eq!(loaded.built_for, built);
    assert!(
        loaded.rows.is_empty(),
        "an unchanged listing was rebuilt anyway"
    );
}
