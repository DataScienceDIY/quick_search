//! The Wayland half of the shortcut: `org.freedesktop.portal.GlobalShortcuts`.
//!
//! **The desktop owns the binding**: what we send is a `preferred_trigger`,
//! and the compositor may bind something else or ask the user; what it bound
//! comes back as a `trigger_description`, which the Settings tab shows.
//!
//! All of this lives on its own thread — a portal call is a D-Bus round trip
//! that can block as long as a dialog stays up. The session must stay open
//! for activations to keep arriving; dropping it is how a rebind starts over.

use std::sync::{Arc, Mutex};

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::Session;
use futures_channel::mpsc;
use futures_util::future::{select, Either};
use futures_util::StreamExt;

use super::Status;

/// The portal keys activations by this id; the desktop lists the entry by it.
const SHORTCUT_ID: &str = "search";

/// Shown next to the key in the desktop's shortcut settings.
const SHORTCUT_DESCRIPTION: &str = "Focus the QuickSearch search box";

pub(super) struct Portal {
    /// `Some(trigger)` binds, `None` unbinds. Unbounded: sends happen on
    /// the UI thread and must never block it.
    tx: mpsc::UnboundedSender<Option<String>>,
    status: Arc<Mutex<Status>>,
}

impl Portal {
    /// The thread runs until the process exits; nothing to shut down.
    pub(super) fn new(ctx: &egui::Context) -> Portal {
        let (tx, rx) = mpsc::unbounded();
        let status = Arc::new(Mutex::new(Status::Pending));
        let portal = Portal {
            tx,
            status: Arc::clone(&status),
        };
        let ctx = ctx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("quicksearch-hotkey-portal".to_string())
            .spawn(move || pollster::block_on(run(ctx, status, rx)))
        {
            // The status must say so, or the Settings tab shows
            // "Waiting for your desktop…" forever.
            quicksearch_core::log_warn!("global shortcut portal thread: {}", e);
            *lock_ok(&portal.status) =
                Status::Error(format!("the shortcut thread could not be started: {}", e));
        }
        portal
    }

    /// Returns immediately; the answer lands in [`Portal::status`]
    /// whenever the desktop gets to it.
    pub(super) fn bind(&self, trigger: Option<String>) {
        *lock_ok(&self.status) = match trigger {
            Some(_) => Status::Pending,
            None => Status::Disabled,
        };
        let _ = self.tx.unbounded_send(trigger);
    }

    pub(super) fn status(&self) -> Status {
        lock_ok(&self.status).clone()
    }
}

/// Ignore poisoning: a portal-thread panic must not take the UI thread too.
fn lock_ok<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn run(
    ctx: egui::Context,
    status: Arc<Mutex<Status>>,
    mut commands: mpsc::UnboundedReceiver<Option<String>>,
) {
    let shortcuts: GlobalShortcuts<'static> = match GlobalShortcuts::new().await {
        Ok(s) => s,
        Err(e) => return fail(&ctx, &status, unavailable(&e)),
    };
    // A signal match on the interface, not a session: survives rebinds.
    let activated = match shortcuts.receive_activated().await {
        Ok(s) => s,
        Err(e) => return fail(&ctx, &status, unavailable(&e)),
    };
    futures_util::pin_mut!(activated);

    let mut session: Option<Session<'static, GlobalShortcuts<'static>>> = None;
    loop {
        match select(activated.next(), commands.next()).await {
            Either::Left((Some(_), _)) => {
                // No id check needed: this session has exactly one shortcut.
                super::fire(&ctx);
            }
            // The portal went away; the session is already dead.
            Either::Left((None, _)) => {
                return fail(
                    &ctx,
                    &status,
                    "the desktop's global shortcuts service stopped".to_string(),
                )
            }
            Either::Right((Some(trigger), _)) => {
                // A rebind is a new session: the portal treats a session's
                // shortcuts as fixed once bound.
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
            // The registry dropped the sender: we are on the way out.
            Either::Right((None, _)) => return,
        }
    }
}

/// Bind the trigger; returns the desktop's own wording for what it settled on.
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
    // A blank description falls back to the preferred trigger.
    let description = bound
        .shortcuts()
        .iter()
        .find(|s| s.id() == SHORTCUT_ID)
        .map(|s| s.trigger_description().to_string())
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| trigger.to_string());
    Ok((session, description))
}

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
    *lock_ok(status) = next;
    ctx.request_repaint();
}

fn fail(ctx: &egui::Context, status: &Mutex<Status>, message: String) {
    quicksearch_core::log_warn!("global shortcut: {}", message);
    set(ctx, status, Status::Error(message));
}
