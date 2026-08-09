//! The security flow: enabling/disabling protection, key derivation,
//! and the prompt that drives both.

use super::*;

use crate::ui_util::centered_modal;

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
        rx: mpsc::Receiver<(SecurityConfig, IndexKey)>,
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

impl QuickSearchApp {
    /// Route a click in the Options window's Security block. Keychain
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
                    let remember = *remember;
                    purge_security_field_state(ctx);
                    let (tx, rx) = mpsc::channel();
                    let repaint = ctx.clone();
                    std::thread::spawn(move || {
                        let salt = generate_salt();
                        let key = derive_key(&password, &salt);
                        drop(password);
                        let new_security = SecurityConfig {
                            password_protected: true,
                            salt: Some(salt_to_hex(&salt)),
                            use_keychain: remember,
                        };
                        let _ = tx.send((new_security, key));
                        repaint.request_repaint();
                    });
                    self.security_prompt = Some(SecurityPrompt::Deriving { rx });
                }
            }
            SecurityPrompt::Deriving { rx } => match rx.try_recv() {
                Ok((new_security, key)) => {
                    self.security_prompt = Some(SecurityPrompt::ConfirmRebuild {
                        new_security,
                        new_key: Some(key),
                    });
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Not `centered_modal`: this one hides its title bar.
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

/// Drop egui's retained text-field state (buffer + undo history) for the
/// password dialog fields.
fn purge_security_field_state(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw1"));
        d.remove::<egui::text_edit::TextEditState>(egui::Id::new("security-pw2"));
    });
}
