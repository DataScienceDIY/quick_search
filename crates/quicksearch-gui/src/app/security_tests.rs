use super::*;

use crate::test_ui::{click_at, painted_text, painted_text_center, two_pass};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

/// Two 64-digit keys that differ, in the lowercase form [`IndexKey::to_hex`]
/// produces.
const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// The reveal modal and the buttons clicked on the frame that painted it.
fn frame(
    ctx: &egui::Context,
    display: &str,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, (bool, bool)) {
    let mut buttons = (false, false);
    let out = two_pass(ctx, SCREEN, events, |ctx| {
        buttons = reveal_key_modal(ctx, display);
    });
    (out, buttons)
}

/// The confirmation half, the same way.
fn confirm_frame(
    ctx: &egui::Context,
    pw: &mut String,
    wrong: bool,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, (bool, bool)) {
    let mut buttons = (false, false);
    let out = two_pass(ctx, SCREEN, events, |ctx| {
        buttons = confirm_key_modal(ctx, pw, wrong);
    });
    (out, buttons)
}

/// An empty field cannot submit: there is nothing to derive from, and a
/// dead button says so more clearly than a rejected attempt would.
#[test]
fn the_confirmation_will_not_submit_an_empty_password() {
    let ctx = crate::test_ui::ctx();
    let mut pw = String::new();
    let (out, buttons) = confirm_frame(&ctx, &mut pw, false, Vec::new());
    assert_eq!(buttons, (false, false));

    let pos = painted_text_center(&out, "Show key").expect("no submit button painted");
    let (_, buttons) = confirm_frame(&ctx, &mut pw, false, click_at(pos));
    assert!(!buttons.0, "an empty password was submitted");
}

#[test]
fn the_confirmation_submits_a_typed_password_and_cancels_on_request() {
    let ctx = crate::test_ui::ctx();
    let mut pw = "hunter2".to_string();
    let (out, _) = confirm_frame(&ctx, &mut pw, false, Vec::new());
    assert!(
        !painted_text(&out).contains(&pw),
        "the password was painted in the clear: {:?}",
        painted_text(&out)
    );

    let submit = painted_text_center(&out, "Show key").expect("no submit button painted");
    let (_, buttons) = confirm_frame(&ctx, &mut pw, false, click_at(submit));
    assert_eq!(buttons, (true, false));

    let cancel = painted_text_center(&out, "Cancel").expect("no cancel button painted");
    let (_, buttons) = confirm_frame(&ctx, &mut pw, false, click_at(cancel));
    assert_eq!(buttons, (false, true));
}

/// A retry has to say why it is asking again, or it reads as the dialog
/// having ignored the first attempt.
#[test]
fn a_retry_says_the_password_was_wrong() {
    let ctx = crate::test_ui::ctx();
    let mut pw = String::new();
    let (quiet, _) = confirm_frame(&ctx, &mut pw, false, Vec::new());
    assert!(
        !painted_text(&quiet)
            .iter()
            .any(|t| t.contains("not correct")),
        "the first attempt was called wrong before it was made"
    );

    let (out, _) = confirm_frame(&ctx, &mut pw, true, Vec::new());
    assert!(
        painted_text(&out)
            .iter()
            .any(|t| t.contains("That password is not correct")),
        "{:?}",
        painted_text(&out)
    );
}

/// The right password derives the installed key, and the key is shown in the
/// `0x` form other SQLCipher tools take.
#[test]
fn the_matching_password_reveals_the_installed_key() {
    assert_eq!(reveal_display(KEY, KEY), Some(format!("0x{KEY}")));
}

/// A wrong password derives some other key. Nothing about the real one may
/// leak from the attempt, so the caller gets no display string at all.
#[test]
fn a_password_that_derives_another_key_reveals_nothing() {
    assert_eq!(reveal_display(KEY, OTHER), None);
    assert_eq!(reveal_display(KEY, ""), None);
    // A prefix must not pass: the whole key is compared, not the start of it.
    assert_eq!(reveal_display(KEY, &KEY[..62]), None);
    // Both sides come from `to_hex`, which is always lowercase, so an
    // uppercase spelling is a mismatch rather than a value to normalise.
    assert_eq!(reveal_display(KEY, &KEY.to_uppercase()), None);
}

#[test]
fn the_reveal_shows_the_key_and_what_holding_it_means() {
    let ctx = crate::test_ui::ctx();
    let display = format!("0x{KEY}");
    let painted = painted_text(&frame(&ctx, &display, Vec::new()).0);

    assert!(
        painted.contains(&display),
        "the key itself is not on screen: {painted:?}"
    );
    assert!(
        painted
            .iter()
            .any(|t| t.contains("read the index without the password")),
        "no warning about what the key is: {painted:?}"
    );
    assert!(painted.contains(&"Copy".to_string()), "{painted:?}");
    assert!(painted.contains(&"Close".to_string()), "{painted:?}");
}

