//! Side-by-side timing for snippet rendering: SQLite's built-in
//! `snippet()` against a regular FTS5 table, vs our zstd-compressed
//! sidecar + Rust renderer.
//!
//! Not a micro-benchmark — we care about end-to-end cost per result page
//! (FTS match + text retrieval + snippet construction) rather than the
//! renderer in isolation. Both paths are run against the same on-disk DB
//! seeded with the same prose, and each query is timed end-to-end from
//! `Connection::prepare` through the final snippet string.
//!
//! Gated by the `QSB_SNIPPET_PERF` env var so the test harness doesn't
//! pay the ~1 s seed cost on every `cargo test`. To run it:
//!
//! ```
//! QSB_SNIPPET_PERF=1 cargo test --release -p quicksearch-core \
//!     --test snippet_perf -- --nocapture
//! ```

use std::time::Instant;

use quicksearch_core::snippet;
use rusqlite::{params, Connection};

const NUM_DOCS: usize = 1000;
const PAGE_SIZE: usize = 50;

/// Word list we draw text from. Has enough variety that trigram posting
/// lists stay non-trivial (hundreds of terms, not "the" 10000 times).
const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "eta",
    "theta",
    "iota",
    "kappa",
    "lambda",
    "mu",
    "nu",
    "xi",
    "omicron",
    "pi",
    "rho",
    "sigma",
    "tau",
    "upsilon",
    "phi",
    "chi",
    "psi",
    "omega",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "rust",
    "cargo",
    "sqlite",
    "baloo",
    "indexer",
    "tokenizer",
    "trigram",
    "contentless",
    "posting",
    "fts5",
    "snippet",
    "highlight",
    "morning",
    "afternoon",
    "evening",
    "midnight",
    "yesterday",
    "today",
    "ocean",
    "forest",
    "mountain",
    "river",
    "valley",
    "bridge",
    "tunnel",
    "tokyo",
    "paris",
    "london",
    "berlin",
    "rome",
    "madrid",
    "vienna",
];

/// Query terms that appear in the seeded corpus, so every query returns
/// real hits (not zero rows, which would skew against both paths equally
/// but wouldn't exercise the snippet renderer at all).
const QUERIES: &[&str] = &[
    "quick",
    "rust",
    "baloo",
    "morning",
    "paris",
    "tokyo",
    "forest",
    "indexer",
    "contentless",
    "trigram",
];

fn seed_text(rng: &mut u64, target_words: usize) -> String {
    let mut out = String::with_capacity(target_words * 6);
    for _ in 0..target_words {
        // xorshift64 — cheap, portable, good enough for text generation.
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let w = WORDS[(*rng as usize) % WORDS.len()];
        out.push_str(w);
        out.push(' ');
    }
    out
}

mod common;
use common::scratch_db as tmp_path;

