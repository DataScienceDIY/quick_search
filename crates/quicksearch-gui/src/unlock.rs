//! Startup gate for password-protected indexes: while locked, [`Gate`]
//! renders a full-window unlock screen and starts none of the backend.
//!
//! Password hygiene: the typed password moves into a [`Zeroizing`] buffer on
//! submit and is dropped by the worker that derives the key; egui's undo
//! history for the field, which copies the text, is removed but not wiped.
//! Only the derived key crosses the channel back.

use std::path::Path;
use std::sync::mpsc;

use quicksearch_core::config::{Config, SecurityConfig};
use quicksearch_core::db;
use quicksearch_core::security::{derive_key, IndexKey};
use zeroize::{Zeroize, Zeroizing};

use crate::app::QuickSearchApp;
use crate::keychain;
use crate::ui_util::hint;

/// Where this session's index key came from, for wording that refers to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Unprotected,
    Prompt,
    /// Supplied by the OS keychain, with no prompt shown.
    Keychain,
}

#[allow(clippy::large_enum_variant)]
pub enum Gate {
    Locked(UnlockScreen),
    Running(Box<QuickSearchApp>),
}

impl Gate {
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

    /// Act on a search shortcut, whether QuickSearch's own registration fired
    /// it or a `--toggle` process relayed the desktop's. Handled here because
    /// while locked the unlock screen *is* the window.
    fn handle_activation(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if let Gate::Running(app) = self {
            // The shortcut must not reshuffle the window under a key capture.
            // The flag is left set, not consumed: capture ends on a key
            // event, which repaints, and the press is acted on that frame.
            if app.capturing_hotkey() {
                return;
            }
        }
        if !crate::activate::take_pending() {
            return;
        }
        // The token travels with the press: on Wayland it is the
        // compositor's permission for the raise below to take focus.
        let token = crate::activate::take_token();
        if let Gate::Running(app) = self {
            app.activate_search(ctx);
        }
        crate::activate::raise(ctx, frame, token.as_deref());
    }
}

impl eframe::App for Gate {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.handle_activation(ctx, frame);
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

    /// Capture runs use an unprotected config, so the gate is `Running`
    /// from frame one.
    #[cfg(feature = "capture")]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Gate::Running(app) = self {
            app.capture_raw_input(raw_input);
        }
    }
}

/// Try the keychain before any window exists; `true` skips the prompt
/// entirely. Only a key remembered under the config's salt is tried
/// ([`keychain::remembered_key`]); with no index on disk it is then taken
/// without a further check (see [`remembered_key_fits`]).
pub fn try_keychain_unlock(cfg: &Config) -> bool {
    if !cfg.security.use_keychain {
        return false;
    }
    let Ok(salt) = cfg.security.salt_bytes() else {
        return false;
    };
    let db_path = cfg.resolved_database_path();
    let text = match keychain::load_entry(&db_path.to_string_lossy()) {
        Ok(Some(text)) => text,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("warning: {}", e);
            return false;
        }
    };
    // A stale entry, such as one an earlier password change failed to
    // replace, prompts.
    let Ok(key) = keychain::remembered_key(&text, &salt) else {
        return false;
    };
    db::set_process_key(Some(key));
    if remembered_key_fits(&db_path) {
        return true;
    }
    // Stale entry or foreign database: fall back to the prompt.
    db::set_process_key(None);
    false
}

/// Whether the process key may stand for the index at `db_path`. With no
/// index (after Clear index, say) there is nothing to check it against, and
/// dropping it would let a mistyped password replace it. It was remembered
/// under the config's salt, which every password change replaces, so it is
/// the current password's.
fn remembered_key_fits(db_path: &Path) -> bool {
    !db_path.exists() || db::verify_process_key(&db_path.to_string_lossy()).is_ok()
}

enum Mode {
    Unlock,
    /// Protection is on but no index exists yet, so nothing can check the
    /// password: whatever is typed becomes the key under the existing salt,
    /// which is why it is asked for twice.
    Create,
    /// The salt is missing or corrupt; only the reset escape hatch applies.
    BrokenSalt(String),
}

