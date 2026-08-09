use super::*;

// All headless-safe: `keychain_active` only probes the OS keychain when
// `use_keychain` is set, and no test here sets it.

#[test]
fn a_fresh_draft_is_not_dirty() {
    let mut w = OptionsWindow::new();
    let cfg = Config::default();
    assert!(!w.is_dirty(&cfg), "no draft at all");
    w.open_with(&cfg);
    assert!(!w.is_dirty(&cfg));
}

#[test]
fn an_edited_draft_is_dirty_until_discarded() {
    let mut w = OptionsWindow::new();
    let cfg = Config::default();
    w.open_with(&cfg);
    w.draft.as_mut().unwrap().search.debounce_ms += 100;
    assert!(w.is_dirty(&cfg));
    w.close_discard();
    assert!(!w.open);
    assert!(!w.is_dirty(&cfg), "the draft is gone");
}

/// The Security block and the mode buttons act on the live config while
/// the window sits open; the stale copies in the draft are not edits.
#[test]
fn live_security_and_mode_changes_are_not_dirty() {
    let mut w = OptionsWindow::new();
    let mut cfg = Config::default();
    w.open_with(&cfg);
    cfg.security.use_keychain = !cfg.security.use_keychain;
    cfg.indexing.auto_index = !cfg.indexing.auto_index;
    assert!(!w.is_dirty(&cfg));
}

#[test]
fn a_dirty_close_is_held_and_a_clean_one_drops_the_draft() {
    let mut w = OptionsWindow::new();
    let cfg = Config::default();
    w.open_with(&cfg);
    w.draft.as_mut().unwrap().search.debounce_ms += 100;

    assert!(
        w.intercept_close(false, &cfg),
        "dirty close raises the guard"
    );
    assert!(w.open, "the window is held open until the user decides");
    assert!(w.draft.is_some(), "the draft survives");

    assert!(!w.intercept_close(true, &cfg), "still open: nothing to do");

    w.draft = Some(cfg.clone());
    assert!(!w.intercept_close(false, &cfg), "a clean close just closes");
    assert!(!w.open);
    assert!(w.draft.is_none());
}

use crate::test_ui::{click_at, painted_text, painted_text_center};

/// One frame of the shortcut control on its own, outside the window's
/// scroll area so it is never below the fold.
fn run_hotkey_edit(
    ctx: &egui::Context,
    setting: &mut String,
    capturing: &mut bool,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), events);
    ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            hotkey_edit(ui, setting, capturing);
        });
    })
}

/// One frame of the color scheme control on its own, for the same reason
/// as [`run_hotkey_edit`].
fn run_color_scheme_edit(
    ctx: &egui::Context,
    setting: &mut String,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), events);
    ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            color_scheme_edit(ui, setting);
        });
    })
}

/// The dropdown says which scheme is in force and writes the one that is
/// picked; what it shows and what it stores are not the same string.
#[test]
fn the_color_scheme_box_shows_and_sets_the_scheme() {
    let ctx = egui::Context::default();
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

/// A hand-edited config can hold anything. The box reports what the app
/// will actually do with it rather than echoing it back.
#[test]
fn an_unknown_scheme_reads_as_dark() {
    assert_eq!(scheme_label("dark"), "Dark");
    assert_eq!(scheme_label("light"), "Light");
    // Human-spelled values are still honoured.
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

/// Click the button, press a combination, and the setting is what was
/// pressed.
#[test]
fn the_shortcut_button_binds_what_was_pressed() {
    let ctx = egui::Context::default();
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

/// Escape backs out, and a press that could not be a shortcut is waited
/// through rather than treated as one.
#[test]
fn capture_ignores_what_it_cannot_bind_and_escape_cancels() {
    let ctx = egui::Context::default();
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
    let ctx = egui::Context::default();
    let mut setting = "Ctrl+Shift+F".to_string();
    let mut capturing = false;

    let first = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    let clear = painted_text_center(&first, "Clear").expect("Clear was not painted");
    run_hotkey_edit(&ctx, &mut setting, &mut capturing, click_at(clear));
    assert_eq!(setting, "");

    // With no shortcut set there is nothing to clear, and the button
    // says what the state is rather than going blank.
    let empty = run_hotkey_edit(&ctx, &mut setting, &mut capturing, vec![]);
    assert!(painted_text(&empty).iter().any(|t| t == "None"));
}

/// The draft is what the button shows, but the registration is what the
/// app is actually holding, and until Apply they can disagree.
#[test]
fn an_unapplied_shortcut_says_it_is_not_in_force_yet() {
    let ctx = egui::Context::default();
    let run = |draft: &str, live: &str| {
        let input = crate::test_ui::raw_input(egui::vec2(600.0, 200.0), vec![]);
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| hotkey_note(ui, draft, live));
        });
        painted_text(&out).join("\n")
    };
    assert!(run("Ctrl+Alt+K", "Ctrl+Shift+F").contains("Apply and Save"));
    // Matching, and nothing registered in a test process: nothing to say.
    assert_eq!(run("Ctrl+Shift+F", "Ctrl+Shift+F"), "");
}

