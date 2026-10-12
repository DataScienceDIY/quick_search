//! The security flow: enabling/disabling protection, key derivation, and
//! the prompts that drive both.

use super::*;

use quicksearch_core::security::SALT_LEN;

use crate::ui_util::{centered_modal, hint};
use crate::unlock::purge_text_state;

/// Collect a password, derive its key off the UI thread, confirm the
/// mandatory rebuild, then delete the old index before anything is
/// committed. Disabling skips straight to the confirmation.
pub(super) enum SecurityPrompt {
    SetPassword {
        pw1: String,
        pw2: String,
        remember: bool,
        change: bool,
    },
    Deriving {
        rx: mpsc::Receiver<IndexKey>,
        new_security: SecurityConfig,
    },
    ConfirmRebuild {
        new_security: SecurityConfig,
        new_key: Option<IndexKey>,
    },
    /// Waiting on the coordinator to delete the old index. The change is
    /// committed only once it is gone: a config and key switched over an
    /// index that survived would leave no password that opens it.
    Deleting {
        rx: mpsc::Receiver<Result<(), String>>,
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

/// Confirm the password, re-derive from it, reveal the installed key.
/// Nothing here can change the key or the config.
pub(super) enum KeyPrompt {
    Confirm { pw: String, wrong: bool },
    Deriving { rx: mpsc::Receiver<IndexKey> },
    Reveal { display: String },
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

/// The password is consumed and dropped on the worker, so it never
/// outlives the derivation.
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

/// A spinner beside `label`, for a wait the user can only sit out: a key
/// derivation, or the delete ahead of a security change. Repaints itself, so
/// the caller's poll keeps running.
fn deriving_window(ctx: &egui::Context, label: &str) {
    egui::Window::new(label)
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(label);
            });
        });
    ctx.request_repaint_after(Duration::from_millis(100));
}

/// Why `action` must not begin, if it must not. Every action but showing the
/// key ends in a config save, and a config that did not load cannot save
/// ([`Config::fallback`]); asked before anything else, since a security
/// change deletes the index ahead of its save, which would then fail and
/// leave the index lost for nothing.
fn refusal(cfg: &Config, action: SecurityAction) -> Option<&'static str> {
    let saves = !matches!(action, SecurityAction::ShowKey);
    (saves && !cfg.can_save()).then_some("Not changed: the config file did not load")
}

/// One poll of the Deleting prompt's reply: `None` while the coordinator is
/// still at it.
fn delete_outcome(
    polled: Result<Result<(), String>, mpsc::TryRecvError>,
) -> Option<Result<(), String>> {
    match polled {
        Ok(deleted) => Some(deleted),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err("the indexer stopped".to_string())),
    }
}

/// The rebuild confirmation's title, and what the deleted index leaves
/// behind when that is worth saying. A delete only unlinks, so enabling
/// leaves plaintext in freed space and a change leaves pages the old
/// password still opens; disabling replaces nothing secret.
fn confirm_text(new_key: bool, protected: bool) -> (&'static str, Option<&'static str>) {
    match (new_key, protected) {
        (false, _) => ("Disable password protection?", None),
        (true, false) => (
            "Enable password protection?",
            Some(
                "The unencrypted index is not securely erased; parts \
                 can remain recoverable in freed disk space.",
            ),
        ),
        (true, true) => (
            "Change password?",
            Some(
                "The old index is not securely erased; recoverable parts \
                 of it still open with the old password.",
            ),
        ),
    }
}

impl QuickSearchApp {
    /// A security change is waiting on the index delete. Until it lands
    /// nothing else may save the config: the change saves its own copy next.
    pub(super) fn security_busy(&self) -> bool {
        matches!(self.security_prompt, Some(SecurityPrompt::Deleting { .. }))
    }

