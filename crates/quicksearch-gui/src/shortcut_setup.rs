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
///
/// Off unix the two named desktops are unreachable — [`detect`] answers
/// `Unsupported` and the dispatchers below never name them — so the variants
/// exist there only to keep this one enum for every platform.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
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
///
/// The three functions here are split by platform the same way [`detect`] is:
/// the desktops that have a home for a binding are all unix, and their modules
/// only exist there, so naming them off unix would not compile.
pub fn installed() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match detect() {
            Desktop::Gnome => gnome::installed(),
            Desktop::Kde => kde::installed(),
            Desktop::Unsupported => false,
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        false
    }
}

/// Write `binding` → `quicksearch --toggle` into the desktop's keyboard
/// configuration. Idempotent: a second install rewrites the same entry.
pub fn install(binding: &Binding) -> Result<(), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let command = toggle_command();
        match detect() {
            Desktop::Gnome => gnome::install(binding, &command),
            Desktop::Kde => kde::install(binding, &command),
            Desktop::Unsupported => Err(UNSUPPORTED.to_string()),
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = binding;
        Err(UNSUPPORTED.to_string())
    }
}

/// What every platform without a home for a binding answers. On Windows the
/// closed-app binding is the installer's `.lnk`, so there is nothing here to
/// write and the Settings tab shows the manual note instead.
const UNSUPPORTED: &str = "this desktop is not supported";

/// The command line the key runs, quoted for a path with spaces in it —
/// both GNOME's `command` key and a desktop file's `Exec` split on
/// whitespace and honour double quotes.
#[cfg(all(unix, not(target_os = "macos")))]
fn toggle_command() -> String {
    let exe = crate::activate::command_name();
    if exe.contains(char::is_whitespace) {
        format!("\"{}\" --toggle", exe)
    } else {
        format!("{} --toggle", exe)
    }
}

