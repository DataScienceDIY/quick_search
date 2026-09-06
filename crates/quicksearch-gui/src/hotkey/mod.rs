//! The in-application half of the search shortcut: the key QuickSearch
//! claims for itself while it is running. Windows and X11 grant that via
//! `global-hotkey` (`RegisterHotKey`/`XGrabKey`). Wayland refuses grabs by
//! design and gets no in-app half at all ([`Status::DesktopOnly`]): the XDG
//! GlobalShortcuts portal was tried and removed — it prompted a dialog on
//! every launch, bound an action that only worked while the app ran, and
//! its persisted claim on the key blocked the desktop's own launch binding.
//! On Wayland the desktop's binding (`crate::shortcut_setup`, `--toggle`)
//! is the whole story.
//!
//! This is the path that needs no setup at all, and it is why the Settings
//! tab can offer an arbitrary combination on every platform. It cannot fire
//! while QuickSearch is not running — nothing an application registers for
//! itself can — which is what [`crate::activate`] and `--toggle` are for.
//! Both funnel into the same pending flag, so the window comes forward the
//! same way whichever one fired.
//!
//! When a [`crate::shortcut_setup`] system binding is installed, this half
//! stands down entirely ([`Status::SystemOwned`]): the desktop delivers the
//! key whether QuickSearch is running or not, and holding our own claim as
//! well would fight it for the key.
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
    /// Nothing registered: no shortcut set, a Wayland session (see the
    /// module docs), or the backend never started.
    Idle,
    /// Windows and X11.
    Grab {
        manager: GlobalHotKeyManager,
        registered: Option<HotKey>,
    },
}

/// Start the shortcut and register `setting`. Must be called on the
/// event-loop thread: on Windows `GlobalHotKeyManager` creates a hidden
/// window whose messages that loop dispatches.
pub fn init(ctx: &egui::Context, setting: &str) {
    // Press only: the crate reports the release as a second event.
    let repaint = ctx.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state == HotKeyState::Pressed {
            fire(&repaint);
        }
    }));

    let backend = match choose_backend() {
        Ok(backend) => backend,
        Err(message) => {
            quicksearch_core::log_warn!("global shortcut: {}", message);
            REGISTRY.with_borrow_mut(|slot| {
                *slot = Some(Registry {
                    backend: Backend::Idle,
                    status: Status::Error(message),
                });
            });
            return;
        }
    };
    REGISTRY.with_borrow_mut(|slot| {
        *slot = Some(Registry {
            backend,
            status: Status::Disabled,
        })
    });
    apply(setting);
}

/// Register `setting`, releasing whatever was registered before. An
/// unparseable or refused shortcut is reported through [`status`], never by
/// failing: not worth blocking a config the user has already applied.
pub fn apply(setting: &str) {
    REGISTRY.with_borrow_mut(|slot| {
        let Some(registry) = slot.as_mut() else {
            return;
        };
        // The desktop's own binding outranks ours: release whatever is held
        // and stand down (see `Status::SystemOwned`). Checked here rather
        // than by the callers so init and every save agree; the probe is a
        // file check on KDE and a one-off subprocess on GNOME, and `apply`
        // only runs on saves.
        if crate::shortcut_setup::installed() {
            let _ = registry.backend.register(None);
            registry.status = Status::SystemOwned;
            return;
        }
        // Wayland with nothing installed: there is nothing to register —
        // the compositor refuses grabs — so say where the key has to live.
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

/// Hand the press to [`crate::activate`], which owns the pending flag and
/// the repaint. One place to consume, whether the press came from our own
/// registration or from a `--toggle` the desktop launched.
fn fire(ctx: &egui::Context) {
    crate::activate::fire(ctx);
}

impl Backend {
    /// Hold `wanted` and nothing else. `None` releases without registering.
    fn register(&mut self, wanted: Option<Binding>) -> Result<(), String> {
        match self {
            Backend::Idle => Ok(()),
            Backend::Grab {
                manager,
                registered,
            } => {
                if let Some(old) = registered.take() {
                    // Worth reporting, not worth refusing the new binding over.
                    if let Err(e) = manager.unregister(old) {
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
                manager.register(hotkey).map_err(|e| match e {
                    global_hotkey::Error::AlreadyRegistered(_) => {
                        format!("another application is already using {}", binding)
                    }
                    other => format!("{} could not be registered: {}", binding, other),
                })?;
                *registered = Some(hotkey);
                Ok(())
            }
        }
    }
}

/// A Wayland session gets no backend at all — the desktop's binding is the
/// Wayland path; see the module docs. No X11 grab as a fallback either: it
/// succeeds inside a Wayland session and then only fires while an XWayland
/// window has focus — a broken-looking shortcut.
fn choose_backend() -> Result<Backend, String> {
    if crate::activate::raise::is_wayland() {
        return Ok(Backend::Idle);
    }
    grab_backend()
}

fn grab_backend() -> Result<Backend, String> {
    GlobalHotKeyManager::new()
        .map(|manager| Backend::Grab {
            manager,
            registered: None,
        })
        .map_err(|e| format!("global shortcuts are unavailable: {}", e))
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
    }
}