    pub(super) fn handle_security_action(&mut self, action: SecurityAction) {
        if self.security_busy() {
            return;
        }
        if let Some(why) = refusal(&self.cfg, action) {
            self.config_error = Some(why.to_string());
            return;
        }
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
                let security = self.cfg.security.clone();
                if let Err(e) = set_keychain(self, security, remember, db::process_key_hex()) {
                    self.config_error = Some(e);
                }
            }
        }
    }

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
                let fields = [egui::Id::new("security-pw1"), egui::Id::new("security-pw2")];
                let buttons = centered_modal(ctx, title, |ui| {
                    ui.set_max_width(360.0);
                    ui.add(
                        egui::TextEdit::singleline(pw1)
                            .id(fields[0])
                            .password(true)
                            .hint_text("Password")
                            .desired_width(240.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(pw2)
                            .id(fields[1])
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
                    purge_text_state(ctx, &fields);
                } else if submit {
                    let password = Zeroizing::new(std::mem::take(pw1));
                    pw2.zeroize();
                    let salt = generate_salt();
                    let new_security = SecurityConfig {
                        password_protected: true,
                        salt: Some(salt_to_hex(&salt)),
                        use_keychain: *remember,
                    };
                    purge_text_state(ctx, &fields);
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
                Err(mpsc::TryRecvError::Empty) => deriving_window(ctx, "Deriving key…"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.config_error = Some("key derivation thread died".to_string());
                    self.security_prompt = None;
                }
            },
            SecurityPrompt::ConfirmRebuild {
                new_security,
                new_key,
            } => {
                let (title, residue) =
                    confirm_text(new_key.is_some(), self.cfg.security.password_protected);
                let buttons = centered_modal(ctx, title, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        "Changing index encryption deletes the index and \
                         re-indexes everything. Searches return incomplete \
                         results until the rebuild finishes. Your files are \
                         not touched.",
                    );
                    if let Some(residue) = residue {
                        ui.label(residue);
                    }
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
                    let rx = self.backend.delete_index();
                    self.security_prompt = Some(SecurityPrompt::Deleting {
                        rx,
                        new_security: new_security.clone(),
                        new_key: new_key.clone(),
                    });
                }
            }
            SecurityPrompt::Deleting {
                rx,
                new_security,
                new_key,
            } => match delete_outcome(rx.try_recv()) {
                None => deriving_window(ctx, "Deleting the index…"),
                Some(Ok(())) => {
                    let (new_security, new_key) = (new_security.clone(), new_key.clone());
                    self.security_prompt = None;
                    let was_remembered = self.cfg.security.use_keychain;
                    if let Err(e) =
                        apply_security_change(self, was_remembered, new_security, new_key)
                    {
                        self.config_error = Some(e);
                    }
                }
                // The old index, config and key all stand.
                Some(Err(e)) => {
                    self.security_prompt = None;
                    self.config_error = Some(format!("Not changed: {e}"));
                }
            },
        }
    }

    /// Only ever open while protection is on, so a salt and a process key
    /// both exist.
    pub(super) fn key_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.key_prompt else {
            return;
        };
        match prompt {
            KeyPrompt::Confirm { pw, wrong } => {
                let (submit, cancel) = confirm_key_modal(ctx, pw, *wrong);
                if cancel {
                    self.key_prompt = None; // Drop impl zeroizes
                    purge_text_state(ctx, &[egui::Id::new(SHOW_KEY_FIELD)]);
                } else if submit {
                    let password = Zeroizing::new(std::mem::take(pw));
                    purge_text_state(ctx, &[egui::Id::new(SHOW_KEY_FIELD)]);
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
                Ok(key) => {
                    // Unreachable while protected (see above).
                    let Some(installed) = db::process_key_hex() else {
                        self.config_error =
                            Some("no key installed; restart and unlock first".to_string());
                        self.key_prompt = None;
                        return;
                    };
                    // Shown is the *installed* key, the one that opens the index.
                    self.key_prompt = Some(match reveal_display(&installed, &key.to_hex()) {
                        Some(display) => KeyPrompt::Reveal { display },
                        None => KeyPrompt::Confirm {
                            pw: String::new(),
                            wrong: true,
                        },
                    });
                }
                Err(mpsc::TryRecvError::Empty) => deriving_window(ctx, "Deriving key…"),
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
}

/// What a security change does outside its prompt, as four calls whose order
/// [`apply_security_change`] and [`set_keychain`] decide. The app does them
/// for real; the tests record them.
trait SecurityEffects {
    /// Write the config with `security` in it. On `Err` the settings in force
    /// are still the previous ones, in memory and on disk.
    fn save_config(&mut self, security: SecurityConfig) -> Result<(), String>;
    /// Have the keychain hold `key_hex`, with the salt in force, or nothing.
    /// Failing to forget is an error only when `was_remembered`
    /// ([`keychain::sync`]).
    fn sync_keychain(&mut self, was_remembered: bool, key_hex: Option<&str>) -> Result<(), String>;
    /// The key every later connection opens the index with.
    fn install_key(&mut self, key: Option<IndexKey>);
    /// Delete the index and build it afresh.
    fn rebuild(&mut self);
}

impl SecurityEffects for QuickSearchApp {
    fn save_config(&mut self, security: SecurityConfig) -> Result<(), String> {
        let previous = std::mem::replace(&mut self.cfg.security, security);
        let saved = self.cfg.save();
        if saved.is_err() {
            self.cfg.security = previous;
        }
        saved
    }

    fn sync_keychain(&mut self, was_remembered: bool, key_hex: Option<&str>) -> Result<(), String> {
        let db_path = self.cfg.resolved_database_path();
        // The key's own salt: `apply_security_change` saves the new one
        // before it syncs, and `set_keychain` changes none.
        let salt = self.cfg.security.salt.as_deref().unwrap_or_default();
        let remembered = key_hex.map(|key| keychain::entry_text(salt, key));
        keychain::sync(
            &db_path.to_string_lossy(),
            was_remembered,
            remembered.as_deref().map(String::as_str),
        )
    }

    fn install_key(&mut self, key: Option<IndexKey>) {
        db::set_process_key(key);
    }

    fn rebuild(&mut self) {
        self.backend.rebuild_index();
        // The listing describes the index being deleted.
        self.drop_duplicates();
    }
}

/// Commit a security change. Runs only once the old index is deleted (see
/// [`SecurityPrompt::Deleting`]), then: config, keychain, process key, and
/// the rebuild last, so the fresh index is created under the new key (or
/// none). A failed save rebuilds under the settings still in force and
/// changes nothing else. `was_remembered` is the keychain preference being
/// replaced. `Err` is the message to show.
fn apply_security_change(
    fx: &mut impl SecurityEffects,
    was_remembered: bool,
    new_security: SecurityConfig,
    new_key: Option<IndexKey>,
) -> Result<(), String> {
    let remember = new_security.use_keychain;
    if let Err(e) = fx.save_config(new_security) {
        // Fail closed: the salt lives only in that file, and carrying on
        // would rebuild an index encrypted with a salt that reached no
        // disk, which no password can open afterwards.
        fx.rebuild();
        return Err(e);
    }
    // No stored key may survive pointing at the previous state.
    let remembered = new_key.as_ref().filter(|_| remember).map(IndexKey::to_hex);
    let synced = fx.sync_keychain(was_remembered, remembered.as_deref());
    fx.install_key(new_key);
    fx.rebuild();
    synced
}

/// Remember `installed_hex`, the key in use, in the keychain, or forget it,
/// then save the preference into `security`. A failed sync saves nothing:
/// the preference must never claim what the keychain does not hold.
fn set_keychain(
    fx: &mut impl SecurityEffects,
    security: SecurityConfig,
    remember: bool,
    installed_hex: Option<String>,
) -> Result<(), String> {
    let key_hex = installed_hex.filter(|_| remember);
    // Always there while protected: the gate installs a key before the app
    // starts.
    if remember && key_hex.is_none() {
        return Err("no key to remember; restart and unlock first".to_string());
    }
    fx.sync_keychain(security.use_keychain, key_hex.as_deref())?;
    fx.save_config(SecurityConfig {
        use_keychain: remember,
        ..security
    })
}

const SHOW_KEY_FIELD: &str = "show-key-pw";

/// `None` when the typed password does not derive the installed key. Both
/// hexes are lowercase, so a plain comparison is exact; nothing secret is
/// learned from its timing, since the caller already holds the guess.
fn reveal_display(installed_hex: &str, derived_hex: &str) -> Option<String> {
    (installed_hex == derived_hex).then(|| format!("0x{}", installed_hex))
}

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
        // Never steals focus from something the user moved to themselves.
        if ui.memory(|m| m.focused().is_none()) {
            field.request_focus();
        }
        if wrong {
            ui.colored_label(ui.visuals().error_fg_color, "That password is not correct.");
        }
        ui.horizontal(|ui| {
            let ok = !pw.is_empty();
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let submit =
                ui.add_enabled(ok, egui::Button::new("Show key")).clicked() || (ok && entered);
            (submit, ui.button("Cancel").clicked())
        })
        .inner
    })
    .unwrap_or((false, false))
}

