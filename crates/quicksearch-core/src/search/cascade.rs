//! The ranked search cascade.
//!
//! One term, four table scans, eleven ranks. Rank base = stage number:
//!
//! | rank | meaning                          | scan |
//! |-----:|----------------------------------|------|
//! |  1.x | exact filename, exact case       | A    |
//! |  2.x | exact filename, any case         | A    |
//! |  3.x | filename substring, exact case   | A    |
//! |  4.x | filename substring, any case     | A    |
//! |  5.x | full text occurrence, exact case | B    |
//! |  6.x | full text occurrence, any case   | B    |
//! |  7.x | fuzzy filename                   | C    |
//! |  8.x | fuzzy full text                  | D    |
//! |  9.x | full path substring, exact case  | A    |
//! | 10.x | full path substring, any case    | A    |
//! | 11.x | fuzzy full path                  | C    |
//!
//! Pass A is one `files` scan classified per-row in Rust; pass B one FTS
//! MATCH verified against the text; passes C/D (opt-in) bitap-scan the table.
//! Path tiers are buffered and flushed last.
//!
//! Batches are ordered within themselves, not against each other: arrival
//! order is not rank order.

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension};

use crate::config::IgnoreSet;
use crate::query::pattern::clamp_match_range;
use crate::query::split::CascadeQuery;
use crate::query::translator::escape_like;
use crate::snippet;

use super::fuzzy::{matcher_for, pigeonhole_chunks, Bitap};
use super::{prefilter, SearchHit, SearchOptions};

mod passes;

/// Cancellation check stride for row-cheap passes; decompression-heavy passes check every row.
const CANCEL_CHECK_ROWS: usize = 256;

/// Snippet window budget: the GUI trims to column width; mouseover shows the rest.
const SNIPPET_WINDOW_BYTES: usize = 600;

const SNIPPET_OPTS: snippet::Options = snippet::Options {
    approx_bytes: SNIPPET_WINDOW_BYTES,
};

/// The Content Match snippet for one document body, cut exactly as the
/// full-text passes cut it; shared with [`crate::live`] so a re-cut matches.
/// `folded` must be `text` ASCII-lowercased; the fold preserves byte length,
/// so offsets found in it slice `text`.
pub fn text_snippet(
    pattern: &crate::query::pattern::TermPattern,
    text: &str,
    folded: &str,
) -> Option<snippet::Snippet> {
    text_snippet_counted(pattern, text, folded)
        .map(|(snip, _)| snip)
        .or_else(|| {
            pattern.find_first_folded(folded).map(|r| {
                // A greedy pattern can match megabytes; clamp before the window.
                let r = clamp_match_range(text, r, SNIPPET_WINDOW_BYTES);
                snippet::window_around(text, (r.start, r.end), &SNIPPET_OPTS)
            })
        })
}

/// [`text_snippet`] for a literal pattern, plus the case-insensitive
/// occurrence count found on the way. `None` when the pattern is not literal.
pub fn text_snippet_counted(
    pattern: &crate::query::pattern::TermPattern,
    text: &str,
    folded: &str,
) -> Option<(snippet::Snippet, usize)> {
    let term = pattern.literal_folded()?;
    Some(snippet::extract_folded(text, folded, term, &SNIPPET_OPTS))
}

