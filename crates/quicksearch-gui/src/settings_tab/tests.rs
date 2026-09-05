use super::*;

// Headless-safe: `keychain_active` only probes the OS keychain when
// `use_keychain` is set, and no test here sets it.

#[test]
fn a_fresh_draft_is_not_dirty() {
    let mut w = SettingsTab::new();
    let cfg = Config::default();
    assert!(!w.is_dirty(&cfg), "no draft at all");
    w.stage(&cfg);
    assert!(!w.is_dirty(&cfg));
}

#[test]
fn an_edited_draft_is_dirty_until_discarded() {
    let mut w = SettingsTab::new();
    let cfg = Config::default();
    w.stage(&cfg);
    w.draft.as_mut().unwrap().search.debounce_ms += 100;
    assert!(w.is_dirty(&cfg));
    w.discard();
    assert!(w.draft.is_none());
    assert!(!w.is_dirty(&cfg), "the draft is gone");
}

/// The stale copies of live-config fields in the draft are not edits.
#[test]
fn live_security_and_mode_changes_are_not_dirty() {
    let mut w = SettingsTab::new();
    let mut cfg = Config::default();
    w.stage(&cfg);
    cfg.security.use_keychain = !cfg.security.use_keychain;
    cfg.indexing.auto_index = !cfg.indexing.auto_index;
    assert!(!w.is_dirty(&cfg));
}

/// Without the restage, an edit made on Manage Index in between would be
/// reverted by a later Apply — `pin_live_fields` does not cover the filters.
#[test]
fn a_draft_is_restaged_from_the_live_config_after_leaving() {
    let mut w = SettingsTab::new();
    let mut cfg = Config::default();
    w.stage(&cfg);
    w.discard();

    cfg.indexing.ignore_patterns.push("*.tmp".to_string());
    w.stage(&cfg);
    assert!(!w.is_dirty(&cfg), "the fresh draft matches the live config");
    assert_eq!(
        w.draft_config().unwrap().indexing.ignore_patterns,
        cfg.indexing.ignore_patterns,
        "the filter added while the tab was away survives"
    );
}

/// A key capture in progress cannot outlive the tab.
#[test]
fn leaving_the_tab_ends_a_shortcut_capture() {
    let mut w = SettingsTab::new();
    let cfg = Config::default();
    w.stage(&cfg);
    w.capturing_hotkey = true;
    w.discard();
    assert!(!w.capturing_hotkey());
}

use crate::test_ui::{click_at, painted_text, painted_text_center};

/// Outside the tab's scroll area so it is never below the fold.
fn run_hotkey_edit(
    ctx: &egui::Context,
    setting: &mut String,
    capturing: &mut bool,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), events);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            hotkey_edit(ui, setting, capturing);
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &out);
    out
}

fn run_color_scheme_edit(
    ctx: &egui::Context,
    setting: &mut String,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), events);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            color_scheme_edit(ui, setting);
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &out);
    out
}

/// What the dropdown shows and what it stores are not the same string.
#[test]
fn the_color_scheme_box_shows_and_sets_the_scheme() {
    let ctx = crate::test_ui::ctx();
    let mut setting = "dark".to_string();

    let closed = run_color_scheme_edit(&ctx, &mut setting, vec![]);
    let target = painted_text_center(&closed, "Dark").expect("the current scheme was not painted");

    run_color_scheme_edit(&ctx, &mut setting, click_at(target));
    let open = run_color_scheme_edit(&ctx, &mut setting, vec![]);
    let light = painted_text_center(&open, "Light").expect("the list did not open");

    run_color_scheme_edit(&ctx, &mut setting, click_at(light));
    assert_eq!(setting, "light", "picking Light stores the config value");

    let after = run_color_scheme_edit(&ctx, &mut setting, vec![]);
    assert!(
        painted_text(&after).iter().any(|t| t == "Light"),
        "the closed box still says what is in force: {:?}",
        painted_text(&after)
    );
}

