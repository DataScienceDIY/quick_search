use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::extract::Registry;
use crate::mime::guess_mime_from_head;

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// A path that does not exist yet — these tests build the tree themselves.
fn tmp(tag: &str) -> std::path::PathBuf {
    crate::testutil::scratch_dir(tag).join("tree")
}

#[test]
fn count_normal_small_tree() {
    let root = tmp("count");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    for name in ["a.txt", "b.txt", "sub/c.txt"] {
        std::fs::write(root.join(name), b"x").unwrap();
    }
    let cancel = AtomicBool::new(false);
    let n = count_tree_entries_fast(root.to_str().unwrap(), &cancel).unwrap();
    // The subdir plus the three files. Unix counts through `find`, which
    // also lists the root it was given; the Windows directory read only sees
    // entries *inside* a directory. Both are fine for a progress estimate.
    assert_eq!(n, if cfg!(windows) { 4 } else { 5 });
    std::fs::remove_dir_all(&root).ok();
}

/// The Windows fast path must agree with a plain walk exactly. The tree is
/// wider than one 64 KiB buffer of directory data, so the resumption between
/// `GetFileInformationByHandleEx` calls is exercised.
#[cfg(windows)]
#[test]
fn the_bulk_directory_read_agrees_with_a_plain_walk() {
    let root = tmp("count-oracle");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    // Long names so the chained records fill more than one 64 KiB buffer.
    for i in 0..600 {
        let name = format!("{}-{:04}.txt", "padding".repeat(12), i);
        std::fs::write(root.join(&name), b"x").unwrap();
        std::fs::write(root.join("a/b/c").join(&name), b"x").unwrap();
    }

    let cancel = AtomicBool::new(false);
    let path = root.to_str().unwrap();
    let fast = count_tree_entries_win32(path, &cancel).unwrap();
    let plain = count_tree_entries_walkdir(path, &cancel).unwrap();
    assert_eq!(fast, plain, "the fast count must match a plain walk");
    // 1200 files + `empty` + `a` + `a/b` + `a/b/c`.
    assert_eq!(fast, 1204);

    std::fs::remove_dir_all(&root).ok();
}

