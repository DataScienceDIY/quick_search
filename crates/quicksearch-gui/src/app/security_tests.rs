use super::*;

use crate::test_ui::{click_at, painted_text, painted_text_center, raw_input};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

/// Two 64-digit keys that differ, in the lowercase form [`IndexKey::to_hex`]
/// produces.
const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Two passes: an `egui::Window` is measured on its first frame and placed on
/// the next, so a single pass paints nothing to read back. Same shape as the
/// verify modal's test frame.
fn frame(
    ctx: &egui::Context,
    display: &str,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, (bool, bool)) {
    let _ = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
        reveal_key_modal(ctx, display);
    });
    let mut buttons = (false, false);
    let out = ctx.run(raw_input(SCREEN, events), |ctx| {
        buttons = reveal_key_modal(ctx, display);
    });
    (out, buttons)
}

/// The confirmation half, in the same two passes.
fn confirm_frame(
    ctx: &egui::Context,
    pw: &mut String,
    wrong: bool,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, (bool, bool)) {
    let _ = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
        confirm_key_modal(ctx, pw, wrong);
    });
    let mut buttons = (false, false);
    let out = ctx.run(raw_input(SCREEN, events), |ctx| {
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

/// A tool cannot be told "HMAC off" in prose — it needs the pragma. The hint
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