/// The key alone opens nothing: a tool left on SQLCipher's defaults decrypts
/// this file to noise and calls the key wrong. The screen has to say both
/// halves of the layout, as the values the other tool needs typed in.
#[test]
fn the_reveal_shows_the_layout_the_index_was_built_under() {
    use quicksearch_core::db::schema::{HMAC_MODE, PAGE_SIZE};

    let ctx = crate::test_ui::ctx();
    let painted = painted_text(&frame(&ctx, &format!("0x{KEY}"), Vec::new()).0);

    assert!(
        painted.contains(&format!("Page size: {PAGE_SIZE}")),
        "the page size is not on screen: {painted:?}"
    );
    assert!(
        painted.contains(&format!("Page HMAC: {}", HMAC_MODE.label())),
        "the page authenticator is not on screen: {painted:?}"
    );
    assert!(
        painted.iter().any(|t| t.contains("set both of the above")),
        "nothing says the layout has to be entered too: {painted:?}"
    );
    assert_ne!(
        PAGE_SIZE, SQLCIPHER_DEFAULT_PAGE_SIZE,
        "the advice only makes sense while the index is off the default"
    );
}

/// A tool cannot be told "HMAC off" in prose; it needs the pragma. The hint
/// carries it whenever the index is off SQLCipher's default authenticator, and
/// omits it when there is nothing to say.
#[test]
fn the_reveal_spells_out_the_hmac_pragma_when_there_is_one() {
    use quicksearch_core::db::schema::{HmacMode, HMAC_MODE, PAGE_SIZE};

    let ctx = crate::test_ui::ctx();
    let painted = painted_text(&frame(&ctx, &format!("0x{KEY}"), Vec::new()).0);
    let hint = painted
        .iter()
        .find(|t| t.contains("set both of the above"))
        .unwrap_or_else(|| panic!("no layout hint painted: {painted:?}"));

    match HMAC_MODE {
        HmacMode::Sha512 => assert!(
            !hint.contains("cipher_use_hmac") && !hint.contains("cipher_hmac_algorithm"),
            "the index is on SQLCipher's own default; there is no pragma to give: {hint}"
        ),
        HmacMode::Off => assert!(
            hint.contains("PRAGMA cipher_use_hmac = OFF;"),
            "the pragma that turns the authenticator off is missing: {hint}"
        ),
        HmacMode::Sha256 => assert!(
            hint.contains("PRAGMA cipher_hmac_algorithm = HMAC_SHA256;"),
            "the pragma that selects the authenticator is missing: {hint}"
        ),
    }
    assert!(
        hint.contains(&format!("PRAGMA cipher_page_size = {};", PAGE_SIZE)),
        "the page-size pragma is missing: {hint}"
    );
}

#[test]
fn both_of_the_reveal_buttons_report_their_click() {
    let display = format!("0x{KEY}");
    for (label, expected) in [("Copy", (true, false)), ("Close", (false, true))] {
        let ctx = crate::test_ui::ctx();
        let (out, _) = frame(&ctx, &display, Vec::new());
        let pos =
            painted_text_center(&out, label).unwrap_or_else(|| panic!("no {label} button painted"));
        let (_, buttons) = frame(&ctx, &display, click_at(pos));
        assert_eq!(
            buttons, expected,
            "clicking {label} reported the wrong pair"
        );
    }
}

/// The displayed string is the whole key and nothing else: a truncated or
/// annotated form would be pasted into other tools and fail there.
#[test]
fn the_display_form_is_the_prefix_and_the_whole_key() {
    let display = reveal_display(KEY, KEY).expect("a match reveals");
    assert_eq!(display.len(), 66);
    assert!(display.starts_with("0x"));
    assert!(display[2..].bytes().all(|b| b.is_ascii_hexdigit()));
}

/// The Deleting prompt waits only while the coordinator has said nothing;
/// any answer, a vanished coordinator included, ends the wait.
#[test]
fn a_delete_reply_ends_the_wait_and_its_absence_does_not() {
    assert_eq!(delete_outcome(Err(mpsc::TryRecvError::Empty)), None);
    assert_eq!(delete_outcome(Ok(Ok(()))), Some(Ok(())));
    assert_eq!(
        delete_outcome(Ok(Err("in use".to_string()))),
        Some(Err("in use".to_string()))
    );
    // Nothing confirms a delete from a coordinator that is gone.
    assert_eq!(
        delete_outcome(Err(mpsc::TryRecvError::Disconnected)),
        Some(Err("the indexer stopped".to_string()))
    );
}

