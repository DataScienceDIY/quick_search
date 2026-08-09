//! Scripted self-capture (feature `capture`): the app drives itself through
//! a scenario so `packaging/capture.sh` can regenerate the website
//! screenshots and screencasts as the software changes.
//!
//! With `QS_CAPTURE_SCRIPT` set, a [`CaptureDriver`] runs one command at a
//! time: keystrokes are injected as real `egui::Event::Text` input, tabs
//! switch through the same pending-nav route a click takes, waits watch
//! live indexer/search/duplicates state, and screenshots and video frames
//! are read back from the GL framebuffer via `ViewportCommand::Screenshot`.
//!
//! Captures come from inside the app: screen-grabbing depends on the
//! display server — black frames from a rootless XWayland, portals on
//! Wayland proper, whatever overlaps the window — while the framebuffer
//! readback works identically on X11, Wayland and Windows and sees nothing
//! but the app. The scenario grammar and command list live in [`script`].
//!
//! Exit codes, for the orchestrator: 2 script parse error, 3 wait timeout,
//! 4 screenshot/recording I/O failure.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use quicksearch_core::indexing::IndexingStatus;

use crate::app::QuickSearchApp;

mod script;

use script::{parse_script, Cmd};

// --- Driver ---

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
    /// Match-cell row the pointer is pinned to (`hover_match`), and the
    /// on-screen position it resolved to on the last rendered frame.
    hover: Option<usize>,
    hover_pos: Option<egui::Pos2>,
    /// The position last injected. Kept separate from `hover_pos` because
    /// injection must be edge-triggered: egui resets its pointer-stillness
    /// clock on *every* `PointerMoved` event, moved or not, and tooltips
    /// only appear once that clock outlives the tooltip delay.
    hover_injected: Option<egui::Pos2>,
    /// One-shot `Event::PointerGone` injection, armed by `hover_off`.
    pointer_gone_pending: bool,
    out_dir: PathBuf,
    /// Set by `quit`; the app drops the driver once it is.
    pub(crate) finished: bool,
}

impl CaptureDriver {
    /// `None` unless `QS_CAPTURE_SCRIPT` names a scenario. A script that
    /// cannot be read or parsed exits immediately.
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
            hover: None,
            hover_pos: None,
            hover_injected: None,
            pointer_gone_pending: false,
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
        // keep its own clock ticking and recorded footage smooth.
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

        // Resolve the pinned hover against what the last frame rendered:
        // rows can move while results stream, and the tooltip should track
        // the cell, not a stale point.
        if let Some(n) = self.hover {
            self.hover_pos = app.capture_match_cell(n).map(|r| r.center());
        }

        let Some(cmd) = self.cmds.get(self.pc).cloned() else {
            self.quit(app, ctx);
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
            Cmd::HoverMatch(n) => {
                self.hover = Some(*n);
                self.hover_pos = None; // resolved from the next rendered frame
            }
            Cmd::HoverOff => {
                self.hover = None;
                self.hover_pos = None;
                self.hover_injected = None;
                self.pointer_gone_pending = true;
            }
            Cmd::Window { w, h } => {
                // ViewportCommand sizes are in egui points, which fold in
                // the UI zoom — divide it back out. The app's own 640x400
                // floor must be lowered first for compact clip sizes to take
                // effect, and the resize lands asynchronously (the window
                // manager has the last word).
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
            Cmd::Quit => self.quit(app, ctx),
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
            // Done once the cell exists on screen and the pointer is on it.
            Cmd::HoverMatch(_) => self.hover_pos.is_some(),
            Cmd::ClearQuery
            | Cmd::FocusSearch
            | Cmd::HoverOff
            | Cmd::Window { .. }
            | Cmd::Tab(_)
            | Cmd::RecordStart(_)
            | Cmd::RecordStop
            | Cmd::Quit => true,
            Cmd::WaitIndexRunning { max_ms } => {
                capped(max_ms)
                    || matches!(
                        app.capture_indexing_status(),
                        IndexingStatus::Preparing { .. }
                            | IndexingStatus::Running { .. }
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

    fn quit(&mut self, app: &mut QuickSearchApp, ctx: &egui::Context) {
        self.stop_recorder();
        // Answer the close guards up front: a still-running reconcile's
        // modal would otherwise hold the window open until the run timed out.
        app.capture_confirm_quit();
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

        if self.pointer_gone_pending {
            self.pointer_gone_pending = false;
            raw.events.push(egui::Event::PointerGone);
        }
        if let Some(pos) = self.hover_pos {
            // Edge-triggered on purpose: egui resets its pointer-stillness
            // clock on every `PointerMoved` event even at an unchanged
            // position, and the tooltip appears only after that clock
            // outlives the tooltip delay. Inject when the pin moves — or
            // after a real OS pointer event, which would otherwise unpin us
            // (appending after it means the pin wins the frame).
            let foreign_pointer = raw.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::PointerGone
                        | egui::Event::PointerButton { .. }
                )
            });
            if foreign_pointer || self.hover_injected != Some(pos) {
                raw.events.push(egui::Event::PointerMoved(pos));
                self.hover_injected = Some(pos);
            }
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
    /// tail and exits — then wait, with a kill as backstop.
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
        | Cmd::HoverOff
        | Cmd::Window { .. }
        | Cmd::Tab(_)
        | Cmd::RecordStart(_)
        | Cmd::RecordStop
        | Cmd::Quit => 10_000,
        // Fails when the scenario asks for a row that never rendered.
        Cmd::HoverMatch(_) => 10_000,
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

// --- App glue ---

/// Take/call/put wrappers: the driver cannot stay a field of the app while
/// borrowing all of it.
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

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        for i in 0..10_000 {
            let j = jitter(i);
            assert!((0.75..=1.25).contains(&j), "jitter({i}) = {j}");
            assert_eq!(j, jitter(i));
        }
    }
}
