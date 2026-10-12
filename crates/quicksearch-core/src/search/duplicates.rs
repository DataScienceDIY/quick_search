//! Duplicate-file listing, grouped by content hash. Synchronous: callers run
//! it on their own worker thread.
//!
//! # Why three queries and not one aggregate
//!
//! `SELECT hash, COUNT(*), SUM(size) - MAX(size) … GROUP BY hash` reads
//! `size`, which lives only in the table row. `idx_files_hash` covers `hash`
//! alone, so SQLite walks the index in hash order and fetches the wide `files`
//! row behind every entry: one random page per indexed file, each of which
//! SQLCipher must decrypt. At 3M files that is the entire cost of the scan.
//!
//! Naming nothing but `hash` keeps the scan inside the index, and `size` is
//! then read once per candidate group rather than once per row: 3M row fetches
//! down to 60k on a 3M-file index, and 2.75 s down to 0.56 s with the whole
//! database in RAM (the gap widens on cold or encrypted storage). An innocent
//! edit can lose this, so a test reads the query plan back and fails on
//! anything but a covering scan.
//!
//! The index exists only once a run has finished (see
//! [`crate::db::repo::build_hash_index`]); before that the same query sorts
//! the table, slower but with the same answer.
//!
//! The hash covers only each file's size and head, so these are
//! [`HeadGroup`]s, not for display: [`split_by_tail`] reads their tails and
//! returns the [`DuplicateGroup`]s a caller shows.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{self, AtomicBool};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::db;

/// One member of a group: `(file_id, name, path, size, mtime)`.
pub type Member = (i64, String, String, u64, i64);

/// Files sharing a head hash, as the index groups them.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadGroup {
    pub hash: Vec<u8>,
    /// Path-ordered. Price a group from these: the counts it was ranked by
    /// came from an earlier statement, with no transaction holding the two
    /// together.
    pub members: Vec<Member>,
}

/// Files sharing a head hash and a tail digest: what [`split_by_tail`] makes
/// of a [`HeadGroup`], and the only kind of group to show.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// The head hash every member was indexed with.
    pub hash: Vec<u8>,
    /// `sha256` of the bytes each member holds past its head. A file the head
    /// hashed whole holds none, so its digest is of nothing, and such files
    /// group by head alone.
    pub tail: [u8; 32],
    /// Path-ordered, as in [`HeadGroup::members`].
    pub members: Vec<Member>,
}

impl DuplicateGroup {
    /// The group's identity, for a UI that wants to remember one: lowercase
    /// hex of the head hash, then of the tail digest. Stable across renames
    /// and moves, because both cover content and nothing about where the
    /// files live, and distinct for each group one head hash splits into.
    pub fn identity(&self) -> String {
        let mut id = crate::security::hex_encode(&self.hash);
        id.push_str(&crate::security::hex_encode(&self.tail));
        id
    }
}

