//! Terminal query mode: `quicksearch [FLAGS] <query terms...>` runs the
//! same ranked cascade the GUI uses and prints results to stdout. With no
//! positional arguments the binary opens the GUI instead.

use std::io::IsTerminal;
use std::sync::atomic::AtomicU64;

use quicksearch_core::config::{Config, SecurityConfig};
use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};
use quicksearch_core::security::{derive_key, IndexKey};
use zeroize::Zeroizing;

use crate::format::{fmt_mtime, human_size};

/// Scripting escape hatch for password-protected indexes. Caveat: other
/// processes of the same user can read this process's environment, and
/// exported variables end up in shell history.
const PASSWORD_ENV: &str = "QUICKSEARCH_PASSWORD";

pub(crate) const USAGE: &str = "\
QuickSearch: indexed file search

USAGE:
    quicksearch                          open the GUI
    quicksearch [FLAGS] <query terms>    search from the terminal
                                         (Windows: quicksearch-cli)

FLAGS:
    --fuzzy         also run the fuzzy filename/full-text passes
    --limit <N>     maximum results (default: [search].display_limit)
    --long          rank, size, mtime, and snippets instead of bare paths
    -h, --help      this help
    -V, --version   version and the commit it was built from

Query syntax matches the GUI: plain words form one phrase; filters like
type:Document, modified:>=2024-01-01, path:/dir, mime:application/pdf,
name:frag combine with it.

A password-protected index unlocks from, in order: the OS keychain (when
'remember on this device' is enabled in the GUI), the QUICKSEARCH_PASSWORD
environment variable, or an interactive prompt. Note that environment
variables are visible to other processes of the same user.";

/// Parse argv; `Some(exit_code)` when the invocation was CLI-mode (query
/// or --help), `None` to open the GUI.
///
/// Invariant: terminal mode never builds an [`IndexCoordinator`] — no
/// watcher, no background threads, no inotify watches consumed. A one-shot
/// query must not compete for the per-user watch budget with a running GUI.
///
/// [`IndexCoordinator`]: quicksearch_core::coordinator::IndexCoordinator
pub fn maybe_run_cli() -> Option<i32> {
    run_cli(std::env::args().skip(1).collect())
}

/// The body of [`maybe_run_cli`], taking argv for testability.
fn run_cli(args: Vec<String>) -> Option<i32> {
    let mut fuzzy = false;
    let mut long = false;
    let mut limit: Option<usize> = None;
    let mut terms: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", USAGE);
                return Some(0);
            }
            "-V" | "--version" => {
                println!("QuickSearch {}", crate::version::BUILD_ID);
                return Some(0);
            }
            "--fuzzy" => fuzzy = true,
            "--long" => long = true,
            "--limit" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => limit = Some(n),
                None => {
                    eprintln!("--limit requires a number\n\n{}", USAGE);
                    return Some(2);
                }
            },
            other if other.starts_with("--limit=") => match other["--limit=".len()..].parse() {
                Ok(n) => limit = Some(n),
                Err(_) => {
                    eprintln!("--limit requires a number\n\n{}", USAGE);
                    return Some(2);
                }
            },
            other if other.starts_with('-') && terms.is_empty() => {
                // Unknown flags without a query fall through to the GUI
                // (they may be eframe/winit flags).
                return None;
            }
            other => terms.push(other.to_string()),
        }
    }

    if terms.is_empty() {
        return None;
    }
    Some(run_query(&terms.join(" "), fuzzy, limit, long))
}

