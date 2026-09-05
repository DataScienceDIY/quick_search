//! Duplicate-file listing, grouped by content hash. Synchronous — callers
//! run it on their own worker thread.
//!
//! # Why this is three queries and not one aggregate
//!
//! The obvious form — `SELECT hash, COUNT(*), SUM(size) - MAX(size) … GROUP BY
//! hash` — reads `size`, which lives only in the table row. `idx_files_hash`
//! covers `hash` alone, so SQLite walks the index in hash order and fetches the
//! wide `files` row behind *every* entry: one random page per indexed file, each
//! of which SQLCipher must decrypt and verify. At 3M files that is the entire
//! cost of the scan.
//!
//! Naming nothing but `hash` keeps the scan inside the index, and `size` is then
//! needed once per *candidate group* rather than once per row — measured 3M row
//! fetches down to 60k on a 3M-file index, and 2.75s down to 0.56s with the
//! whole database already in RAM (the gap widens on cold or encrypted storage).
//! That property is load-bearing and easy to lose to an innocent-looking edit,
//! so `tests::candidate_scan_stays_inside_the_index` reads the query plan back
//! and fails on anything but a covering scan.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db;

#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    pub hash: Vec<u8>,
    pub count: i64,
    pub total_size: i64,
    /// Bytes reclaimable by deduplicating: `size × (count - 1)`; the sort key.
    pub redundant_size: i64,
    /// `(file_id, name, path, size, mtime)` per member, path-ordered.
    pub members: Vec<(i64, String, String, u64, i64)>,
}

impl DuplicateGroup {
    /// The group's identity, for a UI that wants to remember one: lowercase
    /// hex of the hash. Stable across renames and moves, because the hash
    /// covers the content and nothing about where the files live.
    pub fn hash_hex(&self) -> String {
        crate::security::hex_encode(&self.hash)
    }
}

/// A ranked group, ordered so that **greater is better**: more reclaimable
/// bytes first, and the lower rowid on a tie so repeated scans of an unchanged
/// index list the same groups in the same order.
#[derive(Debug, PartialEq, Eq)]
struct Ranked {
    redundant: i64,
    count: i64,
    size: i64,
    rowid: i64,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Ranked) -> Ordering {
        self.redundant
            .cmp(&other.redundant)
            .then_with(|| other.rowid.cmp(&self.rowid))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Ranked) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The `limit` hash groups with the most reclaimable bytes, largest first.
/// Groups of one, NULL-hash rows and zero-size files are excluded.
pub fn find_duplicate_groups(db_path: &str, limit: u32) -> Result<Vec<DuplicateGroup>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let conn = db::open_existing(db_path, false)?;
    let mut candidates = scan_candidates(&conn)?;
    // Rowid order is table-page order, so the size lookups below walk the file
    // forwards instead of jumping around it in hash order.
    candidates.sort_unstable();
    let winners = rank_candidates(&conn, &candidates, limit)?;
    hydrate(&conn, winners)
}

/// Every hash shared by more than one row, as `(lowest rowid, member count)`.
///
/// The index-only half: selecting neither `hash` nor `size` is what keeps this
/// a covering scan. Holding the result costs 16 bytes per duplicate group —
/// bounded by the number of *groups*, not by the size of the index.
fn scan_candidates(conn: &Connection) -> Result<Vec<(i64, i64)>, String> {
    let mut stmt = conn.prepare(CANDIDATE_SQL).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, i64>(0)?)))
        .map_err(|e| e.to_string())?;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(row.map_err(|e| e.to_string())?);
    }
    Ok(candidates)
}

/// `WHERE hash IS NOT NULL` is not optional: without it every unhashed row
/// collapses into a single enormous "group".
const CANDIDATE_SQL: &str = "SELECT COUNT(*), MIN(id) FROM files \
                             WHERE hash IS NOT NULL \
                             GROUP BY hash HAVING COUNT(*) > 1";

