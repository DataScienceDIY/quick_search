use super::ignore::FOLD_BUF;
use super::*;

fn tmp_dir() -> PathBuf {
    crate::testutil::scratch_dir("config")
}

#[test]
fn fresh_install_defaults_to_home_as_only_root() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    assert!(!path.exists(), "fresh install: no config yet");
    let cfg = Config::load_from(&path).unwrap();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .expect("test environment has a home dir");
    assert_eq!(
        cfg.paths.indexing_paths,
        vec![home],
        "the user's home folder must be the only default index root"
    );
    // The auto-created file round-trips identically.
    let reloaded = Config::load_from(&path).unwrap();
    assert_eq!(reloaded.paths.indexing_paths, cfg.paths.indexing_paths);
    // A [paths] section that omits indexing_paths also falls back to
    // home, not to an empty list.
    fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
    let partial = Config::load_from(&path).unwrap();
    assert_eq!(partial.paths.indexing_paths, cfg.paths.indexing_paths);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_file_created_with_defaults() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    let cfg = Config::load_from(&path).unwrap();
    assert!(path.exists(), "default config file should be written");
    assert_eq!(cfg.search.display_limit, 1000);
    assert_eq!(cfg.search.results_per_page, 100);
    assert!(cfg.indexing.auto_index);
    assert_eq!(cfg.source.as_deref(), Some(path.as_path()));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn partial_file_gets_section_defaults() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.paths.indexing_paths, vec!["/x".to_string()]);
    assert_eq!(cfg.processing.batch_size, 500, "missing sections default");
    assert_eq!(cfg.search.debounce_ms, 150);
    assert!((cfg.ui.scale - 1.1).abs() < f32::EPSILON);
    // A config written before the shortcut existed must come back with
    // one, not with no shortcut at all.
    assert_eq!(cfg.ui.search_hotkey, "Ctrl+Shift+F");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn relative_paths_resolve_against_config_dir() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"data\"]\ndatabase_path=\"index.sqlite\"\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.resolved_database_path(), dir.join("index.sqlite"));
    assert_eq!(cfg.resolved_indexing_paths(), vec![dir.join("data")]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_keeps_relative_paths_portable() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"data\"]\ndatabase_path=\"index.sqlite\"\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    cfg.save().unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("database_path = \"index.sqlite\""),
        "relative path must survive a save round-trip: {}",
        text
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn tilde_expansion() {
    if std::env::var_os("HOME").is_none() {
        return; // nothing to assert without a home dir
    }
    let mut cfg = Config::default();
    cfg.paths.database_path = "~/qs/index.sqlite".to_string();
    let resolved = cfg.resolved_database_path();
    assert!(resolved.is_absolute());
    assert!(!resolved.to_string_lossy().contains('~'));
}

#[test]
fn content_allowed_semantics() {
    let mut cfg = Config::default();
    assert!(content_allowed(Path::new("/a/b.xyz"), &cfg), "empty = all");
    cfg.indexing.content_extensions = vec!["txt".into(), ".MD".into()];
    assert!(content_allowed(Path::new("/a/b.txt"), &cfg));
    assert!(content_allowed(Path::new("/a/B.TXT"), &cfg));
    assert!(
        content_allowed(Path::new("/a/readme.md"), &cfg),
        "leading dot + case in filter"
    );
    assert!(!content_allowed(Path::new("/a/b.pdf"), &cfg));
    assert!(!content_allowed(Path::new("/a/noext"), &cfg));
    assert!(
        !content_allowed(Path::new("/a/.bashrc"), &cfg),
        "dot-only name has no ext"
    );
}

