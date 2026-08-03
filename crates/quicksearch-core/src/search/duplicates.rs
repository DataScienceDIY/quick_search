//! Duplicate-file listing, grouped by content hash. Backs the GUI's
//! Duplicates tab; standalone and synchronous — callers run it on their
//! own worker thread.

use rusqlite::params;

use crate::db;

#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    pub hash: Vec<u8>,
    pub count: i64,
    pub total_size: i64,
    /// Bytes reclaimable by deduplicating: `size × (count - 1)` — the
    /// group's sort key.
    pub redundant_size: i64,
    /// `(file_id, name, path, size, mtime)` per member, path-ordered.
    pub members: Vec<(i64, String, String, u64, i64)>,
}

/// Page through hash groups having more than one member, ordered by
/// reclaimable bytes (largest first). Rows with a NULL hash (never
/// hashed) and zero-size files (all trivially identical) are excluded.
pub fn find_duplicate_groups(
    db_path: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<DuplicateGroup>, String> {
    let conn = db::open_existing(db_path, false)?;
    let mut groups: Vec<DuplicateGroup> = Vec::new();
    {
        // SUM(size) - MAX(size) == size × (count - 1); members of a group
        // share a size because the hash covers it.
        let mut stmt = conn
            .prepare(
                "SELECT hash, COUNT(*) AS cnt, SUM(size), SUM(size) - MAX(size) AS redundant \
                 FROM files \
                 WHERE hash IS NOT NULL AND size > 0 \
                 GROUP BY hash HAVING cnt > 1 \
                 ORDER BY redundant DESC, hash \
                 LIMIT ?1 OFFSET ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit, offset], |r| {
                Ok(DuplicateGroup {
                    hash: r.get(0)?,
                    count: r.get(1)?,
                    total_size: r.get(2)?,
                    redundant_size: r.get(3)?,
                    members: Vec::new(),
                })
            })
            .map_err(|e| e.to_string())?;
        for g in rows {
            groups.push(g.map_err(|e| e.to_string())?);
        }
    }

    let mut member_stmt = conn
        .prepare(
            "SELECT id, name, path, size, mtime FROM files WHERE hash = ?1 ORDER BY path",
        )
        .map_err(|e| e.to_string())?;
    for group in &mut groups {
        let rows = member_stmt
            .query_map(params![group.hash], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?.max(0) as u64,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for m in rows {
            group.members.push(m.map_err(|e| e.to_string())?);
        }
    }

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_or_recreate;
    use crate::db::repo::{insert_file, NewFile};
    use crate::mime::FileType;

    fn seed_db() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-dups-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut conn = open_or_recreate(p.to_str().unwrap(), "trigram").unwrap();
        let tx = conn.transaction().unwrap();
        let add = |name: &str, path: &str, size: u64, hash: Option<&[u8]>| {
            insert_file(
                &tx,
                &NewFile {
                    name,
                    path,
                    parent: "/d",
                    size,
                    mtime: 1_700_000_000,
                    inode: None,
                    device_id: None,
                    mime: None,
                    ftype: FileType::TEXT,
                    hash,
                    // No MIME, so nothing claims it — and duplicate detection
                    // never looks at content anyway.
                    needs_content: false,
                },
            )
            .unwrap()
            .expect("unique path");
        };
        // Triple group of small files: redundant = 10 × 2 = 20.
        add("a1.txt", "/d/a1.txt", 10, Some(b"AAA"));
        add("a2.txt", "/d/a2.txt", 10, Some(b"AAA"));
        add("a3.txt", "/d/a3.txt", 10, Some(b"AAA"));
        // Pair of large files: redundant = 100 × 1 = 100 — sorts first
        // despite the smaller member count.
        add("b1.txt", "/d/b1.txt", 100, Some(b"BBB"));
        add("b2.txt", "/d/b2.txt", 100, Some(b"BBB"));
        // Singletons and NULL hashes never appear.
        add("c.txt", "/d/c.txt", 30, Some(b"CCC"));
        add("n1.txt", "/d/n1.txt", 40, None);
        add("n2.txt", "/d/n2.txt", 40, None);
        // Zero-size files are trivially identical — excluded outright.
        add("z1.txt", "/d/z1.txt", 0, Some(b"ZZZ"));
        add("z2.txt", "/d/z2.txt", 0, Some(b"ZZZ"));
        tx.commit().unwrap();
        drop(conn);
        p
    }

    #[test]
    fn groups_ordered_by_redundant_size_zero_size_excluded() {
        let p = seed_db();
        let groups = find_duplicate_groups(p.to_str().unwrap(), 10, 0).unwrap();
        assert_eq!(
            groups.len(),
            2,
            "singletons, NULL hashes, and zero-size groups excluded"
        );
        // Reclaimable bytes beat member count for ordering.
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].total_size, 200);
        assert_eq!(groups[0].redundant_size, 100);
        assert_eq!(groups[1].count, 3);
        assert_eq!(groups[1].redundant_size, 20);
        assert_eq!(groups[1].members.len(), 3);
        assert_eq!(groups[1].members[0].1, "a1.txt", "members path-ordered");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pagination() {
        let p = seed_db();
        let page1 = find_duplicate_groups(p.to_str().unwrap(), 1, 0).unwrap();
        let page2 = find_duplicate_groups(p.to_str().unwrap(), 1, 1).unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(page1[0].hash, page2[0].hash);
        let page3 = find_duplicate_groups(p.to_str().unwrap(), 1, 2).unwrap();
        assert!(page3.is_empty());
        std::fs::remove_file(&p).ok();
    }
}