/// Unlock a protected index using whichever key source is available.
///
/// Order: keychain (when enabled) → `QUICKSEARCH_PASSWORD` → interactive
/// prompt (three attempts) → an instructive error. `try_key` installs a
/// candidate as the process key and verifies it against the index; sources
/// whose key doesn't fit fall through (keychain: stale entry) or fail hard
/// (env var, exhausted prompts). Password buffers are zeroized on drop and
/// consumed immediately by the KDF; nothing here retains or logs them.
pub(crate) fn resolve_key(
    security: &SecurityConfig,
    is_tty: bool,
    env_password: Option<Zeroizing<String>>,
    keychain_hex: Option<String>,
    mut prompt: impl FnMut() -> Option<Zeroizing<String>>,
    mut try_key: impl FnMut(IndexKey) -> Result<(), String>,
) -> Result<(), String> {
    if !security.password_protected {
        return Ok(());
    }
    let salt = security.salt_bytes()?;

    if let Some(hex) = keychain_hex {
        match IndexKey::from_hex(&hex).map_err(|e| format!("keychain entry: {}", e)) {
            Ok(key) => {
                match try_key(key) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.starts_with(db::KEY_MISMATCH_PREFIX) => {
                        eprintln!("warning: the key remembered in the OS keychain no longer opens this index");
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => eprintln!("warning: {}", e),
        }
    }

    if let Some(password) = env_password {
        let key = derive_key(&password, &salt);
        drop(password);
        return match try_key(key) {
            Ok(()) => Ok(()),
            Err(e) if e.starts_with(db::KEY_MISMATCH_PREFIX) => Err(format!(
                "{} does not match this index's password",
                PASSWORD_ENV
            )),
            Err(e) => Err(e),
        };
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
        match try_key(key) {
            Ok(()) => return Ok(()),
            Err(e) if e.starts_with(db::KEY_MISMATCH_PREFIX) => {
                eprintln!("Wrong password.");
            }
            Err(e) => return Err(e),
        }
    }
    Err("wrong password (3 attempts)".to_string())
}

/// Wire [`resolve_key`] to the real terminal, environment, keychain and
/// database, installing the verified key as the process key.
fn resolve_key_for_terminal(security: &SecurityConfig, db_path: &str) -> Result<(), String> {
    let keychain_hex = if security.use_keychain {
        crate::keychain::load_key(db_path).unwrap_or_else(|e| {
            eprintln!("warning: {}", e);
            None
        })
    } else {
        None
    };
    resolve_key(
        security,
        std::io::stdin().is_terminal(),
        std::env::var(PASSWORD_ENV).ok().map(Zeroizing::new),
        keychain_hex,
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

fn run_query(query: &str, fuzzy: bool, limit: Option<usize>, long: bool) -> i32 {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {}", e);
            return 2;
        }
    };
    let db_path = config.resolved_database_path();
    if config.security.password_protected {
        if !db_path.exists() {
            eprintln!(
                "No usable index at {}; run the GUI once to build it.",
                db_path.display()
            );
            return 2;
        }
        if let Err(e) = resolve_key_for_terminal(&config.security, &db_path.to_string_lossy()) {
            eprintln!("{}", e);
            return 2;
        }
    }
    // Read-write purely so SQLite may create the WAL shared-memory file
    // when no other process has the index open; nothing is written.
    let conn = match db::open_existing(&db_path.to_string_lossy(), true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "No usable index at {}; run the GUI once to build it.\n({})",
                db_path.display(),
                e
            );
            return 2;
        }
    };

    let split = match split_for_cascade(query) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("query: {}", e);
            return 2;
        }
    };

    if fuzzy {
        if let Some(warning) = config.search.fuzzy_edits_warning() {
            eprintln!("warning: {}", warning);
        }
    }
    let options = SearchOptions {
        fuzzy,
        fuzzy_max_edits: config.search.fuzzy_max_edits,
        limit: limit.unwrap_or(config.search.display_limit),
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
            let color = long && std::io::stdout().is_terminal() && enable_vt();
            for hit in &hits {
                if long {
                    println!(
                        "{:6.3}  {:>9}  {}  {}",
                        hit.rank,
                        human_size(hit.size),
                        fmt_mtime(hit.mtime),
                        hit.path
                    );
                    if let Some(snip) = &hit.snippet {
                        println!("        {}", render_snippet(snip, color));
                    }
                } else {
                    println!("{}", hit.path);
                }
            }
            if outcome.limited {
                eprintln!("(truncated at {} results; raise with --limit)", hits.len());
            }
            0
        }
        Ok(None) => 0, // unreachable: nothing cancels a CLI search
        Err(e) => {
            eprintln!("search: {}", e);
            2
        }
    }
}

/// Whether ANSI escapes will actually render.
///
/// Always true where the terminal is ANSI by nature. On Windows the console
/// only interprets escapes once `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is set:
/// Windows Terminal and Windows 11 have it already, older conhost needs it
/// turned on, and anything that refuses gets plain text rather than a screen
/// full of `\x1b[1m`.
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
fn render_snippet(snip: &quicksearch_core::snippet::Snippet, color: bool) -> String {
    let mut out = String::new();
    if snip.truncated_start {
        out.push('…');
    }
    let mut cursor = 0;
    for &(start, end) in &snip.ranges {
        out.push_str(&snip.window[cursor..start]);
        if color {
            out.push_str("\x1b[1m");
            out.push_str(&snip.window[start..end]);
            out.push_str("\x1b[0m");
        } else {
            out.push_str(&snip.window[start..end]);
        }
        cursor = end;
    }
    out.push_str(&snip.window[cursor..]);
    if snip.truncated_end {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
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
        Err(format!("{}wrong password", db::KEY_MISMATCH_PREFIX))
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// Informational flags answer and exit 0 without touching the index.
    #[test]
    fn informational_flags_answer_and_succeed() {
        for flag in ["-V", "--version", "-h", "--help"] {
            assert_eq!(run_cli(argv(&[flag])), Some(0), "{flag} should exit 0");
        }
    }

    /// Nothing to search for means "open the GUI".
    #[test]
    fn nothing_to_search_for_opens_the_gui() {
        assert_eq!(run_cli(argv(&[])), None);
        // Including flags the GUI stack might want for itself.
        assert_eq!(run_cli(argv(&["--some-winit-flag"])), None);
    }

    /// A malformed value is a usage error, not a silent default — in both
    /// spellings, since only one of them goes through `it.next()`.
    #[test]
    fn a_non_numeric_limit_is_a_usage_error() {
        assert_eq!(run_cli(argv(&["--limit", "x", "term"])), Some(2));
        assert_eq!(run_cli(argv(&["--limit=x", "term"])), Some(2));
        assert_eq!(run_cli(argv(&["--limit"])), Some(2));
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

    #[test]
    fn keychain_key_wins_without_prompting() {
        let sec = protected();
        let hex = "ab".repeat(32);
        let mut tried = 0;
        let res = resolve_key(
            &sec,
            true,
            Some(Zeroizing::new("unused".to_string())),
            Some(hex.clone()),
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
            Some("cd".repeat(32)),
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
            Some("not hex at all".to_string()),
            || panic!("must not prompt"),
            |_| Ok(()),
        );
        assert!(res.is_ok(), "bad keychain data must not be fatal");
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
        // e.g. the database file vanished between existence check and open:
        // retrying the password would mislead the user.
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