#[test]
fn content_allowed_extensionless_sentinel() {
    let mut cfg = Config::default();
    cfg.indexing.content_extensions = vec!["txt".into(), "  (NonE)  ".into()];
    assert!(content_allowed(Path::new("/a/Makefile"), &cfg));
    assert!(
        content_allowed(Path::new("/a/.bashrc"), &cfg),
        "dot-only name"
    );
    assert!(
        content_allowed(Path::new("/a/b.txt"), &cfg),
        "real extensions still work"
    );
    assert!(
        !content_allowed(Path::new("/a/b.pdf"), &cfg),
        "sentinel is not a wildcard"
    );
    // The sentinel is not itself an extension: a file literally named
    // `x.none` is not whitelisted by it.
    assert!(!content_allowed(Path::new("/a/x.none"), &cfg));
    assert!(!content_allowed(Path::new("/a/x.(none)"), &cfg));

    // Every capitalisation of the word means the same thing.
    for spelling in ["(none)", "(NONE)", "(NonE)", "(nOnE)"] {
        let mut c = Config::default();
        c.indexing.content_extensions = vec![spelling.to_string()];
        assert!(content_allowed(Path::new("/a/README"), &c), "{spelling}");
    }

    // A leading dot is stripped for extensions but must not turn some
    // other entry into the sentinel.
    let mut only_txt = Config::default();
    only_txt.indexing.content_extensions = vec!["txt".into()];
    assert!(!content_allowed(Path::new("/a/Makefile"), &only_txt));
}

#[test]
fn content_allowed_comments() {
    let mut cfg = Config::default();
    cfg.indexing.content_extensions = vec![
        "# source files only".into(),
        "rs # rust".into(),
        "  .MD\t# docs  ".into(),
        "   # indented whole-line comment".into(),
        "(none) # Makefile, LICENSE, ...".into(),
    ];
    assert!(content_allowed(Path::new("/a/b.rs"), &cfg));
    assert!(
        content_allowed(Path::new("/a/b.md"), &cfg),
        "dot + trailing comment"
    );
    assert!(
        content_allowed(Path::new("/a/Makefile"), &cfg),
        "sentinel + comment"
    );
    assert!(!content_allowed(Path::new("/a/b.pdf"), &cfg));
    // Comment text is not itself a filter entry.
    assert!(!content_allowed(Path::new("/a/b.rust"), &cfg));
    assert!(!content_allowed(Path::new("/a/b.only"), &cfg));
    assert!(!content_allowed(Path::new("/a/b.docs"), &cfg));

    // Nothing but comments filters nothing — same as an empty list.
    let mut all_comments = Config::default();
    all_comments.indexing.content_extensions =
        vec!["# nothing enabled yet".into(), "  ".into(), "#".into()];
    assert!(content_allowed(Path::new("/a/b.pdf"), &all_comments));
    assert!(content_allowed(Path::new("/a/Makefile"), &all_comments));
}

/// Comments, spelling and order are not part of the content filter, so
/// editing them is no work at all — and a real change to it is never a
/// rebuild, only a re-decision of the text already stored.
#[test]
fn comment_only_edit_is_no_work_at_all() {
    let mut old = Config::default();
    old.indexing.content_extensions = vec!["txt".into(), "md".into()];

    for cosmetic in [
        vec!["# my notes".into(), "txt".into(), "md  # markdown".into()],
        vec!["md".into(), "txt".into()],
        vec![".TXT".into(), ".Md".into()],
    ] {
        let mut new = old.clone();
        new.indexing.content_extensions = cosmetic;
        let a = diff_actions(&old, &new);
        assert_eq!(a, ConfigActions::default(), "cosmetic edit is not a change");
    }

    // Adding an extension widens the filter: files already indexed by
    // name need their text extracted, which takes a run.
    let mut widened = old.clone();
    widened.indexing.content_extensions = vec!["txt".into(), "md".into(), "(none)".into()];
    let a = diff_actions(&old, &widened);
    assert!(!a.requires_rebuild);
    assert!(a.work.reconcile_content && a.work.reindex);

    // Commenting one out narrows it: the stored text goes, and nothing
    // needs walking to make that true.
    let mut narrowed = old.clone();
    narrowed.indexing.content_extensions = vec!["txt".into(), "# md".into()];
    let a = diff_actions(&old, &narrowed);
    assert!(!a.requires_rebuild);
    assert!(a.work.reconcile_content && !a.work.reindex);
}

/// An empty list means "everything allowed", so it is a superset of every
/// other list — the case plain set arithmetic reads backwards.
#[test]
fn an_empty_content_filter_is_the_widest_one() {
    let mut listed = Config::default();
    listed.indexing.content_extensions = vec!["txt".into()];
    let mut unfiltered = listed.clone();
    unfiltered.indexing.content_extensions = vec![];

    let widening = diff_actions(&listed, &unfiltered).work;
    assert!(widening.reconcile_content && widening.reindex);

    let narrowing = diff_actions(&unfiltered, &listed).work;
    assert!(narrowing.reconcile_content && !narrowing.reindex);
}

