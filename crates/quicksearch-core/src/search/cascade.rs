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
//! stage visits every row anyway. Because a path always ends in its own
//! name, `path LIKE` is a superset of `name LIKE`, so that one scan covers
//! the filename *and* the path tiers. Pass B is one FTS phrase MATCH
//! verified against the decompressed text. Passes C/D (opt-in) iterate the
//! whole table with a bitap matcher, C covering both the name and the path.
//!
//! Wildcard terms (`rep*rt`) rank through the same tiers, with 1/2 meaning
//! the whole name matches the pattern; they skip the fuzzy passes (bitap is
//! a literal matcher). A regex-only query (`regex:…` with no term) runs two
//! dedicated scans that reuse tiers 4 (name), 6 (content) and 10 (path), so
//! downstream stage handling is unchanged. When `regex:` accompanies a
//! term, it is an accept-predicate on every pass, not a rank source.
//!
//! The path tiers rank below everything else, so pass A and pass C buffer
//! them instead of emitting them — stages E and F flush those buffers at
//! the end, dropping files an earlier stage already emitted. Path matching
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
//! A pass hands hits over *while it scans* (see [`FLUSH_INTERVAL`]) rather than
//! at the end. On a large index a single pass runs for seconds, and holding its
//! results back means the UI shows nothing for that whole time even though a
//! rank-1 filename match may have turned up in the first few milliseconds.
//!
//! The cost is that a scan finds hits in table order, not rank order: batch two
//! can contain something better than anything in batch one. Each batch is
//! sorted before it goes, but the consumer owns the ordering *across* batches —
//! the GUI keeps its table sorted by whichever column is keyed and places each
//! arrival where that sort requires. Do not assume arrival order is rank order.

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

/// Cancellation is checked every this many scanned rows in row-cheap
/// passes; decompression-heavy passes check every row.
const CANCEL_CHECK_ROWS: usize = 256;

/// Snippet window budget. Generous on purpose: the GUI trims the cell
/// text down to its column width around the match, and the mouseover
/// shows the rest of this window as extended context.
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
                // generation counter the canceller bumped just before it. On a
                // weakly-ordered CPU the plain load in `cancelled()` may still
                // read the pre-bump value here, which would report routine
                // cancellation as `Search failed: interrupted`. Fence first, so
                // having seen the kill implies seeing the bump that caused it.
                // Error path only — the per-row checks stay relaxed.
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

/// The `files` columns every pass selects, in the order the passes index
/// them: `0` id, `1` name, `2` path, `3` size, `4` mtime. Passes that also
/// want the stored document text append `dt.text_zstd` as column `5`.
///
/// One string rather than seven copies, because the column *order* is what
/// every `row.get(n)` in this file is written against — a pass that spelled
/// its own list in a different order would compile and then quietly serve
/// paths as names.
const HIT_COLUMNS: &str = "f.id, f.name, f.path, f.size, f.mtime";