/// The fuzzy full-text match in one document body: occurrence count within
/// the edit budget and the window around the first occurrence; `None` when
/// absent. Shared with [`crate::live`]. No folded copy: the matcher folds in
/// its mask table.
pub fn fuzzy_snippet(
    bitap: &crate::search::fuzzy::Bitap,
    text: &str,
) -> Option<(usize, snippet::Snippet)> {
    let (count, first) = bitap.count_and_first(text.as_bytes());
    first.map(|range| (count, snippet::window_around(text, range, &SNIPPET_OPTS)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    pub total: usize,
    pub limited: bool,
}

/// Run the cascade, streaming rank-ordered batches into `sink`. `Ok(None)`
/// means cancelled (the generation moved on), and the caller sends no
/// completion. SQL errors come back as strings unless the search was already
/// cancelled: an interrupted statement is normal cancellation.
pub fn run(
    conn: &Connection,
    query: &CascadeQuery,
    options: &SearchOptions,
    generation: u64,
    latest_gen: &AtomicU64,
    sink: &mut dyn FnMut(Vec<SearchHit>),
) -> Result<Option<Outcome>, String> {
    if query.is_empty() {
        return Ok(Some(Outcome::default()));
    }
    let ignore = IgnoreSet::compile(&options.session_ignores)
        .map_err(|e| format!("session ignore filter: {}", e))?;

    let mut cx = Cx {
        conn,
        query,
        options,
        positions: crate::db::repo::fts_positions(conn),
        generation,
        latest_gen,
        ignore,
        emitted: IdSet::default(),
        deferred_path: Deferred::default(),
        deferred_fuzzy_path: Deferred::default(),
        regex_doc: query
            .regex
            .as_ref()
            .map(|_| crate::db::repo::DocDecoder::new())
            .transpose()?,
        total: 0,
        limited: false,
        sink,
    };

    // With no term at all the regex drives its own scans; the path flush
    // still emits the rank-10 hits the name pass sets aside.
    let passes: &[Pass] = if query.pattern.is_empty() {
        &[
            Pass::RegexName,
            Pass::RegexContent,
            Pass::Flush(DeferSlot::Path),
        ]
    } else {
        &[
            Pass::Filename,
            Pass::FullText,
            Pass::FuzzyFilename,
            Pass::FuzzyFullText,
            Pass::Flush(DeferSlot::Path),
            Pass::Flush(DeferSlot::FuzzyPath),
        ]
    };
    for pass in passes {
        if cx.cancelled() {
            return Ok(None);
        }
        // Stop, but not as truncation: an exactly-full result set also has
        // `remaining() == 0`, and `flush_pass` sets `limited` only when it
        // drops rows.
        if cx.remaining() == 0 {
            break;
        }
        let run_pass = match *pass {
            Pass::Filename => cx.pass_filename(),
            Pass::FullText => cx.pass_fulltext(),
            Pass::FuzzyFilename => cx.pass_fuzzy_filename(),
            Pass::FuzzyFullText => cx.pass_fuzzy_fulltext(),
            Pass::RegexName => cx.pass_regex_name(),
            Pass::RegexContent => cx.pass_regex_content(),
            Pass::Flush(slot) => {
                let d = std::mem::take(cx.deferred_mut(slot));
                cx.flush_deferred(d);
                Ok(true)
            }
        };
        match run_pass {
            Ok(true) => {}
            Ok(false) => return Ok(None), // cancelled mid-pass
            Err(e) => {
                // A kill from `interrupt()` arrives as an ordinary SQL error,
                // and SQLite's interrupt flag carries no ordering edge to the
                // generation counter bumped just before it: without this
                // fence a weakly-ordered CPU could report routine
                // cancellation as `Search failed: interrupted`.
                std::sync::atomic::fence(Ordering::Acquire);
                if cx.cancelled() {
                    return Ok(None); // interrupt() killed the statement
                }
                return Err(e);
            }
        }
    }

    Ok(Some(Outcome {
        total: cx.total,
        limited: cx.limited,
    }))
}

enum Pass {
    Filename,
    FullText,
    FuzzyFilename,
    FuzzyFullText,
    /// Regex-only: name hits at rank 4 now, path hits deferred to rank 10.
    RegexName,
    /// Regex-only: content hits at rank 6.
    RegexContent,
    /// Emit the hits the earlier scans set aside in this slot.
    Flush(DeferSlot),
}

/// Which [`Deferred`] buffer a scan's held-back hits go to.
#[derive(Clone, Copy)]
enum DeferSlot {
    /// Ranks 9–10, set aside by pass A and the regex name pass.
    Path,
    /// Rank 11, set aside by pass C.
    FuzzyPath,
}

/// More occurrences → smaller fraction → sorts earlier within a rank base; 1000+ adds zero.
fn count_frac(count: usize) -> f64 {
    (1000usize.saturating_sub(count.min(1000))) as f64 / 1000.0
}

fn col<T: rusqlite::types::FromSql>(row: &rusqlite::Row<'_>, idx: usize) -> Result<T, String> {
    row.get(idx).map_err(|e| e.to_string())
}

/// Rank, then name, then path; the path tiebreak makes the order total.
fn rank_order(a: &SearchHit, b: &SearchHit) -> std::cmp::Ordering {
    a.rank
        .total_cmp(&b.rank)
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.path.cmp(&b.path))
}

/// The columns every pass selects, in the order the passes index them:
/// `0` id, `1` name, `2` parent, `3` size, `4` mtime, optionally `5` text.
/// A pass spelling its own order would quietly serve parents as names.
const HIT_COLUMNS: &str = "f.id, f.name, f.parent, f.size, f.mtime";

/// Columns 3 and 4. `size` is signed in SQLite; without the clamp a corrupt
/// `-1` would become 18 exabytes on the way to `u64`.
fn size_and_mtime(row: &rusqlite::Row<'_>) -> Result<(u64, i64), String> {
    let size = col::<i64>(row, 3)?.max(0) as u64;
    let mtime = col(row, 4)?;
    Ok((size, mtime))
}

/// Same specificity floor as the trigram pass; wildcards count only literals.
fn path_tiers_enabled(pattern: &crate::query::pattern::TermPattern) -> bool {
    pattern.literal_char_count() >= prefilter::TRIGRAM_FLOOR
}

/// Hits ranked below later scans, held back until every better stage emitted.
#[derive(Default)]
struct Deferred {
    hits: Vec<SearchHit>,
    overflowed: bool,
}

/// Longest a pass may sit on hits. Draining on a clock keeps a sparse query
/// painting, and this lands 2–3 batches inside the GUI's 250 ms fade.
const FLUSH_INTERVAL: Duration = Duration::from_millis(80);

/// Multiplicative hasher for SQLite rowids (an odd constant, so a bijection).
/// SipHash measured ~5% of a fuzzy search.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    // Only `write_i64` is ever used; `write` exists because the trait requires it.
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_i64(&mut self, n: i64) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        let mixed = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = mixed.rotate_left(31) ^ mixed;
    }
}

