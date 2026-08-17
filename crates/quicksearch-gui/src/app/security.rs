//! The security flow: enabling/disabling protection, key derivation,
//! and the prompt that drives both.

use super::*;

use quicksearch_core::security::SALT_LEN;

use crate::ui_util::{centered_modal, hint};

/// The two-step security flow: collect a password (enable/change), derive
/// its key off the UI thread, then confirm the mandatory index rebuild.
/// Disabling skips straight to the confirmation.
pub(super) enum SecurityPrompt {
    SetPassword {
        pw1: String,
        pw2: String,
        remember: bool,
        change: bool,
    },
    Deriving {
        rx: mpsc::Receiver<IndexKey>,
        /// Built with the salt the pending key is being derived from, so the
        /// two always describe each other.
        new_security: SecurityConfig,
    },
    ConfirmRebuild {
        new_security: SecurityConfig,
        new_key: Option<IndexKey>,
    },
}

impl Drop for SecurityPrompt {
    fn drop(&mut self) {
        if let SecurityPrompt::SetPassword { pw1, pw2, .. } = self {
            pw1.zeroize();
            pw2.zeroize();
        }
    }
}

/// The show-key flow: confirm the password, re-derive from it, then reveal
/// the installed key. Nothing here can change the key or the config.
pub(super) enum KeyPrompt {
    Confirm {
        pw: String,
        wrong: bool,
    },
    Deriving {
        rx: mpsc::Receiver<IndexKey>,
    },
    /// The key as displayed: `0x` followed by 64 hex digits.
    Reveal {
        display: String,
    },
}

impl Drop for KeyPrompt {
    fn drop(&mut self) {
        match self {
            KeyPrompt::Confirm { pw, .. } => pw.zeroize(),
            KeyPrompt::Reveal { display } => display.zeroize(),
            KeyPrompt::Deriving { .. } => {}
        }
    }
}

/// Derive a key off the UI thread. The password is consumed and dropped
/// there, so it never outlives the derivation.
fn spawn_derive(
    ctx: &egui::Context,
    password: Zeroizing<String>,
    salt: [u8; SALT_LEN],
) -> mpsc::Receiver<IndexKey> {
    let (tx, rx) = mpsc::channel();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        let key = derive_key(&password, &salt);
        drop(password);
        let _ = tx.send(key);
        repaint.request_repaint();
    });
    rx
}

/// Paint the spinner shown while a derivation runs. Not `centered_modal`:
/// this one hides its title bar.
fn deriving_window(ctx: &egui::Context) {
    egui::Window::new("Deriving key")
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Deriving key…");
            });
        });
    ctx.request_repaint_after(Duration::from_millis(100));
}

impl QuickSearchApp {
    /// Route a click in the Settings tab's Security block. Keychain
    /// toggles act immediately; everything else opens the two-step flow.
    pub(super) fn handle_security_action(&mut self, action: SecurityAction) {
        match action {
            SecurityAction::Enable | SecurityAction::ChangePassword => {
                self.security_prompt = Some(SecurityPrompt::SetPassword {
                    pw1: String::new(),
                    pw2: String::new(),
                    remember: self.cfg.security.use_keychain,
                    change: matches!(action, SecurityAction::ChangePassword),
                });
            }
            SecurityAction::Disable => {
                self.security_prompt = Some(SecurityPrompt::ConfirmRebuild {
                    new_security: SecurityConfig::default(),
                    new_key: None,
                });
            }
            SecurityAction::ShowKey => {
                self.key_prompt = Some(KeyPrompt::Confirm {
                    pw: String::new(),
                    wrong: false,
                });
            }
            SecurityAction::SetKeychain(remember) => {
                let db_path = self.cfg.resolved_database_path();
                if remember {
                    match db::process_key_hex() {
                        Some(hex) => {
                            if let Err(e) = keychain::store_key(&db_path.to_string_lossy(), &hex) {
                                self.config_error = Some(e);
                                return; // preference not saved either
                            }
                        }
                        None => {
                            // Unreachable while protected — the gate always
                            // installs a key before the app starts.
                            self.config_error =
                                Some("no key to remember; restart and unlock first".to_string());
                            return;
                        }
                    }
                } else if let Err(e) = keychain::delete_key(&db_path.to_string_lossy()) {
                    // The preference must not claim the key is gone while it
                    // is still on the keychain.
                    self.config_error = Some(e);
                    return;
                }
                self.cfg.security.use_keychain = remember;
                if let Err(e) = self.cfg.save() {
                    self.config_error = Some(e);
                }
            }
        }
    }

