//! Startup gate for password-protected indexes.
//!
//! [`Gate`] wraps the real app: while locked it renders a full-window
//! unlock screen and starts none of the backend (no coordinator, no
//! watcher, no database opens). Only after the password verifies — or the
//! keychain supplied a working key before the window even opened — is
//! [`QuickSearchApp`] constructed.
//!
//! Password hygiene: the typed password moves into a [`Zeroizing`] buffer
//! on submit, travels to a worker thread that derives the key and drops
//! it, and the visible text-field state (including egui's undo buffer) is
//! purged. Only the derived key crosses the channel back.

use std::sync::mpsc;

use quicksearch_core::config::{Config, SecurityConfig};
use quicksearch_core::db;
use quicksearch_core::security::{derive_key, IndexKey};
use zeroize::{Zeroize, Zeroizing};

use crate::app::QuickSearchApp;
use crate::keychain;

/// Where this session's index key came from.
///
/// The app needs it for anything that *refers* to the key: telling someone
/// "the password you just entered" is wrong when they never typed one, because
/// the keychain answered before the window opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// The index is not password-protected.
    Unprotected,
    /// Typed at the unlock prompt during this session.
    Prompt,
    /// Supplied by the OS keychain, with no prompt shown.
    Keychain,
}

/// The application shell handed to eframe: locked (unlock screen) or
/// running (the real app).
// Exactly one of these exists for the lifetime of the process, and it is
// already boxed on the side that would matter.
#[allow(clippy::large_enum_variant)]
pub enum Gate {
    Locked(UnlockScreen),
    Running(Box<QuickSearchApp>),
}

impl Gate {
    /// Start unlocked: protection is off, or the keychain already
    /// provided a verified key.
    pub fn running(
        ctx: &egui::Context,
        cfg: Config,
        config_error: Option<String>,
        initial_query: Option<String>,
        key_source: KeySource,
    ) -> Result<Gate, String> {
        QuickSearchApp::new(ctx, cfg, config_error, initial_query, key_source)
            .map(|app| Gate::Running(Box::new(app)))
    }

    pub fn locked(
        cfg: Config,
        config_error: Option<String>,
        initial_query: Option<String>,
    ) -> Gate {
        Gate::Locked(UnlockScreen::new(cfg, config_error, initial_query))
    }

    /// Act on the system-wide search shortcut, if it fired since the last
    /// frame: bring the window back to the front and, once past the gate,
    /// put the caret in the search box.
    ///
    /// Handled here rather than inside the app because while the index is
    /// locked the unlock screen *is* the window, and a shortcut that did
    /// nothing until the password was typed would be the wrong half of the
    /// feature.
    ///
    /// Getting the window in front of the user is [`crate::hotkey::raise`],
    /// which is not one line and explains why.
    fn handle_hotkey(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !crate::hotkey::take_fired() {
            return;
        }
        if let Gate::Running(app) = self {
            // The Options window is waiting for a key press to bind. Pressing
            // the shortcut that is currently held is how someone checks it
            // still works, and it must not reshuffle the window underneath
            // the dialog asking for its replacement.
            if app.capturing_hotkey() {
                return;
            }
            app.activate_search();
        }
        crate::hotkey::raise(ctx, frame);
    }
}

impl eframe::App for Gate {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.handle_hotkey(ctx, frame);
        match self {
            Gate::Running(app) => app.update(ctx, frame),
            Gate::Locked(screen) => {
                if let Some(app) = screen.update(ctx) {
                    *self = Gate::Running(Box::new(app));
                }
            }
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let Gate::Running(app) = self {
            app.on_exit(gl);
        }
    }

    /// The scripted capture driver injects keystrokes and harvests
    /// screenshots here, before egui sees the frame's input. While locked
    /// there is nothing to drive; capture runs use an unprotected config, so
    /// the gate is `Running` from the first frame.
    #[cfg(feature = "capture")]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Gate::Running(app) = self {
            app.capture_raw_input(raw_input);
        }
    }
}

/// Try to unlock with the keychain before any window exists. `true` means
/// the process key is installed and verified — skip the prompt entirely.
pub fn try_keychain_unlock(cfg: &Config) -> bool {
    if !cfg.security.use_keychain || cfg.security.salt_bytes().is_err() {
        return false;
    }
    let db_path = cfg.resolved_database_path();
    let hex = match keychain::load_key(&db_path.to_string_lossy()) {
        Ok(Some(hex)) => hex,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("warning: {}", e);
            return false;
        }
    };
    let Ok(key) = IndexKey::from_hex(&hex) else {
        return false;
    };
    db::set_process_key(Some(key));
    match db::verify_process_key(&db_path.to_string_lossy()) {
        Ok(()) => true,
        Err(_) => {
            // Stale entry or missing/foreign database file: fall back to
            // the prompt with a clean slate.
            db::set_process_key(None);
            false
        }
    }
}

