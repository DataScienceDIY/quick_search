//! Bringing the running window forward from a second process.
//!
//! The desktop owns the search shortcut: the user binds it to
//! `quicksearch --toggle`, which hands the activation to a running instance
//! and exits, or becomes that instance when nothing is running. Only this
//! shape focuses the search box whether or not the app was started, because
//! a shortcut an application registers for itself cannot fire while the
//! application is not there.
//!
//! "Come forward" is nearly the whole protocol; what little each side says
//! is the permission the other needs. On unix the request line carries the
//! sender's `XDG_ACTIVATION_TOKEN` (usually empty), which on Wayland lets the
//! running window take focus (spent in [`raise`]'s `xdg_activation_v1`
//! path), and the reply lets the sender tell a live instance from a leftover
//! socket. On Windows the reply carries the server's PID instead, for the
//! sender's `AllowSetForegroundWindow` (see the `#[cfg(windows)]` module).

pub mod raise;

pub use raise::raise;

use std::path::Path;
#[cfg(any(unix, test))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag rather than a queue: two presses before the app can redraw mean
/// the same thing as one.
static PENDING: AtomicBool = AtomicBool::new(false);

/// The xdg-activation token that came with the pending press, when a
/// `--toggle` sender forwarded one (see the unix module). Read together with
/// [`take_pending`]; a later press's token simply replaces an unread one,
/// matching the flag's "two presses mean one" rule.
static TOKEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The most a PID reply can be: a 32-bit PID is at most ten digits, and the
/// newline ends it. Also the pipe's buffer size, so the server's write never
/// blocks on a client that reads nothing.
#[cfg_attr(not(windows), allow(dead_code))]
const PID_REPLY_CAP: usize = 16;

/// The longest either side waits on one read or write of a connection, on
/// both platforms (on Windows the sender's wait for the server to hang up
/// too): a peer that connects and stalls must not wedge the one thread that
/// answers every activation, and a `--toggle` must not hang on the keypress.
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How a [`signal`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A live instance took the activation.
    Delivered,
    /// Nothing serves the name, or not yet: start the GUI, or ask again.
    Absent,
    /// Something took the connection but did not finish the exchange within
    /// [`IO_TIMEOUT`]: a wedged instance, or another program on the name.
    /// Asking again would only wait the timeout out again.
    Unanswered,
}

/// The server's PID out of its reply: ASCII decimal up to a newline.
/// Anything else (truncation, garbage, an empty read) is `None`, which skips
/// the foreground grant rather than failing the activation.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_pid_reply(reply: &[u8]) -> Option<u32> {
    let line = reply.split(|&b| b == b'\n').next()?;
    std::str::from_utf8(line).ok()?.parse().ok()
}

/// The socket identifying the instance that `config_path` configures. Keyed
/// by the config file, not the index: the index path can change while
/// running (`IndexLock` moves with it), and a socket that moved too would
/// leave `--toggle` looking for the old one. Two GUIs under different configs
/// get different sockets, the only way two can legitimately run at once.
///
/// It lives in `$XDG_RUNTIME_DIR` when the session has one (tmpfs, private
/// to the session, cleared at logout), so a socket cannot outlive its login
/// or land on a read-only portable install; otherwise in [`runtime_dir`]'s
/// fallback. An error means neither is a directory we can trust.
#[cfg(unix)]
pub fn path_for(config_path: &Path) -> std::io::Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("quicksearch-{:016x}.sock", key(config_path))))
}

/// FNV-1a over the config path. Spelled out rather than taken from
/// `DefaultHasher`, whose output Rust does not promise to keep stable: the
/// `--toggle` process and the running one must agree on this name even when
/// an upgrade has left them different builds.
fn key(config_path: &Path) -> u64 {
    let bytes = config_path.as_os_str().as_encoded_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `$XDG_RUNTIME_DIR` when the session set one, trusted as the session's
/// own. Else `quicksearch-<uid>` under the temporary directory, which other
/// users share, so it is checked to be ours and 0700
/// ([`private_dir_under`](quicksearch_core::platform::private_dir_under)) and
/// refused otherwise: whoever owns it could answer `--toggle` in our place.
#[cfg(unix)]
fn runtime_dir() -> std::io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir);
        }
    }
    quicksearch_core::platform::private_dir_under(&std::env::temp_dir(), "quicksearch")
}

/// Takes the activation asked for since the last call.
pub fn take_pending() -> bool {
    PENDING.swap(false, Ordering::SeqCst)
}

/// The token belonging to the press [`take_pending`] just returned; call
/// right after it, on the same thread's handling of the same frame.
pub fn take_token() -> Option<String> {
    TOKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
}

