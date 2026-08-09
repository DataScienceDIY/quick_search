//! The Wayland half of the search shortcut: `org.freedesktop.portal.GlobalShortcuts`.
//!
//! Wayland deliberately gives an application no way to grab a key it does not
//! already have focus for, so the shortcut is registered with the desktop
//! instead and the desktop tells us when it fires. The consequence worth
//! knowing is that **the desktop owns the binding**: what we send is a
//! `preferred_trigger`, and the compositor is free to bind something else, to
//! ask the user first, or to let them change it later in its own settings.
//! What it actually bound comes back as a human-readable
//! `trigger_description`, which is what the Options window shows.
//!
//! All of this lives on its own thread. The portal is D-Bus, so every call
//! is a round trip that could block for as long as a dialog stays on screen,
//! and none of that may happen on the UI thread. The thread outlives the
//! binding: the session has to stay open for activations to keep arriving,
//! and dropping it is how a rebind starts over.

use std::sync::{Arc, Mutex};

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::Session;
use futures_channel::mpsc;
use futures_util::future::{select, Either};
use futures_util::StreamExt;

use super::Status;

/// Our only shortcut. The portal keys activations by this id, and it is what
/// a desktop's shortcut settings lists the entry under.
const SHORTCUT_ID: &str = "search";

/// Shown next to the key in the desktop's shortcut settings, so it is written
/// for someone reading a list of every app's shortcuts at once.
const SHORTCUT_DESCRIPTION: &str = "Focus the QuickSearch search box";

pub(super) struct Portal {
    /// `Some(trigger)` binds, `None` unbinds. Unbounded because a send
    /// happens on the UI thread and must never block it.
    tx: mpsc::UnboundedSender<Option<String>>,
    status: Arc<Mutex<Status>>,
}

impl Portal {
    /// Start the portal thread. It runs until the process exits; there is
    /// nothing to shut down, since the session's only resource is a D-Bus
    /// connection the OS reclaims.
    pub(super) fn new(ctx: &egui::Context) -> Portal {
        let (tx, rx) = mpsc::unbounded();
        let status = Arc::new(Mutex::new(Status::Pending));
        let portal = Portal {
            tx,
            status: Arc::clone(&status),
        };
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("quicksearch-hotkey-portal".to_string())
            .spawn(move || pollster::block_on(run(ctx, status, rx)))
            // A thread that will not start is a shortcut that will not work,
            // which is not worth taking the app down for.
            .map_err(|e| quicksearch_core::log_warn!("global shortcut portal thread: {}", e))
            .ok();
        portal
    }

    /// Ask for a new binding, or for none at all. Returns immediately; the
    /// answer lands in [`Portal::status`] whenever the desktop gets to it.
    pub(super) fn bind(&self, trigger: Option<String>) {
        *self.status.lock().unwrap() = match trigger {
            Some(_) => Status::Pending,
            None => Status::Disabled,
        };
        let _ = self.tx.unbounded_send(trigger);
    }

    pub(super) fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

async fn run(
    ctx: egui::Context,
    status: Arc<Mutex<Status>>,
    mut commands: mpsc::UnboundedReceiver<Option<String>>,
) {
    // `'static` throughout: the proxy owns its D-Bus connection, so nothing
    // here borrows from a local, and pinning the lifetime keeps the session
    // below from being tied to a borrow of `shortcuts` that a rebind would
    // then have to end.
    let shortcuts: GlobalShortcuts<'static> = match GlobalShortcuts::new().await {
        Ok(s) => s,
        Err(e) => return fail(&ctx, &status, unavailable(&e)),
    };
    // Created once and kept for the life of the thread: it is a D-Bus signal
    // match on the interface, not on a session, so it survives the rebinds
    // below and is one less thing to get wrong when a session is replaced.
    let activated = match shortcuts.receive_activated().await {
        Ok(s) => s,
        Err(e) => return fail(&ctx, &status, unavailable(&e)),
    };
    futures_util::pin_mut!(activated);

    let mut session: Option<Session<'static, GlobalShortcuts<'static>>> = None;
    loop {
        match select(activated.next(), commands.next()).await {
            Either::Left((Some(_), _)) => {
                // Which shortcut it was does not need checking: this session
                // has exactly one.
                super::fire(&ctx);
            }
            // The portal went away (it was restarted, or the bus dropped).
            // Nothing left to listen to, and the session is already dead.
            Either::Left((None, _)) => {
                return fail(
                    &ctx,
                    &status,
                    "the desktop's global shortcuts service stopped".to_string(),
                )
            }
            Either::Right((Some(trigger), _)) => {
                // A rebind is a new session, not a second `BindShortcuts`:
                // the portal treats a session's shortcuts as fixed once bound.
                if let Some(old) = session.take() {
                    let _ = old.close().await;
                }
                let next = match &trigger {
                    None => {
                        set(&ctx, &status, Status::Disabled);
                        None
                    }
                    Some(trigger) => match bind(&shortcuts, trigger).await {
                        Ok((session, description)) => {
                            set(&ctx, &status, Status::PortalBound(description));
                            Some(session)
                        }
                        Err(e) => {
                            fail(&ctx, &status, unavailable(&e));
                            None
                        }
                    },
                };
                session = next;
            }
            // The registry dropped the sender, which only happens on the way
            // out.
            Either::Right((None, _)) => return,
        }
    }
}

/// Open a session and bind the trigger, returning the desktop's own wording
/// for the key it settled on.
async fn bind(
    shortcuts: &GlobalShortcuts<'static>,
    trigger: &str,
) -> Result<(Session<'static, GlobalShortcuts<'static>>, String), ashpd::Error> {
    let session = shortcuts.create_session().await?;
    let shortcut =
        NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION).preferred_trigger(Some(trigger));
    let request = shortcuts
        .bind_shortcuts(&session, &[shortcut], None)
        .await?;
    let bound = request.response()?;
    // A desktop that binds the shortcut but describes it as nothing is not
    // worth a special case: the preferred trigger is then the honest answer.
    let description = bound
        .shortcuts()
        .iter()
        .find(|s| s.id() == SHORTCUT_ID)
        .map(|s| s.trigger_description().to_string())
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| trigger.to_string());
    Ok((session, description))
}

/// Turn a portal failure into something worth putting in front of a user.
/// The distinction that matters is "this desktop cannot do it at all" versus
/// "it went wrong this time"; the rest is passed through.
fn unavailable(e: &ashpd::Error) -> String {
    match e {
        ashpd::Error::PortalNotFound(_) => {
            "this desktop does not offer the global shortcuts portal".to_string()
        }
        ashpd::Error::RequiresVersion(required, found) => format!(
            "this desktop's global shortcuts portal is version {}, and {} is needed",
            found, required
        ),
        ashpd::Error::Response(_) => "the desktop declined the shortcut".to_string(),
        other => other.to_string(),
    }
}

fn set(ctx: &egui::Context, status: &Mutex<Status>, next: Status) {
    *status.lock().unwrap() = next;
    // The Options window may be open and waiting for this.
    ctx.request_repaint();
}

fn fail(ctx: &egui::Context, status: &Mutex<Status>, message: String) {
    quicksearch_core::log_warn!("global shortcut: {}", message);
    set(ctx, status, Status::Error(message));
}
