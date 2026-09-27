//! Indexing-path microbenchmarks.
//!
//! Same convention as `benches/search.rs`: each group pairs two ways of
//! doing the same work, measured together.
//!
//! ```text
//! cargo bench -p quicksearch-core --bench index
//! ```

mod corpus;

use divan::Bencher;
use quicksearch_core::{mime, textenc};

fn main() {
    divan::main();
}

/// Compressing an extracted document: pure CPU, done by the writer as each
/// row is stored. `batch_*` measures a whole walk batch of small bodies.
mod zstd_encode {
    use super::*;

    /// Level 3, matching `db/repo.rs`'s `ZSTD_LEVEL`.
    const LEVEL: i32 = 3;

    #[divan::bench(args = corpus::SIZES)]
    fn encode_all(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 4).as_bytes();
        bencher.bench(|| zstd::encode_all(divan::black_box(text), LEVEL).unwrap());
    }

    #[divan::bench(args = corpus::SIZES)]
    fn bulk_reused(bencher: Bencher, size: usize) {
        let text = corpus::text(size, 4).as_bytes();
        let mut enc = zstd::bulk::Compressor::new(LEVEL).unwrap();
        bencher.bench_local(move || enc.compress(divan::black_box(text)).unwrap());
    }

    /// One walk batch of typical documents. All compression together is
    /// under 1% of a cold run.
    const BATCH: usize = 500;

    #[divan::bench]
    fn batch_encode_all(bencher: Bencher) {
        let text = corpus::text(1 << 10, 4).as_bytes();
        bencher.bench(|| {
            (0..BATCH)
                .map(|_| {
                    zstd::encode_all(divan::black_box(text), LEVEL)
                        .unwrap()
                        .len()
                })
                .sum::<usize>()
        });
    }

    #[divan::bench]
    fn batch_bulk_reused(bencher: Bencher) {
        let text = corpus::text(1 << 10, 4).as_bytes();
        let mut enc = zstd::bulk::Compressor::new(LEVEL).unwrap();
        bencher.bench_local(move || {
            (0..BATCH)
                .map(|_| enc.compress(divan::black_box(text)).unwrap().len())
                .sum::<usize>()
        });
    }
}

/// What a small text file costs between the MIME sniff and the decode:
/// `classify` runs twice on identical bytes, and stays that way. The calls
/// pass different `truncated` flags and disagree at a mid-sequence EOF, and
/// the redundancy is under 1% of a cold run; re-measure here before coupling
/// the layers to share a verdict.
mod text_pipeline {
    use super::*;
    use std::path::Path;

    #[divan::bench]
    fn sniff_then_decode(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| {
            let looks = textenc::looks_like_text(divan::black_box(head));
            let text = textenc::decode_text(head.to_vec(), path, false).unwrap();
            (looks, text.len())
        });
    }

    #[divan::bench]
    fn sniff_only(bencher: Bencher) {
        let head = corpus::text_head();
        bencher.bench(|| textenc::looks_like_text(divan::black_box(head)));
    }

    /// Includes the `head.to_vec()` made only because `decode_text` takes
    /// ownership.
    #[divan::bench]
    fn decode_only(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| textenc::decode_text(divan::black_box(head).to_vec(), path, false));
    }
}

/// The MIME sniff, once per new or changed file: ~180 ns on a text head
/// against ~2.2 µs for the `classify` next to it, so its allocations are not
/// worth removing. This group keeps that ratio visible.
mod mime_sniff {
    use super::*;
    use std::path::Path;

    #[divan::bench]
    fn text_head(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/notes.txt");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }

    #[divan::bench]
    fn binary_head(bencher: Bencher) {
        let head = corpus::binary_head();
        let path = Path::new("/tmp/bench/blob.bin");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }

    /// No extension, so the sniff falls through to magic bytes and the text
    /// classifier.
    #[divan::bench]
    fn no_extension(bencher: Bencher) {
        let head = corpus::text_head();
        let path = Path::new("/tmp/bench/LICENSE");
        bencher.bench(|| mime::guess_mime_from_head(path, divan::black_box(head)));
    }
}

/// `repo::update_file_basic`'s state narrowing: an NA→NA update, the whole
/// of a re-run over a text-free tree, pays one statement instead of four.
/// `clear_always` clears unconditionally, the control that must not move
/// between builds; `narrowed` is the shipped function. NA→NA is idempotent,
/// so iterations are stable without re-seeding. DONE→PENDING is not benched:
/// it clears in both shapes, so there is nothing to regress.
mod update_narrowing {
    use super::*;
    use quicksearch_core::db::repo::{self, NewFile};
    use quicksearch_core::mime::FileType;
    use quicksearch_core::testutil::Scratch;
    use rusqlite::{params, Connection, OptionalExtension};

    const ROWS: usize = 1000;

    fn new_file(name: &str) -> NewFile<'_> {
        NewFile {
            name,
            parent: "/b/",
            size: 7,
            mtime: 7,
            mime: None,
            ftype: FileType::EMPTY,
            hash: None,
            needs_content: false,
        }
    }

    fn seeded_na(tag: &str) -> (Scratch, Connection) {
        let (dir, p) = Scratch::db(tag);
        let mut conn =
            quicksearch_core::db::open_or_recreate(p.to_str().unwrap(), &Default::default())
                .unwrap();
        let tx = conn.transaction().unwrap();
        let names: Vec<String> = (0..ROWS).map(|i| format!("f{:04}.txt", i)).collect();
        for name in &names {
            repo::insert_file(&tx, &new_file(name)).unwrap().unwrap();
        }
        tx.commit().unwrap();
        (dir, conn)
    }