#[test]
fn an_unknown_scheme_reads_as_dark() {
    assert_eq!(scheme_label("dark"), "Dark");
    assert_eq!(scheme_label("light"), "Light");
    assert_eq!(scheme_label("  LIGHT "), "Light");
    for nonsense in ["", "drak", "system", "auto"] {
        assert_eq!(scheme_label(nonsense), "Dark", "{:?}", nonsense);
    }
}

fn press(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    vec![egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }]
}

const CTRL_ALT: egui::Modifiers = egui::Modifiers {
    alt: true,
    ctrl: true,
    shift: false,
    mac_cmd: false,
    command: true,
};

#[test]
fn the_shortcut_button_binds_what_was_pressed() {
    let ctx = crate::test_ui::ctx();
    let mut setting = "Ctrl+Shift+F".to_string();
    let mut capturing = false;

    let first = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    let button =
        painted_text_center(&first, "Ctrl+Shift+F").expect("the current shortcut was not painted");

    run_hotkey_edit(&ctx, &mut setting, &mut capturing, click_at(button));
    assert!(capturing, "clicking the button starts a capture");
    let waiting = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    assert!(
        painted_text(&waiting)
            .iter()
            .any(|t| t.starts_with("Press a key")),
        "a capturing button says so"
    );

    run_hotkey_edit(
        &ctx,
        &mut setting,
        &mut capturing,
        press(egui::Key::G, CTRL_ALT),
    );
    assert_eq!(setting, "Ctrl+Alt+G");
    assert!(!capturing, "a bound press ends the capture");
}

#[test]
fn capture_ignores_what_it_cannot_bind_and_escape_cancels() {
    let ctx = crate::test_ui::ctx();
    let mut setting = "Ctrl+Shift+F".to_string();
    let mut capturing = true;

    // A bare letter: someone reaching for the modifier a moment late.
    run_hotkey_edit(
        &ctx,
        &mut setting,
        &mut capturing,
        press(egui::Key::G, egui::Modifiers::NONE),
    );
    assert_eq!(setting, "Ctrl+Shift+F", "a bare key binds nothing");
    assert!(capturing, "and does not end the capture");

    run_hotkey_edit(
        &ctx,
        &mut setting,
        &mut capturing,
        press(egui::Key::Escape, egui::Modifiers::NONE),
    );
    assert_eq!(setting, "Ctrl+Shift+F", "Escape leaves the shortcut alone");
    assert!(!capturing);
}

#[test]
fn clear_switches_the_shortcut_off() {
    let ctx = crate::test_ui::ctx();
    let mut setting = "Ctrl+Shift+F".to_string();
    let mut capturing = false;

    let first = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    let clear = painted_text_center(&first, "Clear").expect("Clear was not painted");
    run_hotkey_edit(&ctx, &mut setting, &mut capturing, click_at(clear));
    assert_eq!(setting, "");

    // The button says what the state is rather than going blank.
    let empty = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    assert!(painted_text(&empty).iter().any(|t| t == "None"));
}

/// Until Apply, the draft and the live registration can disagree.
#[test]
fn an_unapplied_shortcut_says_it_is_not_in_force_yet() {
    let ctx = crate::test_ui::ctx();
    let run = |draft: &str, live: &str| {
        let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| hotkey_note(ui, draft, live));
        });
        painted_text(&out).join("\n")
    };
    assert!(run("Ctrl+Alt+K", "Ctrl+Shift+F").contains("Apply and Save"));
    // Matching, and nothing registered in a test process.
    assert_eq!(run("Ctrl+Shift+F", "Ctrl+Shift+F"), "");
}

