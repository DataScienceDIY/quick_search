//! Terminal query mode: `quicksearch [FLAGS] <query terms...>` runs the
//! same ranked cascade the GUI uses and prints results to stdout. With no
//! arguments the binary opens the GUI instead.
//!
//! Notes and errors go out through `writeln!` with the result dropped, never
//! `eprintln!`, which panics when stderr is a closed pipe (`2>&1 | head -n1`):
//! a note nobody reads must not turn a finished search into exit 101.

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;

use quicksearch_core::config::{Config, SecurityConfig};
use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::security::{derive_key, IndexKey};
use quicksearch_core::textenc::{one_line, scrub_controls};
use zeroize::Zeroizing;

use crate::format::{fmt_mtime, human_size};

/// Scripting escape hatch. Caveat: other processes of the same user can read
/// the environment, and exported variables end up in shell history.
const PASSWORD_ENV: &str = "QUICKSEARCH_PASSWORD";

pub(crate) const USAGE: &str = "\
QuickSearch: indexed file search

USAGE:
    quicksearch                          open the GUI
    quicksearch --toggle                 focus the running GUI, or start it
    quicksearch [FLAGS] <query terms>    search from the terminal
                                         (Windows: quicksearch-cli)

FLAGS:
    --toggle        bring the running GUI forward with the search box
                    focused, or start it. Bind a key to this in your
                    desktop's keyboard settings for a system-wide shortcut.
    --fuzzy         also run the fuzzy filename and full-text passes
    --limit <N>     maximum results, a positive number
                    (default: [search].display_limit)
    --long          rank, size, mtime and snippets instead of bare paths
    --              every later argument is a query term, even one that
                    starts with '-'
    -h, --help      this help
    -V, --version   version and the commit it was built from

Flags come before the query: from the first term on, every argument is
part of it.

Query syntax matches the GUI: plain words form one phrase, and filters such
as type:Document, modified:>=2024-01-01, path:/dir, mime:application/pdf and
name:frag combine with it.

A password-protected index unlocks from, in order: the OS keychain (when
'Remember on this device' is on in the GUI), the QUICKSEARCH_PASSWORD
environment variable, or a prompt. Other processes of the same user can
read environment variables.";

/// Runs terminal mode on `args`, the arguments after the program name:
/// `Some(exit_code)`, or `None` for an empty argv (open the GUI).
/// Terminal mode never builds an `IndexCoordinator`: a one-shot query must
/// not compete for the per-user inotify budget with a running GUI.
pub fn maybe_run_cli(args: &[OsString]) -> Option<i32> {
    // Write errors are ignored: there is no one left to tell.
    Some(match parse(args_from(args))? {
        Ok(cli) => run_query(&cli),
        Err((0, text)) => {
            let _ = writeln!(io::stdout(), "{}", text);
            0
        }
        Err((code, text)) => {
            let _ = writeln!(io::stderr(), "{}", text);
            code
        }
    })
}

