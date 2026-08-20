//! QuickSearch binary: `quicksearch <query>` searches from the terminal;
//! without a query it opens the egui desktop app.
//!
//! On Windows this is the GUI only, built as a window-subsystem app so no
//! console flashes behind it. Terminal search there is `quicksearch-cli`,
//! which is a console app and so keeps working pipes, exit codes, and a shell
//! that waits for it. A query passed here still does something useful: it
//! seeds the search box.
#![cfg_attr(windows, windows_subsystem = "windows")]

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
#[cfg(test)]
mod test_ui;
mod tips;
mod tracker;
mod tutorial;
mod ui_util;
mod unlock;
mod version;

use quicksearch_core::config::Config;
use quicksearch_core::platform::{IndexLock, LockError};

/// The window icon, shown in the titlebar, taskbar and alt-tab switcher.
///
/// X11 takes these pixels directly via `_NET_WM_ICON`. Wayland ignores them and
/// instead looks up the app id in `/usr/share/applications/`, so the id below has
/// to match the installed `quicksearch.desktop` for the icon to appear there.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/quicksearch-256.png"))
        .expect("bundled icon is a valid PNG")
}

/// Leftover positional arguments, joined — used to seed the search box.
/// Flags are dropped rather than parsed: eframe and winit take some of
/// their own.
fn seed_query() -> Option<String> {
    let terms: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn main() {
    // Must come first: anything below may print, and printing without a
    // stdio handle panics rather than failing quietly.
    #[cfg(windows)]
    platform::redirect_null_stdio();

    #[cfg(not(windows))]
    if let Some(code) = cli::maybe_run_cli() {
        std::process::exit(code);
    }

    // A broken config file should never keep the window from opening —
    // surface the error in-app and run on defaults.
    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let initial_query = seed_query();

    // After the CLI early-exit above, deliberately: `quicksearch <query>` only
    // reads, and must keep working from a terminal while the window is open.
    // Two *windows* on one index are the problem — two indexers writing, and,
    // once either has cancelled the other's SQLite locks, an attach that
    // truncates the wal-index under a live mapping.
    //
    // Held for the life of the process in `platform`'s own slot, so the
    // settings handler can move it when `database_path` changes; the kernel
    // releases it on exit, however that exit happens.
    match IndexLock::hold(&config.resolved_database_path()) {
        Ok(()) => {}
        Err(LockError::Held { pid }) => {
            let who = match pid {
                Some(pid) => format!(" (process {})", pid),
                None => String::new(),
            };
            let msg = format!(
                "QuickSearch is already running{}.\n\nOnly one window can use \
                 the index at a time. Switch to the running window, or close \
                 it and try again.",
                who
            );
            eprintln!("{}", msg);
            // The app is normally launched from a desktop icon or a hotkey,
            // where nothing is watching stderr.
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Info)
                .set_title("QuickSearch")
                .set_description(&msg)
                .show();
            std::process::exit(1);
        }
        // Not "the lock is taken" — the filesystem could not answer. A
        // convenience guard is never a good enough reason to refuse to open.
        Err(LockError::Unsupported(why)) => {
            eprintln!("warning: cannot lock the index ({}); starting anyway", why);
        }
    }

    // With protection on, try the keychain before the window opens; a
    // verified key means no prompt at all. `None` starts locked, and no
    // index is touched until unlocked.
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
            // First: egui is built without its bundled fonts, so a context
            // starts with no faces at all and lays every string out at zero
            // height. `set_fonts` is applied in the next `begin_pass`, and
            // this closure is the last place that is still ahead of frame 1.
            fonts::install(&cc.egui_ctx);
            // On Windows the registration owns a hidden window whose messages
            // the event loop must dispatch, so it must be made on that loop's
            // thread with the loop running — this closure is the first place
            // that is true. Before the gate, so the shortcut works while the
            // unlock screen is up.
            hotkey::init(&cc.egui_ctx, &config.ui.search_hotkey);
            // Before the gate so the unlock screen honors the setting.
            app::apply_theme(&cc.egui_ctx, &config.ui.color_scheme);
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
