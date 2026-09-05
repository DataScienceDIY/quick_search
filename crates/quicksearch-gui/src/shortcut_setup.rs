//! Writing the system-wide search shortcut into the desktop's own keyboard
//! configuration, so "bind this command yourself" becomes one click.
//!
//! Auto-writing was once rejected here on the grounds that a shortcut the
//! user cannot see is worse than one they created. The answer is to write it
//! exactly where the desktop's own settings UI lists and edits it — GNOME's
//! custom shortcuts, KDE's global shortcuts — so the binding stays the
//! user's to inspect, change or delete. Desktops without a known home for a
//! binding keep the manual copy-the-command flow in
//! `crate::settings_tab::shortcut_note`.
//!
//! On Windows the closed-app binding lives in the Start-menu `.lnk` the
//! installer creates, so there is nothing to *create* from here; what this
//! module does is keep that `.lnk`'s hotkey in step with the in-app one via
//! [`hotkey_changed`].

use crate::hotkey::Binding;

/// Where a binding can be written. `Unsupported` hides the one-click button
/// and leaves the manual flow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Desktop {
    Gnome,
    Kde,
    Unsupported,
}

pub fn detect() -> Desktop {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        desktop_for(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        Desktop::Unsupported
    }
}

/// The mapping itself, split from the environment so it can be tested
/// without one; the same shape as `platform::keyboard_settings_for`.
#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_for(desktops: &str) -> Desktop {
    for desktop in desktops.split(':') {
        match desktop.to_ascii_uppercase().as_str() {
            "GNOME" | "UNITY" => return Desktop::Gnome,
            "KDE" => return Desktop::Kde,
            _ => {}
        }
    }
    Desktop::Unsupported
}

/// Whether the binding this module writes is currently present. Asks the
/// desktop, so callers should cache rather than poll every frame.
pub fn installed() -> bool {
    match detect() {
        Desktop::Gnome => gnome::installed(),
        Desktop::Kde => kde::installed(),
        Desktop::Unsupported => false,
    }
}

/// Write `binding` → `quicksearch --toggle` into the desktop's keyboard
/// configuration. Idempotent: a second install rewrites the same entry.
pub fn install(binding: &Binding) -> Result<(), String> {
    let command = format!("{} --toggle", crate::activate::command_name());
    match detect() {
        Desktop::Gnome => gnome::install(binding, &command),
        Desktop::Kde => kde::install(binding),
        Desktop::Unsupported => Err("this desktop is not supported".to_string()),
    }
}

/// Delete the entry [`install`] wrote; a no-op if it is already gone.
pub fn remove() -> Result<(), String> {
    match detect() {
        Desktop::Gnome => gnome::remove(),
        Desktop::Kde => kde::remove(),
        Desktop::Unsupported => Err("this desktop is not supported".to_string()),
    }
}

/// The in-app shortcut was rebound: keep the system-wide binding in step.
/// Best-effort — a failure is logged, not surfaced, because the change the
/// user asked for (the in-app key) already succeeded.
pub fn hotkey_changed(setting: &str) {
    let Ok(Some(binding)) = crate::hotkey::parse_setting(setting) else {
        // Cleared or unparseable: leave the system binding alone rather than
        // guess; the Settings tab's own controls are the way to remove it.
        return;
    };
    #[cfg(windows)]
    {
        if let Err(e) = lnk::update_hotkey(&binding) {
            quicksearch_core::log_warn!("updating the Start menu shortcut key: {}", e);
        }
    }
    #[cfg(not(windows))]
    {
        if installed() {
            if let Err(e) = install(&binding) {
                quicksearch_core::log_warn!("updating the system search shortcut: {}", e);
            }
        }
    }
}

/// Run a program to completion and fail loudly, with its stderr as the why.
#[cfg(all(unix, not(target_os = "macos")))]
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("running {}: {}", program, e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} failed: {}", program, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// GNOME: a custom keybinding under a path of our own. The fixed path is
/// what makes install idempotent and removal exact, and the entry shows up
/// in Settings → Keyboard → Custom Shortcuts under the name given here.
#[cfg(all(unix, not(target_os = "macos")))]
mod gnome {
    use super::*;

    const LIST_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
    const LIST_KEY: &str = "custom-keybindings";
    const ENTRY_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
    pub(super) const ENTRY_PATH: &str =
        "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/quicksearch-search/";

    pub(super) fn installed() -> bool {
        run("gsettings", &["get", LIST_SCHEMA, LIST_KEY])
            .map(|list| parse_string_list(&list).iter().any(|p| p == ENTRY_PATH))
            .unwrap_or(false)
    }