/// What to tell the user (or a desktop binding) to run: the installed name
/// when we are on the path under it, and the full path otherwise (a build run
/// out of `target/`, where "quicksearch" would be wrong advice).
///
/// For an AppImage that path is the `.AppImage` itself
/// ([`launched_exe`](quicksearch_core::platform::launched_exe)): the binary
/// inside runs from a mount that vanishes with the process, so a binding made
/// from it would die when QuickSearch closes.
pub fn command_name() -> String {
    match quicksearch_core::platform::launched_exe() {
        Some(exe) => durable_command(&exe),
        None => "quicksearch".to_string(),
    }
}

/// The rule itself, split from the environment for the tests.
fn durable_command(exe: &Path) -> String {
    let installed = exe
        .parent()
        .is_some_and(|dir| matches!(dir.to_str(), Some("/usr/bin") | Some("/usr/local/bin")));
    if installed {
        exe.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("quicksearch")
            .to_string()
    } else {
        exe.display().to_string()
    }
}

/// Records an activation and wakes the window; without the repaint an idle
/// event loop would leave it unread until the user moved the mouse. Called by
/// the listener here and by `crate::hotkey`, so both paths look the same to
/// the UI.
pub(crate) fn fire(ctx: &egui::Context) {
    fire_with_token(ctx, None);
}

/// [`fire`], carrying the activation token a `--toggle` sender forwarded (on
/// Wayland, the compositor's permission to take focus).
pub(crate) fn fire_with_token(ctx: &egui::Context, token: Option<String>) {
    *TOKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = token;
    PENDING.store(true, Ordering::SeqCst);
    ctx.request_repaint();
}

#[cfg(unix)]
mod imp {
    use super::*;
    // The socket is the only reader and writer here; Windows uses the Win32
    // pipe calls rather than `std::io`.
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// The most a request line may be. Real xdg-activation tokens run some
    /// tens of characters; the cap keeps a hostile peer from feeding
    /// the listener forever.
    const REQUEST_CAP: usize = 1024;

    /// Asks the instance configured by `config_path` to come forward.
    /// [`Signal::Absent`] (a refused connection, or no socket at all) means
    /// the caller should start the GUI itself. A leftover socket file never
    /// counts as an instance, the rule `IndexLock` follows too, so a crash
    /// cannot strand the user behind a file nobody is listening on.
    ///
    /// The request carries this process's `XDG_ACTIVATION_TOKEN` when the
    /// launcher minted one; the running instance spends it on its own window
    /// (`raise`). Launched by hand there is none, and the line is empty.
    pub fn signal(config_path: &Path) -> Signal {
        let token = std::env::var("XDG_ACTIVATION_TOKEN").unwrap_or_default();
        signal_with_token(config_path, &token)
    }

    pub(super) fn signal_with_token(config_path: &Path, token: &str) -> Signal {
        // No trusted directory: nothing can be listening, and `listen`
        // reports why.
        let Ok(path) = path_for(config_path) else {
            return Signal::Absent;
        };
        let Ok(mut stream) = UnixStream::connect(path) else {
            return Signal::Absent;
        };
        // Bounded on both halves: a `--toggle` must never become a process
        // that hangs on the keypress, whatever is on the other end.
        if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
        {
            return Signal::Unanswered;
        }
        // One line is the whole request. A token that could not cross intact
        // (absurd length, a newline of its own) is dropped rather than sent
        // mangled: the press still lands, the raise just loses its token.
        let token = if token.len() < REQUEST_CAP && !token.contains('\n') {
            token
        } else {
            ""
        };
        if stream.write_all(token.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            return Signal::Unanswered;
        }
        // The reply distinguishes "delivered" from "wrote into a
        // socket nobody reads". EOF means the peer went away.
        let mut ack = [0u8; 1];
        match stream.read(&mut ack) {
            Ok(1) => Signal::Delivered,
            _ => Signal::Unanswered,
        }
    }

