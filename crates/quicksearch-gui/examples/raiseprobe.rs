//! Does the window manager honour `_NET_ACTIVE_WINDOW` from a client that is
//! not focused, and does the timestamp matter?
//!
//! `activate::raise` depends on the answer: it sends source indication 2
//! ("direct user action") with `CURRENT_TIME`, and the search shortcut looked
//! broken because the window never came forward. This reproduces that exact
//! request in isolation, so the fix is chosen against a real window manager
//! rather than against a reading of the EWMH spec.
//!
//! It creates its own window, lets a freshly spawned `xterm` take focus, then
//! tries to activate itself and reports whether it won.
//!
//! ```text
//! cargo run -p quicksearch-gui --example raiseprobe -- current
//! cargo run -p quicksearch-gui --example raiseprobe -- server
//! ```
//!
//! `current` sends `CURRENT_TIME`, what the code does today. `server` fetches
//! a real server timestamp first and also sets `_NET_WM_USER_TIME`.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateWindowAux,
    EventMask, PropMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

fn atom(conn: &RustConnection, name: &[u8]) -> Result<u32, Box<dyn std::error::Error>> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

/// The window the WM currently considers active, per the root property.
fn active(conn: &RustConnection, root: Window) -> Result<Window, Box<dyn std::error::Error>> {
    let prop = atom(conn, b"_NET_ACTIVE_WINDOW")?;
    let reply = conn
        .get_property(false, root, prop, AtomEnum::WINDOW, 0, 1)?
        .reply()?;
    Ok(reply.value32().and_then(|mut v| v.next()).unwrap_or(0))
}

/// A real timestamp, the standard way: a zero-length property append comes
/// back as a `PropertyNotify` carrying the server's current time.
fn server_time(conn: &RustConnection, window: Window) -> Result<u32, Box<dyn std::error::Error>> {
    let marker = atom(conn, b"_QUICKSEARCH_TIME")?;
    conn.change_property8(PropMode::APPEND, window, marker, AtomEnum::STRING, &[])?;
    conn.flush()?;
    loop {
        if let Event::PropertyNotify(e) = conn.wait_for_event()? {
            if e.window == window && e.atom == marker {
                return Ok(e.time);
            }
        }
    }
}

/// Minimise someone else's window, so the "restores it if it was minimised"
/// half of `raise` can be tested. There is no command-line tool for this on
/// this machine, and ICCCM says a client asks by sending `WM_CHANGE_STATE`.
fn iconify(target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let target = u32::from_str_radix(target.trim_start_matches("0x"), 16)?;
    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let change_state = atom(&conn, b"WM_CHANGE_STATE")?;
    // 3 is ICCCM's IconicState.
    let event = ClientMessageEvent::new(32, target, change_state, [3, 0, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "current".into());
    if mode == "iconify" {
        let target = std::env::args()
            .nth(2)
            .ok_or("usage: raiseprobe iconify <winid>")?;
        return iconify(&target);
    }
    if mode == "hotkey" {
        return send_hotkey();
    }
    if mode != "current" && mode != "server" {
        eprintln!("usage: raiseprobe [current|server|iconify <winid>|hotkey]");
        std::process::exit(2);
    }

    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // A window the WM will manage like any other: not override-redirect, and
    // asking for PropertyNotify so `server_time` has something to wait on.
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        100,
        100,
        400,
        200,
        2,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"quicksearch raiseprobe",
    )?;
    conn.map_window(window)?;
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(800));
    println!("probe window     : 0x{:x}", window);

    // Something else has to hold focus, or activating ourselves proves nothing.
    let mut thief = std::process::Command::new("xterm")
        .args(["-geometry", "40x8+600+400", "-T", "raiseprobe-thief"])
        .spawn()?;
    std::thread::sleep(Duration::from_millis(2500));

    let before = active(&conn, root)?;
    println!("active before    : 0x{:x}", before);
    if before == window {
        println!("INCONCLUSIVE: the probe still had focus; xterm never took it.");
        let _ = thief.kill();
        return Ok(());
    }

    // The request under test, as `raise::x11_activate` builds it.
    let time = if mode == "server" {
        let t = server_time(&conn, window)?;
        let user_time = atom(&conn, b"_NET_WM_USER_TIME")?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            user_time,
            AtomEnum::CARDINAL,
            &[t],
        )?;
        conn.flush()?;
        println!("server timestamp : {}", t);
        t
    } else {
        println!("server timestamp : CURRENT_TIME (0)");
        CURRENT_TIME
    };

    let net_active = atom(&conn, b"_NET_ACTIVE_WINDOW")?;
    let message = ClientMessageEvent::new(32, window, net_active, [2, time, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        message,
    )?;
    conn.flush()?;

    std::thread::sleep(Duration::from_millis(1200));
    let after = active(&conn, root)?;
    println!("active after     : 0x{:x}", after);
    println!(
        "\nRESULT ({}): {}",
        mode,
        if after == window {
            "RAISED — the window manager honoured it"
        } else {
            "REFUSED — the window did not come forward"
        }
    );

    let _ = thief.kill();
    let _ = thief.wait();
    conn.change_window_attributes(window, &ChangeWindowAttributesAux::new())?;
    conn.destroy_window(window)?;
    conn.flush()?;
    Ok(())
}

/// Synthesise a global `Ctrl+Shift+F` through XTEST, so the whole shortcut
/// path can be tested the way a user exercises it: a real key press arriving
/// while some other window has focus.
fn send_hotkey() -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::protocol::xproto::{GetKeyboardMappingReply, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};

    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let min = conn.setup().min_keycode;
    let count = conn.setup().max_keycode - min + 1;
    let mapping: GetKeyboardMappingReply = conn.get_keyboard_mapping(min, count)?.reply()?;
    let per = mapping.keysyms_per_keycode as usize;

    let keycode = |sym: u32| -> Option<u8> {
        mapping
            .keysyms
            .chunks(per)
            .position(|row| row.contains(&sym))
            .map(|i| min + i as u8)
    };
    // XK_Control_L, XK_Shift_L, XK_f.
    let ctrl = keycode(0xffe3).ok_or("no Control key")?;
    let shift = keycode(0xffe1).ok_or("no Shift key")?;
    let f = keycode(0x0066).ok_or("no F key")?;

    for (ty, code) in [
        (KEY_PRESS_EVENT, ctrl),
        (KEY_PRESS_EVENT, shift),
        (KEY_PRESS_EVENT, f),
        (KEY_RELEASE_EVENT, f),
        (KEY_RELEASE_EVENT, shift),
        (KEY_RELEASE_EVENT, ctrl),
    ] {
        x11rb::protocol::xtest::fake_input(&conn, ty, code, 0, root, 0, 0, 0)?;
    }
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