type IdSet = HashSet<i64, BuildHasherDefault<IdHasher>>;

struct Cx<'a> {
    conn: &'a Connection,
    query: &'a CascadeQuery,
    options: &'a SearchOptions,
    /// Whether the full-text index keeps positions, read from the index each
    /// search: it decides how every `MATCH` is phrased
    /// ([`prefilter::fts_literal`]).
    positions: bool,
    generation: u64,
    latest_gen: &'a AtomicU64,
    ignore: IgnoreSet,
    emitted: IdSet,
    /// Ranks 9–10, filled by pass A.
    deferred_path: Deferred,
    /// Rank 11, filled by pass C.
    deferred_fuzzy_path: Deferred,
    /// Decoder for [`Cx::regex_accepts`]'s per-row content fetches, built once
    /// per search when the query carries a regex.
    regex_doc: Option<crate::db::repo::DocDecoder>,
    total: usize,
    limited: bool,
    sink: &'a mut dyn FnMut(Vec<SearchHit>),
}

struct FlushClock {
    last: Instant,
    sent_anything: bool,
}

impl FlushClock {
    fn new() -> FlushClock {
        FlushClock {
            last: Instant::now(),
            sent_anything: false,
        }
    }

    fn due(&self, len: usize, batch: usize) -> bool {
        len > 0 && (!self.sent_anything || len >= batch || self.last.elapsed() >= FLUSH_INTERVAL)
    }

    fn mark_sent(&mut self) {
        self.last = Instant::now();
        self.sent_anything = true;
    }
}

impl<'a> Cx<'a> {
    fn cancelled(&self) -> bool {
        self.generation != self.latest_gen.load(Ordering::Relaxed)
    }

    fn remaining(&self) -> usize {
        self.options.limit.saturating_sub(self.total)
    }

    /// Headroom so sorting keeps the best candidates, without unbounded growth.
    fn buffer_cap(&self) -> usize {
        4096.max(2 * self.remaining())
    }