    /// Starts answering activations for the instance `config_path`
    /// configures. Call only while holding the index lock, or after a signal
    /// went unanswered: binding unlinks whatever is in the way, and either
    /// shows that no live instance under this config is listening on it.
    /// Failure is logged and otherwise ignored: a runtime directory that
    /// cannot host a socket is not a reason to refuse to open the window.
    pub fn listen(ctx: &egui::Context, config_path: &Path) {
        let path = match path_for(config_path) {
            Ok(path) => path,
            Err(e) => {
                quicksearch_core::log_warn!("the search shortcut has no socket directory: {}", e);
                return;
            }
        };
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                quicksearch_core::log_warn!(
                    "the search shortcut cannot listen on {}: {}",
                    path.display(),
                    e
                );
                return;
            }
        };
        // Ours alone: this socket is a way to make the window jump to the
        // front, and `bind` honours the umask rather than any mode we want.
        if let Err(e) = restrict(&path) {
            quicksearch_core::log_warn!("securing {}: {}", path.display(), e);
        }

        let ctx = ctx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("quicksearch-activate".to_string())
            .spawn(move || serve(&ctx, listener))
        {
            quicksearch_core::log_warn!("the search shortcut listener: {}", e);
        }
    }

    fn restrict(path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }

    /// Runs until the process exits; a connection is one activation.
    fn serve(ctx: &egui::Context, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                // Out of descriptors (EMFILE) and the like persist until
                // something else lets go; retrying at once would spin a core.
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            };
            match request(&mut stream) {
                // Fire *before* the ack: a client that saw the reply may act
                // on "delivered", and delivered means the window was already
                // asked to come forward.
                Ok(token) => {
                    fire_with_token(ctx, (!token.is_empty()).then_some(token));
                    let _ = acknowledge(&mut stream);
                }
                // One stalled or truncated peer must not stop the loop, and
                // must not raise the window on a request it never finished.
                Err(e) => quicksearch_core::log_warn!("a search shortcut request: {}", e),
            }
        }
    }

    /// Reads the request: one newline-terminated line carrying the sender's
    /// activation token, usually empty.
    ///
    /// Hostile input is the norm: any process of this user can connect. The read is bounded in bytes ([`REQUEST_CAP`])
    /// and time, so a peer that connects and stalls, or pours data without a
    /// newline, cannot wedge the one thread that answers every activation.
    pub(super) fn request(stream: &mut UnixStream) -> std::io::Result<String> {
        use std::io::{Error, ErrorKind};

        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        let mut buf = [0u8; REQUEST_CAP];
        let mut len = 0;
        loop {
            let n = stream.read(&mut buf[len..])?;
            if n == 0 {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            len += n;
            if let Some(newline) = buf[..len].iter().position(|&b| b == b'\n') {
                let token = std::str::from_utf8(&buf[..newline])
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                return Ok(token.to_string());
            }
            if len == buf.len() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "no newline within the request cap",
                ));
            }
        }
    }

    /// The reply that lets the sender tell a live instance from a leftover
    /// socket file.
    pub(super) fn acknowledge(stream: &mut UnixStream) -> std::io::Result<()> {
        stream.write_all(b"\n")?;
        stream.flush()
    }
}

