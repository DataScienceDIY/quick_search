//! Scripted self-capture (feature `capture`): the app drives itself through
//! a scenario so `packaging/capture.sh` can regenerate the website
//! screenshots and screencasts as the software changes.
//!
//! With `QS_CAPTURE_SCRIPT` set, a [`CaptureDriver`] runs one command at a
//! time: it injects keystrokes as real `egui::Event::Text` input (so typing
//! goes through the same debounce and streaming-search path a user's would),
//! switches tabs through the same pending-nav route a click takes, waits on
//! live indexer/search/duplicates state, saves pixel-perfect screenshots via
//! `ViewportCommand::Screenshot`, and records video the same way: frames
//! read back from the GL framebuffer, piped to `ffmpeg` as raw video.
//!
//! Everything is captured from inside the app on purpose. Screen-grabbing
//! (`x11grab` and friends) depends on the display server — it records black
//! frames from a rootless XWayland, needs portals on Wayland proper, and
//! picks up whatever overlaps the window — while the framebuffer readback
//! behind `ViewportCommand::Screenshot` works identically on X11, Wayland
//! and Windows, and sees nothing but the app.
//!
//! Driving from the inside is what keeps the captures maintainable: there
//! are no screen coordinates to rot when the layout changes, and no external
//! automation tooling to install. The scenario file is the only thing to
//! edit when re-choreographing.
//!
//! Exit codes, for the orchestrator: 2 script parse error, 3 wait timeout,
//! 4 screenshot/recording I/O failure.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use quicksearch_core::indexing::IndexingStatus;

use crate::app::{QuickSearchApp, Tab};

// ---------------------------------------------------------------------------
// Scenario script
// ---------------------------------------------------------------------------

/// One scenario command. Line-based script, `#` comments outside strings:
///
/// ```text
/// wait_ms INT
/// type "STRING" [cps FLOAT]        # default 7 chars/sec
/// clear_query | focus_search
/// window INT INT                   # resize to width x height, in the same
///                                  # logical points as the startup size
/// tab (search|manage|duplicates|logs|help)
/// wait_index_running [max INT]     # caps in ms; a capped wait cannot fail
/// wait_index_idle    [max INT]
/// wait_search_done   [max INT]
/// wait_dups_done     [max INT]
/// record_start NAME | record_stop  # NAME: [A-Za-z0-9._-]+, no separators
/// screenshot NAME
/// quit
/// ```
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cmd {
    WaitMs(u64),
    Type { text: String, cps: f32 },
    ClearQuery,
    FocusSearch,
    Window { w: f32, h: f32 },
    Tab(Tab),
    WaitIndexRunning { max_ms: Option<u64> },
    WaitIndexIdle { max_ms: Option<u64> },
    WaitSearchDone { max_ms: Option<u64> },
    WaitDupsDone { max_ms: Option<u64> },
    RecordStart(String),
    RecordStop,
    Screenshot(String),
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    /// 1-based line in the scenario file.
    pub line: usize,
    pub msg: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Str(String),
}

/// Split one line into bare words and quoted strings. `#` starts a comment
/// except inside a string; `\"` and `\\` are the only escapes.
fn tokenize(line: &str, line_no: usize) -> Result<Vec<Token>, ParseError> {
    let err = |msg: String| ParseError { line: line_no, msg };
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            break;
        } else if c == '"' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    None => return Err(err("unclosed string".to_string())),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(e @ ('"' | '\\')) => s.push(e),
                        Some(e) => return Err(err(format!("unknown escape \\{e}"))),
                        None => return Err(err("unclosed string".to_string())),
                    },
                    Some(other) => s.push(other),
                }
            }
            tokens.push(Token::Str(s));
        } else {
            let mut w = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '#' {
                    break;
                }
                if c == '"' {
                    return Err(err("quotes may only start a token".to_string()));
                }
                w.push(c);
                chars.next();
            }
            tokens.push(Token::Word(w));
        }
    }
    Ok(tokens)
}

