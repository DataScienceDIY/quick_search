//! Bringing the window to the front when a search shortcut fires — either the
//! one QuickSearch registered for itself (`crate::hotkey`) or one a desktop
//! binding relayed through `--toggle`. Both land here. Every display server
//! stops applications raising themselves, so the request has to carry a
//! reason a window manager will accept.
//!
//! * **X11**: winit asks with `_NET_ACTIVE_WINDOW` source indication 1
//!   ("application"), which KWin, Mutter and Xfwm all refuse from an
//!   unfocused window, and its `focus_window` does nothing while minimised.
//!   Hence [`x11_activate`], which sends source indication 2 (EWMH's "direct
//!   user action"). Do not replace it with winit's version.
//! * **Wayland**: a client cannot raise itself; the compositor only honours
//!   an xdg-activation token. winit 0.30 applies one in exactly one place —
//!   window creation — and its `focus_window` is an empty body, so for a
//!   *live* window we speak `xdg_activation_v1` ourselves ([`wayland`]): the
//!   `--toggle` sender forwards the `XDG_ACTIVATION_TOKEN` its launcher gave
//!   it over the socket, and this process hands it to the compositor for its
//!   own surface. Without a token (a manual binding whose launcher minted
//!   none) the fallback is a request for attention — a highlighted task
//!   entry.
//! * **Windows**: `SetForegroundWindow` is refused to background processes —
//!   but the `--toggle` sender was launched by the user's keypress and
//!   donates its right over the pipe with `AllowSetForegroundWindow` (see
//!   `crate::activate`'s Windows module), after which [`win32_activate`]'s
//!   `SetForegroundWindow` is honoured. The in-app hotkey path needs no
//!   grant: the press was delivered to this process. Only a request with no
//!   grant — e.g. some other process poking the pipe — degrades to a
//!   taskbar flash.

/// Whether this is a Wayland session, where raising needs an activation
/// token; the Settings tab words its note accordingly. See the module docs.
pub fn is_wayland() -> bool {
    cfg!(all(unix, not(target_os = "macos"))) && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Bring the window to the front, restoring it if it was minimised.
/// `token` is an xdg-activation token relayed by a `--toggle` sender, the
/// compositor's permission to take focus; only Wayland consumes it.
pub fn raise(ctx: &egui::Context, frame: &eframe::Frame, token: Option<&str>) {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if x11_activate(frame) {
            // Belt and braces: EWMH says activating an iconified window
            // de-iconifies it, but not every window manager does, and a
            // window left minimised cannot take the caret.
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            return;
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            // With a token the compositor will move focus itself; the
            // de-iconify still has to be asked for separately.
            if let Some(token) = token.filter(|t| !t.is_empty()) {
                if wayland::activate(frame, token) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    return;
                }
            }
            // No token, or the compositor refused: the one request honoured
            // from a background client is a bid for attention.
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
            return;
        }
    }
    #[cfg(windows)]
    {
        let _ = token;
        if win32_activate(frame) {
            return;
        }
    }
    #[cfg(not(any(all(unix, not(target_os = "macos")), windows)))]
    {
        let _ = (frame, token);
    }

    // A window still minimised cannot take focus.
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// Restore and foreground our window with the Win32 calls themselves.
/// `false` falls back to winit's viewport commands.
///
/// Not `ViewportCommand::Focus`: winit's `focus_window` routes through the
/// same `SetForegroundWindow`, but only after the event loop wakes and with
/// its own preconditions, and it neither restores a minimised window nor
/// reports failure. Calling the API here keeps restore-then-foreground in
/// one place, immediately, while the `AllowSetForegroundWindow` grant from
/// the `--toggle` sender is fresh.
#[cfg(windows)]
fn win32_activate(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let handle = match frame.window_handle() {
        Ok(handle) => handle,
        Err(e) => {
            quicksearch_core::log_warn!("raising the window: no window handle: {}", e);
            return false;
        }
    };
    let hwnd = match handle.as_raw() {
        RawWindowHandle::Win32(win32) => win32.hwnd.get() as _,
        other => {
            quicksearch_core::log_warn!("raising the window: not a Win32 window: {:?}", other);
            return false;
        }
    };
    // SAFETY: `hwnd` is this process's live window for the whole call; these
    // APIs accept any window handle and merely fail on a bad one.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) == 0 {
            // No grant in force (see the module docs): the most Windows
            // allows from here is a taskbar flash, which the winit fallback
            // produces. Logged so a shortcut that only flashes is traceable.
            quicksearch_core::log_warn!(
                "raising the window: SetForegroundWindow was refused; \
                 flashing the taskbar instead"
            );
            return false;
        }
    }
    true
}

// The X connection used for activation, kept open across presses.
// Thread-local because `raise` only ever runs on the UI thread, and held
// rather than reconnected because a connect per keypress is both wasteful
// and, with the timestamp round trip below, an extra round trip.
#[cfg(all(unix, not(target_os = "macos")))]
thread_local! {
    static X11: std::cell::RefCell<Option<X11State>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(unix, not(target_os = "macos")))]