#[test]
fn ignore_set_component_vs_path() {
    let set = IgnoreSet::compile(&[
        ".git".to_string(),
        "*.tmp".to_string(),
        "/home/*/secret".to_string(),
        "".to_string(), // blank lines ignored
    ])
    .unwrap();
    assert!(set.matches_component(".git"));
    assert!(set.matches_component("junk.tmp"));
    assert!(!set.matches_component("git"));
    // Full-path checks catch both kinds.
    assert!(set.matches_path(Path::new("/repo/.git/config")));
    assert!(set.matches_path(Path::new("/x/y/file.tmp")));
    assert!(set.matches_path(Path::new("/home/bob/secret")));
    // A dir-matching path pattern ignores everything beneath it, same
    // as the walker pruning that directory.
    assert!(set.matches_path(Path::new("/home/bob/secret/inner/deep.txt")));
    assert!(!set.matches_path(Path::new("/home/bob/public")));
    assert!(!set.matches_path(Path::new("/repo/src/main.rs")));
}

#[test]
fn directory_patterns_with_trailing_slash() {
    let set = IgnoreSet::compile(&[
        "/tmp/".to_string(),     // absolute dir, natural spelling
        "cache/".to_string(),    // becomes a component pattern
        "*/target/".to_string(), // dir anywhere by suffix
        "/".to_string(),         // degenerate: trims to nothing, skipped
    ])
    .unwrap();
    // The directory itself and everything beneath it.
    assert!(set.matches_path(Path::new("/tmp")));
    assert!(set.matches_path(Path::new("/tmp/a/b/c.txt")));
    assert!(!set.matches_path(Path::new("/tmpfoo/file.txt")));
    // "cache/" behaves like the component pattern "cache".
    assert!(set.matches_path(Path::new("/home/x/cache/obj.bin")));
    // Suffix form matches the dir at any depth.
    assert!(set.matches_path(Path::new("/repo/sub/target/debug/app")));
    // A bare "/" must not ignore the universe.
    assert!(!set.matches_path(Path::new("/etc/passwd")));
}

