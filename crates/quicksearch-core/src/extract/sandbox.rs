//! The extraction sandbox. A binary that calls [`enable_sandbox`] parses
//! every file of a sandboxed format in a child process of its own, under CPU
//! and memory limits; [`sandbox_child_main`] is the child's side, and the
//! [`super::Registry`] decides which formats go there. They are the ones
//! parsed mostly by third-party code that can recurse or allocate without
//! bound on crafted input, and a stack overflow or a failed allocation aborts
//! the process past `catch_unwind`:
//! - PDF: lopdf's loader recurses without bound (an indirect `/Length`
//!   naming a stream whose own `/Length` is indirect, thousands deep; an
//!   object stream whose `/Length` lives inside it), so does the Type1 font
//!   parser, and loading inflates without a cap.
//! - Audio: lofty sizes a Vorbis comment list (FLAC, Ogg, Opus) from the
//!   count the file declares, past its own allocation limit, so a damaged
//!   FLAC of a few hundred bytes asks for 80 GB.
//! - Legacy Office (doc, xls, ppt): cfb's compound-file reader, over sector
//!   tables the file chooses.
//!
//! In the child, each of these fails the one file. Without `enable_sandbox`
//! (core's tests, the examples), every format parses in-process.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::{ExtractError, Limits, Registry, Scratch};

/// `argv[1]` of a sandbox child:
/// `<exe> --qs-extract-child <mime> <read limit> <text limit> <path>`.
pub const SANDBOX_FLAG: &str = "--qs-extract-child";

/// The child's exit status for a file that failed to parse; its stdout
/// carries the reason. 0 carries the text; any other status, a signal
/// included, reads as a crash, or after a stop's kill as cancelled.
const EXIT_FAILED: i32 = 2;

/// CPU time one child may spend. PDF's work budget ends rendering within a
/// few seconds; the rest is for loading, which nothing else bounds. Tags and
/// legacy Office text take far less.
const CHILD_CPU: Duration = Duration::from_secs(60);

/// Heap one child may hold, on top of what the process maps anyway
/// ([`crate::platform::spawn_limited`]). A ceiling, not a budget: the
/// largest real documents measured here hold a tenth of it (a 16 MB PDF at
/// 150 MB, a 2 MiB one at 74 MiB), and a crafted stream that inflates past
/// it, or a declared count sizing an allocation, fails alone.
const CHILD_MEMORY: u64 = 1 << 30;

/// Output kept past the text limit. The caller truncates at the limit
/// anyway, so whatever a child sends past this is read and dropped; PDF's
/// sink stops one glyph's text past it.
const OUTPUT_SLACK: usize = 64 * 1024;

/// The binary run as the sandbox child, once [`enable_sandbox`] names it.
static SANDBOX: OnceLock<PathBuf> = OnceLock::new();

/// Parse every file of a sandboxed format in a child process of `exe` from
/// now on. `exe` must hand an `argv[1]` of [`SANDBOX_FLAG`] to
/// [`sandbox_child_main`] before doing anything else. The first call wins.
pub fn enable_sandbox(exe: PathBuf) {
    let _ = SANDBOX.set(exe);
}

/// The binary [`enable_sandbox`] named, if any.
pub(super) fn sandbox_exe() -> Option<&'static Path> {
    SANDBOX.get().map(PathBuf::as_path)
}

/// Sandbox children still running; see [`kill_sandbox_children`].
static RUNNING: Mutex<Vec<Arc<Mutex<Child>>>> = Mutex::new(Vec::new());

/// How many times [`kill_sandbox_children`] has run.
static KILLS: AtomicU64 = AtomicU64::new(0);

/// Kill every sandbox child running now, and every one spawned later under
/// a [`KillMark`] read before this call. The read waiting on each child's
/// output then ends, so a stop never waits on a parse; the file reads as
/// [`super::CANCELLED`] and stays pending.
pub fn kill_sandbox_children() {
    KILLS.fetch_add(1, Ordering::SeqCst);
    for child in crate::lock_ok(&RUNNING).iter() {
        let _ = crate::lock_ok(child).kill();
    }
}

/// How many times [`kill_sandbox_children`] had run when this was read. A
/// sandbox child spawned under a mark dies at once if a kill ran since. A
/// caller reads its mark before it last checks its own stop condition (a
/// content worker once per pass, through [`Scratch::new`]), so a stop that
/// lands after that check still reaches every child the caller spawns.
/// Every stop sets its flag before it kills. Marks order as they were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KillMark(u64);

impl KillMark {
    pub fn now() -> KillMark {
        KillMark(KILLS.load(Ordering::SeqCst))
    }

    /// Whether a kill ran since this mark was read.
    fn passed(self) -> bool {
        KILLS.load(Ordering::SeqCst) != self.0
    }
}

/// A sandbox child, listed in [`RUNNING`] until dropped.
struct Running(Arc<Mutex<Child>>);