/// The page size other SQLCipher tools assume unless told otherwise, and so
/// the one this screen has to talk them out of.
const SQLCIPHER_DEFAULT_PAGE_SIZE: i64 = 4096;

/// The pragma another tool needs for our page authenticator, spelled out
/// because the setting is not one anybody would guess: the reserve it decides
/// is part of the on-disk layout, so getting it wrong reads as a bad key
/// rather than as a failed integrity check.
fn hmac_pragma_hint() -> &'static str {
    use quicksearch_core::db::schema::{HmacMode, HMAC_MODE};
    match HMAC_MODE {
        HmacMode::Off => "PRAGMA cipher_use_hmac = OFF;",
        HmacMode::Sha256 => "PRAGMA cipher_hmac_algorithm = HMAC_SHA256;",
        HmacMode::Sha512 => "",
    }
}

fn reveal_key_modal(ctx: &egui::Context, display: &str) -> (bool, bool) {
    use quicksearch_core::db::schema::{HMAC_MODE, PAGE_SIZE};

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
        // The key on its own is not enough: under any other page size or page
        // authenticator the file decrypts to noise, which every tool reports
        // as a wrong key. Both have to be entered with the key, so each gets
        // its own line rather than the small print.
        ui.label(egui::RichText::new(format!("Page size: {}", PAGE_SIZE)).monospace());
        ui.label(egui::RichText::new(format!("Page HMAC: {}", HMAC_MODE.label())).monospace());
        ui.add_space(6.0);
        ui.label(hint(format!(
            "Other SQLCipher tools take the key in this form but default to \
             {}-byte pages and HMAC_SHA512, so set both of the above as well \
             or the index will not open ({} {}). Copy puts only the key on the \
             clipboard, where it stays until something replaces it.",
            SQLCIPHER_DEFAULT_PAGE_SIZE,
            format_args!("PRAGMA cipher_page_size = {};", PAGE_SIZE),
            hmac_pragma_hint(),
        )));
        ui.add_space(6.0);
        ui.horizontal(|ui| (ui.button("Copy").clicked(), ui.button("Close").clicked()))
            .inner
    })
    .unwrap_or((false, false))
}

#[cfg(test)]
#[path = "security_tests.rs"]
mod tests;
