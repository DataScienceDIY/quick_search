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
