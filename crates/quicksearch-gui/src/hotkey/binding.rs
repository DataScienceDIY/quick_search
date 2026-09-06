//! The one representation of a shortcut and its spellings: the config text
//! (which is also the `global-hotkey` token), the X11 keysym name (which
//! doubles as the GTK keyval for GNOME bindings), and the Qt key code KDE's
//! KGlobalAccel takes. All of them come out of [`KEYS`] and [`qt_key`], so a
//! key cannot be spelled correctly for one backend and wrongly for another.

use std::fmt;
use std::str::FromStr;

use egui::Key;

/// One row per bindable key: egui key, config/`global-hotkey` token,
/// xkbcommon keysym (`XKB_KEY_` stripped). Keys egui synthesises from a
/// character (`Plus`, `Colon`, …) are left out: they are the shifted face of
/// a key that already has a row, and binding both would register the same
/// physical press under two names.
const KEYS: &[(Key, &str, &str)] = &[
    (Key::A, "A", "a"),
    (Key::B, "B", "b"),
    (Key::C, "C", "c"),
    (Key::D, "D", "d"),
    (Key::E, "E", "e"),
    (Key::F, "F", "f"),
    (Key::G, "G", "g"),
    (Key::H, "H", "h"),
    (Key::I, "I", "i"),
    (Key::J, "J", "j"),
    (Key::K, "K", "k"),
    (Key::L, "L", "l"),
    (Key::M, "M", "m"),
    (Key::N, "N", "n"),
    (Key::O, "O", "o"),
    (Key::P, "P", "p"),
    (Key::Q, "Q", "q"),
    (Key::R, "R", "r"),
    (Key::S, "S", "s"),
    (Key::T, "T", "t"),
    (Key::U, "U", "u"),
    (Key::V, "V", "v"),
    (Key::W, "W", "w"),
    (Key::X, "X", "x"),
    (Key::Y, "Y", "y"),
    (Key::Z, "Z", "z"),
    (Key::Num0, "0", "0"),
    (Key::Num1, "1", "1"),
    (Key::Num2, "2", "2"),
    (Key::Num3, "3", "3"),
    (Key::Num4, "4", "4"),
    (Key::Num5, "5", "5"),
    (Key::Num6, "6", "6"),
    (Key::Num7, "7", "7"),
    (Key::Num8, "8", "8"),
    (Key::Num9, "9", "9"),
    (Key::F1, "F1", "F1"),
    (Key::F2, "F2", "F2"),
    (Key::F3, "F3", "F3"),
    (Key::F4, "F4", "F4"),
    (Key::F5, "F5", "F5"),
    (Key::F6, "F6", "F6"),
    (Key::F7, "F7", "F7"),
    (Key::F8, "F8", "F8"),
    (Key::F9, "F9", "F9"),
    (Key::F10, "F10", "F10"),
    (Key::F11, "F11", "F11"),
    (Key::F12, "F12", "F12"),
    (Key::Space, "Space", "space"),
    (Key::Enter, "Enter", "Return"),
    (Key::Tab, "Tab", "Tab"),
    (Key::Backspace, "Backspace", "BackSpace"),
    (Key::Delete, "Delete", "Delete"),
    (Key::Insert, "Insert", "Insert"),
    (Key::Home, "Home", "Home"),
    (Key::End, "End", "End"),
    (Key::PageUp, "PageUp", "Prior"),
    (Key::PageDown, "PageDown", "Next"),
    (Key::ArrowUp, "Up", "Up"),
    (Key::ArrowDown, "Down", "Down"),
    (Key::ArrowLeft, "Left", "Left"),
    (Key::ArrowRight, "Right", "Right"),
    (Key::Comma, "Comma", "comma"),
    (Key::Period, "Period", "period"),
    (Key::Slash, "Slash", "slash"),
    (Key::Backslash, "Backslash", "backslash"),
    (Key::Semicolon, "Semicolon", "semicolon"),
    (Key::Quote, "Quote", "apostrophe"),
    (Key::Backtick, "Backquote", "grave"),
    (Key::Minus, "Minus", "minus"),
    (Key::Equals, "Equal", "equal"),
    (Key::OpenBracket, "BracketLeft", "bracketleft"),
    (Key::CloseBracket, "BracketRight", "bracketright"),
];

