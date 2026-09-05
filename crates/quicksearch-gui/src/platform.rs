//! Opening and revealing files, plus the stdio setup that must happen
//! before anything prints.

use std::process::Command;

/// Give the process somewhere to write when it has no stdio.
///
/// A window-subsystem binary launched from Explorer has NULL standard
/// handles, and `println!`/`eprintln!` *panic* on a failed write. Pointing
/// them at `NUL` makes the writes succeed and go nowhere. Handles inherited
/// from a real console are left alone.
#[cfg(windows)]
pub fn redirect_null_stdio() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let existing = unsafe { GetStdHandle(id) };
        if !existing.is_null() && existing != INVALID_HANDLE_VALUE {
            continue;
        }
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open("NUL") {
            // Leaked: it must outlive every later write.
            unsafe { SetStdHandle(id, file.into_raw_handle() as _) };
        }
    }
}

/// Open a file with the desktop's default application, detached.
pub fn open_file(path: &str) {
    if let Err(e) = open::that_detached(path) {
        quicksearch_core::log_warn!("open {}: {}", path, e);
    }
}

/// Reveal a file in the system file manager with the file selected.
///
/// Linux goes through `dbus-send` (no D-Bus library for one call), falling
/// back to opening the parent directory.
pub fn reveal_in_folder(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        /// Keeps a console window from flashing behind the spawn.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        // explorer.exe parses its own command line: `/select,` must be glued
        // to the path in one token with quotes around the path only, which
        // only `raw_arg` can express — as two arguments it silently ignores
        // the selection. Forward slashes are invalid here alone, and the exit
        // code is meaningless (explorer returns 1 on success).
        let native = path.replace('/', "\\");
        let _ = Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", native))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
        return;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::path::Path;
        let uri = format!("file://{}", uri_escape_path(path));
        let ok = Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{}", uri),
                "string:",
            ])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !ok {
            let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
            let _ = Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// Percent-encode a path for a file:// URI, keeping `/`.
#[cfg(all(unix, not(target_os = "macos")))]
fn uri_escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod keyboard_settings_tests {
    /// The first entry that is recognised wins, so a desktop that lists
    /// itself ahead of a generic fallback gets its own settings application.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_most_specific_desktop_wins() {
        let (program, args, _) = super::keyboard_settings_for("KDE").expect("KDE is known");
        assert_eq!(program, "systemsettings");
        assert_eq!(args, ["kcm_keys"]);

        let (program, _, _) =
            super::keyboard_settings_for("ubuntu:GNOME").expect("GNOME after a vendor prefix");
        assert_eq!(program, "gnome-control-center");
    }

    /// The spec does not pin the case and desktops disagree.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_match_ignores_case() {
        assert!(super::keyboard_settings_for("kde").is_some());
        assert!(super::keyboard_settings_for("Kde").is_some());
    }

    /// An unknown or absent desktop offers no button rather than a broken one.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_unknown_desktop_offers_nothing() {
        assert!(super::keyboard_settings_for("").is_none());
        assert!(super::keyboard_settings_for("i3:sway").is_none());
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn uri_escaping() {
        use super::uri_escape_path;
        assert_eq!(uri_escape_path("/plain/path.txt"), "/plain/path.txt");
        assert_eq!(
            uri_escape_path("/with space/ünïcode&.txt"),
            "/with%20space/%C3%BCn%C3%AFcode%26.txt"
        );
    }
}

/// The desktop's keyboard-shortcut settings, when this is a desktop whose
/// settings application we know how to name.
///
/// A pair with [`open_keyboard_settings`]: the Settings tab only offers the
/// button when there is something to open, rather than showing one that may
/// do nothing.
#[cfg(all(unix, not(target_os = "macos")))]
fn keyboard_settings_command() -> Option<(&'static str, &'static [&'static str], &'static str)> {
    keyboard_settings_for(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

/// The mapping itself, split from the environment so it can be tested
/// without one. `desktops` is `XDG_CURRENT_DESKTOP`: a colon-separated list,
/// most specific first, and matched case-insensitively because the spec does
/// not pin the case and desktops disagree in practice.
#[cfg(all(unix, not(target_os = "macos")))]
fn keyboard_settings_for(
    desktops: &str,
) -> Option<(&'static str, &'static [&'static str], &'static str)> {
    for desktop in desktops.split(':') {
        let found = match desktop.to_ascii_uppercase().as_str() {
            "KDE" => Some((
                "systemsettings",
                &["kcm_keys"][..],
                "Open Shortcuts settings",
            )),
            "GNOME" | "UNITY" => Some((
                "gnome-control-center",
                &["keyboard"][..],
                "Open Keyboard settings",
            )),
            "XFCE" => Some(("xfce4-keyboard-settings", &[][..], "Open Keyboard settings")),
            "CINNAMON" => Some((
                "cinnamon-settings",
                &["keyboard"][..],
                "Open Keyboard settings",
            )),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// The button label for [`open_keyboard_settings`], or `None` when this
/// desktop has no settings application we can name.
pub fn keyboard_settings_label() -> Option<&'static str> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Naming it is not enough; it also has to be installed, or the button
        // would promise something that silently fails.
        let (program, _, label) = keyboard_settings_command()?;
        which(program).then_some(label)
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        None
    }
}

/// Whether `program` is on PATH. `Command::spawn` would tell us, but only by
/// running it, and this decides whether to offer the button at all.
#[cfg(all(unix, not(target_os = "macos")))]
fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Open the desktop's keyboard settings, detached, so the user can bind
/// `quicksearch --toggle` without hunting for the page.
pub fn open_keyboard_settings() {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some((program, args, _)) = keyboard_settings_command() {
        if let Err(e) = Command::new(program).args(args).spawn() {
            quicksearch_core::log_warn!("opening {}: {}", program, e);
        }
    }
}
