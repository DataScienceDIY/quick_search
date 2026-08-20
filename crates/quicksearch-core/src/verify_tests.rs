use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::testutil::{scratch_dir, touch};

/// Run to completion with cancellation switched off, returning every update.
fn run(paths: &[PathBuf]) -> Vec<VerifyUpdate> {
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    verify_identical(paths, &cancel, &mut |u| seen.push(u));
    seen
}

/// The report from a run, asserting it produced exactly one terminal update
/// and that the update was `Done`.
fn report(paths: &[PathBuf]) -> VerifyReport {
    let seen = run(paths);
    let terminal: Vec<&VerifyUpdate> = seen
        .iter()
        .filter(|u| !matches!(u, VerifyUpdate::Progress { .. }))
        .collect();
    assert_eq!(terminal.len(), 1, "expected one terminal update: {seen:?}");
    match terminal[0] {
        VerifyUpdate::Done(r) => r.clone(),
        other => panic!("expected Done, got {other:?}"),
    }
}

/// `n` files in a fresh directory, each with the body it is given.
fn files(tag: &str, bodies: &[&[u8]]) -> Vec<PathBuf> {
    let dir = scratch_dir(tag);
    bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let p = dir.join(format!("copy{i}.bin"));
            touch(&p, body);
            p
        })
        .collect()
}

#[test]
fn identical_files_all_report_identical() {
    for count in [2, 3] {
        let bodies = vec![&b"the same bytes in every copy"[..]; count];
        let paths = files("verify-same", &bodies);
        let r = report(&paths);
        assert_eq!(r.reference, Some(0));
        assert!(r.all_identical(), "{r:?}");
        assert_eq!(r.differing(), 0);
        // Every file was read through: the head hash alone would not do.
        assert_eq!(r.bytes_read, 28 * count as u64);
    }
}

#[test]
fn a_difference_is_reported_at_its_offset() {
    // First byte, mid-file, and the very last byte: the last is the one a
    // head-only hash can never see, and the reason this module exists.
    for (label, a, b, at) in [
        ("first", &b"Xbcdefgh"[..], &b"abcdefgh"[..], 0),
        ("middle", &b"abcdefgh"[..], &b"abcXefgh"[..], 3),
        ("last", &b"abcdefgh"[..], &b"abcdefgX"[..], 7),
    ] {
        let paths = files("verify-diff", &[a, b]);
        let r = report(&paths);
        assert_eq!(r.verdicts[0], MemberVerdict::Identical, "{label}");
        assert_eq!(r.verdicts[1], MemberVerdict::DiffersAt(at), "{label}");
        assert!(!r.all_identical(), "{label}");
        assert_eq!(r.differing(), 1, "{label}");
    }
}

/// The realistic false positive: same size, same head, different tail — a
/// pre-allocated disk image, which is what `[processing] hash_length`
/// documents as the known limitation.
#[test]
fn a_shared_head_with_a_different_tail_is_caught() {
    let head = vec![0u8; 64 * 1024];
    let mut a = head.clone();
    let mut b = head;
    a.extend_from_slice(b"footer-a");
    b.extend_from_slice(b"footer-b");
    let paths = files("verify-tail", &[a.as_slice(), b.as_slice()]);

    let r = report(&paths);
    assert_eq!(r.verdicts[1], MemberVerdict::DiffersAt(64 * 1024 + 7));
}

/// Bigger than one chunk, so the multi-chunk path and the running offset are
/// both exercised rather than assumed.
#[test]
fn a_difference_past_the_first_chunk_is_found() {
    // Two files, so the chunk is the 256 KiB ceiling and the difference sits
    // in the second one.
    let size = MAX_CHUNK * 2 + 1234;
    let a = vec![7u8; size];
    let mut b = a.clone();
    let at = MAX_CHUNK + 500;
    b[at] = 8;
    let paths = files("verify-chunks", &[a.as_slice(), b.as_slice()]);

    let r = report(&paths);
    assert_eq!(r.verdicts[1], MemberVerdict::DiffersAt(at as u64));
    // Two chunks out of each file and then it stops: with nothing left to
    // compare against, reading the remainder of the reference would be work
    // that cannot change the answer.
    assert_eq!(
        r.bytes_read,
        4 * MAX_CHUNK as u64,
        "did not stop once the last live file dropped out"
    );
}

#[test]
fn different_lengths_are_decided_without_reading() {
    let paths = files("verify-len", &[b"abcdefgh", b"abcdefghij"]);
    let r = report(&paths);
    assert_eq!(
        r.verdicts[1],
        MemberVerdict::LengthDiffers {
            len: 10,
            reference_len: 8
        }
    );
    assert_eq!(r.bytes_read, 0, "a length mismatch reads nothing");
}

#[test]
fn empty_files_are_identical() {
    let paths = files("verify-empty", &[b"", b""]);
    let r = report(&paths);
    assert!(r.all_identical(), "{r:?}");
    assert_eq!(r.bytes_read, 0);
}