/// Escape is reserved: it cancels the Settings tab's capture.
const RESERVED: &[Key] = &[Key::Escape];

/// A shortcut the user can press from anywhere. Super/Meta is absent
/// because `egui::Modifiers` has no field for it, so a Super combo could
/// never be captured in the Settings tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    key: Key,
}

/// Why a string or key press is not a usable shortcut; shown in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    Empty,
    NoModifier,
    NoKey,
    UnknownToken(String),
    /// More than one non-modifier token, as in `Ctrl+A+B`.
    TwoKeys,
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindingError::Empty => write!(f, "no shortcut"),
            BindingError::NoModifier => {
                write!(f, "needs at least one of Ctrl, Alt or Shift")
            }
            BindingError::NoKey => write!(f, "needs a key, not just modifiers"),
            BindingError::UnknownToken(t) => write!(f, "{:?} is not a key name", t),
            BindingError::TwoKeys => write!(f, "only one key, plus modifiers"),
        }
    }
}

impl Binding {
    /// Build from a key press egui reported. `None` for a press that cannot
    /// be a shortcut: no row in [`KEYS`], reserved, or no modifier held.
    pub fn from_egui(key: Key, modifiers: &egui::Modifiers) -> Option<Binding> {
        if RESERVED.contains(&key) || !KEYS.iter().any(|(k, _, _)| *k == key) {
            return None;
        }
        let binding = Binding {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            key,
        };
        binding.has_modifier().then_some(binding)
    }

    fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift
    }

    fn row(&self) -> (&'static str, &'static str) {
        KEYS.iter()
            .find(|(k, _, _)| *k == self.key)
            .map(|(_, token, keysym)| (*token, *keysym))
            .expect("every Binding key comes from KEYS")
    }

    /// The key combination as the integer `Qt::Key | Qt::Modifier` value
    /// KGlobalAccel's DBus `setShortcut` takes. Covered like [`Self::row`]:
    /// `tests::every_key_has_a_qt_code` proves the mapping total over
    /// [`KEYS`], so a new row cannot reach KDE as a panic.
    ///
    /// This and the GTK spelling below exist for the desktops
    /// `crate::shortcut_setup` writes to, all of which are unix; off unix
    /// nothing calls them and the lint would say so.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    pub fn qt_key_code(&self) -> u32 {
        let mut code = qt_key(self.key);
        for (held, bit) in [
            (self.shift, 0x0200_0000),
            (self.ctrl, 0x0400_0000),
            (self.alt, 0x0800_0000),
        ] {
            if held {
                code |= bit;
            }
        }
        code
    }

    /// The accelerator in GTK's syntax — `<Ctrl><Shift>f` — which is what a
    /// GNOME custom keybinding's `binding` key stores. GTK keyval names are
    /// the X11 keysym names, so the keysym column serves both spellings.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    pub fn gtk_accelerator(&self) -> String {
        let mut out = String::new();
        for (held, name) in [
            (self.ctrl, "<Ctrl>"),
            (self.alt, "<Alt>"),
            (self.shift, "<Shift>"),
        ] {
            if held {
                out.push_str(name);
            }
        }
        out.push_str(self.row().1);
        out
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if held {
                write!(f, "{}+", name)?;
            }
        }
        f.write_str(self.row().0)
    }
}

impl FromStr for Binding {
    type Err = BindingError;

    fn from_str(s: &str) -> Result<Binding, BindingError> {
        if s.trim().is_empty() {
            return Err(BindingError::Empty);
        }
        let mut binding = Binding {
            ctrl: false,
            alt: false,
            shift: false,
            key: Key::A,
        };
        let mut key = None;
        for raw in s.split('+') {
            let token = raw.trim();
            match token.to_ascii_uppercase().as_str() {
                "" => return Err(BindingError::UnknownToken(token.to_string())),
                "CTRL" | "CONTROL" => binding.ctrl = true,
                "ALT" => binding.alt = true,
                "SHIFT" => binding.shift = true,
                upper => {
                    if key.is_some() {
                        return Err(BindingError::TwoKeys);
                    }
                    key = Some(
                        KEYS.iter()
                            .find(|(_, t, _)| t.eq_ignore_ascii_case(upper))
                            .map(|(k, _, _)| *k)
                            .ok_or_else(|| BindingError::UnknownToken(token.to_string()))?,
                    );
                }
            }
        }
        binding.key = key.ok_or(BindingError::NoKey)?;
        if !binding.has_modifier() {
            return Err(BindingError::NoModifier);
        }
        Ok(binding)
    }
}