/// Delete the entry [`install`] wrote; a no-op if it is already gone.
pub fn remove() -> Result<(), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match detect() {
            Desktop::Gnome => gnome::remove(),
            Desktop::Kde => kde::remove(),
            Desktop::Unsupported => Err(UNSUPPORTED.to_string()),
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        Err(UNSUPPORTED.to_string())
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

/// KDE: registration with the kglobalaccel daemon over DBus, which is the
/// only writer `kglobalshortcutsrc` has — the daemon rewrites that file at
/// will and *drops* groups it did not create, so editing it directly (the
/// first version of this module) produced an entry that neither fired nor
/// survived. `setShortcut` takes effect immediately and the daemon does its
/// own persisting.
///
/// What the key *runs* is a small desktop file in
/// `~/.local/share/kglobalaccel/`, the directory Plasma itself uses for
/// custom command shortcuts: a component whose name ends in `.desktop`
/// resolves to that file, and its `_launch` action runs the `Exec` line.
/// The file's presence is also this module's "installed" marker — written
/// last on install, deleted on remove, and free of a per-frame DBus call.
///
/// Verified live against Plasma 6.6: register → the key launches a closed
/// QuickSearch; `unregister` → the daemon drops the entry from its config.
#[cfg(all(unix, not(target_os = "macos")))]
mod kde {
    use super::*;
    use std::path::PathBuf;

    /// Ends in `.desktop`: what makes the daemon treat the component as a
    /// service it can launch rather than an app that must be running.
    const COMPONENT: &str = "quicksearch-search.desktop";
    const ACTION: &str = "_launch";
    /// KGlobalAccel's NoAutoloading flag: set the key now, not merely as a
    /// default for the next load.
    const SET_NOW: &str = "4";

    /// The action id every `org.kde.KGlobalAccel` call takes, in GVariant
    /// text: `[component, action, component friendly, action friendly]`.
    pub(super) fn action_id() -> String {
        format!("['{}', '{}', 'QuickSearch', 'QuickSearch']", COMPONENT, ACTION)
    }

    /// One `gdbus call` against the daemon. gdbus over qdbus because it
    /// takes GVariant text for the list arguments, which qdbus cannot spell.
    fn call(method: &str, args: &[&str]) -> Result<String, String> {
        let method = format!("org.kde.KGlobalAccel.{}", method);
        let mut argv = vec![
            "call",
            "--session",
            "--dest",
            "org.kde.kglobalaccel",
            "--object-path",
            "/kglobalaccel",
            "--method",
            &method,
        ];
        argv.extend(args);
        run("gdbus", &argv)
    }

    /// Where the launch target lives; the daemon looks here by name.
    pub(super) fn desktop_file() -> PathBuf {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".local/share")
            });
        data.join("kglobalaccel").join(COMPONENT)
    }

    /// `NoDisplay`: the entry exists to be launched by a key, not to appear
    /// in menus next to the real QuickSearch entry.
    pub(super) fn desktop_entry(command: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=QuickSearch\nNoDisplay=true\nExec={}\n",
            command
        )
    }

    pub(super) fn installed() -> bool {
        desktop_file().is_file()
    }

    pub(super) fn install(binding: &Binding, command: &str) -> Result<(), String> {
        let id = action_id();
        call("doRegister", &[&id])?;
        let keys = format!("[{}]", binding.qt_key_code());
        let reply = call("setShortcut", &[&id, &keys, SET_NOW])?;
        // The daemon answers with the keys now in force; ours missing means
        // it kept something else — the key is taken.
        if !reply_ints(&reply).contains(&binding.qt_key_code()) {
            let _ = call("unregister", &[COMPONENT, ACTION]);
            return Err(format!(
                "your desktop refused {} — probably already in use",
                binding
            ));
        }
        // The file last: it is the installed marker, so nothing marks this
        // installed until the key is actually in force.
        let path = desktop_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {}", dir.display(), e))?;
        }
        std::fs::write(&path, desktop_entry(command))
            .map_err(|e| format!("writing {}: {}", path.display(), e))?;
        // Executable, or KConfig refuses to trust the file's Exec line
        // ("not owned by root and executable flag not set") — the same bit
        // Plasma's own Shortcuts page sets on the files it creates here.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("marking {} executable: {}", path.display(), e))?;
        Ok(())
    }

    pub(super) fn remove() -> Result<(), String> {
        call("unregister", &[COMPONENT, ACTION])?;
        let path = desktop_file();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("deleting {}: {}", path.display(), e)),
        }
    }

    /// The integers out of a gdbus reply like `([100663366],)`. Wrong or
    /// hostile shapes yield fewer integers, never a panic.
    pub(super) fn reply_ints(reply: &str) -> Vec<u32> {
        reply
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
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

    /// What the KDE launch key runs: a well-formed desktop entry whose Exec
    /// is the toggle command, hidden from menus.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_kde_desktop_entry_launches_the_toggle() {
        let entry = kde::desktop_entry("/opt/qs/quicksearch --toggle");
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/opt/qs/quicksearch --toggle\n"));
        assert!(entry.contains("NoDisplay=true\n"));
        assert!(kde::desktop_file().ends_with("kglobalaccel/quicksearch-search.desktop"));
    }

    /// The GVariant action id every KGlobalAccel call names; `_launch` is
    /// the action that runs a `.desktop` component's Exec.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_kde_action_id_names_the_launch_action() {
        assert_eq!(
            kde::action_id(),
            "['quicksearch-search.desktop', '_launch', 'QuickSearch', 'QuickSearch']"
        );
    }

    /// gdbus replies, including hostile ones, must parse to integers or to
    /// nothing — never panic.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn gdbus_replies_parse_to_integers_or_nothing() {
        assert_eq!(kde::reply_ints("([100663366],)"), [100663366]);
        assert_eq!(kde::reply_ints("([1, 2],)"), [1, 2]);
        assert_eq!(kde::reply_ints("([],)"), Vec::<u32>::new());
        for garbage in ["", "(true,)", "nonsense", "([99999999999999999999],)"] {
            let _ = kde::reply_ints(garbage);
        }
    }

    /// A path with a space would otherwise split into a broken Exec line.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_toggle_command_survives_spaces_in_the_path() {
        // `toggle_command` reads the real exe path; both shapes it can
        // produce must parse back to program + one flag.
        let command = toggle_command();
        assert!(command.ends_with(" --toggle"), "{command}");
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