/// Enabling leaves plaintext behind and a change leaves pages the old
/// password opens, so both warn; disabling leaves nothing secret.
#[test]
fn the_residue_notice_covers_enabling_and_changing_not_disabling() {
    let (title, enable) = confirm_text(true, false);
    assert_eq!(title, "Enable password protection?");
    assert!(
        enable.is_some_and(|t| t.contains("unencrypted index")),
        "{enable:?}"
    );
    let (title, change) = confirm_text(true, true);
    assert_eq!(title, "Change password?");
    assert!(
        change.is_some_and(|t| t.contains("old password")),
        "{change:?}"
    );
    for protected in [false, true] {
        assert_eq!(
            confirm_text(false, protected),
            ("Disable password protection?", None)
        );
    }
}

#[test]
fn the_wait_window_paints_its_label() {
    for label in ["Deriving key…", "Deleting the index…"] {
        let ctx = crate::test_ui::ctx();
        let out = two_pass(&ctx, SCREEN, Vec::new(), |ctx| deriving_window(ctx, label));
        let painted = painted_text(&out);
        assert!(painted.contains(&label.to_string()), "{painted:?}");
    }
}

/// One call [`SecurityEffects`] made, with what it was given.
#[derive(Debug, PartialEq)]
enum Call {
    Save(SecurityConfig),
    /// Whether a key was remembered until now, and what to hold.
    Keychain(bool, Option<String>),
    Install(Option<String>),
    Rebuild,
}

/// Records every effect, and fails the ones it is told to.
#[derive(Default)]
struct Recorder {
    calls: Vec<Call>,
    fail_save: bool,
    fail_keychain: bool,
}

impl SecurityEffects for Recorder {
    fn save_config(&mut self, security: SecurityConfig) -> Result<(), String> {
        self.calls.push(Call::Save(security));
        if self.fail_save {
            return Err("disk full".to_string());
        }
        Ok(())
    }

    fn sync_keychain(&mut self, was_remembered: bool, key_hex: Option<&str>) -> Result<(), String> {
        self.calls
            .push(Call::Keychain(was_remembered, key_hex.map(str::to_string)));
        if self.fail_keychain {
            return Err("keychain locked".to_string());
        }
        Ok(())
    }

    fn install_key(&mut self, key: Option<IndexKey>) {
        self.calls.push(Call::Install(key.map(|key| key.to_hex())));
    }

    fn rebuild(&mut self) {
        self.calls.push(Call::Rebuild);
    }
}

fn new_key() -> IndexKey {
    IndexKey::from_hex(KEY).unwrap()
}

fn protected(remember: bool) -> SecurityConfig {
    SecurityConfig {
        password_protected: true,
        salt: Some("ab".repeat(16)),
        use_keychain: remember,
    }
}

/// Config first, because the salt lives only there; the rebuild last, so the
/// fresh index is created under the key just installed.
#[test]
fn a_security_change_commits_in_order() {
    let mut fx = Recorder::default();
    apply_security_change(&mut fx, false, protected(true), Some(new_key())).unwrap();
    assert_eq!(
        fx.calls,
        vec![
            Call::Save(protected(true)),
            Call::Keychain(false, Some(KEY.to_string())),
            Call::Install(Some(KEY.to_string())),
            Call::Rebuild,
        ]
    );
}

/// The old index is already gone by now, so a rebuild is owed either way;
/// it runs under the settings and key still in force.
#[test]
fn a_failed_config_save_keeps_the_old_key_and_rebuilds_under_it() {
    let mut fx = Recorder {
        fail_save: true,
        ..Recorder::default()
    };
    let err = apply_security_change(&mut fx, false, protected(true), Some(new_key())).unwrap_err();
    assert_eq!(err, "disk full");
    assert_eq!(fx.calls, vec![Call::Save(protected(true)), Call::Rebuild]);
}

/// Every change syncs the keychain, so whatever it held for the previous
/// state is replaced or forgotten, never kept.
#[test]
fn the_keychain_never_keeps_a_key_for_the_previous_state() {
    let unprotected_but_remembering = SecurityConfig {
        use_keychain: true,
        ..SecurityConfig::default()
    };
    for (security, key, synced) in [
        (protected(true), Some(new_key()), Some(KEY)),
        (protected(false), Some(new_key()), None),
        (SecurityConfig::default(), None, None),
        (unprotected_but_remembering, None, None),
    ] {
        let mut fx = Recorder::default();
        apply_security_change(&mut fx, false, security.clone(), key).unwrap();
        let keychain: Vec<&Call> = fx
            .calls
            .iter()
            .filter(|call| matches!(call, Call::Keychain(..)))
            .collect();
        assert_eq!(
            keychain,
            vec![&Call::Keychain(false, synced.map(str::to_string))],
            "{security:?}"
        );
    }
}