pub(crate) fn parse_script(src: &str) -> Result<Vec<Cmd>, ParseError> {
    let mut cmds = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let line_no = i + 1;
        let tokens = tokenize(line, line_no)?;
        if let Some(cmd) = parse_line(&tokens, line_no)? {
            cmds.push(cmd);
        }
    }
    Ok(cmds)
}

fn parse_line(tokens: &[Token], line_no: usize) -> Result<Option<Cmd>, ParseError> {
    let err = |msg: String| ParseError { line: line_no, msg };
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    let Token::Word(name) = first else {
        return Err(err(
            "a line must start with a command, not a string".to_string()
        ));
    };

    let rest = &mut tokens[1..].iter();
    let cmd = match name.as_str() {
        "wait_ms" => Cmd::WaitMs(parse_int(
            "duration",
            next_word(rest, line_no, "duration in ms")?,
            line_no,
        )?),
        "type" => {
            let text = match rest.next() {
                Some(Token::Str(s)) => s.clone(),
                Some(Token::Word(_)) => {
                    return Err(err("the text to type must be quoted".to_string()));
                }
                None => return Err(err("missing text to type".to_string())),
            };
            let cps = match rest.next() {
                None => 7.0,
                Some(Token::Word(w)) if w == "cps" => {
                    let v = next_word(rest, line_no, "cps value")?;
                    let cps: f32 = v
                        .parse()
                        .map_err(|_| err(format!("invalid cps {v:?}: expected a number")))?;
                    if !(cps.is_finite() && cps > 0.0) {
                        return Err(err("cps must be positive".to_string()));
                    }
                    cps
                }
                Some(other) => return Err(err(format!("expected `cps`, found {other:?}"))),
            };
            Cmd::Type { text, cps }
        }
        "clear_query" => Cmd::ClearQuery,
        "focus_search" => Cmd::FocusSearch,
        "window" => {
            let w = parse_int("width", next_word(rest, line_no, "width in points")?, line_no)?;
            let h = parse_int("height", next_word(rest, line_no, "height in points")?, line_no)?;
            if w == 0 || h == 0 {
                return Err(err("window dimensions must be positive".to_string()));
            }
            Cmd::Window {
                w: w as f32,
                h: h as f32,
            }
        }
        "tab" => Cmd::Tab(match next_word(rest, line_no, "tab name")? {
            "search" => Tab::Search,
            "manage" => Tab::Manage,
            "duplicates" => Tab::Duplicates,
            "logs" => Tab::Logs,
            "help" => Tab::Help,
            other => {
                return Err(err(format!(
                    "unknown tab {other:?}: expected search, manage, duplicates, logs or help"
                )));
            }
        }),
        "wait_index_running" | "wait_index_idle" | "wait_search_done" | "wait_dups_done" => {
            let max_ms = match rest.next() {
                None => None,
                Some(Token::Word(w)) if w == "max" => Some(parse_int(
                    "max",
                    next_word(rest, line_no, "max value in ms")?,
                    line_no,
                )?),
                Some(other) => return Err(err(format!("expected `max`, found {other:?}"))),
            };
            match name.as_str() {
                "wait_index_running" => Cmd::WaitIndexRunning { max_ms },
                "wait_index_idle" => Cmd::WaitIndexIdle { max_ms },
                "wait_search_done" => Cmd::WaitSearchDone { max_ms },
                _ => Cmd::WaitDupsDone { max_ms },
            }
        }
        "record_start" => Cmd::RecordStart(parse_name(
            next_word(rest, line_no, "output name")?,
            line_no,
        )?),
        "record_stop" => Cmd::RecordStop,
        "screenshot" => Cmd::Screenshot(parse_name(
            next_word(rest, line_no, "output name")?,
            line_no,
        )?),
        "quit" => Cmd::Quit,
        other => return Err(err(format!("unknown command {other:?}"))),
    };

    if let Some(extra) = rest.next() {
        return Err(err(format!(
            "unexpected {extra:?} after a complete command"
        )));
    }
    Ok(Some(cmd))
}

