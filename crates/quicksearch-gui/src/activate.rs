//! Bringing the running window forward from a second process.
//!
//! The desktop owns the search shortcut: the user binds it to
//! `quicksearch --toggle`, which either hands the activation to an instance
//! that is already running and exits, or — when nothing is running — becomes
//! that instance. This is the only shape that can satisfy "focus the search
//! box whether or not the app was started", because a shortcut an application
//! registers for itself cannot fire while the application is not there.
//!
//! The message carries nothing: "come forward" is the whole protocol. On
//! unix the reply exists only so the sender can tell a live instance from a
//! leftover socket; on Windows it instead carries the server's PID, which
//! the sender feeds to `AllowSetForegroundWindow` so the running window may
//! actually take the foreground — see the `#[cfg(windows)]` module below.
//! An xdg-activation token would be the natural thing to
//! carry — it is what a compositor wants before letting a background client
//! take focus — but nothing downstream can consume one: winit 0.30 applies a
//! token only in `WindowAttributes`, and egui's `ViewportBuilder` has no
//! field for it, so eframe never plumbs one either. See [`raise`] for what
//! that costs on Wayland.

pub mod raise;

pub use raise::raise;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag rather than a queue: two presses before the app can redraw mean
/// the same thing as one.
static PENDING: AtomicBool = AtomicBool::new(false);

/// The most a PID reply can be: a 32-bit PID is at most ten digits, and the
/// newline ends it. Also the pipe's buffer size, so the server's write never
/// blocks on a client that reads nothing.
#[cfg_attr(not(windows), allow(dead_code))]
const PID_REPLY_CAP: usize = 16;

/// The server's PID out of its reply: ASCII decimal up to a newline.
/// Anything else — truncation, garbage, an empty read — is `None`, which
/// skips the foreground grant rather than failing the activation.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_pid_reply(reply: &[u8]) -> Option<u32> {
    let line = reply.split(|&b| b == b'\n').next()?;
    std::str::from_utf8(line).ok()?.parse().ok()
}

/// The socket identifying the instance that `config_path` configures.
///
/// **Keyed by the config file, not the index.** The index path is a setting
/// the user can change while running — `IndexLock` moves with it — and a
/// socket that moved too would leave `--toggle` looking for the old one; the
/// config path is fixed for the life of both processes and is what makes them
/// agree. Two GUIs under different configs still get different sockets, which
/// is the only way two can legitimately run at once.
///
/// It lives in the runtime directory rather than beside the index: that is
/// tmpfs, private to the session, and cleared at logout, so a socket cannot
/// outlive the login that made it or land on a read-only portable install.
pub fn path_for(config_path: &Path) -> PathBuf {
    runtime_dir().join(format!("quicksearch-{:016x}.sock", key(config_path)))
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

/// `$XDG_RUNTIME_DIR` when the session set one, else a private directory of
/// our own under the temporary directory — which is shared, hence the mode.
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir;
        }
    }
    let fallback = std::env::temp_dir().join(format!(
        "quicksearch-{}",
        std::env::var("USER").unwrap_or_else(|_| "user".to_string())
    ));
    let _ = quicksearch_core::platform::create_dir_private(&fallback);
    fallback
}

/// Take the activation asked for since the last call.
pub fn take_pending() -> bool {
    PENDING.swap(false, Ordering::SeqCst)
}

/// What to tell the user to bind, as they would type it. The installed name
/// when we are on the path under it, and the full path otherwise — a build
/// run out of `target/` is the common case, and "quicksearch" would be wrong
/// advice there.
pub fn command_name() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "quicksearch".to_string();
    };
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