/// `args` decoded lossily, never refused: a name that is not Unicode is never
/// indexed, so a term holding U+FFFD in its place matching nothing is the
/// right answer.
fn args_from(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// A terminal query, as [`parse`] read it.
#[derive(Debug, PartialEq)]
struct Cli {
    query: String,
    fuzzy: bool,
    limit: Option<NonZeroUsize>,
    long: bool,
}

/// `None` only for an empty argv; `Err((0, text))` for --help and
/// --version; `Err((2, message))` for a usage error, with the usage
/// appended. Flags lead: before the first term any other dash-argument is
/// an error, `--` makes every later argument a term, and from the first
/// term on dash-words are terms too.
fn parse(args: Vec<String>) -> Option<Result<Cli, (i32, String)>> {
    if args.is_empty() {
        return None;
    }
    let usage = |message: &str| Some(Err((2, format!("{}\n\n{}", message, USAGE))));
    let mut fuzzy = false;
    let mut long = false;
    let mut limit = None;

    let mut it = args.into_iter();
    let terms: Vec<String> = loop {
        let Some(arg) = it.next() else {
            break Vec::new();
        };
        match arg.as_str() {
            "--" => break it.collect(),
            "-h" | "--help" => return Some(Err((0, USAGE.to_string()))),
            "-V" | "--version" => {
                return Some(Err((
                    0,
                    format!("QuickSearch {}", crate::version::BUILD_ID),
                )));
            }
            "--fuzzy" => fuzzy = true,
            "--long" => long = true,
            other if other == "--limit" || other.starts_with("--limit=") => {
                let value = match other.strip_prefix("--limit=") {
                    Some(v) => Some(v.to_string()),
                    None => it.next(),
                };
                // `NonZeroUsize` refuses 0 as well as anything past `usize::MAX`.
                let Some(n) = value.and_then(|v| v.parse().ok()) else {
                    return usage("--limit requires a positive number");
                };
                limit = Some(n);
            }
            other if other.starts_with('-') => {
                return usage(&format!(
                    "unknown option '{}'; put -- before a term that starts with '-'",
                    other
                ));
            }
            _ => break std::iter::once(arg).chain(it).collect(),
        }
    };

    if terms.is_empty() {
        return usage("nothing to search for");
    }
    Some(Ok(Cli {
        query: terms.join(" "),
        fuzzy,
        limit,
        long,
    }))
}

/// Unlocks a protected index: keychain → `QUICKSEARCH_PASSWORD` (an empty
/// one counts as unset) → interactive prompt (three attempts) → an
/// instructive error. `keychain` is the remembered entry, whose key is tried
/// only if it was remembered under the config's salt. Password buffers are
/// zeroized on drop and consumed immediately by the KDF; nothing here
/// retains or logs them.
pub(crate) fn resolve_key(
    security: &SecurityConfig,
    is_tty: bool,
    env_password: Option<Zeroizing<String>>,
    keychain: Option<Zeroizing<String>>,
    mut prompt: impl FnMut() -> Option<Zeroizing<String>>,
    mut try_key: impl FnMut(IndexKey) -> Result<(), String>,
) -> Result<(), String> {
    if !security.password_protected {
        return Ok(());
    }
    let salt = security.salt_bytes()?;

    if let Some(text) = keychain {
        match crate::keychain::remembered_key(&text, &salt) {
            Ok(key) => match attempt(&mut try_key, key) {
                Ok(()) => return Ok(()),
                Err(Rejection::WrongPassword) => {
                    let _ = writeln!(
                        io::stderr(),
                        "warning: the key remembered in the OS keychain no longer opens this index"
                    );
                }
                // Not a stale entry: say which, then fall through.
                Err(Rejection::Mismatch(detail)) => {
                    let _ = writeln!(io::stderr(), "warning: {}", detail);
                }
                Err(Rejection::Fatal(e)) => return Err(e),
            },
            Err(e) => {
                let _ = writeln!(io::stderr(), "warning: keychain entry: {}", e);
            }
        }
    }

    if let Some(password) = env_password.filter(|p| !p.is_empty()) {
        let key = derive_key(&password, &salt);
        drop(password);
        return attempt(&mut try_key, key).map_err(|rejection| match rejection {
            Rejection::WrongPassword => {
                format!("{} does not match this index's password", PASSWORD_ENV)
            }
            // Don't blame the env var for a cause that is not about it.
            Rejection::Mismatch(detail) => detail,
            Rejection::Fatal(e) => e,
        });
    }

    if !is_tty {
        return Err(format!(
            "the index is password-protected; run from a terminal, set {}, \
             or enable 'Remember on this device' in the GUI",
            PASSWORD_ENV
        ));
    }
    for _ in 0..3 {
        let Some(password) = prompt() else {
            return Err("failed to read password".to_string());
        };
        let key = derive_key(&password, &salt);
        drop(password);
        match attempt(&mut try_key, key) {
            Ok(()) => return Ok(()),
            // Only a wrong password is worth another attempt; the other
            // causes are facts about the index, and retrying makes the user
            // type a *correct* password twice more.
            Err(Rejection::WrongPassword) => {
                let _ = writeln!(io::stderr(), "Wrong password.");
            }
            Err(Rejection::Mismatch(e) | Rejection::Fatal(e)) => return Err(e),
        }
    }
    Err("wrong password (3 attempts)".to_string())
}

/// Why [`attempt`] turned a key down.
enum Rejection {
    /// Wrong for this index: another password may be worth a try.
    WrongPassword,
    /// A key mismatch not about the password; says which.
    Mismatch(String),
    /// Not a key mismatch at all (the database vanished, say).
    Fatal(String),
}

/// Tries one key, sorting a failure by what it means for the next source.
fn attempt(
    try_key: &mut impl FnMut(IndexKey) -> Result<(), String>,
    key: IndexKey,
) -> Result<(), Rejection> {
    try_key(key).map_err(|e| match db::key_mismatch_parts(&e) {
        Some((db::KeyMismatch::WrongPassword, _)) => Rejection::WrongPassword,
        Some((_, detail)) => Rejection::Mismatch(detail.to_string()),
        None => Rejection::Fatal(e),
    })
}

/// [`resolve_key`] wired to the real terminal, environment and keychain.
fn resolve_key_for_terminal(security: &SecurityConfig, db_path: &str) -> Result<(), String> {
    let keychain = if security.use_keychain {
        crate::keychain::load_entry(db_path).unwrap_or_else(|e| {
            let _ = writeln!(io::stderr(), "warning: {}", e);
            None
        })
    } else {
        None
    };
    resolve_key(
        security,
        std::io::stdin().is_terminal(),
        std::env::var(PASSWORD_ENV).ok().map(Zeroizing::new),
        keychain,
        || {
            rpassword::prompt_password("Index password: ")
                .ok()
                .map(Zeroizing::new)
        },
        |key| {
            db::set_process_key(Some(key));
            db::verify_process_key(db_path)
        },
    )
}

fn run_query(cli: &Cli) -> i32 {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io::stderr(), "config: {}", e);
            return 2;
        }
    };
    let db_path = config.resolved_database_path();
    if config.security.password_protected {
        if !db_path.exists() {
            let _ = writeln!(
                io::stderr(),
                "No usable index at {}; run the GUI once to build it.",
                db_path.display()
            );
            return 2;
        }
        if let Err(e) = resolve_key_for_terminal(&config.security, &db_path.to_string_lossy()) {
            let _ = writeln!(io::stderr(), "{}", e);
            return 2;
        }
    }
    // Read-write purely so SQLite may create the WAL shared-memory file;
    // nothing is written.
    let conn = match db::open_existing(&db_path.to_string_lossy(), true) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "No usable index at {}; run the GUI once to build it.\n({})",
                db_path.display(),
                e
            );
            return 2;
        }
    };

    let split = match split_for_cascade(&cli.query) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io::stderr(), "query: {}", e);
            return 2;
        }
    };

    if cli.fuzzy {
        if let Some(warning) = config.search.fuzzy_edits_warning() {
            let _ = writeln!(io::stderr(), "warning: {}", warning);
        }
    }
    let options = SearchOptions {
        fuzzy: cli.fuzzy,
        fuzzy_max_edits: config.search.fuzzy_max_edits,
        limit: cli
            .limit
            .map_or(config.search.display_limit, NonZeroUsize::get),
        batch: config.search.results_per_page.max(1),
        session_ignores: Vec::new(),
    };
    let latest = AtomicU64::new(1);
    let mut hits: Vec<SearchHit> = Vec::new();
    let outcome = cascade::run(&conn, &split, &options, 1, &latest, &mut |batch| {
        hits.extend(batch)
    });

    match outcome {
        Ok(Some(outcome)) => {
            let tty = io::stdout().is_terminal();
            let printed = print_hits(&mut io::stdout().lock(), &hits, cli.long, tty);
            let code = exit_code(&printed);
            if printed.is_ok() && outcome.limited {
                let _ = writeln!(
                    io::stderr(),
                    "(truncated at {} results; raise with --limit)",
                    hits.len()
                );
            }
            code
        }
        Ok(None) => 0, // unreachable: nothing cancels a CLI search
        Err(e) => {
            let _ = writeln!(io::stderr(), "search: {}", e);
            2
        }
    }
}