    pub(super) fn install(binding: &Binding, command: &str) -> Result<(), String> {
        let entry = format!("{}:{}", ENTRY_SCHEMA, ENTRY_PATH);
        for (key, value) in [
            ("name", "QuickSearch".to_string()),
            ("command", command.to_string()),
            ("binding", binding.gtk_accelerator()),
        ] {
            run("gsettings", &["set", &entry, key, &value])?;
        }
        // The entry only takes effect once its path is on the list; last, so
        // a failure above cannot leave a listed entry with no command.
        let list = run("gsettings", &["get", LIST_SCHEMA, LIST_KEY])?;
        let mut paths = parse_string_list(&list);
        if !paths.iter().any(|p| p == ENTRY_PATH) {
            paths.push(ENTRY_PATH.to_string());
            let list = format_string_list(&paths);
            run("gsettings", &["set", LIST_SCHEMA, LIST_KEY, &list])?;
        }
        Ok(())
    }

    pub(super) fn remove() -> Result<(), String> {
        let list = run("gsettings", &["get", LIST_SCHEMA, LIST_KEY])?;
        let paths: Vec<String> = parse_string_list(&list)
            .into_iter()
            .filter(|p| p != ENTRY_PATH)
            .collect();
        let list = format_string_list(&paths);
        run("gsettings", &["set", LIST_SCHEMA, LIST_KEY, &list])?;
        let entry = format!("{}:{}", ENTRY_SCHEMA, ENTRY_PATH);
        run("gsettings", &["reset-recursively", &entry])?;
        Ok(())
    }
}

/// The GVariant `as` (array of strings) spelling `gsettings get` prints and
/// `gsettings set` accepts: `['a', 'b']`, or `@as []` when empty.
///
/// The parser accepts exactly what gsettings emits — single-quoted strings
/// with `\'` and `\\` escapes — and drops anything malformed rather than
/// guessing: a path we misread would be written back verbatim into the
/// user's configuration.
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_string_list(raw: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut current = None;
    let mut escaped = false;
    for ch in raw.chars() {
        match current.as_mut() {
            None => {
                if ch == '\'' {
                    current = Some(String::new());
                }
            }
            Some(path) => {
                if escaped {
                    path.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' {
                    paths.push(current.take().expect("current is Some in this arm"));
                } else {
                    path.push(ch);
                }
            }
        }
    }
    paths
}

