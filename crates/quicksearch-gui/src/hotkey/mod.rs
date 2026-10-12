//! The in-application half of the search shortcut: the key QuickSearch
//! claims for itself while it is running, through `global-hotkey`
//! (`RegisterHotKey` on Windows, `XGrabKey` on X11). It needs no setup, so the
//! Settings tab can offer any combination, but it cannot fire while
//! QuickSearch is closed; [`crate::activate`] and `--toggle` cover that, and
//! both paths set the same pending flag.
//!
//! Wayland refuses grabs and gets no in-app half ([`Status::DesktopOnly`]).
//! The XDG GlobalShortcuts portal is not used: it prompts on every launch,
//! binds an action that only works while the app runs, and its persisted
//! claim blocks the desktop's own launch binding. An installed
//! [`crate::shortcut_setup`] binding stands this half down too
//! ([`Status::SystemOwned`]).
//!
//! Held in a thread-local global rather than a field: the registration is
//! process-wide, the event handler is set-once, and on Windows
//! `GlobalHotKeyManager` is not `Send`. Every entry point is inert until
//! [`init`] runs, so headless UI tests never touch an OS registration.

mod binding;

pub use binding::{parse_setting, Binding};

use std::cell::RefCell;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

thread_local! {
    static REGISTRY: RefCell<Option<Registry>> = const { RefCell::new(None) };
}

/// What the Settings tab says about the shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Disabled,
    Active,
    /// Wayland without a system binding: the compositor refuses in-app
    /// grabs, so only the desktop's own binding can deliver the key.
    DesktopOnly,
    /// A `crate::shortcut_setup` binding is installed, so the desktop owns
    /// the key and nothing is registered in-app: every press arrives through
    /// `--toggle`, which also works while QuickSearch is closed. Registering
    /// here as well would fight the desktop over the key.
    SystemOwned,
    Error(String),
}

struct Registry {
    backend: Backend,
    status: Status,
}

enum Backend {
    /// A Wayland session (see the module docs): nothing is ever registered.
    Idle,
    /// Windows and X11, holding no key, and so no manager either: on X11 one
    /// polls the display twenty times a second for as long as it exists,
    /// whether or not it holds anything.
    Ready,
    /// Windows and X11, holding `registered`.
    Grab {
        manager: GlobalHotKeyManager,
        registered: HotKey,
    },
}

/// Starts the shortcut and registers `setting`. Must be called on the
/// event-loop thread: on Windows `GlobalHotKeyManager` creates a hidden
/// window whose messages that loop dispatches.
pub fn init(ctx: &egui::Context, setting: &str) {
    // Press only: the crate reports the release as a second event. The press
    // goes to `activate`, which consumes it however it arrived.
    let repaint = ctx.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state == HotKeyState::Pressed {
            crate::activate::fire(&repaint);
        }
    }));

    // No X11 grab as a fallback under Wayland: it succeeds inside a Wayland
    // session and then only fires while an XWayland window has focus, which
    // looks broken.
    let backend = if crate::activate::raise::is_wayland() {
        Backend::Idle
    } else {
        Backend::Ready
    };
    REGISTRY.with_borrow_mut(|slot| {
        *slot = Some(Registry {
            backend,
            status: Status::Disabled,
        })
    });
    apply(setting);
}

/// Registers `setting`, releasing whatever was registered before. An
/// unparseable or refused shortcut is reported through [`status`], never by
/// failing: not worth blocking a config the user has already applied. On the
/// event-loop thread, like [`init`]: registering creates the manager.
pub fn apply(setting: &str) {
    REGISTRY.with_borrow_mut(|slot| {
        let Some(registry) = slot.as_mut() else {
            return;
        };
        // The desktop's own binding outranks ours (see
        // `Status::SystemOwned`). Checked here so init and every save agree;
        // the probe is a file check on KDE and one subprocess on GNOME, and
        // `apply` only runs on saves.
        if crate::shortcut_setup::installed() {
            let _ = registry.backend.register(None);
            registry.status = Status::SystemOwned;
            return;
        }
        // Wayland with nothing installed: the compositor refuses grabs, so
        // say where the key has to live.
        if crate::activate::raise::is_wayland() {
            registry.status = Status::DesktopOnly;
            return;
        }
        let wanted = match parse_setting(setting) {
            Ok(binding) => binding,
            Err(e) => {
                registry.status = Status::Error(format!("{:?} is not a shortcut: {}", setting, e));
                let _ = registry.backend.register(None);
                return;
            }
        };
        registry.status = match registry.backend.register(wanted) {
            Ok(()) if wanted.is_some() => Status::Active,
            Ok(()) => Status::Disabled,
            Err(e) => Status::Error(e),
        };
        // A shortcut that quietly does nothing is indistinguishable from
        // one that was never asked for; log it.
        match (&registry.status, wanted) {
            (Status::Active, Some(binding)) => {
                quicksearch_core::log_info!("global shortcut: {} registered", binding)
            }
            (Status::Error(why), _) => quicksearch_core::log_warn!("global shortcut: {}", why),
            _ => {}
        }
    });
}

pub fn status() -> Status {
    REGISTRY.with_borrow(|slot| match slot.as_ref() {
        None => Status::Disabled,
        Some(registry) => registry.status.clone(),
    })
}

impl Backend {
    /// Hold `wanted` and nothing else. `None` releases without registering,
    /// and drops the manager with it.
    fn register(&mut self, wanted: Option<Binding>) -> Result<(), String> {
        if matches!(self, Backend::Idle) {
            return Ok(());
        }
        if let Backend::Grab {
            manager,
            registered,
        } = std::mem::replace(self, Backend::Ready)
        {
            // Worth reporting, not worth refusing the new binding over.
            if let Err(e) = manager.unregister(registered) {
                quicksearch_core::log_warn!("releasing the old global shortcut: {}", e);
            }
        }
        let Some(binding) = wanted else {
            return Ok(());
        };
        let hotkey: HotKey = binding
            .to_string()
            .parse()
            .map_err(|e| format!("{} is not a usable shortcut: {}", binding, e))?;
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| format!("global shortcuts are unavailable: {}", e))?;
        manager.register(hotkey).map_err(|e| match e {
            global_hotkey::Error::AlreadyRegistered(_) => {
                format!("another application is already using {}", binding)
            }
            other => format!("{} could not be registered: {}", binding, other),
        })?;
        *self = Backend::Grab {
            manager,
            registered: hotkey,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing may touch an OS registration before `init`.
    #[test]
    fn an_uninitialised_registry_is_inert() {
        apply("Ctrl+Shift+F");
        assert_eq!(status(), Status::Disabled);
        assert!(!crate::activate::take_pending());
    }

    /// `Idle` must accept every call: `apply` runs on every config save.
    #[test]
    fn an_idle_backend_accepts_everything() {
        let mut backend = Backend::Idle;
        assert_eq!(backend.register(None), Ok(()));
        assert_eq!(
            backend.register(Some("Ctrl+Shift+F".parse().unwrap())),
            Ok(())
        );
        assert!(matches!(backend, Backend::Idle));
    }

    /// Holding no key must not start a manager: on X11 its thread polls the
    /// display until it is dropped.
    #[test]
    fn releasing_holds_no_manager() {
        let mut backend = Backend::Ready;
        assert_eq!(backend.register(None), Ok(()));
        assert!(matches!(backend, Backend::Ready));
    }
}
