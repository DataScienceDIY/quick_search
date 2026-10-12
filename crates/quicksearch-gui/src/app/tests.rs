use super::*;

#[test]
fn a_stale_draft_cannot_revert_the_indexing_mode_or_security() {
    // The draft as it was when the editor last synced: automatic
    // indexing, no password, plus one real edit the user staged.
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

/// The watch-cap OK and the tour's dismissal are written straight to the
/// config; an older draft applied afterwards must not bring either back.
#[test]
fn a_stale_draft_cannot_revert_acknowledgements() {
    // Staged on a first start, before the warning or the tour was answered,
    // plus one real edit.
    let mut draft = Config::default();
    draft.paths.indexing_paths = vec!["/data".to_string(), "/home".to_string()];
    assert_eq!(draft.ui.tutorial_seen, Some(false));
    draft.search.debounce_ms += 100;

    // Since then: OK on the warning, and the tour dismissed.
    let mut live = draft.clone();
    live.search.debounce_ms = Config::default().search.debounce_ms;
    live.ui.watch_cap_warned_roots = live.paths.indexing_paths.clone();
    live.ui.tutorial_seen = Some(true);

    let mut applied = draft.clone();
    pin_live_fields(&mut applied, &live);
    assert_eq!(
        applied.ui.watch_cap_warned_roots, live.ui.watch_cap_warned_roots,
        "applying the draft would warn about the same roots again"
    );
    assert_eq!(
        applied.ui.tutorial_seen,
        Some(true),
        "applying the draft would bring the tour back"
    );
    assert_eq!(applied.search.debounce_ms, draft.search.debounce_ms);

    // Live wins both ways: an acknowledgement the live config no longer
    // holds is not restored from the draft, and an upgrade's `None` stays.
    let mut stale = live.clone();
    live.ui.watch_cap_warned_roots.clear();
    live.ui.tutorial_seen = None;
    pin_live_fields(&mut stale, &live);
    assert!(stale.ui.watch_cap_warned_roots.is_empty());
    assert_eq!(stale.ui.tutorial_seen, None);
}

/// A navigation on hold wins: a later request, from a click, the tour or the
/// search shortcut, leaves the tab and the held navigation as they are, and
/// does not even ask the guard.
#[test]
fn a_held_navigation_drops_a_later_tab_request() {
    assert_eq!(
        route_tab_request(true, || panic!("the guard was asked")),
        TabRoute::Drop
    );
    // With nothing held, the guard decides.
    assert_eq!(route_tab_request(false, || true), TabRoute::Hold);
    assert_eq!(route_tab_request(false, || false), TabRoute::Switch);
}

/// The one decision both `request_tab` and the prompt make. Both
/// draft-backed tabs guard their own departure, and neither answers for the
/// other.
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
        DupState::Loaded(crate::duplicates_tab::LoadedGroups::new(Vec::new(), None)),
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
    let loaded = || DupState::Loaded(crate::duplicates_tab::LoadedGroups::new(Vec::new(), None));

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
    for tab in [
        Tab::Search,
        Tab::Manage,
        Tab::Logs,
        Tab::Help,
        Tab::Settings,
    ] {
        assert_eq!(
            dup_invalidation(tab, &DupState::NotLoaded),
            DupInvalidation::Nothing,
            "{tab:?}"
        );
    }

    // On screen but empty: a rebuild, clear or security change dropped it
    // without a scan (`drop_duplicates`), and the end of the run refills it.
    assert_eq!(
        dup_invalidation(Tab::Duplicates, &DupState::NotLoaded),
        DupInvalidation::Rescan
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

#[test]
fn the_holder_s_pid_is_named_when_known() {
    assert_eq!(held_by(Some(4242)), " (process 4242)");
    assert_eq!(held_by(None), "");
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
