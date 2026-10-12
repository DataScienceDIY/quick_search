//! Writing the system-wide search shortcut into the desktop's own keyboard
//! configuration, so "bind this command yourself" becomes one click. It goes
//! where the desktop's settings list and edit it (GNOME's custom shortcuts,
//! KDE's global shortcuts), so the binding stays the user's to inspect,
//! change or delete. Other desktops keep the manual flow in
//! `crate::settings_tab::shortcut_note`.
//!
//! On Windows the closed-app binding is the installer's Start-menu `.lnk`;
//! [`hotkey_changed`] keeps its hotkey in step with the in-app one.

use crate::hotkey::Binding;

/// Where a binding can be written. `Unsupported` hides the one-click button
/// and leaves the manual flow.
///
/// Off unix the two named desktops are unreachable ([`detect`] answers
/// `Unsupported`); the variants exist there only to keep one enum for every
/// platform.
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

/// Whether the binding this module writes is present. Asks the desktop, so
/// callers cache the answer. This and the two below split by platform like
/// [`detect`]: the desktop modules only exist on unix.
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

/// A successful install, and when the key starts answering. Off unix
/// nothing installs, so nothing constructs these.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Installed {
    Immediately,
    /// Durably written, but the desktop only picks it up at the next login
    /// (KDE with no `kbuildsycoca` on the PATH).
    AfterRelogin,
}

impl Installed {
    /// The line the Settings tab shows after the install.
    pub fn message(self) -> &'static str {
        match self {
            Installed::Immediately => "Added to your desktop's keyboard shortcuts.",
            Installed::AfterRelogin => {
                "Added to your desktop's keyboard shortcuts; the key starts \
                 answering after you next log in."
            }
        }
    }
}

