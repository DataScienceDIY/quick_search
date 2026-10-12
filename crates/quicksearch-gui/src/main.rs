//! QuickSearch binary: `quicksearch <query>` searches from the terminal;
//! with no arguments it opens the egui desktop app. On Windows this is the GUI
//! only (window-subsystem, so no console flashes); terminal search there is
//! `quicksearch-cli`. A query passed here still seeds the search box.
#![cfg_attr(windows, windows_subsystem = "windows")]

// This line puts the app on mimalloc: a library cannot choose an allocator
// for its dependents. See `platform::Allocator` for why it is not glibc's.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

mod activate;
mod app;
mod backend;
#[cfg(feature = "capture")]
mod capture;
#[cfg(not(windows))]
mod cli;
mod color;
mod duplicates_tab;
mod fonts;
mod format;
mod help_tab;
mod hotkey;
mod keychain;
mod logs_tab;
mod manage_tab;
mod platform;
mod query_highlight;
mod search_tab;
mod settings_tab;
mod shortcut_setup;
mod spotlight;
#[cfg(test)]
mod test_ui;
mod tips;
mod tracker;
mod tutorial;
mod ui_util;
mod unlock;
mod version;

use std::ffi::OsString;
use std::time::Duration;

use activate::Signal;
use quicksearch_core::config::Config;
use quicksearch_core::extract::sandbox;
use quicksearch_core::platform::{IndexLock, LockError};

/// X11 takes these pixels directly; Wayland ignores them and looks the app
/// id up in `/usr/share/applications/`, so the id below must match the
/// installed `quicksearch.desktop`.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/quicksearch-256.png"))
        .expect("bundled icon is a valid PNG")
}

/// `args` split into the leading flags and the query terms, by the rule the
/// usage and man page state: flags come first, and from the first term on
/// every argument is part of the query. A `--` ends the flags too, and is
/// dropped.
fn split_flags(args: &[OsString]) -> (&[OsString], &[OsString]) {
    let end = args
        .iter()
        .position(|a| a == "--" || !a.as_encoded_bytes().starts_with(b"-"))
        .unwrap_or(args.len());
    let terms = match args.get(end) {
        Some(a) if a == "--" => &args[end + 1..],
        _ => &args[end..],
    };
    (&args[..end], terms)
}