fn next_word<'a>(
    rest: &mut std::slice::Iter<'a, Token>,
    line_no: usize,
    what: &str,
) -> Result<&'a str, ParseError> {
    match rest.next() {
        Some(Token::Word(w)) => Ok(w.as_str()),
        Some(Token::Str(_)) => Err(ParseError {
            line: line_no,
            msg: format!("expected {what}, found a string"),
        }),
        None => Err(ParseError {
            line: line_no,
            msg: format!("missing {what}"),
        }),
    }
}

fn parse_int(what: &str, w: &str, line_no: usize) -> Result<u64, ParseError> {
    w.parse::<u64>().map_err(|_| ParseError {
        line: line_no,
        msg: format!("invalid {what} {w:?}: expected an integer"),
    })
}

/// Output names stay inside `$QS_CAPTURE_OUT`: a plain filename stem, the
/// driver appends the extension.
fn parse_name(w: &str, line_no: usize) -> Result<String, ParseError> {
    let ok = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(w.to_string())
    } else {
        Err(ParseError {
            line: line_no,
            msg: format!("invalid name {w:?}: use only letters, digits, `.`, `_`, `-`"),
        })
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// In-flight keystroke injection for one `type` command.
struct Typing {
    chars: std::vec::IntoIter<char>,
    /// Nominal seconds per keystroke; each interval is jittered ±25%.
    interval_s: f32,
    due: Instant,
    typed: u64,
}

/// Recording frame rate. Readback requests are paced to this, and
/// [`CaptureDriver::feed_frame`] duplicates or drops frames so the encoded
/// timeline tracks wall time even when frames arrive unevenly.
const RECORD_FPS: u32 = 30;

/// A recording in progress: paced framebuffer readbacks piped to ffmpeg.
struct Recorder {
    path: PathBuf,
    /// Nominal time per frame (1 / [`RECORD_FPS`]).
    interval: Duration,
    /// When to ask for the next framebuffer readback.
    next_request: Instant,
    /// Spawned when the first frame arrives — only then are the exact pixel
    /// dimensions known, and ffmpeg needs them up front for raw video.
    encoder: Option<Encoder>,
}

struct Encoder {
    child: Child,
    size: [usize; 2],
    /// When the first frame arrived; the video's t = 0.
    started: Instant,
    frames_written: u64,
}

/// Marker in a screenshot's `UserData` for the `screenshot` command.
struct ShotTag;

/// Marker in a screenshot's `UserData` for one recording frame.
struct FrameTag;

pub(crate) struct CaptureDriver {
    cmds: Vec<Cmd>,
    /// Index of the command currently executing.
    pc: usize,
    /// When `cmds[pc]`'s one-shot enter action ran; `None` before it has.
    cmd_started: Option<Instant>,
    typing: Option<Typing>,
    /// Screenshot in flight: requested, PNG not yet written.
    shot: Option<PathBuf>,
    rec: Option<Recorder>,
    out_dir: PathBuf,
    /// Set by `quit`; the app drops the driver once it is.
    pub(crate) finished: bool,
}

impl CaptureDriver {
    /// `None` unless `QS_CAPTURE_SCRIPT` names a scenario. A script that
    /// cannot be read or parsed exits immediately — an automation harness
    /// wants a loud parse error, not a window that sits there doing nothing.
    pub(crate) fn from_env() -> Option<Box<CaptureDriver>> {
        let script = std::env::var_os("QS_CAPTURE_SCRIPT")?;
        let src = match std::fs::read_to_string(&script) {
            Ok(src) => src,
            Err(e) => {
                eprintln!(
                    "capture: cannot read {}: {}",
                    Path::new(&script).display(),
                    e
                );
                std::process::exit(2);
            }
        };
        let cmds = match parse_script(&src) {
            Ok(cmds) => cmds,
            Err(e) => {
                eprintln!(
                    "capture: {}:{}: {}",
                    Path::new(&script).display(),
                    e.line,
                    e.msg
                );
                std::process::exit(2);
            }
        };
        let out_dir = std::env::var_os("QS_CAPTURE_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            eprintln!("capture: cannot create {}: {}", out_dir.display(), e);
            std::process::exit(4);
        }
        Some(Box::new(CaptureDriver {
            cmds,
            pc: 0,
            cmd_started: None,
            typing: None,
            shot: None,
            rec: None,
            out_dir,
            finished: false,
        }))
    }

    /// Advance the script by at most one command per frame. Runs at the top
    /// of `update()`, so a command's effect is on screen before the next
    /// command starts.
    pub(crate) fn tick(&mut self, app: &mut QuickSearchApp, ctx: &egui::Context) {
        if self.finished {
            return;
        }
        // The app repaints on demand when idle; the driver needs frames to
        // keep its own clock ticking, and a steady cadence is also what
        // keeps recorded footage smooth.
        ctx.request_repaint_after(Duration::from_millis(15));

        // Pump the recording: one framebuffer readback per frame interval,
        // harvested in `on_raw_input` a frame later.
        if let Some(rec) = self.rec.as_mut() {
            let now = Instant::now();
            if now >= rec.next_request {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    FrameTag,
                )));
                rec.next_request = now + rec.interval;
            }
        }

        let Some(cmd) = self.cmds.get(self.pc).cloned() else {
            self.quit(ctx);
            return;
        };
        let started = match self.cmd_started {
            Some(t) => t,
            None => {
                let now = Instant::now();
                self.cmd_started = Some(now);
                self.enter(&cmd, app, ctx);
                now
            }
        };
        if self.finished {
            return; // `quit` just ran
        }

        let elapsed = started.elapsed();
        if self.done(&cmd, app, elapsed) {
            self.pc += 1;
            self.cmd_started = None;
        } else if elapsed >= Duration::from_millis(hard_timeout_ms(&cmd)) {
            eprintln!(
                "capture: command #{} ({:?}) timed out after {:.1?}",
                self.pc + 1,
                cmd,
                elapsed
            );
            self.stop_recorder();
            std::process::exit(3);
        }
    }

    /// One-shot action when a command starts.
    fn enter(&mut self, cmd: &Cmd, app: &mut QuickSearchApp, ctx: &egui::Context) {
        match cmd {
            Cmd::WaitMs(_)
            | Cmd::WaitIndexRunning { .. }
            | Cmd::WaitIndexIdle { .. }
            | Cmd::WaitSearchDone { .. }
            | Cmd::WaitDupsDone { .. }
            | Cmd::RecordStop => {}
            Cmd::Type { text, cps } => {
                let interval_s = 1.0 / cps;
                self.typing = Some(Typing {
                    chars: text.chars().collect::<Vec<_>>().into_iter(),
                    interval_s,
                    due: Instant::now() + Duration::from_secs_f32(interval_s),
                    typed: 0,
                });
            }
            Cmd::ClearQuery => app.capture_clear_query(),
            Cmd::FocusSearch => app.capture_focus_search(),
            Cmd::Window { w, h } => {
                // Scenario sizes use the same logical points as the startup
                // size in main.rs, so `window 1000 700` restores it exactly.
                // ViewportCommand sizes are in egui points, which fold in the
                // UI zoom ([ui] scale) — divide it back out. The app's own
                // floor is 640x400; lower it first so compact clip sizes
                // actually take effect. The resize lands asynchronously (the
                // window manager has the last word), so scenarios follow
                // this with a wait_ms before recording.
                let size = egui::vec2(*w, *h) / ctx.zoom_factor();
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
            Cmd::Tab(tab) => app.capture_request_tab(*tab),
            Cmd::RecordStart(name) => {
                self.rec = Some(Recorder {
                    path: self.out_dir.join(format!("{name}.cap.mkv")),
                    interval: Duration::from_secs_f64(1.0 / f64::from(RECORD_FPS)),
                    next_request: Instant::now(),
                    encoder: None,
                });
            }
            Cmd::Screenshot(name) => {
                self.shot = Some(self.out_dir.join(format!("{name}.png")));
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    ShotTag,
                )));
            }
            Cmd::Quit => self.quit(ctx),
        }
        if matches!(cmd, Cmd::RecordStop) {
            self.stop_recorder();
        }
    }

    /// Whether the current command has finished. Wait conditions treat a
    /// `max` cap as "done anyway": the caps exist to bound clip length and to
    /// tolerate a state change that happened before the wait began.
    fn done(&self, cmd: &Cmd, app: &QuickSearchApp, elapsed: Duration) -> bool {
        let capped =
            |max_ms: &Option<u64>| max_ms.is_some_and(|ms| elapsed >= Duration::from_millis(ms));
        match cmd {
            Cmd::WaitMs(ms) => elapsed >= Duration::from_millis(*ms),
            Cmd::Type { .. } => self.typing.is_none(),
            Cmd::ClearQuery
            | Cmd::FocusSearch
            | Cmd::Window { .. }
            | Cmd::Tab(_)
            | Cmd::RecordStart(_)
            | Cmd::RecordStop
            | Cmd::Quit => true,
            Cmd::WaitIndexRunning { max_ms } => {
                capped(max_ms)
                    || matches!(
                        app.capture_indexing_status(),
                        IndexingStatus::Running { .. }
                            | IndexingStatus::Stopping
                            | IndexingStatus::Optimizing
                    )
            }
            Cmd::WaitIndexIdle { max_ms } => {
                let status = app.capture_indexing_status();
                if let IndexingStatus::Error(e) = &status {
                    eprintln!("capture: indexing reported an error: {e}");
                }
                capped(max_ms) || matches!(status, IndexingStatus::Idle | IndexingStatus::Error(_))
            }
            Cmd::WaitSearchDone { max_ms } => capped(max_ms) || app.capture_search_settled(),
            Cmd::WaitDupsDone { max_ms } => capped(max_ms) || app.capture_dups_done(),
            Cmd::Screenshot(_) => self.shot.is_none(),
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.stop_recorder();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        self.finished = true;
    }

    /// Runs in `raw_input_hook`, before egui processes this frame's input:
    /// due keystrokes are appended as `Event::Text` (landing in the focused
    /// search box exactly as real typing would), and a finished screenshot is
    /// harvested from the incoming events and written out.
    pub(crate) fn on_raw_input(&mut self, raw: &mut egui::RawInput) {
        let mut drained = false;
        if let Some(t) = self.typing.as_mut() {
            let now = Instant::now();
            while t.due <= now {
                let Some(c) = t.chars.next() else {
                    drained = true;
                    break;
                };
                raw.events.push(egui::Event::Text(c.to_string()));
                t.typed += 1;
                let interval = t.interval_s * jitter(t.typed);
                t.due += Duration::from_secs_f32(interval);
            }
        }
        if drained {
            self.typing = None;
        }

        // One pass over the incoming events harvests both kinds of
        // framebuffer readback: recording frames and still screenshots.
        for event in &raw.events {
            let egui::Event::Screenshot {
                user_data, image, ..
            } = event
            else {
                continue;
            };
            let Some(data) = user_data.data.as_ref() else {
                continue;
            };
            if data.downcast_ref::<FrameTag>().is_some() {
                if let Err(e) = self.feed_frame(image) {
                    eprintln!("capture: recording failed: {e}");
                    std::process::exit(4);
                }
            } else if data.downcast_ref::<ShotTag>().is_some() {
                if let Some(path) = self.shot.take() {
                    if let Err(e) = write_png(image, &path) {
                        eprintln!("capture: cannot write {}: {}", path.display(), e);
                        std::process::exit(4);
                    }
                }
            }
        }
    }

    // -- recording ----------------------------------------------------------

    /// Append one readback to the recording, spawning the encoder on the
    /// first frame (which fixes the dimensions). The frame is written as many
    /// times as whole intervals have elapsed since the recording began —
    /// duplicated to catch up after a slow frame, dropped when readbacks
    /// outpace [`RECORD_FPS`] — so the video's length tracks wall time.
    fn feed_frame(&mut self, image: &egui::ColorImage) -> Result<(), String> {
        let Some(rec) = self.rec.as_mut() else {
            return Ok(()); // stopped while this readback was in flight
        };
        if rec.encoder.is_none() {
            rec.encoder = Some(Encoder {
                child: spawn_encoder(&rec.path, image.size)?,
                size: image.size,
                started: Instant::now(),
                frames_written: 0,
            });
        }
        let encoder = rec.encoder.as_mut().expect("spawned above");
        if encoder.size != image.size {
            return Err(format!(
                "window resized mid-recording ({:?} -> {:?})",
                encoder.size, image.size
            ));
        }
        let elapsed = encoder.started.elapsed().as_secs_f64();
        let target = (elapsed / rec.interval.as_secs_f64()).floor() as u64 + 1;
        let stdin = encoder
            .child
            .stdin
            .as_mut()
            .ok_or("the encoder's stdin is gone")?;
        while encoder.frames_written < target {
            stdin
                .write_all(image.as_raw())
                .map_err(|e| format!("writing to ffmpeg: {e}"))?;
            encoder.frames_written += 1;
        }
        Ok(())
    }

    /// Close the encoder's stdin — end-of-input, on which ffmpeg encodes the
    /// tail and exits — then wait, with a kill as backstop. Blocking the UI
    /// thread here is fine: the recording has already ended, so there is
    /// nothing to miss on screen.
    fn stop_recorder(&mut self) {
        let Some(rec) = self.rec.take() else {
            return;
        };
        let Some(mut encoder) = rec.encoder else {
            eprintln!(
                "capture: recording {} captured no frames",
                rec.path.display()
            );
            std::process::exit(4);
        };
        drop(encoder.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match encoder.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = encoder.child.kill();
                    let _ = encoder.child.wait();
                    break;
                }
            }
        }
        let ok = std::fs::metadata(&rec.path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if !ok {
            eprintln!(
                "capture: recording {} is missing or empty",
                rec.path.display()
            );
            std::process::exit(4);
        }
    }
}

