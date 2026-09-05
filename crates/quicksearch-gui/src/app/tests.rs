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
/// Both draft-backed tabs guard their own departure, and neither answers for
/// the other.
#[test]
fn leaving_a_dirty_editor_tab_is_guarded_however_it_is_asked_for() {
    // (the tab, whether *its* editor is the dirty one in the pair below)
    for (tab, manage_dirty, settings_dirty) in
        [(Tab::Manage, true, false), (Tab::Settings, false, true)]
    {
        assert!(
            switch_needs_guard(tab, manage_dirty, settings_dirty, false),
            "{tab:?} must guard its own unapplied edits"
        );
        assert!(
            !switch_needs_guard(tab, false, false, false),
            "{tab:?}: a clean editor has nothing to ask about"
        );
        assert!(
            !switch_needs_guard(tab, manage_dirty, settings_dirty, true),
            "{tab:?}: one held navigation at a time"
        );
        assert!(
            !switch_needs_guard(tab, !manage_dirty, !settings_dirty, false),
            "{tab:?} must not answer for the other editor"
        );
    }
    for tab in [Tab::Search, Tab::Duplicates, Tab::Logs, Tab::Help] {
        assert!(
            !switch_needs_guard(tab, true, true, false),
            "{tab:?} holds no unapplied edits of its own"
        );
    }
}

#[test]
fn guard_source_orders_quit_prompts_settings_first() {
    use super::NavIntent::*;
    let leave = SwitchTab(Tab::Search);

    // A switch asks about the tab being left, and only about that one.
    assert_eq!(
        guard_source(leave, Tab::Manage, true, true),
        Some(UnsavedSource::Manage)
    );
    assert_eq!(
        guard_source(leave, Tab::Manage, true, false),
        Some(UnsavedSource::Manage)
    );
    assert_eq!(
        guard_source(leave, Tab::Manage, false, true),
        None,
        "the Settings draft is not what leaving Manage disturbs"
    );
    assert_eq!(
        guard_source(leave, Tab::Settings, true, true),
        Some(UnsavedSource::Settings)
    );
    assert_eq!(
        guard_source(leave, Tab::Settings, false, true),
        Some(UnsavedSource::Settings)
    );
    assert_eq!(
        guard_source(leave, Tab::Settings, true, false),
        None,
        "the Manage draft is not what leaving Settings disturbs"
    );
    for tab in [Tab::Search, Tab::Duplicates, Tab::Logs, Tab::Help] {
        assert_eq!(
            guard_source(leave, tab, true, true),
            None,
            "{tab:?} stages nothing, so leaving it asks nothing"
        );
    }

    // Quit asks about both, Settings first.
    for from in [Tab::Search, Tab::Manage, Tab::Settings] {
        assert_eq!(
            guard_source(Quit, from, true, true),
            Some(UnsavedSource::Settings),
            "{from:?}: quitting asks about Settings before Manage"
        );
        assert_eq!(
            guard_source(Quit, from, false, true),
            Some(UnsavedSource::Settings)
        );
        assert_eq!(
            guard_source(Quit, from, true, false),
            Some(UnsavedSource::Manage)
        );
        assert_eq!(guard_source(Quit, from, false, false), None);
    }
}

/// Only a Quit during a running reconcile warns; a tab switch does not end
/// the pass.
#[test]
fn only_quitting_during_a_reconcile_warns() {
    use super::NavIntent::*;
    assert!(quit_needs_reconcile_warning(Quit, true));
    assert!(!quit_needs_reconcile_warning(Quit, false));
    assert!(!quit_needs_reconcile_warning(SwitchTab(Tab::Search), true));
    assert!(!quit_needs_reconcile_warning(
        SwitchTab(Tab::Settings),
        true
    ));
}

/// Every tab is placed on exactly one side of the guard, so a tab added
/// later cannot quietly inherit "stages nothing".
#[test]
fn only_the_two_draft_backed_tabs_have_an_editor() {
    assert_eq!(tab_editor(Tab::Manage), Some(UnsavedSource::Manage));
    assert_eq!(tab_editor(Tab::Settings), Some(UnsavedSource::Settings));
    for tab in [Tab::Search, Tab::Duplicates, Tab::Logs, Tab::Help] {
        assert_eq!(tab_editor(tab), None, "{tab:?} saves as it goes");
    }
}

/// The scan reads every entry in the hash index, so arriving at the tab must
/// not re-run it over a listing that is already there.
#[test]
fn only_an_empty_duplicates_tab_scans_on_arrival() {
    assert!(arrival_starts_dup_scan(
        Tab::Duplicates,
        &DupState::NotLoaded
    ));
    for held in [
        DupState::Loading,
        DupState::Loaded(crate::duplicates_tab::LoadedGroups::new(Vec::new())),
        DupState::Error("boom".into()),
    ] {
        assert!(
            !arrival_starts_dup_scan(Tab::Duplicates, &held),
            "arriving must not disturb a scan already run or running"
        );
    }
    // No other tab scans, whatever the Duplicates tab is holding.
    for tab in [
        Tab::Search,
        Tab::Manage,
        Tab::Logs,
        Tab::Help,
        Tab::Settings,
    ] {
        assert!(!arrival_starts_dup_scan(tab, &DupState::NotLoaded));
    }
}

/// A listing describes the index at the moment it was scanned. Every way that
/// index can change out from under it has to reach the tab.
#[test]
fn an_index_that_changed_invalidates_what_the_tab_holds() {
    let loaded = || DupState::Loaded(crate::duplicates_tab::LoadedGroups::new(Vec::new()));

    // On screen: the user is looking at it, so redo it now.
    assert_eq!(
        dup_invalidation(Tab::Duplicates, &loaded()),
        DupInvalidation::Rescan
    );
    assert_eq!(
        dup_invalidation(Tab::Duplicates, &DupState::Error("boom".into())),
        DupInvalidation::Rescan,
        "a failed scan is worth retrying against the new index"
    );

    // Out of sight: drop it, and let the next visit pay for the rescan.
    assert_eq!(
        dup_invalidation(Tab::Search, &loaded()),
        DupInvalidation::Drop
    );

    // Nothing to invalidate.
    assert_eq!(
        dup_invalidation(Tab::Search, &DupState::NotLoaded),
        DupInvalidation::Nothing
    );

    // In flight: its answer already predates the change, but racing a second
    // scan against it would read the index twice over.
    for tab in [Tab::Duplicates, Tab::Search] {
        assert_eq!(
            dup_invalidation(tab, &DupState::Loading),
            DupInvalidation::WhenItLands
        );
    }
}

/// The two values the Settings tab's color-scheme box writes, plus
/// hand-edited variants.
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

/// In automatic mode the rebuild is already under way behind the modal;
/// commanding another one would delete its progress and start over.
#[test]
fn the_stale_index_prompt_never_restarts_a_rebuild_already_running() {
    use super::modals::stale_prompt_should_command;

    assert!(!stale_prompt_should_command(IndexMode::Auto, true));
    // Auto before the first tick: the coordinator still gets there on its own.
    assert!(!stale_prompt_should_command(IndexMode::Auto, false));
    assert!(!stale_prompt_should_command(IndexMode::ManualRunning, true));

    // Manual and stopped: nothing else would ever start it.
    assert!(stale_prompt_should_command(IndexMode::ManualStopped, false));
}
