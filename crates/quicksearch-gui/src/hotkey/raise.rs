//! Bringing the window to the front when the shortcut fires. Every desktop
//! stops applications raising themselves, so the request has to say *why*.
//!
//! * **Windows**: `SetForegroundWindow` is refused to background processes,
//!   except for one whose registered hotkey was just pressed — so winit's
//!   commands work, as long as they happen straight away.
//! * **X11**: winit asks with `_NET_ACTIVE_WINDOW` source indication 1
//!   ("application"), which KWin, Mutter and Xfwm all refuse from an
//!   unfocused window, and its `focus_window` does nothing while minimised.
//!   Hence [`x11_activate`], which sends source indication 2 (EWMH's "direct
//!   user action"). Do not replace it with winit's version.
//! * **Wayland**: a client cannot raise itself at all, by design.

/// Bring the window to the front, restoring it if it was minimised.
///
/// On Wayland this asks and is ignored — raising needs an xdg-activation
/// token winit will not issue without its own `Window`. The rest of the
/// shortcut still works there.
pub fn raise(ctx: &egui::Context, frame: &eframe::Frame) {
    #[cfg(all(unix, not(target_os = "macos")))]
    if x11_activate(frame) {
        return;
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = frame;

    // A window still minimised cannot take focus.
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// Activate our window, EWMH style. `false` when this is not X11 or the
/// server would not take it, so the caller can fall back to winit.
#[cfg(all(unix, not(target_os = "macos")))]
fn x11_activate(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};

    let Ok(handle) = frame.window_handle() else {
        return false;
    };
    let RawWindowHandle::Xlib(xlib) = handle.as_raw() else {
        return false;
    };
    let window = xlib.window as u32;

    let sent = || -> Result<(), Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let atom = conn.intern_atom(true, b"_NET_ACTIVE_WINDOW")?.reply()?.atom;
        // data: source indication, timestamp, the window losing focus.
        // `CURRENT_TIME` because the shortcut arrives over D-Bus or a grab,
        // not as an X event carrying one; WMs accept it from source 2.
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