/// Columns 3 and 4: the two every pass reads identically and stores without
/// inspecting.
///
/// The clamp is the point of having this in one place. `size` is `INTEGER` in
/// SQLite and so signed; a corrupt or hand-edited row holding `-1` would
/// otherwise become 18 exabytes on the way to `u64` and sort to the top of
/// every size-ordered result.
fn size_and_mtime(row: &rusqlite::Row<'_>) -> Result<(u64, i64), String> {
    let size = row.get::<_, i64>(3).map_err(|e| e.to_string())?.max(0) as u64;
    let mtime = row.get(4).map_err(|e| e.to_string())?;
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

/// Longest a pass may sit on hits before handing them over.
///
/// A pass is a whole-table scan, and on a large index that runs for seconds.
/// Waiting for it to finish means the GUI shows nothing for that whole time,
/// even when a rank-1 filename match turned up in the first few milliseconds.
/// Draining on a clock rather than only on a full buffer is what makes a
/// *sparse* query responsive too — six matches out of seven million still
/// paint as the scan reaches them.
///
/// Short enough to land two or three batches inside the GUI's 250 ms result
/// fade, so the fade reveals a filling list rather than an empty one.
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

    /// Whether a buffer of `len` hits should go now.
    ///
    /// The first batch of a pass goes the moment there is anything to send, so
    /// a single early hit paints immediately instead of waiting out an
    /// interval it has no reason to.
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
    ///
    /// Partial and final flushes are the same operation — [`flush_pass`] sorts
    /// what it is given and emits it — so a pass streams simply by calling this
    /// each time round its row loop. Ordering *between* batches is the caller's
    /// problem: the GUI keeps its table sorted by whichever column is keyed, so
    /// a better-ranked hit found later lands above the ones already shown.
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
        buf.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
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
        buf.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.path.cmp(&b.path))
        });
        buf.truncate(self.remaining());
        true
    }

    /// Pass A — ranks 1–4 now, ranks 9–10 deferred, from one `files` scan.
    /// Returns Ok(false) on cancellation.
    fn pass_filename(&mut self) -> Result<bool, String> {
        let query = self.query;
        let pattern = &query.pattern;
        let with_paths = path_tiers_enabled(pattern);
        // A path always ends in its own name, so `path LIKE` is the
        // superset that feeds both the name and the path tiers.
        let sql = format!(
            "SELECT {} FROM files f \
             WHERE {} LIKE ? ESCAPE '\\'{}",
            HIT_COLUMNS,
            if with_paths { "f.path" } else { "f.name" },
            query.filter_sql
        );
        // Wildcard patterns turn each star into an unescaped `%`; the
        // substring wrap absorbs leading/trailing stars. User `%`/`_`
        // remain escaped literals either way.
        let like = pattern
            .segments()
            .iter()
            .map(|s| escape_like(s))
            .collect::<Vec<_>>()
            .join("%");
        let params =
            self.params_with_filters(vec![rusqlite::types::Value::Text(format!("%{}%", like))]);

        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let mut buf: Vec<SearchHit> = Vec::new();
        let mut path_buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut path_overflowed = false;
        let mut scanned = 0usize;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            scanned += 1;
            if scanned.is_multiple_of(CANCEL_CHECK_ROWS) && self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            // For a literal pattern these are exactly the old `==` /
            // `eq_ignore_ascii_case` / `find` / folded-`find` operations
            // (folding is byte-length preserving, so folded offsets are
            // valid in the original). Wildcards run the same ladder through
            // their compiled matcher — tiers 1/2 mean "whole name matches
            // the pattern", which is what `*.txt` should do.
            let (rank, match_range) = if pattern.whole_match(&name, false) {
                (1.0, (0, name.len()))
            } else if pattern.whole_match(&name, true) {
                (2.0, (0, name.len()))
            } else if let Some(r) = pattern.find_first(&name, false) {
                (3.0, (r.start, r.end))
            } else if let Some(r) = pattern.find_first(&name, true) {
                (4.0, (r.start, r.end))
            } else if !with_paths {
                continue;
            } else if let Some(r) = pattern.find_first(&path, false) {
                (9.0, (r.start, r.end))
            } else if let Some(r) = pattern.find_first(&path, true) {
                (10.0, (r.start, r.end))
            } else {
                // LIKE folds ASCII case only; a row that matched it but
                // neither field is a non-ASCII near-miss. Drop it.
                continue;
            };
            if !self.regex_accepts(file_id, &path, None)? {
                continue;
            }
            let is_path_tier = rank >= 9.0;
            // The "snippet" of a name or path hit is that field itself with
            // the matched span marked — the GUI renders it as [the field].
            let snip = snippet::Snippet {
                ranges: vec![match_range],
                window: if is_path_tier {
                    path.clone()
                } else {
                    name.clone()
                },
                truncated_start: false,
                truncated_end: false,
            };
            let (size, mtime) = size_and_mtime(row)?;
            let hit = SearchHit {
                file_id,
                name,
                path,
                size,
                mtime,
                rank,
                stage: rank as u8,
                snippet: Some(snip),
            };
            if is_path_tier {
                path_buf.push(hit);
                path_overflowed |= self.enforce_cap(&mut path_buf);
            } else {
                buf.push(hit);
                overflowed |= self.enforce_cap(&mut buf);
                self.flush_if_due(&mut buf, &mut clock);
            }
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.deferred_path = Deferred {
            hits: path_buf,
            overflowed: path_overflowed,
        };
        self.flush_pass(buf, overflowed);
        Ok(true)
    }

    /// Pass B — ranks 5–6 from one FTS MATCH, verified/counted in the
    /// decompressed text.
    fn pass_fulltext(&mut self) -> Result<bool, String> {
        let query = self.query;
        let pattern = &query.pattern;
        if pattern.literal_char_count() < 3 {
            // Below the trigram floor the MATCH can't return anything.
            return Ok(true);
        }
        // Column filter goes inside the MATCH expression (`text: "..."`)
        // so only document bodies match — filenames get ranks 1-4 from the
        // filename pass instead. A literal term is one quoted phrase; a
        // wildcard term narrows with an AND of its trigram-sized segments.
        // With no segment of 3+ chars (`ab*cd`) FTS can't narrow at all, so
        // fall back to scanning every stored document — every row is
        // pattern-verified either way.
        let match_expr: Option<String> = match pattern.literal() {
            Some(term) => Some(format!("text: {}", quote_phrase(term))),
            None => {
                let usable: Vec<String> = pattern
                    .segments()
                    .iter()
                    .filter(|s| s.chars().count() >= 3)
                    .map(|s| format!("text: {}", quote_phrase(s)))
                    .collect();
                if usable.is_empty() {
                    None
                } else {
                    Some(usable.join(" AND "))
                }
            }
        };
        let narrowed = match_expr.is_some();
        let (sql, params) = match match_expr {
            Some(expr) => (
                format!(
                    "SELECT {}, dt.text_zstd \
                     FROM searchabletext \
                     JOIN files f ON f.id = searchabletext.rowid \
                     LEFT JOIN documents_text dt ON dt.file_id = f.id \
                     WHERE searchabletext MATCH ?{}",
                    HIT_COLUMNS, query.filter_sql
                ),
                self.params_with_filters(vec![rusqlite::types::Value::Text(expr)]),
            ),
            None => (
                format!(
                    "SELECT {}, dt.text_zstd \
                     FROM documents_text dt \
                     JOIN files f ON f.id = dt.file_id WHERE 1=1{}",
                    HIT_COLUMNS, query.filter_sql
                ),
                self.params_with_filters(Vec::new()),
            ),
        };

        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let snippet_opts = snippet::Options {
            approx_chars: SNIPPET_WINDOW_CHARS,
        };
        let mut buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            // Decompression dominates: check every row.
            if self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            let blob: Option<Vec<u8>> = row.get(5).map_err(|e| e.to_string())?;
            let text = blob
                .and_then(|b| zstd::decode_all(b.as_slice()).ok())
                .map(|raw| String::from_utf8_lossy(&raw).into_owned());

            let (rank, stage, snip) = match &text {
                Some(text) => {
                    // Fold once. The case-insensitive count, the first-match
                    // search and the snippet extraction all need it, and each
                    // used to make its own copy of a document that can run to
                    // `maximum_text_size`. The trigram tokenizer is
                    // case-insensitive, so nearly every candidate takes this
                    // path — it is a per-row cost, not a per-hit one.
                    let mut folded: Option<String> = None;
                    let (count, stage) = {
                        let count_cs = pattern.count(text, false);
                        if count_cs > 0 {
                            (count_cs, 5)
                        } else {
                            let lower = folded.insert(text.to_ascii_lowercase());
                            let count_ci = pattern.count_folded(lower);
                            if count_ci > 0 {
                                (count_ci, 6)
                            } else {
                                // Folded/unordered FTS candidate: the
                                // pattern never occurs — drop it.
                                continue;
                            }
                        }
                    };
                    // Literal terms keep the richer multi-occurrence
                    // extract; a wildcard match marks its own first range.
                    let lower = folded.unwrap_or_else(|| text.to_ascii_lowercase());
                    let snip = match pattern.literal() {
                        Some(term) => Some(snippet::extract_folded(
                            text,
                            &lower,
                            &[term],
                            &snippet_opts,
                        )),
                        None => pattern.find_first_folded(&lower).map(|r| {
                            let r = clamp_match_range(text, r, SNIPPET_WINDOW_CHARS);
                            snippet::window_around(text, (r.start, r.end), &snippet_opts)
                        }),
                    };
                    (stage as f64 + count_frac(count), stage as u8, snip)
                }
                // No stored text (store_text_for_snippets = false or empty
                // body): can't case-verify or count. On the FTS-narrowed
                // path accept at the bottom of rank 6 as count-unknown (for
                // wildcards the AND-of-segments guarantee is weaker —
                // unordered co-occurrence — accepted for recall). On the
                // full-scan fallback there is no FTS evidence at all, so an
                // unverifiable row is just skipped.
                None => {
                    if !narrowed {
                        continue;
                    }
                    (6.0 + count_frac(1), 6, None)
                }
            };
            if !self.regex_accepts(file_id, &path, text.as_deref())? {
                continue;
            }

            let (size, mtime) = size_and_mtime(row)?;
            buf.push(SearchHit {
                file_id,
                name: row.get(1).map_err(|e| e.to_string())?,
                path,
                size,
                mtime,
                rank,
                stage,
                snippet: snip,
            });
            overflowed |= self.enforce_cap(&mut buf);
            self.flush_if_due(&mut buf, &mut clock);
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.flush_pass(buf, overflowed);
        Ok(true)
    }

    /// Pass C — rank 7 now, rank 11 deferred: one bitap sweep over every
    /// filename, falling back to the full path where the name misses.
    fn pass_fuzzy_filename(&mut self) -> Result<bool, String> {
        if !self.options.fuzzy {
            return Ok(true);
        }
        // Bitap is a literal matcher; wildcard terms don't fuzz.
        if self.query.pattern.is_wildcard() {
            return Ok(true);
        }
        let folded_term = self.query.term.to_ascii_lowercase();
        let Some(k) = edit_budget(folded_term.len(), self.options.fuzzy_max_edits) else {
            return Ok(true);
        };
        let Some(bitap) = Bitap::new(folded_term.as_bytes(), k) else {
            return Ok(true);
        };
        let with_paths = path_tiers_enabled(&self.query.pattern);

        let sql = format!(
            "SELECT {} FROM files f WHERE 1=1{}",
            HIT_COLUMNS, self.query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let mut buf: Vec<SearchHit> = Vec::new();
        let mut path_buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut path_overflowed = false;
        let mut scanned = 0usize;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            scanned += 1;
            if scanned.is_multiple_of(1024) && self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            // The name is the better match when both fire, so it wins and
            // only a name miss falls through to the path tier. Distance and
            // match range come from one sweep — taking them separately meant a
            // second full scan of the same buffer for every hit.
            let folded_name = name.to_ascii_lowercase();
            let (rank, field, range) = match bitap.best_distance_and_first(folded_name.as_bytes()) {
                Some((distance, range)) => (7.0 + 0.1 * distance as f64, &name, range),
                None if with_paths => {
                    let folded_path = path.to_ascii_lowercase();
                    match bitap.best_distance_and_first(folded_path.as_bytes()) {
                        Some((distance, range)) => (11.0 + 0.1 * distance as f64, &path, range),
                        None => continue,
                    }
                }
                None => continue,
            };
            if !self.regex_accepts(file_id, &path, None)? {
                continue;
            }
            // Mark the approximate matched span in the matched field for
            // the GUI's [matched field] rendering. window_around clamps
            // and aligns.
            let snip = Some(snippet::window_around(
                field,
                range,
                &snippet::Options {
                    approx_chars: field.len().saturating_mul(2).max(8),
                },
            ));
            let is_path_tier = rank >= 11.0;
            let (size, mtime) = size_and_mtime(row)?;
            let hit = SearchHit {
                file_id,
                name,
                path,
                size,
                mtime,
                rank,
                stage: rank as u8,
                snippet: snip,
            };
            if is_path_tier {
                path_buf.push(hit);
                path_overflowed |= self.enforce_cap(&mut path_buf);
            } else {
                buf.push(hit);
                overflowed |= self.enforce_cap(&mut buf);
                self.flush_if_due(&mut buf, &mut clock);
            }
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.deferred_fuzzy_path = Deferred {
            hits: path_buf,
            overflowed: path_overflowed,
        };
        self.flush_pass(buf, overflowed);
        Ok(true)
    }

    /// Pass D — rank 8, bitap over every stored document text.
    fn pass_fuzzy_fulltext(&mut self) -> Result<bool, String> {
        if !self.options.fuzzy {
            return Ok(true);
        }
        // Bitap is a literal matcher; wildcard terms don't fuzz.
        if self.query.pattern.is_wildcard() {
            return Ok(true);
        }
        let folded_term = self.query.term.to_ascii_lowercase();
        let Some(k) = edit_budget(folded_term.len(), self.options.fuzzy_max_edits) else {
            return Ok(true);
        };
        let Some(bitap) = Bitap::new(folded_term.as_bytes(), k) else {
            return Ok(true);
        };

        let sql = format!(
            "SELECT {}, dt.text_zstd \
             FROM documents_text dt JOIN files f ON f.id = dt.file_id WHERE 1=1{}",
            HIT_COLUMNS, self.query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let snippet_opts = snippet::Options {
            approx_chars: SNIPPET_WINDOW_CHARS,
        };
        let mut buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            if self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            let blob: Option<Vec<u8>> = row.get(5).map_err(|e| e.to_string())?;
            let Some(blob) = blob else {
                continue;
            };
            let Ok(raw) = zstd::decode_all(blob.as_slice()) else {
                continue;
            };
            let text = String::from_utf8_lossy(&raw).into_owned();
            // ASCII folding is byte-length preserving, so ranges found in
            // the folded buffer are valid in the original.
            let folded = text.to_ascii_lowercase();
            let (count, first) = bitap.count_and_first(folded.as_bytes());
            if count == 0 {
                continue;
            }
            if !self.regex_accepts(file_id, &path, Some(&text))? {
                continue;
            }
            let snip = first.map(|range| snippet::window_around(&text, range, &snippet_opts));
            let (size, mtime) = size_and_mtime(row)?;
            buf.push(SearchHit {
                file_id,
                name: row.get(1).map_err(|e| e.to_string())?,
                path,
                size,
                mtime,
                rank: 8.0 + count_frac(count),
                stage: 8,
                snippet: snip,
            });
            overflowed |= self.enforce_cap(&mut buf);
            self.flush_if_due(&mut buf, &mut clock);
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.flush_pass(buf, overflowed);
        Ok(true)
    }

    /// Regex-only pass over `files`: the regex bypasses the FTS trigram
    /// entirely and runs on every name, falling back to the full path.
    /// Name hits reuse rank 4, path hits defer to rank 10, so the GUI's
    /// stage-based rendering needs no new cases.
    fn pass_regex_name(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let sql = format!(
            "SELECT {} FROM files f WHERE 1=1{}",
            HIT_COLUMNS, query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let mut buf: Vec<SearchHit> = Vec::new();
        let mut path_buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut path_overflowed = false;
        let mut scanned = 0usize;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            scanned += 1;
            if scanned.is_multiple_of(1024) && self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            // The name is the better hit; only a name miss falls through
            // to the path tier — mirroring pass A.
            let (rank, match_range, is_path_tier) = match re.find_first(&name) {
                Some(r) => (4.0, (r.start, r.end), false),
                None => match re.find_first(&path) {
                    Some(r) => (10.0, (r.start, r.end), true),
                    None => continue,
                },
            };
            let snip = snippet::Snippet {
                ranges: vec![match_range],
                window: if is_path_tier {
                    path.clone()
                } else {
                    name.clone()
                },
                truncated_start: false,
                truncated_end: false,
            };
            let (size, mtime) = size_and_mtime(row)?;
            let hit = SearchHit {
                file_id,
                name,
                path,
                size,
                mtime,
                rank,
                stage: rank as u8,
                snippet: Some(snip),
            };
            if is_path_tier {
                path_buf.push(hit);
                path_overflowed |= self.enforce_cap(&mut path_buf);
            } else {
                buf.push(hit);
                overflowed |= self.enforce_cap(&mut buf);
                self.flush_if_due(&mut buf, &mut clock);
            }
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.deferred_path = Deferred {
            hits: path_buf,
            overflowed: path_overflowed,
        };
        self.flush_pass(buf, overflowed);
        Ok(true)
    }

    /// Regex-only pass over every stored document text, reusing rank 6.
    fn pass_regex_content(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let sql = format!(
            "SELECT {}, dt.text_zstd \
             FROM documents_text dt JOIN files f ON f.id = dt.file_id WHERE 1=1{}",
            HIT_COLUMNS, query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let snippet_opts = snippet::Options {
            approx_chars: SNIPPET_WINDOW_CHARS,
        };
        let mut buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut clock = FlushClock::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            // Decompression dominates: check every row.
            if self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let path: String = row.get(2).map_err(|e| e.to_string())?;
            if self.skip(file_id, &path) {
                continue;
            }
            let blob: Option<Vec<u8>> = row.get(5).map_err(|e| e.to_string())?;
            let Some(raw) = blob.and_then(|b| zstd::decode_all(b.as_slice()).ok()) else {
                continue;
            };
            let text = String::from_utf8_lossy(&raw).into_owned();
            let count = re.count(&text);
            if count == 0 {
                continue;
            }
            // A greedy user regex can match megabytes; clamp the range
            // before the snippet window is cut.
            let snip = re.find_first(&text).map(|r| {
                let r = clamp_match_range(&text, r, SNIPPET_WINDOW_CHARS);
                snippet::window_around(&text, (r.start, r.end), &snippet_opts)
            });
            let (size, mtime) = size_and_mtime(row)?;
            buf.push(SearchHit {
                file_id,
                name: row.get(1).map_err(|e| e.to_string())?,
                path,
                size,
                mtime,
                rank: 6.0 + count_frac(count),
                stage: 6,
                snippet: snip,
            });
            overflowed |= self.enforce_cap(&mut buf);
            self.flush_if_due(&mut buf, &mut clock);
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        self.flush_pass(buf, overflowed);
        Ok(true)
    }
}
