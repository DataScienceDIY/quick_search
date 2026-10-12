use super::*;
use std::sync::mpsc;
use std::thread;

#[test]
fn join_within_joins_a_thread_that_finishes() {
    // Still running at the first check, so the wait loop is what joins it.
    let handle = thread::spawn(|| thread::sleep(Duration::from_millis(50)));
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(join_within(handle, deadline));
}

#[test]
fn join_within_abandons_a_stuck_thread() {
    let (tx, rx) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let _ = rx.recv();
    });
    assert!(!join_within(handle, Instant::now()));
    // The detached thread ends once its channel closes.
    drop(tx);
}

#[test]
fn join_within_checks_once_past_its_deadline() {
    let deadline = Instant::now();
    let handle = thread::spawn(|| 7);
    while !handle.is_finished() {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(join_within(handle, deadline));
}

#[test]
fn join_within_counts_a_panicked_thread_as_ended() {
    let handle = thread::spawn(|| panic!("a panicking thread for join_within"));
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(join_within(handle, deadline));
}