/// The same handshake over a named pipe, which is what Windows has instead
/// of a unix socket, with one difference: the reply is the foreground grant,
/// and the server hanging up is the acknowledgement. A named pipe exists only
/// while a server holds it open, so there is no leftover to mistake for an
/// instance. What Windows needs is permission: `SetForegroundWindow` is
/// refused to a process the user did not just interact with, which the
/// running instance is. The `--toggle` process was launched by the user's
/// keypress, so it holds that right and may donate it with
/// `AllowSetForegroundWindow`, given the server's PID. The server opens every
/// connection by writing its PID, and the sender grants before sending the
/// request, so the grant is in force by the time the server raises.
#[cfg(windows)]
mod imp {
    use super::*;

    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, FALSE, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcessId, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    /// `\\.\pipe\quicksearch-<key>`, keyed exactly as the unix socket is, so
    /// the two processes agree by the same rule on both platforms.
    pub(super) fn pipe_name(config_path: &Path) -> Vec<u16> {
        let name = format!("\\\\.\\pipe\\quicksearch-{:016x}", key(config_path));
        std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Closes its handle however the scope ends, including on an early error.
    pub(super) struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a Create* call that returned success
            // and is closed exactly once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Asks the instance configured by `config_path` to come forward.
    /// [`Signal::Absent`] means no instance is serving the pipe and the
    /// caller should start the GUI itself. Every read and write, and the wait
    /// for the hang-up, is bounded by [`IO_TIMEOUT`]: whatever holds the
    /// well-known name, a squatter included, cannot hang a `--toggle`.
    pub fn signal(config_path: &Path) -> Signal {
        let name = pipe_name(config_path);
        let timeout = IO_TIMEOUT.as_millis() as u32;
        // A busy pipe means an instance is there but mid-handshake with
        // another `--toggle`; anything else means nobody is listening.
        for _ in 0..5 {
            // SAFETY: `name` is a NUL-terminated wide string that outlives
            // the call, and the two null pointers are documented as optional.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_NONE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    // Identification only: whatever answers on this
                    // well-known name may learn who we are, never act as us.
                    // Overlapped, so every call below can time out.
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                // SAFETY: reads this thread's last error, always valid.
                if unsafe { GetLastError() } == ERROR_PIPE_BUSY {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return Signal::Absent;
            }
            let pipe = Handle(handle);
            let Ok(event) = event() else {
                return Signal::Unanswered;
            };
            // The server opens with its PID; hand it the foreground right we
            // hold from the user's keypress *before* asking it to raise. A
            // reply that does not parse skips the grant, so the raise
            // degrades to a taskbar flash rather than the request being lost;
            // no reply at all is a server that is not answering.
            let mut reply = [0u8; PID_REPLY_CAP];
            // SAFETY: `reply` outlives the operation, which `overlapped` ends
            // before returning; the length passed is the buffer's real length.
            let got = overlapped(&pipe, &event, timeout, |ov| unsafe {
                ReadFile(
                    pipe.0,
                    reply.as_mut_ptr(),
                    reply.len() as u32,
                    std::ptr::null_mut(),
                    ov,
                )
            });
            let Ok(got) = got else {
                return Signal::Unanswered;
            };
            if let Some(pid) = parse_pid_reply(&reply[..got as usize]) {
                // SAFETY: no pointers; any PID value is acceptable input.
                unsafe { AllowSetForegroundWindow(pid) };
            }
            let request = [b'\n'];
            // SAFETY: as for the read, with a one-byte buffer.
            let sent = overlapped(&pipe, &event, timeout, |ov| unsafe {
                WriteFile(pipe.0, request.as_ptr(), 1, std::ptr::null_mut(), ov)
            });
            if sent != Ok(1) {
                return Signal::Unanswered;
            }
            // Delivered once the server hangs up, which it does right after
            // firing: a read that the disconnect fails. Not a flush, which
            // waits as long as the server cares not to read.
            let mut rest = [0u8; 1];
            // SAFETY: as for the read, with a one-byte buffer.
            let end = overlapped(&pipe, &event, timeout, |ov| unsafe {
                ReadFile(pipe.0, rest.as_mut_ptr(), 1, std::ptr::null_mut(), ov)
            });
            return match end {
                Err(ERROR_BROKEN_PIPE) | Err(ERROR_PIPE_NOT_CONNECTED) => Signal::Delivered,
                _ => Signal::Unanswered,
            };
        }
        Signal::Absent
    }

    /// Starts answering activations for the instance `config_path`
    /// configures. Failure is logged and otherwise ignored, as on unix.
    pub fn listen(ctx: &egui::Context, config_path: &Path) {
        let name = pipe_name(config_path);
        let ctx = ctx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("quicksearch-activate".to_string())
            .spawn(move || serve(&ctx, &name))
        {
            quicksearch_core::log_warn!("the search shortcut listener: {}", e);
        }
    }

    /// The one instance of the pipe this process serves, or the Win32 error.
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` fails when the name already exists, so
    /// a process that took it first is found out rather than sharing it with
    /// us, and [`serve`] holds this one instance for the life of the process,
    /// leaving no gap in which to take it later. Remote clients are refused.
    /// The null security descriptor is the default DACL, which does not keep
    /// the pipe to this user or session: the pipe namespace is machine-wide,
    /// and the default lets other local accounts open it for reading. All
    /// such a stranger can do is connect and stall, which the overlapped
    /// handle lets [`serve`] time out.
    pub(super) fn create_pipe(name: &[u16]) -> Result<Handle, u32> {
        // SAFETY: `name` is a NUL-terminated wide string borrowed for the
        // call, and a null security descriptor is documented as the default.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PID_REPLY_CAP as u32,
                PID_REPLY_CAP as u32,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: reads this thread's last error, always valid.
            return Err(unsafe { GetLastError() });
        }
        Ok(Handle(handle))
    }

    /// A manual-reset event for [`overlapped`] to wait on, or the Win32 error.
    fn event() -> Result<Handle, u32> {
        // SAFETY: both pointers are documented as optional; manual reset,
        // initially unset.
        let handle = unsafe { CreateEventW(std::ptr::null(), TRUE, FALSE, std::ptr::null()) };
        if handle.is_null() {
            // SAFETY: reads this thread's last error, always valid.
            return Err(unsafe { GetLastError() });
        }
        Ok(Handle(handle))
    }

    /// Runs one operation on `pipe`, which `start` issues against the
    /// `OVERLAPPED` it is handed, waiting at most `timeout_ms` (`INFINITE`
    /// for no limit) before cancelling it. Gives the bytes moved or the Win32
    /// error. The operation has ended, completed or cancelled, by the time
    /// this returns, so the kernel no longer writes the `OVERLAPPED` or any
    /// buffer `start` lent it.
    fn overlapped(
        pipe: &Handle,
        event: &Handle,
        timeout_ms: u32,
        start: impl FnOnce(*mut OVERLAPPED) -> BOOL,
    ) -> Result<u32, u32> {
        // SAFETY: `OVERLAPPED` is plain data, and all zeroes is its
        // documented initial state.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.0;
        if start(&mut ov) == 0 {
            // SAFETY: reads this thread's last error, always valid.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                // Nothing was started, so there is nothing to wait for.
                return Err(code);
            }
            // SAFETY: the event is open for the length of the call.
            if unsafe { WaitForSingleObject(event.0, timeout_ms) } != WAIT_OBJECT_0 {
                // SAFETY: `ov` is the pending operation's own.
                unsafe { CancelIoEx(pipe.0, &ov) };
            }
        }
        let mut moved = 0u32;
        // SAFETY: `ov` and `moved` are live; waiting (`TRUE`) returns only
        // once the operation has ended, so a cancelled one is reaped here.
        if unsafe { GetOverlappedResult(pipe.0, &ov, &mut moved, TRUE) } == 0 {
            // SAFETY: reads this thread's last error, always valid.
            return Err(unsafe { GetLastError() });
        }
        Ok(moved)
    }

    /// Runs until the process exits on the one instance [`create_pipe`]
    /// makes, disconnected and listened on again after every connection; a
    /// connection is one activation.
    fn serve(ctx: &egui::Context, name: &[u16]) {
        let (pipe, event) = match (create_pipe(name), event()) {
            (Ok(pipe), Ok(event)) => (pipe, event),
            (Err(code), _) | (_, Err(code)) => {
                quicksearch_core::log_warn!("the search shortcut pipe: error {}", code);
                return;
            }
        };
        let timeout = IO_TIMEOUT.as_millis() as u32;
        loop {
            // SAFETY: the pipe is open, and `overlapped` hands over a live
            // `OVERLAPPED`.
            let connected = overlapped(&pipe, &event, INFINITE, |ov| unsafe {
                ConnectNamedPipe(pipe.0, ov)
            });
            match connected {
                // A client that connected before we asked, or that connected
                // and already left; the write and read below tell which.
                Ok(_) | Err(ERROR_PIPE_CONNECTED) | Err(ERROR_NO_DATA) => {}
                Err(code) => {
                    quicksearch_core::log_warn!("the search shortcut pipe: error {}", code);
                    return;
                }
            }

            // Our PID first, before reading anything: the client turns it
            // into a foreground grant and only then sends the request, so
            // the grant precedes the raise however the two threads interleave.
            // Not flushed: a flush waits until the client has read, forever
            // for one that never does. A failed write needs no handling
            // either: the read below then fails or times out.
            // SAFETY: reads this process's own id, always valid.
            let pid = format!("{}\n", unsafe { GetCurrentProcessId() });
            // SAFETY: `pid` outlives the operation, which `overlapped` ends
            // before returning; the length passed is the buffer's real length.
            let _ = overlapped(&pipe, &event, timeout, |ov| unsafe {
                WriteFile(
                    pipe.0,
                    pid.as_ptr(),
                    pid.len() as u32,
                    std::ptr::null_mut(),
                    ov,
                )
            });

            // Bounded in time, as on unix: any local account can open the
            // pipe, and one that connects and says nothing is cut off here
            // rather than wedging the one thread that answers every
            // activation. Nor is that an activation.
            let mut request = [0u8; 1];
            // SAFETY: as for the write, with a one-byte buffer.
            let read = overlapped(&pipe, &event, timeout, |ov| unsafe {
                ReadFile(pipe.0, request.as_mut_ptr(), 1, std::ptr::null_mut(), ov)
            });
            // Fired before the disconnect, so a client watching the pipe
            // close can already rely on the window having been asked.
            if read == Ok(1) {
                fire(ctx);
            }
            // SAFETY: the pipe is open, and nothing is pending on it.
            unsafe { DisconnectNamedPipe(pipe.0) };
        }
    }
}

pub use imp::{listen, signal};

#[cfg(test)]
mod tests {
    use super::*;

    /// `PENDING` is process-wide and these tests run in parallel, so the ones
    /// that read it must not overlap: without this, one test's `take_pending`
    /// consumes the flag another just set and both look correct in isolation
    /// while failing together perhaps one run in fifty.
    static PENDING_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Ignore poisoning: a failed test elsewhere must not cascade into
    /// every other test that touches the flag.
    fn pending_guard() -> std::sync::MutexGuard<'static, ()> {
        PENDING_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The name must be a function of the config path alone, so the
    /// `--toggle` process and the running one always agree on it.
    #[cfg(unix)]
    #[test]
    fn one_config_always_names_one_socket() {
        let config = Path::new("/home/u/.config/quicksearch/config.toml");
        assert_eq!(path_for(config).unwrap(), path_for(config).unwrap());
        assert_ne!(
            path_for(config).unwrap(),
            path_for(Path::new("/elsewhere.toml")).unwrap()
        );
    }

    /// On the path, the bare name; anywhere else, the full path. An
    /// AppImage arrives here as the image's own path, not its mount's
    /// (`launched_exe`, tested in core).
    #[test]
    fn the_bound_command_is_the_installed_name_or_the_full_path() {
        assert_eq!(
            durable_command(Path::new("/usr/bin/quicksearch")),
            "quicksearch"
        );
        assert_eq!(
            durable_command(Path::new("/usr/local/bin/quicksearch")),
            "quicksearch"
        );
        for elsewhere in [
            "/opt/qs/quicksearch",
            "/home/u/Apps/QuickSearch.AppImage",
            "/usr/bin/sub/quicksearch",
        ] {
            assert_eq!(durable_command(Path::new(elsewhere)), elsewhere);
        }
    }

    /// The Windows reply parser, which faces whatever a squatting process
    /// cares to write into the well-known pipe name.
    #[test]
    fn the_pid_reply_parses_strictly() {
        assert_eq!(parse_pid_reply(b"12345\n"), Some(12345));
        assert_eq!(parse_pid_reply(b"1\nrest ignored"), Some(1));
        for garbage in [
            &b""[..],
            b"\n",
            b"-4\n",
            b"12345678901234567890\n", // overflows a u32
            b"abc\n",
            b"12 34\n",
            b"\xff\xfe\n",
        ] {
            assert_eq!(parse_pid_reply(garbage), None, "{:?}", garbage);
        }
    }

    #[test]
    fn a_pending_activation_is_consumed_once() {
        let _serial = pending_guard();
        PENDING.store(true, Ordering::SeqCst);
        assert!(take_pending());
        assert!(!take_pending(), "the flag is consumed");
    }

    /// The token rides the flag: consumed with it, and a fresh press without
    /// one clears a stale token rather than resurrecting it.
    #[test]
    fn the_token_is_consumed_with_the_press() {
        let _serial = pending_guard();
        let ctx = egui::Context::default();
        fire_with_token(&ctx, Some("tok".to_string()));
        assert!(take_pending());
        assert_eq!(take_token(), Some("tok".to_string()));
        assert_eq!(take_token(), None, "the token is consumed");

        fire_with_token(&ctx, Some("stale".to_string()));
        fire(&ctx);
        assert!(take_pending());
        assert_eq!(take_token(), None, "a tokenless press cleared the older token");
    }

    /// A config path unique to this test, so the sockets these bind never
    /// collide: they run on one process, in parallel.
    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let path = PathBuf::from(format!("/qs-activate-{}-{}.toml", name, std::process::id()));
        let _ = std::fs::remove_file(path_for(&path).unwrap());
        path
    }

    /// The next connection to `listener`.
    #[cfg(unix)]
    fn accept(listener: &std::os::unix::net::UnixListener) -> std::os::unix::net::UnixStream {
        listener
            .incoming()
            .next()
            .expect("a connection")
            .expect("accepted")
    }

    /// Nothing is listening on a path that was never bound.
    #[cfg(unix)]
    #[test]
    fn signalling_nothing_reports_nothing() {
        assert_eq!(signal(&scratch("absent")), Signal::Absent);
    }

    /// A leftover socket file with no listener must read as "not running",
    /// or a crash would strand the user behind a file nobody answers.
    #[cfg(unix)]
    #[test]
    fn a_stale_socket_file_is_not_an_instance() {
        let db = scratch("stale");
        std::fs::write(path_for(&db).unwrap(), b"").expect("leftover file");
        assert_eq!(signal(&db), Signal::Absent);
    }

    /// The whole point, end to end: a signal to a live listener is accepted,
    /// the listener sees a well-formed request, and the activation token it
    /// carries crosses intact.
    #[cfg(unix)]
    #[test]
    fn a_signal_crosses_the_socket() {
        use std::os::unix::net::UnixListener;

        let db = scratch("roundtrip");
        let listener = UnixListener::bind(path_for(&db).unwrap()).expect("bind");

        let sender =
            std::thread::spawn(move || imp::signal_with_token(&db, "wayland-token-123"));

        let mut stream = accept(&listener);
        let token = imp::request(&mut stream).expect("a well-formed request");
        assert_eq!(token, "wayland-token-123");
        imp::acknowledge(&mut stream).expect("acknowledged");

        assert_eq!(
            sender.join().expect("sender"),
            Signal::Delivered,
            "the client saw the reply"
        );
    }

    /// A token that cannot cross as one line is dropped, not sent mangled:
    /// the press still lands and only the raise loses its token.
    #[cfg(unix)]
    #[test]
    fn an_unsendable_token_degrades_to_an_empty_request() {
        use std::os::unix::net::UnixListener;

        for bad in ["with\nnewline".to_string(), "x".repeat(5000)] {
            let db = scratch("badtoken");
            let listener = UnixListener::bind(path_for(&db).unwrap()).expect("bind");
            let sender = std::thread::spawn(move || imp::signal_with_token(&db, &bad));
            let mut stream = accept(&listener);
            assert_eq!(imp::request(&mut stream).expect("well-formed"), "");
            imp::acknowledge(&mut stream).expect("acknowledged");
            assert_eq!(sender.join().expect("sender"), Signal::Delivered);
        }
    }

    /// A listener that takes the connection and never replies (a wedged
    /// instance) is given up on after [`IO_TIMEOUT`], and reported as such,
    /// so the caller does not ask again.
    #[cfg(unix)]
    #[test]
    fn a_listener_that_never_replies_is_unanswered() {
        use std::os::unix::net::UnixListener;

        let db = scratch("wedged");
        // Bound and never accepted: the kernel queues the connection, so the
        // sender connects and writes, and its read then waits.
        let _listener = UnixListener::bind(path_for(&db).unwrap()).expect("bind");
        let started = std::time::Instant::now();
        assert_eq!(signal(&db), Signal::Unanswered);
        // Timed out, not failed at once, and not waited on twice.
        let waited = started.elapsed();
        assert!(
            waited >= IO_TIMEOUT / 2 && waited < 2 * IO_TIMEOUT,
            "waited {waited:?}"
        );
    }

    /// A peer pouring bytes with no newline is cut off at the cap rather
    /// than fed forever.
    #[cfg(unix)]
    #[test]
    fn a_newline_less_flood_is_refused_at_the_cap() {
        use std::io::Write;
        use std::os::unix::net::{UnixListener, UnixStream};

        let db = scratch("flood");
        let path = path_for(&db).unwrap();
        let listener = UnixListener::bind(&path).expect("bind");
        let mut peer = UnixStream::connect(&path).expect("connect");
        std::thread::spawn(move || {
            // More than the cap, never a newline; ignore the write error the
            // refusal causes.
            let _ = peer.write_all(&[b'x'; 4096]);
        });
        let mut stream = accept(&listener);
        assert!(
            imp::request(&mut stream).is_err(),
            "a capless request was accepted"
        );
    }

    /// `listen` must replace a crashed predecessor's socket rather than
    /// giving up on it.
    #[cfg(unix)]
    #[test]
    fn binding_replaces_a_leftover_socket() {
        let _serial = pending_guard();
        let db = scratch("rebind");
        let path = path_for(&db).unwrap();
        std::fs::write(&path, b"").expect("leftover file");

        let ctx = egui::Context::default();
        listen(&ctx, &db);

        // Proof the listener took the path over: a signal now lands.
        assert_eq!(signal(&db), Signal::Delivered, "the new listener answers");
        assert!(take_pending(), "and the window was asked to come forward");
    }

    /// A peer that connects and says nothing must not wedge the listener,
    /// nor count as an activation.
    #[cfg(unix)]
    #[test]
    fn a_silent_peer_does_not_raise_the_window() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let db = scratch("silent");
        let path = path_for(&db).unwrap();
        let listener = UnixListener::bind(&path).expect("bind");

        // Connect, send nothing, close: `read_exact` sees EOF.
        let peer = UnixStream::connect(&path).expect("connect");
        drop(peer);

        let mut stream = accept(&listener);
        assert!(
            imp::request(&mut stream).is_err(),
            "an empty request is refused"
        );
    }

    /// A config path unique to this test. Only hashed into a pipe name, never
    /// opened, so it need not exist.
    #[cfg(windows)]
    fn scratch(name: &str) -> PathBuf {
        PathBuf::from(format!(
            "C:\\qs-activate-{}-{}.toml",
            name,
            std::process::id()
        ))
    }

    /// The pipe name must be a function of the config path alone, exactly as
    /// the socket name is, or the two processes would not meet.
    #[cfg(windows)]
    #[test]
    fn one_config_always_names_one_pipe() {
        let a = imp::pipe_name(Path::new("C:\\a\\config.toml"));
        assert_eq!(a, imp::pipe_name(Path::new("C:\\a\\config.toml")));
        assert_ne!(a, imp::pipe_name(Path::new("C:\\b\\config.toml")));
        // NUL-terminated, as every wide Win32 string argument must be.
        assert_eq!(a.last(), Some(&0));
    }

    /// Nothing is serving a pipe nobody created. On Windows this is the
    /// whole liveness test: a pipe cannot outlive the process serving it, so
    /// there is no stale-file case to cover as there is on unix.
    #[cfg(windows)]
    #[test]
    fn signalling_nothing_reports_nothing() {
        assert_eq!(signal(&scratch("absent")), Signal::Absent);
    }

    /// [`signal`] to a listener just started: its thread has to reach
    /// `create_pipe` first, so retry rather than sleep a guessed amount.
    #[cfg(windows)]
    fn signal_once_up(config: &Path) -> bool {
        for _ in 0..100 {
            if signal(config) == Signal::Delivered {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// End to end: `listen` serves the pipe and a signal reaches it. The
    /// sender returns only once the server has hung up, which it does after
    /// firing, so the flag is already set.
    #[cfg(windows)]
    #[test]
    fn a_signal_crosses_the_pipe() {
        let _serial = pending_guard();
        let config = scratch("roundtrip");
        let ctx = egui::Context::default();
        listen(&ctx, &config);

        assert!(signal_once_up(&config), "the listener never answered");
        assert!(take_pending(), "the window was never asked to come forward");
    }

    /// An instance of `name` made as a squatter would make it: any number of
    /// instances, none of them claiming to be the first. Blocking unless
    /// `flags` says otherwise.
    #[cfg(windows)]
    fn rival_pipe(name: &[u16], flags: u32) -> windows_sys::Win32::Foundation::HANDLE {
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        };
        // SAFETY: `name` is a NUL-terminated wide string borrowed for the
        // call, and a null security descriptor is documented as the default.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PID_REPLY_CAP as u32,
                PID_REPLY_CAP as u32,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            handle,
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            "the rival instance"
        );
        handle
    }

    /// The name is taken once: an instance made under a name that already
    /// exists is refused, so a process that squatted the name first is found
    /// out (`serve` logs and stops) rather than sharing the pipe with us. The
    /// squatter allows any number of instances, so only our first-instance
    /// flag can be what refuses.
    #[cfg(windows)]
    #[test]
    fn a_squatted_pipe_name_is_not_served() {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let name = imp::pipe_name(&scratch("squatted"));
        let squatter = rival_pipe(&name, FILE_FLAG_OVERLAPPED);
        // The name takes more instances: a shared one would be no refusal.
        let shared = rival_pipe(&name, FILE_FLAG_OVERLAPPED);
        assert_eq!(imp::create_pipe(&name).err(), Some(ERROR_ACCESS_DENIED));
        // SAFETY: both came from a successful create and are closed once.
        unsafe {
            CloseHandle(shared);
            CloseHandle(squatter);
        }
    }

    /// A squatter on the name that takes the connection and then stalls,
    /// before its PID or after the request, is given up on after
    /// [`IO_TIMEOUT`] rather than hanging the `--toggle`, and reported as
    /// such, so the caller does not ask again.
    #[cfg(windows)]
    #[test]
    fn a_squatter_that_stalls_is_unanswered() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;

        for (sends_pid, tag) in [(false, "stall-early"), (true, "stall-late")] {
            let config = scratch(tag);
            let name = imp::pipe_name(&config);
            let (up, ready) = std::sync::mpsc::channel();
            let squatter = std::thread::spawn(move || {
                let pipe = rival_pipe(&name, 0);
                up.send(()).expect("the test waits");
                let mut moved = 0u32;
                // SAFETY: the pipe is open and blocking, and every buffer and
                // output slot is live for its call.
                unsafe {
                    ConnectNamedPipe(pipe, std::ptr::null_mut());
                    if sends_pid {
                        // Not a PID, so the sender grants nothing to anyone.
                        WriteFile(pipe, b"x\n".as_ptr(), 2, &mut moved, std::ptr::null_mut());
                    }
                    // Read the request, then hold on until the sender gives up.
                    let mut buf = [0u8; PID_REPLY_CAP];
                    while ReadFile(
                        pipe,
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        &mut moved,
                        std::ptr::null_mut(),
                    ) != 0
                    {}
                    CloseHandle(pipe);
                }
            });
            ready.recv().expect("the squatter is up");

            let started = std::time::Instant::now();
            assert_eq!(signal(&config), Signal::Unanswered, "sends_pid {sends_pid}");
            let waited = started.elapsed();
            assert!(
                waited >= IO_TIMEOUT / 2 && waited < 2 * IO_TIMEOUT,
                "sends_pid {sends_pid}: waited {waited:?}"
            );
            squatter.join().expect("the squatter");
        }
    }

    /// The one instance answers press after press: disconnected and
    /// listened on again after each, never dropped and re-created.
    #[cfg(windows)]
    #[test]
    fn a_second_activation_reuses_the_instance() {
        let _serial = pending_guard();
        let config = scratch("reused");
        let ctx = egui::Context::default();
        listen(&ctx, &config);

        assert!(signal_once_up(&config), "the listener never answered");
        assert!(take_pending(), "the first press never fired");
        assert_eq!(
            signal(&config),
            Signal::Delivered,
            "the second press found no instance"
        );
        assert!(take_pending(), "the second press never fired");
    }
}
