//! QuickSearch binary: `quicksearch <query>` searches from the terminal;
//! without a query it opens the egui desktop app. On Windows this is the GUI
//! only (window-subsystem, so no console flashes); terminal search there is
//! `quicksearch-cli`. A query passed here still seeds the search box.
#![cfg_attr(windows, windows_subsystem = "windows")]

// Only the binary that declares it gets it — a library cannot choose an
// allocator for its dependents — so this line is what actually puts the app
// on mimalloc. See `platform::Allocator` for why it is not glibc's.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

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
mod spotlight;
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

/// X11 takes these pixels directly; Wayland ignores them and looks the app
/// id up in `/usr/share/applications/`, so the id below must match the
/// installed `quicksearch.desktop`.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/quicksearch-256.png"))
        .expect("bundled icon is a valid PNG")
}

/// Positional arguments, joined, to seed the search box; flags are dropped
/// unparsed (eframe and winit take some of their own).
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
    // First: printing without a stdio handle panics rather than failing quietly.
    #[cfg(windows)]
    platform::redirect_null_stdio();

    #[cfg(not(windows))]
    if let Some(code) = cli::maybe_run_cli() {
        std::process::exit(code);
    }

    // A broken config must never keep the window from opening.
    let (config, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    // Before any search connection exists: the ceiling is applied at open, and
    // `0` leaves it derived from the index.
    quicksearch_core::db::set_search_cache_override(
        (config.search.cache_size_mib != 0).then_some(config.search.cache_size_mib as i64),
    );
    let initial_query = seed_query();

    // After the CLI early-exit, deliberately: the CLI only reads. Two
    // *windows* on one index are the problem — two indexers writing, and an
    // attach that truncates the wal-index under a live mapping. Held for the
    // life of the process; the kernel releases it however the exit happens.
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
            // Launched from a desktop icon, nothing watches stderr; show a dialog.
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Info)
                .set_title("QuickSearch")
                .set_description(&msg)
                .show();
            std::process::exit(1);
        }
        // The filesystem could not answer: a convenience guard is never a
        // good enough reason to refuse to open.
        Err(LockError::Unsupported(why)) => {
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
            // egui has no bundled fonts; this closure is the last place
            // still ahead of frame 1.
            fonts::install(&cc.egui_ctx);
            // Must run on the event-loop thread with the loop running — this
            // closure is the first place that is true. Before the gate, so
            // the shortcut works while the unlock screen is up.
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
