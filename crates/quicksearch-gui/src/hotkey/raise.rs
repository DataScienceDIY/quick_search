//! Bringing the window to the front when the shortcut fires.
//!
//! Harder than it sounds, and for a good reason: every desktop stops
//! applications raising themselves over whatever the user is doing. The
//! request has to say *why*, and a global shortcut is a direct user action
//! rather than an application deciding it wants attention.
//!
//! What that means in practice differs per platform:
//!
//! * **Windows** refuses `SetForegroundWindow` to background processes, but
//!   makes an explicit exception for a process whose registered hotkey was
//!   just pressed. winit's `Minimized(false)` and `Focus` do the right thing,
//!   as long as they happen straight away.
//! * **X11** is the awkward one. winit asks with `_NET_ACTIVE_WINDOW` and a
//!   source indication of 1, "application", which KWin, Mutter and Xfwm all
//!   refuse from an unfocused window: nothing happens, or the taskbar entry
//!   blinks. Worse, winit's `focus_window` does nothing at all while the
//!   window is minimised. So the request is sent here instead, with source
//!   indication 2, which the EWMH spec defines as a client acting on a direct
//!   user action and which window managers honour. That is exactly what this
//!   is, and it is what every hotkey launcher does.
//! * **Wayland** does not let a client raise itself at all, by design. See
//!   [`raise`].

/// Bring the window to the front, restoring it if it was minimised.
///
/// On Wayland this asks and is ignored: raising requires an xdg-activation
/// token from the compositor, which winit will not issue without its own
/// `Window`, and eframe does not hand that out. The rest of the shortcut
/// still works there (the Search tab is selected and the query box gets the
/// caret), and the desktop's own window-management shortcuts are the way
/// back to the window. The Options window says so.
pub fn raise(ctx: &egui::Context, frame: &eframe::Frame) {
    #[cfg(all(unix, not(target_os = "macos")))]
    if x11_activate(frame) {
        return;
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = frame;

    // Un-minimising comes first: a window still minimised cannot take focus.
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// Ask the window manager to activate our window, EWMH style. `false` when
/// this is not an X11 session, or the X server would not take it, so the
/// caller can fall back to asking winit.
///
/// A fresh connection per press rather than a kept one: this runs at most as
/// often as someone presses a key, the round trip is sub-millisecond, and a
/// cached connection would be one more thing to notice the X server going
/// away on.
#[cfg(all(unix, not(target_os = "macos")))]
fn x11_activate(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};

    let Ok(handle) = frame.window_handle() else {
        return false;
    };
    // Wayland and everything else fall through to the caller's fallback.
    let RawWindowHandle::Xlib(xlib) = handle.as_raw() else {
        return false;
    };
    let window = xlib.window as u32;

    let sent = || -> Result<(), Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let atom = conn.intern_atom(true, b"_NET_ACTIVE_WINDOW")?.reply()?.atom;
        // data: source indication, timestamp, the window losing focus.
        // `CURRENT_TIME` because the shortcut arrives over D-Bus or a grab
        // rather than as an X event we could take a timestamp from; window
        // managers accept it from source 2.
        let event = ClientMessageEvent::new(32, window, atom, [2, x11rb::CURRENT_TIME, 0, 0, 0]);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(())
    }();
    match sent {
        Ok(()) => true,
        Err(e) => {
            quicksearch_core::log_warn!("raising the window: {}", e);
            false
        }
    }
}