/// Every row with the tip it must show and who it is for, so a wrong tooltip
/// is impossible and the everyday/advanced split is written down once.
const ROWS: &[(Section, &str, &tips::Tip, Level)] = &[
    (
        Section::Indexing,
        "Full reindex every",
        &tips::REINDEX_INTERVAL,
        Level::Advanced,
    ),
    (
        Section::Indexing,
        "Follow symlinks",
        &tips::FOLLOW_SYMLINKS,
        Level::Advanced,
    ),
    (
        Section::Indexing,
        "Include hidden files",
        &tips::INCLUDE_HIDDEN,
        Level::Everyday,
    ),
    (
        Section::Processing,
        "Tokenizer",
        &tips::TOKENIZER,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Hash sample size (bytes)",
        &tips::HASH_LENGTH,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Max stored text (bytes)",
        &tips::MAX_STORED_TEXT,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Max text file size (bytes)",
        &tips::MAX_TEXT_FILE_SIZE,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Batch size",
        &tips::BATCH_SIZE,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Max WAL size (bytes)",
        &tips::MAX_WAL_SIZE,
        Level::Advanced,
    ),
    (
        Section::Processing,
        "Store text for snippets",
        &tips::STORE_TEXT,
        Level::Everyday,
    ),
    (
        Section::Search,
        "Fuzzy search ON by default",
        &tips::FUZZY_DEFAULT,
        Level::Everyday,
    ),
    (
        Section::Search,
        "Fuzzy edit distance",
        &tips::FUZZY_EDITS,
        Level::Advanced,
    ),
    (
        Section::Search,
        "Display limit",
        &tips::DISPLAY_LIMIT,
        Level::Advanced,
    ),
    (
        Section::Search,
        "Stream batch size",
        &tips::RESULTS_PER_PAGE,
        Level::Advanced,
    ),
    (
        Section::Search,
        "Debounce (ms)",
        &tips::DEBOUNCE,
        Level::Advanced,
    ),
    (
        Section::Search,
        "Live results",
        &tips::LIVE_RESULTS,
        Level::Everyday,
    ),
    (
        Section::Search,
        "Search cache MiB (0 = auto)",
        &tips::SEARCH_CACHE,
        Level::Advanced,
    ),
];

/// Rendered without the tab's scroll area so nothing sits below the fold, and
/// with advanced on so every row is present to be hovered.
#[test]
fn every_row_shows_its_own_tip() {
    for (section, label, tip, _) in ROWS {
        let ctx = crate::test_ui::ctx();
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let mut cfg = Config::default();
        let form = Form { advanced: true };
        let mut run = |events: Vec<egui::Event>| {
            let input = crate::test_ui::raw_input(egui::vec2(600.0, 800.0), events);
            ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    config_editor_ui(ui, &mut cfg, *section, None, form)
                });
            })
        };

        let first = run(vec![]);
        let pos =
            painted_text_center(&first, label).unwrap_or_else(|| panic!("{label} was not painted"));

        // Enough of the body to be unique.
        let opening: String = tip.body.chars().take(40).collect();
        let mut out = run(vec![egui::Event::PointerMoved(pos)]);
        let mut found = false;
        for _ in 0..3 {
            // The tooltip is its own area, so it can land a frame late.
            if painted_text(&out).join("\n").contains(&opening) {
                found = true;
                break;
            }
            out = run(vec![]);
        }
        assert!(found, "hovering {label:?} did not show {:?}", tip.title);
    }
}

/// Hovering a setting's *name* explains it, not just its control.
#[test]
fn hovering_a_setting_label_explains_it() {
    let ctx = crate::test_ui::ctx();
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
    // Tokenizer is an advanced row, so the whole tab has to be showing them.
    let cfg = Config {
        ui: quicksearch_core::config::UiConfig {
            show_advanced_settings: true,
            ..Default::default()
        },
        ..Config::default()
    };
    let mut w = SettingsTab::new();

    let run = |w: &mut SettingsTab, events: Vec<egui::Event>| {
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                w.ui(ui, &cfg, None);
            });
        })
    };

    let full = run(&mut w, vec![]);
    let target =
        painted_text_center(&full, "Tokenizer").expect("the Tokenizer label was not painted");

    let mut out = run(&mut w, vec![egui::Event::PointerMoved(target)]);
    for _ in 0..3 {
        let painted = painted_text(&out).join("\n");
        if painted.contains(crate::tips::TOKENIZER.title)
            && painted.contains("cut up so that it can be")
        {
            return;
        }
        out = run(&mut w, vec![]);
    }
    panic!("no tooltip appeared over the Tokenizer label");
}

