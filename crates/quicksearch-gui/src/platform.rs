//! Opening and revealing files, plus the stdio setup that must happen
//! before anything prints.

use std::process::Command;

/// Gives the process somewhere to write when it has no stdio.
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

/// Opens a file with the desktop's default application. On unix the first
/// launcher that starts is reaped ([`spawn_reaped`]); Windows starts its
/// launcher detached, which leaves no zombie there.
pub fn open_file(path: &str) {
    #[cfg(unix)]
    {
        // The next launcher only when this one could not start, never on its
        // exit status: xdg-open runs the app in the foreground and returns
        // the app's status, so falling through on one would open it twice.
        let mut failure = None;
        for mut launcher in open::commands(path) {
            match spawn_reaped(&mut launcher) {
                Ok(_) => return,
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            quicksearch_core::log_warn!("open {}: {}", path, e);
        }
    }
    #[cfg(not(unix))]
    if let Err(e) = open::that_detached(path) {
        quicksearch_core::log_warn!("open {}: {}", path, e);
    }
}

/// `cmd` with no stdio and a process group of its own: what it launches
/// neither writes to the terminal we were started from nor receives its
/// Ctrl+C or Ctrl+Z.
#[cfg(unix)]
fn detached(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
}

/// Starts `cmd` [`detached`] and waits for it on a thread of its own, for as
/// long as it runs: a child nobody waits for stays a zombie for the life of
/// the app. Gives the child's pid. Should the thread not start, the child is
/// dropped unwaited.
#[cfg(unix)]
fn spawn_reaped(cmd: &mut Command) -> std::io::Result<u32> {
    let mut child = detached(cmd).spawn()?;
    let pid = child.id();
    let _ = std::thread::Builder::new()
        .name("quicksearch-reap".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

/// Reveals a file in the system file manager with the file selected.
///
/// Linux goes through `dbus-send` (no D-Bus library for one call), falling
/// back to opening the parent directory, both on a thread of their own: a
/// file manager that D-Bus has to start replies only once it is up, which
/// the UI thread must not wait for.
pub fn reveal_in_folder(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        /// Keeps a console window from flashing behind the spawn.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        // explorer.exe parses its own command line: `/select,` must be glued
        // to the path in one token with quotes around the path only, which
        // only `raw_arg` can express (as two arguments the selection is
        // silently ignored). Forward slashes are invalid here alone, and the
        // exit code is meaningless (explorer returns 1 on success).
        let native = path.replace('/', "\\");
        let _ = Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", native))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = spawn_reaped(Command::new("open").arg("-R").arg(path));
        return;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::path::Path;
        let items = format!("array:string:file://{}", uri_escape_path(path));
        let parent = Path::new(path)
            .parent()
            .unwrap_or(Path::new("/"))
            .to_owned();
        // `status` waits for each, so both are reaped where they ran.
        let _ = std::thread::Builder::new()
            .name("quicksearch-reveal".to_string())
            .spawn(move || {
                // `--print-reply` makes the exit status the reply's; its
                // printout goes nowhere.
                let shown = detached(Command::new("dbus-send").args([
                    "--session",
                    "--print-reply",
                    "--dest=org.freedesktop.FileManager1",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                    &items,
                    "string:",
                ]))
                .status()
                .is_ok_and(|status| status.success());
                if !shown {
                    let _ = detached(Command::new("xdg-open").arg(parent)).status();
                }
            });
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

/// A desktop's keyboard-shortcut settings application.
#[cfg(all(unix, not(target_os = "macos")))]
struct KeyboardSettings {
    program: &'static str,
    args: &'static [&'static str],
    /// The label of the button that opens it.
    label: &'static str,
}

/// The desktop's keyboard-shortcut settings, when this is a desktop whose
/// settings application we know how to name. A pair with
/// [`open_keyboard_settings`]: the Settings tab only offers the button when
/// there is something to open.
#[cfg(all(unix, not(target_os = "macos")))]
fn keyboard_settings_command() -> Option<KeyboardSettings> {
    keyboard_settings_for(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

/// The mapping itself, split from the environment so it can be tested
/// without one. `desktops` is `XDG_CURRENT_DESKTOP`: a colon-separated list,
/// most specific first, and matched case-insensitively because the spec does
/// not pin the case and desktops disagree in practice.
#[cfg(all(unix, not(target_os = "macos")))]
fn keyboard_settings_for(desktops: &str) -> Option<KeyboardSettings> {
    desktops.split(':').find_map(|desktop| {
        let (program, args, label): (_, &'static [&'static str], _) =
            match desktop.to_ascii_uppercase().as_str() {
                "KDE" => ("systemsettings", &["kcm_keys"], "Open Shortcuts settings"),
                "GNOME" | "UNITY" => (
                    "gnome-control-center",
                    &["keyboard"],
                    "Open Keyboard settings",
                ),
                "XFCE" => ("xfce4-keyboard-settings", &[], "Open Keyboard settings"),
                "CINNAMON" => ("cinnamon-settings", &["keyboard"], "Open Keyboard settings"),
                _ => return None,
            };
        Some(KeyboardSettings {
            program,
            args,
            label,
        })
    })
}

/// The button label for [`open_keyboard_settings`], or `None` when this
/// desktop has no settings application we can name.
pub fn keyboard_settings_label() -> Option<&'static str> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Naming it is not enough; it also has to be installed, or the button
        // would promise something that silently fails.
        let settings = keyboard_settings_command()?;
        which(settings.program).then_some(settings.label)
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

/// Open the desktop's keyboard settings, reaped ([`spawn_reaped`]), so the
/// user can bind `quicksearch --toggle` without hunting for the page.
pub fn open_keyboard_settings() {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(settings) = keyboard_settings_command() {
        if let Err(e) = spawn_reaped(Command::new(settings.program).args(settings.args)) {
            quicksearch_core::log_warn!("opening {}: {}", settings.program, e);
        }
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

    /// The first entry that is recognised wins, so a desktop that lists
    /// itself ahead of a generic fallback gets its own settings application.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_most_specific_desktop_wins() {
        let kde = super::keyboard_settings_for("KDE").expect("KDE is known");
        assert_eq!(kde.program, "systemsettings");
        assert_eq!(kde.args, ["kcm_keys"]);

        let gnome =
            super::keyboard_settings_for("ubuntu:GNOME").expect("GNOME after a vendor prefix");
        assert_eq!(gnome.program, "gnome-control-center");
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

    /// Whether `/proc/<pid>` is gone within `secs`: a zombie keeps its entry
    /// until it is waited for.
    #[cfg(target_os = "linux")]
    fn reaped_within(pid: u32, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if !std::path::Path::new(&format!("/proc/{}", pid)).exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// An exited launcher is waited for, not left a zombie for the life of
    /// the app.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_reaped_leaves_no_zombie() {
        let pid = super::spawn_reaped(&mut std::process::Command::new("true")).expect("spawn");
        assert!(reaped_within(pid, 2), "pid {} was never reaped", pid);
    }

    /// A launched app has no stdio of ours and leads its own process group,
    /// so the terminal's Ctrl+C never reaches it; it is reaped all the same
    /// once it exits.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_launched_app_has_no_terminal_and_its_own_group() {
        let pid = super::spawn_reaped(std::process::Command::new("sleep").arg("1")).expect("spawn");
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).expect("stat");
        // `pid (comm) state ppid pgrp ...`; comm is `sleep`, free of spaces.
        let pgrp = stat.split_whitespace().nth(4).expect("pgrp");
        assert_eq!(pgrp, pid.to_string(), "not its own process group");
        for fd in 0..3 {
            let target = std::fs::read_link(format!("/proc/{}/fd/{}", pid, fd)).expect("fd");
            assert_eq!(target, std::path::Path::new("/dev/null"), "fd {}", fd);
        }
        assert!(reaped_within(pid, 5), "pid {} was never reaped", pid);
    }
}
