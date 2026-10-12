use super::ignore::FOLD_BUF;
use super::*;

use crate::testutil::Scratch;

fn tmp_dir() -> Scratch {
    Scratch::dir("config")
}

/// A copy of `base` with `f` applied.
fn edited(base: &Config, f: impl FnOnce(&mut Config)) -> Config {
    let mut c = base.clone();
    f(&mut c);
    c
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
    let reloaded = Config::load_from(&path).unwrap();
    assert_eq!(reloaded.paths.indexing_paths, cfg.paths.indexing_paths);
    // A [paths] section that omits indexing_paths also falls back to home.
    fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
    let partial = Config::load_from(&path).unwrap();
    assert_eq!(partial.paths.indexing_paths, cfg.paths.indexing_paths);
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
    assert!((cfg.ui.scale - 1.25).abs() < f32::EPSILON);
    assert_eq!(cfg.ui.search_hotkey, "Ctrl+Shift+F");
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
}

/// A `config.toml` beside the binary is adopted only when it is
/// QuickSearch's: another program's in the same folder (an AppImage's
/// `~/Downloads`, say) is passed over rather than read, and then overwritten
/// by the next save.
#[test]
fn only_a_quicksearch_config_is_taken_as_portable() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    // Every section a save writes is one of ours.
    Config {
        source: Some(path.clone()),
        ..Config::default()
    }
    .save()
    .unwrap();
    assert!(is_quicksearch_config(&path), "a saved config");

    let example =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config_example.toml"))
            .expect("config_example.toml");
    for (text, ours) in [
        ("", true),
        ("# a comment only\n", true),
        (
            "[paths]\nindexing_paths = [\"~\"]\n[ui]\nscale = 1.5\n",
            true,
        ),
        (example.as_str(), true),
        ("[server]\nport = 8080\n", false),
        (
            "[paths]\nindexing_paths = []\n[server]\nport = 8080\n",
            false,
        ),
        ("name = \"other\"\n", false),
    ] {
        fs::write(&path, text).unwrap();
        assert_eq!(is_quicksearch_config(&path), ours, "{text:?}");
    }
    assert!(!is_quicksearch_config(&dir.join("absent.toml")));
    // A directory by the name reads as no config, not ours.
    fs::create_dir(dir.join("folder.toml")).unwrap();
    assert!(!is_quicksearch_config(&dir.join("folder.toml")));
}

/// A portable config the user broke with a typo is still theirs: passing it
/// over would silently swap their settings for the per-user ones. Its load
/// fails instead, so the GUI runs on the inert fallback with the error
/// banner, and the file is left as the user wrote it.
#[test]
fn a_portable_config_that_fails_to_parse_is_still_ours() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    for bytes in [
        &b"[paths\nindexing_paths = [\"/data\"]\n"[..],
        b"[ui]\nscale = 1.5,\n",
        b"[paths]\nindexing_paths = []\n[paths]\n",
        // Not UTF-8, so not even read as text.
        b"\xff\xfe[paths]\n",
    ] {
        let shown = String::from_utf8_lossy(bytes);
        fs::write(&path, bytes).unwrap();
        assert!(is_quicksearch_config(&path), "{shown:?}");
        let err = Config::load_from(&path).expect_err("the load fails");
        assert!(
            err.contains(&path.display().to_string()),
            "{shown:?}: {err}"
        );
        assert_eq!(fs::read(&path).unwrap(), bytes, "{shown:?} is untouched");
    }
}

/// What the GUI runs on when the config fails to load. Never calls `save()`:
/// a regression there would write over the developer's real config.
#[test]
fn fallback_is_inert_and_unsaveable() {
    let cfg = Config::fallback();
    assert_eq!(cfg.source, None);
    assert!(!cfg.can_save());
    let loaded = Config {
        source: Some(PathBuf::from("config.toml")),
        ..Config::default()
    };
    assert!(loaded.can_save());
    assert!(
        !cfg.indexing.auto_index,
        "the fallback must never index on its own"
    );
    assert_eq!(cfg.ui.tutorial_seen, None, "dismissing the tour would save");
    assert!(cfg.paths.indexing_paths.is_empty(), "no made-up $HOME root");
    assert_eq!(
        cfg.paths.database_path,
        Config::default().paths.database_path,
        "search still answers from the index at the default path"
    );
    let err = cfg
        .save_target()
        .expect_err("the fallback has no file to write");
    assert!(
        err.contains(&Config::config_path().display().to_string()),
        "the refusal names the file that did not load: {err}"
    );
}

/// Writes `bytes` as a config file and loads it. The load must fail and leave
/// the directory exactly as it was: the same bytes, no temp file beside them.
fn load_fails_untouched(bytes: &[u8]) -> (String, PathBuf) {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(&path, bytes).unwrap();
    let err = Config::load_from(&path).expect_err("must not load");
    assert_eq!(fs::read(&path).unwrap(), bytes, "the file was modified");
    assert_eq!(
        fs::read_dir(&*dir).unwrap().count(),
        1,
        "something was written"
    );
    (err, path)
}

#[test]
fn malformed_files_are_left_byte_identical() {
    let cases: [&[u8]; 5] = [
        b"[paths\nindexing_paths = [\"/x\"]\n",
        // A string where the field takes a number (`scale = 1` is a valid
        // float and loads).
        b"[ui]\nscale = \"1\"\n",
        b"[paths]\ndatabase_path = \"\xff\xfe.sqlite\"\n",
        b"\xef\xbb\xbf\x00garbage = = [",
        b"[paths]\ndatabase_path = \"\"\"/var/lib/qs\n",
    ];
    for bytes in cases {
        load_fails_untouched(bytes);
    }
}