/// Writes one path per hit, or with `long` a detail line and a snippet.
/// Control characters are scrubbed only for a terminal (the way `ls` does
/// it): raw escape sequences in a filename can rewrite the line or reach the
/// clipboard via OSC 52. Piped output stays byte-exact.
fn print_hits(out: &mut impl Write, hits: &[SearchHit], long: bool, tty: bool) -> io::Result<()> {
    let color = long && tty && enable_vt();
    let show = |s: &str| -> String {
        if tty {
            scrub_controls(s).into_owned()
        } else {
            s.to_string()
        }
    };
    for hit in hits {
        if long {
            writeln!(
                out,
                "{:6.3}  {:>9}  {}  {}",
                hit.rank,
                human_size(hit.size),
                fmt_mtime(hit.mtime),
                show(&hit.path)
            )?;
            if let Some(snip) = &hit.snippet {
                writeln!(out, "        {}", render_snippet(snip, color, tty))?;
            }
        } else {
            writeln!(out, "{}", show(&hit.path))?;
        }
    }
    out.flush()
}

/// What a print ends in. A reader that stops early (`| head`) closed the
/// pipe because it has what it wanted, which is success, not an error.
fn exit_code(printed: &io::Result<()>) -> i32 {
    match printed {
        Ok(()) => 0,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            let _ = writeln!(io::stderr(), "output: {}", e);
            2
        }
    }
}