struct X11State {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
    /// A 1x1 unmapped window of our own, existing only so there is a
    /// property we may change to ask the server what time it is.
    clock: u32,
    net_active: u32,
    user_time: u32,
    marker: u32,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl X11State {
    fn connect() -> Result<X11State, Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, EventMask, WindowClass};

        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let clock = conn.generate_id()?;
        // InputOnly and never mapped: invisible, and the window manager
        // ignores it entirely.
        conn.create_window(
            0,
            clock,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let intern = |name: &[u8]| -> Result<u32, Box<dyn std::error::Error>> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let state = X11State {
            root,
            clock,
            net_active: intern(b"_NET_ACTIVE_WINDOW")?,
            user_time: intern(b"_NET_WM_USER_TIME")?,
            marker: intern(b"_QUICKSEARCH_CLOCK")?,
            conn,
        };
        Ok(state)
    }

    /// The server's current time.
    ///
    /// **Not `CURRENT_TIME`.** KWin reads a zero timestamp as the window
    /// saying it does not want focus and refuses the activation, which is
    /// what made the shortcut raise the window only sometimes. The standard
    /// way to get a real one is a zero-length property append, which comes
    /// back as a `PropertyNotify` stamped by the server.
    fn now(&self) -> Result<u32, Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{AtomEnum, PropMode};
        use x11rb::protocol::Event;
        use x11rb::wrapper::ConnectionExt as _;

        self.conn.change_property8(
            PropMode::APPEND,
            self.clock,
            self.marker,
            AtomEnum::STRING,
            &[],
        )?;
        self.conn.flush()?;
        // Bounded: this runs on the UI thread, which may never block on the
        // X server for longer than a frame or two.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match self.conn.poll_for_event()? {
                Some(Event::PropertyNotify(e)) if e.window == self.clock => return Ok(e.time),
                Some(_) => continue,
                None => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        Err("the X server did not answer with a timestamp".into())
    }

    fn activate(&self, window: u32) -> Result<(), Box<dyn std::error::Error>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, PropMode,
        };
        use x11rb::wrapper::ConnectionExt as _;

        let time = self.now()?;
        // Says the activation traces to real user input, and at this instant.
        // Without it the window manager weighs our request against whatever
        // the user touched most recently and can decide we are stale.
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.user_time,
            AtomEnum::CARDINAL,
            &[time],
        )?;
        // data: source indication, timestamp, the window losing focus.
        // Source 2 is EWMH's "direct user action".
        let event = ClientMessageEvent::new(32, window, self.net_active, [2, time, 0, 0, 0]);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }
}

/// Activate our window, EWMH style. `false` when this is not X11 or the
/// server would not take it, so the caller can fall back to winit.
#[cfg(all(unix, not(target_os = "macos")))]
fn x11_activate(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Both of these used to fail silently, which is how a shortcut that never
    // raised the window looked like a shortcut that never fired.
    let handle = match frame.window_handle() {
        Ok(handle) => handle,
        Err(e) => {
            quicksearch_core::log_warn!("raising the window: no window handle: {}", e);
            return false;
        }
    };
    let window = match handle.as_raw() {
        RawWindowHandle::Xlib(xlib) => xlib.window as u32,
        other => {
            quicksearch_core::log_warn!("raising the window: not an X11 window: {:?}", other);
            return false;
        }
    };

    let sent = X11.with(|slot| -> Result<(), Box<dyn std::error::Error>> {
        let mut slot = slot.borrow_mut();
        let state = match slot.as_mut() {
            Some(state) => state,
            None => slot.insert(X11State::connect()?),
        };
        state.activate(window)
    });
    match sent {
        Ok(()) => true,
        Err(e) => {
            quicksearch_core::log_warn!("raising the window: {}", e);
            // A connection that failed mid-way is not reused: the next press
            // reconnects rather than inheriting a broken one.
            X11.with(|slot| slot.borrow_mut().take());
            false
        }
    }
}

/// The Wayland activation path. winit applies an activation token in exactly
/// one place — window creation — so for a live window we speak the protocol
/// ourselves, over winit's own connection: `from_foreign_display` wraps its
/// `wl_display` as a "guest" backend with a private libwayland event queue.
/// Roundtrips here dispatch only that queue; events for winit's objects stay
/// queued for winit, and dropping the guest never disconnects the display
/// (`owns_display: false`).
#[cfg(all(unix, not(target_os = "macos")))]
mod wayland {
    use std::cell::RefCell;
    use std::error::Error;
    use std::ffi::c_void;

    use raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

    // Kept open across presses like the X11 state above, and for one more
    // reason: wl_registry has no destructor request, so a fresh connection
    // per press would leak a server-side registry every time.
    thread_local! {
        static WAYLAND: RefCell<Option<WaylandState>> = const { RefCell::new(None) };
    }