pub struct UnlockScreen {
    cfg: Config,
    config_error: Option<String>,
    initial_query: Option<String>,
    mode: Mode,
    password: String,
    /// The second entry [`Mode::Create`] asks for.
    confirm: String,
    remember: bool,
    error: Option<String>,
    /// Set at startup and after a failed attempt, not every frame: an
    /// unconditional re-focus traps the caret.
    focus_password: bool,
    /// In-flight Argon2 derivation (+ verification) on a worker thread.
    job: Option<mpsc::Receiver<Result<IndexKey, String>>>,
    /// The "Forgot password…" reset is asking for confirmation.
    confirming_reset: bool,
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
            confirm: String::new(),
            remember,
            error: None,
            focus_password: true,
            job: None,
            confirming_reset: false,
        }
    }

    fn update(&mut self, ctx: &egui::Context) -> Option<QuickSearchApp> {
        if let Some(result) = self.poll_job() {
            match result {
                Ok(key) => return self.unlocked(ctx, key),
                Err(e) => {
                    // Only one of the three tagged-mismatch causes is a wrong
                    // password; collapsing them tells a user with the right
                    // password that it is wrong, and the only button on this
                    // screen deletes their index. Matched on the cause, not
                    // the wording: the message carries the database path.
                    self.error = Some(match db::key_mismatch_parts(&e) {
                        Some((db::KeyMismatch::WrongPassword, _)) => "Wrong password.".to_string(),
                        Some((_, detail)) => detail.to_string(),
                        None => e,
                    });
                    // The field was cleared on submit; put the caret back.
                    self.focus_password = true;
                }
            }
        }

        // The build id identifies a machine that never got past the
        // password. Declared before the central panel, as egui requires.
        egui::TopBottomPanel::bottom("version-bar").show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(hint(crate::version::BUILD_ID))
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
                        ui.label(hint(
                            "The new index will be encrypted with the password you \
                                 enter here.",
                        ));
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
                    let confirm = matches!(self.mode, Mode::Create).then(|| {
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.confirm)
                                .id(confirm_field_id())
                                .password(true)
                                .hint_text("Confirm password")
                                .desired_width(240.0),
                        )
                    });
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
                    // Enter submits from the last field; in the first of two
                    // it moves on to the second.
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let entered = |field: &egui::Response| field.lost_focus() && enter;
                    let last = match &confirm {
                        Some(second) => {
                            if entered(&field) {
                                second.request_focus();
                            }
                            second
                        }
                        None => &field,
                    };
                    submitted = clicked || entered(last);
                    if self.focus_password && !busy {
                        field.request_focus();
                        self.focus_password = false;
                    }
                });
                if busy {
                    ui.add_space(6.0);
                    ui.spinner();
                    ui.label(hint("Deriving key…"));
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
                    self.confirming_reset = true;
                }
            });
        });

        if submitted && self.job.is_none() {
            self.submit(ctx);
        }
        if self.confirming_reset {
            if let Some(app) = self.confirm_reset_ui(ctx) {
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

    fn submit(&mut self, ctx: &egui::Context) {
        self.error = None;
        let Ok(salt) = self.cfg.security.salt_bytes() else {
            return; // BrokenSalt mode never reaches submit
        };
        let password = Zeroizing::new(std::mem::take(&mut self.password));
        let confirm = Zeroizing::new(std::mem::take(&mut self.confirm));
        purge_text_state(ctx, &[pw_field_id(), confirm_field_id()]);
        if matches!(self.mode, Mode::Create) {
            let refused = if password.is_empty() {
                Some("The password may not be empty.")
            } else if password != confirm {
                Some("The passwords do not match.")
            } else {
                None
            };
            if let Some(why) = refused {
                self.error = Some(why.to_string());
                // Both fields were cleared; put the caret back.
                self.focus_password = true;
                return;
            }
        }

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

    fn unlocked(&mut self, ctx: &egui::Context, key: IndexKey) -> Option<QuickSearchApp> {
        let db_path = self.cfg.resolved_database_path();
        // Non-fatal: unlock proceeds, but say that the key could not be
        // stored, or that a key remembered until now could not be forgotten.
        // Read before the preference is overwritten below.
        let was_remembered = self.cfg.security.use_keychain;
        let salt = self.cfg.security.salt.as_deref().unwrap_or_default();
        let remembered = self
            .remember
            .then(|| keychain::entry_text(salt, &Zeroizing::new(key.to_hex())));
        if let Err(e) = keychain::sync(
            &db_path.to_string_lossy(),
            was_remembered,
            remembered.as_deref().map(String::as_str),
        ) {
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
        // The forgot-password path may have disabled protection on the way.
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

    /// The index is derived data: deleting it and disabling protection
    /// loses nothing but time. `Some(app)` when the reset happened.
    fn confirm_reset_ui(&mut self, ctx: &egui::Context) -> Option<QuickSearchApp> {
        let db_path = self.cfg.resolved_database_path();
        let result = crate::ui_util::centered_modal(ctx, "Reset the index?", |ui| {
            ui.set_max_width(420.0);
            ui.label(
                "Without the password the index cannot be read. This deletes \
                 the index database below and turns password protection off. \
                 Your files are not touched; the index is rebuilt by indexing \
                 again.",
            );
            ui.monospace(db_path.display().to_string());
            ui.horizontal(|ui| {
                let mut launched = None;
                let delete = egui::RichText::new("Delete index & disable protection")
                    .color(ui.visuals().error_fg_color);
                if ui.button(delete).clicked() {
                    // No coordinator exists while the gate is locked, so a
                    // plain delete is safe.
                    if let Err(e) = db::remove_index_files(&db_path) {
                        self.error = Some(e);
                    } else {
                        // A surviving keychain entry would point at nothing.
                        let was_remembered = self.cfg.security.use_keychain;
                        if let Err(e) =
                            keychain::sync(&db_path.to_string_lossy(), was_remembered, None)
                        {
                            self.config_error = Some(e);
                        }
                        db::set_process_key(None);
                        self.cfg.security = SecurityConfig::default();
                        if let Err(e) = self.cfg.save() {
                            self.config_error = Some(e);
                        }
                        launched = self.launch(ctx);
                    }
                    return (launched, true);
                }
                (launched, ui.button("Cancel").clicked())
            })
            .inner
        });
        let (launched, close) = result.unwrap_or((None, false));
        if close {
            self.confirming_reset = false;
        }
        launched
    }
}

impl Drop for UnlockScreen {
    fn drop(&mut self) {
        self.password.zeroize();
        self.confirm.zeroize();
    }
}

fn pw_field_id() -> egui::Id {
    egui::Id::new("unlock-password")
}

fn confirm_field_id() -> egui::Id {
    egui::Id::new("unlock-password-confirm")
}

/// Removes egui's undo history for the fields (copies of the text; freed,
/// not wiped).
pub(crate) fn purge_text_state(ctx: &egui::Context, ids: &[egui::Id]) {
    ctx.data_mut(|d| {
        for &id in ids {
            d.remove::<egui::text_edit::TextEditState>(id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: egui::Vec2 = egui::vec2(900.0, 600.0);

    /// A protected config whose salt parses, so the mode is not `BrokenSalt`.
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
        frame_with(ctx, screen, Vec::new());
    }

    fn frame_with(
        ctx: &egui::Context,
        screen: &mut UnlockScreen,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let input = crate::test_ui::raw_input(SCREEN, events);
        ctx.run(input, |ctx| {
            // Nothing here unlocks, so the screen stays up.
            assert!(screen.update(ctx).is_none());
        })
    }

    /// `text` typed into the focused field, then Enter.
    fn typed(text: &str) -> Vec<egui::Event> {
        let mut events = vec![egui::Event::Text(text.to_string())];
        events.extend(crate::test_ui::key_press(
            egui::Key::Enter,
            egui::Modifiers::NONE,
        ));
        events
    }

    /// The caret starts in the password field, then can leave.
    #[test]
    fn the_password_field_takes_focus_once_and_then_releases_it() {
        let ctx = crate::test_ui::ctx();
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

        frame(&ctx, &mut screen);
        assert!(!ctx.memory(|m| m.has_focus(pw_field_id())));
    }

    /// The one case that *should* re-focus: the field was cleared on submit.
    #[test]
    fn a_failed_attempt_puts_the_caret_back() {
        let ctx = crate::test_ui::ctx();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        frame(&ctx, &mut screen);
        ctx.memory_mut(|m| m.surrender_focus(pw_field_id()));
        frame(&ctx, &mut screen);
        assert!(!ctx.memory(|m| m.has_focus(pw_field_id())));

        screen.focus_password = true; // what the error path sets
        frame(&ctx, &mut screen);
        assert!(ctx.memory(|m| m.has_focus(pw_field_id())));
    }

    /// The build id is painted, not merely laid out: a panel declared after
    /// the central one would compile and show nothing.
    #[test]
    fn the_lock_screen_shows_the_build_id() {
        let ctx = crate::test_ui::ctx();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        let input = crate::test_ui::raw_input(SCREEN, Vec::new());

        let out = ctx.run(input, |ctx| {
            assert!(screen.update(ctx).is_none());
        });

        assert!(
            crate::test_ui::painted_text(&out)
                .iter()
                .any(|t| t == crate::version::BUILD_ID),
            "{} is not painted on the lock screen",
            crate::version::BUILD_ID
        );
    }

    /// With no index there is nothing to check a remembered key against, so
    /// it stands. Answered from the path alone: verifying would fail to open
    /// the missing file and drop the key, which is the bug this guards.
    #[test]
    fn a_remembered_key_fits_when_there_is_no_index() {
        let missing =
            std::env::temp_dir().join(format!("qs-unlock-missing-{}.sqlite", std::process::id()));
        assert!(!missing.exists());
        assert!(remembered_key_fits(&missing));
    }

    /// A file at the path is checked, and one the key cannot open refuses it
    /// (whatever key this process holds: garbage opens under none).
    #[test]
    fn a_remembered_key_that_does_not_open_the_index_does_not_fit() {
        let path =
            std::env::temp_dir().join(format!("qs-unlock-garbage-{}.sqlite", std::process::id()));
        std::fs::write(&path, [0xa5u8; 4096]).unwrap();
        let fits = remembered_key_fits(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(!fits);
    }

    /// Nothing checks a new index's password, so Create asks for it twice;
    /// an existing index checks it, so Unlock asks once.
    #[test]
    fn only_create_asks_for_the_password_twice() {
        let ctx = crate::test_ui::ctx();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        assert!(matches!(screen.mode, Mode::Create));
        let painted = crate::test_ui::painted_text(&frame_with(&ctx, &mut screen, Vec::new()));
        assert!(
            painted.contains(&"Confirm password".to_string()),
            "{painted:?}"
        );

        screen.mode = Mode::Unlock;
        let painted = crate::test_ui::painted_text(&frame_with(&ctx, &mut screen, Vec::new()));
        assert!(
            !painted.contains(&"Confirm password".to_string()),
            "{painted:?}"
        );
    }

    /// Enter in the first field moves on to the second instead of submitting
    /// half the form; Enter there submits, and a mismatch is refused.
    #[test]
    fn create_takes_enter_through_both_fields() {
        let ctx = crate::test_ui::ctx();
        let mut screen = UnlockScreen::new(locked_config(), None, None);
        frame(&ctx, &mut screen);

        frame_with(&ctx, &mut screen, typed("hunter2"));
        assert_eq!(screen.password, "hunter2");
        assert_eq!(screen.error, None, "Enter in the first field submitted");
        assert!(
            ctx.memory(|m| m.has_focus(confirm_field_id())),
            "the caret did not move on to the confirmation"
        );

        frame_with(&ctx, &mut screen, typed("hunter3"));
        assert_eq!(screen.error.as_deref(), Some("The passwords do not match."));
        assert!(screen.job.is_none());
    }

    /// A typo in either entry would become the key, so they must agree; both
    /// are cleared for a retry, egui's copies included, and an empty password
    /// is refused first.
    #[test]
    fn create_refuses_passwords_that_differ_or_are_empty() {
        for (password, confirm, refusal) in [
            ("hunter2", "hunter3", "The passwords do not match."),
            ("hunter2", "", "The passwords do not match."),
            ("", "hunter2", "The password may not be empty."),
            ("", "", "The password may not be empty."),
        ] {
            let ctx = crate::test_ui::ctx();
            let mut screen = UnlockScreen::new(locked_config(), None, None);
            screen.password = password.to_string();
            screen.confirm = confirm.to_string();
            screen.focus_password = false;
            let fields = [pw_field_id(), confirm_field_id()];
            for id in fields {
                egui::text_edit::TextEditState::default().store(&ctx, id);
            }
            screen.submit(&ctx);
            assert_eq!(
                screen.error.as_deref(),
                Some(refusal),
                "{password:?} {confirm:?}"
            );
            assert!(
                screen.job.is_none(),
                "{password:?} {confirm:?} went on to derive"
            );
            assert!(screen.password.is_empty() && screen.confirm.is_empty());
            for id in fields {
                assert!(egui::text_edit::TextEditState::load(&ctx, id).is_none());
            }
            assert!(screen.focus_password, "the caret is not put back");
        }
    }
}