/// A ranked group, ordered so that greater is better: more reclaimable bytes
/// first, and the lower rowid on a tie so repeated scans of an unchanged index
/// list the same groups in the same order.
#[derive(Debug, PartialEq, Eq)]
struct Ranked {
    redundant: i64,
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

/// The `limit` head-hash groups with the most reclaimable bytes, largest
/// first; see [`split_by_tail`] before showing them. Groups of one, NULL-hash
/// rows and zero-size files are excluded.
///
/// Grouping is by content alone, so hard links and bind-mounted copies of one
/// file are listed as copies, and deleting one frees nothing. That is
/// documented rather than detected: they really are byte-identical, which
/// is all verification claims.
pub fn find_duplicate_groups(db_path: &str, limit: u32) -> Result<Vec<HeadGroup>, String> {
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
/// Selecting neither `hash` nor `size` keeps this a covering scan. The result
/// costs 16 bytes per duplicate group, not per indexed file.
fn scan_candidates(conn: &Connection) -> Result<Vec<(i64, i64)>, String> {
    let mut stmt = conn.prepare(CANDIDATE_SQL).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, i64>(0)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// `WHERE hash IS NOT NULL` is not optional: without it every unhashed row
/// collapses into a single enormous "group".
const CANDIDATE_SQL: &str = "SELECT COUNT(*), MIN(id) FROM files \
                             WHERE hash IS NOT NULL \
                             GROUP BY hash HAVING COUNT(*) > 1";

/// Price each candidate and keep the best `limit`, best first.
///
/// One row read per group, for `size`. Every member of a hash group has the
/// same size, because the hash is `sha256(size ‖ head)` (see
/// [`crate::file_handling::get_file_hash`]), so the lowest rowid speaks for
/// all of them.
fn rank_candidates(
    conn: &Connection,
    candidates: &[(i64, i64)],
    limit: u32,
) -> Result<Vec<Ranked>, String> {
    let mut stmt = conn
        .prepare("SELECT size FROM files WHERE id = ?1")
        .map_err(|e| e.to_string())?;
    // Peeking at the worst kept group bounds this to `limit` entries however
    // many duplicate groups the index holds.
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
        let ranked = Ranked {
            redundant: size.saturating_mul(count - 1),
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
fn hydrate(conn: &Connection, winners: Vec<Ranked>) -> Result<Vec<HeadGroup>, String> {
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
        let members = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        if members.is_empty() {
            continue;
        }
        groups.push(HeadGroup { hash, members });
    }
    Ok(groups)
}

/// Regroup each group's members by their tails, keeping only members whose
/// tail was checked and groups of two or more of them, and rank what is left
/// as [`find_duplicate_groups`] ranks. Meant for the groups a caller is about
/// to show: it reads a file per member.
///
/// # Why tails, and why here
///
/// Indexing hashes each file's size and head
/// ([`crate::file_handling::get_file_hash`]), bytes the walk reads anyway to
/// tell the file's type, so identity costs no extra read. Same-size files
/// that start alike then group as copies: build outputs, disk images,
/// documents from one template. On 86k real files that misreports 3.44% of
/// them at the default 8 KiB, and checking their last 8 KiB as well brings
/// it to 0.10%, both against a 32 KiB head (`examples/hashprobe.rs`). The
/// project once hashed tails during indexing and dropped it for speed: a
/// seek and a read more on every file indexed. Here the read is paid only
/// for the groups on screen, where precision matters.
///
/// # Rules
///
/// - A member no larger than `tail_len` was hashed whole, and is not read:
///   it holds nothing past its head, so its tail digest is of nothing, and
///   such members group by head alone.
/// - Any other is read from `max(tail_len, size - tail_len)` to its end, so
///   the tail never overlaps the head. `tail_len` is meant to be the
///   `hash_length` the index was built with; another value reads more or
///   checks less, and splits nothing that is identical.
/// - A member whose tail goes unchecked is dropped: one whose size is no
///   longer the one indexed (its head hash describes a file that has since
///   changed), one that cannot be opened or read (gone, unreadable, swapped
///   for a FIFO, or a path `never_open` names), and, once `cancel` is
///   raised, every member not yet read. Nothing comes back on its head's
///   word alone: a copy that cannot be checked goes unlisted.
/// - The rest are grouped by tail digest, the head hash being shared, and a
///   group left with fewer than two members is dropped.
///
/// Verify ([`crate::verify`]) remains the byte-exact check: a difference
/// between the head and the tail is invisible to both hashes.
///
/// Members are read one at a time into one reused buffer of at most
/// `tail_len` bytes. A read can block on slow storage, so call this on a
/// worker thread, never a UI thread.
pub fn split_by_tail(
    groups: Vec<HeadGroup>,
    tail_len: usize,
    never_open: impl Fn(&Path) -> bool,
    cancel: &AtomicBool,
) -> Vec<DuplicateGroup> {
    let tail_len = tail_len as u64;
    let mut buf = Vec::new();
    let mut ranked: Vec<(Ranked, DuplicateGroup)> = Vec::with_capacity(groups.len());
    for group in groups {
        let mut by_tail: HashMap<[u8; 32], Vec<Member>> = HashMap::new();
        for member in group.members {
            if cancel.load(atomic::Ordering::Relaxed) {
                break;
            }
            let path = Path::new(&member.2);
            if let Ok(Some(tail)) = tail_digest(path, member.3, tail_len, &never_open, &mut buf) {
                by_tail.entry(tail).or_default().push(member);
            }
        }
        for (tail, members) in by_tail {
            if members.len() < 2 {
                continue;
            }
            let sub = DuplicateGroup {
                hash: group.hash.clone(),
                tail,
                members,
            };
            ranked.push((rank(&sub.members), sub));
        }
    }
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    ranked.into_iter().map(|(_, group)| group).collect()
}

/// `sha256` of the bytes a member holds past its head, or `Ok(None)` when its
/// size has moved since indexing. A file the head hashed whole holds none and
/// is not opened. Reads into `buf`, which is resized to the tail.
fn tail_digest(
    path: &Path,
    size: u64,
    tail_len: u64,
    never_open: &dyn Fn(&Path) -> bool,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<[u8; 32]>> {
    if size <= tail_len {
        return Ok(Some(Sha256::digest(b"").into()));
    }
    if never_open(path) {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    // Non-blocking, so a FIFO renamed over a member is refused rather than
    // waited on; the size comes from the handle, so it is the file read.
    let (mut file, meta) = crate::platform::open_regular_file(path)?;
    if meta.len() != size {
        return Ok(None);
    }
    let start = tail_len.max(size - tail_len);
    file.seek(SeekFrom::Start(start))?;
    buf.clear();
    buf.resize((size - start) as usize, 0);
    file.read_exact(buf)?;
    Ok(Some(Sha256::digest(&buf[..]).into()))
}

/// [`rank_candidates`]'s order for a group in hand: priced by its lowest
/// rowid's size, as the scan priced it.
fn rank(members: &[Member]) -> Ranked {
    let (rowid, size) = members
        .iter()
        .min_by_key(|m| m.0)
        .map_or((0, 0), |m| (m.0, m.3.min(i64::MAX as u64) as i64));
    Ranked {
        redundant: size.saturating_mul(members.len() as i64 - 1),
        rowid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_recreate;
    use crate::db::repo::{insert_file, NewFile};
    use crate::mime::FileType;
    use crate::testutil::Scratch;

    /// Members of a hash group always share a size, because the hash is
    /// `sha256(size ‖ head)`. Only `malformed_group_does_not_panic` seeds a
    /// group that breaks that, on purpose.
    fn seed(dir: &str, rows: &[(&str, u64, Option<&[u8]>)]) -> (Scratch, String) {
        let (scratch, p) = Scratch::db(dir);
        let p = p.to_str().unwrap().to_string();
        let mut conn = open_or_recreate(&p, &Default::default()).unwrap();
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
        // As a finished run's maintenance leaves it.
        crate::db::repo::build_hash_index(&conn).unwrap();
        (scratch, p)
    }

    fn seed_db() -> (Scratch, String) {
        seed(
            "dups",
            &[
                // redundant = 10 × 2 = 20.
                ("a1.txt", 10, Some(b"AAA")),
                ("a2.txt", 10, Some(b"AAA")),
                ("a3.txt", 10, Some(b"AAA")),
                // redundant = 100 × 1 = 100: sorts first despite fewer members.
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

    /// Each group as `(first member's name, member count)`, in listed order.
    fn shape(groups: &[HeadGroup]) -> Vec<(&str, usize)> {
        groups
            .iter()
            .map(|g| (g.members[0].1.as_str(), g.members.len()))
            .collect()
    }

    #[test]
    fn groups_ordered_by_redundant_size_zero_size_excluded() {
        let (_dir, p) = seed_db();
        let groups = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(
            shape(&groups),
            [("b1.txt", 2), ("a1.txt", 3)],
            "singletons, NULL hashes, and zero-size groups excluded; \
             100 reclaimable bytes before 20"
        );
        let names: Vec<&str> = groups[1].members.iter().map(|m| m.1.as_str()).collect();
        assert_eq!(
            names,
            ["a1.txt", "a2.txt", "a3.txt"],
            "members path-ordered"
        );
    }

    /// A fresh index has no hash index until its first run's maintenance
    /// builds one; the listing must not depend on it, only get faster.
    #[test]
    fn groups_are_the_same_before_the_hash_index_is_built() {
        let (_dir, p) = seed_db();
        let indexed = find_duplicate_groups(&p, 10).unwrap();
        let conn = db::open_existing(&p, true).unwrap();
        conn.execute_batch("DROP INDEX idx_files_hash;").unwrap();
        assert_eq!(find_duplicate_groups(&p, 10).unwrap(), indexed);

        crate::db::repo::maintain(&conn, &p).unwrap();
        let rebuilt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'idx_files_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rebuilt, 1, "maintenance builds the missing index");
        assert_eq!(find_duplicate_groups(&p, 10).unwrap(), indexed);
    }

    #[test]
    fn limit_keeps_the_largest_groups() {
        let (_dir, p) = seed_db();
        let all = find_duplicate_groups(&p, 10).unwrap();
        let one = find_duplicate_groups(&p, 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0], all[0], "the truncated page is the *largest* group");
        let over = find_duplicate_groups(&p, 500).unwrap();
        assert_eq!(
            over, all,
            "a limit above the group count returns everything"
        );
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
        let (_dir, p) = seed("dups_many", &rows);
        let groups = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(
            shape(&groups),
            [("small00.txt", 20), ("big1.bin", 2)],
            "ranked on reclaimable bytes (1900 before 1000), not on file size"
        );
    }

    #[test]
    fn ties_are_broken_deterministically() {
        let (_dir, p) = seed(
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
        let first = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(
            shape(&first),
            [("t1.txt", 2), ("u1.txt", 2), ("v1.txt", 2)],
            "all tied on 50 bytes, so the lower rowid lists first"
        );
        let again = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(first, again, "an unchanged index lists the same order");
        // And the tie-break must survive truncation: the top 2 of a tied set are
        // the first 2 of the full listing.
        let top2 = find_duplicate_groups(&p, 2).unwrap();
        assert_eq!(
            top2.as_slice(),
            &first[..2],
            "truncation agrees with the full listing"
        );
    }

    /// The indexer cannot produce this (the hash covers the size), but a row
    /// hand-edited into the index must not take the scan down with it.
    #[test]
    fn malformed_group_does_not_panic() {
        let (_dir, p) = seed(
            "dups_malformed",
            &[
                ("m1.txt", 10, Some(b"MMM")),
                ("m2.txt", 999, Some(b"MMM")),
                ("p1.txt", 50, Some(b"PPP")),
                ("p2.txt", 50, Some(b"PPP")),
            ],
        );
        let groups = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(
            shape(&groups),
            [("p1.txt", 2), ("m1.txt", 2)],
            "priced by the lowest rowid's size, 10, not the 999 beside it"
        );
        let sizes: Vec<u64> = groups[1].members.iter().map(|m| m.3).collect();
        assert_eq!(sizes, [10, 999], "each member reports its own size");
    }

    /// `size × (count - 1)` past `i64::MAX` would panic a debug build.
    #[test]
    fn absurd_sizes_rank_without_panicking() {
        let huge = i64::MAX as u64;
        let (_dir, p) = seed(
            "dups_overflow",
            &[
                ("s1.txt", 10, Some(b"SSS")),
                ("s2.txt", 10, Some(b"SSS")),
                ("h1.bin", huge, Some(b"HHH")),
                ("h2.bin", huge, Some(b"HHH")),
                ("h3.bin", huge, Some(b"HHH")),
            ],
        );
        let groups = find_duplicate_groups(&p, 10).unwrap();
        assert_eq!(
            shape(&groups),
            [("h1.bin", 3), ("s1.txt", 2)],
            "saturated, not wrapped to a negative that sorts last"
        );
    }

    /// Guards the module's reason to exist: a candidate scan that reads a table
    /// row per indexed file fails here rather than merely getting slow.
    #[test]
    fn candidate_scan_stays_inside_the_index() {
        let (_dir, p) = seed_db();
        let conn = db::open_existing(&p, false).unwrap();
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
    }

    // --- split_by_tail ------------------------------------------------------

    /// The head the hand-built groups below were "indexed" with.
    const TAIL: usize = 8;

    /// A group as `find_duplicate_groups` hands one over: one member per body,
    /// written under `dir`, indexed at its length, rowids from `first`.
    fn on_disk(dir: &Path, first: i64, bodies: &[Vec<u8>]) -> HeadGroup {
        let members = bodies
            .iter()
            .zip(first..)
            .map(|(body, id)| {
                let name = format!("f{id}.bin");
                let path = dir.join(&name);
                std::fs::write(&path, body).unwrap();
                let path = path.to_str().unwrap().to_string();
                (id, name, path, body.len() as u64, 1_700_000_000)
            })
            .collect();
        HeadGroup {
            hash: vec![first as u8; 32],
            members,
        }
    }

    /// `len` bytes of one head, ending in `last`.
    fn ending(len: usize, last: u8) -> Vec<u8> {
        let mut body = vec![b'h'; len];
        body[len - 1] = last;
        body
    }

    /// The tail digest a file holding `body` is grouped by.
    fn tail_of(body: &[u8]) -> [u8; 32] {
        let start = TAIL.max(body.len().saturating_sub(TAIL)).min(body.len());
        Sha256::digest(&body[start..]).into()
    }

    /// `group` as `split_by_tail` lists it when every member checks out.
    fn checked(group: &HeadGroup, tail: [u8; 32]) -> DuplicateGroup {
        DuplicateGroup {
            hash: group.hash.clone(),
            tail,
            members: group.members.clone(),
        }
    }

    fn split(groups: &[HeadGroup]) -> Vec<DuplicateGroup> {
        split_by_tail(groups.to_vec(), TAIL, |_| false, &AtomicBool::new(false))
    }

    /// Each group's member rowids, in listed order.
    fn ids(groups: &[DuplicateGroup]) -> Vec<Vec<i64>> {
        groups
            .iter()
            .map(|g| g.members.iter().map(|m| m.0).collect())
            .collect()
    }

    fn path_of(group: &HeadGroup, i: usize) -> &Path {
        Path::new(&group.members[i].2)
    }

    #[test]
    fn a_different_tail_splits_the_group() {
        let dir = Scratch::dir("tails-differ");
        let bodies = [ending(24, b'a'), ending(24, b'a'), ending(24, b'b')];
        let group = on_disk(&dir, 1, &bodies);
        let refined = split(std::slice::from_ref(&group));
        assert_eq!(ids(&refined), [[1, 2]], "the odd one out leaves, alone");
        assert_eq!(refined[0].hash, group.hash, "a part keeps the head hash");
        assert_eq!(refined[0].tail, tail_of(&bodies[0]));
    }

    #[test]
    fn a_matching_tail_keeps_the_group_together() {
        let dir = Scratch::dir("tails-match");
        let body = ending(24, b'a');
        let group = on_disk(&dir, 1, &[body.clone(), body.clone()]);
        assert_eq!(
            split(std::slice::from_ref(&group)),
            [checked(&group, tail_of(&body))]
        );
    }

    /// Two real sets of copies that start alike: a group each, named apart,
    /// so hiding one cannot hide the other.
    #[test]
    fn two_copy_sets_sharing_a_head_are_two_groups() {
        let dir = Scratch::dir("tails-two-sets");
        let (a, b) = (ending(24, b'a'), ending(24, b'b'));
        let group = on_disk(&dir, 1, &[a.clone(), b.clone(), a.clone(), b.clone()]);
        let refined = split(std::slice::from_ref(&group));
        assert_eq!(
            ids(&refined),
            [[1, 3], [2, 4]],
            "tied on waste, so the lower rowid lists first"
        );
        assert_eq!(
            (refined[0].tail, refined[1].tail),
            (tail_of(&a), tail_of(&b))
        );
        assert!(refined.iter().all(|g| g.hash == group.hash));
        assert_ne!(
            refined[0].identity(),
            refined[1].identity(),
            "two groups must not answer to one name"
        );
    }

    /// The identity a UI remembers a dismissed group by is spelled one way,
    /// or a hidden group comes back under a different spelling.
    #[test]
    fn a_group_s_identity_is_its_head_then_its_tail() {
        let dir = Scratch::dir("tails-identity");
        let groups = [
            on_disk(&dir, 1, &vec![ending(24, b'a'); 2]),
            on_disk(&dir, 10, &vec![ending(4, b'b'); 2]),
        ];
        let refined = split(&groups);
        assert_eq!(ids(&refined), [[1, 2], [10, 11]]);
        for group in &refined {
            let id = group.identity();
            assert_eq!(id.len(), (group.hash.len() + 32) * 2);
            assert!(id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        }
        assert!(
            refined[0].identity().starts_with(&"01".repeat(32)),
            "the head hash first: {}",
            refined[0].identity()
        );
        assert_eq!(
            refined[1].identity(),
            format!(
                "{}{}",
                "0a".repeat(32),
                // sha256 of nothing: a file the head hashed whole.
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            )
        );
        let named = |gs: &[DuplicateGroup]| gs.iter().map(|g| g.identity()).collect::<Vec<_>>();
        assert_eq!(
            named(&refined),
            named(&split(&groups)),
            "unchanged files name their groups the same way twice"
        );
    }

    /// The head hashed these whole, so there is nothing left to read, and they
    /// group by head alone. Their files have since grown apart or gone: a read
    /// would see that and drop every member.
    #[test]
    fn files_the_head_hashed_whole_group_by_head_unread() {
        let dir = Scratch::dir("tails-small");
        let mut at_tail = on_disk(&dir, 1, &[ending(20, b'a'), ending(20, b'b')]);
        let mut under = on_disk(&dir, 10, &[ending(20, b'c'), ending(20, b'd')]);
        for m in &mut at_tail.members {
            m.3 = TAIL as u64;
        }
        for m in &mut under.members {
            m.3 = 3;
        }
        std::fs::remove_file(path_of(&under, 1)).unwrap();
        let groups = [at_tail, under];
        let nothing = tail_of(&[]);
        let whole: Vec<_> = groups.iter().map(|g| checked(g, nothing)).collect();
        assert_eq!(split(&groups), whole);
        assert_eq!(
            split_by_tail(groups.to_vec(), TAIL, |_| true, &AtomicBool::new(false)),
            whole,
            "nothing is opened, so never_open has nothing to refuse"
        );
    }

    #[test]
    fn a_member_whose_size_changed_is_dropped() {
        let dir = Scratch::dir("tails-resized");
        let three = on_disk(&dir, 1, &vec![ending(24, b'a'); 3]);
        let pair = on_disk(&dir, 10, &[ending(24, b'b'), ending(24, b'b')]);
        // Grown since indexing; the head hash still matches what it was.
        std::fs::write(path_of(&three, 2), ending(30, b'a')).unwrap();
        std::fs::write(path_of(&pair, 1), ending(23, b'b')).unwrap();
        assert_eq!(
            ids(&split(&[three, pair])),
            [[1, 2]],
            "the changed members leave, and a pair down to one is not listed"
        );
    }

    /// A copy whose tail cannot be read is not listed, and a group it leaves
    /// with one copy is not listed at all.
    #[test]
    fn an_unreadable_member_is_dropped() {
        let dir = Scratch::dir("tails-unreadable");
        let three = on_disk(&dir, 1, &vec![ending(24, b'a'); 3]);
        let pair = on_disk(&dir, 10, &vec![ending(24, b'b'); 2]);
        let groups = [three.clone(), pair.clone()];
        let listed = |never_open: &dyn Fn(&Path) -> bool| {
            ids(&split_by_tail(
                groups.to_vec(),
                TAIL,
                never_open,
                &AtomicBool::new(false),
            ))
        };
        let everything = [vec![1, 2, 3], vec![10, 11]];
        assert_eq!(listed(&|_| false), everything);

        std::fs::remove_file(path_of(&three, 1)).unwrap();
        std::fs::remove_file(path_of(&pair, 0)).unwrap();
        assert_eq!(listed(&|_| false), [[1, 3]], "deleted since indexing");
        std::fs::write(path_of(&three, 1), ending(24, b'a')).unwrap();
        std::fs::write(path_of(&pair, 0), ending(24, b'b')).unwrap();

        let refused = [path_of(&three, 2), path_of(&pair, 1)];
        assert_eq!(
            listed(&|p: &Path| refused.contains(&p)),
            [[1, 2]],
            "a path never_open names"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = [path_of(&three, 0), path_of(&pair, 0)];
            let chmod = |mode| {
                for path in locked {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
                }
            };
            chmod(0o000);
            // Root reads them anyway, and then there is nothing to prove.
            let readable = std::fs::File::open(locked[0]).is_ok();
            let refined = listed(&|_| false);
            chmod(0o644);
            if !readable {
                assert_eq!(refined, [[2, 3]], "permission denied");
            }
        }
        assert_eq!(listed(&|_| false), everything, "readable again");
    }

    #[test]
    fn splitting_re_ranks() {
        let dir = Scratch::dir("tails-rerank");
        // 300 reclaimable bytes as one group, but two pairs of 100 each.
        let four = on_disk(
            &dir,
            1,
            &[
                ending(100, b'a'),
                ending(100, b'a'),
                ending(100, b'b'),
                ending(100, b'b'),
            ],
        );
        let pair = on_disk(&dir, 10, &[ending(150, b'c'), ending(150, b'c')]);
        assert_eq!(
            ids(&split(&[four, pair])),
            [vec![10, 11], vec![1, 2], vec![3, 4]],
            "150 bytes before 100, and the lower rowid first on the tie"
        );
    }

    /// At `2 × TAIL` the head and the tail are the whole file between them;
    /// one byte longer and the byte between them is read by neither.
    #[test]
    fn the_tail_starts_where_the_head_ends() {
        let dir = Scratch::dir("tails-boundary");
        let differing_at = |len: usize, at: usize| {
            let mut other = vec![b'h'; len];
            other[at] = b'x';
            [vec![b'h'; len], other]
        };
        let last_of_head = [on_disk(&dir, 1, &differing_at(2 * TAIL, TAIL - 1))];
        assert_eq!(
            ids(&split(&last_of_head)),
            [[1, 2]],
            "the head's own bytes are not read again"
        );
        let first_of_tail = [on_disk(&dir, 10, &differing_at(2 * TAIL, TAIL))];
        assert!(split(&first_of_tail).is_empty(), "the tail starts at TAIL");
        let middle = [on_disk(&dir, 20, &differing_at(2 * TAIL + 1, TAIL))];
        assert_eq!(ids(&split(&middle)), [[20, 21]], "left for Verify");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_swapped_in_is_dropped_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = Scratch::dir("tails-fifo");
        let group = on_disk(&dir, 1, &vec![ending(24, b'a'); 3]);
        let fifo = path_of(&group, 2);
        std::fs::remove_file(fifo).unwrap();
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = std::sync::mpsc::channel();
        let groups = vec![group.clone()];
        std::thread::spawn(move || tx.send(split(&groups)).unwrap());
        let refined = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the tail check blocked opening a FIFO");
        assert_eq!(ids(&refined), [[1, 2]], "refused, so not listed");
    }

    /// Members not yet read when `cancel` rises are dropped, never listed on
    /// their head's word; what was read before it still counts.
    #[test]
    fn a_raised_cancel_lists_nothing_unchecked() {
        let dir = Scratch::dir("tails-cancel");
        let groups = [
            on_disk(&dir, 1, &vec![ending(24, b'a'); 3]),
            on_disk(&dir, 10, &vec![ending(24, b'b'); 2]),
        ];
        let raised = AtomicBool::new(true);
        assert!(split_by_tail(groups.to_vec(), TAIL, |_| false, &raised).is_empty());

        // Raised as the second member is opened, which is still read.
        let cancel = AtomicBool::new(false);
        let second = path_of(&groups[0], 1);
        let raise_at_second = |p: &Path| {
            if p == second {
                cancel.store(true, atomic::Ordering::Relaxed);
            }
            false
        };
        let refined = split_by_tail(groups.to_vec(), TAIL, raise_at_second, &cancel);
        assert_eq!(ids(&refined), [[1, 2]]);
    }
}
