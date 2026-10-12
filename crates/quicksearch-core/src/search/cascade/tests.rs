use std::sync::Arc;

use super::*;

/// Pass A over `conn` as generation `generation`, with a sink that moves
/// `latest` on past it at the first batch: its result, and the batches sent.
fn filename_pass(
    conn: &Connection,
    generation: u64,
    latest: &AtomicU64,
) -> (Result<bool, String>, usize) {
    let query = crate::query::split::split_for_cascade(crate::testutil::NEEDLE).unwrap();
    let options = SearchOptions::default();
    let mut batches = 0usize;
    let mut sink = |_batch: Vec<SearchHit>| {
        batches += 1;
        latest.store(generation + 1, Ordering::SeqCst);
    };
    let mut cx = Cx::new(conn, &query, &options, generation, latest, &mut sink).unwrap();
    let outcome = cx.pass_filename();
    drop(cx);
    (outcome, batches)
}

/// Below [`run`], which reads a kill as cancellation, the progress handler
/// shows itself: the one name hit is the first row, and every later row is
/// one the `LIKE` rejects inside SQLite, so no row reaches the pass's own
/// check until the scan ends. Unarmed, that check is what ends it.
#[test]
fn the_armed_handler_kills_a_pass_mid_scan() {
    let (_dir, path) = crate::testutil::Scratch::db("pass-kill");
    crate::testutil::seed_index(
        &path,
        &crate::testutil::SeedSpec {
            files: 5_000,
            needle_names: 1,
            content_every: usize::MAX,
            ..Default::default()
        },
    );
    let conn = crate::db::open::open_search_reader(&path.to_string_lossy()).unwrap();
    let latest = Arc::new(AtomicU64::new(7));

    let (unarmed, batches) = filename_pass(&conn, 7, &latest);
    assert_eq!(
        unarmed,
        Ok(false),
        "the scan's end reads the moved generation"
    );
    assert_eq!(batches, 1);

    crate::search::arm_cancel(&conn, 8, latest.clone()).unwrap();
    let (armed, batches) = filename_pass(&conn, 8, &latest);
    let err = armed.expect_err("the handler must kill the statement mid-scan");
    assert!(err.contains("interrupted"), "{err}");
    assert_eq!(batches, 1, "no batch after the generation moved");
}