#[test]
fn disabling_installs_no_key_and_rebuilds() {
    let mut fx = Recorder::default();
    apply_security_change(&mut fx, true, SecurityConfig::default(), None).unwrap();
    assert_eq!(
        fx.calls,
        vec![
            Call::Save(SecurityConfig::default()),
            Call::Keychain(true, None),
            Call::Install(None),
            Call::Rebuild,
        ]
    );
}

/// The config is saved by then, so the index must still be built under it;
/// the failure is reported, not a reason to stop half-way.
#[test]
fn a_failed_keychain_sync_still_installs_the_key_and_rebuilds() {
    let mut fx = Recorder {
        fail_keychain: true,
        ..Recorder::default()
    };
    let err = apply_security_change(&mut fx, false, protected(true), Some(new_key())).unwrap_err();
    assert_eq!(err, "keychain locked");
    assert_eq!(
        fx.calls,
        vec![
            Call::Save(protected(true)),
            Call::Keychain(false, Some(KEY.to_string())),
            Call::Install(Some(KEY.to_string())),
            Call::Rebuild,
        ]
    );
}

#[test]
fn a_failed_keychain_sync_leaves_the_preference_unsaved() {
    let mut fx = Recorder {
        fail_keychain: true,
        ..Recorder::default()
    };
    let err = set_keychain(&mut fx, protected(false), true, Some(KEY.to_string())).unwrap_err();
    assert_eq!(err, "keychain locked");
    assert_eq!(fx.calls, vec![Call::Keychain(false, Some(KEY.to_string()))]);
}

#[test]
fn the_keychain_preference_is_saved_once_the_keychain_matches_it() {
    let mut fx = Recorder::default();
    set_keychain(&mut fx, protected(false), true, Some(KEY.to_string())).unwrap();
    assert_eq!(
        fx.calls,
        vec![
            Call::Keychain(false, Some(KEY.to_string())),
            Call::Save(protected(true)),
        ]
    );

    // Forgetting stores nothing, even with a key installed.
    let mut fx = Recorder::default();
    set_keychain(&mut fx, protected(true), false, Some(KEY.to_string())).unwrap();
    assert_eq!(
        fx.calls,
        vec![Call::Keychain(true, None), Call::Save(protected(false))]
    );
}

/// Failing to forget a key matters only if one was remembered until now, so
/// the keychain is told the preference being replaced, not the one saved.
#[test]
fn the_keychain_is_told_whether_a_key_was_remembered_before() {
    for was_remembered in [false, true] {
        let held = (!was_remembered).then(|| KEY.to_string());
        let change = protected(!was_remembered);
        let mut fx = Recorder::default();
        apply_security_change(&mut fx, was_remembered, change, Some(new_key())).unwrap();
        assert_eq!(fx.calls[1], Call::Keychain(was_remembered, held.clone()));

        let mut fx = Recorder::default();
        set_keychain(
            &mut fx,
            protected(was_remembered),
            !was_remembered,
            Some(KEY.to_string()),
        )
        .unwrap();
        assert_eq!(fx.calls[0], Call::Keychain(was_remembered, held));
    }
}

#[test]
fn remembering_without_an_installed_key_touches_nothing() {
    let mut fx = Recorder::default();
    let err = set_keychain(&mut fx, protected(false), true, None).unwrap_err();
    assert!(err.contains("no key to remember"), "{err}");
    assert!(fx.calls.is_empty());
}

/// Under the fallback for a config that did not load, every security change
/// would delete the index, then fail its save: refused before anything
/// starts. Showing the key writes nothing, so it still may.
#[test]
fn a_config_that_did_not_load_refuses_every_change_that_saves() {
    let saving = [
        SecurityAction::Enable,
        SecurityAction::Disable,
        SecurityAction::ChangePassword,
        SecurityAction::SetKeychain(true),
        SecurityAction::SetKeychain(false),
    ];
    let fallback = Config::fallback();
    for action in saving {
        assert_eq!(
            refusal(&fallback, action),
            Some("Not changed: the config file did not load"),
            "{action:?}"
        );
    }
    assert_eq!(refusal(&fallback, SecurityAction::ShowKey), None);

    let loaded = Config {
        source: Some(PathBuf::from("config.toml")),
        ..Config::default()
    };
    for action in saving.into_iter().chain([SecurityAction::ShowKey]) {
        assert_eq!(refusal(&loaded, action), None, "{action:?}");
    }
}
