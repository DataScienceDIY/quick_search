use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::testutil::{scratch_dir, touch};

fn run(paths: &[PathBuf]) -> Vec<VerifyUpdate> {
    run_with(paths, || {})
}

/// [`run`], calling `hook` at each progress report.
fn run_with(paths: &[PathBuf], mut hook: impl FnMut()) -> Vec<VerifyUpdate> {
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    verify_identical(paths, &cancel, &mut |u| {
        if matches!(u, VerifyUpdate::Progress { .. }) {
            hook();
        }
        seen.push(u);
    });
    seen
}

fn report(paths: &[PathBuf]) -> VerifyReport {
    done(run(paths))
}

/// [`report`], with `edit` applied at the first progress report. That comes
/// right after the first chunk of the first batch, so the edit lands mid-run.
fn report_edited(paths: &[PathBuf], edit: impl FnOnce()) -> VerifyReport {
    let mut edit = Some(edit);
    done(run_with(paths, || {
        if let Some(edit) = edit.take() {
            edit();
        }
    }))
}

/// Asserts the run produced exactly one terminal update, a `Done`.
fn done(seen: Vec<VerifyUpdate>) -> VerifyReport {
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

fn set_len(path: &Path, len: u64) {
    let file = File::options().write(true).open(path).unwrap();
    file.set_len(len).unwrap();
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = File::options().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
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
    // First byte, mid-file, and the very last byte: a head-only hash never
    // sees the last, which is why this module exists.
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

/// The realistic false positive: same size, same head, different tail, as in
/// a pre-allocated disk image; `[processing] hash_length` documents it as the
/// known limitation.
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
    let size = MAX_CHUNK * 2 + 1234;
    let a = vec![7u8; size];
    let mut b = a.clone();
    let at = MAX_CHUNK + 500;
    b[at] = 8;
    let paths = files("verify-chunks", &[a.as_slice(), b.as_slice()]);

    let r = report(&paths);
    assert_eq!(r.verdicts[1], MemberVerdict::DiffersAt(at as u64));
    // Two chunks out of each file and then it stops: nothing left to
    // compare against.
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
/// verdict it never reached.
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

/// A member decided at open reads nothing, so it leaves the denominator:
/// the bar reaches its end before `Done` instead of stopping short.
#[test]
fn progress_reaches_its_end_past_a_member_decided_at_open() {
    let same: &[u8] = b"the same bytes";
    let paths = files(
        "verify-progress-decided",
        &[same, &b"a longer body than the others"[..], same],
    );

    let seen = run(&paths);
    let last = seen.iter().rev().find_map(|u| match u {
        VerifyUpdate::Progress {
            bytes_read,
            bytes_total,
        } => Some((*bytes_read, *bytes_total)),
        _ => None,
    });
    // The reference and the identical member, once each.
    assert_eq!(last, Some((28, 28)), "{seen:?}");
    let r = done(seen);
    assert!(
        matches!(r.verdicts[1], MemberVerdict::LengthDiffers { .. }),
        "{r:?}"
    );
    assert_eq!(r.verdicts[2], MemberVerdict::Identical);
}

/// A directory is not a file this can compare, however the platform refuses
/// it; either way it is that member's problem, not the run's.
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

/// More members than one batch holds: the later batch is opened, compared
/// against the reference read again from its start, and its difference found.
#[test]
fn a_group_larger_than_one_batch_is_compared_in_full() {
    let last = MAX_OPEN + 2;
    let mut bodies = vec![&b"the same bytes in every copy"[..]; last + 1];
    bodies[last] = b"the same bytes in every cop!";
    let paths = files("verify-batches", &bodies);

    let r = report(&paths);
    assert_eq!(r.reference, Some(0));
    assert_eq!(r.verdicts[last], MemberVerdict::DiffersAt(27));
    assert!(
        r.verdicts[..last].iter().all(MemberVerdict::is_identical),
        "{r:?}"
    );
    // Every member once, and the reference once for each of the two batches.
    assert_eq!(r.bytes_read, 28 * (last as u64 + 2));
}

/// Each batch counts offsets from the start of the files: an offset carried
/// over from the batch before would misplace a later batch's difference.
#[test]
fn a_difference_in_a_later_batch_reports_its_offset() {
    let mut bodies = vec![&b"abcdefgh"[..]; MAX_OPEN + 3];
    bodies[1] = b"abcXefgh";
    bodies[MAX_OPEN + 1] = b"abcdefXh";
    bodies[MAX_OPEN + 2] = b"Xbcdefgh";
    let paths = files("verify-later-diff", &bodies);

    let r = report(&paths);
    assert_eq!(r.verdicts[1], MemberVerdict::DiffersAt(3));
    assert_eq!(r.verdicts[MAX_OPEN + 1], MemberVerdict::DiffersAt(6));
    assert_eq!(r.verdicts[MAX_OPEN + 2], MemberVerdict::DiffersAt(0));
    assert_eq!(r.differing(), 3, "{r:?}");
}

/// Across batches, progress climbs to its own denominator and never past it.
/// Sleeping out the interval at each report makes every chunk report, which
/// here is once per batch.
#[test]
fn progress_with_batches_never_overruns_its_denominator() {
    let body = vec![5u8; 1000];
    let paths = files(
        "verify-progress-batches",
        &vec![body.as_slice(); MAX_OPEN + 3],
    );

    let seen = run_with(&paths, || std::thread::sleep(PROGRESS_INTERVAL));
    let progress: Vec<(u64, u64)> = seen
        .iter()
        .filter_map(|u| match u {
            VerifyUpdate::Progress {
                bytes_read,
                bytes_total,
            } => Some((*bytes_read, *bytes_total)),
            _ => None,
        })
        .collect();
    // The reference and 66 members, read once each plus the reference again
    // for the second batch.
    let total = 1000 * (MAX_OPEN as u64 + 4);
    assert_eq!(
        progress,
        vec![(1000 * (MAX_OPEN as u64 + 1), total), (total, total)]
    );
    let r = done(seen);
    assert!(r.all_identical(), "{r:?}");
    assert_eq!(r.bytes_read, total);
}

/// However large the group, only the reference and one batch are open at
/// once: descriptors are shared with search, SQLite and extraction. Sleeping
/// out the interval makes each batch's single chunk report.
#[cfg(target_os = "linux")]
#[test]
fn verify_holds_at_most_one_batch_open() {
    let paths = files("verify-fds", &vec![&b"same"[..]; 2 * MAX_OPEN + 2]);
    let dir = std::fs::canonicalize(paths[0].parent().unwrap()).unwrap();
    let open_here = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .filter(|target| target.starts_with(&dir))
            .count()
    };

    let mut samples = Vec::new();
    let r = done(run_with(&paths, || {
        samples.push(open_here());
        std::thread::sleep(PROGRESS_INTERVAL);
    }));
    assert!(r.all_identical(), "{r:?}");
    // Two full batches, then one of a single member: each batch's handles
    // were closed before the next one opened.
    assert_eq!(samples, vec![MAX_OPEN + 1, MAX_OPEN + 1, 2]);
    assert_eq!(open_here(), 0, "a handle outlived the run");
}

/// A member swapped for a FIFO after the run started is refused at its own
/// open, when its batch comes, instead of stranding the worker there.
#[cfg(unix)]
#[test]
fn a_member_replaced_by_a_fifo_before_its_batch_is_unreadable() {
    use std::os::unix::ffi::OsStrExt;

    let paths = files("verify-fifo", &vec![&b"same"[..]; MAX_OPEN + 2]);
    let late = &paths[MAX_OPEN + 1];
    let r = report_edited(&paths, || {
        std::fs::remove_file(late).unwrap();
        let c_path = std::ffi::CString::new(late.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    });
    assert!(
        matches!(r.verdicts[MAX_OPEN + 1], MemberVerdict::Unreadable(_)),
        "{r:?}"
    );
    assert!(
        r.verdicts[..=MAX_OPEN]
            .iter()
            .all(MemberVerdict::is_identical),
        "{r:?}"
    );
}

/// A reference cut short mid-run ends early: what survived matched what it
/// had, not the length the members were matched against.
#[test]
fn a_reference_truncated_mid_run_is_not_identical() {
    let body = vec![1u8; MAX_CHUNK * 4];
    let paths = files("verify-ref-cut", &[body.as_slice(), body.as_slice()]);

    let r = report_edited(&paths, || set_len(&paths[0], MAX_CHUNK as u64 * 2));
    let why = MemberVerdict::Unreadable(changed(&paths[0]));
    assert_eq!(r.verdicts, vec![why.clone(), why]);
}

/// The reference emptied once the first batch has read it: neither that
/// batch's survivors nor the later batch, never opened, keep Identical.
#[test]
fn a_reference_emptied_between_batches_leaves_no_member_identical() {
    let paths = files("verify-ref-emptied", &vec![&b"same"[..]; MAX_OPEN + 2]);

    let r = report_edited(&paths, || set_len(&paths[0], 0));
    let why = MemberVerdict::Unreadable(changed(&paths[0]));
    assert!(r.verdicts.iter().all(|v| *v == why), "{r:?}");
}

/// Growth past the stat'd length is never read, so it shows only in the
/// handle's length at the end.
#[test]
fn a_member_appended_mid_run_is_not_identical() {
    let body = vec![2u8; MAX_CHUNK * 4];
    let paths = files("verify-member-grew", &[body.as_slice(), body.as_slice()]);

    let r = report_edited(&paths, || append(&paths[1], b"more"));
    assert_eq!(r.verdicts[0], MemberVerdict::Identical);
    assert_eq!(r.verdicts[1], MemberVerdict::Unreadable(changed(&paths[1])));
}

/// The reference is read only to its stat'd length, so a member is not
/// blamed for ending there; the reference's own handle shows the change.
#[test]
fn a_reference_appended_mid_run_is_not_identical() {
    let body = vec![3u8; MAX_CHUNK * 4];
    let paths = files("verify-ref-grew", &[body.as_slice(), body.as_slice()]);

    let r = report_edited(&paths, || append(&paths[0], b"more"));
    let why = MemberVerdict::Unreadable(changed(&paths[0]));
    assert_eq!(r.verdicts, vec![why.clone(), why]);
}

/// A member cut short mid-run reads short against the reference, which says
/// where it ended.
#[test]
fn a_member_truncated_mid_run_is_unreadable() {
    let body = vec![4u8; MAX_CHUNK * 4];
    let paths = files("verify-member-cut", &[body.as_slice(), body.as_slice()]);
    let cut = MAX_CHUNK as u64 + 100;

    let r = report_edited(&paths, || set_len(&paths[1], cut));
    assert_eq!(r.verdicts[0], MemberVerdict::Identical);
    match &r.verdicts[1] {
        MemberVerdict::Unreadable(why) => {
            assert!(why.contains(&format!("ended at byte {cut} ")), "{why}")
        }
        other => panic!("expected Unreadable, got {other:?}"),
    }
}
