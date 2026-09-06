//! The cascade's six scan passes: a prelude and a per-row classifier
//! each, all driven through [`Cx::scan_pass`]'s shared skeleton.

use super::*;

enum RowHit {
    Skip,
    Emit(SearchHit),
    Defer(SearchHit),
}

#[allow(clippy::too_many_arguments)]
fn row_hit(
    row: &rusqlite::Row<'_>,
    file_id: i64,
    path: &str,
    name: &str,
    rank: f64,
    stage: u8,
    snippet: Option<snippet::Snippet>,
    defer: bool,
) -> Result<RowHit, String> {
    let (size, mtime) = size_and_mtime(row)?;
    let hit = SearchHit {
        file_id,
        name: name.to_string(),
        path: path.to_string(),
        size,
        mtime,
        rank,
        stage,
        snippet,
    };
    Ok(if defer {
        RowHit::Defer(hit)
    } else {
        RowHit::Emit(hit)
    })
}

/// The row's decompressed document body (column 5), or `None` where no text
/// is stored or the blob will not decode.
fn stored_text<'d>(
    row: &rusqlite::Row<'_>,
    doc: &'d mut crate::db::repo::DocDecoder,
) -> Result<Option<&'d str>, String> {
    let blob: Option<&[u8]> = row
        .get_ref(5)
        .map_err(|e| e.to_string())?
        .as_blob_or_null()
        .map_err(|e| e.to_string())?;
    Ok(blob.and_then(|b| doc.decode(b)))
}

/// Scan of every stored document body, for a pass with no usable FTS narrowing.
fn doc_scan_sql(filter_sql: &str) -> String {
    format!(
        "SELECT {}, dt.text_zstd \
         FROM documents_text dt JOIN files f ON f.id = dt.file_id WHERE 1=1{}",
        HIT_COLUMNS, filter_sql
    )
}

/// The scan behind a `searchabletext MATCH` prefilter. `require_text` picks
/// the join to `documents_text`: `false` is [`Cx::pass_fulltext`]'s LEFT
/// JOIN, which keeps an FTS-matched row whose stored text was dropped.
fn fts_match_sql(filter_sql: &str, require_text: bool) -> String {
    format!(
        "SELECT {}, dt.text_zstd \
         FROM searchabletext \
         JOIN files f ON f.id = searchabletext.rowid \
         {} documents_text dt ON dt.file_id = f.id \
         WHERE searchabletext MATCH ?{}",
        HIT_COLUMNS,
        if require_text { "JOIN" } else { "LEFT JOIN" },
        filter_sql
    )
}

/// Fold `text` into `dst`, reusing its allocation. The ASCII fold is
/// byte-length preserving, so folded offsets are valid in the original.
fn fold_into(dst: &mut String, text: &str) {
    dst.clear();
    dst.push_str(text);
    dst.make_ascii_lowercase();
}

/// The segment a straddling wildcard can be SQL-prefiltered on: the longest
/// one containing no path separator, or `None` when every segment has one.
/// Any single segment is a superset test, and a separator-free one cannot
/// span the `parent`/`name` join — see `search/prefilter.rs` (the one rule).
fn anchor_segment(pattern: &crate::query::pattern::TermPattern) -> Option<&str> {
    pattern
        .segments()
        .iter()
        .filter(|s| !s.contains(std::path::MAIN_SEPARATOR))
        .max_by_key(|s| s.len())
        .map(String::as_str)
}

/// Which [`Deferred`] buffer a scan's held-back hits go to.
enum DeferSlot {
    /// Ranks 9–10, flushed by [`Pass::Path`].
    Path,
    /// Rank 11, flushed by [`Pass::FuzzyPath`].
    FuzzyPath,
}

