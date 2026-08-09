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
//! Pass A is a single `files` scan (`LIKE`, the ASCII-nocase superset of
//! its ranks) classified per-row in Rust — no index needed, the substring
//! stage visits every row anyway. A path always ends in its own name, so
//! `path LIKE` is a superset of `name LIKE` and that one scan covers the
//! filename *and* the path tiers. Pass B is one FTS phrase MATCH verified
//! against the decompressed text. Passes C/D (opt-in) iterate the whole
//! table with a bitap matcher, C covering both the name and the path.
//!
//! Wildcard terms (`rep*rt`) rank through the same tiers, with 1/2 meaning
//! the whole name matches the pattern; they skip the fuzzy passes (bitap is
//! a literal matcher). A regex-only query (`regex:…` with no term) runs two
//! dedicated scans that reuse tiers 4 (name), 6 (content) and 10 (path); a
//! regex accompanying a term is an accept-predicate on every pass instead,
//! not a rank source.
//!
//! The path tiers rank below everything else, so passes A and C buffer them
//! instead of emitting them — stages E and F flush those buffers at the
//! end, dropping files an earlier stage already emitted. Path matching
//! needs a term of at least three characters, the same floor pass B has.
//!
//! Full-text ranks order equal-based hits by occurrence count via a
//! decimal fraction: `base + (1000 - min(count, 1000)) / 1000` — more
//! occurrences sorts earlier, 1000+ occurrences adds zero. Fuzzy ranks add
//! `0.1 × edit_distance` instead.
//!
//! Every scan appends the caller's structured-filter SQL (anonymous
//! placeholders over alias `f`) and checks the generation counter as it
//! streams; a bumped generation aborts mid-statement.
//!
//! # Batches are ordered within themselves, not against each other
//!
//! A pass hands hits over *while* it scans (see [`FLUSH_INTERVAL`]), and a
//! scan finds hits in table order — batch two can hold something better than
//! anything in batch one. Each batch is sorted before it goes; the consumer
//! owns the ordering *across* batches. Do not assume arrival order is rank
//! order.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use rusqlite::OptionalExtension;

use crate::config::IgnoreSet;
use crate::query::pattern::clamp_match_range;
use crate::query::split::CascadeQuery;
use crate::query::translator::{escape_like, quote_phrase};
use crate::snippet;

use super::fuzzy::{edit_budget, Bitap};
use super::{SearchHit, SearchOptions};

mod passes;

/// Cancellation is checked every this many scanned rows in row-cheap
/// passes; decompression-heavy passes check every row.
const CANCEL_CHECK_ROWS: usize = 256;

/// Snippet window budget: the GUI trims the cell text to its column width
/// around the match, and the mouseover shows the rest as extended context.
const SNIPPET_WINDOW_CHARS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub total: usize,
    pub limited: bool,
}

