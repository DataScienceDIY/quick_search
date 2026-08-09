use super::*;

#[test]
fn a_stale_draft_cannot_revert_the_indexing_mode_or_security() {
    // The draft as it was when the editor last synced: automatic
    // indexing, no password — plus one real edit the user staged.
    let mut draft = Config::default();
    draft.indexing.auto_index = true;
    draft.indexing.reindex_interval_minutes = 60;

    // Since then: Stop was clicked and protection was enabled.
    let mut live = Config::default();
    live.indexing.auto_index = false;
    live.security = SecurityConfig {
        password_protected: true,
        salt: Some("ab".repeat(16)),
        use_keychain: true,
    };

    pin_live_fields(&mut draft, &live);
    assert!(
        !draft.indexing.auto_index,
        "applying the draft must not restart automatic indexing"
    );
    assert_eq!(draft.security, live.security);
    assert_eq!(
        draft.indexing.reindex_interval_minutes, 60,
        "the staged edit itself still applies"
    );
}

/// The guard decision table for leaving a tab, however it is asked for.
#[test]
fn leaving_a_dirty_manage_tab_is_guarded_however_it_is_asked_for() {
    assert!(switch_needs_guard(Tab::Manage, true, false));
    assert!(
        !switch_needs_guard(Tab::Manage, false, false),
        "a clean editor has nothing to ask about"
    );
    assert!(
        !switch_needs_guard(Tab::Manage, true, true),
        "one held navigation at a time"
    );
    for tab in [Tab::Search, Tab::Duplicates, Tab::Logs, Tab::Help] {
        assert!(
            !switch_needs_guard(tab, true, false),
            "{tab:?} holds no unapplied edits of its own"
        );
    }
}

#[test]
fn guard_source_orders_quit_prompts_options_first() {
    use super::NavIntent::*;
    let tab = SwitchTab(Tab::Search);

    assert_eq!(guard_source(tab, true, true), Some(UnsavedSource::Manage));
    assert_eq!(guard_source(tab, true, false), Some(UnsavedSource::Manage));
    assert_eq!(
        guard_source(tab, false, true),
        None,
        "options guard its own close"
    );
    assert_eq!(guard_source(tab, false, false), None);

    assert_eq!(
        guard_source(CloseOptions, true, true),
        Some(UnsavedSource::Options)
    );
    assert_eq!(
        guard_source(CloseOptions, false, true),
        Some(UnsavedSource::Options)
    );
    assert_eq!(
        guard_source(CloseOptions, true, false),
        None,
        "manage guards tab switches"
    );
    assert_eq!(guard_source(CloseOptions, false, false), None);

    assert_eq!(guard_source(Quit, true, true), Some(UnsavedSource::Options));
    assert_eq!(
        guard_source(Quit, false, true),
        Some(UnsavedSource::Options)
    );
    assert_eq!(guard_source(Quit, true, false), Some(UnsavedSource::Manage));
    assert_eq!(guard_source(Quit, false, false), None);
}

/// Only a Quit during a running reconcile warns; a tab switch does not end
/// the pass.
#[test]
fn only_quitting_during_a_reconcile_warns() {
    use super::NavIntent::*;
    assert!(quit_needs_reconcile_warning(Quit, true));
    assert!(!quit_needs_reconcile_warning(Quit, false));
    assert!(!quit_needs_reconcile_warning(SwitchTab(Tab::Search), true));
    assert!(!quit_needs_reconcile_warning(CloseOptions, true));
}

/// The two values the Options window writes, plus hand-edited variants.
#[test]
fn only_light_is_light() {
    assert_eq!(theme_for("light"), egui::Theme::Light);
    assert_eq!(theme_for("dark"), egui::Theme::Dark);

    // Spelled the user's way, not the config's.
    assert_eq!(theme_for("  LIGHT  "), egui::Theme::Light);
    assert_eq!(theme_for("Dark"), egui::Theme::Dark);

    // A typo costs the preference, not the config file.
    for nonsense in ["", "   ", "lite", "system", "auto", "true"] {
        assert_eq!(
            theme_for(nonsense),
            egui::Theme::Dark,
            "{:?} should be dark",
            nonsense
        );
    }
}
