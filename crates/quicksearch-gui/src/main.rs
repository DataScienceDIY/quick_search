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
mod format;
mod help_tab;
mod hotkey;
mod keychain;
mod logs_tab;
mod manage_tab;
mod options;
mod platform;
mod query_highlight;
mod search_tab;
#[cfg(test)]
mod test_ui;
mod tips;
mod tracker;
mod ui_util;
mod unlock;
mod version;

use quicksearch_core::config::Config;

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
///
/// Flags are dropped rather than parsed: eframe and winit take some of their
/// own, and a stray `--foo` should not end up in the query.
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

    // With protection on, try the keychain before the window opens; a
    // verified key means no prompt at all. Anything else starts locked —
    // the unlock screen owns password entry, bad-salt reporting, and the
    // forgot-password escape hatch. No index is touched until unlocked.
    //
    // `None` means "start locked". The two unlocked cases stay distinct
    // rather than collapsing to a bool, because anything the app later says
    // *about* the key has to know whether the user typed one.
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
            // Here rather than earlier in `main`: on Windows the registration
            // owns a hidden window whose messages the event loop has to
            // dispatch, so it has to be made on that loop's thread with the
            // loop already running. This closure is the first place that is
            // true. Registering before the gate also means the shortcut works
            // while the unlock screen is up.
            hotkey::init(&cc.egui_ctx, &config.ui.search_hotkey);
            // Before the gate so the unlock screen is not the one window that
            // ignores the setting.
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