/// Record an activation and wake the window. Without the repaint an idle
/// event loop would leave it unread until the user moved the mouse.
///
/// Called both by the socket listener here and by `crate::hotkey` when the
/// application's own registration fires, so the two paths are identical from
/// the UI's point of view.
pub(crate) fn fire(ctx: &egui::Context) {
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

    /// Ask the instance configured by `config_path` to come forward.
    ///
    /// `true` means a live instance accepted it. `false` means there is none
    /// — a refused connection, or no socket at all — and the caller should
    /// start the GUI itself. **A leftover socket file never counts as an
    /// instance**: the same rule `IndexLock` follows, so a crash cannot
    /// strand the user behind a file nobody is listening on.
    pub fn signal(config_path: &Path) -> bool {
        let Ok(mut stream) = UnixStream::connect(path_for(config_path)) else {
            return false;
        };
        // Bounded on both halves: a `--toggle` must never become a process
        // that hangs on the keypress, whatever is on the other end.
        let timeout = std::time::Duration::from_secs(5);
        if stream.set_read_timeout(Some(timeout)).is_err()
            || stream.set_write_timeout(Some(timeout)).is_err()
        {
            return false;
        }
        if stream.write_all(b"\n").is_err() || stream.flush().is_err() {
            return false;
        }
        // The reply is what distinguishes "delivered" from "wrote into a
        // socket nobody reads". EOF means the peer went away.
        let mut ack = [0u8; 1];
        matches!(stream.read(&mut ack), Ok(1))
    }

    /// Start answering activations for the instance `config_path` configures.
    ///
    /// Call only while holding the index lock: binding unlinks whatever is
    /// in the way, and the lock is what proves no live instance under this
    /// config could be listening on it.
    ///
    /// Failure is logged and otherwise ignored — a runtime directory that
    /// cannot host a socket is not a reason to refuse to open the window.
    pub fn listen(ctx: &egui::Context, config_path: &Path) {
        let path = path_for(config_path);
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
            let Ok(mut stream) = stream else { continue };
            match request(&mut stream) {
                // Fire *before* the ack: a client that saw the reply may act
                // on "delivered", and delivered means the window was already
                // asked to come forward.
                Ok(()) => {
                    fire(ctx);
                    let _ = acknowledge(&mut stream);
                }
                // One stalled or truncated peer must not stop the loop, and
                // must not raise the window on a request it never finished.
                Err(e) => quicksearch_core::log_warn!("a search shortcut request: {}", e),
            }
        }
    }

    /// Read the request.
    ///
    /// Hostile input is the norm rather than the exception: any process of
    /// this user can connect. The read is bounded in both bytes and time, so
    /// a peer that connects and stalls cannot wedge the one thread that
    /// answers every activation.
    pub(super) fn request(stream: &mut UnixStream) -> std::io::Result<()> {
        let timeout = std::time::Duration::from_secs(5);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        // One byte is the whole request; the cap is what keeps a peer from
        // holding this thread for as long as it cares to send.
        let mut scratch = [0u8; 1];
        stream.read_exact(&mut scratch)
    }

    /// The reply that lets the sender tell a live instance from a leftover
    /// socket file.
    pub(super) fn acknowledge(stream: &mut UnixStream) -> std::io::Result<()> {
        stream.write_all(b"\n")?;
        stream.flush()
    }
}