/// Writes `binding` → `quicksearch --toggle` into the desktop's keyboard
/// configuration. Idempotent: a second install rewrites the same entry.
pub fn install(binding: &Binding) -> Result<Installed, String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let command = toggle_command(&crate::activate::command_name());
        match detect() {
            Desktop::Gnome => gnome::install(binding, &command).map(|()| Installed::Immediately),
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

/// The command line the key runs, with `exe` quoted if it holds a space:
/// GNOME's `command` key and a desktop file's `Exec` both split on
/// whitespace and honour double quotes.
#[cfg(all(unix, not(target_os = "macos")))]
fn toggle_command(exe: &str) -> String {
    if exe.contains(char::is_whitespace) {
        format!("\"{}\" --toggle", exe)
    } else {
        format!("{} --toggle", exe)
    }
}

/// Deletes the entry [`install`] wrote; a no-op if it is already gone.
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

/// Keeps the system-wide binding in step with a rebound in-app shortcut.
/// Best-effort: a failure is logged, not surfaced, because the change the
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

/// Runs a program to completion and fails loudly, with its stderr as the why.
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

/// Rebuilds KDE's service database with whichever `kbuildsycoca` exists;
/// false if neither ran.
#[cfg(all(unix, not(target_os = "macos")))]
fn rebuild_sycoca() -> bool {
    run("kbuildsycoca6", &[])
        .or_else(|_| run("kbuildsycoca5", &[]))
        .is_ok()
}

/// GNOME: a custom keybinding under a path of our own. The fixed path makes
/// install idempotent and removal exact, and the entry shows up in Settings
/// → Keyboard → Custom Shortcuts under the name given here.
#[cfg(all(unix, not(target_os = "macos")))]
mod gnome {
    use super::*;

    const LIST_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
    const LIST_KEY: &str = "custom-keybindings";
    const ENTRY_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
    const ENTRY_PATH: &str =
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
/// Accepts exactly what gsettings emits, single-quoted strings with `\'` and
/// `\\` escapes, and drops an unterminated string rather than guessing: a
/// path we misread would be written back verbatim into the user's
/// configuration.
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_string_list(raw: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut chars = raw.chars();
    // Each pass skips to an opening quote and reads to its closing one.
    while chars.by_ref().any(|c| c == '\'') {
        let mut path = String::new();
        loop {
            match chars.next() {
                Some('\'') => break,
                Some('\\') => path.extend(chars.next()),
                Some(c) => path.push(c),
                None => return paths,
            }
        }
        paths.push(path);
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

/// KDE: a desktop file carrying its own key, found through the service
/// database (per the kglobalacceld v6.6.5 source,
/// `globalshortcutsregistry.cpp`). Three other approaches failed in the field:
///
/// * DBus `doRegister`+`setShortcut` records a mapping but never arms it
///   (`getOrCreateComponent` skips `loadSettings`, the only path to the
///   grab), and the claim dies with the registering connection.
/// * Editing `kglobalshortcutsrc` around a daemon restart loses a race: KDE
///   clients DBus-activate the daemon back before the edit lands, and its
///   debounced `writeSettings` erases the entry.
/// * The daemon's startup scan of `~/.local/share/kglobalaccel/` skips
///   `NoDisplay=true` files.
///
/// What works, restart-free: `detectAppsWithShortcuts()` arms the `_launch`
/// of every application whose desktop file carries `X-KDE-Shortcuts=`, at
/// daemon startup and on every `KSycoca::databaseChanged`. So install writes
/// a hidden desktop entry holding the key into `~/.local/share/applications/`
/// and rebuilds the database; the daemon arms it live and again at every
/// login. The file is also the "installed" marker. DBus `unregister` disarms
/// live, so removal needs no database round trip to take effect.
#[cfg(all(unix, not(target_os = "macos")))]
mod kde {
    use super::*;
    use std::path::PathBuf;

    /// The service's storage id (its filename), which is also the
    /// component name every DBus query answers with.
    const COMPONENT: &str = "quicksearch-search.desktop";
    const ACTION: &str = "_launch";

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

    fn data_dir() -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join(".local/share")
            })
    }

    /// In the applications directory, the one place the service database
    /// indexes and the one `detectAppsWithShortcuts` queries.
    pub(super) fn desktop_file() -> PathBuf {
        data_dir().join("applications").join(COMPONENT)
    }

    /// Where an earlier build put the file; deleted on sight so upgrades
    /// leave one binding, not two.
    fn legacy_desktop_file() -> PathBuf {
        data_dir().join("kglobalaccel").join(COMPONENT)
    }

    /// `NoDisplay` keeps it out of menus next to the real QuickSearch entry
    /// (the shortcut detection reads it regardless); `X-KDE-Shortcuts` is the
    /// key itself, in QKeySequence text.
    pub(super) fn desktop_entry(command: &str, binding: &Binding) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=QuickSearch\nNoDisplay=true\n\
             Exec={}\nX-KDE-Shortcuts={}\n",
            command, binding
        )
    }

    pub(super) fn installed() -> bool {
        desktop_file().is_file() || legacy_desktop_file().is_file()
    }

    pub(super) fn install(binding: &Binding, command: &str) -> Result<super::Installed, String> {
        let code = binding.qt_key_code();
        evict_our_old_claims(code);
        // A key another application claims is reported before anything is
        // registered: the daemon records both claims and then delivers the
        // press to neither.
        if let Some(holder) = holders_of(code).iter().find(|h| !h.is_ours()) {
            return Err(format!(
                "{} is taken by \"{}\" ({}); free it under System Settings → \
                 Keyboard → Shortcuts, or pick a different combination",
                binding, holder.action_friendly, holder.component_friendly,
            ));
        }
        tear_down_previous();
        let path = write_desktop_file(command, binding)?;
        // The database rebuild tells the running daemon (see the
        // module docs); without the tool the file arms at the next login.
        if !rebuild_sycoca() {
            return Ok(super::Installed::AfterRelogin);
        }
        if !wait_until_armed(code) {
            let _ = std::fs::remove_file(&path);
            rebuild_sycoca();
            return Err(format!(
                "your desktop did not arm {}; if this keeps happening, add a \
                 shortcut for the command by hand in System Settings",
                binding
            ));
        }
        Ok(super::Installed::Immediately)
    }

    /// Unregisters every other claim of ours on the key, above all the
    /// portal shortcut ("Focus the QuickSearch search box") left by builds
    /// that registered in-app on Wayland: its persisted claim keeps the
    /// launch binding from firing. The daemon names the holders, and a
    /// foreign claim is never touched.
    fn evict_our_old_claims(code: u32) {
        for holder in holders_of(code) {
            if holder.is_ours() && holder.component != COMPONENT {
                let _ = call("unregister", &[&holder.component, &holder.action]);
            }
        }
    }

    /// Removes a previous install, so a re-install starts from nothing. The
    /// daemon's refresh only drops components whose file vanished and never
    /// re-reads a changed one, so installing over an existing binding would
    /// keep serving the old Exec line (fatal for an AppImage, whose old
    /// mount path died with the last run).
    fn tear_down_previous() {
        if desktop_file().is_file() || legacy_desktop_file().is_file() {
            let _ = call("unregister", &[COMPONENT, ACTION]);
            let _ = std::fs::remove_file(desktop_file());
            let _ = std::fs::remove_file(legacy_desktop_file());
            if rebuild_sycoca() {
                // Gone when getComponent stops answering; bounded, and a
                // timeout just falls through to the rebuild.
                for _ in 0..10 {
                    if call("getComponent", &[COMPONENT]).is_err() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        } else {
            // No marker file, but an earlier claim may still be registered.
            let _ = call("unregister", &[COMPONENT, ACTION]);
        }
    }

    /// Writes the desktop file: the whole registration, key included, and
    /// the installed marker. Executable, matching the trust rules KIO applies
    /// before launching what a desktop file names.
    fn write_desktop_file(command: &str, binding: &Binding) -> Result<PathBuf, String> {
        let path = desktop_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("creating {}: {}", dir.display(), e))?;
        }
        std::fs::write(&path, desktop_entry(command, binding))
            .map_err(|e| format!("writing {}: {}", path.display(), e))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("marking {} executable: {}", path.display(), e))?;
        Ok(path)
    }

    /// Whether the daemon's routing table sends `code` to us, the proof that
    /// the key is armed and not just recorded. Retried, since the
    /// database-changed signal reaches the daemon asynchronously.
    fn wait_until_armed(code: u32) -> bool {
        for _ in 0..20 {
            let armed = call("action", &[&code.to_string()])
                .map(|reply| parse_string_list(&reply).iter().any(|s| s == COMPONENT))
                .unwrap_or(false);
            if armed {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        false
    }

    pub(super) fn remove() -> Result<(), String> {
        // Disarms live; deleting the file and rebuilding the database keeps
        // the next login from re-detecting it.
        let _ = call("unregister", &[COMPONENT, ACTION]);
        for path in [desktop_file(), legacy_desktop_file()] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("deleting {}: {}", path.display(), e)),
            }
        }
        rebuild_sycoca();
        Ok(())
    }

    /// One current owner of a key, out of `getGlobalShortcutsByKey`.
    pub(super) struct Holder {
        pub(super) action: String,
        pub(super) action_friendly: String,
        pub(super) component: String,
        pub(super) component_friendly: String,
    }

    impl Holder {
        /// Whether this claim is QuickSearch's own, under any of the names a
        /// portal or an older build may have registered it as.
        pub(super) fn is_ours(&self) -> bool {
            [
                &self.action,
                &self.action_friendly,
                &self.component,
                &self.component_friendly,
            ]
            .iter()
            .any(|name| name.to_ascii_lowercase().contains("quicksearch"))
        }
    }

    fn holders_of(code: u32) -> Vec<Holder> {
        call("getGlobalShortcutsByKey", &[&code.to_string()])
            .map(|reply| parse_holders(&reply))
            .unwrap_or_default()
    }

    /// The holders out of a reply like
    /// `([('action', 'Action', 'comp', 'Component', 'default',
    /// 'Default Context', [100663366], @ai [])],)`: six strings then two int
    /// lists per tuple, in the daemon's own order (probed against Plasma
    /// 6.6). Short or hostile shapes yield fewer holders, never a panic.
    pub(super) fn parse_holders(reply: &str) -> Vec<Holder> {
        super::parse_string_list(reply)
            .chunks_exact(6)
            .map(|names| Holder {
                action: names[0].clone(),
                action_friendly: names[1].clone(),
                component: names[2].clone(),
                component_friendly: names[3].clone(),
            })
            .collect()
    }

}

