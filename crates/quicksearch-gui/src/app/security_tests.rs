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