/// The same handshake over a named pipe, which is what Windows has instead
/// of a unix socket.
///
/// **One deliberate difference: the reply is the foreground grant, not a
/// liveness check.** A named pipe exists only while a server holds an
/// instance open — there is no file left behind — so a successful
/// `CreateFileW` already proves a live instance accepted us. What Windows
/// *does* need is permission: `SetForegroundWindow` is refused to a process
/// the user did not just interact with, and the running instance is exactly
/// that. The `--toggle` process was launched by the user's keypress and so
/// holds the right — and may donate it with `AllowSetForegroundWindow`, given
/// the server's PID. The server therefore opens every connection by writing
/// its PID, and the sender grants before sending the request, so the grant
/// is always in force by the time the server raises.
#[cfg(windows)]
mod imp {
    use super::*;

    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, WriteFile};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
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
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a Create* call that returned success
            // and is closed exactly once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Ask the instance configured by `config_path` to come forward.
    ///
    /// `false` means no instance is serving the pipe and the caller should
    /// start the GUI itself.
    pub fn signal(config_path: &Path) -> bool {
        let name = pipe_name(config_path);
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
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                // SAFETY: reads this thread's last error, always valid.
                if unsafe { GetLastError() } == ERROR_PIPE_BUSY {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                return false;
            }
            let pipe = Handle(handle);
            // The server opens with its PID; hand it the foreground right we
            // hold from the user's keypress *before* asking it to raise. A
            // reply that does not parse skips the grant — the raise then
            // degrades to a taskbar flash rather than the request being lost.
            let mut reply = [0u8; PID_REPLY_CAP];
            let mut got = 0u32;
            // SAFETY: `reply` and `got` are live for the call; the buffer
            // length passed is the buffer's real length.
            let ok = unsafe {
                ReadFile(
                    pipe.0,
                    reply.as_mut_ptr(),
                    reply.len() as u32,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                if let Some(pid) = parse_pid_reply(&reply[..got as usize]) {
                    // SAFETY: no pointers; any PID value is acceptable input.
                    unsafe { AllowSetForegroundWindow(pid) };
                }
            }
            let mut written = 0u32;
            // SAFETY: a one-byte buffer and an output slot, both live here.
            let ok = unsafe {
                WriteFile(
                    pipe.0,
                    [b'\n'].as_ptr(),
                    1,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written != 1 {
                return false;
            }
            // SAFETY: the handle is open for the length of this call.
            unsafe { FlushFileBuffers(pipe.0) };
            return true;
        }
        false
    }

    /// Start answering activations for the instance `config_path` configures.
    ///
    /// Failure is logged and otherwise ignored, exactly as on unix: a pipe
    /// that cannot be created is not a reason to refuse to open the window.
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

    /// Runs until the process exits; one instance serves one activation, and
    /// a fresh instance is created for the next.
    fn serve(ctx: &egui::Context, name: &[u16]) {
        loop {
            // SAFETY: `name` is a NUL-terminated wide string borrowed for the
            // call; the null security descriptor gives the default, which
            // keeps the pipe to this user's own session.
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    PID_REPLY_CAP as u32,
                    PID_REPLY_CAP as u32,
                    0,
                    std::ptr::null(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                // SAFETY: reads this thread's last error, always valid.
                let code = unsafe { GetLastError() };
                quicksearch_core::log_warn!("the search shortcut pipe: error {}", code);
                return;
            }
            let pipe = Handle(handle);
            // SAFETY: the handle is open and no overlapped structure is used.
            let connected = unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) };
            // Zero can still mean a client that connected before we asked;
            // the read below is what decides, so it is not checked here.
            let _ = connected;

            // Our PID first, before reading anything: the client turns it
            // into a foreground grant and only then sends the request, so
            // the grant precedes the raise however the two threads interleave.
            // SAFETY: reads this process's own id, always valid.
            let pid = format!("{}\n", unsafe { GetCurrentProcessId() });
            let mut written = 0u32;
            // SAFETY: the buffer and output slot are live; the length passed
            // is the buffer's real length.
            let wrote = unsafe {
                WriteFile(
                    pipe.0,
                    pid.as_ptr(),
                    pid.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if wrote != 0 {
                // SAFETY: the handle is open for the length of this call.
                unsafe { FlushFileBuffers(pipe.0) };
            }

            let mut scratch = [0u8; 1];
            let mut read = 0u32;
            // SAFETY: a one-byte buffer and an output slot, both live here.
            let ok = unsafe {
                ReadFile(
                    pipe.0,
                    scratch.as_mut_ptr(),
                    1,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            // A peer that connected and said nothing is not an activation.
            // Fired before the disconnect, so a client watching the pipe
            // close can already rely on the window having been asked.
            if ok != 0 && read == 1 {
                fire(ctx);
            }
            // SAFETY: the handle is open for the length of this call.
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
    #[test]
    fn one_config_always_names_one_socket() {
        let config = Path::new("/home/u/.config/quicksearch/config.toml");
        assert_eq!(path_for(config), path_for(config));
        assert_ne!(path_for(config), path_for(Path::new("/elsewhere.toml")));
    }

    /// The index path is a live setting; the socket must not follow it, or a
    /// `database_path` change would strand every later `--toggle`.
    #[test]
    fn the_socket_does_not_depend_on_the_index() {
        assert!(!path_for(Path::new("/c.toml"))
            .to_string_lossy()
            .contains("index"));
    }

    /// Distinct paths that share a suffix must not collide.
    #[test]
    fn different_configs_name_different_sockets() {
        let a = key(Path::new("/a/config.toml"));
        let b = key(Path::new("/b/config.toml"));
        assert_ne!(a, b);
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

    /// A config path unique to this test, so the sockets these bind never
    /// collide: they run on one process, in parallel.
    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let path = PathBuf::from(format!("/qs-activate-{}-{}.toml", name, std::process::id()));
        let _ = std::fs::remove_file(path_for(&path));
        path
    }

    /// Nothing is listening on a path that was never bound.
    #[cfg(unix)]
    #[test]
    fn signalling_nothing_reports_nothing() {
        assert!(!signal(&scratch("absent")));
    }

    /// A leftover socket file with no listener must read as "not running",
    /// or a crash would strand the user behind a file nobody answers.
    #[cfg(unix)]
    #[test]
    fn a_stale_socket_file_is_not_an_instance() {
        let db = scratch("stale");
        std::fs::write(path_for(&db), b"").expect("leftover file");
        assert!(!signal(&db));
    }

    /// The whole point, end to end: a signal to a live listener is accepted,
    /// and the listener sees a well-formed request.
    #[cfg(unix)]
    #[test]
    fn a_signal_crosses_the_socket() {
        use std::os::unix::net::UnixListener;

        let db = scratch("roundtrip");
        let listener = UnixListener::bind(path_for(&db)).expect("bind");

        let sender = std::thread::spawn(move || signal(&db));

        let mut stream = listener
            .incoming()
            .next()
            .expect("a connection")
            .expect("accepted");
        imp::request(&mut stream).expect("a well-formed request");
        imp::acknowledge(&mut stream).expect("acknowledged");

        assert!(sender.join().expect("sender"), "the client saw the reply");
    }

    /// `listen` must replace a crashed predecessor's socket rather than
    /// giving up on it.
    #[cfg(unix)]
    #[test]
    fn binding_replaces_a_leftover_socket() {
        let _serial = pending_guard();
        let db = scratch("rebind");
        let path = path_for(&db);
        std::fs::write(&path, b"").expect("leftover file");

        let ctx = egui::Context::default();
        listen(&ctx, &db);

        // Proof the listener took the path over: a signal now lands.
        assert!(signal(&db), "the new listener answers");
        assert!(take_pending(), "and the window was asked to come forward");
    }

    /// A peer that connects and says nothing must not wedge the listener,
    /// nor count as an activation.
    #[cfg(unix)]
    #[test]
    fn a_silent_peer_does_not_raise_the_window() {
        use std::os::unix::net::{UnixListener, UnixStream};

        let db = scratch("silent");
        let path = path_for(&db);
        let listener = UnixListener::bind(&path).expect("bind");

        // Connect, send nothing, close: `read_exact` sees EOF.
        let peer = UnixStream::connect(&path).expect("connect");
        drop(peer);

        let mut stream = listener
            .incoming()
            .next()
            .expect("a connection")
            .expect("accepted");
        assert!(
            imp::request(&mut stream).is_err(),
            "an empty request is refused"
        );
    }

    /// A config path unique to this test. Never opened as a file — only
    /// hashed into a pipe name — so it need not exist.
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
        assert!(!signal(&scratch("absent")));
    }

    /// End to end: `listen` serves the pipe and a signal reaches it.
    #[cfg(windows)]
    #[test]
    fn a_signal_crosses_the_pipe() {
        let _serial = pending_guard();
        let config = scratch("roundtrip");
        let ctx = egui::Context::default();
        listen(&ctx, &config);

        // The server thread has to reach its first `CreateNamedPipeW`; retry
        // rather than sleeping a guessed amount.
        let mut delivered = false;
        for _ in 0..100 {
            if signal(&config) {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(delivered, "the listener never answered");
        // Unlike unix there is no ack after the fire: the server reads the
        // request after the client's write returns, so give it a moment.
        let mut fired = false;
        for _ in 0..100 {
            if take_pending() {
                fired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(fired, "the window was never asked to come forward");
    }
}