    /// Render the active security flow (drawn with the other modals).
    pub(super) fn security_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.security_prompt else {
            return;
        };
        match prompt {
            SecurityPrompt::SetPassword {
                pw1,
                pw2,
                remember,
                change,
            } => {
                let title = if *change {
                    "Change password"
                } else {
                    "Enable password protection"
                };
                let buttons = centered_modal(ctx, title, |ui| {
                    ui.set_max_width(360.0);
                    ui.add(
                        egui::TextEdit::singleline(pw1)
                            .id(egui::Id::new("security-pw1"))
                            .password(true)
                            .hint_text("Password")
                            .desired_width(240.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(pw2)
                            .id(egui::Id::new("security-pw2"))
                            .password(true)
                            .hint_text("Confirm password")
                            .desired_width(240.0),
                    );
                    ui.checkbox(remember, "Remember on this device")
                        .on_hover_text(
                            "Stores the derived key (not the password) in the OS \
                         keychain and skips the startup prompt.",
                        );
                    if !pw1.is_empty() && !pw2.is_empty() && pw1 != pw2 {
                        ui.colored_label(ui.visuals().error_fg_color, "Passwords do not match.");
                    }
                    ui.horizontal(|ui| {
                        let ok = !pw1.is_empty() && pw1 == pw2;
                        let submit = ui.add_enabled(ok, egui::Button::new("Continue")).clicked();
                        (submit, ui.button("Cancel").clicked())
                    })
                    .inner
                });
                let (submit, cancel) = buttons.unwrap_or((false, false));
                if cancel {
                    self.security_prompt = None; // Drop impl zeroizes
                    purge_security_field_state(ctx);
                } else if submit {
                    let password = Zeroizing::new(std::mem::take(pw1));
                    pw2.zeroize();
                    let salt = generate_salt();
                    let new_security = SecurityConfig {
                        password_protected: true,
                        salt: Some(salt_to_hex(&salt)),
                        use_keychain: *remember,
                    };
                    purge_security_field_state(ctx);
                    let rx = spawn_derive(ctx, password, salt);
                    self.security_prompt = Some(SecurityPrompt::Deriving { rx, new_security });
                }
            }
            SecurityPrompt::Deriving { rx, new_security } => match rx.try_recv() {
                Ok(key) => {
                    self.security_prompt = Some(SecurityPrompt::ConfirmRebuild {
                        new_security: new_security.clone(),
                        new_key: Some(key),
                    });
                }
                Err(mpsc::TryRecvError::Empty) => deriving_window(ctx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.config_error = Some("key derivation thread died".to_string());
                    self.security_prompt = None;
                }
            },
            SecurityPrompt::ConfirmRebuild {
                new_security,
                new_key,
            } => {
                let title = match (new_key.is_some(), self.cfg.security.password_protected) {
                    (false, _) => "Disable password protection?",
                    (true, false) => "Enable password protection?",
                    (true, true) => "Change password?",
                };
                let buttons = centered_modal(ctx, title, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        "Changing index encryption deletes the index and \
                         re-indexes everything. Searches return incomplete \
                         results until the rebuild finishes. Your files are \
                         not touched.",
                    );
                    ui.horizontal(|ui| {
                        let rebuild = egui::RichText::new("Delete & rebuild index")
                            .color(ui.visuals().error_fg_color);
                        (ui.button(rebuild).clicked(), ui.button("Cancel").clicked())
                    })
                    .inner
                });
                let (confirm, cancel) = buttons.unwrap_or((false, false));
                if cancel {
                    self.security_prompt = None;
                } else if confirm {
                    let new_security = new_security.clone();
                    let new_key = new_key.clone();
                    self.security_prompt = None;
                    self.apply_security_change(new_security, new_key);
                }
            }
        }
    }

    /// Render the show-key flow (drawn with the other modals). Only ever
    /// open while protection is on, so a salt and a process key both exist.
    pub(super) fn key_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.key_prompt else {
            return;
        };
        match prompt {
            KeyPrompt::Confirm { pw, wrong } => {
                let (submit, cancel) = confirm_key_modal(ctx, pw, *wrong);
                if cancel {
                    self.key_prompt = None; // Drop impl zeroizes
                    purge_security_field_state(ctx);
                } else if submit {
                    let password = Zeroizing::new(std::mem::take(pw));
                    purge_security_field_state(ctx);
                    match self.cfg.security.salt_bytes() {
                        Ok(salt) => {
                            let rx = spawn_derive(ctx, password, salt);
                            self.key_prompt = Some(KeyPrompt::Deriving { rx });
                        }
                        Err(e) => {
                            self.config_error = Some(e);
                            self.key_prompt = None;
                        }
                    }
                }
            }
            KeyPrompt::Deriving { rx } => match rx.try_recv() {
                Ok(key) => match db::process_key_hex() {
                    // What is shown is the installed key, not the derived
                    // one: it is the key that actually opens the index.
                    Some(installed) => match reveal_display(&installed, &key.to_hex()) {
                        Some(display) => {
                            self.key_prompt = Some(KeyPrompt::Reveal { display });
                        }
                        None => {
                            self.key_prompt = Some(KeyPrompt::Confirm {
                                pw: String::new(),
                                wrong: true,
                            });
                        }
                    },
                    None => {
                        // Unreachable while protected — the gate always
                        // installs a key before the app starts.
                        self.config_error =
                            Some("no key installed; restart and unlock first".to_string());
                        self.key_prompt = None;
                    }
                },
                Err(mpsc::TryRecvError::Empty) => deriving_window(ctx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.config_error = Some("key derivation thread died".to_string());
                    self.key_prompt = None;
                }
            },
            KeyPrompt::Reveal { display } => {
                let (copy, close) = reveal_key_modal(ctx, display);
                if copy {
                    ctx.copy_text(display.clone());
                }
                if close {
                    self.key_prompt = None; // Drop impl zeroizes
                }
            }
        }
    }

    /// Commit a confirmed security change: config, keychain, process key —
    /// in that order, before the rebuild so the fresh index is created
    /// under the new key (or none).
    fn apply_security_change(&mut self, new_security: SecurityConfig, new_key: Option<IndexKey>) {
        let db_path = self
            .cfg
            .resolved_database_path()
            .to_string_lossy()
            .into_owned();
        self.cfg.security = new_security;
        if let Err(e) = self.cfg.save() {
            self.config_error = Some(e);
        }
        match (&new_key, self.cfg.security.use_keychain) {
            (Some(key), true) => {
                if let Err(e) = keychain::store_key(&db_path, &key.to_hex()) {
                    self.config_error = Some(e);
                }
            }
            // Disabling protection, or "remember" off: no stored key may
            // survive pointing at the previous encryption state.
            _ => {
                if let Err(e) = keychain::delete_key(&db_path) {
                    self.config_error = Some(e);
                }
            }
        }
        db::set_process_key(new_key);
        self.backend.coordinator.rebuild_index();
        self.dups.state = DupState::NotLoaded;
    }
}

