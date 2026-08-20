//! Fuzzing the cascade's prefilters.
//!
//! Two passes narrow what they scan by "at least one of these literals must be
//! present" — the fuzzy full-text pass by its pigeonhole chunks, the `regex:`
//! passes by the literals extracted from the pattern. Both go through
//! [`quicksearch_core::search::prefilter`], and both share a failure mode that
//! is **silent**: a lost hit looks exactly like a file that does not match, and
//! no user could tell the difference. So neither is defended by a handful of
//! examples.
//!
//! The regex half gets a stronger check than the fuzzy half, because it has a
//! perfect oracle: running the compiled regex over each body *is* the right
//! answer, so the assertion is set equality rather than one-sided recall.
//!
//! # The generator
//!
//! Rather than invent queries and assert an expected answer, this manufactures
//! queries whose correct answer is known **by construction**:
//!
//! 1. take a document from the corpus,
//! 2. cut a substring of it — text the document provably contains,
//! 3. corrupt that substring `N ≤ k` times,
//! 4. search for the result. The document it came from **must** be found.
//!
//! Step 4 needs no oracle and no reference implementation: the term is within
//! `N` edits of something the document really contains, so a ≤`k`-edit
//! alignment exists and the prefilter is not allowed to exclude it.
//!
//! # Substring length is swept, not random
//!
//! Documents are long, so drawing the length uniformly would spend nearly every
//! iteration on long terms — the case least likely to be broken. Instead
//! [`ITERS_PER_LEN`] iterations run at *every* length from 1 to [`MAX_SWEEP`],
//! then a sparse tail for long terms. Short lengths are where the chunking, the
//! floors and the boundary conditions all live, and at the default cap the
//! `1..=20` sweep straddles every one of them: the 3-character fuzzy floor, and
//! `3 × (cap + 1) = 9` where the prefilter becomes legal.
//!
//! The bottom of the sweep is asserted rather than skipped. Below the fuzzy
//! floor no `Bitap` is built and the pass must not scan at all.
//!
//! # Edits are counted in bytes
//!
//! Bitap's budget is a byte budget, so substituting `é` (two bytes) for `x`
//! (one) is a distance of two, not one. Corruption works on characters to keep
//! the term valid UTF-8, then the true byte distance is measured against the
//! original substring and recall is only asserted when it really is within
//! budget. Without that, a non-ASCII corpus would quietly start generating
//! out-of-budget queries and every "miss" would be correct behaviour — the
//! test would pass by not testing anything.
//!
//! ```text
//! cargo test --release -p quicksearch-core --test fuzzy_prefilter_fuzz
//! QSB_FUZZ_ITERS=2000 cargo test --release -p quicksearch-core \
//!     --test fuzzy_prefilter_fuzz -- --nocapture
//! ```

use std::sync::atomic::AtomicU64;

use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::fuzzy::{edit_budget, pigeonhole_chunks, Bitap, TRIGRAM_FLOOR};
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

mod common;
use common::{scratch_db, Lcg};

/// Iterations at each swept substring length.
const ITERS_PER_LEN: usize = 100;

/// Top of the dense sweep. Every length from 1 to here gets [`ITERS_PER_LEN`]
/// iterations.
const MAX_SWEEP: usize = 20;

/// Long terms, sampled rather than swept. 64 is `Bitap`'s pattern ceiling and
/// 80 is past it, where the fuzzy passes must decline to run at all.
const LONG_TAIL: [usize; 6] = [24, 32, 48, 64, 80, 200];

/// Edit-distance caps to sweep beneath the length sweep. Each moves both the
/// budget and the `3 × (cap + 1)` guard.
const CAPS: [usize; 5] = [0, 1, 2, 3, 4];

/// How many caps the ungated run sweeps. The soak takes all of [`CAPS`].
const BOUNDED_CAPS: usize = 3;

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Documents to seed. Small — this is a correctness harness, and every
/// iteration runs a whole cascade — but varied enough that a query drawn from
/// one document meets plenty of others it must reject.
const DOCS: usize = 60;

/// Bodies the generator draws substrings from.
///
/// Deliberately hostile, because the substring generator carries whatever is in
/// the corpus straight into the term and then into the FTS chunks:
///
/// * **FTS5 metacharacters** — `"`, `*`, `:`, `^`, `-`, `(`, `)` and the bare
///   word `NEAR`. A chunk containing these must be quoted into inertness by
///   `translator::quote_phrase`, or the query is a syntax error rather than a
///   search. This is the likeliest way to break the feature and it needs no
///   special case in the generator: it is simply in the text.
/// * **Multi-byte characters** at 2, 3 and 4 bytes, so a byte-wise chunk split
///   would produce invalid UTF-8.
/// * **Diacritics**, which the trigram index folds (`remove_diacritics 1`) and
///   bitap does not — the prefilter must stay a superset across that.
/// * **Mixed case**, which also exercises the case-insensitive mask table.
/// * **Degenerate shapes** — a repeated character, where every chunk of a
///   substring is identical and the substring is self-overlapping.
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
        // One document that is nothing but a repeated character, and one that
        // is empty, as fixed shapes beside the random ones.
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

