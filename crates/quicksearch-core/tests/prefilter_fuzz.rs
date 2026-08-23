//! Fuzzing the cascade's prefilters.
//!
//! The generator manufactures queries whose correct answer is known **by
//! construction**: cut a substring from a document, corrupt it at most `k`
//! times (counted in **bytes**, the metric `Bitap` uses), and the document
//! it came from must still be found.
//!
//! The reference is a true oracle, not a differential against the
//! pre-prefilter code path: a bug in `Bitap` would be present in both paths
//! and a differential between them would agree on the wrong answer.
//!
//! ```text
//! cargo test --release -p quicksearch-core --test prefilter_fuzz
//! QSB_FUZZ_ITERS=2000 cargo test --release -p quicksearch-core \
//!     --test prefilter_fuzz -- --nocapture
//! ```

use std::sync::atomic::AtomicU64;

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::fuzzy::{edit_budget, pigeonhole_chunks, Bitap, TRIGRAM_FLOOR};
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

use quicksearch_core::testutil::Scratch;

mod common;
use common::Lcg;

const FUZZY_CONTENT_STAGE: u8 = 8;

const ITERS_PER_LEN: usize = 100;

const MAX_SWEEP: usize = 20;

/// Long terms, sampled rather than swept. 64 is `Bitap`'s pattern ceiling and
/// 80 is past it, where the fuzzy passes must decline to run at all.
const LONG_TAIL: [usize; 6] = [24, 32, 48, 64, 80, 200];

/// Edit caps to sweep; each moves the budget and the `3 × (cap + 1)` guard.
const CAPS: [usize; 5] = [0, 1, 2, 3, 4];

const BOUNDED_CAPS: usize = 3;

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

const DOCS: usize = 60;

/// Bodies the generator draws substrings from. Deliberately hostile, since
/// whatever is in the corpus goes straight into the term and the FTS chunks:
/// FTS5 metacharacters (must be quoted into inertness by
/// `translator::quote_phrase`), multi-byte characters at 2, 3 and 4 bytes,
/// diacritics (folded by the trigram index but not by bitap — the prefilter
/// must stay a superset across that), mixed case, and a degenerate
/// repeated-character shape.
fn bodies() -> Vec<String> {
    let mut out = Vec::new();
    let mut lcg = Lcg::new(0xf0072);
    let words = [
        "quartzite",
        "Report",
        "SUMMARY",
        "café",
        "naïve",
        "Ünicode",
        "日本語テキスト",
        "emoji🙂here",
        "NEAR",
        "wild*card",
        "colon:sep",
        "quote\"mark",
        "paren(then)",
        "dash-joined",
        "caret^up",
        "aaaaaaaaaa",
        "mixedCaseWord",
        "budget",
        "revenue",
        "planning",
    ];
    for d in 0..DOCS {
        let n = 20 + (lcg.next() as usize % 40);
        let mut body = String::new();
        for _ in 0..n {
            body.push_str(words[lcg.next() as usize % words.len()]);
            body.push(' ');
        }
        // Fixed shapes beside the random ones: all-one-character, and empty.
        match d {
            0 => body = "b".repeat(400),
            1 => body = String::new(),
            _ => {}
        }
        out.push(body);
    }
    out
}

// ---------------------------------------------------------------------------
// Oracles
// ---------------------------------------------------------------------------

/// Minimum Levenshtein distance between `pattern` and any substring of
/// `hay`, over **bytes**, ASCII-case-insensitively — the metric the matcher
/// uses. The shared brute-force reference.
fn oracle_distance(pattern: &[u8], hay: &[u8]) -> usize {
    if hay.is_empty() || pattern.is_empty() {
        return usize::MAX;
    }
    let m = pattern.len();
    let mut prev: Vec<usize> = vec![0; hay.len() + 1];
    let mut cur = vec![0; hay.len() + 1];
    for i in 1..=m {
        cur[0] = i;
        for j in 1..=hay.len() {
            let cost = usize::from(!pattern[i - 1].eq_ignore_ascii_case(&hay[j - 1]));
            cur[j] = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.iter().copied().min().unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

fn corrupt(chars: &mut Vec<char>, lcg: &mut Lcg) {
    // Includes multi-byte replacements: one character edit, several bytes.
    const REPLACEMENTS: [char; 6] = ['x', 'Q', '7', 'é', '語', '🙂'];
    let pick = REPLACEMENTS[lcg.next() as usize % REPLACEMENTS.len()];
    if chars.is_empty() {
        chars.push(pick);
        return;
    }
    let at = lcg.next() as usize % chars.len();
    match lcg.next() % 3 {
        0 => chars[at] = pick,       // substitution
        1 => chars.insert(at, pick), // insertion
        _ => {
            chars.remove(at); // deletion
        }
    }
}

fn substring_of<'a>(body: &'a str, len: usize, lcg: &mut Lcg) -> Option<&'a str> {
    let bounds: Vec<usize> = body
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(body.len()))
        .collect();
    let chars = bounds.len() - 1;
    if chars < len || len == 0 {
        return None;
    }
    let start = lcg.next() as usize % (chars - len + 1);
    Some(&body[bounds[start]..bounds[start + len]])
}