/// Cancellation is checked per entry, not per directory, so a huge single
/// directory stops as promptly as a deep tree.
#[cfg(windows)]
#[test]
fn the_bulk_directory_read_stops_when_cancelled() {
    let root = tmp("count-cancel");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..200 {
        std::fs::write(root.join(format!("f{:03}.txt", i)), b"x").unwrap();
    }
    let cancel = AtomicBool::new(true);
    let err = count_tree_entries_win32(root.to_str().unwrap(), &cancel).unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn count_cancelled_returns_promptly() {
    // A pre-set token must kill the subprocesses on the first poll —
    // "/" would otherwise take minutes to scan.
    let cancel = AtomicBool::new(true);
    let started = std::time::Instant::now();
    let result = count_tree_entries_fast("/", &cancel);
    let elapsed = started.elapsed();
    assert!(result.is_err(), "cancelled count must not succeed");
    assert!(
        result.unwrap_err().contains("cancelled"),
        "error must be recognizable as cancellation"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "cancellation took {:?}",
        elapsed
    );
    // The token is observational only — nothing resets it.
    assert!(cancel.load(Ordering::Relaxed));
}

/// If these two ever disagree, a file the walk wrote off as NA would
/// silently never be full-text indexed. Fails the moment someone edits one
/// predicate and not the other.
#[test]
fn content_extractable_is_decide_contents_not_applicable() {
    let root = tmp("extractable");
    std::fs::create_dir_all(&root).unwrap();
    let mut cfg = Config::default();
    let registry = Registry::default_set();

    // Real files, because `decide_content` runs the extractor for anything
    // it claims. The last two are identical non-UTF-8 bytes behind a known
    // and an unknown extension, so both sides of the claimed/unclaimed line
    // are exercised.
    let legacy = b"Le caf\xe9 pr\xe8s de la fen\xeatre est agr\xe9able en \xe9t\xe9.";
    let cases: [(&str, &[u8]); 12] = [
        ("notes.txt", b"plain bytes with no magic"),
        ("data.json", b"plain bytes with no magic"),
        ("schema.sql", b"plain bytes with no magic"),
        ("song.mp3", b"plain bytes with no magic"),
        ("photo.jpg", b"plain bytes with no magic"),
        ("movie.mp4", b"plain bytes with no magic"),
        ("archive.zip", b"plain bytes with no magic"),
        ("blob.bin", b"plain bytes with no magic"),
        ("noextension", b"plain bytes with no magic"),
        ("real.bin", b"\x00\x01\x02\x03"),
        ("legacy.txt", legacy),
        ("legacy.unknownext", legacy),
    ];
    for (name, body) in cases {
        let p = root.join(name);
        std::fs::write(&p, body).unwrap();
    }

    // Once with the filter off (the default: everything the registry
    // claims), once with it narrowed to `.txt`.
    for filter in [Vec::new(), vec!["txt".to_string()]] {
        cfg.indexing.content_extensions = filter.clone();
        for (name, body) in cases {
            let p = root.join(name);
            let path = p.to_str().unwrap();
            let mime = guess_mime_from_head(&p, body);
            let claimed = content_extractable(&p, mime.as_deref(), &cfg, &registry);
            let outcome = decide_content(path, mime.as_deref(), &registry, &cfg);
            assert_eq!(
                claimed,
                outcome != ContentOutcome::NotApplicable,
                "{} (mime {:?}, filter {:?}): predicate says {}, decide_content says {:?}",
                name,
                mime,
                filter,
                claimed,
                outcome
            );
        }
    }

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn prepare_file_record_marks_only_claimable_files() {
    let root = tmp("claimable");
    std::fs::create_dir_all(&root).unwrap();
    let registry = Registry::default_set();
    let needs = |cfg: &Config, name: &str| -> bool {
        let p = root.join(name);
        let meta = std::fs::metadata(&p).unwrap();
        prepare_file_record(p.to_str().unwrap(), &meta, cfg, &registry)
            .expect("regular file")
            .needs_content
    };

    for name in ["notes.txt", "song.mp3", "movie.mp4", "README"] {
        std::fs::write(root.join(name), b"body").unwrap();
    }
    // NUL bytes: the text sniff must not rescue a genuinely binary blob.
    std::fs::write(root.join("blob.bin"), b"\x00\x01\x02\x03").unwrap();
    let big = root.join("huge.txt");
    std::fs::write(&big, vec![b'x'; 4096]).unwrap();

    let cfg = Config::default();
    assert!(needs(&cfg, "notes.txt"), "plaintext is claimed");
    assert!(needs(&cfg, "song.mp3"), "audio tags are content too");
    assert!(
        needs(&cfg, "README"),
        "an extensionless text head sniffs as text/plain"
    );
    assert!(!needs(&cfg, "movie.mp4"), "no extractor claims video");
    assert!(
        !needs(&cfg, "blob.bin"),
        "binary content: no MIME, no extractor"
    );

    // Over `maximum_text_file_size`, so the content pass would never read
    // it even though plaintext claims the MIME.
    let mut small_cap = Config::default();
    small_cap.processing.maximum_text_file_size = 1024;
    assert!(!needs(&small_cap, "huge.txt"));
    assert!(needs(&cfg, "huge.txt"), "and claimed under the default cap");

    // The `content_extensions` allowlist excludes it.
    let mut only_md = Config::default();
    only_md.indexing.content_extensions = vec!["md".to_string()];
    assert!(!needs(&only_md, "notes.txt"));

    std::fs::remove_dir_all(&root).ok();
}

/// The headline regression test: the number the manage-index tab shows as
/// the extraction denominator, measured where `indexing.rs` measures it —
/// after the walk's inserts, before any content pass runs.
#[test]
fn extract_scope_counts_only_files_an_extractor_claims() {
    let root = tmp("denominator");
    std::fs::create_dir_all(&root).unwrap();
    let mut db = root.clone();
    db.set_extension("sqlite");

    // 4 files an extractor claims (README via the extensionless text
    // sniff), 5 it never will — the unclaimed set gets NUL bodies so
    // neither the extension tables nor the sniff have anything to say.
    let claimed = ["a.txt", "b.json", "c.mp3", "README"];
    let unclaimed = ["d.mp4", "e.zip", "f.bin", "g.exe", "h"];
    for name in claimed.iter() {
        std::fs::write(root.join(name), b"body bytes, no magic").unwrap();
    }
    for name in unclaimed.iter() {
        std::fs::write(root.join(name), b"\x00\x01body\x00").unwrap();
    }

    let config = Config::default();
    let registry = Registry::default_set();
    let records: Vec<OwnedNewFile> = claimed
        .iter()
        .chain(unclaimed.iter())
        .map(|name| {
            let p = root.join(name);
            let meta = std::fs::metadata(&p).unwrap();
            prepare_file_record(p.to_str().unwrap(), &meta, &config, &registry)
                .expect("regular file")
        })
        .collect();

    let conn_mutex = Arc::new(Mutex::new(
        crate::db::open_or_recreate(db.to_str().unwrap(), "trigram").unwrap(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    process_batch_inserts(&conn_mutex, &records, &stop, &config).unwrap();

    let cursor = ExtractCursor::for_root(root.to_str().unwrap());
    let scope = extract_scope_prepare(&conn_mutex, &cursor, &config).unwrap();

    // `extract_total` in the GUI. The three small text-ish files were
    // finished inline by the walk so they land in `already_done`; the mp3
    // needs the disk pass. Either way the denominator is the claimed set.
    assert_eq!(
        (scope.pending, scope.already_done),
        (1, 3),
        "denominator must be the files needing text, not every indexed file"
    );
    assert_eq!(scope.pending + scope.already_done, claimed.len());

    let total: i64 = conn_mutex
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total as usize, claimed.len() + unclaimed.len());

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_file(&db).ok();
}