#[cfg(all(unix, not(target_os = "macos")))]
fn format_string_list(paths: &[String]) -> String {
    if paths.is_empty() {
        // A bare `[]` has no type; this is the empty list gsettings prints.
        return "@as []".to_string();
    }
    let quoted: Vec<String> = paths
        .iter()
        .map(|p| format!("'{}'", p.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// KDE: the global-shortcuts entry for the `Search` action that
/// `packaging/quicksearch.desktop` declares (`Exec=quicksearch --toggle`).
/// kglobalaccel launches desktop-file actions itself, so no command is
/// written here — only the key, in the file KDE's own Shortcuts settings
/// page reads and edits.
#[cfg(all(unix, not(target_os = "macos")))]
mod kde {
    use super::*;

    const FILE: &str = "kglobalshortcutsrc";
    const GROUP: &str = "quicksearch.desktop";

    /// Plasma 6's tool first; 5's second. The first present wins.
    fn config_tool(names: [&'static str; 2]) -> &'static str {
        let on_path = |name: &str| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
            })
        };
        if on_path(names[0]) {
            names[0]
        } else {
            names[1]
        }
    }

    /// The entry format is `active,default,description`.
    pub(super) fn entry(binding: &Binding) -> String {
        format!("{},none,Search", binding)
    }

    pub(super) fn installed() -> bool {
        let tool = config_tool(["kreadconfig6", "kreadconfig5"]);
        run(tool, &["--file", FILE, "--group", GROUP, "--key", "Search"])
            .map(|out| {
                let active = out.trim().split(',').next().unwrap_or("");
                !active.is_empty() && active != "none"
            })
            .unwrap_or(false)
    }

    pub(super) fn install(binding: &Binding) -> Result<(), String> {
        let tool = config_tool(["kwriteconfig6", "kwriteconfig5"]);
        let entry = entry(binding);
        for (key, value) in [("_k_friendly_name", "QuickSearch"), ("Search", &entry)] {
            run(
                tool,
                &["--file", FILE, "--group", GROUP, "--key", key, value],
            )?;
        }
        reload();
        Ok(())
    }

    pub(super) fn remove() -> Result<(), String> {
        let tool = config_tool(["kwriteconfig6", "kwriteconfig5"]);
        for key in ["Search", "_k_friendly_name"] {
            run(
                tool,
                &["--file", FILE, "--group", GROUP, "--key", key, "--delete"],
            )?;
        }
        reload();
        Ok(())
    }

    /// Ask kglobalaccel to re-read its file. Best-effort: without it the
    /// binding takes effect at the next login, which install's caller says.
    fn reload() {
        for qdbus in ["qdbus6", "qdbus"] {
            if run(
                qdbus,
                &[
                    "org.kde.kglobalaccel",
                    "/kglobalaccel",
                    "org.kde.KGlobalAccel.reloadConfig",
                ],
            )
            .is_ok()
            {
                return;
            }
        }
    }
}

/// Windows: the hotkey field of the Start-menu `.lnk` is the closed-app
/// binding (see `packaging/quicksearch.nsi`), rewritten through
/// `WScript.Shell` — whose `Hotkey` property takes exactly the
/// `Ctrl+Shift+F` spelling [`Binding`] displays — rather than through a COM
/// vtable of our own.
#[cfg(windows)]
mod lnk {
    use super::*;
    use std::path::PathBuf;

    fn start_menu_lnk(env: &str) -> Option<PathBuf> {
        let base = std::env::var_os(env)?;
        let path = PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs\QuickSearch.lnk");
        path.is_file().then_some(path)
    }

    pub(super) fn update_hotkey(binding: &Binding) -> Result<(), String> {
        // The installer elevates and writes the all-users Start menu; a
        // per-user one is checked first because that is the one this
        // unelevated process can rewrite.
        let lnk = start_menu_lnk("APPDATA")
            .or_else(|| start_menu_lnk("ProgramData"))
            .ok_or("no Start menu shortcut exists; re-run the installer")?;
        let script = format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); \
             $s.Hotkey = '{}'; $s.Save()",
            lnk.display().to_string().replace('\'', "''"),
            binding,
        );
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|e| format!("running powershell: {}", e))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // The common failure: the .lnk is the elevated installer's.
            return Err(format!(
                "could not rewrite {} ({}); if QuickSearch was installed for \
                 all users, re-run the installer to change the key",
                lnk.display(),
                stderr.trim(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_desktop_is_detected_from_the_list_case_insensitively() {
        assert_eq!(desktop_for("GNOME"), Desktop::Gnome);
        assert_eq!(desktop_for("ubuntu:GNOME"), Desktop::Gnome);
        assert_eq!(desktop_for("kde"), Desktop::Kde);
        assert_eq!(desktop_for("Unity"), Desktop::Gnome);
        assert_eq!(desktop_for(""), Desktop::Unsupported);
        assert_eq!(desktop_for("i3:sway"), Desktop::Unsupported);
    }

    /// Round trip through the exact spellings gsettings prints.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_gvariant_list_round_trips() {
        assert_eq!(parse_string_list("@as []"), Vec::<String>::new());
        assert_eq!(parse_string_list("[]"), Vec::<String>::new());
        let two = parse_string_list("['/a/path/', '/b/path/']");
        assert_eq!(two, ["/a/path/", "/b/path/"]);
        assert_eq!(format_string_list(&two), "['/a/path/', '/b/path/']");
        assert_eq!(format_string_list(&[]), "@as []");
    }

    /// A path with a quote in it must survive both directions, or the write
    /// back would corrupt the user's other bindings.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn escaped_quotes_round_trip() {
        let paths = vec!["/it's/".to_string(), "/back\\slash/".to_string()];
        let formatted = format_string_list(&paths);
        assert_eq!(parse_string_list(&formatted), paths);
    }

    /// Malicious or truncated gsettings output must never panic; at worst it
    /// yields fewer paths.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn garbage_lists_parse_to_something_harmless() {
        for garbage in ["", "[", "['unterminated", "not a list", "['a'", "\\"] {
            let _ = parse_string_list(garbage);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_kde_entry_carries_the_binding_first() {
        let binding: Binding = "Ctrl+Shift+F".parse().unwrap();
        assert_eq!(kde::entry(&binding), "Ctrl+Shift+F,none,Search");
    }

    /// The fixed GNOME path is load-bearing twice over: idempotence and
    /// exact removal both key on it.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_gnome_entry_path_is_fixed_and_well_formed() {
        assert!(gnome::ENTRY_PATH.starts_with('/'));
        assert!(gnome::ENTRY_PATH.ends_with('/'));
        assert!(gnome::ENTRY_PATH.contains("quicksearch"));
    }
}
