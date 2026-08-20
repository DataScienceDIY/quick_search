//! The cascade's six scan passes: a prelude and a per-row classifier
//! each, all driven through [`Cx::scan_pass`]'s shared skeleton.

use super::*;

/// What a pass's per-row classifier decided about one row.
enum RowHit {
    Skip,
    Emit(SearchHit),
    Defer(SearchHit),
}

/// Fold `text` into `dst` in place, reusing its allocation.
///
/// The ASCII fold is byte-length preserving, which is what lets the cascade
/// use offsets found in the folded copy against the unfolded original.
fn fold_into(dst: &mut String, text: &str) {
    dst.clear();
    dst.push_str(text);
    dst.make_ascii_lowercase();
}

/// Which [`Deferred`] buffer a scan's held-back hits go to.
enum DeferSlot {
    /// Ranks 9–10, flushed by [`Pass::Path`]. Shared by passes A and E,
    /// which never appear in the same pass list.
    Path,
    /// Rank 11, flushed by [`Pass::FuzzyPath`]. Filled by pass C.
    FuzzyPath,
}

impl<'a> Cx<'a> {
    /// One scan pass: prepare `sql`, stream its rows, classify each into
    /// emit/defer/skip, and flush. Every pass shares this skeleton; only the
    /// prelude and `classify` differ. Returns Ok(false) on cancellation.
    ///
    /// `cancel_every` is the cancellation-check cadence: row-cheap scans
    /// check every [`CANCEL_CHECK_ROWS`], decompression-heavy scans every
    /// row. `classify` sees only rows that already passed [`Cx::skip`]. A
    /// `Defer`red hit lands in `defer_slot` at the end of the scan rather
    /// than being emitted — path-tier ranks sort below stages that have not
    /// run yet, so they are held back and never flushed mid-scan.
    fn scan_pass(
        &mut self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
        cancel_every: usize,
        defer_slot: Option<DeferSlot>,
        mut classify: impl FnMut(&mut Self, &rusqlite::Row<'_>, i64, &str) -> Result<RowHit, String>,
    ) -> Result<bool, String> {
        let conn = self.conn;
        // Cached: a search re-runs the same six statements on every keystroke,
        // and only the bound term changes between them — the filter SQL each
        // one interpolates is fixed for the life of the query.
        let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;

        let mut buf: Vec<SearchHit> = Vec::new();
        let mut overflowed = false;
        let mut deferred = Deferred::default();
        let mut scanned = 0usize;
        let mut clock = FlushClock::new();
        // Set when the loop stops on the display limit rather than on the end
        // of the candidate set — rows were left unexamined, so the result set
        // is cut whatever the final flush happens to hold.
        let mut cut_short = false;
        // The reassembled path for the row in hand. One buffer for the whole
        // scan, refilled per row: `files` stores `parent` and `name` and no
        // longer a third column holding their concatenation, so this is where
        // the concatenation happens. Reused rather than allocated because it
        // runs for every *scanned* row — a full-table scan on three of the
        // passes — while only the few that become hits keep an owned copy.
        //
        // A local, not a field on `self`: the classifiers take `&mut Self`, and
        // a buffer borrowed out of `self` could not be handed to them.
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
            // Parent first, and no separator between them: every stored parent
            // already ends in one. See `file_handling::dir_to_db_parent`.
            path.push_str(borrowed(2)?);
            path.push_str(borrowed(1)?);
            if self.skip(file_id, &path) {
                continue;
            }
            match classify(self, row, file_id, &path)? {
                RowHit::Skip => {}
                RowHit::Emit(hit) => {
                    buf.push(hit);
                    overflowed |= self.enforce_cap(&mut buf);
                    self.flush_if_due(&mut buf, &mut clock);
                    // Stop once the display limit is full. Everything past
                    // this point is work `flush_pass` would throw away, and it
                    // is not small: the full-text passes decompress the
                    // document, fold a copy of it and cut a snippet, per row.
                    // `cascade::run` makes the same test between passes;
                    // without this one a single pass over a common term runs
                    // to the end of its candidate set.
                    //
                    // Tested *here* rather than at the top of the loop, where
                    // it is tempting: a scanned row is not a match. It may be
                    // one an earlier pass already emitted, or one `classify`
                    // rejects — the filename pass's SQL is deliberately a
                    // superset feeding both the name and the path tier — so
                    // stopping on one would end the pass over a row that was
                    // never going to be shown.
                    //
                    // The break is recorded rather than turned into `limited`
                    // on the spot. `flush_pass` decides that, because it knows
                    // how much it had to truncate; claiming it here would say
                    // a set was cut whenever it happened to end exactly at the
                    // limit. But leaving the scan *unfinished* silently is the
                    // opposite error — see the flush below.
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
        // After the flush, so it cannot be undone by one that truncated
        // nothing. A scan that broke on the limit left rows unexamined, and
        // the two places that would otherwise notice both miss the boundary
        // case: `flush_pass` compares `buf.len() > room`, which is `0 > 0`
        // when a mid-scan flush landed exactly on the limit, and
        // `cascade::run` tests `remaining()` at the top of the *next* pass,
        // of which the last pass has none. Without this a cut result set can
        // report itself complete.
        self.limited |= cut_short;
        Ok(true)
    }

    /// Pass A — ranks 1–4 now, ranks 9–10 deferred, from one `files` scan.
    /// Returns Ok(false) on cancellation.
    pub(super) fn pass_filename(&mut self) -> Result<bool, String> {
        let query = self.query;
        let pattern = &query.pattern;
        let with_paths = path_tiers_enabled(pattern);
        // Wildcard patterns turn each star into an unescaped `%`; the
        // substring wrap absorbs leading/trailing stars. User `%`/`_`
        // remain escaped literals either way.
        let like = format!(
            "%{}%",
            pattern
                .segments()
                .iter()
                .map(|s| escape_like(s))
                .collect::<Vec<_>>()
                .join("%")
        );
        // There is no `path` column to `LIKE` against any more, and the
        // prefilter has to stay a *superset* of what the classifier accepts or
        // real hits vanish. Three cases:
        //
        // * Name tier only — unchanged, `name LIKE`.
        // * Path tiers, and the pattern is a single segment containing no
        //   separator: `name LIKE ? OR parent LIKE ?` is then exactly
        //   equivalent to the old `path LIKE ?`. A match inside `parent || name`
        //   either sits wholly in one or straddles the boundary, and the
        //   boundary character is a separator the pattern does not contain. This
        //   is ordinary typing, so it is the case worth keeping cheap.
        // * Anything else — a multi-segment wildcard whose `%` can span the
        //   boundary (`doc*q3` over `/x/docs/q3.txt`), or a term with a
        //   separator in it. No SQL predicate on one column covers those, so
        //   scan and let the classifier decide, exactly as passes C and E do.
        let straddles = pattern.segments().len() > 1
            || pattern
                .segments()
                .iter()
                .any(|s| s.contains(std::path::MAIN_SEPARATOR));
        let (predicate, terms) = match (with_paths, straddles) {
            (false, _) => (
                "f.name LIKE ? ESCAPE '\\'".to_string(),
                vec![rusqlite::types::Value::Text(like)],
            ),
            (true, false) => (
                "(f.name LIKE ? ESCAPE '\\' OR f.parent LIKE ? ESCAPE '\\')".to_string(),
                vec![
                    rusqlite::types::Value::Text(like.clone()),
                    rusqlite::types::Value::Text(like),
                ],
            ),
            (true, true) => ("1=1".to_string(), Vec::new()),
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
            |cx, row, file_id, path| {
                let name: String = col(row, 1)?;
                // Folding is byte-length preserving, so folded offsets are
                // valid in the original. For wildcards, tiers 1/2 mean "the
                // whole name matches the pattern".
                let (rank, match_range) = if pattern.whole_match(&name, false) {
                    (1.0, (0, name.len()))
                } else if pattern.whole_match(&name, true) {
                    (2.0, (0, name.len()))
                } else if let Some(r) = pattern.find_first(&name, false) {
                    (3.0, (r.start, r.end))
                } else if let Some(r) = pattern.find_first(&name, true) {
                    (4.0, (r.start, r.end))
                } else if !with_paths {
                    return Ok(RowHit::Skip);
                } else if let Some(r) = pattern.find_first(path, false) {
                    (9.0, (r.start, r.end))
                } else if let Some(r) = pattern.find_first(path, true) {
                    (10.0, (r.start, r.end))
                } else {
                    // LIKE folds ASCII case only; a row that matched it but
                    // neither field is a non-ASCII near-miss. Drop it.
                    return Ok(RowHit::Skip);
                };
                if !cx.regex_accepts(file_id, path, None)? {
                    return Ok(RowHit::Skip);
                }
                let is_path_tier = rank >= 9.0;
                // The "snippet" of a name or path hit is that field itself
                // with the matched span marked.
                let snip = snippet::whole_field(
                    if is_path_tier { path } else { name.as_str() },
                    match_range,
                );
                let (size, mtime) = size_and_mtime(row)?;
                let hit = SearchHit {
                    file_id,
                    name,
                    path: path.to_string(),
                    size,
                    mtime,
                    rank,
                    stage: rank as u8,
                    snippet: Some(snip),
                };
                Ok(if is_path_tier {
                    RowHit::Defer(hit)
                } else {
                    RowHit::Emit(hit)
                })
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
        // One decoder and one fold buffer for the whole scan; both are reused
        // per row rather than reallocated.
        let mut doc = crate::db::repo::DocDecoder::new()?;
        let mut lower = String::new();
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |cx, row, file_id, path| {
            let blob: Option<&[u8]> = row
                .get_ref(5)
                .map_err(|e| e.to_string())?
                .as_blob_or_null()
                .map_err(|e| e.to_string())?;
            let text = blob.and_then(|b| doc.decode(b));

            let (rank, stage, snip) = match text {
                Some(text) => {
                    // Fold once: the case-insensitive count, the first-match
                    // search and the snippet extraction all need it, and
                    // nearly every candidate takes this path.
                    let mut folded = false;
                    let (count, stage) = {
                        let count_cs = pattern.count(text, false);
                        if count_cs > 0 {
                            (count_cs, 5)
                        } else {
                            fold_into(&mut lower, text);
                            folded = true;
                            let count_ci = pattern.count_folded(&lower);
                            if count_ci > 0 {
                                (count_ci, 6)
                            } else {
                                // Folded/unordered FTS candidate: the
                                // pattern never occurs — drop it.
                                return Ok(RowHit::Skip);
                            }
                        }
                    };
                    if !folded {
                        fold_into(&mut lower, text);
                    }
                    let snip = super::text_snippet(pattern, text, &lower);
                    (stage as f64 + count_frac(count), stage as u8, snip)
                }
                // No stored text: can't case-verify or count. On the
                // FTS-narrowed path accept at the bottom of rank 6 as
                // count-unknown; on the full-scan fallback there is no FTS
                // evidence at all, so an unverifiable row is skipped.
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

            let (size, mtime) = size_and_mtime(row)?;
            Ok(RowHit::Emit(SearchHit {
                file_id,
                name: col(row, 1)?,
                path: path.to_string(),
                size,
                mtime,
                rank,
                stage,
                snippet: snip,
            }))
        })
    }

    /// Pass C — rank 7 now, rank 11 deferred: one bitap sweep over every
    /// filename, falling back to the full path where the name misses.
    pub(super) fn pass_fuzzy_filename(&mut self) -> Result<bool, String> {
        if !self.options.fuzzy {
            return Ok(true);
        }
        let query = self.query;
        // Bitap is a literal matcher; wildcard terms don't fuzz.
        if query.pattern.is_wildcard() {
            return Ok(true);
        }
        let folded_term = query.term.to_ascii_lowercase();
        let Some(k) = edit_budget(folded_term.len(), self.options.fuzzy_max_edits) else {
            return Ok(true);
        };
        let Some(bitap) = Bitap::new(folded_term.as_bytes(), k) else {
            return Ok(true);
        };
        let with_paths = path_tiers_enabled(&query.pattern);

        let sql = format!(
            "SELECT {} FROM files f WHERE 1=1{}",
            HIT_COLUMNS, query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        self.scan_pass(
            &sql,
            params,
            CANCEL_CHECK_ROWS,
            Some(DeferSlot::FuzzyPath),
            |cx, row, file_id, path| {
                let name: String = col(row, 1)?;
                // The name wins when both fire; only a name miss falls
                // through to the path tier.
                let folded_name = name.to_ascii_lowercase();
                let (rank, field, range) = match bitap
                    .best_distance_and_first(folded_name.as_bytes())
                {
                    Some((distance, range)) => (7.0 + 0.1 * distance as f64, name.as_str(), range),
                    None if with_paths => {
                        let folded_path = path.to_ascii_lowercase();
                        match bitap.best_distance_and_first(folded_path.as_bytes()) {
                            Some((distance, range)) => (11.0 + 0.1 * distance as f64, path, range),
                            None => return Ok(RowHit::Skip),
                        }
                    }
                    None => return Ok(RowHit::Skip),
                };
                if !cx.regex_accepts(file_id, path, None)? {
                    return Ok(RowHit::Skip);
                }
                // The matched field itself with the fuzzy span marked — the
                // same shape pass A emits, and what `SearchHit::snippet`
                // documents for the name and path tiers. Windowing it here
                // used to hand back a *suffix* whenever the match sat past
                // two thirds of the way through, which broke that contract
                // and left a frontend unable to line the ranges up against
                // the field it paints.
                let snip = Some(snippet::whole_field(field, range));
                let is_path_tier = rank >= 11.0;
                let (size, mtime) = size_and_mtime(row)?;
                let hit = SearchHit {
                    file_id,
                    name,
                    path: path.to_string(),
                    size,
                    mtime,
                    rank,
                    // Stamped, not truncated from `rank`: this is the one pass
                    // whose ranks carry a fraction large enough to reach the
                    // next integer. `edit_budget` is only warned about above
                    // 3, so a distance of 10 makes rank 8.0 — and truncating
                    // that would file a *filename* hit under stage 8, the
                    // fuzzy full-text tier, telling every frontend to render
                    // it as a content match.
                    stage: if is_path_tier { 11 } else { 7 },
                    snippet: snip,
                };
                Ok(if is_path_tier {
                    RowHit::Defer(hit)
                } else {
                    RowHit::Emit(hit)
                })
            },
        )
    }

    /// Pass D — rank 8, bitap over every stored document text.
    pub(super) fn pass_fuzzy_fulltext(&mut self) -> Result<bool, String> {
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
        // One decoder and one fold buffer for the whole scan, reused per row.
        let mut doc = crate::db::repo::DocDecoder::new()?;
        let mut folded = String::new();
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |cx, row, file_id, path| {
            let blob: Option<&[u8]> = row
                .get_ref(5)
                .map_err(|e| e.to_string())?
                .as_blob_or_null()
                .map_err(|e| e.to_string())?;
            let Some(text) = blob.and_then(|b| doc.decode(b)) else {
                return Ok(RowHit::Skip);
            };
            // ASCII folding is byte-length preserving, so ranges found in
            // the folded buffer are valid in the original.
            fold_into(&mut folded, text);
            let Some((count, snip)) = super::fuzzy_snippet(&bitap, text, &folded) else {
                return Ok(RowHit::Skip);
            };
            if !cx.regex_accepts(file_id, path, Some(text))? {
                return Ok(RowHit::Skip);
            }
            let (size, mtime) = size_and_mtime(row)?;
            Ok(RowHit::Emit(SearchHit {
                file_id,
                name: col(row, 1)?,
                path: path.to_string(),
                size,
                mtime,
                rank: 8.0 + count_frac(count),
                stage: 8,
                snippet: Some(snip),
            }))
        })
    }

    /// Regex-only pass over `files`: the regex bypasses the FTS trigram
    /// entirely and runs on every name, falling back to the full path.
    /// Name hits reuse rank 4, path hits defer to rank 10, so the GUI's
    /// stage-based rendering needs no new cases.
    pub(super) fn pass_regex_name(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let sql = format!(
            "SELECT {} FROM files f WHERE 1=1{}",
            HIT_COLUMNS, query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        self.scan_pass(
            &sql,
            params,
            CANCEL_CHECK_ROWS,
            Some(DeferSlot::Path),
            |_cx, row, file_id, path| {
                let name: String = col(row, 1)?;
                // The name is the better hit; only a name miss falls through
                // to the path tier — mirroring pass A.
                let (rank, match_range, is_path_tier) = match re.find_first(&name) {
                    Some(r) => (4.0, (r.start, r.end), false),
                    None => match re.find_first(path) {
                        Some(r) => (10.0, (r.start, r.end), true),
                        None => return Ok(RowHit::Skip),
                    },
                };
                let snip = snippet::whole_field(
                    if is_path_tier { path } else { name.as_str() },
                    match_range,
                );
                let (size, mtime) = size_and_mtime(row)?;
                let hit = SearchHit {
                    file_id,
                    name,
                    path: path.to_string(),
                    size,
                    mtime,
                    rank,
                    stage: rank as u8,
                    snippet: Some(snip),
                };
                Ok(if is_path_tier {
                    RowHit::Defer(hit)
                } else {
                    RowHit::Emit(hit)
                })
            },
        )
    }

    /// Regex-only pass over every stored document text, reusing rank 6.
    pub(super) fn pass_regex_content(&mut self) -> Result<bool, String> {
        let query = self.query;
        let re = query.regex.as_ref().expect("regex-only pass list");
        let sql = format!(
            "SELECT {}, dt.text_zstd \
             FROM documents_text dt JOIN files f ON f.id = dt.file_id WHERE 1=1{}",
            HIT_COLUMNS, query.filter_sql
        );
        let params = self.params_with_filters(Vec::new());
        let snippet_opts = snippet::Options {
            approx_chars: SNIPPET_WINDOW_CHARS,
        };
        // One decoder for the whole scan, reused per row.
        let mut doc = crate::db::repo::DocDecoder::new()?;
        // Decompression dominates: check cancellation every row.
        self.scan_pass(&sql, params, 1, None, |_cx, row, file_id, path| {
            let blob: Option<&[u8]> = row
                .get_ref(5)
                .map_err(|e| e.to_string())?
                .as_blob_or_null()
                .map_err(|e| e.to_string())?;
            let Some(text) = blob.and_then(|b| doc.decode(b)) else {
                return Ok(RowHit::Skip);
            };
            let count = re.count(text);
            if count == 0 {
                return Ok(RowHit::Skip);
            }
            // A greedy user regex can match megabytes; clamp the range
            // before the snippet window is cut.
            let snip = re.find_first(text).map(|r| {
                let r = clamp_match_range(text, r, SNIPPET_WINDOW_CHARS);
                snippet::window_around(text, (r.start, r.end), &snippet_opts)
            });
            let (size, mtime) = size_and_mtime(row)?;
            Ok(RowHit::Emit(SearchHit {
                file_id,
                name: col(row, 1)?,
                path: path.to_string(),
                size,
                mtime,
                rank: 6.0 + count_frac(count),
                stage: 6,
                snippet: snip,
            }))
        })
    }
}
