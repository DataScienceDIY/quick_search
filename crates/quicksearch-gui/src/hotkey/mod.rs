//! The system-wide shortcut that raises QuickSearch and focuses the search
//! box.
//!
//! It has to be registered with the operating system rather than handled as
//! an egui shortcut, because the whole point is that it works when the window
//! is minimised, behind something else, or not focused — none of which
//! deliver key events to the app. There are two ways to get one, chosen by
//! what the session is:
//!
//! * **Windows and X11** let an application claim a key for itself
//!   (`RegisterHotKey`, `XGrabKey`), which `global-hotkey` wraps. The key is
//!   exactly the one that was asked for, or the registration fails.
//! * **Wayland** does not, on purpose, so the shortcut goes through the XDG
//!   desktop portal instead and the *desktop* owns the binding. See
//!   [`portal`].
//!
//! Both are driven from here, through one interface, so the rest of the app
//! only ever deals with "did the shortcut fire" and "what should the Options
//! window say about it".
//!
//! # Why this is a global rather than a field
//!
//! The registration is process-wide however it is made, and
//! `GlobalHotKeyEvent::set_event_handler` is itself a set-once global. On
//! Windows the manager owns a hidden message window, so it is not `Send` and
//! has to stay on the thread that runs the winit event loop — the same thread
//! every caller below is already on. The alternative, threading a handle from
//! [`crate::main`] through [`crate::unlock::Gate`] into
//! [`crate::app::QuickSearchApp`], has to survive the app being *built
//! mid-session* when a password unlocks the index, and buys nothing for it.
//!
//! Every entry point is inert until [`init`] runs, so the headless UI tests
//! never touch an OS registration.

mod binding;
#[cfg(all(unix, not(target_os = "macos")))]
mod portal;
mod raise;

pub use binding::{parse_setting, Binding};
pub use raise::raise;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// Set from whichever thread the shortcut arrives on, consumed by the UI
/// thread in [`take_fired`]. A flag rather than a queue: two presses before
/// the app can redraw mean the same thing as one.
static FIRED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// UI-thread only. See the module docs for why it is not a field.
    static REGISTRY: RefCell<Option<Registry>> = const { RefCell::new(None) };
}

/// What the Options window says about the shortcut. Every variant is
/// something the user can act on, which is why "registered but the desktop
/// picked the key" is not folded into [`Status::Active`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The setting is empty: no shortcut, by choice.
    Disabled,
    /// Registered with the display server, exactly as asked.
    Active,
    /// Asked for, and the desktop has not answered yet.
    Pending,
    /// Wayland: registered, described in the desktop's own words because the
    /// desktop, not the setting, decides the key.
    PortalBound(String),
    /// It is not going to work, and this says why.
    Error(String),
}

struct Registry {
    backend: Backend,
    /// The status of everything except the portal, which reports its own
    /// asynchronously; see [`status`].
    status: Status,
}

enum Backend {
    /// Nothing registered: no shortcut set, or the backend never started.
    Idle,
    /// Windows and X11.
    Grab {
        manager: GlobalHotKeyManager,
        /// The registration currently held, to be released before the next.
        registered: Option<HotKey>,
    },
    #[cfg(all(unix, not(target_os = "macos")))]
    Portal(portal::Portal),
}

