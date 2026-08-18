use super::*;

use quicksearch_core::verify::MemberVerdict::{
    DiffersAt, Identical, LengthDiffers, Unreadable as CannotRead,
};

use crate::test_ui::{click_at, painted_text, painted_text_center, raw_input};

const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

fn modal(state: VerifyState, n: usize) -> VerifyModal {
    VerifyModal {
        paths: (0..n)
            .map(|i| PathBuf::from(format!("/d/copy{i}.bin")))
            .collect(),
        state,
    }
}

fn report(verdicts: Vec<MemberVerdict>, bytes_read: u64) -> VerifyState {
    VerifyState::Done(Box::new(VerifyReport {
        reference: Some(0),
        verdicts,
        bytes_read,
    }))
}

/// Two passes: an `egui::Window` is measured on its first frame and placed on
/// the next, so a single pass paints nothing to read back.
fn frame(
    ctx: &egui::Context,
    m: &VerifyModal,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, bool) {
    let _ = ctx.run(raw_input(SCREEN, Vec::new()), |ctx| {
        verify_modal(ctx, m);
    });
    let mut closed = false;
    let out = ctx.run(raw_input(SCREEN, events), |ctx| {
        closed = verify_modal(ctx, m);
    });
    (out, closed)
}

#[test]
fn a_run_in_progress_says_what_it_is_doing_and_offers_a_way_out() {
    let ctx = crate::test_ui::ctx();
    let m = modal(
        VerifyState::Running {
            bytes_read: 5 * 1024 * 1024,
            bytes_total: 20 * 1024 * 1024,
        },
        3,
    );
    let painted = painted_text(&frame(&ctx, &m, Vec::new()).0);
    assert!(
        painted.contains(&"Comparing 3 files byte for byte…".to_string()),
        "{painted:?}"
    );
    assert!(
        painted.iter().any(|t| t.contains("5.2 MB of 21.0 MB")),
        "no byte counter: {painted:?}"
    );
    assert!(painted.contains(&"Cancel".to_string()), "{painted:?}");
}

/// Before the worker's first update there is no denominator, so the modal
/// reports what it has rather than dividing by zero.
#[test]
fn a_run_with_no_denominator_yet_still_reports() {
    let ctx = crate::test_ui::ctx();
    let m = modal(
        VerifyState::Running {
            bytes_read: 0,
            bytes_total: 0,
        },
        2,
    );
    let painted = painted_text(&frame(&ctx, &m, Vec::new()).0);
    assert!(
        painted.iter().any(|t| t.contains("0 B read")),
        "{painted:?}"
    );
}

#[test]
fn a_clean_result_says_so_and_lists_what_was_read() {
    let ctx = crate::test_ui::ctx();
    let m = modal(report(vec![Identical, Identical, Identical], 300), 3);
    let painted = painted_text(&frame(&ctx, &m, Vec::new()).0);
    assert!(
        painted.contains(&"All 3 files are byte-for-byte identical.".to_string()),
        "{painted:?}"
    );
    assert!(painted.contains(&"/d/copy2.bin".to_string()), "{painted:?}");
    assert!(
        painted.contains(&"compared against".to_string()),
        "{painted:?}"
    );
    assert!(painted.contains(&"Close".to_string()), "{painted:?}");
}

/// The case the feature exists for: same size, same head, different bytes.
#[test]
fn a_mismatch_names_the_file_and_the_offset() {
    let ctx = crate::test_ui::ctx();
    let m = modal(report(vec![Identical, DiffersAt(1_234_567)], 2), 2);
    let painted = painted_text(&frame(&ctx, &m, Vec::new()).0);
    assert!(
        painted.contains(&"1 of 2 files is not identical.".to_string()),
        "{painted:?}"
    );
    assert!(
        painted.contains(&"differs at byte 1,234,567".to_string()),
        "{painted:?}"
    );
    assert!(painted.contains(&"/d/copy1.bin".to_string()), "{painted:?}");
}

#[test]
fn a_cancelled_run_says_so_rather_than_showing_a_verdict() {
    let ctx = crate::test_ui::ctx();
    let m = modal(VerifyState::Cancelled, 2);
    let painted = painted_text(&frame(&ctx, &m, Vec::new()).0);
    assert!(
        painted.contains(&"Verification cancelled.".to_string()),
        "{painted:?}"
    );
    assert!(
        !painted.iter().any(|t| t.contains("identical")),
        "a cancelled run claimed a verdict: {painted:?}"
    );
}

/// Every state's dismiss button reports the dismissal, whatever it is called.
#[test]
fn both_dismiss_buttons_report_the_dismissal() {
    for (label, state) in [
        (
            "Cancel",
            VerifyState::Running {
                bytes_read: 1,
                bytes_total: 2,
            },
        ),
        ("Close", VerifyState::Cancelled),
        ("Close", report(vec![Identical, Identical], 8)),
    ] {
        let ctx = crate::test_ui::ctx();
        let m = modal(state, 2);
        let (out, _) = frame(&ctx, &m, Vec::new());
        let pos =
            painted_text_center(&out, label).unwrap_or_else(|| panic!("no {label} button painted"));
        let (_, closed) = frame(&ctx, &m, click_at(pos));
        assert!(closed, "clicking {label} did not dismiss the modal");
    }
}

#[test]
fn every_verdict_reads_as_a_sentence_about_the_file() {
    assert_eq!(verdict_line(&Identical, false), "identical");
    assert_eq!(verdict_line(&Identical, true), "compared against");
    assert_eq!(verdict_line(&DiffersAt(0), false), "differs at byte 0");
    assert_eq!(
        verdict_line(&DiffersAt(1_048_576), false),
        "differs at byte 1,048,576"
    );
    assert_eq!(
        verdict_line(
            &LengthDiffers {
                len: 2048,
                reference_len: 1024
            },
            false
        ),
        "size differs: 2.0 KB against 1.0 KB"
    );
    assert!(verdict_line(&CannotRead("/d/x: denied".into()), false)
        .contains("could not be read — /d/x: denied"));
}

#[test]
fn the_summary_counts_what_it_found() {
    let of = |verdicts: Vec<MemberVerdict>, reference| {
        summary_line(&VerifyReport {
            reference,
            verdicts,
            bytes_read: 0,
        })
    };
    assert_eq!(
        of(vec![Identical, Identical], Some(0)),
        "All 2 files are byte-for-byte identical."
    );
    assert_eq!(
        of(vec![Identical, DiffersAt(4)], Some(0)),
        "1 of 2 files is not identical."
    );
    assert_eq!(
        of(vec![Identical, DiffersAt(4), DiffersAt(9)], Some(0)),
        "2 of 3 files are not identical."
    );
    // A group of one cannot disagree with itself, and saying "all 1 files are
    // identical" would read as an answer to a question nobody asked.
    assert_eq!(
        of(vec![Identical], Some(0)),
        "Nothing to compare: the group holds one file."
    );
    assert_eq!(
        of(
            vec![CannotRead("gone".into()), CannotRead("gone".into())],
            None
        ),
        "None of these files could be read."
    );
}