    struct WaylandState {
        /// The wl_display this guest connection wraps — winit's. Compared on
        /// every press so a different display gets a fresh connection.
        display_ptr: *mut c_void,
        conn: Connection,
        queue: EventQueue<ActivationGlobals>,
        globals: ActivationGlobals,
    }

    struct ActivationGlobals {
        activation: Option<XdgActivationV1>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for ActivationGlobals {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version: _,
            } = event
            {
                if interface == "xdg_activation_v1" && state.activation.is_none() {
                    // We speak version 1; a compositor that advertises the
                    // global supports at least that.
                    state.activation =
                        Some(registry.bind::<XdgActivationV1, _, _>(name, 1, qh, ()));
                }
            }
        }
    }

    // xdg_activation_v1 has no events; `unreachable!()` if that ever changes.
    wayland_client::delegate_noop!(ActivationGlobals: XdgActivationV1);

    impl WaylandState {
        fn connect(display_ptr: *mut c_void) -> Result<Self, Box<dyn Error>> {
            // SAFETY: `display_ptr` is the wl_display of winit's live
            // connection, which outlives every `Frame` we are handed. Guest
            // mode: on drop only our private queue and our own proxies are
            // destroyed, never the display itself.
            let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
            let conn = Connection::from_backend(backend);
            let mut queue = conn.new_event_queue::<ActivationGlobals>();
            let qh = queue.handle();
            let _registry = conn.display().get_registry(&qh, ());
            let mut globals = ActivationGlobals { activation: None };
            // Blocks for one compositor round trip (local socket, well under
            // a frame) and dispatches only this queue.
            queue.roundtrip(&mut globals)?;
            if globals.activation.is_none() {
                return Err("the compositor does not support xdg_activation_v1".into());
            }
            Ok(WaylandState {
                display_ptr,
                conn,
                queue,
                globals,
            })
        }

        fn activate(&mut self, surface_ptr: *mut c_void, token: &str) -> Result<(), Box<dyn Error>> {
            // Drain registry chatter (global add/remove) accumulated since
            // the last press so the queue buffer cannot grow over a session.
            self.queue.dispatch_pending(&mut self.globals)?;
            let activation = self
                .globals
                .activation
                .as_ref()
                .ok_or("xdg_activation_v1 disappeared from the registry")?;

            // SAFETY: `surface_ptr` is winit's live wl_surface for the very
            // window we are raising; it outlives this call. `from_ptr`
            // verifies the interface really is wl_surface.
            let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr.cast()) }?;
            let surface = WlSurface::from_id(&self.conn, id)?;

            activation.activate(token.to_owned(), &surface);
            // A roundtrip rather than a bare flush: it proves the compositor
            // consumed the request, so a connection that died since the last
            // press surfaces as an error here (and the cache is dropped)
            // instead of "succeeding" into a closed socket forever.
            self.queue.roundtrip(&mut self.globals)?;
            Ok(())
        }
    }

    /// Activate our already-mapped window with an xdg-activation token minted
    /// by the compositor for a real user action. `false` when this is not a
    /// Wayland window, the compositor lacks `xdg_activation_v1`, or anything
    /// on the way failed — the caller then falls back to a request for
    /// attention. Note the compositor may still quietly downgrade a stale
    /// token to that same attention hint; v1 has no error event to say so.
    pub(super) fn activate(frame: &eframe::Frame, token: &str) -> bool {
        let display_ptr = match frame.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(w)) => w.display.as_ptr(),
            Ok(other) => {
                quicksearch_core::log_warn!("raising the window: not a Wayland display: {:?}", other);
                return false;
            }
            Err(e) => {
                quicksearch_core::log_warn!("raising the window: no display handle: {}", e);
                return false;
            }
        };
        let surface_ptr = match frame.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Wayland(w)) => w.surface.as_ptr(),
            Ok(other) => {
                quicksearch_core::log_warn!("raising the window: not a Wayland window: {:?}", other);
                return false;
            }
            Err(e) => {
                quicksearch_core::log_warn!("raising the window: no window handle: {}", e);
                return false;
            }
        };

        let sent = WAYLAND.with(|slot| -> Result<(), Box<dyn Error>> {
            let mut slot = slot.borrow_mut();
            // A cached connection is only good for the display it wraps.
            if slot.as_ref().is_some_and(|s| s.display_ptr != display_ptr) {
                *slot = None;
            }
            let state = match slot.as_mut() {
                Some(state) => state,
                None => slot.insert(WaylandState::connect(display_ptr)?),
            };
            state.activate(surface_ptr, token)
        });
        match sent {
            Ok(()) => true,
            Err(e) => {
                quicksearch_core::log_warn!("raising the window: {}", e);
                // Do not reuse a connection that failed mid-way; the next
                // press reconnects.
                WAYLAND.with(|slot| slot.borrow_mut().take());
                false
            }
        }
    }
}