#[test]
fn snippet_paths_perf_comparison() {
    if std::env::var("QSB_SNIPPET_PERF").is_err() {
        eprintln!("skipping: set QSB_SNIPPET_PERF=1 to run");
        return;
    }

    let p = tmp_path("both");
    let conn = Connection::open(&p).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = 0;
         PRAGMA temp_store = MEMORY;",
    )
    .unwrap();

    // Path A: the 'old' shape — regular FTS5 with stored text, snippet()
    // built into SQLite. This is what our search SQL used to run before
    // schema v3.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE st_regular USING fts5(
            name, text,
            tokenize='trigram remove_diacritics 1'
         );",
    )
    .unwrap();

    // Path B: the new shape — contentless FTS5 + zstd-compressed sidecar.
    // Matches the real schema. We rebuild it in-place here so perf is
    // measured against the same DB layout production runs against.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE st_contentless USING fts5(
            text,
            tokenize='trigram remove_diacritics 1',
            content='',
            contentless_delete=1
         );
         CREATE TABLE documents_text (
            file_id    INTEGER PRIMARY KEY,
            text_zstd  BLOB NOT NULL
         );",
    )
    .unwrap();

    // Seed both tables with identical content.
    let seed_start = Instant::now();
    let mut rng: u64 = 0x1234_5678_9ABC_DEF0;
    {
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut ins_reg = tx
                .prepare("INSERT INTO st_regular(rowid, name, text) VALUES (?1, ?2, ?3)")
                .unwrap();
            let mut ins_con = tx
                .prepare("INSERT INTO st_contentless(rowid, text) VALUES (?1, ?2)")
                .unwrap();
            let mut ins_blob = tx
                .prepare("INSERT INTO documents_text(file_id, text_zstd) VALUES (?1, ?2)")
                .unwrap();
            for i in 1..=NUM_DOCS {
                let target = 50 + ((rng as usize) % 400);
                let text = seed_text(&mut rng, target);
                let name = format!("doc_{:05}.txt", i);
                ins_reg.execute(params![i as i64, &name, &text]).unwrap();
                ins_con.execute(params![i as i64, &text]).unwrap();
                let compressed = zstd::encode_all(text.as_bytes(), 3).unwrap();
                ins_blob.execute(params![i as i64, &compressed]).unwrap();
            }
        }
        tx.commit().expect("seed commit");
    }
    eprintln!(
        "seeded {} docs in both FTS5 shapes in {:.2?}",
        NUM_DOCS,
        seed_start.elapsed()
    );

    // Warm each table's page cache so the first run doesn't skew.
    for q in QUERIES.iter().take(2) {
        let mut s = conn
            .prepare("SELECT rowid FROM st_regular WHERE st_regular MATCH ?1 LIMIT 50")
            .unwrap();
        let _ = s
            .query_map(params![q], |r| r.get::<_, i64>(0))
            .unwrap()
            .count();
        let mut s = conn
            .prepare("SELECT rowid FROM st_contentless WHERE st_contentless MATCH ?1 LIMIT 50")
            .unwrap();
        let _ = s
            .query_map(params![q], |r| r.get::<_, i64>(0))
            .unwrap()
            .count();
    }

    // Path A: SQLite's built-in snippet() on a regular FTS5 table.
    let a_reps = 10;
    let start_a = Instant::now();
    let mut rows_a_total = 0usize;
    for _ in 0..a_reps {
        for q in QUERIES {
            let mut stmt = conn
                .prepare(
                    "SELECT rowid, name, snippet(st_regular, 1, '<b>', '</b>', '...', 64) \
                     FROM st_regular WHERE st_regular MATCH ?1 \
                     ORDER BY rank LIMIT ?2",
                )
                .unwrap();
            let rows = stmt
                .query_map(params![q, PAGE_SIZE as i64], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .unwrap();
            for r in rows {
                let _ = r.unwrap();
                rows_a_total += 1;
            }
        }
    }
    let dur_a = start_a.elapsed();

    // Path B: contentless FTS match → pull text_zstd → decompress → render.
    let b_reps = 10;
    let start_b = Instant::now();
    let mut rows_b_total = 0usize;
    let opts = snippet::Options { approx_chars: 64 };
    for _ in 0..b_reps {
        for q in QUERIES {
            // Contentless FTS5 returns NULL for stored columns (that's the
            // point of contentless). The real search SQL joins to `files`
            // for name/path; here we don't need those fields — we're
            // timing the snippet pipeline, not the row projection.
            let mut stmt = conn
                .prepare(
                    "SELECT st.rowid, dt.text_zstd \
                     FROM st_contentless AS st \
                     LEFT JOIN documents_text dt ON dt.file_id = st.rowid \
                     WHERE st_contentless MATCH ?1 \
                     ORDER BY rank LIMIT ?2",
                )
                .unwrap();
            let rows = stmt
                .query_map(params![q, PAGE_SIZE as i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
                })
                .unwrap();
            for row in rows {
                let (_rowid, blob) = row.unwrap();
                let text = match blob {
                    Some(b) => {
                        let raw = zstd::decode_all(b.as_slice()).unwrap();
                        String::from_utf8(raw).unwrap()
                    }
                    None => String::new(),
                };
                let folded = text.to_ascii_lowercase();
                let _snip = snippet::extract_folded(&text, &folded, &[q], &opts);
                rows_b_total += 1;
            }
        }
    }
    let dur_b = start_b.elapsed();

    // Report — `cargo test -- --nocapture` surfaces this.
    let a_per_query = dur_a.as_secs_f64() / (a_reps * QUERIES.len()) as f64 * 1000.0;
    let b_per_query = dur_b.as_secs_f64() / (b_reps * QUERIES.len()) as f64 * 1000.0;
    let a_per_row = dur_a.as_secs_f64() / rows_a_total as f64 * 1_000_000.0;
    let b_per_row = dur_b.as_secs_f64() / rows_b_total as f64 * 1_000_000.0;
    eprintln!();
    eprintln!(
        "snippet perf (NUM_DOCS={NUM_DOCS}, PAGE_SIZE={PAGE_SIZE}, QUERIES={}, reps={a_reps}):",
        QUERIES.len()
    );
    eprintln!(
        "  A (SQLite snippet(), regular FTS5):       {:.2?} total, {:.2} ms/query, {:.1} µs/row ({} rows)",
        dur_a, a_per_query, a_per_row, rows_a_total
    );
    eprintln!(
        "  B (contentless + zstd + Rust snippet):    {:.2?} total, {:.2} ms/query, {:.1} µs/row ({} rows)",
        dur_b, b_per_query, b_per_row, rows_b_total
    );
    eprintln!(
        "  ratio B/A: {:.2}x (>1 means our path is slower)",
        b_per_query / a_per_query
    );

    drop(conn);
    let _ = std::fs::remove_file(&p);
}