#[test]
fn the_tab_renders_and_apply_reports_the_draft() {
    let ctx = crate::test_ui::ctx();
    let cfg = Config::default();
    let mut w = SettingsTab::new();
    w.stage(&cfg);
    w.draft.as_mut().unwrap().search.debounce_ms += 100;

    let run = |w: &mut SettingsTab, events: Vec<egui::Event>| {
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
        let mut out = SettingsOutput::default();
        let full = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| out = w.ui(ui, &cfg, None));
        });
        crate::test_ui::assert_no_tofu(&ctx, &full);
        (out, full)
    };

    let (untouched, full) = run(&mut w, vec![]);
    assert!(untouched.applied.is_none());
    let target = painted_text_center(&full, "Apply & Save")
        .expect("the Apply & Save button was not painted");
    let clicks = [true, false]
        .into_iter()
        .map(|pressed| egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        })
        .collect();
    let (clicked, _) = run(&mut w, clicks);
    let applied = clicked.applied.expect("the click did not report a config");
    assert_eq!(
        applied.search.debounce_ms,
        Config::default().search.debounce_ms + 100,
        "the click reported the edited draft"
    );
}

fn run_columns(
    ctx: &egui::Context,
    current: &ColumnsConfig,
    events: Vec<egui::Event>,
) -> (Option<ColumnsConfig>, egui::FullOutput) {
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 200.0), events);
    let mut picked = None;
    let full = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            picked = columns_ui(ui, current);
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &full);
    (picked, full)
}

/// The picker acts on the *live* config, so it reports a change the moment
/// a box moves, with no Apply.
#[test]
fn the_columns_block_reports_a_change_immediately() {
    let ctx = crate::test_ui::ctx();
    let current = ColumnsConfig::default();
    assert!(!current.size, "the fixture assumes Size ships off");

    let (quiet, full) = run_columns(&ctx, &current, vec![]);
    assert!(quiet.is_none(), "reported a change nobody made");
    let target = painted_text_center(&full, "Size").expect("no Size checkbox");

    let (picked, _) = run_columns(&ctx, &current, click_at(target));
    let picked = picked.expect("the click reported nothing");
    assert!(picked.size, "clicking Size did not switch it on");
    assert_eq!(
        picked,
        ColumnsConfig {
            size: true,
            ..current
        }
    );
}

/// The path is shown checked and greyed rather than left out, so "why can
/// I not remove it?" has an answer on screen.
#[test]
fn the_columns_block_offers_every_column_but_the_path() {
    let ctx = crate::test_ui::ctx();
    let (_, full) = run_columns(&ctx, &ColumnsConfig::default(), vec![]);
    let painted = painted_text(&full);
    for label in ["Name", "Path", "Content Match", "Size", "Modified", "Rank"] {
        assert!(
            painted.iter().any(|t| t == label),
            "{label} missing: {painted:?}"
        );
    }

    let target = painted_text_center(&full, "Path").expect("no Path entry");
    let (picked, _) = run_columns(&ctx, &ColumnsConfig::default(), click_at(target));
    assert!(picked.is_none(), "the path column was switched off");
}

/// `keychain_active` is passed straight through: nothing touches the OS keychain.
fn run_security(
    ctx: &egui::Context,
    current: &Config,
    events: Vec<egui::Event>,
) -> (Option<SecurityAction>, egui::FullOutput) {
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 300.0), events);
    let mut action = None;
    let full = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            action = security_ui(ui, current, false, Form { advanced: true });
        });
    });
    crate::test_ui::assert_no_tofu(ctx, &full);
    (action, full)
}

/// An unprotected index has no key, so the button is left out, not shown dead.
#[test]
fn the_key_button_appears_only_while_the_index_is_encrypted() {
    let ctx = crate::test_ui::ctx();
    let mut cfg = Config::default();
    assert!(
        !cfg.security.password_protected,
        "the fixture assumes protection ships off"
    );

    let (_, full) = run_security(&ctx, &cfg, vec![]);
    assert!(
        painted_text_center(&full, "Show database key").is_none(),
        "offered the key of an unencrypted index: {:?}",
        painted_text(&full)
    );

    cfg.security.password_protected = true;
    let (_, full) = run_security(&ctx, &cfg, vec![]);
    assert!(
        painted_text_center(&full, "Show database key").is_some(),
        "no key button while encrypted: {:?}",
        painted_text(&full)
    );
}