/// A drive-root pattern must survive the trailing-separator trim as a
/// path pattern — trimmed to "D:" it would land in the component set,
/// where nothing is ever named "D:".
#[test]
fn drive_root_patterns_are_not_component_patterns() {
    let set = IgnoreSet::compile(&[r"D:\".to_string(), "E:/".to_string()]).unwrap();
    assert!(!set.matches_component("D:"));
    assert!(!set.matches_component(r"D:\"));
    assert!(!set.matches_component("E:"));
}

/// The full drive-root behavior needs Windows path semantics:
/// `Path::parent` only walks up to `D:\` there, and globset only folds
/// `\` to `/` where `\` is a separator.
#[cfg(windows)]
#[test]
fn drive_root_pattern_ignores_the_whole_drive() {
    let set = IgnoreSet::compile(&[r"D:\".to_string()]).unwrap();
    assert!(set.matches_path(Path::new(r"D:\")));
    assert!(set.matches_path(Path::new(r"D:\Users\x\file.txt")));
    assert!(set.matches_path(Path::new(r"d:\case\folded.txt")));
    assert!(!set.matches_path(Path::new(r"E:\file.txt")));
}

/// A bare "D:" (no separator) compiles but can only match a component
/// literally named "D:", which no file ever is. The GUI warns about
/// this shape; the compiler intentionally leaves it alone.
#[test]
fn bare_drive_letter_stays_a_component_pattern() {
    let set = IgnoreSet::compile(&["D:".to_string()]).unwrap();
    assert!(set.matches_component("D:"));
    #[cfg(windows)]
    assert!(!set.matches_path(Path::new(r"D:\file.txt")));
}

#[test]
fn ignore_set_invalid_pattern_errors() {
    let err = IgnoreSet::compile(&["[".to_string()]).unwrap_err();
    assert!(err.contains("invalid ignore pattern"), "{}", err);
}

#[test]
fn empty_ignore_set_matches_nothing() {
    let set = IgnoreSet::compile(&[]).unwrap();
    assert!(set.is_empty());
    assert!(!set.matches_path(Path::new("/any/thing")));
    assert!(!set.matches_component("anything"));
}

/// Pattern matching must follow the filesystem's own case rules, or
/// `node_modules` silently fails to exclude `Node_Modules` on Windows.
#[test]
fn ignore_matching_follows_platform_case_rules() {
    let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
    assert!(
        set.matches_component("node_modules"),
        "exact always matches"
    );

    let folded = cfg!(any(windows, target_os = "macos"));
    assert_eq!(
        set.matches_component("Node_Modules"),
        folded,
        "case folding must track the platform's filesystem semantics"
    );
}

/// Which patterns take the fast path, and that taking it changes nothing
/// observable. A plain name is matched whole — never as a prefix, a
/// substring or a wildcard — and only its case is allowed to vary.
#[test]
fn the_literal_fast_path_matches_whole_names_only() {
    let literal = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
    assert!(
        !literal.literal_components.is_empty(),
        "a plain name belongs on the fast path"
    );
    for globby in ["node_module?", "node_*", "*.tmp", "a[bc]d", "x{1,2}"] {
        assert!(
            IgnoreSet::compile(&[globby.to_string()])
                .unwrap()
                .literal_components
                .is_empty(),
            "{} has glob syntax and must stay with globset",
            globby
        );
    }

    assert!(literal.matches_component("node_modules"));
    for cased in ["Node_Modules", "NODE_MODULES", "node_moduleS"] {
        assert_eq!(
            literal.matches_component(cased),
            cfg!(any(windows, target_os = "macos")),
            "only case may vary, and only where the filesystem says so: {}",
            cased
        );
    }
    for name in [
        "node_modules_",
        "_node_modules",
        "nodemodules",
        "node_module",
        "src",
        "",
    ] {
        assert!(
            !literal.matches_component(name),
            "{} is not the ignored name",
            name
        );
    }
}

/// A non-ASCII pattern keeps globset's Unicode folding rather than being
/// silently downgraded to the ASCII fast path.
#[test]
fn non_ascii_patterns_stay_on_the_glob_path() {
    let set = IgnoreSet::compile(&["café".to_string()]).unwrap();
    assert!(
        set.literal_components.is_empty(),
        "a non-ASCII name must not join the ASCII-folded set"
    );
    assert!(set.matches_component("café"));
    assert!(!set.matches_component("cafe"));
}

/// Names longer than the stack fold buffer take the heap path, and must
/// come back with the same answer.
#[test]
fn overlong_names_still_fold_correctly() {
    let long = "a".repeat(FOLD_BUF + 10);
    let set = IgnoreSet::compile(std::slice::from_ref(&long)).unwrap();
    assert!(set.matches_component(&long));
    assert_eq!(
        set.matches_component(&long.to_uppercase()),
        cfg!(any(windows, target_os = "macos"))
    );
    assert!(!set.matches_component(&"a".repeat(FOLD_BUF + 9)));
}

/// Watcher events are matched by whole path, and the literal patterns have
/// to be visible on that route too — otherwise the watcher indexes exactly
/// what the walker prunes and the index churns every cycle.
#[test]
fn full_path_matching_sees_literal_component_patterns() {
    let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
    assert!(set.matches_path(Path::new("/home/me/proj/node_modules/pkg/index.js")));
    assert!(!set.matches_path(Path::new("/home/me/proj/src/index.js")));
}

#[test]
fn default_ignore_patterns_cover_the_platform() {
    let d = IndexingConfig::default().ignore_patterns;
    for shared in [".git", "node_modules", "*.tmp", ".venv", "venv"] {
        assert!(d.iter().any(|p| p == shared), "missing {}", shared);
    }

    // `$RECYCLE.BIN` holds deleted files; indexing it would surface their
    // contents in search results.
    let recycle = d.iter().any(|p| p == "$RECYCLE.BIN");
    assert_eq!(recycle, cfg!(windows), "Windows-only exclusions");

    // Whatever the platform, the defaults must actually compile — a
    // pattern like `$RECYCLE.BIN` going through globset is the risk.
    let set = IgnoreSet::compile(&d).expect("default patterns compile");
    assert!(!set.is_empty());
    if cfg!(windows) {
        assert!(
            set.matches_component("$RECYCLE.BIN"),
            "`$` must be matched literally, not as a metacharacter"
        );
    }
}

#[test]
fn nested_roots_matrix() {
    // Straight nesting (paths don't exist → compared as spelled).
    assert_eq!(
        nested_roots(&["/qs-x/b".into(), "/qs-x/b/c".into()]),
        vec![("/qs-x/b/c".to_string(), "/qs-x/b".to_string())]
    );
    // Component boundary: /a/bc is NOT under /a/b.
    assert!(nested_roots(&["/qs-x/b".into(), "/qs-x/bc".into()]).is_empty());
    // Disjoint roots.
    assert!(nested_roots(&["/qs-x/b".into(), "/qs-x/c".into()]).is_empty());
    // Exact duplicates flag once.
    assert_eq!(nested_roots(&["/qs-x".into(), "/qs-x".into()]).len(), 1);
    // Empty and singleton lists are fine.
    assert!(nested_roots(&[]).is_empty());
    assert!(nested_roots(&["/qs-x".into()]).is_empty());
    // Symlinked spellings of the same real directory are caught via
    // canonicalization.
    #[cfg(unix)]
    {
        let dir = tmp_dir();
        let real = dir.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pairs = nested_roots(&[
            real.to_string_lossy().into_owned(),
            link.to_string_lossy().into_owned(),
        ]);
        assert_eq!(pairs.len(), 1, "alias of the same dir counts as duplicate");
        fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn removed_precount_key_still_parses() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[processing]\nprecount_files_for_progress = true\nbatch_size = 42\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.processing.batch_size, 42, "known keys still load");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn root_workers_round_trip() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    let mut cfg = Config {
        source: Some(path.clone()),
        ..Config::default()
    };
    cfg.paths.indexing_paths = vec!["/data".into(), "/share".into()];
    cfg.indexing.root_workers.insert("/share".into(), 24);
    cfg.save().unwrap();
    let loaded = Config::load_from(&path).unwrap();
    assert_eq!(loaded.indexing.root_workers.get("/share"), Some(&24));
    assert_eq!(
        loaded.indexing.root_workers.get("/data"),
        None,
        "absent = auto"
    );
    fs::remove_dir_all(&dir).ok();
}

/// Only three settings may wipe the index: the FTS tokenizer, the hash
/// length and the encryption key. Anything else that reaches
/// `requires_rebuild` is a bug — it costs the user everything the index
/// took hours to learn.
#[test]
fn only_unreadable_data_forces_a_rebuild() {
    let base = Config::default();

    let mut tokenizer = base.clone();
    tokenizer.processing.tokenize = "unicode61".into();
    let mut hash = base.clone();
    hash.processing.hash_length = base.processing.hash_length + 1;
    let mut protect = base.clone();
    protect.security.password_protected = true;
    let mut salt = base.clone();
    salt.security.salt = Some("00".repeat(16));

    for c in [&tokenizer, &hash, &protect, &salt] {
        let a = diff_actions(&base, c);
        assert!(a.requires_rebuild, "must wipe");
        assert!(
            a.work.is_empty(),
            "a wipe subsumes reconciliation; leaving work behind would run it \
             against a file that is about to be deleted"
        );
    }

    // The keychain only decides where the key is remembered, not what the
    // file was written with.
    let mut keychain = base.clone();
    keychain.security.use_keychain = true;
    assert_eq!(diff_actions(&base, &keychain), ConfigActions::default());
}

/// Narrowing deletes; widening walks. Nothing here may wipe.
#[test]
fn diff_actions_matrix() {
    let dir = tmp_dir();
    let kept = dir.join("kept");
    let dropped = dir.join("dropped");
    fs::create_dir_all(&kept).unwrap();
    fs::create_dir_all(&dropped).unwrap();
    let (kept, dropped) = (
        kept.to_string_lossy().into_owned(),
        dropped.to_string_lossy().into_owned(),
    );

    let mut base = Config::default();
    base.paths.indexing_paths = vec![kept.clone(), dropped.clone()];
    base.indexing.ignore_patterns = vec!["node_modules".into()];
    base.indexing.include_hidden = true;
    base.indexing.follow_symlinks = true;

    assert_eq!(diff_actions(&base, &base.clone()), ConfigActions::default());

    // Removing a root: its rows are deleted by range, and no walk is
    // needed to establish that they should go.
    let mut c = base.clone();
    c.paths.indexing_paths = vec![kept.clone()];
    let a = diff_actions(&base, &c);
    assert!(!a.requires_rebuild);
    assert_eq!(a.work.drop_roots, vec![dropped.clone()]);
    assert!(!a.work.reindex && !a.work.prune_scope);

    // Adding one: nothing stored is wrong, there is just more to find.
    let a = diff_actions(&c, &base);
    assert!(!a.requires_rebuild);
    assert!(a.work.drop_roots.is_empty() && a.work.reindex && !a.work.prune_scope);

    for (narrow, widen, what) in [
        (
            {
                let mut c = base.clone();
                c.indexing.ignore_patterns.push("*.log".into());
                c
            },
            {
                let mut c = base.clone();
                c.indexing.ignore_patterns.clear();
                c
            },
            "ignore patterns",
        ),
        (
            {
                let mut c = base.clone();
                c.indexing.include_hidden = false;
                c
            },
            base.clone(),
            "hidden files",
        ),
    ] {
        let a = diff_actions(&base, &narrow);
        assert!(!a.requires_rebuild, "{} must not wipe", what);
        assert!(
            a.work.prune_scope && !a.work.reindex,
            "narrowing {} prunes and needs no walk",
            what
        );
        let a = diff_actions(&narrow, &widen);
        assert!(!a.requires_rebuild, "{} must not wipe", what);
        assert!(
            a.work.reindex && !a.work.prune_scope,
            "widening {} walks and deletes nothing",
            what
        );
    }

    // Symlinks take their own route: with links on, a target inside a root
    // is stored under exactly the path a direct walk would produce, so
    // nothing in scope changes. What turning them off strands is the rows
    // *outside* every root, which no per-root scan would ever revisit.
    let mut no_links = base.clone();
    no_links.indexing.follow_symlinks = false;
    let a = diff_actions(&base, &no_links);
    assert!(!a.requires_rebuild);
    assert!(
        a.work.drop_aliases && !a.work.prune_scope && !a.work.reindex,
        "turning links off sweeps outside the roots and nothing else"
    );
    let a = diff_actions(&no_links, &base);
    assert!(
        a.work.reindex && !a.work.drop_aliases && !a.work.prune_scope,
        "turning links on only adds"
    );

    // Stored text: turning it on means re-extracting, turning it off means
    // throwing the blobs away — never a rebuild either way.
    let mut off = base.clone();
    off.processing.store_text_for_snippets = false;
    let mut on = base.clone();
    on.processing.store_text_for_snippets = true;
    let a = diff_actions(&on, &off);
    assert!(a.work.drop_text && !a.work.restore_text && !a.work.reindex);
    let a = diff_actions(&off, &on);
    assert!(a.work.restore_text && !a.work.drop_text && a.work.reindex);

    let mut c = base.clone();
    c.paths.database_path = "/elsewhere.sqlite".into();
    let a = diff_actions(&base, &c);
    assert!(a.search_db_changed && !a.requires_rebuild && a.work.is_empty());

    let mut c = base.clone();
    c.search.display_limit = 5000;
    c.processing.batch_size = 999;
    c.processing.maximum_wal_size = 0;
    c.indexing.auto_index = false;
    c.indexing.reindex_interval_minutes = 5;
    assert_eq!(
        diff_actions(&base, &c),
        ConfigActions::default(),
        "soft knobs are not index work"
    );

    fs::remove_dir_all(&dir).ok();
}

/// A second edit landing while the first is still being applied must not
/// lose the first's work: the two plans are computed against different
/// configurations, so neither knows what the other left undone.
#[test]
fn merging_two_plans_loses_nothing() {
    let first = IndexWork {
        drop_roots: vec!["/gone".into(), "/shared".into()],
        prune_scope: true,
        drop_text: true,
        ..IndexWork::default()
    };
    let second = IndexWork {
        drop_roots: vec!["/shared".into(), "/also-gone".into()],
        reconcile_content: true,
        reindex: true,
        ..IndexWork::default()
    };

    let mut merged = second.clone();
    merged.merge_from(&first);
    assert_eq!(
        merged.drop_roots,
        vec!["/shared", "/also-gone", "/gone"],
        "every root from both, each once"
    );
    assert!(merged.prune_scope && merged.drop_text);
    assert!(merged.reconcile_content && merged.reindex);

    // Merging an empty plan changes nothing, and merging a plan into
    // itself is the identity — both are what make a restart safe.
    let mut untouched = first.clone();
    untouched.merge_from(&IndexWork::default());
    assert_eq!(untouched, first);
    untouched.merge_from(&first);
    assert_eq!(untouched, first);
}

/// Order and spelling are not configuration: a reordered list or a
/// re-spelled root must not cost the index anything.
#[test]
fn respelling_a_list_is_not_a_change() {
    let dir = tmp_dir();
    let a_dir = dir.join("alpha");
    let b_dir = dir.join("beta");
    fs::create_dir_all(&a_dir).unwrap();
    fs::create_dir_all(&b_dir).unwrap();

    let mut base = Config::default();
    base.paths.indexing_paths = vec![
        a_dir.to_string_lossy().into_owned(),
        b_dir.to_string_lossy().into_owned(),
    ];
    base.indexing.ignore_patterns = vec!["node_modules".into(), "*.tmp".into()];

    let mut reordered = base.clone();
    reordered.paths.indexing_paths.reverse();
    reordered.indexing.ignore_patterns.reverse();
    assert_eq!(diff_actions(&base, &reordered), ConfigActions::default());

    // A trailing separator, a `.` hop and a duplicate entry all name the
    // same two roots.
    let mut respelled = base.clone();
    respelled.paths.indexing_paths = vec![
        format!("{}{}", a_dir.to_string_lossy(), std::path::MAIN_SEPARATOR),
        b_dir.join(".").to_string_lossy().into_owned(),
        a_dir.to_string_lossy().into_owned(),
    ];
    assert_eq!(diff_actions(&base, &respelled), ConfigActions::default());

    // Whitespace around an ignore pattern is trimmed before it compiles,
    // so it cannot be a change either.
    let mut padded = base.clone();
    padded.indexing.ignore_patterns = vec!["  node_modules ".into(), "*.tmp".into()];
    assert_eq!(diff_actions(&base, &padded), ConfigActions::default());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn security_config_round_trips_and_salt_is_omitted_when_none() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");

    // Defaults: protection off, no salt — and crucially the file must
    // not contain an invented salt value.
    let cfg = Config::load_from(&path).unwrap();
    assert!(!cfg.security.password_protected);
    assert_eq!(cfg.security.salt, None);
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("salt"), "no default salt may be written");

    // With a salt set, it round-trips exactly.
    let mut cfg = cfg;
    cfg.security.password_protected = true;
    cfg.security.salt = Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string());
    cfg.security.use_keychain = true;
    cfg.save().unwrap();
    let reloaded = Config::load_from(&path).unwrap();
    assert_eq!(reloaded.security, cfg.security);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn absent_security_section_is_default() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.security, SecurityConfig::default());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn salt_bytes_validates_hostile_configs() {
    // Protected but no salt: hard error, nothing invented.
    let mut sec = SecurityConfig {
        password_protected: true,
        salt: None,
        use_keychain: false,
    };
    assert!(sec.salt_bytes().is_err());

    // Hand-crafted hostile values: truncated, oversized, non-hex,
    // embedded whitespace/quotes. All rejected.
    for bad in [
        "",
        "abcd",
        &"ab".repeat(17),
        &"ab".repeat(4096),
        "0g1e2d3c4b5a69788796a5b4c3d2e1f0",
        "0f1e2d3c4b5a6978 796a5b4c3d2e1f0",
        "0f1e2d3c4b5a69788796a5b4c3d2e1f'",
    ] {
        sec.salt = Some(bad.to_string());
        assert!(sec.salt_bytes().is_err(), "must reject salt {:?}", bad);
    }

    // A valid salt decodes, upper- or lowercase.
    sec.salt = Some("0F1E2D3C4B5A69788796A5B4C3D2E1F0".to_string());
    assert!(sec.salt_bytes().is_ok());
}

/// Dismissing the watch-cap warning must not trigger a rebuild or a
/// watcher restart — it is pure UI bookkeeping, and restarting the
/// watcher would re-trip the very warning being dismissed.
#[test]
fn watch_cap_warned_roots_is_a_soft_knob() {
    let base = Config::default();
    let mut c = base.clone();
    c.ui.watch_cap_warned_roots = vec!["/media/ApolloStore".to_string()];
    assert_eq!(diff_actions(&base, &c), ConfigActions::default());
}

#[test]
fn watch_cap_warned_roots_round_trips() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    let mut cfg = Config {
        source: Some(path.clone()),
        ..Config::default()
    };
    cfg.ui.watch_cap_warned_roots =
        vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()];
    cfg.save().unwrap();

    let loaded = Config::load_from(&path).unwrap();
    assert_eq!(
        loaded.ui.watch_cap_warned_roots,
        vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()]
    );
    fs::remove_dir_all(&dir).ok();
}

/// Configs written before this field existed must still load.
#[test]
fn config_without_watch_cap_warned_roots_parses() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n[ui]\nscale=1.25\n",
    )
    .unwrap();

    let cfg = Config::load_from(&path).unwrap();
    assert!(cfg.ui.watch_cap_warned_roots.is_empty());
    assert_eq!(cfg.ui.scale, 1.25, "existing ui keys still parse");
    fs::remove_dir_all(&dir).ok();
}

/// Which theme the window uses is nobody's business but the window's: it
/// must never cost a reindex or a watcher restart.
#[test]
fn color_scheme_is_a_soft_knob() {
    let base = Config::default();
    let mut c = base.clone();
    c.ui.color_scheme = "light".to_string();
    assert_eq!(diff_actions(&base, &c), ConfigActions::default());
}

#[test]
fn color_scheme_round_trips_and_defaults_to_dark() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    assert_eq!(Config::default().ui.color_scheme, "dark");

    let mut cfg = Config {
        source: Some(path.clone()),
        ..Config::default()
    };
    cfg.ui.color_scheme = "light".to_string();
    cfg.save().unwrap();
    assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "light");

    // A config written before the setting existed keeps the appearance it
    // had, which was dark.
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n[ui]\nscale=1.25\n",
    )
    .unwrap();
    assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "dark");

    // A value nobody recognises is not a broken config file: the whole
    // point of storing it as a string is that the app still starts.
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
         [ui]\ncolor_scheme=\"drak\"\n",
    )
    .unwrap();
    assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "drak");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fuzzy_max_edits_round_trips() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    let mut cfg = Config {
        source: Some(path.clone()),
        ..Config::default()
    };
    cfg.search.fuzzy_max_edits = 4;
    cfg.save().unwrap();

    let loaded = Config::load_from(&path).unwrap();
    assert_eq!(loaded.search.fuzzy_max_edits, 4);
    fs::remove_dir_all(&dir).ok();
}

/// Configs written before this field existed keep the historic budget.
#[test]
fn config_without_fuzzy_max_edits_defaults_to_two() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
         [search]\nfuzzy_default=true\ndisplay_limit=250\n",
    )
    .unwrap();

    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.search.fuzzy_max_edits, 2);
    assert!(cfg.search.fuzzy_default, "existing search keys still parse");
    assert_eq!(cfg.search.display_limit, 250);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn fuzzy_edits_warning_only_above_the_threshold() {
    let mut cfg = SearchConfig::default();
    for quiet in 0..=FUZZY_EDITS_WARN_ABOVE {
        cfg.fuzzy_max_edits = quiet;
        assert!(
            cfg.fuzzy_edits_warning().is_none(),
            "{} should be quiet",
            quiet
        );
    }
    for loud in [FUZZY_EDITS_WARN_ABOVE + 1, 8, usize::MAX] {
        cfg.fuzzy_max_edits = loud;
        let msg = cfg
            .fuzzy_edits_warning()
            .expect("warns above the threshold");
        assert!(msg.contains(&loud.to_string()));
        assert!(msg.contains(&FUZZY_EDITS_WARN_ABOVE.to_string()));
    }
}
