//! Key combinations as the user types them (`SUPER + ALT + P`) and as each compositor spells them.

use std::fmt;

const SHIFT: u8 = 1;
const CTRL: u8 = 4;
const ALT: u8 = 8;
const SUPER: u8 = 64;

/// Modifiers in the order Portico prints them, with Hyprland's modmask bit for each.
const MODIFIERS: [(u8, &str); 4] = [
    (SUPER, "SUPER"),
    (CTRL, "CTRL"),
    (ALT, "ALT"),
    (SHIFT, "SHIFT"),
];

/// One or more modifiers and one key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Combo {
    /// Hyprland's modmask: SHIFT 1, CTRL 4, ALT 8, SUPER 64.
    mods: u8,
    /// Letters, digits and `_` only, so the key can be written into any configuration as is.
    key: String,
}

impl Combo {
    /// The proposal: SUPER + ALT + P.
    pub fn default_proposal() -> Self {
        Self {
            mods: SUPER | ALT,
            key: "P".into(),
        }
    }

    /// Modifiers and one key joined by `+` or spaces, in any case.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut mods = 0;
        let mut key: Option<String> = None;
        for token in text
            .split(|c: char| c == '+' || c.is_whitespace())
            .filter(|token| !token.is_empty())
        {
            match modifier(token) {
                Some(bit) => mods |= bit,
                None if key.is_some() => {
                    return Err(format!("{text:?} names more than one key"));
                }
                None if token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                    key = Some(token.to_ascii_uppercase());
                }
                None => return Err(format!("{token:?} is not a key name")),
            }
        }
        let Some(key) = key else {
            return Err(format!("{text:?} names no key"));
        };
        if mods == 0 {
            return Err(format!(
                "{text:?} has no modifier (SUPER, CTRL, ALT or SHIFT)"
            ));
        }
        Ok(Self { mods, key })
    }

    pub fn modmask(&self) -> u8 {
        self.mods
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Hyprland's `.conf` spelling of the modifiers: `SUPER ALT`.
    pub fn hyprland_mods(&self) -> String {
        self.names().collect::<Vec<_>>().join(" ")
    }

    /// Sway's spelling: `$mod+Alt+p` when `$mod` is the SUPER key, `Mod4+Alt+p` otherwise.
    pub fn sway(&self, mod_is_super: bool) -> String {
        let mut parts: Vec<String> = MODIFIERS
            .iter()
            .filter(|(bit, _)| self.mods & bit != 0)
            .map(|(bit, _)| match *bit {
                SUPER if mod_is_super => "$mod".into(),
                SUPER => "Mod4".into(),
                CTRL => "Ctrl".into(),
                ALT => "Alt".into(),
                _ => "Shift".into(),
            })
            .collect();
        parts.push(if self.key.len() == 1 {
            self.key.to_ascii_lowercase()
        } else {
            self.key.clone()
        });
        parts.join("+")
    }

    fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        MODIFIERS
            .iter()
            .filter(|(bit, _)| self.mods & bit != 0)
            .map(|(_, name)| *name)
    }
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for name in self.names() {
            write!(f, "{name} + ")?;
        }
        write!(f, "{}", self.key)
    }
}

/// A modifier's bit, from any of its spellings; Sway's names included.
pub(crate) fn modifier(token: &str) -> Option<u8> {
    match token.to_ascii_uppercase().as_str() {
        "SUPER" | "WIN" | "LOGO" | "MOD4" => Some(SUPER),
        "CTRL" | "CONTROL" => Some(CTRL),
        "ALT" | "MOD1" => Some(ALT),
        "SHIFT" => Some(SHIFT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_any_spelling_and_prints_canonically() {
        for text in [
            "SUPER + ALT + P",
            "super+alt+p",
            "alt super p",
            "Mod4 + Mod1 + P",
            "WIN+ALT+P",
        ] {
            let combo = Combo::parse(text).unwrap();
            assert_eq!(combo, Combo::default_proposal(), "{text}");
            assert_eq!(combo.to_string(), "SUPER + ALT + P");
        }
        assert_eq!(
            Combo::parse("shift ctrl super f5").unwrap().to_string(),
            "SUPER + CTRL + SHIFT + F5"
        );
    }

    #[test]
    fn rejects_what_cannot_be_bound_safely() {
        assert!(Combo::parse("P").unwrap_err().contains("no modifier"));
        assert!(Combo::parse("SUPER").unwrap_err().contains("no key"));
        assert!(
            Combo::parse("SUPER + P + Q")
                .unwrap_err()
                .contains("more than one key")
        );
        assert!(
            Combo::parse("SUPER + \"")
                .unwrap_err()
                .contains("not a key name")
        );
        assert!(Combo::parse("").is_err());
    }

    #[test]
    fn spells_each_compositor_s_form() {
        let combo = Combo::default_proposal();
        assert_eq!(combo.modmask(), 72);
        assert_eq!(combo.hyprland_mods(), "SUPER ALT");
        assert_eq!(combo.sway(true), "$mod+Alt+p");
        assert_eq!(combo.sway(false), "Mod4+Alt+p");
        assert_eq!(
            Combo::parse("CTRL+SHIFT+F12").unwrap().sway(true),
            "Ctrl+Shift+F12"
        );
    }
}