/// The click only asks for the flow; the reveal lives in the app.
#[test]
fn clicking_the_key_button_reports_show_key() {
    let ctx = crate::test_ui::ctx();
    let cfg = Config {
        security: quicksearch_core::config::SecurityConfig {
            password_protected: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let (quiet, full) = run_security(&ctx, &cfg, vec![]);
    assert!(quiet.is_none(), "reported an action nobody clicked");
    let target = painted_text_center(&full, "Show database key").expect("no key button");

    let (action, _) = run_security(&ctx, &cfg, click_at(target));
    assert_eq!(action, Some(SecurityAction::ShowKey));
}

/// The assertion that `app::pin_live_fields` covers the columns field.
#[test]
fn a_stale_draft_cannot_revert_the_columns() {
    let mut w = SettingsTab::new();
    let mut cfg = Config::default();
    w.stage(&cfg);

    // The header menu switches a column on while the tab is on screen.
    cfg.search.columns.size = true;
    assert!(!w.is_dirty(&cfg), "a live column change read as an edit");

    let draft = w.draft_config().expect("a draft");
    let mut applied = draft;
    crate::app::pin_live_fields(&mut applied, &cfg);
    assert!(
        applied.search.columns.size,
        "applying the stale draft reverted the column"
    );
}

/// The hint is the only place the automatic ceiling is visible, and the only
/// thing that makes the override discoverable when the cap bites.
#[test]
fn the_search_cache_hint_explains_the_automatic_value() {
    let mut cfg = Config::default();

    assert_eq!(
        search_cache_hint(&cfg, None),
        None,
        "with no file count there is no honest number to show"
    );

    // An explicit setting is not automatic, so there is nothing to explain.
    cfg.search.cache_size_mib = 64;
    assert_eq!(search_cache_hint(&cfg, Some(200_000)), None);
    cfg.search.cache_size_mib = 0;

    // Unencrypted: a fixed value, and the reason for it.
    let plain = search_cache_hint(&cfg, Some(200_000)).expect("a hint");
    assert!(plain.contains("16 MiB"), "{}", plain);
    assert!(plain.contains("unencrypted"), "{}", plain);

    // Encrypted and inside the cap: the derived value, and the file count it
    // came from.
    cfg.security.password_protected = true;
    let keyed = search_cache_hint(&cfg, Some(200_000)).expect("a hint");
    assert!(keyed.contains("32 MiB"), "{}", keyed);
    assert!(keyed.contains("200,000"), "{}", keyed);

    // Encrypted and past it: says so, and says what the index actually wants,
    // or the override cannot be found by the people who need it.
    let capped = search_cache_hint(&cfg, Some(2_000_000)).expect("a hint");
    assert!(
        capped.contains("128 MiB") && capped.contains("320 MiB"),
        "the capped hint must name both the cap and the want: {}",
        capped
    );
}

/// The whole point: with advanced off, only the everyday rows are on screen,
/// and with it on nothing has gone missing. `ROWS` is the categorisation, so
/// this fails the moment a row is added without deciding who it is for.
#[test]
fn advanced_rows_are_hidden_until_asked_for() {
    let painted_labels = |advanced: bool| -> Vec<&'static str> {
        let ctx = crate::test_ui::ctx();
        let mut cfg = Config::default();
        let form = Form { advanced };
        let mut shown = Vec::new();
        for section in [Section::Indexing, Section::Processing, Section::Search] {
            let out = ctx.run(
                crate::test_ui::raw_input(egui::vec2(600.0, 800.0), vec![]),
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        config_editor_ui(ui, &mut cfg, section, None, form)
                    });
                },
            );
            let text = painted_text(&out).join("\n");
            for (row_section, label, _, _) in ROWS {
                if *row_section == section && text.contains(*label) {
                    shown.push(*label);
                }
            }
        }
        shown
    };

    let everyday: Vec<&str> = ROWS
        .iter()
        .filter(|(_, _, _, level)| *level == Level::Everyday)
        .map(|(_, label, _, _)| *label)
        .collect();
    let all: Vec<&str> = ROWS.iter().map(|(_, label, _, _)| *label).collect();

    assert!(
        !everyday.is_empty() && everyday.len() < all.len(),
        "a split with nothing on one side is not a split: {} of {}",
        everyday.len(),
        all.len()
    );
    assert_eq!(
        painted_labels(false),
        everyday,
        "the default view must show the everyday rows and only those"
    );
    assert_eq!(
        painted_labels(true),
        all,
        "turning advanced on must bring every row back"
    );
}