/// What the unlock screen is being used for.
enum Mode {
    /// An index exists: the password must open it.
    Unlock,
    /// Protection is on but no index file exists yet, so the typed password
    /// becomes the one the new index is built under.
    ///
    /// Still not a *new* password: this mode is only reachable with a salt
    /// already in the config, so one was chosen previously and this is the
    /// user re-entering it. Choosing a genuinely new password happens in the
    /// Options window, which does its own confirmation.
    Create,
    /// `password_protected = true` but the salt is missing or corrupt; no
    /// password can help. Only the reset escape hatch applies.
    BrokenSalt(String),
}

pub struct UnlockScreen {
    cfg: Config,
    config_error: Option<String>,
    initial_query: Option<String>,
    mode: Mode,
    password: String,
    remember: bool,
    error: Option<String>,
    /// Put the caret in the password field on the next frame. Set once at
    /// startup and again after a failed attempt, so the user can retype
    /// straight away — but *not* every frame: re-focusing unconditionally
    /// traps the caret, and nothing else on the screen can be tabbed to or
    /// clicked into.
    focus_password: bool,
    /// In-flight Argon2 derivation (+ verification) on a worker thread.
    job: Option<mpsc::Receiver<Result<IndexKey, String>>>,
    forgot_confirm: bool,
}

impl UnlockScreen {
    fn new(
        cfg: Config,
        config_error: Option<String>,
        initial_query: Option<String>,
    ) -> UnlockScreen {
        let mode = match cfg.security.salt_bytes() {
            Err(e) => Mode::BrokenSalt(e),
            Ok(_) => {
                if cfg.resolved_database_path().exists() {
                    Mode::Unlock
                } else {
                    Mode::Create
                }
            }
        };
        let remember = cfg.security.use_keychain;
        UnlockScreen {
            cfg,
            config_error,
            initial_query,
            mode,
            password: String::new(),
            remember,
            error: None,
            focus_password: true,
            job: None,
            forgot_confirm: false,
        }
    }