/// Run the cascade, streaming rank-ordered batches into `sink`.
/// `Ok(None)` means the search was cancelled (generation moved on) — the
/// caller sends no completion. SQL errors are returned as strings *unless*
/// the search was already cancelled (an interrupted statement is normal
/// cancellation, not an error).
pub fn run(
    conn: &Connection,
    query: &CascadeQuery,
    options: &SearchOptions,
    generation: u64,
    latest_gen: &AtomicU64,
    sink: &mut dyn FnMut(Vec<SearchHit>),
) -> Result<Option<Outcome>, String> {
    if query.is_empty() {
        return Ok(Some(Outcome {
            total: 0,
            limited: false,
        }));
    }
    let ignore = IgnoreSet::compile(&options.session_ignores)
        .map_err(|e| format!("session ignore filter: {}", e))?;

    let mut cx = Cx {
        conn,
        query,
        options,
        generation,
        latest_gen,
        ignore,
        emitted: HashSet::new(),
        deferred_path: Deferred::default(),
        deferred_fuzzy_path: Deferred::default(),
        total: 0,
        limited: false,
        sink,
    };

    // With no term at all the regex drives its own scans; `Path` still
    // flushes the deferred rank-10 buffer the name pass sets aside.
    let passes: &[Pass] = if query.pattern.is_empty() {
        &[Pass::RegexName, Pass::RegexContent, Pass::Path]
    } else {
        &[
            Pass::Filename,
            Pass::FullText,
            Pass::FuzzyFilename,
            Pass::FuzzyFullText,
            Pass::Path,
            Pass::FuzzyPath,
        ]
    };
    for pass in passes {
        if cx.cancelled() {
            return Ok(None);
        }
        if cx.remaining() == 0 {
            cx.limited = true;
            break;
        }
        let run_pass = match pass {
            Pass::Filename => cx.pass_filename(),
            Pass::FullText => cx.pass_fulltext(),
            Pass::FuzzyFilename => cx.pass_fuzzy_filename(),
            Pass::FuzzyFullText => cx.pass_fuzzy_fulltext(),
            Pass::RegexName => cx.pass_regex_name(),
            Pass::RegexContent => cx.pass_regex_content(),
            Pass::Path => {
                let d = std::mem::take(&mut cx.deferred_path);
                cx.flush_deferred(d)
            }
            Pass::FuzzyPath => {
                let d = std::mem::take(&mut cx.deferred_fuzzy_path);
                cx.flush_deferred(d)
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
    /// Flush of the rank 9–10 hits pass A set aside.
    Path,
    /// Flush of the rank 11 hits pass C set aside.
    FuzzyPath,
}

/// Occurrence-count fraction: more occurrences → smaller fraction → sorts
/// earlier within a rank base; 1000+ adds zero.
fn count_frac(count: usize) -> f64 {
    (1000usize.saturating_sub(count.min(1000))) as f64 / 1000.0
}

/// `row.get` with the crate's string-error convention.
fn col<T: rusqlite::types::FromSql>(row: &rusqlite::Row<'_>, idx: usize) -> Result<T, String> {
    row.get(idx).map_err(|e| e.to_string())
}

/// Rank, then name, then path — the path tiebreak makes the order total, so
/// equal-rank hits cannot shuffle between otherwise-identical sorts.
fn rank_order(a: &SearchHit, b: &SearchHit) -> std::cmp::Ordering {
    a.rank
        .total_cmp(&b.rank)
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.path.cmp(&b.path))
}

/// The `files` columns every pass selects, in the order the passes index
/// them: `0` id, `1` name, `2` path, `3` size, `4` mtime. Passes that also
/// want the stored document text append `dt.text_zstd` as column `5`. A pass
/// spelling its own list in a different order would compile and then quietly
/// serve paths as names.
const HIT_COLUMNS: &str = "f.id, f.name, f.path, f.size, f.mtime";

/// Columns 3 and 4. The clamp matters: `size` is `INTEGER` in SQLite and so
/// signed; a corrupt row holding `-1` would otherwise become 18 exabytes on
/// the way to `u64` and sort to the top of every size-ordered result.
fn size_and_mtime(row: &rusqlite::Row<'_>) -> Result<(u64, i64), String> {
    let size = col::<i64>(row, 3)?.max(0) as u64;
    let mtime = col(row, 4)?;
    Ok((size, mtime))
}

/// The path tiers only make sense with enough term to be specific — same
/// floor the trigram full-text pass uses. Wildcards count only their
/// literal content (`a*b` is two characters of specificity, not three).
fn path_tiers_enabled(pattern: &crate::query::pattern::TermPattern) -> bool {
    pattern.literal_char_count() >= 3
}

/// Hits collected by one scan but ranked below later scans, so held back
/// until every better stage has emitted.
#[derive(Default)]
struct Deferred {
    hits: Vec<SearchHit>,
    overflowed: bool,
}

/// Longest a pass may sit on hits before handing them over. A pass is a
/// whole-table scan that can run for seconds; draining on a clock rather
/// than only on a full buffer keeps even a sparse query painting as the
/// scan reaches its matches. Short enough to land two or three batches
/// inside the GUI's 250 ms result fade.
const FLUSH_INTERVAL: Duration = Duration::from_millis(80);

struct Cx<'a> {
    conn: &'a Connection,
    query: &'a CascadeQuery,
    options: &'a SearchOptions,
    generation: u64,
    latest_gen: &'a AtomicU64,
    ignore: IgnoreSet,
    emitted: HashSet<i64>,
    /// Ranks 9–10, filled by pass A.
    deferred_path: Deferred,
    /// Rank 11, filled by pass C.
    deferred_fuzzy_path: Deferred,
    total: usize,
    limited: bool,
    sink: &'a mut dyn FnMut(Vec<SearchHit>),
}

/// Drives [`Cx::flush_if_due`]: when this pass last handed hits over, and
/// whether it has handed over anything at all.
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

    /// Whether a buffer of `len` hits should go now. The first batch of a
    /// pass goes the moment there is anything to send.
    fn due(&self, len: usize, batch: usize) -> bool {
        if len == 0 {
            return false;
        }
        !self.sent_anything || len >= batch || self.last.elapsed() >= FLUSH_INTERVAL
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

    /// Buffer cap for scan passes: enough headroom that sorting keeps the
    /// best candidates, without unbounded growth on huge hit sets.
    fn buffer_cap(&self) -> usize {
        4096.max(2 * self.remaining())
    }

    fn params_with_filters(
        &self,
        leading: Vec<rusqlite::types::Value>,
    ) -> Vec<rusqlite::types::Value> {
        let mut p = leading;
        p.extend(self.query.filter_params.iter().cloned());
        p
    }

    /// Skip rows already emitted at a better rank or hidden by session
    /// ignore chips.
    fn skip(&self, file_id: i64, path: &str) -> bool {
        self.emitted.contains(&file_id) || self.ignore.matches_path(std::path::Path::new(path))
    }

    /// The `regex:` accept-predicate applied to every candidate row when a
    /// regex accompanies a term. The path contains the name, so one path
    /// check covers both; content is fetched (and decompressed) only for
    /// rows whose path missed — bounded by the pass's hit count, not its
    /// scan count. Pass `text` when the pass already has the content.
    fn regex_accepts(&self, file_id: i64, path: &str, text: Option<&str>) -> Result<bool, String> {
        let Some(re) = &self.query.regex else {
            return Ok(true);
        };
        if re.is_match(path) {
            return Ok(true);
        }
        if let Some(text) = text {
            return Ok(re.is_match(text));
        }
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT text_zstd FROM documents_text WHERE file_id = ?1",
                [file_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(raw) = blob.and_then(|b| zstd::decode_all(b.as_slice()).ok()) else {
            return Ok(false);
        };
        Ok(re.is_match(&String::from_utf8_lossy(&raw)))
    }

    /// Hand `buf` over mid-scan if it is due, leaving it empty when it goes.
    /// Ordering *between* batches is the consumer's problem.
    fn flush_if_due(&mut self, buf: &mut Vec<SearchHit>, clock: &mut FlushClock) {
        if !clock.due(buf.len(), self.options.batch.max(1)) {
            return;
        }
        let batch = std::mem::take(buf);
        // `overflowed` belongs to the pass as a whole, not to one batch; the
        // final flush reports it.
        self.flush_pass(batch, false);
        clock.mark_sent();
    }

    /// Sort a finished pass buffer, truncate to what's left of the display
    /// limit, and stream it out in `options.batch`-sized events.
    fn flush_pass(&mut self, mut buf: Vec<SearchHit>, overflowed: bool) {
        buf.sort_by(rank_order);
        let room = self.remaining();
        if buf.len() > room {
            buf.truncate(room);
            self.limited = true;
        }
        if overflowed {
            self.limited = true;
        }
        self.total += buf.len();
        for hit in &buf {
            self.emitted.insert(hit.file_id);
        }
        let batch = self.options.batch.max(1);
        let mut buf = buf.into_iter().peekable();
        while buf.peek().is_some() {
            // A cancelled search stops emitting immediately — the newer
            // generation owns the UI.
            if self.cancelled() {
                return;
            }
            let chunk: Vec<SearchHit> = buf.by_ref().take(batch).collect();
            (self.sink)(chunk);
        }
    }

    /// Emit a buffer held back from an earlier scan. Anything a better
    /// stage already emitted drops out here — `emitted` was still empty (or
    /// smaller) when these hits were collected.
    fn flush_deferred(&mut self, mut deferred: Deferred) -> Result<bool, String> {
        deferred.hits.retain(|h| !self.emitted.contains(&h.file_id));
        self.flush_pass(deferred.hits, deferred.overflowed);
        Ok(true)
    }

    /// Keep a scan buffer bounded: sort + cut back to the display-limit
    /// room once it doubles past it. Returns whether anything was dropped.
    fn enforce_cap(&self, buf: &mut Vec<SearchHit>) -> bool {
        if buf.len() <= self.buffer_cap() {
            return false;
        }
        buf.sort_by(rank_order);
        buf.truncate(self.remaining());
        true
    }
}