/// Revealing a setting is not editing one: the toggle writes through
/// `SettingsOutput` and must never make the tab read as dirty, or looking at
/// an advanced setting would demand an Apply.
#[test]
fn showing_advanced_settings_is_not_an_unsaved_edit() {
    let mut w = SettingsTab::new();
    let mut cfg = Config::default();
    assert!(!cfg.ui.show_advanced_settings, "hidden by default");
    w.stage(&cfg);

    // The checkbox writes straight to the live config, as the app does.
    cfg.ui.show_advanced_settings = true;
    assert!(!w.is_dirty(&cfg), "revealing rows read as an edit");

    // And a draft staged while they were hidden must not put them away again.
    let mut applied = w.draft_config().expect("a draft");
    crate::app::pin_live_fields(&mut applied, &cfg);
    assert!(
        applied.ui.show_advanced_settings,
        "applying the stale draft hid the advanced settings again"
    );
}

/// The panel that tells a user how to get a shortcut that also starts
/// QuickSearch has to actually show the command they must bind — with or
/// without a one-click desktop to lean on.
#[test]
fn the_shortcut_note_names_the_command_to_bind() {
    let ctx = crate::test_ui::ctx();
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 300.0), vec![]);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::shortcut_note_for(
                ui,
                "Ctrl+Shift+F",
                crate::shortcut_setup::Desktop::Unsupported,
            )
        });
    });
    let painted = painted_text(&out).join("\n");
    assert!(
        painted.contains("--toggle"),
        "the command to bind was not shown: {painted}"
    );
    assert!(painted.contains("Copy"), "no way to copy it: {painted}");
}

/// On a desktop we can write, the note leads with the one-click button (the
/// probe is skipped by seeding the cached state, so the test stays
/// hermetic), and the manual command stays as the fallback.
#[test]
fn a_supported_desktop_gets_the_one_click_button() {
    let ctx = crate::test_ui::ctx();
    ctx.data_mut(|d| {
        d.insert_temp(
            egui::Id::new("system-shortcut-state"),
            super::SystemShortcutState {
                installed: false,
                feedback: None,
            },
        )
    });
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 300.0), vec![]);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::shortcut_note_for(ui, "Ctrl+Shift+F", crate::shortcut_setup::Desktop::Gnome)
        });
    });
    let painted = painted_text(&out).join("\n");
    assert!(
        painted.contains("Set up Ctrl+Shift+F system-wide"),
        "no one-click button: {painted}"
    );
    assert!(painted.contains("--toggle"), "the fallback vanished: {painted}");

    // Already installed: the button flips to removal.
    ctx.data_mut(|d| {
        d.insert_temp(
            egui::Id::new("system-shortcut-state"),
            super::SystemShortcutState {
                installed: true,
                feedback: None,
            },
        )
    });
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 300.0), vec![]);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::shortcut_note_for(ui, "Ctrl+Shift+F", crate::shortcut_setup::Desktop::Gnome)
        });
    });
    let painted = painted_text(&out).join("\n");
    assert!(painted.contains("Remove"), "no removal offered: {painted}");
}

/// No usable binding means nothing to write: the one-click flow bows out
/// even on a supported desktop.
#[test]
fn no_binding_means_no_one_click_button() {
    let ctx = crate::test_ui::ctx();
    let input = crate::test_ui::raw_input(egui::vec2(700.0, 300.0), vec![]);
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::shortcut_note_for(ui, "", crate::shortcut_setup::Desktop::Gnome)
        });
    });
    let painted = painted_text(&out).join("\n");
    assert!(
        !painted.contains("system-wide"),
        "offered to bind nothing: {painted}"
    );
    assert!(painted.contains("--toggle"), "the manual flow vanished: {painted}");
}