    /// Render one frame; `Some(app)` when the gate opens.
    fn update(&mut self, ctx: &egui::Context) -> Option<QuickSearchApp> {
        if let Some(result) = self.poll_job() {
            match result {
                Ok(key) => return self.unlocked(ctx, key),
                Err(e) => {
                    self.error = Some(if e.starts_with(db::KEY_MISMATCH_PREFIX) {
                        "Wrong password.".to_string()
                    } else {
                        e
                    });
                    // The field was cleared on submit, so put the caret back
                    // in it rather than making the user click before retrying.
                    self.focus_password = true;
                }
            }
        }

        // The lock screen owns the whole window and so has no status bar to
        // carry the build id. Give it the same corner the unlocked app uses,
        // so a screenshot of a machine that never got past the password is
        // still identifiable. Declared before the central panel, as egui
        // requires.
        egui::TopBottomPanel::bottom("version-bar").show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(crate::version::BUILD_ID).small().weak())
                    .on_hover_text(crate::version::BUILD_ID_HINT);
            });
        });

        let mut submitted = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.25);
                ui.heading("QuickSearch");
                ui.add_space(12.0);
                match &self.mode {
                    Mode::BrokenSalt(reason) => {
                        ui.colored_label(ui.visuals().error_fg_color, reason);
                        ui.label(
                            egui::RichText::new(
                                "The index cannot be unlocked with this configuration.",
                            )
                            .small(),
                        );
                        return;
                    }
                    Mode::Unlock => {
                        ui.label("The search index is password-protected.");
                    }
                    Mode::Create => {
                        ui.label("Password protection is enabled, but no index exists yet.");
                        ui.label(
                            egui::RichText::new(
                                "The new index will be encrypted with the password you \
                                 enter here.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
                ui.add_space(8.0);

                let busy = self.job.is_some();
                ui.add_enabled_ui(!busy, |ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .id(pw_field_id())
                            .password(true)
                            .hint_text("Password")
                            .desired_width(240.0),
                    );
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.remember, "Remember on this device")
                        .on_hover_text(
                            "Stores the derived key (not the password) in the OS \
                             keychain and skips this prompt.",
                        );
                    ui.add_space(8.0);

                    let label = match self.mode {
                        Mode::Unlock => "Unlock",
                        _ => "Create index",
                    };
                    let clicked = ui.button(label).clicked();
                    let entered =
                        field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    submitted = clicked || entered;
                    if self.focus_password && !busy {
                        field.request_focus();
                        self.focus_password = false;
                    }
                });
                if busy {
                    ui.add_space(6.0);
                    ui.spinner();
                    ui.label(egui::RichText::new("Deriving key…").small().weak());
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                if let Some(error) = &self.error {
                    ui.add_space(6.0);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                if !matches!(self.mode, Mode::Create)
                    && ui.small_button("Forgot password…").clicked()
                {
                    self.forgot_confirm = true;
                }
            });
        });

        if submitted && self.job.is_none() {
            self.submit(ctx);
        }
        if self.forgot_confirm {
            if let Some(app) = self.forgot_confirm_ui(ctx) {
                return Some(app);
            }
        }
        None
    }

    fn poll_job(&mut self) -> Option<Result<IndexKey, String>> {
        let rx = self.job.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.job = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                Some(Err("key derivation thread died".to_string()))
            }
        }
    }

    /// Move the typed password off to the derivation thread and scrub the
    /// UI-side buffers.
    fn submit(&mut self, ctx: &egui::Context) {
        self.error = None;
        if matches!(self.mode, Mode::Create) && self.password.is_empty() {
            self.error = Some("The password may not be empty.".to_string());
            return;
        }
        let Ok(salt) = self.cfg.security.salt_bytes() else {
            return; // BrokenSalt mode never reaches submit
        };
        let password = Zeroizing::new(std::mem::take(&mut self.password));
        purge_text_state(ctx, pw_field_id());

        let verify_against = match self.mode {
            Mode::Unlock => Some(self.cfg.resolved_database_path()),
            _ => None,
        };
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let key = derive_key(&password, &salt);
            drop(password);
            db::set_process_key(Some(key.clone()));
            let result = match &verify_against {
                Some(db_path) => db::verify_process_key(&db_path.to_string_lossy()).map(|()| key),
                None => Ok(key),
            };
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        self.job = Some(rx);
    }

    /// The key verified (or a fresh index is being created): remember it if
    /// asked, persist the keychain preference, and start the real app.
    fn unlocked(&mut self, ctx: &egui::Context, key: IndexKey) -> Option<QuickSearchApp> {
        let db_path = self.cfg.resolved_database_path();
        if self.remember {
            if let Err(e) = keychain::store_key(&db_path.to_string_lossy(), &key.to_hex()) {
                // Non-fatal: unlock proceeds, the preference just can't
                // stick. Surface it in the running app's banner.
                self.config_error = Some(e);
            }
        } else if let Err(e) = keychain::delete_key(&db_path.to_string_lossy()) {
            // Same treatment as the store half above: unlock proceeds, but
            // the banner says the old key is still on the keychain.
            self.config_error = Some(e);
        }
        if self.cfg.security.use_keychain != self.remember {
            self.cfg.security.use_keychain = self.remember;
            if let Err(e) = self.cfg.save() {
                self.config_error = Some(e);
            }
        }
        self.launch(ctx)
    }

    /// Construct the real app; on failure stay locked and show why.
    fn launch(&mut self, ctx: &egui::Context) -> Option<QuickSearchApp> {
        // Reaching here means either the password was just typed, or the
        // "forgot password" path disabled protection on the way. The config
        // says which, and no caller has to remember to pass it.
        let key_source = if self.cfg.security.password_protected {
            KeySource::Prompt
        } else {
            KeySource::Unprotected
        };
        match QuickSearchApp::new(
            ctx,
            self.cfg.clone(),
            self.config_error.take(),
            self.initial_query.take(),
            key_source,
        ) {
            Ok(app) => Some(app),
            Err(e) => {
                self.error = Some(format!("Failed to start: {}", e));
                None
            }
        }
    }

    /// "Forgot password" confirmation. The index is derived data: deleting
    /// it and disabling protection loses nothing but time. `Some(app)` when
    /// the reset happened and the app launched unprotected.
    fn forgot_confirm_ui(&mut self, ctx: &egui::Context) -> Option<QuickSearchApp> {
        let mut launched = None;
        let mut close = false;
        egui::Window::new("Reset the index?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(
                    "Without the password the index cannot be read. This deletes \
                     the index and turns password protection off. Your files are \
                     not touched; the index is rebuilt by indexing again.",
                );
                ui.horizontal(|ui| {
                    if ui
                        .button(
                            egui::RichText::new("Delete index & disable protection")
                                .color(ui.visuals().error_fg_color),
                        )
                        .clicked()
                    {
                        let db_path = self.cfg.resolved_database_path();
                        if let Err(e) = delete_index_files(&db_path) {
                            self.error = Some(e);
                        } else {
                            // The index files are already gone; a surviving
                            // entry would point at a database that no longer
                            // exists. Non-fatal, but not silent.
                            if let Err(e) = keychain::delete_key(&db_path.to_string_lossy()) {
                                self.config_error = Some(e);
                            }
                            db::set_process_key(None);
                            self.cfg.security = SecurityConfig::default();
                            if let Err(e) = self.cfg.save() {
                                self.config_error = Some(e);
                            }
                            launched = self.launch(ctx);
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.forgot_confirm = false;
        }
        launched
    }
}

impl Drop for UnlockScreen {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

fn pw_field_id() -> egui::Id {
    egui::Id::new("unlock-password")
}

/// Drop egui's retained state for a password field — its text buffer and
/// undo history — so the plaintext doesn't outlive the submit.
fn purge_text_state(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
}

/// Delete the index and its WAL/SHM/journal sidecars. No coordinator
/// exists while the gate is locked, so plain filesystem deletes are safe.
fn delete_index_files(db_path: &std::path::Path) -> Result<(), String> {
    match quicksearch_core::platform::remove_file_retrying(db_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Failed to delete {}: {}", db_path.display(), e)),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let name = format!(
            "{}{}",
            db_path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            suffix
        );
        let _ = quicksearch_core::platform::remove_file_retrying(&db_path.with_file_name(name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unlock screen is the whole window, so its viewport is the window's.
    const SCREEN: egui::Vec2 = egui::vec2(900.0, 600.0);

    /// A protected config whose salt parses, so the screen lands in a real
    /// password mode rather than `BrokenSalt`.
    fn locked_config() -> Config {
        let mut cfg = Config::default();
        cfg.security.password_protected = true;
        cfg.security.salt = Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string());
        cfg.paths.database_path = std::env::temp_dir()
            .join(format!("qs-unlock-test-{}.sqlite", std::process::id()))
            .to_string_lossy()
            .into_owned();
        cfg
    }

    fn frame(ctx: &egui::Context, screen: &mut UnlockScreen) {
        let input = crate::test_ui::raw_input(SCREEN, Vec::new());
        let _ = ctx.run(input, |ctx| {
            // `update` only builds the app on a successful unlock, which needs
            // a derived key — so with nothing submitted this stays on screen.
            assert!(screen.update(ctx).is_none());
        });
    }

    /// The caret starts in the password field, and — the regression — can then
    /// leave it.
    ///
    /// The screen used to call `request_focus()` on every frame the field did
    /// not have focus, which yanked the caret back the instant anything else
    /// took it. Nothing else on the screen could be tabbed to or clicked into.
    #[test]
    fn the_password_field_takes_focus_once_and_then_releases_it() {
        let ctx = egui::Context::default();
        let mut screen = UnlockScreen::new(locked_config(), None, None);

        frame(&ctx, &mut screen);
        assert!(
            ctx.memory(|m| m.has_focus(pw_field_id())),
            "the caret should start in the password field"
        );

        // Whatever the user clicks or tabs to next takes focus away.
        ctx.memory_mut(|m| m.surrender_focus(pw_field_id()));
        frame(&ctx, &mut screen);
        assert!(
            !ctx.memory(|m| m.has_focus(pw_field_id())),
            "focus was stolen back; the caret is trapped in the password field"
        );

        // And it stays released across further frames.
        frame(&ctx, &mut screen);
        assert!(!ctx.memory(|m| m.has_focus(pw_field_id())));
    }

    /// A failed attempt is the one case that *should* re-focus: the field was
    /// cleared on submit, so the user would otherwise have to click before
    /// retyping.
    #[test]
    fn a_failed_attempt_puts_the_caret_back() {
        let ctx = egui::Context::default();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        frame(&ctx, &mut screen);
        ctx.memory_mut(|m| m.surrender_focus(pw_field_id()));
        frame(&ctx, &mut screen);
        assert!(!ctx.memory(|m| m.has_focus(pw_field_id())));

        screen.focus_password = true; // what the error path sets
        frame(&ctx, &mut screen);
        assert!(ctx.memory(|m| m.has_focus(pw_field_id())));
    }

    /// This screen is the whole window on a protected index, so the build id
    /// in its corner is the only thing identifying a machine that never got
    /// past the password. Assert it is painted rather than merely laid out —
    /// a panel declared after the central one would compile and show nothing.
    #[test]
    fn the_lock_screen_shows_the_build_id() {
        let ctx = egui::Context::default();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        let input = crate::test_ui::raw_input(SCREEN, Vec::new());

        let out = ctx.run(input, |ctx| {
            assert!(screen.update(ctx).is_none());
        });

        let painted = out.shapes.iter().any(|clipped| match &clipped.shape {
            egui::epaint::Shape::Text(text) => text.galley.text() == crate::version::BUILD_ID,
            _ => false,
        });
        assert!(
            painted,
            "{} is not painted on the lock screen",
            crate::version::BUILD_ID
        );
    }
}