/// The query terms, joined, to seed the search box. On Linux the GUI sees
/// arguments only with `--toggle` (any other argv is terminal mode); the
/// Windows GUI binary seeds from whatever it is given. Decoded lossily, as
/// terminal mode decodes them.
fn seed_query(args: &[OsString]) -> Option<String> {
    let terms: Vec<String> = split_flags(args)
        .1
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// What the desktop's search shortcut runs. Not a query flag: it is checked
/// before terminal mode, because `--toggle` is about the window.
const TOGGLE_FLAG: &str = "--toggle";

/// Whether `--toggle` is among the leading flags; a term once the query has
/// begun.
fn wants_toggle(args: &[OsString]) -> bool {
    split_flags(args).0.iter().any(|a| a == TOGGLE_FLAG)
}

/// Hands the activation to the instance holding the index: `true` once one
/// takes it. Asked up to `tries` times, `pause` apart, while nothing answers,
/// since the holder takes the lock as soon as `main` reaches it but listens
/// only once eframe's creation closure has run. One that answers and then
/// stalls (a wedged instance, or another program on the name) is not asked
/// again: each ask would wait the timeout out once more.
fn hand_over(mut ask: impl FnMut() -> Signal, tries: u32, pause: Duration) -> bool {
    for _ in 0..tries {
        match ask() {
            Signal::Delivered => return true,
            Signal::Unanswered => return false,
            Signal::Absent => std::thread::sleep(pause),
        }
    }
    false
}

fn main() {
    // An extraction sandbox child, before stdio, logging or a window: its
    // stdout is the parent's pipe.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == sandbox::SANDBOX_FLAG)
    {
        std::process::exit(sandbox::sandbox_child_main(std::env::args_os().skip(2)));
    }
    // Every PDF, audio and legacy Office file this process parses goes to
    // such a child. On Linux through /proc/self/exe, which names this image
    // even once an upgrade has replaced the file on disk. In an AppImage that
    // is the binary inside the mounted image, and deliberately not $APPIMAGE
    // (`platform::launched_exe`): running the .AppImage would mount the image
    // again for every file. The mount lasts as long as this process, which
    // no child outlives.
    #[cfg(target_os = "linux")]
    sandbox::enable_sandbox(std::path::PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    if let Ok(exe) = std::env::current_exe() {
        sandbox::enable_sandbox(exe);
    }

    // Before anything prints: printing without a stdio handle panics rather
    // than failing quietly.
    #[cfg(windows)]
    platform::redirect_null_stdio();

    // Never `std::env::args`, which panics on an argument that is not Unicode.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let toggle = wants_toggle(&args);

    // Before terminal mode, which would read `--toggle` as an unknown flag.
    #[cfg(not(windows))]
    if !toggle {
        if let Some(code) = cli::maybe_run_cli(&args) {
            std::process::exit(code);
        }
    }

    // A config that fails to load (TOML, values or ignore patterns) must not
    // keep the window from opening; the fallback is never saved and never
    // indexes on its own.
    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (
            Config::fallback(),
            Some(format!(
                "Config problem: {}; running on defaults, nothing is saved",
                e
            )),
        ),
    };

    // The desktop's shortcut, on an app that is already up: hand the
    // activation over and get out of the way. Failure is the normal case
    // (nothing is running) and falls through to starting the GUI, so one
    // binding both raises and launches.
    let toggled = toggle.then(|| activate::signal(&Config::config_path()));
    if toggled == Some(Signal::Delivered) {
        return;
    }
    // Before any search connection exists: the ceiling is applied at open, and
    // `0` leaves it derived from the index.
    quicksearch_core::db::set_search_cache_override(
        (config.search.cache_size_mib != 0).then_some(config.search.cache_size_mib as i64),
    );
    let initial_query = seed_query(&args);

    // After the CLI early-exit, deliberately: the CLI only reads. Two windows
    // on one index are the problem: two indexers writing, and an attach that
    // truncates the wal-index under a live mapping. Held for the life of the
    // process; the kernel releases it however the exit happens.
    match IndexLock::hold(&config.resolved_database_path()) {
        Ok(()) => {}
        Err(LockError::Held { pid }) => {
            // A second launch, or one that lost the race to an instance that
            // came up after the signal above: either way there is a window to
            // show. Retried for two seconds; a slower or wedged one gets the
            // dialog, at once if it left the toggle above unanswered.
            let ask = || activate::signal(&Config::config_path());
            if toggled != Some(Signal::Unanswered) && hand_over(ask, 20, Duration::from_millis(100))
            {
                return;
            }
            let msg = format!(
                "QuickSearch is already running{}.\n\nOnly one window can use \
                 the index at a time. Switch to the running window, or close \
                 it and try again.",
                app::held_by(pid)
            );
            eprintln!("{}", msg);
            // Launched from a desktop icon, nothing watches stderr; show a dialog.
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Info)
                .set_title("QuickSearch")
                .set_description(&msg)
                .show();
            std::process::exit(1);
        }
        // The filesystem could not answer: a convenience guard is never a
        // good enough reason to refuse to open. Without the lock only a
        // signal tells whether an instance is up, and `listen` would unlink
        // its live socket; a `--toggle` already asked above.
        Err(LockError::Unsupported(why)) => {
            if !toggle && activate::signal(&Config::config_path()) == Signal::Delivered {
                return;
            }
            eprintln!("warning: cannot lock the index ({}); starting anyway", why);
        }
    }

    // Try the keychain before the window opens; a verified key means no
    // prompt at all. `None` starts locked.
    let key_source = if !config.security.password_protected {
        Some(unlock::KeySource::Unprotected)
    } else if unlock::try_keychain_unlock(&config) {
        Some(unlock::KeySource::Keychain)
    } else {
        None
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("QuickSearch")
            .with_app_id("quicksearch")
            .with_icon(app_icon())
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "QuickSearch",
        native_options,
        Box::new(move |cc| {
            // Everything here runs before frame 1 and before the unlock gate,
            // so the unlock screen already has fonts, theme and working
            // shortcuts.
            fonts::install(&cc.egui_ctx);
            // Safe to bind now: we hold the index lock, or a signal went
            // unanswered, so no other instance is listening.
            activate::listen(&cc.egui_ctx, &Config::config_path());
            // Needs the running event loop, which starts here.
            hotkey::init(&cc.egui_ctx, &config.ui.search_hotkey);
            app::apply_theme(&cc.egui_ctx, &config.ui.color_scheme);
            color::apply_text_contrast(&cc.egui_ctx);
            let gate = match key_source {
                Some(source) => {
                    unlock::Gate::running(&cc.egui_ctx, config, config_error, initial_query, source)
                        .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?
                }
                None => unlock::Gate::locked(config, config_error, initial_query),
            };
            Ok(Box::new(gate) as Box<dyn eframe::App>)
        }),
    );
    if let Err(e) = result {
        eprintln!("failed to start GUI: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// From the first term on, `--toggle` is a word to search for, as it
    /// is after a `--`.
    #[test]
    fn wants_toggle_reads_only_the_leading_flags() {
        assert!(wants_toggle(&os(&["--toggle"])));
        assert!(wants_toggle(&os(&["--fuzzy", "--toggle", "notes"])));
        assert!(wants_toggle(&os(&["--toggle", "--"])));
        assert!(!wants_toggle(&os(&["notes", "--toggle"])));
        assert!(!wants_toggle(&os(&["notes", "--toggle", "--"])));
        assert!(!wants_toggle(&os(&["--", "--toggle"])));
        assert!(!wants_toggle(&os(&["--toggles", "-toggle"])));
        assert!(!wants_toggle(&os(&[])));
    }

    #[test]
    fn seed_query_drops_leading_flags_and_keeps_every_term() {
        assert_eq!(seed_query(&os(&[])), None);
        assert_eq!(seed_query(&os(&["--toggle"])), None);
        assert_eq!(seed_query(&os(&["--toggle", "--"])), None);
        assert_eq!(
            seed_query(&os(&["--toggle", "q3", "-x", "report"])).as_deref(),
            Some("q3 -x report")
        );
        assert_eq!(
            seed_query(&os(&["--toggle", "--", "-draft", "--"])).as_deref(),
            Some("-draft --")
        );
        assert_eq!(
            seed_query(&os(&["--toggle", "a", "--", "b"])).as_deref(),
            Some("a -- b")
        );
    }

    /// What [`hand_over`] did with `answers` (then `Absent` for good): its
    /// verdict, and how many times it asked.
    fn handed_over(answers: &[Signal]) -> (bool, usize) {
        let mut asked = 0;
        let ask = || {
            asked += 1;
            answers.get(asked - 1).copied().unwrap_or(Signal::Absent)
        };
        let handed = hand_over(ask, 5, Duration::ZERO);
        (handed, asked)
    }

    /// A held index is asked again only while nothing answers: a stalled
    /// answer would only stall again, and five more would hang the launch.
    #[test]
    fn a_held_index_is_asked_again_only_while_nothing_answers() {
        use Signal::{Absent, Delivered, Unanswered};
        assert_eq!(handed_over(&[Delivered]), (true, 1));
        assert_eq!(handed_over(&[Absent, Absent, Delivered]), (true, 3));
        assert_eq!(handed_over(&[Unanswered, Delivered]), (false, 1));
        assert_eq!(handed_over(&[Absent, Unanswered]), (false, 2));
        assert_eq!(handed_over(&[]), (false, 5));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_seeds_lossily() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![OsString::from_vec(vec![b'a', 0xff])];
        assert!(!wants_toggle(&args));
        assert_eq!(seed_query(&args).as_deref(), Some("a\u{FFFD}"));
    }
}
