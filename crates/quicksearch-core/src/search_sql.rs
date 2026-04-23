//! SQL builders for the GUI's three legacy search modes (full-text,
//! filename, duplicate). Lives in core so it has unit-test coverage; the
//! GUI just composes these into per-page queries.
//!
//! For the structured Baloo-subset query language (`type:`, `modified:`,
//! …), see [`crate::query`].

/// All inputs needed to run (and re-run) one of the three search modes.
/// Cached after a fresh search so paging buttons don't have to rebuild
/// from form state.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchArgs {
    pub search_type: String,
    pub term: String,
    pub fulltext_exact: bool,
    pub fulltext_case_sensitive: bool,
}

/// SQL that counts every row matching `args`, ignoring pagination. Used to
/// drive the "page X of Y" UI. For very large FTS hit sets this can take a
/// noticeable fraction of the per-page query time, but it only runs on a
/// fresh search — page navigation reuses the cached total.
pub fn build_count(args: &SearchArgs) -> Result<String, String> {
    match args.search_type.as_str() {
        "fulltext" => {
            let where_clause = build_fulltext_where(args)?;
            Ok(format!(
                "SELECT COUNT(*) FROM searchabletext AS st WHERE {}",
                where_clause
            ))
        }
        "filename" => {
            if args.term.trim().is_empty() {
                return Err("Please enter a filename pattern".into());
            }
            Ok(format!(
                "SELECT COUNT(*) FROM files WHERE name LIKE '%{}%'",
                args.term.replace('\'', "''")
            ))
        }
        "duplicates" => Ok(
            "SELECT COUNT(*) FROM (SELECT 1 FROM files WHERE hash IS NOT NULL \
             GROUP BY hash HAVING count(*) > 1)"
                .into(),
        ),
        _ => Err("Unknown search type".into()),
    }
}

/// SQL that returns one page of results. Columns emitted by the `fulltext`
/// branch are `(name, path, file_id, text_zstd)` — the snippet is rendered
/// in Rust from the zstd-compressed `documents_text` row (FTS5 is
/// contentless, so SQLite's `snippet()` doesn't work on it). The GUI
/// should use [`crate::indexing::IndexingService::execute_fulltext_search`]
/// which stitches the decompress + snippet step on top of this SQL.
pub fn build_select(args: &SearchArgs, limit: u32, offset: u32) -> Result<String, String> {
    match args.search_type.as_str() {
        "fulltext" => {
            let where_clause = build_fulltext_where(args)?;
            Ok(format!(
                "SELECT f.name, f.path, f.id, dt.text_zstd \
                 FROM searchabletext AS st \
                 JOIN files f ON f.id = st.rowid \
                 LEFT JOIN documents_text dt ON dt.file_id = f.id \
                 WHERE {} ORDER BY rank LIMIT {} OFFSET {}",
                where_clause, limit, offset
            ))
        }
        "filename" => {
            if args.term.trim().is_empty() {
                return Err("Please enter a filename pattern".into());
            }
            Ok(format!(
                "SELECT name, path FROM files WHERE name LIKE '%{}%' ORDER BY name LIMIT {} OFFSET {}",
                args.term.replace('\'', "''"),
                limit,
                offset
            ))
        }
        "duplicates" => Ok(format!(
            "SELECT name, count(*) as cnt, path FROM files WHERE hash IS NOT NULL \
             GROUP BY hash HAVING cnt > 1 ORDER BY cnt DESC LIMIT {} OFFSET {}",
            limit, offset
        )),
        _ => Err("Unknown search type".into()),
    }
}

