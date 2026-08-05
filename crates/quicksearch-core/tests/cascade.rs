//! Integration tests for the ranked search cascade and the streaming
//! search service, against real temp databases.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use quicksearch_core::db::open_or_recreate;
use quicksearch_core::db::repo::{insert_file, set_content_done, NewFile};
use quicksearch_core::mime::FileType;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions, SearchService, SearchUpdate};

mod common;
use common::scratch_db as tmp_db;

struct Seeder {
    conn: rusqlite::Connection,
    store_text: bool,
}

impl Seeder {
    fn new(path: &Path, store_text: bool) -> Seeder {
        Seeder {
            conn: open_or_recreate(path.to_str().unwrap(), "trigram").unwrap(),
            store_text,
        }
    }

    /// Insert a file; `text: Some(..)` also content-indexes it.
    fn add(&mut self, name: &str, dir: &str, mtime: u64, text: Option<&str>) -> i64 {
        let path = format!("{}/{}", dir, name);
        let tx = self.conn.transaction().unwrap();
        let id = insert_file(
            &tx,
            &NewFile {
                name,
                path: &path,
                parent: dir,
                size: 42,
                mtime,
                inode: None,
                device_id: None,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        if let Some(text) = text {
            set_content_done(&tx, id, name, text, &[], self.store_text).unwrap();
        }
        tx.commit().unwrap();
        id
    }

    fn done(self) -> rusqlite::Connection {
        self.conn
    }
}

/// Run the cascade synchronously, collecting every batch. Returns
/// (flattened hits in emission order, outcome).
/// Run a search and return its hits in rank order.
///
/// The cascade streams batches *while* each pass scans, so arrival order is
/// table order, not rank order — batch two can hold something better than
/// anything in batch one. Ordering across batches belongs to the consumer, and
/// this mirrors what the GUI does with the default sort key, so the ranking
/// assertions below stay about ranking rather than about scan order.
///
/// Use [`run_collect_batches`] to assert on the stream itself.
fn run_collect(
    conn: &rusqlite::Connection,
    input: &str,
    options: &SearchOptions,
) -> (Vec<SearchHit>, cascade::Outcome) {
    let (batches, outcome) = run_collect_batches(conn, input, options);
    let mut hits: Vec<SearchHit> = batches.into_iter().flatten().collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    (hits, outcome)
}

/// The raw batch stream, one `Vec` per sink call.
fn run_collect_batches(
    conn: &rusqlite::Connection,
    input: &str,
    options: &SearchOptions,
) -> (Vec<Vec<SearchHit>>, cascade::Outcome) {
    let split = split_for_cascade(input).expect("split");
    let latest = AtomicU64::new(7);
    let mut batches = Vec::new();
    let outcome = cascade::run(conn, &split, options, 7, &latest, &mut |batch| {
        batches.push(batch)
    })
    .expect("cascade run")
    .expect("not cancelled");
    (batches, outcome)
}

fn fuzzy_options() -> SearchOptions {
    SearchOptions {
        fuzzy: true,
        ..SearchOptions::default()
    }
}

fn fuzzy_options_with_edits(max_edits: usize) -> SearchOptions {
    SearchOptions {
        fuzzy: true,
        fuzzy_max_edits: max_edits,
        ..SearchOptions::default()
    }
}

#[test]
fn rank_classification_across_all_stages() {
    let p = tmp_db("ranks");
    let mut s = Seeder::new(&p, true);
    let rank1 = s.add("Report", "/a", 1, None);
    let rank2 = s.add("report", "/b", 2, None);
    let rank3 = s.add("Quarterly_Report.txt", "/c", 3, None);
    let rank4 = s.add("quarterly_report.txt", "/d", 4, None);
    let rank5 = s.add("notes-cs.txt", "/e", 5, Some("the Report was filed today"));
    let rank6 = s.add("notes-ci.txt", "/f", 6, Some("the report was filed today"));
    let rank7 = s.add("Reprot.txt", "/g", 7, None); // 2 substitutions
    let rank8 = s.add("body-fuzzy.txt", "/h", 8, Some("the reoprt went missing"));
    // Path tiers: the term is in the directory, never in the name.
    let rank9 = s.add("alpha.bin", "/Report-archive", 9, None);
    let rank10 = s.add("beta.bin", "/report-archive", 10, None);
    let rank11 = s.add("gamma.bin", "/Reprot-archive", 11, None);
    let _miss = s.add("unrelated.bin", "/z", 12, Some("nothing to see"));
    let conn = s.done();

    let (hits, outcome) = run_collect(&conn, "Report", &fuzzy_options());
    let order: Vec<(i64, u8)> = hits.iter().map(|h| (h.file_id, h.stage)).collect();
    assert_eq!(
        order,
        vec![
            (rank1, 1),
            (rank2, 2),
            (rank3, 3),
            (rank4, 4),
            (rank5, 5),
            (rank6, 6),
            (rank7, 7),
            (rank8, 8),
            (rank9, 9),
            (rank10, 10),
            (rank11, 11),
        ],
        "each fixture lands at its designed rank, in order"
    );
    assert_eq!(outcome.total, 11);
    assert!(!outcome.limited);

    // Rank monotonicity across the whole emission stream.
    for pair in hits.windows(2) {
        assert!(
            pair[0].rank <= pair[1].rank,
            "ranks must never decrease: {} then {}",
            pair[0].rank,
            pair[1].rank
        );
    }

    // Full-text hits carry snippets with valid ranges.
    for h in hits
        .iter()
        .filter(|h| h.stage == 5 || h.stage == 6 || h.stage == 8)
    {
        let snip = h.snippet.as_ref().expect("full-text hit has a snippet");
        for &(a, b) in &snip.ranges {
            assert!(a < b && b <= snip.window.len());
        }
    }

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn path_substring_tiers_split_by_case() {
    let p = tmp_db("pathcase");
    let mut s = Seeder::new(&p, true);
    let exact = s.add("a.bin", "/Vacation-2024", 1, None);
    let anycase = s.add("b.bin", "/vacation-2023", 2, None);
    let _miss = s.add("c.bin", "/holiday", 3, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "Vacation", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(exact, 9), (anycase, 10)],
        "exact-case path matches outrank any-case ones"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn path_hits_carry_a_highlighted_path_snippet() {
    let p = tmp_db("pathsnip");
    let mut s = Seeder::new(&p, true);
    s.add("a.bin", "/srv/Vacation/raw", 1, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "Vacation", &SearchOptions::default());
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    let snip = hit.snippet.as_ref().expect("path hits mark the match");
    assert_eq!(snip.window, hit.path, "the window is the whole path");
    assert_eq!(snip.ranges.len(), 1);
    let (a, b) = snip.ranges[0];
    assert_eq!(&snip.window[a..b], "Vacation");
    assert!(!snip.truncated_start && !snip.truncated_end);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn path_match_is_deduped_against_a_better_name_match() {
    let p = tmp_db("pathdedup");
    let mut s = Seeder::new(&p, true);
    // Name matches at rank 3 and the path would match at 9 as well.
    let star = s.add("Budget.txt", "/Budget/2024", 1, None);
    let conn = s.done();

    let (hits, outcome) = run_collect(&conn, "Budget", &fuzzy_options());
    assert_eq!(outcome.total, 1);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, star);
    assert_eq!(hits[0].stage, 3, "the name tier wins");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn path_tiers_respect_the_three_char_floor() {
    let p = tmp_db("pathfloor");
    let mut s = Seeder::new(&p, true);
    let dir_only = s.add("z.bin", "/abcdir", 1, None);
    let name_hit = s.add("ab.txt", "/d", 2, None);
    let conn = s.done();

    // Below the floor nothing path-shaped leaks in, fuzzy tier included.
    let (short, _) = run_collect(&conn, "ab", &fuzzy_options());
    assert_eq!(
        short.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![name_hit],
        "2-char term: no path matching at all"
    );

    // At the floor the substring tier surfaces the directory match. (Fuzzy
    // is off here so `ab.txt`, a 1-edit match for `abc`, stays out of it.)
    let (long, _) = run_collect(&conn, "abc", &SearchOptions::default());
    assert_eq!(
        long.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(dir_only, 9)],
        "3-char term: the directory match surfaces"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn term_with_separator_matches_across_the_path() {
    let p = tmp_db("pathsep");
    let mut s = Seeder::new(&p, true);
    let nested = s.add("report-final.txt", "/home/docs", 1, None);
    let _elsewhere = s.add("report-final.txt", "/home/other", 2, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "docs/report", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(nested, 9)],
        "a term spanning a separator can only match the full path"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn fuzzy_path_tier_requires_the_fuzzy_flag() {
    let p = tmp_db("fuzzypath");
    let mut s = Seeder::new(&p, true);
    let typo_dir = s.add("gamma.bin", "/Reprot-archive", 1, None);
    let conn = s.done();

    let (off, _) = run_collect(&conn, "Report", &SearchOptions::default());
    assert!(off.is_empty(), "no fuzzy stages without the flag");

    let (on, _) = run_collect(&conn, "Report", &fuzzy_options());
    assert_eq!(
        on.iter().map(|h| (h.file_id, h.stage)).collect::<Vec<_>>(),
        vec![(typo_dir, 11)]
    );
    assert!((on[0].rank - 11.2).abs() < 1e-9, "2 edits adds 0.2");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn fuzzy_max_edits_widens_and_narrows_the_budget() {
    let p = tmp_db("fuzzybudget");
    let mut s = Seeder::new(&p, true);
    let two_edits = s.add("quartrely.txt", "/d", 1, None);
    let three_edits = s.add("quxxxerly.txt", "/d", 2, None);
    let conn = s.done();

    let (default, _) = run_collect(&conn, "quarterly", &fuzzy_options());
    assert_eq!(
        default.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![two_edits],
        "the default budget of 2 can't reach a 3-edit typo"
    );
    assert!((default[0].rank - 7.2).abs() < 1e-9);

    let (widened, _) = run_collect(&conn, "quarterly", &fuzzy_options_with_edits(3));
    assert_eq!(
        widened.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![two_edits, three_edits],
        "raising the cap admits the 3-edit typo, ranked after the closer one"
    );
    assert!((widened[1].rank - 7.3).abs() < 1e-9);

    let (strict, _) = run_collect(&conn, "quarterly", &fuzzy_options_with_edits(1));
    assert!(strict.is_empty(), "a cap of 1 rejects both typos");

    let (off, _) = run_collect(&conn, "quarterly", &fuzzy_options_with_edits(0));
    assert!(off.is_empty(), "a cap of 0 disables the fuzzy stages");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn dedup_keeps_best_rank() {
    let p = tmp_db("dedup");
    let mut s = Seeder::new(&p, true);
    // Exact-case filename match whose body also contains the term: would
    // hit ranks 1, 3 (substring of itself is exact) and 5 — must appear
    // once, at rank 1.
    let star = s.add("Budget", "/a", 1, Some("Budget Budget Budget"));
    let conn = s.done();

    let (hits, outcome) = run_collect(&conn, "Budget", &fuzzy_options());
    assert_eq!(outcome.total, 1);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_id, star);
    assert_eq!(hits[0].stage, 1);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn occurrence_counts_order_within_rank() {
    let p = tmp_db("frac");
    let mut s = Seeder::new(&p, true);
    let one = s.add("one.txt", "/d", 1, Some("zebra"));
    let three = s.add("three.txt", "/d", 2, Some("zebra zebra zebra"));
    let thousand = s.add("thousand.txt", "/d", 3, Some(&"zebra ".repeat(1500)));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "zebra", &SearchOptions::default());
    let ids: Vec<i64> = hits.iter().map(|h| h.file_id).collect();
    assert_eq!(
        ids,
        vec![thousand, three, one],
        "more occurrences sorts earlier within the rank"
    );
    assert_eq!(hits[0].rank, 5.0, "1000+ occurrences adds zero");
    assert!((hits[1].rank - 5.997).abs() < 1e-9);
    assert!((hits[2].rank - 5.999).abs() < 1e-9);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn like_metacharacters_are_literal() {
    let p = tmp_db("like");
    let mut s = Seeder::new(&p, true);
    let real = s.add("100%.txt", "/d", 1, None);
    let _decoy = s.add("x100y.txt", "/d", 2, None);
    let _decoy2 = s.add("100_.txt", "/d", 3, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "100%", &SearchOptions::default());
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![real],
        "% in the term must not act as a wildcard"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn trigram_floor_skips_text_stages() {
    let p = tmp_db("floor");
    let mut s = Seeder::new(&p, true);
    let name_hit = s.add("ab.txt", "/d", 1, None);
    let _text_only = s.add("body.txt", "/d", 2, Some("ab ab ab"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "ab", &fuzzy_options());
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![name_hit],
        "2-char term: filename stages only (fuzzy also skipped below 3)"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn diacritic_fts_candidates_are_dropped() {
    let p = tmp_db("diacritic");
    let mut s = Seeder::new(&p, true);
    // trigram remove_diacritics 1 makes this an FTS candidate for "cafe",
    // but the exact bytes never occur — exact full-text must drop it.
    let _accented = s.add("menu.txt", "/d", 1, Some("le café est ouvert"));
    let plain = s.add("plain.txt", "/d", 2, Some("the cafe is open"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "cafe", &SearchOptions::default());
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![plain]
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn contentless_mode_degrades_to_unranked_stage6() {
    let p = tmp_db("notext");
    let mut s = Seeder::new(&p, false); // store_text_for_snippets = false
    let doc = s.add("doc.txt", "/d", 1, Some("walrus walrus walrus"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "walrus", &fuzzy_options());
    assert_eq!(hits.len(), 1, "FTS still finds it");
    let h = &hits[0];
    assert_eq!(h.file_id, doc);
    assert_eq!(h.stage, 6, "cannot case-verify without text");
    assert!((h.rank - 6.999).abs() < 1e-9, "count-unknown fraction");
    assert!(h.snippet.is_none());
    // And no fuzzy full-text stage without documents_text.
    assert!(!hits.iter().any(|h| h.stage == 8));

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn filters_apply_to_every_stage() {
    let p = tmp_db("filters");
    let mut s = Seeder::new(&p, true);
    let keep_name = s.add("alpha.txt", "/keep", 1, None);
    let _skip_name = s.add("alpha.txt", "/skip", 1, None);
    let keep_text = s.add("k.txt", "/keep", 2, Some("alpha body"));
    let _skip_text = s.add("s.txt", "/skip", 2, Some("alpha body"));
    let keep_fuzzy = s.add("alpah.txt", "/keep", 3, None);
    let _skip_fuzzy = s.add("alpah.txt", "/skip", 3, None);
    let keep_path = s.add("p.bin", "/keep/alpha-sub", 4, None);
    let _skip_path = s.add("p.bin", "/skip/alpha-sub", 4, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "alpha path:/keep", &fuzzy_options());
    let mut ids: Vec<i64> = hits.iter().map(|h| h.file_id).collect();
    ids.sort();
    let mut want = vec![keep_name, keep_text, keep_fuzzy, keep_path];
    want.sort();
    assert_eq!(ids, want, "the path filter must gate all stages");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn limit_truncates_and_flags() {
    let p = tmp_db("limit");
    let mut s = Seeder::new(&p, true);
    for i in 0..10 {
        s.add(&format!("match-{:02}.txt", i), "/d", 1, None);
    }
    let conn = s.done();

    let options = SearchOptions {
        limit: 3,
        ..SearchOptions::default()
    };
    let (hits, outcome) = run_collect(&conn, "match", &options);
    assert_eq!(hits.len(), 3);
    assert_eq!(outcome.total, 3);
    assert!(outcome.limited);
    // Best-ranked (here: name-ordered within rank 3) survive.
    assert_eq!(hits[0].name, "match-00.txt");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn session_ignores_hide_hits_before_the_cap() {
    let p = tmp_db("ignores");
    let mut s = Seeder::new(&p, true);
    let keep = s.add("keep-match.txt", "/d", 1, None);
    let _log = s.add("match.log", "/d", 2, None);
    let _sub = s.add("match.txt", "/d/logs", 3, None);
    // Would be a rank-9 path hit, but its parent is an ignored component.
    let _path_hit = s.add("z.bin", "/d/logs/match-sub", 4, None);
    let conn = s.done();

    let options = SearchOptions {
        session_ignores: vec!["*.log".to_string(), "logs".to_string()],
        ..SearchOptions::default()
    };
    let (hits, outcome) = run_collect(&conn, "match", &options);
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![keep]
    );
    assert_eq!(outcome.total, 1, "ignored rows never count toward totals");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn empty_and_filter_only_terms_return_nothing() {
    let p = tmp_db("empty");
    let mut s = Seeder::new(&p, true);
    s.add("anything.txt", "/d", 1, Some("anything"));
    let conn = s.done();

    for input in ["", "   ", "type:Text"] {
        let (hits, outcome) = run_collect(&conn, input, &SearchOptions::default());
        assert!(hits.is_empty(), "input {:?}", input);
        assert_eq!(outcome.total, 0);
    }

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn hostile_terms_are_inert() {
    let p = tmp_db("hostile");
    let mut s = Seeder::new(&p, true);
    s.add("innocent.txt", "/d", 1, Some("innocent content"));
    let conn = s.done();

    for term in [
        "'; DROP TABLE files; --",
        "\" OR 1=1 --",
        // `term*` is a live wildcard now; the FTS metacharacters after it
        // must still be inert.
        "term* (NEAR) : ^",
        "a\0b",
        // Star-only and star-heavy terms must not scan-everything or error.
        "*",
        "****",
        "* *",
        &format!("{}*", "x".repeat(10_000)),
    ] {
        let split = split_for_cascade(term).expect("split never fails on words");
        let latest = AtomicU64::new(1);
        let result = cascade::run(
            &conn,
            &split,
            &SearchOptions::default(),
            1,
            &latest,
            &mut |_| {},
        );
        assert!(result.is_ok(), "term {:?}: {:?}", term, result.err());
    }
    // Table survived.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_name_ranks_through_the_same_tiers() {
    let p = tmp_db("wildranks");
    let mut s = Seeder::new(&p, true);
    // `report*` anchors the whole name, so these are tiers 1 and 2 …
    let whole_cs = s.add("report2024.pdf", "/a", 1, None);
    let whole_ci = s.add("Report2024.pdf", "/b", 2, None);
    // … and a name that merely contains the pattern is tier 3/4. A
    // trailing star can match nothing, so "report" mid-name counts too —
    // substring semantics make the edge star free.
    let sub_cs = s.add("my-report-final.txt", "/c", 3, None);
    let sub_ci = s.add("my-Report-final.txt", "/d", 4, None);
    let suffix = s.add("2024report.pdf", "/e", 5, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "report*", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        // Within rank 3 the tie breaks by name: "2024…" sorts first.
        vec![
            (whole_cs, 1),
            (whole_ci, 2),
            (suffix, 3),
            (sub_cs, 3),
            (sub_ci, 4)
        ],
        "wildcard terms rank exactly like literal ones"
    );

    // Ordered-segment check the other way: `report*2024` must not match a
    // name where 2024 precedes report.
    let (ordered, _) = run_collect(&conn, "report*2024", &SearchOptions::default());
    assert!(
        !ordered.iter().any(|h| h.name == "2024report.pdf"),
        "segments must match in order"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn extension_glob_whole_matches_every_such_file() {
    let p = tmp_db("extglob");
    let mut s = Seeder::new(&p, true);
    let a = s.add("alpha.txt", "/d", 1, None);
    let b = s.add("beta.txt", "/d", 2, None);
    let upper = s.add("GAMMA.TXT", "/d", 3, None);
    let _other = s.add("delta.pdf", "/d", 4, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "*.txt", &SearchOptions::default());
    let mut tier1: Vec<i64> = hits
        .iter()
        .filter(|h| h.stage == 1)
        .map(|h| h.file_id)
        .collect();
    tier1.sort();
    assert_eq!(tier1, vec![a, b], "every exact-case .txt is a tier-1 hit");
    assert_eq!(
        hits.iter()
            .filter(|h| h.stage == 2)
            .map(|h| h.file_id)
            .collect::<Vec<_>>(),
        vec![upper],
        "case-folded whole match lands at tier 2"
    );
    assert!(!hits.iter().any(|h| h.name == "delta.pdf"));

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_leaves_like_metacharacters_literal() {
    let p = tmp_db("wildlike");
    let mut s = Seeder::new(&p, true);
    let percent = s.add("100%.txt", "/d", 1, None);
    let underscore = s.add("100_.txt", "/d", 2, None);
    // `100*` must not let `%`/`_` semantics leak: this name has no "100".
    let _decoy = s.add("1x0y.txt", "/d", 3, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "100*", &SearchOptions::default());
    let mut ids: Vec<i64> = hits.iter().map(|h| h.file_id).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![percent, underscore],
        "star globs, % and _ stay literal"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_fulltext_narrows_with_fts_and_verifies_order() {
    let p = tmp_db("wildtext");
    let mut s = Seeder::new(&p, true);
    let ordered = s.add("a.txt", "/d", 1, Some("a wondrous world indeed"));
    // FTS AND-of-segments finds this too (both trigram runs occur), but the
    // pattern requires "wond" before "world" — verification drops it.
    let _reversed = s.add("b.txt", "/d", 2, Some("world of wonders"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "wond*world", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(ordered, 5)],
        "unordered FTS candidates must fail pattern verification"
    );
    let snip = hits[0]
        .snippet
        .as_ref()
        .expect("wildcard hit has a snippet");
    assert_eq!(snip.ranges.len(), 1);
    let (a, b) = snip.ranges[0];
    assert_eq!(&snip.window[a..b], "wondrous world");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_with_short_segments_falls_back_to_a_full_scan() {
    let p = tmp_db("wildshort");
    let mut s = Seeder::new(&p, true);
    // `ab*cd`: no segment reaches the trigram floor, so FTS can't narrow —
    // the fallback scans documents_text and pattern-verifies each row.
    let hit = s.add("doc.txt", "/d", 1, Some("zz abXcd zz"));
    let _miss = s.add("other.txt", "/d", 2, Some("cd before ab"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "ab*cd", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(hit, 5)]
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_path_tier_and_filters() {
    let p = tmp_db("wildpath");
    let mut s = Seeder::new(&p, true);
    let dir_hit = s.add("a.bin", "/Vacation-2024", 1, None);
    let _filtered = s.add("b.bin", "/elsewhere/Vacation-2023", 2, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "Vac*tion", &SearchOptions::default());
    let mut ids: Vec<i64> = hits.iter().map(|h| h.file_id).collect();
    ids.sort();
    assert_eq!(ids, vec![dir_hit, _filtered]);
    assert!(hits.iter().all(|h| h.stage == 9), "term only in the path");

    // Structured filters gate wildcard scans like any other.
    let (kept, _) = run_collect(&conn, "Vac*tion path:/elsewhere", &SearchOptions::default());
    assert_eq!(
        kept.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![_filtered]
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn wildcard_terms_skip_the_fuzzy_stages() {
    let p = tmp_db("wildfuzzy");
    let mut s = Seeder::new(&p, true);
    let real = s.add("report.txt", "/d", 1, None);
    // A 2-edit typo of "report": fuzzy would admit it for a literal term,
    // but a wildcard term must not fuzz.
    let _typo = s.add("Reprot.txt", "/d", 2, None);
    let _typo_body = s.add("body.txt", "/d", 3, Some("the reoprt went missing"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, "rep*rt", &fuzzy_options());
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![real],
        "no stage 7/8/11 hits for a wildcard term even with fuzzy on"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn contentless_wildcard_degrades_to_unranked_stage6() {
    let p = tmp_db("wildnotext");
    let mut s = Seeder::new(&p, false); // store_text_for_snippets = false
    let doc = s.add("doc.txt", "/d", 1, Some("walrus columns"));
    let conn = s.done();

    // Both segments clear the trigram floor, so FTS narrows; without stored
    // text the row can't be pattern-verified and lands at count-unknown 6.
    let (hits, _) = run_collect(&conn, "wal*rus", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(doc, 6)]
    );
    assert!(hits[0].snippet.is_none());

    // The short-segment fallback has no FTS evidence to lean on, so a
    // contentless DB simply finds nothing there.
    let (none, _) = run_collect(&conn, "wa*us", &SearchOptions::default());
    assert!(none.is_empty());

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn regex_only_query_hits_name_content_and_path() {
    let p = tmp_db("regexonly");
    let mut s = Seeder::new(&p, true);
    let by_name = s.add("qz42.txt", "/d", 1, None);
    let by_content = s.add("notes.txt", "/d", 2, Some("ref qz7 in the body"));
    let by_path = s.add("b.bin", "/qz99-dir", 3, None);
    let _miss = s.add("plain.txt", "/d", 4, Some("nothing here"));
    let conn = s.done();

    let (hits, _) = run_collect(&conn, r"regex:qz\d+", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(by_name, 4), (by_content, 6), (by_path, 10)],
        "regex-only reuses the name/content/path tiers in cascade order"
    );

    // Name and path hits mark the match in the field itself.
    let name_snip = hits[0].snippet.as_ref().unwrap();
    let (a, b) = name_snip.ranges[0];
    assert_eq!(&name_snip.window[a..b], "qz42");
    let path_snip = hits[2].snippet.as_ref().unwrap();
    let (a, b) = path_snip.ranges[0];
    assert_eq!(&path_snip.window[a..b], "qz99");
    // Content hits get a windowed snippet around the first match.
    let body_snip = hits[1].snippet.as_ref().unwrap();
    let (a, b) = body_snip.ranges[0];
    assert_eq!(&body_snip.window[a..b], "qz7");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn regex_is_case_insensitive_by_default_and_respects_filters() {
    let p = tmp_db("regexci");
    let mut s = Seeder::new(&p, true);
    let keep = s.add("QZ1.txt", "/keep", 1, None);
    let _skip = s.add("qz2.txt", "/skip", 2, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, r"regex:qz\d path:/keep", &SearchOptions::default());
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![keep]
    );

    // Inline opt-out flips it back to case-sensitive.
    let (cs, _) = run_collect(&conn, r"regex:(?-i:qz)\d", &SearchOptions::default());
    assert!(!cs.iter().any(|h| h.file_id == keep));

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn regex_alongside_a_term_is_an_accept_predicate() {
    let p = tmp_db("regexpred");
    let mut s = Seeder::new(&p, true);
    // Term hit whose *content* satisfies the regex: kept, via the lazy
    // content fetch (the regex is nowhere in its name or path).
    let kept = s.add("budget-a.txt", "/d", 1, Some("code acme7 inside"));
    // Term hit with no content at all: the regex can't be satisfied.
    let _dropped = s.add("budget-b.txt", "/d", 2, None);
    let conn = s.done();

    let (hits, _) = run_collect(&conn, r"budget regex:acme\d", &SearchOptions::default());
    assert_eq!(
        hits.iter()
            .map(|h| (h.file_id, h.stage))
            .collect::<Vec<_>>(),
        vec![(kept, 3)],
        "the term drives ranking; the regex gates acceptance"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn hostile_regexes_complete_quickly() {
    let p = tmp_db("regexhostile");
    let mut s = Seeder::new(&p, true);
    s.add("aaa.txt", "/d", 1, Some(&"a".repeat(50_000)));
    let conn = s.done();

    // Backtracking bomb against a pathological haystack: the linear engine
    // must simply finish.
    let start = std::time::Instant::now();
    let (hits, _) = run_collect(&conn, r#"regex:"(a+)+$""#, &SearchOptions::default());
    assert!(!hits.is_empty(), "the all-a body does end in a run of a's");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "hostile regex must not blow up: took {:?}",
        start.elapsed()
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn service_surfaces_invalid_regex_as_an_error() {
    let p = tmp_db("regexerr");
    let mut s = Seeder::new(&p, true);
    s.add("anything.txt", "/d", 1, None);
    drop(s.done());

    let (service, updates) = SearchService::new(p.clone(), Arc::new(|| {}));
    let generation = service.search("regex:[", SearchOptions::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut message = None;
    while std::time::Instant::now() < deadline {
        match updates.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(SearchUpdate::Error {
                generation: g,
                message: m,
            }) if g == generation => {
                message = Some(m);
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let message = message.expect("invalid regex must surface as a search error");
    assert!(message.contains("regex"), "unhelpful message: {}", message);
    service.shutdown();

    std::fs::remove_file(&p).ok();
}

#[test]
fn generation_bump_cancels_mid_stream() {
    let p = tmp_db("cancel");
    let mut s = Seeder::new(&p, true);
    for i in 0..500 {
        s.add(&format!("bulk-{:04}.txt", i), "/d", 1, None);
    }
    let conn = s.done();

    let split = split_for_cascade("bulk").unwrap();
    let latest = Arc::new(AtomicU64::new(3));
    let latest_for_sink = latest.clone();
    let mut batches = 0usize;
    let outcome = cascade::run(
        &conn,
        &split,
        &SearchOptions::default(),
        3,
        &latest,
        &mut |_batch| {
            batches += 1;
            // Simulate a new keystroke arriving after the first batch.
            latest_for_sink.store(99, Ordering::SeqCst);
        },
    )
    .unwrap();
    assert!(outcome.is_none(), "cancelled search must not complete");
    assert_eq!(batches, 1, "no further batches after the generation moved");

    drop(conn);
    std::fs::remove_file(&p).ok();
}

#[test]
fn service_rapid_fire_completes_only_the_last_generation() {
    let p = tmp_db("service");
    let mut s = Seeder::new(&p, true);
    for i in 0..2000 {
        s.add(
            &format!("file-{:04}.txt", i),
            "/d",
            1,
            Some("shared corpus body text"),
        );
    }
    drop(s.done());

    let (service, updates) = SearchService::new(p.clone(), Arc::new(|| {}));
    for i in 0..50 {
        service.search(&format!("corpus body {}", i % 3), SearchOptions::default());
    }
    // Final query that actually matches, so completion carries hits too.
    let last_gen = service.search("corpus", SearchOptions::default());

    let mut completed: Vec<u64> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match updates.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(SearchUpdate::Completed { generation, .. }) => {
                completed.push(generation);
                if generation == last_gen {
                    break;
                }
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert_eq!(
        completed.last().copied(),
        Some(last_gen),
        "the newest generation must be the one that completes (saw {:?})",
        completed
    );
    service.shutdown();

    std::fs::remove_file(&p).ok();
}

#[test]
fn service_reports_missing_db_as_error() {
    let missing = tmp_db("missing");
    let (service, updates) = SearchService::new(missing.clone(), Arc::new(|| {}));
    let generation = service.search("anything", SearchOptions::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut got_error = false;
    while std::time::Instant::now() < deadline {
        match updates.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(SearchUpdate::Error { generation: g, .. }) if g == generation => {
                got_error = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(got_error, "missing index must surface as a search error");
    service.shutdown();
}

/// The point of the whole change: a pass hands hits over *while* it scans, so
/// the UI has something to show long before the scan ends.
///
/// Proven by ordering rather than by batch count — `flush_pass` has always
/// chunked its output, so counting sink calls proves nothing. Pass A scans in
/// `files.path` order, so seeding a *worse* match at an early path and a
/// *better* one at a late path separates the two designs: emitting at the end
/// sorts them and leads with rank 1, while streaming hands over the rank-3 hit
/// before the scan has even reached the rank-1 one.
///
/// That is the trade being made deliberately: arrival order is scan order, and
/// ordering across batches belongs to the consumer.
#[test]
fn a_pass_hands_hits_over_before_the_scan_reaches_the_end() {
    let p = tmp_db("stream");
    let mut s = Seeder::new(&p, true);
    let early_worse = s.add("my_zebra_file.txt", "/aaa", 1, None); // rank 3
    for i in 0..400 {
        s.add(&format!("filler{:04}.txt", i), "/mmm", i as u64 + 2, None);
    }
    let late_better = s.add("zebra", "/zzz", 999, None); // rank 1
    let conn = s.done();

    let options = SearchOptions {
        batch: 100,
        ..SearchOptions::default()
    };
    let (batches, outcome) = run_collect_batches(&conn, "zebra", &options);

    assert_eq!(outcome.total, 2, "both matches still reach the sink");
    assert_eq!(
        batches
            .first()
            .map(|b| b.as_slice())
            .and_then(|b| b.first())
            .map(|h| h.file_id),
        Some(early_worse),
        "the early hit should have gone out before the scan found the better one; \
         batch sizes: {:?}",
        batches.iter().map(|b| b.len()).collect::<Vec<_>>()
    );

    // ...and sorting the stream the way the GUI does still puts rank 1 on top.
    let (hits, _) = run_collect(&conn, "zebra", &options);
    assert_eq!(
        hits.iter().map(|h| h.file_id).collect::<Vec<_>>(),
        vec![late_better, early_worse],
        "the consumer's sort restores rank order"
    );

    drop(conn);
    std::fs::remove_file(&p).ok();
}

/// The time bound, which a size-only rule would miss: a query matching a
/// handful of rows out of many still paints them as the scan reaches them
/// rather than at the end.
#[test]
fn a_sparse_match_still_streams_before_the_scan_ends() {
    let p = tmp_db("sparse");
    let mut s = Seeder::new(&p, true);
    // Three needles, spread through a haystack far larger than one batch.
    for i in 0..3000 {
        let name = if i % 1000 == 500 {
            format!("needle{:04}.txt", i)
        } else {
            format!("hay{:04}.txt", i)
        };
        s.add(&name, "/d", i as u64 + 1, Some("body"));
    }
    let conn = s.done();

    let options = SearchOptions {
        batch: 100,
        ..SearchOptions::default()
    };
    let (batches, outcome) = run_collect_batches(&conn, "needle", &options);

    assert_eq!(outcome.total, 3, "all three needles found");
    assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), 3);
    assert!(
        !batches.is_empty() && batches[0].len() < 3,
        "the first needle should not have waited for the other two; batches: {:?}",
        batches.iter().map(|b| b.len()).collect::<Vec<_>>()
    );
}

/// Streaming must not change *what* a search returns, only when. The rank
/// ordering assertions elsewhere in this file are the detailed version; this
/// pins the set and the count against a mixed-tier query.
#[test]
fn streaming_does_not_change_the_result_set() {
    let p = tmp_db("stream-set");
    let mut s = Seeder::new(&p, true);
    let exact = s.add("zebra", "/d", 1, Some("nothing here"));
    let sub = s.add("my_zebra_file.txt", "/d", 2, Some("nothing here"));
    let content = s.add("unrelated.txt", "/d", 3, Some("a zebra in the text"));
    let conn = s.done();

    for batch in [1usize, 2, 100] {
        let options = SearchOptions {
            batch,
            ..SearchOptions::default()
        };
        let (hits, outcome) = run_collect(&conn, "zebra", &options);
        let ids: Vec<i64> = hits.iter().map(|h| h.file_id).collect();
        assert_eq!(
            ids,
            vec![exact, sub, content],
            "batch size {} must not change ranking",
            batch
        );
        assert_eq!(outcome.total, 3, "batch size {}", batch);
    }
}