/// ffmpeg encoding raw RGBA frames from stdin into a *lossless* intermediate;
/// `capture.sh` transcodes to VP9 afterwards. Realtime VP9 at capture quality
/// drops frames, while `libx264rgb -qp 0 -preset ultrafast` is cheap, keeps
/// text crisp (no chroma subsampling at capture time), and mkv survives an
/// unclean stop.
fn spawn_encoder(path: &Path, size: [usize; 2]) -> Result<Child, String> {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "rawvideo", "-pixel_format", "rgba"])
        .args(["-video_size", &format!("{}x{}", size[0], size[1])])
        .args(["-framerate", &RECORD_FPS.to_string()])
        .args(["-i", "pipe:0"])
        .args([
            "-c:v",
            "libx264rgb",
            "-qp",
            "0",
            "-preset",
            "ultrafast",
            "-g",
            "60",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))
}

/// Deterministic per-keystroke pacing factor in [0.75, 1.25] — human enough
/// on video, identical on every run, and no rand dependency.
fn jitter(keystroke: u64) -> f32 {
    let mut x = keystroke
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(1);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    0.75 + (x % 1000) as f32 / 1000.0 * 0.5
}

/// Ceiling after which a wait without `max` aborts the run: generous enough
/// for a full index of the demo tree, small enough that a wedged run fails
/// instead of hanging the orchestrator.
fn hard_timeout_ms(cmd: &Cmd) -> u64 {
    match cmd {
        // Always finish on their own; the bound is just a backstop.
        Cmd::WaitMs(ms) => ms + 60_000,
        Cmd::Type { text, cps } => (text.chars().count() as f32 / cps * 1000.0) as u64 + 30_000,
        Cmd::ClearQuery
        | Cmd::FocusSearch
        | Cmd::Window { .. }
        | Cmd::Tab(_)
        | Cmd::RecordStart(_)
        | Cmd::RecordStop
        | Cmd::Quit => 10_000,
        Cmd::Screenshot(_) => 10_000,
        Cmd::WaitIndexRunning { .. } => 120_000,
        Cmd::WaitIndexIdle { .. } => 1_800_000,
        Cmd::WaitSearchDone { .. } => 60_000,
        Cmd::WaitDupsDone { .. } => 300_000,
    }
}