/// Translate the user-typed term into the FTS5 `MATCH` expression and any
/// supplemental case-sensitivity filters. Shared by count and select so
/// pagination doesn't accidentally diverge from the totals.
fn build_fulltext_where(args: &SearchArgs) -> Result<String, String> {
    let trimmed = args.term.trim();
    if trimmed.is_empty() {
        return Err("Please enter a search term".into());
    }

    // Strip FTS5 control characters that confuse the parser. Replace with
    // spaces so word boundaries survive.
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if matches!(
                c,
                ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '~' | '"'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();

    let tokens: Vec<&str> = sanitized.split_whitespace().collect();
    if tokens.is_empty() {
        return Err("Please enter a valid search term".into());
    }

    // Trigram tokenizer needs each word to be at least 3 characters. Exact
    // phrase mode skips this filter — a quoted phrase of short words still
    // matches because the trigrams overlap the spaces between words.
    let words: Vec<&str> = if args.fulltext_exact {
        tokens
    } else {
        let filtered: Vec<&str> = tokens
            .into_iter()
            .filter(|w| w.chars().count() >= 3)
            .collect();
        if filtered.is_empty() {
            return Err(
                "Trigram index needs each word to be at least 3 characters unless you use exact phrase search."
                    .into(),
            );
        }
        filtered
    };

    let sql_quote = |s: &str| s.replace('\'', "''");

    let fts_match = if args.fulltext_exact {
        let phrase = words.join(" ");
        format!("\"{}\"", phrase.replace('"', "\"\""))
    } else {
        words.join(" AND ")
    };

    // Contentless FTS5 doesn't store column text, so case-sensitive
    // filtering can't live in SQL anymore. It's re-applied in
    // `IndexingService::execute_fulltext_search` by checking the
    // decompressed body text for literal-case matches before returning
    // the row. The MATCH itself stays case-insensitive (tokenizer folds),
    // which is the correct candidate-set for a post-filter.
    let where_clause = format!("st.text MATCH '{}'", sql_quote(&fts_match));
    Ok(where_clause)
}

/// Pull out the raw words the user typed so the snippet renderer and the
/// case-sensitive post-filter can see the same tokens `build_fulltext_where`
/// fed into FTS5. Returns empty when the user's term is empty or contains
/// only too-short words under the non-exact path.
pub fn fulltext_terms(args: &SearchArgs) -> Vec<String> {
    let trimmed = args.term.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if matches!(
                c,
                ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '~' | '"'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let tokens: Vec<String> = sanitized
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    if args.fulltext_exact {
        // One composite phrase. Snippet rendering wants to highlight the
        // whole phrase contiguously; the renderer supports multiple terms
        // already so we collapse to the joined form.
        vec![tokens.join(" ")]
    } else {
        tokens
            .into_iter()
            .filter(|w| w.chars().count() >= 3)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open_or_recreate, repo::{insert_file, set_content_done, NewFile}};
    use crate::mime::FileType;

    fn args(search_type: &str, term: &str) -> SearchArgs {
        SearchArgs {
            search_type: search_type.into(),
            term: term.into(),
            fulltext_exact: false,
            fulltext_case_sensitive: false,
        }
    }

    #[test]
    fn fulltext_select_has_limit_and_offset() {
        let sql = build_select(&args("fulltext", "hello world"), 50, 100).unwrap();
        assert!(sql.contains("LIMIT 50"));
        assert!(sql.contains("OFFSET 100"));
        assert!(sql.contains("ORDER BY rank"));
        // Snippet rendering moved to Rust; SQL returns the compressed blob
        // so the post-processor can decompress + highlight.
        assert!(sql.contains("dt.text_zstd"), "got {sql}");
    }

    #[test]
    fn fulltext_case_sensitive_no_longer_in_sql() {
        let mut a = args("fulltext", "Hello");
        a.fulltext_case_sensitive = true;
        let sql = build_select(&a, 50, 0).unwrap();
        // Post-filter lives in Rust now — no instr() or st.text reference.
        assert!(!sql.contains("instr("), "got {sql}");
    }

    #[test]
    fn fulltext_terms_extract_non_exact() {
        let a = args("fulltext", "the quick brown");
        let t = fulltext_terms(&a);
        // Short words like "the" are dropped (trigram min length 3 applies
        // in non-exact mode — matches the SQL build rules).
        assert!(t.iter().any(|s| s == "quick"));
        assert!(t.iter().any(|s| s == "brown"));
    }

    #[test]
    fn fulltext_terms_exact_mode_returns_joined_phrase() {
        let mut a = args("fulltext", "hello world");
        a.fulltext_exact = true;
        let t = fulltext_terms(&a);
        assert_eq!(t, vec!["hello world".to_string()]);
    }

    #[test]
    fn fulltext_count_lacks_pagination_and_join() {
        let sql = build_count(&args("fulltext", "hello world")).unwrap();
        assert!(sql.starts_with("SELECT COUNT(*)"));
        assert!(!sql.contains("LIMIT"));
        assert!(!sql.contains("OFFSET"));
        assert!(!sql.contains("JOIN files"));
    }

    #[test]
    fn fulltext_short_words_filtered_unless_exact() {
        let err = build_select(&args("fulltext", "a b"), 50, 0).unwrap_err();
        assert!(err.contains("3 characters"));
        let mut a = args("fulltext", "a b");
        a.fulltext_exact = true;
        assert!(build_select(&a, 50, 0).is_ok());
    }

    #[test]
    fn fulltext_quotes_are_escaped() {
        let sql = build_select(&args("fulltext", "it's working"), 50, 0).unwrap();
        // SQL literals double single quotes.
        assert!(sql.contains("it''s"));
    }

    #[test]
    fn filename_select_has_limit_offset_and_order() {
        let sql = build_select(&args("filename", "report"), 50, 0).unwrap();
        assert!(sql.contains("LIMIT 50"));
        assert!(sql.contains("OFFSET 0"));
        assert!(sql.contains("ORDER BY name"));
        assert!(sql.contains("name LIKE '%report%'"));
    }

    #[test]
    fn filename_empty_term_errors() {
        assert!(build_select(&args("filename", "   "), 50, 0).is_err());
        assert!(build_count(&args("filename", "")).is_err());
    }

    #[test]
    fn duplicates_select_has_limit_offset() {
        let sql = build_select(&args("duplicates", ""), 50, 100).unwrap();
        assert!(sql.contains("LIMIT 50"));
        assert!(sql.contains("OFFSET 100"));
        assert!(sql.contains("GROUP BY hash"));
    }

    #[test]
    fn unknown_search_type_errors() {
        assert!(build_select(&args("nope", ""), 50, 0).is_err());
        assert!(build_count(&args("nope", "")).is_err());
    }

    fn tmp_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-search-sql-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    /// End-to-end: seed three rows, run count + paged select, verify
    /// pagination boundaries actually behave on a real DB.
    #[test]
    fn end_to_end_pagination_smoke() {
        let p = tmp_path();
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        {
            let tx = conn.transaction().unwrap();
            for i in 0..7 {
                let path = format!("/tmp/file_{}.txt", i);
                let id = insert_file(
                    &tx,
                    &NewFile {
                        name: &format!("file_{}.txt", i),
                        path: &path,
                        parent: "/tmp",
                        size: 1,
                        mtime: 1,
                        inode: None,
                        device_id: None,
                        mime: Some("text/plain"),
                        ftype: FileType::TEXT,
                        hash: None,
                    },
                )
                .unwrap()
                .expect("unique path");
                set_content_done(&tx, id, &format!("file_{}.txt", i), "shared body content", &[], true).unwrap();
            }
            tx.commit().unwrap();
        }

        // Count: all 7 rows match "shared".
        let count_sql = build_count(&args("fulltext", "shared body content")).unwrap();
        let n: i64 = conn.query_row(&count_sql, [], |r| r.get(0)).unwrap();
        assert_eq!(n, 7);

        // Page 1, page_size 3 → 3 rows.
        let sel1 = build_select(&args("fulltext", "shared body content"), 3, 0).unwrap();
        let rows1: Vec<String> = conn
            .prepare(&sel1)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows1.len(), 3);

        // Page 3, page_size 3 → 1 row (offset 6, 7 total).
        let sel3 = build_select(&args("fulltext", "shared body content"), 3, 6).unwrap();
        let rows3: Vec<String> = conn
            .prepare(&sel3)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows3.len(), 1);

        // Filename pagination on the same fixture.
        let fn_count = build_count(&args("filename", "file_")).unwrap();
        let n2: i64 = conn.query_row(&fn_count, [], |r| r.get(0)).unwrap();
        assert_eq!(n2, 7);

        let fn_sel = build_select(&args("filename", "file_"), 5, 0).unwrap();
        let rows: Vec<String> = conn
            .prepare(&fn_sel)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 5);

        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
