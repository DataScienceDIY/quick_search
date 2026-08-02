//! Terminal query mode: `quicksearch [FLAGS] <query terms...>` runs the
//! same ranked cascade the GUI uses and prints results to stdout. With no
//! positional arguments the binary opens the GUI instead.

use std::io::IsTerminal;
use std::sync::atomic::AtomicU64;

use quicksearch_core::config::Config;
use quicksearch_core::db;
use quicksearch_core::query::split::split_for_cascade;
use quicksearch_core::search::{cascade, SearchHit, SearchOptions};

use crate::format::{fmt_mtime, human_size};

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

Query syntax matches the GUI: plain words form one phrase; filters like
type:Document, modified:>=2024-01-01, path:/dir, mime:application/pdf,
name:frag combine with it.";

/// Parse argv; `Some(exit_code)` when the invocation was CLI-mode (query
/// or --help), `None` to open the GUI.
///
/// Invariant: terminal mode never builds an [`IndexCoordinator`], so it
/// starts no filesystem watcher, no background threads, and consumes no
/// inotify watches — a one-shot query must not leave anything running or
/// compete for the per-user watch budget with a running GUI. It opens the
/// database, queries, prints, and exits. Keep it that way: the coordinator
/// belongs to the GUI path in `backend.rs` alone.
///
/// [`IndexCoordinator`]: quicksearch_core::coordinator::IndexCoordinator
pub fn maybe_run_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
            "--fuzzy" => fuzzy = true,
            "--long" => long = true,
            "--limit" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => limit = Some(n),
                None => {
                    eprintln!("--limit requires a number\n\n{}", USAGE);
                    return Some(2);
                }
            },
            other if other.starts_with("--limit=") => {
                match other["--limit=".len()..].parse() {
                    Ok(n) => limit = Some(n),
                    Err(_) => {
                        eprintln!("--limit requires a number\n\n{}", USAGE);
                        return Some(2);
                    }
                }
            }
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

fn run_query(query: &str, fuzzy: bool, limit: Option<usize>, long: bool) -> i32 {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {}", e);
            return 2;
        }
    };
    let db_path = config.resolved_database_path();
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