/// Whether ANSI escapes will render. On Windows the console only interprets
/// them once `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is set; anything that
/// refuses gets plain text rather than a screen of `\x1b[1m`.
#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

#[cfg(windows)]
fn enable_vt() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// One-line snippet with matches emphasized (ANSI bold on TTYs).
fn render_snippet(snip: &quicksearch_core::snippet::Snippet, color: bool, tty: bool) -> String {
    // Each piece is scrubbed as it goes in, before the emphasis codes, so
    // the only escapes in the result are the ones put there here.
    let mut out = String::new();
    let push = |out: &mut String, piece: &str| {
        // Flattened *before* the scrub; `one_line` says why.
        let flat = one_line(piece);
        if tty {
            out.push_str(&scrub_controls(&flat));
        } else {
            out.push_str(&flat);
        }
    };
    if snip.truncated_start {
        out.push('…');
    }
    let mut cursor = 0;
    for &(start, end) in &snip.ranges {
        push(&mut out, &snip.window[cursor..start]);
        if color {
            out.push_str("\x1b[1m");
        }
        push(&mut out, &snip.window[start..end]);
        if color {
            out.push_str("\x1b[0m");
        }
        cursor = end;
    }
    push(&mut out, &snip.window[cursor..]);
    if snip.truncated_end {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicksearch_core::security::{generate_salt, salt_to_hex};

    fn protected() -> SecurityConfig {
        SecurityConfig {
            password_protected: true,
            salt: Some(salt_to_hex(&generate_salt())),
            use_keychain: false,
        }
    }

    fn mismatch() -> Result<(), String> {
        Err(format!(
            "{}wrong-password: index at /x: wrong password",
            db::KEY_MISMATCH_PREFIX
        ))
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// The query `args` parse to.
    fn query(args: &[&str]) -> Cli {
        match parse(argv(args)) {
            Some(Ok(cli)) => cli,
            other => panic!("{args:?} should be a query, got {other:?}"),
        }
    }

    /// The message of the usage error `args` must be.
    fn usage_error(args: &[&str]) -> String {
        match parse(argv(args)) {
            Some(Err((2, message))) => {
                assert!(message.ends_with(USAGE), "{args:?}: usage appended");
                message
            }
            other => panic!("{args:?} should be a usage error, got {other:?}"),
        }
    }

    #[test]
    fn help_and_version_return_their_text_with_code_0() {
        for flag in ["-h", "--help"] {
            assert_eq!(parse(argv(&[flag])), Some(Err((0, USAGE.to_string()))));
        }
        for flag in ["-V", "--version"] {
            let Some(Err((0, text))) = parse(argv(&[flag])) else {
                panic!("{flag} should answer with code 0");
            };
            assert!(text.contains(crate::version::BUILD_ID), "{text}");
        }
        // Still flags after other flags, before any term.
        assert_eq!(
            parse(argv(&["--fuzzy", "-h"])).map(|r| r.map_err(|e| e.0)),
            Some(Err(0))
        );
    }

    #[test]
    fn only_empty_argv_opens_the_gui() {
        assert_eq!(parse(argv(&[])), None);
        // What used to fall through to the GUI as a possible windowing flag.
        usage_error(&["--some-winit-flag"]);
    }

    #[test]
    fn flags_without_terms_are_usage_errors() {
        for args in [
            &["--fuzzy"][..],
            &["--long", "--limit", "3"],
            &["--"],
            &["--fuzzy", "--"],
        ] {
            assert!(
                usage_error(args).starts_with("nothing to search for"),
                "{args:?}"
            );
        }
    }

    #[test]
    fn an_unknown_leading_flag_is_a_usage_error() {
        for flag in ["--frobnicate", "-x", "-", "--toggle"] {
            let message = usage_error(&[flag, "term"]);
            assert!(message.contains(&format!("'{flag}'")), "{message}");
        }
    }

    /// `-h` after a query is something to find, not a request for help.
    #[test]
    fn a_later_dash_word_is_a_term() {
        let cli = query(&["--fuzzy", "notes", "-h", "--limit", "0", "--", "-draft"]);
        assert_eq!(cli.query, "notes -h --limit 0 -- -draft");
        assert!(cli.fuzzy);
        assert_eq!(cli.limit, None);
    }

    #[test]
    fn double_dash_makes_the_rest_terms() {
        let cli = query(&["--long", "--", "-draft", "--help"]);
        assert_eq!(
            cli,
            Cli {
                query: "-draft --help".to_string(),
                fuzzy: false,
                limit: None,
                long: true,
            }
        );
        // Only the first one is the separator.
        assert_eq!(query(&["--", "--"]).query, "--");
    }

    /// Both spellings, since only one of them goes through `it.next()`.
    #[test]
    fn a_non_numeric_limit_is_a_usage_error() {
        for args in [
            &["--limit", "x", "term"][..],
            &["--limit=x", "term"],
            &["--limit=", "term"],
            &["--limit", "-3", "term"],
            &["--limit"],
        ] {
            assert!(
                usage_error(args).starts_with("--limit requires"),
                "{args:?}"
            );
        }
    }

    /// `0` would print nothing and then "(truncated at 0 results...)".
    #[test]
    fn a_zero_limit_is_a_usage_error() {
        usage_error(&["--limit", "0", "term"]);
        usage_error(&["--limit=0", "term"]);
    }

    #[test]
    fn an_overflowing_limit_is_a_usage_error() {
        let past = format!("{}0", usize::MAX);
        usage_error(&["--limit", &past, "term"]);
        usage_error(&[&format!("--limit={past}"), "term"]);
    }

    /// No upper bound: the cascade saturates its arithmetic instead.
    #[test]
    fn a_usize_max_limit_is_accepted() {
        let max = usize::MAX.to_string();
        assert_eq!(
            query(&["--limit", &max, "term"]).limit,
            NonZeroUsize::new(usize::MAX)
        );
        let cli = query(&[&format!("--limit={max}"), "term"]);
        assert_eq!(cli.limit, NonZeroUsize::new(usize::MAX));
        assert_eq!(query(&["--limit", "7", "term"]).limit, NonZeroUsize::new(7));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_is_a_term_not_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let args = args_from(&[OsString::from_vec(vec![b'a', 0xff])]);
        assert_eq!(args, ["a\u{FFFD}"]);
        assert_eq!(
            parse(args).map(|r| r.map(|c| c.query)),
            Some(Ok("a\u{FFFD}".to_string()))
        );
    }

    /// A writer whose reader has gone, as `| head` leaves stdout.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn hit(path: &str) -> SearchHit {
        SearchHit {
            file_id: 1,
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            path: path.to_string(),
            size: 42,
            mtime: 0,
            rank: 1.0,
            stage: 1,
            snippet: None,
        }
    }

    #[test]
    fn a_closed_pipe_is_reported_as_broken_pipe() {
        let hits = [hit("/d/a.txt")];
        for long in [false, true] {
            let printed = print_hits(&mut ClosedPipe, &hits, long, false);
            assert_eq!(
                printed.as_ref().map_err(|e| e.kind()),
                Err(io::ErrorKind::BrokenPipe)
            );
            assert_eq!(exit_code(&printed), 0, "a closed pipe is success");
        }
        let other = Err(io::Error::other("disk full"));
        assert_eq!(exit_code(&other), 2, "any other write error is not");
        assert_eq!(exit_code(&Ok(())), 0);
    }

    /// Piped output is byte-exact: no scrub, one path per line.
    #[test]
    fn piped_hits_print_one_raw_path_per_line() {
        let hits = [hit("/d/a.txt"), hit("/d/b\x1b[2Jc.txt")];
        let mut out = Vec::new();
        print_hits(&mut out, &hits, false, false).unwrap();
        assert_eq!(out, b"/d/a.txt\n/d/b\x1b[2Jc.txt\n");
    }

    /// Breaks print as spaces piped or not; a terminal also gets the OSC 52
    /// escape scrubbed, leaving the emphasis codes the only escapes.
    #[test]
    fn a_snippet_prints_on_one_line() {
        let snip = quicksearch_core::snippet::Snippet {
            window: "a\nb\r\nc\td\x1b]52;c;eA==\x07e".to_string(),
            ranges: vec![(2, 3)],
            truncated_start: true,
            truncated_end: false,
        };
        let mut long = hit("/d/a.txt");
        long.snippet = Some(snip.clone());
        let mut out = Vec::new();
        print_hits(&mut out, &[long], true, false).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), 2, "{out:?}");
        assert!(
            out.ends_with("        …a b  c d\x1b]52;c;eA==\x07e\n"),
            "{out:?}"
        );

        let scrubbed = "  c d\u{fffd}]52;c;eA==\u{fffd}e";
        assert_eq!(
            render_snippet(&snip, false, true),
            format!("…a b{scrubbed}")
        );
        assert_eq!(
            render_snippet(&snip, true, true),
            format!("…a \x1b[1mb\x1b[0m{scrubbed}")
        );
    }

    #[test]
    fn unprotected_needs_nothing() {
        let sec = SecurityConfig::default();
        let res = resolve_key(
            &sec,
            false,
            None,
            None,
            || panic!("must not prompt"),
            |_| panic!("must not try a key"),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn protected_without_salt_is_a_hard_error() {
        let sec = SecurityConfig {
            password_protected: true,
            salt: None,
            use_keychain: false,
        };
        let res = resolve_key(&sec, true, None, None, || None, |_| Ok(()));
        assert!(res.unwrap_err().contains("no salt"));
    }

    /// What the keychain would hold for `key_hex` under `sec`'s salt.
    fn remembered(sec: &SecurityConfig, key_hex: &str) -> Option<Zeroizing<String>> {
        let salt = sec.salt.as_deref().expect("a protected config has a salt");
        Some(crate::keychain::entry_text(salt, key_hex))
    }

    #[test]
    fn keychain_key_wins_without_prompting() {
        let sec = protected();
        let hex = "ab".repeat(32);
        let mut tried = 0;
        let res = resolve_key(
            &sec,
            true,
            Some(Zeroizing::new("unused".to_string())),
            remembered(&sec, &hex),
            || panic!("must not prompt"),
            |key| {
                tried += 1;
                assert_eq!(key.to_hex(), hex);
                Ok(())
            },
        );
        assert!(res.is_ok());
        assert_eq!(tried, 1);
    }

    #[test]
    fn stale_keychain_falls_through_to_env() {
        let sec = protected();
        let mut calls = 0;
        let res = resolve_key(
            &sec,
            false,
            Some(Zeroizing::new("pw".to_string())),
            remembered(&sec, &"cd".repeat(32)),
            || panic!("must not prompt"),
            |_| {
                calls += 1;
                if calls == 1 {
                    mismatch() // the stale keychain key
                } else {
                    Ok(()) // the env-derived key
                }
            },
        );
        assert!(res.is_ok());
        assert_eq!(calls, 2);
    }

    #[test]
    fn malformed_keychain_entry_is_skipped() {
        let sec = protected();
        let res = resolve_key(
            &sec,
            false,
            Some(Zeroizing::new("pw".to_string())),
            remembered(&sec, "not hex at all"),
            || panic!("must not prompt"),
            |_| Ok(()),
        );
        assert!(res.is_ok(), "bad keychain data must not be fatal");
    }

    /// An entry remembered for an earlier password, or in the old format
    /// with no salt, is never tried: the next source decides.
    #[test]
    fn a_stale_keychain_entry_is_not_tried() {
        let sec = protected();
        let derived = derive_key("pw", &sec.salt_bytes().unwrap()).to_hex();
        let earlier = SecurityConfig {
            salt: Some(salt_to_hex(&[0x11; 16])),
            ..protected()
        };
        let unsalted = Some(Zeroizing::new("ab".repeat(32)));
        for stale in [remembered(&earlier, &"ab".repeat(32)), unsalted] {
            let mut tried = Vec::new();
            let res = resolve_key(
                &sec,
                false,
                Some(Zeroizing::new("pw".to_string())),
                stale,
                || panic!("must not prompt"),
                |key| {
                    tried.push(key.to_hex());
                    Ok(())
                },
            );
            assert!(res.is_ok());
            assert_eq!(tried, vec![derived.clone()], "only the env password's key");
        }
    }

    #[test]
    fn wrong_env_password_fails_without_prompting() {
        let sec = protected();
        let res = resolve_key(
            &sec,
            true, // even on a TTY: a wrong explicit secret is an error, not a prompt
            Some(Zeroizing::new("wrong".to_string())),
            None,
            || panic!("must not prompt"),
            |_| mismatch(),
        );
        assert!(res.unwrap_err().contains(PASSWORD_ENV));
    }

    /// A set-but-empty variable is unset, not a wrong password: without a
    /// terminal the error says how to unlock, and with one the prompt runs.
    #[test]
    fn an_empty_env_password_counts_as_unset() {
        let sec = protected();
        let empty = || Some(Zeroizing::new(String::new()));
        let err = resolve_key(
            &sec,
            false,
            empty(),
            None,
            || panic!("must not prompt"),
            |_| panic!("must not try a key"),
        )
        .unwrap_err();
        assert!(err.contains("run from a terminal"), "{err}");
        assert!(!err.contains("does not match"), "{err}");

        let mut prompts = 0;
        let res = resolve_key(
            &sec,
            true,
            empty(),
            None,
            || {
                prompts += 1;
                Some(Zeroizing::new("pw".to_string()))
            },
            |_| Ok(()),
        );
        assert!(res.is_ok());
        assert_eq!(prompts, 1, "the prompt is the next source");
    }

    #[test]
    fn no_tty_no_sources_is_instructive() {
        let sec = protected();
        let err = resolve_key(&sec, false, None, None, || None, |_| Ok(())).unwrap_err();
        assert!(err.contains(PASSWORD_ENV));
        assert!(err.contains("Remember on this device"));
    }

    #[test]
    fn prompt_retries_then_succeeds() {
        let sec = protected();
        let mut prompts = 0;
        let mut tries = 0;
        let res = resolve_key(
            &sec,
            true,
            None,
            None,
            || {
                prompts += 1;
                Some(Zeroizing::new(format!("attempt{}", prompts)))
            },
            |_| {
                tries += 1;
                if tries < 2 {
                    mismatch()
                } else {
                    Ok(())
                }
            },
        );
        assert!(res.is_ok());
        assert_eq!(prompts, 2);
    }

    #[test]
    fn prompt_gives_up_after_three_wrong_passwords() {
        let sec = protected();
        let mut prompts = 0;
        let err = resolve_key(
            &sec,
            true,
            None,
            None,
            || {
                prompts += 1;
                Some(Zeroizing::new("nope".to_string()))
            },
            |_| mismatch(),
        )
        .unwrap_err();
        assert_eq!(prompts, 3);
        assert!(err.contains("3 attempts"));
    }

    #[test]
    fn unreadable_prompt_is_an_error() {
        let sec = protected();
        let err = resolve_key(&sec, true, None, None, || None, |_| Ok(())).unwrap_err();
        assert!(err.contains("failed to read password"));
    }

    #[test]
    fn non_mismatch_errors_are_fatal_immediately() {
        // e.g. the database vanished: retrying the password would mislead.
        let sec = protected();
        let err = resolve_key(
            &sec,
            true,
            None,
            None,
            || Some(Zeroizing::new("pw".to_string())),
            |_| Err("Failed to open database at /x: unable to open database file".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("unable to open"));
    }
}