#[test]
fn a_missing_member_is_unreadable_and_the_rest_still_compare() {
    let mut paths = files("verify-missing", &[b"same", b"same"]);
    paths.insert(1, PathBuf::from("/nonexistent/quicksearch-verify-missing"));

    let r = report(&paths);
    assert_eq!(r.reference, Some(0));
    assert!(matches!(r.verdicts[1], MemberVerdict::Unreadable(_)));
    assert_eq!(
        r.verdicts[2],
        MemberVerdict::Identical,
        "an unreadable member stopped the others being compared"
    );
    assert_eq!(r.differing(), 1);
}

/// The first path is the obvious reference, but not a required one: an
/// unreadable first member must not sink the whole run.
#[test]
fn the_reference_falls_through_to_the_first_readable_member() {
    let mut paths = files("verify-refmissing", &[b"same", b"same"]);
    paths.insert(0, PathBuf::from("/nonexistent/quicksearch-verify-ref"));

    let r = report(&paths);
    assert_eq!(r.reference, Some(1));
    assert!(matches!(r.verdicts[0], MemberVerdict::Unreadable(_)));
    assert_eq!(r.verdicts[1], MemberVerdict::Identical);
    assert_eq!(r.verdicts[2], MemberVerdict::Identical);
}

#[test]
fn nothing_readable_reports_no_reference() {
    let paths = vec![
        PathBuf::from("/nonexistent/quicksearch-verify-a"),
        PathBuf::from("/nonexistent/quicksearch-verify-b"),
    ];
    let r = report(&paths);
    assert_eq!(r.reference, None);
    assert_eq!(r.verdicts.len(), 2);
    assert!(r.verdicts.iter().all(|v| !v.is_identical()));
}

#[test]
fn a_single_file_and_an_empty_set_are_vacuously_identical() {
    let paths = files("verify-one", &[b"alone"]);
    let r = report(&paths);
    assert_eq!(r.reference, Some(0));
    assert!(r.all_identical());
    assert_eq!(r.bytes_read, 0, "nothing to compare it against");

    let r = report(&[]);
    assert_eq!(r.reference, None);
    assert!(r.verdicts.is_empty());
    assert!(r.all_identical());
}

#[test]
fn a_run_cancelled_before_it_starts_reports_only_that() {
    let paths = files("verify-cancel", &[b"same", b"same"]);
    let cancel = AtomicBool::new(true);
    let mut seen = Vec::new();
    verify_identical(&paths, &cancel, &mut |u| seen.push(u));
    assert_eq!(seen, vec![VerifyUpdate::Cancelled]);
}

/// Cancelling part way through ends the run there, with no `Done` claiming a
/// verdict it never reached. The first chunk always reports progress, so the
/// flag goes up between two chunks rather than at a time the test has to race
/// for.
#[test]
fn cancelling_mid_run_ends_it_without_a_verdict() {
    let body = vec![3u8; MAX_CHUNK * 4];
    let paths = files("verify-cancel-mid", &[body.as_slice(), body.as_slice()]);

    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    verify_identical(&paths, &cancel, &mut |u| {
        cancel.store(true, Ordering::Relaxed);
        seen.push(u);
    });
    assert!(
        matches!(seen.first(), Some(VerifyUpdate::Progress { .. })),
        "the first chunk did not report progress: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&VerifyUpdate::Cancelled), "{seen:?}");
    assert!(
        !seen.iter().any(|u| matches!(u, VerifyUpdate::Done(_))),
        "a cancelled run still reported a verdict: {seen:?}"
    );
}

/// Whatever progress reports, it is a fraction that makes sense: monotonic,
/// and never past its own denominator.
#[test]
fn progress_climbs_and_never_overruns_its_denominator() {
    let body = vec![9u8; MAX_CHUNK * 6];
    let paths = files(
        "verify-progress",
        &[body.as_slice(), body.as_slice(), body.as_slice()],
    );

    let mut last = 0;
    let mut count = 0;
    for update in run(&paths) {
        if let VerifyUpdate::Progress {
            bytes_read,
            bytes_total,
        } = update
        {
            assert!(
                bytes_read <= bytes_total,
                "{bytes_read} read of {bytes_total}"
            );
            assert!(bytes_read >= last, "progress went backwards");
            last = bytes_read;
            count += 1;
        }
    }
    assert!(count > 0, "a six-chunk comparison reported no progress");
}

/// A directory is not a file this can compare, however the platform refuses
/// it — `File::open` fails outright on Windows, while on Linux it opens and
/// then refuses to be read. Either way it is that member's problem, not the
/// run's.
#[test]
fn an_unreadable_member_does_not_stop_the_run() {
    let dir = scratch_dir("verify-dir");
    let a = dir.join("a.bin");
    let b = dir.join("b.bin");
    touch(&a, b"identical bytes");
    touch(&b, b"identical bytes");
    let sub = dir.join("subdir");
    std::fs::create_dir_all(&sub).unwrap();

    let r = report(&[a, sub, b]);
    assert_eq!(r.reference, Some(0));
    assert!(
        !r.verdicts[1].is_identical(),
        "a directory was called an identical file"
    );
    assert_eq!(r.verdicts[2], MemberVerdict::Identical);
}