/// Id of the show-key confirmation field, shared by the widget and the
/// purge below.
const SHOW_KEY_FIELD: &str = "show-key-pw";

/// Drop egui's retained text-field state (buffer + undo history) for the
/// password dialog fields.
fn purge_security_field_state(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw1"));
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw2"));
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new(SHOW_KEY_FIELD));
    });
}

/// The display form of the installed key, or `None` when the password the
/// user typed does not derive it. Both arguments come from
/// [`IndexKey::to_hex`], which is always lowercase, so a plain comparison is
/// exact; nothing secret is learned from its timing, since the caller
/// already holds the guess.
fn reveal_display(installed_hex: &str, derived_hex: &str) -> Option<String> {
    (installed_hex == derived_hex).then(|| format!("0x{}", installed_hex))
}

/// Paint the password confirmation; `(submit, cancel)` from its buttons.
/// Free, like the reveal below, so both halves of the flow can be rendered
/// against a bare context.
fn confirm_key_modal(ctx: &egui::Context, pw: &mut String, wrong: bool) -> (bool, bool) {
    centered_modal(ctx, "Show database key", |ui| {
        ui.set_max_width(360.0);
        ui.label(
            "Confirm your password to show the raw key the index is \
             encrypted with.",
        );
        let field = ui.add(
            egui::TextEdit::singleline(pw)
                .id(egui::Id::new(SHOW_KEY_FIELD))
                .password(true)
                .hint_text("Password")
                .desired_width(240.0),
        );
        // On open, and again after a wrong attempt. Never steals focus from
        // something the user moved to themselves.
        if ui.memory(|m| m.focused().is_none()) {
            field.request_focus();
        }
        if wrong {
            ui.colored_label(ui.visuals().error_fg_color, "That password is not correct.");
        }
        ui.horizontal(|ui| {
            let ok = !pw.is_empty();
            // Enter in the field submits, like the unlock screen.
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let submit =
                ui.add_enabled(ok, egui::Button::new("Show key")).clicked() || (ok && entered);
            (submit, ui.button("Cancel").clicked())
        })
        .inner
    })
    .unwrap_or((false, false))
}

/// Paint the revealed key; `(copy, close)` from its buttons. A free function
/// rather than a method so it can be rendered against a bare context.
fn reveal_key_modal(ctx: &egui::Context, display: &str) -> (bool, bool) {
    centered_modal(ctx, "Database key", |ui| {
        ui.set_max_width(420.0);
        ui.label(
            "This is the SQLCipher raw key for the index. Anyone holding it can \
             read the index without the password.",
        );
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(display).monospace());
        });
        ui.add_space(6.0);
        ui.label(hint(
            "Other SQLCipher tools take the key in this form. A copy stays on the \
             clipboard until something else replaces it.",
        ));
        ui.add_space(6.0);
        ui.horizontal(|ui| (ui.button("Copy").clicked(), ui.button("Close").clicked()))
            .inner
    })
    .unwrap_or((false, false))
}

#[cfg(test)]
#[path = "security_tests.rs"]
mod tests;
