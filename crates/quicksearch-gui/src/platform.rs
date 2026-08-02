//! Opening files, revealing them in the system file manager, and the one bit
//! of process setup that has to happen before anything prints.

use std::process::Command;

/// Give the process somewhere to write when it has no stdio.
///
/// A window-subsystem binary launched from Explorer has NULL standard handles,
/// and `println!`/`eprintln!` *panic* when the write fails rather than
/// dropping the output. Pointing the handles at `NUL` makes those writes
/// succeed and go nowhere.
///
/// Background reporting no longer depends on this — it goes through
/// [`quicksearch_core::log`], which ignores a failed stderr write and keeps
/// the line for the Logs tab — but the remaining direct prints (a startup
/// failure, a panic message) still reach a handle that accepts them.
///
/// Handles inherited from a real console are left alone, so running the binary
/// from a shell still prints normally.
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
            // Deliberately leaked: the handle has to outlive every later
            // write, which means the whole process.
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
/// Linux: `org.freedesktop.FileManager1.ShowItems` over the session bus
/// (supported by every mainstream file manager) via `dbus-send` — no
/// D-Bus library dependency for one call. Falls back to opening the
/// parent directory. Windows/macOS use their native select verbs.
pub fn reveal_in_folder(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        /// Keep a console window from flashing behind the spawn.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        // explorer.exe parses its own command line rather than using the
        // standard argv splitting, and wants `/select,` glued to the path as a
        // single token with quotes around the path only. Passed as two
        // arguments it ignores the selection and just opens the folder, and
        // std's quoting would wrap the whole token. `raw_arg` is the only way
        // to say exactly this.
        //
        // Forward slashes are valid everywhere else on Windows but not here,
        // so normalize first. The exit code is not worth checking: explorer
        // returns 1 even on success.
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

/// Percent-encode a filesystem path for a file:// URI, keeping `/`.
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