#[test]
fn tilde_expansion() {
    if std::env::var_os("HOME").is_none() {
        return;
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
    // The sentinel is not itself an extension.
    assert!(!content_allowed(Path::new("/a/x.none"), &cfg));
    assert!(!content_allowed(Path::new("/a/x.(none)"), &cfg));

    for spelling in ["(none)", "(NONE)", "(NonE)", "(nOnE)"] {
        let c = edited(&Config::default(), |c| {
            c.indexing.content_extensions = vec![spelling.to_string()]
        });
        assert!(content_allowed(Path::new("/a/README"), &c), "{spelling}");
    }

    let only_txt = edited(&Config::default(), |c| {
        c.indexing.content_extensions = vec!["txt".into()]
    });
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

    // Nothing but comments = same as an empty list.
    let all_comments = edited(&Config::default(), |c| {
        c.indexing.content_extensions =
            vec!["# nothing enabled yet".into(), "  ".into(), "#".into()]
    });
    assert!(content_allowed(Path::new("/a/b.pdf"), &all_comments));
    assert!(content_allowed(Path::new("/a/Makefile"), &all_comments));
}

/// Comments, spelling and order are not part of the content filter; a real
/// change is never a rebuild, only a re-decision of the text already stored.
#[test]
fn comment_only_edit_is_no_work_at_all() {
    let mut old = Config::default();
    old.indexing.content_extensions = vec!["txt".into(), "md".into()];

    for cosmetic in [
        vec!["# my notes".into(), "txt".into(), "md  # markdown".into()],
        vec!["md".into(), "txt".into()],
        vec![".TXT".into(), ".Md".into()],
    ] {
        let new = edited(&old, |c| c.indexing.content_extensions = cosmetic);
        let a = diff_actions(&old, &new);
        assert_eq!(a, ConfigActions::default(), "cosmetic edit is not a change");
    }

    // Widening: files already indexed by name need a run to extract text.
    let widened = edited(&old, |c| {
        c.indexing.content_extensions = vec!["txt".into(), "md".into(), "(none)".into()]
    });
    let a = diff_actions(&old, &widened);
    assert!(!a.requires_rebuild);
    assert!(a.work.reconcile_content && a.work.reindex);

    // Narrowing: the stored text goes, and nothing needs walking.
    let narrowed = edited(&old, |c| {
        c.indexing.content_extensions = vec!["txt".into(), "# md".into()]
    });
    let a = diff_actions(&old, &narrowed);
    assert!(!a.requires_rebuild);
    assert!(a.work.reconcile_content && !a.work.reindex);
}

/// The size cap decides what gets read exactly as the extension filter does
/// (see `IndexWork::reconcile_content`), so a raised cap must re-decide the
/// files already written off as too large: nothing else re-asks about a file
/// until the file itself changes.
#[test]
fn changing_the_text_file_size_cap_re_decides_the_stored_rows() {
    let old = Config::default();
    let cap = old.processing.maximum_text_file_size;

    let raised = edited(&old, |c| c.processing.maximum_text_file_size = cap * 2);
    let a = diff_actions(&old, &raised);
    assert!(
        !a.requires_rebuild,
        "no rebuild: the rows are all still right"
    );
    assert!(
        a.work.reconcile_content,
        "rows written off as too large must be re-decided"
    );
    assert!(
        a.work.reindex,
        "widening needs a run: the content pass is what reads the newly eligible files"
    );

    let lowered = edited(&old, |c| c.processing.maximum_text_file_size = cap / 2);
    let a = diff_actions(&old, &lowered);
    assert!(
        a.work.reconcile_content,
        "rows now over the cap must give up their text"
    );
    assert!(
        !a.work.reindex,
        "narrowing finds nothing new on disk, so there is nothing to walk for"
    );

    assert_eq!(
        diff_actions(&old, &old.clone()),
        ConfigActions::default(),
        "an unchanged cap is not work"
    );
}

/// An empty list means "everything allowed", a superset of every other list:
/// the case plain set arithmetic reads backwards.
#[test]
fn an_empty_content_filter_is_the_widest_one() {
    let listed = edited(&Config::default(), |c| {
        c.indexing.content_extensions = vec!["txt".into()]
    });
    let unfiltered = edited(&listed, |c| c.indexing.content_extensions = vec![]);

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
    assert!(set.matches_path(Path::new("/repo/.git/config")));
    assert!(set.matches_path(Path::new("/x/y/file.tmp")));
    assert!(set.matches_path(Path::new("/home/bob/secret")));
    // A dir-matching path pattern ignores everything beneath it.
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
    assert!(set.matches_path(Path::new("/tmp")));
    assert!(set.matches_path(Path::new("/tmp/a/b/c.txt")));
    assert!(!set.matches_path(Path::new("/tmpfoo/file.txt")));
    // "cache/" behaves like the component pattern "cache".
    assert!(set.matches_path(Path::new("/home/x/cache/obj.bin")));
    assert!(set.matches_path(Path::new("/repo/sub/target/debug/app")));
    // A bare "/" must not ignore the universe.
    assert!(!set.matches_path(Path::new("/etc/passwd")));
}

/// A drive-root pattern must survive the trailing-separator trim as a path
/// pattern: trimmed to "D:" it would land in the component set.
#[test]
fn drive_root_patterns_are_not_component_patterns() {
    let set = IgnoreSet::compile(&[r"D:\".to_string(), "E:/".to_string()]).unwrap();
    assert!(!set.matches_component("D:"));
    assert!(!set.matches_component(r"D:\"));
    assert!(!set.matches_component("E:"));
}

#[cfg(windows)]
#[test]
fn drive_root_pattern_ignores_the_whole_drive() {
    let set = IgnoreSet::compile(&[r"D:\".to_string()]).unwrap();
    assert!(set.matches_path(Path::new(r"D:\")));
    assert!(set.matches_path(Path::new(r"D:\Users\x\file.txt")));
    assert!(set.matches_path(Path::new(r"d:\case\folded.txt")));
    assert!(!set.matches_path(Path::new(r"E:\file.txt")));
}

/// A bare "D:" compiles but can only match a component literally named "D:";
/// the GUI warns about this shape, the compiler leaves it alone.
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

/// A pattern built from a name matches that name, in either matcher, and
/// nothing its glob syntax would otherwise have reached.
#[test]
fn a_literal_glob_matches_exactly_its_name() {
    let names = [
        "Music [FLAC]",
        "a*b",
        "what?",
        "{x}",
        "{a,b}",
        "[x",
        "x]",
        "[!x]",
        "Café {1}",
        " lead",
        "trail ",
        " ",
        "plain",
    ];
    // A backslash is a name character only off Windows.
    #[cfg(unix)]
    let names = [&names[..], &[r"a\b", r"x\", r"\", r"\*", r"[\]"][..]].concat();
    for name in names {
        let pattern = glob_literal(name);
        let set = IgnoreSet::compile(&[pattern.clone()])
            .unwrap_or_else(|e| panic!("{name:?} as {pattern:?}: {e}"));
        assert!(set.matches_component(name), "{name:?} as {pattern:?}");
        let below = Path::new("/r").join(name).join("f");
        assert!(set.matches_path(&below), "{name:?} as {pattern:?}");
    }

    let reaches = |name: &str, other: &str| {
        IgnoreSet::compile(&[glob_literal(name)])
            .unwrap()
            .matches_component(other)
    };
    assert!(!reaches("Music [FLAC]", "Music F"));
    assert!(!reaches("a*b", "aXb"));
    assert!(!reaches("what?", "whats"));
    assert!(!reaches("{a,b}", "a"));
    assert!(!reaches(" lead", "lead"), "the space was trimmed away");
    #[cfg(unix)]
    {
        assert!(!reaches(r"a\b", "ab"), "the backslash escaped the b");
        assert!(!reaches(r"x\", "x"), "the backslash was trimmed away");
    }
}

#[test]
fn glob_literal_wraps_only_what_a_glob_would_read() {
    for name in ["plain", "node_modules", ".git", "a-b,c!d", "Café"] {
        assert_eq!(glob_literal(name), name);
    }
    assert_eq!(glob_literal("Music [FLAC]"), "Music [[]FLAC[]]");
    assert_eq!(glob_literal(" x "), "[ ]x[ ]");
    #[cfg(unix)]
    assert_eq!(glob_literal(r"a\b"), r"a[\]b");
    #[cfg(windows)]
    assert_eq!(glob_literal(r"C:\a [b]"), r"C:\a [[]b[]]");
}

/// On Unix a backslash is the glob escape, not a separator: a pattern using
/// one is still a component pattern.
#[cfg(unix)]
#[test]
fn an_escaped_unix_pattern_stays_a_component_pattern() {
    let set = IgnoreSet::compile(&[r"\*.tmp".to_string()]).unwrap();
    assert!(set.matches_component("*.tmp"));
    assert!(!set.matches_component("x.tmp"));
    assert!(set.matches_path(Path::new("/r/*.tmp/f")));
}

/// `ignore_patterns` as a TOML array of literal strings.
fn ignore_section(patterns: &[&str]) -> String {
    let quoted: Vec<String> = patterns.iter().map(|p| format!("'{}'", p)).collect();
    format!("[indexing]\nignore_patterns = [{}]\n", quoted.join(", "))
}

/// A bad glob is a load error like a TOML one; left to the indexer, it would
/// fail the app's start instead.
#[test]
fn an_invalid_ignore_glob_fails_the_load() {
    for bad in ["[abc", "{a,b", "[z-a]", "{a,b}}"] {
        let (err, path) = load_fails_untouched(ignore_section(&[".git", bad]).as_bytes());
        assert!(err.contains("Invalid ignore pattern"), "{bad}: {err}");
        assert!(err.contains(&path.display().to_string()), "{bad}: {err}");
    }
}

#[test]
fn an_ignore_glob_past_the_regex_size_limit_fails_the_load() {
    let p = "*a".repeat(500_000);
    assert!(
        IgnoreSet::compile(&[p.clone()]).is_err(),
        "precondition: the pattern is past globset's regex size limit"
    );
    load_fails_untouched(ignore_section(&[&p]).as_bytes());
}

#[test]
fn valid_edge_patterns_still_load() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    // A drive root, one that trims to nothing, and nested alternates, which
    // globset accepts.
    let patterns = ["D:/", " / ", "{a,{b}}"];
    fs::write(&path, ignore_section(&patterns)).unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.indexing.ignore_patterns, patterns, "loaded verbatim");
}

#[test]
fn empty_ignore_set_matches_nothing() {
    let set = IgnoreSet::compile(&[]).unwrap();
    assert!(set.is_empty());
    assert!(!set.matches_path(Path::new("/any/thing")));
    assert!(!set.matches_component("anything"));
}

/// Matching must follow the filesystem's own case rules, or `node_modules`
/// silently fails to exclude `Node_Modules` on Windows.
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

/// Which patterns take the literal fast path, and that taking it changes
/// nothing observable.
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

/// A non-ASCII pattern must not be downgraded to the ASCII fast path.
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

/// Names longer than the stack fold buffer take the heap path.
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

/// Literal patterns must be visible on the whole-path route too, or the
/// watcher indexes exactly what the walker prunes.
#[test]
fn full_path_matching_sees_literal_component_patterns() {
    let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
    assert!(set.matches_path(Path::new("/home/me/proj/node_modules/pkg/index.js")));
    assert!(!set.matches_path(Path::new("/home/me/proj/src/index.js")));
}

/// The walk tests names below its root only, so a root inside an ignored
/// name keeps its files; a full-path pattern still reaches past the root.
#[test]
fn matches_below_tests_names_only_below_the_root() {
    let set = IgnoreSet::compile(&["venv".to_string(), "/srv/archive".to_string()]).unwrap();
    let roots = [PathBuf::from("/home/me/venv/proj")];
    assert!(!set.matches_below(Path::new("/home/me/venv/proj/main.py"), &roots));
    assert!(!set.matches_below(Path::new("/home/me/venv/proj"), &roots));
    assert!(set.matches_below(Path::new("/home/me/venv/proj/venv/lib.py"), &roots));
    // Under no root every name is tested, as `matches_path` tests them.
    assert!(set.matches_below(Path::new("/home/me/venv/other.py"), &roots));

    let roots = [PathBuf::from("/srv/archive/2020")];
    assert!(
        set.matches_below(Path::new("/srv/archive/2020/a.txt"), &roots),
        "a path pattern matching an ancestor of the root still excludes"
    );
}

#[test]
fn matches_below_measures_from_the_innermost_root() {
    let set = IgnoreSet::compile(&["node_modules".to_string()]).unwrap();
    let roots = [
        PathBuf::from("/data"),
        PathBuf::from("/data/node_modules/pkg"),
    ];
    assert!(!set.matches_below(Path::new("/data/node_modules/pkg/index.js"), &roots));
    assert!(set.matches_below(Path::new("/data/node_modules/other/index.js"), &roots));
    // By whole components: `pkg2` is not under `pkg`.
    assert!(set.matches_below(Path::new("/data/node_modules/pkg2/index.js"), &roots));
}

#[test]
fn default_ignore_patterns_cover_the_platform() {
    let d = IndexingConfig::default().ignore_patterns;
    for shared in [".git", "node_modules", "*.tmp", ".venv", "venv"] {
        assert!(d.iter().any(|p| p == shared), "missing {}", shared);
    }

    let recycle = d.iter().any(|p| p == "$RECYCLE.BIN");
    assert_eq!(recycle, cfg!(windows), "Windows-only exclusions");

    let set = IgnoreSet::compile(&d).expect("default patterns compile");
    assert!(!set.is_empty());
    if cfg!(windows) {
        assert!(
            set.matches_component("$RECYCLE.BIN"),
            "`$` must be matched literally, not as a metacharacter"
        );
    }
}

/// [`nested_roots`] over a config with no file, listing `roots`.
fn nested(roots: &[&str]) -> Vec<(String, String)> {
    let mut cfg = Config::default();
    cfg.paths.indexing_paths = roots.iter().map(|r| r.to_string()).collect();
    nested_roots(&cfg)
}

#[test]
fn nested_roots_matrix() {
    assert_eq!(
        nested(&["/qs-x/b", "/qs-x/b/c"]),
        vec![("/qs-x/b/c".to_string(), "/qs-x/b".to_string())]
    );
    // Component boundary: /a/bc is NOT under /a/b.
    assert!(nested(&["/qs-x/b", "/qs-x/bc"]).is_empty());
    assert!(nested(&["/qs-x/b", "/qs-x/c"]).is_empty());
    assert_eq!(nested(&["/qs-x", "/qs-x"]).len(), 1);
    assert!(nested(&[]).is_empty());
    assert!(nested(&["/qs-x"]).is_empty());
    // Symlinked spellings of the same real directory are caught.
    #[cfg(unix)]
    {
        let dir = tmp_dir();
        let real = dir.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pairs = nested(&[real.to_str().unwrap(), link.to_str().unwrap()]);
        assert_eq!(pairs.len(), 1, "alias of the same dir counts as duplicate");
    }
}

/// A child that does not exist yet compares as the existing root it sits
/// under does: canonicalized (`\\?\C:\` on Windows), and through the link
/// its parent is spelled with.
#[test]
fn a_missing_child_of_an_existing_root_is_nested() {
    let dir = tmp_dir();
    let real = dir.join("real");
    fs::create_dir_all(&real).unwrap();
    let parent = real.to_str().unwrap().to_string();
    let child = real.join("later/deeper").to_str().unwrap().to_string();
    assert_eq!(
        nested(&[&parent, &child]),
        vec![(child.clone(), parent.clone())]
    );
    assert!(nested(&[&parent, dir.join("elsewhere").to_str().unwrap()]).is_empty());
    #[cfg(unix)]
    {
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let parent = link.to_str().unwrap().to_string();
        let child = link.join("later").to_str().unwrap().to_string();
        assert_eq!(
            nested(&[&parent, &child]),
            vec![(child.clone(), parent.clone())],
            "both through the link"
        );
        let real_child = real.join("later").to_str().unwrap().to_string();
        assert_eq!(
            nested(&[&parent, &real_child]).len(),
            1,
            "child spelled real"
        );
        let real_parent = real.to_str().unwrap();
        assert_eq!(
            nested(&[real_parent, &child]).len(),
            1,
            "parent spelled real"
        );
    }
    // Missing up to the filesystem root, both: still compared by components.
    assert_eq!(nested(&["/qs-gone/b", "/qs-gone/b/c"]).len(), 1);
    assert!(nested(&["/qs-gone/b", "/qs-gone/bc"]).is_empty());
}

/// A relative root is where the walk looks for it, under the config file's
/// directory, not wherever the process happens to run.
#[test]
fn nested_roots_resolve_relative_roots_against_the_config_dir() {
    let dir = tmp_dir();
    fs::create_dir_all(dir.join("docs/inner")).unwrap();
    let docs = dir.join("docs").to_string_lossy().into_owned();
    let mut cfg = Config::default();
    cfg.source = Some(dir.join("config.toml"));

    cfg.paths.indexing_paths = vec![docs.clone(), "docs".into()];
    assert_eq!(
        nested_roots(&cfg),
        vec![("docs".to_string(), docs.clone())],
        "the same folder spelled relative is a duplicate, reported as spelled"
    );
    cfg.paths.indexing_paths = vec![docs.clone(), "docs/inner".into()];
    assert_eq!(
        nested_roots(&cfg).len(),
        1,
        "relative child of an absolute root"
    );
    cfg.paths.indexing_paths = vec![docs, "elsewhere".into()];
    assert!(nested_roots(&cfg).is_empty());
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
}

/// Links are no longer followed: a config that still sets the old key loads,
/// and the next save drops it.
#[test]
fn removed_follow_symlinks_key_still_parses_and_is_dropped_on_save() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[indexing]\nfollow_symlinks = true\ninclude_hidden = true\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert!(cfg.indexing.include_hidden, "known keys still load");
    cfg.save().unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("follow_symlinks"), "{saved}");
    assert!(saved.contains("include_hidden = true"), "{saved}");
}

/// Trigram is the only tokenizer: a config that still names one, even a word
/// tokenizer, loads, and the next save drops the key.
#[test]
fn removed_tokenize_key_still_parses_and_is_dropped_on_save() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[processing]\ntokenize = \"unicode61\"\nbatch_size = 42\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.processing.batch_size, 42, "known keys still load");
    cfg.save().unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("tokenize"), "{saved}");
    assert!(saved.contains("batch_size = 42"), "{saved}");
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
}

/// Only three settings may wipe the index: the FTS position setting, the hash
/// length and the encryption key. Anything else reaching `requires_rebuild`
/// is a bug.
#[test]
fn only_unreadable_data_forces_a_rebuild() {
    let base = Config::default();

    let positions = edited(&base, |c| c.processing.compact_text_index = false);
    let hash = edited(&base, |c| c.processing.hash_length += 1);
    let protect = edited(&base, |c| c.security.password_protected = true);
    let salt = edited(&base, |c| c.security.salt = Some("00".repeat(16)));

    for c in [&positions, &hash, &protect, &salt] {
        let a = diff_actions(&base, c);
        assert!(a.requires_rebuild, "must wipe");
        assert!(
            a.work.is_empty(),
            "a wipe subsumes reconciliation; leaving work behind would run it \
             against a file that is about to be deleted"
        );
    }

    // The keychain only decides where the key is remembered.
    let keychain = edited(&base, |c| c.security.use_keychain = true);
    assert_eq!(diff_actions(&base, &keychain), ConfigActions::default());
}

/// A base with every narrowable setting at its widest: one ignore pattern,
/// and hidden files on.
fn wide_base() -> Config {
    edited(&Config::default(), |c| {
        c.indexing.ignore_patterns = vec!["node_modules".into()];
        c.indexing.include_hidden = true;
    })
}

/// Removing a root deletes its rows by range; adding one walks. Neither wipes.
#[test]
fn a_removed_root_is_dropped_and_an_added_one_walked() {
    let dir = tmp_dir();
    let kept = dir.join("kept");
    let dropped = dir.join("dropped");
    fs::create_dir_all(&kept).unwrap();
    fs::create_dir_all(&dropped).unwrap();
    let (kept, dropped) = (
        kept.to_string_lossy().into_owned(),
        dropped.to_string_lossy().into_owned(),
    );

    let base = edited(&wide_base(), |c| {
        c.paths.indexing_paths = vec![kept.clone(), dropped.clone()]
    });
    assert_eq!(diff_actions(&base, &base.clone()), ConfigActions::default());

    // Removing a root: rows deleted by range, no walk needed.
    let c = edited(&base, |c| c.paths.indexing_paths = vec![kept.clone()]);
    let a = diff_actions(&base, &c);
    assert!(!a.requires_rebuild);
    assert_eq!(a.work.drop_roots, vec![dropped.clone()]);
    assert!(!a.work.reindex && !a.work.prune_scope);

    // Adding one: nothing stored is wrong, there is just more to find.
    let a = diff_actions(&c, &base);
    assert!(!a.requires_rebuild);
    assert!(a.work.drop_roots.is_empty() && a.work.reindex && !a.work.prune_scope);
}

/// Narrowing the walk prunes stored rows; widening it walks. Neither wipes.
#[test]
fn narrowing_the_walk_prunes_and_widening_it_walks() {
    let base = wide_base();
    for (narrow, widen, what) in [
        (
            edited(&base, |c| c.indexing.ignore_patterns.push("*.log".into())),
            edited(&base, |c| c.indexing.ignore_patterns.clear()),
            "ignore patterns",
        ),
        (
            edited(&base, |c| c.indexing.include_hidden = false),
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
}

/// The position setting is part of the FTS table definition, so a change
/// either way rebuilds.
#[test]
fn the_compact_index_flag_rebuilds_either_way() {
    let base = wide_base();
    let full = edited(&base, |c| c.processing.compact_text_index = false);
    assert!(diff_actions(&base, &full).requires_rebuild);
    assert!(diff_actions(&full, &base).requires_rebuild);
}

#[test]
fn soft_knobs_are_not_index_work() {
    let base = wide_base();
    let moved = edited(&base, |c| {
        c.paths.database_path = "/elsewhere.sqlite".into()
    });
    let a = diff_actions(&base, &moved);
    assert!(a.search_db_changed && !a.requires_rebuild && a.work.is_empty());

    let tuned = edited(&base, |c| {
        c.search.display_limit = 5000;
        c.processing.batch_size = 999;
        c.processing.maximum_wal_size = 0;
        c.indexing.auto_index = false;
        c.indexing.reindex_interval_minutes = 5;
    });
    assert_eq!(
        diff_actions(&base, &tuned),
        ConfigActions::default(),
        "soft knobs are not index work"
    );
}

/// A second edit landing while the first is still being applied must not
/// lose the first's work.
#[test]
fn merging_two_plans_loses_nothing() {
    let first = IndexWork {
        drop_roots: vec!["/gone".into(), "/shared".into()],
        prune_scope: true,
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
    assert!(merged.prune_scope);
    assert!(merged.reconcile_content && merged.reindex);

    // Empty merge is a no-op and self-merge is the identity; both make a
    // restart safe.
    let mut untouched = first.clone();
    untouched.merge_from(&IndexWork::default());
    assert_eq!(untouched, first);
    untouched.merge_from(&first);
    assert_eq!(untouched, first);
}

/// Order and spelling are not configuration.
#[test]
fn respelling_a_list_is_not_a_change() {
    let dir = tmp_dir();
    let a_dir = dir.join("alpha");
    let b_dir = dir.join("beta");
    fs::create_dir_all(&a_dir).unwrap();
    fs::create_dir_all(&b_dir).unwrap();

    let base = edited(&Config::default(), |c| {
        c.paths.indexing_paths = vec![
            a_dir.to_string_lossy().into_owned(),
            b_dir.to_string_lossy().into_owned(),
        ];
        c.indexing.ignore_patterns = vec!["node_modules".into(), "*.tmp".into()];
    });

    let reordered = edited(&base, |c| {
        c.paths.indexing_paths.reverse();
        c.indexing.ignore_patterns.reverse();
    });
    assert_eq!(diff_actions(&base, &reordered), ConfigActions::default());

    // A trailing separator, a `.` hop and a duplicate all name the same roots.
    let respelled = edited(&base, |c| {
        c.paths.indexing_paths = vec![
            format!("{}{}", a_dir.to_string_lossy(), std::path::MAIN_SEPARATOR),
            b_dir.join(".").to_string_lossy().into_owned(),
            a_dir.to_string_lossy().into_owned(),
        ]
    });
    assert_eq!(diff_actions(&base, &respelled), ConfigActions::default());

    // Whitespace around an ignore pattern is trimmed before it compiles.
    let padded = edited(&base, |c| {
        c.indexing.ignore_patterns = vec!["  node_modules ".into(), "*.tmp".into()]
    });
    assert_eq!(diff_actions(&base, &padded), ConfigActions::default());

    // So is a trailing separator, and an empty pattern compiles to nothing:
    // neither may cost a prune and a full reindex.
    let slashed = edited(&base, |c| {
        c.indexing.ignore_patterns = vec!["node_modules/".into(), "*.tmp".into(), " / ".into()]
    });
    assert_eq!(diff_actions(&base, &slashed), ConfigActions::default());
}

#[test]
fn security_config_round_trips_and_salt_is_omitted_when_none() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");

    // Crucially, the file must not contain an invented salt value.
    let cfg = Config::load_from(&path).unwrap();
    assert!(!cfg.security.password_protected);
    assert_eq!(cfg.security.salt, None);
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("salt"), "no default salt may be written");

    let mut cfg = cfg;
    cfg.security.password_protected = true;
    cfg.security.salt = Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string());
    cfg.security.use_keychain = true;
    cfg.save().unwrap();
    let reloaded = Config::load_from(&path).unwrap();
    assert_eq!(reloaded.security, cfg.security);
}

#[test]
fn absent_security_section_is_default() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(&path, "[paths]\ndatabase_path = \"x.sqlite\"\n").unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert_eq!(cfg.security, SecurityConfig::default());
}

#[test]
fn salt_bytes_validates_hostile_configs() {
    let mut sec = SecurityConfig {
        password_protected: true,
        salt: None,
        use_keychain: false,
    };
    assert!(sec.salt_bytes().is_err());

    // Truncated, oversized, non-hex, embedded whitespace/quotes.
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

    sec.salt = Some("0F1E2D3C4B5A69788796A5B4C3D2E1F0".to_string());
    assert!(sec.salt_bytes().is_ok());
}

/// Pure UI bookkeeping must never cost a rebuild or a watcher restart:
/// restarting the watcher would re-trip the very warning being dismissed.
#[test]
fn ui_bookkeeping_fields_are_soft_knobs() {
    let base = Config::default();
    // Named so the closures below coerce to fn pointers and share one array
    // type; without an annotation each would be its own anonymous type.
    type Knob = (&'static str, fn(&mut Config));
    let cases: [Knob; 4] = [
        ("watch_cap_warned_roots", |c| {
            c.ui.watch_cap_warned_roots = vec!["/media/ApolloStore".to_string()]
        }),
        ("color_scheme", |c| c.ui.color_scheme = "light".to_string()),
        // The duplicates filters decide what one tab lists and nothing else:
        // a file left out of that listing is still indexed and still found.
        ("duplicates.exclude_patterns", |c| {
            c.duplicates.exclude_patterns = vec!["*.iso".to_string()]
        }),
        ("duplicates.hidden_groups", |c| {
            c.duplicates.hidden_groups = vec!["ab".repeat(32)]
        }),
    ];
    for (label, mutate) in cases {
        let c = edited(&base, mutate);
        assert_eq!(diff_actions(&base, &c), ConfigActions::default(), "{label}");
    }
}

#[test]
fn newer_fields_round_trip() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");

    let mut cfg = Config {
        source: Some(path.clone()),
        ..Config::default()
    };
    cfg.ui.watch_cap_warned_roots =
        vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()];
    cfg.ui.color_scheme = "light".to_string();
    cfg.search.fuzzy_max_edits = 4;
    cfg.duplicates.exclude_patterns = vec!["*.iso".to_string()];
    cfg.duplicates.hidden_groups = vec!["ab".repeat(32)];
    cfg.save().unwrap();
    let loaded = Config::load_from(&path).unwrap();
    assert_eq!(
        loaded.ui.watch_cap_warned_roots,
        vec!["/media/ApolloStore".to_string(), "/media/GSSD".to_string()]
    );
    assert_eq!(loaded.ui.color_scheme, "light");
    assert_eq!(loaded.search.fuzzy_max_edits, 4);
    assert_eq!(loaded.duplicates, cfg.duplicates);
}

/// A config written before the newer fields existed gets their defaults, and
/// its existing keys still parse.
#[test]
fn newer_fields_default_when_absent() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
         [ui]\nscale=1.25\n[search]\nfuzzy_default=false\ndisplay_limit=250\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).unwrap();
    assert!(cfg.ui.watch_cap_warned_roots.is_empty());
    assert_eq!(
        cfg.duplicates,
        DuplicatesConfig::default(),
        "a config predating the duplicates filters must not arrive with any"
    );
    assert_eq!(cfg.ui.color_scheme, "dark");
    assert_eq!(cfg.search.fuzzy_max_edits, 2);
    assert_eq!(cfg.ui.scale, 1.25, "existing ui keys still parse");
    // `false` is the non-default value, so this only passes if the key parsed.
    assert!(
        !cfg.search.fuzzy_default,
        "existing search keys still parse"
    );
    assert_eq!(cfg.search.display_limit, 250);
}

/// A value nobody recognises is not a broken config file.
#[test]
fn an_unknown_color_scheme_still_loads() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[paths]\nindexing_paths=[\"/x\"]\ndatabase_path=\"db.sqlite\"\n\
         [ui]\ncolor_scheme=\"drak\"\n",
    )
    .unwrap();
    assert_eq!(Config::load_from(&path).unwrap().ui.color_scheme, "drak");
    assert_eq!(Config::default().ui.color_scheme, "dark");
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

/// `config_example.toml` is the documentation for every setting: a key
/// renamed in the struct and not here would silently ship a config file
/// that does nothing.
#[test]
fn the_documented_example_config_parses_to_the_defaults() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config_example.toml")
        .canonicalize()
        .expect("config_example.toml sits at the repository root");
    let text = std::fs::read_to_string(&path).expect("readable");
    let parsed: Config = toml::from_str(&text).expect("config_example.toml parses");

    let d = Config::default();
    assert_eq!(
        parsed.search, d.search,
        "[search] drifted from the defaults"
    );
    assert_eq!(parsed.processing, d.processing);
    assert_eq!(parsed.duplicates, d.duplicates);
    assert_eq!(parsed.ui.scale, d.ui.scale);
    assert_eq!(parsed.ui.color_scheme, d.ui.color_scheme);
    assert_eq!(parsed.ui.tutorial_seen, Some(false));
}

/// The tour is offered to an installation this version created, and no other.
#[test]
fn only_a_freshly_written_config_asks_for_the_tour() {
    assert_eq!(UiConfig::default().tutorial_seen, Some(false));

    let older: Config = toml::from_str("[ui]\nscale = 1.1\n").expect("parses");
    assert_eq!(
        older.ui.tutorial_seen, None,
        "a config predating the tour must not be offered it"
    );

    let dismissed: Config = toml::from_str("[ui]\ntutorial_seen = true\n").expect("parses");
    assert_eq!(dismissed.ui.tutorial_seen, Some(true));
}

#[test]
fn the_search_table_ships_without_size_or_modified() {
    let cols = ColumnsConfig::default();
    assert!(cols.name && cols.content_match && cols.rank);
    assert!(!cols.size, "size must default off");
    assert!(!cols.modified, "modified must default off");

    let older: Config = toml::from_str("[search]\ndisplay_limit = 500\n").expect("parses");
    assert_eq!(older.search.columns, cols);
    assert_eq!(older.search.display_limit, 500);
    assert!(older.search.live_results, "live results default to on");
}

/// The guard consulted before opening a path that came from a `files` row;
/// see [`crate::file_handling::index_file_set`] for what opening one costs.
#[test]
fn the_index_and_every_sidecar_are_recognised() {
    let mut c = Config::default();
    c.paths.database_path = "/var/lib/qs/index.sqlite".to_string();

    for name in [
        "index.sqlite",
        "index.sqlite-wal",
        "index.sqlite-shm",
        "index.sqlite-journal",
        "index.sqlite.lock",
    ] {
        let p = PathBuf::from(format!("/var/lib/qs/{}", name));
        assert!(c.is_index_file(&p), "{} should be recognised", name);
    }

    for p in [
        "/var/lib/qs/index.sqlite-walrus",
        "/var/lib/qs/notes-wal",
        "/var/lib/qs/index.sqlite2",
        "/var/lib/other/index.sqlite-wal",
        "/var/lib/qs/sub/index.sqlite",
    ] {
        assert!(
            !c.is_index_file(Path::new(p)),
            "{} is not part of the index",
            p
        );
    }
}

#[test]
fn a_tilde_database_path_still_matches_the_real_file() {
    let Some(home) = crate::platform::home_dir() else {
        return;
    };
    let mut c = Config::default();
    c.paths.database_path = "~/.local/share/quicksearch/index.sqlite".to_string();

    let absolute = PathBuf::from(home).join(".local/share/quicksearch/index.sqlite");
    assert!(c.is_index_file(&absolute));
    let wal = absolute.with_file_name("index.sqlite-wal");
    assert!(c.is_index_file(&wal));
    assert!(!c.is_index_file(&absolute.with_file_name("other.sqlite")));
}

/// Pushed as it stands, the rest after `~/` is absolute when it starts with
/// another separator, and replaces the home directory. On Unix a backslash
/// is part of a name, not a separator.
#[test]
fn a_doubled_separator_after_the_tilde_stays_under_home() {
    let Some(home) = crate::platform::home_dir() else {
        return;
    };
    let home = PathBuf::from(home);
    assert_eq!(expand_tilde("~//x"), home.join("x"));
    assert_eq!(expand_tilde("~///x/y"), home.join("x").join("y"));
    assert_eq!(expand_tilde("~/x"), home.join("x"));
    assert_eq!(expand_tilde("~/"), home);
    #[cfg(unix)]
    assert_eq!(expand_tilde("~/\\x"), home.join("\\x"));
    #[cfg(windows)]
    assert_eq!(expand_tilde("~\\/\\x"), home.join("x"));
}

/// The home is passed in, so no test touches this process's `HOME`.
#[cfg(unix)]
#[test]
fn a_home_that_is_not_utf8_gives_no_default_root() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let odd = OsString::from_vec(b"/home/\xffme".to_vec());
    assert!(default_roots(Some(odd)).is_empty(), "never a lossy root");
    assert_eq!(default_roots(Some("/home/me".into())), ["/home/me"]);
    assert_eq!(default_roots(None), ["."]);
}

/// The data folder is passed in, so no test touches this process's
/// environment. A non-Unicode one puts the index beside the config file.
#[cfg(unix)]
#[test]
fn a_data_dir_that_is_not_utf8_gives_a_relative_database_path() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let odd = PathBuf::from(OsString::from_vec(
        b"/data/\xffme/quicksearch/index.sqlite".to_vec(),
    ));
    assert_eq!(database_path_setting(odd.clone()), "index.sqlite");
    assert_eq!(database_path_setting(odd), "index.sqlite", "every call");
    assert_eq!(
        database_path_setting(PathBuf::from("/data/me/quicksearch/index.sqlite")),
        "/data/me/quicksearch/index.sqlite"
    );

    let dir = tmp_dir();
    let cfg = Config {
        source: Some(dir.join("config.toml")),
        paths: PathConfig {
            database_path: "index.sqlite".into(),
            ..PathConfig::default()
        },
        ..Config::default()
    };
    assert_eq!(cfg.resolved_database_path(), dir.join("index.sqlite"));
}

/// Zero would panic `chunks`; the top is the settings form's own maximum.
#[test]
fn batch_is_held_between_one_and_the_gui_maximum() {
    let batch = |batch_size| {
        ProcessingConfig {
            batch_size,
            ..ProcessingConfig::default()
        }
        .batch()
    };
    assert_eq!(batch(0), 1);
    assert_eq!(batch(usize::MAX), 100_000);
    assert_eq!(batch(MAX_BATCH_SIZE), MAX_BATCH_SIZE);
    assert_eq!(batch(500), 500);
}

/// `display_limit = 0` would make *every* search return nothing; a
/// hand-editable file must not be able to do that.
#[test]
fn out_of_range_numbers_are_clamped_not_rejected() {
    let dir = tmp_dir();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        "[search]\ndisplay_limit=0\n\
         [processing]\nmaximum_text_file_size=0\nhash_length=8\nwriter_turn_slice_ms=18446744073709551615\n",
    )
    .unwrap();
    let cfg = Config::load_from(&path).expect("a bad number must not stop the app starting");
    assert_eq!(cfg.search.display_limit, 1);
    assert_eq!(cfg.processing.maximum_text_file_size, 1);
    assert_eq!(cfg.processing.hash_length, 262);
    assert_eq!(cfg.processing.writer_turn_slice_ms, 10_000);
}

/// The clamp must be silent about values that are merely unusual, or
/// `hash_length` would read as changed and force a rebuild on every start.
#[test]
fn in_range_numbers_are_left_exactly_alone() {
    let mut cfg = Config::default();
    let before = cfg.clone();
    let warnings = cfg.clamp_out_of_range();
    assert!(warnings.is_empty(), "defaults must not warn: {warnings:?}");
    assert_eq!(cfg, before);

    // Zero is a legal turn slice: one batch per turn.
    cfg.processing.writer_turn_slice_ms = 0;
    assert!(cfg.clamp_out_of_range().is_empty());
    assert_eq!(cfg.processing.writer_turn_slice_ms, 0);
}

#[test]
fn a_clamped_field_is_named_in_its_warning() {
    let mut cfg = Config::default();
    cfg.search.display_limit = 0;
    let warnings = cfg.clamp_out_of_range();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("display_limit"), "{warnings:?}");
    assert!(warnings[0].contains('1'), "{warnings:?}");
}