// ---------------------------------------------------------------------------
// Property 1: the pigeonhole invariant, with no database in sight
// ---------------------------------------------------------------------------

/// The pigeonhole argument the prefilter rests on, isolated from FTS, SQLite
/// and bitap: if the term occurs within `k` edits, some chunk of the
/// `k + 1`-way split occurs **verbatim**. Driven by the corrupt-a-substring
/// generator; a randomly drawn pair almost never aligns and would be vacuous.
#[test]
fn a_surviving_chunk_always_remains_after_k_edits() {
    let bodies = bodies();
    let mut lcg = Lcg::new(0xc0ffee);
    let mut exercised = 0usize;

    for &cap in &CAPS {
        for len in sweep_lengths() {
            for _ in 0..ITERS_PER_LEN {
                let body = &bodies[lcg.next() as usize % bodies.len()];
                let Some(original) = substring_of(body, len, &mut lcg) else {
                    continue;
                };
                let Some(k) = edit_budget(original.len(), cap) else {
                    continue;
                };
                let mut chars: Vec<char> = original.chars().collect();
                let edits = lcg.next() as usize % (k + 1);
                for _ in 0..edits {
                    corrupt(&mut chars, &mut lcg);
                }
                let term: String = chars.into_iter().collect();

                // Only assert within the byte budget — see the module note.
                let distance = oracle_distance(term.as_bytes(), body.as_bytes());
                let Some(chunks) = pigeonhole_chunks(&term, k) else {
                    continue;
                };
                if distance > k {
                    continue;
                }

                // The partition itself, checked every time: chunks that
                // overlapped or skipped text would break the argument silently.
                assert_eq!(chunks.concat(), term, "chunks must partition the term");
                assert_eq!(chunks.len(), k + 1, "one chunk per edit, plus one");
                assert!(
                    chunks.iter().all(|c| c.chars().count() >= TRIGRAM_FLOOR),
                    "every chunk must reach the trigram floor: {:?}",
                    chunks
                );

                let folded_body = body.to_lowercase();
                assert!(
                    chunks
                        .iter()
                        .any(|c| folded_body.contains(&c.to_lowercase())),
                    "no chunk of {:?} survived in the body it came from \
                     (k={}, distance={}, chunks={:?})",
                    term,
                    k,
                    distance,
                    chunks
                );
                exercised += 1;
            }
        }
    }

    assert!(
        exercised > 500,
        "the generator produced only {} in-budget cases; it is not exercising \
         the property",
        exercised
    );
}