/// The GL framebuffer is opaque, so premultiplied and straight alpha agree
/// and the pixels can be reused as-is. eframe's icon helper brings the PNG
/// encoder — no extra dependency.
fn write_png(image: &egui::ColorImage, path: &Path) -> Result<(), String> {
    use eframe::icon_data::IconDataExt as _;
    let icon = egui::IconData {
        width: image.size[0] as u32,
        height: image.size[1] as u32,
        rgba: image.as_raw().to_vec(),
    };
    std::fs::write(path, icon.to_png_bytes()?).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// App glue
// ---------------------------------------------------------------------------

/// Take/call/put wrappers: the driver borrows the whole app mutably, so it
/// cannot stay a field of it during the call.
impl QuickSearchApp {
    pub(crate) fn capture_tick(&mut self, ctx: &egui::Context) {
        let Some(mut driver) = self.capture.take() else {
            return;
        };
        driver.tick(self, ctx);
        if !driver.finished {
            self.capture = Some(driver);
        }
    }

    pub(crate) fn capture_raw_input(&mut self, raw: &mut egui::RawInput) {
        let Some(mut driver) = self.capture.take() else {
            return;
        };
        driver.on_raw_input(raw);
        self.capture = Some(driver);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(line: &str) -> Cmd {
        let cmds = parse_script(line).expect("line should parse");
        assert_eq!(cmds.len(), 1, "expected exactly one command from {line:?}");
        cmds.into_iter().next().unwrap()
    }

    fn parse_err(src: &str) -> ParseError {
        parse_script(src).expect_err("script should be rejected")
    }

    #[test]
    fn every_command_parses() {
        let script = r#"
            wait_ms 250
            type "hello world"
            type "fast" cps 30
            clear_query
            focus_search
            window 500 350
            tab search
            tab manage
            tab duplicates
            tab logs
            tab help
            wait_index_running
            wait_index_running max 15000
            wait_index_idle max 13000
            wait_search_done max 6000
            wait_dups_done
            record_start manage-indexing
            record_stop
            screenshot query-highlight.v2
            quit
        "#;
        let cmds = parse_script(script).expect("script should parse");
        assert_eq!(
            cmds,
            vec![
                Cmd::WaitMs(250),
                Cmd::Type {
                    text: "hello world".to_string(),
                    cps: 7.0
                },
                Cmd::Type {
                    text: "fast".to_string(),
                    cps: 30.0
                },
                Cmd::ClearQuery,
                Cmd::FocusSearch,
                Cmd::Window { w: 500.0, h: 350.0 },
                Cmd::Tab(Tab::Search),
                Cmd::Tab(Tab::Manage),
                Cmd::Tab(Tab::Duplicates),
                Cmd::Tab(Tab::Logs),
                Cmd::Tab(Tab::Help),
                Cmd::WaitIndexRunning { max_ms: None },
                Cmd::WaitIndexRunning {
                    max_ms: Some(15000)
                },
                Cmd::WaitIndexIdle {
                    max_ms: Some(13000)
                },
                Cmd::WaitSearchDone { max_ms: Some(6000) },
                Cmd::WaitDupsDone { max_ms: None },
                Cmd::RecordStart("manage-indexing".to_string()),
                Cmd::RecordStop,
                Cmd::Screenshot("query-highlight.v2".to_string()),
                Cmd::Quit,
            ]
        );
    }

    #[test]
    fn string_escapes_and_hash_inside_strings() {
        assert_eq!(
            parse_one(r#"type "say \"hi\" \\ done""#),
            Cmd::Type {
                text: r#"say "hi" \ done"#.to_string(),
                cps: 7.0
            }
        );
        // `#` inside a quoted string is content, not a comment.
        assert_eq!(
            parse_one(r##"type "a # b""##),
            Cmd::Type {
                text: "a # b".to_string(),
                cps: 7.0
            }
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let cmds =
            parse_script("\n# a full-line comment\n   \nquit # trailing comment\n#another\n")
                .expect("should parse");
        assert_eq!(cmds, vec![Cmd::Quit]);
    }

    #[test]
    fn errors_carry_the_right_line_number() {
        let e = parse_err("wait_ms 100\nquit\nfrobnicate\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("frobnicate"), "got: {}", e.msg);
    }

    #[test]
    fn unclosed_string_is_rejected() {
        let e = parse_err("type \"never closed\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("unclosed"), "got: {}", e.msg);
    }

    #[test]
    fn unknown_escape_is_rejected() {
        let e = parse_err(r#"type "a\nb""#);
        assert!(e.msg.contains("escape"), "got: {}", e.msg);
    }

    #[test]
    fn non_numeric_int_is_rejected() {
        let e = parse_err("wait_ms soon");
        assert!(e.msg.contains("integer"), "got: {}", e.msg);
        let e = parse_err("wait_index_idle max never");
        assert!(e.msg.contains("integer"), "got: {}", e.msg);
    }

    #[test]
    fn names_with_path_separators_are_rejected() {
        for bad in [
            "screenshot ../escape",
            "screenshot a/b",
            "record_start a\\b",
        ] {
            let e = parse_err(bad);
            assert!(e.msg.contains("invalid name"), "{bad:?} got: {}", e.msg);
        }
    }

    #[test]
    fn missing_arguments_are_rejected() {
        assert!(parse_err("wait_ms").msg.contains("missing"));
        assert!(parse_err("type").msg.contains("missing"));
        assert!(parse_err("tab").msg.contains("missing"));
        assert!(parse_err("screenshot").msg.contains("missing"));
        assert!(parse_err("window").msg.contains("missing"));
        assert!(parse_err("window 500").msg.contains("missing"));
    }

    #[test]
    fn degenerate_window_sizes_are_rejected() {
        assert!(parse_err("window 0 350").msg.contains("positive"));
        assert!(parse_err("window 500 0").msg.contains("positive"));
        assert!(parse_err("window 500 -1").msg.contains("integer"));
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let e = parse_err("quit now");
        assert!(e.msg.contains("unexpected"), "got: {}", e.msg);
        let e = parse_err("wait_ms 100 200");
        assert!(e.msg.contains("unexpected"), "got: {}", e.msg);
    }

    #[test]
    fn unknown_tab_and_bad_cps_are_rejected() {
        assert!(parse_err("tab settings").msg.contains("unknown tab"));
        assert!(parse_err(r#"type "x" cps 0"#).msg.contains("positive"));
        assert!(parse_err(r#"type "x" cps -3"#).msg.contains("positive"));
    }

    #[test]
    fn unquoted_type_text_is_rejected() {
        let e = parse_err("type hello");
        assert!(e.msg.contains("quoted"), "got: {}", e.msg);
    }

    /// The scenario that ships in packaging/ must always parse — this pins
    /// the file to the grammar so neither can drift without failing tests.
    #[test]
    fn the_shipped_scenario_parses() {
        let src = include_str!("../../../packaging/capture-scenario.txt");
        let cmds = parse_script(src).expect("packaging/capture-scenario.txt should parse");
        assert!(
            cmds.len() > 10,
            "scenario looks truncated: {} commands",
            cmds.len()
        );
        assert_eq!(
            cmds.last(),
            Some(&Cmd::Quit),
            "scenario should end with quit"
        );
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        for i in 0..10_000 {
            let j = jitter(i);
            assert!((0.75..=1.25).contains(&j), "jitter({i}) = {j}");
            assert_eq!(j, jitter(i));
        }
    }
}