/// Start the shortcut and register `setting`.
///
/// Must be called on the thread running the event loop, and only from there:
/// on Windows `GlobalHotKeyManager` creates a hidden window whose messages
/// that loop is what dispatches. In practice that means eframe's app-creation
/// closure, which runs on the main thread with the loop already going.
pub fn init(ctx: &egui::Context, setting: &str) {
    // Set once for the process, so it goes here rather than next to the
    // manager, which comes and goes with the backend.
    // Press only: the crate reports the release as a second event, and
    // acting on both means every press of the shortcut does its work twice.
    let repaint = ctx.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state == HotKeyState::Pressed {
            fire(&repaint);
        }
    }));

    let backend = match choose_backend(ctx) {
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

/// Register `setting`, releasing whatever was registered before. Empty means
/// no shortcut. An unparseable or refused shortcut is reported through
/// [`status`], never by failing: a shortcut is not worth blocking a config
/// the user has already applied.
pub fn apply(setting: &str) {
    REGISTRY.with_borrow_mut(|slot| {
        let Some(registry) = slot.as_mut() else {
            return;
        };
        let wanted = match parse_setting(setting) {
            Ok(binding) => binding,
            Err(e) => {
                registry.status = Status::Error(format!("{:?} is not a shortcut: {}", setting, e));
                // Releasing cannot fail in a way worth a second message.
                let _ = registry.backend.register(None);
                return;
            }
        };
        registry.status = match registry.backend.register(wanted) {
            Ok(()) if wanted.is_some() => Status::Active,
            Ok(()) => Status::Disabled,
            Err(e) => Status::Error(e),
        };
        // In the Logs tab, because a shortcut that quietly does nothing is
        // otherwise impossible to tell apart from one that was never asked
        // for. The Options window says the same thing, but only while it is
        // open, and only about the state it left behind.
        match (&registry.status, wanted) {
            (Status::Active, Some(binding)) => {
                quicksearch_core::log_info!("global shortcut: {} registered", binding)
            }
            (Status::Error(why), _) => quicksearch_core::log_warn!("global shortcut: {}", why),
            _ => {}
        }
    });
}

/// Whether the shortcut was pressed since this was last asked, clearing it.
pub fn take_fired() -> bool {
    FIRED.swap(false, Ordering::SeqCst)
}

/// What to tell the user about the shortcut right now.
pub fn status() -> Status {
    REGISTRY.with_borrow(|slot| match slot.as_ref() {
        None => Status::Disabled,
        // The portal answers on its own schedule, so it keeps its own status
        // and this one is stale the moment a bind is sent.
        #[cfg(all(unix, not(target_os = "macos")))]
        Some(Registry {
            backend: Backend::Portal(portal),
            ..
        }) => portal.status(),
        Some(registry) => registry.status.clone(),
    })
}

/// Record a press and wake the UI. The repaint is the load-bearing half:
/// with nothing happening on screen the app is idle, and a minimised window
/// is not drawing at all, so without it the flag would sit unread until
/// something else asked for a frame.
fn fire(ctx: &egui::Context) {
    FIRED.store(true, Ordering::SeqCst);
    ctx.request_repaint();
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
                    // A failed unregister leaves a key claimed that nothing
                    // listens for any more. Worth reporting, but not worth
                    // refusing the new binding over.
                    if let Err(e) = manager.unregister(old) {
                        quicksearch_core::log_warn!("releasing the old global shortcut: {}", e);
                    }
                }
                let Some(binding) = wanted else {
                    return Ok(());
                };
                // Infallible in practice: `Binding`'s tokens are held to
                // being parseable by a test, precisely so this cannot be a
                // silent runtime failure.
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
            #[cfg(all(unix, not(target_os = "macos")))]
            Backend::Portal(portal) => {
                portal.bind(wanted.map(|b| b.portal_trigger()));
                Ok(())
            }
        }
    }
}

/// Wayland refuses key grabs by design, so a session with a Wayland display
/// gets the portal and everything else gets a grab. There is deliberately no
/// falling back from one to the other: an X11 grab made from inside a Wayland
/// session succeeds and then only ever fires while an XWayland window has
/// focus, which looks like a broken shortcut rather than an unavailable one.
#[cfg(all(unix, not(target_os = "macos")))]
fn choose_backend(ctx: &egui::Context) -> Result<Backend, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Ok(Backend::Portal(portal::Portal::new(ctx)));
    }
    grab_backend()
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn choose_backend(_ctx: &egui::Context) -> Result<Backend, String> {
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

    /// Nothing may touch an OS registration before `init`, so that the
    /// headless UI tests can render the Options row.
    #[test]
    fn an_uninitialised_registry_is_inert() {
        apply("Ctrl+Shift+F");
        assert_eq!(status(), Status::Disabled);
        assert!(!take_fired());
    }

    #[test]
    fn a_press_is_reported_once() {
        FIRED.store(true, Ordering::SeqCst);
        assert!(take_fired());
        assert!(!take_fired(), "the flag is consumed");
    }

    /// `Idle` stands in for a backend that never started; it must accept
    /// every call rather than panic, since `apply` runs on every config save.
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
