use std::fmt;
use std::str::FromStr;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};

/// A global key combination, e.g. `alt+shift+h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub(crate) modifiers: HOT_KEY_MODIFIERS,
    /// Win32 virtual-key code.
    pub(crate) vk: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseHotkeyError(String);

impl fmt::Display for ParseHotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseHotkeyError {}

impl FromStr for Hotkey {
    type Err = ParseHotkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |msg: &str| ParseHotkeyError(format!("invalid hotkey `{s}`: {msg}"));
        let parts: Vec<String> = s
            .split('+')
            .map(|p| p.trim().to_ascii_lowercase())
            .collect();
        let (key, mods) = parts.split_last().ok_or_else(|| err("empty"))?;

        let mut modifiers = HOT_KEY_MODIFIERS(0);
        for m in mods {
            modifiers |= match m.as_str() {
                "alt" => MOD_ALT,
                "ctrl" | "control" => MOD_CONTROL,
                "shift" => MOD_SHIFT,
                "win" | "super" => MOD_WIN,
                other => return Err(err(&format!("unknown modifier `{other}`"))),
            };
        }
        let vk = key_code(key).ok_or_else(|| err(&format!("unknown key `{key}`")))?;
        Ok(Hotkey { modifiers, vk })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (flag, name) in [
            (MOD_WIN, "win"),
            (MOD_CONTROL, "ctrl"),
            (MOD_ALT, "alt"),
            (MOD_SHIFT, "shift"),
        ] {
            if self.modifiers.contains(flag) {
                write!(f, "{name}+")?;
            }
        }
        match key_name(self.vk) {
            Some(name) => f.write_str(&name),
            None => write!(f, "vk{:#04x}", self.vk),
        }
    }
}

const NAMED_KEYS: &[(&str, u32)] = &[
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
    ("down", 0x28),
    ("enter", 0x0D),
    ("space", 0x20),
    ("tab", 0x09),
    ("escape", 0x1B),
    ("backspace", 0x08),
    ("delete", 0x2E),
    ("minus", 0xBD),
    ("equal", 0xBB),
    ("comma", 0xBC),
    ("period", 0xBE),
    ("semicolon", 0xBA),
];

fn key_code(key: &str) -> Option<u32> {
    if let [c] = key.as_bytes()
        && c.is_ascii_alphanumeric()
    {
        // Virtual-key codes for letters and digits are their uppercase ASCII.
        return Some(c.to_ascii_uppercase() as u32);
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    let key = if key == "esc" { "escape" } else { key };
    NAMED_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|&(_, vk)| vk)
}

fn key_name(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => Some((vk as u8 as char).to_ascii_lowercase().to_string()),
        0x70..=0x87 => Some(format!("f{}", vk - 0x70 + 1)),
        _ => NAMED_KEYS
            .iter()
            .find(|&&(_, code)| code == vk)
            .map(|(name, _)| name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Hotkey {
        s.parse().unwrap()
    }

    #[test]
    fn parses_modifiers_and_letter() {
        let hk = parse("Alt+Shift+H");
        assert_eq!(hk.modifiers, MOD_ALT | MOD_SHIFT);
        assert_eq!(hk.vk, 'H' as u32);
    }

    #[test]
    fn parses_function_and_named_keys() {
        assert_eq!(parse("ctrl+f12").vk, 0x7B);
        assert_eq!(parse("win+left").vk, 0x25);
        assert_eq!(parse("alt+esc").vk, 0x1B);
        assert_eq!(parse("alt+1").vk, '1' as u32);
    }

    #[test]
    fn rejects_bad_input() {
        assert!("alt+".parse::<Hotkey>().is_err());
        assert!("hyper+h".parse::<Hotkey>().is_err());
        assert!("alt+f25".parse::<Hotkey>().is_err());
        assert!("alt+hh".parse::<Hotkey>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for s in ["win+ctrl+alt+shift+h", "alt+f4", "alt+enter", "shift+7"] {
            assert_eq!(parse(s).to_string(), s);
        }
    }
}