/// Adversarial placement, which random corruption will almost never construct:
/// damage exactly `k` of the `k + 1` chunks, every combination, and check the
/// untouched one is still there to be found.
#[test]
fn damaging_every_chunk_but_one_leaves_that_one_intact() {
    for k in 0..=3usize {
        let term: String = (0..(k + 1) * 4)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let chunks = pigeonhole_chunks(&term, k).expect("long enough by construction");
        for spared in 0..chunks.len() {
            let mut text = String::new();
            for (i, chunk) in chunks.iter().enumerate() {
                if i == spared {
                    text.push_str(chunk);
                } else {
                    let mut c: Vec<char> = chunk.chars().collect();
                    c[0] = 'Z';
                    text.extend(c);
                }
            }
            assert!(
                chunks.iter().any(|c| text.contains(*c)),
                "k={} spared={} term={:?} text={:?}",
                k,
                spared,
                term,
                text
            );
            // The damaged text is within budget, so the pass must find it.
            assert!(
                oracle_distance(term.as_bytes(), text.as_bytes()) <= k,
                "the constructed text should be within {} edits",
                k
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2: end-to-end recall, three ways
// ---------------------------------------------------------------------------

/// Lengths the sweep visits: dense from 1, then a sparse tail.
fn sweep_lengths() -> Vec<usize> {
    (1..=MAX_SWEEP).chain(LONG_TAIL).collect()
}

fn iters_per_len() -> usize {
    std::env::var("QSB_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ITERS_PER_LEN)
}

fn caps() -> &'static [usize] {
    if std::env::var("QSB_FUZZ_ITERS").is_ok() {
        &CAPS
    } else {
        &CAPS[..BOUNDED_CAPS]
    }
}

/// Run one search and collect `(file_id, stage)` per hit, deduplicated
/// defensively by file id. `fuzzy_cap` turns the fuzzy passes on; `None`
/// leaves them off, which is the `regex:` configuration.
fn search_hits(
    conn: &rusqlite::Connection,
    query: &str,
    fuzzy_cap: Option<usize>,
) -> Vec<(i64, u8)> {
    let split = split_for_cascade(query).expect("any term parses; it degrades rather than errors");
    let latest = AtomicU64::new(1);
    let options = SearchOptions {
        fuzzy: fuzzy_cap.is_some(),
        fuzzy_max_edits: fuzzy_cap.unwrap_or_default(),
        limit: 100_000,
        ..SearchOptions::default()
    };
    let mut hits = Vec::new();
    let mut sink = |batch: Vec<SearchHit>| hits.extend(batch.iter().map(|h| (h.file_id, h.stage)));
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("the cascade runs");
    hits.sort();
    hits.dedup_by_key(|h| h.0);
    hits
}

fn search_ids(conn: &rusqlite::Connection, term: &str, cap: usize) -> Vec<i64> {
    search_hits(conn, term, Some(cap))
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

#[test]
fn a_corrupted_substring_still_finds_the_document_it_came_from() {
    let bodies = bodies();
    let (_dir, db) = Scratch::db("fuzzprefilter");
    let ids = seed(&db, &bodies);
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the seeded index");

    let mut lcg = Lcg::new(0x5ca1ab1e);
    let mut checked = 0usize;
    let mut below_floor = 0usize;

    for &cap in caps() {
        for len in sweep_lengths() {
            for _ in 0..iters_per_len() {
                let doc = lcg.next() as usize % bodies.len();
                let body = &bodies[doc];
                let Some(original) = substring_of(body, len, &mut lcg) else {
                    continue;
                };

                let mut chars: Vec<char> = original.chars().collect();
                let planned = edit_budget(original.len(), cap).unwrap_or(0);
                let edits = if planned == 0 {
                    0
                } else {
                    lcg.next() as usize % (planned + 1)
                };
                for _ in 0..edits {
                    corrupt(&mut chars, &mut lcg);
                }
                let term: String = chars.into_iter().collect();
                if term.trim().is_empty() {
                    continue;
                }

                // The cascade does not search the string typed at it: the
                // lexer strips and re-joins whitespace, makes `*` a wildcard
                // and `key:value` a filter. A term like `" Qi"` loses its
                // leading space and falls below the fuzzy floor — correct
                // behaviour that read as a lost hit when asserted against the
                // raw string. Every decision below uses the *parsed* term.
                let Ok(split) = split_for_cascade(&term) else {
                    // A term that parses to an error is not a fuzzy search.
                    continue;
                };
                if split.pattern.is_wildcard() || split.regex.is_some() {
                    // Wildcard terms don't fuzz: the passes decline outright.
                    continue;
                }
                let effective = split.term.as_str();

                let Some(k) = edit_budget(effective.len(), cap) else {
                    // Below the fuzzy floor or past Bitap's ceiling: the pass must not run.
                    below_floor += 1;
                    let _ = search_ids(&conn, &term, cap);
                    continue;
                };
                if Bitap::new(effective.as_bytes(), k).is_none() {
                    let _ = search_ids(&conn, &term, cap);
                    continue;
                }

                // The budget is over bytes; only assert recall within it.
                if oracle_distance(effective.as_bytes(), body.as_bytes()) > k {
                    continue;
                }

                let found = search_ids(&conn, &term, cap);
                assert!(
                    found.contains(&ids[doc]),
                    "lost the document the term came from\n  typed     {:?}\n  \
                     parsed    {:?}\n  original  {:?}\n  cap {} k {} edits {}\n  \
                     chunks    {:?}\n  body      {:?}",
                    term,
                    effective,
                    original,
                    cap,
                    k,
                    edits,
                    pigeonhole_chunks(effective, k),
                    &body.chars().take(120).collect::<String>(),
                );
                checked += 1;
            }
        }
    }

    eprintln!(
        "fuzzy prefilter fuzz: {} recall assertions, {} terms below the fuzzy floor",
        checked, below_floor
    );
    assert!(
        checked > 200,
        "only {} recall assertions ran; the generator is not producing \
         in-budget terms",
        checked
    );
}

/// The two-sided property, against a brute-force oracle over the **whole**
/// corpus:
///
/// * **No lost hits** — every document within `k` edits appears; the
///   direction the prefilter can break, with no visible symptom.
/// * **No invented hits** — every stage-8 hit is within `k` edits; stage 8
///   exactly, since a filename or path pass may legitimately surface a
///   document whose *body* is outside the budget.
#[test]
fn every_document_within_the_budget_is_found_and_nothing_outside_it_is() {
    const ITERS: usize = 5;

    let bodies = bodies();
    let (_dir, db) = Scratch::db("fuzzprefilter-oracle");
    let ids = seed(&db, &bodies);
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the seeded index");

    let mut lcg = Lcg::new(0xd1ce);
    let mut compared = 0usize;

    for &cap in caps() {
        for len in sweep_lengths() {
            for _ in 0..ITERS {
                let doc = lcg.next() as usize % bodies.len();
                let Some(original) = substring_of(&bodies[doc], len, &mut lcg) else {
                    continue;
                };
                let mut chars: Vec<char> = original.chars().collect();
                let planned = edit_budget(original.len(), cap).unwrap_or(0);
                let edits = if planned == 0 {
                    0
                } else {
                    lcg.next() as usize % (planned + 1)
                };
                for _ in 0..edits {
                    corrupt(&mut chars, &mut lcg);
                }
                let term: String = chars.into_iter().collect();
                if term.trim().is_empty() {
                    continue;
                }
                let Ok(split) = split_for_cascade(&term) else {
                    continue;
                };
                if split.pattern.is_wildcard() || split.regex.is_some() {
                    continue;
                }
                let effective = split.term.as_str();
                let Some(k) = edit_budget(effective.len(), cap) else {
                    continue;
                };
                if Bitap::new(effective.as_bytes(), k).is_none() {
                    continue;
                }

                let found = search_hits(&conn, &term, Some(cap));

                for (i, body) in bodies.iter().enumerate() {
                    let within = oracle_distance(effective.as_bytes(), body.as_bytes()) <= k;
                    if within {
                        assert!(
                            found.iter().any(|&(id, _)| id == ids[i]),
                            "lost a hit the oracle says is within {} edits\n  \
                             parsed {:?}\n  doc {} {:?}\n  chunks {:?}",
                            k,
                            effective,
                            i,
                            &body.chars().take(80).collect::<String>(),
                            pigeonhole_chunks(effective, k),
                        );
                    }
                }

                // Precision, restricted to stage 8: the prefilter is only a
                // superset; verification must reject the rest.
                for &(id, stage) in &found {
                    if stage != FUZZY_CONTENT_STAGE {
                        continue;
                    }
                    let i = ids
                        .iter()
                        .position(|&seeded| seeded == id)
                        .expect("every hit refers to a seeded row");
                    let distance = oracle_distance(effective.as_bytes(), bodies[i].as_bytes());
                    assert!(
                        distance <= k,
                        "invented a stage-{} hit: doc {} is {} edits from {:?} \
                         (k={})\n  body {:?}",
                        FUZZY_CONTENT_STAGE,
                        i,
                        distance,
                        effective,
                        k,
                        &bodies[i].chars().take(80).collect::<String>(),
                    );
                }
                compared += 1;
            }
        }
    }

    eprintln!(
        "prefilter oracle: {} queries compared corpus-wide",
        compared
    );
    assert!(compared > 50, "only {} queries compared", compared);
}

// ---------------------------------------------------------------------------
// The regex prefilter
// ---------------------------------------------------------------------------

/// Rewrite `sub` into a regex that still matches it: loosen the *pattern* in
/// ways that provably preserve the match, so the source document stays a
/// known-correct answer. The point is to reach patterns whose
/// required-literal set is interesting: a class or alternation splits one
/// literal into several, `?` splits the set into with/without, and a leading
/// `.*` destroys the prefix set so only the suffix set can save it.
fn regexify(sub: &str, lcg: &mut Lcg) -> String {
    let esc = |c: char| regex::escape(&c.to_string());
    let mut out = String::new();
    for c in sub.chars() {
        // Most characters stay literal, or every literal set goes empty.
        match lcg.next() % 10 {
            0 if c != '\n' => out.push('.'),
            1 => out.push_str(&format!("[{}z]", esc(c))),
            2 => out.push_str(&format!("(?:{}|zzq)", esc(c))),
            3 => out.push_str(&format!("{}?", esc(c))),
            4 => out.push_str(&format!("{}+", esc(c))),
            _ => out.push_str(&esc(c)),
        }
    }
    match lcg.next() % 6 {
        0 => format!(".*{out}"),
        1 => format!("{out}.*"),
        _ => out,
    }
}

/// Every document the regex matches is found, and nothing else is. Set
/// equality pins the prefilter from both sides: it may not lose a row, and
/// the rows it lets through must still be verified rather than admitted on
/// the strength of holding a literal.
#[test]
fn a_regex_finds_exactly_the_documents_it_matches() {
    let bodies = bodies();
    let (_dir, db) = Scratch::db("prefilter-regex");
    let ids = seed(&db, &bodies);
    let conn = quicksearch_core::db::open::open_search_reader(&db.to_string_lossy())
        .expect("open the seeded index");

    let mut lcg = Lcg::new(0xb0a7);
    let mut checked = 0usize;
    let mut with_prefilter = 0usize;

    for len in sweep_lengths() {
        for _ in 0..iters_per_len().min(40) {
            let doc = lcg.next() as usize % bodies.len();
            let Some(sub) = substring_of(&bodies[doc], len, &mut lcg) else {
                continue;
            };
            let pattern = regexify(sub, &mut lcg);
            let query = format!("regex:\"{}\"", pattern);
            // Anything the parser does not hand back verbatim is not a test
            // of the prefilter; round-tripping is the cheapest way to say so.
            let Ok(split) = split_for_cascade(&query) else {
                continue;
            };
            let Some(re) = split.regex.as_ref() else {
                continue;
            };
            if re.source != pattern {
                continue;
            }
            if re.required().is_some() {
                with_prefilter += 1;
            }

            // The oracle: the pattern may match any of the three fields.
            let mut want: Vec<i64> = Vec::new();
            for (i, body) in bodies.iter().enumerate() {
                let name = format!("row{:04}.bin", i);
                let path = format!("/fuzz/{}", name);
                if re.is_match(body) || re.is_match(&name) || re.is_match(&path) {
                    want.push(ids[i]);
                }
            }
            want.sort();

            let got: Vec<i64> = search_hits(&conn, &query, None)
                .into_iter()
                .map(|(id, _)| id)
                .collect();

            assert_eq!(
                got,
                want,
                "\n  pattern  {:?}\n  from     {:?}\n  literals {:?}",
                pattern,
                sub,
                re.required().map(|r| r.literals().to_vec()),
            );
            checked += 1;
        }
    }

    eprintln!(
        "regex prefilter: {} patterns compared corpus-wide, {} of them prefiltered",
        checked, with_prefilter
    );
    assert!(checked > 100, "only {} patterns compared", checked);
    assert!(
        with_prefilter > checked / 4,
        "only {} of {} patterns produced a prefilter; the generator is not \
         reaching the path under test",
        with_prefilter,
        checked
    );
}

fn seed(path: &std::path::Path, bodies: &[String]) -> Vec<i64> {
    use quicksearch_core::db::repo::{insert_file, set_content_done, NewFile};
    use quicksearch_core::mime::FileType;
    use quicksearch_core::testutil::zstd_of;

    let mut conn =
        quicksearch_core::db::open_or_recreate(path.to_str().unwrap(), "trigram").unwrap();
    let tx = conn.transaction().unwrap();
    let mut ids = Vec::with_capacity(bodies.len());
    for (i, body) in bodies.iter().enumerate() {
        let id = insert_file(
            &tx,
            &NewFile {
                // Names deliberately share nothing with the bodies, so a hit
                // can only come from the full-text pass.
                name: &format!("row{:04}.bin", i),
                parent: "/fuzz/",
                size: body.len() as u64,
                mtime: 1_700_000_000,
                mime: Some("text/plain"),
                ftype: FileType::TEXT,
                hash: None,
                needs_content: true,
            },
        )
        .unwrap()
        .expect("unique path");
        set_content_done(&tx, id, body, zstd_of(body).as_deref()).unwrap();
        ids.push(id);
    }
    tx.commit().unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    ids
}