impl<'a> Cx<'a> {
    /// One scan pass: prepare `sql`, stream rows, classify each into
    /// emit/defer/skip, and flush; only the prelude and `classify` differ per
    /// pass. Returns Ok(false) on cancellation. A `Defer`red hit lands in
    /// `defer_slot` at the end of the scan — path-tier ranks sort below
    /// stages that have not run yet. `classify` receives the reassembled
    /// path and the name as a borrowed slice of it.
    fn scan_pass(
        &mut self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
        cancel_every: usize,
        defer_slot: Option<DeferSlot>,
        mut classify: impl FnMut(
            &mut Self,
            &rusqlite::Row<'_>,
            i64,
            &str,
            &str,
        ) -> Result<RowHit, String>,
    ) -> Result<bool, String> {
        let conn = self.conn;
        // Cached: the same statements re-run on every keystroke.
        let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let mut buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut deferred = Deferred::default();
        let mut scanned = 0usize;
        let mut clock = FlushClock::new();
        // Set when the loop stops on the display limit: the result set is cut.
        let mut cut_short = false;
        // One path buffer per scan; a local because classifiers take `&mut Self`.
        let mut path = String::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            scanned += 1;
            if scanned.is_multiple_of(cancel_every) && self.cancelled() {
                return Ok(false);
            }
            let file_id: i64 = col(row, 0)?;
            let borrowed = |idx| -> Result<&str, String> {
                row.get_ref(idx)
                    .map_err(|e| e.to_string())?
                    .as_str()
                    .map_err(|e| e.to_string())
            };
            path.clear();
            // Every stored parent already ends in a separator (`dir_to_db_parent`).
            path.push_str(borrowed(2)?);
            // The parent is written whole, so this offset stays a char boundary.
            let name_at = path.len();
            path.push_str(borrowed(1)?);
            if self.skip(file_id, &path) {
                continue;
            }
            match classify(self, row, file_id, &path, &path[name_at..])? {
                RowHit::Skip => {}
                RowHit::Emit(hit) => {
                    buf.push(hit);
                    overflowed |= self.enforce_cap(&mut buf);
                    self.flush_if_due(&mut buf, &mut clock);
                    // Stop once the display limit is full. Recorded, not
                    // turned into `limited` here: `flush_pass` owns that call.
                    if self.remaining() == 0 {
                        cut_short = true;
                        break;
                    }
                }
                RowHit::Defer(hit) => {
                    deferred.hits.push(hit);
                    deferred.overflowed |= self.enforce_cap(&mut deferred.hits);
                }
            }
        }
        drop(rows);
        if self.cancelled() {
            return Ok(false);
        }
        match defer_slot {
            Some(DeferSlot::Path) => self.deferred_path = deferred,
            Some(DeferSlot::FuzzyPath) => self.deferred_fuzzy_path = deferred,
            None => debug_assert!(deferred.hits.is_empty(), "deferred hits with no slot"),
        }
        self.flush_pass(buf, overflowed);
        // After the flush, so it cannot be undone by one that truncated nothing;
        // without this a cut set can report itself complete.
        self.limited |= cut_short;
        Ok(true)
    }

    /// Pass A — ranks 1–4 now, ranks 9–10 deferred, from one `files` scan.
    /// Returns Ok(false) on cancellation.
    pub(super) fn pass_filename(&mut self) -> Result<bool, String> {
        let query = self.query;
        let pattern = &query.pattern;
        let with_paths = path_tiers_enabled(pattern);
        // Each star becomes an unescaped `%`; user `%`/`_` stay escaped literals.
        let like = format!(
            "%{}%",
            pattern
                .segments()
                .iter()
                .map(|s| escape_like(s))
                .collect::<Vec<_>>()
                .join("%")
        );
        // The prefilter must stay a *superset* of what the classifier accepts
        // (see `search/prefilter.rs`): a straddling pattern falls back to its
        // [`anchor_segment`]. Straddling is ordinary wildcard typing
        // (`rep*rt`) — don't send it back to `1=1`.
        let straddles = pattern.segments().len() > 1
            || pattern
                .segments()
                .iter()
                .any(|s| s.contains(std::path::MAIN_SEPARATOR));
        let two_column =
            || "(f.name LIKE ? ESCAPE '\\' OR f.parent LIKE ? ESCAPE '\\')".to_string();
        let bind_twice = |pat: String| {
            vec![
                rusqlite::types::Value::Text(pat.clone()),
                rusqlite::types::Value::Text(pat),
            ]
        };
        let (predicate, terms) = match (with_paths, straddles) {
            (false, _) => (
                "f.name LIKE ? ESCAPE '\\'".to_string(),
                vec![rusqlite::types::Value::Text(like)],
            ),
            (true, false) => (two_column(), bind_twice(like)),
            (true, true) => match anchor_segment(pattern) {
                Some(anchor) => (
                    two_column(),
                    bind_twice(format!("%{}%", escape_like(anchor))),
                ),
                None => ("1=1".to_string(), Vec::new()),
            },
        };
        let sql = format!(
            "SELECT {} FROM files f WHERE {}{}",
            HIT_COLUMNS, predicate, query.filter_sql
        );
        let params = self.params_with_filters(terms);
        self.scan_pass(
            &sql,
            params,
            CANCEL_CHECK_ROWS,
            Some(DeferSlot::Path),
            |cx, row, file_id, path, name| {
                // For wildcards, tiers 1/2 mean "the whole name matches".
                let (rank, match_range) = if pattern.whole_match(name, false) {
                    (1.0, (0, name.len()))
                } else if pattern.whole_match(name, true) {
                    (2.0, (0, name.len()))
                } else if let Some(r) = pattern.find_first(name, false) {
                    (3.0, (r.start, r.end))
                } else if let Some(r) = pattern.find_first(name, true) {
                    (4.0, (r.start, r.end))
                } else if !with_paths {
                    return Ok(RowHit::Skip);
                } else if let Some(r) = pattern.find_first(path, false) {
                    (9.0, (r.start, r.end))
                } else if let Some(r) = pattern.find_first(path, true) {
                    (10.0, (r.start, r.end))
                } else {
                    // LIKE folds ASCII case only; this row is a non-ASCII near-miss.
                    return Ok(RowHit::Skip);
                };
                if !cx.regex_accepts(file_id, path, None)? {
                    return Ok(RowHit::Skip);
                }
                let is_path_tier = rank >= 9.0;
                // A name/path hit's snippet is the field itself, span marked.
                let snip =
                    snippet::whole_field(if is_path_tier { path } else { name }, match_range);
                row_hit(
                    row,
                    file_id,
                    path,
                    name,
                    rank,
                    rank as u8,
                    Some(snip),
                    is_path_tier,
                )
            },
        )
    }

    /// Pass B — ranks 5–6 from one FTS MATCH, verified/counted in the
    /// decompressed text.
    pub(super) fn pass_fulltext(&mut self) -> Result<bool, String> {
        let query = self.query;
        let pattern = &query.pattern;
        if pattern.literal_char_count() < 3 {
            // Below the trigram floor the MATCH can't return anything.
            return Ok(true);
        }
        // The column filter goes inside the MATCH (`text: "..."`) so only
        // bodies match. A literal term is one quoted phrase; a wildcard
        // narrows with an AND of its 3+-char segments; with none (`ab*cd`)
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
                fts_match_sql(&query.filter_sql, false),
                self.params_with_filters(vec![rusqlite::types::Value::Text(expr)]),
            ),
            None => (
                doc_scan_sql(&query.filter_sql),
                self.params_with_filters(Vec::new()),
            ),
        };
        let mut doc = crate::db::repo::DocDecoder::new()?;
        let mut lower = String::new();
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |cx, row, file_id, path, name| {
            let text = stored_text(row, &mut doc)?;

            let (rank, stage, snip) = match text {
                Some(text) => {
                    // Fold once: count, first-match and snippet all need it —
                    // including rows about to be dropped.
                    fold_into(&mut lower, text);
                    match super::text_snippet_counted(pattern, text, &lower) {
                        Some((snip, count_ci)) => {
                            if count_ci == 0 {
                                return Ok(RowHit::Skip);
                            }
                            let count_cs = pattern.count(text, false);
                            let (count, stage) = if count_cs > 0 {
                                (count_cs, 5)
                            } else {
                                (count_ci, 6)
                            };
                            (stage as f64 + count_frac(count), stage as u8, Some(snip))
                        }
                        None => {
                            let count_cs = pattern.count(text, false);
                            let (count, stage) = if count_cs > 0 {
                                (count_cs, 5)
                            } else {
                                let count_ci = pattern.count_folded(&lower);
                                if count_ci == 0 {
                                    return Ok(RowHit::Skip);
                                }
                                (count_ci, 6)
                            };
                            let snip = super::text_snippet(pattern, text, &lower);
                            (stage as f64 + count_frac(count), stage as u8, snip)
                        }
                    }
                }
                // No stored text: FTS-narrowed rows are accepted at the bottom
                // of rank 6 as count-unknown; on the full-scan fallback an
                // unverifiable row is skipped.
                None => {
                    if !narrowed {
                        return Ok(RowHit::Skip);
                    }
                    (6.0 + count_frac(1), 6, None)
                }
            };
            if !cx.regex_accepts(file_id, path, text)? {
                return Ok(RowHit::Skip);
            }

            row_hit(row, file_id, path, name, rank, stage, snip, false)
        })
    }

    /// The fuzzy passes' shared gate: the edit budget and matcher, or `None`
    /// when fuzzy is off, the term is a wildcard, it earns no budget, or it
    /// does not fit. The term goes in as typed: the matcher folds in its mask
    /// table.
    fn fuzzy_matcher(&self) -> Option<(usize, Bitap)> {
        if !self.options.fuzzy || self.query.pattern.is_wildcard() {
            return None;
        }
        let k = edit_budget(self.query.term.len(), self.options.fuzzy_max_edits)?;
        Bitap::new(self.query.term.as_bytes(), k).map(|bitap| (k, bitap))
    }

    /// Pass C — rank 7 now, rank 11 deferred: one bitap sweep over the
    /// filenames, falling back to the full path where the name misses.
    ///
    /// Narrowed by the same pigeonhole split pass D uses: at least one of the
    /// `k+1` chunks survives `≤k` edits verbatim ([`pigeonhole_chunks`]), and
    /// a separator-free chunk cannot span the `parent‖name` join, so
    /// `(name LIKE OR parent LIKE)` per chunk covers the name tier and the
    /// path tier both ([`prefilter::Required::like_predicate`]). `LIKE` folds
    /// ASCII case, a superset of the matcher's own folding — the direction
    /// the one rule in `search/prefilter.rs` allows. Any `None` along the way
    /// keeps the full scan — which, under the default edit cap of 2, is every
    /// term shorter than 9 characters (`3 × (k + 1)` with k already 2 at 6).
    ///
    /// Measured (`benches/search_perf.rs`, fuzzy pairing, 200k rows, warm):
    /// `quartzite` 113 ms → 62 plain and 113 → 59 keyed; `quartzites`
    /// 112 → 97 and 110 → 97 (its split ends in a weak 3-char chunk); the
    /// short-term fallback rows did not move.
    pub(super) fn pass_fuzzy_filename(&mut self) -> Result<bool, String> {
        let Some((k, bitap)) = self.fuzzy_matcher() else {
            return Ok(true);
        };
        let query = self.query;
        let with_paths = path_tiers_enabled(&query.pattern);

        let (predicate, terms) = pigeonhole_chunks(&query.term, k)
            .and_then(|chunks| {
                prefilter::Required::new(chunks.iter().map(|c| c.to_string()).collect())
            })
            .and_then(|req| req.like_predicate())
            .unwrap_or_else(|| ("1=1".to_string(), Vec::new()));

        let sql = format!(
            "SELECT {} FROM files f WHERE {}{}",
            HIT_COLUMNS, predicate, query.filter_sql
        );
        let params = self.params_with_filters(terms);
        self.scan_pass(
            &sql,
            params,
            CANCEL_CHECK_ROWS,
            Some(DeferSlot::FuzzyPath),
            |cx, row, file_id, path, name| {
                // The name wins when both fire. Fields are read as stored —
                // the matcher folds in its mask table; don't fold per row.
                let (rank, field, range) = match bitap.best_distance_and_first(name.as_bytes()) {
                    Some((distance, range)) => (7.0 + 0.1 * distance as f64, name, range),
                    None if with_paths => match bitap.best_distance_and_first(path.as_bytes()) {
                        Some((distance, range)) => (11.0 + 0.1 * distance as f64, path, range),
                        None => return Ok(RowHit::Skip),
                    },
                    None => return Ok(RowHit::Skip),
                };
                if !cx.regex_accepts(file_id, path, None)? {
                    return Ok(RowHit::Skip);
                }
                // The matched field itself, span marked. Don't window it: that
                // breaks the ranges-index-the-field contract.
                let snip = Some(snippet::whole_field(field, range));
                let is_path_tier = rank >= 11.0;
                // The stage is stamped, not truncated from `rank`: a distance
                // of 10 makes rank 8.0, and truncating would file a filename
                // hit under stage 8, the fuzzy full-text tier.
                let stage = if is_path_tier { 11 } else { 7 };
                row_hit(row, file_id, path, name, rank, stage, snip, is_path_tier)
            },
        )
    }

    /// Pass D — rank 8, bitap over every stored document text.
    pub(super) fn pass_fuzzy_fulltext(&mut self) -> Result<bool, String> {
        let Some((k, bitap)) = self.fuzzy_matcher() else {
            return Ok(true);
        };

        // Candidate prefilter: the pigeonhole split is a sound superset
        // narrowing — see [`pigeonhole_chunks`] and `search/prefilter.rs`
        // (the one rule). `None` means too short to split; the pass scans.
        let (sql, params) = match pigeonhole_chunks(&self.query.term, k) {
            Some(chunks) => {
                // Every chunk is quoted into inertness: a slice of user text
                // can contain FTS5 syntax, and unquoted it is a syntax error.
                let expr = chunks
                    .iter()
                    .map(|c| format!("text: {}", quote_phrase(c)))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                (
                    fts_match_sql(&self.query.filter_sql, true),
                    self.params_with_filters(vec![rusqlite::types::Value::Text(format!(
                        "({})",
                        expr
                    ))]),
                )
            }
            None => (
                doc_scan_sql(&self.query.filter_sql),
                self.params_with_filters(Vec::new()),
            ),
        };
        // No fold buffer: the matcher folds in its mask table — don't
        // copy-and-fold per document.
        let mut doc = crate::db::repo::DocDecoder::new()?;
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |cx, row, file_id, path, name| {
            let Some(text) = stored_text(row, &mut doc)? else {
                return Ok(RowHit::Skip);
            };
            let Some((count, snip)) = super::fuzzy_snippet(&bitap, text) else {
                return Ok(RowHit::Skip);
            };
            if !cx.regex_accepts(file_id, path, Some(text))? {
                return Ok(RowHit::Skip);
            }
            row_hit(
                row,
                file_id,
                path,
                name,
                8.0 + count_frac(count),
                8,
                Some(snip),
                false,
            )
        })
    }

    /// Regex-only pass over `files`: name hits reuse rank 4, path hits defer
    /// to rank 10, so stage-based rendering needs no new cases. Narrowed by
    /// the pattern's required literals
    /// ([`crate::search::prefilter::Required::like_predicate`]); a pattern
    /// with no literal (`\d+`) still scans every name and path per keystroke.
    pub(super) fn pass_regex_name(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let (predicate, terms) = match re.required().and_then(|r| r.like_predicate()) {
            Some((sql, params)) => (sql, params),
            None => ("1=1".to_string(), Vec::new()),
        };
        let sql = format!(
            "SELECT {} FROM files f WHERE {}{}",
            HIT_COLUMNS, predicate, query.filter_sql
        );
        let params = self.params_with_filters(terms);
        self.scan_pass(
            &sql,
            params,
            CANCEL_CHECK_ROWS,
            Some(DeferSlot::Path),
            |_cx, row, file_id, path, name| {
                let (rank, match_range, is_path_tier) = match re.find_first(name) {
                    Some(r) => (4.0, (r.start, r.end), false),
                    None => match re.find_first(path) {
                        Some(r) => (10.0, (r.start, r.end), true),
                        None => return Ok(RowHit::Skip),
                    },
                };
                let snip =
                    snippet::whole_field(if is_path_tier { path } else { name }, match_range);
                row_hit(
                    row,
                    file_id,
                    path,
                    name,
                    rank,
                    rank as u8,
                    Some(snip),
                    is_path_tier,
                )
            },
        )
    }

    /// Regex-only pass over every stored document text, reusing rank 6.
    /// Narrowed by required literals through
    /// [`crate::search::prefilter::Required::fts_expr`]; without a usable set
    /// this decompresses and regex-scans every document per keystroke.
    pub(super) fn pass_regex_content(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let (sql, params) = match re.required().and_then(|r| r.fts_expr()) {
            Some(expr) => (
                fts_match_sql(&query.filter_sql, true),
                self.params_with_filters(vec![rusqlite::types::Value::Text(expr)]),
            ),
            None => (
                doc_scan_sql(&query.filter_sql),
                self.params_with_filters(Vec::new()),
            ),
        };
        let snippet_opts = snippet::Options {
            approx_chars: SNIPPET_WINDOW_CHARS,
        };
        let mut doc = crate::db::repo::DocDecoder::new()?;
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |_cx, row, file_id, path, name| {
            let Some(text) = stored_text(row, &mut doc)? else {
                return Ok(RowHit::Skip);
            };
            let count = re.count(text);
            if count == 0 {
                return Ok(RowHit::Skip);
            }
            // A greedy user regex can match megabytes; clamp before the window.
            let snip = re.find_first(text).map(|r| {
                let r = clamp_match_range(text, r, SNIPPET_WINDOW_CHARS);
                snippet::window_around(text, (r.start, r.end), &snippet_opts)
            });
            row_hit(
                row,
                file_id,
                path,
                name,
                6.0 + count_frac(count),
                6,
                snip,
                false,
            )
        })
    }
}