/// Every row of every section, with the tip it must show; this table is
/// what makes a row with the *wrong* tooltip impossible.
const ROWS: &[(Section, &str, &tips::Tip)] = &[
    (
        Section::Indexing,
        "Full reindex every",
        &tips::REINDEX_INTERVAL,
    ),
    (Section::Indexing, "Follow symlinks", &tips::FOLLOW_SYMLINKS),
    (
        Section::Indexing,
        "Include hidden files",
        &tips::INCLUDE_HIDDEN,
    ),
    (Section::Processing, "Tokenizer", &tips::TOKENIZER),
    (
        Section::Processing,
        "Hash sample size (bytes)",
        &tips::HASH_LENGTH,
    ),
    (
        Section::Processing,
        "Max stored text (bytes)",
        &tips::MAX_STORED_TEXT,
    ),
    (
        Section::Processing,
        "Max text file size (bytes)",
        &tips::MAX_TEXT_FILE_SIZE,
    ),
    (Section::Processing, "Batch size", &tips::BATCH_SIZE),
    (
        Section::Processing,
        "Max WAL size (bytes)",
        &tips::MAX_WAL_SIZE,
    ),
    (
        Section::Processing,
        "Store text for snippets",
        &tips::STORE_TEXT,
    ),
    (
        Section::Search,
        "Fuzzy search ON by default",
        &tips::FUZZY_DEFAULT,
    ),
    (Section::Search, "Fuzzy edit distance", &tips::FUZZY_EDITS),
    (Section::Search, "Display limit", &tips::DISPLAY_LIMIT),
    (
        Section::Search,
        "Stream batch size",
        &tips::RESULTS_PER_PAGE,
    ),
    (Section::Search, "Debounce (ms)", &tips::DEBOUNCE),
];

/// Hovering a row's name paints that row's own explanation. Rendered
/// without the window's scroll area so nothing sits below the fold.
#[test]
fn every_row_shows_its_own_tip() {
    for (section, label, tip) in ROWS {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let mut cfg = Config::default();
        let mut run = |events: Vec<egui::Event>| {
            let input = crate::test_ui::raw_input(egui::vec2(600.0, 800.0), events);
            ctx.run(input, |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| config_editor_ui(ui, &mut cfg, *section));
            })
        };

        let first = run(vec![]);
        let pos =
            painted_text_center(&first, label).unwrap_or_else(|| panic!("{label} was not painted"));

        // Enough of the body to be unique, and short enough to survive
        // an edit to the sentence it starts.
        let opening: String = tip.body.chars().take(40).collect();
        let mut out = run(vec![egui::Event::PointerMoved(pos)]);
        let mut found = false;
        for _ in 0..3 {
            // The tooltip is an area of its own, so it can land a frame
            // late.
            if painted_text(&out).join("\n").contains(&opening) {
                found = true;
                break;
            }
            out = run(vec![]);
        }
        assert!(found, "hovering {label:?} did not show {:?}", tip.title);
    }
}

/// Hovering a setting's *name* explains it, not just its control. The
/// wiring is under test, so tooltip timing is turned off.
#[test]
fn hovering_a_setting_label_explains_it() {
    let ctx = egui::Context::default();
    ctx.style_mut(|s| {
        s.interaction.tooltip_delay = 0.0;
        s.interaction.show_tooltips_only_when_still = false;
    });
    let cfg = Config::default();
    let mut w = OptionsWindow::new();
    w.open_with(&cfg);

    let run = |w: &mut OptionsWindow, events: Vec<egui::Event>| {
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
        ctx.run(input, |ctx| {
            w.ui(ctx, &cfg);
        })
    };

    // The window spends its first frames sizing itself and painting
    // nothing; run until the label is on screen.
    let mut target = None;
    for _ in 0..5 {
        let full = run(&mut w, vec![]);
        target = painted_text_center(&full, "Tokenizer");
        if target.is_some() {
            break;
        }
    }
    let target = target.expect("the Tokenizer label was not painted");

    // The tooltip is an area of its own, so it can land a frame late.
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

/// One real frame of the window in a headless context: it renders, and
/// the Apply & Save click comes back out as `applied`.
#[test]
fn the_window_renders_and_apply_reports_the_draft() {
    let ctx = egui::Context::default();
    let cfg = Config::default();
    let mut w = OptionsWindow::new();
    w.open_with(&cfg);
    w.draft.as_mut().unwrap().search.debounce_ms += 100;

    let run = |w: &mut OptionsWindow, events: Vec<egui::Event>| {
        let input = crate::test_ui::raw_input(egui::vec2(1000.0, 900.0), events);
        let mut out = OptionsOutput::default();
        let full = ctx.run(input, |ctx| out = w.ui(ctx, &cfg));
        (out, full)
    };

    // A new egui window spends its first frames in sizing passes that
    // suppress painting; run untouched frames until the settled button
    // is actually on screen.
    let mut target = None;
    for _ in 0..5 {
        let (untouched, full) = run(&mut w, vec![]);
        assert!(untouched.applied.is_none());
        assert!(!untouched.close_requested);
        target = painted_text_center(&full, "Apply & Save");
        if target.is_some() {
            break;
        }
    }
    let target = target.expect("the Apply & Save button was not painted");
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