/// Price each candidate and keep the best `limit`, best first.
///
/// One row read per group, for `size`. Every member of a hash group has the
/// same size — the hash is `sha256(size ‖ head)` (see
/// [`crate::file_handling::get_file_hash`]) — so the lowest rowid speaks for
/// all of them.
fn rank_candidates(
    conn: &Connection,
    candidates: &[(i64, i64)],
    limit: u32,
) -> Result<Vec<Ranked>, String> {
    let mut stmt = conn
        .prepare("SELECT size FROM files WHERE id = ?1")
        .map_err(|e| e.to_string())?;
    // Peeking at the *worst* kept group is what bounds this to `limit` entries
    // however many duplicate groups the index holds.
    let mut best: BinaryHeap<std::cmp::Reverse<Ranked>> = BinaryHeap::new();
    for &(rowid, count) in candidates {
        let size: Option<i64> = stmt
            .query_row(params![rowid], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        // A row deleted between the scan and now, or a zero-size file: every
        // empty file shares one hash, and grouping them is noise.
        let Some(size) = size.filter(|s| *s > 0) else {
            continue;
        };
        // Saturating, not wrapping: a corrupt row must mis-rank one group, not
        // panic the scan in a debug build.
        let redundant = size.saturating_mul(count - 1);
        let ranked = Ranked {
            redundant,
            count,
            size,
            rowid,
        };
        if best.len() < limit as usize {
            best.push(std::cmp::Reverse(ranked));
        } else if let Some(std::cmp::Reverse(worst)) = best.peek() {
            if ranked > *worst {
                best.pop();
                best.push(std::cmp::Reverse(ranked));
            }
        }
    }
    // Ascending in `Reverse<Ranked>` is descending in `Ranked`: best first.
    Ok(best
        .into_sorted_vec()
        .into_iter()
        .map(|std::cmp::Reverse(r)| r)
        .collect())
}

/// Turn the winners into full groups: the hash they were ranked by, then their
/// members. Nothing here is proportional to the size of the index.
fn hydrate(conn: &Connection, winners: Vec<Ranked>) -> Result<Vec<DuplicateGroup>, String> {
    let mut hash_stmt = conn
        .prepare("SELECT hash FROM files WHERE id = ?1")
        .map_err(|e| e.to_string())?;
    let mut member_stmt = conn
        .prepare(
            "SELECT id, name, parent, size, mtime FROM files \
             WHERE hash = ?1 ORDER BY parent, name",
        )
        .map_err(|e| e.to_string())?;
    let mut groups = Vec::with_capacity(winners.len());
    for winner in winners {
        let hash: Option<Vec<u8>> = hash_stmt
            .query_row(params![winner.rowid], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        // The row went away since it was ranked; the group goes with it.
        let Some(hash) = hash else { continue };
        let rows = member_stmt
            .query_map(params![hash], |r| {
                let name: String = r.get(1)?;
                let parent: String = r.get(2)?;
                let path = format!("{}{}", parent, name);
                Ok((
                    r.get::<_, i64>(0)?,
                    name,
                    path,
                    r.get::<_, i64>(3)?.max(0) as u64,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut members = Vec::new();
        for m in rows {
            members.push(m.map_err(|e| e.to_string())?);
        }
        if members.is_empty() {
            continue;
        }
        groups.push(DuplicateGroup {
            hash,
            count: winner.count,
            total_size: winner.size.saturating_mul(winner.count),
            redundant_size: winner.redundant,
            members,
        });
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_recreate;
    use crate::db::repo::{insert_file, NewFile};
    use crate::mime::FileType;

    /// Members of a hash group always share a size, because the hash is
    /// `sha256(size ‖ head)`. Seeds that break that are testing a state the
    /// indexer cannot produce — `malformed_group_does_not_panic` does so on
    /// purpose, and is the only one that may.
    fn seed(dir: &str, rows: &[(&str, u64, Option<&[u8]>)]) -> std::path::PathBuf {
        let p = crate::testutil::scratch_dir(dir).join("index.sqlite");
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let tx = conn.transaction().unwrap();
        for &(name, size, hash) in rows {
            insert_file(
                &tx,
                &NewFile {
                    name,
                    parent: "/d/",
                    size,
                    mtime: 1_700_000_000,
                    mime: None,
                    ftype: FileType::TEXT,
                    hash,
                    needs_content: false,
                },
            )
            .unwrap()
            .expect("unique path");
        }
        tx.commit().unwrap();
        drop(conn);
        p
    }

    fn seed_db() -> std::path::PathBuf {
        seed(
            "dups",
            &[
                // redundant = 10 × 2 = 20.
                ("a1.txt", 10, Some(b"AAA")),
                ("a2.txt", 10, Some(b"AAA")),
                ("a3.txt", 10, Some(b"AAA")),
                // redundant = 100 × 1 = 100 — sorts first despite fewer members.
                ("b1.txt", 100, Some(b"BBB")),
                ("b2.txt", 100, Some(b"BBB")),
                ("c.txt", 30, Some(b"CCC")),
                ("n1.txt", 40, None),
                ("n2.txt", 40, None),
                // Zero-size files are excluded outright.
                ("z1.txt", 0, Some(b"ZZZ")),
                ("z2.txt", 0, Some(b"ZZZ")),
            ],
        )
    }

    #[test]
    fn groups_ordered_by_redundant_size_zero_size_excluded() {
        let p = seed_db();
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(
            groups.len(),
            2,
            "singletons, NULL hashes, and zero-size groups excluded"
        );
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].total_size, 200);
        assert_eq!(groups[0].redundant_size, 100);
        assert_eq!(groups[1].count, 3);
        assert_eq!(groups[1].total_size, 30);
        assert_eq!(groups[1].redundant_size, 20);
        assert_eq!(groups[1].members.len(), 3);
        assert_eq!(groups[1].members[0].1, "a1.txt", "members path-ordered");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn limit_keeps_the_largest_groups() {
        let p = seed_db();
        let all = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        let one = find_duplicate_groups(p.to_str().unwrap(), 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0], all[0], "the truncated page is the *largest* group");
        let over = find_duplicate_groups(p.to_str().unwrap(), 500).unwrap();
        assert_eq!(
            over, all,
            "a limit above the group count returns everything"
        );
        std::fs::remove_file(&p).ok();
    }

    /// The identity a UI remembers a dismissed group by: it must be the hash
    /// itself, spelled the one way, or a group hidden today comes back
    /// tomorrow under a different spelling.
    #[test]
    fn a_group_spells_its_hash_the_one_way() {
        let p = seed_db();
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        for group in &groups {
            let hex = group.hash_hex();
            assert_eq!(hex.len(), group.hash.len() * 2);
            assert!(hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
            assert_eq!(hex, group.hash_hex(), "not stable across calls");
        }
        let again = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        let spelled = |gs: &[DuplicateGroup]| gs.iter().map(|g| g.hash_hex()).collect::<Vec<_>>();
        assert_eq!(
            spelled(&groups),
            spelled(&again),
            "an unchanged index names its groups the same way twice"
        );
        assert_ne!(
            groups[0].hash_hex(),
            groups[1].hash_hex(),
            "two groups must not answer to one name"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn zero_limit_answers_without_opening_the_index() {
        assert!(find_duplicate_groups("/nonexistent/index.sqlite", 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn many_small_copies_outrank_one_large_pair() {
        let mut rows: Vec<(String, u64, &[u8])> = Vec::new();
        for i in 0..20 {
            rows.push((format!("small{i:02}.txt"), 100, b"SSS"));
        }
        rows.push(("big1.bin".to_string(), 1000, b"BBB"));
        rows.push(("big2.bin".to_string(), 1000, b"BBB"));
        let rows: Vec<(&str, u64, Option<&[u8]>)> = rows
            .iter()
            .map(|(n, s, h)| (n.as_str(), *s, Some(*h)))
            .collect();
        let p = seed("dups_many", &rows);
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(groups[0].count, 20);
        assert_eq!(
            groups[0].redundant_size, 1900,
            "ranked on reclaimable bytes, not on file size"
        );
        assert_eq!(groups[1].redundant_size, 1000);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn ties_are_broken_deterministically() {
        let p = seed(
            "dups_ties",
            &[
                ("t1.txt", 50, Some(b"TA")),
                ("t2.txt", 50, Some(b"TA")),
                ("u1.txt", 50, Some(b"TB")),
                ("u2.txt", 50, Some(b"TB")),
                ("v1.txt", 50, Some(b"TC")),
                ("v2.txt", 50, Some(b"TC")),
            ],
        );
        let first = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|g| g.redundant_size == 50), "all tied");
        let again = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(first, again, "an unchanged index lists the same order");
        // And the tie-break must survive truncation: the top 2 of a tied set are
        // the first 2 of the full listing.
        let top2 = find_duplicate_groups(p.to_str().unwrap(), 2).unwrap();
        assert_eq!(
            top2.as_slice(),
            &first[..2],
            "truncation agrees with the full listing"
        );
        std::fs::remove_file(&p).ok();
    }

    /// The indexer cannot produce this — the hash covers the size — but a row
    /// hand-edited into the index must not take the scan down with it.
    #[test]
    fn malformed_group_does_not_panic() {
        let p = seed(
            "dups_malformed",
            &[("m1.txt", 10, Some(b"MMM")), ("m2.txt", 999, Some(b"MMM"))],
        );
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 2);
        assert_eq!(
            groups[0].redundant_size, 10,
            "totals come from the lowest rowid's size"
        );
        assert_eq!(groups[0].members.len(), 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn absurd_sizes_saturate_instead_of_overflowing() {
        let huge = i64::MAX as u64;
        let p = seed(
            "dups_overflow",
            &[
                ("h1.bin", huge, Some(b"HHH")),
                ("h2.bin", huge, Some(b"HHH")),
            ],
        );
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].redundant_size, i64::MAX);
        assert_eq!(groups[0].total_size, i64::MAX, "saturated, not wrapped");
        std::fs::remove_file(&p).ok();
    }

    /// The guard on the whole point of this module. If a later edit makes the
    /// candidate scan read a table row per indexed file again, this fails
    /// rather than merely getting slow.
    #[test]
    fn candidate_scan_stays_inside_the_index() {
        let p = seed_db();
        let conn = db::open_existing(p.to_str().unwrap(), false).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", CANDIDATE_SQL))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            plan.iter()
                .any(|s| s.contains("COVERING INDEX idx_files_hash")),
            "the candidate scan must not touch table rows; plan was {plan:?}"
        );
        drop(conn);
        std::fs::remove_file(&p).ok();
    }
}