    #[divan::bench]
    fn clear_always(bencher: Bencher) {
        let (_dir, mut conn) = seeded_na("bench-update-old");
        let names: Vec<String> = (0..ROWS).map(|i| format!("f{:04}.txt", i)).collect();
        bencher.bench_local(move || {
            let tx = conn.transaction().unwrap();
            for name in &names {
                let f = new_file(name);
                let id: i64 = tx
                    .prepare_cached(
                        "UPDATE files
                            SET size = ?1, mtime = ?2, hash = ?3, mime = ?4, type = ?5,
                                content_state = ?6
                          WHERE parent = ?7 AND name = ?8
                      RETURNING id",
                    )
                    .unwrap()
                    .query_row(
                        params![
                            f.size as i64,
                            f.mtime as i64,
                            f.hash,
                            f.mime,
                            f.ftype.bits() as i64,
                            repo::STATE_NA,
                            f.parent,
                            f.name,
                        ],
                        |r| r.get(0),
                    )
                    .optional()
                    .unwrap()
                    .unwrap();
                repo::remove_content_for_id(&tx, id).unwrap();
                tx.prepare_cached("DELETE FROM failed_files WHERE file_id = ?1")
                    .unwrap()
                    .execute([id])
                    .unwrap();
            }
            tx.commit().unwrap();
        });
    }

    #[divan::bench]
    fn narrowed(bencher: Bencher) {
        let (_dir, mut conn) = seeded_na("bench-update-new");
        let names: Vec<String> = (0..ROWS).map(|i| format!("f{:04}.txt", i)).collect();
        bencher.bench_local(move || {
            let tx = conn.transaction().unwrap();
            for name in &names {
                repo::update_file_basic(&tx, &new_file(name)).unwrap().unwrap();
            }
            tx.commit().unwrap();
        });
    }
}

/// A watcher event for a directory of indexed files, applied in one
/// transaction as the coordinator applies a slice of its queue.
mod directory_event {
    use super::*;
    use quicksearch_core::config::{Config, IgnoreSet};
    use quicksearch_core::extract::Registry;
    use quicksearch_core::incremental::{apply_fs_event, Budget};
    use quicksearch_core::testutil::Scratch;
    use quicksearch_core::watcher::FsEvent;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    const FILES: usize = 500;

    /// Every file unchanged: a re-sent `Create`, a `chmod -R`, the live view
    /// re-checking what it shows. Nothing is written, so the cost is whatever
    /// it reads to learn that; the full walk's answer is one `stat` per file.
    #[divan::bench]
    fn unchanged(bencher: Bencher) {
        let (dir, db) = Scratch::db("bench-unchanged-event");
        let tree = db.with_file_name("tree");
        std::fs::create_dir_all(&tree).unwrap();
        for i in 0..FILES {
            std::fs::write(
                tree.join(format!("f{:04}.txt", i)),
                corpus::text(1 << 10, 4),
            )
            .unwrap();
        }
        let mut conn =
            quicksearch_core::db::open_or_recreate(db.to_str().unwrap(), &Default::default())
                .unwrap();
        let config = Config::default();
        let ignore = IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap();
        let registry = Registry::default_set();
        let event = FsEvent::Create(tree.clone());
        let cancel = AtomicBool::new(false);
        let mut apply = move || {
            let budget = Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &cancel,
                resume_from: 0,
            };
            // One transaction per event, as the coordinator's apply slice is.
            let tx = conn.transaction().unwrap();
            let applied =
                apply_fs_event(&tx, &event, &config, &ignore, &registry, &budget).unwrap();
            tx.commit().unwrap();
            applied
        };
        // Indexed once; every timed run finds it unchanged.
        apply();
        bencher.bench_local(apply);
        drop(dir);
    }

    /// The same directory with every file's mtime moved on before each run,
    /// so each event rewrites all of its rows and their text: a checkout or
    /// an unpack, where commit and FTS5 segment costs add up per file.
    #[divan::bench]
    fn changed(bencher: Bencher) {
        let (dir, db) = Scratch::db("bench-changed-event");
        let tree = db.with_file_name("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let files: Vec<std::path::PathBuf> =
            (0..FILES).map(|i| tree.join(format!("f{:04}.txt", i))).collect();
        for file in &files {
            std::fs::write(file, corpus::text(1 << 10, 4)).unwrap();
        }
        let mut conn =
            quicksearch_core::db::open_or_recreate(db.to_str().unwrap(), &Default::default())
                .unwrap();
        let config = Config::default();
        let ignore = IgnoreSet::compile(&config.indexing.ignore_patterns).unwrap();
        let registry = Registry::default_set();
        let event = FsEvent::Create(tree.clone());
        let cancel = AtomicBool::new(false);
        let mut apply = move || {
            let budget = Budget {
                deadline: Instant::now() + Duration::from_secs(3600),
                cancel: &cancel,
                resume_from: 0,
            };
            // One transaction per event, as the coordinator's apply slice is.
            let tx = conn.transaction().unwrap();
            let applied =
                apply_fs_event(&tx, &event, &config, &ignore, &registry, &budget).unwrap();
            tx.commit().unwrap();
            applied
        };
        apply();
        let mut tick = 0u64;
        bencher
            .with_inputs(|| {
                tick += 1;
                let at = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + tick);
                for file in &files {
                    let f = std::fs::File::options().write(true).open(file).unwrap();
                    f.set_modified(at).unwrap();
                }
            })
            .bench_local_values(|()| apply());
        drop(dir);
    }
}