/// Windows: the hotkey field of the Start-menu `.lnk` is the closed-app
/// binding (see `packaging/quicksearch.nsi`), rewritten through
/// `WScript.Shell`, whose `Hotkey` property takes exactly the `Ctrl+Shift+F`
/// spelling [`Binding`] displays.
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

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;

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
    #[test]
    fn escaped_quotes_round_trip() {
        let paths = vec!["/it's/".to_string(), "/back\\slash/".to_string()];
        let formatted = format_string_list(&paths);
        assert_eq!(parse_string_list(&formatted), paths);
    }

    /// Malicious or truncated gsettings output must never panic; at worst it
    /// yields fewer paths.
    #[test]
    fn garbage_lists_parse_to_something_harmless() {
        for garbage in ["", "[", "['unterminated", "not a list", "['a'", "\\"] {
            let _ = parse_string_list(garbage);
        }
    }

    /// What the KDE launch key runs: a well-formed desktop entry whose Exec
    /// is the toggle command, hidden from menus.
    #[test]
    fn the_kde_desktop_entry_launches_the_toggle() {
        let binding: Binding = "Ctrl+Shift+F".parse().unwrap();
        let entry = kde::desktop_entry("/opt/qs/quicksearch --toggle", &binding);
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/opt/qs/quicksearch --toggle\n"));
        assert!(entry.contains("NoDisplay=true\n"));
        // The key rides inside the file: it is what `detectAppsWithShortcuts`
        // arms, with no config entry and no daemon restart.
        assert!(entry.contains("X-KDE-Shortcuts=Ctrl+Shift+F\n"));
        // The applications dir, because that is the one the service database
        // indexes and the shortcut detection queries.
        assert!(kde::desktop_file().ends_with("applications/quicksearch-search.desktop"));
    }

    /// The holder tuples come out in the daemon's own field order (captured
    /// live from Plasma 6.6), and it is the portal's leftover entry that the
    /// ours-test must recognise.
    #[test]
    fn key_holders_parse_and_ours_is_recognised() {
        let reply = "([('search', 'Focus the QuickSearch search box', \
                     'some-portal-id', 'Portal App', 'default', \
                     'Default Context', [100663366], @ai [])],)";
        let holders = kde::parse_holders(reply);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].action, "search");
        assert_eq!(holders[0].component, "some-portal-id");
        assert!(holders[0].is_ours(), "the portal leftover was not recognised");

        let foreign = kde::parse_holders(
            "([('copy', 'Copy Screenshot', 'org.kde.spectacle.desktop', \
             'Spectacle', 'default', 'Default Context', [1], @ai [])],)",
        );
        assert!(!foreign[0].is_ours(), "a foreign claim must never be evicted");

        assert!(kde::parse_holders("(@a(ssssssaiai) [],)").is_empty());
        for garbage in ["", "([('a', 'b')],)", "no quotes at all"] {
            let _ = kde::parse_holders(garbage);
        }
    }

    /// Unquoted, a path with a space would split into a broken Exec line.
    #[test]
    fn the_toggle_command_quotes_a_path_with_a_space() {
        assert_eq!(
            toggle_command("/usr/bin/quicksearch"),
            "/usr/bin/quicksearch --toggle"
        );
        assert_eq!(
            toggle_command("/home/me/My Apps/QuickSearch.AppImage"),
            "\"/home/me/My Apps/QuickSearch.AppImage\" --toggle"
        );
    }
}
