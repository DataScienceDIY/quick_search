//! Fast subtree entry counting for progress denominators: `find | wc -l`
//! on Unix, raw directory enumeration on Windows.

#[cfg(unix)]
use std::process::{Command, Stdio};

#[cfg(unix)]
fn parse_wc_l_stdout(bytes: &[u8]) -> Result<usize, String> {
    let s = String::from_utf8_lossy(bytes);
    let token = s
        .split_whitespace()
        .next()
        .ok_or_else(|| "wc: empty output".to_string())?;
    token
        .parse()
        .map_err(|e| format!("wc: invalid count {:?}: {}", token, e))
}

/// Poll interval while waiting on count subprocesses; the granularity of
/// cancellation.
#[cfg(unix)]
const COUNT_POLL_MS: u64 = 50;

/// Wait for `terminal` (the last process in the pipeline) while honouring
/// `cancel`: on cancellation every process in `children` is killed and a
/// recognizable error is returned. On normal exit, returns the terminal
/// child's stdout.
#[cfg(unix)]
fn wait_pipeline_cancellable(
    children: &mut [&mut std::process::Child],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>, String> {
    use std::sync::atomic::Ordering;
    loop {
        if cancel.load(Ordering::Relaxed) {
            for child in children.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err("count cancelled".to_string());
        }
        let Some(terminal) = children.last_mut() else {
            return Err("count pipeline spawned no processes".to_string());
        };
        match terminal.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(format!("count pipeline exited with {}", status));
                }
                // The terminal child's output is a couple dozen bytes
                // (a `wc -l` figure), so reading after exit can't deadlock.
                let mut out = Vec::new();
                if let Some(stdout) = terminal.stdout.take() {
                    use std::io::Read;
                    let mut stdout = stdout;
                    let _ = stdout.read_to_end(&mut out);
                }
                // Reap the rest of the pipeline.
                for child in children.iter_mut() {
                    let _ = child.wait();
                }
                return Ok(out);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(COUNT_POLL_MS)),
            Err(e) => return Err(format!("count wait: {}", e)),
        }
    }
}

#[cfg(unix)]
fn count_find_pipe_wc(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
    printf_newlines: bool,
) -> Result<usize, String> {
    let mut find_cmd = Command::new("find");
    find_cmd.arg(path);
    if printf_newlines {
        // GNU find: emit one newline per entry without formatting paths.
        find_cmd.arg("-printf").arg("\n");
    }
    let mut find = find_cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("find: {}", e))?;
    let find_stdout = find.stdout.take().ok_or("find: stdout")?;
    let mut wc = match Command::new("wc")
        .arg("-l")
        .stdin(find_stdout)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(wc) => wc,
        Err(e) => {
            let _ = find.kill();
            let _ = find.wait();
            return Err(format!("wc: {}", e));
        }
    };
    let out = wait_pipeline_cancellable(&mut [&mut find, &mut wc], cancel)?;
    parse_wc_l_stdout(&out)
}

/// Count tree entries with a plain directory walk — the oracle
/// [`count_tree_entries_win32`] is tested against.
#[cfg(windows)]
fn count_tree_entries_walkdir(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    use std::sync::atomic::Ordering;

    let mut n = 0usize;
    // Unreadable subtrees are skipped rather than fatal: this is a progress
    // estimate, and the walk proper reports what it could not read.
    for entry in WalkDir::new(path).min_depth(1) {
        if cancel.load(Ordering::Relaxed) {
            return Err("count cancelled".to_string());
        }
        if entry.is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// Count tree entries by reading directories in bulk through
/// `GetFileInformationByHandleEx`.
///
/// `std::fs::read_dir` issues one `FindNextFileW` syscall per entry and
/// allocates a `PathBuf` for each; `FileIdBothDirectoryInfo` fills a
/// caller-supplied buffer with as many chained records as fit — roughly one
/// syscall per 64 KiB of directory data and one allocation per directory.
///
/// Recursion is explicit and iterative so a deep tree cannot exhaust the
/// thread's stack.
#[cfg(windows)]
fn count_tree_entries_win32(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileIdBothDirectoryInfo, GetFileInformationByHandleEx,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    /// One call returns as many entries as fit here.
    const BUFFER_BYTES: usize = 64 * 1024;

    /// `FILE_ATTRIBUTE_DIRECTORY`, const-asserted against the real header
    /// below.
    const ATTR_DIRECTORY: u32 = 0x10;
    const _: () = assert!(
        ATTR_DIRECTORY == windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
    );

    fn wide(path: &std::path::Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let mut pending = vec![std::path::PathBuf::from(path)];
    let mut count = 0usize;
    // One allocation for the whole walk, reused for every directory.
    let mut buffer = vec![0u8; BUFFER_BYTES];

    while let Some(dir) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err("count cancelled".to_string());
        }

        // FILE_FLAG_BACKUP_SEMANTICS is what makes CreateFileW open a
        // directory rather than fail; the share flags let the tree keep being
        // used while we count it.
        let handle = unsafe {
            CreateFileW(
                wide(&dir).as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                // hTemplateFile: a HANDLE, which windows-sys spells `isize`.
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // An unreadable directory costs its own entries, not the tree's.
            continue;
        }

        loop {
            // Returns zero both on error and at the end of the listing; either
            // way this directory is done.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileIdBothDirectoryInfo,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                )
            };
            if ok == 0 {
                break;
            }

            let mut offset = 0usize;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    unsafe { CloseHandle(handle) };
                    return Err("count cancelled".to_string());
                }
                // SAFETY: the API filled `buffer` with a chain of these
                // records, each at the offset the previous one gave.
                let info =
                    unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_ID_BOTH_DIR_INFO) };

                // FileNameLength is in bytes; FileName is UTF-16 and is *not*
                // NUL-terminated, so the length is the only thing that says
                // where it ends.
                let name_units = (info.FileNameLength as usize) / 2;
                let name =
                    unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), name_units) };
                let name = OsString::from_wide(name);

                // "." and ".." are entries of the listing, not of the tree.
                let is_dot = name == "." || name == "..";
                if !is_dot {
                    count += 1;
                    if info.FileAttributes & ATTR_DIRECTORY != 0 {
                        pending.push(dir.join(&name));
                    }
                }

                match info.NextEntryOffset {
                    0 => break,
                    next => offset += next as usize,
                }
            }
        }
        unsafe { CloseHandle(handle) };
    }
    Ok(count)
}

/// Rough tree entry count for progress totals. Setting `cancel` stops it
/// rather than letting it scan an entire root after the run stopped: on Unix
/// that kills the `find`/`wc` subprocesses at ~50 ms granularity, on Windows
/// the bulk directory read notices almost immediately. Runs concurrently with
/// indexing — its scope is not identical to the walker's classified file
/// count.
pub fn count_tree_entries_fast(
    path: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    #[cfg(windows)]
    {
        return count_tree_entries_win32(path, cancel);
    }
    #[cfg(all(unix, target_os = "linux"))]
    {
        count_find_pipe_wc(path, cancel, true).or_else(|e| {
            if e.contains("cancelled") {
                Err(e)
            } else {
                // Non-GNU find without -printf: plain listing.
                count_find_pipe_wc(path, cancel, false)
            }
        })
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        return count_find_pipe_wc(path, cancel, false);
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("tree entry count is not supported on this target".to_string())
    }
}