    fn params_with_filters(&self, leading: Vec<Value>) -> Vec<Value> {
        let mut p = leading;
        p.extend(self.query.filter_params.iter().cloned());
        p
    }

    fn deferred_mut(&mut self, slot: DeferSlot) -> &mut Deferred {
        match slot {
            DeferSlot::Path => &mut self.deferred_path,
            DeferSlot::FuzzyPath => &mut self.deferred_fuzzy_path,
        }
    }

    fn skip(&self, file_id: i64, path: &str) -> bool {
        self.emitted.contains(&file_id) || self.ignore.matches_path(std::path::Path::new(path))
    }

    /// The `regex:` accept-predicate when a regex accompanies a term. The
    /// path contains the name, so one path check covers both. Content is
    /// fetched only for rows whose path missed, once per candidate row, so the
    /// statement is cached, the decoder ([`Cx::regex_doc`]) reused and the
    /// blob decoded borrowed. Measured (`benches/search_alloc.rs`, "term +
    /// regex"): 42.6 MiB churned per keystroke → 4.3, and ~1.3x faster, against
    /// `query_row` plus `decode_all`.
    fn regex_accepts(
        &mut self,
        file_id: i64,
        path: &str,
        text: Option<&str>,
    ) -> Result<bool, String> {
        let Cx {
            conn,
            query,
            regex_doc,
            ..
        } = self;
        let Some(re) = &query.regex else {
            return Ok(true);
        };
        if re.is_match(path) {
            return Ok(true);
        }
        if let Some(text) = text {
            return Ok(re.is_match(text));
        }
        let doc = regex_doc
            .as_mut()
            .expect("built in run() whenever the query carries a regex");
        let accepted = conn
            .prepare_cached("SELECT text_zstd FROM documents_text WHERE file_id = ?1")
            .and_then(|mut stmt| {
                stmt.query_row([file_id], |r| {
                    let blob = r.get_ref(0)?.as_blob()?;
                    // Strict UTF-8 loses nothing: bodies are written from `&str`
                    // (`repo::store_pending_text`), and `stored_text` also treats
                    // non-UTF-8 as absent.
                    Ok(doc.decode(blob).is_some_and(|t| re.is_match(t)))
                })
                .optional()
            })
            .map_err(|e| e.to_string())?;
        Ok(accepted.unwrap_or(false))
    }

    /// Hand `buf` over mid-scan if it is due, leaving it empty when it goes.
    fn flush_if_due(&mut self, buf: &mut Vec<SearchHit>, clock: &mut FlushClock) {
        if !clock.due(buf.len(), self.options.batch.max(1)) {
            return;
        }
        let batch = std::mem::take(buf);
        // `overflowed` belongs to the pass as a whole; the final flush reports it.
        self.flush_pass(batch, false);
        clock.mark_sent();
    }

    fn flush_pass(&mut self, mut buf: Vec<SearchHit>, overflowed: bool) {
        buf.sort_by(rank_order);
        let room = self.remaining();
        self.limited |= overflowed || buf.len() > room;
        buf.truncate(room);
        self.total += buf.len();
        for hit in &buf {
            self.emitted.insert(hit.file_id);
        }
        let batch = self.options.batch.max(1);
        let mut buf = buf.into_iter().peekable();
        while buf.peek().is_some() {
            // A cancelled search stops emitting: the newer generation owns the UI.
            if self.cancelled() {
                return;
            }
            let chunk: Vec<SearchHit> = buf.by_ref().take(batch).collect();
            (self.sink)(chunk);
        }
    }

    /// Emit a held-back buffer; anything a better stage emitted since drops out here.
    fn flush_deferred(&mut self, mut deferred: Deferred) {
        deferred.hits.retain(|h| !self.emitted.contains(&h.file_id));
        self.flush_pass(deferred.hits, deferred.overflowed);
    }

    /// Sort + cut back to display-limit room once past the cap; true if anything dropped.
    fn enforce_cap(&self, buf: &mut Vec<SearchHit>) -> bool {
        if buf.len() <= self.buffer_cap() {
            return false;
        }
        buf.sort_by(rank_order);
        buf.truncate(self.remaining());
        true
    }
}