impl Running {
    /// List `child`, killing it at once if a kill ran since `kills`: the
    /// stop it belongs to has already gone past its caller's check.
    fn new(child: Child, kills: KillMark) -> Running {
        let child = Arc::new(Mutex::new(child));
        crate::lock_ok(&RUNNING).push(child.clone());
        if kills.passed() {
            let _ = crate::lock_ok(&child).kill();
        }
        Running(child)
    }

    /// Reap the child. Called once its stdout has closed, so it is exiting
    /// and the lock that [`kill_sandbox_children`] waits on is held briefly;
    /// a kill after the reap is a no-op, never a signal to a reused pid.
    fn wait(&self) -> std::io::Result<ExitStatus> {
        crate::lock_ok(&self.0).wait()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        crate::lock_ok(&RUNNING).retain(|child| !Arc::ptr_eq(child, &self.0));
    }
}

/// The sandbox child's `main`; `args` follow [`SANDBOX_FLAG`]: the MIME, the
/// read limit, the text limit, then the path. Runs the registry's extractor
/// for that MIME in-process ([`Registry::extract_in_process`]), writes the
/// text to stdout and returns 0, or writes the reason and returns
/// [`EXIT_FAILED`]. Parses on a [`crate::platform::spawn_worker`] thread, the
/// stack the indexer parses on in-process, so the recursion bounds tested
/// there hold here.
pub fn sandbox_child_main(mut args: impl Iterator<Item = OsString>) -> i32 {
    // Nothing before the parent has applied its limits: on Windows the
    // child exists before it can be put in its job, and the parent closes
    // this handle once it is (`platform::spawn_limited`). On Unix the limits
    // are in force before this image runs at all, and stdin is closed.
    #[cfg(windows)]
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
    let Some(mime) = args.next().and_then(|m| m.into_string().ok()) else {
        return 1;
    };
    let mut number = || args.next()?.to_str()?.parse::<usize>().ok();
    let (Some(read), Some(text)) = (number(), number()) else {
        return 1;
    };
    let (Some(path), None) = (args.next(), args.next()) else {
        return 1;
    };
    let path = PathBuf::from(path);
    let parsed = crate::platform::spawn_worker("qs-extract-child", move || {
        let mut out = String::new();
        let mut scratch = Scratch::with_limits(Limits::new(read, text), KillMark::now());
        Registry::default_set()
            .extract_in_process(&path, &mime, &mut out, &mut scratch)
            .map(|claimed| claimed.then_some(out))
    })
    .join();
    let (code, output) = match parsed {
        Ok(Ok(Some(text))) => (0, text),
        Ok(Err(reason)) => (EXIT_FAILED, reason),
        // A MIME no extractor claims is a caller's mistake, as bad arguments are.
        Ok(Ok(None)) | Err(_) => return 1,
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => code,
        Err(_) => 1,
    }
}

/// [`Registry::extract_in_process`] for `mime` in a sandbox child of `exe`,
/// under `limits`. A child that a kill since `kills` cut short gives
/// [`super::CANCELLED`], not a crash, and so does a child that could not be
/// spawned or reaped (descriptors, processes or memory ran out): neither is
/// a verdict on the file, which stays pending.
pub(super) fn extract_in_child(
    exe: &Path,
    path: &Path,
    mime: &str,
    limits: Limits,
    kills: KillMark,
) -> Result<String, ExtractError> {
    let mut command = Command::new(exe);
    command
        .arg(SANDBOX_FLAG)
        .arg(mime)
        .arg(limits.read.to_string())
        .arg(limits.text.to_string())
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, _limits) =
        crate::platform::spawn_limited(&mut command, CHILD_CPU, CHILD_MEMORY)
            .map_err(|e| not_run("spawn", path, e))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let child = Running::new(child, kills);
    let mut output = Vec::new();
    let mut capped = stdout.take(limits.text.saturating_add(OUTPUT_SLACK) as u64);
    let _ = capped.read_to_end(&mut output);
    // To EOF even past the cap, so a child is never left blocked on a full
    // pipe: the text it sends is bounded by its extractor, and its failures
    // are one line.
    let _ = std::io::copy(&mut capped.into_inner(), &mut std::io::sink());
    let status = child.wait().map_err(|e| not_run("reap", path, e))?;
    let output = String::from_utf8_lossy(&output).into_owned();
    match status.code() {
        Some(0) => Ok(output),
        Some(EXIT_FAILED) => Err(output),
        // A signal, or on Windows the termination `Child::kill` exits with.
        _ if kills.passed() => Err(super::CANCELLED.to_string()),
        _ => Err(format!(
            "extractor crashed on {}: {}",
            path.display(),
            status
        )),
    }
}

/// A sandbox child that failed to `what` for `path`, logged: the file was
/// never judged, so it reads as [`super::CANCELLED`].
fn not_run(what: &str, path: &Path, e: std::io::Error) -> ExtractError {
    crate::log_warn!(
        "extractor sandbox: could not {} the child for {}: {}",
        what,
        path.display(),
        e
    );
    super::CANCELLED.to_string()
}