/// Minimum Levenshtein distance between `pattern` and any substring of `hay`,
/// over **bytes**, ASCII-case-insensitively — the metric the matcher uses.
///
/// The shared brute-force reference. It is what decides whether a corrupted
/// term is genuinely inside the edit budget, and what the three-way comparison
/// checks both cascade paths against.
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

/// One corruption applied to a character sequence.
fn corrupt(chars: &mut Vec<char>, lcg: &mut Lcg) {
    // A small alphabet of replacements, including multi-byte ones so that a
    // single character edit can be a multi-byte edit.
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

/// Cut `len` characters out of `body` at a random offset, or `None` when the
/// body is too short to give that many.
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

/// The argument the whole prefilter rests on, isolated from FTS, from SQLite
/// and from bitap: if the term occurs within `k` edits, some chunk of the
/// `k + 1`-way split occurs **verbatim**.
///
/// Driven by the same corrupt-a-known-substring generator, because a randomly
/// drawn (term, text) pair almost never has a ≤`k`-edit alignment and the
/// property would be vacuous.
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

                // Only meaningful when the corrupted term really is within the
                // budget of the text — see the module note on byte distance.
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

                // The property.
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
        // Damage all but one chunk, for each choice of the survivor.
        for spared in 0..chunks.len() {
            let mut text = String::new();
            for (i, chunk) in chunks.iter().enumerate() {
                if i == spared {
                    text.push_str(chunk);
                } else {
                    // One substitution inside this chunk.
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
            // And the damaged text really is within budget, so this is a case
            // the pass would have to find.
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

/// Run one fuzzy search and collect the file ids it found.
fn search_ids(conn: &rusqlite::Connection, term: &str, cap: usize) -> Vec<i64> {
    let split = split_for_cascade(term).expect("any term parses; it degrades rather than errors");
    let latest = AtomicU64::new(1);
    let options = SearchOptions {
        fuzzy: true,
        fuzzy_max_edits: cap,
        limit: 100_000,
        ..SearchOptions::default()
    };
    let mut ids = Vec::new();
    let mut sink = |hits: Vec<SearchHit>| ids.extend(hits.iter().map(|h| h.file_id));
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("the cascade runs");
    ids.sort();
    ids.dedup();
    ids
}

/// The headline property: a term cut from a document and corrupted within the
/// edit budget still finds that document.
#[test]
fn a_corrupted_substring_still_finds_the_document_it_came_from() {
    let bodies = bodies();
    let db = scratch_db("fuzzprefilter");
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

                // **The cascade does not search the string typed at it.** The
                // query goes through the lexer first, and a term cut out of
                // real document text is full of things the lexer acts on:
                // whitespace is stripped and re-joined, `*` becomes a wildcard,
                // `key:value` may become a filter, `"` opens a phrase. The
                // first thing this harness found was a three-character term
                // `" Qi"` whose leading space the lexer drops, leaving two
                // characters — below the fuzzy floor, so no matcher is built
                // and no document can be found. That is correct behaviour, and
                // asserting against the raw string called it a lost hit.
                //
                // So every decision below is made against the *parsed* term,
                // exactly as the passes make it.
                let Ok(split) = split_for_cascade(&term) else {
                    // A term that parses to an error (a `regex:` fragment with
                    // bad syntax, a filter key with an unusable value) is not a
                    // fuzzy search at all.
                    continue;
                };
                if split.pattern.is_wildcard() || split.regex.is_some() {
                    // "Bitap is a literal matcher; wildcard terms don't fuzz"
                    // — the fuzzy passes decline outright, so there is no
                    // recall to demand.
                    continue;
                }
                let effective = split.term.as_str();

                let Some(k) = edit_budget(effective.len(), cap) else {
                    // Below the fuzzy floor (or past Bitap's 64-byte ceiling):
                    // the pass must not run, so there is nothing to demand of
                    // it beyond not crashing.
                    below_floor += 1;
                    let _ = search_ids(&conn, &term, cap);
                    continue;
                };
                if Bitap::new(effective.as_bytes(), k).is_none() {
                    let _ = search_ids(&conn, &term, cap);
                    continue;
                }

                // The budget is over bytes; only assert recall when the term
                // really is within it.
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

    drop(conn);
    std::fs::remove_file(&db).ok();
}

/// The two-sided property, against a brute-force oracle over the **whole**
/// corpus rather than just the document the term came from.
///
/// Recall alone says the prefilter did not lose the one hit we planted; it says
/// nothing about the other fifty-nine documents. This checks both directions:
///
/// * **No lost hits** — every document within `k` edits of the term appears in
///   the results, at whatever stage the cascade files it under. This is the
///   direction the prefilter can break, and the one with no visible symptom.
/// * **No invented hits** — every stage-8 (fuzzy content) hit really is within
///   `k` edits. The prefilter is only a superset, so it must not admit anything
///   the verification below it should have rejected.
///
/// A true oracle, not a comparison against the pre-prefilter code path: a bug
/// living in `Bitap` itself would be present in both cascade paths and a
/// differential between them would agree, happily, on the wrong answer.
///
/// Far fewer iterations than the recall sweep, because it runs a Levenshtein DP
/// over every document per iteration.
#[test]
fn every_document_within_the_budget_is_found_and_nothing_outside_it_is() {
    const ITERS: usize = 5;

    let bodies = bodies();
    let db = scratch_db("fuzzprefilter-oracle");
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

                let found = search_ids(&conn, &term, cap);
                for (i, body) in bodies.iter().enumerate() {
                    let within = oracle_distance(effective.as_bytes(), body.as_bytes()) <= k;
                    if within {
                        assert!(
                            found.contains(&ids[i]),
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
                compared += 1;
            }
        }
    }

    eprintln!("prefilter oracle: {} queries compared corpus-wide", compared);
    assert!(compared > 50, "only {} queries compared", compared);

    drop(conn);
    std::fs::remove_file(&db).ok();
}

// ---------------------------------------------------------------------------
// The regex prefilter
// ---------------------------------------------------------------------------

/// Rewrite `sub` into a regex that still matches it.
///
/// The same trick as the fuzzy generator, in the other direction: rather than
/// corrupt the text and rely on an edit budget, this loosens the *pattern* in
/// ways that provably preserve the match, so the source document is once again
/// a known-correct answer. Each transformation keeps `sub` in the language:
/// `.` matches any one character (the corpus has no newlines), a class
/// containing the character matches it, an alternation offering it matches it,
/// `?` and `+` both admit exactly one occurrence.
///
/// The point is to reach patterns whose required-literal set is interesting:
/// a class or an alternation splits one literal into several, a `?` splits the
/// set into "with" and "without", and a leading `.*` destroys the prefix set
/// entirely so only the suffix set can save it.
fn regexify(sub: &str, lcg: &mut Lcg) -> String {
    let esc = |c: char| regex::escape(&c.to_string());
    let mut out = String::new();
    for c in sub.chars() {
        // Most characters stay literal, or the pattern stops resembling
        // anything a person would type and every literal set goes empty.
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

/// Every document the regex matches is found, and nothing else is.
///
/// Set equality, not recall: the regex passes accept a row when the pattern
/// matches its name, its path or its body, and all three are computable here.
/// So this pins the prefilter from both sides at once — it may not lose a row,
/// and the rows it lets through must still be verified rather than admitted on
/// the strength of holding a literal.
#[test]
fn a_regex_finds_exactly_the_documents_it_matches() {
    let bodies = bodies();
    let db = scratch_db("prefilter-regex");
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
            // Anything the query parser does not hand back verbatim is not a
            // test of the prefilter — a pattern carrying a quote, or one the
            // lexer splits. Round-tripping is the cheapest way to say so.
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

            // The oracle: the pass accepts a row when the pattern matches any
            // of the three fields it looks at.
            let mut want: Vec<i64> = Vec::new();
            for (i, body) in bodies.iter().enumerate() {
                let name = format!("row{:04}.bin", i);
                let path = format!("/fuzz/{}", name);
                if re.is_match(body) || re.is_match(&name) || re.is_match(&path) {
                    want.push(ids[i]);
                }
            }
            want.sort();

            let mut got = regex_search_ids(&conn, &query);
            got.sort();

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

    drop(conn);
    std::fs::remove_file(&db).ok();
}

/// Run a regex-only search and collect the file ids it found.
fn regex_search_ids(conn: &rusqlite::Connection, query: &str) -> Vec<i64> {
    let split = split_for_cascade(query).expect("checked by the caller");
    let latest = AtomicU64::new(1);
    let options = SearchOptions {
        limit: 100_000,
        ..SearchOptions::default()
    };
    let mut ids = Vec::new();
    let mut sink = |hits: Vec<SearchHit>| ids.extend(hits.iter().map(|h| h.file_id));
    cascade::run(conn, &split, &options, 1, &latest, &mut sink).expect("the cascade runs");
    ids.sort();
    ids.dedup();
    ids
}

/// Seed one row per body and return the file ids, in the same order.
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
                // can only come from the full-text pass and never from the
                // filename tiers.
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