/// The `Qt::Key` value for a bindable key. Printable keys are their ASCII
/// uppercase; the named keys are Qt's `0x0100_00xx` block. A fourth [`KEYS`]
/// column in all but layout: kept as a match so the table stays readable,
/// with `tests::every_key_has_a_qt_code` holding the two together.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn qt_key(key: Key) -> u32 {
    match key {
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::F1 => 0x0100_0030,
        Key::F2 => 0x0100_0031,
        Key::F3 => 0x0100_0032,
        Key::F4 => 0x0100_0033,
        Key::F5 => 0x0100_0034,
        Key::F6 => 0x0100_0035,
        Key::F7 => 0x0100_0036,
        Key::F8 => 0x0100_0037,
        Key::F9 => 0x0100_0038,
        Key::F10 => 0x0100_0039,
        Key::F11 => 0x0100_003A,
        Key::F12 => 0x0100_003B,
        Key::Space => 0x20,
        Key::Enter => 0x0100_0004,
        Key::Tab => 0x0100_0001,
        Key::Backspace => 0x0100_0003,
        Key::Delete => 0x0100_0007,
        Key::Insert => 0x0100_0006,
        Key::Home => 0x0100_0010,
        Key::End => 0x0100_0011,
        Key::PageUp => 0x0100_0016,
        Key::PageDown => 0x0100_0017,
        Key::ArrowUp => 0x0100_0013,
        Key::ArrowDown => 0x0100_0015,
        Key::ArrowLeft => 0x0100_0012,
        Key::ArrowRight => 0x0100_0014,
        Key::Comma => 0x2C,
        Key::Period => 0x2E,
        Key::Slash => 0x2F,
        Key::Backslash => 0x5C,
        Key::Semicolon => 0x3B,
        Key::Quote => 0x27,
        Key::Backtick => 0x60,
        Key::Minus => 0x2D,
        Key::Equals => 0x3D,
        Key::OpenBracket => 0x5B,
        Key::CloseBracket => 0x5D,
        other => unreachable!("{other:?} is not in KEYS; see every_key_has_a_qt_code"),
    }
}

/// Empty means "no shortcut" rather than an error.
pub fn parse_setting(setting: &str) -> Result<Option<Binding>, BindingError> {
    if setting.trim().is_empty() {
        return Ok(None);
    }
    setting.parse().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_setting_parses() {
        let cfg = quicksearch_core::config::UiConfig::default();
        let binding: Binding = cfg.search_hotkey.parse().expect("the default is valid");
        assert_eq!(binding.to_string(), "Ctrl+Shift+F");
        assert_eq!(binding.gtk_accelerator(), "<Ctrl><Shift>f");
    }

    /// `qt_key`'s `unreachable!` is only sound while every row of [`KEYS`]
    /// has an arm; this is what holds the match and the table together.
    #[test]
    fn every_key_has_a_qt_code() {
        for (key, token, _) in KEYS {
            let code = qt_key(*key);
            assert_ne!(code, 0, "{token} has no Qt key code");
        }
    }

    /// The values KGlobalAccel actually receives, spot-checked against
    /// `Qt::Key`: the default binding (verified live against Plasma 6.6),
    /// a named key, and a punctuation key.
    #[test]
    fn qt_key_codes_match_qt() {
        let binding: Binding = "Ctrl+Shift+F".parse().unwrap();
        assert_eq!(binding.qt_key_code(), 0x0600_0046);
        let binding: Binding = "Alt+PageUp".parse().unwrap();
        assert_eq!(binding.qt_key_code(), 0x0900_0016);
        let binding: Binding = "Ctrl+Comma".parse().unwrap();
        assert_eq!(binding.qt_key_code(), 0x0400_002C);
    }

    /// The keysym column doubles as the GTK keyval, so a named key must come
    /// out under GTK's name for it, not egui's.
    #[test]
    fn gtk_accelerators_use_keysym_names() {
        let binding: Binding = "Ctrl+Alt+PageUp".parse().unwrap();
        assert_eq!(binding.gtk_accelerator(), "<Ctrl><Alt>Prior");
        let binding: Binding = "Shift+Enter".parse().unwrap();
        assert_eq!(binding.gtk_accelerator(), "<Shift>Return");
    }

    #[test]
    fn every_key_round_trips() {
        for (key, token, _) in KEYS {
            let binding = Binding {
                ctrl: true,
                alt: false,
                shift: false,
                key: *key,
            };
            let text = binding.to_string();
            assert_eq!(text, format!("Ctrl+{}", token));
            assert_eq!(text.parse::<Binding>(), Ok(binding), "{text} did not parse");
        }
    }

    /// Stops a typo in [`KEYS`] reaching `RegisterHotKey` as a silent failure.
    #[test]
    fn tokens_are_parseable_by_global_hotkey() {
        for (_, token, _) in KEYS {
            let text = format!("Ctrl+{}", token);
            assert!(
                text.parse::<global_hotkey::hotkey::HotKey>().is_ok(),
                "global-hotkey rejected {text:?}"
            );
        }
    }

    /// Two keys sharing a keysym would silently bind the wrong one on Wayland.
    #[test]
    fn rows_are_unique() {
        for (i, (key, token, keysym)) in KEYS.iter().enumerate() {
            for (other_key, other_token, other_keysym) in &KEYS[i + 1..] {
                assert_ne!(key, other_key, "{token} and {other_token} share a key");
                assert_ne!(token, other_token, "duplicate token {token}");
                assert_ne!(keysym, other_keysym, "duplicate keysym {keysym}");
            }
        }
    }

    #[test]
    fn modifiers_are_ordered_and_case_insensitive() {
        let binding: Binding = "shift+ALT+ctrl+f".parse().unwrap();
        assert_eq!(binding.to_string(), "Ctrl+Alt+Shift+F");
        assert_eq!(binding.gtk_accelerator(), "<Ctrl><Alt><Shift>f");
        assert_eq!(
            " Ctrl + Shift + F ".parse::<Binding>(),
            Ok(binding_of("Ctrl+Shift+F"))
        );
    }

    fn binding_of(s: &str) -> Binding {
        s.parse().unwrap()
    }

    #[test]
    fn bad_settings_are_rejected() {
        assert_eq!("F".parse::<Binding>(), Err(BindingError::NoModifier));
        assert_eq!("Ctrl".parse::<Binding>(), Err(BindingError::NoKey));
        assert_eq!("Ctrl+Shift".parse::<Binding>(), Err(BindingError::NoKey));
        assert_eq!("Ctrl+A+B".parse::<Binding>(), Err(BindingError::TwoKeys));
        assert_eq!(
            "Ctrl+".parse::<Binding>(),
            Err(BindingError::UnknownToken(String::new()))
        );
        assert_eq!(
            "Ctrl+Nope".parse::<Binding>(),
            Err(BindingError::UnknownToken("Nope".to_string()))
        );
        assert_eq!("".parse::<Binding>(), Err(BindingError::Empty));
        assert_eq!(
            "Ctrl+Escape".parse::<Binding>(),
            Err(BindingError::UnknownToken("Escape".to_string()))
        );
    }

    #[test]
    fn an_empty_setting_is_no_shortcut_not_an_error() {
        assert_eq!(parse_setting(""), Ok(None));
        assert_eq!(parse_setting("   "), Ok(None));
        assert_eq!(
            parse_setting("Ctrl+Shift+F"),
            Ok(Some(binding_of("Ctrl+Shift+F")))
        );
        assert!(parse_setting("Ctrl+Nope").is_err());
    }

    #[test]
    fn capture_needs_a_modifier_and_a_known_key() {
        let ctrl = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(
            Binding::from_egui(Key::F, &ctrl),
            Some(binding_of("Ctrl+F"))
        );
        assert_eq!(
            Binding::from_egui(Key::F, &egui::Modifiers::default()),
            None
        );
        assert_eq!(Binding::from_egui(Key::Escape, &ctrl), None);
        assert_eq!(Binding::from_egui(Key::Plus, &ctrl), None);
    }

    /// egui sets `command` alongside `ctrl` off Mac; must not double up.
    #[test]
    fn the_egui_command_alias_is_ignored() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            Binding::from_egui(Key::F, &modifiers).map(|b| b.to_string()),
            Some("Ctrl+Shift+F".to_string())
        );
    }
}
